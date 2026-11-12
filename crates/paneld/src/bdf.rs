//! A small BDF (Glyph Bitmap Distribution Format) parser, for runtime font
//! loading (DESIGN.md §5.4).
//!
//! Supported: `FONTBOUNDINGBOX`, `FONT_ASCENT`, `FONT_DESCENT`, and per glyph
//! `STARTCHAR`, `ENCODING`, `DWIDTH` (so proportional fonts work), `BBX` and
//! `BITMAP`. Everything else (`SWIDTH`, `COMMENT`, other properties, the
//! vertical-writing keywords) is ignored. `ENCODING` is read as a Unicode
//! code point, which is what ISO10646 fonts (Spleen, Terminus, Cozette) use
//! and which equals Latin-1 for the first 256 characters. Glyphs without an
//! encoding (`ENCODING -1`) are skipped.
//!
//! Errors carry the 1-based line number of the offending line.

use std::collections::HashMap;
use std::fmt;

/// Largest glyph dimension accepted, to keep a corrupt file from allocating
/// absurd amounts of memory.
const MAX_GLYPH_DIM: i64 = 512;

/// A parse failure, with the line it happened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BdfError {
    /// 1-based line number; 0 when the problem is the file as a whole.
    pub line: usize,
    pub message: String,
}

impl fmt::Display for BdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for BdfError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, BdfError> {
    Err(BdfError {
        line,
        message: message.into(),
    })
}

/// One glyph: a bitmap plus its placement relative to the pen and baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    /// Pen advance in pixels (`DWIDTH`).
    pub advance: i32,
    pub width: u32,
    pub height: u32,
    /// Offset of the bitmap's bottom-left corner from the pen, y up (`BBX`).
    pub x_off: i32,
    pub y_off: i32,
    /// Row-major, MSB first, each row padded to whole bytes.
    bits: Vec<u8>,
}

impl Glyph {
    fn stride(&self) -> usize {
        (self.width as usize).div_ceil(8)
    }

    /// Set pixels as `(x, y)` relative to the pen position on the baseline,
    /// y pointing down (so glyphs above the baseline have negative y).
    pub fn pixels(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        let stride = self.stride();
        let top = -(self.y_off + self.height as i32);
        (0..self.height as usize).flat_map(move |row| {
            (0..self.width as usize).filter_map(move |col| {
                let byte = self.bits[row * stride + col / 8];
                (byte & (0x80 >> (col % 8)) != 0)
                    .then_some((self.x_off + col as i32, top + row as i32))
            })
        })
    }
}

/// A parsed bitmap font.
#[derive(Debug, Clone)]
pub struct BdfFont {
    glyphs: HashMap<u32, Glyph>,
    /// Pixels above the baseline / below it, for the line box.
    pub ascent: u32,
    pub descent: u32,
    /// Advance used for a character that has no glyph and no `?` to fall back to.
    pub default_advance: u32,
}

impl BdfFont {
    pub fn parse(text: &str) -> Result<BdfFont, BdfError> {
        let mut bbox: Option<(i64, i64, i64, i64)> = None;
        let mut ascent: Option<i64> = None;
        let mut descent: Option<i64> = None;
        let mut glyphs: HashMap<u32, Glyph> = HashMap::new();
        let mut cur: Option<GlyphBuilder> = None;
        let mut last_line = 0;

        for (idx, raw) in text.lines().enumerate() {
            let n = idx + 1;
            last_line = n;
            let line = raw.trim();
            if n == 1 && !line.starts_with("STARTFONT") {
                return err(1, "not a BDF file (first line must be STARTFONT)");
            }
            if line.is_empty() {
                continue;
            }
            // Inside a BITMAP, every line is a row of hex digits.
            if let Some(g) = cur.as_mut().filter(|g| g.in_bitmap) {
                if line != "ENDCHAR" {
                    g.push_row(n, line)?;
                    continue;
                }
            }
            let (key, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            match key {
                "FONTBOUNDINGBOX" if cur.is_none() => {
                    let v = ints(n, key, rest, 4)?;
                    if v[0] <= 0 || v[1] <= 0 || v[0] > MAX_GLYPH_DIM || v[1] > MAX_GLYPH_DIM {
                        return err(
                            n,
                            format!("FONTBOUNDINGBOX size {}x{} out of range", v[0], v[1]),
                        );
                    }
                    bbox = Some((v[0], v[1], v[2], v[3]));
                }
                "FONT_ASCENT" if cur.is_none() => ascent = Some(ints(n, key, rest, 1)?[0]),
                "FONT_DESCENT" if cur.is_none() => descent = Some(ints(n, key, rest, 1)?[0]),
                "STARTCHAR" => {
                    if cur.is_some() {
                        return err(n, "STARTCHAR before the previous glyph's ENDCHAR");
                    }
                    cur = Some(GlyphBuilder::new(n, rest));
                }
                "ENCODING" => {
                    let g = in_glyph(&mut cur, n, key)?;
                    let v = ints_at_least(n, key, rest, 1)?;
                    g.encoding = Some(v[0]);
                }
                "DWIDTH" => {
                    let g = in_glyph(&mut cur, n, key)?;
                    g.advance = Some(ints(n, key, rest, 2)?[0]);
                }
                "BBX" => {
                    let g = in_glyph(&mut cur, n, key)?;
                    let v = ints(n, key, rest, 4)?;
                    if !(0..=MAX_GLYPH_DIM).contains(&v[0]) || !(0..=MAX_GLYPH_DIM).contains(&v[1])
                    {
                        return err(n, format!("BBX size {}x{} out of range", v[0], v[1]));
                    }
                    g.bbx = Some((
                        v[0] as u32,
                        v[1] as u32,
                        v[2].clamp(-MAX_GLYPH_DIM, MAX_GLYPH_DIM) as i32,
                        v[3].clamp(-MAX_GLYPH_DIM, MAX_GLYPH_DIM) as i32,
                    ));
                }
                "BITMAP" => {
                    let g = in_glyph(&mut cur, n, key)?;
                    if g.bbx.is_none() {
                        return err(n, "BITMAP before BBX");
                    }
                    g.in_bitmap = true;
                }
                "ENDCHAR" => {
                    let Some(g) = cur.take() else {
                        return err(n, "ENDCHAR without STARTCHAR");
                    };
                    if let Some((code, glyph)) = g.finish(n)? {
                        glyphs.insert(code, glyph);
                    }
                }
                "ENDFONT" => {
                    if cur.is_some() {
                        return err(n, "ENDFONT inside a glyph (missing ENDCHAR)");
                    }
                    break;
                }
                _ => {}
            }
        }
        if let Some(g) = &cur {
            return err(
                last_line,
                format!(
                    "unexpected end of file inside glyph {:?} (started on line {})",
                    g.name, g.start
                ),
            );
        }
        let Some((bw, bh, _, by)) = bbox else {
            return err(0, "missing FONTBOUNDINGBOX");
        };
        if glyphs.is_empty() {
            return err(0, "font has no glyphs");
        }
        // Fall back to the bounding box when the properties are absent.
        let ascent = ascent.unwrap_or(bh + by).clamp(0, MAX_GLYPH_DIM) as u32;
        let descent = descent.unwrap_or(-by).clamp(0, MAX_GLYPH_DIM) as u32;
        if ascent + descent == 0 {
            return err(
                0,
                "font has no height (FONT_ASCENT and FONT_DESCENT are both 0)",
            );
        }
        Ok(BdfFont {
            glyphs,
            ascent,
            descent,
            default_advance: bw as u32,
        })
    }

    /// The glyph for `c`, if the font has one.
    pub fn glyph(&self, c: char) -> Option<&Glyph> {
        self.glyphs.get(&(c as u32))
    }

    /// The glyph drawn for `c`: its own, else `?`, else none (blank).
    pub fn glyph_or_fallback(&self, c: char) -> Option<&Glyph> {
        self.glyph(c).or_else(|| self.glyph('?'))
    }

    /// Pen advance for `c`, following [`BdfFont::glyph_or_fallback`].
    pub fn advance(&self, c: char) -> i32 {
        self.glyph_or_fallback(c)
            .map_or(self.default_advance as i32, |g| g.advance)
    }

    pub fn line_height(&self) -> u32 {
        self.ascent + self.descent
    }

    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

struct GlyphBuilder {
    name: String,
    start: usize,
    encoding: Option<i64>,
    advance: Option<i64>,
    bbx: Option<(u32, u32, i32, i32)>,
    in_bitmap: bool,
    bits: Vec<u8>,
    rows: u32,
}

impl GlyphBuilder {
    fn new(start: usize, name: &str) -> Self {
        GlyphBuilder {
            name: name.to_string(),
            start,
            encoding: None,
            advance: None,
            bbx: None,
            in_bitmap: false,
            bits: Vec::new(),
            rows: 0,
        }
    }

    fn push_row(&mut self, n: usize, hex: &str) -> Result<(), BdfError> {
        let (w, h, _, _) = self.bbx.expect("checked when BITMAP starts");
        if self.rows >= h {
            return err(n, format!("more than {h} bitmap rows (BBX height)"));
        }
        let stride = (w as usize).div_ceil(8);
        if hex.len() != stride * 2 {
            return err(
                n,
                format!(
                    "bitmap row has {} hex digits, expected {} for width {w}",
                    hex.len(),
                    stride * 2
                ),
            );
        }
        for pair in hex.as_bytes().chunks(2) {
            let s = std::str::from_utf8(pair).unwrap_or("");
            match u8::from_str_radix(s, 16) {
                Ok(b) => self.bits.push(b),
                Err(_) => return err(n, format!("invalid hex digits {s:?} in bitmap row")),
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// `None` for a glyph without a character code.
    fn finish(self, n: usize) -> Result<Option<(u32, Glyph)>, BdfError> {
        let Some((w, h, x_off, y_off)) = self.bbx else {
            return err(n, format!("glyph {:?} has no BBX", self.name));
        };
        if !self.in_bitmap {
            return err(n, format!("glyph {:?} has no BITMAP", self.name));
        }
        if self.rows != h {
            return err(
                n,
                format!(
                    "glyph {:?} has {} bitmap rows, BBX says {h}",
                    self.name, self.rows
                ),
            );
        }
        let Some(enc) = self.encoding else {
            return err(n, format!("glyph {:?} has no ENCODING", self.name));
        };
        let Some(adv) = self.advance else {
            return err(n, format!("glyph {:?} has no DWIDTH", self.name));
        };
        let Ok(code) = u32::try_from(enc) else {
            return Ok(None); // ENCODING -1: not addressable by character
        };
        Ok(Some((
            code,
            Glyph {
                advance: adv.clamp(-MAX_GLYPH_DIM, MAX_GLYPH_DIM) as i32,
                width: w,
                height: h,
                x_off,
                y_off,
                bits: self.bits,
            },
        )))
    }
}

fn in_glyph<'a>(
    cur: &'a mut Option<GlyphBuilder>,
    n: usize,
    key: &str,
) -> Result<&'a mut GlyphBuilder, BdfError> {
    match cur {
        Some(g) => Ok(g),
        None => err(n, format!("{key} outside a glyph")),
    }
}

/// Exactly `count` integers.
fn ints(n: usize, key: &str, rest: &str, count: usize) -> Result<Vec<i64>, BdfError> {
    let v = ints_at_least(n, key, rest, count)?;
    if v.len() != count {
        return err(n, format!("{key} takes {count} number(s), got {}", v.len()));
    }
    Ok(v)
}

fn ints_at_least(n: usize, key: &str, rest: &str, count: usize) -> Result<Vec<i64>, BdfError> {
    let v: Result<Vec<i64>, _> = rest.split_whitespace().map(str::parse).collect();
    match v {
        Ok(v) if v.len() >= count => Ok(v),
        Ok(v) => err(n, format!("{key} needs {count} number(s), got {}", v.len())),
        Err(_) => err(n, format!("{key} expects integers, got {rest:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 'i' is narrow (advance 3), 'M' wide (advance 7); no 'Z' and no '?'.
    const TINY: &str = "STARTFONT 2.1
FONT -test-tiny
SIZE 8 75 75
FONTBOUNDINGBOX 7 8 0 -2
STARTPROPERTIES 2
FONT_ASCENT 6
FONT_DESCENT 2
ENDPROPERTIES
CHARS 3
STARTCHAR i
ENCODING 105
SWIDTH 400 0
DWIDTH 3 0
BBX 1 5 1 0
BITMAP
80
80
80
80
80
ENDCHAR
STARTCHAR M
ENCODING 77
DWIDTH 7 0
BBX 7 2 0 0
BITMAP
FE
82
ENDCHAR
STARTCHAR unencoded
ENCODING -1
DWIDTH 1 0
BBX 1 1 0 0
BITMAP
80
ENDCHAR
ENDFONT
";

    fn line_of(needle: &str) -> usize {
        TINY.lines().position(|l| l == needle).unwrap() + 1
    }

    #[test]
    fn parses_a_proportional_font() {
        let f = BdfFont::parse(TINY).unwrap();
        assert_eq!((f.ascent, f.descent, f.line_height()), (6, 2, 8));
        assert_eq!(f.glyph_count(), 2, "the unencoded glyph is skipped");
        assert_eq!(f.advance('i'), 3);
        assert_eq!(f.advance('M'), 7);
        // 'i': one column at x_off = 1, five rows ending at the baseline.
        let px: Vec<_> = f.glyph('i').unwrap().pixels().collect();
        assert_eq!(px, vec![(1, -5), (1, -4), (1, -3), (1, -2), (1, -1)]);
        // 'M': 0xFE = seven set pixels, then 0x82 = first and last.
        let m: Vec<_> = f.glyph('M').unwrap().pixels().collect();
        assert_eq!(m.len(), 9);
        assert_eq!(m[0], (0, -2));
        assert_eq!(m[8], (6, -1));
    }

    #[test]
    fn missing_glyph_has_no_entry_and_uses_the_default_advance() {
        let f = BdfFont::parse(TINY).unwrap();
        assert!(f.glyph('Z').is_none());
        assert!(f.glyph_or_fallback('Z').is_none(), "no '?' in this font");
        assert_eq!(f.advance('Z'), 7, "FONTBOUNDINGBOX width");
    }

    #[test]
    fn missing_glyph_falls_back_to_question_mark() {
        let text = TINY.replace("ENCODING 77", "ENCODING 63");
        let f = BdfFont::parse(&text).unwrap();
        assert_eq!(f.glyph_or_fallback('Z'), f.glyph('?'));
        assert_eq!(f.advance('Z'), 7);
    }

    #[test]
    fn ascent_and_descent_default_to_the_bounding_box() {
        let text = TINY
            .replace("FONT_ASCENT 6\n", "")
            .replace("FONT_DESCENT 2\n", "");
        let f = BdfFont::parse(&text).unwrap();
        assert_eq!((f.ascent, f.descent), (6, 2));
    }

    fn error_of(text: &str) -> String {
        BdfFont::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn errors_carry_line_numbers() {
        assert_eq!(
            error_of("hello"),
            "line 1: not a BDF file (first line must be STARTFONT)"
        );

        let bad = TINY.replacen("80\n80\n80\n80\n80\n", "80\n8Z\n80\n80\n80\n", 1);
        let want = line_of("BITMAP") + 2;
        let e = error_of(&bad);
        assert!(
            e.starts_with(&format!("line {want}: invalid hex digits \"8Z\"")),
            "{e}"
        );

        let short = TINY.replacen("FE\n", "FE00\n", 1);
        let want = line_of("FE");
        let e = error_of(&short);
        assert!(
            e.starts_with(&format!(
                "line {want}: bitmap row has 4 hex digits, expected 2"
            )),
            "{e}"
        );

        let bad_bbx = TINY.replacen("BBX 1 5 1 0", "BBX 1 five 1 0", 1);
        let want = line_of("BBX 1 5 1 0");
        let e = error_of(&bad_bbx);
        assert!(
            e.starts_with(&format!("line {want}: BBX expects integers")),
            "{e}"
        );
    }

    #[test]
    fn structural_errors() {
        let e = error_of(&TINY.replacen("BITMAP\n80\n80\n80\n80\n80\n", "BITMAP\n80\n80\n", 1));
        assert!(e.contains("has 2 bitmap rows, BBX says 5"), "{e}");
        let e = error_of(&TINY.replace("FONTBOUNDINGBOX 7 8 0 -2\n", ""));
        assert_eq!(e, "missing FONTBOUNDINGBOX");
        let e = error_of(&TINY.replacen("DWIDTH 3 0\n", "", 1));
        assert!(e.contains("has no DWIDTH"), "{e}");
        let truncated = &TINY[..TINY.find("FE\n").unwrap()];
        let e = error_of(truncated);
        assert!(
            e.contains("unexpected end of file inside glyph \"M\""),
            "{e}"
        );
        let e = error_of("STARTFONT 2.1\nFONTBOUNDINGBOX 1 1 0 0\nENDFONT\n");
        assert_eq!(e, "font has no glyphs");
    }

    #[test]
    fn absurd_sizes_are_rejected() {
        let e = error_of(&TINY.replacen("BBX 1 5 1 0", "BBX 100000 5 1 0", 1));
        assert!(e.contains("out of range"), "{e}");
    }
}
