//! The logical canvas the engine renders into, and zone-clipped views of it.

use std::convert::Infallible;

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::prelude::*;
use serde::Deserialize;

/// An axis-aligned rectangle in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Rect { x, y, w, h }
    }

    /// True if the rectangle lies entirely inside a `width × height` area.
    pub fn fits_in(&self, width: u32, height: u32) -> bool {
        self.x.checked_add(self.w).is_some_and(|r| r <= width)
            && self.y.checked_add(self.h).is_some_and(|b| b <= height)
    }

    /// Intersection with a `width × height` area anchored at the origin.
    fn clamp_to(self, width: u32, height: u32) -> Rect {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let w = self.w.min(width - x);
        let h = self.h.min(height - y);
        Rect { x, y, w, h }
    }
}

/// A `width × height` RGB888 frame buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<Rgb888>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Canvas {
            width,
            height,
            pixels: vec![Rgb888::BLACK; (width as usize) * (height as usize)],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixel at `(x, y)`, or `None` outside the canvas.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Rgb888> {
        (x < self.width && y < self.height).then(|| self.pixels[(y * self.width + x) as usize])
    }

    /// Set the pixel at `(x, y)`; out-of-bounds writes are ignored.
    pub fn set_pixel(&mut self, x: u32, y: u32, c: Rgb888) {
        if x < self.width && y < self.height {
            self.pixels[(y * self.width + x) as usize] = c;
        }
    }

    pub fn fill(&mut self, c: Rgb888) {
        self.pixels.fill(c);
    }

    /// Row-major pixels.
    pub fn pixels(&self) -> &[Rgb888] {
        &self.pixels
    }

    /// Packed `RGBRGB…` bytes, row-major.
    pub fn to_rgb_bytes(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .flat_map(|p| [p.r(), p.g(), p.b()])
            .collect()
    }

    /// A draw target clipped to `rect` (clamped to the canvas), with its
    /// origin at the rect's top-left corner.
    pub fn zone(&mut self, rect: Rect) -> ZoneTarget<'_> {
        let rect = rect.clamp_to(self.width, self.height);
        ZoneTarget { canvas: self, rect }
    }
}

impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 {
                self.set_pixel(p.x as u32, p.y as u32, c);
            }
        }
        Ok(())
    }
}

/// A view of a canvas region. Coordinates are zone-local and anything drawn
/// outside the zone is discarded.
pub struct ZoneTarget<'a> {
    canvas: &'a mut Canvas,
    rect: Rect,
}

impl ZoneTarget<'_> {
    pub fn width(&self) -> u32 {
        self.rect.w
    }

    pub fn height(&self) -> u32 {
        self.rect.h
    }

    /// Fill the whole zone with one colour.
    pub fn fill(&mut self, c: Rgb888) {
        for y in self.rect.y..self.rect.y + self.rect.h {
            for x in self.rect.x..self.rect.x + self.rect.w {
                self.canvas.set_pixel(x, y, c);
            }
        }
    }
}

impl OriginDimensions for ZoneTarget<'_> {
    fn size(&self) -> Size {
        Size::new(self.rect.w, self.rect.h)
    }
}

impl DrawTarget for ZoneTarget<'_> {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let (w, h) = (self.rect.w as i32, self.rect.h as i32);
        for Pixel(p, c) in pixels {
            if (0..w).contains(&p.x) && (0..h).contains(&p.y) {
                self.canvas
                    .set_pixel(self.rect.x + p.x as u32, self.rect.y + p.y as u32, c);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};

    #[test]
    fn new_canvas_is_black() {
        let c = Canvas::new(4, 2);
        assert_eq!(c.pixels().len(), 8);
        assert!(c.pixels().iter().all(|p| *p == Rgb888::BLACK));
        assert_eq!(c.pixel(4, 0), None);
    }

    #[test]
    fn zone_translates_and_clips() {
        let mut c = Canvas::new(8, 4);
        let mut z = c.zone(Rect::new(2, 1, 3, 2));
        // A rectangle larger than the zone must only touch the zone.
        Rectangle::new(Point::new(-5, -5), Size::new(100, 100))
            .into_styled(PrimitiveStyle::with_fill(Rgb888::RED))
            .draw(&mut z)
            .unwrap();
        for y in 0..4 {
            for x in 0..8 {
                let inside = (2..5).contains(&x) && (1..3).contains(&y);
                let want = if inside { Rgb888::RED } else { Rgb888::BLACK };
                assert_eq!(c.pixel(x, y), Some(want), "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn zone_is_clamped_to_canvas() {
        let mut c = Canvas::new(4, 4);
        let z = c.zone(Rect::new(3, 3, 10, 10));
        assert_eq!(z.size(), Size::new(1, 1));
        let z = c.zone(Rect::new(10, 10, 1, 1));
        assert_eq!(z.size(), Size::new(0, 0));
    }

    #[test]
    fn rgb_bytes_are_row_major() {
        let mut c = Canvas::new(2, 1);
        c.set_pixel(1, 0, Rgb888::new(1, 2, 3));
        assert_eq!(c.to_rgb_bytes(), vec![0, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn rect_fits() {
        assert!(Rect::new(0, 0, 128, 32).fits_in(128, 32));
        assert!(!Rect::new(1, 0, 128, 32).fits_in(128, 32));
        assert!(!Rect::new(u32::MAX, 0, 2, 1).fits_in(128, 32));
    }
}
