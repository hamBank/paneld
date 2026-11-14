use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::text::Alignment;

use super::{font, FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Align, TextConfig};
use crate::fonts::Font;
use crate::store::StoreSnapshot;
use crate::template::Template;

/// Static or scrolling text.
pub struct Text {
    /// The text to draw: the literal, or the last rendering of `template`.
    text: String,
    /// Width of `text` in pixels, recomputed only when `text` changes.
    text_width: u32,
    /// Parsed `{key}` template; `None` for plain text. Validation rejects bad
    /// templates, so one that fails to parse here is shown literally.
    template: Option<Template>,
    /// Fingerprint of the inputs `text` was rendered from.
    fingerprint: Option<u64>,
    /// How many times the template was rendered (tests check it stays put
    /// while the inputs do).
    renders: u64,
    font: Font,
    color: Rgb888,
    align: Align,
    scroll_px_per_s: Option<f32>,
    /// Pixels scrolled since the scene started (unwrapped).
    travelled: u64,
}

impl Text {
    pub fn new(cfg: &TextConfig) -> Self {
        let font = font(&cfg.font, &cfg.face, cfg.font_scale);
        // Static templates (no placeholders) are rendered once, here, so `{{`
        // and `}}` come out as single braces.
        let (template, text) = match Template::parse(&cfg.text) {
            Ok(t) if t.is_static() => (None, t.render(&StoreSnapshot::default())),
            Ok(t) => (Some(t), String::new()),
            Err(_) => (None, cfg.text.clone()),
        };
        let text_width = font.text_width(&text);
        Text {
            text,
            text_width,
            template,
            fingerprint: None,
            renders: 0,
            font,
            color: cfg.color.0,
            align: cfg.align,
            scroll_px_per_s: cfg.scroll_px_per_s,
            travelled: 0,
        }
    }

    /// Replace the text with a literal (used by the clock).
    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.text_width = self.font.text_width(&self.text);
    }

    /// The text as currently rendered.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Times the template has been rendered so far.
    pub fn render_count(&self) -> u64 {
        self.renders
    }
}

impl Widget for Text {
    fn update(&mut self, ctx: &FrameCtx, store: &StoreSnapshot) {
        if let Some(t) = &self.template {
            // Re-render only when something the template shows has changed,
            // reusing the String's buffer.
            let fp = t.fingerprint(store);
            if self.fingerprint != Some(fp) {
                self.fingerprint = Some(fp);
                self.text.clear();
                t.render_into(store, &mut self.text);
                self.text_width = self.font.text_width(&self.text);
                self.renders += 1;
            }
        }
        if let Some(speed) = self.scroll_px_per_s {
            // Derive the position from elapsed time, not by accumulating per
            // frame, so dropped frames don't slow the scroll down.
            self.travelled = (ctx.scene_elapsed.as_secs_f64() * f64::from(speed)) as u64;
        }
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width() as i32, target.height() as i32);
        let y = h / 2;
        let text_width = self.text_width;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entry, Value};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn ctx() -> FrameCtx {
        FrameCtx {
            now: chrono::DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
            scene_elapsed: Duration::ZERO,
            frame: 0,
        }
    }

    fn store(v: f64, other: f64) -> StoreSnapshot {
        let e = |value| Entry {
            value,
            updated_at: SystemTime::UNIX_EPOCH,
            source: "test".into(),
        };
        let mut m = HashMap::new();
        m.insert(
            "ha:sensor.outdoor_temperature".to_owned(),
            e(Value::Number {
                value: v,
                unit: None,
            }),
        );
        m.insert(
            "ha:sensor.unrelated".to_owned(),
            e(Value::Number {
                value: other,
                unit: None,
            }),
        );
        Arc::new(m)
    }

    fn widget(text: &str) -> Text {
        let cfg: TextConfig = toml::from_str(&format!("text = {text:?}")).unwrap();
        Text::new(&cfg)
    }

    #[test]
    fn unchanged_inputs_do_not_re_render_or_reallocate() {
        let mut w = widget("{ha:sensor.outdoor_temperature:.1}°");
        w.update(&ctx(), &store(21.5, 1.0));
        assert_eq!(w.text(), "21.5°");
        assert_eq!(w.render_count(), 1);
        let ptr = w.text().as_ptr();

        // New snapshots with the same shown values, and a change to an
        // unrelated key: nothing to do.
        for other in [1.0, 2.0, 3.0] {
            w.update(&ctx(), &store(21.5, other));
        }
        assert_eq!(w.render_count(), 1);
        assert_eq!(w.text().as_ptr(), ptr);

        // A real change re-renders into the same buffer.
        w.update(&ctx(), &store(22.0, 1.0));
        assert_eq!(w.text(), "22.0°");
        assert_eq!(w.render_count(), 2);
        assert_eq!(w.text().as_ptr(), ptr);
    }

    #[test]
    fn plain_text_is_never_rendered_and_braces_are_unescaped() {
        let mut w = widget("Hello");
        w.update(&ctx(), &store(1.0, 1.0));
        assert_eq!((w.text(), w.render_count()), ("Hello", 0));
        let w = widget("a {{b}}");
        assert_eq!(w.text(), "a {b}");
    }

    #[test]
    fn width_follows_the_rendered_text() {
        let mut w = widget("{ha:sensor.outdoor_temperature}");
        w.update(&ctx(), &store(5.0, 0.0));
        let narrow = w.text_width;
        w.update(&ctx(), &store(12345.0, 0.0));
        assert!(w.text_width > narrow);
    }
}
