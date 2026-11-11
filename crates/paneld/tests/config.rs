//! Config parsing and validation, including the error messages users see.

use std::path::Path;

use paneld::config::{Align, Config, OutputConfig, WidgetConfig};

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
    let OutputConfig::Preview(p) = &cfg.outputs[0];
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
