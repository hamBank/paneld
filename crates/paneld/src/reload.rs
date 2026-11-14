//! Config hot reload (DESIGN.md §7.3).
//!
//! Three pieces, none of which runs on the render thread:
//!
//! - [`ConfigWatcher`] decides *when* to reload: it polls the file and
//!   applies a debounce rule.
//! - [`ReloadCtx::prepare`] decides *whether* a new file is acceptable and
//!   builds the [`SceneSet`] (config parse and validation, image decoding,
//!   `Widget::prepare`). It is a plain function of the new bytes so tests
//!   need no files or tasks.
//! - [`spawn`] glues them together in a tokio task and hands accepted scene
//!   sets to the render loop over a channel; the render loop swaps them in
//!   with a non-blocking check at the top of a frame.
//!
//! # Change detection
//! Polling, not `notify`: every [`POLL_INTERVAL`] the whole file is read and
//! hashed (a config is a few KB). Reading the content makes the decision
//! independent of mtime granularity, editors that write in place or replace
//! the file, atomic renames and network filesystems, and needs no extra
//! dependency or per-platform backend.
//!
//! **Debounce rule:** a change is acted on once the file has the *same new
//! content on two consecutive polls*. A half-written file is therefore not
//! parsed unless the writer pauses for more than a poll interval, and a
//! touch that leaves the content identical never reloads. Latency is two
//! poll intervals. A file that is briefly missing (rename in progress) is
//! skipped. SIGHUP (unix) bypasses the debounce and reloads immediately.
//!
//! Content that failed to load is not retried until it changes again (or
//! SIGHUP is sent).
//!
//! # Limitations
//! - Image files referenced by the config are not watched. Edit the config
//!   (any change) or send SIGHUP to pick up a changed image.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::Notify;

use crate::config::Config;
use crate::engine::SceneSet;

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Reload counters (exported as `paneld_config_reload_errors_total` etc. in M5).
#[derive(Debug, Default)]
pub struct ReloadStats {
    /// Reloads that were accepted and handed to the render loop.
    pub reloads: AtomicU64,
    /// Reloads rejected for any reason; the old config kept running.
    pub reload_errors: AtomicU64,
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Polls a file for content changes. Pure bookkeeping plus a file read, so
/// it can be driven step by step in tests.
#[derive(Debug)]
pub struct ConfigWatcher {
    path: PathBuf,
    /// Hash of the content last handed out (or present at start-up).
    applied: Option<u64>,
    /// Hash seen on the previous poll if it differed from `applied`.
    pending: Option<u64>,
}

impl ConfigWatcher {
    /// Start watching; the current content counts as already applied.
    /// Start from whatever the file holds now.
    pub fn new(path: &Path) -> Self {
        Self::with_baseline(path, std::fs::read(path).ok().as_deref())
    }

    /// Start from `loaded`, the content that is actually running. An edit
    /// made after it was read (even before the watcher exists) is then seen
    /// as a change.
    pub fn with_baseline(path: &Path, loaded: Option<&[u8]>) -> Self {
        ConfigWatcher {
            path: path.to_path_buf(),
            applied: loaded.map(hash),
            pending: None,
        }
    }

    /// One poll. Returns the new content once it has been stable for two
    /// polls in a row (see the module docs).
    pub fn poll(&mut self) -> Option<Vec<u8>> {
        // Unreadable (e.g. mid-rename): skip, keep the pending state.
        let bytes = std::fs::read(&self.path).ok()?;
        let h = hash(&bytes);
        if Some(h) == self.applied {
            self.pending = None;
            return None;
        }
        if self.pending == Some(h) {
            self.pending = None;
            self.applied = Some(h);
            Some(bytes)
        } else {
            self.pending = Some(h);
            None
        }
    }

    /// Read the file now, regardless of whether it changed (SIGHUP).
    pub fn force(&mut self) -> std::io::Result<Vec<u8>> {
        let bytes = std::fs::read(&self.path)?;
        self.applied = Some(hash(&bytes));
        self.pending = None;
        Ok(bytes)
    }
}

/// The settings that cannot change without a restart, captured from the
/// config the process started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartOnly {
    pub canvas: (u32, u32),
    pub fps: u32,
    pub http_bind: SocketAddr,
    /// `Debug` rendering of the `[[output]]` list. Outputs (preview and
    /// Colorlight sinks, including gamma, brightness and colour order) are
    /// set up once at start-up.
    outputs: String,
    /// `Debug` rendering of `[sources]`. Sources are started once at
    /// start-up; restarting just the changed ones on reload is a follow-up
    /// (DESIGN.md §7.3).
    sources: String,
}

impl RestartOnly {
    pub fn from_config(cfg: &Config) -> Self {
        RestartOnly {
            canvas: (cfg.canvas.width, cfg.canvas.height),
            fps: cfg.canvas.fps,
            http_bind: cfg.http.bind,
            outputs: format!("{:?}", cfg.outputs),
            sources: format!("{:?}", cfg.sources),
        }
    }

    /// Names of the restart-only settings that differ in `new`.
    pub fn changed(&self, new: &RestartOnly) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.fps != new.fps {
            v.push("canvas.fps");
        }
        if self.http_bind != new.http_bind {
            v.push("http.bind");
        }
        if self.outputs != new.outputs {
            v.push("[[output]]");
        }
        if self.sources != new.sources {
            v.push("[sources]");
        }
        v
    }
}

/// A prepared reload: the scene set plus warnings to log.
pub struct Prepared {
    pub set: SceneSet,
    pub warnings: Vec<String>,
}

/// What the reload pipeline needs to know about the running daemon.
#[derive(Debug, Clone)]
pub struct ReloadCtx {
    pub path: PathBuf,
    pub running: RestartOnly,
    /// `run --scene X`: keep showing X.
    pub only: Option<String>,
    /// The config text that is running, if known. The watcher starts from
    /// it, so no edit is missed between the initial load and the first poll.
    pub loaded: Option<Vec<u8>>,
}

impl ReloadCtx {
    pub fn new(path: &Path, running_cfg: &Config, only: Option<&str>) -> Self {
        ReloadCtx {
            path: path.to_path_buf(),
            running: RestartOnly::from_config(running_cfg),
            only: only.map(str::to_owned),
            loaded: None,
        }
    }

    /// Record the exact text the running config was loaded from.
    pub fn loaded_from(mut self, text: &str) -> Self {
        self.loaded = Some(text.as_bytes().to_vec());
        self
    }

    /// Turn the new file content into a scene set, or say why not. Blocking
    /// (decodes images); never call it from the render thread.
    pub fn prepare(&self, bytes: &[u8]) -> anyhow::Result<Prepared> {
        let text = std::str::from_utf8(bytes).context("config is not valid UTF-8")?;
        let base = self.path.parent().unwrap_or(Path::new(""));
        let cfg = Config::parse_in(text, base)
            .with_context(|| format!("loading config {}", self.path.display()))?;

        let new = RestartOnly::from_config(&cfg);
        anyhow::ensure!(
            new.canvas == self.running.canvas,
            "canvas size changed from {}x{} to {}x{}; restart paneld to change it",
            self.running.canvas.0,
            self.running.canvas.1,
            new.canvas.0,
            new.canvas.1
        );
        let set = SceneSet::from_config(&cfg, self.only.as_deref())?;

        let mut warnings = Vec::new();
        let changed = self.running.changed(&new);
        if !changed.is_empty() {
            warnings.push(format!(
                "{} changed but only takes effect after a restart; scene and playlist \
                 changes were applied",
                changed.join(", ")
            ));
        }
        Ok(Prepared { set, warnings })
    }

    /// One watcher step: poll (or force), and prepare if there is new content.
    /// `None` means nothing to do.
    fn step(&self, w: &mut ConfigWatcher, forced: bool) -> Option<anyhow::Result<Prepared>> {
        let bytes = if forced {
            match w.force() {
                Ok(b) => b,
                Err(e) => {
                    return Some(Err(anyhow::Error::new(e)
                        .context(format!("reading config {}", self.path.display()))))
                }
            }
        } else {
            w.poll()?
        };
        Some(self.prepare(&bytes))
    }
}

/// The watcher task. Polls every `interval` and on every `force` wake-up
/// (SIGHUP). File reads and config parsing run on the blocking pool.
/// Returns when the render loop's receiver is gone.
pub async fn watch(
    ctx: ReloadCtx,
    watcher: ConfigWatcher,
    tx: Sender<SceneSet>,
    stats: Arc<ReloadStats>,
    interval: Duration,
    force: Arc<Notify>,
) {
    let ctx = Arc::new(ctx);
    let watcher = Arc::new(Mutex::new(watcher));
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let forced = tokio::select! {
            _ = tick.tick() => false,
            () = force.notified() => {
                tracing::info!("reloading config on SIGHUP");
                true
            }
        };
        let (c, w) = (ctx.clone(), watcher.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
            c.step(&mut w, forced)
        })
        .await;
        match outcome {
            Ok(None) => {}
            Ok(Some(Ok(p))) => {
                for w in &p.warnings {
                    tracing::warn!("{w}");
                }
                if tx.send(p.set).is_err() {
                    return;
                }
                stats.reloads.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(Err(e))) => {
                stats.reload_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!("config reload failed, keeping the running config: {e:#}");
            }
            Err(e) => {
                stats.reload_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!("config reload task failed: {e}");
            }
        }
    }
}

/// Start watching on the current tokio runtime, including SIGHUP on unix.
pub fn spawn(ctx: ReloadCtx, tx: Sender<SceneSet>, stats: Arc<ReloadStats>) {
    let force = Arc::new(Notify::new());
    #[cfg(unix)]
    {
        let force = force.clone();
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(mut hup) => {
                tokio::spawn(async move {
                    while hup.recv().await.is_some() {
                        force.notify_one();
                    }
                });
            }
            Err(e) => tracing::error!(error = %e, "listening for SIGHUP"),
        }
    }
    let watcher = match &ctx.loaded {
        Some(bytes) => ConfigWatcher::with_baseline(&ctx.path, Some(bytes)),
        None => ConfigWatcher::new(&ctx.path),
    };
    tokio::spawn(watch(ctx, watcher, tx, stats, POLL_INTERVAL, force));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Unique temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "paneld-reload-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cfg_text(w: u32, h: u32, fps: u32, scenes: &[&str]) -> String {
        let mut s = format!("[canvas]\nwidth = {w}\nheight = {h}\nfps = {fps}\n");
        for n in scenes {
            s += &format!("[[scene]]\nname = \"{n}\"\n");
        }
        s
    }

    fn ctx(text: &str, only: Option<&str>) -> ReloadCtx {
        let cfg = Config::parse(text).unwrap();
        ReloadCtx::new(Path::new("paneld.toml"), &cfg, only)
    }

    fn err(r: anyhow::Result<Prepared>) -> String {
        format!("{:#}", r.err().expect("expected an error"))
    }

    // ---- pipeline ----

    #[test]
    fn accepts_a_changed_scene_list() {
        let c = ctx(&cfg_text(16, 8, 30, &["a"]), None);
        let p = c
            .prepare(cfg_text(16, 8, 30, &["a", "b"]).as_bytes())
            .unwrap();
        assert_eq!(p.set.scenes.len(), 2);
        assert_eq!(p.set.scheduler.len(), 2);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn rejects_parse_validation_and_media_errors() {
        let c = ctx(&cfg_text(16, 8, 30, &["a"]), None);
        assert!(err(c.prepare(b"[canvas\nwidth =")).contains("loading config"));
        assert!(err(c.prepare(b"this is not toml = = =")).contains("loading config"));
        // Validation: fps out of range, and an unknown key.
        assert!(err(c.prepare(cfg_text(16, 8, 500, &["a"]).as_bytes())).contains("fps"));
        let unknown = cfg_text(16, 8, 30, &["a"]) + "bogus = 1\n";
        assert!(err(c.prepare(unknown.as_bytes())).contains("bogus"));
        // Media load failure.
        let img = cfg_text(16, 8, 30, &[]).to_string()
            + "[[scene]]\nname = \"p\"\n[[scene.zone]]\nrect = { x = 0, y = 0, w = 8, h = 8 }\n\
               widget = { type = \"image\", path = \"does-not-exist.png\" }\n";
        assert!(err(c.prepare(img.as_bytes())).contains("cannot load"));
        assert!(err(c.prepare(&[0xff, 0xfe, 0x00])).contains("UTF-8"));
    }

    #[test]
    fn rejects_a_canvas_size_change() {
        let c = ctx(&cfg_text(16, 8, 30, &["a"]), None);
        let e = err(c.prepare(cfg_text(32, 8, 30, &["a"]).as_bytes()));
        assert!(e.contains("canvas size changed from 16x8 to 32x8"), "{e}");
        assert!(e.contains("restart"), "{e}");
    }

    #[test]
    fn restart_only_changes_warn_but_apply() {
        let c = ctx(&cfg_text(16, 8, 30, &["a"]), None);
        let p = c
            .prepare(cfg_text(16, 8, 25, &["a", "b"]).as_bytes())
            .unwrap();
        assert_eq!(p.set.scenes.len(), 2);
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("canvas.fps"));
        assert!(p.warnings[0].contains("restart"));

        let new = cfg_text(16, 8, 30, &["a"])
            + "[http]\nbind = \"127.0.0.1:9999\"\n[[output]]\ntype = \"preview\"\n";
        let p = c.prepare(new.as_bytes()).unwrap();
        assert!(p.warnings[0].contains("http.bind"));
        assert!(p.warnings[0].contains("[[output]]"));
        assert!(!p.warnings[0].contains("canvas.fps"));
    }

    #[test]
    fn single_scene_mode_survives_and_rejects_a_missing_scene() {
        let c = ctx(&cfg_text(16, 8, 30, &["a", "b"]), Some("b"));
        let p = c
            .prepare(cfg_text(16, 8, 30, &["a", "b", "c"]).as_bytes())
            .unwrap();
        assert_eq!(p.set.scenes.len(), 1);
        assert_eq!(p.set.scenes[0].name(), "b");
        let e = err(c.prepare(cfg_text(16, 8, 30, &["a", "c"]).as_bytes()));
        assert!(e.contains("no scene named \"b\""), "{e}");
    }

    // ---- watcher ----

    #[test]
    fn watcher_needs_two_identical_polls_and_ignores_a_touch() {
        let dir = TempDir::new();
        let f = dir.file("c.toml");
        std::fs::write(&f, "one").unwrap();
        let mut w = ConfigWatcher::new(&f);
        assert_eq!(w.poll(), None);

        // Same content rewritten (new mtime): not a change.
        std::fs::write(&f, "one").unwrap();
        assert_eq!(w.poll(), None);
        assert_eq!(w.poll(), None);

        // A change is held back for one poll; a further edit restarts the wait.
        std::fs::write(&f, "tw").unwrap();
        assert_eq!(w.poll(), None);
        std::fs::write(&f, "two").unwrap();
        assert_eq!(w.poll(), None);
        assert_eq!(w.poll(), Some(b"two".to_vec()));
        assert_eq!(w.poll(), None);

        // Going back to the previous content is a change too.
        std::fs::write(&f, "one").unwrap();
        assert_eq!(w.poll(), None);
        assert_eq!(w.poll(), Some(b"one".to_vec()));
    }

    #[test]
    fn watcher_with_baseline_sees_an_edit_made_before_it_existed() {
        let dir = TempDir::new();
        let f = dir.file("c.toml");
        // The daemon loaded "one"; the file was edited before the watcher
        // started. Starting from the loaded text, the edit is not lost.
        std::fs::write(&f, "two").unwrap();
        let mut w = ConfigWatcher::with_baseline(&f, Some(b"one"));
        assert_eq!(w.poll(), None);
        assert_eq!(w.poll(), Some(b"two".to_vec()));

        // Baseline equal to the file: nothing to do.
        let mut w = ConfigWatcher::with_baseline(&f, Some(b"two"));
        assert_eq!(w.poll(), None);
        assert_eq!(w.poll(), None);
    }

    #[test]
    fn watcher_sees_an_atomic_rename_replace_and_a_missing_file() {
        let dir = TempDir::new();
        let f = dir.file("c.toml");
        std::fs::write(&f, "one").unwrap();
        let mut w = ConfigWatcher::new(&f);

        let tmp = dir.file("c.toml.tmp");
        std::fs::write(&tmp, "two").unwrap();
        std::fs::rename(&tmp, &f).unwrap();
        assert_eq!(w.poll(), None);
        assert_eq!(w.poll(), Some(b"two".to_vec()));

        // Briefly missing, then replaced with the same new content: the
        // pending state survives the gap.
        std::fs::write(&f, "three").unwrap();
        assert_eq!(w.poll(), None);
        std::fs::remove_file(&f).unwrap();
        assert_eq!(w.poll(), None);
        std::fs::write(&f, "three").unwrap();
        assert_eq!(w.poll(), Some(b"three".to_vec()));
    }

    #[test]
    fn watcher_force_bypasses_debounce_and_change_check() {
        let dir = TempDir::new();
        let f = dir.file("c.toml");
        std::fs::write(&f, "one").unwrap();
        let mut w = ConfigWatcher::new(&f);
        assert_eq!(w.force().unwrap(), b"one".to_vec());
        std::fs::write(&f, "two").unwrap();
        assert_eq!(w.force().unwrap(), b"two".to_vec());
        // Forced content counts as applied.
        assert_eq!(w.poll(), None);
        std::fs::remove_file(&f).unwrap();
        assert!(w.force().is_err());
    }

    // ---- the task ----

    #[test]
    fn watch_task_reloads_counts_and_keeps_going_after_errors() {
        let dir = TempDir::new();
        let f = dir.file("c.toml");
        let first = cfg_text(16, 8, 30, &["a"]);
        std::fs::write(&f, &first).unwrap();
        let c = ReloadCtx::new(&f, &Config::parse(&first).unwrap(), None);
        let watcher = ConfigWatcher::new(&f);
        let (tx, rx) = std::sync::mpsc::channel();
        let stats = Arc::new(ReloadStats::default());
        let force = Arc::new(Notify::new());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let task = tokio::spawn(watch(
                c,
                watcher,
                tx,
                stats.clone(),
                Duration::from_millis(5),
                force.clone(),
            ));
            // Broken file: counted, nothing delivered.
            std::fs::write(&f, "not = [valid").unwrap();
            for _ in 0..2000 {
                if stats.reload_errors.load(Ordering::Relaxed) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(stats.reload_errors.load(Ordering::Relaxed), 1);
            assert!(rx.try_recv().is_err());
            // Fixed file: delivered.
            std::fs::write(&f, cfg_text(16, 8, 30, &["a", "b"])).unwrap();
            for _ in 0..2000 {
                if stats.reloads.load(Ordering::Relaxed) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(stats.reloads.load(Ordering::Relaxed), 1);
            assert_eq!(rx.try_recv().unwrap().scenes.len(), 2);
            // Forced reload (SIGHUP path) of unchanged content still reloads.
            force.notify_one();
            for _ in 0..2000 {
                if stats.reloads.load(Ordering::Relaxed) > 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(stats.reloads.load(Ordering::Relaxed), 2);
            // The task ends when the render side goes away.
            drop(rx);
            force.notify_one();
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("task exits")
                .unwrap();
        });
    }
}
