//! Golden tests for the `image` widget (DESIGN.md §12). Same workflow as
//! `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld golden`, then look at
//! the PNGs in `tests/fixtures/golden/image_*.png` before committing.
//!
//! The scenes load the fixtures in `tests/fixtures/media/` (made by
//! `make_fixtures.py`). Zones are small so each golden is easy to inspect
//! after upscaling.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use paneld::config::Config;
use paneld::media::Bitmap;
use paneld::png_io;
use paneld::render::render_scene;
use paneld::store::{Store, StoreSnapshot, Value};
use paneld::widgets::FrameCtx;

const CONFIG: &str = r##"
[canvas]
width = 48
height = 32

# palette.png is 4x2: a wide image in a 48x32 zone.
[[scene]]
name = "fit_contain"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", path = "palette.png", fit = "contain" }
[[scene]]
name = "fit_cover"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", path = "palette.png", fit = "cover" }
[[scene]]
name = "fit_stretch"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", path = "palette.png", fit = "stretch" }
[[scene]]
name = "fit_none_aligned"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", path = "palette.png", fit = "none", align = "bottom_right" }

[[scene]]
name = "alpha_over_background"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 24, h = 32 }
  background = "#306090"
  widget = { type = "image", path = "alpha.png" }
  [[scene.zone]]
  rect = { x = 24, y = 0, w = 24, h = 32 }
  widget = { type = "image", path = "alpha.png" }

[[scene]]
name = "gif"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", path = "anim.gif" }
[[scene]]
name = "gif_no_loop"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", path = "anim.gif", loop = false }

[[scene]]
name = "smooth_downscale"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 32, h = 16 }
  widget = { type = "image", path = "detail.png", fit = "stretch" }
  [[scene.zone]]
  rect = { x = 0, y = 16, w = 32, h = 16 }
  widget = { type = "image", path = "detail.png", fit = "stretch", smooth = true }

[[scene]]
name = "from_store"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 48, h = 32 }
  background = "#202020"
  widget = { type = "image", key = "media:test", fit = "contain" }
"##;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn config() -> Config {
    Config::parse_in(CONFIG, &root().join("media")).unwrap()
}

fn ctx(elapsed_ms: u64) -> FrameCtx {
    FrameCtx {
        now: DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
        scene_elapsed: Duration::from_millis(elapsed_ms),
        frame: 0,
    }
}

fn check_with(name: &str, scene: &str, elapsed_ms: u64, store: &StoreSnapshot) {
    let actual = render_scene(&config(), scene, &ctx(elapsed_ms), store).unwrap();
    let dir = root().join("golden");
    let path = dir.join(format!("{name}.png"));
    let actual_png = png_io::encode(&actual, 1, false);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
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
        panic!(
            "{name}: render differs from {}; actual written to {}",
            path.display(),
            actual_path.display()
        );
    }
}

fn check(name: &str, scene: &str, elapsed_ms: u64) {
    check_with(name, scene, elapsed_ms, &Store::new().snapshot());
}

#[test]
fn golden_image_fit_contain() {
    check("image_fit_contain", "fit_contain", 0);
}

#[test]
fn golden_image_fit_cover() {
    check("image_fit_cover", "fit_cover", 0);
}

#[test]
fn golden_image_fit_stretch() {
    check("image_fit_stretch", "fit_stretch", 0);
}

#[test]
fn golden_image_fit_none_aligned() {
    check("image_fit_none_aligned", "fit_none_aligned", 0);
}

#[test]
fn golden_image_alpha_over_background() {
    check("image_alpha_over_background", "alpha_over_background", 0);
}

#[test]
fn golden_image_gif_frame0() {
    check("image_gif_frame0", "gif", 0);
}

#[test]
fn golden_image_gif_frame1() {
    // Frame 0 lasts 100 ms.
    check("image_gif_frame1", "gif", 150);
}

#[test]
fn golden_image_gif_frame2() {
    // Frames 0 and 1 last 100 + 200 ms.
    check("image_gif_frame2", "gif", 320);
}

#[test]
fn golden_image_gif_loops() {
    // Total 350 ms, so 670 ms is 320 ms into the second loop: frame 2.
    // (Identical to the frame 2 golden, so no file of its own.)
    let a = render_scene(&config(), "gif", &ctx(670), &Store::new().snapshot()).unwrap();
    let b = render_scene(&config(), "gif", &ctx(320), &Store::new().snapshot()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn golden_image_gif_no_loop_holds_last_frame() {
    check("image_gif_frame2", "gif_no_loop", 10_000);
}

#[test]
fn golden_image_smooth_downscale() {
    check("image_smooth_downscale", "smooth_downscale", 0);
}

#[test]
fn golden_image_from_store_follows_the_stored_image() {
    // Missing key: nothing drawn but the background.
    check("image_from_store_empty", "from_store", 0);

    let red = Arc::new(Bitmap::new(2, 2, [255u8, 0, 0, 255].repeat(4)).unwrap());
    let store = Store::new();
    store.writer("test").set("media:test", Value::Image(red));
    check_with("image_from_store_red", "from_store", 0, &store.snapshot());

    // A later write replaces the image.
    let blue = Arc::new(Bitmap::new(2, 2, [0u8, 0, 255, 255].repeat(4)).unwrap());
    store.writer("test").set("media:test", Value::Image(blue));
    check_with("image_from_store_blue", "from_store", 0, &store.snapshot());
}

#[test]
fn golden_image_store_swap_changes_output_and_steady_frames_do_not() {
    use paneld::scene::Scene;

    let cfg = config();
    let mut scene = Scene::from_config(cfg.scene("from_store").unwrap());
    let store = Store::new();
    let w = store.writer("test");
    let mut canvas = paneld::canvas::Canvas::new(48, 32);
    let px = |c: [u8; 4]| Arc::new(Bitmap::new(1, 1, c.to_vec()).unwrap());

    w.set("media:test", Value::Image(px([255, 0, 0, 255])));
    scene.render(&ctx(0), &store.snapshot(), &mut canvas);
    let first = canvas.clone();
    scene.render(&ctx(40), &store.snapshot(), &mut canvas);
    assert_eq!(canvas, first);

    w.set("media:test", Value::Image(px([0, 255, 0, 255])));
    scene.render(&ctx(80), &store.snapshot(), &mut canvas);
    assert_ne!(canvas, first);
}
