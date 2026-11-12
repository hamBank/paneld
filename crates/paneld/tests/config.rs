//! Config parsing and validation, including the error messages users see.

use std::path::Path;

use paneld::config::{Align, BrightnessMode, ColorOrder, Config, OutputConfig, WidgetConfig};

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
    assert!(cfg.scene("image").is_some());
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

fn media_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media")
}

fn image_cfg(widget: &str) -> String {
    format!(
        "{MINIMAL}  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 8, h = 8 }}\n  widget = {{ type = \"image\", {widget} }}\n"
    )
}

fn image_err(widget: &str) -> String {
    format!(
        "{:#}",
        Config::parse_in(&image_cfg(widget), &media_dir()).expect_err("should be rejected")
    )
}

#[test]
fn image_defaults_and_loading_relative_to_base_dir() {
    use paneld::config::{Fit, ImageAlign};
    let cfg = Config::parse_in(&image_cfg(r#"path = "anim.gif""#), &media_dir()).unwrap();
    let WidgetConfig::Image(i) = &cfg.scenes[0].zones[0].widget else {
        panic!("expected image");
    };
    assert_eq!(i.fit, Fit::Contain);
    assert_eq!(i.align, ImageAlign::Center);
    assert!(!i.smooth);
    assert!(i.looping);
    assert_eq!(i.loaded.as_ref().unwrap().frames().len(), 3);
    assert_eq!(cfg.base_dir, media_dir());
}

#[test]
fn image_options_parse() {
    use paneld::config::{Fit, ImageAlign};
    let cfg = Config::parse_in(
        &image_cfg(
            r#"key = "media:x", fit = "cover", align = "bottom_right", smooth = true, loop = false"#,
        ),
        &media_dir(),
    )
    .unwrap();
    let WidgetConfig::Image(i) = &cfg.scenes[0].zones[0].widget else {
        panic!("expected image");
    };
    assert_eq!(i.key.as_deref(), Some("media:x"));
    assert!(i.loaded.is_none());
    assert_eq!(i.fit, Fit::Cover);
    assert_eq!(i.align, ImageAlign::BottomRight);
    assert!(i.smooth && !i.looping);
}

#[test]
fn image_needs_exactly_one_of_path_and_key() {
    let e = image_err("");
    assert!(e.contains("one of path or key is required"), "{e}");
    let e = image_err(r#"path = "palette.png", key = "media:x""#);
    assert!(e.contains("set either path or key, not both"), "{e}");
    let e = image_err(r#"key = " ""#);
    assert!(e.contains("key must not be empty"), "{e}");
}

#[test]
fn image_load_errors_name_the_file_and_the_reason() {
    let e = image_err(r#"path = "nope.png""#);
    assert!(e.contains(r#"scene "a" zone[0] image: cannot load"#), "{e}");
    assert!(e.contains("nope.png"), "{e}");
    assert!(e.contains("cannot read file"), "{e}");

    // A file that exists but is not an image.
    let e = image_err(r#"path = "make_fixtures.py""#);
    assert!(e.contains("make_fixtures.py"), "{e}");
    assert!(e.contains("unsupported image format"), "{e}");
}

#[test]
fn image_enums_reject_unknown_values() {
    assert!(image_err(r#"path = "palette.png", fit = "zoom""#).contains("unknown variant `zoom`"));
    assert!(
        image_err(r#"path = "palette.png", align = "middle""#).contains("unknown variant `middle`")
    );
    assert!(
        image_err(r#"path = "palette.png", looping = false"#).contains("unknown field `looping`")
    );
}
