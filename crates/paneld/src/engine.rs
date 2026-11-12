//! The render loop (DESIGN.md §5.1): a dedicated thread ticking at a fixed
//! rate. It only reads store snapshots and never blocks on I/O. Each frame
//! is published as an `Arc<Canvas>` on a `watch` channel, which behaves like
//! a capacity-1 channel where the latest frame wins.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Local;
use tokio::sync::watch;

use anyhow::Context;

use crate::canvas::Canvas;
use crate::config::Config;
use crate::scene::Scene;
use crate::scheduler::Scheduler;
use crate::store::Store;
use crate::widgets::FrameCtx;

pub type FrameSender = watch::Sender<Arc<Canvas>>;
pub type FrameReceiver = watch::Receiver<Arc<Canvas>>;

/// Counters shared with the rest of the daemon (exported as metrics in M5).
#[derive(Debug, Default)]
pub struct RenderStats {
    pub frames_rendered: AtomicU64,
    pub frames_skipped: AtomicU64,
}

/// Everything a config reload replaces: the scenes (with prepared widgets)
/// and the playlist over them. Built off the render thread (see
/// `crate::reload`) and swapped in whole.
pub struct SceneSet {
    /// Every scene, each with its own widget state. The scheduler's indices
    /// refer to this list.
    pub scenes: Vec<Scene>,
    pub scheduler: Scheduler,
}

impl SceneSet {
    /// Build the playlist from config. With `only`, that scene is shown
    /// permanently and its condition is ignored: asking for a scene by name
    /// (`run --scene`) means "show me this", e.g. to check a layout before its
    /// data source exists. Otherwise all scenes rotate in config order.
    pub fn from_config(cfg: &Config, only: Option<&str>) -> anyhow::Result<Self> {
        let indices: Vec<usize> = match only {
            Some(name) => {
                let i = cfg
                    .scenes
                    .iter()
                    .position(|s| s.name == name)
                    .with_context(|| {
                        let names: Vec<_> = cfg.scenes.iter().map(|s| s.name.as_str()).collect();
                        format!("no scene named {name:?} (have: {})", names.join(", "))
                    })?;
                vec![i]
            }
            None => (0..cfg.scenes.len()).collect(),
        };
        let mut scheduler = Scheduler::from_config(cfg, &indices)?;
        if only.is_some() {
            scheduler.ignore_conditions();
        }
        Ok(SceneSet {
            scenes: indices
                .iter()
                .map(|&i| Scene::from_config(&cfg.scenes[i]))
                .collect(),
            scheduler,
        })
    }
}

pub struct RenderLoop {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Every scene, each with its own widget state. The scheduler's indices
    /// refer to this list.
    pub scenes: Vec<Scene>,
    pub scheduler: Scheduler,
    pub store: Store,
    /// Prepared scene sets from the config reloader. Polled without blocking
    /// at the top of every frame.
    pub reload: Option<Receiver<SceneSet>>,
}

/// Handle to a running render loop. Dropping it does not stop the loop;
/// call [`RenderHandle::stop`].
pub struct RenderHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<RenderStats>,
}

impl RenderHandle {
    /// Ask the loop to stop and wait for it.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                tracing::error!("render thread panicked");
            }
        }
    }
}

/// Make the frame channel, starting with a black frame.
pub fn frame_channel(width: u32, height: u32) -> (FrameSender, FrameReceiver) {
    watch::channel(Arc::new(Canvas::new(width, height)))
}

impl RenderLoop {
    /// Build the loop from config; see [`SceneSet::from_config`] for `only`.
    pub fn from_config(cfg: &Config, only: Option<&str>, store: Store) -> anyhow::Result<Self> {
        let set = SceneSet::from_config(cfg, only)?;
        Ok(RenderLoop {
            width: cfg.canvas.width,
            height: cfg.canvas.height,
            fps: cfg.canvas.fps,
            scenes: set.scenes,
            scheduler: set.scheduler,
            store,
            reload: None,
        })
    }

    /// Take prepared scene sets from `rx` (config hot reload).
    pub fn with_reload(mut self, rx: Receiver<SceneSet>) -> Self {
        self.reload = Some(rx);
        self
    }

    /// Non-blocking check for a reloaded scene set; swaps in the newest one
    /// if several are queued. Returns whether a swap happened. The new
    /// playlist starts fresh at its first eligible scene.
    fn apply_reload(&mut self) -> bool {
        let Some(rx) = &self.reload else {
            return false;
        };
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(set) => latest = Some(set),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.reload = None;
                    break;
                }
            }
        }
        let Some(set) = latest else { return false };
        self.scenes = set.scenes;
        self.scheduler = set.scheduler;
        tracing::info!("config reloaded: {} scenes", self.scenes.len());
        true
    }

    pub fn spawn(self, tx: FrameSender) -> std::io::Result<RenderHandle> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(RenderStats::default());
        let thread = std::thread::Builder::new().name("render".into()).spawn({
            let shutdown = shutdown.clone();
            let stats = stats.clone();
            move || self.run(&tx, &shutdown, &stats)
        })?;
        Ok(RenderHandle {
            shutdown,
            thread: Some(thread),
            stats,
        })
    }

    fn run(mut self, tx: &FrameSender, shutdown: &AtomicBool, stats: &RenderStats) {
        let period = Duration::from_secs(1) / self.fps.max(1);
        let mut next = Instant::now();
        let mut frame = 0u64;
        // Scene shown last frame; `None` means black (nothing eligible). The
        // outer Option is "no frame yet", so the first frame always logs.
        let mut shown: Option<Option<usize>> = None;
        tracing::info!(
            fps = self.fps,
            scenes = self.scenes.len(),
            "render loop started"
        );

        while !shutdown.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }

            if self.apply_reload() {
                // Log the first scene of the new playlist even if its index
                // and name match the old one.
                shown = None;
            }

            let snapshot = self.store.snapshot();
            let active = self.scheduler.tick(Instant::now(), &snapshot);
            let current = active.map(|a| a.index);
            if shown != Some(current) {
                match current {
                    Some(i) => tracing::info!(scene = self.scenes[i].name(), "scene changed"),
                    None => tracing::info!("no scene eligible, showing black"),
                }
                shown = Some(current);
            }

            let mut canvas = Canvas::new(self.width, self.height);
            if let Some(a) = active {
                let ctx = FrameCtx {
                    now: Local::now().fixed_offset(),
                    scene_elapsed: a.elapsed,
                    frame,
                };
                self.scenes[a.index].render(&ctx, &snapshot, &mut canvas);
            }
            // `send_replace` never blocks and works with no receivers.
            tx.send_replace(Arc::new(canvas));
            frame += 1;
            stats.frames_rendered.fetch_add(1, Ordering::Relaxed);

            next += period;
            let now = Instant::now();
            if now > next {
                // Overran: skip the missed ticks instead of queueing them up.
                let behind = now - next;
                let missed = (behind.as_nanos() / period.as_nanos()) as u64 + 1;
                stats.frames_skipped.fetch_add(missed, Ordering::Relaxed);
                next += period * missed as u32;
            }
        }
        tracing::info!(frames = frame, "render loop stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn produces_frames_and_stops() {
        let cfg = Config::parse(
            r##"
            [canvas]
            width = 16
            height = 8
            fps = 60
            [[scene]]
            name = "s"
              [[scene.zone]]
              rect = { x = 0, y = 0, w = 16, h = 8 }
              background = "#ff0000"
              widget = { type = "text", text = "" }
            "##,
        )
        .unwrap();
        let (tx, mut rx) = frame_channel(16, 8);
        let handle = RenderLoop {
            width: 16,
            height: 8,
            fps: 60,
            scenes: vec![Scene::from_config(&cfg.scenes[0])],
            scheduler: Scheduler::from_config(&cfg, &[0]).unwrap(),
            store: Store::new(),
            reload: None,
        }
        .spawn(tx)
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.changed())
                .await
                .expect("a frame within 5s")
                .unwrap();
        });
        let frame = rx.borrow().clone();
        assert_eq!(
            frame.pixel(3, 3),
            Some(embedded_graphics::pixelcolor::Rgb888::new(255, 0, 0))
        );
        let stats = handle.stats.clone();
        handle.stop();
        assert!(stats.frames_rendered.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn from_config_builds_the_playlist_or_a_single_scene() {
        let cfg = Config::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/example.toml"
        )))
        .unwrap();
        let all = RenderLoop::from_config(&cfg, None, Store::new()).unwrap();
        assert_eq!(all.scenes.len(), cfg.scenes.len());
        assert_eq!(all.scheduler.len(), cfg.scenes.len());
        let mut one = RenderLoop::from_config(&cfg, Some("banner"), Store::new()).unwrap();
        assert_eq!(one.scenes.len(), 1);
        assert_eq!(one.scenes[0].name(), "banner");
        // `banner` has a condition that is false with an empty store, but a
        // scene requested by name is shown regardless.
        assert!(cfg.scene("banner").unwrap().condition.is_some());
        let active = one
            .scheduler
            .tick(std::time::Instant::now(), &Store::new().snapshot());
        assert_eq!(active.map(|a| a.index), Some(0));
        let e = RenderLoop::from_config(&cfg, Some("nope"), Store::new())
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("no scene named \"nope\" (have: home, banner, image)"));
    }

    fn solid_cfg(color: &str) -> Config {
        Config::parse(&format!(
            r##"
            [canvas]
            width = 16
            height = 8
            fps = 60
            [[scene]]
            name = "s"
              [[scene.zone]]
              rect = {{ x = 0, y = 0, w = 16, h = 8 }}
              background = "{color}"
              widget = {{ type = "text", text = "" }}
            "##
        ))
        .unwrap()
    }

    #[test]
    fn swaps_in_a_reloaded_scene_set() {
        use embedded_graphics::pixelcolor::Rgb888;
        let (tx, mut rx) = frame_channel(16, 8);
        let (reload_tx, reload_rx) = std::sync::mpsc::channel();
        let handle = RenderLoop::from_config(&solid_cfg("#ff0000"), None, Store::new())
            .unwrap()
            .with_reload(reload_rx)
            .spawn(tx)
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                // Wait for a red frame, then send the blue scene set.
                while rx.borrow().pixel(3, 3) != Some(Rgb888::new(255, 0, 0)) {
                    rx.changed().await.unwrap();
                }
                reload_tx
                    .send(SceneSet::from_config(&solid_cfg("#0000ff"), None).unwrap())
                    .unwrap();
                while rx.borrow().pixel(3, 3) != Some(Rgb888::new(0, 0, 255)) {
                    rx.changed().await.unwrap();
                }
            })
            .await
            .expect("frames from the new scene set within 5s");
        });
        handle.stop();
    }

    #[test]
    fn apply_reload_keeps_the_latest_and_survives_a_dropped_sender() {
        let mut lp = RenderLoop::from_config(&solid_cfg("#ff0000"), None, Store::new()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        lp = lp.with_reload(rx);
        assert!(!lp.apply_reload());
        let two = Config::parse(
            r##"
            [canvas]
            width = 16
            height = 8
            [[scene]]
            name = "a"
            [[scene]]
            name = "b"
            "##,
        )
        .unwrap();
        tx.send(SceneSet::from_config(&solid_cfg("#00ff00"), None).unwrap())
            .unwrap();
        tx.send(SceneSet::from_config(&two, None).unwrap()).unwrap();
        drop(tx);
        assert!(lp.apply_reload());
        assert_eq!(lp.scenes.len(), 2);
        assert_eq!(lp.scheduler.len(), 2);
        assert!(!lp.apply_reload());
        assert!(lp.reload.is_none());
    }
}
