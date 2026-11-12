//! Config file schema (DESIGN.md §8) and validation.
//!
//! Only what the current milestone implements is accepted; unknown keys are
//! rejected so typos don't silently do nothing.

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use chrono::format::{Item, StrftimeItems};
use serde::Deserialize;

use crate::canvas::Rect;
use crate::color::Color;
use crate::fonts;

pub const MAX_FPS: u32 = 60;
pub const MAX_PREVIEW_SCALE: u32 = 32;
pub const MIN_GAMMA: f32 = 1.0;
pub const MAX_GAMMA: f32 = 4.0;
pub const MAX_SYNC_GAP_US: u32 = 20_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub canvas: CanvasConfig,
    #[serde(default, rename = "output")]
    pub outputs: Vec<OutputConfig>,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default, rename = "scene")]
    pub scenes: Vec<SceneConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasConfig {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
}

fn default_fps() -> u32 {
    30
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputConfig {
    Preview(PreviewOutput),
    Colorlight(ColorlightOutput),
}

/// A Colorlight 5A-75B card reached over raw Ethernet (DESIGN.md §4, §8).
///
/// In M1 one output maps the whole canvas to card origin (0,0); multi-region
/// output maps arrive in M6.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorlightOutput {
    /// Network interface the card is attached to, e.g. `eth1`.
    pub interface: String,
    /// Gamma exponent for the panel's colour LUT. Valid: 1.0..=4.0.
    #[serde(default = "default_gamma")]
    pub gamma: f32,
    /// Global brightness 0..=255 (see `brightness_mode`).
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// Colour a full-white pixel should become; scales R, G and B to match
    /// the panel's colour temperature.
    #[serde(default = "default_white_point")]
    pub white_point: Color,
    #[serde(default)]
    pub brightness_mode: BrightnessMode,
    /// Card firmware major version, for when discovery fails (DESIGN.md §4.2).
    #[serde(default)]
    pub firmware_major: Option<u8>,
    /// Pause between the last row packet and the display frame, in µs.
    #[serde(default = "default_sync_gap_us")]
    pub sync_gap_us: u32,
}

impl ColorlightOutput {
    /// The brightness byte to put in the card's display frame: the configured
    /// brightness in hardware mode, 255 in software mode (the LUT scales).
    pub fn card_brightness(&self) -> u8 {
        match self.brightness_mode {
            BrightnessMode::Hardware => self.brightness,
            BrightnessMode::Software => 255,
        }
    }
}

/// Where global brightness is applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrightnessMode {
    /// In the card's display frame; the colour LUT does not scale.
    #[default]
    Hardware,
    /// In the colour LUT; the card is told 255.
    Software,
}

fn default_gamma() -> f32 {
    2.2
}

fn default_brightness() -> u8 {
    255
}

fn default_white_point() -> Color {
    Color::WHITE
}

fn default_sync_gap_us() -> u32 {
    500
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewOutput {
    /// Each canvas pixel becomes a `scale × scale` block.
    #[serde(default = "default_scale")]
    pub scale: u32,
    /// Draw dark gaps between pixels to look more like a panel.
    #[serde(default)]
    pub grid: bool,
}

fn default_scale() -> u32 {
    8
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            bind: default_bind(),
        }
    }
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneConfig {
    pub name: String,
    #[serde(default, rename = "zone")]
    pub zones: Vec<ZoneConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneConfig {
    pub rect: Rect,
    pub widget: WidgetConfig,
    /// Fill colour behind the widget. Default: transparent (canvas is black).
    #[serde(default)]
    pub background: Option<Color>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WidgetConfig {
    Clock(ClockConfig),
    Text(TextConfig),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockConfig {
    /// chrono `strftime` format, e.g. `"%H:%M"`.
    #[serde(default = "default_clock_format")]
    pub format: String,
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_color")]
    pub color: Color,
    #[serde(default)]
    pub align: Align,
}

fn default_clock_format() -> String {
    "%H:%M".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextConfig {
    pub text: String,
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_color")]
    pub color: Color,
    /// Ignored while scrolling.
    #[serde(default)]
    pub align: Align,
    /// Scroll right-to-left at this many pixels per second. Unset = static.
    #[serde(default)]
    pub scroll_px_per_s: Option<f32>,
}

fn default_font() -> String {
    fonts::DEFAULT_FONT.into()
}

fn default_color() -> Color {
    Color::WHITE
}

/// Every problem found while validating, so users can fix them in one go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<String>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid config:")?;
        for e in &self.0 {
            write!(f, "\n  - {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

impl Config {
    /// Read, parse and validate a config file.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("loading config {}", path.display()))
    }

    /// Parse and validate config text.
    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn scene(&self, name: &str) -> Option<&SceneConfig> {
        self.scenes.iter().find(|s| s.name == name)
    }

    pub fn preview(&self) -> Option<&PreviewOutput> {
        self.outputs.iter().find_map(|o| match o {
            OutputConfig::Preview(p) => Some(p),
            _ => None,
        })
    }

    pub fn validate(&self) -> Result<(), ConfigErrors> {
        let mut errs = Vec::new();
        let c = &self.canvas;

        if c.width == 0 || c.height == 0 {
            errs.push(format!(
                "canvas: width and height must be > 0 (got {}x{})",
                c.width, c.height
            ));
        }
        if !(1..=MAX_FPS).contains(&c.fps) {
            errs.push(format!("canvas.fps: must be 1..={MAX_FPS} (got {})", c.fps));
        }

        let mut previews = 0;
        let mut interfaces: Vec<(&str, usize)> = Vec::new();
        for (i, o) in self.outputs.iter().enumerate() {
            match o {
                OutputConfig::Preview(p) => {
                    previews += 1;
                    if !(1..=MAX_PREVIEW_SCALE).contains(&p.scale) {
                        errs.push(format!(
                            "output[{i}] (preview): scale must be 1..={MAX_PREVIEW_SCALE} (got {})",
                            p.scale
                        ));
                    }
                }
                OutputConfig::Colorlight(cl) => {
                    let at = format!("output[{i}] (colorlight)");
                    if cl.interface.trim().is_empty() {
                        errs.push(format!("{at}: interface must not be empty"));
                    } else if let Some(&(_, first)) =
                        interfaces.iter().find(|(name, _)| *name == cl.interface)
                    {
                        errs.push(format!(
                            "{at}: interface {:?} is already used by output[{first}]",
                            cl.interface
                        ));
                    } else {
                        interfaces.push((&cl.interface, i));
                    }
                    if !(cl.gamma.is_finite() && (MIN_GAMMA..=MAX_GAMMA).contains(&cl.gamma)) {
                        errs.push(format!(
                            "{at}: gamma must be {MIN_GAMMA}..={MAX_GAMMA} (got {})",
                            cl.gamma
                        ));
                    }
                    if cl.sync_gap_us > MAX_SYNC_GAP_US {
                        errs.push(format!(
                            "{at}: sync_gap_us must be 0..={MAX_SYNC_GAP_US} (got {})",
                            cl.sync_gap_us
                        ));
                    }
                }
            }
        }
        if previews > 1 {
            errs.push("output: at most one preview output is allowed".into());
        }

        if self.scenes.is_empty() {
            errs.push("at least one [[scene]] is required".into());
        }
        let mut names = HashSet::new();
        for s in &self.scenes {
            if s.name.trim().is_empty() {
                errs.push("scene: name must not be empty".into());
            } else if !names.insert(s.name.as_str()) {
                errs.push(format!("scene {:?}: duplicate scene name", s.name));
            }
            for (i, z) in s.zones.iter().enumerate() {
                let at = format!("scene {:?} zone[{i}]", s.name);
                let r = z.rect;
                if r.w == 0 || r.h == 0 {
                    errs.push(format!("{at}: rect must have non-zero size"));
                } else if !r.fits_in(c.width, c.height) {
                    errs.push(format!(
                        "{at}: rect {{ x = {}, y = {}, w = {}, h = {} }} does not fit the {}x{} canvas",
                        r.x, r.y, r.w, r.h, c.width, c.height
                    ));
                }
                validate_widget(&z.widget, &at, &mut errs);
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(errs))
        }
    }
}

fn validate_widget(w: &WidgetConfig, at: &str, errs: &mut Vec<String>) {
    let font = match w {
        WidgetConfig::Clock(c) => {
            if c.format.is_empty() {
                errs.push(format!("{at} clock: format must not be empty"));
            } else if StrftimeItems::new(&c.format).any(|i| matches!(i, Item::Error)) {
                errs.push(format!(
                    "{at} clock: invalid strftime format {:?}",
                    c.format
                ));
            }
            &c.font
        }
        WidgetConfig::Text(t) => {
            if let Some(v) = t.scroll_px_per_s {
                if !(v.is_finite() && v > 0.0) {
                    errs.push(format!("{at} text: scroll_px_per_s must be > 0 (got {v})"));
                }
            }
            &t.font
        }
    };
    if fonts::lookup(font).is_none() {
        errs.push(format!(
            "{at}: unknown font {font:?} (known: {})",
            fonts::known_names()
        ));
    }
}
