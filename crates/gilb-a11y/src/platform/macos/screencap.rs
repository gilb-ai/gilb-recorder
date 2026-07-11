//! macOS screenshot grab + worker (GILB-80). cfg(macos) only.
//!
//! Grabs the main display via CoreGraphics `CGDisplay::image`, downscales to
//! ≤1440px longest side, and JPEG-encodes it. The [`ScreenshotCadence`] (in
//! `crate::screenshot`, cross-platform) decides *when*; this module does the
//! grab + file-write on the blocking pool, then hands the metadata row to the
//! engine writer.
//!
//! NOTE: this path is macOS-only and therefore not exercised by the Linux CI —
//! it is compiled + clippy'd by the `macos-check` CI job. Runtime capture needs
//! the Screen Recording TCC permission (granted by the user, or by an MDM PPPC
//! profile in the future Unattended build). Full-display capture is the v1
//! grab; window-scoped grab (CGWindowListCreateImage) is a follow-up. WebP and
//! JPEG-quality tuning are later optimisations (default-quality JPEG for now).

use core_graphics::display::CGDisplay;
use std::io::Cursor;
use tracing::debug;

use gilb_core::{Screenshot, SessionId, WriterMessage};
use gilb_events::{EventBus, HealthEvent};
use tokio::sync::mpsc;

use crate::screenshot::{Grab, ScreenGrabber, ScreenshotRequest};

/// Bounded queue between the normalizer and the grab worker. Lossy on the
/// normalizer side (`try_send`), so a slow grab never parks capture.
const SCREENSHOT_CHANNEL_CAPACITY: usize = 8;
/// Longest-side cap for the downscaled image.
const MAX_EDGE: u32 = 1440;

/// Grabs the main display as a JPEG via CoreGraphics.
pub struct MacScreenGrabber;

impl ScreenGrabber for MacScreenGrabber {
    fn grab(&mut self) -> Option<Grab> {
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
/// to the engine writer. Exits when the request channel closes.
pub fn spawn_worker(
    session_id: SessionId,
    writer_tx: mpsc::Sender<WriterMessage>,
    event_bus: EventBus,
) -> (mpsc::Sender<ScreenshotRequest>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<ScreenshotRequest>(SCREENSHOT_CHANNEL_CAPACITY);
    let handle = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            capture(session_id, req, &writer_tx, &event_bus).await;
        }
        debug!("screenshot worker: request channel closed; exiting");
    });
    (tx, handle)
}

async fn capture(
    session_id: SessionId,
    req: ScreenshotRequest,
    writer_tx: &mpsc::Sender<WriterMessage>,
    event_bus: &EventBus,
) {
    // Grab + encode on the blocking pool (CGDisplay image + JPEG are heavy).
    let grab = tokio::task::spawn_blocking(|| MacScreenGrabber.grab())
        .await
        .ok()
        .flatten();
    let Some(grab) = grab else {
        // No image (no permission / no display) — nothing to persist.
        return;
    };

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
    if writer_tx.try_send(WriterMessage::Screenshot(shot)).is_err() {
        event_bus.publish_health(HealthEvent::DroppedEvent {
            reason: "screenshot_writer_full".into(),
            count: 1,
        });
    }
}
