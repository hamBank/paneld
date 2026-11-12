//! Fonts usable from config: the built-in `embedded-graphics` mono fonts and
//! BDF fonts loaded at config time (DESIGN.md §5.4), behind one [`Font`] type
//! that measures and draws text at an integer scale.
//!
//! Built-in names are the ISO-8859-1 mono fonts (so `°` works). Loaded fonts
//! are declared with `[[font]]` and get their names from there.
//!
//! The text layout contract, identical for both kinds of font:
//! - the line box is `line_height * scale` pixels tall and is centred
//!   vertically on the requested y (for a mono font at scale 1 this is the
//!   same placement as `Baseline::Middle`);
//! - the width is the sum of the glyph advances (mono: `n * (width +
//!   spacing) - spacing`), times the scale;
//! - `Alignment::Right` puts the last pixel column of the text's width at x
//!   and `Center` puts the middle there, the way `embedded-graphics` does;
//! - a character the font lacks is drawn as `?` if the font has one, else
//!   it leaves a blank of the font's default advance (BDF only; the built-in
//!   fonts already substitute `?`).

use std::fmt;
use std::sync::Arc;

use embedded_graphics::mono_font::{iso_8859_1 as f, MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};

use crate::bdf::BdfFont;

/// Largest accepted `font_scale`.
pub const MAX_FONT_SCALE: u32 = 8;

/// All built-in fonts by config name, e.g. `font = "6x10"`.
pub const FONTS: &[(&str, &MonoFont<'static>)] = &[
    ("4x6", &f::FONT_4X6),
    ("5x7", &f::FONT_5X7),
    ("5x8", &f::FONT_5X8),
    ("6x9", &f::FONT_6X9),
    ("6x10", &f::FONT_6X10),
    ("6x12", &f::FONT_6X12),
    ("6x13", &f::FONT_6X13),
    ("6x13_bold", &f::FONT_6X13_BOLD),
    ("7x13", &f::FONT_7X13),
    ("7x13_bold", &f::FONT_7X13_BOLD),
    ("7x14", &f::FONT_7X14),
    ("7x14_bold", &f::FONT_7X14_BOLD),
    ("8x13", &f::FONT_8X13),
    ("8x13_bold", &f::FONT_8X13_BOLD),
    ("9x15", &f::FONT_9X15),
    ("9x15_bold", &f::FONT_9X15_BOLD),
    ("9x18", &f::FONT_9X18),
    ("9x18_bold", &f::FONT_9X18_BOLD),
    ("10x20", &f::FONT_10X20),
];

pub const DEFAULT_FONT: &str = "6x10";

/// Look up a built-in font by its config name.
pub fn lookup(name: &str) -> Option<&'static MonoFont<'static>> {
    FONTS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

/// Comma-separated list of known font names (built-in, then `extra`, the
/// names of fonts loaded from config), for error messages.
pub fn known_names(extra: &[&str]) -> String {
    FONTS
        .iter()
        .map(|(n, _)| *n)
        .chain(extra.iter().copied())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A font's glyph data, without a scale.
#[derive(Clone)]
pub enum FontFace {
    Mono(&'static MonoFont<'static>),
    Bdf(Arc<BdfFont>),
}

impl fmt::Debug for FontFace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FontFace::Mono(_) => f.write_str("FontFace::Mono"),
            FontFace::Bdf(b) => write!(f, "FontFace::Bdf({} glyphs)", b.glyph_count()),
        }
    }
}

impl FontFace {
    /// The built-in font called `name`.
    pub fn builtin(name: &str) -> Option<FontFace> {
        lookup(name).map(FontFace::Mono)
    }
}

/// A font at an integer scale: every font pixel becomes `scale`×`scale`
/// panel pixels.
#[derive(Debug, Clone)]
pub struct Font {
    face: FontFace,
    scale: u32,
}

impl Font {
    /// `scale` is clamped to `1..=MAX_FONT_SCALE` (config validation rejects
    /// anything else; this just avoids a panic).
    pub fn new(face: FontFace, scale: u32) -> Font {
        Font {
            face,
            scale: scale.clamp(1, MAX_FONT_SCALE),
        }
    }

    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Height of the line box in panel pixels.
    pub fn line_height(&self) -> u32 {
        let h = match &self.face {
            FontFace::Mono(m) => m.character_size.height,
            FontFace::Bdf(b) => b.line_height(),
        };
        h * self.scale
    }

    /// Width of `text` in panel pixels.
    pub fn text_width(&self, text: &str) -> u32 {
        let w = match &self.face {
            FontFace::Mono(m) => {
                let step = m.character_size.width + m.character_spacing;
                (text.chars().count() as u32 * step).saturating_sub(m.character_spacing)
            }
            FontFace::Bdf(b) => text
                .chars()
                .map(|c| b.advance(c))
                .sum::<i32>()
                .max(0)
                .unsigned_abs(),
        };
        w * self.scale
    }

    /// Draw `text` on one line. `x` is interpreted per `alignment` and `cy`
    /// is the vertical centre of the line box. Anything outside the target
    /// is clipped by the target.
    pub fn draw<D>(
        &self,
        target: &mut D,
        text: &str,
        x: i32,
        cy: i32,
        alignment: Alignment,
        color: Rgb888,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let width = self.text_width(text) as i32;
        let left = match alignment {
            Alignment::Left => x,
            Alignment::Center => x - (width - 1) / 2,
            Alignment::Right => x - (width - 1),
        };
        let top = cy - (self.line_height() as i32 - 1) / 2;
        let mut scaled = ScaleTarget {
            inner: target,
            origin: Point::new(left, top),
            scale: self.scale as i32,
        };
        match &self.face {
            FontFace::Mono(m) => {
                let style = MonoTextStyle::new(m, color);
                let text_style = TextStyleBuilder::new()
                    .alignment(Alignment::Left)
                    .baseline(Baseline::Top)
                    .build();
                Text::with_text_style(text, Point::zero(), style, text_style).draw(&mut scaled)?;
            }
            FontFace::Bdf(b) => {
                let baseline = b.ascent as i32;
                let mut pen = 0;
                let mut placed = Vec::new();
                for c in text.chars() {
                    if let Some(g) = b.glyph_or_fallback(c) {
                        placed.push((pen, g));
                    }
                    pen += b.advance(c);
                }
                scaled.draw_iter(placed.into_iter().flat_map(|(pen, g)| {
                    g.pixels()
                        .map(move |(x, y)| Pixel(Point::new(pen + x, baseline + y), color))
                }))?;
            }
        }
        Ok(())
    }
}

/// Draws font pixels (one unit = one font pixel) onto `inner` as
/// `scale`×`scale` blocks, with the font's (0, 0) at `origin` on `inner`.
struct ScaleTarget<'a, D> {
    inner: &'a mut D,
    origin: Point,
    scale: i32,
}

impl<D> Dimensions for ScaleTarget<'_, D> {
    fn bounding_box(&self) -> Rectangle {
        // Clipping is the inner target's job. This must not clip anything
        // itself, because default `fill_contiguous` implementations clip to it.
        Rectangle::new(
            Point::new(-(1 << 20), -(1 << 20)),
            Size::new(1 << 21, 1 << 21),
        )
    }
}

impl<D: DrawTarget<Color = Rgb888>> DrawTarget for ScaleTarget<'_, D> {
    type Color = Rgb888;
    type Error = D::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Rgb888>>,
    {
        let (origin, s) = (self.origin, self.scale);
        self.inner
            .draw_iter(pixels.into_iter().flat_map(move |Pixel(p, c)| {
                let base = origin + p * s;
                (0..s).flat_map(move |dy| (0..s).map(move |dx| Pixel(base + Point::new(dx, dy), c)))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{Canvas, Rect};

    const TINY: &str = "STARTFONT 2.1
FONTBOUNDINGBOX 4 3 0 0
FONT_ASCENT 3
FONT_DESCENT 0
STARTCHAR i
ENCODING 105
DWIDTH 2 0
BBX 1 3 0 0
BITMAP
80
80
80
ENDCHAR
STARTCHAR M
ENCODING 77
DWIDTH 4 0
BBX 3 3 0 0
BITMAP
E0
A0
A0
ENDCHAR
ENDFONT
";

    fn tiny(scale: u32) -> Font {
        Font::new(
            FontFace::Bdf(Arc::new(BdfFont::parse(TINY).unwrap())),
            scale,
        )
    }

    fn mono(name: &str, scale: u32) -> Font {
        Font::new(FontFace::builtin(name).unwrap(), scale)
    }

    #[test]
    fn default_font_exists() {
        assert!(lookup(DEFAULT_FONT).is_some());
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn known_names_lists_builtin_then_loaded() {
        let s = known_names(&["big", "small"]);
        assert!(s.starts_with("4x6, 5x7"));
        assert!(s.ends_with("10x20, big, small"));
    }

    #[test]
    fn measures_mono_and_bdf_with_scale() {
        assert_eq!(mono("6x10", 1).text_width("abc"), 18);
        assert_eq!(mono("6x10", 3).text_width("abc"), 54);
        assert_eq!(mono("6x10", 3).line_height(), 30);
        assert_eq!(mono("6x10", 1).text_width(""), 0);
        // Proportional: i = 2, M = 4, unknown = default advance (bbox width 4).
        assert_eq!(tiny(1).text_width("iMi"), 8);
        assert_eq!(tiny(2).text_width("iMi"), 16);
        assert_eq!(tiny(1).text_width("iZ"), 6);
        assert_eq!(tiny(5).line_height(), 15);
    }

    fn lit(c: &Canvas) -> Vec<(u32, u32)> {
        let mut v = Vec::new();
        for y in 0..c.height() {
            for x in 0..c.width() {
                if c.pixel(x, y) != Some(Rgb888::BLACK) {
                    v.push((x, y));
                }
            }
        }
        v
    }

    #[test]
    fn scaling_expands_each_pixel_to_a_block() {
        let mut c = Canvas::new(20, 10);
        let mut z = c.zone(Rect::new(0, 0, 20, 10));
        // Line box is 3 * 2 = 6 high: centred on y = 5 it starts at 5 - 2 = 3.
        tiny(2)
            .draw(&mut z, "i", 1, 5, Alignment::Left, Rgb888::WHITE)
            .unwrap();
        let px = lit(&c);
        assert_eq!(px.len(), 3 * 4, "three font pixels, 2x2 each");
        assert!(px
            .iter()
            .all(|&(x, y)| (1..3).contains(&x) && (3..9).contains(&y)));
    }

    #[test]
    fn right_and_center_alignment_use_the_measured_width() {
        let width_of = |a, x| {
            let mut c = Canvas::new(30, 6);
            let mut z = c.zone(Rect::new(0, 0, 30, 6));
            tiny(1).draw(&mut z, "iM", x, 3, a, Rgb888::WHITE).unwrap();
            let px = lit(&c);
            (
                px.iter().map(|p| p.0).min().unwrap(),
                px.iter().map(|p| p.0).max().unwrap(),
            )
        };
        // "iM" is 6 wide: 'i' at column 0, 'M' at columns 2..=4.
        assert_eq!(width_of(Alignment::Left, 4), (4, 8));
        assert_eq!(width_of(Alignment::Right, 20), (15, 19));
        assert_eq!(width_of(Alignment::Center, 15), (13, 17));
    }

    #[test]
    fn missing_chars_leave_a_gap_without_a_question_mark_glyph() {
        let mut c = Canvas::new(30, 6);
        let mut z = c.zone(Rect::new(0, 0, 30, 6));
        tiny(1)
            .draw(&mut z, "Zi", 0, 3, Alignment::Left, Rgb888::WHITE)
            .unwrap();
        // Z blanks 4 columns, then 'i' is at x = 4.
        assert!(lit(&c).iter().all(|&(x, _)| x == 4));
    }
}
