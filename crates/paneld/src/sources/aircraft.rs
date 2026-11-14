//! `type = "aircraft"`: aircraft near a point, from an ADS-B Exchange v2
//! style API such as adsb.fi (DESIGN.md §6.1), with routes from adsbdb.
//!
//! Aircraft data: adsb.fi, <https://adsb.fi>. Its terms ask for attribution
//! wherever the data is shown; see [`ATTRIBUTION_HTML`].
//!
//! # Keys
//! Keys are prefixed with the **source's name**, so `[sources.aircraft]`
//! writes `aircraft:*` and `[sources.planes]` writes `planes:*`:
//! `<name>:count`, `<name>:nearest.<field>` and `<name>:list` (JSON). Every
//! poll writes all of them in one [`StoreWriter::apply`] batch, starting with
//! a `RemovePrefix("<name>:nearest.")`, so a snapshot never shows half of one
//! poll and an empty result leaves no `nearest.*` keys behind.
//!
//! # Privacy
//! The location is rounded to `location_decimals` before it goes into the
//! URL; the request radius grows by the most that rounding can move the point
//! (see [`query_radius_nm`]) so no aircraft in range is lost. Distance and
//! bearing are always computed here, from the exact configured location: the
//! provider's `dst`/`dir` are relative to the rounded point, so they are
//! ignored. The exact location never leaves the process. The URL and location
//! are never logged above debug level, and transport errors are logged with
//! their URL stripped.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde_json::{json, Map, Value as Json};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use super::{Source, SourceCtx, SourceFuture};
use crate::config::{AircraftConfig, LatLon};
use crate::store::{Change, StoreWriter, Value};

#[cfg(test)]
mod tests;

/// HTML for the credit adsb.fi's terms require (README and preview footer).
pub const ATTRIBUTION_HTML: &str =
    r#"Aircraft data: <a href="https://adsb.fi" rel="noopener">adsb.fi</a>"#;

const NM_PER_EARTH_RADIUS: f64 = 3440.065;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const ROUTE_CACHE_CAP: usize = 512;

/// Timing and endpoint knobs that tests override; [`Tuning::from_config`]
/// gives the production values.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Minimum time between polls (`interval_s`).
    pub interval: Duration,
    /// Longest wait after repeated errors.
    pub backoff_cap: Duration,
    /// Wait after a 429 without a usable `Retry-After`.
    pub retry_after_default: Duration,
    /// Upper bound on any `Retry-After` we honour.
    pub retry_after_max: Duration,
    /// adsbdb base URL; the callsign is appended after a `/`.
    pub routes_base: String,
    pub route_ttl_hit: Duration,
    pub route_ttl_miss: Duration,
    /// How long a failed lookup (network error, 5xx, 429) is not retried.
    pub route_ttl_error: Duration,
    pub route_timeout: Duration,
}

impl Tuning {
    pub fn from_config(cfg: &AircraftConfig) -> Self {
        Tuning {
            interval: Duration::from_secs_f64(cfg.interval_s),
            backoff_cap: Duration::from_secs(300),
            retry_after_default: Duration::from_secs(60),
            retry_after_max: Duration::from_secs(3600),
            routes_base: "https://api.adsbdb.com/v0/callsign".into(),
            route_ttl_hit: Duration::from_secs(12 * 3600),
            route_ttl_miss: Duration::from_secs(3600),
            route_ttl_error: Duration::from_secs(60),
            route_timeout: Duration::from_secs(5),
        }
    }
}

pub struct AircraftSource {
    cfg: AircraftConfig,
    tuning: Tuning,
}

impl AircraftSource {
    pub fn new(_name: &str, cfg: &AircraftConfig) -> Self {
        Self::with_tuning(cfg, Tuning::from_config(cfg))
    }

    pub fn with_tuning(cfg: &AircraftConfig, tuning: Tuning) -> Self {
        AircraftSource {
            cfg: cfg.clone(),
            tuning,
        }
    }
}

// ---------------------------------------------------------------------------
// Data model and parsing

/// One aircraft, whatever provider it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Aircraft {
    pub hex: String,
    pub callsign: Option<String>,
    pub registration: Option<String>,
    pub type_code: Option<String>,
    pub desc: Option<String>,
    /// Barometric altitude in feet (`alt_geom` if that is all there is).
    /// `None` on the ground or when unknown.
    pub alt_ft: Option<f64>,
    pub on_ground: bool,
    pub speed_kt: Option<f64>,
    pub track: Option<f64>,
    pub baro_rate: Option<f64>,
    pub squawk: Option<String>,
    pub emergency: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Nautical miles from the query point.
    pub dist_nm: f64,
    /// Bearing from the query point, degrees (0 = north).
    pub bearing: f64,
}

/// Great-circle distance in nautical miles.
pub fn haversine_nm(a: LatLon, b: LatLon) -> f64 {
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dp = p2 - p1;
    let dl = (b.lon - a.lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * NM_PER_EARTH_RADIUS * h.sqrt().min(1.0).asin()
}

/// Initial bearing from `a` to `b`, degrees in `[0, 360)`.
pub fn bearing_deg(a: LatLon, b: LatLon) -> f64 {
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dl = (b.lon - a.lon).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// 8-point compass name (`N`, `NE`, ...) for a bearing.
pub fn compass(deg: f64) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    NAMES[((deg.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}

fn text(v: &Json, key: &str) -> Option<String> {
    let s = v.get(key)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

fn num(v: &Json, key: &str) -> Option<f64> {
    v.get(key)?.as_f64().filter(|n| n.is_finite())
}

fn parse_one(v: &Json, origin: LatLon) -> Option<Aircraft> {
    let hex = text(v, "hex")?;
    let (lat, lon) = (num(v, "lat")?, num(v, "lon")?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let (alt_ft, on_ground) = match v.get("alt_baro") {
        Some(Json::String(s)) if s.eq_ignore_ascii_case("ground") => (None, true),
        Some(Json::Number(_)) => (num(v, "alt_baro"), false),
        _ => (num(v, "alt_geom"), false),
    };
    let here = LatLon { lat, lon };
    Some(Aircraft {
        hex,
        callsign: text(v, "flight"),
        registration: text(v, "r"),
        type_code: text(v, "t"),
        desc: text(v, "desc"),
        alt_ft,
        on_ground,
        speed_kt: num(v, "gs"),
        track: num(v, "track"),
        baro_rate: num(v, "baro_rate"),
        squawk: text(v, "squawk"),
        emergency: text(v, "emergency").filter(|e| e != "none"),
        lat,
        lon,
        // Always ours: the provider's dst/dir are measured from the rounded
        // query point, `origin` is the exact configured location.
        dist_nm: haversine_nm(origin, here),
        bearing: bearing_deg(origin, here),
    })
}

/// Parse an ADS-B Exchange v2 response (`aircraft` or `ac` array). Aircraft
/// without a position are skipped. Distance and bearing are computed from
/// `origin` (the exact configured location); any `dst`/`dir` in the response
/// are ignored.
pub fn parse_response(body: &[u8], origin: LatLon) -> anyhow::Result<Vec<Aircraft>> {
    let root: Json = serde_json::from_slice(body).context("response is not JSON")?;
    let list = match root.get("aircraft").or_else(|| root.get("ac")) {
        Some(Json::Array(a)) => a.as_slice(),
        // Some providers send `"ac": null` when nothing is in range.
        Some(Json::Null) => &[],
        _ => bail!("response has no `aircraft` or `ac` array"),
    };
    Ok(list.iter().filter_map(|v| parse_one(v, origin)).collect())
}

/// What stays in the list.
#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub radius_nm: f64,
    pub max_altitude_ft: Option<f64>,
    pub include_ground: bool,
}

/// Apply the radius, altitude and ground filters, then sort by distance
/// (ties by hex, so the order is stable). An aircraft with unknown altitude
/// passes `max_altitude_ft`.
pub fn filter_and_sort(mut list: Vec<Aircraft>, f: &Filter) -> Vec<Aircraft> {
    list.retain(|a| {
        a.dist_nm <= f.radius_nm
            && (f.include_ground || !a.on_ground)
            && match (f.max_altitude_ft, a.alt_ft) {
                (Some(max), Some(alt)) => alt <= max,
                _ => true,
            }
    });
    list.sort_by(|a, b| {
        a.dist_nm
            .total_cmp(&b.dist_nm)
            .then_with(|| a.hex.cmp(&b.hex))
    });
    list
}

// ---------------------------------------------------------------------------
// Location rounding and URL

/// `v` rounded to `decimals` places as text, without a `-0.00`.
pub fn round_coord(v: f64, decimals: u8) -> f64 {
    let f = 10f64.powi(i32::from(decimals));
    let r = (v * f).round() / f;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

fn fmt_coord(v: f64, decimals: u8) -> String {
    format!("{:.*}", usize::from(decimals), round_coord(v, decimals))
}

/// The radius to ask the provider for: `radius_nm` plus the most that
/// rounding to `decimals` can move the query point (half a unit in the last
/// place on both axes; a degree of latitude is 60 NM and a degree of
/// longitude never more), rounded up and capped at the 250 NM API limit.
/// Aircraft are then filtered on the exact `radius_nm` from the exact point.
pub fn query_radius_nm(radius_nm: u32, decimals: u8) -> u32 {
    let half_unit_deg = 0.5 * 10f64.powi(-i32::from(decimals));
    let margin_nm = 60.0 * half_unit_deg * std::f64::consts::SQRT_2;
    ((f64::from(radius_nm) + margin_nm).ceil() as u32).min(250)
}

/// Fill `{lat}`, `{lon}` and `{radius_nm}`; the location is rounded first
/// and the radius widened to match ([`query_radius_nm`]).
pub fn fill_url(cfg: &AircraftConfig) -> String {
    cfg.url
        .replace("{lat}", &fmt_coord(cfg.location.lat, cfg.location_decimals))
        .replace("{lon}", &fmt_coord(cfg.location.lon, cfg.location_decimals))
        .replace(
            "{radius_nm}",
            &query_radius_nm(cfg.radius_nm, cfg.location_decimals).to_string(),
        )
}

// ---------------------------------------------------------------------------
// Routes (adsbdb)

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub origin: Option<String>,
    pub destination: Option<String>,
}

impl Route {
    /// `SYD-AKL` (with `separator` between), only when both ends are known.
    pub fn text(&self, separator: &str) -> Option<String> {
        Some(format!(
            "{}{separator}{}",
            self.origin.as_ref()?,
            self.destination.as_ref()?
        ))
    }
}

/// Parse an adsbdb `/v0/callsign/{callsign}` answer. An unknown callsign
/// (`"response": "unknown callsign"`) gives `None`.
pub fn parse_route(body: &[u8]) -> Option<Route> {
    let root: Json = serde_json::from_slice(body).ok()?;
    let fr = root.get("response")?.get("flightroute")?;
    let end = |k: &str| {
        let a = fr.get(k)?;
        text(a, "iata_code").or_else(|| text(a, "icao_code"))
    };
    let route = Route {
        origin: end("origin"),
        destination: end("destination"),
    };
    (route.origin.is_some() || route.destination.is_some()).then_some(route)
}

enum CacheHit {
    Route(Route),
    /// Looked up, nothing known.
    Miss,
    /// Not cached (or expired).
    Unknown,
}

/// In-memory route cache: bounded, with a per-entry expiry. Nothing is
/// persisted (adsbdb's data may not be copied into other databases).
struct RouteCache {
    cap: usize,
    map: HashMap<String, (Instant, Option<Route>)>,
    inflight: HashSet<String>,
}

impl RouteCache {
    fn new(cap: usize) -> Self {
        RouteCache {
            cap,
            map: HashMap::new(),
            inflight: HashSet::new(),
        }
    }

    fn get(&self, callsign: &str, now: Instant) -> CacheHit {
        match self.map.get(callsign) {
            Some((exp, r)) if *exp > now => match r {
                Some(r) => CacheHit::Route(r.clone()),
                None => CacheHit::Miss,
            },
            _ => CacheHit::Unknown,
        }
    }

    fn insert(&mut self, callsign: String, route: Option<Route>, ttl: Duration, now: Instant) {
        if !self.map.contains_key(&callsign) && self.map.len() >= self.cap {
            self.map.retain(|_, (exp, _)| *exp > now);
        }
        if !self.map.contains_key(&callsign) && self.map.len() >= self.cap {
            // Still full: drop the entry that expires soonest.
            if let Some(k) = self
                .map
                .iter()
                .min_by_key(|(_, (exp, _))| *exp)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&k);
            }
        }
        self.map.insert(callsign, (now + ttl, route));
    }
}

type SharedCache = Arc<Mutex<RouteCache>>;

fn lock(c: &SharedCache) -> std::sync::MutexGuard<'_, RouteCache> {
    c.lock().unwrap_or_else(|e| e.into_inner())
}

/// A callsign is safe to put in a URL path.
fn valid_callsign(cs: &str) -> bool {
    (2..=8).contains(&cs.len()) && cs.bytes().all(|b| b.is_ascii_alphanumeric())
}

enum RouteFetch {
    Found(Route),
    Unknown,
    Failed,
    RateLimited,
}

async fn fetch_route(client: &reqwest::Client, t: &Tuning, callsign: &str) -> RouteFetch {
    let url = format!("{}/{}", t.routes_base.trim_end_matches('/'), callsign);
    let resp = match client.get(url).timeout(t.route_timeout).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e.without_url(), "route lookup failed");
            return RouteFetch::Failed;
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return RouteFetch::RateLimited;
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return RouteFetch::Unknown;
    }
    if !status.is_success() {
        return RouteFetch::Failed;
    }
    match resp.bytes().await {
        Ok(b) if b.len() <= 1024 * 1024 => parse_route(&b)
            .map(RouteFetch::Found)
            .unwrap_or(RouteFetch::Unknown),
        _ => RouteFetch::Failed,
    }
}

/// Look up `callsigns` one after another (so adsbdb sees a trickle, not a
/// burst), caching each answer and waking the poll loop after each.
async fn lookup_routes(
    client: reqwest::Client,
    tuning: Tuning,
    callsigns: Vec<String>,
    cache: SharedCache,
    wake: Arc<Notify>,
) {
    let mut rate_limited = false;
    for cs in &callsigns {
        let (route, ttl) = if rate_limited {
            (None, tuning.route_ttl_error)
        } else {
            match fetch_route(&client, &tuning, cs).await {
                RouteFetch::Found(r) => (Some(r), tuning.route_ttl_hit),
                RouteFetch::Unknown => (None, tuning.route_ttl_miss),
                RouteFetch::Failed => (None, tuning.route_ttl_error),
                RouteFetch::RateLimited => {
                    rate_limited = true;
                    (None, tuning.route_ttl_error)
                }
            }
        };
        {
            let mut c = lock(&cache);
            c.inflight.remove(cs);
            c.insert(cs.clone(), route, ttl, Instant::now());
        }
        wake.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Store keys

fn round_to(v: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (v * f).round() / f
}

fn number(value: f64) -> Value {
    Value::Number { value, unit: None }
}

fn list_entry(a: &Aircraft, route: Option<&Route>, route_sep: &str) -> Json {
    let mut m = Map::new();
    let mut put_s = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            m.insert(k.into(), Json::String(v));
        }
    };
    put_s("hex", Some(a.hex.clone()));
    put_s("callsign", a.callsign.clone());
    put_s("registration", a.registration.clone());
    put_s("type", a.type_code.clone());
    put_s("desc", a.desc.clone());
    put_s("bearing_compass", Some(compass(a.bearing).into()));
    put_s("route", route.and_then(|r| r.text(route_sep)));
    put_s("origin", route.and_then(|r| r.origin.clone()));
    put_s("destination", route.and_then(|r| r.destination.clone()));
    let mut put_n = |k: &str, v: Option<f64>| {
        if let Some(v) = v {
            m.insert(k.into(), json!(v));
        }
    };
    put_n("alt_ft", a.alt_ft);
    put_n("speed_kt", a.speed_kt);
    put_n("dist_nm", Some(round_to(a.dist_nm, 2)));
    put_n("bearing", Some(round_to(a.bearing, 1)));
    Json::Object(m)
}

/// Every change for one poll: `RemovePrefix("<name>:nearest.")` first, then
/// the keys that have values. `list` is already filtered and sorted;
/// `route_of` gives the cached route for a callsign.
pub fn build_changes(
    name: &str,
    list: &[Aircraft],
    list_len: usize,
    route_sep: &str,
    route_of: &dyn Fn(&str) -> Option<Route>,
) -> Vec<Change> {
    let mut out = vec![
        Change::RemovePrefix(format!("{name}:nearest.")),
        Change::Set(format!("{name}:count"), number(list.len() as f64)),
    ];
    let route_for = |a: &Aircraft| a.callsign.as_deref().and_then(route_of);
    if let Some(a) = list.first() {
        let route = route_for(a);
        let key = |f: &str| format!("{name}:nearest.{f}");
        let mut text = |f: &str, v: Option<String>| {
            if let Some(v) = v {
                out.push(Change::Set(key(f), Value::Text(v)));
            }
        };
        text("callsign", a.callsign.clone());
        text("registration", a.registration.clone());
        text("type", a.type_code.clone());
        text("desc", a.desc.clone());
        text("bearing_compass", Some(compass(a.bearing).into()));
        text("route", route.as_ref().and_then(|r| r.text(route_sep)));
        text("origin", route.as_ref().and_then(|r| r.origin.clone()));
        text(
            "destination",
            route.as_ref().and_then(|r| r.destination.clone()),
        );
        let mut numeric = |f: &str, v: Option<f64>| {
            if let Some(v) = v {
                out.push(Change::Set(key(f), number(v)));
            }
        };
        numeric("alt_ft", a.alt_ft);
        numeric("speed_kt", a.speed_kt);
        numeric("dist_nm", Some(round_to(a.dist_nm, 2)));
        numeric("bearing", Some(round_to(a.bearing, 1)));
    }
    let entries: Vec<Json> = list
        .iter()
        .take(list_len)
        .map(|a| list_entry(a, route_for(a).as_ref(), route_sep))
        .collect();
    out.push(Change::Set(
        format!("{name}:list"),
        Value::Json(Arc::new(Json::Array(entries))),
    ));
    out
}

// ---------------------------------------------------------------------------
// Polling

/// Delay after the `failures`-th consecutive error: the interval doubled per
/// failure, up to `cap` (never below the interval itself).
pub fn backoff_delay(interval: Duration, failures: u32, cap: Duration) -> Duration {
    let scaled = interval.saturating_mul(1u32 << failures.min(16));
    scaled.min(cap).max(interval)
}

enum Poll {
    Ok(Vec<Aircraft>),
    RateLimited(Duration),
    Failed(String),
}

fn parse_retry_after(h: Option<&reqwest::header::HeaderValue>, t: &Tuning) -> Duration {
    h.and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(t.retry_after_default)
        .min(t.retry_after_max)
}

async fn poll_once(client: &reqwest::Client, url: &str, origin: LatLon, t: &Tuning) -> Poll {
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        // reqwest errors embed the URL, which holds the location.
        Err(e) => return Poll::Failed(e.without_url().to_string()),
    };
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Poll::RateLimited(parse_retry_after(
            resp.headers().get(reqwest::header::RETRY_AFTER),
            t,
        ));
    }
    if !status.is_success() {
        return Poll::Failed(format!("HTTP {status}"));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Poll::Failed("response too large".into());
    }
    let body = match resp.bytes().await {
        Ok(b) if b.len() <= MAX_BODY_BYTES => b,
        Ok(_) => return Poll::Failed("response too large".into()),
        Err(e) => return Poll::Failed(e.without_url().to_string()),
    };
    match parse_response(&body, origin) {
        Ok(list) => Poll::Ok(list),
        Err(e) => Poll::Failed(format!("{e:#}")),
    }
}

struct Publisher {
    name: String,
    store: StoreWriter,
    list_len: usize,
    route_sep: String,
    routes: bool,
    cache: SharedCache,
    client: reqwest::Client,
    tuning: Tuning,
    wake: Arc<Notify>,
    lookups: JoinSet<()>,
}

impl Publisher {
    /// Write one batch for `list`, then start background lookups for the
    /// listed aircraft whose routes are not cached yet.
    fn publish(&mut self, list: &[Aircraft]) {
        let now = Instant::now();
        let (cache, routes) = (self.cache.clone(), self.routes);
        let route_of = move |cs: &str| match lock(&cache).get(&cs.to_ascii_uppercase(), now) {
            CacheHit::Route(r) if routes => Some(r),
            _ => None,
        };
        let changes = build_changes(&self.name, list, self.list_len, &self.route_sep, &route_of);
        self.store.apply(changes);

        if !self.routes {
            return;
        }
        while self.lookups.try_join_next().is_some() {}
        let mut todo = Vec::new();
        {
            let mut c = lock(&self.cache);
            for cs in list
                .iter()
                .take(self.list_len)
                .filter_map(|a| a.callsign.as_deref())
                .map(str::to_ascii_uppercase)
            {
                if valid_callsign(&cs)
                    && !c.inflight.contains(&cs)
                    && matches!(c.get(&cs, now), CacheHit::Unknown)
                    && !todo.contains(&cs)
                {
                    c.inflight.insert(cs.clone());
                    todo.push(cs);
                }
            }
        }
        if !todo.is_empty() {
            self.lookups.spawn(lookup_routes(
                self.client.clone(),
                self.tuning.clone(),
                todo,
                self.cache.clone(),
                self.wake.clone(),
            ));
        }
    }
}

impl Source for AircraftSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let cfg = self.cfg;
            let t = self.tuning;
            let client = super::http_client()
                .build()
                .context("building HTTP client")?;
            let url = fill_url(&cfg);
            tracing::debug!(%url, "aircraft source url");
            // Distances are measured from the exact location, locally. Only
            // the rounded point is ever sent (fill_url).
            let origin = LatLon {
                lat: cfg.location.lat,
                lon: cfg.location.lon,
            };
            let filter = Filter {
                radius_nm: f64::from(cfg.radius_nm),
                max_altitude_ft: cfg.max_altitude_ft,
                include_ground: cfg.include_ground,
            };
            let mut p = Publisher {
                name: ctx.name.to_string(),
                store: ctx.store.clone(),
                list_len: cfg.list_len,
                route_sep: cfg.route_separator.clone(),
                routes: cfg.routes,
                cache: Arc::new(Mutex::new(RouteCache::new(ROUTE_CACHE_CAP))),
                client: client.clone(),
                tuning: t.clone(),
                wake: Arc::new(Notify::new()),
                lookups: JoinSet::new(),
            };
            let wake = p.wake.clone();
            let mut failures = 0u32;
            // The last good, filtered list; `None` while the latest poll failed.
            let mut last: Option<Vec<Aircraft>> = None;
            let mut next_at = tokio::time::Instant::now();
            loop {
                // Wait for the next poll; a finished route lookup republishes
                // the last good list meanwhile (no request to the provider).
                loop {
                    tokio::select! {
                        () = tokio::time::sleep_until(next_at) => break,
                        () = wake.notified() => {
                            if let Some(list) = &last {
                                p.publish(list);
                            }
                        }
                        () = ctx.shutdown.cancelled() => return Ok(()),
                    }
                }
                let poll = tokio::select! {
                    r = poll_once(&client, &url, origin, &t) => r,
                    () = ctx.shutdown.cancelled() => return Ok(()),
                };
                let done = tokio::time::Instant::now();
                match poll {
                    Poll::Ok(list) => {
                        if failures > 0 {
                            tracing::info!(source = %ctx.name, "aircraft source recovered");
                        }
                        failures = 0;
                        let list = filter_and_sort(list, &filter);
                        p.publish(&list);
                        last = Some(list);
                        ctx.ok();
                        next_at = done + t.interval;
                    }
                    Poll::RateLimited(wait) => {
                        last = None;
                        ctx.error();
                        let wait = wait.max(t.interval);
                        tracing::warn!(source = %ctx.name, retry_in = ?wait, "aircraft provider rate limit (HTTP 429)");
                        next_at = done + wait;
                    }
                    Poll::Failed(msg) => {
                        last = None;
                        failures = failures.saturating_add(1);
                        ctx.error();
                        let wait = backoff_delay(t.interval, failures, t.backoff_cap);
                        tracing::warn!(source = %ctx.name, error = %msg, retry_in = ?wait, "aircraft poll failed");
                        next_at = done + wait;
                    }
                }
            }
        })
    }
}
