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
use crate::config::{Config, NotificationsConfig};
use crate::control::{Command, ControlLink, NotificationContent, OutputLevel, Status};
use crate::inline;
use crate::scene::Scene;
use crate::scheduler::{Scheduler, Target};
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
    /// Look of inline notifications.
    pub notifications: NotificationsConfig,
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
            notifications: cfg.notifications.clone(),
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
    /// Look of inline notifications.
    pub notifications: NotificationsConfig,
    /// The scene built for the inline notification showing, with the key of
    /// that showing (so it is built once per showing, not per frame).
    inline: Option<((u64, u32), Scene)>,
    pub store: Store,
    /// Prepared scene sets from the config reloader. Polled without blocking
    /// at the top of every frame.
    pub reload: Option<Receiver<SceneSet>>,
    /// Control plane (M4): commands in, status and output level out.
    pub control: Option<ControlLink>,
    /// Runtime state set through the control plane.
    pub state: RuntimeState,
}

/// What the control plane has set; survives config reloads.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeState {
    /// Pinned scene by name (re-resolved after a reload).
    pub pinned: Option<String>,
    pub on: bool,
    pub brightness: u8,
    pub dim: u8,
}

impl Default for RuntimeState {
    fn default() -> Self {
        RuntimeState {
            pinned: None,
            on: true,
            brightness: 255,
            dim: 255,
        }
    }
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
            notifications: set.notifications,
            inline: None,
            store,
            reload: None,
            control: None,
            state: RuntimeState::default(),
        })
    }

    /// Take commands from, and publish status and output level to, the
    /// control plane (`control::channel`).
    pub fn with_control(mut self, link: ControlLink) -> Self {
        self.control = Some(link);
        self
    }

    /// Apply every queued control command. Never blocks.
    fn apply_commands(&mut self, now: Instant) {
        let Some(link) = &self.control else { return };
        let mut output_changed = false;
        while let Ok(cmd) = link.commands.try_recv() {
            match cmd {
                Command::Pin(Some(name)) => {
                    match self.scenes.iter().position(|s| s.name() == name) {
                        Some(i) => {
                            self.scheduler.pin(Some(i), now);
                            tracing::info!(scene = %name, "scene pinned");
                            self.state.pinned = Some(name);
                        }
                        None => tracing::warn!(scene = %name, "cannot pin: no such scene"),
                    }
                }
                Command::Pin(None) => {
                    if self.state.pinned.take().is_some() {
                        self.scheduler.pin(None, now);
                        tracing::info!("scene pin released");
                    }
                }
                Command::Notify(n) => {
                    let known = match &n.content {
                        NotificationContent::Scene(name) => {
                            self.scenes.iter().any(|s| s.name() == name)
                        }
                        NotificationContent::Inline { .. } => true,
                    };
                    if known {
                        tracing::info!(
                            priority = n.priority,
                            repeat = n.repeat,
                            "notification queued"
                        );
                        self.scheduler.notify(n);
                    } else {
                        tracing::warn!("notification for an unknown scene ignored");
                    }
                }
                Command::ClearNotifications => {
                    self.scheduler.clear_notifications();
                    tracing::info!("notifications cleared");
                }
                Command::SetPower(on) => {
                    output_changed |= self.state.on != on;
                    self.state.on = on;
                }
                Command::SetBrightness(b) => {
                    output_changed |= self.state.brightness != b;
                    self.state.brightness = b;
                }
                Command::SetDim(d) => {
                    output_changed |= self.state.dim != d;
                    self.state.dim = d;
                }
            }
        }
        if output_changed {
            let level =
                OutputLevel::from_parts(self.state.on, self.state.brightness, self.state.dim);
            link.output.send_replace(level);
        }
    }

    /// Publish the status if it differs from the last one.
    fn publish_status(&self, scene: Option<usize>, fps: f32) {
        let Some(link) = &self.control else { return };
        let status = Status {
            scenes: self.scenes.iter().map(|s| s.name().to_owned()).collect(),
            scene: scene.map(|i| self.scenes[i].name().to_owned()),
            pinned: self.state.pinned.clone(),
            notifications: self.scheduler.notification_status(),
            on: self.state.on,
            brightness: self.state.brightness,
            dim: self.state.dim,
            fps,
        };
        if **link.status.borrow() != status {
            link.status.send_replace(Arc::new(status));
        }
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
        let queue = self.scheduler.take_queue();
        self.scheduler = set.scheduler;
        self.scheduler.adopt_queue(queue);
        self.notifications = set.notifications;
        self.inline = None;
        tracing::info!("config reloaded: {} scenes", self.scenes.len());
        // Keep a pin across the reload, by name.
        if let Some(name) = &self.state.pinned {
            match self.scenes.iter().position(|s| s.name() == name) {
                Some(i) => self.scheduler.pin(Some(i), Instant::now()),
                None => {
                    tracing::warn!(scene = %name, "pinned scene is gone after reload; pin released");
                    self.state.pinned = None;
                }
            }
        }
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
        // Frames in the current one-second window, for the status `fps`.
        let (mut fps_window, mut fps_frames, mut fps) = (Instant::now(), 0u32, 0f32);
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
            self.apply_commands(Instant::now());

            let snapshot = self.store.snapshot();
            let active = self.scheduler.tick(Instant::now(), &snapshot);
            let current = active.as_ref().and_then(|a| a.index());
            if shown != Some(current) {
                match current {
                    Some(i) => tracing::info!(scene = self.scenes[i].name(), "scene changed"),
                    None if active.is_some() => tracing::info!("inline notification showing"),
                    None => tracing::info!("no scene eligible, showing black"),
                }
                shown = Some(current);
            }

            fps_frames += 1;
            let since = fps_window.elapsed();
            if since >= Duration::from_secs(1) {
                fps = fps_frames as f32 / since.as_secs_f32();
                (fps_window, fps_frames) = (Instant::now(), 0);
            }
            self.publish_status(current, (fps * 10.0).round() / 10.0);

            let mut canvas = Canvas::new(self.width, self.height);
            if let Some(a) = active {
                let ctx = FrameCtx {
                    now: Local::now().fixed_offset(),
                    scene_elapsed: a.elapsed,
                    frame,
                };
                match &a.target {
                    Target::Scene(i) => self.scenes[*i].render(&ctx, &snapshot, &mut canvas),
                    Target::Inline { key, notice } => {
                        if self.inline.as_ref().is_none_or(|(k, _)| k != key) {
                            let scene =
                                inline::scene(&self.notifications, self.width, self.height, notice);
                            self.inline = Some((*key, scene));
                        }
                        if let Some((_, scene)) = &mut self.inline {
                            scene.render(&ctx, &snapshot, &mut canvas);
                        }
                    }
                }
            } else {
                self.inline = None;
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
    fn control_commands_pin_and_set_the_output_level() {
        let cfg = Config::parse(
            r##"
            [canvas]
            width = 8
            height = 8
            fps = 60
            [[scene]]
            name = "a"
            [[scene]]
            name = "b"
            condition = "x:never"
            "##,
        )
        .unwrap();
        let (control, link, mut output) =
            crate::control::channel(cfg.scenes.iter().map(|s| s.name.clone()).collect());
        let (tx, _rx) = frame_channel(8, 8);
        let handle = RenderLoop::from_config(&cfg, None, Store::new())
            .unwrap()
            .with_control(link)
            .spawn(tx)
            .unwrap();
        let mut status = control.subscribe();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let wait = |status: &mut watch::Receiver<Arc<Status>>, f: &dyn Fn(&Status) -> bool| {
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), status.wait_for(|s| f(s)))
                    .await
                    .expect("status within 5 s")
                    .unwrap();
            })
        };
        wait(&mut status, &|s| s.scene.as_deref() == Some("a"));

        // Pin the never-eligible scene: it shows anyway.
        control.send(Command::Pin(Some("b".into()))).unwrap();
        wait(&mut status, &|s| {
            s.scene.as_deref() == Some("b") && s.pinned.as_deref() == Some("b")
        });
        control.send(Command::Pin(Some("nope".into()))).unwrap();
        control.send(Command::Pin(None)).unwrap();
        wait(&mut status, &|s| {
            s.scene.as_deref() == Some("a") && s.pinned.is_none()
        });

        control.send(Command::SetBrightness(128)).unwrap();
        control.send(Command::SetDim(128)).unwrap();
        control.send(Command::SetPower(false)).unwrap();
        wait(&mut status, &|s| {
            !s.on && s.brightness == 128 && s.dim == 128
        });
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), output.wait_for(|o| !o.on))
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(
            *output.borrow(),
            OutputLevel {
                on: false,
                level: 64
            }
        );
        handle.stop();
    }

    #[test]
    fn notifications_show_report_status_and_clear() {
        use crate::color::Color;
        use crate::control::Notification;
        use embedded_graphics::pixelcolor::Rgb888;
        use embedded_graphics::pixelcolor::RgbColor;

        let cfg = Config::parse(
            r##"
            [canvas]
            width = 64
            height = 32
            fps = 60
            [[scene]]
            name = "a"
              [[scene.zone]]
              rect = { x = 0, y = 0, w = 64, h = 32 }
              background = "#0000ff"
              widget = { type = "text", text = "" }
            "##,
        )
        .unwrap();
        let (control, link, _out) = crate::control::channel(vec!["a".into()]);
        let (tx, mut frames) = frame_channel(64, 32);
        let handle = RenderLoop::from_config(&cfg, None, Store::new())
            .unwrap()
            .with_control(link)
            .spawn(tx)
            .unwrap();
        let mut status = control.subscribe();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let wait = |status: &mut watch::Receiver<Arc<Status>>, f: &dyn Fn(&Status) -> bool| {
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), status.wait_for(|s| f(s)))
                    .await
                    .expect("status within 5 s")
                    .unwrap();
            })
        };
        let inline = |text: &str, priority, secs| Notification {
            content: NotificationContent::Inline {
                text: text.into(),
                icon: None,
                color: Color::WHITE,
            },
            duration: Duration::from_secs(secs),
            priority,
            repeat: 1,
        };
        wait(&mut status, &|s| s.scene.as_deref() == Some("a"));
        let blue = Some(Rgb888::new(0, 0, 255));
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while frames.borrow().pixel(1, 1) != blue {
                    frames.changed().await.unwrap();
                }
            })
            .await
            .expect("the scene frame within 5 s");
        });

        control
            .send(Command::Notify(inline("hello", 1, 600)))
            .unwrap();
        control
            .send(Command::Notify(Notification {
                content: NotificationContent::Scene("a".into()),
                duration: Duration::from_secs(600),
                priority: 0,
                repeat: 2,
            }))
            .unwrap();
        wait(&mut status, &|s| {
            s.scene.is_none()
                && s.notifications.len() == 2
                && s.notifications[0].showing
                && s.notifications[0].summary == "hello"
                && s.notifications[1].summary == "scene:a"
                && s.notifications[1].repeats_left == 2
        });
        // The inline notification is white text on black: the blue is gone
        // and some white pixels were drawn.
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let f = frames.borrow().clone();
                    if f.pixel(1, 1) == Some(Rgb888::BLACK) && f.pixels().contains(&Rgb888::WHITE) {
                        break;
                    }
                    frames.changed().await.unwrap();
                }
            })
            .await
            .expect("an inline frame within 5 s");
        });

        control.send(Command::ClearNotifications).unwrap();
        wait(&mut status, &|s| {
            s.notifications.is_empty() && s.scene.as_deref() == Some("a")
        });
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while frames.borrow().pixel(1, 1) != blue {
                    frames.changed().await.unwrap();
                }
            })
            .await
            .expect("the scene is back within 5 s");
        });
        handle.stop();
    }

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
            notifications: NotificationsConfig::default(),
            inline: None,
            store: Store::new(),
            reload: None,
            control: None,
            state: RuntimeState::default(),
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
        assert_eq!(active.and_then(|a| a.index()), Some(0));
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
