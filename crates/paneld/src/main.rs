use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use paneld::config::{Config, OutputConfig};
use paneld::engine::{frame_channel, RenderLoop};
use paneld::scene::Scene;
use paneld::store::Store;
use paneld::widgets::FrameCtx;
use paneld::{png_io, preview, render};

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
        /// Scene to show. Default: the first one. (Rotation arrives in M2.)
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
    let cfg = Config::load(&config_path)?;
    let scene_cfg = match &scene {
        Some(name) => cfg
            .scene(name)
            .with_context(|| format!("no scene named {name:?}"))?,
        None => &cfg.scenes[0],
    };

    for o in &cfg.outputs {
        if let OutputConfig::Colorlight(cl) = o {
            tracing::warn!(
                "colorlight output on {} not implemented yet, ignoring",
                cl.interface
            );
        }
    }

    let store = Store::new();
    let (tx, rx) = frame_channel(cfg.canvas.width, cfg.canvas.height);
    let render = RenderLoop {
        width: cfg.canvas.width,
        height: cfg.canvas.height,
        fps: cfg.canvas.fps,
        scene: Scene::from_config(scene_cfg),
        store,
    }
    .spawn(tx)
    .context("starting render thread")?;

    let mut app = Router::new();
    if let Some(p) = cfg.preview() {
        app = app.merge(preview::router(rx, p, cfg.canvas.fps));
    }

    let rt = tokio::runtime::Runtime::new()?;
    let served = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(cfg.http.bind)
            .await
            .with_context(|| format!("binding {}", cfg.http.bind))?;
        tracing::info!(addr = %cfg.http.bind, "http listening");
        if cfg.preview().is_some() {
            tracing::info!("preview at http://{}/preview", cfg.http.bind);
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("http server")
    });

    render.stop();
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
