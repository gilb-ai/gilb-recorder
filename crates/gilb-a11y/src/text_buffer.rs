//! 300ms debounce aggregator that flushes typing bursts into a single
//! `text` action.
//!
//! Phase 0 ships only the buffer state machine + unit tests; the real keycode
//! → string translation (UCKeyTranslate + deadkey state) lands in Phase 1.

use std::time::{Duration, Instant};

pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// What caused the buffer to flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushReason {
    Timeout,
    Click,
    FocusChange,
    NavigationKey,
    PasswordEntered,
    Stopping,
}

#[derive(Debug, Clone)]
pub struct FlushedText {
    pub text: String,
    pub started_at: Instant,
    pub ended_at: Instant,
    pub reason: FlushReason,
    pub masked: bool,
}

#[derive(Debug)]
pub struct TextBuffer {
    buf: String,
    started_at: Option<Instant>,
    last_input_at: Option<Instant>,
    masked: bool,
    debounce: Duration,
}

impl TextBuffer {
    pub fn new(debounce: Duration) -> Self {
        Self {
            buf: String::new(),
            started_at: None,
            last_input_at: None,
            masked: false,
            debounce,
        }
    }

    pub fn push(&mut self, s: &str, masked: bool) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
        self.buf.push_str(s);
        self.last_input_at = Some(Instant::now());
        self.masked = self.masked || masked;
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn should_timeout(&self, now: Instant) -> bool {
        match self.last_input_at {
            Some(last) => now.saturating_duration_since(last) >= self.debounce,
            None => false,
        }
    }

    pub fn flush(&mut self, reason: FlushReason) -> Option<FlushedText> {
        if self.buf.is_empty() {
            self.reset();
            return None;
        }
        let now = Instant::now();
        let started_at = self.started_at.unwrap_or(now);
        let ended_at = self.last_input_at.unwrap_or(now);
        let text = if self.masked {
            "[masked]".to_string()
        } else {
            std::mem::take(&mut self.buf)
        };
        let masked = self.masked;
        self.reset();
        Some(FlushedText {
            text,
            started_at,
            ended_at,
            reason,
            masked,
        })
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.started_at = None;
        self.last_input_at = None;
        self.masked = false;
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_DEBOUNCE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn empty_buffer_does_not_flush() {
        let mut buf = TextBuffer::default();
        assert!(buf.flush(FlushReason::Timeout).is_none());
    }

    #[test]
    fn pushes_aggregate_into_one_flush() {
        let mut buf = TextBuffer::default();
        buf.push("h", false);
        buf.push("i", false);
        let flushed = buf.flush(FlushReason::Click).unwrap();
        assert_eq!(flushed.text, "hi");
        assert_eq!(flushed.reason, FlushReason::Click);
        assert!(!flushed.masked);
    }

    #[test]
    fn masked_push_masks_entire_flush() {
        let mut buf = TextBuffer::default();
        buf.push("safe", false);
        buf.push("secret", true);
        let flushed = buf.flush(FlushReason::Timeout).unwrap();
        assert_eq!(flushed.text, "[masked]");
        assert!(flushed.masked);
    }

    #[test]
    fn timeout_fires_after_debounce_window() {
        let mut buf = TextBuffer::new(Duration::from_millis(20));
        buf.push("a", false);
        assert!(!buf.should_timeout(Instant::now()));
        sleep(Duration::from_millis(30));
        assert!(buf.should_timeout(Instant::now()));
    }
}
