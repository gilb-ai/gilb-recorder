//! One whisper model per process, however many workers want it.
//!
//! Two of them do: post-meeting transcription and real-time suggestions. Each
//! used to load its own `LocalTranscriber` with its own idle timer, so the
//! moment a meeting ended — post-processing starting while the realtime copy
//! was still warm — the process held **two** ~570 MB models. Not a leak, not
//! occasional: guaranteed, every meeting.
//!
//! [`SharedModel`] hands both the same `Arc`, loads it once (on the blocking
//! pool — it is a large synchronous read) and drops the cache's reference after
//! [`SharedModel::idle_unload`] without a borrow. Memory actually returns only
//! when the last holder drops its `Arc` too, which is the honest behaviour: a
//! worker mid-inference keeps what it is using.
//!
//! Generic over the loaded type so the caching, the single-flight guarantee and
//! the idle sweep can be tested without a 570 MB file on disk.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::info;

/// Default: long enough to survive the gap between two meetings' processing,
/// short enough that an idle laptop does not hold half a gigabyte.
pub const DEFAULT_IDLE_UNLOAD: Duration = Duration::from_secs(5 * 60);

pub struct SharedModel<T> {
    state: Mutex<State<T>>,
    idle_unload: Duration,
}

struct State<T> {
    model: Option<Arc<T>>,
    last_used: Instant,
}

impl<T: Send + Sync + 'static> SharedModel<T> {
    pub fn new(idle_unload: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                model: None,
                last_used: Instant::now(),
            }),
            idle_unload,
        })
    }

    /// The loaded model, loading it first if nobody has yet.
    ///
    /// `load` runs on the blocking pool. Two callers racing here both wait and
    /// then share one instance — the point of the whole type.
    pub async fn get<F>(&self, load: F) -> Result<Arc<T>>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        if let Some(model) = self.cached() {
            return Ok(model);
        }
        let loaded = tokio::task::spawn_blocking(load)
            .await
            .map_err(|e| anyhow::anyhow!("model load join: {e}"))??;

        let mut state = self.state.lock().expect("shared model mutex poisoned");
        // Someone may have finished loading while we were on the pool; keep
        // theirs so every holder ends up with the same instance.
        let model = match &state.model {
            Some(existing) => existing.clone(),
            None => {
                let model = Arc::new(loaded);
                state.model = Some(model.clone());
                model
            }
        };
        state.last_used = Instant::now();
        Ok(model)
    }

    fn cached(&self) -> Option<Arc<T>> {
        let mut state = self.state.lock().expect("shared model mutex poisoned");
        let model = state.model.clone();
        if model.is_some() {
            state.last_used = Instant::now();
        }
        model
    }

    /// Drop the cache's reference if nothing has asked for the model in
    /// [`Self::idle_unload`]. Returns whether it dropped anything.
    pub fn unload_if_idle(&self) -> bool {
        let mut state = self.state.lock().expect("shared model mutex poisoned");
        if state.model.is_some() && state.last_used.elapsed() >= self.idle_unload {
            state.model = None;
            info!("whisper model unloaded after idle");
            return true;
        }
        false
    }

    /// Whether the model is currently held by the cache (tests, diagnostics).
    pub fn is_loaded(&self) -> bool {
        self.state
            .lock()
            .expect("shared model mutex poisoned")
            .model
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Dummy(usize);

    #[tokio::test]
    async fn loads_once_and_shares() {
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(DEFAULT_IDLE_UNLOAD);

        let a = shared
            .get(|| {
                LOADS.fetch_add(1, Ordering::SeqCst);
                Ok(Dummy(1))
            })
            .await
            .unwrap();
        let b = shared.get(|| panic!("must not load twice")).await.unwrap();

        assert_eq!(LOADS.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn unloads_only_after_the_idle_window() {
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(Duration::from_millis(50));
        let _model = shared.get(|| Ok(Dummy(1))).await.unwrap();

        assert!(!shared.unload_if_idle(), "just used — must stay");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(shared.unload_if_idle());
        assert!(!shared.is_loaded());
    }

    /// A holder mid-inference keeps its instance alive after the cache lets go
    /// — otherwise unloading would pull the model out from under a running
    /// transcription.
    #[tokio::test]
    async fn a_live_borrow_survives_unload() {
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(Duration::from_millis(10));
        let held = shared.get(|| Ok(Dummy(7))).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(shared.unload_if_idle());
        assert_eq!(held.0, 7);

        // The next request loads a fresh instance rather than resurrecting one.
        let again = shared.get(|| Ok(Dummy(8))).await.unwrap();
        assert_eq!(again.0, 8);
        assert!(!Arc::ptr_eq(&held, &again));
    }
}
