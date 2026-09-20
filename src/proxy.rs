//! The proxy itself: request dispatch, range validation and the poller.
//!
//! Concurrency model is threads rather than an event loop: a client per thread,
//! one poller thread, one HTTP thread. Every upstream request goes through the
//! single connection guarded by a mutex, so "N clients cost one upstream read"
//! is enforced by construction rather than by careful bookkeeping.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cache::{Cache, HOT_TTL};
use crate::config::{Config, PolicyCfg, RangeCfg};
use crate::json::Json;
use crate::logging;
use crate::modbus::*;
use crate::stats::Stats;
use crate::upstream::{Upstream, UpstreamError};

pub struct Proxy {
    pub cfg: Config,
    pub stats: Arc<Stats>,
    pub up: Arc<Upstream>,
    pub cache: Arc<Cache>,
    poll_ranges: Vec<RangeCfg>,
    max_per_read: u16,
    policy: PolicyCfg,
    range_failures: Mutex<BTreeMap<String, u32>>,
    range_skip_until: Mutex<BTreeMap<String, Instant>>,
    /// Single-flight guard so a cache miss storm costs one upstream read.
    fetch_lock: Mutex<()>,
    pub stop: Arc<AtomicBool>,
}

impl Proxy {
    pub fn new(cfg: Config, stats: Arc<Stats>, stop: Arc<AtomicBool>) -> Proxy {
        let up = Arc::new(Upstream::new(
            &cfg.upstream,
            stats.clone(),
            cfg.poll.min_request_gap,
        ));
        let cache = Arc::new(Cache::new(cfg.poll.ondemand_ttl));
        Proxy {
            poll_ranges: cfg.poll.ranges.clone(),
            max_per_read: cfg.poll.max_registers_per_read,
            policy: cfg.policy.clone(),
            cfg,
            stats,
            up,
            cache,
            range_failures: Mutex::new(BTreeMap::new()),
            range_skip_until: Mutex::new(BTreeMap::new()),
            fetch_lock: Mutex::new(()),
            stop,
        }
    }

    // ------------------------------------------------ range validation

    /// Reject implausible blocks before they reach the cache. The device
    /// intermittently answers with a shifted or stale block instead of an error;
    /// without this the symptom is not a failure but wrong physics.
    pub fn validate(&self, rng: &RangeCfg, regs: &[u16]) -> Option<String> {
        if let Some((want_did, want_len)) = rng.expect_header {
            if regs.len() >= 2 && (regs[0] != want_did || regs[1] != want_len) {
                return Some(format!(
                    "header {:?} expected [{}, {}]",
                    &regs[..2],
                    want_did,
                    want_len
                ));
            }
        }
        for off in &rng.sf_offsets {
            if let Some(raw) = regs.get(*off) {
                let val = as_i16(*raw) as i32;
                if !(-8..=4).contains(&val) {
                    return Some(format!("scale factor at +{} is {}", off, val));
                }
            }
        }
        for off in &rng.sf18_offsets {
            if let Some(raw) = regs.get(*off) {
                // Python compares an integer against -5.5..=4.5, i.e. -5..=4
                let val = as_i16(*raw) as i32;
                if !(-5..=4).contains(&val) {
                    return Some(format!("scale factor at +{} is {}", off, val));
                }
            }
        }
        None
    }

    // ------------------------------------------------------- poller

    fn refresh_range(&self, rng: &RangeCfg) {
        {
            let skip = self.range_skip_until.lock().unwrap();
            if let Some(until) = skip.get(&rng.name) {
                if Instant::now() < *until {
                    return;
                }
            }
        }

        let mut collected: Vec<u16> = Vec::with_capacity(rng.count as usize);
        let mut cur = rng.address;
        let mut remaining = rng.count;
        let mut failure: Option<String> = None;

        while remaining > 0 {
            let n = remaining.min(self.max_per_read);
            match self.up.read_registers(cur, n, FC_READ_HOLDING) {
                Ok(regs) => {
                    collected.extend_from_slice(&regs);
                    cur = cur.wrapping_add(n);
                    remaining -= n;
                }
                Err(e) => {
                    failure = Some(format!("{}", e));
                    break;
                }
            }
        }
        let _ = &collected;

        if failure.is_none() {
            if let Some(problem) = self.validate(rng, &collected) {
                // never let a corrupted block overwrite good cached data
                self.stats
                    .validation_failures
                    .fetch_add(1, Ordering::Relaxed);
                failure = Some(format!("validation failed: {}", problem));
            }
        }

        if failure.is_none() {
            self.cache
                .put(rng.address, collected, HOT_TTL, true, FC_READ_HOLDING);
            self.range_failures
                .lock()
                .unwrap()
                .insert(rng.name.clone(), 0);
            self.stats.backoff_ranges.lock().unwrap().remove(&rng.name);
            return;
        }

        let msg = failure.unwrap();
        let fails = {
            let mut f = self.range_failures.lock().unwrap();
            let e = f.entry(rng.name.clone()).or_insert(0);
            *e += 1;
            *e
        };
        self.stats.poll_failures.fetch_add(1, Ordering::Relaxed);
        // 30s, doubling per consecutive failure, capped at 15 min
        let factor = 2u32.saturating_pow((fails - 1).min(5));
        let skip = Duration::from_secs((30 * factor).min(900) as u64);
        self.range_skip_until
            .lock()
            .unwrap()
            .insert(rng.name.clone(), Instant::now() + skip);
        self.stats
            .backoff_ranges
            .lock()
            .unwrap()
            .insert(rng.name.clone(), fails);
        logging::warn(format!(
            "poll range {} ({} regs @{}) failed {}x: {} - skipping {:.0}s",
            rng.name,
            rng.count,
            rng.address,
            fails,
            msg,
            skip.as_secs_f64()
        ));
    }

    pub fn poll_loop(&self) {
        let delay = self.cfg.poll.startup_delay;
        if self.sleep_interruptible(delay) {
            return;
        }
        while !self.stop.load(Ordering::Relaxed) {
            let clients = self.stats.clients_active.load(Ordering::Relaxed);
            let interval = if clients > 0 {
                self.cfg.poll.interval_active
            } else {
                self.cfg.poll.interval_idle
            };
            let started = Instant::now();
            self.stats
                .last_poll_started_ms
                .store(crate::stats::now_ms(), Ordering::Relaxed);
            self.stats.poll_cycles.fetch_add(1, Ordering::Relaxed);
            for rng in &self.poll_ranges {
                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
                self.refresh_range(rng);
            }
            self.stats
                .last_poll_finished_ms
                .store(crate::stats::now_ms(), Ordering::Relaxed);
            let target = started + interval.max(Duration::from_millis(500));
            let now = Instant::now();
            let wait = target.saturating_duration_since(now);
            if self.sleep_interruptible(wait) {
                return;
            }
        }
    }

    /// Sleep, but wake immediately when a stop is requested. Returns true if stopped.
    fn sleep_interruptible(&self, dur: Duration) -> bool {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if self.stop.load(Ordering::Relaxed) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
        self.stop.load(Ordering::Relaxed)
    }

    /// /metrics text: the counters plus the cache gauges, so this endpoint
    /// reports the same key set the Python version does.
    pub fn metrics_text(&self) -> String {
        let mut s = self.stats.metrics_text();
        s.push_str(&format!("cache_chunks {}\n", self.cache.len()));
        s.push_str(&format!(
            "cache_registers {}\n",
            self.cache.register_count()
        ));
        s
    }

    /// /ranges payload: what the poller keeps hot, including the validation
    /// expectations, so a reader can see exactly what is being asserted.
    pub fn ranges_json(&self) -> Json {
        Json::Arr(
            self.poll_ranges
                .iter()
                .map(|r| {
                    let mut m = BTreeMap::new();
                    m.insert("name".into(), Json::Str(r.name.clone()));
                    m.insert("address".into(), Json::Num(r.address as f64));
                    m.insert("count".into(), Json::Num(r.count as f64));
                    if let Some((did, len)) = r.expect_header {
                        m.insert(
                            "expect_header".into(),
                            Json::Arr(vec![Json::Num(did as f64), Json::Num(len as f64)]),
                        );
                    }
                    if !r.sf_offsets.is_empty() {
                        m.insert(
                            "sf_offsets".into(),
                            Json::Arr(r.sf_offsets.iter().map(|o| Json::Num(*o as f64)).collect()),
                        );
                    }
                    if !r.sf18_offsets.is_empty() {
                        m.insert(
                            "sf18_offsets".into(),
                            Json::Arr(
                                r.sf18_offsets
                                    .iter()
                                    .map(|o| Json::Num(*o as f64))
                                    .collect(),
                            ),
                        );
                    }
                    Json::Obj(m)
                })
                .collect(),
        )
    }

    // ------------------------------------------------------- serving

    fn serve_read(&self, addr: u16, count: u16, fc: u8) -> Result<Vec<u16>, UpstreamError> {
        if let Some(hit) = self.cache.get(addr, count, fc) {
            self.stats
                .requests_cache_hit
                .fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        self.stats
            .requests_cache_miss
            .fetch_add(1, Ordering::Relaxed);
        let _guard = self.fetch_lock.lock().unwrap();
        // another thread may have filled it while we waited
        if let Some(hit) = self.cache.get(addr, count, fc) {
            return Ok(hit);
        }
        let regs = self.up.read_registers(addr, count, fc)?;
        self.cache
            .put(addr, regs.clone(), self.cache.ttl_ondemand, true, fc);
        Ok(regs)
    }

    pub fn dispatch(&self, pdu: &[u8], peer: &str) -> Vec<u8> {
        self.stats.requests_total.fetch_add(1, Ordering::Relaxed);
        if pdu.is_empty() {
            return exc_pdu(0, EXC_ILLEGAL_FUNCTION);
        }
        let fc = pdu[0];
        if self.policy.log_every_request {
            logging::debug(format!("pdu from {}: {}", peer, hex(pdu)));
        }

        if (fc == FC_READ_HOLDING || fc == FC_READ_INPUT) && pdu.len() >= 5 {
            let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
            let count = u16::from_be_bytes([pdu[3], pdu[4]]);
            if count < 1 || count > 125 {
                return exc_pdu(fc, EXC_ILLEGAL_ADDRESS);
            }
            match self.serve_read(addr, count, fc) {
                Ok(regs) => return read_response(fc, &regs),
                Err(UpstreamError::Modbus(code)) => {
                    // the device answered - relay its own exception verbatim
                    logging::info(format!(
                        "device exception 0x{:02x} for read {}@{}",
                        code, count, addr
                    ));
                    return exc_pdu(fc, code);
                }
                Err(e) => {
                    if e.is_timeout() {
                        logging::warn(format!("read {}@{} timed out", count, addr));
                    } else {
                        logging::warn(format!("read {}@{} failed: {}", count, addr, e));
                    }
                    // No reading, so no answer: pass the failure on instead of serving an
                    // expired cached frame. A client cannot tell such a frame from a real
                    // reading (the read *succeeded* as far as it can see), so a leftover
                    // would let a controller act on arbitrarily old values - the proxy
                    // would be lying about the site.
                    return exc_pdu(fc, EXC_GATEWAY_FAIL);
                }
            }
        }

        if (fc == FC_READ_COILS || fc == FC_READ_DISCRETE) && pdu.len() >= 5 {
            let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
            let count = u16::from_be_bytes([pdu[3], pdu[4]]);
            match self.serve_read(addr, count, fc) {
                Ok(regs) => return bits_response(fc, &regs, count as usize),
                Err(e) => {
                    logging::warn(format!("coil read {}@{} failed: {}", count, addr, e));
                    return exc_pdu(fc, EXC_GATEWAY_FAIL);
                }
            }
        }

        if is_write_fc(fc) && pdu.len() >= 5 {
            if !self.policy.allow_writes {
                logging::warn(format!("write rejected by policy from {}", peer));
                return exc_pdu(fc, EXC_ILLEGAL_FUNCTION);
            }
            match self.up.request(pdu, true) {
                Ok(resp) => {
                    self.stats.upstream_writes.fetch_add(1, Ordering::Relaxed);
                    if fc == FC_WRITE_REGISTER && pdu.len() >= 5 {
                        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
                        let val = u16::from_be_bytes([pdu[3], pdu[4]]);
                        self.cache.invalidate(addr, addr as u32, None);
                        logging::info(format!("write single {} = {} from {}", addr, val, peer));
                    } else if fc == FC_WRITE_REGISTERS && pdu.len() >= 5 {
                        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
                        let count = u16::from_be_bytes([pdu[3], pdu[4]]);
                        self.cache
                            .invalidate(addr, addr as u32 + count as u32 - 1, None);
                        logging::info(format!(
                            "write multiple {} regs @{} from {}",
                            count, addr, peer
                        ));
                    } else {
                        logging::info(format!("write fc={} from {}", fc, peer));
                    }
                    if !resp.is_empty() && resp[0] & 0x80 != 0 {
                        logging::warn(format!(
                            "upstream rejected write fc={}: exc 0x{:02x}",
                            fc,
                            resp.get(1).copied().unwrap_or(0)
                        ));
                    }
                    return resp;
                }
                Err(e) => {
                    logging::warn(format!("write fc={} failed: {}", fc, e));
                    return exc_pdu(fc, EXC_GATEWAY_FAIL);
                }
            }
        }

        if self.policy.reject_unsupported_functions {
            return exc_pdu(fc, EXC_ILLEGAL_FUNCTION);
        }
        match self.up.request(pdu, true) {
            Ok(resp) => resp,
            Err(e) => {
                logging::warn(format!("passthrough fc={} failed: {}", fc, e));
                exc_pdu(fc, EXC_GATEWAY_FAIL)
            }
        }
    }

    /// Serve one downstream connection until it closes.
    pub fn handle_client(&self, mut stream: TcpStream) {
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".to_string());
        self.stats.clients_total.fetch_add(1, Ordering::Relaxed);
        let active = self.stats.clients_active.fetch_add(1, Ordering::Relaxed) + 1;
        logging::info(format!("client connected {} (active={})", peer, active));

        let mut hdr = [0u8; MBAP_LEN];
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            if let Err(e) = stream.read_exact(&mut hdr) {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    logging::debug(format!("client {} read: {}", peer, e));
                }
                break;
            }
            let h = match parse_mbap(&hdr) {
                Some(h) => h,
                None => break,
            };
            if h.length < 2 || h.length > 260 {
                logging::warn(format!("bad MBAP length {} from {}", h.length, peer));
                break;
            }
            let mut pdu = vec![0u8; h.length as usize - 1];
            if stream.read_exact(&mut pdu).is_err() {
                break;
            }
            let started = Instant::now();
            let resp = self.dispatch(&pdu, &peer);
            if !resp.is_empty() {
                let frame = mbap(h.tid, h.pid, h.uid, &resp);
                let wrote = stream
                    .write_all(&frame)
                    .and_then(|_| stream.flush())
                    .is_ok();
                if self.policy.log_every_request {
                    logging::debug(format!(
                        "reply to {}: unit={} tid={} req={} -> resp={} in {}ms{}",
                        peer,
                        h.uid,
                        h.tid,
                        hex(&pdu),
                        hex(&resp),
                        started.elapsed().as_millis(),
                        if wrote { "" } else { " (WRITE FAILED)" }
                    ));
                }
                if !wrote {
                    break;
                }
            } else if self.policy.log_every_request {
                logging::debug(format!(
                    "no reply to {}: unit={} tid={} req={} ({}ms)",
                    peer,
                    h.uid,
                    h.tid,
                    hex(&pdu),
                    started.elapsed().as_millis()
                ));
            }
        }

        let active = self.stats.clients_active.fetch_sub(1, Ordering::Relaxed) - 1;
        logging::info(format!("client disconnected {} (active={})", peer, active));
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{:02x}", x))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::json::Json;

    fn test_proxy(policy: &str, ranges: &str) -> Proxy {
        let raw = format!(
            r#"{{"upstream":{{"host":"127.0.0.1","port":1}},"listen":{{"port":0}},
                "poll":{{"ranges":{}}},"policy":{}}}"#,
            ranges, policy
        );
        let cfg = Config::from_json(&Json::parse(&raw).unwrap()).unwrap();
        Proxy::new(
            cfg,
            Arc::new(Stats::new()),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn rejects_a_bad_sunspec_header() {
        let p = test_proxy(
            "{}",
            r#"[{"name":"m","address":0,"count":4,"expect_header":[203,105]}]"#,
        );
        let rng = &p.poll_ranges[0];
        assert!(p.validate(rng, &[203, 105, 1, 2]).is_none());
        let bad = p.validate(rng, &[0, 0, 1, 2]).unwrap();
        assert!(bad.contains("header"), "{}", bad);
    }

    #[test]
    fn rejects_a_garbage_scale_factor() {
        let p = test_proxy(
            "{}",
            r#"[{"name":"m","address":0,"count":4,"sf_offsets":[2]}]"#,
        );
        let rng = &p.poll_ranges[0];
        assert!(p.validate(rng, &[0, 0, 65534, 0]).is_none()); // -2 is fine
        assert!(p.validate(rng, &[0, 0, 1234, 0]).is_some()); // a live value is not
    }

    #[test]
    fn rejects_a_move_of_the_word_order() {
        // the shifted-block symptom: a voltage lands in a scale-factor slot
        let p = test_proxy(
            "{}",
            r#"[{"name":"m","address":0,"count":4,"sf18_offsets":[1]}]"#,
        );
        let rng = &p.poll_ranges[0];
        assert!(p.validate(rng, &[0, 65533, 0, 0]).is_none()); // -3
        assert!(p.validate(rng, &[0, 2305, 0, 0]).is_some()); // 230.5 V
    }

    #[test]
    fn write_policy_blocks_writes_with_illegal_function() {
        let p = test_proxy(r#"{"allow_writes":false}"#, "[]");
        let pdu = [FC_WRITE_REGISTER, 0, 5, 0x04, 0xD2];
        let resp = p.dispatch(&pdu, "test");
        assert_eq!(resp, vec![0x86, EXC_ILLEGAL_FUNCTION]);
    }

    #[test]
    fn an_out_of_range_read_count_is_an_illegal_address() {
        let p = test_proxy("{}", "[]");
        // count 0 and 126 are both rejected before any upstream traffic
        assert_eq!(p.dispatch(&[3, 0, 0, 0, 0], "t"), vec![0x83, 0x02]);
        assert_eq!(p.dispatch(&[3, 0, 0, 0, 126], "t"), vec![0x83, 0x02]);
    }

    #[test]
    fn unsupported_function_is_rejected_when_policy_says_so() {
        let p = test_proxy(r#"{"reject_unsupported_functions":true}"#, "[]");
        let resp = p.dispatch(&[0x41, 0x0E], "t");
        assert_eq!(resp, vec![0xC1, EXC_ILLEGAL_FUNCTION]);
    }
}
