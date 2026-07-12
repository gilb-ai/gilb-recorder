//! Screenshot sampling cadence — pure timing state machine (GILB-80).
//!
//! Decides *when* to grab a screenshot: on focus/context boundaries (debounced
//! so the window has settled), coalescing rapid app-switching, suppressing on
//! idle, with a periodic heartbeat for deep single-app work and a hard hourly
//! cap. Platform-free and fully unit-testable on Linux CI — the actual grab
//! lives behind the [`ScreenGrabber`] trait (macOS impl in `platform::macos`).
//!
//! Decided cadence (2026-07-11): ~500 ms settle, ≥3 s min-gap, 180 s heartbeat,
//! ≤40 shots/hour. See research/18 and GILB_CAPTURE_PLAN §2G.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Delay after a boundary before the shot fires (let the new window render).
const SETTLE: Duration = Duration::from_millis(500);
/// Minimum spacing between shots — coalesces rapid switching to the settled ctx.
const MIN_GAP: Duration = Duration::from_secs(3);
/// Periodic shot when the context is unchanged (deep single-app work).
const HEARTBEAT: Duration = Duration::from_secs(180);
/// Safety cap on shots per rolling hour, per device.
const CAP_PER_HOUR: usize = 40;
/// The rolling window the cap is measured over.
const HOUR: Duration = Duration::from_secs(3600);

/// A captured screenshot's grabbed bytes + dimensions.
pub struct Grab {
    pub bytes: Vec<u8>,
    pub width: i64,
    pub height: i64,
}

/// A request the normalizer sends to the screenshot worker when the cadence
/// fires: capture now, tagging the row with the current foreground app.
/// `requested_at` lets the worker drop stale requests — a grab that would run
/// seconds after the decision may capture a completely different context.
#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    pub app: gilb_core::AppInfo,
    pub requested_at: Instant,
}

/// Grabs the screen as an encoded image. Prod impl is macOS-only; tests use a
/// mock. Returns `None` when the grab isn't possible (no display, permission
/// missing, unsupported platform).
///
/// PRIVACY NOTE: the v1 macOS impl captures the **entire main display** — not
/// just the frontmost window. Notification banners and any window merely
/// visible on that display are included. The exclusion gate (checked at
/// decision time AND re-checked at grab time) covers only the frontmost app;
/// a window-scoped grab (`CGWindowListCreateImage`) is the planned fix, and
/// until then the worker suppresses capture entirely on multi-display setups.
pub trait ScreenGrabber: Send {
    fn grab(&mut self) -> Option<Grab>;
}

/// Timing gate for screenshot sampling. Fed a boundary signal + a periodic
/// poll by the normalizer; says when to actually capture.
pub struct ScreenshotCadence {
    settle: Duration,
    min_gap: Duration,
    heartbeat: Duration,
    cap_per_hour: usize,
    /// A boundary was seen at this time; a shot is due at `t + settle`.
    pending: Option<Instant>,
    last_capture: Option<Instant>,
    /// Capture timestamps within the last hour (for the cap).
    recent: VecDeque<Instant>,
}

impl Default for ScreenshotCadence {
    fn default() -> Self {
        Self::with(SETTLE, MIN_GAP, HEARTBEAT, CAP_PER_HOUR)
    }
}

impl ScreenshotCadence {
    /// Custom tunables — used by tests to shrink the timings.
    pub fn with(
        settle: Duration,
        min_gap: Duration,
        heartbeat: Duration,
        cap_per_hour: usize,
    ) -> Self {
        Self {
            settle,
            min_gap,
            heartbeat,
            cap_per_hour,
            pending: None,
            last_capture: None,
            recent: VecDeque::new(),
        }
    }

    /// A focus / context boundary was observed — schedule a shot after settle.
    pub fn on_boundary(&mut self, now: Instant) {
        self.pending = Some(now);
    }

    /// Tick + boundary follow-up. Returns `true` iff a screenshot should be
    /// captured **now**. `idle` suppresses (nothing changes while idle);
    /// `excluded` skips (secure field / excluded app — never captured). The
    /// caller passes the CURRENT frontmost context for the exclusion gate.
    pub fn poll(&mut self, now: Instant, idle: bool, excluded: bool) -> bool {
        // Slide the rolling-hour cap window.
        while let Some(&t) = self.recent.front() {
            if now.duration_since(t) >= HOUR {
                self.recent.pop_front();
            } else {
                break;
            }
        }

        if idle {
            self.pending = None; // don't capture a frozen screen
            return false;
        }

        let boundary_due = matches!(self.pending, Some(t) if now.duration_since(t) >= self.settle);
        let heartbeat_due =
            matches!(self.last_capture, Some(t) if now.duration_since(t) >= self.heartbeat);
        if !(boundary_due || heartbeat_due) {
            return false;
        }

        // Coalesce: if we shot very recently, DEFER (keep `pending` armed) so
        // the settled context still gets its shot once the gap clears —
        // dropping it outright would leave a new context unphotographed until
        // the heartbeat (up to minutes later).
        if let Some(t) = self.last_capture {
            if now.duration_since(t) < self.min_gap {
                return false;
            }
        }
        // Hourly safety cap.
        if self.recent.len() >= self.cap_per_hour {
            self.pending = None;
            return false;
        }
        // Exclusion gate — secure / excluded context is never captured.
        if excluded {
            self.pending = None;
            return false;
        }

        self.pending = None;
        self.last_capture = Some(now);
        self.recent.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cadence() -> ScreenshotCadence {
        // Small timings for fast tests: 100ms settle, 1s gap, 10s heartbeat, cap 3.
        ScreenshotCadence::with(
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(10),
            3,
        )
    }

    #[test]
    fn boundary_fires_after_settle_not_before() {
        let mut c = cadence();
        let t = Instant::now();
        c.on_boundary(t);
        // Before settle → nothing.
        assert!(!c.poll(t + Duration::from_millis(50), false, false));
        // After settle → one shot.
        assert!(c.poll(t + Duration::from_millis(120), false, false));
        // Pending consumed → no repeat.
        assert!(!c.poll(t + Duration::from_millis(130), false, false));
    }

    #[test]
    fn rapid_switching_defers_to_settled_context_after_min_gap() {
        let mut c = cadence();
        let t = Instant::now();
        c.on_boundary(t);
        assert!(c.poll(t + Duration::from_millis(120), false, false)); // first shot
                                                                       // Second boundary 300ms later → settles but is < 1s min-gap → deferred.
        c.on_boundary(t + Duration::from_millis(300));
        assert!(!c.poll(t + Duration::from_millis(450), false, false));
        // Once the min-gap clears, the deferred shot of the settled context
        // fires — the new context is NOT left waiting for the heartbeat.
        assert!(c.poll(t + Duration::from_millis(1200), false, false));
        // Consumed: no repeat.
        assert!(!c.poll(t + Duration::from_millis(1300), false, false));
    }

    #[test]
    fn idle_suppresses_and_clears_pending() {
        let mut c = cadence();
        let t = Instant::now();
        c.on_boundary(t);
        // Idle at the moment the shot would fire → suppressed + pending cleared.
        assert!(!c.poll(t + Duration::from_millis(120), true, false));
        // Idle over → the (now-cleared) boundary does NOT fire retroactively.
        assert!(!c.poll(t + Duration::from_millis(200), false, false));
    }

    #[test]
    fn excluded_context_never_captures() {
        let mut c = cadence();
        let t = Instant::now();
        c.on_boundary(t);
        assert!(!c.poll(t + Duration::from_millis(120), false, true));
    }

    #[test]
    fn heartbeat_fires_when_context_unchanged() {
        let mut c = cadence();
        let t = Instant::now();
        c.on_boundary(t);
        assert!(c.poll(t + Duration::from_millis(120), false, false)); // baseline shot
                                                                       // No further boundary; 10s heartbeat elapses → a shot.
        assert!(c.poll(t + Duration::from_secs(11), false, false));
    }

    #[test]
    fn hourly_cap_limits_then_recovers() {
        let mut c = cadence(); // cap = 3
        let t = Instant::now();
        // Three boundary shots, spaced past the 1s min-gap.
        for i in 0..3 {
            let b = t + Duration::from_secs(i * 2);
            c.on_boundary(b);
            assert!(
                c.poll(b + Duration::from_millis(120), false, false),
                "shot {i}"
            );
        }
        // Fourth within the hour → capped.
        let b = t + Duration::from_secs(6);
        c.on_boundary(b);
        assert!(!c.poll(b + Duration::from_millis(120), false, false));
        // After the first shot ages out of the hour window → allowed again.
        let b = t + Duration::from_secs(3601);
        c.on_boundary(b);
        assert!(c.poll(b + Duration::from_millis(120), false, false));
    }
}
