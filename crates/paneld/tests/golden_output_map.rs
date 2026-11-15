//! Golden images of the card-space framebuffer (`paneld render --output`)
//! for an asymmetric pattern under every rotation and flip,
//! `tests/fixtures/golden/output_map_*.png`.
//! Same workflow as `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld golden`.

use std::path::PathBuf;

use embedded_graphics::pixelcolor::Rgb888;
use paneld::canvas::Canvas;
use paneld::config::Config;
use paneld::png_io;
use paneld::render::card_frame;
use paneld::test_pattern::{self, Pattern};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden")
}

const W: u32 = 32;
const H: u32 = 20;

/// Colour bars, with a white block at the top-left and a blue block at the
/// bottom-left: it looks different under every rotation and flip.
fn pattern() -> Canvas {
    let mut c = test_pattern::render(Pattern::Bars, W, H, W, H, 0);
    for y in 0..3 {
        for x in 0..8 {
            c.set_pixel(x, y, Rgb888::new(255, 255, 255));
        }
    }
    for y in H - 8..H {
        for x in 0..3 {
            c.set_pixel(x, y, Rgb888::new(0, 0, 255));
        }
    }
    c
}

fn config(region: &str) -> Config {
    Config::parse(&format!(
        "[canvas]\nwidth = {W}\nheight = {H}\n[[scene]]\nname = \"s\"\n\
         [[output]]\ntype = \"preview\"\n\
         [[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\n{region}"
    ))
    .unwrap()
}

/// Compare against (or with `UPDATE_GOLDEN` write) `output_map_<name>.png`.
fn check(name: &str, region: &str) {
    let cfg = config(region);
    let actual = card_frame(&cfg, 1, &pattern()).unwrap();
    let actual_png = png_io::encode(&actual, 1, false);
    let path = golden_dir().join(format!("output_map_{name}.png"));
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
        panic!(
            "{name}: differs from {} (actual written to {})",
            path.display(),
            actual_path.display()
        );
    }
}

fn region(rotate: u32, flip: &str) -> String {
    format!(
        "[[output.region]]\ncanvas = {{ x = 0, y = 0, w = {W}, h = {H} }}\n\
         rotate = {rotate}\nflip = \"{flip}\"\n"
    )
}

#[test]
fn golden_output_map_identity() {
    check("rotate0", &region(0, "none"));
}

#[test]
fn golden_output_map_rotate90() {
    check("rotate90", &region(90, "none"));
}

#[test]
fn golden_output_map_rotate180() {
    check("rotate180", &region(180, "none"));
}

#[test]
fn golden_output_map_rotate270() {
    check("rotate270", &region(270, "none"));
}

#[test]
fn golden_output_map_flip_horizontal() {
    check("flip_horizontal", &region(0, "horizontal"));
}

#[test]
fn golden_output_map_flip_vertical() {
    check("flip_vertical", &region(0, "vertical"));
}

#[test]
fn golden_output_map_flip_then_rotate() {
    check("rotate90_flip_horizontal", &region(90, "horizontal"));
}

#[test]
fn golden_output_map_stacked_panels_bottom_upside_down() {
    // The real layout in miniature: two 32x10 panels, bottom one rotated.
    check(
        "stacked_bottom_180",
        &format!(
            "[[output.region]]\ncanvas = {{ x = 0, y = 0, w = {W}, h = 10 }}\n\
             [[output.region]]\ncanvas = {{ x = 0, y = 10, w = {W}, h = 10 }}\n\
             card_offset = {{ x = 0, y = 10 }}\nrotate = 180\n"
        ),
    );
}

#[test]
fn golden_output_map_gaps_are_black() {
    // Two regions on a larger card: the rest stays black.
    check(
        "card_with_gaps",
        "card_size = { w = 44, h = 34 }\n\
         [[output.region]]\ncanvas = { x = 0, y = 0, w = 16, h = 10 }\n\
         card_offset = { x = 2, y = 2 }\n\
         [[output.region]]\ncanvas = { x = 16, y = 0, w = 16, h = 10 }\n\
         card_offset = { x = 22, y = 16 }\nrotate = 270\n",
    );
}
