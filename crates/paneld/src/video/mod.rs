//! Video zone (DESIGN.md §9, milestone M7): shared types between the
//! decoder (an `ffmpeg` subprocess, `decoder.rs`) and the `video` widget.
//!
//! ```text
//!  ffmpeg ──rawvideo rgb24 WxH──> decoder thread ──> FrameSlot (latest frame only)
//!                                        ▲                     │
//!                         touch() keeps it running             ▼
//!  video widget  update(): touch() + latest()        draw(): blit into the zone
//! ```
//!
//! Rules the two sides rely on:
//! - **The render thread never waits on the decoder.** `latest()` takes a
//!   mutex only long enough to clone an `Arc`; the decoder replaces the
//!   `Arc` under the same short lock. There is no queue: a slow render just
//!   skips frames.
//! - **Frames are exactly zone-sized** (`spec.width × spec.height`, RGB888):
//!   `ffmpeg` does the scaling and fit, so the widget only copies bytes.
//! - **Decoding runs only while the zone is on screen.** The widget calls
//!   [`VideoHandle::touch`] on every `update` (which the scene only calls
//!   while it is shown). The decoder starts `ffmpeg` on the first touch and
//!   stops it once no touch has arrived for [`IDLE_STOP`]. A video scene in
//!   a playlist therefore costs nothing while other scenes show.
//! - **Dropping the handle stops everything** (config reload rebuilds the
//!   scenes, and with them the handles).

pub mod decoder;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Fit;

/// Stop `ffmpeg` after this long without a [`VideoHandle::touch`].
pub const IDLE_STOP: Duration = Duration::from_secs(5);

/// One decoded frame, already scaled to the zone: `width × height` RGB888,
/// row-major, `3 * width * height` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    /// Increments per decoded frame (the widget can skip unchanged frames).
    pub seq: u64,
}

/// Where the video comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A local file, looped when `looping`, paced at its own frame rate.
    File {
        path: std::path::PathBuf,
        looping: bool,
    },
    /// A network stream (`rtsp://`, `http(s)://`, …). The URL may hold
    /// credentials (read from an env var by the config layer), so it is
    /// never logged or shown in `Debug` output by the decoder.
    Url(String),
}

/// Everything the decoder needs.
#[derive(Debug, Clone, PartialEq)]
pub struct DecoderSpec {
    /// The `ffmpeg` executable (default `ffmpeg`, found on `PATH`).
    pub program: std::path::PathBuf,
    pub input: Input,
    /// Output size: the zone size.
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
    /// Output frame rate cap (frames per second).
    pub fps: f64,
}

/// The latest frame, shared between the decoder (writer) and the widget
/// (reader).
#[derive(Debug, Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Arc<Frame>>>,
}

impl FrameSlot {
    pub fn put(&self, frame: Frame) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(frame));
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn clear(&self) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Counters for logs and `/metrics`.
#[derive(Debug, Default)]
pub struct VideoStats {
    pub frames_decoded: AtomicU64,
    pub starts: AtomicU64,
    pub errors: AtomicU64,
}

/// Shared between a [`VideoHandle`] and its decoder thread.
#[derive(Debug)]
pub struct Shared {
    pub slot: FrameSlot,
    pub stats: VideoStats,
    /// Milliseconds since `epoch` of the last touch; 0 = never.
    last_touch_ms: AtomicU64,
    epoch: Instant,
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            slot: FrameSlot::default(),
            stats: VideoStats::default(),
            last_touch_ms: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    /// Record that the zone is on screen now.
    pub fn touch(&self) {
        let ms = self.epoch.elapsed().as_millis() as u64;
        self.last_touch_ms.store(ms.max(1), Ordering::Relaxed);
    }

    /// Whether the zone was on screen within `idle` (decoder side).
    pub fn wanted(&self, idle: Duration) -> bool {
        let last = self.last_touch_ms.load(Ordering::Relaxed);
        last != 0 && self.epoch.elapsed().as_millis() as u64 <= last + idle.as_millis() as u64
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

/// The widget's handle on one decoder. Cheap calls only; never blocks.
/// Dropping it stops the decoder thread and any running `ffmpeg`.
pub struct VideoHandle {
    shared: Arc<Shared>,
    /// Owned by the decoder implementation; dropped with the handle.
    _decoder: decoder::DecoderGuard,
}

impl VideoHandle {
    /// Start the decoder for `spec` (it waits for the first touch before
    /// running `ffmpeg`).
    pub fn spawn(spec: DecoderSpec) -> Self {
        let shared = Arc::new(Shared::new());
        let guard = decoder::spawn(spec, shared.clone());
        VideoHandle {
            shared,
            _decoder: guard,
        }
    }

    /// A handle with no decoder, fed by tests (or a future in-process
    /// source) through [`VideoHandle::shared`].
    pub fn detached() -> Self {
        VideoHandle {
            shared: Arc::new(Shared::new()),
            _decoder: decoder::DecoderGuard::none(),
        }
    }

    pub fn touch(&self) {
        self.shared.touch();
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.shared.slot.latest()
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_keeps_only_the_latest_frame() {
        let s = FrameSlot::default();
        assert!(s.latest().is_none());
        for seq in 0..3 {
            s.put(Frame {
                width: 1,
                height: 1,
                rgb: vec![seq as u8; 3],
                seq,
            });
        }
        assert_eq!(s.latest().unwrap().seq, 2);
        s.clear();
        assert!(s.latest().is_none());
    }

    #[test]
    fn touch_marks_the_zone_wanted_for_the_idle_window() {
        let sh = Shared::new();
        assert!(!sh.wanted(IDLE_STOP), "never touched");
        sh.touch();
        assert!(sh.wanted(IDLE_STOP));
        std::thread::sleep(Duration::from_millis(5));
        assert!(!sh.wanted(Duration::from_millis(1)), "idle window passed");
    }

    #[test]
    fn detached_handle_reads_what_is_put() {
        let h = VideoHandle::detached();
        h.touch();
        h.shared().slot.put(Frame {
            width: 2,
            height: 1,
            rgb: vec![9; 6],
            seq: 7,
        });
        assert_eq!(h.latest().unwrap().seq, 7);
    }
}
