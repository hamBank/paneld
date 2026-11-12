//! Test patterns for hardware bring-up (DESIGN.md §13).
//!
//! Each pattern is drawn into a [`Canvas`] from nothing but the canvas size
//! and the physical panel size, so it works for any layout. Panels are tiled
//! row-major from the top-left; panels that stick out past the canvas edge are
//! clipped.
//!
//! What each pattern is for, when checking a Colorlight/LEDVision mapping:
//! - [`Pattern::Bars`]: colour channel order (R/G/B swapped?) and column mirroring.
//! - [`Pattern::Gradient`]: gamma and bit depth.
//! - [`Pattern::PanelIds`]: panel order, mirroring and rotation.
//! - [`Pattern::Grid`]: row placement and the scan-half boundary (e.g. rows
//!   0–42 vs 43–85 of a 1/43-scan panel being swapped).
//! - [`Pattern::Solid`]: dead pixels and power draw.
//! - [`Pattern::Walk`]: any remaining per-pixel mapping error.

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};

use crate::canvas::Canvas;

/// A test pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Eight vertical bars: white, yellow, cyan, green, magenta, red, blue, black.
    Bars,
    /// Four horizontal bands: red, green, blue and white ramps from black.
    Gradient,
    /// Per panel: 1 px border in a distinct colour, its index (0 = top-left,
    /// row-major) large in the centre of the visible part, and a white square
    /// marker in the top-left corner.
    PanelIds,
    /// 1 px grey lines every [`GRID_STEP`] px, cyan lines on each panel's top
    /// and left edge, and a yellow line on each panel's scan-half boundary.
    Grid,
    /// The whole canvas in one colour.
    Solid(Rgb888),
    /// One white pixel stepping through the canvas in raster order, one pixel
    /// per frame, wrapping around.
    Walk,
}

/// Distance between grid lines in [`Pattern::Grid`].
pub const GRID_STEP: u32 = 8;
pub const GRID_COLOR: Rgb888 = Rgb888::new(64, 64, 64);
pub const GRID_PANEL_EDGE_COLOR: Rgb888 = Rgb888::CYAN;
pub const GRID_SCAN_HALF_COLOR: Rgb888 = Rgb888::YELLOW;

/// Side of the top-left marker square in [`Pattern::PanelIds`], at most.
pub const MARKER_SIZE: u32 = 6;

/// Bar colours of [`Pattern::Bars`], left to right.
pub const BARS: [Rgb888; 8] = [
    Rgb888::WHITE,
    Rgb888::YELLOW,
    Rgb888::CYAN,
    Rgb888::GREEN,
    Rgb888::MAGENTA,
    Rgb888::RED,
    Rgb888::BLUE,
    Rgb888::BLACK,
];

/// Panel border colours of [`Pattern::PanelIds`]; panel `i` uses `i % len`.
pub const PANEL_COLORS: [Rgb888; 6] = [
    Rgb888::RED,
    Rgb888::GREEN,
    Rgb888::BLUE,
    Rgb888::YELLOW,
    Rgb888::CYAN,
    Rgb888::MAGENTA,
];

/// Row of a panel (relative to its top) where the second scan half starts.
pub fn scan_half(panel_h: u32) -> u32 {
    panel_h / 2
}

/// Render a pattern into a fresh canvas.
pub fn render(
    pattern: Pattern,
    width: u32,
    height: u32,
    panel_w: u32,
    panel_h: u32,
    frame: u64,
) -> Canvas {
    let mut canvas = Canvas::new(width, height);
    draw(pattern, &mut canvas, panel_w, panel_h, frame);
    canvas
}

/// Draw `pattern` into `canvas` (overwriting every pixel), with panels of
/// `panel_w × panel_h`. `frame` only matters for [`Pattern::Walk`]. A zero
/// panel size is treated as 1.
pub fn draw(pattern: Pattern, canvas: &mut Canvas, panel_w: u32, panel_h: u32, frame: u64) {
    let (pw, ph) = (panel_w.max(1), panel_h.max(1));
    match pattern {
        Pattern::Bars => bars(canvas),
        Pattern::Gradient => gradient(canvas),
        Pattern::PanelIds => panel_ids(canvas, pw, ph),
        Pattern::Grid => grid(canvas, pw, ph),
        Pattern::Solid(c) => canvas.fill(c),
        Pattern::Walk => walk(canvas, frame),
    }
}

/// Fill a rectangle (parts outside the canvas are clipped).
fn fill_rect(canvas: &mut Canvas, x: u32, y: u32, w: u32, h: u32, c: Rgb888) {
    if w == 0 || h == 0 {
        return;
    }
    let _ = Rectangle::new(Point::new(x as i32, y as i32), Size::new(w, h))
        .into_styled(PrimitiveStyle::with_fill(c))
        .draw(canvas);
}

fn bars(canvas: &mut Canvas) {
    let w = canvas.width() as u64;
    for y in 0..canvas.height() {
        for x in 0..canvas.width() {
            let i = (x as u64 * BARS.len() as u64 / w) as usize;
            canvas.set_pixel(x, y, BARS[i]);
        }
    }
}

fn gradient(canvas: &mut Canvas) {
    let (w, h) = (canvas.width(), canvas.height());
    for y in 0..h {
        let band = (y as u64 * 4 / h as u64) as usize;
        for x in 0..w {
            let v = if w > 1 {
                (x as u64 * 255 / (w as u64 - 1)) as u8
            } else {
                255
            };
            let c = match band {
                0 => Rgb888::new(v, 0, 0),
                1 => Rgb888::new(0, v, 0),
                2 => Rgb888::new(0, 0, v),
                _ => Rgb888::new(v, v, v),
            };
            canvas.set_pixel(x, y, c);
        }
    }
}

/// Panels as `(index, x, y)` of their top-left corner, row-major.
fn panels(canvas: &Canvas, pw: u32, ph: u32) -> impl Iterator<Item = (u32, u32, u32)> {
    let cols = canvas.width().div_ceil(pw);
    let rows = canvas.height().div_ceil(ph);
    (0..rows).flat_map(move |r| (0..cols).map(move |c| (r * cols + c, c * pw, r * ph)))
}

fn panel_ids(canvas: &mut Canvas, pw: u32, ph: u32) {
    canvas.fill(Rgb888::BLACK);
    let (cw, ch) = (canvas.width(), canvas.height());
    for (index, x0, y0) in panels(canvas, pw, ph) {
        let color = PANEL_COLORS[index as usize % PANEL_COLORS.len()];
        // Part of the panel that is actually on the canvas.
        let (vw, vh) = (pw.min(cw - x0), ph.min(ch - y0));
        let _ = Rectangle::new(Point::new(x0 as i32, y0 as i32), Size::new(pw, ph))
            .into_styled(PrimitiveStyle::with_stroke(color, 1))
            .draw(canvas);
        // Marker inside the border, so it stays visible next to it.
        let m = MARKER_SIZE
            .min(pw.saturating_sub(2) / 3)
            .min(ph.saturating_sub(2) / 3);
        fill_rect(canvas, x0 + 1, y0 + 1, m, m, Rgb888::WHITE);
        draw_number(canvas, index, x0, y0, vw, vh, color);
    }
}

/// 5×7 digit glyphs; each row is the low 5 bits, MSB on the left.
const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ],
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ],
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ],
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ],
];
const GLYPH_W: u32 = 5;
const GLYPH_H: u32 = 7;

/// Draw `n` in decimal, scaled by the largest integer factor that fits in
/// about 75% of the visible area (at least 1), centred in the visible area.
fn draw_number(canvas: &mut Canvas, n: u32, x0: u32, y0: u32, vw: u32, vh: u32, c: Rgb888) {
    let digits: Vec<usize> = n.to_string().bytes().map(|b| (b - b'0') as usize).collect();
    let count = digits.len() as u32;
    // Total width in glyph cells: digits plus one blank column between them.
    let cells_w = count * GLYPH_W + (count - 1);
    let k = (vw * 3 / 4 / cells_w).min(vh * 3 / 4 / GLYPH_H).max(1);
    let (tw, th) = (cells_w * k, GLYPH_H * k);
    let left = x0 + vw.saturating_sub(tw) / 2;
    let top = y0 + vh.saturating_sub(th) / 2;
    for (i, d) in digits.iter().enumerate() {
        let gx = left + i as u32 * (GLYPH_W + 1) * k;
        for (row, bits) in DIGITS[*d].iter().enumerate() {
            for col in 0..GLYPH_W {
                if (bits >> (GLYPH_W - 1 - col)) & 1 == 1 {
                    fill_rect(canvas, gx + col * k, top + row as u32 * k, k, k, c);
                }
            }
        }
    }
}

fn grid(canvas: &mut Canvas, pw: u32, ph: u32) {
    let (w, h) = (canvas.width(), canvas.height());
    canvas.fill(Rgb888::BLACK);
    for y in 0..h {
        for x in 0..w {
            if x % GRID_STEP == 0 || y % GRID_STEP == 0 {
                canvas.set_pixel(x, y, GRID_COLOR);
            }
        }
    }
    for (_, x0, y0) in panels(canvas, pw, ph) {
        let (vw, vh) = (pw.min(w - x0), ph.min(h - y0));
        fill_rect(canvas, x0, y0, vw, 1, GRID_PANEL_EDGE_COLOR);
        fill_rect(canvas, x0, y0, 1, vh, GRID_PANEL_EDGE_COLOR);
        let half = scan_half(ph);
        if half > 0 && half < vh {
            fill_rect(canvas, x0, y0 + half, vw, 1, GRID_SCAN_HALF_COLOR);
        }
    }
}

fn walk(canvas: &mut Canvas, frame: u64) {
    canvas.fill(Rgb888::BLACK);
    let (w, h) = (canvas.width() as u64, canvas.height() as u64);
    if w == 0 || h == 0 {
        return;
    }
    let pos = frame % (w * h);
    canvas.set_pixel((pos % w) as u32, (pos / w) as u32, Rgb888::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(c: &Canvas, x: u32, y: u32) -> Rgb888 {
        c.pixel(x, y).unwrap()
    }

    #[test]
    fn bars_order_and_edges() {
        let c = render(Pattern::Bars, 172, 20, 172, 20, 0);
        assert_eq!(px(&c, 0, 0), Rgb888::WHITE);
        assert_eq!(px(&c, 171, 19), Rgb888::BLACK);
        // 8 equal bars on a width divisible by 8.
        let c = render(Pattern::Bars, 16, 1, 16, 1, 0);
        for (i, want) in BARS.iter().enumerate() {
            assert_eq!(px(&c, i as u32 * 2, 0), *want);
            assert_eq!(px(&c, i as u32 * 2 + 1, 0), *want);
        }
    }

    #[test]
    fn gradient_bands_and_ends() {
        let c = render(Pattern::Gradient, 256, 8, 256, 8, 0);
        assert_eq!(px(&c, 0, 0), Rgb888::BLACK);
        assert_eq!(px(&c, 255, 0), Rgb888::new(255, 0, 0));
        assert_eq!(px(&c, 255, 2), Rgb888::new(0, 255, 0));
        assert_eq!(px(&c, 255, 4), Rgb888::new(0, 0, 255));
        assert_eq!(px(&c, 255, 7), Rgb888::WHITE);
        assert_eq!(px(&c, 100, 7), Rgb888::new(100, 100, 100));
    }

    #[test]
    fn panel_ids_border_and_marker() {
        let c = render(Pattern::PanelIds, 172, 172, 172, 86, 0);
        // Panel 0 (red) and panel 1 (green).
        assert_eq!(px(&c, 171, 0), PANEL_COLORS[0]);
        assert_eq!(px(&c, 0, 85), PANEL_COLORS[0]);
        assert_eq!(px(&c, 100, 85), PANEL_COLORS[0]);
        assert_eq!(px(&c, 100, 86), PANEL_COLORS[1]);
        assert_eq!(px(&c, 171, 171), PANEL_COLORS[1]);
        // Marker just inside the corner, none at the other corners.
        assert_eq!(px(&c, 1, 1), Rgb888::WHITE);
        assert_eq!(px(&c, 6, 6), Rgb888::WHITE);
        assert_eq!(px(&c, 7, 7), Rgb888::BLACK);
        assert_eq!(px(&c, 1, 87), Rgb888::WHITE);
        assert_eq!(px(&c, 170, 1), Rgb888::BLACK);
        assert_eq!(px(&c, 170, 84), Rgb888::BLACK);
    }

    #[test]
    fn panel_ids_number_is_drawn_in_the_centre() {
        let c = render(Pattern::PanelIds, 172, 172, 172, 86, 0);
        let lit = |y0: u32| {
            (y0 + 20..y0 + 66)
                .flat_map(|y| (40..132).map(move |x| (x, y)))
                .filter(|&(x, y)| px(&c, x, y) != Rgb888::BLACK)
                .count()
        };
        assert!(lit(0) > 100 && lit(86) > 100);
        // Different colour for panel 0 and 1 in the centre rows.
        let first_lit = |y0: u32| {
            (y0 + 20..y0 + 66)
                .flat_map(|y| (40..132).map(move |x| (x, y)))
                .map(|(x, y)| px(&c, x, y))
                .find(|p| *p != Rgb888::BLACK)
                .unwrap()
        };
        assert_eq!(first_lit(0), PANEL_COLORS[0]);
        assert_eq!(first_lit(86), PANEL_COLORS[1]);
    }

    #[test]
    fn panel_ids_tile_row_major() {
        // 2×2 panels of 10×10: indices 0 1 / 2 3 pick colours 0..4.
        let c = render(Pattern::PanelIds, 20, 20, 10, 10, 0);
        assert_eq!(px(&c, 9, 0), PANEL_COLORS[0]);
        assert_eq!(px(&c, 10, 0), PANEL_COLORS[1]);
        assert_eq!(px(&c, 0, 10), PANEL_COLORS[2]);
        assert_eq!(px(&c, 19, 19), PANEL_COLORS[3]);
    }

    #[test]
    fn grid_lines_and_scan_half() {
        let c = render(Pattern::Grid, 172, 172, 172, 86, 0);
        // Scan-half line at row 43 of each panel, full width.
        for x in [0, 1, 85, 171] {
            assert_eq!(px(&c, x, 43), GRID_SCAN_HALF_COLOR);
            assert_eq!(px(&c, x, 86 + 43), GRID_SCAN_HALF_COLOR);
        }
        // Panel edges.
        assert_eq!(px(&c, 5, 86), GRID_PANEL_EDGE_COLOR);
        assert_eq!(px(&c, 0, 90), GRID_PANEL_EDGE_COLOR);
        // Ordinary grid lines and empty cells.
        assert_eq!(px(&c, 8, 4), GRID_COLOR);
        assert_eq!(px(&c, 4, 8), GRID_COLOR);
        assert_eq!(px(&c, 4, 4), Rgb888::BLACK);
    }

    #[test]
    fn grid_scan_half_follows_panel_size() {
        let c = render(Pattern::Grid, 64, 64, 64, 32, 0);
        assert_eq!(px(&c, 3, 16), GRID_SCAN_HALF_COLOR);
        assert_eq!(px(&c, 3, 48), GRID_SCAN_HALF_COLOR);
        assert_eq!(scan_half(86), 43);
    }

    #[test]
    fn solid_fills_everything() {
        let col = Rgb888::new(1, 2, 3);
        let c = render(Pattern::Solid(col), 5, 4, 5, 4, 0);
        assert!(c.pixels().iter().all(|p| *p == col));
    }

    #[test]
    fn walk_is_deterministic_raster_order() {
        let c = render(Pattern::Walk, 4, 3, 4, 3, 5);
        assert_eq!(
            c.pixels().iter().filter(|p| **p != Rgb888::BLACK).count(),
            1
        );
        assert_eq!(px(&c, 1, 1), Rgb888::WHITE);
        assert_eq!(render(Pattern::Walk, 4, 3, 4, 3, 5 + 12), c);
        assert_eq!(
            px(&render(Pattern::Walk, 4, 3, 4, 3, 0), 0, 0),
            Rgb888::WHITE
        );
    }

    #[test]
    fn partial_panels_are_clipped_and_every_pattern_survives_odd_sizes() {
        let all = [
            Pattern::Bars,
            Pattern::Gradient,
            Pattern::PanelIds,
            Pattern::Grid,
            Pattern::Solid(Rgb888::RED),
            Pattern::Walk,
        ];
        for p in all {
            for (w, h, pw, ph) in [
                (100, 50, 64, 32),
                (1, 1, 1, 1),
                (3, 200, 172, 86),
                (7, 5, 0, 0),
            ] {
                let c = render(p, w, h, pw, ph, 3);
                assert_eq!((c.width(), c.height()), (w, h));
            }
        }
        // Panel 1 of a 100×50 canvas with 64×32 panels is 36 px wide.
        let c = render(Pattern::PanelIds, 100, 50, 64, 32, 0);
        assert_eq!(px(&c, 64, 5), PANEL_COLORS[1]);
        assert_eq!(px(&c, 99, 0), PANEL_COLORS[1]);
        // Panel 3 is clipped on the right and bottom: top border still there.
        assert_eq!(px(&c, 80, 32), PANEL_COLORS[3]);
    }
}
