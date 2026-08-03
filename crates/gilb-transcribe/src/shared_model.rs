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
//! [`SharedModel::unload_if_idle`] without a borrow. Memory actually returns
//! only when the last holder drops its `Arc` too, which is the honest
//! behaviour: a worker mid-inference keeps what it is using.
//!
//! The cache is keyed by whatever configuration the load captures (for whisper,
//! the language): a caller asking for a different key gets a fresh load, never
//! the other configuration's instance. One slot, though — a key change
//! *replaces* the cached entry, so alternating keys alternate reloads; the two
//! consumers here settle on one configuration each per workload.
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
    /// Serializes cold loads: two callers racing a miss must not each read a
    /// ~570 MB model off disk — the second waits, then reuses the first's.
    load_lock: tokio::sync::Mutex<()>,
    idle_unload: Duration,
}

struct State<T> {
    model: Option<Cached<T>>,
    last_used: Instant,
}

struct Cached<T> {
    /// The configuration `model` was loaded with (e.g. the language).
    key: String,
    model: Arc<T>,
}

impl<T: Send + Sync + 'static> SharedModel<T> {
    pub fn new(idle_unload: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                model: None,
                last_used: Instant::now(),
            }),
            load_lock: tokio::sync::Mutex::new(()),
            idle_unload,
        })
    }

    /// The loaded model for `key`, loading it first if the cache is empty or
    /// holds a different configuration.
    ///
    /// `key` names whatever the `load` closure captures that changes the
    /// instance — for a whisper model, the language. A different key replaces
    /// the cached entry rather than silently sharing it; a live borrower of
    /// the replaced model keeps its instance until it lets go.
    ///
    /// `load` runs on the blocking pool. Two callers racing a miss both wait
    /// and then share one instance — the point of the whole type.
    pub async fn get<F>(&self, key: &str, load: F) -> Result<Arc<T>>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        if let Some(model) = self.cached(key) {
            return Ok(model);
        }
        let _guard = self.load_lock.lock().await;
        // Re-check under the load lock: a racing caller may have loaded this
        // very key while we waited for our turn.
        if let Some(model) = self.cached(key) {
            return Ok(model);
        }
        let loaded = tokio::task::spawn_blocking(load)
            .await
            .map_err(|e| anyhow::anyhow!("model load join: {e}"))??;

        let mut state = self.state.lock().expect("shared model mutex poisoned");
        let model = Arc::new(loaded);
        state.model = Some(Cached {
            key: key.to_string(),
            model: model.clone(),
        });
        state.last_used = Instant::now();
        Ok(model)
    }

    fn cached(&self, key: &str) -> Option<Arc<T>> {
        let mut state = self.state.lock().expect("shared model mutex poisoned");
        let model = state
            .model
            .as_ref()
            .filter(|cached| cached.key == key)
            .map(|cached| cached.model.clone());
        if model.is_some() {
            state.last_used = Instant::now();
        }
        model
    }

    /// Drop the cache's reference right now, ignoring the idle timer: the
    /// configuration changed (e.g. the transcription language) and the next
    /// [`get`](Self::get) must reload instead of serving the stale instance.
    /// Live borrowers keep what they hold; memory returns when they finish.
    pub fn invalidate(&self) {
        let mut state = self.state.lock().expect("shared model mutex poisoned");
        if state.model.take().is_some() {
            info!("whisper model invalidated; the next use reloads it");
        }
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
            .get("auto", || {
                LOADS.fetch_add(1, Ordering::SeqCst);
                Ok(Dummy(1))
            })
            .await
            .unwrap();
        let b = shared
            .get("auto", || panic!("must not load twice"))
            .await
            .unwrap();

        assert_eq!(LOADS.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// A different configuration must never be handed the cached instance:
    /// the caller that asked for "ru" transcribing through the "auto" model
    /// is exactly the bug the key exists for.
    #[tokio::test]
    async fn a_different_key_reloads() {
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(DEFAULT_IDLE_UNLOAD);

        let auto = shared.get("auto", || Ok(Dummy(1))).await.unwrap();
        let ru = shared.get("ru", || Ok(Dummy(2))).await.unwrap();

        assert_eq!(ru.0, 2);
        assert!(!Arc::ptr_eq(&auto, &ru));
        // The new key now owns the single slot; asking for it again is a hit.
        let ru_again = shared.get("ru", || panic!("must be cached")).await.unwrap();
        assert!(Arc::ptr_eq(&ru, &ru_again));
    }

    /// Two callers racing a cold cache run `load` once, not twice — a 570 MB
    /// read is not something to do speculatively and discard.
    #[tokio::test]
    async fn concurrent_cold_gets_load_once() {
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(DEFAULT_IDLE_UNLOAD);

        let load = || {
            LOADS.fetch_add(1, Ordering::SeqCst);
            // Slow the load down so the second caller actually races it.
            std::thread::sleep(Duration::from_millis(50));
            Ok(Dummy(1))
        };
        let (a, b) = tokio::join!(shared.get("auto", load), shared.get("auto", load));

        assert_eq!(LOADS.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
    }

    #[tokio::test]
    async fn unloads_only_after_the_idle_window() {
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(Duration::from_millis(50));
        let _model = shared.get("auto", || Ok(Dummy(1))).await.unwrap();

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
        let held = shared.get("auto", || Ok(Dummy(7))).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(shared.unload_if_idle());
        assert_eq!(held.0, 7);

        // The next request loads a fresh instance rather than resurrecting one.
        let again = shared.get("auto", || Ok(Dummy(8))).await.unwrap();
        assert_eq!(again.0, 8);
        assert!(!Arc::ptr_eq(&held, &again));
    }

    /// `invalidate` drops the cache entry immediately — a live borrow survives,
    /// but the next `get` reloads even inside the idle window.
    #[tokio::test]
    async fn invalidate_forces_a_reload() {
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        let shared: Arc<SharedModel<Dummy>> = SharedModel::new(DEFAULT_IDLE_UNLOAD);
        let load = || Ok(Dummy(LOADS.fetch_add(1, Ordering::SeqCst) + 1));

        let first = shared.get("auto", load).await.unwrap();
        shared.invalidate();
        assert!(!shared.is_loaded());
        assert_eq!(first.0, 1, "the live borrow is untouched");

        let second = shared.get("auto", load).await.unwrap();
        assert_eq!(second.0, 2, "the next get must reload, not resurrect");
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
