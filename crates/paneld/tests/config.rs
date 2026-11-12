//! Config parsing and validation, including the error messages users see.

use std::path::Path;

use paneld::config::{Align, BrightnessMode, ColorOrder, Config, Fill, OutputConfig, WidgetConfig};

const MINIMAL: &str = r#"
[canvas]
width = 64
height = 32
[[scene]]
name = "a"
"#;

fn err(text: &str) -> String {
    format!(
        "{:#}",
        Config::parse(text).expect_err("config should be rejected")
    )
}

#[test]
fn example_config_is_valid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/example.toml");
    let cfg = Config::load(&path).unwrap();
    assert_eq!((cfg.canvas.width, cfg.canvas.height), (172, 172));
    assert!(cfg.preview().is_some());
    assert!(cfg.scene("home").is_some());
}

#[test]
fn defaults() {
    let cfg = Config::parse(&format!(
        "{MINIMAL}\n  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 8, h = 8 }}\n  widget = {{ type = \"clock\" }}\n"
    ))
    .unwrap();
    assert_eq!(cfg.canvas.fps, 30);
    assert_eq!(cfg.http.bind.to_string(), "127.0.0.1:8080");
    assert!(cfg.outputs.is_empty());
    let WidgetConfig::Clock(c) = &cfg.scenes[0].zones[0].widget else {
        panic!("expected clock");
    };
    assert_eq!(c.format, "%H:%M");
    assert_eq!(c.font, "6x10");
    assert_eq!(c.align, Align::Center);
    assert_eq!(c.color.to_string(), "#ffffff");
}

#[test]
fn preview_defaults() {
    let cfg = Config::parse(&format!("{MINIMAL}\n[[output]]\ntype = \"preview\"\n")).unwrap();
    let OutputConfig::Preview(p) = &cfg.outputs[0] else {
        panic!("expected preview");
    };
    assert_eq!(p.scale, 8);
    assert!(!p.grid);
}

#[test]
fn rejects_unknown_keys() {
    let e = err("[canvas]\nwidth = 64\nheight = 32\nfsp = 30\n[[scene]]\nname = \"a\"\n");
    assert!(e.contains("unknown field `fsp`"), "{e}");
}

#[test]
fn rejects_unknown_widget_type() {
    let e = err(&format!(
        "{MINIMAL}  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 8, h = 8 }}\n  widget = {{ type = \"video\" }}\n"
    ));
    assert!(e.contains("unknown variant `video`"), "{e}");
}

#[test]
fn rejects_bad_colour() {
    let e = err(&format!(
        "{MINIMAL}  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 8, h = 8 }}\n  widget = {{ type = \"text\", text = \"x\", color = \"red\" }}\n"
    ));
    assert!(e.contains(r#"invalid colour "red""#), "{e}");
}

#[test]
fn reports_all_validation_errors_at_once() {
    let e = err(r#"
[canvas]
width = 64
height = 32
fps = 120

[[output]]
type = "preview"
scale = 0

[[scene]]
name = "a"
  [[scene.zone]]
  rect = { x = 60, y = 0, w = 8, h = 8 }
  widget = { type = "clock", format = "%Q", font = "huge" }
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 0, h = 8 }
  widget = { type = "text", text = "x", scroll_px_per_s = -1.0 }

[[scene]]
name = "a"
"#);
    for want in [
        "canvas.fps: must be 1..=60 (got 120)",
        "output[0] (preview): scale must be 1..=32 (got 0)",
        r#"scene "a" zone[0]: rect { x = 60, y = 0, w = 8, h = 8 } does not fit the 64x32 canvas"#,
        r#"scene "a" zone[0] clock: invalid strftime format "%Q""#,
        r#"scene "a" zone[0]: unknown font "huge" (known: 4x6,"#,
        r#"scene "a" zone[1]: rect must have non-zero size"#,
        r#"scene "a" zone[1] text: scroll_px_per_s must be > 0 (got -1)"#,
        r#"scene "a": duplicate scene name"#,
    ] {
        assert!(e.contains(want), "missing {want:?} in:\n{e}");
    }
}

#[test]
fn requires_a_scene_and_a_canvas() {
    assert!(
        err("[canvas]\nwidth = 64\nheight = 32\n").contains("at least one [[scene]] is required")
    );
    assert!(err("[[scene]]\nname = \"a\"\n").contains("missing field `canvas`"));
    assert!(
        err("[canvas]\nwidth = 0\nheight = 32\n[[scene]]\nname = \"a\"\n")
            .contains("canvas: width and height must be > 0 (got 0x32)")
    );
}

#[test]
fn rejects_two_previews() {
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"preview\"\n[[output]]\ntype = \"preview\"\n"
    ));
    assert!(e.contains("at most one preview output"), "{e}");
}

fn colorlight(cfg: &Config, i: usize) -> &paneld::config::ColorlightOutput {
    match &cfg.outputs[i] {
        OutputConfig::Colorlight(c) => c,
        other => panic!("expected colorlight, got {other:?}"),
    }
}

#[test]
fn colorlight_defaults() {
    let cfg = Config::parse(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\n"
    ))
    .unwrap();
    let c = colorlight(&cfg, 0);
    assert_eq!(c.interface, "eth1");
    assert_eq!(c.gamma, 2.2);
    assert_eq!(c.brightness, 255);
    assert_eq!(c.white_point.to_string(), "#ffffff");
    assert_eq!(c.brightness_mode, BrightnessMode::Hardware);
    assert_eq!(c.firmware_major, None);
    assert_eq!(c.sync_gap_us, 500);
    assert_eq!(c.color_order, ColorOrder::Rgb);
    assert_eq!(c.card_brightness(), 255);
    assert!(cfg.preview().is_none());
}

#[test]
fn colorlight_all_fields() {
    let cfg = Config::parse(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ngamma = 1.8\nbrightness = 0\nwhite_point = \"#ffd8b0\"\nbrightness_mode = \"software\"\nfirmware_major = 11\nsync_gap_us = 20000\ncolor_order = \"bgr\"\n[[output]]\ntype = \"preview\"\n"
    ))
    .unwrap();
    let c = colorlight(&cfg, 0);
    assert_eq!(c.gamma, 1.8);
    assert_eq!(c.brightness, 0);
    assert_eq!(c.white_point.to_string(), "#ffd8b0");
    assert_eq!(c.brightness_mode, BrightnessMode::Software);
    assert_eq!(c.firmware_major, Some(11));
    assert_eq!(c.sync_gap_us, 20000);
    assert_eq!(c.color_order, ColorOrder::Bgr);
    assert_eq!(c.card_brightness(), 255);
    // preview() finds the preview output even when it is not first.
    assert!(cfg.preview().is_some());
}

#[test]
fn colorlight_color_orders() {
    for (name, order) in [
        ("rgb", ColorOrder::Rgb),
        ("rbg", ColorOrder::Rbg),
        ("grb", ColorOrder::Grb),
        ("gbr", ColorOrder::Gbr),
        ("brg", ColorOrder::Brg),
        ("bgr", ColorOrder::Bgr),
    ] {
        let cfg = Config::parse(&format!(
            "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ncolor_order = \"{name}\"\n"
        ))
        .unwrap();
        assert_eq!(colorlight(&cfg, 0).color_order, order, "{name}");
    }
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ncolor_order = \"rgbw\"\n"
    ));
    assert!(e.contains("unknown variant `rgbw`"), "{e}");
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ncolor_order = \"RGB\"\n"
    ));
    assert!(e.contains("unknown variant `RGB`"), "{e}");
}

#[test]
fn colorlight_requires_interface() {
    let e = err(&format!("{MINIMAL}[[output]]\ntype = \"colorlight\"\n"));
    assert!(e.contains("missing field `interface`"), "{e}");
}

#[test]
fn colorlight_rejects_unknown_keys_and_modes() {
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\nregion = 1\n"
    ));
    assert!(e.contains("unknown field `region`"), "{e}");
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\nbrightness_mode = \"auto\"\n"
    ));
    assert!(e.contains("unknown variant `auto`"), "{e}");
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\nbrightness = 256\n"
    ));
    assert!(e.contains("256"), "{e}");
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\nwhite_point = \"warm\"\n"
    ));
    assert!(e.contains(r#"invalid colour "warm""#), "{e}");
}

#[test]
fn color_order_parses_from_cli_strings() {
    assert_eq!("bgr".parse::<ColorOrder>(), Ok(ColorOrder::Bgr));
    assert_eq!("GRB".parse::<ColorOrder>(), Ok(ColorOrder::Grb));
    assert!("rgbw".parse::<ColorOrder>().is_err());
}

#[test]
fn colorlight_validation_errors() {
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"  \"\ngamma = 0.5\nsync_gap_us = 20001\n\
         [[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ngamma = 4.5\n\
         [[output]]\ntype = \"colorlight\"\ninterface = \"eth2\"\ngamma = nan\n"
    ));
    for want in [
        "output[0] (colorlight): interface must not be empty",
        "output[0] (colorlight): gamma must be 1..=4 (got 0.5)",
        "output[0] (colorlight): sync_gap_us must be 0..=20000 (got 20001)",
        "output[1] (colorlight): gamma must be 1..=4 (got 4.5)",
        "output[2] (colorlight): gamma must be 1..=4 (got NaN)",
    ] {
        assert!(e.contains(want), "missing {want:?} in:\n{e}");
    }
}

#[test]
fn colorlight_rejects_duplicate_interface() {
    let e = err(&format!(
        "{MINIMAL}[[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\n\
         [[output]]\ntype = \"colorlight\"\ninterface = \"eth2\"\n\
         [[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\n"
    ));
    assert!(
        e.contains(r#"output[2] (colorlight): interface "eth1" is already used by output[0]"#),
        "{e}"
    );
    assert!(!e.contains("output[1]"), "{e}");
}

#[test]
fn playlist_defaults_and_scene_fields_parse() {
    let cfg = Config::parse(&format!(
        "{MINIMAL}duration_s = 20\ncondition = \"prom:house_power > 0 && !ha:away\"\n[[scene]]\nname = \"b\"\n"
    ))
    .unwrap();
    assert_eq!(cfg.playlist.default_duration_s, 10.0);
    assert_eq!(cfg.scenes[0].duration_s, Some(20.0));
    assert!(cfg.scenes[0].condition.is_some());
    assert_eq!(cfg.scenes[1].duration_s, None);
    assert_eq!(cfg.scenes[1].condition, None);

    let cfg = Config::parse(&format!("[playlist]\ndefault_duration_s = 2.5\n{MINIMAL}")).unwrap();
    assert_eq!(cfg.playlist.default_duration_s, 2.5);
}

#[test]
fn rejects_bad_durations_and_conditions() {
    let e = err(&format!(
        "[playlist]\ndefault_duration_s = 0\n{MINIMAL}duration_s = -3\n\
         [[scene]]\nname = \"b\"\nduration_s = 1e300\n\
         [[scene]]\nname = \"c\"\ncondition = \"prom:x >\"\n\
         [[scene]]\nname = \"d\"\ncondition = \"temp > 1\"\n"
    ));
    for want in [
        "playlist.default_duration_s: must be > 0 and at most 86400 (got 0)",
        r#"scene "a": duration_s must be > 0 and at most 86400 (got -3)"#,
        r#"scene "b": duration_s must be > 0 and at most 86400"#,
        r#"scene "c": invalid condition "prom:x >": unexpected end of expression"#,
        r#"scene "d": invalid condition "temp > 1": `temp` is not a valid key"#,
    ] {
        assert!(e.contains(want), "missing {want:?} in:\n{e}");
    }
    assert!(err(&format!("[playlist]\nspeed = 1\n{MINIMAL}")).contains("unknown field"));
}

fn zone(widget: &str) -> String {
    format!(
        "{MINIMAL}  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 8, h = 8 }}\n  widget = {widget}\n"
    )
}

#[test]
fn value_defaults() {
    let cfg = Config::parse(&zone(r#"{ type = "value", key = "k" }"#)).unwrap();
    let WidgetConfig::Value(v) = &cfg.scenes[0].zones[0].widget else {
        panic!("expected value");
    };
    assert_eq!(v.key, "k");
    assert_eq!(v.format, "{}");
    assert_eq!(v.unit, None);
    assert_eq!(v.font, "6x10");
    assert_eq!(v.color.to_string(), "#ffffff");
    assert_eq!(v.align, Align::Center);
    assert_eq!(v.icon, None);
    assert!(v.thresholds.is_empty());
    assert_eq!(v.stale_after_s, None);
}

#[test]
fn value_all_fields() {
    let cfg = Config::parse(&zone(
        r##"{ type = "value", key = "k", format = "{:>5.1}", unit = " W", font = "5x8", color = "#112233", align = "right", icon = "bolt", stale_after_s = 90, thresholds = [{ above = 25.0, color = "#ff4000" }, { above = 10, color = "#00ff00" }] }"##,
    ))
    .unwrap();
    let WidgetConfig::Value(v) = &cfg.scenes[0].zones[0].widget else {
        panic!("expected value");
    };
    assert_eq!(v.format, "{:>5.1}");
    assert_eq!(v.unit.as_deref(), Some(" W"));
    assert_eq!(v.icon.as_deref(), Some("bolt"));
    assert_eq!(v.stale_after_s, Some(90.0));
    assert_eq!(v.thresholds.len(), 2);
    assert_eq!(v.thresholds[1].above, 10.0);
}

#[test]
fn value_validation_errors() {
    let e = err(&zone(
        r##"{ type = "value", key = " ", format = "{:x}", icon = "toaster", stale_after_s = 0, thresholds = [{ above = 1.0, color = "#ffffff" }, { above = 1.0, color = "#000000" }, { above = nan, color = "#000000" }] }"##,
    ));
    for want in [
        r#"scene "a" zone[0] value: key must not be empty"#,
        r#"scene "a" zone[0] value: format "{:x}": unsupported format spec "x""#,
        r#"scene "a" zone[0] value: unknown icon "toaster" (known: thermometer, droplet, bolt, sun, cloud, rain, home, wind)"#,
        r#"scene "a" zone[0] value: thresholds[1].above = 1 is a duplicate"#,
        r#"scene "a" zone[0] value: thresholds[2].above must be a finite number (got NaN)"#,
        r#"scene "a" zone[0] value: stale_after_s must be > 0 (got 0)"#,
    ] {
        assert!(e.contains(want), "missing {want:?} in:\n{e}");
    }
    let e = err(&zone(
        r#"{ type = "value", key = "k", format = "no placeholder" }"#,
    ));
    assert!(e.contains("needs a placeholder"), "{e}");
    let e = err(&zone(r#"{ type = "value", key = "k", format = "{} {}" }"#));
    assert!(e.contains("only one placeholder"), "{e}");
    let e = err(&zone(r#"{ type = "value", key = "k", font = "huge" }"#));
    assert!(e.contains(r#"unknown font "huge""#), "{e}");
    let e = err(&zone(
        r#"{ type = "value", key = "k", stale_after_s = -5 }"#,
    ));
    assert!(e.contains("stale_after_s must be > 0 (got -5)"), "{e}");
    let e = err(&zone(r#"{ type = "value" }"#));
    assert!(e.contains("missing field `key`"), "{e}");
    let e = err(&zone(
        r##"{ type = "value", key = "k", colour = "#ffffff" }"##,
    ));
    assert!(e.contains("unknown field `colour`"), "{e}");
}

#[test]
fn sparkline_defaults_and_fill_forms() {
    let cfg = Config::parse(&zone(r#"{ type = "sparkline", key = "k" }"#)).unwrap();
    let WidgetConfig::Sparkline(s) = &cfg.scenes[0].zones[0].widget else {
        panic!("expected sparkline");
    };
    assert_eq!(s.color.to_string(), "#ffffff");
    assert_eq!(s.fill, None);
    assert_eq!((s.min, s.max, s.points), (None, None, None));

    let fill = |f: &str| {
        let cfg = Config::parse(&zone(&format!(
            r##"{{ type = "sparkline", key = "k", fill = {f}, min = -1.5, max = 10, points = 30 }}"##
        )))
        .unwrap();
        let WidgetConfig::Sparkline(s) = &cfg.scenes[0].zones[0].widget else {
            panic!("expected sparkline");
        };
        assert_eq!((s.min, s.max, s.points), (Some(-1.5), Some(10.0), Some(30)));
        s.fill
    };
    assert_eq!(fill("true"), Some(Fill::Enabled(true)));
    assert_eq!(fill("false"), Some(Fill::Enabled(false)));
    assert!(matches!(fill(r##""#003060""##), Some(Fill::Color(_))));
}

#[test]
fn sparkline_validation_errors() {
    let e = err(&zone(
        r#"{ type = "sparkline", key = "", min = 5, max = 5, points = 1 }"#,
    ));
    for want in [
        r#"scene "a" zone[0] sparkline: key must not be empty"#,
        r#"scene "a" zone[0] sparkline: min must be less than max (got min = 5, max = 5)"#,
        r#"scene "a" zone[0] sparkline: points must be at least 2 (got 1)"#,
    ] {
        assert!(e.contains(want), "missing {want:?} in:\n{e}");
    }
    let e = err(&zone(r#"{ type = "sparkline", key = "k", min = nan }"#));
    assert!(
        e.contains("sparkline: min must be a finite number (got NaN)"),
        "{e}"
    );
    let e = err(&zone(r#"{ type = "sparkline", key = "k", fill = "red" }"#));
    assert!(e.contains("did not match any variant"), "{e}");
    let e = err(&zone(r#"{ type = "sparkline", key = "k", font = "6x10" }"#));
    assert!(e.contains("unknown field `font`"), "{e}");
    let e = err(&zone(r#"{ type = "sparkline", key = "k", points = -3 }"#));
    assert!(e.contains("points"), "{e}");
}
