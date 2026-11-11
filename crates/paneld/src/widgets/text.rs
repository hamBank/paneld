use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::text::renderer::TextRenderer;
use embedded_graphics::text::{Alignment, Baseline, Text as EgText, TextStyleBuilder};

use super::{font, FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Align, TextConfig};
use crate::store::StoreSnapshot;

/// Static or scrolling text.
pub struct Text {
    text: String,
    font: &'static MonoFont<'static>,
    color: Rgb888,
    align: Align,
    scroll_px_per_s: Option<f32>,
    /// Pixels scrolled since the scene started (unwrapped).
    travelled: u64,
}

impl Text {
    pub fn new(cfg: &TextConfig) -> Self {
        Text {
            text: cfg.text.clone(),
            font: font(&cfg.font),
            color: cfg.color.0,
            align: cfg.align,
            scroll_px_per_s: cfg.scroll_px_per_s,
            travelled: 0,
        }
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    fn style(&self) -> MonoTextStyle<'static, Rgb888> {
        MonoTextStyle::new(self.font, self.color)
    }

    fn text_width(&self) -> u32 {
        self.style()
            .measure_string(&self.text, Point::zero(), Baseline::Middle)
            .bounding_box
            .size
            .width
    }
}

impl Widget for Text {
    fn update(&mut self, ctx: &FrameCtx, _store: &StoreSnapshot) {
        if let Some(speed) = self.scroll_px_per_s {
            // Derive the position from elapsed time, not by accumulating per
            // frame, so dropped frames don't slow the scroll down.
            self.travelled = (ctx.scene_elapsed.as_secs_f64() * f64::from(speed)) as u64;
        }
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width() as i32, target.height() as i32);
        let y = h / 2;
        let (x, alignment) = match (self.scroll_px_per_s, self.align) {
            // Starts just off the right edge and moves left.
            (Some(_), _) => {
                let cycle = u64::from(target.width() + self.text_width()).max(1);
                (w - (self.travelled % cycle) as i32, Alignment::Left)
            }
            (None, Align::Left) => (0, Alignment::Left),
            (None, Align::Center) => (w / 2, Alignment::Center),
            (None, Align::Right) => (w - 1, Alignment::Right),
        };
        let text_style = TextStyleBuilder::new()
            .alignment(alignment)
            .baseline(Baseline::Middle)
            .build();
        let _ = EgText::with_text_style(&self.text, Point::new(x, y), self.style(), text_style)
            .draw(target);
    }
}
