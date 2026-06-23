//! Double/triple-click detection — pure timing state machine (GILB-63).
//!
//! Tracks the time of the last click; if the next click arrives within a
//! threshold (default ~500ms, the OS double-click interval), increments the
//! count (2=double, 3=triple); otherwise resets to 1. Fully unit-testable on
//! Linux CI — no platform code.

use std::time::{Duration, Instant};

/// Default click-burst threshold: matches the macOS double-click interval.
const CLICK_BURST_THRESHOLD: Duration = Duration::from_millis(500);

/// Tracks click timing to detect double/triple clicks.
pub struct ClickState {
    last_click: Option<Instant>,
    count: u32,
}

impl ClickState {
    pub fn new() -> Self {
        Self {
            last_click: None,
            count: 0,
        }
    }

    /// Register a click at `now`. Returns the current burst count: 1 = single,
    /// 2 = double, 3 = triple, etc. Resets to 1 if the gap exceeds the threshold.
    pub fn on_click(&mut self, now: Instant) -> u32 {
        self.on_click_with_threshold(now, CLICK_BURST_THRESHOLD)
    }

    /// Same as [`on_click`] but with a custom threshold (for testing).
    pub fn on_click_with_threshold(&mut self, now: Instant, threshold: Duration) -> u32 {
        match self.last_click {
            Some(last) if now.duration_since(last) < threshold => {
                self.count += 1;
            }
            _ => {
                self.count = 1;
            }
        }
        self.last_click = Some(now);
        self.count
    }
}

impl Default for ClickState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_click_after_gap() {
        let mut s = ClickState::new();
        let start = Instant::now();
        assert_eq!(s.on_click(start), 1);
        // Long gap → reset to 1.
        assert_eq!(s.on_click(start + Duration::from_secs(2)), 1);
    }

    #[test]
    fn double_then_triple_click() {
        let mut s = ClickState::new();
        let start = Instant::now();
        let th = Duration::from_millis(100);
        assert_eq!(s.on_click_with_threshold(start, th), 1);
        assert_eq!(
            s.on_click_with_threshold(start + Duration::from_millis(50), th),
            2
        );
        assert_eq!(
            s.on_click_with_threshold(start + Duration::from_millis(80), th),
            3
        );
    }

    #[test]
    fn burst_resets_after_threshold() {
        let mut s = ClickState::new();
        let start = Instant::now();
        let th = Duration::from_millis(100);
        s.on_click_with_threshold(start, th);
        s.on_click_with_threshold(start + Duration::from_millis(50), th); // count = 2
                                                                          // Gap > threshold → reset.
        assert_eq!(
            s.on_click_with_threshold(start + Duration::from_millis(200), th),
            1
        );
    }
}
