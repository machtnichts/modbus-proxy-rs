//! The one and only connection to the device.
//!
//! Everything about this type exists to guarantee a single-client device sees
//! exactly one client: one persistent socket, one request in flight (mutex),
//! a configurable minimum gap between requests, MBAP transaction-id validation
//! and a backoff after *every* kind of failure - connect and read alike.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::UpstreamCfg;
use crate::logging;
use crate::modbus::{mbap, parse_mbap, MBAP_LEN};
use crate::stats::Stats;

#[derive(Debug)]
pub enum UpstreamError {
    /// The device itself returned a Modbus exception. The code is relayed
    /// verbatim downstream so a client can tell "no such register" from
    /// "device unreachable".
    Modbus(u8),
    Timeout(String),
    Io(String),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Modbus(code) => write!(f, "device modbus exception 0x{:02x}", code),
            UpstreamError::Timeout(m) => write!(f, "timeout: {}", m),
            UpstreamError::Io(m) => write!(f, "{}", m),
        }
    }
}

impl UpstreamError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, UpstreamError::Timeout(_))
    }
}

// A few bytes as hex, for the debug trace.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join("")
}

/// The longest the proxy will stay quiet after a failure.
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Retry schedule after a failure: 5 s, 10, 20, 40, 80, 160, then the 5 minute cap,
/// for as long as it keeps failing. A pure function of the previous value, so the
/// schedule can be tested without a device, a socket or a clock.
///
/// Why this matters: reconnecting immediately after a failed read turns one slow or
/// desynced response into a retry storm (960 attempts an hour, measured), and a busy
/// Modbus front end never gets the quiet it needs to drain its queue. A patient client
/// that retries on its own poll cadence loses a cycle; an impatient one loses the night.
fn next_backoff(cur: Duration) -> Duration {
    if cur.is_zero() {
        Duration::from_secs(5)
    } else {
        (cur * 2).min(BACKOFF_MAX)
    }
}

pub struct Conn {
    stream: Option<TcpStream>,
    tid: u16,
    last_request_at: Option<Instant>,
    backoff: Duration,
    next_attempt_at: Option<Instant>,
}

pub struct Upstream {
    pub host: String,
    pub port: u16,
    unit: u8,
    connect_timeout: Duration,
    response_timeout: Duration,
    min_gap: Duration,
    /// See `UpstreamCfg::idle_close`. Zero disables the idle close.
    idle_close: Duration,
    conn: Mutex<Conn>,
    stats: Arc<Stats>,
}

impl Upstream {
    pub fn new(cfg: &UpstreamCfg, stats: Arc<Stats>, min_gap: Duration) -> Upstream {
        Upstream {
            host: cfg.host.clone(),
            port: cfg.port,
            unit: cfg.unit,
            connect_timeout: cfg.connect_timeout,
            response_timeout: cfg.response_timeout,
            min_gap,
            idle_close: cfg.idle_close,
            conn: Mutex::new(Conn {
                stream: None,
                tid: 0,
                last_request_at: None,
                backoff: Duration::ZERO,
                next_attempt_at: None,
            }),
            stats,
        }
    }

    /// Drop the upstream connection if it has been quiet for longer than `idle_close`, so the
    /// next burst gets a fresh one.
    ///
    /// The device closes every Modbus/TCP connection after ~330 s, whether or not it is being
    /// read on (measured on this plant: of 844 failed reads with a known connection age, 809
    /// sat on connections 5-6 minutes old, median and p90 330.3 s - 11 x the app's 30 s cycle,
    /// which is why the age is so sharp). The old behaviour carried one connection across those
    /// 11 cycles and discovered the closure with the first read after it, costing the client
    /// that cycle. Closing it ourselves while it is idle means no connection ever gets old
    /// enough to be closed under a read - and a connect costs ~1 ms on this LAN (measured:
    /// median 1 ms, max 32 ms from client arrival to upstream connect).
    ///
    /// Deliberately not `fail_locked`: nothing failed here, so this must not enter the failure
    /// backoff, must not count as an upstream error, and must not raise `upstream_reconnects`
    /// (which the app's proxy card shows as a fault signal).
    fn close_if_stale(&self, conn: &mut Conn) {
        if self.idle_close.is_zero() || conn.stream.is_none() {
            return;
        }
        let quiet = match conn.last_request_at {
            Some(t) => t.elapsed() >= self.idle_close,
            None => false,
        };
        if !quiet {
            return;
        }
        if let Some(s) = conn.stream.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
            self.stats
                .upstream_idle_closes
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn connect_locked(&self, conn: &mut Conn) -> Result<(), UpstreamError> {
        if conn.stream.is_some() {
            return Ok(());
        }
        if let Some(next) = conn.next_attempt_at {
            if Instant::now() < next {
                return Err(UpstreamError::Io("upstream backoff active".into()));
            }
        }
        let addr = format!("{}:{}", self.host, self.port);
        let sock = std::net::ToSocketAddrs::to_socket_addrs(&addr)
            .map_err(|e| UpstreamError::Io(format!("resolve {}: {}", addr, e)))?
            .next()
            .ok_or_else(|| UpstreamError::Io(format!("no address for {}", addr)))?;

        match TcpStream::connect_timeout(&sock, self.connect_timeout) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(self.response_timeout));
                let _ = stream.set_write_timeout(Some(self.response_timeout));
                conn.stream = Some(stream);
                // NOTE: the backoff is deliberately *not* reset here. It belongs to the
                // read path: a connect that succeeds and a read that then fails is still
                // a failing device, and the wait must keep doubling until a read actually
                // answers (see fail_locked / the Ok arm in request()).
                logging::info(format!("upstream connected to {}", addr));
                Ok(())
            }
            Err(e) => {
                // Same schedule as a failed read: a device that refuses connections
                // needs quiet just as much as one that answers wrongly.
                conn.backoff = next_backoff(conn.backoff);
                conn.next_attempt_at = Some(Instant::now() + conn.backoff);
                self.stats.note_upstream_error(format!("connect: {}", e));
                self.stats
                    .upstream_backoff_ms
                    .store(conn.backoff.as_millis() as u64, Ordering::Relaxed);
                logging::warn(format!(
                    "upstream connect failed ({}), retry in {:.0}s",
                    e,
                    conn.backoff.as_secs_f64()
                ));
                Err(UpstreamError::Io(format!("connect: {}", e)))
            }
        }
    }

    fn drop_locked(conn: &mut Conn, stats: &Stats) {
        if let Some(s) = conn.stream.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
            stats.upstream_reconnects.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One failed read: close the socket and stop talking to the device for a while.
    ///
    /// This is the difference between losing a cycle and losing a night. Reconnecting
    /// immediately after a failure - which is what happened here before - turns a single
    /// slow or desynced response into a retry storm, and the device never gets the quiet
    /// it needs to drain its queue. The next successful read zeroes the schedule again.
    fn fail_locked(&self, conn: &mut Conn, msg: String) {
        conn.backoff = next_backoff(conn.backoff);
        conn.next_attempt_at = Some(Instant::now() + conn.backoff);
        Self::drop_locked(conn, &self.stats);
        self.stats.note_upstream_error(msg.clone());
        self.stats
            .upstream_backoff_ms
            .store(conn.backoff.as_millis() as u64, Ordering::Relaxed);
        logging::warn(format!(
            "upstream read failed ({}), leaving the device alone for {:.0}s",
            msg,
            conn.backoff.as_secs_f64()
        ));
    }

    /// Send one PDU upstream and return the response PDU. Serialised.
    pub fn request(&self, pdu: &[u8], expect_response: bool) -> Result<Vec<u8>, UpstreamError> {
        let started = Instant::now();
        let mut conn = self.conn.lock().unwrap();
        self.close_if_stale(&mut conn);
        self.connect_locked(&mut conn)?;

        if let Some(last) = conn.last_request_at {
            let elapsed = last.elapsed();
            if elapsed < self.min_gap {
                std::thread::sleep(self.min_gap - elapsed);
            }
        }

        conn.tid = conn.tid.wrapping_add(1) & 0xFFFF;
        if conn.tid == 0 {
            conn.tid = 1;
        }
        let tid = conn.tid;
        let frame = mbap(tid, 0, self.unit, pdu);

        {
            let stream = conn
                .stream
                .as_mut()
                .ok_or_else(|| UpstreamError::Io("not connected".into()))?;
            if let Err(e) = stream.write_all(&frame).and_then(|_| stream.flush()) {
                self.fail_locked(&mut conn, format!("write: {}", e));
                return Err(UpstreamError::Io(format!("write: {}", e)));
            }
        }
        conn.last_request_at = Some(Instant::now());

        if !expect_response {
            return Ok(Vec::new());
        }

        let mut hdr = [0u8; MBAP_LEN];
        let read_result = (|| -> Result<Vec<u8>, UpstreamError> {
            let stream = conn
                .stream
                .as_mut()
                .ok_or_else(|| UpstreamError::Io("not connected".into()))?;
            stream
                .read_exact(&mut hdr)
                .map_err(|e| map_io(e, self.response_timeout))?;
            let h =
                parse_mbap(&hdr).ok_or_else(|| UpstreamError::Io("short MBAP header".into()))?;
            if h.tid != tid {
                return Err(UpstreamError::Io(format!(
                    "transaction id mismatch (want {} got {})",
                    tid, h.tid
                )));
            }
            let want = h.length.saturating_sub(1) as usize;
            let mut body = vec![0u8; want];
            if want > 0 {
                stream
                    .read_exact(&mut body)
                    .map_err(|e| map_io(e, self.response_timeout))?;
            }
            Ok(body)
        })();

        match read_result {
            Ok(body) => {
                // The device answered properly: the schedule starts over.
                conn.backoff = Duration::ZERO;
                conn.next_attempt_at = None;
                self.stats.upstream_backoff_ms.store(0, Ordering::Relaxed);
                self.stats.mark_upstream_ok();
                logging::debug(format!(
                    "upstream tid={} ok in {}ms, {} bytes: {}",
                    tid,
                    started.elapsed().as_millis(),
                    body.len(),
                    hex(&body)
                ));
                Ok(body)
            }
            Err(UpstreamError::Timeout(msg)) => {
                self.stats.upstream_timeouts.fetch_add(1, Ordering::Relaxed);
                self.fail_locked(&mut conn, msg.clone());
                logging::debug(format!(
                    "upstream tid={} TIMEOUT after {}ms: {}",
                    tid,
                    started.elapsed().as_millis(),
                    msg
                ));
                Err(UpstreamError::Timeout(msg))
            }
            Err(e) => {
                self.fail_locked(&mut conn, e.to_string());
                logging::debug(format!(
                    "upstream tid={} failed after {}ms: {}",
                    tid,
                    started.elapsed().as_millis(),
                    e
                ));
                Err(e)
            }
        }
    }

    /// Read `count` registers (FC3/FC4) and return them as words.
    pub fn read_registers(
        &self,
        address: u16,
        count: u16,
        fc: u8,
    ) -> Result<Vec<u16>, UpstreamError> {
        if count == 0 || count > 125 {
            return Err(UpstreamError::Io(format!(
                "invalid register count {}",
                count
            )));
        }
        let mut pdu = Vec::with_capacity(5);
        pdu.push(fc);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&count.to_be_bytes());
        let body = self.request(&pdu, true)?;
        if body.is_empty() {
            return Err(UpstreamError::Io("empty response".into()));
        }
        if body[0] & 0x80 != 0 {
            let code = *body.get(1).unwrap_or(&0x0B);
            return Err(UpstreamError::Modbus(code));
        }
        if body[0] != fc {
            return Err(UpstreamError::Io(format!(
                "unexpected function code 0x{:02x}",
                body[0]
            )));
        }
        let nbytes = *body.get(1).unwrap_or(&0) as usize;
        if nbytes != count as usize * 2 {
            return Err(UpstreamError::Io(format!(
                "byte count {} != {}",
                nbytes,
                count as usize * 2
            )));
        }
        if body.len() < 2 + nbytes {
            return Err(UpstreamError::Io(format!(
                "short payload: {} bytes, wanted {}",
                body.len(),
                2 + nbytes
            )));
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let hi = body[2 + i * 2];
            let lo = body[3 + i * 2];
            out.push(u16::from_be_bytes([hi, lo]));
        }
        self.stats.upstream_reads.fetch_add(1, Ordering::Relaxed);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(steps: usize) -> Vec<u64> {
        let mut cur = Duration::ZERO;
        let mut out = Vec::new();
        for _ in 0..steps {
            cur = next_backoff(cur);
            out.push(cur.as_secs());
        }
        out
    }

    #[test]
    fn backoff_starts_at_five_seconds_and_doubles_to_the_cap() {
        assert_eq!(schedule(8), vec![5, 10, 20, 40, 80, 160, 300, 300]);
    }

    #[test]
    fn backoff_never_grows_past_five_minutes() {
        assert_eq!(next_backoff(Duration::from_secs(300)), BACKOFF_MAX);
        assert_eq!(next_backoff(Duration::from_secs(299)), BACKOFF_MAX);
        assert_eq!(next_backoff(Duration::from_secs(100_000)), BACKOFF_MAX);
    }

    #[test]
    fn a_successful_read_starts_the_schedule_over() {
        // the reset itself is an assignment in the Ok arm; what can be tested here is
        // that zero means "first failure", not "no wait"
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(5));
    }
}

fn map_io(e: std::io::Error, timeout: Duration) -> UpstreamError {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            UpstreamError::Timeout(format!("timeout after {:.1}s", timeout.as_secs_f64()))
        }
        _ => UpstreamError::Io(e.to_string()),
    }
}
