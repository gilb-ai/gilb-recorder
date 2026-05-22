//! Adaptive frame rate controller for tree walks. Filled in Phase 2.

use std::time::Duration;

/// Phase 0 placeholder: returns the policy intended for Phase 2 so callers can
/// already start to read it.
#[derive(Debug, Clone, Copy)]
pub enum ActivityState {
    Active,
    Idle,
    DeepIdle,
}

impl ActivityState {
    /// Target inter-walk interval per activity tier.
    pub fn walk_interval(self) -> Duration {
        match self {
            // 5 Hz — typing/clicking burst.
            ActivityState::Active => Duration::from_millis(200),
            // 1 Hz — idle but app focused.
            ActivityState::Idle => Duration::from_secs(1),
            // 0.5 Hz — no input for >2 minutes.
            ActivityState::DeepIdle => Duration::from_secs(2),
        }
    }
}
