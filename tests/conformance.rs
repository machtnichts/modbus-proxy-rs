//! Wire-level conformance suite for the Rust proxy.
//!
//! These are the same behaviours the Python implementation had to satisfy,
//! checked on the raw bytes: framing, cache amplification, on-demand fetch,
//! exception relay, write passthrough and invalidation, policy gating and the
//! single-upstream-connection guarantee.
//!
//! Everything runs against `stubmodbus` on loopback - no test ever connects to
//! the real inverter, which must keep exactly one client at a time.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROXY_BIN: &str = env!("CARGO_BIN_EXE_muxproxy");
const STUB_BIN: &str = env!("CARGO_BIN_EXE_stubmodbus");

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port(port: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{} never came up on port {}", what, port);
}

struct Harness {
    stub: Child,
    proxy: Child,
    proxy_port: u16,
    http_port: u16,
    dir: std::path::PathBuf,
}

impl Harness {
    fn start(ranges: &str, policy: &str) -> Harness {
        Self::start_with_ttl(ranges, policy, 5.0)
    }

    fn start_with_ttl(ranges: &str, policy: &str, ttl: f64) -> Harness {
        let stub_port = free_port();
        let proxy_port = free_port();
        let http_port = free_port();
        let dir = std::env::temp_dir().join(format!("muxproxy-test-{}", free_port()));

        let stub = Command::new(STUB_BIN)
            .args(["--port", &stub_port.to_string(), "--forbidden", "50-52"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn stub");
        wait_for_port(stub_port, "stub");

        let config = format!(
            r#"{{
  "listen": {{"host": "127.0.0.1", "port": {proxy_port}}},
  "upstream": {{"host": "127.0.0.1", "port": {stub_port}, "unit": 1,
               "connect_timeout": 5.0, "response_timeout": 5.0}},
  "http": {{"host": "127.0.0.1", "port": {http_port}}},
  "poll": {{"interval_active": 30.0, "interval_idle": 120.0, "min_request_gap": 0.01,
           "ondemand_ttl": {ttl}, "max_registers_per_read": 100, "startup_delay": 0.5,
           "ranges": {ranges}}},
  "policy": {policy},
  "logging": {{"level": "WARNING", "file": null, "max_bytes": 1000000, "backup_count": 2}}
}}"#
        );
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.json");
        std::fs::write(&cfg_path, config).unwrap();

        let proxy = Command::new(PROXY_BIN)
            .args(["-c", cfg_path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn proxy");
        wait_for_port(proxy_port, "proxy");
        wait_for_port(http_port, "http");

        Harness {
            stub,
            proxy,
            proxy_port,
            http_port,
            dir,
        }
    }

    fn client(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.proxy_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    /// One Modbus exchange, returning the response PDU.
    fn exchange(stream: &mut TcpStream, pdu: &[u8], unit: u8) -> Vec<u8> {
        let tid: u16 = 0x1234;
        let mut frame = Vec::new();
        frame.extend_from_slice(&tid.to_be_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes());
        frame.extend_from_slice(&((pdu.len() + 1) as u16).to_be_bytes());
        frame.push(unit);
        frame.extend_from_slice(pdu);
        stream.write_all(&frame).unwrap();
        stream.flush().unwrap();

        let mut hdr = [0u8; 7];
        stream.read_exact(&mut hdr).unwrap();
        assert_eq!(
            u16::from_be_bytes([hdr[0], hdr[1]]),
            tid,
            "transaction id must be echoed"
        );
        assert_eq!(hdr[6], unit, "unit id must be echoed");
        let len = u16::from_be_bytes([hdr[4], hdr[5]]);
        let mut body = vec![0u8; len as usize - 1];
        stream.read_exact(&mut body).unwrap();
        body
    }

    fn metrics(&self) -> Vec<(String, f64)> {
        let body = self.http_get("/metrics");
        body.lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let k = it.next()?.to_string();
                let v: f64 = it.next()?.parse().ok()?;
                Some((k, v))
            })
            .collect()
    }

    fn metric(&self, key: &str) -> f64 {
        self.metrics()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("metric {} missing", key))
    }

    fn http_get(&self, path: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.http_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("GET {} HTTP/1.1\r\nHost: x\r\n\r\n", path).as_bytes())
            .unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).to_string();
        // strip headers
        match text.find("\r\n\r\n") {
            Some(i) => text[i + 4..].to_string(),
            None => text,
        }
    }

    /// Wait until the poller has actually filled the cache.
    fn wait_for_cache(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let body = self.http_get("/cache");
            if body.trim_start().starts_with('[') && body.contains("\"count\"") {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("cache never filled");
    }

    fn stop(&mut self) -> String {
        let _ = self.proxy.kill();
        let _ = self.stub.kill();
        let _ = self.proxy.wait();
        // the stub reports its own connection count on stdout
        let mut out = String::new();
        if let Some(mut so) = self.stub.stdout.take() {
            let _ = so.read_to_string(&mut out);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        out
    }
}

fn read_pdu(addr: u16, count: u16) -> Vec<u8> {
    let mut p = vec![3u8];
    p.extend_from_slice(&addr.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    p
}

fn regs_from(pdu: &[u8]) -> Vec<u16> {
    assert_eq!(pdu[0], 3, "expected a read-holding response, got {:?}", pdu);
    let bc = pdu[1] as usize;
    assert_eq!(bc, pdu.len() - 2, "byte count must match the payload");
    pdu[2..]
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn framing_values_and_unit_passthrough() {
    let mut h = Harness::start(r#"[]"#, r#"{"allow_writes": true}"#);
    let mut c = h.client();
    let pdu = Harness::exchange(&mut c, &read_pdu(10, 4), 7);
    assert_eq!(
        regs_from(&pdu),
        vec![100, 110, 120, 130],
        "device values are reg[i] = i * 10"
    );
    let out = h.stop();
    assert!(
        out.contains("CONNECTION"),
        "stub should have logged a connection"
    );
}

#[test]
fn repeat_reads_are_served_from_cache() {
    let mut h = Harness::start(r#"[]"#, r#"{"allow_writes": true}"#);
    let mut c = h.client();
    let first = regs_from(&Harness::exchange(&mut c, &read_pdu(100, 4), 1));
    let before = h.metric("upstream_reads");
    for _ in 0..5 {
        let again = regs_from(&Harness::exchange(&mut c, &read_pdu(100, 4), 1));
        assert_eq!(again, first, "cached reads must stay identical");
    }
    let after = h.metric("upstream_reads");
    assert_eq!(
        before, after,
        "five repeat reads must not produce a single extra upstream read"
    );
    assert!(
        h.metric("requests_cache_hit") >= 5.0,
        "repeat reads should count as cache hits"
    );
    h.stop();
}

#[test]
fn polled_ranges_come_back_without_touching_the_device() {
    let mut h = Harness::start(
        r#"[{"name": "meter", "address": 0, "count": 8}]"#,
        r#"{"allow_writes": true}"#,
    );
    h.wait_for_cache();
    let before = h.metric("upstream_reads");
    let mut c = h.client();
    let vals = regs_from(&Harness::exchange(&mut c, &read_pdu(0, 8), 1));
    assert_eq!(vals, vec![0, 10, 20, 30, 40, 50, 60, 70]);
    assert_eq!(
        h.metric("upstream_reads"),
        before,
        "a polled range must be answered from cache"
    );
    h.stop();
}

#[test]
fn an_unpolled_range_is_fetched_on_demand_inside_the_polled_block() {
    // second range avoids the forbidden 50-52 so the poller stays healthy
    let mut h = Harness::start(
        r#"[{"name": "low", "address": 0, "count": 10}, {"name": "high", "address": 200, "count": 4}]"#,
        r#"{"allow_writes": true}"#,
    );
    h.wait_for_cache();
    let before = h.metric("upstream_reads");
    let mut c = h.client();
    let vals = regs_from(&Harness::exchange(&mut c, &read_pdu(60, 3), 1));
    assert_eq!(
        vals,
        vec![600, 610, 620],
        "on-demand fetch returns device values"
    );
    assert!(
        h.metric("upstream_reads") > before,
        "an unpolled range must trigger an upstream read"
    );
    h.stop();
}

#[test]
fn device_exception_is_relayed_verbatim() {
    let mut h = Harness::start(r#"[]"#, r#"{"allow_writes": true}"#);
    let mut c = h.client();
    let pdu = Harness::exchange(&mut c, &read_pdu(50, 2), 1);
    assert_eq!(
        pdu,
        vec![0x83, 0x02],
        "the device's illegal-address exception must reach the client unchanged"
    );
    h.stop();
}

#[test]
fn an_out_of_range_count_is_refused_without_bothering_the_device() {
    let mut h = Harness::start(r#"[]"#, r#"{"allow_writes": true}"#);
    let mut c = h.client();
    let before = h.metric("upstream_reads");
    let pdu = Harness::exchange(&mut c, &read_pdu(0, 200), 1);
    assert_eq!(pdu, vec![0x83, 0x02], "count > 125 is illegal");
    assert_eq!(
        h.metric("upstream_reads"),
        before,
        "the proxy must reject it itself, not forward it"
    );
    h.stop();
}

#[test]
fn write_single_passes_through_and_invalidates_the_cache() {
    let mut h = Harness::start(
        r#"[{"name": "cfg", "address": 0, "count": 4}]"#,
        r#"{"allow_writes": true}"#,
    );
    h.wait_for_cache();
    let mut c = h.client();
    assert_eq!(
        regs_from(&Harness::exchange(&mut c, &read_pdu(0, 2), 1)),
        vec![0, 10]
    );

    // FC6 write single register: addr 0 = 1234
    let mut w = vec![6u8];
    w.extend_from_slice(&0u16.to_be_bytes());
    w.extend_from_slice(&1234u16.to_be_bytes());
    let echo = Harness::exchange(&mut c, &w, 1);
    assert_eq!(echo, w, "a write response echoes the request");

    let after = regs_from(&Harness::exchange(&mut c, &read_pdu(0, 2), 1));
    assert_eq!(
        after,
        vec![1234, 10],
        "the write must invalidate the cached block, not be masked by it"
    );
    h.stop();
}

#[test]
fn write_multiple_passes_through() {
    let mut h = Harness::start(r#"[]"#, r#"{"allow_writes": true}"#);
    let mut c = h.client();
    let mut w = vec![16u8];
    w.extend_from_slice(&10u16.to_be_bytes());
    w.extend_from_slice(&2u16.to_be_bytes());
    w.push(4);
    w.extend_from_slice(&777u16.to_be_bytes());
    w.extend_from_slice(&888u16.to_be_bytes());
    let echo = Harness::exchange(&mut c, &w, 1);
    assert_eq!(echo[0], 16);
    assert_eq!(&echo[1..5], &w[1..5], "address and count are echoed");
    assert_eq!(
        regs_from(&Harness::exchange(&mut c, &read_pdu(10, 2), 1)),
        vec![777, 888]
    );
    h.stop();
}

#[test]
fn writes_are_refused_when_policy_forbids_them() {
    let mut h = Harness::start(
        r#"[]"#,
        r#"{"allow_writes": false, "log_every_request": false}"#,
    );
    let mut c = h.client();
    let mut w = vec![6u8];
    w.extend_from_slice(&0u16.to_be_bytes());
    w.extend_from_slice(&1234u16.to_be_bytes());
    let pdu = Harness::exchange(&mut c, &w, 1);
    assert_eq!(
        pdu,
        vec![0x86, 0x01],
        "a blocked write is an illegal-function exception"
    );
    assert_eq!(
        regs_from(&Harness::exchange(&mut c, &read_pdu(0, 1), 1)),
        vec![0],
        "and it must not have reached the device"
    );
    h.stop();
}

#[test]
fn unsupported_functions_follow_the_policy() {
    let mut h = Harness::start(
        r#"[]"#,
        r#"{"allow_writes": true, "reject_unsupported_functions": true}"#,
    );
    let mut c = h.client();
    let pdu = Harness::exchange(&mut c, &[0x11, 0x00, 0x00, 0x00, 0x01], 1);
    assert_eq!(pdu, vec![0x91, 0x01]);
    h.stop();
}

#[test]
fn many_clients_share_one_upstream_connection() {
    let mut h = Harness::start(
        r#"[{"name": "meter", "address": 0, "count": 16}]"#,
        r#"{"allow_writes": true}"#,
    );
    h.wait_for_cache();

    // six concurrent readers, each doing repeated reads
    let port = h.proxy_port;
    let mut handles = Vec::new();
    for n in 0..6u16 {
        handles.push(std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            for _ in 0..5 {
                let addr = n % 8;
                let vals = regs_from(&Harness::exchange(&mut s, &read_pdu(addr, 2), 1));
                assert_eq!(vals, vec![addr * 10, addr * 10 + 10]);
            }
        }));
    }
    for hnd in handles {
        hnd.join().unwrap();
    }

    assert_eq!(
        h.metric("upstream_reconnects"),
        0.0,
        "the proxy must not have reconnected upstream"
    );
    let out = h.stop();
    let connections = out.matches("TRAFFIC").count();
    assert_eq!(
        connections, 1,
        "the device must have carried traffic over exactly one connection, saw {}:\n{}",
        connections, out
    );
}

/// The TTL is the promise: inside it the cache answers, after it the value is gone and a
/// failed read has to be reported as a failure. Serving the expired frame would look like
/// a healthy reading on the wire (the client's read *succeeded*), which is how a controller
/// ends up acting on values from hours ago - the case the app-side staleness gate cannot
/// see through.
#[test]
fn an_expired_read_is_reported_instead_of_served_stale() {
    let mut h = Harness::start_with_ttl(r#"[]"#, r#"{"allow_writes": true}"#, 2.0);
    let mut c = h.client();
    // outwait the proxy's own 5 s upstream timeout: the honest answer may need it
    c.set_read_timeout(Some(Duration::from_secs(12))).unwrap();
    let fresh = regs_from(&Harness::exchange(&mut c, &read_pdu(40000, 4), 1));

    // upstream gone, but the chunk is still inside its TTL -> the cache may answer
    let _ = h.stub.kill();
    let still_fresh = regs_from(&Harness::exchange(&mut c, &read_pdu(40000, 4), 1));
    assert_eq!(still_fresh, fresh, "inside the TTL the cached span is served");

    // past the TTL there is nothing left to serve: the failure must be visible
    std::thread::sleep(Duration::from_millis(2600));
    let expired = Harness::exchange(&mut c, &read_pdu(40000, 4), 1);
    assert_eq!(expired[0], 0x83, "expected an exception PDU, got {:?}", expired);
    assert_eq!(expired[1], 0x0b, "gateway failure is the honest answer");
    assert!(
        expired.len() < 3 + fresh.len() * 2,
        "an expired frame must not be handed out as a reading"
    );

    // No h.stop() here: it would wait on the already-killed stub's stdout. The proxy is
    // killed instead, which is all this harness still owns.
    let _ = h.proxy.kill();
}
