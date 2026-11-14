//! `type = "prometheus"`: PromQL instant and range queries against a
//! Prometheus server (DESIGN.md §6.4).
//!
//! # HTTP API (confirmed against the official docs,
//! <https://prometheus.io/docs/prometheus/latest/querying/api/>)
//! - Instant query: `GET {base}/api/v1/query?query=<promql>`
//!   ("Instant queries"). Range query:
//!   `GET {base}/api/v1/query_range?query=<promql>&start=<unix>&end=<unix>&step=<seconds>`
//!   ("Range queries"; `start`/`end` are RFC 3339 or unix timestamps,
//!   inclusive; `step` is a duration or float seconds).
//! - Envelope ("Format overview"): `{"status": "success" | "error",
//!   "data": ..., "errorType": "...", "error": "...", "warnings": [...]}`.
//!   2xx on success; 400 (bad parameters), 422 (expression cannot be
//!   executed) and 503 (timeout/abort) carry the error envelope.
//! - `data` is `{"resultType": ..., "result": ...}` ("Expression query result
//!   formats"): `vector` is `[{"metric": {...}, "value": [<unix_time>,
//!   "<sample_value>"]}, ...]`; `scalar` is `[<unix_time>, "<scalar_value>"]`;
//!   `matrix` (a range query) is `[{"metric": {...}, "values":
//!   [[<unix_time>, "<sample_value>"], ...]}, ...]`, sorted by metric;
//!   `string` also exists. Sample values are **strings** because JSON has no
//!   `NaN`/`Inf`. Native histograms appear as `histogram(s)` instead of
//!   `value(s)`; they are not supported and count as no value.
//!
//! # Result handling (per key)
//! - Instant, `scalar`: its value. `vector` with one series: its value.
//!   Empty vector: the key is removed. `NaN`/`Inf` (or a histogram): the key
//!   is removed. `vector` with 2 or more series: **nothing is written** for
//!   that key (the old value stays), a warning names the key and the series
//!   count, and `ctx.error()` is counted; aggregate in PromQL (`sum(...)`).
//! - Range: points of the **first** series, oldest first, as a `Series`
//!   (non-finite samples skipped); no series or no finite point removes the
//!   key.
//! - A 400/422 for one query (bad PromQL) is a per-key problem like the
//!   multi-series case. Transport errors, 5xx, 401/403 and bad bodies back
//!   off the whole source; 429 honours `Retry-After`.
//!
//! All queries of a round run concurrently, at most [`CONCURRENCY`] at once,
//! and everything that succeeded is written in one batch.
//!
//! The optional bearer token comes only from the environment variable named
//! by `bearer_env`; it is marked sensitive and never logged.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use futures_util::stream::{self, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::Value as Json;

use super::ha::read_secret;
use super::poll::{
    check_http_url, fetch_body, is_env_var_name, run_poll_loop, same_host_redirects, Fetch, Limits,
    Outcome, Timing,
};
use super::{Source, SourceCtx, SourceFuture};
use crate::config::PromConfig;
use crate::store::{Change, Value};

/// Queries in flight at once.
pub const CONCURRENCY: usize = 4;
/// Prometheus rejects range queries with more points per series than this.
const MAX_POINTS: f64 = 11_000.0;

pub fn validate(c: &PromConfig, at: &str, errs: &mut Vec<String>) {
    match check_http_url(&c.url) {
        Err(e) => errs.push(format!("{at}: {e}")),
        Ok(()) if c.url.contains(['?', '#']) => errs.push(format!(
            "{at}: url is the server base, e.g. \"http://prometheus.local:9090\" (no query string)"
        )),
        Ok(()) => {}
    }
    if !(c.interval_s.is_finite() && (1.0..=86_400.0).contains(&c.interval_s)) {
        errs.push(format!(
            "{at}: interval_s must be 1 to 86400 (got {})",
            c.interval_s
        ));
    }
    if !(c.timeout_s.is_finite() && (1.0..=120.0).contains(&c.timeout_s)) {
        errs.push(format!(
            "{at}: timeout_s must be 1 to 120 (got {})",
            c.timeout_s
        ));
    }
    if c.queries.is_empty() && c.range_queries.is_empty() {
        errs.push(format!(
            "{at}: queries or range_queries must list at least one key"
        ));
    }
    let key_ok = |key: &str, field: &str, errs: &mut Vec<String>| {
        if !crate::condition::is_valid_key(key) {
            errs.push(format!(
                "{at}: {field} key {key:?} is not a valid key; keys look like `source:name`"
            ));
        }
    };
    for (key, spec) in &c.queries {
        key_ok(key, "queries", errs);
        if spec.query().trim().is_empty() {
            errs.push(format!("{at}: queries {key:?}: the query is empty"));
        }
        if c.range_queries.contains_key(key) {
            errs.push(format!(
                "{at}: {key:?} is in both queries and range_queries"
            ));
        }
    }
    for (key, r) in &c.range_queries {
        key_ok(key, "range_queries", errs);
        if r.query.trim().is_empty() {
            errs.push(format!("{at}: range_queries {key:?}: the query is empty"));
        }
        let (range, step) = (r.range_s, r.step_s);
        if !(range.is_finite() && step.is_finite() && step > 0.0 && range >= step) {
            errs.push(format!(
                "{at}: range_queries {key:?}: need 0 < step_s <= range_s (got range_s = {range}, step_s = {step})"
            ));
        } else if range > 31.0 * 86_400.0 || range / step > MAX_POINTS {
            errs.push(format!(
                "{at}: range_queries {key:?}: range_s at most 31 days and at most {MAX_POINTS} points (range_s / step_s)"
            ));
        }
    }
    if let Some(var) = &c.bearer_env {
        if !is_env_var_name(var) {
            errs.push(format!(
                "{at}: bearer_env must be the NAME of an environment variable (letters, digits and '_'), not the token itself"
            ));
        }
    }
}

/// Fail early when `bearer_env` is set but the variable is not.
pub fn check_environment(c: &PromConfig) -> Result<(), String> {
    if let Some(var) = &c.bearer_env {
        read_secret(var, "a Prometheus bearer token")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Response parsing (pure)

/// What an instant query gave.
#[derive(Debug, Clone, PartialEq)]
pub enum Instant {
    Number(f64),
    /// Empty vector, `NaN`/`Inf` or a histogram: no value.
    NoValue,
    /// A vector with this many series (not exactly one).
    Many(usize),
}

/// Cap an error text from the server for logs.
fn short(s: &str) -> String {
    let s: String = s.chars().take(200).collect();
    s.trim().to_owned()
}

/// Check the envelope and return `data`. A `status: "error"` becomes
/// `Err("<errorType>: <error>")`.
fn envelope(body: &[u8]) -> Result<Json, String> {
    let mut root: Json = serde_json::from_slice(body).map_err(|_| "response is not JSON")?;
    match root.get("status").and_then(Json::as_str) {
        Some("success") => Ok(root.get_mut("data").map(Json::take).unwrap_or(Json::Null)),
        Some("error") => {
            let get = |k| root.get(k).and_then(Json::as_str).unwrap_or("");
            Err(short(&format!("{}: {}", get("errorType"), get("error"))))
        }
        _ => Err("response is not a Prometheus API reply (no `status`)".into()),
    }
}

/// `[<unix_time>, "<value>"]` as a finite number.
fn sample(v: &Json) -> Option<f64> {
    let raw = v.as_array()?.get(1)?;
    let n = match raw {
        Json::String(s) => s.trim().parse::<f64>().ok()?,
        Json::Number(n) => n.as_f64()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

fn sample_time(v: &Json) -> Option<SystemTime> {
    let secs = v.as_array()?.first()?.as_f64()?;
    (secs.is_finite() && secs >= 0.0).then(|| UNIX_EPOCH + Duration::from_secs_f64(secs))
}

/// Parse an `/api/v1/query` reply.
pub fn parse_instant(body: &[u8]) -> Result<Instant, String> {
    let data = envelope(body)?;
    let result = data.get("result").unwrap_or(&Json::Null);
    match data.get("resultType").and_then(Json::as_str) {
        Some("scalar") => Ok(sample(result).map_or(Instant::NoValue, Instant::Number)),
        Some("vector") => {
            let series = result.as_array().ok_or("vector result is not an array")?;
            Ok(match series.as_slice() {
                [] => Instant::NoValue,
                [one] => one
                    .get("value")
                    .and_then(sample)
                    .map_or(Instant::NoValue, Instant::Number),
                many => Instant::Many(many.len()),
            })
        }
        other => Err(format!(
            "unexpected resultType {} for an instant query (need a scalar or a vector)",
            other.unwrap_or("(missing)")
        )),
    }
}

/// Parse an `/api/v1/query_range` reply: the first series, oldest first.
pub fn parse_range(body: &[u8]) -> Result<Vec<(SystemTime, f64)>, String> {
    let data = envelope(body)?;
    match data.get("resultType").and_then(Json::as_str) {
        Some("matrix") => {}
        other => {
            return Err(format!(
                "unexpected resultType {} for a range query (need a matrix)",
                other.unwrap_or("(missing)")
            ))
        }
    }
    let series = data
        .get("result")
        .and_then(Json::as_array)
        .ok_or("matrix result is not an array")?;
    let mut points: Vec<(SystemTime, f64)> = series
        .first()
        .and_then(|s| s.get("values"))
        .and_then(Json::as_array)
        .map(|vals| {
            vals.iter()
                .filter_map(|p| Some((sample_time(p)?, sample(p)?)))
                .collect()
        })
        .unwrap_or_default();
    points.sort_by_key(|p| p.0);
    Ok(points)
}

// ---------------------------------------------------------------------------
// Source

#[derive(Debug, Clone)]
enum Job {
    Instant {
        key: String,
        query: String,
        unit: Option<String>,
    },
    Range {
        key: String,
        query: String,
        range_s: f64,
        step_s: f64,
    },
}

impl Job {
    fn key(&self) -> &str {
        match self {
            Job::Instant { key, .. } | Job::Range { key, .. } => key,
        }
    }
}

/// What one query did to its key.
#[derive(Debug)]
enum KeyOut {
    Set(Value),
    Remove,
    /// Nothing written; warn and count an error.
    Problem(String),
}

enum Qres {
    Key(KeyOut),
    RateLimited(Duration),
    Failed(String),
}

pub struct PromSource {
    base: String,
    jobs: Vec<Job>,
    headers: HeaderMap,
    timeout: Duration,
    timing: Timing,
    limits: Limits,
}

// Not derived: `headers` may hold the bearer token.
impl std::fmt::Debug for PromSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromSource")
            .field("jobs", &self.jobs.len())
            .field("headers", &self.headers) // sensitive values print as "Sensitive"
            .finish_non_exhaustive()
    }
}

impl PromSource {
    /// Reads `bearer_env` now; the error names the variable, never a value.
    pub fn new(c: &PromConfig) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        if let Some(var) = &c.bearer_env {
            let secret =
                read_secret(var, "a Prometheus bearer token").map_err(anyhow::Error::msg)?;
            let mut v =
                HeaderValue::from_str(&format!("Bearer {}", secret.expose())).map_err(|_| {
                    anyhow::anyhow!("environment variable {var} is not a valid bearer token")
                })?;
            v.set_sensitive(true);
            headers.insert(AUTHORIZATION, v);
        }
        let mut jobs = Vec::new();
        for (key, q) in &c.queries {
            jobs.push(Job::Instant {
                key: key.clone(),
                query: q.query().to_owned(),
                unit: q.unit().map(str::to_owned),
            });
        }
        for (key, r) in &c.range_queries {
            jobs.push(Job::Range {
                key: key.clone(),
                query: r.query.clone(),
                range_s: r.range_s,
                step_s: r.step_s,
            });
        }
        Ok(PromSource {
            base: c.url.trim_end_matches('/').to_owned(),
            jobs,
            headers,
            timeout: Duration::from_secs_f64(c.timeout_s),
            timing: Timing::new(c.interval_s),
            limits: Limits::default(),
        })
    }

    #[cfg(test)]
    pub fn with_timing(mut self, timing: Timing, limits: Limits) -> Self {
        self.timing = timing;
        self.limits = limits;
        self
    }
}

/// Map one response to its key's fate.
fn interpret(fetch: Fetch, parse: impl Fn(&[u8]) -> Result<KeyOut, String>) -> Qres {
    match fetch {
        Fetch::Body(b) => Qres::Key(parse(&b).unwrap_or_else(KeyOut::Problem)),
        // 400 (bad parameters) and 422 (expression cannot be executed) say
        // what is wrong with this query; other statuses are the server's.
        Fetch::Rejected(s, b) if s.as_u16() == 400 || s.as_u16() == 422 => {
            let why = envelope(&b).err().unwrap_or_else(|| format!("HTTP {s}"));
            Qres::Key(KeyOut::Problem(format!(
                "the server rejected the query: {why}"
            )))
        }
        Fetch::Rejected(s, _) => Qres::Failed(format!("HTTP {s}")),
        Fetch::RateLimited(d) => Qres::RateLimited(d),
        Fetch::Failed(m) => Qres::Failed(m),
    }
}

fn instant_out(r: Instant, unit: &Option<String>) -> KeyOut {
    match r {
        Instant::Number(value) => KeyOut::Set(Value::Number {
            value,
            unit: unit.clone(),
        }),
        Instant::NoValue => KeyOut::Remove,
        Instant::Many(n) => KeyOut::Problem(format!(
            "query returned {n} series, but exactly one is needed (aggregate it, e.g. with sum() or max())"
        )),
    }
}

async fn run_job(client: &reqwest::Client, s: &PromSource, job: &Job, now: f64) -> Qres {
    match job {
        Job::Instant { query, unit, .. } => {
            let req = client
                .get(format!("{}/api/v1/query", s.base))
                .headers(s.headers.clone())
                .query(&[("query", query.as_str())]);
            interpret(fetch_body(req, &s.limits).await, |b| {
                parse_instant(b).map(|r| instant_out(r, unit))
            })
        }
        Job::Range {
            query,
            range_s,
            step_s,
            ..
        } => {
            let req = client
                .get(format!("{}/api/v1/query_range", s.base))
                .headers(s.headers.clone())
                .query(&[
                    ("query", query.clone()),
                    ("start", format!("{:.3}", now - range_s)),
                    ("end", format!("{now:.3}")),
                    ("step", format!("{step_s}")),
                ]);
            interpret(fetch_body(req, &s.limits).await, |b| {
                parse_range(b).map(|pts| {
                    if pts.is_empty() {
                        KeyOut::Remove
                    } else {
                        KeyOut::Set(Value::Series(pts.into()))
                    }
                })
            })
        }
    }
}

async fn poll_once(
    client: &reqwest::Client,
    s: &PromSource,
    ctx_name: &str,
    store: &crate::store::StoreWriter,
) -> Outcome {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    // Indexes, not `&Job`s, flow through the stream (borrowed items make the
    // future fail the `Send` check).
    let results: Vec<(usize, Qres)> = stream::iter(0..s.jobs.len())
        .map(|i| async move { (i, run_job(client, s, &s.jobs[i], now).await) })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    let mut changes = Vec::new();
    let (mut problems, mut successes) = (0usize, 0usize);
    let mut rate_limited: Option<Duration> = None;
    let mut failed: Option<String> = None;
    let mut failures = 0usize;
    for (i, res) in results {
        let job = &s.jobs[i];
        match res {
            Qres::Key(KeyOut::Set(v)) => {
                successes += 1;
                changes.push(Change::Set(job.key().to_owned(), v));
            }
            Qres::Key(KeyOut::Remove) => {
                successes += 1;
                changes.push(Change::Remove(job.key().to_owned()));
            }
            Qres::Key(KeyOut::Problem(why)) => {
                problems += 1;
                tracing::warn!(source = %ctx_name, key = %job.key(), "prometheus query not applied: {why}");
            }
            Qres::RateLimited(d) => rate_limited = Some(rate_limited.map_or(d, |o| o.max(d))),
            Qres::Failed(m) => {
                failures += 1;
                failed.get_or_insert(m);
            }
        }
    }
    // One batch for everything that worked; a failed query keeps its old value.
    store.apply(changes);
    if let Some(d) = rate_limited {
        Outcome::RateLimited(d)
    } else if let (0, Some(m)) = (successes, &failed) {
        Outcome::Failed(m.clone())
    } else {
        if let Some(m) = &failed {
            tracing::warn!(source = %ctx_name, failed = failures, error = %m, "some prometheus queries failed");
        }
        Outcome::Ok {
            problems: problems + failures,
        }
    }
}

impl Source for PromSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let client = super::http_client()
                .timeout(self.timeout)
                .redirect(same_host_redirects())
                .build()
                .context("building HTTP client")?;
            let this = Arc::new(*self);
            let timing = this.timing;
            let (store, name) = (ctx.store.clone(), ctx.name.clone());
            run_poll_loop(&mut ctx, "prometheus", timing, || {
                let (client, this, store, name) =
                    (client.clone(), this.clone(), store.clone(), name.clone());
                async move { poll_once(&client, &this, &name, &store).await }
            })
            .await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
