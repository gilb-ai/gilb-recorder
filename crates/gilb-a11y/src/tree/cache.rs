//! SimHash dedup for `tree_snapshots`.
//!
//! Per-(app, window) cache of the last persisted SimHash + when. A new
//! snapshot is persisted only when:
//!   - hamming(new, last) > [`HAMMING_THRESHOLD`] (the screen actually
//!     changed substantively), or
//!   - the last snapshot is older than [`MAX_TTL_SECS`] (refresh even
//!     identical-looking screens occasionally so the audit trail isn't
//!     suspiciously sparse).
//!
//! Bounded by [`MAX_ENTRIES`] with LRU-ish eviction — long sessions
//! visit many windows but we don't need every one indefinitely.

use std::collections::HashMap;
use std::time::Instant;

pub const HAMMING_THRESHOLD: u32 = 10;
pub const MAX_TTL_SECS: u64 = 60;
pub const MAX_ENTRIES: usize = 100;

/// Hamming distance between two SimHash values.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 64-bit SimHash over 3-word shingles of `tokens`. The exact tokenisation
/// is up to the caller (we just see strings); the walker turns each
/// element into `<role>:<text>` and feeds them in document order.
pub fn simhash(tokens: &[String]) -> u64 {
    if tokens.is_empty() {
        return 0;
    }
    let mut counters = [0i32; 64];
    let shingles: Vec<String> = tokens.windows(3).map(|w| w.join(" ")).collect();
    let bag: Vec<&str> = if shingles.is_empty() {
        tokens.iter().map(|s| s.as_str()).collect()
    } else {
        shingles.iter().map(|s| s.as_str()).collect()
    };
    for token in bag {
        let h = fnv1a_64(token.as_bytes());
        for (i, c) in counters.iter_mut().enumerate() {
            if (h >> i) & 1 == 1 {
                *c += 1;
            } else {
                *c -= 1;
            }
        }
    }
    let mut out: u64 = 0;
    for (i, c) in counters.iter().enumerate() {
        if *c > 0 {
            out |= 1u64 << i;
        }
    }
    out
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Debug, Clone)]
struct Entry {
    simhash: u64,
    last_stored: Instant,
}

/// Per-(app, window) memory of what we last persisted.
#[derive(Debug, Default)]
pub struct SnapshotCache {
    entries: HashMap<(String, String), Entry>,
}

impl SnapshotCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether to persist a new snapshot for `(app, window)` whose
    /// SimHash is `simhash`. Records the new entry if we decide to store.
    pub fn should_store(&mut self, app: &str, window: &str, simhash: u64) -> bool {
        let key = (app.to_string(), window.to_string());
        let now = Instant::now();
        let decision = match self.entries.get(&key) {
            Some(entry) => {
                hamming(entry.simhash, simhash) > HAMMING_THRESHOLD
                    || now.duration_since(entry.last_stored).as_secs() >= MAX_TTL_SECS
            }
            None => true,
        };
        if decision {
            self.entries.insert(
                key,
                Entry {
                    simhash,
                    last_stored: now,
                },
            );
            self.evict_if_full();
        }
        decision
    }

    fn evict_if_full(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        if let Some(oldest_key) = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.last_stored)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_token_streams_share_simhash() {
        let a = simhash(&["AXButton:Save".into(), "AXTextField:Name".into()]);
        let b = simhash(&["AXButton:Save".into(), "AXTextField:Name".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn first_time_always_stores() {
        let mut c = SnapshotCache::new();
        assert!(c.should_store("Chrome", "GitHub", 0xdead_beef));
    }

    #[test]
    fn nearly_identical_snapshot_is_skipped() {
        let mut c = SnapshotCache::new();
        assert!(c.should_store("Chrome", "GitHub", 0x0));
        // hamming = 2, below threshold (10)
        assert!(!c.should_store("Chrome", "GitHub", 0x3));
    }

    #[test]
    fn substantively_different_snapshot_is_stored() {
        let mut c = SnapshotCache::new();
        assert!(c.should_store("Chrome", "GitHub", 0x0));
        // hamming = 64, way above threshold
        assert!(c.should_store("Chrome", "GitHub", u64::MAX));
    }
}
