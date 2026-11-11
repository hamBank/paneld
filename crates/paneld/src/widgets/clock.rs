use super::{FrameCtx, Text, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{ClockConfig, TextConfig};
use crate::store::StoreSnapshot;

/// The current time formatted with a `strftime` pattern.
pub struct Clock {
    format: String,
    text: Text,
}

impl Clock {
    pub fn new(cfg: &ClockConfig) -> Self {
        Clock {
            format: cfg.format.clone(),
            text: Text::new(&TextConfig {
                text: String::new(),
                font: cfg.font.clone(),
                color: cfg.color,
                align: cfg.align,
                scroll_px_per_s: None,
            }),
        }
    }
}

impl Widget for Clock {
    fn update(&mut self, ctx: &FrameCtx, store: &StoreSnapshot) {
        self.text.set_text(ctx.now.format(&self.format).to_string());
        self.text.update(ctx, store);
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        self.text.draw(target);
    }
}
