//! Per-sink colour post-processing (DESIGN.md §5.2).
//!
//! The preview shows the canvas as rendered; a panel sink runs it through a
//! [`PostProcess`] first.
//!
//! # Order of operations
//!
//! For each channel `c` and input value `v` (0..=255), computed once at
//! construction:
//!
//! 1. **Gamma**: `x = (v / 255) ^ gamma`. Canvas values are perceptual
//!    (sRGB-like); LEDs are linear in duty cycle, so this converts to the
//!    light output we want.
//! 2. **White point**: `x *= white_point[c] / 255`. Scaling happens after gamma
//!    because colour temperature and brightness are multipliers on emitted
//!    light, which is linear.
//! 3. **Brightness** (software mode only): `x *= brightness / 255`, for the
//!    same reason.
//! 4. Round once: `out = round(x * 255)`.
//!
//! Doing all steps in floating point with a single rounding at the end keeps
//! the dark end of the range as accurate as 8 bits allow. The result is one
//! 256-entry table per channel, so applying it is one lookup per channel per
//! pixel and allocates nothing (given a reused destination buffer).

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};

use crate::canvas::Canvas;
use crate::color::Color;
use crate::config::{BrightnessMode, ColorlightOutput};

/// Settings for building a [`PostProcess`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PostProcessParams {
    pub gamma: f32,
    pub white_point: Color,
    /// Software global brightness, 0..=255. Use 255 when the card applies it.
    pub brightness: u8,
}

impl Default for PostProcessParams {
    /// The identity: gamma 1.0, white `#ffffff`, brightness 255.
    fn default() -> Self {
        PostProcessParams {
            gamma: 1.0,
            white_point: Color::WHITE,
            brightness: 255,
        }
    }
}

/// Precomputed gamma, white point and brightness, one LUT per channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostProcess {
    lut: [[u8; 256]; 3],
}

impl PostProcess {
    pub fn new(p: PostProcessParams) -> Self {
        let wp = [
            p.white_point.0.r(),
            p.white_point.0.g(),
            p.white_point.0.b(),
        ];
        let brightness = f64::from(p.brightness) / 255.0;
        let gamma = f64::from(p.gamma);
        let mut lut = [[0u8; 256]; 3];
        for (table, w) in lut.iter_mut().zip(wp) {
            let scale = f64::from(w) / 255.0 * brightness;
            for (v, out) in table.iter_mut().enumerate() {
                let x = (v as f64 / 255.0).powf(gamma) * scale;
                *out = (x * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        PostProcess { lut }
    }

    /// Build from a colorlight output config. In hardware brightness mode the
    /// card applies `brightness`, so the LUT does not scale by it.
    pub fn from_output(o: &ColorlightOutput) -> Self {
        PostProcess::new(PostProcessParams {
            gamma: o.gamma,
            white_point: o.white_point,
            brightness: match o.brightness_mode {
                BrightnessMode::Hardware => 255,
                BrightnessMode::Software => o.brightness,
            },
        })
    }

    /// Correct one pixel.
    #[inline]
    pub fn apply_pixel(&self, p: Rgb888) -> Rgb888 {
        Rgb888::new(
            self.lut[0][usize::from(p.r())],
            self.lut[1][usize::from(p.g())],
            self.lut[2][usize::from(p.b())],
        )
    }

    /// Replace `dst` with the corrected canvas as packed row-major `RGBRGB…`
    /// bytes. Reuses `dst`'s capacity, so a sink that keeps one buffer does not
    /// allocate per frame. Channel reordering (e.g. BGR) is the sink's job.
    pub fn apply_into(&self, src: &Canvas, dst: &mut Vec<u8>) {
        dst.clear();
        dst.reserve(src.pixels().len() * 3);
        dst.extend(src.pixels().iter().flat_map(|p| {
            [
                self.lut[0][usize::from(p.r())],
                self.lut[1][usize::from(p.g())],
                self.lut[2][usize::from(p.b())],
            ]
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, OutputConfig};

    fn params(gamma: f32, white: &str, brightness: u8) -> PostProcessParams {
        PostProcessParams {
            gamma,
            white_point: white.parse().unwrap(),
            brightness,
        }
    }

    #[test]
    fn identity_is_a_no_op() {
        let pp = PostProcess::new(PostProcessParams::default());
        for table in &pp.lut {
            for (v, &out) in table.iter().enumerate() {
                assert_eq!(usize::from(out), v);
            }
        }
        let mut cv = Canvas::new(2, 1);
        cv.set_pixel(0, 0, Rgb888::new(1, 128, 254));
        cv.set_pixel(1, 0, Rgb888::new(255, 0, 7));
        let mut out = Vec::new();
        pp.apply_into(&cv, &mut out);
        assert_eq!(out, cv.to_rgb_bytes());
    }

    #[test]
    fn gamma_endpoints_are_fixed() {
        for g in [1.0, 1.8, 2.2, 4.0] {
            let pp = PostProcess::new(params(g, "#ffffff", 255));
            for table in &pp.lut {
                assert_eq!(table[0], 0, "gamma {g}");
                assert_eq!(table[255], 255, "gamma {g}");
            }
        }
    }

    #[test]
    fn gamma_curve_values() {
        let pp = PostProcess::new(params(2.2, "#ffffff", 255));
        // (128/255)^2.2 * 255 = 55.7
        assert_eq!(pp.lut[0][128], 56);
        // Dark values collapse toward zero.
        assert_eq!(pp.lut[0][1], 0);
    }

    #[test]
    fn luts_are_monotonic() {
        for p in [
            params(2.2, "#ffffff", 255),
            params(2.8, "#ffd8b0", 100),
            params(1.0, "#102030", 7),
        ] {
            let pp = PostProcess::new(p);
            for table in &pp.lut {
                assert!(table.windows(2).all(|w| w[0] <= w[1]), "{p:?}");
            }
        }
    }

    #[test]
    fn brightness_scales_light() {
        let pp = PostProcess::new(params(1.0, "#ffffff", 128));
        assert_eq!(pp.lut[0][255], 128);
        assert_eq!(pp.lut[0][100], 50); // 100 * 128/255 = 50.2
        let off = PostProcess::new(params(2.2, "#ffffff", 0));
        assert!(off.lut.iter().all(|t| t.iter().all(|&v| v == 0)));
    }

    #[test]
    fn white_point_scales_each_channel() {
        let pp = PostProcess::new(params(1.0, "#ff8000", 255));
        assert_eq!(pp.apply_pixel(Rgb888::WHITE), Rgb888::new(255, 128, 0));
        assert_eq!(
            pp.apply_pixel(Rgb888::new(0, 100, 100)),
            Rgb888::new(0, 50, 0)
        );
    }

    #[test]
    fn rounds_to_nearest() {
        // v * 127/255 for v = 1..=4 is 0.498, 0.996, 1.494, 1.992.
        let pp = PostProcess::new(params(1.0, "#7f7f7f", 255));
        assert_eq!(&pp.lut[0][1..5], &[0, 1, 1, 2]);
    }

    #[test]
    fn apply_into_reuses_buffer_and_matches_apply_pixel() {
        let pp = PostProcess::new(params(2.2, "#ffd8b0", 200));
        let mut cv = Canvas::new(3, 2);
        for y in 0..2 {
            for x in 0..3 {
                cv.set_pixel(x, y, Rgb888::new((x * 80) as u8, (y * 120) as u8, 200));
            }
        }
        let mut out = vec![9; 100];
        pp.apply_into(&cv, &mut out);
        assert_eq!(out.len(), 18);
        let cap = out.capacity();
        let want: Vec<u8> = cv
            .pixels()
            .iter()
            .flat_map(|&p| {
                let q = pp.apply_pixel(p);
                [q.r(), q.g(), q.b()]
            })
            .collect();
        assert_eq!(out, want);
        pp.apply_into(&cv, &mut out);
        assert_eq!(out.capacity(), cap);
    }

    #[test]
    fn from_output_honours_brightness_mode() {
        let parse = |mode: &str| -> ColorlightOutput {
            let cfg = Config::parse(&format!(
                "[canvas]\nwidth = 8\nheight = 8\n[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ngamma = 1.0\nbrightness = 128\nbrightness_mode = \"{mode}\"\n[[scene]]\nname = \"a\"\n"
            ))
            .unwrap();
            match cfg.outputs.into_iter().next().unwrap() {
                OutputConfig::Colorlight(c) => c,
                _ => panic!("expected colorlight"),
            }
        };
        let hw = parse("hardware");
        assert_eq!(PostProcess::from_output(&hw).lut[0][255], 255);
        assert_eq!(hw.card_brightness(), 128);
        let sw = parse("software");
        assert_eq!(PostProcess::from_output(&sw).lut[0][255], 128);
        assert_eq!(sw.card_brightness(), 255);
    }
}
