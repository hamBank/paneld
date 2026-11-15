use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};

use super::{FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::icons::{Icon, ICON_SIZE};
use crate::store::StoreSnapshot;

/// One built-in icon, blown up by an integer scale and centred vertically
/// in its zone. Not selectable from config: inline notifications build it.
pub struct IconWidget {
    icon: &'static Icon,
    color: Rgb888,
    scale: u32,
}

impl IconWidget {
    pub fn new(icon: &'static Icon, color: Rgb888, scale: u32) -> Self {
        IconWidget {
            icon,
            color,
            scale: scale.max(1),
        }
    }

    /// Width and height in panel pixels at `scale`.
    pub fn pixel_size(scale: u32) -> u32 {
        ICON_SIZE * scale.max(1)
    }
}

impl Widget for IconWidget {
    fn update(&mut self, _ctx: &FrameCtx, _store: &StoreSnapshot) {}

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let side = Self::pixel_size(self.scale);
        let y0 = (target.height() as i32 - side as i32) / 2;
        if self.scale == 1 {
            self.icon.draw(target, Point::new(0, y0), self.color);
            return;
        }
        let style = PrimitiveStyle::with_fill(self.color);
        for y in 0..ICON_SIZE {
            for x in 0..ICON_SIZE {
                if self.icon.lit(x, y) {
                    let at = Point::new((x * self.scale) as i32, y0 + (y * self.scale) as i32);
                    let _ = Rectangle::new(at, Size::new(self.scale, self.scale))
                        .into_styled(style)
                        .draw(target);
                }
            }
        }
    }
}
