//! Prometheus metrics at `GET /metrics` (DESIGN.md §10).
//!
//! Three parts:
//! - [`Histogram`]: a lock-free histogram the render loop and the sinks write
//!   to (atomics only, never blocks).
//! - [`Encoder`]: the Prometheus text exposition format, version 0.0.4,
//!   written by hand (it is small; no crate needed). Reference:
//!   <https://prometheus.io/docs/instrumenting/exposition_formats/>.
//! - [`MetricsRegistry`]: gathers every stats source at scrape time into a
//!   plain [`Snapshot`], which [`Snapshot::encode`] turns into text. The
//!   render loop never touches the registry; it only bumps its own atomics.
//!
//! Names follow <https://prometheus.io/docs/practices/naming/>: `paneld_`
//! prefix, base units (seconds), `_total` on counters, `_timestamp_seconds`
//! for points in time, `_build_info` for the build metadata gauge.
//!
//! Labels carry only names from the config (interface, source name, scene
//! name) and the crate version. Never put URLs, locations, tokens or store
//! values in a label or a sample.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::colorlight_sink::SinkStats;
use crate::control::Control;
use crate::engine::RenderStats;
use crate::mqtt_ha::CardCheck;
use crate::reload::ReloadStats;
use crate::sources::{SourceHealth, Supervisor};
use crate::store::Store;

/// `Content-Type` of the text format 0.0.4.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Render time buckets, in seconds: 0.5 ms to 100 ms around a 33 ms frame
/// budget at 30 fps.
pub const RENDER_BUCKETS: &[f64] = &[0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.033, 0.05, 0.1];

/// Sink send time buckets, in seconds: 0.1 ms to 50 ms.
pub const SEND_BUCKETS: &[f64] = &[
    0.0001, 0.0002, 0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05,
];

/// How long a scrape waits for the supervisor lock (a config reload holds it
/// while it stops sources) before it serves the source list from the last
/// scrape instead.
const SUPERVISOR_WAIT: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Histogram

/// A fixed-bucket histogram of durations, written with atomics only.
///
/// `observe` is a handful of relaxed atomic adds, so it is safe on the render
/// thread. Each bucket holds its own count (not cumulative); the cumulative
/// view is built at scrape time. The sum is kept in whole nanoseconds.
#[derive(Debug)]
pub struct Histogram {
    /// Upper bounds in seconds, strictly increasing. An implicit `+Inf`
    /// bucket follows.
    bounds: &'static [f64],
    /// `bounds.len() + 1` counters; the last one is the `+Inf` overflow.
    counts: Vec<AtomicU64>,
    sum_ns: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        debug_assert!(bounds.windows(2).all(|w| w[0] < w[1]));
        Histogram {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_ns: AtomicU64::new(0),
        }
    }

    /// Record one observation. Lock-free.
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        let i = self
            .bounds
            .iter()
            .position(|&b| secs <= b)
            .unwrap_or(self.bounds.len());
        self.counts[i].fetch_add(1, Ordering::Relaxed);
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
    }

    /// A copy of the current state. Buckets are read one by one while the
    /// writer may run, so the sum can lag the counts by a frame; the count
    /// is always the total of the buckets read, so `+Inf` equals `_count`.
    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            bounds: self.bounds,
            counts: self
                .counts
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect(),
            sum_seconds: self.sum_ns.load(Ordering::Relaxed) as f64 / 1e9,
        }
    }
}

/// Plain-data copy of a [`Histogram`].
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramSnapshot {
    pub bounds: &'static [f64],
    /// Per-bucket counts, `bounds.len() + 1` long (the last is overflow).
    pub counts: Vec<u64>,
    pub sum_seconds: f64,
}

// ---------------------------------------------------------------------------
// Text exposition format 0.0.4

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// Writes metric families in the text format. Call [`Encoder::family`] once
/// per metric, then its samples; all lines of a family stay together.
#[derive(Debug, Default)]
pub struct Encoder {
    out: String,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a family: the `# HELP` and `# TYPE` lines.
    pub fn family(&mut self, name: &str, help: &str, kind: Kind) {
        let _ = writeln!(self.out, "# HELP {name} {}", escape_help(help));
        let _ = writeln!(self.out, "# TYPE {name} {}", kind.as_str());
    }

    /// One sample line. `name` is the family name, or for a histogram the
    /// family name plus `_bucket`/`_sum`/`_count`.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let _ = write!(self.out, "{k}=\"{}\"", escape_label_value(v));
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {}", format_value(value));
    }

    /// The `_bucket` (cumulative, ending in `+Inf`), `_sum` and `_count`
    /// lines of one histogram series.
    pub fn histogram(&mut self, name: &str, labels: &[(&str, &str)], h: &HistogramSnapshot) {
        let bucket = format!("{name}_bucket");
        let mut cumulative = 0u64;
        for (i, count) in h.counts.iter().enumerate() {
            cumulative += count;
            let le = match h.bounds.get(i) {
                Some(b) => format_value(*b),
                None => "+Inf".to_owned(),
            };
            let mut l: Vec<(&str, &str)> = labels.to_vec();
            l.push(("le", &le));
            self.sample(&bucket, &l, cumulative as f64);
        }
        self.sample(&format!("{name}_sum"), labels, h.sum_seconds);
        self.sample(&format!("{name}_count"), labels, cumulative as f64);
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// A sample value: Go `ParseFloat` syntax, `+Inf`/`-Inf`/`NaN` spelled as
/// the format wants.
fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else {
        // Rust's shortest round-trip form, never an exponent.
        format!("{v}")
    }
}

/// Label values escape backslash, double quote and line feed.
fn escape_label_value(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o
}

/// HELP text escapes backslash and line feed.
fn escape_help(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

// ---------------------------------------------------------------------------
// Snapshot: everything one scrape reports, as plain data

#[derive(Debug, Clone, PartialEq)]
pub struct SinkSample {
    /// Network interface (the `sink` and `card` label).
    pub name: String,
    pub send: HistogramSnapshot,
    pub errors: u64,
    /// Frames were sent without error since the previous scrape.
    pub card_detected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceSample {
    pub name: String,
    pub up: bool,
    pub errors: u64,
    pub restarts: u64,
    /// Seconds since the Unix epoch of the last success; `None` = never.
    pub last_update_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub version: String,
    pub frames_rendered: u64,
    pub frames_skipped: u64,
    pub render: HistogramSnapshot,
    pub sinks: Vec<SinkSample>,
    pub sources: Vec<SourceSample>,
    pub store_keys: u64,
    pub notifications_queued: u64,
    pub reloads: u64,
    pub reload_errors: u64,
    /// Scene on screen; `None` while black or an inline notification shows.
    pub scene: Option<String>,
    pub on: bool,
    pub brightness: u8,
    pub dim: u8,
    pub fps: f64,
}

impl Snapshot {
    /// The scrape body.
    pub fn encode(&self) -> String {
        let mut e = Encoder::new();
        let n = |v: u64| v as f64;

        e.family(
            "paneld_build_info",
            "Build information; the value is always 1.",
            Kind::Gauge,
        );
        e.sample("paneld_build_info", &[("version", &self.version)], 1.0);

        e.family(
            "paneld_frames_rendered_total",
            "Frames rendered.",
            Kind::Counter,
        );
        e.sample("paneld_frames_rendered_total", &[], n(self.frames_rendered));
        e.family(
            "paneld_frames_skipped_total",
            "Frame ticks skipped because rendering overran its period.",
            Kind::Counter,
        );
        e.sample("paneld_frames_skipped_total", &[], n(self.frames_skipped));
        e.family(
            "paneld_render_duration_seconds",
            "Time to build one frame, excluding the wait for the next tick.",
            Kind::Histogram,
        );
        e.histogram("paneld_render_duration_seconds", &[], &self.render);
        e.family("paneld_fps", "Frames rendered per second.", Kind::Gauge);
        e.sample("paneld_fps", &[], self.fps);

        e.family(
            "paneld_sink_send_duration_seconds",
            "Time to send one frame to a Colorlight card, by output interface.",
            Kind::Histogram,
        );
        for s in &self.sinks {
            e.histogram(
                "paneld_sink_send_duration_seconds",
                &[("sink", &s.name)],
                &s.send,
            );
        }
        e.family(
            "paneld_sink_errors_total",
            "Frames that failed to send, by output interface.",
            Kind::Counter,
        );
        for s in &self.sinks {
            e.sample(
                "paneld_sink_errors_total",
                &[("sink", &s.name)],
                n(s.errors),
            );
        }
        e.family(
            "paneld_card_detected",
            "1 if the card's sink sent frames without error since the previous scrape.",
            Kind::Gauge,
        );
        for s in &self.sinks {
            let v = if s.card_detected { 1.0 } else { 0.0 };
            e.sample("paneld_card_detected", &[("card", &s.name)], v);
        }

        e.family(
            "paneld_source_up",
            "1 if the source's last attempt succeeded.",
            Kind::Gauge,
        );
        for s in &self.sources {
            let v = if s.up { 1.0 } else { 0.0 };
            e.sample("paneld_source_up", &[("source", &s.name)], v);
        }
        e.family(
            "paneld_source_errors_total",
            "Errors reported by a source; resets when the source is restarted by a config reload.",
            Kind::Counter,
        );
        for s in &self.sources {
            e.sample(
                "paneld_source_errors_total",
                &[("source", &s.name)],
                n(s.errors),
            );
        }
        e.family(
            "paneld_source_restarts_total",
            "Times the supervisor restarted a failed source.",
            Kind::Counter,
        );
        for s in &self.sources {
            e.sample(
                "paneld_source_restarts_total",
                &[("source", &s.name)],
                n(s.restarts),
            );
        }
        e.family(
            "paneld_source_last_update_timestamp_seconds",
            "Unix time of the source's last success; no sample until it first succeeds.",
            Kind::Gauge,
        );
        for s in &self.sources {
            if let Some(t) = s.last_update_seconds {
                e.sample(
                    "paneld_source_last_update_timestamp_seconds",
                    &[("source", &s.name)],
                    t,
                );
            }
        }

        e.family("paneld_store_keys", "Keys in the data store.", Kind::Gauge);
        e.sample("paneld_store_keys", &[], n(self.store_keys));
        e.family(
            "paneld_notifications_queued",
            "Notifications waiting or showing.",
            Kind::Gauge,
        );
        e.sample(
            "paneld_notifications_queued",
            &[],
            n(self.notifications_queued),
        );

        e.family(
            "paneld_config_reloads_total",
            "Config reloads that were accepted.",
            Kind::Counter,
        );
        e.sample("paneld_config_reloads_total", &[], n(self.reloads));
        e.family(
            "paneld_config_reload_errors_total",
            "Config reloads that were rejected; the old config kept running.",
            Kind::Counter,
        );
        e.sample(
            "paneld_config_reload_errors_total",
            &[],
            n(self.reload_errors),
        );

        e.family(
            "paneld_current_scene",
            "1 for the scene on screen; no sample while black or an inline notification shows.",
            Kind::Gauge,
        );
        if let Some(scene) = &self.scene {
            e.sample("paneld_current_scene", &[("scene", scene)], 1.0);
        }
        e.family(
            "paneld_power_on",
            "1 if the panel is switched on.",
            Kind::Gauge,
        );
        e.sample("paneld_power_on", &[], if self.on { 1.0 } else { 0.0 });
        e.family(
            "paneld_brightness",
            "User brightness, 0 to 255.",
            Kind::Gauge,
        );
        e.sample("paneld_brightness", &[], f64::from(self.brightness));
        e.family(
            "paneld_dim",
            "Auto-dim factor, 0 to 255 (255 = no dimming).",
            Kind::Gauge,
        );
        e.sample("paneld_dim", &[], f64::from(self.dim));

        e.finish()
    }
}

// ---------------------------------------------------------------------------
// Registry

struct SinkEntry {
    name: String,
    stats: Arc<SinkStats>,
    /// Previous scrape's counters, for `card_detected`.
    check: Mutex<CardCheck>,
}

/// Collects the daemon's stats at scrape time.
pub struct MetricsRegistry {
    render: Arc<RenderStats>,
    sinks: Vec<SinkEntry>,
    supervisor: Arc<tokio::sync::Mutex<Supervisor>>,
    /// Source health as of the last scrape that got the supervisor lock.
    last_sources: Mutex<BTreeMap<String, Arc<SourceHealth>>>,
    reload: Arc<ReloadStats>,
    control: Control,
    store: Store,
}

impl MetricsRegistry {
    /// `sinks` is `(interface, stats)` for each Colorlight sink.
    pub fn new(
        render: Arc<RenderStats>,
        sinks: Vec<(String, Arc<SinkStats>)>,
        supervisor: Arc<tokio::sync::Mutex<Supervisor>>,
        reload: Arc<ReloadStats>,
        control: Control,
        store: Store,
    ) -> Arc<Self> {
        Arc::new(MetricsRegistry {
            render,
            sinks: sinks
                .into_iter()
                .map(|(name, stats)| SinkEntry {
                    name,
                    stats,
                    check: Mutex::new(CardCheck::default()),
                })
                .collect(),
            supervisor,
            last_sources: Mutex::new(BTreeMap::new()),
            reload,
            control,
            store,
        })
    }

    /// Read every stat once. Waits at most [`SUPERVISOR_WAIT`] for the
    /// supervisor and never holds its lock beyond cloning the health list.
    pub async fn collect(&self) -> Snapshot {
        let health = match tokio::time::timeout(SUPERVISOR_WAIT, self.supervisor.lock()).await {
            Ok(sup) => {
                let h = sup.health.clone();
                drop(sup);
                *self.last_sources.lock().unwrap_or_else(|e| e.into_inner()) = h.clone();
                h
            }
            Err(_) => self
                .last_sources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        };
        let status = self.control.status();
        let ord = Ordering::Relaxed;
        Snapshot {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            frames_rendered: self.render.frames_rendered.load(ord),
            frames_skipped: self.render.frames_skipped.load(ord),
            render: self.render.render_duration.snapshot(),
            sinks: self
                .sinks
                .iter()
                .map(|s| SinkSample {
                    name: s.name.clone(),
                    send: s.stats.send_duration.snapshot(),
                    errors: s.stats.send_errors.load(ord),
                    card_detected: s
                        .check
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .sample(&s.stats),
                })
                .collect(),
            sources: health
                .iter()
                .map(|(name, h)| {
                    let ms = h.last_ok_unix_ms.load(ord);
                    SourceSample {
                        name: name.clone(),
                        up: h.up.load(ord),
                        errors: h.errors.load(ord),
                        restarts: h.restarts.load(ord),
                        last_update_seconds: (ms > 0).then(|| ms as f64 / 1000.0),
                    }
                })
                .collect(),
            store_keys: self.store.snapshot().len() as u64,
            notifications_queued: status.notifications.len() as u64,
            reloads: self.reload.reloads.load(ord),
            reload_errors: self.reload.reload_errors.load(ord),
            scene: status.scene.clone(),
            on: status.on,
            brightness: status.brightness,
            dim: status.dim,
            // The render loop already rounds to 0.1; undo the f32 noise.
            fps: (f64::from(status.fps) * 10.0).round() / 10.0,
        }
    }
}

/// `GET /metrics`. Never authenticated (DESIGN.md §7.1); mount it outside
/// the API router's auth layer.
pub fn router(registry: Arc<MetricsRegistry>) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(registry)
}

async fn scrape(State(r): State<Arc<MetricsRegistry>>) -> Response {
    let body = r.collect().await.encode();
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE))],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{self, ApiSettings};
    use crate::control::channel;
    use crate::sources::ha::Secret;
    use std::future::IntoFuture;

    // -- a small parser-based checker for the text format ------------------

    /// Parse `{k="v",...}` starting at `s` (which begins with `{`); returns
    /// the labels and the rest after `}`.
    type Labels = Vec<(String, String)>;

    fn parse_labels(s: &str) -> Result<(Labels, &str), String> {
        let mut labels = Vec::new();
        let body = &s[1..];
        let mut pos = 0;
        loop {
            // name
            let rest = &body[pos..];
            if let Some(r) = rest.strip_prefix('}') {
                return Ok((labels, r));
            }
            let eq = rest.find('=').ok_or("label without =")?;
            let name = &rest[..eq];
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || name.starts_with(|c: char| c.is_ascii_digit())
            {
                return Err(format!("bad label name {name:?}"));
            }
            let after = &rest[eq + 1..];
            let after = after.strip_prefix('"').ok_or("label value not quoted")?;
            let mut val = String::new();
            let mut chars = after.char_indices();
            let end;
            loop {
                match chars.next() {
                    None => return Err("unterminated label value".into()),
                    Some((i, '"')) => {
                        end = i;
                        break;
                    }
                    Some((_, '\\')) => match chars.next() {
                        Some((_, '\\')) => val.push('\\'),
                        Some((_, '"')) => val.push('"'),
                        Some((_, 'n')) => val.push('\n'),
                        other => return Err(format!("bad escape {other:?}")),
                    },
                    Some((_, '\n')) => return Err("raw newline in label".into()),
                    Some((_, c)) => val.push(c),
                }
            }
            labels.push((name.to_owned(), val));
            pos += eq + 1 + 1 + end + 1;
            let rest = &body[pos..];
            if rest.starts_with(',') {
                pos += 1;
            } else if !rest.starts_with('}') {
                return Err("expected , or }".into());
            }
        }
    }

    fn parse_value(s: &str) -> Result<f64, String> {
        match s {
            "NaN" => Ok(f64::NAN),
            "+Inf" => Ok(f64::INFINITY),
            "-Inf" => Ok(f64::NEG_INFINITY),
            _ => s.parse::<f64>().map_err(|e| format!("value {s:?}: {e}")),
        }
    }

    #[derive(Default)]
    struct Family {
        kind: String,
        samples: usize,
    }

    /// Check `text` against the 0.0.4 rules that matter to a scraper: line
    /// syntax, one HELP/TYPE per family with TYPE first, families
    /// contiguous, unique series, and for histograms increasing `le`,
    /// cumulative counts and `+Inf` == `_count`. Returns family name to
    /// (type, sample count).
    fn validate(text: &str) -> Result<BTreeMap<String, (String, usize)>, String> {
        if !text.ends_with('\n') {
            return Err("no trailing newline".into());
        }
        let mut fams: BTreeMap<String, Family> = BTreeMap::new();
        let mut helps = std::collections::BTreeSet::new();
        let mut current: Option<String> = None;
        let mut closed = std::collections::BTreeSet::new();
        let mut series = std::collections::BTreeSet::new();
        // histogram state per (family, labels without le)
        #[derive(Default)]
        struct H {
            last_le: Option<f64>,
            last_count: f64,
            inf: Option<f64>,
            count: Option<f64>,
            sum: bool,
        }
        let mut hist: BTreeMap<(String, Vec<(String, String)>), H> = BTreeMap::new();

        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest.split(' ').next().unwrap();
                if !helps.insert(name.to_owned()) {
                    return Err(format!("two HELP for {name}"));
                }
                if let Some(c) = &current {
                    if c != name {
                        closed.insert(c.clone());
                    }
                }
                if closed.contains(name) {
                    return Err(format!("{name} is not contiguous"));
                }
                current = Some(name.to_owned());
                continue;
            }
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').ok_or("bad TYPE")?;
                if !["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind) {
                    return Err(format!("unknown type {kind}"));
                }
                if fams.contains_key(name) {
                    return Err(format!("two TYPE for {name}"));
                }
                if current.as_deref() != Some(name) {
                    return Err(format!("TYPE {name} outside its group"));
                }
                fams.insert(
                    name.to_owned(),
                    Family {
                        kind: kind.to_owned(),
                        samples: 0,
                    },
                );
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            // sample
            let name_end = line.find(['{', ' ']).ok_or("sample without value")?;
            let name = &line[..name_end];
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
            {
                return Err(format!("bad metric name {name:?}"));
            }
            let (labels, rest) = if line[name_end..].starts_with('{') {
                parse_labels(&line[name_end..])?
            } else {
                (Vec::new(), &line[name_end..])
            };
            let value = parse_value(rest.strip_prefix(' ').ok_or("no space before value")?)?;
            let cur = current.clone().ok_or("sample before any family")?;
            let fam = fams.get_mut(&cur).ok_or("sample before TYPE")?;
            let suffix = name
                .strip_prefix(cur.as_str())
                .ok_or_else(|| format!("sample {name} is not in family {cur}"))?;
            let ok_suffix = match fam.kind.as_str() {
                "histogram" => ["_bucket", "_sum", "_count"].contains(&suffix),
                _ => suffix.is_empty(),
            };
            if !ok_suffix {
                return Err(format!("sample {name} does not fit {} {cur}", fam.kind));
            }
            if !series.insert(line[..line.len() - rest.len()].to_owned()) {
                return Err(format!("duplicate series {line}"));
            }
            fam.samples += 1;
            if fam.kind == "histogram" {
                let le = labels.iter().find(|(k, _)| k == "le").map(|(_, v)| v);
                let base: Vec<_> = labels.iter().filter(|(k, _)| k != "le").cloned().collect();
                let h = hist.entry((cur.clone(), base)).or_default();
                match suffix {
                    "_bucket" => {
                        let le = le.ok_or("bucket without le")?;
                        let b = parse_value(le)?;
                        if h.last_le.is_some_and(|p| b <= p) {
                            return Err(format!("{cur}: le not increasing at {le}"));
                        }
                        if value < h.last_count {
                            return Err(format!("{cur}: buckets not cumulative at {le}"));
                        }
                        h.last_le = Some(b);
                        h.last_count = value;
                        if b.is_infinite() {
                            h.inf = Some(value);
                        }
                    }
                    "_sum" => h.sum = true,
                    _ => h.count = Some(value),
                }
            } else if value.is_nan() {
                return Err("NaN sample".into());
            }
        }
        for ((fam, labels), h) in &hist {
            if h.inf.is_none() || h.inf != h.count || !h.sum {
                return Err(format!("{fam}{labels:?}: +Inf/_count/_sum mismatch"));
            }
        }
        for name in &helps {
            if !fams.contains_key(name) {
                return Err(format!("{name} has HELP but no TYPE"));
            }
        }
        Ok(fams
            .into_iter()
            .map(|(k, f)| (k, (f.kind, f.samples)))
            .collect())
    }

    // -- encoder ------------------------------------------------------------

    #[test]
    fn escaping() {
        assert_eq!(escape_label_value("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
        assert_eq!(escape_help("a\\b\nc\"d"), "a\\\\b\\nc\"d");
        let mut e = Encoder::new();
        e.family("paneld_x", "line one\nline \\ two", Kind::Gauge);
        e.sample("paneld_x", &[("scene", "we\"ird\\\nname"), ("b", "ü")], 1.0);
        let out = e.finish();
        assert_eq!(
            out,
            "# HELP paneld_x line one\\nline \\\\ two\n# TYPE paneld_x gauge\n\
             paneld_x{scene=\"we\\\"ird\\\\\\nname\",b=\"ü\"} 1\n"
        );
        validate(&out).unwrap();
        let line = out.lines().last().unwrap();
        let (labels, _) = parse_labels(&line["paneld_x".len()..]).unwrap();
        assert_eq!(labels[0].1, "we\"ird\\\nname");
    }

    #[test]
    fn values() {
        assert_eq!(format_value(1.0), "1");
        assert_eq!(format_value(0.0005), "0.0005");
        assert_eq!(format_value(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(1e-9), "0.000000001");
    }

    #[test]
    fn histogram_is_cumulative_with_inf_sum_and_count() {
        static B: &[f64] = &[0.001, 0.01, 0.1];
        let h = Histogram::new(B);
        h.observe(Duration::from_micros(500)); // <= 0.001
        h.observe(Duration::from_millis(1)); // exactly on the bound: le is inclusive
        h.observe(Duration::from_millis(5)); // <= 0.01
        h.observe(Duration::from_millis(500)); // overflow
        let snap = h.snapshot();
        assert_eq!(snap.counts, [2, 1, 0, 1]);
        assert!((snap.sum_seconds - 0.5065).abs() < 1e-9);
        let mut e = Encoder::new();
        e.family("d_seconds", "d", Kind::Histogram);
        e.histogram("d_seconds", &[("sink", "eth1")], &snap);
        let out = e.finish();
        assert_eq!(
            out,
            "# HELP d_seconds d\n# TYPE d_seconds histogram\n\
             d_seconds_bucket{sink=\"eth1\",le=\"0.001\"} 2\n\
             d_seconds_bucket{sink=\"eth1\",le=\"0.01\"} 3\n\
             d_seconds_bucket{sink=\"eth1\",le=\"0.1\"} 3\n\
             d_seconds_bucket{sink=\"eth1\",le=\"+Inf\"} 4\n\
             d_seconds_sum{sink=\"eth1\"} 0.5065\n\
             d_seconds_count{sink=\"eth1\"} 4\n"
        );
        validate(&out).unwrap();
    }

    #[test]
    fn empty_histogram_is_valid() {
        let h = Histogram::new(RENDER_BUCKETS);
        let mut e = Encoder::new();
        e.family("r_seconds", "r", Kind::Histogram);
        e.histogram("r_seconds", &[], &h.snapshot());
        let out = e.finish();
        assert!(out.contains("r_seconds_bucket{le=\"+Inf\"} 0\n"));
        assert!(out.contains("r_seconds_sum 0\nr_seconds_count 0\n"));
        validate(&out).unwrap();
    }

    #[test]
    fn checker_rejects_bad_output() {
        // Guard against a checker that accepts everything.
        assert!(validate("# TYPE a gauge\na 1").is_err(), "no final newline");
        assert!(validate("a 1\n").is_err(), "no family");
        assert!(validate("# HELP a x\n# TYPE a gauge\na 1\na 2\n").is_err());
        assert!(validate("# HELP a x\n# TYPE a gauge\na{b=\"c\\q\"} 1\n").is_err());
        let bad = "# HELP h x\n# TYPE h histogram\nh_bucket{le=\"1\"} 2\n\
                   h_bucket{le=\"+Inf\"} 1\nh_sum 1\nh_count 1\n";
        assert!(validate(bad).is_err(), "not cumulative");
        let bad = "# HELP h x\n# TYPE h histogram\nh_bucket{le=\"+Inf\"} 2\nh_sum 1\nh_count 1\n";
        assert!(validate(bad).is_err(), "inf != count");
        let bad = "# HELP a x\n# TYPE a gauge\na 1\n# HELP b x\n# TYPE b gauge\nb 1\n\
                   # HELP a y\n# TYPE a gauge\na{x=\"1\"} 1\n";
        assert!(validate(bad).is_err(), "family split");
    }

    #[test]
    fn observe_from_threads_loses_nothing() {
        static B: &[f64] = &[0.001];
        let h = Arc::new(Histogram::new(B));
        let ts: Vec<_> = (0..4)
            .map(|_| {
                let h = h.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        h.observe(Duration::from_micros(10));
                    }
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        let s = h.snapshot();
        assert_eq!(s.counts, [4000, 0]);
        assert!((s.sum_seconds - 0.04).abs() < 1e-9);
    }

    // -- full scrape -------------------------------------------------------

    fn hist(counts: &[u64], bounds: &'static [f64], sum: f64) -> HistogramSnapshot {
        HistogramSnapshot {
            bounds,
            counts: counts.to_vec(),
            sum_seconds: sum,
        }
    }

    fn fixed_snapshot() -> Snapshot {
        Snapshot {
            version: "1.2.3".into(),
            frames_rendered: 900,
            frames_skipped: 2,
            render: hist(&[0, 850, 40, 8, 1, 0, 0, 1, 0, 0], RENDER_BUCKETS, 1.25),
            sinks: vec![
                SinkSample {
                    name: "eth1".into(),
                    send: hist(&[0, 0, 5, 880, 10, 3, 1, 1, 0, 0], SEND_BUCKETS, 0.9),
                    errors: 1,
                    card_detected: true,
                },
                SinkSample {
                    name: "wei\"rd\\if".into(),
                    send: hist(&[0; 10], SEND_BUCKETS, 0.0),
                    errors: 0,
                    card_detected: false,
                },
            ],
            sources: vec![
                SourceSample {
                    name: "ha".into(),
                    up: true,
                    errors: 3,
                    restarts: 1,
                    last_update_seconds: Some(1_767_225_600.5),
                },
                SourceSample {
                    name: "weather".into(),
                    up: false,
                    errors: 0,
                    restarts: 0,
                    last_update_seconds: None,
                },
            ],
            store_keys: 42,
            notifications_queued: 1,
            reloads: 4,
            reload_errors: 2,
            scene: Some("home".into()),
            on: true,
            brightness: 128,
            dim: 255,
            fps: 29.9,
        }
    }

    #[test]
    fn metrics_golden_scrape() {
        let text = fixed_snapshot().encode();
        validate(&text).unwrap();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/golden/metrics.txt"
        );
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(path, &text).unwrap();
        }
        let want = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{path}: {e} (run with UPDATE_GOLDEN=1 to create)"));
        assert_eq!(
            text, want,
            "metrics golden differs; UPDATE_GOLDEN=1 to regenerate"
        );
    }

    #[test]
    fn no_scene_means_no_current_scene_series() {
        let mut s = fixed_snapshot();
        s.scene = None;
        s.sources.clear();
        s.sinks.clear();
        let text = s.encode();
        let fams = validate(&text).unwrap();
        assert_eq!(fams["paneld_current_scene"], ("gauge".into(), 0));
        assert_eq!(fams["paneld_source_up"].1, 0);
        assert!(!text.contains("paneld_current_scene{"));
    }

    #[test]
    fn every_documented_family_is_present() {
        let fams = validate(&fixed_snapshot().encode()).unwrap();
        let want = [
            ("paneld_build_info", "gauge"),
            ("paneld_frames_rendered_total", "counter"),
            ("paneld_frames_skipped_total", "counter"),
            ("paneld_render_duration_seconds", "histogram"),
            ("paneld_fps", "gauge"),
            ("paneld_sink_send_duration_seconds", "histogram"),
            ("paneld_sink_errors_total", "counter"),
            ("paneld_card_detected", "gauge"),
            ("paneld_source_up", "gauge"),
            ("paneld_source_errors_total", "counter"),
            ("paneld_source_restarts_total", "counter"),
            ("paneld_source_last_update_timestamp_seconds", "gauge"),
            ("paneld_store_keys", "gauge"),
            ("paneld_notifications_queued", "gauge"),
            ("paneld_config_reloads_total", "counter"),
            ("paneld_config_reload_errors_total", "counter"),
            ("paneld_current_scene", "gauge"),
            ("paneld_power_on", "gauge"),
            ("paneld_brightness", "gauge"),
            ("paneld_dim", "gauge"),
        ];
        assert_eq!(fams.len(), want.len());
        for (name, kind) in want {
            assert_eq!(fams[name].0, kind, "{name}");
        }
    }

    // -- registry and HTTP ---------------------------------------------------

    struct Rig {
        registry: Arc<MetricsRegistry>,
        render: Arc<RenderStats>,
        sink: Arc<SinkStats>,
        reload: Arc<ReloadStats>,
        store: Store,
        link: crate::control::ControlLink,
        control: Control,
        health: Arc<SourceHealth>,
        supervisor: Arc<tokio::sync::Mutex<Supervisor>>,
    }

    async fn rig() -> Rig {
        let (control, link, _out) = channel(vec!["home".into()]);
        let render = Arc::new(RenderStats::default());
        let sink = Arc::new(SinkStats::default());
        let reload = Arc::new(ReloadStats::default());
        let store = Store::new();
        let health = Arc::new(SourceHealth::default());
        let cfg = crate::config::Config::parse(
            "[canvas]\nwidth = 8\nheight = 8\n[[scene]]\nname = \"home\"\n",
        )
        .unwrap();
        let mut sup = Supervisor::start(&cfg, &store);
        sup.health.insert("ha".into(), health.clone());
        let supervisor = Arc::new(tokio::sync::Mutex::new(sup));
        let registry = MetricsRegistry::new(
            render.clone(),
            vec![("eth1".into(), sink.clone())],
            supervisor.clone(),
            reload.clone(),
            control.clone(),
            store.clone(),
        );
        Rig {
            registry,
            render,
            sink,
            reload,
            store,
            link,
            control,
            health,
            supervisor,
        }
    }

    #[tokio::test]
    async fn registry_reports_sources_sinks_and_reloads_with_labels() {
        let r = rig().await;
        r.render.frames_rendered.store(10, Ordering::Relaxed);
        r.render.render_duration.observe(Duration::from_millis(3));
        r.sink.record(Duration::from_micros(300));
        r.health.up.store(true, Ordering::Relaxed);
        r.health.errors.store(5, Ordering::Relaxed);
        r.health
            .last_ok_unix_ms
            .store(1_700_000_000_250, Ordering::Relaxed);
        r.reload.reloads.store(7, Ordering::Relaxed);
        r.reload.reload_errors.store(3, Ordering::Relaxed);
        r.store
            .writer("t")
            .set("t:a", crate::store::Value::Text("x".into()));
        r.link.status.send_replace(Arc::new(crate::control::Status {
            scenes: vec!["home".into()],
            scene: Some("home".into()),
            on: true,
            brightness: 200,
            dim: 100,
            fps: 30.0,
            ..Default::default()
        }));

        let text = r.registry.collect().await.encode();
        validate(&text).unwrap();
        for line in [
            "paneld_frames_rendered_total 10",
            "paneld_render_duration_seconds_count 1",
            "paneld_sink_errors_total{sink=\"eth1\"} 0",
            "paneld_sink_send_duration_seconds_count{sink=\"eth1\"} 1",
            "paneld_source_up{source=\"ha\"} 1",
            "paneld_source_errors_total{source=\"ha\"} 5",
            "paneld_source_last_update_timestamp_seconds{source=\"ha\"} 1700000000.25",
            "paneld_config_reloads_total 7",
            "paneld_config_reload_errors_total 3",
            "paneld_store_keys 1",
            "paneld_current_scene{scene=\"home\"} 1",
            "paneld_brightness 200",
            "paneld_dim 100",
            "paneld_power_on 1",
            "paneld_fps 30",
            "paneld_notifications_queued 0",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in\n{text}"
            );
        }
        // The card check compares with the previous scrape.
        assert!(text.contains("paneld_card_detected{card=\"eth1\"} 1"));
        r.sink.record(Duration::from_micros(300));
        r.sink.send_errors.store(2, Ordering::Relaxed);
        let again = r.registry.collect().await.encode();
        assert!(
            again.contains("paneld_card_detected{card=\"eth1\"} 0"),
            "error in the window"
        );
        assert!(again.contains("paneld_sink_errors_total{sink=\"eth1\"} 2"));
        let _ = &r.control;
    }

    #[tokio::test]
    async fn a_busy_supervisor_does_not_block_a_scrape() {
        let r = rig().await;
        r.health.up.store(true, Ordering::Relaxed);
        let first = r.registry.collect().await;
        assert_eq!(first.sources.len(), 1);
        // A reload holds the lock for a long time.
        let _held = r.supervisor.lock().await;
        let started = std::time::Instant::now();
        let second = r.registry.collect().await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(second.sources, first.sources, "served from the last scrape");
    }

    async fn serve(token: Option<&str>, r: &Rig) -> std::net::SocketAddr {
        let settings = ApiSettings {
            token: token.map(Secret::for_test),
            data_prefix: "push".into(),
            info: api::ApiInfo {
                canvas: (8, 8),
                preview: false,
                app: true,
            },
        };
        let app =
            api::router(r.control.clone(), &r.store, settings).merge(router(r.registry.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app).into_future());
        addr
    }

    #[tokio::test]
    async fn metrics_is_open_with_and_without_an_api_token() {
        for token in [None, Some("s3cret-token")] {
            let r = rig().await;
            let addr = serve(token, &r).await;
            let client = crate::sources::http_client().build().unwrap();
            let resp = client
                .get(format!("http://{addr}/metrics"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "token {token:?}");
            assert_eq!(
                resp.headers()["content-type"],
                "text/plain; version=0.0.4; charset=utf-8"
            );
            let body = resp.text().await.unwrap();
            validate(&body).unwrap();
            assert!(body.contains("paneld_build_info{version=\""));
            assert!(!body.contains("s3cret-token"));
            if token.is_some() {
                let api = client
                    .get(format!("http://{addr}/api/state"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(api.status(), 401, "the API itself stays protected");
            }
        }
    }
}
