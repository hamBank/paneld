//! The Colorlight sink (DESIGN.md §4, §5.1): takes frames from the engine's
//! `watch` channel and sends them to a 5A-75B card over a [`Link`].
//!
//! * [`FrameSender`] is the pure part: given a canvas it produces the exact
//!   packet sequence for one frame. It is tested against a mock link.
//! * [`spawn`] / [`spawn_with`] run it on its own thread. The render loop
//!   never waits for it: the `watch` channel keeps only the latest frame, so
//!   a slow link just means skipped frames.
//!
//! # Frame order (PROTOCOL.md, "Frame order for the sink")
//!
//! 1. `0x0A` brightness (twice on firmware >= 13)
//! 2. `0x55` row packets, every row of the card space (see below), offset 0
//! 3. pause `sync_gap_us`
//! 4. `0x01` display frame (twice on firmware >= 13)
//!
//! # Output map
//!
//! The rows are rows of the *card* space, not of the canvas: the sink first
//! gathers the canvas through the output's [`OutputMap`] (a no-op without
//! `[[output.region]]`), then applies gamma/white point and colour order.
//! Every row of the card space is sent every frame, so card rows no region
//! covers are sent as black. The card keeps showing whatever it last
//! received, so skipping them would leave stale or power-on pixels there.
//! Whole black rows cost a few percent of the frame, which is cheap next to
//! a wrong picture.
//!
//! If sending the rows fails, the display frame is not sent, so the card
//! keeps showing the last complete frame (as FPP does).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use colorlight_net::{discover, Link, LinkError, RawLink};
use colorlight_proto::{
    needs_double_sync, row_packets, BrightnessFrame, DisplayFrame, ROW_HEADER_LEN,
};
use tokio::sync::watch;

use crate::metrics::{Histogram, SEND_BUCKETS};
use crate::output_map::OutputMap;

use crate::canvas::Canvas;
use crate::config::{BrightnessMode, ColorOrder, ColorlightOutput};
use crate::control::OutputLevel;
use crate::engine::FrameReceiver;
use crate::postprocess::{PostProcess, PostProcessParams};

/// How long discovery waits for an answer to each request (FPP uses 300 ms).
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(300);

/// Consecutive failed frames after which the socket is re-opened even if the
/// error doesn't look fatal.
const REOPEN_AFTER_FAILURES: u32 = 3;

/// Everything the sink needs, independent of the config file so that the
/// `test-pattern` command can build it from flags.
#[derive(Debug, Clone)]
pub struct SinkSettings {
    pub interface: String,
    /// How the output level turns into LUT and card brightness.
    pub plan: BrightnessPlan,
    pub color_order: ColorOrder,
    pub sync_gap: Duration,
    /// Firmware major version override; `None` = use discovery.
    pub firmware_major: Option<u8>,
    /// The card's display space, used for the discovery size check.
    pub card_size: (u32, u32),
    /// Canvas to card mapping.
    pub map: Arc<OutputMap>,
    /// First and maximum wait before re-opening a dead socket.
    pub reconnect_backoff: (Duration, Duration),
}

impl SinkSettings {
    pub fn from_output(o: &ColorlightOutput, canvas_w: u32, canvas_h: u32) -> anyhow::Result<Self> {
        let map = o.output_map(canvas_w, canvas_h).map_err(|e| {
            anyhow::anyhow!(
                "output map for the colorlight output on {}: {e}",
                o.interface
            )
        })?;
        Ok(SinkSettings {
            interface: o.interface.clone(),
            plan: BrightnessPlan::from_output(o),
            color_order: o.color_order,
            sync_gap: Duration::from_micros(u64::from(o.sync_gap_us)),
            firmware_major: o.firmware_major,
            card_size: map.card_size(),
            map: Arc::new(map),
            reconnect_backoff: (Duration::from_secs(1), Duration::from_secs(10)),
        })
    }
}

/// What a sink applies for one [`OutputLevel`].
#[derive(Debug, Clone)]
pub struct Applied {
    /// Gamma / white point (and, in software mode, brightness) LUT.
    pub post: PostProcess,
    /// Brightness byte for both the `0x0A` and the `0x01` frame.
    pub card_brightness: u8,
    /// Send black pixels instead of the canvas.
    pub blank: bool,
}

/// How an output's configured `brightness` (its maximum) follows the live
/// [`OutputLevel`] (DESIGN.md section 5.2 and 7):
///
/// - hardware mode: the card brightness byte is `level.scale(configured)`;
///   the LUT stays at gamma and white point only.
/// - software mode: the card byte stays 255 and the LUT is rebuilt with
///   brightness `level.scale(configured)`.
/// - off: black pixels and card brightness 0 in both modes. Frames keep
///   flowing, so the card never holds a stale image.
///
/// The preview is not affected: it shows content in true colour.
#[derive(Debug, Clone, Copy)]
pub struct BrightnessPlan {
    pub mode: BrightnessMode,
    /// The output's configured (maximum) brightness.
    pub configured: u8,
    pub gamma: f32,
    pub white_point: crate::color::Color,
}

impl BrightnessPlan {
    pub fn from_output(o: &ColorlightOutput) -> Self {
        BrightnessPlan {
            mode: o.brightness_mode,
            configured: o.brightness,
            gamma: o.gamma,
            white_point: o.white_point,
        }
    }

    pub fn apply(&self, level: OutputLevel) -> Applied {
        let b = level.scale(self.configured);
        let (lut_brightness, card_brightness) = match self.mode {
            BrightnessMode::Hardware => (255, b),
            BrightnessMode::Software => (b, if level.on { 255 } else { 0 }),
        };
        Applied {
            post: PostProcess::new(PostProcessParams {
                gamma: self.gamma,
                white_point: self.white_point,
                brightness: lut_brightness,
            }),
            card_brightness,
            blank: !level.on,
        }
    }
}

/// Counters, shared with the rest of the daemon (exported in M5).
#[derive(Debug)]
pub struct SinkStats {
    pub frames_sent: AtomicU64,
    /// Frames that failed to send.
    pub send_errors: AtomicU64,
    /// Times the socket was (re-)opened successfully.
    pub opens: AtomicU64,
    pub last_send_us: AtomicU64,
    pub max_send_us: AtomicU64,
    pub total_send_us: AtomicU64,
    /// Send time per frame, for `/metrics`.
    pub send_duration: Histogram,
}

impl Default for SinkStats {
    fn default() -> Self {
        SinkStats {
            frames_sent: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            opens: AtomicU64::new(0),
            last_send_us: AtomicU64::new(0),
            max_send_us: AtomicU64::new(0),
            total_send_us: AtomicU64::new(0),
            send_duration: Histogram::new(SEND_BUCKETS),
        }
    }
}

impl SinkStats {
    pub(crate) fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.last_send_us.store(us, Ordering::Relaxed);
        self.max_send_us.fetch_max(us, Ordering::Relaxed);
        self.total_send_us.fetch_add(us, Ordering::Relaxed);
        self.send_duration.observe(d);
    }

    /// Mean time to send a frame, in µs (0 before the first frame).
    pub fn mean_send_us(&self) -> u64 {
        self.total_send_us
            .load(Ordering::Relaxed)
            .checked_div(self.frames_sent.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// Turns canvases into the packet sequence for one frame. Holds reusable
/// buffers, so keep one per sink.
pub struct FrameSender {
    post: PostProcess,
    color_order: ColorOrder,
    brightness: u8,
    blank: bool,
    sync_gap: Duration,
    double_sync: bool,
    /// Canvas to card mapping; `None` (or an identity map) sends the canvas
    /// as it is.
    map: Option<Arc<OutputMap>>,
    /// The gathered card-space frame (reused).
    card: Canvas,
    /// Corrected, reordered pixels of the current frame.
    pixels: Vec<u8>,
    /// All row packets of the current frame, back to back.
    wire: Vec<u8>,
    /// `(start, end)` of each row packet in `wire`.
    spans: Vec<(usize, usize)>,
}

impl FrameSender {
    pub fn new(
        post: PostProcess,
        color_order: ColorOrder,
        brightness: u8,
        sync_gap: Duration,
        double_sync: bool,
    ) -> Self {
        FrameSender {
            post,
            color_order,
            brightness,
            blank: false,
            sync_gap,
            double_sync,
            map: None,
            card: Canvas::new(0, 0),
            pixels: Vec::new(),
            wire: Vec::new(),
            spans: Vec::new(),
        }
    }

    /// Send the rows of the card space given by `map` instead of the canvas.
    pub fn with_map(mut self, map: Arc<OutputMap>) -> Self {
        self.map = Some(map);
        self
    }

    /// Switch to a new brightness result (call when the level changes).
    pub fn set_applied(&mut self, a: Applied) {
        self.post = a.post;
        self.brightness = a.card_brightness;
        self.blank = a.blank;
    }

    pub fn double_sync(&self) -> bool {
        self.double_sync
    }

    pub fn set_double_sync(&mut self, v: bool) {
        self.double_sync = v;
    }

    /// Apply reloaded settings (everything but the brightness result, which
    /// the caller sets with [`FrameSender::set_applied`]).
    pub fn set_output(&mut self, color_order: ColorOrder, sync_gap: Duration, map: Arc<OutputMap>) {
        self.color_order = color_order;
        self.sync_gap = sync_gap;
        self.map = Some(map);
    }

    /// Send one frame. Returns how long it took, pause included.
    pub fn send<L: Link + ?Sized>(
        &mut self,
        link: &mut L,
        canvas: &Canvas,
    ) -> anyhow::Result<Duration> {
        let map = self.map.clone().filter(|m| !m.is_identity());
        let (w, h) = match &map {
            Some(m) => {
                let (w, h) = m.card_size();
                (w as usize, h as usize)
            }
            None => (canvas.width() as usize, canvas.height() as usize),
        };
        anyhow::ensure!(
            (1..=1 << 16).contains(&w) && (1..=1 << 16).contains(&h),
            "frame {w}x{h} does not fit the Colorlight protocol (max 65536 per side)"
        );
        // Preparing the packets is not part of the send time.
        if self.blank {
            self.pixels.clear();
            self.pixels.resize(w * h * 3, 0);
        } else if let Some(m) = &map {
            m.gather_into(canvas, &mut self.card)
                .map_err(anyhow::Error::msg)?;
            self.post.apply_into(&self.card, &mut self.pixels);
        } else {
            self.post.apply_into(canvas, &mut self.pixels);
        }
        self.color_order.reorder_in_place(&mut self.pixels);

        self.wire.clear();
        self.spans.clear();
        for (y, row) in self.pixels.chunks_exact(w * 3).enumerate() {
            let packets = row_packets(y as u16, 0, row).context("encoding row")?;
            for p in packets {
                let start = self.wire.len();
                self.wire.resize(start + p.frame_len(), 0);
                p.write_to(&mut self.wire[start..])
                    .context("encoding row")?;
                self.spans.push((start, self.wire.len()));
            }
        }
        debug_assert!(self.wire.len() >= h * ROW_HEADER_LEN);

        let brightness = BrightnessFrame::uniform(self.brightness).encode();
        let display = DisplayFrame::neutral(self.brightness).encode();
        let syncs = if self.double_sync { 2 } else { 1 };

        let started = Instant::now();
        let b: Vec<&[u8]> = vec![&brightness[..]; syncs];
        link.send(&b)?;
        let rows: Vec<&[u8]> = self.spans.iter().map(|&(s, e)| &self.wire[s..e]).collect();
        link.send(&rows)?;
        link.pause(self.sync_gap);
        let d: Vec<&[u8]> = vec![&display[..]; syncs];
        link.send(&d)?;
        Ok(started.elapsed())
    }
}

/// Allows one event per `every`, counting what it swallowed in between.
#[derive(Debug)]
pub struct RateLimit {
    every: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl RateLimit {
    pub fn new(every: Duration) -> Self {
        RateLimit {
            every,
            last: None,
            suppressed: 0,
        }
    }

    /// `Some(n)` if the event may be logged now, `n` being how many were
    /// suppressed since the last one that was.
    pub fn allow(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(t) if now.duration_since(t) < self.every => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

/// Sends reloaded settings to a running sink (see [`SinkHandle::update`]).
#[derive(Debug, Clone)]
pub struct SinkUpdater(Arc<watch::Sender<Arc<SinkSettings>>>);

impl SinkUpdater {
    pub fn update(&self, settings: SinkSettings) {
        self.0.send_replace(Arc::new(settings));
    }

    /// An updater with no sink behind it, and the receiver a sink would use.
    #[cfg(test)]
    pub(crate) fn detached(
        settings: SinkSettings,
    ) -> (SinkUpdater, watch::Receiver<Arc<SinkSettings>>) {
        let (tx, rx) = watch::channel(Arc::new(settings));
        (SinkUpdater(Arc::new(tx)), rx)
    }
}

/// Handle to a running sink. Call [`SinkHandle::stop`] to end it.
pub struct SinkHandle {
    stop: watch::Sender<bool>,
    settings: SinkUpdater,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<SinkStats>,
    /// Network interface this sink sends on (the `sink` metric label).
    pub interface: String,
}

impl SinkHandle {
    /// Apply reloaded settings from the next frame on: brightness plan
    /// (gamma, brightness, white point, mode), colour order, sync gap,
    /// firmware override and output map. The interface cannot change; a
    /// different one is ignored (the caller restarts instead).
    pub fn update(&self, settings: SinkSettings) {
        self.settings.update(settings);
    }

    /// A handle that only updates settings, for the reload task.
    pub fn updater(&self) -> SinkUpdater {
        self.settings.clone()
    }

    /// Ask the thread to stop and wait for it.
    pub fn stop(mut self) {
        self.stop.send_replace(true);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                tracing::error!("colorlight sink thread panicked");
            }
        }
    }
}

/// Start a sink on a real raw socket.
pub fn spawn(
    settings: SinkSettings,
    rx: FrameReceiver,
    levels: watch::Receiver<OutputLevel>,
) -> std::io::Result<SinkHandle> {
    spawn_with(settings, rx, levels, RawLink::open)
}

/// Start a sink that gets its link from `open`, which is called again (with
/// backoff) whenever the link dies. Tests pass a mock.
pub fn spawn_with<L, F>(
    settings: SinkSettings,
    rx: FrameReceiver,
    levels: watch::Receiver<OutputLevel>,
    open: F,
) -> std::io::Result<SinkHandle>
where
    L: Link + 'static,
    F: FnMut(&str) -> Result<L, LinkError> + Send + 'static,
{
    let (stop_tx, stop_rx) = watch::channel(false);
    let stats = Arc::new(SinkStats::default());
    let interface = settings.interface.clone();
    let (settings_tx, settings_rx) = watch::channel(Arc::new(settings));
    let thread = std::thread::Builder::new()
        .name("colorlight".into())
        .spawn({
            let stats = stats.clone();
            move || run(settings_rx, rx, levels, stop_rx, open, &stats)
        })?;
    Ok(SinkHandle {
        stop: stop_tx,
        settings: SinkUpdater(Arc::new(settings_tx)),
        thread: Some(thread),
        stats,
        interface,
    })
}

fn run<L, F>(
    mut settings_rx: watch::Receiver<Arc<SinkSettings>>,
    mut rx: FrameReceiver,
    mut levels: watch::Receiver<OutputLevel>,
    mut stop: watch::Receiver<bool>,
    mut open: F,
    stats: &SinkStats,
) where
    L: Link,
    F: FnMut(&str) -> Result<L, LinkError>,
{
    // A private current-thread runtime only to wait on the channels with a
    // timeout; all sending is plain blocking code.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "colorlight sink: cannot start runtime");
            return;
        }
    };
    // Sleep unless asked to stop; true if the sleep completed.
    let sleep = |d: Duration, stop: &mut watch::Receiver<bool>| {
        rt.block_on(async {
            tokio::time::timeout(d, stop.wait_for(|s| *s))
                .await
                .is_err()
        })
    };

    let mut settings = settings_rx.borrow_and_update().clone();
    let iface_owned = settings.interface.clone();
    let iface = iface_owned.as_str();
    let mut settings_open = true;
    // What discovery found, kept so a reloaded `firmware_major` can be
    // resolved again without re-discovering.
    let mut discovered: Option<Option<u8>> = None;
    let mut current = *levels.borrow_and_update();
    let first = settings.plan.apply(current);
    let mut sender = FrameSender::new(
        first.post.clone(),
        settings.color_order,
        first.card_brightness,
        settings.sync_gap,
        false,
    )
    .with_map(settings.map.clone());
    sender.set_applied(first);
    let mut levels_open = true;
    let mut configured = false;
    let mut backoff = settings.reconnect_backoff.0;
    let mut open_log = RateLimit::new(Duration::from_secs(30));
    let mut send_log = RateLimit::new(Duration::from_secs(5));
    rx.mark_changed();

    'reopen: loop {
        if *stop.borrow() {
            return;
        }
        let mut link = match open(iface) {
            Ok(l) => l,
            Err(e) => {
                if let Some(n) = open_log.allow(Instant::now()) {
                    tracing::error!(interface = iface, suppressed = n, "colorlight: {e}");
                }
                if !sleep(backoff, &mut stop) {
                    return;
                }
                backoff = (backoff * 2).min(settings.reconnect_backoff.1);
                continue 'reopen;
            }
        };
        backoff = settings.reconnect_backoff.0;
        stats.opens.fetch_add(1, Ordering::Relaxed);
        tracing::info!(interface = iface, "colorlight: socket open");

        if !configured {
            match startup_discovery(&mut link, &settings) {
                Ok(found) => {
                    sender.set_double_sync(resolve_double_sync(&settings, found));
                    discovered = Some(found);
                    configured = true;
                }
                Err(e) => {
                    tracing::error!(interface = iface, "colorlight: discovery failed: {e}");
                    if !sleep(backoff, &mut stop) {
                        return;
                    }
                    continue 'reopen;
                }
            }
        }

        let mut consecutive_failures = 0u32;
        loop {
            let got = rt.block_on(async {
                tokio::select! {
                    r = rx.changed() => r.is_ok(),
                    // A level change re-sends the current canvas. If the
                    // control plane is gone the level just stays put.
                    r = levels.changed(), if levels_open => {
                        levels_open = r.is_ok();
                        true
                    }
                    // Reloaded settings re-send the current canvas too.
                    r = settings_rx.changed(), if settings_open => {
                        settings_open = r.is_ok();
                        true
                    }
                    _ = stop.wait_for(|s| *s) => false,
                }
            });
            if !got {
                return;
            }
            let latest = settings_rx.borrow_and_update().clone();
            if !Arc::ptr_eq(&latest, &settings) {
                let new = latest;
                apply_reload(&mut sender, &settings, &new, current, discovered);
                settings = new;
            }
            // Read the level once per frame; rebuild the LUT only on change.
            let level = *levels.borrow_and_update();
            if level != current {
                current = level;
                sender.set_applied(settings.plan.apply(level));
                tracing::debug!(
                    on = level.on,
                    level = level.level,
                    "colorlight: output level"
                );
            }
            let canvas = rx.borrow_and_update().clone();
            match sender.send(&mut link, &canvas) {
                Ok(took) => {
                    consecutive_failures = 0;
                    stats.record(took);
                    tracing::debug!(send_us = took.as_micros() as u64, "colorlight frame sent");
                }
                Err(e) => {
                    stats.send_errors.fetch_add(1, Ordering::Relaxed);
                    consecutive_failures += 1;
                    let fatal = e
                        .downcast_ref::<LinkError>()
                        .is_some_and(LinkError::is_fatal);
                    if let Some(n) = send_log.allow(Instant::now()) {
                        tracing::warn!(
                            interface = iface,
                            suppressed = n,
                            "colorlight: sending a frame failed: {e:#}"
                        );
                    }
                    if fatal || consecutive_failures >= REOPEN_AFTER_FAILURES {
                        tracing::warn!(interface = iface, "colorlight: re-opening the socket");
                        if !sleep(backoff, &mut stop) {
                            return;
                        }
                        backoff = (backoff * 2).min(settings.reconnect_backoff.1);
                        continue 'reopen;
                    }
                }
            }
        }
    }
}

/// Switch a running sender to reloaded settings. The interface is fixed for
/// the life of the sink: a different one is logged and ignored.
fn apply_reload(
    sender: &mut FrameSender,
    old: &SinkSettings,
    new: &SinkSettings,
    level: OutputLevel,
    discovered: Option<Option<u8>>,
) {
    if new.interface != old.interface {
        tracing::warn!(
            interface = old.interface,
            new_interface = new.interface,
            "colorlight: the interface of a running output cannot change; restart paneld"
        );
    }
    sender.set_output(new.color_order, new.sync_gap, new.map.clone());
    sender.set_applied(new.plan.apply(level));
    if new.firmware_major != old.firmware_major {
        if let Some(found) = discovered {
            sender.set_double_sync(resolve_double_sync(new, found));
        }
    }
    if new.card_size != old.card_size {
        tracing::info!(
            interface = old.interface,
            card_size = format!("{}x{}", new.card_size.0, new.card_size.1),
            "colorlight: new card space (the discovery size check runs at start-up only)"
        );
    }
    tracing::info!(
        interface = old.interface,
        "colorlight: output settings reloaded"
    );
}

/// Whether to send syncs twice, from the configured override and what
/// discovery found (the highest firmware major, `None` if nothing answered).
fn resolve_double_sync(s: &SinkSettings, found: Option<u8>) -> bool {
    let major = match (s.firmware_major, found) {
        (Some(m), found) => {
            if found.is_some_and(|f| f != m) {
                tracing::warn!(
                    configured = m,
                    discovered = found,
                    "colorlight: firmware_major in the config differs from what the card reports; using the config"
                );
            }
            m
        }
        (None, Some(m)) => m,
        (None, None) => {
            tracing::warn!(
                "colorlight: no receiver answered discovery on {} and firmware_major is not set; \
                 assuming firmware < 13",
                s.interface
            );
            0
        }
    };
    let double = needs_double_sync(major);
    tracing::info!(
        firmware_major = major,
        double_sync = double,
        "colorlight: ready"
    );
    double
}

/// Discover cards and log what they report. Returns the highest firmware
/// major found (`None` if nothing answered). Warnings only; an error means
/// the link itself failed.
fn startup_discovery<L: Link>(link: &mut L, s: &SinkSettings) -> Result<Option<u8>, LinkError> {
    let cards = discover(link, DISCOVERY_TIMEOUT)?;
    for c in &cards {
        tracing::info!(
            receiver = c.receiver_index,
            firmware = format!("{}.{}", c.firmware_major, c.firmware_minor),
            reported_size = format!("{}x{}", c.width, c.height),
            "colorlight: found receiver"
        );
    }
    // Compare against the output's card space (output map), not the canvas.
    for w in colorlight_net::check_layout(s.card_size, &cards) {
        tracing::warn!("colorlight: {w}");
    }
    Ok(cards.iter().map(|c| c.firmware_major).max())
}

#[cfg(test)]
mod tests {
    use super::*;
    use colorlight_net::mock::{Event, MockLink};
    use colorlight_proto::{encode_row_header, MAX_PIXELS_PER_PACKET};
    use embedded_graphics::pixelcolor::Rgb888;
    use std::sync::Mutex;

    const GAP: Duration = Duration::from_micros(500);

    fn sender(order: ColorOrder, brightness: u8, double: bool) -> FrameSender {
        FrameSender::new(
            PostProcess::new(PostProcessParams::default()),
            order,
            brightness,
            GAP,
            double,
        )
    }

    /// Pixel (x, y) = (x, y, 200).
    fn canvas(w: u32, h: u32) -> Canvas {
        let mut c = Canvas::new(w, h);
        for y in 0..h {
            for x in 0..w {
                c.set_pixel(x, y, Rgb888::new(x as u8, y as u8, 200));
            }
        }
        c
    }

    /// What kind of packet an event is: its type byte, or `None` for a pause.
    fn kinds(link: &MockLink) -> Vec<Option<u8>> {
        link.events
            .iter()
            .map(|e| match e {
                Event::Frame(f) => Some(f[12]),
                Event::Pause(_) => None,
            })
            .collect()
    }

    fn expected_order(syncs: usize, rows: usize) -> Vec<Option<u8>> {
        let mut v = vec![Some(0x0A); syncs];
        v.extend(vec![Some(0x55); rows]);
        v.push(None);
        v.extend(vec![Some(0x01); syncs]);
        v
    }

    #[test]
    fn frame_order_on_firmware_11() {
        let mut link = MockLink::new();
        sender(ColorOrder::Rgb, 100, false)
            .send(&mut link, &canvas(8, 5))
            .unwrap();
        assert_eq!(kinds(&link), expected_order(1, 5));
        assert_eq!(link.events[1 + 5], Event::Pause(GAP));
    }

    #[test]
    fn frame_order_on_firmware_13_doubles_brightness_and_sync_only() {
        let mut link = MockLink::new();
        sender(ColorOrder::Rgb, 100, true)
            .send(&mut link, &canvas(8, 5))
            .unwrap();
        // 0x0A x2, rows once each, pause, 0x01 x2.
        assert_eq!(kinds(&link), expected_order(2, 5));
        let frames = link.frames();
        assert_eq!(frames[0], frames[1]);
        assert_eq!(frames[frames.len() - 1], frames[frames.len() - 2]);
    }

    #[test]
    fn the_double_sync_rule_follows_the_firmware_major() {
        for (major, double) in [
            (10, false),
            (11, false),
            (12, false),
            (13, true),
            (14, true),
        ] {
            assert_eq!(needs_double_sync(major), double, "firmware {major}");
        }
    }

    #[test]
    fn brightness_goes_in_both_frames() {
        let mut link = MockLink::new();
        // 10% on the 0x01 frame is 0x1a (hkubota's table); the same byte is
        // used for 0x0A, as FPP does.
        sender(ColorOrder::Rgb, 0x1a, false)
            .send(&mut link, &canvas(4, 2))
            .unwrap();
        let frames = link.frames();
        let b = frames[0];
        assert_eq!(b.len(), colorlight_proto::BRIGHTNESS_FRAME_LEN);
        assert_eq!(b[12], 0x0A);
        assert_eq!(&b[13..16], &[0x1a, 0x1a, 0x1a]);
        assert_eq!(b, &BrightnessFrame::uniform(0x1a).encode()[..]);
        let d = frames[frames.len() - 1];
        assert_eq!(d.len(), colorlight_proto::DISPLAY_FRAME_LEN);
        assert_eq!(&d[12..14], &[0x01, 0x07]);
        assert_eq!(d[35], 0x1a);
        assert_eq!(&d[38..41], &[0x1a, 0x1a, 0x1a]);
        assert_eq!(d, &DisplayFrame::neutral(0x1a).encode()[..]);
    }

    #[test]
    fn settings_use_card_brightness() {
        let cfg = |mode: &str| {
            crate::config::Config::parse(&format!(
                "[canvas]\nwidth = 4\nheight = 2\n[[scene]]\nname = \"s\"\n\
                 [[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\nbrightness = 180\n\
                 brightness_mode = \"{mode}\"\ncolor_order = \"bgr\"\nsync_gap_us = 700\n"
            ))
            .unwrap()
        };
        for (mode, byte) in [("hardware", 180u8), ("software", 255)] {
            let cfg = cfg(mode);
            let crate::config::OutputConfig::Colorlight(o) = &cfg.outputs[0] else {
                unreachable!()
            };
            let s = SinkSettings::from_output(o, 4, 2).unwrap();
            let a = s.plan.apply(OutputLevel::FULL);
            assert_eq!(a.card_brightness, byte, "{mode}");
            assert_eq!(s.color_order, ColorOrder::Bgr);
            assert_eq!(s.sync_gap, Duration::from_micros(700));
        }
    }

    fn plan(mode: BrightnessMode, configured: u8) -> BrightnessPlan {
        BrightnessPlan {
            mode,
            configured,
            gamma: 1.0,
            white_point: crate::color::Color::WHITE,
        }
    }

    /// Send a solid mid-grey 1x1 frame; return the card brightness bytes
    /// of the 0x0A and 0x01 frames and the first pixel byte.
    fn send_at(p: &BrightnessPlan, level: OutputLevel) -> (u8, u8, u8) {
        let mut s = sender(ColorOrder::Rgb, 0, false);
        s.set_applied(p.apply(level));
        let mut cv = Canvas::new(1, 1);
        cv.fill(Rgb888::new(200, 200, 200));
        let mut link = MockLink::new();
        s.send(&mut link, &cv).unwrap();
        let frames = link.frames();
        let row = frames.iter().find(|f| f[12] == 0x55).unwrap();
        let (b, d) = (frames[0], frames[frames.len() - 1]);
        (b[13], d[35], row[ROW_HEADER_LEN])
    }

    #[test]
    fn hardware_mode_scales_the_card_brightness_with_the_level() {
        let p = plan(BrightnessMode::Hardware, 200);
        for (level, card) in [(255u8, 200u8), (128, 100), (64, 50), (0, 0)] {
            let lv = OutputLevel::from_parts(true, level, 255);
            let (b, d, px) = send_at(&p, lv);
            assert_eq!((b, d), (card, card), "level {level}");
            assert_eq!(px, 200, "pixels untouched in hardware mode");
        }
    }

    #[test]
    fn software_mode_scales_the_pixels_and_keeps_the_card_at_full() {
        let p = plan(BrightnessMode::Software, 200);
        for (level, px) in [(255u8, 157u8), (128, 78), (0, 0)] {
            let lv = OutputLevel::from_parts(true, level, 255);
            let (b, d, got) = send_at(&p, lv);
            assert_eq!((b, d), (255, 255), "level {level}");
            // 200 * (configured * level / 255) / 255, within rounding.
            let want = (200.0 * f64::from(lv.scale(200)) / 255.0).round() as u8;
            assert_eq!(got, want, "level {level}");
            assert!(got.abs_diff(px) <= 1, "level {level}: {got} vs ~{px}");
        }
    }

    #[test]
    fn off_sends_black_with_brightness_zero_in_both_modes() {
        for mode in [BrightnessMode::Hardware, BrightnessMode::Software] {
            let off = OutputLevel {
                on: false,
                level: 255,
            };
            assert_eq!(send_at(&plan(mode, 200), off), (0, 0, 0), "{mode:?}");
        }
    }

    #[test]
    fn rows_carry_row_number_and_pixels() {
        let mut link = MockLink::new();
        let cv = canvas(8, 5);
        sender(ColorOrder::Rgb, 255, false)
            .send(&mut link, &cv)
            .unwrap();
        let frames = link.frames();
        for y in 0..5u32 {
            let f = frames[1 + y as usize];
            assert_eq!(f.len(), ROW_HEADER_LEN + 8 * 3);
            // Header: everything after the Ethernet header is the proto's.
            assert_eq!(
                &f[..ROW_HEADER_LEN],
                &encode_row_header(y as u16, 0, 8).unwrap()
            );
            for x in 0..8u32 {
                let at = ROW_HEADER_LEN + 3 * x as usize;
                assert_eq!(&f[at..at + 3], &[x as u8, y as u8, 200]);
            }
        }
    }

    #[test]
    fn rows_wider_than_one_packet_are_split_with_offsets() {
        let mut link = MockLink::new();
        sender(ColorOrder::Rgb, 255, false)
            .send(&mut link, &canvas(600, 2))
            .unwrap();
        let rows: Vec<_> = link
            .frames()
            .into_iter()
            .filter(|f| f[12] == 0x55)
            .collect();
        assert_eq!(rows.len(), 4);
        let field = |f: &[u8], at: usize| u16::from_be_bytes([f[at], f[at + 1]]);
        // (row, offset, count)
        let got: Vec<_> = rows
            .iter()
            .map(|f| (field(f, 13), field(f, 15), field(f, 17)))
            .collect();
        let first = MAX_PIXELS_PER_PACKET as u16;
        assert_eq!(
            got,
            vec![
                (0, 0, first),
                (0, first, 600 - first),
                (1, 0, first),
                (1, first, 600 - first)
            ]
        );
        // The second packet starts at pixel 497, whose red value is 497 % 256.
        assert_eq!(rows[1][ROW_HEADER_LEN], (497 % 256) as u8);
    }

    use crate::canvas::Rect;
    use crate::output_map::{CardOffset, CardSize, Flip, RegionConfig, Rotation};

    fn region(r: Rect, off: (u32, u32), rotate: Rotation) -> RegionConfig {
        RegionConfig {
            canvas: r,
            card_offset: CardOffset { x: off.0, y: off.1 },
            rotate,
            flip: Flip::None,
        }
    }

    /// The payload bytes (after the row header) of each `0x55` row packet.
    fn row_payloads(link: &MockLink) -> Vec<(u16, Vec<u8>)> {
        link.frames()
            .into_iter()
            .filter(|f| f[12] == 0x55)
            .map(|f| {
                (
                    u16::from_be_bytes([f[13], f[14]]),
                    f[ROW_HEADER_LEN..].to_vec(),
                )
            })
            .collect()
    }

    #[test]
    fn two_regions_one_upside_down_send_the_expected_rows() {
        // Two 4x2 panels stacked on a 4x4 canvas; the bottom one is mounted
        // upside down, so the card needs it rotated by 180 degrees.
        let top = region(Rect::new(0, 0, 4, 2), (0, 0), Rotation::Deg0);
        let bottom = region(Rect::new(0, 2, 4, 2), (0, 2), Rotation::Deg180);
        let map = Arc::new(OutputMap::new(4, 4, &[top, bottom], None).unwrap());
        let mut link = MockLink::new();
        let mut s = sender(ColorOrder::Rgb, 255, false).with_map(map);
        s.send(&mut link, &canvas(4, 4)).unwrap();
        assert_eq!(kinds(&link), expected_order(1, 4));

        let rows = row_payloads(&link);
        let want = |f: &dyn Fn(u32) -> (u8, u8)| -> Vec<u8> {
            (0..4)
                .flat_map(|x| {
                    let (px, py) = f(x);
                    [px, py, 200]
                })
                .collect()
        };
        assert_eq!(rows.len(), 4);
        // Rows 0 and 1: canvas rows 0 and 1 as they are.
        assert_eq!(rows[0], (0, want(&|x| (x as u8, 0))));
        assert_eq!(rows[1], (1, want(&|x| (x as u8, 1))));
        // Rows 2 and 3: canvas rows 3 and 2, each reversed left to right.
        assert_eq!(rows[2], (2, want(&|x| (3 - x as u8, 3))));
        assert_eq!(rows[3], (3, want(&|x| (3 - x as u8, 2))));
    }

    #[test]
    fn uncovered_card_rows_and_pixels_are_sent_as_black() {
        // A 2x2 region at (3, 1) of a 6x4 card: all four rows are sent, each
        // 6 pixels wide, black outside the region.
        let r = region(Rect::new(1, 1, 2, 2), (3, 1), Rotation::Deg0);
        let cs = CardSize { w: 6, h: 4 };
        let map = Arc::new(OutputMap::new(4, 4, &[r], Some(cs)).unwrap());
        let mut link = MockLink::new();
        let mut s = sender(ColorOrder::Rgb, 255, false).with_map(map);
        s.send(&mut link, &canvas(4, 4)).unwrap();
        let rows = row_payloads(&link);
        assert_eq!(rows.len(), 4);
        for (y, (row, bytes)) in rows.iter().enumerate() {
            assert_eq!(*row as usize, y);
            assert_eq!(bytes.len(), 6 * 3);
            for x in 0..6usize {
                let px = &bytes[x * 3..x * 3 + 3];
                let inside = (3..5).contains(&x) && (1..3).contains(&y);
                if inside {
                    let (sx, sy) = (x - 3 + 1, y - 1 + 1);
                    assert_eq!(px, &[sx as u8, sy as u8, 200], "({x},{y})");
                } else {
                    assert_eq!(px, &[0, 0, 0], "({x},{y})");
                }
            }
        }
    }

    #[test]
    fn a_mapped_card_is_wider_than_one_packet_and_still_split() {
        let r = region(Rect::new(0, 0, 600, 1), (0, 0), Rotation::Deg90);
        let map = Arc::new(OutputMap::new(600, 1, &[r], None).unwrap());
        let mut link = MockLink::new();
        let mut s = sender(ColorOrder::Rgb, 255, false).with_map(map);
        s.send(&mut link, &canvas(600, 1)).unwrap();
        // 600 rows of one pixel each.
        assert_eq!(row_payloads(&link).len(), 600);
    }

    #[test]
    fn post_processing_and_colour_order_apply_after_the_map() {
        let r = region(Rect::new(0, 0, 2, 1), (0, 0), Rotation::Deg180);
        let map = Arc::new(OutputMap::new(2, 1, &[r], None).unwrap());
        let mut cv = Canvas::new(2, 1);
        cv.set_pixel(0, 0, Rgb888::new(10, 20, 30));
        cv.set_pixel(1, 0, Rgb888::new(40, 50, 60));
        let mut link = MockLink::new();
        let mut s = sender(ColorOrder::Bgr, 255, false).with_map(map);
        s.send(&mut link, &cv).unwrap();
        let rows = row_payloads(&link);
        assert_eq!(rows[0].1, vec![60, 50, 40, 30, 20, 10]);
    }

    #[test]
    fn a_canvas_of_the_wrong_size_is_an_error_not_a_panic() {
        let map = Arc::new(
            OutputMap::new(
                4,
                4,
                &[region(Rect::new(0, 0, 4, 4), (0, 0), Rotation::Deg90)],
                None,
            )
            .unwrap(),
        );
        let mut link = MockLink::new();
        let mut s = sender(ColorOrder::Rgb, 255, false).with_map(map);
        assert!(s.send(&mut link, &canvas(3, 3)).is_err());
        assert!(link.events.is_empty());
    }

    #[test]
    fn blank_frames_cover_the_whole_card_space() {
        let r = region(Rect::new(0, 0, 4, 2), (0, 0), Rotation::Deg90);
        let map = Arc::new(OutputMap::new(4, 2, &[r], None).unwrap());
        let mut link = MockLink::new();
        let mut s = sender(ColorOrder::Rgb, 0, false).with_map(map);
        s.set_applied(Applied {
            post: PostProcess::new(PostProcessParams::default()),
            card_brightness: 0,
            blank: true,
        });
        s.send(&mut link, &canvas(4, 2)).unwrap();
        let rows = row_payloads(&link);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|(_, b)| b == &vec![0u8; 6]));
    }

    #[test]
    fn settings_build_the_map_from_the_output() {
        let cfg = crate::config::Config::parse(
            "[canvas]\nwidth = 6\nheight = 4\n[[scene]]\nname = \"s\"\n\
             [[output]]\ntype = \"colorlight\"\ninterface = \"eth1\"\ncard_size = { w = 8, h = 8 }\n\
             [[output.region]]\ncanvas = { x = 0, y = 0, w = 6, h = 4 }\nrotate = 90\n",
        )
        .unwrap();
        let crate::config::OutputConfig::Colorlight(o) = &cfg.outputs[0] else {
            unreachable!()
        };
        let s = SinkSettings::from_output(o, 6, 4).unwrap();
        assert_eq!(s.card_size, (8, 8));
        assert!(!s.map.is_identity());
        assert_eq!(s.map.source_at(3, 0), Some((0, 0)));
    }

    #[test]
    fn color_orders_reorder_the_wire_bytes() {
        // Canvas pixel (10, 20, 30); wire bytes named by what they carry.
        let cases = [
            (ColorOrder::Rgb, [10, 20, 30]),
            (ColorOrder::Rbg, [10, 30, 20]),
            (ColorOrder::Grb, [20, 10, 30]),
            (ColorOrder::Gbr, [20, 30, 10]),
            (ColorOrder::Brg, [30, 10, 20]),
            (ColorOrder::Bgr, [30, 20, 10]),
        ];
        for (order, wire) in cases {
            let mut cv = Canvas::new(2, 1);
            cv.fill(Rgb888::new(10, 20, 30));
            let mut link = MockLink::new();
            sender(order, 255, false).send(&mut link, &cv).unwrap();
            let row = link.frames().into_iter().find(|f| f[12] == 0x55).unwrap();
            assert_eq!(&row[ROW_HEADER_LEN..ROW_HEADER_LEN + 3], &wire, "{order:?}");
            assert_eq!(&row[ROW_HEADER_LEN + 3..], &wire, "{order:?}");

            let mut bytes = vec![10, 20, 30, 10, 20, 30, 99];
            order.reorder_in_place(&mut bytes);
            assert_eq!(&bytes[..3], &wire);
            assert_eq!(bytes[6], 99, "partial pixel untouched");
        }
    }

    #[test]
    fn post_processing_runs_before_reordering() {
        let post = PostProcess::new(PostProcessParams {
            gamma: 2.0,
            ..PostProcessParams::default()
        });
        let mut s = FrameSender::new(post, ColorOrder::Bgr, 255, GAP, false);
        let mut cv = Canvas::new(1, 1);
        cv.fill(Rgb888::new(255, 128, 0));
        let mut link = MockLink::new();
        s.send(&mut link, &cv).unwrap();
        let row = link.frames().into_iter().find(|f| f[12] == 0x55).unwrap();
        // gamma 2.0: 128 -> round(255 * (128/255)^2) = 64. BGR: 0, 64, 255.
        assert_eq!(&row[ROW_HEADER_LEN..], &[0, 64, 255]);
    }

    #[test]
    fn a_failed_send_stops_the_frame_before_the_display_frame() {
        let mut link = MockLink::new();
        link.fail_next_sends(1, false);
        let mut s = sender(ColorOrder::Rgb, 255, true);
        let e = s.send(&mut link, &canvas(4, 2)).unwrap_err();
        assert!(!e.downcast_ref::<LinkError>().unwrap().is_fatal());
        assert!(link.events.is_empty());

        // The next frame is complete again.
        s.send(&mut link, &canvas(4, 2)).unwrap();
        assert_eq!(kinds(&link), expected_order(2, 2));
    }

    #[test]
    fn thread_applies_reloaded_settings_from_the_next_frame() {
        use colorlight_net::mock::Event;
        let (tx, rx) = crate::engine::frame_channel(4, 2);
        let link = Shared(Arc::new(Mutex::new(MockLink::new())));
        let open = {
            let link = link.clone();
            move |_: &str| Ok(link.clone())
        };
        let settings = |gap_us: u64, firmware: u8, order: ColorOrder| SinkSettings {
            interface: "eth9".into(),
            plan: BrightnessPlan {
                mode: BrightnessMode::Hardware,
                configured: 255,
                gamma: 1.0,
                white_point: crate::color::Color::WHITE,
            },
            color_order: order,
            sync_gap: Duration::from_micros(gap_us),
            firmware_major: Some(firmware),
            card_size: (4, 2),
            map: Arc::new(OutputMap::identity(4, 2)),
            reconnect_backoff: (Duration::from_millis(5), Duration::from_millis(20)),
        };
        let (_level_tx, level_rx) = watch::channel(OutputLevel::FULL);
        let handle = spawn_with(settings(0, 13, ColorOrder::Rgb), rx, level_rx, open).unwrap();
        let mut red = canvas(4, 2);
        for y in 0..2 {
            for x in 0..4 {
                red.set_pixel(x, y, Rgb888::new(255, 0, 0));
            }
        }
        let wait_for = |done: &dyn Fn(&MockLink) -> bool| {
            let start = Instant::now();
            while !done(&link.0.lock().unwrap()) {
                assert!(start.elapsed() < Duration::from_secs(10), "timed out");
                tx.send_replace(Arc::new(red.clone()));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for(&|l| l.frames().iter().any(|f| f[12] == 0x01));
        // Firmware 13: doubled syncs; RGB order: red in the first byte.
        assert!(link.types().windows(2).any(|w| w == [0x0A, 0x0A]));

        handle.update(settings(7_000, 11, ColorOrder::Bgr));
        link.0.lock().unwrap().events.clear();
        let gap = Event::Pause(Duration::from_micros(7_000));
        wait_for(&|l| l.events.contains(&gap) && l.frames().iter().any(|f| f[12] == 0x01));
        let l = link.0.lock().unwrap();
        // The first complete frame after the reload: single sync, new gap,
        // blue-green-red byte order (red is now the third byte).
        let start = l
            .events
            .iter()
            .position(|e| matches!(e, Event::Frame(f) if f[12] == 0x0A))
            .unwrap();
        let after: Vec<&Event> = l.events[start..].iter().collect();
        let Event::Frame(second) = after[1] else {
            panic!("frame expected")
        };
        assert_eq!(second[12], 0x55, "one brightness packet, then rows");
        assert!(after.contains(&&gap));
        let row = after
            .iter()
            .find_map(|e| match e {
                Event::Frame(f) if f[12] == 0x55 => Some(f),
                _ => None,
            })
            .unwrap();
        let px = &row[row.len() - 12..row.len() - 9];
        assert_eq!(px, [0, 0, 255]);
        drop(l);
        handle.stop();
    }

    #[test]
    fn rate_limit_allows_one_per_interval_and_counts_the_rest() {
        let mut r = RateLimit::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(r.allow(t0), Some(0));
        assert_eq!(r.allow(t0 + Duration::from_secs(1)), None);
        assert_eq!(r.allow(t0 + Duration::from_secs(4)), None);
        assert_eq!(r.allow(t0 + Duration::from_secs(5)), Some(2));
        assert_eq!(r.allow(t0 + Duration::from_secs(6)), None);
    }

    /// A mock link the test can still reach after the sink thread owns it.
    #[derive(Clone)]
    struct Shared(Arc<Mutex<MockLink>>);

    impl Link for Shared {
        fn send(&mut self, frames: &[&[u8]]) -> Result<(), LinkError> {
            self.0.lock().unwrap().send(frames)
        }
        fn recv(&mut self, buf: &mut [u8], t: Duration) -> Result<Option<usize>, LinkError> {
            self.0.lock().unwrap().recv(buf, t)
        }
        fn pause(&mut self, d: Duration) {
            self.0.lock().unwrap().pause(d);
        }
    }

    impl Shared {
        /// Types of the sent frames, leaving out discovery requests.
        fn types(&self) -> Vec<u8> {
            self.0
                .lock()
                .unwrap()
                .frames()
                .iter()
                .map(|f| f[12])
                .filter(|&t| t != 0x07)
                .collect()
        }
    }

    #[test]
    fn thread_sends_frames_and_reopens_after_a_dead_socket() {
        let (tx, rx) = crate::engine::frame_channel(4, 2);
        let links: Arc<Mutex<Vec<Shared>>> = Arc::default();
        let mut attempts = 0;
        let open = {
            let links = links.clone();
            move |_: &str| {
                attempts += 1;
                if attempts <= 2 {
                    return Err(LinkError::UnknownInterface("eth9".into()));
                }
                let l = Shared(Arc::new(Mutex::new(MockLink::new())));
                links.lock().unwrap().push(l.clone());
                Ok(l)
            }
        };
        let settings = SinkSettings {
            interface: "eth9".into(),
            plan: BrightnessPlan {
                mode: BrightnessMode::Hardware,
                configured: 77,
                gamma: 1.0,
                white_point: crate::color::Color::WHITE,
            },
            color_order: ColorOrder::Rgb,
            sync_gap: Duration::ZERO,
            firmware_major: Some(13),
            card_size: (4, 2),
            map: Arc::new(OutputMap::identity(4, 2)),
            reconnect_backoff: (Duration::from_millis(5), Duration::from_millis(20)),
        };
        let (_level_tx, level_rx) = watch::channel(OutputLevel::FULL);
        let handle = spawn_with(settings, rx, level_rx, open).unwrap();

        // Keep publishing frames until `done` holds (latest frame wins, so a
        // single send could be skipped).
        let publish_until = |done: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !done() {
                assert!(start.elapsed() < Duration::from_secs(10), "timed out");
                tx.send_replace(Arc::new(canvas(4, 2)));
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // Two failed opens (with backoff), then the third works.
        publish_until(&|| {
            links
                .lock()
                .unwrap()
                .first()
                .is_some_and(|l| l.types().contains(&0x01))
        });
        let first = links.lock().unwrap()[0].clone();
        // Discovery found nobody; the override says firmware 13, so doubled.
        assert_eq!(&first.types()[..5], &[0x0A, 0x0A, 0x55, 0x55, 0x01]);

        // Kill the socket: the next send fails fatally and it is re-opened.
        first.0.lock().unwrap().fail_next_sends(1, true);
        publish_until(&|| {
            links
                .lock()
                .unwrap()
                .get(1)
                .is_some_and(|l| l.types().contains(&0x01))
        });
        let second = links.lock().unwrap()[1].clone();
        // No second discovery, but the firmware decision was kept.
        assert_eq!(&second.types()[..2], &[0x0A, 0x0A]);

        let stats = handle.stats.clone();
        handle.stop();
        assert_eq!(stats.opens.load(Ordering::Relaxed), 2);
        assert!(stats.send_errors.load(Ordering::Relaxed) >= 1);
        assert!(stats.frames_sent.load(Ordering::Relaxed) >= 2);
        assert!(stats.max_send_us.load(Ordering::Relaxed) >= stats.mean_send_us());
    }
}
