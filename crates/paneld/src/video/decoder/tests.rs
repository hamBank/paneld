use super::*;
use std::path::{Path, PathBuf};

fn strs(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn spec(input: Input, fit: Fit) -> DecoderSpec {
    DecoderSpec {
        program: "ffmpeg".into(),
        input,
        width: 64,
        height: 32,
        fit,
        fps: 10.0,
    }
}

fn file(looping: bool) -> Input {
    Input::File {
        path: "/media/a b.mp4".into(),
        looping,
    }
}

// ---------------------------------------------------------------------------
// Argument building
// ---------------------------------------------------------------------------

#[test]
fn args_looping_file_contain() {
    let a = strs(&ffmpeg_args(&spec(file(true), Fit::Contain)));
    assert_eq!(
        a,
        [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-stream_loop",
            "-1",
            "-i",
            "/media/a b.mp4",
            "-an",
            "-vf",
            "scale=64:32:force_original_aspect_ratio=decrease,\
             pad=64:32:(ow-iw)/2:(oh-ih)/2:color=black,fps=10,format=rgb24",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ]
    );
}

#[test]
fn args_non_looping_file_has_no_stream_loop() {
    let a = strs(&ffmpeg_args(&spec(file(false), Fit::Stretch)));
    assert!(a.contains(&"-re".to_string()));
    assert!(!a.contains(&"-stream_loop".to_string()));
    assert!(a.contains(&"scale=64:32,fps=10,format=rgb24".to_string()));
}

#[test]
fn args_fit_filters() {
    let vf = |fit| {
        let a = strs(&ffmpeg_args(&spec(file(false), fit)));
        let i = a.iter().position(|x| x == "-vf").unwrap();
        a[i + 1].clone()
    };
    assert_eq!(
        vf(Fit::Cover),
        "scale=64:32:force_original_aspect_ratio=increase,crop=64:32,fps=10,format=rgb24"
    );
    assert_eq!(vf(Fit::Stretch), "scale=64:32,fps=10,format=rgb24");
    assert_eq!(
        vf(Fit::None),
        "pad=max(iw\\,64):max(ih\\,32):(ow-iw)/2:(oh-ih)/2:color=black,crop=64:32,\
         fps=10,format=rgb24"
    );
    assert!(vf(Fit::Contain).starts_with("scale=64:32:force_original_aspect_ratio=decrease,pad="));
}

#[test]
fn args_fractional_fps() {
    let mut s = spec(file(false), Fit::Stretch);
    s.fps = 7.5;
    let a = strs(&ffmpeg_args(&s));
    assert!(a.iter().any(|x| x.contains("fps=7.5,")));
}

#[test]
fn args_rtsp_url() {
    let a = strs(&ffmpeg_args(&spec(
        Input::Url("rtsp://u:p@cam/stream".into()),
        Fit::Cover,
    )));
    assert_eq!(
        &a[..11],
        [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-timeout",
            "10000000",
            "-i",
            "rtsp://u:p@cam/stream",
            "-an"
        ]
    );
    assert!(
        !a.contains(&"-re".to_string()),
        "live sources are not paced"
    );
}

#[test]
fn args_http_url_and_other_schemes() {
    let a = strs(&ffmpeg_args(&spec(
        Input::Url("HTTPS://host/x.m3u8".into()),
        Fit::Stretch,
    )));
    let i = a.iter().position(|x| x == "-rw_timeout").unwrap();
    assert_eq!(a[i + 1], "10000000");
    assert!(
        i < a.iter().position(|x| x == "-i").unwrap(),
        "input option"
    );
    assert!(!a.contains(&"-rtsp_transport".to_string()));

    let a = strs(&ffmpeg_args(&spec(
        Input::Url("udp://239.0.0.1:1234".into()),
        Fit::None,
    )));
    assert!(!a.contains(&"-rw_timeout".to_string()));
    assert!(!a.contains(&"-timeout".to_string()));
}

// ---------------------------------------------------------------------------
// Redaction and backoff
// ---------------------------------------------------------------------------

#[test]
fn redacts_urls_with_credentials_and_keys() {
    assert_eq!(
        redact(
            "rtsp://user:pass@10.0.0.5:554/s?key=abc: Connection refused",
            &[]
        ),
        "rtsp://*** Connection refused"
    );
    assert_eq!(
        redact("Error opening input file http://h/a?token=1 (x). Done", &[]),
        "Error opening input file http://*** (x). Done"
    );
    assert_eq!(
        redact(r#"a "rtsp://u:p@h/x" b rtsps://u2:p2@h2/y c"#, &[]),
        r#"a "rtsp://***" b rtsps://*** c"#
    );
    // No URL: untouched.
    assert_eq!(redact("Invalid data found", &[]), "Invalid data found");
    // Exact secret without a scheme.
    assert_eq!(
        redact("bad host u:p@cam failed", &["u:p@cam"]),
        "bad host *** failed"
    );
    // Empty secrets are ignored (would match everywhere).
    assert_eq!(redact("abc", &[""]), "abc");
}

#[test]
fn backoff_doubles_to_the_cap() {
    let max = Duration::from_secs(30);
    let mut b = Duration::from_secs(1);
    let mut seen = vec![];
    for _ in 0..7 {
        seen.push(b.as_secs());
        b = next_backoff(b, max);
    }
    assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30]);
}

// ---------------------------------------------------------------------------
// Helpers for process tests
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::AtomicU32;
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "paneld-video-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_until(what: &str, max: Duration, shared: Option<&Shared>, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + max;
    while Instant::now() < end {
        if let Some(s) = shared {
            s.touch();
        }
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn fast() -> Timing {
    Timing {
        idle_stop: Duration::from_millis(300),
        poll: Duration::from_millis(5),
        idle_poll: Duration::from_millis(20),
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        reset_after: Duration::from_secs(60),
        stale_after: Duration::from_secs(10),
        reader_grace: Duration::from_millis(100),
    }
}

#[cfg(target_os = "linux")]
mod fake {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 4x2 zone: 24 bytes per frame.
    const W: u32 = 4;
    const H: u32 = 2;

    /// Shell prelude: `frame N` prints one 24-byte frame filled with byte N.
    fn script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-ffmpeg.sh");
        let text = format!(
            "#!/bin/sh\nframe() {{ head -c 24 /dev/zero | tr '\\000' \"\\\\$(printf '%03o' $1)\"; }}\n{body}\n"
        );
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn fake_spec(program: PathBuf, input: Input) -> DecoderSpec {
        DecoderSpec {
            program,
            input,
            width: W,
            height: H,
            fit: Fit::Stretch,
            fps: 10.0,
        }
    }

    fn url() -> Input {
        Input::Url("rtsp://u:p@cam/s".into())
    }

    fn alive(pid: &str) -> bool {
        Path::new(&format!("/proc/{}", pid.trim())).exists()
    }

    fn read_trim(p: &Path) -> String {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    #[test]
    fn frames_arrive_in_order_and_are_exact() {
        let d = TempDir::new("order");
        let prog = script(
            d.path(),
            "for i in 1 2 3 4 5; do frame $i; sleep 0.05; done; exec sleep 30",
        );
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn_with(fake_spec(prog, url()), shared.clone(), fast());
        let mut seen = vec![];
        wait_until("5 frames", Duration::from_secs(5), Some(&shared), || {
            if let Some(f) = shared.slot.latest() {
                assert_eq!((f.width, f.height, f.rgb.len()), (W, H, 24));
                assert!(f.rgb.iter().all(|&b| b == f.seq as u8), "bytes match seq");
                if seen.last() != Some(&f.seq) {
                    seen.push(f.seq);
                }
            }
            seen.last() == Some(&5)
        });
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        assert_eq!(shared.stats.frames_decoded.load(Ordering::Relaxed), 5);
        drop(g);
    }

    #[test]
    fn wanted_gates_the_process_and_idle_clears_the_slot() {
        let d = TempDir::new("gate");
        let starts = d.path().join("starts");
        let pidf = d.path().join("pid");
        let body = format!(
            "echo $$ > {pid}; echo x >> {st}; while :; do frame 7; sleep 0.02; done",
            pid = pidf.display(),
            st = starts.display()
        );
        let prog = script(d.path(), &body);
        let shared = Arc::new(Shared::new());
        let g = spawn_with(fake_spec(prog, url()), shared.clone(), fast());

        // Never touched: nothing runs.
        thread::sleep(Duration::from_millis(250));
        assert!(!starts.exists());
        assert_eq!(shared.stats.starts.load(Ordering::Relaxed), 0);

        // Touched: starts and delivers.
        wait_until("first frame", Duration::from_secs(5), Some(&shared), || {
            shared.slot.latest().is_some()
        });
        let pid = read_trim(&pidf);
        assert!(alive(&pid));

        // No more touches: process is killed and reaped, slot cleared.
        wait_until("process gone", Duration::from_secs(5), None, || {
            !alive(&pid)
        });
        wait_until("slot cleared", Duration::from_secs(5), None, || {
            shared.slot.latest().is_none()
        });

        // Touch again: a second start.
        wait_until("restart", Duration::from_secs(5), Some(&shared), || {
            shared.stats.starts.load(Ordering::Relaxed) == 2 && shared.slot.latest().is_some()
        });
        assert_eq!(shared.stats.errors.load(Ordering::Relaxed), 0);
        drop(g);
    }

    #[test]
    fn early_exit_restarts_with_growing_backoff() {
        let d = TempDir::new("backoff");
        let times = d.path().join("times");
        let body = format!("date +%s%N >> {}; frame 1; exit 1", times.display());
        let prog = script(d.path(), &body);
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn_with(
            fake_spec(
                prog,
                Input::File {
                    path: "x.mp4".into(),
                    looping: true,
                },
            ),
            shared.clone(),
            fast(),
        );
        wait_until("4 starts", Duration::from_secs(10), Some(&shared), || {
            read_trim(&times).lines().count() >= 4
        });
        drop(g);
        let t: Vec<u128> = read_trim(&times)
            .lines()
            .map(|l| l.parse::<u128>().unwrap() / 1_000_000)
            .collect();
        assert!(t.len() >= 4);
        // Backoff 50, 100, 200 ms (lower bounds only: scheduling adds delay).
        assert!(t[1] - t[0] >= 45, "{t:?}");
        assert!(t[2] - t[1] >= 95, "{t:?}");
        assert!(t[3] - t[2] >= 190, "{t:?}");
        assert!(shared.stats.errors.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn partial_frame_at_eof_is_discarded() {
        let d = TempDir::new("partial");
        let body = "frame 5; head -c 12 /dev/zero | tr '\\000' '\\011'; exit 0";
        let prog = script(d.path(), body);
        let shared = Arc::new(Shared::new());
        shared.touch();
        let mut t = fast();
        t.initial_backoff = Duration::from_secs(30); // no restart during the test
        t.max_backoff = Duration::from_secs(30);
        let g = spawn_with(fake_spec(prog, url()), shared.clone(), t);
        wait_until(
            "error counted",
            Duration::from_secs(5),
            Some(&shared),
            || shared.stats.errors.load(Ordering::Relaxed) == 1,
        );
        let f = shared.slot.latest().expect("full frame kept");
        assert_eq!(f.seq, 1);
        assert!(f.rgb.iter().all(|&b| b == 5));
        assert_eq!(shared.stats.frames_decoded.load(Ordering::Relaxed), 1);
        drop(g);
    }

    #[test]
    fn finished_non_looping_file_keeps_the_last_frame_and_does_not_restart() {
        let d = TempDir::new("finished");
        let prog = script(d.path(), "frame 1; frame 2; exit 0");
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn_with(
            fake_spec(
                prog,
                Input::File {
                    path: "x.mp4".into(),
                    looping: false,
                },
            ),
            shared.clone(),
            fast(),
        );
        wait_until("2 frames", Duration::from_secs(5), Some(&shared), || {
            shared.stats.frames_decoded.load(Ordering::Relaxed) == 2
        });
        // Keep touching for a while: no restart, last frame stays.
        let end = Instant::now() + Duration::from_millis(400);
        while Instant::now() < end {
            shared.touch();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shared.stats.starts.load(Ordering::Relaxed), 1);
        assert_eq!(shared.stats.errors.load(Ordering::Relaxed), 0);
        assert_eq!(shared.slot.latest().unwrap().seq, 2);
        // Idle: cleared; shown again: replays.
        wait_until("cleared", Duration::from_secs(5), None, || {
            shared.slot.latest().is_none()
        });
        wait_until("replay", Duration::from_secs(5), Some(&shared), || {
            shared.stats.starts.load(Ordering::Relaxed) == 2
        });
        drop(g);
    }

    #[test]
    fn chatty_stderr_does_not_block_decoding() {
        let d = TempDir::new("stderr");
        let body = "head -c 2000000 /dev/zero | tr '\\000' 'e' >&2; frame 9; exec sleep 30";
        let prog = script(d.path(), body);
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn_with(fake_spec(prog, url()), shared.clone(), fast());
        wait_until("frame", Duration::from_secs(5), Some(&shared), || {
            shared.slot.latest().is_some()
        });
        drop(g);
    }

    #[test]
    fn drop_kills_a_hanging_process_quickly() {
        for hang in ["exec sleep 60", "while :; do sleep 1; done"] {
            let d = TempDir::new("drop");
            let pidf = d.path().join("pid");
            let body = format!("echo $$ > {}; frame 3; {hang}", pidf.display());
            let prog = script(d.path(), &body);
            let shared = Arc::new(Shared::new());
            shared.touch();
            let g = spawn_with(fake_spec(prog, url()), shared.clone(), Timing::default());
            wait_until("frame", Duration::from_secs(5), Some(&shared), || {
                shared.slot.latest().is_some()
            });
            let pid = read_trim(&pidf);
            assert!(alive(&pid));
            let t = Instant::now();
            drop(g);
            assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
            // Killed and reaped (a zombie would still be in /proc).
            assert!(!alive(&pid), "process {pid} still present ({hang})");
        }
    }

    #[test]
    fn missing_program_counts_errors_and_does_not_panic() {
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn_with(
            fake_spec("/nonexistent/ffmpeg".into(), url()),
            shared.clone(),
            fast(),
        );
        wait_until("errors", Duration::from_secs(5), Some(&shared), || {
            shared.stats.errors.load(Ordering::Relaxed) >= 2
        });
        assert!(shared.slot.latest().is_none());
        drop(g);
    }
}

// ---------------------------------------------------------------------------
// Real ffmpeg
// ---------------------------------------------------------------------------

/// These tests need an `ffmpeg` on PATH; CI installs it. They fail (never
/// skip) when it is missing.
#[cfg(target_os = "linux")]
mod real {
    use super::*;
    use std::process::Command;

    fn require_ffmpeg() {
        let ok = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            ok,
            "the video tests need `ffmpeg` on PATH (CI: sudo apt-get install -y ffmpeg)"
        );
    }

    /// Write `secs` seconds of a lavfi source as uncompressed NUT (no codec
    /// dependency). `src` is the lavfi input, e.g. `color=c=red:s=64x32:r=25`.
    fn make_clip(dir: &Path, name: &str, src: &str, secs: f32) -> PathBuf {
        let p = dir.join(name);
        let out = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", src, "-t", &secs.to_string()])
            .args(["-c:v", "rawvideo", "-pix_fmt", "rgb24", "-f", "nut"])
            .arg(&p)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        p
    }

    fn real_spec(path: PathBuf, looping: bool, w: u32, h: u32, fit: Fit) -> DecoderSpec {
        DecoderSpec {
            program: "ffmpeg".into(),
            input: Input::File { path, looping },
            width: w,
            height: h,
            fit,
            fps: 25.0,
        }
    }

    fn first_frame(spec: DecoderSpec) -> Arc<Frame> {
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn(spec, shared.clone());
        wait_until("a frame", Duration::from_secs(15), Some(&shared), || {
            shared.slot.latest().is_some()
        });
        let f = shared.slot.latest().unwrap();
        drop(g);
        f
    }

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * f.width + x) * 3) as usize;
        [f.rgb[i], f.rgb[i + 1], f.rgb[i + 2]]
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= 6)
    }

    const RED: [u8; 3] = [255, 0, 0];
    const BLACK: [u8; 3] = [0, 0, 0];

    #[test]
    fn stretch_fills_the_zone_with_exact_size() {
        require_ffmpeg();
        let d = TempDir::new("stretch");
        let clip = make_clip(d.path(), "red.nut", "color=c=red:s=64x32:r=25", 0.4);
        let f = first_frame(real_spec(clip, true, 32, 16, Fit::Stretch));
        assert_eq!((f.width, f.height, f.rgb.len()), (32, 16, 32 * 16 * 3));
        for (x, y) in [(0, 0), (31, 0), (0, 15), (31, 15), (16, 8)] {
            assert!(near(px(&f, x, y), RED), "({x},{y}) {:?}", px(&f, x, y));
        }
    }

    #[test]
    fn contain_pads_with_black_centred() {
        require_ffmpeg();
        let d = TempDir::new("contain");
        let clip = make_clip(d.path(), "red.nut", "color=c=red:s=64x32:r=25", 0.4);
        // 2:1 into 32x32 -> 32x16 picture, 8 black rows above and below.
        let f = first_frame(real_spec(clip, true, 32, 32, Fit::Contain));
        assert_eq!((f.width, f.height), (32, 32));
        for y in [0, 4, 7, 24, 28, 31] {
            assert!(near(px(&f, 16, y), BLACK), "row {y} {:?}", px(&f, 16, y));
        }
        for y in [8, 16, 23] {
            assert!(near(px(&f, 16, y), RED), "row {y} {:?}", px(&f, 16, y));
        }
    }

    #[test]
    fn cover_crops_and_leaves_no_padding() {
        require_ffmpeg();
        let d = TempDir::new("cover");
        let clip = make_clip(d.path(), "red.nut", "color=c=red:s=64x32:r=25", 0.4);
        let f = first_frame(real_spec(clip, true, 32, 32, Fit::Cover));
        assert_eq!((f.width, f.height), (32, 32));
        for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31), (16, 16)] {
            assert!(near(px(&f, x, y), RED), "({x},{y}) {:?}", px(&f, x, y));
        }
    }

    #[test]
    fn none_pads_small_and_crops_large_without_scaling() {
        require_ffmpeg();
        let d = TempDir::new("none");
        let small = make_clip(d.path(), "s.nut", "color=c=red:s=16x16:r=25", 0.4);
        let f = first_frame(real_spec(small, true, 32, 32, Fit::None));
        assert_eq!((f.width, f.height), (32, 32));
        assert!(near(px(&f, 16, 16), RED));
        assert!(near(px(&f, 0, 0), BLACK));
        assert!(near(px(&f, 31, 31), BLACK));
        assert!(near(px(&f, 7, 16), BLACK), "left of the centred 16 px");
        assert!(near(px(&f, 9, 16), RED));

        let big = make_clip(d.path(), "b.nut", "color=c=red:s=100x80:r=25", 0.4);
        let f = first_frame(real_spec(big, true, 32, 32, Fit::None));
        assert_eq!((f.width, f.height), (32, 32));
        for (x, y) in [(0, 0), (31, 31)] {
            assert!(near(px(&f, x, y), RED));
        }
    }

    #[test]
    fn testsrc_at_an_odd_size() {
        require_ffmpeg();
        let d = TempDir::new("odd");
        let clip = make_clip(d.path(), "t.nut", "testsrc=s=50x30:r=25", 0.4);
        for fit in [Fit::Contain, Fit::Cover, Fit::Stretch, Fit::None] {
            let f = first_frame(real_spec(clip.clone(), true, 21, 13, fit));
            assert_eq!((f.width, f.height, f.rgb.len()), (21, 13, 21 * 13 * 3));
        }
    }

    #[test]
    fn looping_file_keeps_producing_frames_with_one_process() {
        require_ffmpeg();
        let d = TempDir::new("loop");
        // 10 frames at 25 fps = 0.4 s per pass.
        let clip = make_clip(d.path(), "t.nut", "testsrc=s=32x16:r=25", 0.4);
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn(real_spec(clip, true, 32, 16, Fit::Stretch), shared.clone());
        wait_until("two passes", Duration::from_secs(20), Some(&shared), || {
            shared.stats.frames_decoded.load(Ordering::Relaxed) >= 22
        });
        assert_eq!(shared.stats.starts.load(Ordering::Relaxed), 1);
        assert_eq!(shared.stats.errors.load(Ordering::Relaxed), 0);
        drop(g);
    }

    #[test]
    fn non_looping_file_plays_once_and_stops() {
        require_ffmpeg();
        let d = TempDir::new("once");
        let clip = make_clip(d.path(), "t.nut", "testsrc=s=32x16:r=25", 0.4);
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn(real_spec(clip, false, 32, 16, Fit::Stretch), shared.clone());
        wait_until("a pass", Duration::from_secs(20), Some(&shared), || {
            shared.stats.frames_decoded.load(Ordering::Relaxed) >= 10
        });
        let end = Instant::now() + Duration::from_millis(800);
        while Instant::now() < end {
            shared.touch();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shared.stats.frames_decoded.load(Ordering::Relaxed), 10);
        assert_eq!(shared.stats.starts.load(Ordering::Relaxed), 1);
        assert_eq!(shared.stats.errors.load(Ordering::Relaxed), 0);
        assert!(shared.slot.latest().is_some(), "last frame kept");
        drop(g);
    }

    #[test]
    fn unreadable_source_counts_errors() {
        require_ffmpeg();
        let shared = Arc::new(Shared::new());
        shared.touch();
        let g = spawn_with(
            real_spec("/nonexistent/clip.mp4".into(), false, 8, 8, Fit::Stretch),
            shared.clone(),
            fast(),
        );
        wait_until("error", Duration::from_secs(10), Some(&shared), || {
            shared.stats.errors.load(Ordering::Relaxed) >= 1
        });
        assert!(shared.slot.latest().is_none());
        drop(g);
    }
}
