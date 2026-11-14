//! `type = "home_assistant"`: entity states, weather forecasts and rolling
//! series from the Home Assistant WebSocket API (DESIGN.md §6 item 1).
//!
//! Message formats are recorded, with links and what could not be confirmed,
//! in `HA_PROTOCOL.md` next to this file.
//!
//! ```toml
//! [sources.ha]
//! type = "home_assistant"
//! url = "ws://homeassistant.local:8123/api/websocket"
//! token_env = "HA_TOKEN"                  # env var holding a long-lived token
//! series = ["sensor.outdoor_temperature"]  # also keep ha:<id>@series
//! forecasts = ["weather.home"]             # also fetch weather.get_forecasts
//! ```
//!
//! The source subscribes only to the entities the config uses: every
//! `ha:<entity_id>…` key the scenes read (the supervisor copies
//! `Config::referenced_keys()` into [`HaConfig::referenced`]), plus
//! `entities`, `series` and `forecasts`.
//!
//! One connection per run. Anything that goes wrong (connect, auth, no
//! pong, server close) returns `Err`, and the supervisor restarts the source
//! with backoff. The store keeps the last values meanwhile; their
//! `updated_at` ages.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value as Json};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::{Source, SourceCtx, SourceFuture};
use crate::config::{ForecastType, HaConfig};
use crate::store::{Change, StoreSnapshot, Value};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// How far back the series backfill asks the recorder for.
const BACKFILL_WINDOW: Duration = Duration::from_secs(24 * 3600);
const SUBSCRIBE_ID: u64 = 1;

// ---------------------------------------------------------------------------
// Config helpers

/// Validate a [`HaConfig`]; called from `Config::validate`. Error messages
/// never echo `token_env` (a mistake there may be the token itself).
pub fn validate(c: &HaConfig, at: &str, errs: &mut Vec<String>) {
    let url_ok = (c.url.starts_with("ws://") || c.url.starts_with("wss://"))
        && c.url.split_once("://").is_some_and(|(_, r)| !r.is_empty())
        && !c.url.contains('@')
        && !c.url.contains(char::is_whitespace);
    if !url_ok {
        errs.push(format!(
            "{at}: url must start with ws:// or wss://, e.g. \"ws://homeassistant.local:8123/api/websocket\" (no user:password@)"
        ));
    }
    let name_ok = !c.token_env.is_empty()
        && c.token_env.len() <= 64
        && !c.token_env.starts_with(|ch: char| ch.is_ascii_digit())
        && c.token_env
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if !name_ok {
        errs.push(format!(
            "{at}: token_env must be the NAME of an environment variable (letters, digits and '_', e.g. \"HA_TOKEN\"), not the token itself"
        ));
    }
    for (field, list) in [
        ("entities", &c.entities),
        ("series", &c.series),
        ("forecasts", &c.forecasts),
    ] {
        for id in list {
            if !is_entity_id(id) {
                errs.push(format!(
                    "{at}: {field}: {id:?} is not an entity ID like \"sensor.outdoor_temperature\""
                ));
            }
        }
    }
    for id in &c.forecasts {
        if is_entity_id(id) && !id.starts_with("weather.") {
            errs.push(format!("{at}: forecasts: {id:?} is not a weather entity"));
        }
    }
    if !(1..=10_000).contains(&c.series_points) {
        errs.push(format!(
            "{at}: series_points must be 1..=10000 (got {})",
            c.series_points
        ));
    }
    if !(30..=86_400).contains(&c.forecast_interval_s) {
        errs.push(format!(
            "{at}: forecast_interval_s must be 30..=86400 (got {})",
            c.forecast_interval_s
        ));
    }
    if !(1..=48).contains(&c.forecast_len) {
        errs.push(format!(
            "{at}: forecast_len must be 1..=48 (got {})",
            c.forecast_len
        ));
    }
}

/// `domain.object_id`, lowercase letters, digits and `_`.
pub fn is_entity_id(s: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    s.split_once('.').is_some_and(|(d, o)| ok(d) && ok(o))
}

/// The entity ID behind a store key: `ha:sensor.x`, `ha:sensor.x.attr` and
/// `ha:sensor.x@series` all give `sensor.x`. `None` for other sources' keys.
pub fn entity_of_key(key: &str) -> Option<String> {
    let rest = key.strip_prefix("ha:")?;
    let rest = rest.strip_suffix("@series").unwrap_or(rest);
    let mut parts = rest.split('.');
    let id = format!("{}.{}", parts.next()?, parts.next()?);
    is_entity_id(&id).then_some(id)
}

/// A secret that cannot be printed by accident.
#[derive(Clone)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Read the token from the environment variable `var`. The error names the
/// variable, never a value.
pub fn read_token(var: &str) -> Result<Secret, String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Ok(Secret(v.trim().to_owned())),
        Ok(_) => Err(format!(
            "environment variable {var} is empty; it must hold a Home Assistant long-lived access token"
        )),
        Err(_) => Err(format!(
            "environment variable {var} is not set; it must hold a Home Assistant long-lived access token"
        )),
    }
}

// ---------------------------------------------------------------------------
// State tracking (pure: no I/O, so it is unit-testable)

#[derive(Default)]
struct Ent {
    state: String,
    attrs: Map<String, Json>,
    /// Attribute store keys currently written, to remove stale ones.
    keys: BTreeSet<String>,
}

struct Tracker {
    series_ids: BTreeSet<String>,
    series_points: usize,
    forecast_ids: BTreeSet<String>,
    entities: HashMap<String, Ent>,
    series: HashMap<String, Vec<(SystemTime, f64)>>,
    /// Forecast store keys currently written, per weather entity.
    forecast_keys: HashMap<String, BTreeSet<String>>,
}

fn is_missing(state: &str) -> bool {
    state == "unavailable" || state == "unknown"
}

fn numeric(state: &str) -> Option<f64> {
    state.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

fn state_value(state: &str, attrs: &Map<String, Json>) -> Value {
    match numeric(state) {
        Some(value) => Value::Number {
            value,
            unit: attrs
                .get("unit_of_measurement")
                .and_then(Json::as_str)
                .filter(|u| !u.is_empty())
                .map(str::to_owned),
        },
        None => Value::Text(state.to_owned()),
    }
}

fn json_to_value(v: &Json) -> Option<Value> {
    match v {
        Json::Null => None,
        Json::Bool(b) => Some(Value::Bool(*b)),
        Json::Number(n) => n.as_f64().map(|value| Value::Number { value, unit: None }),
        Json::String(s) => Some(Value::Text(s.clone())),
        Json::Array(_) | Json::Object(_) => Some(Value::Json(Arc::new(v.clone()))),
    }
}

fn ts(secs: f64) -> Option<SystemTime> {
    (secs.is_finite() && secs >= 0.0).then(|| UNIX_EPOCH + Duration::from_secs_f64(secs))
}

impl Tracker {
    fn new(series: &BTreeSet<String>, series_points: usize, forecasts: &[String]) -> Self {
        Tracker {
            series_ids: series.clone(),
            series_points: series_points.max(1),
            forecast_ids: forecasts.iter().cloned().collect(),
            entities: HashMap::new(),
            series: HashMap::new(),
            forecast_keys: HashMap::new(),
        }
    }

    /// Resume series an earlier run left in the store.
    fn seed_series(&mut self, snap: &StoreSnapshot) {
        for id in &self.series_ids {
            if let Some(Value::Series(s)) = snap.get(&format!("ha:{id}@series")).map(|e| &e.value) {
                self.series.insert(id.clone(), s.to_vec());
            }
        }
    }

    /// Handle the payload of a `subscribe_entities` event (`a`, `c`, `r`).
    fn on_event(&mut self, ev: &Json, now: SystemTime) -> Vec<Change> {
        let mut out = Vec::new();
        if let Some(added) = ev.get("a").and_then(Json::as_object) {
            for (id, cs) in added {
                let Some(state) = cs.get("s").and_then(Json::as_str) else {
                    continue;
                };
                let attrs = cs
                    .get("a")
                    .and_then(Json::as_object)
                    .cloned()
                    .unwrap_or_default();
                // Keep the old attribute keys so stale ones get removed.
                let keys = self.entities.remove(id).map(|e| e.keys).unwrap_or_default();
                self.entities.insert(
                    id.clone(),
                    Ent {
                        state: state.to_owned(),
                        attrs,
                        keys,
                    },
                );
                self.sync(id, true, now, &mut out);
            }
        }
        if let Some(changed) = ev.get("c").and_then(Json::as_object) {
            for (id, diff) in changed {
                let Some(ent) = self.entities.get_mut(id) else {
                    continue;
                };
                let mut state_changed = false;
                if let Some(plus) = diff.get("+") {
                    if let Some(s) = plus.get("s").and_then(Json::as_str) {
                        ent.state = s.to_owned();
                        state_changed = true;
                    }
                    if let Some(a) = plus.get("a").and_then(Json::as_object) {
                        for (k, v) in a {
                            ent.attrs.insert(k.clone(), v.clone());
                        }
                    }
                }
                if let Some(gone) = diff
                    .get("-")
                    .and_then(|m| m.get("a"))
                    .and_then(Json::as_array)
                {
                    for k in gone.iter().filter_map(Json::as_str) {
                        ent.attrs.remove(k);
                    }
                }
                self.sync(id, state_changed, now, &mut out);
            }
        }
        if let Some(removed) = ev.get("r").and_then(Json::as_array) {
            for id in removed.iter().filter_map(Json::as_str) {
                self.remove(id, &mut out);
            }
        }
        out
    }

    /// Emit the keys for one entity from its tracked state.
    fn sync(&mut self, id: &str, state_changed: bool, now: SystemTime, out: &mut Vec<Change>) {
        let Some(ent) = self.entities.get_mut(id) else {
            return;
        };
        let state_key = format!("ha:{id}");
        out.push(if is_missing(&ent.state) {
            Change::Remove(state_key)
        } else {
            Change::Set(state_key, state_value(&ent.state, &ent.attrs))
        });
        let has_forecast = self.forecast_ids.contains(id);
        let mut keys = BTreeSet::new();
        for (name, v) in &ent.attrs {
            // The forecast attribute of old integrations would collide with
            // the `.forecast.*` keys written from `weather.get_forecasts`.
            if has_forecast && name == "forecast" {
                continue;
            }
            if let Some(val) = json_to_value(v) {
                let key = format!("ha:{id}.{name}");
                out.push(Change::Set(key.clone(), val));
                keys.insert(key);
            }
        }
        for stale in ent.keys.difference(&keys) {
            out.push(Change::Remove(stale.clone()));
        }
        ent.keys = keys;
        if state_changed && self.series_ids.contains(id) {
            if let Some(n) = numeric(&ent.state).filter(|_| !is_missing(&ent.state)) {
                let points = self.series.entry(id.to_owned()).or_default();
                points.push((now, n));
                if points.len() > self.series_points {
                    points.drain(..points.len() - self.series_points);
                }
                out.push(series_change(id, points));
            }
        }
    }

    fn remove(&mut self, id: &str, out: &mut Vec<Change>) {
        if let Some(ent) = self.entities.remove(id) {
            out.push(Change::Remove(format!("ha:{id}")));
            out.extend(ent.keys.into_iter().map(Change::Remove));
        }
        if self.series.remove(id).is_some() {
            out.push(Change::Remove(format!("ha:{id}@series")));
        }
        if let Some(keys) = self.forecast_keys.remove(id) {
            out.extend(keys.into_iter().map(Change::Remove));
        }
    }

    /// Keys from a `weather.get_forecasts` response (`result.response`).
    /// `None` when the response has no forecast for `id`.
    fn forecast(&mut self, id: &str, response: &Json, len: usize) -> Option<Vec<Change>> {
        let list = response.get(id)?.get("forecast")?.as_array()?;
        let list = &list[..list.len().min(len)];
        let mut out = Vec::new();
        let mut keys = BTreeSet::new();
        for (n, item) in list.iter().enumerate() {
            let Some(fields) = item.as_object() else {
                continue;
            };
            for (field, v) in fields {
                if let Some(val) = json_to_value(v) {
                    let key = format!("ha:{id}.forecast.{n}.{field}");
                    out.push(Change::Set(key.clone(), val));
                    keys.insert(key);
                }
            }
        }
        let whole = format!("ha:{id}.forecast");
        out.push(Change::Set(
            whole.clone(),
            Value::Json(Arc::new(Json::Array(list.to_vec()))),
        ));
        keys.insert(whole);
        if let Some(old) = self.forecast_keys.get(id) {
            out.extend(old.difference(&keys).cloned().map(Change::Remove));
        }
        self.forecast_keys.insert(id.to_owned(), keys);
        Some(out)
    }

    /// Merge recorder history (`history/history_during_period` with compressed
    /// states: `{"s": state, "lu": unix seconds}`) into the series.
    fn backfill(&mut self, id: &str, history: &[Json]) -> Option<Change> {
        if !self.series_ids.contains(id) {
            return None;
        }
        let mut hist: Vec<(SystemTime, f64)> = history
            .iter()
            .filter_map(|p| {
                let n = numeric(p.get("s")?.as_str()?)?;
                let t = ts(p.get("lu").or_else(|| p.get("lc"))?.as_f64()?)?;
                Some((t, n))
            })
            .collect();
        hist.sort_by_key(|p| p.0);
        let hist = downsample(hist, self.series_points);
        let last = hist.last().map(|p| p.0);
        let live = self.series.entry(id.to_owned()).or_default();
        // History runs up to now, so only newer local points add anything.
        let mut merged = hist;
        merged.extend(
            live.iter()
                .filter(|p| last.is_none_or(|l| p.0 > l))
                .copied(),
        );
        if merged.len() > self.series_points {
            merged.drain(..merged.len() - self.series_points);
        }
        *live = merged;
        (!live.is_empty()).then(|| series_change(id, live))
    }
}

fn series_change(id: &str, points: &[(SystemTime, f64)]) -> Change {
    Change::Set(format!("ha:{id}@series"), Value::Series(points.into()))
}

/// Keep `n` evenly spaced points (always including the newest).
fn downsample(v: Vec<(SystemTime, f64)>, n: usize) -> Vec<(SystemTime, f64)> {
    if v.len() <= n {
        return v;
    }
    if n == 1 {
        return vec![v[v.len() - 1]];
    }
    (0..n).map(|i| v[i * (v.len() - 1) / (n - 1)]).collect()
}

// ---------------------------------------------------------------------------
// The source

#[derive(Debug)]
pub struct HaSource {
    url: String,
    token: Secret,
    token_env: String,
    subscribe: Vec<String>,
    series: BTreeSet<String>,
    series_points: usize,
    forecasts: Vec<String>,
    forecast_type: ForecastType,
    forecast_every: Duration,
    forecast_len: usize,
    ping_every: Duration,
    pong_timeout: Duration,
}

/// What a pending request id was for.
enum Pending {
    Subscribe,
    Forecast(String),
    History,
}

impl HaSource {
    /// Build from config, reading the token from `token_env`.
    pub fn new(c: &HaConfig) -> anyhow::Result<Self> {
        let token = read_token(&c.token_env).map_err(anyhow::Error::msg)?;
        Ok(Self::with_token(c, token))
    }

    fn with_token(c: &HaConfig, token: Secret) -> Self {
        let mut subscribe: BTreeSet<String> = c
            .referenced
            .iter()
            .filter_map(|k| entity_of_key(k))
            .collect();
        subscribe.extend(c.entities.iter().cloned());
        subscribe.extend(c.series.iter().cloned());
        subscribe.extend(c.forecasts.iter().cloned());
        HaSource {
            url: c.url.clone(),
            token,
            token_env: c.token_env.clone(),
            subscribe: subscribe.into_iter().collect(),
            series: c.series.iter().cloned().collect(),
            series_points: c.series_points,
            forecasts: c.forecasts.clone(),
            forecast_type: c.forecast_type,
            forecast_every: Duration::from_secs(c.forecast_interval_s),
            forecast_len: c.forecast_len,
            ping_every: PING_EVERY,
            pong_timeout: PONG_TIMEOUT,
        }
    }

    async fn connect(&self) -> anyhow::Result<Ws> {
        if self.url.starts_with("wss://") {
            crate::tls::init();
        }
        let (mut ws, _) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(&self.url))
                .await
                .context("connecting to Home Assistant timed out")?
                .context("connecting to Home Assistant")?;
        let first = next_json(&mut ws, AUTH_TIMEOUT).await?;
        if first["type"] != "auth_required" {
            bail!(
                "expected auth_required from Home Assistant, got {:?}",
                first["type"]
            );
        }
        send(
            &mut ws,
            &json!({"type": "auth", "access_token": self.token.0}),
        )
        .await?;
        let reply = next_json(&mut ws, AUTH_TIMEOUT).await?;
        match reply["type"].as_str() {
            Some("auth_ok") => Ok(ws),
            Some("auth_invalid") => bail!(
                "Home Assistant rejected the access token in ${} ({}); create a new long-lived token and update the variable",
                self.token_env,
                reply["message"].as_str().unwrap_or("auth_invalid")
            ),
            other => bail!("unexpected reply to auth from Home Assistant: {other:?}"),
        }
    }

    async fn session(&self, ctx: &mut SourceCtx) -> anyhow::Result<()> {
        let mut ws = self.connect().await?;
        tracing::info!(source = %ctx.name, "connected to Home Assistant");
        ctx.ok();
        let mut tracker = Tracker::new(&self.series, self.series_points, &self.forecasts);
        tracker.seed_series(&ctx.store.snapshot());

        let mut next_id = SUBSCRIBE_ID;
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        if self.subscribe.is_empty() {
            tracing::warn!(source = %ctx.name, "no Home Assistant entities are used by the config; nothing to subscribe to");
        } else {
            // Never send an empty `entity_ids`: HA reads it as "everything".
            pending.insert(next_id, Pending::Subscribe);
            send(
                &mut ws,
                &json!({"id": next_id, "type": "subscribe_entities", "entity_ids": self.subscribe}),
            )
            .await?;
            next_id += 1;
        }
        if !self.series.is_empty() {
            self.request_history(&mut ws, &mut next_id, &mut pending)
                .await?;
        }
        self.request_forecasts(&mut ws, &mut next_id, &mut pending)
            .await?;

        let start = Instant::now();
        let mut ping = tokio::time::interval_at(start + self.ping_every, self.ping_every);
        let mut fc = tokio::time::interval_at(start + self.forecast_every, self.forecast_every);
        let mut pong_deadline: Option<Instant> = None;
        loop {
            tokio::select! {
                () = ctx.shutdown.cancelled() => {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
                msg = ws.next() => {
                    let msg = match msg {
                        None => bail!("Home Assistant closed the connection"),
                        Some(Err(e)) => return Err(e).context("Home Assistant connection error"),
                        Some(Ok(m)) => m,
                    };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => bail!("Home Assistant closed the connection"),
                        _ => continue,
                    };
                    let Ok(v) = serde_json::from_str::<Json>(text.as_str()) else {
                        tracing::warn!(source = %ctx.name, "ignoring a message that is not JSON");
                        continue;
                    };
                    self.handle(&v, ctx, &mut tracker, &mut pending, &mut pong_deadline)?;
                }
                _ = ping.tick() => {
                    if pong_deadline.is_none() {
                        send(&mut ws, &json!({"id": next_id, "type": "ping"})).await?;
                        next_id += 1;
                        pong_deadline = Some(Instant::now() + self.pong_timeout);
                    }
                }
                _ = fc.tick(), if !self.forecasts.is_empty() => {
                    self.request_forecasts(&mut ws, &mut next_id, &mut pending).await?;
                }
                () = tokio::time::sleep_until(pong_deadline.unwrap_or_else(Instant::now)), if pong_deadline.is_some() => {
                    bail!("no pong from Home Assistant within {:?}; treating the connection as dead", self.pong_timeout);
                }
            }
        }
    }

    fn handle(
        &self,
        v: &Json,
        ctx: &SourceCtx,
        tracker: &mut Tracker,
        pending: &mut HashMap<u64, Pending>,
        pong_deadline: &mut Option<Instant>,
    ) -> anyhow::Result<()> {
        let id = v["id"].as_u64();
        match v["type"].as_str() {
            Some("pong") => *pong_deadline = None,
            Some("event") if id == Some(SUBSCRIBE_ID) => {
                let changes = tracker.on_event(&v["event"], SystemTime::now());
                ctx.store.apply(changes);
                ctx.ok();
            }
            Some("result") => {
                let Some(kind) = id.and_then(|i| pending.remove(&i)) else {
                    return Ok(());
                };
                let ok = v["success"].as_bool() == Some(true);
                let why = || {
                    let e = &v["error"];
                    format!(
                        "{}: {}",
                        e["code"].as_str().unwrap_or("error"),
                        e["message"].as_str().unwrap_or("")
                    )
                };
                match kind {
                    Pending::Subscribe if !ok => {
                        bail!("subscribe_entities failed ({})", why());
                    }
                    Pending::Subscribe => {}
                    Pending::Forecast(entity) if ok => {
                        match tracker.forecast(&entity, &v["result"]["response"], self.forecast_len)
                        {
                            Some(changes) => {
                                ctx.store.apply(changes);
                                ctx.ok();
                            }
                            None => tracing::warn!(
                                source = %ctx.name, entity = %entity,
                                "weather.get_forecasts returned no forecast for the entity"
                            ),
                        }
                    }
                    Pending::Forecast(entity) => {
                        ctx.error();
                        tracing::warn!(source = %ctx.name, entity = %entity, "weather.get_forecasts failed ({})", why());
                    }
                    Pending::History if ok => {
                        let mut changes = Vec::new();
                        if let Some(map) = v["result"].as_object() {
                            for (entity, list) in map {
                                if let Some(list) = list.as_array() {
                                    changes.extend(tracker.backfill(entity, list));
                                }
                            }
                        }
                        ctx.store.apply(changes);
                    }
                    Pending::History => {
                        tracing::warn!(source = %ctx.name, "series backfill unavailable ({}); series start empty", why());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn request_forecasts(
        &self,
        ws: &mut Ws,
        next_id: &mut u64,
        pending: &mut HashMap<u64, Pending>,
    ) -> anyhow::Result<()> {
        for entity in &self.forecasts {
            pending.insert(*next_id, Pending::Forecast(entity.clone()));
            send(
                ws,
                &json!({
                    "id": *next_id,
                    "type": "call_service",
                    "domain": "weather",
                    "service": "get_forecasts",
                    "service_data": {"type": self.forecast_type.as_str()},
                    "target": {"entity_id": entity},
                    "return_response": true,
                }),
            )
            .await?;
            *next_id += 1;
        }
        Ok(())
    }

    async fn request_history(
        &self,
        ws: &mut Ws,
        next_id: &mut u64,
        pending: &mut HashMap<u64, Pending>,
    ) -> anyhow::Result<()> {
        let start = chrono::DateTime::<chrono::Utc>::from(SystemTime::now() - BACKFILL_WINDOW);
        pending.insert(*next_id, Pending::History);
        send(
            ws,
            &json!({
                "id": *next_id,
                "type": "history/history_during_period",
                "start_time": start.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                "entity_ids": self.series.iter().collect::<Vec<_>>(),
                "include_start_time_state": true,
                "significant_changes_only": false,
                "minimal_response": true,
                "no_attributes": true,
            }),
        )
        .await?;
        *next_id += 1;
        Ok(())
    }
}

async fn send(ws: &mut Ws, v: &Json) -> anyhow::Result<()> {
    ws.send(Message::text(v.to_string()))
        .await
        .context("sending to Home Assistant")
}

/// Next text message as JSON, skipping control frames.
async fn next_json(ws: &mut Ws, timeout: Duration) -> anyhow::Result<Json> {
    let read = async {
        loop {
            match ws.next().await {
                None => bail!("Home Assistant closed the connection during login"),
                Some(Err(e)) => return Err(e).context("Home Assistant connection error"),
                Some(Ok(Message::Text(t))) => {
                    return serde_json::from_str(t.as_str())
                        .context("Home Assistant sent invalid JSON during login")
                }
                Some(Ok(Message::Close(_))) => {
                    bail!("Home Assistant closed the connection during login")
                }
                Some(Ok(_)) => {}
            }
        }
    };
    tokio::time::timeout(timeout, read)
        .await
        .context("timed out waiting for Home Assistant during login")?
}

impl Source for HaSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            // Always an error unless shutting down: the supervisor restarts.
            self.session(&mut ctx).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::sources::{supervise, Shutdown, SourceHealth};
    use crate::store::Store;
    use std::sync::atomic::Ordering;
    use tokio::sync::{mpsc, watch};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::accept_async;

    const TOKEN: &str = "test-token-0123456789-not-real";

    type ServerWs = WebSocketStream<TcpStream>;

    struct Server {
        addr: std::net::SocketAddr,
        conns: mpsc::UnboundedReceiver<ServerWs>,
    }

    /// A fake HA: does the documented auth handshake, then hands each
    /// authenticated connection to the test.
    async fn server(accept_token: &'static str) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, conns) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = accept_async(tcp).await else {
                        return;
                    };
                    stx(
                        &mut ws,
                        json!({"type": "auth_required", "ha_version": "2025.1.0"}),
                    )
                    .await;
                    let auth = srecv(&mut ws).await;
                    assert_eq!(auth["type"], "auth");
                    if auth["access_token"] == accept_token {
                        stx(
                            &mut ws,
                            json!({"type": "auth_ok", "ha_version": "2025.1.0"}),
                        )
                        .await;
                        let _ = tx.send(ws);
                    } else {
                        stx(
                            &mut ws,
                            json!({"type": "auth_invalid", "message": "Invalid access token or password"}),
                        )
                        .await;
                        let _ = ws.close(None).await;
                    }
                });
            }
        });
        Server { addr, conns }
    }

    async fn stx(ws: &mut ServerWs, v: Json) {
        ws.send(Message::text(v.to_string())).await.unwrap();
    }

    async fn srecv(ws: &mut ServerWs) -> Json {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("server timed out waiting for the client")
            {
                Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
                Some(Ok(_)) => {}
                other => panic!("connection ended: {other:?}"),
            }
        }
    }

    fn config(addr: std::net::SocketAddr, extra: &str) -> HaConfig {
        toml::from_str(&format!(
            "url = \"ws://{addr}/api/websocket\"\ntoken_env = \"PANELD_TEST_HA_TOKEN\"\n{extra}"
        ))
        .unwrap()
    }

    fn source(c: &HaConfig, token: &str) -> HaSource {
        HaSource::with_token(c, Secret(token.to_owned()))
    }

    struct Running {
        task: JoinHandle<anyhow::Result<()>>,
        stop: watch::Sender<bool>,
        health: Arc<SourceHealth>,
    }

    fn start(src: HaSource, store: &Store) -> Running {
        let (stop, rx) = watch::channel(false);
        let health = Arc::new(SourceHealth::default());
        let ctx = SourceCtx {
            name: "ha".into(),
            store: store.writer("ha"),
            shutdown: Shutdown(rx),
            health: health.clone(),
        };
        Running {
            task: tokio::spawn(Box::new(src).run(ctx)),
            stop,
            health,
        }
    }

    async fn wait_for(
        store: &Store,
        what: &str,
        pred: impl Fn(&crate::store::StoreSnapshot) -> bool,
    ) {
        for _ in 0..400 {
            if pred(&store.snapshot()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "timed out waiting for: {what}; store = {:?}",
            store.snapshot()
        );
    }

    fn num(v: f64, unit: Option<&str>) -> Value {
        Value::Number {
            value: v,
            unit: unit.map(str::to_owned),
        }
    }

    fn series_len(store: &Store, key: &str) -> usize {
        match store.snapshot().get(key).map(|e| &e.value) {
            Some(Value::Series(s)) => s.len(),
            _ => 0,
        }
    }

    /// Fixtures follow the compressed formats in `messages.py` / `core.py`
    /// (see HA_PROTOCOL.md); entity IDs are generic.
    fn initial_event() -> Json {
        json!({"id": 1, "type": "event", "event": {"a": {
            "sensor.outdoor_temperature": {
                "s": "21.5",
                "a": {"unit_of_measurement": "°C", "friendly_name": "Outdoor temperature", "battery": 87, "tags": ["a", "b"]},
                "c": "01HXXXXXXXXXXXXXXXXXXXXXXX", "lc": 1700000000.5
            },
            "sensor.inverter_ac_power": {
                "s": "unavailable", "a": {"unit_of_measurement": "W"},
                "c": "01HXXXXXXXXXXXXXXXXXXXXXXY", "lc": 1700000000.5
            },
            "light.hall": {
                "s": "on", "a": {"brightness": 128, "is_group": false},
                "c": {"id": "01HXXXXXXXXXXXXXXXXXXXXXXZ", "parent_id": null, "user_id": "u"},
                "lc": 1700000000.5, "lu": 1700000001.0
            }
        }}})
    }

    #[tokio::test]
    async fn states_attributes_units_unavailable_and_removal() {
        let mut srv = server(TOKEN).await;
        let c = config(
            srv.addr,
            "entities = [\"sensor.inverter_ac_power\", \"light.hall\"]\nseries = [\"sensor.outdoor_temperature\"]\nseries_points = 3\n",
        );
        let store = Store::new();
        let run = start(source(&c, TOKEN), &store);
        let mut ws = srv.conns.recv().await.unwrap();

        let sub = srecv(&mut ws).await;
        assert_eq!(sub["id"], 1);
        assert_eq!(sub["type"], "subscribe_entities");
        assert_eq!(
            sub["entity_ids"],
            json!([
                "light.hall",
                "sensor.inverter_ac_power",
                "sensor.outdoor_temperature"
            ])
        );
        // The series backfill request follows; this test has no history.
        let hist = srecv(&mut ws).await;
        assert_eq!(hist["type"], "history/history_during_period");
        stx(
            &mut ws,
            json!({"id": 1, "type": "result", "success": true, "result": null}),
        )
        .await;
        stx(
            &mut ws,
            json!({"id": 2, "type": "result", "success": true, "result": {}}),
        )
        .await;
        stx(&mut ws, initial_event()).await;

        wait_for(&store, "initial state", |s| s.contains_key("ha:light.hall")).await;
        let s = store.snapshot();
        assert_eq!(
            s["ha:sensor.outdoor_temperature"].value,
            num(21.5, Some("°C"))
        );
        assert_eq!(
            s["ha:sensor.outdoor_temperature.friendly_name"].value,
            Value::Text("Outdoor temperature".into())
        );
        assert_eq!(
            s["ha:sensor.outdoor_temperature.battery"].value,
            num(87.0, None)
        );
        assert!(matches!(
            s["ha:sensor.outdoor_temperature.tags"].value,
            Value::Json(_)
        ));
        assert_eq!(s["ha:light.hall"].value, Value::Text("on".into()));
        assert_eq!(s["ha:light.hall.brightness"].value, num(128.0, None));
        assert_eq!(s["ha:light.hall.is_group"].value, Value::Bool(false));
        // `unavailable` has no state key, but attributes are kept.
        assert!(!s.contains_key("ha:sensor.inverter_ac_power"));
        assert!(s.contains_key("ha:sensor.inverter_ac_power.unit_of_measurement"));
        assert_eq!(&*s["ha:light.hall"].source, "ha");
        assert_eq!(
            series_len(&store, "ha:sensor.outdoor_temperature@series"),
            1
        );
        assert!(run.health.up.load(Ordering::Relaxed));

        // State change only: the diff has no attributes, so they must be kept.
        for (i, v) in ["22.0", "22.5", "23.0"].iter().enumerate() {
            stx(
                &mut ws,
                json!({"id": 1, "type": "event", "event": {"c": {
                    "sensor.outdoor_temperature": {"+": {"s": v, "lc": 1.0 + i as f64}}
                }}}),
            )
            .await;
        }
        wait_for(&store, "23.0", |s| {
            s["ha:sensor.outdoor_temperature"].value == num(23.0, Some("°C"))
        })
        .await;
        assert_eq!(
            store.snapshot()["ha:sensor.outdoor_temperature.battery"].value,
            num(87.0, None)
        );
        // 4 numeric changes in total, capped at series_points = 3.
        let snap = store.snapshot();
        let Value::Series(pts) = &snap["ha:sensor.outdoor_temperature@series"].value else {
            panic!()
        };
        assert_eq!(
            pts.iter().map(|p| p.1).collect::<Vec<_>>(),
            [22.0, 22.5, 23.0]
        );

        // Attribute-only change with a removal: no new series point.
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"c": {
                "sensor.outdoor_temperature": {"+": {"a": {"battery": 80}}, "-": {"a": ["tags"]}}
            }}}),
        )
        .await;
        wait_for(&store, "tags removed", |s| {
            !s.contains_key("ha:sensor.outdoor_temperature.tags")
        })
        .await;
        let s = store.snapshot();
        assert_eq!(
            s["ha:sensor.outdoor_temperature.battery"].value,
            num(80.0, None)
        );
        assert_eq!(
            series_len(&store, "ha:sensor.outdoor_temperature@series"),
            3
        );

        // Becoming unavailable removes the state key but not the series.
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"c": {
                "sensor.outdoor_temperature": {"+": {"s": "unavailable"}}
            }}}),
        )
        .await;
        wait_for(&store, "state removed", |s| {
            !s.contains_key("ha:sensor.outdoor_temperature")
        })
        .await;
        assert_eq!(
            series_len(&store, "ha:sensor.outdoor_temperature@series"),
            3
        );

        // Entity removal removes all of its keys.
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"r": ["light.hall", "sensor.outdoor_temperature"]}}),
        )
        .await;
        wait_for(&store, "removal", |s| {
            !s.keys().any(|k| k.starts_with("ha:light.hall"))
        })
        .await;
        let s = store.snapshot();
        assert!(!s
            .keys()
            .any(|k| k.starts_with("ha:sensor.outdoor_temperature")));
        assert!(s.contains_key("ha:sensor.inverter_ac_power.unit_of_measurement"));

        let _ = run.stop.send(true);
        assert!(run.task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn only_referenced_entities_are_subscribed() {
        let mut srv = server(TOKEN).await;
        let mut c = config(srv.addr, "entities = [\"sensor.extra\"]\n");
        c.referenced = [
            "ha:sensor.outdoor_temperature",
            "ha:sensor.inverter_ac_power@series",
            "ha:weather.home.forecast.1.temperature",
            "aircraft:count",
            "demo:x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let store = Store::new();
        let run = start(source(&c, TOKEN), &store);
        let mut ws = srv.conns.recv().await.unwrap();
        let sub = srecv(&mut ws).await;
        assert_eq!(
            sub["entity_ids"],
            json!([
                "sensor.extra",
                "sensor.inverter_ac_power",
                "sensor.outdoor_temperature",
                "weather.home"
            ])
        );
        let _ = run.stop.send(true);
        let _ = run.task.await;
    }

    #[tokio::test]
    async fn forecasts_are_fetched_and_written() {
        let mut srv = server(TOKEN).await;
        let c = config(
            srv.addr,
            "forecasts = [\"weather.home\"]\nforecast_type = \"hourly\"\nforecast_len = 2\n",
        );
        let store = Store::new();
        let run = start(source(&c, TOKEN), &store);
        let mut ws = srv.conns.recv().await.unwrap();
        let sub = srecv(&mut ws).await;
        assert_eq!(sub["entity_ids"], json!(["weather.home"]));
        let call = srecv(&mut ws).await;
        assert_eq!(call["type"], "call_service");
        assert_eq!(call["domain"], "weather");
        assert_eq!(call["service"], "get_forecasts");
        assert_eq!(call["service_data"], json!({"type": "hourly"}));
        assert_eq!(call["target"], json!({"entity_id": "weather.home"}));
        assert_eq!(call["return_response"], true);
        let forecast: Vec<Json> = (0..4)
            .map(|i| {
                json!({"datetime": format!("2024-01-01T{i:02}:00:00+00:00"), "condition": "cloudy",
                       "temperature": 20.0 + i as f64, "precipitation_probability": 10, "is_daytime": true})
            })
            .collect();
        stx(
            &mut ws,
            json!({"id": call["id"], "type": "result", "success": true, "result": {
                "context": {"id": "c", "parent_id": null, "user_id": null},
                "response": {"weather.home": {"forecast": forecast}}
            }}),
        )
        .await;
        wait_for(&store, "forecast", |s| {
            s.contains_key("ha:weather.home.forecast.1.temperature")
        })
        .await;
        let s = store.snapshot();
        assert_eq!(
            s["ha:weather.home.forecast.0.temperature"].value,
            num(20.0, None)
        );
        assert_eq!(
            s["ha:weather.home.forecast.1.condition"].value,
            Value::Text("cloudy".into())
        );
        assert_eq!(
            s["ha:weather.home.forecast.1.is_daytime"].value,
            Value::Bool(true)
        );
        assert!(!s.contains_key("ha:weather.home.forecast.2.temperature"));
        let Value::Json(j) = &s["ha:weather.home.forecast"].value else {
            panic!()
        };
        assert_eq!(j.as_array().unwrap().len(), 2);
        let _ = run.stop.send(true);
        let _ = run.task.await;
    }

    #[test]
    fn forecast_shrink_removes_stale_keys() {
        let mut t = Tracker::new(&BTreeSet::new(), 10, &["weather.home".to_owned()]);
        let resp = |n: usize| json!({"weather.home": {"forecast": (0..n).map(|i| json!({"temperature": i, "condition": "sunny"})).collect::<Vec<_>>()}});
        let first = t.forecast("weather.home", &resp(3), 5).unwrap();
        assert_eq!(first.len(), 3 * 2 + 1);
        let second = t.forecast("weather.home", &resp(1), 5).unwrap();
        let removed: BTreeSet<_> = second
            .iter()
            .filter_map(|c| match c {
                Change::Remove(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            removed,
            BTreeSet::from([
                "ha:weather.home.forecast.1.condition",
                "ha:weather.home.forecast.1.temperature",
                "ha:weather.home.forecast.2.condition",
                "ha:weather.home.forecast.2.temperature",
            ])
        );
        assert!(t.forecast("weather.home", &json!({}), 5).is_none());
    }

    #[tokio::test]
    async fn history_backfills_the_series_then_live_points_append() {
        let mut srv = server(TOKEN).await;
        let c = config(
            srv.addr,
            "series = [\"sensor.outdoor_temperature\"]\nseries_points = 4\n",
        );
        let store = Store::new();
        let run = start(source(&c, TOKEN), &store);
        let mut ws = srv.conns.recv().await.unwrap();
        let _sub = srecv(&mut ws).await;
        let hist = srecv(&mut ws).await;
        assert_eq!(hist["type"], "history/history_during_period");
        assert_eq!(hist["entity_ids"], json!(["sensor.outdoor_temperature"]));
        assert_eq!(hist["minimal_response"], true);
        assert_eq!(hist["significant_changes_only"], false);
        let start_time = hist["start_time"].as_str().unwrap();
        assert!(
            start_time.ends_with('Z') && start_time.contains('T'),
            "{start_time}"
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let pts: Vec<Json> = (0..10)
            .map(|i| json!({"s": format!("{i}.0"), "lu": now - 100.0 + i as f64}))
            .chain([json!({"s": "unavailable", "lu": now - 50.0})])
            .collect();
        stx(
            &mut ws,
            json!({"id": hist["id"], "type": "result", "success": true,
                "result": {"sensor.outdoor_temperature": pts}}),
        )
        .await;
        wait_for(&store, "backfill", |s| {
            s.contains_key("ha:sensor.outdoor_temperature@series")
        })
        .await;
        let snap = store.snapshot();
        let Value::Series(p) = &snap["ha:sensor.outdoor_temperature@series"].value else {
            panic!()
        };
        // 10 numeric points downsampled to 4, newest included.
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].1, 0.0);
        assert_eq!(p[3].1, 9.0);
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"a": {
                "sensor.outdoor_temperature": {"s": "30", "a": {}, "c": "x", "lc": 1.0}
            }}}),
        )
        .await;
        wait_for(&store, "live point", |s| {
            matches!(&s["ha:sensor.outdoor_temperature@series"].value, Value::Series(p) if p.last().unwrap().1 == 30.0)
        })
        .await;
        assert_eq!(
            series_len(&store, "ha:sensor.outdoor_temperature@series"),
            4
        );
        let _ = run.stop.send(true);
        let _ = run.task.await;
    }

    #[tokio::test]
    async fn failed_history_is_not_fatal() {
        let mut srv = server(TOKEN).await;
        let c = config(srv.addr, "series = [\"sensor.outdoor_temperature\"]\n");
        let store = Store::new();
        let run = start(source(&c, TOKEN), &store);
        let mut ws = srv.conns.recv().await.unwrap();
        let _ = srecv(&mut ws).await;
        let hist = srecv(&mut ws).await;
        stx(
            &mut ws,
            json!({"id": hist["id"], "type": "result", "success": false,
                "error": {"code": "unknown_command", "message": "Unknown command."}}),
        )
        .await;
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"a": {
                "sensor.outdoor_temperature": {"s": "5", "a": {}, "c": "x", "lc": 1.0}
            }}}),
        )
        .await;
        wait_for(&store, "state", |s| {
            s.contains_key("ha:sensor.outdoor_temperature")
        })
        .await;
        assert!(!run.task.is_finished());
        let _ = run.stop.send(true);
        let _ = run.task.await;
    }

    #[tokio::test]
    async fn auth_invalid_is_a_clear_error_that_does_not_leak_the_token() {
        let srv = server("some-other-token").await;
        let c = config(srv.addr, "entities = [\"sensor.extra\"]\n");
        let src = source(&c, TOKEN);
        let dbg = format!("{src:?}");
        assert!(!dbg.contains(TOKEN), "{dbg}");
        assert!(dbg.contains("redacted"));
        let store = Store::new();
        let run = start(src, &store);
        let err = run.task.await.unwrap().unwrap_err();
        let msg = format!("{err:#} / {err:?}");
        assert!(msg.contains("rejected"), "{msg}");
        assert!(msg.contains("PANELD_TEST_HA_TOKEN"), "{msg}");
        assert!(!msg.contains(TOKEN), "{msg}");
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn missing_token_variable_is_named_but_a_set_one_is_never_printed() {
        let unset = toml::from_str::<HaConfig>(
            "url = \"ws://h/api/websocket\"\ntoken_env = \"PANELD_TEST_HA_SURELY_UNSET\"\n",
        )
        .unwrap();
        let err = HaSource::new(&unset).unwrap_err();
        assert!(err.to_string().contains("PANELD_TEST_HA_SURELY_UNSET"));

        std::env::set_var("PANELD_TEST_HA_SET_TOKEN", TOKEN);
        let cfg = toml::from_str::<HaConfig>(
            "url = \"ws://h/api/websocket\"\ntoken_env = \"PANELD_TEST_HA_SET_TOKEN\"\n",
        )
        .unwrap();
        let src = HaSource::new(&cfg).unwrap();
        assert!(!format!("{src:?} {cfg:?} {:?}", src.token).contains(TOKEN));

        // check_environment (run at startup) names the source and variable.
        let full = Config::parse(&ha_toml(
            "url = \"ws://h/api/websocket\"\ntoken_env = \"PANELD_TEST_HA_SURELY_UNSET\"\n",
        ))
        .unwrap();
        let e = crate::sources::check_environment(&full)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("PANELD_TEST_HA_SURELY_UNSET") && e.contains("sources.ha"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn missing_pong_kills_the_connection_and_a_pong_keeps_it() {
        // No pong: error within ping_every + pong_timeout.
        let mut srv = server(TOKEN).await;
        let c = config(srv.addr, "entities = [\"sensor.extra\"]\n");
        let mut src = source(&c, TOKEN);
        src.ping_every = Duration::from_millis(40);
        src.pong_timeout = Duration::from_millis(80);
        let run = start(src, &Store::new());
        let mut ws = srv.conns.recv().await.unwrap();
        let _sub = srecv(&mut ws).await;
        let ping = srecv(&mut ws).await;
        assert_eq!(ping["type"], "ping");
        let err = tokio::time::timeout(Duration::from_secs(2), run.task)
            .await
            .expect("source gives up")
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("no pong"), "{err:#}");

        // With pongs: still running after several ping periods.
        let mut srv = server(TOKEN).await;
        let c = config(srv.addr, "entities = [\"sensor.extra\"]\n");
        let mut src = source(&c, TOKEN);
        src.ping_every = Duration::from_millis(30);
        src.pong_timeout = Duration::from_millis(60);
        let run = start(src, &Store::new());
        let mut ws = srv.conns.recv().await.unwrap();
        let _sub = srecv(&mut ws).await;
        for _ in 0..5 {
            let ping = srecv(&mut ws).await;
            assert_eq!(ping["type"], "ping");
            stx(&mut ws, json!({"id": ping["id"], "type": "pong"})).await;
        }
        assert!(!run.task.is_finished());
        let _ = run.stop.send(true);
        assert!(run.task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn reconnects_keeping_last_values_and_resumes() {
        let mut srv = server(TOKEN).await;
        let c = config(srv.addr, "series = [\"sensor.outdoor_temperature\"]\n");
        let store = Store::new();
        let (stop, rx) = watch::channel(false);
        let health = Arc::new(SourceHealth::default());
        let task = tokio::spawn(supervise(
            "ha".into(),
            {
                let c = c.clone();
                move || Ok(Box::new(source(&c, TOKEN)) as Box<dyn Source>)
            },
            store.writer("ha"),
            Shutdown(rx),
            health.clone(),
            Duration::from_millis(20),
        ));

        let mut ws = srv.conns.recv().await.unwrap();
        let _sub = srecv(&mut ws).await;
        let _hist = srecv(&mut ws).await;
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"a": {
                "sensor.outdoor_temperature": {"s": "10", "a": {"unit_of_measurement": "°C"}, "c": "x", "lc": 1.0}
            }}}),
        )
        .await;
        wait_for(&store, "first value", |s| {
            s.contains_key("ha:sensor.outdoor_temperature")
        })
        .await;
        let first_at = store.snapshot()["ha:sensor.outdoor_temperature"].updated_at;

        drop(ws); // the server goes away
        for _ in 0..200 {
            if health.restarts.load(Ordering::Relaxed) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(health.restarts.load(Ordering::Relaxed) >= 1);
        // Last values are kept while down.
        assert_eq!(
            store.snapshot()["ha:sensor.outdoor_temperature"].value,
            num(10.0, Some("°C"))
        );
        assert_eq!(
            store.snapshot()["ha:sensor.outdoor_temperature"].updated_at,
            first_at
        );

        let mut ws = srv.conns.recv().await.unwrap();
        let sub = srecv(&mut ws).await;
        assert_eq!(sub["type"], "subscribe_entities");
        stx(
            &mut ws,
            json!({"id": 1, "type": "event", "event": {"a": {
                "sensor.outdoor_temperature": {"s": "11", "a": {"unit_of_measurement": "°C"}, "c": "x", "lc": 2.0}
            }}}),
        )
        .await;
        wait_for(&store, "second value", |s| {
            s["ha:sensor.outdoor_temperature"].value == num(11.0, Some("°C"))
        })
        .await;
        // The series survived the reconnect (seeded from the store).
        assert_eq!(
            series_len(&store, "ha:sensor.outdoor_temperature@series"),
            2
        );
        assert!(health.up.load(Ordering::Relaxed));
        let _ = stop.send(true);
        task.await.unwrap();
    }

    #[test]
    fn entity_ids_and_keys() {
        assert!(is_entity_id("sensor.outdoor_temperature"));
        assert!(!is_entity_id("sensor"));
        assert!(!is_entity_id("Sensor.x"));
        assert!(!is_entity_id("sensor.x.y"));
        assert_eq!(entity_of_key("ha:sensor.x").as_deref(), Some("sensor.x"));
        assert_eq!(
            entity_of_key("ha:sensor.x@series").as_deref(),
            Some("sensor.x")
        );
        assert_eq!(
            entity_of_key("ha:sensor.x.unit").as_deref(),
            Some("sensor.x")
        );
        assert_eq!(
            entity_of_key("ha:weather.home.forecast.0.temperature").as_deref(),
            Some("weather.home")
        );
        assert_eq!(entity_of_key("aircraft:count"), None);
        assert_eq!(entity_of_key("ha:nodot"), None);
    }

    fn ha_toml(extra: &str) -> String {
        format!(
            "[canvas]\nwidth = 8\nheight = 8\n[[scene]]\nname = \"a\"\n[sources.ha]\ntype = \"home_assistant\"\n{extra}"
        )
    }

    #[test]
    fn config_defaults_and_validation() {
        let cfg = Config::parse(&ha_toml(
            "url = \"wss://ha.example.org/api/websocket\"\ntoken_env = \"HA_TOKEN\"\n",
        ))
        .unwrap();
        let crate::config::SourceConfig::HomeAssistant(c) = &cfg.sources["ha"] else {
            panic!()
        };
        assert_eq!(c.series_points, 288);
        assert_eq!(c.forecast_interval_s, 1800);
        assert_eq!(c.forecast_len, 5);
        assert_eq!(c.forecast_type, ForecastType::Daily);

        let bad = |extra: &str, needle: &str| {
            let e = Config::parse(&ha_toml(extra)).unwrap_err().to_string();
            assert!(e.contains(needle), "{needle:?} not in {e}");
            e
        };
        bad("url = \"http://h/x\"\ntoken_env = \"HA_TOKEN\"\n", "ws://");
        bad(
            "url = \"ws://user:pw@h/x\"\ntoken_env = \"HA_TOKEN\"\n",
            "ws://",
        );
        // A pasted token is rejected without being echoed.
        let leaked = "eyJhbGciOiJIUzI1NiJ9.abc.def";
        let e = bad(
            &format!("url = \"ws://h/x\"\ntoken_env = \"{leaked}\"\n"),
            "NAME of an environment variable",
        );
        assert!(!e.contains(leaked));
        let base = "url = \"ws://h/x\"\ntoken_env = \"HA_TOKEN\"\n";
        bad(&format!("{base}series = [\"nope\"]\n"), "entity ID");
        bad(
            &format!("{base}forecasts = [\"sensor.x\"]\n"),
            "weather entity",
        );
        bad(&format!("{base}series_points = 0\n"), "series_points");
        bad(
            &format!("{base}forecast_interval_s = 5\n"),
            "forecast_interval_s",
        );
        bad(&format!("{base}forecast_len = 0\n"), "forecast_len");
        bad(&format!("{base}forecast_type = \"weekly\"\n"), "weekly");
        bad(&format!("{base}bogus = 1\n"), "bogus");
    }

    #[tokio::test]
    async fn supervisor_hands_referenced_keys_to_the_source() {
        // Start the real supervisor against a fake server and check which
        // entities the source subscribes to.
        std::env::set_var("PANELD_TEST_HA_SUP_TOKEN", TOKEN);
        let mut srv = server(TOKEN).await;
        let cfg = Config::parse(&format!(
            "[canvas]\nwidth = 64\nheight = 32\n[[scene]]\nname = \"a\"\ncondition = \"ha:sensor.inverter_ac_power > 5\"\n  [[scene.zone]]\n  rect = {{ x = 0, y = 0, w = 64, h = 8 }}\n  widget = {{ type = \"value\", key = \"ha:sensor.outdoor_temperature\" }}\n[sources.ha]\ntype = \"home_assistant\"\nurl = \"ws://{}/api/websocket\"\ntoken_env = \"PANELD_TEST_HA_SUP_TOKEN\"\n",
            srv.addr
        ))
        .unwrap();
        let store = Store::new();
        let sup = crate::sources::Supervisor::start(&cfg, &store);
        let mut ws = srv.conns.recv().await.unwrap();
        let sub = srecv(&mut ws).await;
        assert_eq!(
            sub["entity_ids"],
            json!(["sensor.inverter_ac_power", "sensor.outdoor_temperature"])
        );
        sup.stop(Duration::from_secs(2)).await;
    }
}
