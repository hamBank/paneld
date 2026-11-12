//! Golden images for the bring-up test patterns, `tests/fixtures/golden/test_pattern_*.png`.
//! Same workflow as `golden.rs`: `UPDATE_GOLDEN=1 cargo test -p paneld golden`.

use std::path::PathBuf;

use embedded_graphics::pixelcolor::Rgb888;
use paneld::png_io;
use paneld::test_pattern::{render, Pattern};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/golden")
}

/// Compare against (or with `UPDATE_GOLDEN` write) `test_pattern_<name>.png`.
fn check(name: &str, pattern: Pattern, size: (u32, u32), panel: (u32, u32), frame: u64) {
    let actual = render(pattern, size.0, size.1, panel.0, panel.1, frame);
    let actual_png = png_io::encode(&actual, 1, false);
    let path = golden_dir().join(format!("test_pattern_{name}.png"));
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

const FULL: (u32, u32) = (172, 172);
const PANEL: (u32, u32) = (172, 86);

#[test]
fn golden_test_pattern_bars() {
    check("bars", Pattern::Bars, FULL, PANEL, 0);
}

#[test]
fn golden_test_pattern_gradient() {
    check("gradient", Pattern::Gradient, FULL, PANEL, 0);
}

#[test]
fn golden_test_pattern_panel_ids() {
    check("panel_ids", Pattern::PanelIds, FULL, PANEL, 0);
}

#[test]
fn golden_test_pattern_grid() {
    check("grid", Pattern::Grid, FULL, PANEL, 0);
}

#[test]
fn golden_test_pattern_solid() {
    check(
        "solid",
        Pattern::Solid(Rgb888::new(0x20, 0x80, 0xc0)),
        FULL,
        PANEL,
        0,
    );
}

#[test]
fn golden_test_pattern_walk() {
    // Row 90, column 37 of the 172-wide canvas.
    check("walk", Pattern::Walk, FULL, PANEL, 90 * 172 + 37);
}

#[test]
fn golden_test_pattern_panel_ids_partial() {
    check(
        "panel_ids_100x50",
        Pattern::PanelIds,
        (100, 50),
        (64, 32),
        0,
    );
}

#[test]
fn golden_test_pattern_grid_partial() {
    check("grid_100x50", Pattern::Grid, (100, 50), (64, 32), 0);
}
