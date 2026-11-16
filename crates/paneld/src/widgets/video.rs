use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};

use super::{FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Fit, VideoConfig, VideoInput};
use crate::sources::ha::Secret;
use crate::store::StoreSnapshot;
use crate::video::{DecoderSpec, Frame, Input, VideoHandle};

/// A live video zone (DESIGN.md §9): the latest frame of an `ffmpeg`
/// subprocess, blitted into the zone.
///
/// - `ffmpeg` scales to the zone size, so frames are exactly zone-sized RGB888
///   and `draw` is a plain copy with no scaling.
/// - `update` marks the zone as on screen ([`VideoHandle::touch`], which makes
///   the decoder run) and takes the newest frame (an `Arc` clone under a
///   short lock). Nothing here waits on the decoder or does I/O.
/// - **No-frame state:** until the first frame arrives (decoder starting,
///   stream down, `ffmpeg` missing, URL variable unset) the zone is filled
///   black. It stays opaque (the zone `background` is not shown) and there is
///   no "no signal" text; the scene's other zones and the logs say what is
///   wrong. A frame whose size does not match the zone is ignored the same way.
/// - The handle is created by `prepare` (config load) or, if that never ran,
///   on first use. Dropping the widget (config reload) stops the decoder.
pub struct Video {
    state: RefCell<State>,
    frame: Option<Arc<Frame>>,
}

enum State {
    /// Waiting for the zone size to start a decoder.
    Pending(Pending),
    Running(VideoHandle),
    /// No decoder (the URL variable was unset at build time).
    Failed,
}

/// What a decoder needs besides the zone size. `Debug` redacts the URL.
struct Pending {
    program: PathBuf,
    input: PendingInput,
    fit: Fit,
    fps: f64,
}

enum PendingInput {
    File { path: PathBuf, looping: bool },
    Url(Secret),
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let input = match &self.input {
            PendingInput::File { path, .. } => format!("File({})", path.display()),
            PendingInput::Url(_) => "Url(<redacted>)".to_owned(),
        };
        f.debug_struct("Pending")
            .field("program", &self.program)
            .field("input", &input)
            .field("fit", &self.fit)
            .field("fps", &self.fps)
            .finish()
    }
}

impl Pending {
    fn into_spec(self, width: u32, height: u32) -> DecoderSpec {
        DecoderSpec {
            program: self.program,
            input: match self.input {
                PendingInput::File { path, looping } => Input::File { path, looping },
                PendingInput::Url(s) => Input::Url(s.expose().to_owned()),
            },
            width,
            height,
            fit: self.fit,
            fps: self.fps,
        }
    }
}

impl Video {
    /// Resolve the source (reading the URL variable now) and wait for the
    /// zone size. A missing variable is logged (by name) and leaves the zone
    /// black; startup and reload reject such configs before this runs
    /// ([`crate::config::Config::check_video_environment`]).
    pub fn new(cfg: &VideoConfig) -> Self {
        let state = match cfg.input() {
            Ok(input) => State::Pending(Pending {
                program: cfg.program.clone().unwrap_or_else(|| "ffmpeg".into()),
                input: match input {
                    VideoInput::File(path) => PendingInput::File {
                        path,
                        looping: cfg.looping,
                    },
                    VideoInput::Url(s) => PendingInput::Url(s),
                },
                fit: cfg.fit,
                fps: f64::from(cfg.fps),
            }),
            Err(e) => {
                tracing::error!("video widget: {e}");
                State::Failed
            }
        };
        Video {
            state: RefCell::new(state),
            frame: None,
        }
    }

    /// A widget on an existing handle (tests, in-process sources).
    pub fn with_handle(handle: VideoHandle) -> Self {
        Video {
            state: RefCell::new(State::Running(handle)),
            frame: None,
        }
    }

    /// Start the decoder if this is the first call.
    fn ensure_started(&self, w: u32, h: u32) {
        let mut state = self.state.borrow_mut();
        if matches!(&*state, State::Pending(_)) {
            let State::Pending(p) = std::mem::replace(&mut *state, State::Failed) else {
                unreachable!()
            };
            *state = State::Running(VideoHandle::spawn(p.into_spec(w, h)));
        }
    }
}

impl Widget for Video {
    fn update(&mut self, _ctx: &FrameCtx, _store: &StoreSnapshot) {
        // The zone size is unknown here; `prepare` or `draw` starts the
        // decoder. Until then there is nothing to touch.
        if let State::Running(h) = &*self.state.borrow() {
            h.touch();
            self.frame = h.latest();
        }
    }

    fn prepare(&mut self, zone_w: u32, zone_h: u32) {
        self.ensure_started(zone_w, zone_h);
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width(), target.height());
        // `prepare` normally did this; if it was skipped, start now (cheap:
        // the decoder runs `ffmpeg` only after the first touch, next update).
        self.ensure_started(w, h);
        let frame = self
            .frame
            .as_deref()
            .filter(|f| f.width == w && f.height == h && f.rgb.len() == 3 * (w * h) as usize);
        let Some(frame) = frame else {
            target.fill(Rgb888::BLACK);
            return;
        };
        target.blit_rgb(&frame.rgb);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::DateTime;

    use super::*;
    use crate::canvas::{Canvas, Rect};
    use crate::store::Store;
    use crate::video::IDLE_STOP;

    fn ctx() -> FrameCtx {
        FrameCtx {
            now: DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
            scene_elapsed: Duration::ZERO,
            frame: 0,
        }
    }

    fn frame(w: u32, h: u32, seq: u64) -> Frame {
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.extend([(x * 10) as u8, (y * 10) as u8, seq as u8]);
            }
        }
        Frame {
            width: w,
            height: h,
            rgb,
            seq,
        }
    }

    fn render(widget: &mut Video, canvas: &mut Canvas, rect: Rect, bg: Rgb888) {
        widget.update(&ctx(), &Store::new().snapshot());
        let mut t = canvas.zone(rect);
        t.fill(bg);
        widget.draw(&mut t);
    }

    const RECT: Rect = Rect {
        x: 3,
        y: 2,
        w: 5,
        h: 4,
    };

    #[test]
    fn no_frame_is_black_even_over_a_background() {
        let mut w = Video::with_handle(VideoHandle::detached());
        let mut c = Canvas::new(12, 8);
        render(&mut w, &mut c, RECT, Rgb888::new(9, 9, 9));
        assert!(c.pixels().iter().all(|p| *p == Rgb888::BLACK));
    }

    #[test]
    fn a_fed_frame_is_blitted_exactly_and_clipped_to_the_zone() {
        let h = VideoHandle::detached();
        h.shared().slot.put(frame(5, 4, 1));
        let mut w = Video::with_handle(h);
        let mut c = Canvas::new(12, 8);
        render(&mut w, &mut c, RECT, Rgb888::BLACK);
        for y in 0..8 {
            for x in 0..12 {
                let inside = (3..8).contains(&x) && (2..6).contains(&y);
                let want = if inside {
                    Rgb888::new(((x - 3) * 10) as u8, ((y - 2) * 10) as u8, 1)
                } else {
                    Rgb888::BLACK
                };
                assert_eq!(c.pixel(x, y), Some(want), "({x},{y})");
            }
        }
    }

    #[test]
    fn newer_frames_replace_older_ones() {
        let h = VideoHandle::detached();
        let shared = h.shared().clone();
        shared.slot.put(frame(5, 4, 1));
        let mut w = Video::with_handle(h);
        let mut c = Canvas::new(12, 8);
        render(&mut w, &mut c, RECT, Rgb888::BLACK);
        let first = c.clone();
        shared.slot.put(frame(5, 4, 2));
        render(&mut w, &mut c, RECT, Rgb888::BLACK);
        assert_ne!(c, first);
        assert_eq!(c.pixel(3, 2), Some(Rgb888::new(0, 0, 2)));
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_ignored() {
        let h = VideoHandle::detached();
        h.shared().slot.put(frame(4, 4, 1));
        let mut w = Video::with_handle(h);
        let mut c = Canvas::new(12, 8);
        render(&mut w, &mut c, RECT, Rgb888::new(9, 9, 9));
        assert!(c.pixels().iter().all(|p| *p == Rgb888::BLACK));
    }

    #[test]
    fn update_touches_the_decoder_and_draw_alone_does_not() {
        let h = VideoHandle::detached();
        let shared = h.shared().clone();
        let mut w = Video::with_handle(h);
        let mut c = Canvas::new(12, 8);
        let mut t = c.zone(RECT);
        w.draw(&mut t);
        assert!(!shared.wanted(IDLE_STOP), "draw must not touch");
        w.update(&ctx(), &Store::new().snapshot());
        assert!(shared.wanted(IDLE_STOP), "update touches");
    }
}
