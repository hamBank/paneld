//! Golden images for templated `text` zones and the `value` widget's
//! condition icons, against a fixed store snapshot and clock. Same workflow
//! as `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld golden`, then look
//! at the images.

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

/// Every condition Home Assistant defines for `weather.*` entities.
const CONDITIONS: [&str; 15] = [
    "clear-night",
    "cloudy",
    "exceptional",
    "fog",
    "hail",
    "lightning",
    "lightning-rainy",
    "partlycloudy",
    "pouring",
    "rainy",
    "snowy",
    "snowy-rainy",
    "sunny",
    "windy",
    "windy-variant",
];

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

fn number(v: f64) -> Entry {
    entry(
        Value::Number {
            value: v,
            unit: Some("stored".into()),
        },
        10,
    )
}

fn text(s: &str) -> Entry {
    entry(Value::Text(s.into()), 10)
}

fn store() -> StoreSnapshot {
    let mut m: HashMap<String, Entry> = HashMap::new();
    m.insert("ha:sensor.outdoor_temperature".into(), number(21.54));
    m.insert("ha:sensor.cold".into(), number(-0.04));
    m.insert("ha:sensor.humidity".into(), number(63.4));
    m.insert("ha:sensor.power".into(), number(1234.4));
    m.insert("ha:sensor.inf".into(), number(f64::INFINITY));
    m.insert("aircraft:nearest.callsign".into(), text("DLH4YA"));
    m.insert("aircraft:nearest.route".into(), text("FRA-LHR"));
    m.insert("aircraft:nearest.altitude".into(), number(35000.0));
    m.insert("ha:binary_sensor.door".into(), entry(Value::Bool(true), 10));
    m.insert(
        "ha:binary_sensor.window".into(),
        entry(Value::Bool(false), 10),
    );
    m.insert(
        "ha:sensor.series".into(),
        entry(
            Value::Series(vec![(SystemTime::from(now()), 1.0)].into()),
            10,
        ),
    );
    m.insert("ha:weather.home".into(), text("partlycloudy"));
    m.insert("ha:weather.unknown".into(), text("tornado"));
    m.insert("ha:weather.number".into(), number(7.0));
    // 10 minutes old; the widgets below go stale after 5.
    m.insert(
        "ha:weather.stale".into(),
        entry(Value::Text("sunny".into()), 600),
    );
    for (i, c) in CONDITIONS.iter().enumerate() {
        m.insert(format!("ha:weather.c{i}"), text(c));
    }
    Arc::new(m)
}

fn text_zone(i: u32, extra: &str, t: &str) -> String {
    format!(
        r##"
  [[scene.zone]]
  rect = {{ x = 0, y = {y}, w = 172, h = 21 }}
  background = "{bg}"
  widget = {{ type = "text", text = "{t}", font = "9x15"{extra} }}"##,
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
name = "template_text"
"##,
    );
    for (i, (extra, t)) in [
        ("", "{ha:sensor.outdoor_temperature:.1}°C"),
        (
            ", color = \"#40a0ff\"",
            "Out {ha:sensor.outdoor_temperature:.0}° {ha:sensor.humidity:.0}%",
        ),
        (
            ", color = \"#ffd000\"",
            "{aircraft:nearest.callsign:<8}{aircraft:nearest.altitude:>6.0}ft",
        ),
        (
            "",
            "Door {ha:binary_sensor.door} win {ha:binary_sensor.window}",
        ),
        // Missing, non-finite and unsupported values are all "--", and the
        // width still pads them.
        (
            ", color = \"#ff8080\"",
            "[{ha:sensor.nope:.1}] [{ha:sensor.inf:>5.1}] [{ha:sensor.series:<4}]",
        ),
        ("", "{{{ha:sensor.humidity:.0}}} }}ok{{ {ha:sensor.cold:.1}"),
        (
            ", align = \"center\", color = \"#80ff80\"",
            "{ha:sensor.power:.0} W",
        ),
        (
            ", align = \"right\", color = \"#80ff80\"",
            "{aircraft:nearest.route}",
        ),
    ]
    .iter()
    .enumerate()
    {
        c += &text_zone(i as u32, extra, t);
    }

    // Scrolling: the text width changes with the value, and the scroll
    // cycle follows it. Rendered at two scene times below.
    c += r##"

[[scene]]
name = "template_scroll"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 21 }
  background = "#101030"
  widget = { type = "text", text = "{aircraft:nearest.callsign} {aircraft:nearest.route} at {aircraft:nearest.altitude:.0} ft", font = "9x15", scroll_px_per_s = 40, color = "#ffd000" }
  [[scene.zone]]
  rect = { x = 0, y = 21, w = 172, h = 21 }
  widget = { type = "text", text = "{ha:sensor.power:.0}", font = "9x15", scroll_px_per_s = 40, color = "#80c0ff" }
  [[scene.zone]]
  rect = { x = 0, y = 42, w = 172, h = 21 }
  background = "#101030"
  widget = { type = "text", text = "{ha:sensor.nope} missing then {ha:sensor.humidity:.1}%", font = "9x15", scroll_px_per_s = 40 }
"##;

    // Condition icons.
    c += "\n[[scene]]\nname = \"condition_icons\"";
    for (i, key) in [
        "ha:weather.c12",
        "ha:weather.c0",
        "ha:weather.home",
        "ha:weather.c9",
        "ha:weather.c10",
        "ha:weather.c3",
        "ha:weather.c13",
        "ha:weather.unknown",
    ]
    .iter()
    .enumerate()
    {
        c += &format!(
            r##"
  [[scene.zone]]
  rect = {{ x = 0, y = {y}, w = 172, h = 21 }}
  background = "{bg}"
  widget = {{ type = "value", key = "{key}", condition_icons = {{}}, font = "9x15", color = "#ffffff", align = "left" }}"##,
            y = i * 21,
            bg = if i % 2 == 0 { "#101010" } else { "#000000" },
        );
    }

    // Icon only, every Home Assistant condition: 4 x 4 grid.
    c += "\n\n[[scene]]\nname = \"condition_icons_only\"";
    for i in 0..16u32 {
        let key = if i < 15 {
            format!("ha:weather.c{i}")
        } else {
            "ha:weather.unknown".into()
        };
        c += &format!(
            r##"
  [[scene.zone]]
  rect = {{ x = {x}, y = {y}, w = 43, h = 43 }}
  background = "{bg}"
  widget = {{ type = "value", key = "{key}", condition_icons = {{}}, show_text = false, color = "#ffd000" }}"##,
            x = (i % 4) * 43,
            y = (i / 4) * 43,
            bg = if (i / 4 + i % 4) % 2 == 0 {
                "#101010"
            } else {
                "#000000"
            },
        );
    }

    // Overrides, fallbacks, and the placeholder cases.
    c += r##"

[[scene]]
name = "condition_icons_custom"
  # Override: rainy -> droplet. Unlisted conditions use the built-in map.
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "ha:weather.c9", condition_icons = { rainy = "droplet" }, font = "9x15", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 21, w = 172, h = 21 }
  widget = { type = "value", key = "ha:weather.home", condition_icons = { partlycloudy = "sun" }, font = "9x15", align = "left" }
  # Unknown text: the fallback `icon`, and the text is shown even with show_text = false.
  [[scene.zone]]
  rect = { x = 0, y = 42, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "ha:weather.unknown", condition_icons = {}, icon = "home", show_text = false, font = "9x15", align = "left" }
  # A custom text of our own.
  [[scene.zone]]
  rect = { x = 0, y = 63, w = 172, h = 21 }
  widget = { type = "value", key = "aircraft:nearest.callsign", condition_icons = { DLH4YA = "wind" }, font = "9x15", align = "left", color = "#80c0ff" }
  # Missing key, stale value and a number: placeholder / number.
  [[scene.zone]]
  rect = { x = 0, y = 84, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "ha:weather.nope", condition_icons = {}, icon = "cloud", font = "9x15", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 105, w = 172, h = 21 }
  widget = { type = "value", key = "ha:weather.stale", condition_icons = {}, stale_after_s = 300, font = "9x15", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 126, w = 172, h = 21 }
  background = "#101010"
  widget = { type = "value", key = "ha:weather.number", condition_icons = {}, format = "{:.1}", unit = " m/s", icon = "wind", font = "9x15", align = "left" }
  # Alignment of icon + text together.
  [[scene.zone]]
  rect = { x = 0, y = 147, w = 172, h = 21 }
  widget = { type = "value", key = "ha:weather.c12", condition_icons = {}, font = "9x15", align = "right", color = "#ffaa00" }
"##;
    c
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden")
}

fn check(name: &str, scene: &str, elapsed: Duration) {
    let cfg = Config::parse(&config()).unwrap();
    let ctx = FrameCtx {
        now: now(),
        scene_elapsed: elapsed,
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
fn golden_template_text() {
    check("template_text", "template_text", Duration::ZERO);
}

#[test]
fn golden_template_scroll() {
    // 3 s at 40 px/s: 120 px in, so the text is mid-zone.
    check(
        "template_scroll_3s",
        "template_scroll",
        Duration::from_secs(3),
    );
    // 6 s: 240 px. The short value's cycle is only 172 + its width, so it
    // has already wrapped and re-entered; the long text is on its way out.
    check(
        "template_scroll_6s",
        "template_scroll",
        Duration::from_secs(6),
    );
}

#[test]
fn golden_condition_icons() {
    check("condition_icons", "condition_icons", Duration::ZERO);
}

#[test]
fn golden_condition_icons_only() {
    check(
        "condition_icons_only",
        "condition_icons_only",
        Duration::ZERO,
    );
}

#[test]
fn golden_condition_icons_custom() {
    check(
        "condition_icons_custom",
        "condition_icons_custom",
        Duration::ZERO,
    );
}
