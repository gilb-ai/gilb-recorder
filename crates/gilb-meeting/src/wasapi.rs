//! Windows meeting detection via WASAPI audio-session events.
//!
//! Unlike macOS — where Control Center logs the full set of active mic
//! attributions on every change — WASAPI is *incremental*: each capture
//! session reports its own `new`/`active`/`inactive`/`expired`
//! transitions. [`SessionTracker`] converges those increments into the
//! full snapshot the shared [`Tracker`] expects: it maps each session's
//! process exe to a bundle ID, keeps the set of apps currently holding
//! the mic, and feeds that snapshot into `Tracker`, which owns the
//! `Started`/`AppsChanged`/`Ended` count logic shared with macOS.
//!
//! The mapping and the active/inactive/expired set semantics come from an
//! earlier Electron implementation and its native mic tracker, which had
//! already been shaken out against real calls. `SessionTracker` and
//! the process map are pure and unit-tested without COM; the live COM
//! chain ([`WindowsDetector`]) is `cfg(target_os = "windows")` and
//! smoke-tested by hand on a Windows host.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::{allowlist, MeetingApp, MeetingEvent, Tracker};

/// One WASAPI capture-session transition, as surfaced by the COM layer.
///
/// Mirrors the `IAudioSessionEvents`/`IAudioSessionNotification` signals
/// the OS emits: a session is created (`New`), starts
/// using the mic (`Active`), stops (`Inactive`), or goes away
/// (`Expired`); `DefaultDeviceChanged` fires when the default capture
/// endpoint changes. Only `Active`/`Inactive`/`Expired` move the active
/// set — see [`SessionTracker::observe`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SessionEvent {
    New,
    Active,
    Inactive,
    Expired,
    DefaultDeviceChanged,
}

/// Converges incremental WASAPI session events into [`MeetingEvent`]s.
///
/// Keeps the set of bundle IDs whose sessions are currently `Active`
/// (add on `Active`, remove on `Inactive`/`Expired`; `New` and
/// `DefaultDeviceChanged` leave it untouched). After each relevant
/// transition it feeds the
/// full snapshot into the shared [`Tracker`], reusing the exact
/// count-transition logic the macOS detector uses. Sessions whose
/// process is not a known meeting app are ignored.
#[derive(Debug, Default)]
pub struct SessionTracker {
    active: BTreeSet<&'static str>,
    tracker: Tracker,
}

impl SessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one session transition for `process_name` at time `now`.
    ///
    /// Returns the resulting [`MeetingEvent`], if the active set crossed
    /// a count boundary the shared [`Tracker`] cares about. Unknown
    /// processes and the no-op events (`New`, `DefaultDeviceChanged`)
    /// return `None` without touching the active set.
    pub fn observe(
        &mut self,
        event: SessionEvent,
        process_name: &str,
        now: DateTime<Utc>,
    ) -> Option<MeetingEvent> {
        let bundle_id = allowlist::bundle_id_from_process_name(process_name)?;

        match event {
            SessionEvent::Active => {
                self.active.insert(bundle_id);
            }
            SessionEvent::Inactive | SessionEvent::Expired => {
                self.active.remove(bundle_id);
            }
            // A session opening, and default-device changes, never move
            // the active set on their own — only active / inactive /
            // expired count. Nothing changed, so nothing to emit.
            SessionEvent::New | SessionEvent::DefaultDeviceChanged => return None,
        }

        let apps = self
            .active
            .iter()
            .map(|&bundle_id| MeetingApp {
                bundle_id: bundle_id.to_string(),
                display_name: allowlist::display_name(bundle_id)
                    .unwrap_or(bundle_id)
                    .to_string(),
            })
            .collect();
        self.tracker.observe(apps, now)
    }
}

#[cfg(target_os = "windows")]
mod detector {
    use std::sync::mpsc as std_mpsc;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread::{self, JoinHandle};

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;
    use tokio::sync::{mpsc, Mutex};
    use tokio::task;
    use windows::core::{implement, Interface, Result as WinResult};
    use windows::Win32::Foundation::{CloseHandle, BOOL, MAX_PATH};
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, AudioSessionDisconnectReason, AudioSessionState,
        AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive,
        IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl,
        IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
        IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_core::{GUID, PCWSTR, PWSTR};

    use super::{SessionEvent, SessionTracker};
    use crate::{MeetingDetector, MeetingEvent, EVENT_CHANNEL_CAPACITY};

    /// A raw session transition plus the originating process exe, sent
    /// from a COM callback thread to the converging forward task.
    type RawSessionEvent = (SessionEvent, String);

    struct Running {
        shutdown: std_mpsc::Sender<()>,
        com_thread: JoinHandle<()>,
        forward: task::JoinHandle<()>,
    }

    /// Meeting detector backed by WASAPI audio-session events; see the
    /// module docs. The COM chain runs on a dedicated MTA thread and
    /// forwards raw transitions to an async task that owns the
    /// [`SessionTracker`].
    #[derive(Default)]
    pub struct WindowsDetector {
        running: Mutex<Option<Running>>,
    }

    impl WindowsDetector {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait]
    impl MeetingDetector for WindowsDetector {
        async fn start(&self) -> Result<mpsc::Receiver<MeetingEvent>> {
            let mut guard = self.running.lock().await;
            if guard.is_some() {
                return Err(anyhow!("WindowsDetector already started"));
            }
            let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
            let (raw_tx, raw_rx) = mpsc::channel::<RawSessionEvent>(EVENT_CHANNEL_CAPACITY);
            let (shutdown_tx, shutdown_rx) = std_mpsc::channel();

            let forward = tokio::spawn(forward(raw_rx, event_tx.clone()));
            let com_thread = thread::spawn(move || com_main(raw_tx, shutdown_rx, event_tx));

            *guard = Some(Running {
                shutdown: shutdown_tx,
                com_thread,
                forward,
            });
            Ok(event_rx)
        }

        async fn stop(&self) -> Result<()> {
            let running = self.running.lock().await.take();
            if let Some(Running {
                shutdown,
                com_thread,
                forward,
            }) = running
            {
                let _ = shutdown.send(());
                let _ = task::spawn_blocking(move || com_thread.join()).await;
                let _ = forward.await;
            }
            Ok(())
        }
    }

    /// Drain raw transitions, converge them through [`SessionTracker`],
    /// and emit the resulting [`MeetingEvent`]s. Exits when every raw
    /// sender is dropped (i.e. the COM thread tore down).
    async fn forward(mut raw_rx: mpsc::Receiver<RawSessionEvent>, tx: mpsc::Sender<MeetingEvent>) {
        let mut tracker = SessionTracker::new();
        while let Some((event, process_name)) = raw_rx.recv().await {
            if let Some(meeting_event) = tracker.observe(event, &process_name, Utc::now()) {
                if tx.send(meeting_event).await.is_err() {
                    break;
                }
            }
        }
    }

    /// Owns the COM chain and the live event handlers, keeping them
    /// registered for as long as the detector runs.
    #[derive(Clone)]
    struct Registry {
        raw_tx: mpsc::Sender<RawSessionEvent>,
        sessions: Arc<StdMutex<Vec<(IAudioSessionControl2, IAudioSessionEvents)>>>,
    }

    impl Registry {
        fn send(&self, event: SessionEvent, process_name: &str) {
            // try_send: COM callback threads must not block; dropping a
            // rare transition under backpressure is acceptable.
            let _ = self.raw_tx.try_send((event, process_name.to_string()));
        }

        /// Hook an existing or newly created session: emit `New` (and
        /// `Active` if already capturing), then register for its state
        /// changes and retain the handler so callbacks keep arriving.
        fn register_session(&self, control: &IAudioSessionControl) -> WinResult<()> {
            let control2: IAudioSessionControl2 = control.cast()?;
            let pid = unsafe { control2.GetProcessId()? };
            if pid == 0 {
                return Ok(());
            }
            let Some(process_name) = process_name_by_pid(pid) else {
                return Ok(());
            };

            self.send(SessionEvent::New, &process_name);
            if unsafe { control2.GetState()? } == AudioSessionStateActive {
                self.send(SessionEvent::Active, &process_name);
            }

            let events: IAudioSessionEvents = SessionEvents {
                raw_tx: self.raw_tx.clone(),
                process_name,
            }
            .into();
            unsafe { control2.RegisterAudioSessionNotification(&events)? };
            self.sessions.lock().unwrap().push((control2, events));
            Ok(())
        }
    }

    /// Build the COM chain on this (MTA) thread, register notifications,
    /// then block until `stop` signals shutdown and tear everything down.
    fn com_main(
        raw_tx: mpsc::Sender<RawSessionEvent>,
        shutdown: std_mpsc::Receiver<()>,
        health_tx: mpsc::Sender<MeetingEvent>,
    ) {
        let registry = Registry {
            raw_tx,
            sessions: Arc::new(StdMutex::new(Vec::new())),
        };

        let chain = match unsafe { setup(&registry) } {
            Ok(chain) => chain,
            Err(e) => {
                let _ = health_tx.blocking_send(MeetingEvent::HealthDegraded {
                    reason: format!("failed to start WASAPI session monitor: {e}"),
                });
                unsafe { CoUninitialize() };
                return;
            }
        };

        // MTA delivers session callbacks on COM RPC threads, so this
        // thread just parks until shutdown — no message pump needed.
        let _ = shutdown.recv();

        unsafe { teardown(&registry, chain) };
        unsafe { CoUninitialize() };
    }

    /// The top-level COM objects kept alive for the session's lifetime.
    struct Chain {
        manager: IAudioSessionManager2,
        notification: IAudioSessionNotification,
    }

    unsafe fn setup(registry: &Registry) -> WinResult<Chain> {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;

        // Pick up sessions already running before we attached.
        let session_enum = manager.GetSessionEnumerator()?;
        for i in 0..session_enum.GetCount()? {
            if let Ok(control) = session_enum.GetSession(i) {
                let _ = registry.register_session(&control);
            }
        }

        let notification: IAudioSessionNotification = SessionNotification {
            registry: registry.clone(),
        }
        .into();
        manager.RegisterSessionNotification(&notification)?;

        Ok(Chain {
            manager,
            notification,
        })
    }

    unsafe fn teardown(registry: &Registry, chain: Chain) {
        let _ = chain
            .manager
            .UnregisterSessionNotification(&chain.notification);
        for (control, events) in registry.sessions.lock().unwrap().drain(..) {
            let _ = control.UnregisterAudioSessionNotification(&events);
        }
    }

    /// Resolve a PID to its exe basename via the Win32 process image
    /// path (`OpenProcess` -> `QueryFullProcessImageNameW`).
    fn process_name_by_pid(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; MAX_PATH as usize];
            let mut size = buf.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            let path = String::from_utf16_lossy(&buf[..size as usize]);
            path.rsplit(['\\', '/']).next().map(str::to_string)
        }
    }

    /// `IAudioSessionEvents` handler for one session: forwards state
    /// changes and disconnects as [`SessionEvent`]s.
    #[implement(IAudioSessionEvents)]
    struct SessionEvents {
        raw_tx: mpsc::Sender<RawSessionEvent>,
        process_name: String,
    }

    impl SessionEvents {
        fn send(&self, event: SessionEvent) {
            let _ = self.raw_tx.try_send((event, self.process_name.clone()));
        }
    }

    #[allow(non_snake_case)]
    impl IAudioSessionEvents_Impl for SessionEvents_Impl {
        fn OnStateChanged(&self, newstate: AudioSessionState) -> WinResult<()> {
            let event = if newstate == AudioSessionStateActive {
                Some(SessionEvent::Active)
            } else if newstate == AudioSessionStateInactive {
                Some(SessionEvent::Inactive)
            } else if newstate == AudioSessionStateExpired {
                Some(SessionEvent::Expired)
            } else {
                None
            };
            if let Some(event) = event {
                self.send(event);
            }
            Ok(())
        }

        fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> WinResult<()> {
            self.send(SessionEvent::Expired);
            Ok(())
        }

        fn OnDisplayNameChanged(&self, _newname: &PCWSTR, _context: *const GUID) -> WinResult<()> {
            Ok(())
        }

        fn OnIconPathChanged(&self, _newpath: &PCWSTR, _context: *const GUID) -> WinResult<()> {
            Ok(())
        }

        fn OnSimpleVolumeChanged(
            &self,
            _newvolume: f32,
            _newmute: BOOL,
            _context: *const GUID,
        ) -> WinResult<()> {
            Ok(())
        }

        fn OnChannelVolumeChanged(
            &self,
            _channelcount: u32,
            _newvolumes: *const f32,
            _changedchannel: u32,
            _context: *const GUID,
        ) -> WinResult<()> {
            Ok(())
        }

        fn OnGroupingParamChanged(
            &self,
            _newgroupingparam: *const GUID,
            _context: *const GUID,
        ) -> WinResult<()> {
            Ok(())
        }
    }

    /// `IAudioSessionNotification` handler: registers each newly created
    /// session with the shared [`Registry`].
    #[implement(IAudioSessionNotification)]
    struct SessionNotification {
        registry: Registry,
    }

    #[allow(non_snake_case)]
    impl IAudioSessionNotification_Impl for SessionNotification_Impl {
        fn OnSessionCreated(&self, newsession: Option<&IAudioSessionControl>) -> WinResult<()> {
            if let Some(control) = newsession {
                let _ = self.registry.register_session(control);
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
pub use detector::WindowsDetector;
