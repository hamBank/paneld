//! Golden images for notifications (DESIGN.md §5.5, §12): inline content
//! (short text with an icon, long scrolling text) and a scene notification.
//! Same workflow as `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld
//! golden`, then look at `tests/fixtures/golden/notification_*.png`.

use std::path::PathBuf;
use std::time::Duration;

use chrono::DateTime;
use paneld::canvas::Canvas;
use paneld::color::Color;
use paneld::config::Config;
use paneld::inline;
use paneld::notifications::InlineNotice;
use paneld::png_io;
use paneld::render::render_scene;
use paneld::store::Store;
use paneld::widgets::FrameCtx;

const CONFIG: &str = r##"
[canvas]
width = 172
height = 172

[[scene]]
name = "alert"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 172 }
  background = "#300000"
  widget = { type = "text", text = "Door open", font = "10x20", font_scale = 1, color = "#ff4040" }
"##;

fn ctx(elapsed_s: f64) -> FrameCtx {
    FrameCtx {
        now: DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
        scene_elapsed: Duration::from_secs_f64(elapsed_s),
        frame: 0,
    }
}

fn inline_frame(text: &str, icon: Option<&str>, color: &str, elapsed_s: f64) -> Canvas {
    let cfg = Config::parse(CONFIG).unwrap();
    let notice = InlineNotice {
        text: text.into(),
        icon: icon.map(str::to_owned),
        color: color.parse::<Color>().unwrap(),
    };
    let mut scene = inline::scene(&cfg.notifications, 172, 172, &notice);
    let mut canvas = Canvas::new(172, 172);
    scene.render(&ctx(elapsed_s), &Store::new().snapshot(), &mut canvas);
    canvas
}

fn check(name: &str, actual: Canvas) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden");
    let path = dir.join(format!("{name}.png"));
    let actual_png = png_io::encode(&actual, 1, false);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
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
        panic!("{name}: differs from {}", path.display());
    }
}

#[test]
fn golden_notification_inline_short() {
    check(
        "notification_inline_short",
        inline_frame("Washing done", Some("bolt"), "#00ff80", 0.0),
    );
}

#[test]
fn golden_notification_inline_scroll_entering() {
    // 40 px/s: after 1 s the text has entered 40 px from the right edge.
    check(
        "notification_inline_scroll_entering",
        inline_frame(
            "The washing machine in the basement has finished its cycle",
            Some("home"),
            "#ffcc00",
            1.0,
        ),
    );
}

#[test]
fn golden_notification_inline_scroll_midway() {
    check(
        "notification_inline_scroll_midway",
        inline_frame(
            "The washing machine in the basement has finished its cycle",
            Some("home"),
            "#ffcc00",
            6.0,
        ),
    );
}

#[test]
fn golden_notification_scene() {
    let cfg = Config::parse(CONFIG).unwrap();
    let actual = render_scene(&cfg, "alert", &ctx(0.0), &Store::new().snapshot()).unwrap();
    check("notification_scene", actual);
}
