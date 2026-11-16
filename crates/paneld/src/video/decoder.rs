//! The `ffmpeg` subprocess that fills a [`FrameSlot`](super::FrameSlot)
//! (DESIGN.md §9, milestone M7).
//!
//! # Lifecycle
//!
//! One plain `std::thread` (the *supervisor*) per video zone; no tokio.
//!
//! ```text
//!            not wanted                       wanted (touched < IDLE_STOP ago)
//!  Idle ─────────────────────────────────────────────────────────────▶ Running
//!   ▲  (poll every 100 ms; slot cleared on entry)                          │
//!   │                                                                      │
//!   │  not wanted: kill ffmpeg, wait(), clear slot                         │
//!   ├──────────────────────────────────────────────────────────────────────┤
//!   │  EOF / error / bad exit: errors += 1, Backoff 1 s, 2 s, … 30 s ──────┤
//!   │  (back to 1 s after a run that delivered frames for >= 60 s)         │
//!   │  non-looping file played to the end: keep last frame, stay stopped ──┘
//!   drop(DecoderGuard): stop flag + unpark, kill ffmpeg, wait(), join (< 1 s)
//! ```
//!
//! While running, a *reader* thread owns `ffmpeg`'s stdout, reads exactly
//! `width*height*3` bytes per frame and calls [`FrameSlot::put`](super::FrameSlot::put);
//! a *stderr* thread drains stderr. The supervisor owns the `Child` and polls
//! (20 ms) for: stop, no longer wanted, reader finished. That is how a
//! blocked `read` never delays killing the process. A partial frame at EOF is
//! discarded. If `ffmpeg` left a grandchild holding the pipe open, the
//! supervisor stops waiting for the reader after 200 ms and cancels it (it
//! checks a flag before every `put`).
//!
//! **Last frame:** on idle-stop the slot is *cleared* (a returning scene must
//! not flash a stale picture; the widget draws nothing until a fresh frame
//! arrives). On a failure the last frame is *kept* for up to 10 s so a quick
//! reconnect does not flicker, then cleared (a frozen camera image must not
//! pass for live video). A non-looping file that finished keeps its last
//! frame until the zone goes idle, then it is cleared and the file replays on
//! the next show.
//!
//! The render loop never calls into this module except through the slot.
//!
//! # Secrets
//!
//! A source URL can hold `user:pass@` or a query-string key. It is never
//! logged, never in `Debug` output of anything here, and the module says "the
//! video source". `ffmpeg` echoes its input on errors, so every stderr line
//! goes through [`redact`] (everything after `scheme://` up to whitespace
//! becomes `***`, plus an exact-match pass for the configured URL) before it
//! is logged, truncated to 200 bytes and rate-limited (5 lines per 10 s, the
//! rest counted).
//!
//! # `ffmpeg` options (checked against the docs and the local 6.1 binary)
//!
//! Sources: <https://ffmpeg.org/ffmpeg.html> (main options),
//! <https://ffmpeg.org/ffmpeg-protocols.html>,
//! <https://ffmpeg.org/ffmpeg-formats.html>,
//! <https://ffmpeg.org/ffmpeg-filters.html>. Each was also confirmed with
//! `ffmpeg -h full`, `-h filter=…`, `-h demuxer=rtsp`, `-h protocol=http`.
//!
//! - `-nostdin`: "To explicitly disable interaction you need to specify
//!   `-nostdin`" (ffmpeg.html). `-hide_banner`, `-loglevel error`: quiet.
//! - `-re`: local `-h full`: "read input at native frame rate; equivalent to
//!   `-readrate 1`". Files only; live sources pace themselves.
//! - `-stream_loop -1` (input option, before `-i`): "Loop 0 means no loop,
//!   loop -1 means infinite loop" (ffmpeg.html).
//! - `-rtsp_transport tcp` (ffmpeg-protocols.html, RTSP): UDP is lossy on
//!   Wi-Fi/NAT; TCP is reliable and what cameras/go2rtc handle best.
//! - Network timeouts, all in **microseconds**, input options before `-i`:
//!   RTSP `-timeout 10000000` ("Set socket TCP I/O timeout in microseconds",
//!   ffmpeg-protocols.html; `-h demuxer=rtsp` agrees). Before FFmpeg 5.0 this
//!   was `-stimeout` and `-timeout` meant the listen timeout; Debian 12
//!   ships 5.1 and Debian 13 ships 7.1, both after the rename, so `-timeout`
//!   is right for both (NOT verified on a 5.1 binary here). HTTP(S): generic
//!   `-rw_timeout 10000000` ("Maximum time to wait for (network) read/write
//!   operations to complete, in microseconds"). Other schemes get none:
//!   an unknown input option makes `ffmpeg` exit with an error.
//! - `-an`: skip audio ("-vn / -an / -sn / -dn … skip inclusion of … streams").
//! - `-f rawvideo -pix_fmt rgb24 pipe:1`: raw packed RGB888 to stdout.
//! - Filters (ffmpeg-filters.html; option names confirmed with `-h filter=`):
//!   `scale=W:H:force_original_aspect_ratio=decrease|increase`, `pad=w:h:x:y:color=`
//!   (expressions `iw ih ow oh`), `crop=W:H` (x/y default to centred), `fps=N`,
//!   `format=rgb24`. The `,` inside `max(a\,b)` is escaped with `\`.
//!   (The filters page is ~600 kB; the doc fetch tool only returned its first
//!   part, so the filter options were confirmed against the local binary and
//!   by the real-`ffmpeg` tests.)
//!
//! Known limits: source pixel aspect ratio is ignored; for 4:2:0 sources
//! `pad`/`crop` offsets are rounded to even pixels so centring can be 1 px off.
//!
//! # CPU on a Pi
//!
//! `ffmpeg` decodes the full source resolution, so cost follows the *source*,
//! not the zone. A 1080p H.264 camera stream on a Pi 4 costs roughly a core
//! of software decode; use the camera's sub-stream (e.g. 640x360) where
//! possible. `fps=` caps the output only (the decode still runs at the source
//! rate). The idle stop means a video scene in a playlist costs nothing while
//! other scenes show.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{DecoderSpec, Frame, Input, Shared, IDLE_STOP};
use crate::config::Fit;

/// Network I/O timeout handed to `ffmpeg`, in microseconds.
const NET_TIMEOUT_US: &str = "10000000";

/// Keeps the decoder thread alive; dropping it stops the thread and any
/// running `ffmpeg` (within about a second).
pub struct DecoderGuard {
    inner: Option<Inner>,
}

struct Inner {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl DecoderGuard {
    /// A guard that owns nothing (for detached handles).
    pub fn none() -> Self {
        DecoderGuard { inner: None }
    }
}

impl Drop for DecoderGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.stop.store(true, Ordering::SeqCst);
            inner.thread.thread().unpark();
            let _ = inner.thread.join();
        }
    }
}

/// Start the decoder thread for `spec`, writing into `shared`.
pub fn spawn(spec: DecoderSpec, shared: Arc<Shared>) -> DecoderGuard {
    spawn_with(spec, shared, Timing::default())
}

/// Intervals of the supervisor; tests shorten them.
#[derive(Debug, Clone, Copy)]
struct Timing {
    idle_stop: Duration,
    poll: Duration,
    idle_poll: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    /// A run that delivered frames this long resets the backoff.
    reset_after: Duration,
    /// Keep the last frame this long after the last decoded frame on failure.
    stale_after: Duration,
    /// How long to wait for the reader after killing `ffmpeg`.
    reader_grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            idle_stop: IDLE_STOP,
            poll: Duration::from_millis(20),
            idle_poll: Duration::from_millis(100),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
            stale_after: Duration::from_secs(10),
            reader_grace: Duration::from_millis(200),
        }
    }
}

fn spawn_with(spec: DecoderSpec, shared: Arc<Shared>, timing: Timing) -> DecoderGuard {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let built = thread::Builder::new()
        .name("video-decoder".into())
        .spawn(move || supervise(spec, shared, timing, stop2));
    match built {
        Ok(thread) => DecoderGuard {
            inner: Some(Inner { stop, thread }),
        },
        Err(e) => {
            tracing::error!("cannot start the video decoder thread: {e}");
            DecoderGuard::none()
        }
    }
}

/// Next backoff: doubled, capped.
fn next_backoff(cur: Duration, max: Duration) -> Duration {
    (cur * 2).min(max)
}

/// The `ffmpeg` arguments for `spec` (pure; see the module doc for sources).
pub fn ffmpeg_args(spec: &DecoderSpec) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |s: &str| a.push(OsString::from(s));
    push("-nostdin");
    push("-hide_banner");
    push("-loglevel");
    push("error");

    let source: OsString = match &spec.input {
        Input::File { path, looping } => {
            // -re: native frame rate (a file would otherwise decode as fast as
            // possible). -stream_loop is an input option, so it precedes -i.
            push("-re");
            if *looping {
                push("-stream_loop");
                push("-1");
            }
            path.as_os_str().to_owned()
        }
        Input::Url(url) => {
            let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
            match scheme.as_str() {
                "rtsp" | "rtsps" => {
                    push("-rtsp_transport");
                    push("tcp");
                    push("-timeout");
                    push(NET_TIMEOUT_US);
                }
                "http" | "https" => {
                    push("-rw_timeout");
                    push(NET_TIMEOUT_US);
                }
                _ => {}
            }
            OsString::from(url)
        }
    };
    a.push("-i".into());
    a.push(source);

    let mut push = |s: &str| a.push(OsString::from(s));
    push("-an");
    push("-vf");
    push(&filter_chain(spec));
    push("-f");
    push("rawvideo");
    push("-pix_fmt");
    push("rgb24");
    push("pipe:1");
    a
}

/// The `-vf` value: fit, then `fps`, then `format=rgb24`.
fn filter_chain(spec: &DecoderSpec) -> String {
    let (w, h) = (spec.width, spec.height);
    let fit = match spec.fit {
        // Scale down (or up) to fit inside, then pad with black, centred.
        Fit::Contain => format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black"
        ),
        // Scale until it covers, then crop (x, y default to centred).
        Fit::Cover => format!("scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"),
        Fit::Stretch => format!("scale={w}:{h}"),
        // No scaling: pad up to at least the zone size, then crop to it.
        Fit::None => {
            format!("pad=max(iw\\,{w}):max(ih\\,{h}):(ow-iw)/2:(oh-ih)/2:color=black,crop={w}:{h}")
        }
    };
    format!("{fit},fps={},format=rgb24", spec.fps)
}

/// Replace every `scheme://…` (up to whitespace or a quote) with
/// `scheme://***`, and every occurrence of each string in `secrets`
/// with `***`. Used on everything `ffmpeg` prints before it is logged.
pub fn redact(line: &str, secrets: &[&str]) -> String {
    let mut s = line.to_string();
    for sec in secrets.iter().filter(|x| !x.is_empty()) {
        s = s.replace(sec, "***");
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find("://") {
        out.push_str(&rest[..i + 3]);
        let after = &rest[i + 3..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(after.len());
        if end > 0 {
            out.push_str("***");
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Why a run ended.
enum RunEnd {
    /// The guard was dropped.
    Stopped,
    /// The zone is no longer wanted.
    Idle,
    /// A non-looping file played to the end.
    Finished,
    Failed {
        reason: String,
        /// How long frames were flowing.
        good_for: Duration,
    },
}

struct Ctx {
    spec: DecoderSpec,
    shared: Arc<Shared>,
    timing: Timing,
    stop: Arc<AtomicBool>,
    /// Monotonic across restarts.
    seq: Arc<AtomicU64>,
    /// Redaction secrets (the URL).
    secrets: Vec<String>,
}

impl Ctx {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    fn wanted(&self) -> bool {
        self.shared.wanted(self.timing.idle_stop)
    }
}

fn supervise(spec: DecoderSpec, shared: Arc<Shared>, timing: Timing, stop: Arc<AtomicBool>) {
    let secrets = match &spec.input {
        Input::Url(u) => vec![u.clone()],
        Input::File { .. } => vec![],
    };
    let ctx = Ctx {
        spec,
        shared,
        timing,
        stop,
        seq: Arc::new(AtomicU64::new(0)),
        secrets,
    };
    let mut backoff = timing.initial_backoff;
    // Frames are in the slot that we put there (so idle must clear it).
    let mut dirty = false;
    let mut finished = false;
    while !ctx.stopped() {
        if !ctx.wanted() {
            if dirty || finished {
                ctx.shared.slot.clear();
                dirty = false;
                finished = false;
                backoff = timing.initial_backoff;
            }
            thread::park_timeout(timing.idle_poll);
            continue;
        }
        if finished {
            thread::park_timeout(timing.idle_poll);
            continue;
        }
        let (end, last_frame) = run_once(&ctx);
        dirty |= last_frame.is_some();
        match end {
            RunEnd::Stopped => break,
            RunEnd::Idle => continue, // top of the loop clears the slot
            RunEnd::Finished => {
                finished = true;
            }
            RunEnd::Failed { reason, good_for } => {
                ctx.shared.stats.errors.fetch_add(1, Ordering::Relaxed);
                if good_for >= timing.reset_after {
                    backoff = timing.initial_backoff;
                }
                tracing::warn!(
                    "video decoder stopped ({reason}); restarting in {:.1}s",
                    backoff.as_secs_f32()
                );
                let deadline = Instant::now() + backoff;
                while Instant::now() < deadline && !ctx.stopped() && ctx.wanted() {
                    if dirty && last_frame.is_some_and(|t| t.elapsed() > timing.stale_after) {
                        ctx.shared.slot.clear();
                        dirty = false;
                    }
                    thread::park_timeout(timing.poll.min(deadline - Instant::now()));
                }
                backoff = next_backoff(backoff, timing.max_backoff);
            }
        }
    }
}

/// Frame progress shared with the reader thread.
#[derive(Default)]
struct Progress {
    /// Millis since `t0` + 1 of the first / last frame; 0 = none.
    first_ms: AtomicU64,
    last_ms: AtomicU64,
}

/// Run `ffmpeg` once. Returns why it ended and when the last frame arrived.
fn run_once(ctx: &Ctx) -> (RunEnd, Option<Instant>) {
    let t0 = Instant::now();
    ctx.shared.stats.starts.fetch_add(1, Ordering::Relaxed);
    let mut child = match Command::new(&ctx.spec.program)
        .args(ffmpeg_args(&ctx.spec))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                RunEnd::Failed {
                    reason: format!("cannot start the decoder program: {}", e.kind()),
                    good_for: Duration::ZERO,
                },
                None,
            )
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Progress::default());
    let reader = {
        let (shared, seq, cancel, progress) = (
            ctx.shared.clone(),
            ctx.seq.clone(),
            cancel.clone(),
            progress.clone(),
        );
        let (w, h) = (ctx.spec.width, ctx.spec.height);
        thread::Builder::new()
            .name("video-reader".into())
            .spawn(move || read_frames(stdout, w, h, &shared, &seq, &cancel, &progress, t0))
    };
    let reader = match reader {
        Ok(r) => r,
        Err(_) => {
            kill_and_reap(&mut child);
            return (
                RunEnd::Failed {
                    reason: "cannot start the reader thread".into(),
                    good_for: Duration::ZERO,
                },
                None,
            );
        }
    };
    let secrets = ctx.secrets.clone();
    // Detached: ends when the pipe closes.
    let _ = thread::Builder::new()
        .name("video-stderr".into())
        .spawn(move || drain_stderr(stderr, &secrets));

    // Supervise.
    let early = loop {
        if ctx.stopped() {
            break Some(RunEnd::Stopped);
        }
        if !ctx.wanted() {
            break Some(RunEnd::Idle);
        }
        if reader.is_finished() {
            break None;
        }
        thread::park_timeout(ctx.timing.poll);
    };

    let last_frame = |p: &Progress| {
        let ms = p.last_ms.load(Ordering::Relaxed);
        (ms != 0).then(|| t0 + Duration::from_millis(ms - 1))
    };
    let good_for = |p: &Progress| {
        let (f, l) = (
            p.first_ms.load(Ordering::Relaxed),
            p.last_ms.load(Ordering::Relaxed),
        );
        if f == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(l - f)
        }
    };

    if let Some(end) = early {
        kill_and_reap(&mut child);
        cancel.store(true, Ordering::SeqCst);
        wait_finished(&reader, ctx.timing.reader_grace);
        return (end, last_frame(&progress));
    }

    // The reader hit EOF or an error: let ffmpeg exit by itself briefly to
    // learn its status, then make sure it is gone.
    let status = wait_exit(&mut child, Duration::from_millis(500));
    kill_and_reap(&mut child);
    let clean_eof = reader.join().unwrap_or(false);
    let ok_exit = status.is_some_and(|s| s.success());
    let last = last_frame(&progress);
    let finished =
        matches!(ctx.spec.input, Input::File { looping: false, .. }) && clean_eof && ok_exit;
    if finished {
        return (RunEnd::Finished, last);
    }
    let reason = if !clean_eof {
        "partial frame or read error".to_string()
    } else if let Some(s) = status.filter(|s| !s.success()) {
        format!("ffmpeg exited with {s}")
    } else {
        "the video source ended".to_string()
    };
    (
        RunEnd::Failed {
            reason,
            good_for: good_for(&progress),
        },
        last,
    )
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Poll for a natural exit up to `max`.
fn wait_exit(child: &mut Child, max: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + max;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => return None,
        }
    }
}

fn wait_finished<T>(h: &JoinHandle<T>, max: Duration) {
    let deadline = Instant::now() + max;
    while !h.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Read exactly one frame into `buf`. `Ok(true)` full frame, `Ok(false)` clean
/// EOF before any byte of it; a partial frame is an `UnexpectedEof` error.
fn read_frame(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Reader thread body. Returns true when it ended on a clean frame boundary.
#[allow(clippy::too_many_arguments)]
fn read_frames(
    mut stdout: impl Read,
    width: u32,
    height: u32,
    shared: &Shared,
    seq: &AtomicU64,
    cancel: &AtomicBool,
    progress: &Progress,
    t0: Instant,
) -> bool {
    let len = width as usize * height as usize * 3;
    loop {
        // `Frame` owns its bytes, so a new buffer per frame (zone-sized, small).
        let mut buf = vec![0u8; len];
        match read_frame(&mut stdout, &mut buf) {
            Ok(true) => {}
            Ok(false) => return true,
            Err(_) => return false,
        }
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        let ms = t0.elapsed().as_millis() as u64 + 1;
        let _ = progress
            .first_ms
            .compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed);
        progress.last_ms.store(ms, Ordering::Relaxed);
        shared.slot.put(Frame {
            width,
            height,
            rgb: buf,
            seq: seq.fetch_add(1, Ordering::Relaxed) + 1,
        });
        shared.stats.frames_decoded.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drain stderr to the end; log redacted, truncated, rate-limited lines.
fn drain_stderr(mut stderr: impl Read, secrets: &[String]) {
    const MAX_LINE: usize = 200;
    const BURST: u32 = 5;
    const WINDOW: Duration = Duration::from_secs(10);
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    let mut line: Vec<u8> = Vec::new();
    let mut window_start = Instant::now();
    let mut logged = 0u32;
    let mut suppressed = 0u64;
    let mut chunk = [0u8; 4096];
    let mut emit = |line: &mut Vec<u8>| {
        if line.is_empty() {
            return;
        }
        if window_start.elapsed() > WINDOW {
            if suppressed > 0 {
                tracing::warn!("ffmpeg: {suppressed} more message(s) suppressed");
            }
            window_start = Instant::now();
            logged = 0;
            suppressed = 0;
        }
        if logged < BURST {
            logged += 1;
            let text = String::from_utf8_lossy(line);
            // Redact before truncating so a cut cannot split a secret.
            let mut t = redact(&text, &secrets);
            if t.len() > MAX_LINE {
                let mut cut = MAX_LINE;
                while !t.is_char_boundary(cut) {
                    cut -= 1;
                }
                t.truncate(cut);
                t.push_str("...");
            }
            tracing::warn!("ffmpeg: {t}");
        } else {
            suppressed += 1;
        }
        line.clear();
    };
    loop {
        let n = match stderr.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' || b == b'\r' {
                emit(&mut line);
            } else if line.len() < 4 * MAX_LINE {
                // Bounded: the rest of an overlong line is dropped.
                line.push(b);
            }
        }
    }
    emit(&mut line);
}

#[cfg(test)]
mod tests;
