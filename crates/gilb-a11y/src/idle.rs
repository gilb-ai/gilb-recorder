//! Idle / alive lifecycle tracking for `system` actions.
//!
//! Pure state machine: the normalizer feeds it input timestamps + a periodic
//! tick, and it says when to emit a `system` Action (`IdleStart` / `IdleEnd` /
//! `Alive`). The macOS-specific lock/unlock/sleep/wake signals will live in
//! `platform/macos/session.rs` (later card); this module is the cross-platform
//! timing core, so it is fully unit-testable on Linux CI.
//!
//! See GILB_CAPTURE_PLAN.md §2C. Defaults: idle after 120s of no input, alive
//! heartbeat every 15min.

use std::time::{Duration, Instant};

/// Idle threshold — no input for this long fires `IdleStart`.
const IDLE_AFTER: Duration = Duration::from_secs(120);
/// Alive heartbeat cadence.
const ALIVE_EVERY: Duration = Duration::from_secs(15 * 60);

/// A lifecycle `system` signal the normalizer can persist. The macOS session
/// source will add Lock/Unlock/Recording variants; only the timing-driven ones
/// are produced here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSignal {
    IdleStart,
    IdleEnd,
    Alive,
}

impl SystemSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            SystemSignal::IdleStart => "idle_start",
            SystemSignal::IdleEnd => "idle_end",
            SystemSignal::Alive => "alive",
        }
    }
}

/// Tracks input cadence and produces idle/alive `system` signals.
pub struct IdleTracker {
    last_input: Instant,
    idle: bool,
    last_alive: Instant,
}

impl IdleTracker {
    pub fn new(now: Instant) -> Self {
        Self {
            last_input: now,
            idle: false,
            last_alive: now,
        }
    }

    /// Call on every real input event. Returns `IdleEnd` if the user was idle.
    pub fn on_input(&mut self, now: Instant) -> Option<SystemSignal> {
        let was_idle = self.idle;
        self.idle = false;
        self.last_input = now;
        if was_idle {
            Some(SystemSignal::IdleEnd)
        } else {
            None
        }
    }

    /// Call on the periodic tick. May emit `IdleStart` and/or `Alive` (in that
    /// order when both apply).
    pub fn tick(&mut self, now: Instant) -> Vec<SystemSignal> {
        let mut out = Vec::new();
        if !self.idle && now.duration_since(self.last_input) >= IDLE_AFTER {
            self.idle = true;
            out.push(SystemSignal::IdleStart);
        }
        if now.duration_since(self.last_alive) >= ALIVE_EVERY {
            self.last_alive = now;
            out.push(SystemSignal::Alive);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_signal_within_thresholds() {
        let start = Instant::now();
        let mut t = IdleTracker::new(start);
        // A short gap fires nothing.
        assert!(t.tick(start + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn idle_starts_after_threshold_ends_on_input() {
        let start = Instant::now();
        let mut t = IdleTracker::new(start);

        // Past the 120s idle threshold with no input → IdleStart.
        let after_idle = start + Duration::from_secs(200);
        assert_eq!(t.tick(after_idle), vec![SystemSignal::IdleStart]);
        // Idling is sticky: a second tick doesn't re-fire IdleStart.
        assert!(t.tick(after_idle + Duration::from_secs(5)).is_empty());

        // Next real input ends idle; a further input while active does nothing.
        let resumed = after_idle + Duration::from_secs(30);
        assert_eq!(t.on_input(resumed), Some(SystemSignal::IdleEnd));
        assert_eq!(t.on_input(resumed), None);
    }

    #[test]
    fn alive_fires_on_cadence() {
        let start = Instant::now();
        let mut t = IdleTracker::new(start);
        // 16min with no input exceeds both the 120s idle threshold and the
        // 15min alive cadence → IdleStart then Alive.
        let later = start + Duration::from_secs(16 * 60);
        let signals = t.tick(later);
        assert!(signals.contains(&SystemSignal::IdleStart));
        assert!(signals.contains(&SystemSignal::Alive));
    }
}
