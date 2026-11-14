//! Data sources (DESIGN.md §6): async tasks that write into the store.
//!
//! Each configured `[sources.<name>]` becomes one [`Source`], run by the
//! [`Supervisor`] on the tokio runtime. A source that fails (returns `Err`)
//! or ends early is rebuilt from its config and restarted with exponential
//! backoff. The render loop never waits on a source: it only reads store
//! snapshots.
//!
//! Writing a source:
//! - Implement [`Source::run`]: loop until `ctx.shutdown` fires, writing with
//!   `ctx.store` (use [`crate::store::StoreWriter::apply`] to update several
//!   related keys at once).
//! - Report recoverable problems (one failed poll) with [`SourceCtx::error`]
//!   and successes with [`SourceCtx::ok`], so health is visible (metrics in
//!   M5). Return `Err` only for failures that need a fresh start.
//! - Never log secrets (tokens, passwords). They come from env vars named in
//!   config (`*_env`).
//! - HTTP: use [`http_client`] (one TLS stack for the whole daemon).

pub mod aircraft;
pub mod ha;
pub mod mqtt;
pub mod static_values;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::{Config, SourceConfig};
use crate::store::{Store, StoreWriter};

/// What [`Source::run`] returns.
pub type SourceFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

/// A running data source. Built fresh from its config on every (re)start.
pub trait Source: Send {
    /// Run until `ctx.shutdown` fires (then return `Ok`). Returning `Err`
    /// (or `Ok` before shutdown) makes the supervisor restart the source.
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture;
}

/// Build the source for one `[sources.<name>]` entry.
pub fn build(name: &str, cfg: &SourceConfig) -> anyhow::Result<Box<dyn Source>> {
    match cfg {
        SourceConfig::Static(c) => Ok(Box::new(static_values::StaticSource::new(name, c))),
        SourceConfig::Aircraft(c) => Ok(Box::new(aircraft::AircraftSource::new(name, c))),
        SourceConfig::HomeAssistant(c) => Ok(Box::new(ha::HaSource::new(c)?)),
        SourceConfig::Mqtt(c) => Ok(Box::new(mqtt::MqttSource::new(name, c)?)),
    }
}

/// Fail early, at `run` start, when a source needs an environment variable
/// (a token) that is not set. Names the variable, never a value.
pub fn check_environment(cfg: &Config) -> anyhow::Result<()> {
    for (name, scfg) in &cfg.sources {
        match scfg {
            SourceConfig::HomeAssistant(c) => {
                ha::read_token(&c.token_env)
                    .map_err(|e| anyhow::anyhow!("[sources.{name}]: {e}"))?;
            }
            SourceConfig::Mqtt(c) => {
                if let Some(var) = &c.password_env {
                    mqtt::read_password(var)
                        .map_err(|e| anyhow::anyhow!("[sources.{name}]: {e}"))?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// HTML credits that data providers' terms require, one per provider in use
/// (shown on the preview page).
pub fn attributions(cfg: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    if cfg
        .sources
        .values()
        .any(|s| matches!(s, SourceConfig::Aircraft(_)))
    {
        out.push(aircraft::ATTRIBUTION_HTML);
    }
    out
}

/// Everything a running source gets.
pub struct SourceCtx {
    pub name: Arc<str>,
    pub store: StoreWriter,
    pub shutdown: Shutdown,
    pub health: Arc<SourceHealth>,
}

impl SourceCtx {
    /// Record a successful fetch or update.
    pub fn ok(&self) {
        self.health.up.store(true, Ordering::Relaxed);
        self.health
            .last_ok_unix_ms
            .store(unix_ms(SystemTime::now()), Ordering::Relaxed);
    }

    /// Record a recoverable error (logged by the caller). Marks the source
    /// down until the next [`SourceCtx::ok`].
    pub fn error(&self) {
        self.health.up.store(false, Ordering::Relaxed);
        self.health.errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// Cancellation signal shared by all sources.
#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves when shutdown starts.
    pub async fn cancelled(&mut self) {
        // An error means the sender is gone, which is also shutdown.
        let _ = self.0.wait_for(|&stop| stop).await;
    }

    /// Sleep for `d`, or less if shutdown starts. Returns `false` on shutdown,
    /// so poll loops read `while ctx.shutdown.sleep(interval).await { … }`.
    pub async fn sleep(&mut self, d: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(d) => !self.is_cancelled(),
            () = self.cancelled() => false,
        }
    }
}

/// Health of one source, for logs now and `/metrics` in M5
/// (`source_up`, `source_errors_total`, `source_last_update_timestamp`).
#[derive(Debug, Default)]
pub struct SourceHealth {
    pub up: AtomicBool,
    pub errors: AtomicU64,
    pub restarts: AtomicU64,
    /// Last [`SourceCtx::ok`], milliseconds since the Unix epoch; 0 = never.
    pub last_ok_unix_ms: AtomicU64,
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Restart backoff: 1 s doubling to 60 s, reset once a run lasted 60 s.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Runs every configured source and restarts failed ones.
pub struct Supervisor {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    pub health: BTreeMap<String, Arc<SourceHealth>>,
}

impl Supervisor {
    /// Start every source in `cfg`. Must be called inside a tokio runtime.
    pub fn start(cfg: &Config, store: &Store) -> Self {
        let (stop, rx) = watch::channel(false);
        let mut tasks = Vec::new();
        let mut health = BTreeMap::new();
        let referenced = cfg.referenced_keys();
        for (name, scfg) in &cfg.sources {
            let h = Arc::new(SourceHealth::default());
            health.insert(name.clone(), h.clone());
            let (n, mut c) = (name.clone(), scfg.clone());
            if let SourceConfig::HomeAssistant(ha) = &mut c {
                // Tell the source which keys the scenes read (see `ha.rs`).
                ha.referenced = referenced.clone();
            }
            tasks.push(tokio::spawn(supervise(
                name.clone(),
                move || build(&n, &c),
                store.writer(name),
                Shutdown(rx.clone()),
                h,
                BACKOFF_MIN,
            )));
        }
        if !cfg.sources.is_empty() {
            tracing::info!(count = cfg.sources.len(), "sources started");
        }
        Supervisor {
            stop,
            tasks,
            health,
        }
    }

    /// Signal shutdown and wait (up to `timeout`) for sources to finish.
    pub async fn stop(self, timeout: Duration) {
        let _ = self.stop.send(true);
        let all = futures_util::future::join_all(self.tasks);
        if tokio::time::timeout(timeout, all).await.is_err() {
            tracing::warn!("some sources did not stop within {timeout:?}");
        }
    }
}

async fn supervise(
    name: String,
    factory: impl Fn() -> anyhow::Result<Box<dyn Source>>,
    store: StoreWriter,
    mut shutdown: Shutdown,
    health: Arc<SourceHealth>,
    backoff_min: Duration,
) {
    let name: Arc<str> = name.into();
    let mut backoff = backoff_min;
    loop {
        let started = Instant::now();
        let result = match factory() {
            Ok(source) => {
                source
                    .run(SourceCtx {
                        name: name.clone(),
                        store: store.clone(),
                        shutdown: shutdown.clone(),
                        health: health.clone(),
                    })
                    .await
            }
            Err(e) => Err(e),
        };
        if shutdown.is_cancelled() {
            return;
        }
        health.up.store(false, Ordering::Relaxed);
        health.restarts.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(()) => tracing::warn!(source = %name, "source ended unexpectedly; restarting"),
            Err(e) => {
                health.errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(source = %name, error = %format!("{e:#}"), retry_in = ?backoff, "source failed");
            }
        }
        if started.elapsed() >= BACKOFF_MAX {
            backoff = backoff_min;
        }
        if !shutdown.sleep(backoff).await {
            return;
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// The shared HTTP client builder: rustls with the `ring` provider (see
/// [`crate::tls::init`]), a generic User-Agent (never anything identifying
/// the user), and sane timeouts. Sources may adjust it (e.g. a shorter
/// timeout) before `build()`.
pub fn http_client() -> reqwest::ClientBuilder {
    crate::tls::init();
    reqwest::Client::builder()
        .user_agent(concat!("paneld/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Value;

    fn cfg(sources: &str) -> Config {
        Config::parse(&format!(
            "[canvas]\nwidth = 8\nheight = 8\n[[scene]]\nname = \"a\"\n{sources}"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn static_source_writes_and_stops() {
        let store = Store::new();
        let sup = Supervisor::start(
            &cfg("[sources.demo]\ntype = \"static\"\nvalues = { \"demo:t\" = 21.5, \"demo:s\" = \"hi\", \"demo:b\" = true }\n"),
            &store,
        );
        for _ in 0..100 {
            if store.snapshot().len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let snap = store.snapshot();
        assert_eq!(
            snap["demo:t"].value,
            Value::Number {
                value: 21.5,
                unit: None
            }
        );
        assert_eq!(snap["demo:s"].value, Value::Text("hi".into()));
        assert_eq!(&*snap["demo:b"].source, "demo");
        assert!(sup.health["demo"].up.load(Ordering::Relaxed));
        sup.stop(Duration::from_secs(1)).await;
    }

    /// A source that fails immediately, counting its runs.
    struct Failing(Arc<AtomicU64>);
    impl Source for Failing {
        fn run(self: Box<Self>, _ctx: SourceCtx) -> SourceFuture {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("boom")
            })
        }
    }

    #[tokio::test]
    async fn failing_source_is_restarted_with_backoff_and_stops_on_shutdown() {
        let runs = Arc::new(AtomicU64::new(0));
        let (stop, rx) = watch::channel(false);
        let health = Arc::new(SourceHealth::default());
        let r = runs.clone();
        let task = tokio::spawn(supervise(
            "f".into(),
            move || Ok(Box::new(Failing(r.clone())) as Box<dyn Source>),
            Store::new().writer("f"),
            Shutdown(rx),
            health.clone(),
            Duration::from_millis(1),
        ));
        // Backoff 1, 2, 4, 8, 16 ms...: several restarts within 100 ms, not hundreds.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("stops promptly")
            .unwrap();
        let n = runs.load(Ordering::Relaxed);
        assert!((3..=10).contains(&n), "runs = {n}");
        assert_eq!(health.restarts.load(Ordering::Relaxed), n);
        assert_eq!(health.errors.load(Ordering::Relaxed), n);
        assert!(!health.up.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_source_that_cannot_be_built_is_retried() {
        let attempts = Arc::new(AtomicU64::new(0));
        let (stop, rx) = watch::channel(false);
        let a = attempts.clone();
        let task = tokio::spawn(supervise(
            "b".into(),
            move || {
                a.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("bad config")
            },
            Store::new().writer("b"),
            Shutdown(rx),
            Arc::new(SourceHealth::default()),
            Duration::from_millis(1),
        ));
        // Wait for the condition, not a fixed time: CI machines can be slow.
        let deadline = Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = stop.send(true);
        task.await.unwrap();
        assert!(attempts.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test]
    async fn supervisor_with_no_sources_stops_cleanly() {
        let sup = Supervisor::start(&cfg(""), &Store::new());
        assert!(sup.health.is_empty());
        sup.stop(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn shutdown_sleep_returns_early() {
        let (stop, rx) = watch::channel(false);
        let mut s = Shutdown(rx);
        let t = tokio::spawn(async move { s.sleep(Duration::from_secs(60)).await });
        let _ = stop.send(true);
        let finished = tokio::time::timeout(Duration::from_secs(1), t).await;
        assert!(!finished.expect("returns early").unwrap());
    }
}
