use std::cell::RefCell;
use std::sync::Arc;

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::prelude::*;

use super::{FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Fit, ImageAlign, ImageConfig};
use crate::media::{Bitmap, Media};
use crate::store::{StoreSnapshot, Value};

/// A still image or GIF, scaled to the zone.
///
/// No I/O happens here. A `path` image arrives already decoded in the config
/// (`Config::load_media`); a `key` image is picked up from the store snapshot.
/// Scaling runs once per (image, zone size) and the zone-sized frames are
/// cached; drawing a frame is then just a blend over whatever the zone
/// already holds (its background). GIF frames are chosen from
/// `FrameCtx::scene_elapsed`.
///
/// M3's media source will move file loading off the config-load path (async,
/// on demand); this widget will then see those images through `key`.
pub struct Image {
    source: Source,
    fit: Fit,
    align: ImageAlign,
    smooth: bool,
    looping: bool,
    media: Option<Arc<Media>>,
    frame: usize,
    cache: RefCell<Option<Scaled>>,
}

enum Source {
    Fixed,
    Key(String),
}

/// All frames of `media`, already fitted to a `zone` sized area.
struct Scaled {
    media: Arc<Media>,
    zone: (u32, u32),
    frames: Vec<Bitmap>,
}

impl Image {
    pub fn new(cfg: &ImageConfig) -> Self {
        let source = match &cfg.key {
            Some(k) => Source::Key(k.clone()),
            None => Source::Fixed,
        };
        if matches!(source, Source::Fixed) && cfg.loaded.is_none() {
            tracing::warn!(
                path = ?cfg.path,
                "image widget built from a config whose media was not loaded"
            );
        }
        Image {
            source,
            fit: cfg.fit,
            align: cfg.align,
            smooth: cfg.smooth,
            looping: cfg.looping,
            media: cfg.loaded.clone(),
            frame: 0,
            cache: RefCell::new(None),
        }
    }

    /// Make sure the cache holds the current media at this zone size.
    fn ensure_scaled(&self, w: u32, h: u32) {
        let Some(media) = &self.media else { return };
        let mut cache = self.cache.borrow_mut();
        if let Some(c) = &*cache {
            if c.zone == (w, h) && Arc::ptr_eq(&c.media, media) {
                return;
            }
        }
        let frames = media
            .frames()
            .iter()
            .map(|f| f.bitmap.fitted(w, h, self.fit, self.align, self.smooth))
            .collect();
        *cache = Some(Scaled {
            media: media.clone(),
            zone: (w, h),
            frames,
        });
    }
}

impl Widget for Image {
    fn update(&mut self, ctx: &FrameCtx, store: &StoreSnapshot) {
        if let Source::Key(key) = &self.source {
            match store.get(key).map(|e| &e.value) {
                Some(Value::Image(bmp)) => {
                    let same = self
                        .media
                        .as_ref()
                        .is_some_and(|m| Arc::ptr_eq(&m.frames()[0].bitmap, bmp));
                    if !same {
                        self.media = Some(Arc::new(Media::still(bmp.clone())));
                    }
                }
                // Missing or not an image: show nothing.
                _ => self.media = None,
            }
        }
        if let Some(m) = &self.media {
            self.frame = m.frame_at(ctx.scene_elapsed, self.looping);
        }
    }

    fn prepare(&mut self, zone_w: u32, zone_h: u32) {
        self.ensure_scaled(zone_w, zone_h);
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width(), target.height());
        self.ensure_scaled(w, h);
        let cache = self.cache.borrow();
        let Some(frame) = cache.as_ref().and_then(|c| c.frames.get(self.frame)) else {
            return;
        };
        for y in 0..h {
            for x in 0..w {
                let Some([r, g, b, a]) = frame.pixel(x, y) else {
                    continue;
                };
                let px = match a {
                    0 => continue,
                    255 => Rgb888::new(r, g, b),
                    _ => {
                        let under = target.pixel(x, y).unwrap_or(Rgb888::BLACK);
                        Rgb888::new(
                            blend(r, under.r(), a),
                            blend(g, under.g(), a),
                            blend(b, under.b(), a),
                        )
                    }
                };
                let _ = Pixel(Point::new(x as i32, y as i32), px).draw(target);
            }
        }
    }
}

/// `src * a + dst * (1 - a)` with `a` in 0..=255, rounded.
fn blend(src: u8, dst: u8, a: u8) -> u8 {
    let a = u32::from(a);
    ((u32::from(src) * a + u32::from(dst) * (255 - a) + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::blend;

    #[test]
    fn blend_endpoints_and_midpoint() {
        assert_eq!(blend(200, 10, 0), 10);
        assert_eq!(blend(200, 10, 255), 200);
        assert_eq!(blend(255, 0, 128), 128);
    }
}
