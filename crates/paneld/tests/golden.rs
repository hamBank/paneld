//! Golden image tests (DESIGN.md §12): render scenes with a fixed clock and
//! store, compare to `tests/fixtures/golden/*.png`.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p paneld golden`, then look
//! at the images before committing. On a mismatch the actual render is
//! written next to the golden as `<name>.actual.png` (git-ignored).

use std::path::PathBuf;
use std::time::Duration;

use chrono::DateTime;
use paneld::config::Config;
use paneld::png_io;
use paneld::render::render_scene;
use paneld::store::Store;
use paneld::widgets::FrameCtx;

const CONFIG: &str = r##"
[canvas]
width = 128
height = 32

[[scene]]
name = "clock_and_text"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 64, h = 32 }
  widget = { type = "clock", format = "%H:%M", font = "10x20", color = "#ffaa00" }
  [[scene.zone]]
  rect = { x = 64, y = 0, w = 64, h = 16 }
  widget = { type = "clock", format = "%a %d %b", font = "6x10", color = "#80c0ff" }
  [[scene.zone]]
  rect = { x = 64, y = 16, w = 64, h = 16 }
  background = "#101030"
  widget = { type = "text", text = "21.5°C", font = "6x10", align = "right" }

[[scene]]
name = "alignment"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 128, h = 10 }
  background = "#200000"
  widget = { type = "text", text = "left", font = "5x8", align = "left" }
  [[scene.zone]]
  rect = { x = 0, y = 11, w = 128, h = 10 }
  background = "#002000"
  widget = { type = "text", text = "center", font = "5x8" }
  [[scene.zone]]
  rect = { x = 0, y = 22, w = 128, h = 10 }
  background = "#000020"
  widget = { type = "text", text = "right", font = "5x8", align = "right" }

[[scene]]
name = "scroll"
  [[scene.zone]]
  rect = { x = 16, y = 8, w = 96, h = 16 }
  background = "#202020"
  widget = { type = "text", text = "clipped scrolling text", font = "6x10", scroll_px_per_s = 20 }
"##;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden")
}

fn check(name: &str, scene: &str, elapsed_s: f64) {
    let cfg = Config::parse(CONFIG).unwrap();
    let ctx = FrameCtx {
        now: DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
        scene_elapsed: Duration::from_secs_f64(elapsed_s),
        frame: 0,
    };
    let actual = render_scene(&cfg, scene, &ctx, &Store::new().snapshot()).unwrap();

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
            "{name}: {differing} pixel(s) differ from {} (size {}x{} vs {}x{}); actual written to {}",
            path.display(),
            golden.width(),
            golden.height(),
            actual.width(),
            actual.height(),
            actual_path.display()
        );
    }
}

#[test]
fn golden_clock_and_text() {
    check("clock_and_text", "clock_and_text", 0.0);
}

#[test]
fn golden_alignment() {
    check("alignment", "alignment", 0.0);
}

#[test]
fn golden_scroll_entering() {
    // 1s at 20 px/s: the text has entered 20px from the right edge.
    check("scroll_entering", "scroll", 1.0);
}

#[test]
fn golden_scroll_midway() {
    check("scroll_midway", "scroll", 4.0);
}
