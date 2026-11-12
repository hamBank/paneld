//! Golden images for BDF fonts and `font_scale` (DESIGN.md §5.4, §12). Same
//! workflow as `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld golden`,
//! then look at the PNGs in `tests/fixtures/golden/font_*.png` before
//! committing.
//!
//! `prop.bdf` (tests/fixtures/fonts, made by `make_prop_font.py`) is a tiny
//! proportional font; Spleen comes from `assets/fonts/`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset};
use paneld::config::Config;
use paneld::png_io;
use paneld::render::render_scene;
use paneld::store::{Entry, StoreSnapshot, Value};
use paneld::widgets::FrameCtx;

fn now() -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap()
}

fn store() -> StoreSnapshot {
    let mut m: HashMap<String, Entry> = HashMap::new();
    m.insert(
        "t".into(),
        Entry {
            value: Value::Number {
                value: 21.5,
                unit: None,
            },
            updated_at: SystemTime::from(now()),
            source: "test".into(),
        },
    );
    Arc::new(m)
}

const CONFIG: &str = r##"
[canvas]
width = 172
height = 172

[[font]]
name = "prop"
path = "prop.bdf"
[[font]]
name = "spleen12"
path = "../../../assets/fonts/spleen-12x24.bdf"
[[font]]
name = "spleen16"
path = "../../../assets/fonts/spleen-16x32.bdf"
[[font]]
name = "spleen32"
path = "../../../assets/fonts/spleen-32x64.bdf"

# A BDF font in the clock, text and value widgets.
[[scene]]
name = "bdf_widgets"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 64 }
  background = "#101010"
  widget = { type = "clock", format = "%H:%M", font = "spleen32", color = "#ffaa00" }
  [[scene.zone]]
  rect = { x = 0, y = 64, w = 172, h = 32 }
  widget = { type = "clock", format = "%H:%M:%S", font = "spleen16", color = "#80c0ff", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 96, w = 172, h = 24 }
  background = "#101030"
  widget = { type = "text", text = "Spleen 12x24", font = "spleen12", color = "#ffffff", align = "right" }
  [[scene.zone]]
  rect = { x = 0, y = 120, w = 172, h = 26 }
  widget = { type = "value", key = "t", format = "{:.1}", unit = "°C", icon = "thermometer", font = "spleen12", color = "#00ff80" }
  [[scene.zone]]
  rect = { x = 0, y = 146, w = 172, h = 26 }
  background = "#101010"
  widget = { type = "value", key = "missing", font = "spleen12", align = "left" }

# font_scale on built-in and BDF fonts, at several scales.
[[scene]]
name = "font_scale"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 86, h = 12 }
  widget = { type = "text", text = "6x10 x1", font = "6x10", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 12, w = 86, h = 20 }
  background = "#101010"
  widget = { type = "text", text = "6x10 x2", font = "6x10", font_scale = 2, align = "left", color = "#ffaa00" }
  [[scene.zone]]
  rect = { x = 0, y = 32, w = 86, h = 30 }
  widget = { type = "text", text = "x3", font = "6x10", font_scale = 3, align = "left", color = "#80c0ff" }
  [[scene.zone]]
  rect = { x = 86, y = 0, w = 86, h = 62 }
  background = "#101030"
  widget = { type = "clock", format = "%H:%M", font = "5x8", font_scale = 3, color = "#00ff80" }
  [[scene.zone]]
  rect = { x = 0, y = 62, w = 172, h = 16 }
  widget = { type = "text", text = "Hill 1.0", font = "prop", color = "#ffffff", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 78, w = 172, h = 16 }
  background = "#101010"
  widget = { type = "text", text = "Hill 1.0 2", font = "prop", font_scale = 2, color = "#ffaa00", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 94, w = 172, h = 24 }
  widget = { type = "text", text = "Hill 1.0 3", font = "prop", font_scale = 3, color = "#80c0ff", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 118, w = 172, h = 54 }
  background = "#101010"
  widget = { type = "value", key = "t", format = "{:.1}", unit = "°C", font = "spleen12", font_scale = 2, color = "#ff80ff" }

# Proportional font: each line's alignment is by measured width, and a
# character the font lacks (Z) shows as the font's '?'.
[[scene]]
name = "proportional_align"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 18 }
  background = "#200000"
  widget = { type = "text", text = "Hill 1.0 Wm", font = "prop", font_scale = 2, align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 18, w = 172, h = 18 }
  background = "#002000"
  widget = { type = "text", text = "Hill 1.0 Wm", font = "prop", font_scale = 2, align = "center" }
  [[scene.zone]]
  rect = { x = 0, y = 36, w = 172, h = 18 }
  background = "#000020"
  widget = { type = "text", text = "Hill 1.0 Wm", font = "prop", font_scale = 2, align = "right" }
  [[scene.zone]]
  rect = { x = 0, y = 54, w = 172, h = 18 }
  background = "#101010"
  widget = { type = "text", text = "ZiZ Z?", font = "prop", font_scale = 2, align = "center" }
  [[scene.zone]]
  rect = { x = 0, y = 72, w = 172, h = 24 }
  background = "#200020"
  widget = { type = "value", key = "t", format = "{:.1}", unit = "°C", icon = "thermometer", font = "prop", font_scale = 3, align = "right" }
  [[scene.zone]]
  rect = { x = 0, y = 96, w = 172, h = 24 }
  widget = { type = "value", key = "t", format = "{:.1}", unit = "°C", icon = "thermometer", font = "prop", font_scale = 3, align = "center" }
  [[scene.zone]]
  rect = { x = 0, y = 120, w = 172, h = 24 }
  background = "#200020"
  widget = { type = "value", key = "t", format = "{:.1}", unit = "°C", font = "prop", font_scale = 3, align = "left" }

[[scene]]
name = "proportional_scroll"
  [[scene.zone]]
  rect = { x = 10, y = 10, w = 120, h = 20 }
  background = "#202020"
  widget = { type = "text", text = "Hill 1.0 Wm 03", font = "prop", font_scale = 2, scroll_px_per_s = 30 }
  [[scene.zone]]
  rect = { x = 10, y = 40, w = 120, h = 20 }
  background = "#202020"
  widget = { type = "text", text = "m ll m", font = "spleen12", scroll_px_per_s = 30 }
"##;

fn base_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fonts")
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden")
}

fn check(name: &str, scene: &str, elapsed_s: f64) {
    let cfg = Config::parse_in(CONFIG, &base_dir()).unwrap();
    let ctx = FrameCtx {
        now: now(),
        scene_elapsed: Duration::from_secs_f64(elapsed_s),
        frame: 0,
    };
    let actual = render_scene(&cfg, scene, &ctx, &store()).unwrap();

    let path = golden_dir().join(format!("{name}.png"));
    let actual_png = png_io::encode(&actual, 1, false);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, &actual_png).unwrap();
        return;
    }
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (run with UPDATE_GOLDEN=1 to create)",
            path.display()
        )
    });
    let golden = png_io::decode(&golden).unwrap();
    if golden != actual {
        let actual_path = path.with_extension("actual.png");
        std::fs::write(&actual_path, &actual_png).unwrap();
        let differing = golden
            .pixels()
            .iter()
            .zip(actual.pixels())
            .filter(|(a, b)| a != b)
            .count();
        panic!(
            "{name}: {differing} pixel(s) differ from {}; actual written to {}",
            path.display(),
            actual_path.display()
        );
    }
}

#[test]
fn golden_font_bdf_widgets() {
    check("font_bdf_widgets", "bdf_widgets", 0.0);
}

#[test]
fn golden_font_scale() {
    check("font_scale", "font_scale", 0.0);
}

#[test]
fn golden_font_proportional_align() {
    check("font_proportional_align", "proportional_align", 0.0);
}

#[test]
fn golden_font_proportional_scroll_entering() {
    // 2s at 30 px/s: 60 px into the cycle, the text is part-way in from the right.
    check(
        "font_proportional_scroll_entering",
        "proportional_scroll",
        2.0,
    );
}

#[test]
fn golden_font_proportional_scroll_midway() {
    check(
        "font_proportional_scroll_midway",
        "proportional_scroll",
        5.0,
    );
}
