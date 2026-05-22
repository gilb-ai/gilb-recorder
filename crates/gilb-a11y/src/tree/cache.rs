//! SimHash-based dedup for `tree_snapshots`. Wired up in Phase 2.

/// Hamming distance between two SimHash values.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Threshold used in Phase 2 dedup logic. We persist a new snapshot when
/// either:
/// - hamming(new, last) > `HAMMING_THRESHOLD`, or
/// - the last snapshot is older than `MAX_TTL_SECS`.
pub const HAMMING_THRESHOLD: u32 = 10;
pub const MAX_TTL_SECS: u64 = 60;
