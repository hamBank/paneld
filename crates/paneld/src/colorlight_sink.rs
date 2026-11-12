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
//! 2. `0x55` row packets, every row, card offset 0
//! 3. pause `sync_gap_us`
//! 4. `0x01` display frame (twice on firmware >= 13)
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

use crate::canvas::Canvas;
use crate::config::{ColorOrder, ColorlightOutput};
use crate::engine::FrameReceiver;
use crate::postprocess::PostProcess;

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
    pub post: PostProcess,
    pub color_order: ColorOrder,
    /// Brightness byte for both the `0x0A` and the `0x01` frame.
    pub brightness: u8,
    pub sync_gap: Duration,
    /// Firmware major version override; `None` = use discovery.
    pub firmware_major: Option<u8>,
    /// Canvas size, used for the card-size sanity check.
    pub canvas: (u32, u32),
    /// First and maximum wait before re-opening a dead socket.
    pub reconnect_backoff: (Duration, Duration),
}

impl SinkSettings {
    pub fn from_output(o: &ColorlightOutput, canvas_w: u32, canvas_h: u32) -> Self {
        SinkSettings {
            interface: o.interface.clone(),
            post: PostProcess::from_output(o),
            color_order: o.color_order,
            brightness: o.card_brightness(),
            sync_gap: Duration::from_micros(u64::from(o.sync_gap_us)),
            firmware_major: o.firmware_major,
            canvas: (canvas_w, canvas_h),
            reconnect_backoff: (Duration::from_secs(1), Duration::from_secs(10)),
        }
    }
}

/// Counters, shared with the rest of the daemon (exported in M5).
#[derive(Debug, Default)]
pub struct SinkStats {
    pub frames_sent: AtomicU64,
    /// Frames that failed to send.
    pub send_errors: AtomicU64,
    /// Times the socket was (re-)opened successfully.
    pub opens: AtomicU64,
    pub last_send_us: AtomicU64,
    pub max_send_us: AtomicU64,
    pub total_send_us: AtomicU64,
}

impl SinkStats {
    fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.last_send_us.store(us, Ordering::Relaxed);
        self.max_send_us.fetch_max(us, Ordering::Relaxed);
        self.total_send_us.fetch_add(us, Ordering::Relaxed);
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
    sync_gap: Duration,
    double_sync: bool,
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
            sync_gap,
            double_sync,
            pixels: Vec::new(),
            wire: Vec::new(),
            spans: Vec::new(),
        }
    }

    pub fn double_sync(&self) -> bool {
        self.double_sync
    }

    pub fn set_double_sync(&mut self, v: bool) {
        self.double_sync = v;
    }

    /// Send one frame. Returns how long it took, pause included.
    pub fn send<L: Link + ?Sized>(
        &mut self,
        link: &mut L,
        canvas: &Canvas,
    ) -> anyhow::Result<Duration> {
        let (w, h) = (canvas.width() as usize, canvas.height() as usize);
        anyhow::ensure!(
            (1..=1 << 16).contains(&w) && (1..=1 << 16).contains(&h),
            "canvas {w}x{h} does not fit the Colorlight protocol (max 65536 per side)"
        );
        // Preparing the packets is not part of the send time.
        self.post.apply_into(canvas, &mut self.pixels);
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

/// Handle to a running sink. Call [`SinkHandle::stop`] to end it.
pub struct SinkHandle {
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<SinkStats>,
}

impl SinkHandle {
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
pub fn spawn(settings: SinkSettings, rx: FrameReceiver) -> std::io::Result<SinkHandle> {
    spawn_with(settings, rx, RawLink::open)
}

/// Start a sink that gets its link from `open`, which is called again (with
/// backoff) whenever the link dies. Tests pass a mock.
pub fn spawn_with<L, F>(
    settings: SinkSettings,
    rx: FrameReceiver,
    open: F,
) -> std::io::Result<SinkHandle>
where
    L: Link + 'static,
    F: FnMut(&str) -> Result<L, LinkError> + Send + 'static,
{
    let (stop_tx, stop_rx) = watch::channel(false);
    let stats = Arc::new(SinkStats::default());
    let thread = std::thread::Builder::new()
        .name("colorlight".into())
        .spawn({
            let stats = stats.clone();
            move || run(settings, rx, stop_rx, open, &stats)
        })?;
    Ok(SinkHandle {
        stop: stop_tx,
        thread: Some(thread),
        stats,
    })
}

fn run<L, F>(
    settings: SinkSettings,
    mut rx: FrameReceiver,
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

    let iface = settings.interface.as_str();
    let mut sender = FrameSender::new(
        settings.post.clone(),
        settings.color_order,
        settings.brightness,
        settings.sync_gap,
        false,
    );
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
                Ok(double) => {
                    sender.set_double_sync(double);
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
                    _ = stop.wait_for(|s| *s) => false,
                }
            });
            if !got {
                return;
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

/// Discover cards and decide whether to send syncs twice. Warnings only; an
/// error means the link itself failed.
fn startup_discovery<L: Link>(link: &mut L, s: &SinkSettings) -> Result<bool, LinkError> {
    let cards = discover(link, DISCOVERY_TIMEOUT)?;
    for c in &cards {
        tracing::info!(
            receiver = c.receiver_index,
            firmware = format!("{}.{}", c.firmware_major, c.firmware_minor),
            reported_size = format!("{}x{}", c.width, c.height),
            "colorlight: found receiver"
        );
        if (u32::from(c.width), u32::from(c.height)) != s.canvas {
            // The card reports its own cabinet size, which need not equal
            // the canvas (e.g. two panels on two ports), and FPP says the
            // field is not always reliable (PROTOCOL.md). Hence a warning.
            tracing::warn!(
                receiver = c.receiver_index,
                "colorlight: card reports {}x{} but the canvas is {}x{}; check the LEDVision \
                 settings if the picture is wrong",
                c.width,
                c.height,
                s.canvas.0,
                s.canvas.1
            );
        }
    }
    let highest = cards.iter().map(|c| c.firmware_major).max();
    let mixed = cards.iter().any(|c| Some(c.firmware_major) != highest);
    if mixed {
        tracing::warn!("colorlight: receivers on one interface run different firmware majors");
    }
    let major = match (s.firmware_major, highest) {
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
    Ok(double)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postprocess::PostProcessParams;
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
        for (mode, byte) in [("hardware", 180), ("software", 255)] {
            let cfg = cfg(mode);
            let crate::config::OutputConfig::Colorlight(o) = &cfg.outputs[0] else {
                unreachable!()
            };
            let s = SinkSettings::from_output(o, 4, 2);
            assert_eq!(s.brightness, byte, "{mode}");
            assert_eq!(s.color_order, ColorOrder::Bgr);
            assert_eq!(s.sync_gap, Duration::from_micros(700));
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
            post: PostProcess::new(PostProcessParams::default()),
            color_order: ColorOrder::Rgb,
            brightness: 77,
            sync_gap: Duration::ZERO,
            firmware_major: Some(13),
            canvas: (4, 2),
            reconnect_backoff: (Duration::from_millis(5), Duration::from_millis(20)),
        };
        let handle = spawn_with(settings, rx, open).unwrap();

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
