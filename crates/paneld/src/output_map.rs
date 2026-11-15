//! The output map (DESIGN.md §4.4): which canvas pixel each pixel of a
//! Colorlight card's display space shows.
//!
//! The engine renders one logical canvas. A card has its own coordinate
//! space (row `y`, pixel offset `x`, as in the `0x55` row packets). An output
//! lists *regions*; each copies a rectangle of the canvas to a place in the
//! card space, optionally flipped and rotated. Without regions there is one
//! implicit region: the whole canvas, unrotated, at card origin.
//!
//! # Semantics
//!
//! A region takes the canvas rectangle `canvas = {x, y, w, h}`, then:
//!
//! 1. **flip** mirrors the rectangle's own content: `horizontal` swaps its
//!    left and right edges, `vertical` swaps top and bottom.
//! 2. **rotate** turns the flipped image clockwise by 0, 90, 180 or 270
//!    degrees. Rotating 90 moves the region's top edge to the card's right
//!    side and its top-left pixel to the top-right corner of the footprint.
//! 3. The result is placed with its top-left corner at `card_offset`. Its
//!    footprint is `w × h`, or `h × w` for 90 and 270.
//!
//! Flip comes first, so `flip` always means "mirror what the region looks
//! like on the canvas", whatever the rotation. For a source pixel `(sx, sy)`
//! of a `w × h` region (relative to the region), after flip `(fx, fy)`, the
//! footprint pixel `(u, v)` is:
//!
//! | rotate | `(u, v)`                   |
//! |--------|----------------------------|
//! | 0      | `(fx, fy)`                 |
//! | 90     | `(h - 1 - fy, fx)`         |
//! | 180    | `(w - 1 - fx, h - 1 - fy)` |
//! | 270    | `(fy, w - 1 - fx)`         |
//!
//! Regions may not overlap in card space (that would be ambiguous). They may
//! overlap in canvas space: one canvas area can be shown in two places.
//! Card pixels no region covers are black.
//!
//! # Cost
//!
//! [`OutputMap::new`] builds, once, a table with one `u32` per card pixel:
//! the canvas pixel index it shows, or [`NONE`]. Mapping a frame is then one
//! gather pass over a reused buffer ([`OutputMap::gather_into`]). The sink
//! maps before gamma, white point and colour order: those are per-pixel
//! LUTs, so they cost the same either way, and mapping first lets
//! uncovered pixels be plain black (LUT[0] is 0). An identity map
//! ([`OutputMap::is_identity`]) is skipped entirely.

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use serde::Deserialize;

use crate::canvas::{Canvas, Rect};

/// Largest side of a card space, in pixels.
pub const MAX_CARD_SIDE: u32 = 4096;
/// Largest card space area, in pixels (bounds the lookup table).
pub const MAX_CARD_PIXELS: u64 = 1 << 22;
/// Table entry for a card pixel no region covers.
pub const NONE: u32 = u32::MAX;

/// Clockwise rotation of a region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "u16")]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl TryFrom<u16> for Rotation {
    type Error = String;

    fn try_from(v: u16) -> Result<Self, String> {
        match v {
            0 => Ok(Rotation::Deg0),
            90 => Ok(Rotation::Deg90),
            180 => Ok(Rotation::Deg180),
            270 => Ok(Rotation::Deg270),
            _ => Err(format!("rotate must be 0, 90, 180 or 270 (got {v})")),
        }
    }
}

impl Rotation {
    pub const ALL: [Rotation; 4] = [
        Rotation::Deg0,
        Rotation::Deg90,
        Rotation::Deg180,
        Rotation::Deg270,
    ];

    /// True if the footprint is `h × w` instead of `w × h`.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Deg90 | Rotation::Deg270)
    }
}

/// Mirroring of a region, applied before rotation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flip {
    #[default]
    None,
    Horizontal,
    Vertical,
}

impl Flip {
    pub const ALL: [Flip; 3] = [Flip::None, Flip::Horizontal, Flip::Vertical];
}

/// A position in card space.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardOffset {
    pub x: u32,
    pub y: u32,
}

/// The size of a card's whole display space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardSize {
    pub w: u32,
    pub h: u32,
}

/// One `[[output.region]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionConfig {
    /// The canvas rectangle to show.
    pub canvas: Rect,
    /// Where the (rotated) region's top-left corner goes in card space.
    #[serde(default)]
    pub card_offset: CardOffset,
    #[serde(default)]
    pub rotate: Rotation,
    #[serde(default)]
    pub flip: Flip,
}

impl RegionConfig {
    /// The whole-canvas region used when an output has none.
    pub fn implicit(canvas_w: u32, canvas_h: u32) -> Self {
        RegionConfig {
            canvas: Rect::new(0, 0, canvas_w, canvas_h),
            card_offset: CardOffset::default(),
            rotate: Rotation::Deg0,
            flip: Flip::None,
        }
    }

    /// Footprint size in card space: `w × h`, or `h × w` for 90 and 270.
    pub fn footprint(&self) -> (u32, u32) {
        let (w, h) = (self.canvas.w, self.canvas.h);
        if self.rotate.swaps_axes() {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// The footprint as a rectangle in card space.
    pub fn card_rect(&self) -> Rect {
        let (w, h) = self.footprint();
        Rect::new(self.card_offset.x, self.card_offset.y, w, h)
    }

    /// Where canvas pixel `(x, y)` (absolute) lands in card space, or `None`
    /// if it is outside the region. The forward direction of the mapping.
    pub fn card_of(&self, x: u32, y: u32) -> Option<(u32, u32)> {
        let r = self.canvas;
        if x < r.x || y < r.y || x - r.x >= r.w || y - r.y >= r.h {
            return None;
        }
        let (w, h) = (r.w, r.h);
        let (sx, sy) = (x - r.x, y - r.y);
        let (fx, fy) = match self.flip {
            Flip::None => (sx, sy),
            Flip::Horizontal => (w - 1 - sx, sy),
            Flip::Vertical => (sx, h - 1 - sy),
        };
        let (u, v) = match self.rotate {
            Rotation::Deg0 => (fx, fy),
            Rotation::Deg90 => (h - 1 - fy, fx),
            Rotation::Deg180 => (w - 1 - fx, h - 1 - fy),
            Rotation::Deg270 => (fy, w - 1 - fx),
        };
        Some((self.card_offset.x + u, self.card_offset.y + v))
    }

    /// The canvas pixel (absolute) shown at card pixel `(cx, cy)`, or `None`
    /// if the region does not cover it. The inverse of [`Self::card_of`].
    pub fn canvas_of(&self, cx: u32, cy: u32) -> Option<(u32, u32)> {
        let (fw, fh) = self.footprint();
        let o = self.card_offset;
        if cx < o.x || cy < o.y || cx - o.x >= fw || cy - o.y >= fh {
            return None;
        }
        let (u, v) = (cx - o.x, cy - o.y);
        let (w, h) = (self.canvas.w, self.canvas.h);
        let (fx, fy) = match self.rotate {
            Rotation::Deg0 => (u, v),
            Rotation::Deg90 => (v, h - 1 - u),
            Rotation::Deg180 => (w - 1 - u, h - 1 - v),
            Rotation::Deg270 => (w - 1 - v, u),
        };
        let (sx, sy) = match self.flip {
            Flip::None => (fx, fy),
            Flip::Horizontal => (w - 1 - fx, fy),
            Flip::Vertical => (fx, h - 1 - fy),
        };
        Some((self.canvas.x + sx, self.canvas.y + sy))
    }
}

/// Check an output's regions; pushes one message per problem, each starting
/// with `at` (e.g. `output[1] (colorlight)`) and naming the region index.
pub fn validate(
    at: &str,
    canvas: (u32, u32),
    regions: &[RegionConfig],
    card_size: Option<CardSize>,
    errs: &mut Vec<String>,
) {
    let implicit = [RegionConfig::implicit(canvas.0, canvas.1)];
    let is_implicit = regions.is_empty();
    let list: &[RegionConfig] = if is_implicit { &implicit } else { regions };

    if let Some(cs) = card_size {
        if cs.w == 0 || cs.h == 0 {
            errs.push(format!("{at}: card_size w and h must be >= 1"));
        } else if cs.w > MAX_CARD_SIDE
            || cs.h > MAX_CARD_SIDE
            || u64::from(cs.w) * u64::from(cs.h) > MAX_CARD_PIXELS
        {
            errs.push(format!(
                "{at}: card_size {}x{} is too large (at most {MAX_CARD_SIDE} per side and \
                 {MAX_CARD_PIXELS} pixels)",
                cs.w, cs.h
            ));
        }
    }
    let card_ok = card_size.filter(|c| c.w >= 1 && c.h >= 1);

    let mut valid: Vec<(usize, Rect)> = Vec::new();
    let (mut max_r, mut max_b) = (0u64, 0u64);
    for (j, r) in list.iter().enumerate() {
        let who = if is_implicit {
            format!("{at}: the whole canvas (no [[output.region]])")
        } else {
            format!("{at} region[{j}]")
        };
        let sep = if is_implicit { "," } else { ":" };
        let c = r.canvas;
        if c.w == 0 || c.h == 0 {
            errs.push(format!("{who}: canvas w and h must be >= 1"));
            continue;
        }
        if !c.fits_in(canvas.0, canvas.1) {
            errs.push(format!(
                "{who}: canvas rect {{x={}, y={}, w={}, h={}}} is outside the {}x{} canvas",
                c.x, c.y, c.w, c.h, canvas.0, canvas.1
            ));
            continue;
        }
        let cr = r.card_rect();
        let (right, bottom) = (
            u64::from(cr.x) + u64::from(cr.w),
            u64::from(cr.y) + u64::from(cr.h),
        );
        if right > u64::from(MAX_CARD_SIDE) || bottom > u64::from(MAX_CARD_SIDE) {
            errs.push(format!(
                "{who}{sep} footprint {}x{} at card_offset ({}, {}) reaches beyond \
                 {MAX_CARD_SIDE} pixels",
                cr.w, cr.h, cr.x, cr.y
            ));
            continue;
        }
        if let Some(cs) = card_ok {
            if right > u64::from(cs.w) || bottom > u64::from(cs.h) {
                errs.push(format!(
                    "{who}{sep} footprint {}x{} at card_offset ({}, {}) does not fit in \
                     card_size {}x{}",
                    cr.w, cr.h, cr.x, cr.y, cs.w, cs.h
                ));
                continue;
            }
        }
        max_r = max_r.max(right);
        max_b = max_b.max(bottom);
        valid.push((j, cr));
    }
    for (n, &(k, b)) in valid.iter().enumerate() {
        if let Some(&(j, _)) = valid[..n].iter().find(|(_, a)| intersects(a, &b)) {
            errs.push(format!(
                "{at} region[{k}]: overlaps region[{j}] in card space (a card pixel can show \
                 only one canvas pixel)"
            ));
        }
    }
    if card_size.is_none() && max_r * max_b > MAX_CARD_PIXELS {
        errs.push(format!(
            "{at}: the regions span a {max_r}x{max_b} card space, over the limit of \
             {MAX_CARD_PIXELS} pixels"
        ));
    }
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    // Both rects passed the MAX_CARD_SIDE check, so no overflow.
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// The precomputed card-pixel to canvas-pixel lookup of one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputMap {
    canvas: (u32, u32),
    card: (u32, u32),
    /// Row-major over the card space: canvas pixel index, or [`NONE`].
    table: Vec<u32>,
    identity: bool,
}

impl OutputMap {
    /// Build the map for a canvas size and an output's regions. Validates
    /// like the config does; the error lists every problem.
    pub fn new(
        canvas_w: u32,
        canvas_h: u32,
        regions: &[RegionConfig],
        card_size: Option<CardSize>,
    ) -> Result<Self, String> {
        let mut errs = Vec::new();
        validate(
            "output",
            (canvas_w, canvas_h),
            regions,
            card_size,
            &mut errs,
        );
        if !errs.is_empty() {
            return Err(errs.join("; "));
        }
        let implicit = [RegionConfig::implicit(canvas_w, canvas_h)];
        let list: &[RegionConfig] = if regions.is_empty() {
            &implicit
        } else {
            regions
        };
        let card = match card_size {
            Some(c) => (c.w, c.h),
            None => list.iter().fold((0, 0), |(w, h), r| {
                let c = r.card_rect();
                (w.max(c.x + c.w), h.max(c.y + c.h))
            }),
        };
        let mut table = vec![NONE; card.0 as usize * card.1 as usize];
        for r in list {
            let c = r.card_rect();
            for cy in c.y..c.y + c.h {
                for cx in c.x..c.x + c.w {
                    let (sx, sy) = r.canvas_of(cx, cy).expect("inside the footprint");
                    table[(cy * card.0 + cx) as usize] = sy * canvas_w + sx;
                }
            }
        }
        let identity =
            card == (canvas_w, canvas_h) && table.iter().enumerate().all(|(i, &s)| s as usize == i);
        Ok(OutputMap {
            canvas: (canvas_w, canvas_h),
            card,
            table,
            identity,
        })
    }

    /// The map of an output without regions: canvas to card origin.
    pub fn identity(canvas_w: u32, canvas_h: u32) -> Self {
        OutputMap::new(canvas_w, canvas_h, &[], None).expect("a whole-canvas region is valid")
    }

    pub fn canvas_size(&self) -> (u32, u32) {
        self.canvas
    }

    /// The card's display space, `(w, h)`.
    pub fn card_size(&self) -> (u32, u32) {
        self.card
    }

    /// True if the card frame equals the canvas, so no gather is needed.
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// The canvas pixel `(x, y)` shown at card pixel `(cx, cy)`, if any.
    pub fn source_at(&self, cx: u32, cy: u32) -> Option<(u32, u32)> {
        if cx >= self.card.0 || cy >= self.card.1 {
            return None;
        }
        match self.table[(cy * self.card.0 + cx) as usize] {
            NONE => None,
            s => Some((s % self.canvas.0, s / self.canvas.0)),
        }
    }

    /// Fill `dst` (resized to the card space if needed) with the card frame
    /// for `src`; uncovered pixels are black. One pass, no allocation when
    /// `dst` already has the right size.
    pub fn gather_into(&self, src: &Canvas, dst: &mut Canvas) -> Result<(), String> {
        if (src.width(), src.height()) != self.canvas {
            return Err(format!(
                "canvas is {}x{} but the output map was built for {}x{}",
                src.width(),
                src.height(),
                self.canvas.0,
                self.canvas.1
            ));
        }
        if (dst.width(), dst.height()) != self.card {
            *dst = Canvas::new(self.card.0, self.card.1);
        }
        let px = src.pixels();
        for (d, &s) in dst.pixels_mut().iter_mut().zip(&self.table) {
            *d = if s == NONE {
                Rgb888::BLACK
            } else {
                px[s as usize]
            };
        }
        Ok(())
    }

    /// The card frame for `src` as a new canvas (for `render --output`).
    pub fn map_canvas(&self, src: &Canvas) -> Result<Canvas, String> {
        let mut dst = Canvas::new(0, 0);
        self.gather_into(src, &mut dst)?;
        Ok(dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(canvas: Rect, off: (u32, u32), rotate: Rotation, flip: Flip) -> RegionConfig {
        RegionConfig {
            canvas,
            card_offset: CardOffset { x: off.0, y: off.1 },
            rotate,
            flip,
        }
    }

    /// Canvas whose pixel (x, y) is (x, y, 7), for sizes up to 256.
    fn coords(w: u32, h: u32) -> Canvas {
        let mut c = Canvas::new(w, h);
        for y in 0..h {
            for x in 0..w {
                c.set_pixel(x, y, Rgb888::new(x as u8, y as u8, 7));
            }
        }
        c
    }

    #[test]
    fn rotate_90_puts_the_top_edge_on_the_right() {
        // 3x2 region: top-left (0,0) goes to the top-right of the 2x3
        // footprint, and the top row runs down the right column.
        let r = region(Rect::new(0, 0, 3, 2), (0, 0), Rotation::Deg90, Flip::None);
        assert_eq!(r.footprint(), (2, 3));
        assert_eq!(r.card_of(0, 0), Some((1, 0)));
        assert_eq!(r.card_of(1, 0), Some((1, 1)));
        assert_eq!(r.card_of(2, 0), Some((1, 2)));
        assert_eq!(r.card_of(0, 1), Some((0, 0)));
        let r = region(Rect::new(0, 0, 3, 2), (0, 0), Rotation::Deg270, Flip::None);
        assert_eq!(r.card_of(0, 0), Some((0, 2)));
        assert_eq!(r.card_of(2, 0), Some((0, 0)));
        let r = region(Rect::new(0, 0, 3, 2), (0, 0), Rotation::Deg180, Flip::None);
        assert_eq!(r.card_of(0, 0), Some((2, 1)));
    }

    #[test]
    fn flip_comes_before_rotation() {
        // Horizontal flip then 90: the top-right pixel becomes the top-left
        // pixel after the flip, then goes to the top-right corner.
        let r = region(
            Rect::new(0, 0, 3, 2),
            (0, 0),
            Rotation::Deg90,
            Flip::Horizontal,
        );
        assert_eq!(r.card_of(2, 0), Some((1, 0)));
        assert_eq!(r.card_of(0, 0), Some((1, 2)));
        let r = region(
            Rect::new(0, 0, 3, 2),
            (0, 0),
            Rotation::Deg0,
            Flip::Vertical,
        );
        assert_eq!(r.card_of(0, 0), Some((0, 1)));
    }

    #[test]
    fn every_rotation_and_flip_is_a_bijection_that_round_trips() {
        for w in 1..=6 {
            for h in 1..=6 {
                for rot in Rotation::ALL {
                    for flip in Flip::ALL {
                        // Offsets and a canvas position, to catch sign mix-ups.
                        let r = region(Rect::new(2, 1, w, h), (3, 5), rot, flip);
                        let (fw, fh) = r.footprint();
                        assert_eq!((fw, fh), if rot.swaps_axes() { (h, w) } else { (w, h) });
                        let mut hit = vec![0u32; (fw * fh) as usize];
                        for y in 1..1 + h {
                            for x in 2..2 + w {
                                let (cx, cy) = r.card_of(x, y).unwrap();
                                assert!(
                                    (3..3 + fw).contains(&cx) && (5..5 + fh).contains(&cy),
                                    "{w}x{h} {rot:?} {flip:?}: ({x},{y}) -> ({cx},{cy})"
                                );
                                hit[((cy - 5) * fw + cx - 3) as usize] += 1;
                                assert_eq!(r.canvas_of(cx, cy), Some((x, y)), "{rot:?} {flip:?}");
                            }
                        }
                        assert!(hit.iter().all(|&n| n == 1), "{w}x{h} {rot:?} {flip:?}");
                        // Outside the region and the footprint: nothing.
                        assert_eq!(r.card_of(1, 1), None);
                        assert_eq!(r.card_of(2, 0), None);
                        assert_eq!(r.canvas_of(2, 5), None);
                        assert_eq!(r.canvas_of(3 + fw, 5), None);
                        assert_eq!(r.canvas_of(3, 5 + fh), None);
                    }
                }
            }
        }
    }

    #[test]
    fn rotating_90_twice_is_rotating_180() {
        for (w, h) in [(3, 2), (5, 4), (1, 4), (4, 4)] {
            for flip in Flip::ALL {
                let r90 = region(Rect::new(0, 0, w, h), (0, 0), Rotation::Deg90, flip);
                let r180 = region(Rect::new(0, 0, w, h), (0, 0), Rotation::Deg180, flip);
                // The second turn acts on the h x w footprint of the first.
                let second = region(Rect::new(0, 0, h, w), (0, 0), Rotation::Deg90, Flip::None);
                for y in 0..h {
                    for x in 0..w {
                        let (a, b) = r90.card_of(x, y).unwrap();
                        assert_eq!(second.card_of(a, b), r180.card_of(x, y), "{flip:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn default_is_the_whole_canvas_at_origin() {
        let m = OutputMap::identity(7, 5);
        assert!(m.is_identity());
        assert_eq!(m.card_size(), (7, 5));
        let c = coords(7, 5);
        assert_eq!(m.map_canvas(&c).unwrap(), c);
        // An explicit whole-canvas region is the same map.
        let r = region(Rect::new(0, 0, 7, 5), (0, 0), Rotation::Deg0, Flip::None);
        assert_eq!(OutputMap::new(7, 5, &[r], None).unwrap(), m);
        // A bigger card with the canvas in a corner is not the identity.
        let m = OutputMap::new(7, 5, &[], Some(CardSize { w: 9, h: 5 })).unwrap();
        assert!(!m.is_identity());
    }

    #[test]
    fn two_stacked_panels_one_upside_down() {
        // The real layout, small: canvas 6x8, panels 6x4 stacked; the bottom
        // one is mounted upside down.
        let top = region(Rect::new(0, 0, 6, 4), (0, 0), Rotation::Deg0, Flip::None);
        let bottom = region(Rect::new(0, 4, 6, 4), (0, 4), Rotation::Deg180, Flip::None);
        let m = OutputMap::new(6, 8, &[top, bottom], None).unwrap();
        assert_eq!(m.card_size(), (6, 8));
        assert!(!m.is_identity());
        for y in 0..4 {
            for x in 0..6 {
                assert_eq!(m.source_at(x, y), Some((x, y)));
                assert_eq!(m.source_at(x, 4 + y), Some((5 - x, 4 + 3 - y)));
            }
        }
    }

    #[test]
    fn offsets_gaps_and_uncovered_pixels() {
        // Card 8x6, one 3x2 region at (4, 3): everything else is black.
        let r = region(Rect::new(1, 1, 3, 2), (4, 3), Rotation::Deg0, Flip::None);
        let cs = CardSize { w: 8, h: 6 };
        let m = OutputMap::new(5, 4, &[r], Some(cs)).unwrap();
        assert_eq!(m.card_size(), (8, 6));
        let mut cv = Canvas::new(5, 4);
        cv.fill(Rgb888::new(9, 9, 9));
        let out = m.map_canvas(&cv).unwrap();
        for y in 0..6 {
            for x in 0..8 {
                let inside = (4..7).contains(&x) && (3..5).contains(&y);
                let want = if inside {
                    Rgb888::new(9, 9, 9)
                } else {
                    Rgb888::BLACK
                };
                assert_eq!(out.pixel(x, y), Some(want), "({x},{y})");
                assert_eq!(m.source_at(x, y).is_some(), inside);
            }
        }
        // Without card_size the card is the bounding box from the origin.
        let m = OutputMap::new(5, 4, &[r], None).unwrap();
        assert_eq!(m.card_size(), (7, 5));
    }

    #[test]
    fn the_gather_copies_the_right_pixels_for_every_variant() {
        for w in 1..=5u32 {
            for h in 1..=4u32 {
                for rot in Rotation::ALL {
                    for flip in Flip::ALL {
                        let r = region(Rect::new(1, 2, w, h), (2, 1), rot, flip);
                        let m = OutputMap::new(w + 1, h + 2, &[r], None).unwrap();
                        let cv = coords(w + 1, h + 2);
                        let out = m.map_canvas(&cv).unwrap();
                        let (cw, ch) = m.card_size();
                        let mut shown = 0;
                        for cy in 0..ch {
                            for cx in 0..cw {
                                let got = out.pixel(cx, cy).unwrap();
                                match r.canvas_of(cx, cy) {
                                    Some((x, y)) => {
                                        shown += 1;
                                        assert_eq!(got, Rgb888::new(x as u8, y as u8, 7));
                                    }
                                    None => assert_eq!(got, Rgb888::BLACK),
                                }
                            }
                        }
                        assert_eq!(shown, w * h);
                    }
                }
            }
        }
    }

    #[test]
    fn several_regions_including_a_mirrored_canvas_area() {
        // The same canvas area shown three times.
        let a = region(Rect::new(0, 0, 4, 3), (0, 0), Rotation::Deg0, Flip::None);
        let b = region(
            Rect::new(0, 0, 4, 3),
            (4, 0),
            Rotation::Deg0,
            Flip::Horizontal,
        );
        let c = region(
            Rect::new(0, 0, 4, 3),
            (0, 3),
            Rotation::Deg90,
            Flip::Vertical,
        );
        let m = OutputMap::new(4, 3, &[a, b, c], None).unwrap();
        assert_eq!(m.card_size(), (8, 7));
        for y in 0..3 {
            for x in 0..4 {
                assert_eq!(m.source_at(x, y), Some((x, y)));
                assert_eq!(m.source_at(4 + x, y), Some((3 - x, y)));
            }
        }
        let mut n = [[0u32; 4]; 3];
        for cy in 0..7 {
            for cx in 0..8 {
                if let Some((x, y)) = m.source_at(cx, cy) {
                    n[y as usize][x as usize] += 1;
                }
            }
        }
        assert!(n.iter().flatten().all(|&k| k == 3), "{n:?}");
        // Uncovered corner: black.
        assert_eq!(m.source_at(7, 6), None);
    }

    #[test]
    fn a_wrong_sized_canvas_is_an_error() {
        let m = OutputMap::identity(4, 4);
        assert!(m.map_canvas(&Canvas::new(4, 5)).is_err());
    }

    fn errors(canvas: (u32, u32), regions: &[RegionConfig], card: Option<CardSize>) -> Vec<String> {
        let mut e = Vec::new();
        validate("output[2] (colorlight)", canvas, regions, card, &mut e);
        e
    }

    #[test]
    fn validation_messages_name_the_output_and_region() {
        let ok = region(Rect::new(0, 0, 4, 4), (0, 0), Rotation::Deg0, Flip::None);
        assert!(errors((8, 8), &[ok], None).is_empty());

        let outside = region(Rect::new(6, 0, 4, 4), (4, 0), Rotation::Deg0, Flip::None);
        let e = errors((8, 8), &[ok, outside], None);
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].starts_with("output[2] (colorlight) region[1]:"),
            "{e:?}"
        );
        assert!(e[0].contains("outside the 8x8 canvas"), "{e:?}");

        let empty = region(Rect::new(0, 0, 0, 4), (0, 0), Rotation::Deg0, Flip::None);
        let e = errors((8, 8), &[empty], None);
        assert!(e[0].contains("region[0]") && e[0].contains(">= 1"), "{e:?}");

        // Overlap in card space is an error, in canvas space it is fine.
        let b = region(Rect::new(0, 0, 4, 4), (3, 3), Rotation::Deg0, Flip::None);
        let e = errors((8, 8), &[ok, b], None);
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].contains("region[1]") && e[0].contains("overlaps region[0]"),
            "{e:?}"
        );
        let b = region(Rect::new(0, 0, 4, 4), (4, 0), Rotation::Deg0, Flip::None);
        assert!(errors((8, 8), &[ok, b], None).is_empty());
    }

    #[test]
    fn rotation_swaps_the_footprint_when_checking_card_size() {
        // 6x2 at 90 degrees is a 2x6 footprint.
        let r = region(Rect::new(0, 0, 6, 2), (0, 0), Rotation::Deg90, Flip::None);
        let fits = CardSize { w: 2, h: 6 };
        assert!(errors((6, 2), &[r], Some(fits)).is_empty());
        let wide = CardSize { w: 6, h: 2 };
        let e = errors((6, 2), &[r], Some(wide));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].contains("footprint 2x6") && e[0].contains("card_size 6x2"),
            "{e:?}"
        );
        let r0 = region(Rect::new(0, 0, 6, 2), (0, 0), Rotation::Deg0, Flip::None);
        assert!(errors((6, 2), &[r0], Some(wide)).is_empty());
        // Offsets count too.
        let off = region(Rect::new(0, 0, 6, 2), (1, 0), Rotation::Deg0, Flip::None);
        assert_eq!(errors((6, 2), &[off], Some(wide)).len(), 1);
    }

    #[test]
    fn card_size_checks_apply_to_the_implicit_region_too() {
        let e = errors((8, 8), &[], Some(CardSize { w: 4, h: 8 }));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].contains("whole canvas") && e[0].contains("card_size 4x8"),
            "{e:?}"
        );
        assert!(errors((8, 8), &[], Some(CardSize { w: 9, h: 8 })).is_empty());
        let e = errors((8, 8), &[], Some(CardSize { w: 0, h: 8 }));
        assert!(e[0].contains("card_size"), "{e:?}");
        let e = errors((8, 8), &[], Some(CardSize { w: 5000, h: 8 }));
        assert!(e.iter().any(|m| m.contains("too large")), "{e:?}");
    }

    #[test]
    fn huge_offsets_do_not_overflow() {
        let r = region(
            Rect::new(0, 0, 4, 4),
            (u32::MAX - 1, 0),
            Rotation::Deg0,
            Flip::None,
        );
        let e = errors((8, 8), &[r], None);
        assert_eq!(e.len(), 1, "{e:?}");
        let r = region(
            Rect::new(u32::MAX, 0, 4, 4),
            (0, 0),
            Rotation::Deg0,
            Flip::None,
        );
        assert_eq!(errors((8, 8), &[r], None).len(), 1);
        assert!(OutputMap::new(8, 8, &[r], None).is_err());
    }

    #[test]
    fn rotation_parses_only_right_angles() {
        assert_eq!(Rotation::try_from(270), Ok(Rotation::Deg270));
        assert!(Rotation::try_from(45).is_err());
        assert!(Rotation::try_from(360).is_err());
    }
}
