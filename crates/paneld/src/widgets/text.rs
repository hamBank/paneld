use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::text::Alignment;

use super::{font, FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Align, TextConfig};
use crate::fonts::Font;
use crate::store::StoreSnapshot;

/// Static or scrolling text.
pub struct Text {
    text: String,
    font: Font,
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
            font: font(&cfg.font, &cfg.face, cfg.font_scale),
            color: cfg.color.0,
            align: cfg.align,
            scroll_px_per_s: cfg.scroll_px_per_s,
            travelled: 0,
        }
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
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
        let text_width = self.font.text_width(&self.text);
        let (x, alignment) = match (self.scroll_px_per_s, self.align) {
            // Starts just off the right edge and moves left.
            (Some(_), _) => {
                let cycle = u64::from(target.width() + text_width).max(1);
                (w - (self.travelled % cycle) as i32, Alignment::Left)
            }
            (None, Align::Left) => (0, Alignment::Left),
            (None, Align::Center) => (w / 2, Alignment::Center),
            (None, Align::Right) => (w - 1, Alignment::Right),
        };
        let _ = self
            .font
            .draw(target, &self.text, x, y, alignment, self.color);
    }
}
