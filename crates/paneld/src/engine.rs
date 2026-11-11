//! The render loop (DESIGN.md §5.1): a dedicated thread ticking at a fixed
//! rate. It only reads store snapshots and never blocks on I/O. Each frame
//! is published as an `Arc<Canvas>` on a `watch` channel, which behaves like
//! a capacity-1 channel where the latest frame wins.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Local;
use tokio::sync::watch;

use crate::canvas::Canvas;
use crate::scene::Scene;
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

pub struct RenderLoop {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub scene: Scene,
    pub store: Store,
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
        let started = Instant::now();
        let mut next = started;
        let mut frame = 0u64;
        tracing::info!(
            fps = self.fps,
            scene = self.scene.name(),
            "render loop started"
        );

        while !shutdown.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }

            let ctx = FrameCtx {
                now: Local::now().fixed_offset(),
                scene_elapsed: started.elapsed(),
                frame,
            };
            let mut canvas = Canvas::new(self.width, self.height);
            self.scene.render(&ctx, &self.store.snapshot(), &mut canvas);
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
            scene: Scene::from_config(&cfg.scenes[0]),
            store: Store::new(),
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
}
