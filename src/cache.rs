//! Register cache built from independently expiring chunks.
//!
//! Whole blocks are cached, never individual registers: a client that pairs a
//! value with a scale factor read at a different moment mis-scales by 10x with no
//! error anywhere.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::json::Json;
#[cfg(test)]
use crate::modbus::FC_READ_HOLDING;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub start: u16,
    pub regs: Vec<u16>,
    pub expires_at: Instant,
    pub fc: u8,
    pub healthy: bool,
    pub fetched_at: Instant,
}

impl Chunk {
    pub fn end(&self) -> u32 {
        self.start as u32 + self.regs.len() as u32
    }

    pub fn age_s(&self) -> f64 {
        self.fetched_at.elapsed().as_secs_f64()
    }
}

pub struct Cache {
    chunks: Mutex<Vec<Chunk>>,
    pub ttl_ondemand: Duration,
}

impl Cache {
    pub fn new(ttl_ondemand: Duration) -> Cache {
        Cache {
            chunks: Mutex::new(Vec::new()),
            ttl_ondemand,
        }
    }

    pub fn put(&self, start: u16, regs: Vec<u16>, ttl: Duration, healthy: bool, fc: u8) {
        let end = start as u32 + regs.len() as u32 - 1;
        self.invalidate(start, end, Some(fc));
        let now = Instant::now();
        let mut guard = self.chunks.lock().unwrap();
        guard.push(Chunk {
            start,
            regs,
            expires_at: now + ttl,
            fc,
            healthy,
            fetched_at: now,
        });
    }

    /// Drop or trim every chunk overlapping `start..=end` (optionally per fc).
    pub fn invalidate(&self, start: u16, end: u32, fc: Option<u8>) {
        let now = Instant::now();
        let mut guard = self.chunks.lock().unwrap();
        let mut out: Vec<Chunk> = Vec::with_capacity(guard.len() + 1);
        for c in guard.drain(..) {
            if let Some(want) = fc {
                if c.fc != want {
                    out.push(c);
                    continue;
                }
            }
            if c.end() <= start as u32 || (c.start as u32) > end {
                out.push(c);
                continue;
            }
            // keep the part before, and the part after
            if (c.start as u32) < start as u32 {
                let keep = start as u32 - c.start as u32;
                out.push(Chunk {
                    start: c.start,
                    regs: c.regs[..keep as usize].to_vec(),
                    expires_at: c.expires_at,
                    fc: c.fc,
                    healthy: c.healthy,
                    fetched_at: c.fetched_at,
                });
            }
            if c.end() > end + 1 {
                let from = (end + 1 - c.start as u32) as usize;
                out.push(Chunk {
                    start: (end + 1) as u16,
                    regs: c.regs[from..].to_vec(),
                    expires_at: c.expires_at,
                    fc: c.fc,
                    healthy: c.healthy,
                    fetched_at: c.fetched_at,
                });
            }
        }
        *guard = out;
        let _ = now;
    }

    /// Return `count` registers at `start` if one *fresh* chunk covers them.
    ///
    /// An expired chunk is never handed out. The TTL is the promise: after it the value
    /// is gone, and if no new one could be read the client gets the error instead of a
    /// frame that looks healthy but is arbitrarily old. A controller on the other end
    /// cannot tell a served leftover from a real reading - its own freshness check sees a
    /// successful read - so serving stale data is how a car charges on yesterday's sun.
    pub fn get(&self, start: u16, count: u16, fc: u8) -> Option<Vec<u16>> {
        let end = start as u32 + count as u32 - 1;
        let now = Instant::now();
        let guard = self.chunks.lock().unwrap();
        let mut fresh: Vec<&Chunk> = guard
            .iter()
            .filter(|c| c.fc == fc && (c.start as u32) <= start as u32 && c.end() > end
                    && c.expires_at > now)
            .collect();
        if fresh.is_empty() {
            return None;
        }
        // prefer the freshest, widest chunk
        fresh.sort_by(|a, b| (a.expires_at, a.regs.len()).cmp(&(b.expires_at, b.regs.len())));
        let best = fresh.last().unwrap();
        let off = (start as u32 - best.start as u32) as usize;
        Some(best.regs[off..off + count as usize].to_vec())
    }

    pub fn len(&self) -> usize {
        self.chunks.lock().unwrap().len()
    }

    /// Kept for symmetry with the other levels; the poller reports range
    /// trouble as a warning, which is what the Python version does too.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// For the /cache endpoint.
    pub fn summary(&self) -> Json {
        let now = Instant::now();
        let guard = self.chunks.lock().unwrap();
        let mut items: Vec<&Chunk> = guard.iter().collect();
        items.sort_by_key(|c| c.start);
        Json::Arr(
            items
                .iter()
                .map(|c| {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("start".to_string(), Json::Num(c.start as f64));
                    m.insert("count".to_string(), Json::Num(c.regs.len() as f64));
                    m.insert("fc".to_string(), Json::Num(c.fc as f64));
                    m.insert(
                        "age_s".to_string(),
                        Json::Num((c.age_s() * 10.0).round() / 10.0),
                    );
                    m.insert("fresh".to_string(), Json::Bool(c.expires_at > now));
                    m.insert("healthy".to_string(), Json::Bool(c.healthy));
                    Json::Obj(m)
                })
                .collect(),
        )
    }

    /// Total registers currently cached (diagnostics only).
    pub fn register_count(&self) -> usize {
        self.chunks
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.regs.len())
            .sum()
    }
}

pub const HOT_TTL: Duration = Duration::from_secs(3600);

#[cfg(test)]
mod tests {
    use super::*;

    fn c() -> Cache {
        Cache::new(Duration::from_millis(50))
    }

    #[test]
    fn serves_a_fully_covering_chunk() {
        let cache = c();
        cache.put(0, (0..40).collect(), HOT_TTL, true, FC_READ_HOLDING);
        assert_eq!(cache.get(0, 4, 3), Some(vec![0, 1, 2, 3]));
        assert_eq!(cache.get(36, 4, 3), Some(vec![36, 37, 38, 39]));
        // one register past the end -> no cover
        assert_eq!(cache.get(37, 4, 3), None);
        assert_eq!(cache.get(0, 41, 3), None);
    }

    #[test]
    fn put_invalidates_only_the_overlapping_span() {
        let cache = c();
        cache.put(0, (0..10).collect(), HOT_TTL, true, 3);
        cache.put(4, vec![99, 99, 99], HOT_TTL, true, 3);
        // the tail of the original chunk survives
        assert_eq!(cache.get(7, 3, 3), Some(vec![7, 8, 9]));
        assert_eq!(cache.get(4, 3, 3), Some(vec![99, 99, 99]));
        // and the head still does
        assert_eq!(cache.get(0, 4, 3), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn a_newer_narrower_chunk_wins() {
        let cache = c();
        cache.put(0, (0..40).collect(), HOT_TTL, true, 3);
        cache.put(10, vec![7, 7], HOT_TTL, true, 3);
        assert_eq!(cache.get(10, 2, 3), Some(vec![7, 7]));
        assert_eq!(cache.get(0, 2, 3), Some(vec![0, 1]));
    }

    #[test]
    fn an_expired_chunk_is_never_served() {
        // The TTL is the promise: after it the value is gone, and a client asking again
        // gets nothing rather than a frame it cannot tell from a fresh reading.
        let cache = c();
        cache.put(0, (0..4).collect(), Duration::from_millis(1), true, 3);
        assert_eq!(cache.get(0, 4, 3), Some(vec![0, 1, 2, 3]));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(cache.get(0, 4, 3), None);
    }

    #[test]
    fn chunks_are_tracked_per_function_code() {
        let cache = c();
        cache.put(0, (0..4).collect(), HOT_TTL, true, 3);
        cache.put(0, vec![1, 1, 1, 1], HOT_TTL, true, 4);
        assert_eq!(cache.get(0, 4, 3), Some(vec![0, 1, 2, 3]));
        assert_eq!(cache.get(0, 4, 4), Some(vec![1, 1, 1, 1]));
    }

    #[test]
    fn invalidate_drops_the_whole_overlap() {
        let cache = c();
        cache.put(0, (0..10).collect(), HOT_TTL, true, 3);
        cache.invalidate(2, 5, None);
        assert_eq!(cache.get(0, 10, 3), None);
        assert_eq!(cache.get(0, 2, 3), Some(vec![0, 1]));
        assert_eq!(cache.get(6, 4, 3), Some(vec![6, 7, 8, 9]));
    }
}
