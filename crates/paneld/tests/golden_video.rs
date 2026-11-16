//! Golden tests for the `video` widget (DESIGN.md §9, §12). Same workflow as
//! `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld golden`, then look at
//! the PNGs in `tests/fixtures/golden/video_*.png` before committing.
//!
//! No `ffmpeg` runs: the widget gets a detached [`VideoHandle`] and the test
//! puts frames into its slot, exactly as a decoder would. The video zone
//! sits next to and under other zones so the offset and the clipping show.

use std::path::PathBuf;
use std::time::Duration;

use chrono::DateTime;
use embedded_graphics::pixelcolor::Rgb888;
use paneld::canvas::{Canvas, Rect};
use paneld::config::Config;
use paneld::png_io;
use paneld::scene::{Scene, Zone};
use paneld::store::Store;
use paneld::video::{Frame, VideoHandle};
use paneld::widgets::{self, FrameCtx, Video};

/// The text zones of the scene; the last one is drawn over the video zone.
const CONFIG: &str = r##"
[canvas]
width = 64
height = 32
[[scene]]
name = "labels"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 18, h = 32 }
  widget = { type = "text", text = "CAM", font = "6x10", color = "#ffffff" }
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 18, h = 6 }
  widget = { type = "text", text = "live", font = "4x6", color = "#ffff00" }
  # Drawn after the video zone, over its corner.
  [[scene.zone]]
  rect = { x = 48, y = 20, w = 16, h = 12 }
  background = "#c00000"
  widget = { type = "text", text = "REC", font = "4x6", color = "#ffffff" }
"##;

const VIDEO_RECT: Rect = Rect {
    x: 20,
    y: 4,
    w: 40,
    h: 24,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn ctx() -> FrameCtx {
    FrameCtx {
        now: DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap(),
        scene_elapsed: Duration::ZERO,
        frame: 0,
    }
}

/// A 40x24 test frame: a red/green gradient on top, eight colour bars below.
fn test_frame(w: u32, h: u32) -> Frame {
    const BARS: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            if y < h * 2 / 3 {
                rgb.extend([
                    (x * 255 / (w - 1)) as u8,
                    (y * 255 / (h * 2 / 3 - 1)) as u8,
                    64,
                ]);
            } else {
                rgb.extend(BARS[(x * 8 / w) as usize]);
            }
        }
    }
    Frame {
        width: w,
        height: h,
        rgb,
        seq: 1,
    }
}

fn render(frame: Option<Frame>, background: Option<Rgb888>) -> Canvas {
    let cfg = Config::parse(CONFIG).unwrap();
    let mut zones: Vec<Zone> = cfg.scenes[0]
        .zones
        .iter()
        .map(|z| Zone::new(z.rect, z.background.map(|c| c.0), widgets::build(&z.widget)))
        .collect();
    let handle = VideoHandle::detached();
    if let Some(f) = frame {
        handle.shared().slot.put(f);
    }
    let overlay = zones.pop().unwrap();
    zones.push(Zone::new(
        VIDEO_RECT,
        background,
        Box::new(Video::with_handle(handle)),
    ));
    zones.push(overlay);
    let mut scene = Scene::from_zones("video", zones);
    let mut canvas = Canvas::new(64, 32);
    scene.render(&ctx(), &Store::new().snapshot(), &mut canvas);
    canvas
}

fn check(name: &str, actual: &Canvas) {
    let dir = root().join("golden");
    let path = dir.join(format!("{name}.png"));
    let actual_png = png_io::encode(actual, 1, false);
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
    if golden != *actual {
        let actual_path = path.with_extension("actual.png");
        std::fs::write(&actual_path, &actual_png).unwrap();
        panic!(
            "{name}: render differs from {}; actual written to {}",
            path.display(),
            actual_path.display()
        );
    }
}

#[test]
fn golden_video_frame() {
    check("video_frame", &render(Some(test_frame(40, 24)), None));
}

#[test]
fn golden_video_no_frame_is_black_over_a_background() {
    // The zone background is a lit grey; the no-frame state still draws black.
    check(
        "video_no_frame",
        &render(None, Some(Rgb888::new(0x40, 0x40, 0x40))),
    );
}
