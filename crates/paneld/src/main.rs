use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use chrono::{DateTime, Local};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use paneld::color::Color;
use paneld::colorlight_sink::{self, SinkSettings};
use paneld::config::{
    BrightnessMode, ColorOrder, ColorlightOutput, Config, OutputConfig, MAX_GAMMA, MAX_SYNC_GAP_US,
    MIN_GAMMA,
};
use paneld::engine::{frame_channel, RenderLoop};
use paneld::store::Store;
use paneld::test_pattern::{self, Pattern};
use paneld::widgets::FrameCtx;
use paneld::{png_io, preview, reload, render};

#[derive(Parser)]
#[command(
    version,
    about = "Drive HUB75 LED panels from Home Assistant and friends"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
    Run {
        #[arg(long)]
        config: PathBuf,
        /// Show only this scene. Default: rotate through all scenes.
        #[arg(long)]
        scene: Option<String>,
    },
    /// Validate a config file and exit.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// Render one frame of a scene to a PNG.
    Render {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        scene: String,
        #[arg(long)]
        out: PathBuf,
        /// Time to render, RFC 3339 (e.g. 2026-01-02T13:37:00+01:00). Default: now.
        #[arg(long)]
        time: Option<DateTime<chrono::FixedOffset>>,
        /// Seconds since the scene started (drives scrolling).
        #[arg(long, default_value_t = 0.0)]
        elapsed: f64,
        /// Pixel scale of the output image.
        #[arg(long, default_value_t = 1)]
        scale: u32,
    },
    /// Render a hardware bring-up test pattern to a PNG and/or send it to the card.
    TestPattern(TestPatternArgs),
    /// Find Colorlight receiver cards on an interface and print what they report.
    /// Needs CAP_NET_RAW.
    Probe {
        /// Network interface the card is attached to, e.g. eth1.
        #[arg(long)]
        interface: String,
        /// How long to wait for an answer to each discovery request.
        #[arg(long, default_value_t = 500)]
        timeout_ms: u64,
    },
}

/// Test pattern names for `--pattern`.
#[derive(Clone, Copy, ValueEnum)]
enum PatternArg {
    Bars,
    Gradient,
    PanelIds,
    Grid,
    Solid,
    Walk,
}

/// Arguments of `test-pattern`.
///
/// With `--out` the pattern is written to a PNG. With `--interface` it is
/// sent to the Colorlight card continuously until Ctrl-C, through the same
/// sink code `run` uses. At least one of the two is required.
#[derive(Args)]
struct TestPatternArgs {
    /// Canvas width in pixels.
    #[arg(long)]
    width: u32,
    /// Canvas height in pixels.
    #[arg(long)]
    height: u32,
    /// Width of one panel (default: the canvas width).
    #[arg(long)]
    panel_width: Option<u32>,
    /// Height of one panel (default: the canvas height).
    #[arg(long)]
    panel_height: Option<u32>,
    #[arg(long, value_enum, default_value = "bars")]
    pattern: PatternArg,
    /// Colour for `--pattern solid`, as "#rrggbb" (default #ffffff).
    #[arg(long)]
    color: Option<Color>,
    /// Frame index (position of `--pattern walk`).
    #[arg(long, default_value_t = 0)]
    frame: u64,
    /// Output PNG.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Pixel scale of the output image.
    #[arg(long, default_value_t = 1)]
    scale: u32,
    /// Send the pattern to the card on this interface (needs CAP_NET_RAW).
    #[arg(long)]
    interface: Option<String>,
    /// Frames per second when sending (default 30). `--interface` only.
    #[arg(long)]
    fps: Option<u32>,
    /// Card brightness 0..=255 (default 64, so bring-up is not blinding).
    /// `--interface` only.
    #[arg(long)]
    brightness: Option<u8>,
    /// Gamma 1.0..=4.0 (default 1.0, so raw values are visible).
    /// `--interface` only.
    #[arg(long)]
    gamma: Option<f32>,
    /// Wire colour order: rgb (default), rbg, grb, gbr, brg, bgr.
    /// `--interface` only.
    #[arg(long)]
    color_order: Option<ColorOrder>,
    /// Card firmware major version, if discovery doesn't find it.
    /// `--interface` only.
    #[arg(long)]
    firmware_major: Option<u8>,
    /// Pause between the last row and the display frame, in µs (default 500).
    /// `--interface` only.
    #[arg(long)]
    sync_gap_us: Option<u32>,
}

fn test_pattern_cmd(a: TestPatternArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=4096).contains(&a.width) && (1..=4096).contains(&a.height),
        "--width and --height must be 1..=4096"
    );
    let (pw, ph) = (
        a.panel_width.unwrap_or(a.width),
        a.panel_height.unwrap_or(a.height),
    );
    anyhow::ensure!(
        pw >= 1 && ph >= 1,
        "--panel-width and --panel-height must be >= 1"
    );
    anyhow::ensure!((1..=64).contains(&a.scale), "--scale must be 1..=64");
    anyhow::ensure!(
        a.out.is_some() || a.interface.is_some(),
        "give --out <png> and/or --interface <nic>"
    );
    if a.interface.is_none() {
        anyhow::ensure!(
            a.fps.is_none()
                && a.brightness.is_none()
                && a.gamma.is_none()
                && a.color_order.is_none()
                && a.firmware_major.is_none()
                && a.sync_gap_us.is_none(),
            "--fps, --brightness, --gamma, --color-order, --firmware-major and --sync-gap-us \
             only apply with --interface"
        );
    }
    anyhow::ensure!(
        a.color.is_none() || matches!(a.pattern, PatternArg::Solid),
        "--color only applies to --pattern solid"
    );
    let pattern = match a.pattern {
        PatternArg::Bars => Pattern::Bars,
        PatternArg::Gradient => Pattern::Gradient,
        PatternArg::PanelIds => Pattern::PanelIds,
        PatternArg::Grid => Pattern::Grid,
        PatternArg::Solid => Pattern::Solid(a.color.unwrap_or(Color::WHITE).0),
        PatternArg::Walk => Pattern::Walk,
    };
    if let Some(out) = &a.out {
        let canvas = test_pattern::render(pattern, a.width, a.height, pw, ph, a.frame);
        std::fs::write(out, png_io::encode(&canvas, a.scale, false))
            .with_context(|| format!("writing {}", out.display()))?;
        println!("wrote {}", out.display());
    }
    let Some(interface) = a.interface.clone() else {
        return Ok(());
    };

    let fps = a.fps.unwrap_or(30);
    anyhow::ensure!(
        (1..=paneld::config::MAX_FPS).contains(&fps),
        "--fps must be 1..={}",
        paneld::config::MAX_FPS
    );
    let gamma = a.gamma.unwrap_or(1.0);
    anyhow::ensure!(
        gamma.is_finite() && (MIN_GAMMA..=MAX_GAMMA).contains(&gamma),
        "--gamma must be {MIN_GAMMA}..={MAX_GAMMA}"
    );
    let sync_gap_us = a.sync_gap_us.unwrap_or(500);
    anyhow::ensure!(
        sync_gap_us <= MAX_SYNC_GAP_US,
        "--sync-gap-us must be 0..={MAX_SYNC_GAP_US}"
    );
    let output = ColorlightOutput {
        interface,
        gamma,
        brightness: a.brightness.unwrap_or(64),
        white_point: Color::WHITE,
        brightness_mode: BrightnessMode::Hardware,
        color_order: a.color_order.unwrap_or_default(),
        firmware_major: a.firmware_major,
        sync_gap_us,
    };
    send_pattern(&output, pattern, &a, (pw, ph), fps)
}

/// Send a test pattern to the card at `fps` until Ctrl-C, advancing the
/// frame counter each tick (so `walk` moves).
fn send_pattern(
    output: &ColorlightOutput,
    pattern: Pattern,
    a: &TestPatternArgs,
    (pw, ph): (u32, u32),
    fps: u32,
) -> anyhow::Result<()> {
    let (tx, rx) = frame_channel(a.width, a.height);
    let settings = SinkSettings::from_output(output, a.width, a.height);
    let sink = colorlight_sink::spawn(settings, rx).context("starting the colorlight sink")?;
    println!(
        "sending to {} at {fps} fps, brightness {}, gamma {}, colour order {:?}; Ctrl-C to stop",
        output.interface, output.brightness, output.gamma, output.color_order
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut tick = tokio::time::interval(Duration::from_secs(1) / fps);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut frame = a.frame;
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let c = test_pattern::render(pattern, a.width, a.height, pw, ph, frame);
                    tx.send_replace(std::sync::Arc::new(c));
                    frame += 1;
                }
                _ = &mut ctrl_c => break,
            }
        }
    });

    let stats = sink.stats.clone();
    sink.stop();
    println!(
        "sent {} frames, {} errors, send time mean {} us, max {} us",
        stats.frames_sent.load(std::sync::atomic::Ordering::Relaxed),
        stats.send_errors.load(std::sync::atomic::Ordering::Relaxed),
        stats.mean_send_us(),
        stats.max_send_us.load(std::sync::atomic::Ordering::Relaxed),
    );
    Ok(())
}

fn probe_cmd(interface: &str, timeout_ms: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=10_000).contains(&timeout_ms),
        "--timeout-ms must be 1..=10000"
    );
    let mut link = colorlight_net::RawLink::open(interface)?;
    let cards = colorlight_net::discover(&mut link, Duration::from_millis(timeout_ms))?;
    anyhow::ensure!(
        !cards.is_empty(),
        "no Colorlight receiver answered on {interface} (waited {timeout_ms} ms per request). \
         Check the cable, that the card is powered and on this NIC, that the NIC has link, \
         and that the card's firmware answers raw Ethernet discovery"
    );
    for c in &cards {
        let model = if c.model == colorlight_proto::MODEL_5A {
            "5A".to_string()
        } else {
            format!("unknown (0x{:02x})", c.model)
        };
        println!(
            "receiver {}: model {model}, firmware {}.{}, reported size {}x{}, double sync: {}",
            c.receiver_index,
            c.firmware_major,
            c.firmware_minor,
            c.width,
            c.height,
            if c.needs_double_sync() { "yes" } else { "no" },
        );
    }
    println!("{} receiver(s) found", cards.len());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Run { config, scene } => run(config, scene),
        Cmd::Check { config } => {
            let cfg = Config::load(&config)?;
            println!(
                "{}: ok ({}x{} canvas, {} scene(s))",
                config.display(),
                cfg.canvas.width,
                cfg.canvas.height,
                cfg.scenes.len()
            );
            Ok(())
        }
        Cmd::TestPattern(args) => test_pattern_cmd(args),
        Cmd::Probe {
            interface,
            timeout_ms,
        } => probe_cmd(&interface, timeout_ms),
        Cmd::Render {
            config,
            scene,
            out,
            time,
            elapsed,
            scale,
        } => {
            anyhow::ensure!(
                elapsed.is_finite() && elapsed >= 0.0,
                "--elapsed must be >= 0"
            );
            anyhow::ensure!((1..=64).contains(&scale), "--scale must be 1..=64");
            let cfg = Config::load(&config)?;
            let ctx = FrameCtx {
                now: time.unwrap_or_else(|| Local::now().fixed_offset()),
                scene_elapsed: Duration::from_secs_f64(elapsed),
                frame: 0,
            };
            let canvas = render::render_scene(&cfg, &scene, &ctx, &Store::new().snapshot())?;
            std::fs::write(&out, png_io::encode(&canvas, scale, false))
                .with_context(|| format!("writing {}", out.display()))?;
            println!("wrote {}", out.display());
            Ok(())
        }
    }
}

fn run(config_path: PathBuf, scene: Option<String>) -> anyhow::Result<()> {
    let (cfg, loaded_text) = Config::load_with_text(&config_path)?;
    paneld::sources::check_environment(&cfg)?;
    let (tx, rx) = frame_channel(cfg.canvas.width, cfg.canvas.height);
    let (reload_tx, reload_rx) = std::sync::mpsc::channel();
    let reload_ctx =
        reload::ReloadCtx::new(&config_path, &cfg, scene.as_deref()).loaded_from(&loaded_text);
    let reload_stats = std::sync::Arc::new(reload::ReloadStats::default());
    paneld::tls::init();
    let store = Store::new();
    let render = RenderLoop::from_config(&cfg, scene.as_deref(), store.clone())?
        .with_reload(reload_rx)
        .spawn(tx)
        .context("starting render thread")?;

    let sinks: Vec<_> = cfg
        .outputs
        .iter()
        .filter_map(|o| match o {
            OutputConfig::Colorlight(cl) => Some(cl),
            _ => None,
        })
        .map(|cl| {
            let settings = SinkSettings::from_output(cl, cfg.canvas.width, cfg.canvas.height);
            colorlight_sink::spawn(settings, rx.clone())
                .with_context(|| format!("starting colorlight sink on {}", cl.interface))
        })
        .collect::<anyhow::Result<_>>()?;

    let mut app = Router::new();
    if let Some(p) = cfg.preview() {
        app = app.merge(preview::router(
            rx,
            p,
            cfg.canvas.fps,
            &paneld::sources::attributions(&cfg),
        ));
    }

    let rt = tokio::runtime::Runtime::new()?;
    let served = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(cfg.http.bind)
            .await
            .with_context(|| format!("binding {}", cfg.http.bind))?;
        reload::spawn(reload_ctx, reload_tx, reload_stats.clone());
        let sources = paneld::sources::Supervisor::start(&cfg, &store);
        tracing::info!(addr = %cfg.http.bind, "http listening");
        if cfg.preview().is_some() {
            tracing::info!("preview at http://{}/preview", cfg.http.bind);
        }
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("http server");
        sources.stop(std::time::Duration::from_secs(5)).await;
        served
    });

    tracing::info!(
        reloads = reload_stats
            .reloads
            .load(std::sync::atomic::Ordering::Relaxed),
        reload_errors = reload_stats
            .reload_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        "config reload totals"
    );
    // Stop the render loop first so the sinks see no new frames, then the sinks.
    render.stop();
    for sink in sinks {
        let stats = sink.stats.clone();
        sink.stop();
        tracing::info!(
            frames = stats.frames_sent.load(std::sync::atomic::Ordering::Relaxed),
            errors = stats.send_errors.load(std::sync::atomic::Ordering::Relaxed),
            mean_send_us = stats.mean_send_us(),
            "colorlight sink stopped"
        );
    }
    served
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "listening for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "listening for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutting down");
}
