//! Config parsing and validation of the `video` widget (DESIGN.md §9).

use std::path::Path;

use paneld::config::{Config, Fit, VideoConfig, VideoInput, WidgetConfig};

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

fn video_cfg(widget: &str) -> String {
    format!(
        "{MINIMAL}\n  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 16, h = 16 }}\n  widget = {{ type = \"video\", {widget} }}\n"
    )
}

fn media_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media")
}

fn video_of(cfg: &Config) -> &VideoConfig {
    match &cfg.scenes[0].zones[0].widget {
        WidgetConfig::Video(v) => v,
        w => panic!("not a video widget: {w:?}"),
    }
}

#[test]
fn defaults_and_path_resolution() {
    // Any existing file will do: validation does not decode it.
    let cfg = Config::parse_in(&video_cfg("path = \"anim.gif\""), &media_dir()).unwrap();
    let v = video_of(&cfg);
    assert_eq!(v.fps, 15);
    assert!(v.looping);
    assert_eq!(v.fit, Fit::Contain);
    assert!(v.program.is_none());
    assert_eq!(
        v.resolved_path.as_deref(),
        Some(media_dir().join("anim.gif").as_path())
    );
}

#[test]
fn all_fields() {
    let cfg = Config::parse(&video_cfg(
        "url_env = \"PANELD_TEST_CAM\", fit = \"cover\", fps = 60, loop = false, program = \"/opt/ffmpeg\"",
    ))
    .unwrap();
    let v = video_of(&cfg);
    assert_eq!((v.fps, v.looping, v.fit), (60, false, Fit::Cover));
    assert_eq!(v.program.as_deref(), Some(Path::new("/opt/ffmpeg")));
}

#[test]
fn needs_exactly_one_source() {
    let e = err(&video_cfg(""));
    assert!(
        e.contains("scene \"a\" zone[0] video: one of path or url_env is required"),
        "{e}"
    );
    let e = err(&video_cfg("path = \"x.mp4\", url_env = \"CAM\""));
    assert!(e.contains("set either path or url_env, not both"), "{e}");
}

#[test]
fn literal_url_is_rejected_without_echoing_it() {
    let e = err(&video_cfg("url = \"rtsp://user:hunter2@cam/stream\""));
    assert!(
        e.contains("scene \"a\" zone[0] video: a literal url is not allowed"),
        "{e}"
    );
    assert!(e.contains("url_env"), "{e}");
    assert!(!e.contains("hunter2") && !e.contains("rtsp://"), "{e}");
    // Also next to a valid source.
    let e = err(&video_cfg(
        "url = \"rtsp://u:hunter2@cam/s\", url_env = \"CAM\"",
    ));
    assert!(
        e.contains("literal url is not allowed") && !e.contains("hunter2"),
        "{e}"
    );
}

#[test]
fn fps_range() {
    for ok in ["1", "60"] {
        Config::parse(&video_cfg(&format!("url_env = \"CAM\", fps = {ok}"))).unwrap();
    }
    for bad in ["0", "61"] {
        let e = err(&video_cfg(&format!("url_env = \"CAM\", fps = {bad}")));
        assert!(e.contains("zone[0] video: fps must be 1..=60"), "{e}");
    }
}

#[test]
fn path_must_exist() {
    let e = format!(
        "{:#}",
        Config::parse_in(&video_cfg("path = \"missing.mp4\""), &media_dir()).unwrap_err()
    );
    assert!(
        e.contains("zone[0] video: file ") && e.contains("missing.mp4"),
        "{e}"
    );
}

#[test]
fn url_env_must_be_a_name() {
    for bad in ["rtsp://cam/stream", "", "1CAM", "A B"] {
        let e = err(&video_cfg(&format!("url_env = \"{bad}\"")));
        assert!(
            e.contains("url_env must be the NAME of an environment variable"),
            "{bad}: {e}"
        );
    }
}

#[test]
fn unknown_field_is_rejected() {
    let e = err(&video_cfg("url_env = \"CAM\", bogus = 1"));
    assert!(e.contains("bogus"), "{e}");
}

#[test]
fn missing_variable_is_caught_by_the_startup_check() {
    let cfg = Config::parse(&video_cfg("url_env = \"PANELD_TEST_VIDEO_UNSET\"")).unwrap();
    let e = cfg.check_video_environment().unwrap_err().to_string();
    assert!(
        e.contains("scene \"a\" zone[0] video:")
            && e.contains("PANELD_TEST_VIDEO_UNSET is not set"),
        "{e}"
    );
    // `paneld check` only warns.
    assert!(cfg
        .video_warnings()
        .iter()
        .any(|w| w.contains("PANELD_TEST_VIDEO_UNSET")));

    std::env::set_var("PANELD_TEST_VIDEO_EMPTY", "  ");
    let cfg = Config::parse(&video_cfg("url_env = \"PANELD_TEST_VIDEO_EMPTY\"")).unwrap();
    assert!(cfg
        .check_video_environment()
        .unwrap_err()
        .to_string()
        .contains("is empty"));

    std::env::set_var("PANELD_TEST_VIDEO_SET", "rtsp://user:hunter2@cam/stream");
    let cfg = Config::parse(&video_cfg("url_env = \"PANELD_TEST_VIDEO_SET\"")).unwrap();
    cfg.check_video_environment().unwrap();
}

#[test]
fn file_source_needs_no_environment() {
    let cfg = Config::parse_in(&video_cfg("path = \"anim.gif\""), &media_dir()).unwrap();
    cfg.check_video_environment().unwrap();
}

#[test]
fn check_warns_when_ffmpeg_is_missing() {
    let cfg = Config::parse(&video_cfg(
        "url_env = \"CAM\", program = \"/nonexistent/paneld-test-ffmpeg\"",
    ))
    .unwrap();
    let w = cfg.video_warnings();
    assert!(
        w.iter()
            .any(|w| w.contains("zone[0] video:") && w.contains("not found")),
        "{w:?}"
    );
}

#[test]
fn url_never_shows_in_debug_output() {
    std::env::set_var("PANELD_TEST_VIDEO_DEBUG", "rtsp://user:hunter2@cam/stream");
    let cfg = Config::parse(&video_cfg("url_env = \"PANELD_TEST_VIDEO_DEBUG\"")).unwrap();
    let input = video_of(&cfg).input().unwrap();
    for dbg in [format!("{cfg:?}"), format!("{input:?}")] {
        assert!(
            !dbg.contains("hunter2") && !dbg.contains("rtsp://"),
            "{dbg}"
        );
    }
    assert!(format!("{input:?}").contains("redacted"));
    match input {
        VideoInput::Url(s) => assert!(s.expose().contains("hunter2")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn literal_url_is_not_kept_in_the_parsed_config() {
    // Parsing succeeds (validation rejects it); the value is discarded.
    let cfg: Config = toml::from_str(&video_cfg("url = \"rtsp://u:hunter2@cam/s\"")).unwrap();
    assert!(!format!("{cfg:?}").contains("hunter2"));
}
