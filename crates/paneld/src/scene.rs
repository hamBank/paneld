//! A scene is a set of zones, each a canvas rect with one widget.

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};

use crate::canvas::{Canvas, Rect};
use crate::config::SceneConfig;
use crate::store::StoreSnapshot;
use crate::widgets::{self, FrameCtx, Widget};

pub struct Zone {
    rect: Rect,
    background: Option<Rgb888>,
    widget: Box<dyn Widget>,
}

pub struct Scene {
    name: String,
    zones: Vec<Zone>,
}

impl Scene {
    pub fn from_config(cfg: &SceneConfig) -> Self {
        Scene {
            name: cfg.name.clone(),
            zones: cfg
                .zones
                .iter()
                .map(|z| {
                    let mut widget = widgets::build(&z.widget);
                    // One-off sizing work (e.g. scaling every GIF frame) happens
                    // here, while the config is loaded, not on the first frame.
                    // Validation guarantees the rect fits the canvas, so this is
                    // the size `draw` will see.
                    widget.prepare(z.rect.w, z.rect.h);
                    Zone {
                        rect: z.rect,
                        background: z.background.map(|c| c.0),
                        widget,
                    }
                })
                .collect(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Clear the canvas and draw every zone, in config order.
    pub fn render(&mut self, ctx: &FrameCtx, store: &StoreSnapshot, canvas: &mut Canvas) {
        canvas.fill(Rgb888::BLACK);
        for zone in &mut self.zones {
            zone.widget.update(ctx, store);
            let mut target = canvas.zone(zone.rect);
            if let Some(bg) = zone.background {
                target.fill(bg);
            }
            zone.widget.draw(&mut target);
        }
    }
}
