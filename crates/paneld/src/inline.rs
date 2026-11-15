//! The scene an inline notification (text and optional icon, DESIGN.md
//! §5.5) is drawn with. It is made of the ordinary widgets, so the text
//! machinery (fonts, scrolling) is the one scenes use.

use crate::canvas::Rect;
use crate::color::Color;
use crate::config::{Align, NotificationsConfig, TextConfig};
use crate::fonts::{self, Font, FontFace};
use crate::icons::{self, ICON_SIZE};
use crate::notifications::InlineNotice;
use crate::scene::{Scene, Zone};
use crate::widgets::{IconWidget, Text};

/// Pixels kept free at the left and right edge.
const MARGIN: u32 = 2;
/// Pixels between the icon and the text.
const GAP: u32 = 4;
/// Minimum scroll speed of text that is too wide, in pixels per second.
pub const SCROLL_PX_PER_S: f32 = 40.0;
/// Scrolling text moves about this many characters per second, so a larger
/// font does not read slower.
const SCROLL_CHARS_PER_S: f32 = 3.0;
/// Scrolling text uses the largest font at most this fraction of the canvas
/// height (1/6: about 28 px on a 172 px canvas). Big enough to read from
/// across a room, small enough that a line scrolls past in reasonable time.
const SCROLL_HEIGHT_DIVISOR: u32 = 6;
/// With no `[notifications] font`, only built-in fonts at least this tall
/// are considered: smaller text is unreadable from a distance.
const MIN_LINE_HEIGHT: u32 = 13;

/// The size an icon is drawn at next to text of `line_height`.
fn icon_scale(line_height: u32) -> u32 {
    ((line_height + ICON_SIZE / 2) / ICON_SIZE).max(1)
}

/// Build the full-screen scene for `notice`: icon and text centred on the
/// canvas in the notice's colour over the configured background, in the
/// largest font that fits. Text too wide even for the smallest font
/// scrolls beside a fixed icon, in the largest font no taller than a
/// sixth of the canvas height.
pub fn scene(cfg: &NotificationsConfig, width: u32, height: u32, notice: &InlineNotice) -> Scene {
    let icon = notice.icon.as_deref().and_then(icons::lookup);
    let color = notice.color.0;
    let avail_w = width.saturating_sub(2 * MARGIN);
    let icon_w_for = |font: &Font| {
        icon.map_or(0, |_| {
            IconWidget::pixel_size(icon_scale(font.line_height())) + GAP
        })
    };
    let group_w = |font: &Font| icon_w_for(font) + font.text_width(&notice.text);
    let fits = |font: &Font| font.line_height() <= height && group_w(font) <= avail_w;

    let explicit = cfg
        .font
        .as_deref()
        .and_then(|n| cfg.face.clone().or_else(|| FontFace::builtin(n)));
    let mut candidates: Vec<Font> = match (explicit, cfg.font_scale) {
        (Some(face), Some(s)) => vec![Font::new(face, s)],
        (Some(face), None) => (1..=fonts::MAX_FONT_SCALE)
            .map(|s| Font::new(face.clone(), s))
            .collect(),
        _ => fonts::FONTS
            .iter()
            .filter(|(_, f)| f.character_size.height >= MIN_LINE_HEIGHT)
            .flat_map(|(_, f)| (1..=fonts::MAX_FONT_SCALE).map(|s| Font::new(FontFace::Mono(f), s)))
            .collect(),
    };
    // Smallest first; on equal height the wider glyphs sort later.
    candidates.sort_by_key(|f| (f.line_height(), f.text_width("0")));
    let best = candidates.iter().rev().find(|f| fits(f));
    let (font, scrolls) = match best {
        Some(f) => (f.clone(), false),
        None => {
            let max_h = (height / SCROLL_HEIGHT_DIVISOR).max(1);
            let f = candidates
                .iter()
                .rev()
                .find(|f| f.line_height() <= max_h)
                .or(candidates.first())
                .expect("candidates")
                .clone();
            (f, true)
        }
    };
    let scroll_speed = SCROLL_PX_PER_S.max(SCROLL_CHARS_PER_S * font.text_width("0") as f32);

    let scale = icon_scale(font.line_height());
    let icon_px = IconWidget::pixel_size(scale);
    let icon_w = icon_w_for(&font);
    let text_widget = |text: &str, scroll: Option<f32>| {
        let mut t = Text::new(&TextConfig {
            text: String::new(),
            font: String::new(),
            font_scale: font.scale(),
            face: Some(font.face().clone()),
            color: Color(color),
            align: Align::Left,
            scroll_px_per_s: scroll,
        });
        // Set as a literal, not parsed as a template: braces stay as typed.
        t.set_text(text.to_owned());
        t
    };

    let mut zones = vec![Zone::new(
        Rect::new(0, 0, width, height),
        Some(cfg.background.0),
        Box::new(text_widget("", None)),
    )];
    let (x0, text_x, text_w) = if scrolls {
        (MARGIN, MARGIN + icon_w, avail_w.saturating_sub(icon_w))
    } else {
        let x0 = width.saturating_sub(group_w(&font)) / 2;
        (x0, x0 + icon_w, font.text_width(&notice.text))
    };
    if let Some(icon) = icon {
        zones.push(Zone::new(
            Rect::new(x0, 0, icon_px, height),
            None,
            Box::new(IconWidget::new(icon, color, scale)),
        ));
    }
    zones.push(Zone::new(
        Rect::new(text_x, 0, text_w, height),
        None,
        Box::new(text_widget(&notice.text, scrolls.then_some(scroll_speed))),
    ));
    Scene::from_zones("notification", zones)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Canvas;
    use crate::widgets::FrameCtx;
    use embedded_graphics::pixelcolor::Rgb888;
    use std::time::Duration;

    fn render(cfg: &NotificationsConfig, text: &str, elapsed_s: u64) -> Canvas {
        let notice = InlineNotice {
            text: text.into(),
            icon: Some("bolt".into()),
            color: Color::WHITE,
        };
        let mut scene = scene(cfg, 172, 172, &notice);
        let ctx = FrameCtx {
            now: chrono::DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
            scene_elapsed: Duration::from_secs(elapsed_s),
            frame: 0,
        };
        let mut canvas = Canvas::new(172, 172);
        scene.render(&ctx, &crate::store::Store::new().snapshot(), &mut canvas);
        canvas
    }

    #[test]
    fn short_text_is_static_and_long_text_scrolls() {
        let cfg = NotificationsConfig::default();
        assert_eq!(render(&cfg, "Hi", 0), render(&cfg, "Hi", 5));
        let long = "x".repeat(80);
        assert_ne!(render(&cfg, &long, 1), render(&cfg, &long, 5));
    }

    #[test]
    fn fixed_font_and_background_are_used() {
        let cfg = NotificationsConfig {
            font: Some("9x15".into()),
            font_scale: Some(2),
            background: Color(Rgb888::new(1, 2, 3)),
            face: None,
        };
        let c = render(&cfg, "Hi", 0);
        assert_eq!(c.pixel(0, 0), Some(Rgb888::new(1, 2, 3)));
        assert!(c.pixels().contains(&Rgb888::new(255, 255, 255)));
    }
}
