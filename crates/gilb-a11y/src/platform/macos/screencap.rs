//! macOS screenshot grab + worker (GILB-80). cfg(macos) only.
//!
//! Grabs the main display via CoreGraphics `CGDisplay::image`, downscales to
//! ≤1440px longest side, and JPEG-encodes it. The [`ScreenshotCadence`] (in
//! `crate::screenshot`, cross-platform) decides *when*; this module does the
//! grab + file-write on the blocking pool, then hands the metadata row to the
//! engine writer.
//!
//! PRIVACY INVARIANTS (review 2026-07-12):
//! - Exclusion is re-checked HERE, immediately before the grab, and again
//!   after it — the normalizer's decision-time check reads a snapshot that
//!   can be up to ~500ms stale, and queued requests can execute later still.
//! - Requests older than [`STALE_AFTER`] are dropped: the context they were
//!   scheduled for is gone.
//! - A grab whose frontmost app changed mid-flight is discarded (the row
//!   would attribute another app's pixels).
//! - Multi-display setups are suppressed entirely: the v1 grab captures only
//!   the main display, so the pixels could be unrelated to the frontmost
//!   context. Window-scoped grab (`CGWindowListCreateImage`) is the real fix
//!   and is a privacy requirement before lifting this — NOT an optimisation.
//! - The worker honours a stop flag: after Stop, queued requests are
//!   discarded, never captured.
//!
//! NOTE: this path is macOS-only and therefore not exercised by the Linux CI —
//! it is compiled + clippy'd by the `macos-check` CI job. Runtime capture needs
//! the Screen Recording TCC permission (granted by the user, or by an MDM PPPC
//! profile in the future Unattended build); the platform gates worker spawn on
//! it (without the grant, CGDisplay happily returns wallpaper-only frames).

use core_graphics::display::CGDisplay;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::debug;

use gilb_core::{Screenshot, SessionId, WriterMessage};
use gilb_events::{EventBus, HealthEvent};
use tokio::sync::mpsc;

use crate::focus::FocusState;
use crate::password_masking::is_screenshot_excluded;
use crate::screenshot::{Grab, ScreenGrabber, ScreenshotRequest};

/// Bounded queue between the normalizer and the grab worker. Lossy on the
/// normalizer side (`try_send`), so a slow grab never parks capture.
const SCREENSHOT_CHANNEL_CAPACITY: usize = 8;
/// Longest-side cap for the downscaled image.
const MAX_EDGE: u32 = 1440;
/// Drop queued requests older than this — the context they were scheduled
/// for has moved on (grabs take hundreds of ms; the queue holds 8).
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// Grabs the main display as a JPEG via CoreGraphics.
pub struct MacScreenGrabber;

impl ScreenGrabber for MacScreenGrabber {
    fn grab(&mut self) -> Option<Grab> {
        // Privacy: with >1 active display the main-display grab captures
        // pixels unrelated to the frontmost context (and misattributes them
        // to it). Suppress until the grab is window-scoped.
        match CGDisplay::active_displays() {
            Ok(ids) if ids.len() == 1 => {}
            Ok(ids) => {
                debug!(
                    displays = ids.len(),
                    "screenshot suppressed: multi-display setup"
                );
                return None;
            }
            Err(_) => return None,
        }
        let cg = CGDisplay::main().image()?;
        let w = cg.width();
        let h = cg.height();
        let bpr = cg.bytes_per_row();
        let data = cg.data();
        let src = data.bytes(); // BGRA, row stride = bpr
        if w == 0 || h == 0 || src.len() < (h - 1) * bpr + w * 4 {
            return None;
        }

        // Repack BGRA (strided) → tightly-packed RGBA.
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            let row = &src[y * bpr..y * bpr + w * 4];
            let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                let s = &row[x * 4..x * 4 + 4];
                let d = &mut dst[x * 4..x * 4 + 4];
                d[0] = s[2]; // R
                d[1] = s[1]; // G
                d[2] = s[0]; // B
                d[3] = s[3]; // A
            }
        }

        let buf = image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
        let img = image::DynamicImage::ImageRgba8(buf);
        // Downscale to ≤ MAX_EDGE on the longest side.
        let (iw, ih) = (img.width(), img.height());
        let img = if iw.max(ih) > MAX_EDGE {
            let ratio = MAX_EDGE as f32 / iw.max(ih) as f32;
            img.resize(
                (iw as f32 * ratio) as u32,
                (ih as f32 * ratio) as u32,
                image::imageops::FilterType::Triangle,
            )
        } else {
            img
        };
        // JPEG has no alpha — flatten to RGB, then encode.
        let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
        let (fw, fh) = (rgb.width(), rgb.height());
        let mut bytes = Vec::new();
        rgb.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .ok()?;
        Some(Grab {
            bytes,
            width: fw as i64,
            height: fh as i64,
        })
    }
}

/// Spawn the screenshot worker. Mirrors the tree snapshotter: capture requests
/// arrive on the returned channel; the worker grabs on the blocking pool,
/// writes the JPEG under `<data_dir>/screenshots/`, and sends the metadata row
/// to the engine writer. Exits when the request channel closes OR the returned
/// stop flag is set — the flag makes shutdown discard queued requests instead
/// of capturing the post-Stop screen.
pub fn spawn_worker(
    session_id: SessionId,
    writer_tx: mpsc::Sender<WriterMessage>,
    event_bus: EventBus,
    focus: FocusState,
) -> (
    mpsc::Sender<ScreenshotRequest>,
    tokio::task::JoinHandle<()>,
    Arc<AtomicBool>,
) {
    let (tx, mut rx) = mpsc::channel::<ScreenshotRequest>(SCREENSHOT_CHANNEL_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let handle = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            if stop_flag.load(Ordering::SeqCst) {
                debug!("screenshot worker: stopping; discarding queued request");
                continue; // drain-and-drop until the channel closes
            }
            capture(session_id, req, &writer_tx, &event_bus, &focus).await;
        }
        debug!("screenshot worker: request channel closed; exiting");
    });
    (tx, handle, stop)
}

async fn capture(
    session_id: SessionId,
    req: ScreenshotRequest,
    writer_tx: &mpsc::Sender<WriterMessage>,
    event_bus: &EventBus,
    focus: &FocusState,
) {
    // Stale request: the decision was made for a context that may be long
    // gone (queue backlog + slow grabs). Never capture on old intent.
    if req.requested_at.elapsed() > STALE_AFTER {
        debug!("screenshot worker: dropping stale request");
        return;
    }
    // GRAB-TIME exclusion re-check. The normalizer checked at decision time,
    // but the user may have switched into a password manager since. This
    // reads the freshest focus snapshot we have.
    if is_screenshot_excluded(&focus.current()) {
        debug!("screenshot worker: context now excluded; dropping request");
        return;
    }

    // Grab + encode on the blocking pool (CGDisplay image + JPEG are heavy).
    let grab = tokio::task::spawn_blocking(|| MacScreenGrabber.grab())
        .await
        .ok()
        .flatten();
    let Some(grab) = grab else {
        // No image (no permission / multi-display / no display) — nothing to
        // persist.
        return;
    };

    // POST-grab check: if the frontmost context changed (or became excluded)
    // while the grab ran, the pixels don't match `req.app` — discard rather
    // than persist misattributed (possibly sensitive) content.
    let now_snap = focus.current();
    if is_screenshot_excluded(&now_snap) || now_snap.app.bundle_id != req.app.bundle_id {
        debug!("screenshot worker: context changed during grab; discarding image");
        return;
    }

    let screenshot_id = gilb_core::new_correlation_id();
    let dir = match gilb_config::data_dir() {
        Ok(d) => d.join("screenshots"),
        Err(_) => return,
    };
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return;
    }
    let path = dir.join(format!("{screenshot_id}.jpg"));
    if let Err(err) = tokio::fs::write(&path, &grab.bytes).await {
        debug!(?err, "screenshot worker: failed to write image file");
        return;
    }

    let shot = Screenshot {
        session_id,
        captured_at: chrono::Utc::now(),
        app: req.app,
        screenshot_id,
        image_path: path.to_string_lossy().to_string(),
        width: grab.width,
        height: grab.height,
    };
    if let Err(err) = writer_tx.try_send(WriterMessage::Screenshot(shot)) {
        // The row is lost — remove the file too, or it leaks as an orphan
        // (the shipper prunes via rows only; the janitor sweep is a backstop,
        // not the primary path).
        let _ = tokio::fs::remove_file(&path).await;
        match err {
            mpsc::error::TrySendError::Full(_) => {
                event_bus.publish_health(HealthEvent::DroppedEvent {
                    reason: "screenshot_writer_full".into(),
                    count: 1,
                });
            }
            // Closed = engine writer already shut down (post-Stop) — expected
            // during teardown, not a health problem.
            mpsc::error::TrySendError::Closed(_) => {
                debug!("screenshot worker: writer closed; dropped post-stop screenshot");
            }
        }
    }
}
