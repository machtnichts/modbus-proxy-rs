//! Shared counters. Same names as the Python implementation so the existing
//! /metrics consumers and tests keep working across the two.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::json::Json;

pub struct Stats {
    pub started_at: Instant,
    pub started_wall: u64,
    pub clients_total: AtomicU64,
    pub clients_active: AtomicI64,
    pub requests_total: AtomicU64,
    pub requests_cache_hit: AtomicU64,
    pub requests_cache_miss: AtomicU64,
    pub upstream_reads: AtomicU64,
    pub upstream_writes: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub upstream_reconnects: AtomicU64,
    /// Current retry backoff in ms. Non-zero means the proxy is deliberately leaving
    /// the device alone after a failed read - the honest version of "is it calm?".
    pub upstream_backoff_ms: AtomicU64,
    pub upstream_timeouts: AtomicU64,
    pub poll_cycles: AtomicU64,
    pub poll_failures: AtomicU64,
    pub validation_failures: AtomicU64,
    pub last_upstream_ok_ms: AtomicU64,
    /// Wall-clock ms of the last upstream error, so a UI can say "15 min ago" instead of
    /// showing a bare message with no idea whether it is one minute or one day old.
    pub last_upstream_error_ms: AtomicU64,
    pub last_poll_started_ms: AtomicU64,
    pub last_poll_finished_ms: AtomicU64,
    pub last_upstream_error: Mutex<String>,
    pub backoff_ranges: Mutex<BTreeMap<String, u32>>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Stats {
    pub fn new() -> Stats {
        Stats {
            started_at: Instant::now(),
            started_wall: now_ms(),
            clients_total: AtomicU64::new(0),
            clients_active: AtomicI64::new(0),
            requests_total: AtomicU64::new(0),
            requests_cache_hit: AtomicU64::new(0),
            requests_cache_miss: AtomicU64::new(0),
            upstream_reads: AtomicU64::new(0),
            upstream_writes: AtomicU64::new(0),
            upstream_errors: AtomicU64::new(0),
            upstream_reconnects: AtomicU64::new(0),
            upstream_backoff_ms: AtomicU64::new(0),
            upstream_timeouts: AtomicU64::new(0),
            poll_cycles: AtomicU64::new(0),
            poll_failures: AtomicU64::new(0),
            validation_failures: AtomicU64::new(0),
            last_upstream_ok_ms: AtomicU64::new(0),
            last_upstream_error_ms: AtomicU64::new(0),
            last_poll_started_ms: AtomicU64::new(0),
            last_poll_finished_ms: AtomicU64::new(0),
            last_upstream_error: Mutex::new(String::new()),
            backoff_ranges: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn uptime_s(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    pub fn mark_upstream_ok(&self) {
        self.last_upstream_ok_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn note_upstream_error(&self, msg: String) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
        self.last_upstream_error_ms
            .store(now_ms(), Ordering::Relaxed);
        *self.last_upstream_error.lock().unwrap() = msg;
    }

    /// Numeric view, keyed like the Python stats dict.
    pub fn numeric(&self) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = vec![
            ("started_at".into(), (self.started_wall as f64) / 1000.0),
            ("uptime_s".into(), self.uptime_s()),
            (
                "clients_total".into(),
                self.clients_total.load(Ordering::Relaxed) as f64,
            ),
            (
                "clients_active".into(),
                self.clients_active.load(Ordering::Relaxed) as f64,
            ),
            (
                "requests_total".into(),
                self.requests_total.load(Ordering::Relaxed) as f64,
            ),
            (
                "requests_cache_hit".into(),
                self.requests_cache_hit.load(Ordering::Relaxed) as f64,
            ),
            (
                "requests_cache_miss".into(),
                self.requests_cache_miss.load(Ordering::Relaxed) as f64,
            ),
            (
                "upstream_reads".into(),
                self.upstream_reads.load(Ordering::Relaxed) as f64,
            ),
            (
                "upstream_writes".into(),
                self.upstream_writes.load(Ordering::Relaxed) as f64,
            ),
            (
                "upstream_errors".into(),
                self.upstream_errors.load(Ordering::Relaxed) as f64,
            ),
            (
                "upstream_reconnects".into(),
                self.upstream_reconnects.load(Ordering::Relaxed) as f64,
            ),
            (
                "upstream_backoff_s".into(),
                (self.upstream_backoff_ms.load(Ordering::Relaxed) as f64) / 1000.0,
            ),
            (
                "upstream_timeouts".into(),
                self.upstream_timeouts.load(Ordering::Relaxed) as f64,
            ),
            (
                "poll_cycles".into(),
                self.poll_cycles.load(Ordering::Relaxed) as f64,
            ),
            (
                "poll_failures".into(),
                self.poll_failures.load(Ordering::Relaxed) as f64,
            ),
            (
                "validation_failures".into(),
                self.validation_failures.load(Ordering::Relaxed) as f64,
            ),
        ];
        let ok = self.last_upstream_ok_ms.load(Ordering::Relaxed);
        if ok > 0 {
            v.push(("last_upstream_ok".into(), (ok as f64) / 1000.0));
        }
        let le = self.last_upstream_error_ms.load(Ordering::Relaxed);
        if le > 0 {
            v.push(("last_upstream_error_at".into(), (le as f64) / 1000.0));
        }
        let ps = self.last_poll_started_ms.load(Ordering::Relaxed);
        if ps > 0 {
            v.push(("last_poll_started".into(), (ps as f64) / 1000.0));
        }
        let pf = self.last_poll_finished_ms.load(Ordering::Relaxed);
        if pf > 0 {
            v.push(("last_poll_finished".into(), (pf as f64) / 1000.0));
        }
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// `key value` lines, the format the Python proxy's /metrics emits.
    pub fn metrics_text(&self) -> String {
        let mut out = String::new();
        for (k, v) in self.numeric() {
            if v.fract() == 0.0 && v.abs() < 1e15 {
                out.push_str(&format!("{} {}\n", k, v as i64));
            } else {
                out.push_str(&format!("{} {}\n", k, v));
            }
        }
        out
    }

    pub fn as_json(&self) -> Json {
        let mut m = BTreeMap::new();
        for (k, v) in self.numeric() {
            m.insert(k, Json::Num(v));
        }
        let err = self.last_upstream_error.lock().unwrap().clone();
        m.insert("last_upstream_error".into(), Json::Str(err));
        let mut backoff = BTreeMap::new();
        for (k, v) in self.backoff_ranges.lock().unwrap().iter() {
            backoff.insert(k.clone(), Json::Num(*v as f64));
        }
        m.insert("backoff_ranges".into(), Json::Obj(backoff));
        Json::Obj(m)
    }

    /// True if the upstream answered within the last `window_s` seconds.
    pub fn upstream_recently_ok(&self, window_s: u64) -> bool {
        let ok = self.last_upstream_ok_ms.load(Ordering::Relaxed);
        ok > 0 && now_ms().saturating_sub(ok) < window_s * 1000
    }
}
