//! Widgets draw into a zone (DESIGN.md §5.4).

mod clock;
mod image;
mod text;

use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use embedded_graphics::mono_font::MonoFont;

use crate::canvas::ZoneTarget;
use crate::config::WidgetConfig;
use crate::fonts;
use crate::store::StoreSnapshot;

pub use clock::Clock;
pub use image::Image;
pub use text::Text;

/// Per-frame context. Everything time-dependent comes from here so renders
/// are deterministic for a given context (golden tests rely on this).
#[derive(Debug, Clone, Copy)]
pub struct FrameCtx {
    /// Wall-clock time of this frame, in the display's local offset.
    pub now: DateTime<FixedOffset>,
    /// Time since the current scene became active.
    pub scene_elapsed: Duration,
    /// Frames rendered since start.
    pub frame: u64,
}

pub trait Widget: Send {
    /// Cheap state advance (scroll position, animation frame). No I/O.
    fn update(&mut self, ctx: &FrameCtx, store: &StoreSnapshot);
    /// Optional hook, called with the zone size before the first frame, so
    /// widgets can do one-off sizing work (e.g. pre-scaling images) ahead of
    /// the first rendered frame. Widgets must still cope if it is never called.
    fn prepare(&mut self, _zone_w: u32, _zone_h: u32) {}
    /// Draw into the zone. Drawing is clipped to the zone.
    fn draw(&self, target: &mut ZoneTarget<'_>);
}

/// Build a widget from (already validated) config.
pub fn build(cfg: &WidgetConfig) -> Box<dyn Widget> {
    match cfg {
        WidgetConfig::Clock(c) => Box::new(Clock::new(c)),
        WidgetConfig::Text(t) => Box::new(Text::new(t)),
        WidgetConfig::Image(i) => Box::new(Image::new(i)),
    }
}

fn font(name: &str) -> &'static MonoFont<'static> {
    // Validation rejects unknown fonts; fall back rather than panic anyway.
    fonts::lookup(name)
        .or_else(|| fonts::lookup(fonts::DEFAULT_FONT))
        .expect("default font exists")
}
