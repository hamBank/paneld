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

use std::collections::{BTreeMap, BTreeSet};
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

/// Fail early, at `run` start and before a reload restarts a source, when
/// one source needs an environment variable (a token or password) that is
/// not set. Names the variable, never a value. Variants with such needs
/// extend this.
pub fn check_source_environment(name: &str, scfg: &SourceConfig) -> anyhow::Result<()> {
    match scfg {
        SourceConfig::HomeAssistant(c) => {
            ha::read_token(&c.token_env).map_err(|e| anyhow::anyhow!("[sources.{name}]: {e}"))?;
        }
        SourceConfig::Mqtt(c) => {
            if let Some(var) = &c.password_env {
                mqtt::read_password(var).map_err(|e| anyhow::anyhow!("[sources.{name}]: {e}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// [`check_source_environment`] for every source in `cfg`.
pub fn check_environment(cfg: &Config) -> anyhow::Result<()> {
    for (name, scfg) in &cfg.sources {
        check_source_environment(name, scfg)?;
    }
    Ok(())
}

/// The config a source actually runs with: its `[sources.<name>]` entry plus
/// whatever it derives from the rest of the config. This is the single place
/// where a variant is tied to the whole config; both [`Supervisor::start`]
/// and [`Supervisor::reconcile`] use it, and reconcile restarts a source
/// exactly when this value changes (compared as its `Debug` text, which is
/// deterministic: the config types hold no hash maps and no secrets).
///
/// Home Assistant: the scenes' `ha:` keys (`referenced`), reduced to the
/// entities they name, so a scene edit that touches other sources' keys, or
/// the same entities, leaves it alone.
pub fn effective_config(scfg: &SourceConfig, referenced: &BTreeSet<String>) -> SourceConfig {
    let mut c = scfg.clone();
    if let SourceConfig::HomeAssistant(ha) = &mut c {
        ha.referenced = referenced
            .iter()
            .filter_map(|k| ha::entity_of_key(k).map(|e| format!("ha:{e}")))
            .collect();
    }
    c
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

/// One running source.
struct Running {
    /// `Debug` text of the [`effective_config`] it was started with.
    fingerprint: String,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// What [`Supervisor::reconcile`] did, by source name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub started: Vec<String>,
    pub restarted: Vec<String>,
    pub stopped: Vec<String>,
    /// Sources that could not be changed (e.g. a missing environment
    /// variable). The old source, if any, keeps running.
    pub errors: Vec<String>,
}

/// How long a single source gets to stop before its task is aborted.
const SOURCE_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs every configured source, restarts failed ones, and applies config
/// changes to exactly the sources that changed.
pub struct Supervisor {
    running: BTreeMap<String, Running>,
    pub health: BTreeMap<String, Arc<SourceHealth>>,
}

/// How long a restarted source gets to report its first success before its
/// leftover keys are swept anyway.
const STALE_SWEEP_TIMEOUT: Duration = Duration::from_secs(600);

/// After a restart: wait for the new instance's first [`SourceCtx::ok`] (or
/// `timeout`), then remove `name`'s keys last written before `restarted_at`.
async fn sweep_after_restart(
    name: String,
    health: Arc<SourceHealth>,
    store: Store,
    restarted_at: SystemTime,
    timeout: Duration,
) {
    let since = unix_ms(restarted_at);
    let deadline = Instant::now() + timeout;
    while health.last_ok_unix_ms.load(Ordering::Relaxed) < since && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let n = store.clear_source_older_than(&name, restarted_at);
    if n > 0 {
        tracing::info!("source {name}: {n} keys it no longer writes were cleared");
    }
}

impl Supervisor {
    /// Start every source in `cfg`. Must be called inside a tokio runtime.
    pub fn start(cfg: &Config, store: &Store) -> Self {
        let mut sup = Supervisor {
            running: BTreeMap::new(),
            health: BTreeMap::new(),
        };
        let referenced = cfg.referenced_keys();
        for (name, scfg) in &cfg.sources {
            sup.spawn(name, scfg, &referenced, store);
        }
        if !cfg.sources.is_empty() {
            tracing::info!(count = cfg.sources.len(), "sources started");
        }
        sup
    }

    fn spawn(
        &mut self,
        name: &str,
        scfg: &SourceConfig,
        referenced: &BTreeSet<String>,
        store: &Store,
    ) {
        let c = effective_config(scfg, referenced);
        let fingerprint = format!("{c:?}");
        let (stop, rx) = watch::channel(false);
        let h = Arc::new(SourceHealth::default());
        self.health.insert(name.to_owned(), h.clone());
        let n = name.to_owned();
        let task = tokio::spawn(supervise(
            name.to_owned(),
            move || build(&n, &c),
            store.writer(name),
            Shutdown(rx),
            h,
            BACKOFF_MIN,
        ));
        self.running.insert(
            name.to_owned(),
            Running {
                fingerprint,
                stop,
                task,
            },
        );
    }

    /// Stop one source and wait for it (aborting after [`SOURCE_STOP_TIMEOUT`]).
    async fn stop_one(r: Running) {
        let _ = r.stop.send(true);
        let mut task = r.task;
        if tokio::time::timeout(SOURCE_STOP_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            tracing::warn!("a source did not stop within {SOURCE_STOP_TIMEOUT:?}; aborting it");
            task.abort();
            let _ = task.await;
        }
    }

    /// Make the running sources match `new`, touching only what changed:
    ///
    /// - unchanged (same [`effective_config`]): left running, health intact;
    /// - changed: restarted; its old values stay until the new instance
    ///   reports success, then keys it no longer writes are removed;
    /// - removed: stopped and its store keys cleared;
    /// - added: started.
    ///
    /// A changed or added source whose environment check fails (a missing
    /// token variable) is skipped: the old source keeps running, and the
    /// problem is returned in [`Reconciled::errors`] for the caller to count.
    pub async fn reconcile(&mut self, new: &Config, store: &Store) -> Reconciled {
        let referenced = new.referenced_keys();
        let mut out = Reconciled::default();
        for (name, scfg) in &new.sources {
            let fingerprint = format!("{:?}", effective_config(scfg, &referenced));
            let changed = match self.running.get(name) {
                Some(r) if r.fingerprint == fingerprint => continue,
                Some(_) => true,
                None => false,
            };
            if let Err(e) = check_source_environment(name, scfg) {
                out.errors.push(format!("{e:#}"));
                continue;
            }
            let restarted_at = SystemTime::now();
            let restarting = match self.running.remove(name) {
                Some(old) => {
                    Self::stop_one(old).await;
                    true
                }
                None => false,
            };
            self.spawn(name, scfg, &referenced, store);
            if restarting {
                // Keep the old values on screen until the new instance has
                // written; then drop only keys it did not rewrite (ones the new
                // config no longer produces). Clearing at once would flash
                // placeholders and throw away e.g. a series the new instance
                // would have continued.
                if let Some(h) = self.health.get(name) {
                    tokio::spawn(sweep_after_restart(
                        name.clone(),
                        h.clone(),
                        store.clone(),
                        restarted_at,
                        STALE_SWEEP_TIMEOUT,
                    ));
                }
            }
            if changed {
                tracing::info!("source restarted: {name} (config changed)");
                out.restarted.push(name.clone());
            } else {
                tracing::info!("source started: {name}");
                out.started.push(name.clone());
            }
        }
        let gone: Vec<String> = self
            .running
            .keys()
            .filter(|n| !new.sources.contains_key(*n))
            .cloned()
            .collect();
        for name in gone {
            if let Some(old) = self.running.remove(&name) {
                Self::stop_one(old).await;
            }
            self.health.remove(&name);
            let n = store.clear_source(&name);
            tracing::info!("source stopped: {name} (removed from config; {n} keys cleared)");
            out.stopped.push(name);
        }
        out
    }

    /// Signal shutdown and wait (up to `timeout`) for sources to finish.
    pub async fn stop(mut self, timeout: Duration) {
        self.stop_all(timeout).await;
    }

    /// Like [`Supervisor::stop`], for a supervisor behind a lock.
    pub async fn stop_all(&mut self, timeout: Duration) {
        let running = std::mem::take(&mut self.running);
        let tasks: Vec<_> = running
            .into_values()
            .map(|r| {
                let _ = r.stop.send(true);
                r.task
            })
            .collect();
        let all = futures_util::future::join_all(tasks);
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

    // ---- reconcile ----

    fn stat(name: &str, vals: &str) -> String {
        format!("[sources.{name}]\ntype = \"static\"\nvalues = {{ {vals} }}\n")
    }

    async fn wait_for(store: &Store, key: &str, want: Option<Value>) {
        for _ in 0..400 {
            if store.snapshot().get(key).map(|e| e.value.clone()) == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{key} never became {want:?}");
    }

    fn num(v: f64) -> Option<Value> {
        Some(Value::Number {
            value: v,
            unit: None,
        })
    }

    #[tokio::test]
    async fn reconcile_leaves_unchanged_sources_alone_and_restarts_changed_ones() {
        let store = Store::new();
        let mut sup = Supervisor::start(
            &cfg(&(stat("a", "\"a:x\" = 1, \"a:old\" = 1") + &stat("b", "\"b:x\" = 1"))),
            &store,
        );
        wait_for(&store, "b:x", num(1.0)).await;
        wait_for(&store, "a:old", num(1.0)).await;
        let (ha, hb) = (sup.health["a"].clone(), sup.health["b"].clone());

        // `a` changes (and loses a key); `b` is textually reformatted only.
        let new = cfg(&(stat("a", "\"a:x\" = 2") + &stat("b", "  \"b:x\"   =   1  ")));
        let done = sup.reconcile(&new, &store).await;
        assert_eq!(
            done,
            Reconciled {
                restarted: vec!["a".into()],
                ..Default::default()
            }
        );
        wait_for(&store, "a:x", num(2.0)).await;
        // The dropped key goes once the new instance has written (no flash
        // of placeholders for keys it still produces).
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.snapshot().contains_key("a:old") && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(store.snapshot().get("a:old"), None, "old keys are cleared");
        assert!(!Arc::ptr_eq(&sup.health["a"], &ha), "a got a fresh task");
        assert!(Arc::ptr_eq(&sup.health["b"], &hb), "b was not restarted");
        assert_eq!(hb.restarts.load(Ordering::Relaxed), 0);
        assert_eq!(store.snapshot()["b:x"].value, num(1.0).unwrap());

        // Reconciling the same config again changes nothing.
        let again = sup.reconcile(&new, &store).await;
        assert_eq!(again, Reconciled::default());
        sup.stop(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn reconcile_adds_and_removes_sources_and_clears_removed_keys() {
        let store = Store::new();
        let mut sup = Supervisor::start(&cfg(&stat("a", "\"a:x\" = 1")), &store);
        wait_for(&store, "a:x", num(1.0)).await;
        // A key another source wrote must survive `a` going away.
        store.writer("other").set("other:k", Value::Bool(true));

        let done = sup.reconcile(&cfg(&stat("b", "\"b:x\" = 5")), &store).await;
        assert_eq!(done.started, ["b"]);
        assert_eq!(done.stopped, ["a"]);
        assert!(done.restarted.is_empty() && done.errors.is_empty());
        wait_for(&store, "b:x", num(5.0)).await;
        assert_eq!(store.snapshot().get("a:x"), None);
        assert!(store.snapshot().contains_key("other:k"));
        assert_eq!(sup.health.keys().collect::<Vec<_>>(), ["b"]);
        assert!(sup.health["b"].up.load(Ordering::Relaxed));
        sup.stop(Duration::from_secs(1)).await;
    }

    fn ha_cfg(scene_body: &str, extra: &str) -> Config {
        Config::parse(&format!(
            "[canvas]\nwidth = 64\nheight = 32\n[[scene]]\nname = \"a\"\n{scene_body}\n\
             [sources.ha]\ntype = \"home_assistant\"\nurl = \"ws://127.0.0.1:9/api/websocket\"\n\
             token_env = \"PANELD_TEST_RECONCILE_TOKEN\"\n{extra}\
             {}",
            stat("s", "\"s:x\" = 1")
        ))
        .unwrap()
    }

    fn zone(key: &str) -> String {
        format!(
            "[[scene.zone]]\nrect = {{ x = 0, y = 0, w = 64, h = 8 }}\nwidget = {{ type = \"value\", key = \"{key}\" }}\n"
        )
    }

    #[test]
    fn ha_fingerprint_depends_only_on_the_entities_scenes_reference() {
        let fp = |c: &Config| {
            let refs = c.referenced_keys();
            format!("{:?}", effective_config(&c.sources["ha"], &refs))
        };
        let base = fp(&ha_cfg(&zone("ha:sensor.a"), ""));
        // Same entity via an attribute or series suffix; other sources' keys;
        // a different widget: no change.
        assert_eq!(base, fp(&ha_cfg(&zone("ha:sensor.a.unit"), "")));
        assert_eq!(base, fp(&ha_cfg(&zone("ha:sensor.a@series"), "")));
        assert_eq!(
            base,
            fp(&ha_cfg(
                &(zone("ha:sensor.a") + &zone("aircraft:nearest.callsign")),
                ""
            ))
        );
        // A new entity, or a changed source setting: change.
        assert_ne!(
            base,
            fp(&ha_cfg(&(zone("ha:sensor.a") + &zone("ha:sensor.b")), ""))
        );
        assert_ne!(
            base,
            fp(&ha_cfg(&zone("ha:sensor.a"), "series_points = 10\n"))
        );
    }

    #[tokio::test]
    async fn reconcile_restarts_ha_only_when_its_referenced_entities_change() {
        std::env::set_var("PANELD_TEST_RECONCILE_TOKEN", "t");
        let store = Store::new();
        let mut sup = Supervisor::start(&ha_cfg(&zone("ha:sensor.a"), ""), &store);
        let (h_ha, h_s) = (sup.health["ha"].clone(), sup.health["s"].clone());

        // A scene edit that adds an unrelated key restarts nothing.
        let unrelated = ha_cfg(&(zone("ha:sensor.a") + &zone("s:x")), "");
        assert_eq!(
            sup.reconcile(&unrelated, &store).await,
            Reconciled::default()
        );
        assert!(Arc::ptr_eq(&sup.health["ha"], &h_ha));

        // Referencing a new HA entity restarts HA, and only HA.
        let new_entity = ha_cfg(&(zone("ha:sensor.a") + &zone("ha:sensor.b")), "");
        let done = sup.reconcile(&new_entity, &store).await;
        assert_eq!(done.restarted, ["ha"]);
        assert!(!Arc::ptr_eq(&sup.health["ha"], &h_ha));
        assert!(Arc::ptr_eq(&sup.health["s"], &h_s));
        sup.stop(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn reconcile_keeps_the_old_source_when_an_environment_variable_is_missing() {
        std::env::remove_var("PANELD_TEST_RECONCILE_MISSING");
        let store = Store::new();
        let mut sup = Supervisor::start(&cfg(&stat("a", "\"a:x\" = 1")), &store);
        wait_for(&store, "a:x", num(1.0)).await;
        let h = sup.health["a"].clone();

        let new = cfg(
            "[sources.a]\ntype = \"home_assistant\"\nurl = \"ws://127.0.0.1:9/api/websocket\"\n\
             token_env = \"PANELD_TEST_RECONCILE_MISSING\"\n",
        );
        let done = sup.reconcile(&new, &store).await;
        assert_eq!(done.errors.len(), 1);
        assert!(done.errors[0].contains("PANELD_TEST_RECONCILE_MISSING"));
        assert!(done.errors[0].contains("[sources.a]"));
        assert!(done.restarted.is_empty() && done.started.is_empty());
        assert!(Arc::ptr_eq(&sup.health["a"], &h));
        assert!(h.up.load(Ordering::Relaxed));
        assert_eq!(store.snapshot()["a:x"].value, num(1.0).unwrap());
        sup.stop(Duration::from_secs(1)).await;
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
