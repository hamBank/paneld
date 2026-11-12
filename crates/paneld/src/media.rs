//! Bitmaps, image decoding (PNG, GIF) and scaling for the `image` widget.
//!
//! Everything here is plain CPU work on byte buffers; the only I/O is
//! [`Media::load`], which reads a file. Callers must not use it on the render
//! thread (DESIGN.md §5.1): the `image` widget calls it while the config is
//! loaded (see `Config::load_media`).
//!
//! # GIF semantics
//!
//! Frames are fully composited onto a transparent logical screen, honouring
//! each frame's rectangle, transparent index and disposal method
//! (`Background` clears the frame's rectangle to transparent, `Previous`
//! restores the screen as it was before the frame was drawn, `Keep` and
//! unspecified leave it). Every output frame is the whole screen.
//!
//! Delays are stored in centiseconds. Delays below 2 cs (20 ms) are treated
//! as 100 ms, which is what browsers do for the common "0" and "1" values;
//! anything else is used as is.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context};

use crate::config::{Fit, ImageAlign};

/// Largest accepted image edge, in pixels.
pub const MAX_DIMENSION: u32 = 4096;
/// Largest accepted decoded size of one file (all frames), in bytes.
pub const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
/// Largest accepted file, in bytes.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Delays shorter than this are replaced by [`DEFAULT_FRAME_DELAY`].
pub const MIN_FRAME_DELAY: Duration = Duration::from_millis(20);
pub const DEFAULT_FRAME_DELAY: Duration = Duration::from_millis(100);

/// A straight-alpha (non-premultiplied) 8-bit RGBA image, row-major.
#[derive(Clone, PartialEq, Eq)]
pub struct Bitmap {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitmap({}x{})", self.width, self.height)
    }
}

impl Bitmap {
    /// Wrap `width * height * 4` bytes of `RGBARGBA…`. Both dimensions must be
    /// non-zero and at most [`MAX_DIMENSION`].
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> anyhow::Result<Bitmap> {
        ensure!(
            width > 0 && height > 0,
            "image has zero size ({width}x{height})"
        );
        ensure!(
            width <= MAX_DIMENSION && height <= MAX_DIMENSION,
            "image is {width}x{height}, larger than the {MAX_DIMENSION}x{MAX_DIMENSION} limit"
        );
        ensure!(
            rgba.len() == width as usize * height as usize * 4,
            "pixel buffer has {} bytes, expected {}",
            rgba.len(),
            width as usize * height as usize * 4
        );
        Ok(Bitmap {
            width,
            height,
            rgba,
        })
    }

    /// A fully transparent bitmap. `width` and `height` must be non-zero.
    pub fn transparent(width: u32, height: u32) -> Bitmap {
        Bitmap {
            width,
            height,
            rgba: vec![0; width as usize * height as usize * 4],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// `RGBARGBA…` bytes, row-major.
    pub fn as_raw(&self) -> &[u8] {
        &self.rgba
    }

    /// The `[r, g, b, a]` pixel at `(x, y)`, or `None` outside the bitmap.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        (x < self.width && y < self.height).then(|| {
            let i = (y as usize * self.width as usize + x as usize) * 4;
            [
                self.rgba[i],
                self.rgba[i + 1],
                self.rgba[i + 2],
                self.rgba[i + 3],
            ]
        })
    }

    /// Render this image into a new `zone_w × zone_h` bitmap according to
    /// `fit` and `align`. Areas the image does not cover are transparent;
    /// parts of the image outside the zone are cropped.
    ///
    /// `smooth` averages source pixels (box filter, alpha-weighted) on axes
    /// that shrink; axes that grow always use nearest-neighbour so pixel art
    /// stays sharp.
    pub fn fitted(
        &self,
        zone_w: u32,
        zone_h: u32,
        fit: Fit,
        align: ImageAlign,
        smooth: bool,
    ) -> Bitmap {
        if zone_w == 0 || zone_h == 0 {
            return Bitmap {
                width: zone_w,
                height: zone_h,
                rgba: Vec::new(),
            };
        }
        let (sw, sh) = (u64::from(self.width), u64::from(self.height));
        let (zw, zh) = (u64::from(zone_w), u64::from(zone_h));
        let by_width = (zw, ((sh * zw * 2 + sw) / (2 * sw)).max(1));
        let by_height = (((sw * zh * 2 + sh) / (2 * sh)).max(1), zh);
        let (nw, nh) = match fit {
            Fit::None => (sw, sh),
            Fit::Stretch => (zw, zh),
            // Width-limited when the zone is relatively narrower than the image.
            Fit::Contain => {
                if zw * sh <= zh * sw {
                    by_width
                } else {
                    by_height
                }
            }
            Fit::Cover => {
                if zw * sh >= zh * sw {
                    by_width
                } else {
                    by_height
                }
            }
        };
        let (nw, nh) = (nw as u32, nh as u32);

        let resized;
        let src = if (nw, nh) == (self.width, self.height) {
            self
        } else {
            resized = self.resized(nw, nh, smooth);
            &resized
        };

        let (fx, fy) = align.factors();
        let ox = ((i64::from(zone_w) - i64::from(nw)) * fx).div_euclid(2);
        let oy = ((i64::from(zone_h) - i64::from(nh)) * fy).div_euclid(2);

        let mut out = Bitmap::transparent(zone_w, zone_h);
        for dy in 0..zone_h {
            let sy = i64::from(dy) - oy;
            if sy < 0 || sy >= i64::from(nh) {
                continue;
            }
            let x0 = ox.max(0);
            let x1 = (ox + i64::from(nw)).min(i64::from(zone_w));
            if x0 >= x1 {
                continue;
            }
            let n = (x1 - x0) as usize * 4;
            let d = (dy as usize * zone_w as usize + x0 as usize) * 4;
            let s = (sy as usize * nw as usize + (x0 - ox) as usize) * 4;
            out.rgba[d..d + n].copy_from_slice(&src.rgba[s..s + n]);
        }
        out
    }

    /// Resize to `nw × nh` (both non-zero).
    fn resized(&self, nw: u32, nh: u32, smooth: bool) -> Bitmap {
        let xs = axis_taps(self.width, nw, smooth);
        let ys = axis_taps(self.height, nh, smooth);
        let mut rgba = Vec::with_capacity(nw as usize * nh as usize * 4);
        for ty in &ys {
            for tx in &xs {
                if let ([(sx, _)], [(sy, _)]) = (tx.as_slice(), ty.as_slice()) {
                    let i = (*sy as usize * self.width as usize + *sx as usize) * 4;
                    rgba.extend_from_slice(&self.rgba[i..i + 4]);
                    continue;
                }
                // Alpha-weighted average: colours contribute in proportion to
                // their alpha so transparent pixels don't darken the edges.
                let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
                for &(sy, wy) in ty {
                    for &(sx, wx) in tx {
                        let i = (sy as usize * self.width as usize + sx as usize) * 4;
                        let w = wx * wy;
                        let pa = f32::from(self.rgba[i + 3]) * w;
                        r += f32::from(self.rgba[i]) * pa;
                        g += f32::from(self.rgba[i + 1]) * pa;
                        b += f32::from(self.rgba[i + 2]) * pa;
                        a += pa;
                    }
                }
                if a <= 0.0 {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                } else {
                    rgba.extend_from_slice(&[
                        (r / a).round() as u8,
                        (g / a).round() as u8,
                        (b / a).round() as u8,
                        a.round() as u8,
                    ]);
                }
            }
        }
        Bitmap {
            width: nw,
            height: nh,
            rgba,
        }
    }
}

/// For each of `dst` output positions, the source indices and normalised
/// weights that feed it.
fn axis_taps(src: u32, dst: u32, smooth: bool) -> Vec<Vec<(u32, f32)>> {
    let (s, d) = (u64::from(src), u64::from(dst));
    (0..d)
        .map(|i| {
            if !smooth || dst >= src {
                // Nearest: the source pixel under the centre of the output pixel.
                return vec![(((2 * i + 1) * s / (2 * d)) as u32, 1.0)];
            }
            // Box filter over the source span [i*s/d, (i+1)*s/d).
            let (lo, hi) = (
                i as f64 * s as f64 / d as f64,
                (i + 1) as f64 * s as f64 / d as f64,
            );
            let first = lo.floor() as u32;
            let last = (hi.ceil() as u32).min(src).max(first + 1);
            let mut taps: Vec<(u32, f32)> = (first..last)
                .map(|p| {
                    let w = (hi.min(f64::from(p) + 1.0) - lo.max(f64::from(p))).max(0.0);
                    (p, w as f32)
                })
                .filter(|(_, w)| *w > 0.0)
                .collect();
            let total: f32 = taps.iter().map(|(_, w)| w).sum();
            for t in &mut taps {
                t.1 /= total;
            }
            taps
        })
        .collect()
}

/// One displayed frame and how long it stays up.
#[derive(Debug, Clone)]
pub struct MediaFrame {
    pub bitmap: Arc<Bitmap>,
    pub delay: Duration,
}

/// A still image (one frame) or an animation.
#[derive(Debug, Clone)]
pub struct Media {
    frames: Vec<MediaFrame>,
    total: Duration,
}

impl Media {
    /// A single-frame media from an existing bitmap.
    pub fn still(bitmap: Arc<Bitmap>) -> Media {
        Media {
            frames: vec![MediaFrame {
                bitmap,
                delay: Duration::ZERO,
            }],
            total: Duration::ZERO,
        }
    }

    pub fn frames(&self) -> &[MediaFrame] {
        &self.frames
    }

    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Which frame is showing `elapsed` after the start. Looping wraps around;
    /// without `looping` the last frame stays up.
    pub fn frame_at(&self, elapsed: Duration, looping: bool) -> usize {
        if !self.is_animated() || self.total.is_zero() {
            return 0;
        }
        let t = if looping {
            Duration::from_nanos((elapsed.as_nanos() % self.total.as_nanos()) as u64)
        } else {
            elapsed
        };
        let mut end = Duration::ZERO;
        for (i, f) in self.frames.iter().enumerate() {
            end += f.delay;
            if t < end {
                return i;
            }
        }
        self.frames.len() - 1
    }

    /// Read and decode a PNG or GIF file. **Blocking file I/O**: never call
    /// this from the render loop.
    pub fn load(path: &Path) -> anyhow::Result<Media> {
        let meta = std::fs::metadata(path).context("cannot read file")?;
        ensure!(
            meta.len() <= MAX_FILE_BYTES,
            "file is {} bytes, larger than the {MAX_FILE_BYTES} byte limit",
            meta.len()
        );
        let bytes = std::fs::read(path).context("cannot read file")?;
        Media::decode(&bytes)
    }

    /// Decode PNG or GIF bytes, choosing the format from the file signature.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Media> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            decode_png(bytes).context("invalid PNG")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            decode_gif(bytes).context("invalid GIF")
        } else {
            bail!("unsupported image format (only PNG and GIF are supported)")
        }
    }
}

fn decode_png(bytes: &[u8]) -> anyhow::Result<Media> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // EXPAND: palette and low-bit-depth grey to 8-bit, tRNS to an alpha channel.
    // STRIP_16: 16-bit samples to 8-bit.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .context("image is too large to decode")?;
    ensure!(
        size <= MAX_DECODED_BYTES,
        "image is too large to decode ({size} bytes)"
    );
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf)?;
    ensure!(
        info.bit_depth == png::BitDepth::Eight,
        "unexpected bit depth {:?}",
        info.bit_depth
    );
    let px = &buf[..info.buffer_size()];
    let n = info.width as usize * info.height as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(px),
        png::ColorType::Rgb => {
            for p in px.chunks_exact(3) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &v in px {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in px.chunks_exact(2) {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Indexed => bail!("palette was not expanded"),
    }
    Ok(Media::still(Arc::new(Bitmap::new(
        info.width,
        info.height,
        rgba,
    )?)))
}

fn decode_gif(bytes: &[u8]) -> anyhow::Result<Media> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(Cursor::new(bytes))?;
    let (sw, sh) = (u32::from(decoder.width()), u32::from(decoder.height()));
    ensure!(sw > 0 && sh > 0, "logical screen has zero size");
    ensure!(
        sw <= MAX_DIMENSION && sh <= MAX_DIMENSION,
        "image is {sw}x{sh}, larger than the {MAX_DIMENSION}x{MAX_DIMENSION} limit"
    );
    let frame_bytes = sw as usize * sh as usize * 4;

    let mut screen = vec![0u8; frame_bytes];
    // Disposal to apply before drawing the next frame.
    let mut pending: Option<(gif::DisposalMethod, [u32; 4], Option<Vec<u8>>)> = None;
    let mut frames: Vec<MediaFrame> = Vec::new();

    while let Some(frame) = decoder.read_next_frame()? {
        ensure!(
            (frames.len() + 1) * frame_bytes <= MAX_DECODED_BYTES,
            "animation is too large to decode (over {MAX_DECODED_BYTES} bytes of frames)"
        );
        if let Some((method, [x, y, w, h], saved)) = pending.take() {
            match method {
                gif::DisposalMethod::Background => {
                    clear_rect(&mut screen, sw, sh, x, y, w, h);
                }
                gif::DisposalMethod::Previous => {
                    if let Some(saved) = saved {
                        screen = saved;
                    }
                }
                gif::DisposalMethod::Any | gif::DisposalMethod::Keep => {}
            }
        }
        let saved = (frame.dispose == gif::DisposalMethod::Previous).then(|| screen.clone());

        let (fl, ft) = (u32::from(frame.left), u32::from(frame.top));
        let (fw, fh) = (u32::from(frame.width), u32::from(frame.height));
        ensure!(
            frame.buffer.len() == fw as usize * fh as usize * 4,
            "frame buffer has an unexpected size"
        );
        for row in 0..fh {
            let y = ft + row;
            if y >= sh {
                break;
            }
            for col in 0..fw {
                let x = fl + col;
                if x >= sw {
                    break;
                }
                let s = (row as usize * fw as usize + col as usize) * 4;
                let px = &frame.buffer[s..s + 4];
                if px[3] == 0 {
                    continue; // transparent: keep what is underneath
                }
                let d = (y as usize * sw as usize + x as usize) * 4;
                screen[d..d + 4].copy_from_slice(px);
            }
        }

        frames.push(MediaFrame {
            bitmap: Arc::new(Bitmap::new(sw, sh, screen.clone())?),
            delay: frame_delay(frame.delay),
        });
        pending = Some((frame.dispose, [fl, ft, fw, fh], saved));
    }

    ensure!(!frames.is_empty(), "GIF has no frames");
    if frames.len() == 1 {
        let only = frames.remove(0);
        return Ok(Media::still(only.bitmap));
    }
    let total = frames.iter().map(|f| f.delay).sum();
    Ok(Media { frames, total })
}

fn clear_rect(screen: &mut [u8], sw: u32, sh: u32, x: u32, y: u32, w: u32, h: u32) {
    for yy in y..(y + h).min(sh) {
        for xx in x..(x + w).min(sw) {
            let d = (yy as usize * sw as usize + xx as usize) * 4;
            screen[d..d + 4].fill(0);
        }
    }
}

/// GIF delay (centiseconds) to a frame duration; see the module docs.
fn frame_delay(centis: u16) -> Duration {
    let d = Duration::from_millis(u64::from(centis) * 10);
    if d < MIN_FRAME_DELAY {
        DEFAULT_FRAME_DELAY
    } else {
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/media")
            .join(name)
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const YELLOW: [u8; 4] = [255, 255, 0, 255];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];

    fn bmp(w: u32, h: u32, px: &[[u8; 4]]) -> Bitmap {
        Bitmap::new(w, h, px.concat()).unwrap()
    }

    #[test]
    fn decodes_palette_png() {
        let m = Media::load(&fixture("palette.png")).unwrap();
        assert!(!m.is_animated());
        let b = &m.frames()[0].bitmap;
        assert_eq!((b.width(), b.height()), (4, 2));
        let want = [[RED, GREEN, BLUE, YELLOW], [YELLOW, BLUE, GREEN, RED]];
        for (y, row) in want.iter().enumerate() {
            for (x, c) in row.iter().enumerate() {
                assert_eq!(b.pixel(x as u32, y as u32), Some(*c), "({x},{y})");
            }
        }
    }

    #[test]
    fn decodes_rgba_png_with_partial_alpha() {
        let m = Media::load(&fixture("alpha.png")).unwrap();
        let b = &m.frames()[0].bitmap;
        assert_eq!((b.width(), b.height()), (4, 4));
        assert_eq!(b.pixel(0, 0), Some(RED));
        assert_eq!(b.pixel(1, 0), Some(GREEN));
        assert_eq!(b.pixel(2, 0), Some([0, 0, 255, 128]));
        assert_eq!(b.pixel(3, 0).unwrap()[3], 0);
        assert_eq!(b.pixel(0, 1), Some([255, 255, 255, 128]));
        assert_eq!(b.pixel(3, 2), Some([255, 255, 255, 128]));
        assert_eq!(b.pixel(2, 3), Some(YELLOW));
    }

    fn encode_png(
        w: u32,
        h: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
        extra: impl FnOnce(&mut png::Encoder<'_, &mut Vec<u8>>),
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        extra(&mut enc);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn normalises_every_png_colour_type() {
        use png::{BitDepth as D, ColorType as C};
        let px = |bytes: &[u8]| Media::decode(bytes).unwrap().frames()[0].bitmap.clone();

        let rgb = encode_png(2, 1, C::Rgb, D::Eight, &[1, 2, 3, 4, 5, 6], |_| {});
        let b = px(&rgb);
        assert_eq!(b.pixel(0, 0), Some([1, 2, 3, 255]));
        assert_eq!(b.pixel(1, 0), Some([4, 5, 6, 255]));

        let grey = encode_png(2, 1, C::Grayscale, D::Eight, &[10, 200], |_| {});
        assert_eq!(px(&grey).pixel(1, 0), Some([200, 200, 200, 255]));

        let grey1 = encode_png(8, 1, C::Grayscale, D::One, &[0b1010_0000], |_| {});
        let b = px(&grey1);
        assert_eq!(b.pixel(0, 0), Some([255, 255, 255, 255]));
        assert_eq!(b.pixel(1, 0), Some([0, 0, 0, 255]));

        let ga = encode_png(1, 1, C::GrayscaleAlpha, D::Eight, &[90, 40], |_| {});
        assert_eq!(px(&ga).pixel(0, 0), Some([90, 90, 90, 40]));

        // 16-bit RGBA: the high byte of each sample is kept.
        let rgba16 = encode_png(
            1,
            1,
            C::Rgba,
            D::Sixteen,
            &[0xff, 0x00, 0x80, 0x00, 0x12, 0x34, 0xab, 0xcd],
            |_| {},
        );
        assert_eq!(px(&rgba16).pixel(0, 0), Some([0xff, 0x80, 0x12, 0xab]));

        // Palette with a tRNS chunk: entry 1 is half transparent.
        let idx = encode_png(2, 1, C::Indexed, D::Eight, &[0, 1], |e| {
            e.set_palette(vec![10, 20, 30, 40, 50, 60]);
            e.set_trns(vec![255, 100]);
        });
        let b = px(&idx);
        assert_eq!(b.pixel(0, 0), Some([10, 20, 30, 255]));
        assert_eq!(b.pixel(1, 0), Some([40, 50, 60, 100]));
    }

    #[test]
    fn rejects_garbage_and_truncated_files() {
        assert!(format!("{:#}", Media::decode(b"hello").unwrap_err()).contains("unsupported"));
        let m = std::fs::read(fixture("palette.png")).unwrap();
        assert!(Media::decode(&m[..m.len() / 2]).is_err());
        let g = std::fs::read(fixture("anim.gif")).unwrap();
        assert!(Media::decode(&g[..20]).is_err());
    }

    #[test]
    fn composites_gif_frames_with_disposal_and_transparency() {
        let m = Media::load(&fixture("anim.gif")).unwrap();
        assert!(m.is_animated());
        assert_eq!(m.frames().len(), 3);
        let delays: Vec<_> = m.frames().iter().map(|f| f.delay.as_millis()).collect();
        assert_eq!(delays, [100, 200, 50]);
        let f = |i: usize| m.frames()[i].bitmap.clone();
        for b in [f(0), f(1), f(2)] {
            assert_eq!((b.width(), b.height()), (8, 8));
        }

        // Frame 0: all red.
        assert!((0..8).all(|y| (0..8).all(|x| f(0).pixel(x, y) == Some(RED))));

        // Frame 1: blue square at (2..6, 2..6) over red.
        for y in 0..8 {
            for x in 0..8 {
                let want = if (2..6).contains(&x) && (2..6).contains(&y) {
                    BLUE
                } else {
                    RED
                };
                assert_eq!(f(1).pixel(x, y), Some(want), "f1 ({x},{y})");
            }
        }

        // Frame 2: disposal 2 cleared the blue square; green at (0..4,0..4)
        // with a transparent hole at (1..3,1..3) that shows what is below:
        // red where the square never was, cleared where it was.
        for y in 0..8u32 {
            for x in 0..8u32 {
                let hole = (1..3).contains(&x) && (1..3).contains(&y);
                let square = (2..6).contains(&x) && (2..6).contains(&y);
                let want = if x < 4 && y < 4 && !hole {
                    GREEN
                } else if square {
                    CLEAR
                } else {
                    RED
                };
                assert_eq!(f(2).pixel(x, y), Some(want), "f2 ({x},{y})");
            }
        }
    }

    #[test]
    fn gif_delay_rule() {
        assert_eq!(frame_delay(0), DEFAULT_FRAME_DELAY);
        assert_eq!(frame_delay(1), DEFAULT_FRAME_DELAY);
        assert_eq!(frame_delay(2), Duration::from_millis(20));
        assert_eq!(frame_delay(10), Duration::from_millis(100));
    }

    #[test]
    fn frame_selection_by_elapsed_time() {
        let m = Media::load(&fixture("anim.gif")).unwrap(); // 100, 200, 50 ms
        let at = |ms: u64, looping| m.frame_at(Duration::from_millis(ms), looping);
        assert_eq!(at(0, true), 0);
        assert_eq!(at(99, true), 0);
        assert_eq!(at(100, true), 1);
        assert_eq!(at(299, true), 1);
        assert_eq!(at(300, true), 2);
        assert_eq!(at(349, true), 2);
        assert_eq!(at(350, true), 0); // wraps
        assert_eq!(at(1000, false), 2); // stays on the last frame
        assert_eq!(at(120, false), 1);
    }

    #[test]
    fn nearest_scaling_and_fit_modes() {
        // 4x2 source into a 8x8 zone.
        let b = bmp(4, 2, &[RED, GREEN, BLUE, YELLOW, YELLOW, BLUE, GREEN, RED]);
        // contain: 8x4, centred vertically at y=2..6.
        let c = b.fitted(8, 8, Fit::Contain, ImageAlign::Center, false);
        assert_eq!(c.pixel(0, 1), Some(CLEAR));
        assert_eq!(c.pixel(0, 2), Some(RED));
        assert_eq!(c.pixel(1, 3), Some(RED));
        assert_eq!(c.pixel(2, 2), Some(GREEN));
        assert_eq!(c.pixel(0, 4), Some(YELLOW));
        assert_eq!(c.pixel(7, 5), Some(RED));
        assert_eq!(c.pixel(7, 6), Some(CLEAR));
        // contain aligned top.
        let t = b.fitted(8, 8, Fit::Contain, ImageAlign::Top, false);
        assert_eq!(t.pixel(0, 0), Some(RED));
        assert_eq!(t.pixel(0, 4), Some(CLEAR));
        // cover: 16x8, cropped horizontally; centre keeps source x 1..3.
        let v = b.fitted(8, 8, Fit::Cover, ImageAlign::Center, false);
        assert_eq!(v.pixel(0, 0), Some(GREEN));
        assert_eq!(v.pixel(3, 0), Some(GREEN));
        assert_eq!(v.pixel(4, 0), Some(BLUE));
        assert_eq!(v.pixel(7, 7), Some(GREEN));
        // stretch fills the zone.
        let s = b.fitted(8, 4, Fit::Stretch, ImageAlign::Center, false);
        assert_eq!(s.pixel(0, 0), Some(RED));
        assert_eq!(s.pixel(7, 3), Some(RED));
        assert!(s.as_raw().chunks_exact(4).all(|p| p[3] == 255));
        // none: native size placed by alignment, cropped if too big.
        let n = b.fitted(6, 4, Fit::None, ImageAlign::BottomRight, false);
        assert_eq!(n.pixel(2, 2), Some(RED));
        assert_eq!(n.pixel(5, 3), Some(RED));
        assert_eq!(n.pixel(1, 2), Some(CLEAR));
        let k = b.fitted(2, 2, Fit::None, ImageAlign::Center, false);
        assert_eq!(k.pixel(0, 0), Some(GREEN));
        assert_eq!(k.pixel(1, 1), Some(GREEN));
    }

    #[test]
    fn smooth_downscale_averages() {
        // 4x1 source: black, white, transparent, red -> 2x1.
        let b = bmp(4, 1, &[[0, 0, 0, 255], [255, 255, 255, 255], CLEAR, RED]);
        let s = b.fitted(2, 1, Fit::Stretch, ImageAlign::Center, true);
        assert_eq!(s.pixel(0, 0), Some([128, 128, 128, 255]));
        // Transparent pixel contributes no colour, half the alpha.
        assert_eq!(s.pixel(1, 0), Some([255, 0, 0, 128]));
        // Without smoothing the source pixel under each output centre is picked.
        let n = b.fitted(2, 1, Fit::Stretch, ImageAlign::Center, false);
        assert_eq!(n.pixel(0, 0), Some([255, 255, 255, 255]));
        assert_eq!(n.pixel(1, 0), Some(RED));
    }

    #[test]
    fn smooth_does_not_blur_upscales() {
        let b = bmp(2, 1, &[RED, BLUE]);
        let s = b.fitted(4, 1, Fit::Stretch, ImageAlign::Center, true);
        assert_eq!(s.pixel(1, 0), Some(RED));
        assert_eq!(s.pixel(2, 0), Some(BLUE));
    }

    #[test]
    fn bitmap_validates_size() {
        assert!(Bitmap::new(0, 1, vec![]).is_err());
        assert!(Bitmap::new(1, 1, vec![0; 3]).is_err());
        assert!(Bitmap::new(MAX_DIMENSION + 1, 1, vec![0; 4]).is_err());
    }
}
