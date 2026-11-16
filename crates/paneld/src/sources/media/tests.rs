use super::*;
use crate::canvas::Canvas;
use embedded_graphics::pixelcolor::Rgb888;

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("paneld-media-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut c = Canvas::new(w, h);
    c.set_pixel(0, 0, Rgb888::new(255, 0, 0));
    crate::png_io::encode(&c, 1, false)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media")
        .join(name)
}

#[test]
fn loads_png_and_gif_and_ignores_other_files() {
    let d = tmp("load");
    std::fs::write(d.join("beach.png"), png(8, 4)).unwrap();
    std::fs::copy(fixture("anim.gif"), d.join("Spin.GIF")).unwrap();
    std::fs::write(d.join("notes.txt"), "hello").unwrap();
    std::fs::create_dir_all(d.join("sub.png")).unwrap();

    let s = scan("photos", &d, 512, &Scan::default()).unwrap();
    assert_eq!(s.names(), ["Spin", "beach"]);
    assert!(s.problems.is_empty(), "{:?}", s.problems);
    assert_eq!(s.get("beach").unwrap().size(), (8, 4));
    assert!(s.get("Spin").unwrap().is_animated(), "GIF frames are kept");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn skips_bad_names_reserved_names_and_broken_files() {
    let d = tmp("skip");
    std::fs::write(d.join("my photo.png"), png(2, 2)).unwrap();
    std::fs::write(d.join("count.png"), png(2, 2)).unwrap();
    std::fs::write(d.join("broken.png"), b"\x89PNG\r\n\x1a\nnope").unwrap();
    std::fs::write(d.join("ok.png"), png(2, 2)).unwrap();

    let s = scan("photos", &d, 512, &Scan::default()).unwrap();
    assert_eq!(s.names(), ["ok"]);
    assert_eq!(s.problems.len(), 3, "{:?}", s.problems);
    assert!(s.problems.iter().any(|p| p.starts_with("my photo.png")));
    assert!(s.problems.iter().any(|p| p.contains("reserved")));
    assert!(s.problems.iter().any(|p| p.starts_with("broken.png")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn large_images_are_scaled_down_to_max_px() {
    let d = tmp("max");
    std::fs::write(d.join("wide.png"), png(400, 100)).unwrap();
    std::fs::write(d.join("small.png"), png(20, 10)).unwrap();
    let s = scan("m", &d, 64, &Scan::default()).unwrap();
    assert_eq!(s.get("wide").unwrap().size(), (64, 16));
    assert_eq!(s.get("small").unwrap().size(), (20, 10));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rescan_reuses_unchanged_files_and_diff_tracks_changes() {
    let d = tmp("rescan");
    std::fs::write(d.join("a.png"), png(2, 2)).unwrap();
    std::fs::write(d.join("b.png"), png(2, 2)).unwrap();
    let first = scan("m", &d, 512, &Scan::default()).unwrap();
    let c = diff("m", &Scan::default(), &first);
    assert_eq!(c.len(), 3, "a, b and count");

    // Unchanged: the same decoded image, nothing but `count` to write.
    let again = scan("m", &d, 512, &first).unwrap();
    assert!(Arc::ptr_eq(
        again.get("a").unwrap(),
        first.get("a").unwrap()
    ));
    assert_eq!(diff("m", &first, &again).len(), 1);

    // b changes size, a is removed, c is added.
    std::fs::write(d.join("b.png"), png(3, 3)).unwrap();
    std::fs::remove_file(d.join("a.png")).unwrap();
    std::fs::write(d.join("c.png"), png(2, 2)).unwrap();
    let next = scan("m", &d, 512, &again).unwrap();
    let c = diff("m", &again, &next);
    assert!(c.contains(&Change::Remove("m:a".into())));
    assert!(c
        .iter()
        .any(|ch| matches!(ch, Change::Set(k, Value::Image(_)) if k == "m:b")));
    assert!(c
        .iter()
        .any(|ch| matches!(ch, Change::Set(k, Value::Image(_)) if k == "m:c")));
    assert!(c.contains(&Change::Set(
        "m:count".into(),
        Value::Number {
            value: 2.0,
            unit: None
        }
    )));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn missing_folder_is_an_error() {
    assert!(scan("m", Path::new("/nonexistent/paneld"), 512, &Scan::default()).is_err());
}

fn shown(c: &Change, scan: &Scan) -> Option<String> {
    match c {
        Change::Set(k, Value::Image(m)) if k == "m:current" => scan
            .images
            .iter()
            .find(|(_, l)| Arc::ptr_eq(&l.media, m))
            .map(|(n, _)| n.clone()),
        Change::Remove(k) if k == "m:current" => None,
        other => panic!("unexpected change {other:?}"),
    }
}

#[test]
fn rotation_cycles_in_name_order_and_survives_rescans() {
    let d = tmp("rotate");
    for n in ["a", "b", "c"] {
        std::fs::write(d.join(format!("{n}.png")), png(2, 2)).unwrap();
    }
    let s = scan("m", &d, 512, &Scan::default()).unwrap();
    let mut r = Rotation::default();
    assert_eq!(shown(&r.settle("m", &s), &s).as_deref(), Some("a"));
    assert_eq!(shown(&r.advance("m", &s), &s).as_deref(), Some("b"));
    assert_eq!(
        shown(&r.settle("m", &s), &s).as_deref(),
        Some("b"),
        "rescan keeps it"
    );
    assert_eq!(shown(&r.advance("m", &s), &s).as_deref(), Some("c"));
    assert_eq!(
        shown(&r.advance("m", &s), &s).as_deref(),
        Some("a"),
        "wraps"
    );

    // The shown image disappears: move on to the next name.
    std::fs::remove_file(d.join("a.png")).unwrap();
    let s2 = scan("m", &d, 512, &s).unwrap();
    assert_eq!(shown(&r.settle("m", &s2), &s2).as_deref(), Some("b"));

    // Empty folder: no current image.
    for n in ["b", "c"] {
        std::fs::remove_file(d.join(format!("{n}.png"))).unwrap();
    }
    let empty = scan("m", &d, 512, &s2).unwrap();
    assert_eq!(shown(&r.settle("m", &empty), &empty), None);
    let _ = std::fs::remove_dir_all(&d);
}
