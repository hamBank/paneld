//! Config file schema (DESIGN.md §8) and validation.
//!
//! Only what the current milestone implements is accepted; unknown keys are
//! rejected so typos don't silently do nothing.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::format::{Item, StrftimeItems};
use serde::Deserialize;

use crate::bdf::BdfFont;
use crate::canvas::Rect;
use crate::color::Color;
use crate::condition::Condition;
use crate::fonts::{self, FontFace};
use crate::format::NumberFormat;
use crate::icons;
use crate::media::Media;

pub const MAX_FPS: u32 = 60;
pub const MAX_PREVIEW_SCALE: u32 = 32;
pub const MIN_GAMMA: f32 = 1.0;
pub const MAX_GAMMA: f32 = 4.0;
pub const MAX_SYNC_GAP_US: u32 = 20_000;
/// Upper bound for scene durations, in seconds (one day).
pub const MAX_DURATION_S: f64 = 86_400.0;
pub const DEFAULT_SCENE_DURATION_S: f64 = 10.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub canvas: CanvasConfig,
    #[serde(default, rename = "output")]
    pub outputs: Vec<OutputConfig>,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub playlist: PlaylistConfig,
    /// BDF fonts, usable by name from any text, clock or value widget.
    #[serde(default, rename = "font")]
    pub fonts: Vec<FontConfig>,
    #[serde(default, rename = "scene")]
    pub scenes: Vec<SceneConfig>,
    /// Directory relative media and font paths are resolved against: the config file's
    /// directory for [`Config::load`], the working directory for
    /// [`Config::parse`].
    #[serde(skip)]
    pub base_dir: PathBuf,
}

/// A BDF font file made available under a name (`[[font]]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontConfig {
    /// Name widgets use in `font = "..."`. Must not be a built-in font name.
    pub name: String,
    /// `.bdf` file, relative to the config file's directory. Read and parsed
    /// once, while the config is loaded.
    pub path: PathBuf,
    /// Parsed font, filled in by [`Config::load_fonts`].
    #[serde(skip)]
    pub loaded: Option<Arc<BdfFont>>,
}

/// Playlist-wide settings (DESIGN.md §5.5).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistConfig {
    /// How long a scene is shown when it has no `duration_s` of its own.
    /// Valid: finite, > 0, at most 86400.
    #[serde(default = "default_scene_duration_s")]
    pub default_duration_s: f64,
}

impl Default for PlaylistConfig {
    fn default() -> Self {
        PlaylistConfig {
            default_duration_s: default_scene_duration_s(),
        }
    }
}

impl PlaylistConfig {
    pub fn default_duration(&self) -> Duration {
        seconds(self.default_duration_s)
    }
}

fn default_scene_duration_s() -> f64 {
    DEFAULT_SCENE_DURATION_S
}

/// Seconds to a `Duration`. Only for values that passed validation; anything
/// else is clamped to `0..=MAX_DURATION_S` rather than panicking.
pub fn seconds(s: f64) -> Duration {
    let s = if s.is_finite() { s } else { 0.0 };
    Duration::from_secs_f64(s.clamp(0.0, MAX_DURATION_S))
}

fn valid_duration(s: f64) -> bool {
    s.is_finite() && s > 0.0 && s <= MAX_DURATION_S
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
    /// Order of the three colour bytes on the wire. The protocol does not fix
    /// it; it depends on the panel (PROTOCOL.md, "Pixel byte order"). Find
    /// the right value with `paneld test-pattern --pattern bars`.
    #[serde(default)]
    pub color_order: ColorOrder,
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

/// Order of the colour bytes sent to the card, named by what each wire byte
/// carries: `Bgr` sends blue first, then green, then red.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorOrder {
    #[default]
    Rgb,
    Rbg,
    Grb,
    Gbr,
    Brg,
    Bgr,
}

impl ColorOrder {
    pub const ALL: [ColorOrder; 6] = [
        ColorOrder::Rgb,
        ColorOrder::Rbg,
        ColorOrder::Grb,
        ColorOrder::Gbr,
        ColorOrder::Brg,
        ColorOrder::Bgr,
    ];

    /// For wire byte `i`, the index (0 = R, 1 = G, 2 = B) of the canvas
    /// channel to send there.
    pub const fn source_channels(self) -> [usize; 3] {
        match self {
            ColorOrder::Rgb => [0, 1, 2],
            ColorOrder::Rbg => [0, 2, 1],
            ColorOrder::Grb => [1, 0, 2],
            ColorOrder::Gbr => [1, 2, 0],
            ColorOrder::Brg => [2, 0, 1],
            ColorOrder::Bgr => [2, 1, 0],
        }
    }

    /// Reorder packed `RGBRGB…` bytes in place. A trailing partial pixel is
    /// left alone.
    pub fn reorder_in_place(self, rgb: &mut [u8]) {
        if self == ColorOrder::Rgb {
            return;
        }
        let [a, b, c] = self.source_channels();
        for px in rgb.chunks_exact_mut(3) {
            let (r, g, bl) = (px[0], px[1], px[2]);
            let src = [r, g, bl];
            px[0] = src[a];
            px[1] = src[b];
            px[2] = src[c];
        }
    }
}

impl std::str::FromStr for ColorOrder {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        ColorOrder::ALL
            .into_iter()
            .find(|o| format!("{o:?}").eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown colour order {s:?} (rgb, rbg, grb, gbr, brg, bgr)"))
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
    /// Seconds this scene stays on screen per turn. Default:
    /// `[playlist] default_duration_s`.
    #[serde(default)]
    pub duration_s: Option<f64>,
    /// Only show this scene while the expression is true, e.g.
    /// `"prom:house_power > 0"`. See `condition.rs` for the grammar.
    #[serde(default)]
    pub condition: Option<String>,
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
    Value(ValueConfig),
    Sparkline(SparklineConfig),
    Image(ImageConfig),
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
    /// Integer scale for the font, 1..=8: each font pixel becomes an N×N block.
    #[serde(default = "default_font_scale")]
    pub font_scale: u32,
    /// Resolved font (built-in or loaded), filled in by [`Config::load_fonts`].
    #[serde(skip)]
    pub face: Option<FontFace>,
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
    /// Integer scale for the font, 1..=8: each font pixel becomes an N×N block.
    #[serde(default = "default_font_scale")]
    pub font_scale: u32,
    /// Resolved font (built-in or loaded), filled in by [`Config::load_fonts`].
    #[serde(skip)]
    pub face: Option<FontFace>,
    #[serde(default = "default_color")]
    pub color: Color,
    /// Ignored while scrolling.
    #[serde(default)]
    pub align: Align,
    /// Scroll right-to-left at this many pixels per second. Unset = static.
    #[serde(default)]
    pub scroll_px_per_s: Option<f32>,
}

/// A number from the store, formatted, with optional icon and threshold
/// colours. Shows `--` when the value is missing, stale or not a number.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueConfig {
    /// Store key, e.g. `ha:sensor.outside_temperature`.
    pub key: String,
    /// One placeholder: `{}`, `{:.N}`, `{:>W.N}` or `{:<W.N}` (see `format.rs`).
    #[serde(default = "default_value_format")]
    pub format: String,
    /// Appended verbatim after the formatted number. The unit stored with
    /// the value is ignored. Default: none.
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default = "default_font")]
    pub font: String,
    /// Integer scale for the font, 1..=8: each font pixel becomes an N×N block.
    #[serde(default = "default_font_scale")]
    pub font_scale: u32,
    /// Resolved font (built-in or loaded), filled in by [`Config::load_fonts`].
    #[serde(skip)]
    pub face: Option<FontFace>,
    /// Colour below every threshold (and above, when `thresholds` is empty).
    #[serde(default = "default_color")]
    pub color: Color,
    /// Aligns icon and text together within the zone.
    #[serde(default)]
    pub align: Align,
    /// Built-in icon drawn left of the text in the text colour.
    #[serde(default)]
    pub icon: Option<String>,
    /// Colour bands: the value uses the colour of the highest `above` it is
    /// strictly greater than. Any order; `above` values must be unique.
    #[serde(default)]
    pub thresholds: Vec<ThresholdConfig>,
    /// Show the placeholder when the value is older than this. Default: never.
    #[serde(default)]
    pub stale_after_s: Option<f64>,
}

fn default_value_format() -> String {
    "{}".into()
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdConfig {
    pub above: f64,
    pub color: Color,
}

/// A line graph of a `Series` value, newest point on the right.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SparklineConfig {
    pub key: String,
    #[serde(default = "default_color")]
    pub color: Color,
    /// `true` fills under the line with a dim version of `color`; a colour
    /// fills with that colour. Default: no fill.
    #[serde(default)]
    pub fill: Option<Fill>,
    /// Fixed lower end of the scale. Default: from the data.
    #[serde(default)]
    pub min: Option<f64>,
    /// Fixed upper end of the scale. Default: from the data.
    #[serde(default)]
    pub max: Option<f64>,
    /// How many of the most recent points to show (at least 2). Default: the
    /// zone width, i.e. one point per pixel column.
    #[serde(default)]
    pub points: Option<u32>,
}

/// `fill = true`, `fill = false` or `fill = "#rrggbb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Fill {
    Enabled(bool),
    Color(Color),
}

/// A still image or GIF, from a file or from the store.
///
/// Exactly one of `path` and `key` must be set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    /// PNG or GIF file, relative to the config file's directory. Loaded and
    /// decoded once, while the config is loaded.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Store key holding a `Value::Image` (filled by sources, e.g. `media:…`).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub fit: Fit,
    /// Where the image sits in the zone when it does not fill it.
    #[serde(default)]
    pub align: ImageAlign,
    /// Average source pixels when shrinking instead of picking one. Better
    /// for photos; leave off for pixel art.
    #[serde(default)]
    pub smooth: bool,
    /// Animated images: restart after the last frame (default) or stop on it.
    #[serde(default = "default_true", rename = "loop")]
    pub looping: bool,
    /// Decoded `path` image, filled in by [`Config::load_media`].
    #[serde(skip)]
    pub loaded: Option<Arc<Media>>,
}

fn default_true() -> bool {
    true
}

/// How an image is sized to its zone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Scale (up or down, keeping the aspect ratio) until it fits entirely.
    #[default]
    Contain,
    /// Scale (keeping the aspect ratio) until it covers the zone; crop the rest.
    Cover,
    /// Scale each axis independently to exactly fill the zone.
    Stretch,
    /// Native size, cropped if larger than the zone.
    None,
}

/// Placement of an image inside its zone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAlign {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ImageAlign {
    /// Horizontal and vertical placement as 0 (start), 1 (middle) or 2 (end).
    pub const fn factors(self) -> (i64, i64) {
        match self {
            ImageAlign::TopLeft => (0, 0),
            ImageAlign::Top => (1, 0),
            ImageAlign::TopRight => (2, 0),
            ImageAlign::Left => (0, 1),
            ImageAlign::Center => (1, 1),
            ImageAlign::Right => (2, 1),
            ImageAlign::BottomLeft => (0, 2),
            ImageAlign::Bottom => (1, 2),
            ImageAlign::BottomRight => (2, 2),
        }
    }
}

fn default_font_scale() -> u32 {
    1
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
        let base = path.parent().unwrap_or(Path::new(""));
        Config::parse_in(&text, base).with_context(|| format!("loading config {}", path.display()))
    }

    /// Parse and validate config text. Relative media paths resolve against
    /// the working directory; use [`Config::load`] or [`Config::parse_in`] to
    /// anchor them elsewhere.
    pub fn parse(text: &str) -> anyhow::Result<Config> {
        Config::parse_in(text, Path::new(""))
    }

    /// Parse and validate config text, resolving relative media paths against
    /// `base_dir`. Image files are read and decoded here (blocking), so that
    /// the render loop never does file I/O; problems are reported as
    /// validation errors naming the file.
    pub fn parse_in(text: &str, base_dir: &Path) -> anyhow::Result<Config> {
        let mut cfg: Config = toml::from_str(text)?;
        cfg.base_dir = base_dir.to_path_buf();
        let mut errs = cfg.validate().err().map(|e| e.0).unwrap_or_default();
        errs.extend(cfg.load_fonts());
        errs.extend(cfg.load_media());
        if errs.is_empty() {
            Ok(cfg)
        } else {
            Err(ConfigErrors(errs).into())
        }
    }

    /// Read and parse every `[[font]]` file into its `loaded` field, then
    /// resolve each text, clock and value widget's font name to a
    /// [`FontFace`]. Returns one message per failure. Blocking I/O:
    /// config-load time only. A pure function of the config and the files it
    /// names, so a reload can simply call it again.
    pub fn load_fonts(&mut self) -> Vec<String> {
        let mut errs = Vec::new();
        for f in &mut self.fonts {
            let full = self.base_dir.join(&f.path);
            let result = std::fs::read(&full)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    BdfFont::parse(&String::from_utf8_lossy(&bytes)).map_err(|e| e.to_string())
                });
            match result {
                Ok(font) => f.loaded = Some(Arc::new(font)),
                Err(e) => errs.push(format!(
                    "font {:?}: cannot load {}: {e}",
                    f.name,
                    full.display()
                )),
            }
        }
        let loaded: HashMap<&str, &Arc<BdfFont>> = self
            .fonts
            .iter()
            .filter_map(|f| f.loaded.as_ref().map(|b| (f.name.as_str(), b)))
            .collect();
        let resolve = |name: &str| {
            loaded
                .get(name)
                .map(|b| FontFace::Bdf(Arc::clone(b)))
                .or_else(|| FontFace::builtin(name))
        };
        for s in &mut self.scenes {
            for z in &mut s.zones {
                match &mut z.widget {
                    WidgetConfig::Clock(c) => c.face = resolve(&c.font),
                    WidgetConfig::Text(t) => t.face = resolve(&t.font),
                    WidgetConfig::Value(v) => v.face = resolve(&v.font),
                    _ => {}
                }
            }
        }
        errs
    }

    /// Read and decode every `image` widget's `path` file into its `loaded`
    /// field (a file used in several zones is decoded once). Returns one
    /// message per failure. Blocking I/O: config-load time only.
    pub fn load_media(&mut self) -> Vec<String> {
        let mut errs = Vec::new();
        let mut cache: HashMap<PathBuf, Result<Arc<Media>, String>> = HashMap::new();
        for s in &mut self.scenes {
            for (i, z) in s.zones.iter_mut().enumerate() {
                let WidgetConfig::Image(img) = &mut z.widget else {
                    continue;
                };
                let Some(rel) = &img.path else { continue };
                let full = self.base_dir.join(rel);
                let result = cache.entry(full.clone()).or_insert_with(|| {
                    Media::load(&full)
                        .map(Arc::new)
                        .map_err(|e| format!("{e:#}"))
                });
                match result {
                    Ok(m) => img.loaded = Some(m.clone()),
                    Err(e) => errs.push(format!(
                        "scene {:?} zone[{i}] image: cannot load {}: {e}",
                        s.name,
                        full.display()
                    )),
                }
            }
        }
        errs
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

        if !valid_duration(self.playlist.default_duration_s) {
            errs.push(format!(
                "playlist.default_duration_s: must be > 0 and at most {MAX_DURATION_S} (got {})",
                self.playlist.default_duration_s
            ));
        }

        let mut font_names: Vec<&str> = Vec::new();
        for (i, f) in self.fonts.iter().enumerate() {
            if f.name.trim().is_empty() {
                errs.push(format!("font[{i}]: name must not be empty"));
            } else if fonts::lookup(&f.name).is_some() {
                errs.push(format!(
                    "font {:?}: name is already a built-in font",
                    f.name
                ));
            } else if font_names.contains(&f.name.as_str()) {
                errs.push(format!("font {:?}: duplicate font name", f.name));
            } else {
                font_names.push(&f.name);
            }
            if f.path.as_os_str().is_empty() {
                errs.push(format!("font {:?}: path must not be empty", f.name));
            }
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
            if let Some(d) = s.duration_s {
                if !valid_duration(d) {
                    errs.push(format!(
                        "scene {:?}: duration_s must be > 0 and at most {MAX_DURATION_S} (got {d})",
                        s.name
                    ));
                }
            }
            if let Some(cond) = &s.condition {
                if let Err(e) = Condition::parse(cond) {
                    errs.push(format!(
                        "scene {:?}: invalid condition {cond:?}: {e}",
                        s.name
                    ));
                }
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
                validate_widget(&z.widget, &at, &font_names, &mut errs);
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(errs))
        }
    }
}

fn validate_value(v: &ValueConfig, at: &str, errs: &mut Vec<String>) {
    if v.key.trim().is_empty() {
        errs.push(format!("{at} value: key must not be empty"));
    }
    if let Err(e) = NumberFormat::parse(&v.format) {
        errs.push(format!("{at} value: {e}"));
    }
    if let Some(icon) = &v.icon {
        if icons::lookup(icon).is_none() {
            errs.push(format!(
                "{at} value: unknown icon {icon:?} (known: {})",
                icons::known_names()
            ));
        }
    }
    let mut seen: Vec<f64> = Vec::new();
    for (i, t) in v.thresholds.iter().enumerate() {
        if !t.above.is_finite() {
            errs.push(format!(
                "{at} value: thresholds[{i}].above must be a finite number (got {})",
                t.above
            ));
        } else if seen.contains(&t.above) {
            errs.push(format!(
                "{at} value: thresholds[{i}].above = {} is a duplicate",
                t.above
            ));
        } else {
            seen.push(t.above);
        }
    }
    if let Some(s) = v.stale_after_s {
        if !(s.is_finite() && s > 0.0) {
            errs.push(format!("{at} value: stale_after_s must be > 0 (got {s})"));
        }
    }
}

fn validate_sparkline(s: &SparklineConfig, at: &str, errs: &mut Vec<String>) {
    if s.key.trim().is_empty() {
        errs.push(format!("{at} sparkline: key must not be empty"));
    }
    for (name, v) in [("min", s.min), ("max", s.max)] {
        if let Some(v) = v.filter(|v| !v.is_finite()) {
            errs.push(format!(
                "{at} sparkline: {name} must be a finite number (got {v})"
            ));
        }
    }
    if let (Some(lo), Some(hi)) = (s.min, s.max) {
        if lo.is_finite() && hi.is_finite() && lo >= hi {
            errs.push(format!(
                "{at} sparkline: min must be less than max (got min = {lo}, max = {hi})"
            ));
        }
    }
    if let Some(p) = s.points {
        if p < 2 {
            errs.push(format!(
                "{at} sparkline: points must be at least 2 (got {p})"
            ));
        }
    }
}

fn validate_widget(w: &WidgetConfig, at: &str, font_names: &[&str], errs: &mut Vec<String>) {
    let (font, scale) = match w {
        WidgetConfig::Sparkline(s) => {
            validate_sparkline(s, at, errs);
            return;
        }
        WidgetConfig::Value(v) => {
            validate_value(v, at, errs);
            (&v.font, v.font_scale)
        }
        WidgetConfig::Image(img) => {
            match (&img.path, &img.key) {
                (Some(_), Some(_)) => {
                    errs.push(format!("{at} image: set either path or key, not both"));
                }
                (None, None) => errs.push(format!("{at} image: one of path or key is required")),
                (Some(p), None) if p.as_os_str().is_empty() => {
                    errs.push(format!("{at} image: path must not be empty"));
                }
                (None, Some(k)) if k.trim().is_empty() => {
                    errs.push(format!("{at} image: key must not be empty"));
                }
                _ => {}
            }
            return;
        }
        WidgetConfig::Clock(c) => {
            if c.format.is_empty() {
                errs.push(format!("{at} clock: format must not be empty"));
            } else if StrftimeItems::new(&c.format).any(|i| matches!(i, Item::Error)) {
                errs.push(format!(
                    "{at} clock: invalid strftime format {:?}",
                    c.format
                ));
            }
            (&c.font, c.font_scale)
        }
        WidgetConfig::Text(t) => {
            if let Some(v) = t.scroll_px_per_s {
                if !(v.is_finite() && v > 0.0) {
                    errs.push(format!("{at} text: scroll_px_per_s must be > 0 (got {v})"));
                }
            }
            (&t.font, t.font_scale)
        }
    };
    if fonts::lookup(font).is_none() && !font_names.contains(&font.as_str()) {
        errs.push(format!(
            "{at}: unknown font {font:?} (known: {})",
            fonts::known_names(font_names)
        ));
    }
    if !(1..=fonts::MAX_FONT_SCALE).contains(&scale) {
        errs.push(format!(
            "{at}: font_scale must be 1..={} (got {scale})",
            fonts::MAX_FONT_SCALE
        ));
    }
}
