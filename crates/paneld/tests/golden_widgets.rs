//! Golden images for the `value` and `sparkline` widgets, against a fixed
//! store snapshot and clock. Same workflow as `golden.rs`:
//! `UPDATE_GOLDEN=1 cargo test -p paneld golden`, then look at the images.

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

fn entry(value: Value, age_s: u64) -> Entry {
    Entry {
        value,
        updated_at: SystemTime::from(now()) - Duration::from_secs(age_s),
        source: "test".into(),
    }
}

fn number(v: f64, age_s: u64) -> Entry {
    entry(
        Value::Number {
            value: v,
            unit: Some("stored".into()),
        },
        age_s,
    )
}

fn series(values: &[f64]) -> Entry {
    let n = values.len() as u64;
    let points: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            (
                SystemTime::from(now()) - Duration::from_secs((n - i as u64) * 60),
                v,
            )
        })
        .collect();
    entry(Value::Series(points.into()), 0)
}

fn store() -> StoreSnapshot {
    let mut m: HashMap<String, Entry> = HashMap::new();
    for (key, v) in [
        ("t15", 15.0),
        ("t22", 22.0),
        ("t25", 25.0), // exactly on a threshold: not above it
        ("t27", 27.0),
        ("t35", 35.0),
        ("t_neg", -0.04),
        ("n", 42.0),
        ("watts", 1234.4),
        ("small", 7.0),
    ] {
        m.insert(key.into(), number(v, 10));
    }
    // 10 minutes old; the widgets below go stale after 5.
    m.insert("t_stale".into(), number(30.0, 600));
    m.insert("t_text".into(), entry(Value::Text("warm".into()), 10));
    m.insert("t_inf".into(), number(f64::INFINITY, 10));

    // Steps and ramps, built from integers so the data is exact everywhere.
    let wave: Vec<f64> = (0..172)
        .map(|i: i32| f64::from((i % 40 - 20).abs() * 3 / 2 + (i / 43) * 10))
        .collect();
    m.insert("wave".into(), series(&wave));
    m.insert("flat".into(), series(&[5.0; 40]));
    m.insert("few".into(), series(&[10.0, 30.0, 20.0]));
    m.insert("one".into(), series(&[10.0]));
    m.insert("empty".into(), series(&[]));
    Arc::new(m)
}

/// One `value` zone, 21px tall, at row `i`.
fn value_zone(i: u32, key: &str) -> String {
    format!(
        r##"
  [[scene.zone]]
  rect = {{ x = 0, y = {y}, w = 172, h = 21 }}
  background = "{bg}"
  widget = {{ type = "value", key = "{key}", format = "{{:.1}}°", icon = "thermometer", color = "#40a0ff", stale_after_s = 300, thresholds = [
    {{ above = 30.0, color = "#ff2000" }}, {{ above = 20.0, color = "#00ff80" }}, {{ above = 25.0, color = "#ffaa00" }} ] }}"##,
        y = i * 21,
        bg = if i % 2 == 0 { "#101010" } else { "#000000" },
    )
}

fn config() -> String {
    let mut c = String::from(
        r##"
[canvas]
width = 172
height = 172

[[scene]]
name = "value_thresholds"
"##,
    );
    for (i, key) in [
        "t15", "t22", "t25", "t27", "t35", "t_stale", "missing", "t_text",
    ]
    .iter()
    .enumerate()
    {
        c += &value_zone(i as u32, key);
    }

    c += "\n\n[[scene]]\nname = \"value_icons\"";
    for (i, (icon, unit)) in [
        ("thermometer", "°C"),
        ("droplet", "%"),
        ("bolt", " W"),
        ("sun", " lx"),
        ("cloud", "%"),
        ("rain", " mm"),
        ("home", ""),
        ("wind", " m/s"),
    ]
    .iter()
    .enumerate()
    {
        c += &format!(
            r##"
  [[scene.zone]]
  rect = {{ x = 0, y = {y}, w = 172, h = 21 }}
  widget = {{ type = "value", key = "n", format = "{{:.0}}", unit = "{unit}", icon = "{icon}", align = "left", font = "9x15" }}"##,
            y = i * 21
        );
    }

    c += r##"

[[scene]]
name = "value_layout"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "watts", format = "{:.0}", unit = " W", icon = "bolt", color = "#ffd000", align = "right" }
  [[scene.zone]]
  rect = { x = 0, y = 21, w = 172, h = 21 }
  widget = { type = "value", key = "watts", format = "{:.0}", unit = " W", icon = "bolt", color = "#ffd000", align = "center" }
  [[scene.zone]]
  rect = { x = 0, y = 42, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "watts", format = "{:.0}", unit = " W", color = "#ffd000", align = "left", font = "10x20" }
  # Padded, so the number does not jump around as its width changes.
  [[scene.zone]]
  rect = { x = 0, y = 63, w = 172, h = 21 }
  widget = { type = "value", key = "small", format = "[{:>6.1}]", unit = "kW", color = "#80c0ff", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 84, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "small", format = "[{:<6.1}]", unit = "kW", color = "#80c0ff", align = "left" }
  # A value that rounds to negative zero prints without the sign.
  [[scene.zone]]
  rect = { x = 0, y = 105, w = 172, h = 21 }
  widget = { type = "value", key = "t_neg", format = "{:.1}°", color = "#ffffff" }
  # Not finite: placeholder.
  [[scene.zone]]
  rect = { x = 0, y = 126, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "t_inf", icon = "home" }
  # Too wide for the zone: left edge kept, right side clipped.
  [[scene.zone]]
  rect = { x = 0, y = 147, w = 40, h = 21 }
  widget = { type = "value", key = "watts", format = "{:.3}", icon = "bolt", align = "center", color = "#ff80ff" }

[[scene]]
name = "sparklines"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 24 }
  background = "#101010"
  widget = { type = "sparkline", key = "wave", color = "#00ff80" }
  [[scene.zone]]
  rect = { x = 0, y = 24, w = 172, h = 24 }
  widget = { type = "sparkline", key = "wave", color = "#ffaa00", min = 0, max = 200 }
  [[scene.zone]]
  rect = { x = 0, y = 48, w = 172, h = 24 }
  background = "#101010"
  widget = { type = "sparkline", key = "flat", color = "#ff4040", points = 40 }
  [[scene.zone]]
  rect = { x = 0, y = 72, w = 172, h = 24 }
  widget = { type = "sparkline", key = "few", color = "#ff80ff", points = 6 }
  [[scene.zone]]
  rect = { x = 0, y = 96, w = 172, h = 24 }
  background = "#101010"
  widget = { type = "sparkline", key = "wave", color = "#00ff80", fill = true }
  [[scene.zone]]
  rect = { x = 0, y = 120, w = 172, h = 24 }
  widget = { type = "sparkline", key = "wave", color = "#40a0ff", fill = "#003060", points = 60 }
  # Single point, then missing key and empty series: nothing but the one dot.
  [[scene.zone]]
  rect = { x = 0, y = 144, w = 60, h = 24 }
  background = "#101010"
  widget = { type = "sparkline", key = "one" }
  [[scene.zone]]
  rect = { x = 60, y = 144, w = 56, h = 24 }
  background = "#101010"
  widget = { type = "sparkline", key = "missing" }
  [[scene.zone]]
  rect = { x = 116, y = 144, w = 56, h = 24 }
  background = "#101010"
  widget = { type = "sparkline", key = "empty" }
"##;
    c
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden")
}

fn check(name: &str, scene: &str) {
    let cfg = Config::parse(&config()).unwrap();
    let ctx = FrameCtx {
        now: now(),
        scene_elapsed: Duration::ZERO,
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
fn golden_value_thresholds() {
    check("value_thresholds", "value_thresholds");
}

#[test]
fn golden_value_icons() {
    check("value_icons", "value_icons");
}

#[test]
fn golden_value_layout() {
    check("value_layout", "value_layout");
}

#[test]
fn golden_sparklines() {
    check("sparklines", "sparklines");
}
