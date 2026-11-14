use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::sync::watch;

use super::*;
use crate::sources::{Shutdown, SourceHealth};
use crate::store::Store;

const FI: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/aircraft/adsb.fi.json"
));
const LOL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/aircraft/adsb.lol.json"
));
const READSB: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/aircraft/readsb.json"
));

/// Sydney airport, the landmark all fixtures were captured at.
const SYD: LatLon = LatLon {
    lat: -33.94,
    lon: 151.18,
};

fn ac(hex: &str, dist: f64, alt: Option<f64>, ground: bool) -> Aircraft {
    Aircraft {
        hex: hex.into(),
        callsign: Some(format!("CS{hex}")),
        registration: None,
        type_code: None,
        desc: None,
        alt_ft: alt,
        on_ground: ground,
        speed_kt: Some(200.0),
        track: None,
        baro_rate: None,
        squawk: None,
        emergency: None,
        lat: 0.0,
        lon: 0.0,
        dist_nm: dist,
        bearing: 90.0,
    }
}

// ---- parsing ---------------------------------------------------------------

#[test]
fn parses_adsb_fi_fixture() {
    let list = parse_response(FI.as_bytes(), SYD).unwrap();
    assert_eq!(list.len(), 5);
    let jst = list.iter().find(|a| a.hex == "7c6b3b").unwrap();
    assert_eq!(jst.callsign.as_deref(), Some("JST520"));
    assert_eq!(jst.registration.as_deref(), Some("VH-VGT"));
    assert_eq!(jst.type_code.as_deref(), Some("A320"));
    assert_eq!(jst.desc.as_deref(), Some("AIRBUS A-320"));
    assert!(jst.on_ground);
    assert_eq!(jst.alt_ft, None);
    // dst/dir are computed by us (the provider's are relative to the rounded
    // query point). Here the query point was SYD itself, so they agree with
    // the provider's 0.288 NM / 203.6° to within rounding of its lat/lon.
    assert!((jst.dist_nm - 0.288).abs() < 0.01, "{}", jst.dist_nm);
    assert!((jst.bearing - 203.6).abs() < 1.0, "{}", jst.bearing);
    // A ground vehicle with a blank callsign.
    let veh = list.iter().find(|a| a.hex == "7cf441").unwrap();
    assert_eq!(veh.callsign, None);
}

#[test]
fn parses_adsb_lol_fixture_with_ac_key() {
    let list = parse_response(LOL.as_bytes(), SYD).unwrap();
    assert_eq!(list.len(), 4);
    assert!(list.iter().all(|a| a.on_ground));
    let none_type = list.iter().find(|a| a.hex == "7cf4cf").unwrap();
    assert_eq!(none_type.type_code, None);
    assert_eq!(none_type.callsign.as_deref(), Some("RED O"));
}

#[test]
fn parses_readsb_fixture_and_computes_dst_dir() {
    let list = parse_response(READSB.as_bytes(), SYD).unwrap();
    // The aircraft without a position is skipped.
    assert_eq!(list.len(), 5);
    assert!(list.iter().all(|a| a.hex != "7c0a06"));
    let q = list.iter().find(|a| a.hex == "7c0a01").unwrap();
    assert_eq!(q.callsign.as_deref(), Some("QFA12"));
    assert_eq!(q.registration, None);
    assert_eq!(q.type_code, None);
    assert_eq!(q.alt_ft, Some(2500.0));
    assert_eq!(q.speed_kt, Some(160.2));
    assert_eq!(q.baro_rate, Some(-700.0));
    // 0.04 deg north and 0.02 deg east of the origin: about 2.6 NM, bearing 23.
    assert!((q.dist_nm - 2.6).abs() < 0.1, "{}", q.dist_nm);
    assert!((q.bearing - 22.6).abs() < 1.0, "{}", q.bearing);
    assert_eq!(compass(q.bearing), "NE");
    // alt_geom is used when there is no alt_baro.
    let g = list.iter().find(|a| a.hex == "7c0a05").unwrap();
    assert_eq!(g.alt_ft, Some(4200.0));
    // "emergency": "none" is not an emergency.
    let v = list.iter().find(|a| a.hex == "7c0a02").unwrap();
    assert_eq!(v.emergency, None);
}

#[test]
fn parse_rejects_bad_bodies_but_accepts_null_ac() {
    assert!(parse_response(b"not json", SYD).is_err());
    assert!(parse_response(br#"{"foo": []}"#, SYD).is_err());
    assert!(parse_response(br#"{"ac": null}"#, SYD).unwrap().is_empty());
    assert!(parse_response(br#"{"aircraft": []}"#, SYD)
        .unwrap()
        .is_empty());
}

// ---- geometry ----------------------------------------------------------------

#[test]
fn haversine_known_distances() {
    let d = haversine_nm(LatLon { lat: 0.0, lon: 0.0 }, LatLon { lat: 1.0, lon: 0.0 });
    assert!((d - 60.04).abs() < 0.01, "{d}");
    assert_eq!(haversine_nm(SYD, SYD), 0.0);
    // Sydney to Melbourne airports is about 380 NM.
    let mel = LatLon {
        lat: -37.67,
        lon: 144.84,
    };
    let d = haversine_nm(SYD, mel);
    assert!((375.0..386.0).contains(&d), "{d}");
    // Symmetric.
    assert!((haversine_nm(mel, SYD) - d).abs() < 1e-9);
}

#[test]
fn bearings_and_compass_points() {
    let o = LatLon { lat: 0.0, lon: 0.0 };
    let at = |lat, lon| bearing_deg(o, LatLon { lat, lon });
    assert!(at(1.0, 0.0).abs() < 1e-9);
    assert!((at(0.0, 1.0) - 90.0).abs() < 1e-9);
    assert!((at(-1.0, 0.0) - 180.0).abs() < 1e-9);
    assert!((at(0.0, -1.0) - 270.0).abs() < 1e-9);
    assert!((at(1.0, -1e-9) - 360.0).abs() < 1e-3 || at(1.0, -1e-9) > 359.0);

    for (deg, name) in [
        (0.0, "N"),
        (22.4, "N"),
        (22.6, "NE"),
        (90.0, "E"),
        (135.0, "SE"),
        (180.0, "S"),
        (225.0, "SW"),
        (270.0, "W"),
        (315.0, "NW"),
        (337.4, "NW"),
        (337.6, "N"),
        (359.9, "N"),
        (360.0, "N"),
        (-90.0, "W"),
    ] {
        assert_eq!(compass(deg), name, "{deg}");
    }
}

// ---- filters and sorting -----------------------------------------------------

#[test]
fn filters_by_radius_altitude_and_ground_then_sorts() {
    let list = vec![
        ac("c", 5.0, Some(3000.0), false),
        ac("a", 1.0, Some(20000.0), false),  // too high
        ac("b", 2.0, None, true),            // on the ground
        ac("d", 11.0, Some(1000.0), false),  // too far
        ac("e", 3.0, None, false),           // unknown altitude passes
        ac("f", 3.0, Some(1000.0), false),   // ties break by hex
        ac("g", 10.0, Some(15000.0), false), // exactly on both limits
    ];
    let f = Filter {
        radius_nm: 10.0,
        max_altitude_ft: Some(15000.0),
        include_ground: false,
    };
    let hexes = |l: &[Aircraft]| l.iter().map(|a| a.hex.clone()).collect::<Vec<_>>();
    assert_eq!(
        hexes(&filter_and_sort(list.clone(), &f)),
        ["e", "f", "c", "g"]
    );
    let f = Filter {
        include_ground: true,
        max_altitude_ft: None,
        ..f
    };
    assert_eq!(
        hexes(&filter_and_sort(list, &f)),
        ["a", "b", "e", "f", "c", "g"]
    );
}

// ---- store keys ---------------------------------------------------------------

fn no_routes(_: &str) -> Option<Route> {
    None
}

#[test]
fn empty_result_removes_nearest_keys_in_one_batch() {
    let store = Store::new();
    let w = store.writer("aircraft");
    let list = vec![ac("a", 1.234, Some(2000.0), false)];
    w.apply(build_changes("aircraft", &list, 5, &no_routes));
    let snap = store.snapshot();
    assert!(snap.contains_key("aircraft:nearest.callsign"));
    assert!(snap.contains_key("aircraft:nearest.alt_ft"));
    // An unrelated key from another source must survive.
    store.writer("other").set("aircraft2:x", Value::Bool(true));

    let changes = build_changes("aircraft", &[], 5, &no_routes);
    // The first change is the prefix removal, so the whole poll is one batch.
    assert_eq!(changes[0], Change::RemovePrefix("aircraft:nearest.".into()));
    w.apply(changes);
    let snap = store.snapshot();
    assert!(!snap.keys().any(|k| k.starts_with("aircraft:nearest.")));
    assert_eq!(
        snap["aircraft:count"].value,
        Value::Number {
            value: 0.0,
            unit: None
        }
    );
    assert_eq!(
        snap["aircraft:list"].value,
        Value::Json(Arc::new(Json::Array(vec![])))
    );
    assert!(snap.contains_key("aircraft2:x"));
}

#[test]
fn keys_follow_the_source_name_and_carry_route_and_list() {
    let mut a = ac("a", 1.234, Some(2000.0), false);
    a.registration = Some("VH-ABC".into());
    a.type_code = Some("A320".into());
    a.bearing = 200.0;
    let b = ac("b", 4.0, None, false);
    let route = |cs: &str| {
        (cs == "CSa").then(|| Route {
            origin: Some("SYD".into()),
            destination: Some("AKL".into()),
        })
    };
    let store = Store::new();
    store
        .writer("planes")
        .apply(build_changes("planes", &[a, b], 1, &route));
    let snap = store.snapshot();
    let text = |k: &str| match &snap[k].value {
        Value::Text(t) => t.clone(),
        v => panic!("{k}: {v:?}"),
    };
    let nr = |k: &str| match &snap[k].value {
        Value::Number { value, .. } => *value,
        v => panic!("{k}: {v:?}"),
    };
    assert_eq!(nr("planes:count"), 2.0);
    assert_eq!(text("planes:nearest.callsign"), "CSa");
    assert_eq!(text("planes:nearest.registration"), "VH-ABC");
    assert_eq!(text("planes:nearest.type"), "A320");
    assert_eq!(text("planes:nearest.bearing_compass"), "S");
    assert_eq!(text("planes:nearest.route"), "SYD→AKL");
    assert_eq!(text("planes:nearest.origin"), "SYD");
    assert_eq!(text("planes:nearest.destination"), "AKL");
    assert_eq!(nr("planes:nearest.alt_ft"), 2000.0);
    assert_eq!(nr("planes:nearest.speed_kt"), 200.0);
    assert_eq!(nr("planes:nearest.dist_nm"), 1.23);
    assert_eq!(nr("planes:nearest.bearing"), 200.0);
    // Missing fields are not written at all.
    assert!(!snap.contains_key("planes:nearest.desc"));
    let Value::Json(list) = &snap["planes:list"].value else {
        panic!("list is not JSON");
    };
    // list_len = 1.
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["route"], "SYD→AKL");
    assert_eq!(list[0]["dist_nm"], 1.23);
    assert!(snap.keys().all(|k| k.starts_with("planes:")));
}

// ---- privacy ------------------------------------------------------------------

#[test]
fn location_is_rounded_before_it_reaches_the_url() {
    let cfg = |lat, lon, decimals| {
        toml::from_str::<AircraftConfig>(&format!(
            "location = {{ lat = {lat}, lon = {lon} }}\nlocation_decimals = {decimals}\nradius_nm = 25\n"
        ))
        .unwrap()
    };
    // Sydney airport's published coordinates, with 4 decimals.
    assert_eq!(
        fill_url(&cfg(-33.9461, 151.1772, 2)),
        "https://opendata.adsb.fi/api/v2/lat/-33.95/lon/151.18/dist/26"
    );
    assert_eq!(
        fill_url(&cfg(-33.9461, 151.1772, 1)),
        "https://opendata.adsb.fi/api/v2/lat/-33.9/lon/151.2/dist/30"
    );
    assert_eq!(
        fill_url(&cfg(-33.9461, 151.1772, 0)),
        "https://opendata.adsb.fi/api/v2/lat/-34/lon/151/dist/68"
    );
    assert_eq!(
        fill_url(&cfg(-33.9461, 151.1772, 3)),
        "https://opendata.adsb.fi/api/v2/lat/-33.946/lon/151.177/dist/26"
    );
    // No exact digits survive, and no "-0.00".
    assert_eq!(fmt_coord(-0.001, 2), "0.00");
    assert!(!round_coord(-0.001, 2).is_sign_negative());
    assert_eq!(round_coord(-33.9461, 2), -33.95);
    // Placeholders are optional (a local readsb URL has none).
    let mut c = cfg(-33.9461, 151.1772, 2);
    c.url = "http://receiver.local/data/aircraft.json".into();
    assert_eq!(fill_url(&c), c.url);
}

// ---- backoff and cache ----------------------------------------------------------

#[test]
fn backoff_doubles_and_caps() {
    let i = Duration::from_secs(15);
    let cap = Duration::from_secs(300);
    let secs = |n| backoff_delay(i, n, cap).as_secs();
    assert_eq!(
        [secs(1), secs(2), secs(3), secs(4), secs(5)],
        [30, 60, 120, 240, 300]
    );
    assert_eq!(secs(40), 300);
    // Never below the interval, even if it exceeds the cap.
    assert_eq!(
        backoff_delay(Duration::from_secs(600), 1, cap),
        Duration::from_secs(600)
    );
}

fn route(o: &str, d: &str) -> Option<Route> {
    Some(Route {
        origin: Some(o.into()),
        destination: Some(d.into()),
    })
}

#[test]
fn route_cache_expires_and_evicts() {
    let now = Instant::now();
    let mut c = RouteCache::new(3);
    c.insert(
        "A".into(),
        route("SYD", "AKL"),
        Duration::from_secs(10),
        now,
    );
    c.insert("B".into(), None, Duration::from_secs(5), now);
    assert!(matches!(c.get("A", now), CacheHit::Route(_)));
    assert!(matches!(c.get("B", now), CacheHit::Miss));
    assert!(matches!(c.get("Z", now), CacheHit::Unknown));
    let later = now + Duration::from_secs(6);
    assert!(matches!(c.get("B", later), CacheHit::Unknown));
    assert!(matches!(c.get("A", later), CacheHit::Route(_)));
    assert!(matches!(
        c.get("A", now + Duration::from_secs(11)),
        CacheHit::Unknown
    ));

    // At capacity, expired entries go first, then the soonest to expire.
    c.insert("C".into(), route("A", "B"), Duration::from_secs(100), now);
    assert_eq!(c.map.len(), 3);
    c.insert("D".into(), route("A", "B"), Duration::from_secs(100), later);
    assert_eq!(c.map.len(), 3);
    assert!(!c.map.contains_key("B"));
    c.insert("E".into(), route("A", "B"), Duration::from_secs(100), later);
    assert_eq!(c.map.len(), 3);
    assert!(!c.map.contains_key("A"), "soonest to expire is evicted");
    for i in 0..2000 {
        c.insert(format!("X{i}"), None, Duration::from_secs(100), later);
    }
    assert_eq!(c.map.len(), 3);
    let mut big = RouteCache::new(ROUTE_CACHE_CAP);
    for i in 0..ROUTE_CACHE_CAP + 100 {
        big.insert(format!("X{i}"), None, Duration::from_secs(100), now);
    }
    assert_eq!(big.map.len(), ROUTE_CACHE_CAP);
}

#[test]
fn parses_adsbdb_answers() {
    let hit = br#"{"response":{"flightroute":{"callsign":"QFA749","origin":{"iata_code":"SYD","icao_code":"YSSY"},"destination":{"iata_code":"","icao_code":"YPAD"}}}}"#;
    let r = parse_route(hit).unwrap();
    assert_eq!(r.origin.as_deref(), Some("SYD"));
    // Falls back to ICAO when there is no IATA code.
    assert_eq!(r.destination.as_deref(), Some("YPAD"));
    assert_eq!(r.text().as_deref(), Some("SYD→YPAD"));
    assert_eq!(parse_route(br#"{"response":"unknown callsign"}"#), None);
    assert_eq!(parse_route(b"garbage"), None);
    assert!(valid_callsign("QFA749"));
    assert!(!valid_callsign("A/../B"));
    assert!(!valid_callsign("Q"));
}

// ---- loopback server --------------------------------------------------------------

#[derive(Clone)]
struct Reply {
    status: u16,
    retry_after: Option<&'static str>,
    body: String,
}

fn ok(body: &str) -> Reply {
    Reply {
        status: 200,
        retry_after: None,
        body: body.into(),
    }
}

struct Mock {
    script: Mutex<VecDeque<Reply>>,
    fallback: Reply,
    /// (time, path) of each positions request.
    hits: Mutex<Vec<(Instant, String)>>,
    route_hits: Mutex<Vec<String>>,
    route_delay: Duration,
}

fn respond(r: &Reply) -> Response {
    let mut b = Response::builder().status(StatusCode::from_u16(r.status).unwrap());
    if let Some(ra) = r.retry_after {
        b = b.header("retry-after", ra);
    }
    b.body(Body::from(r.body.clone())).unwrap()
}

async fn positions(State(m): State<Arc<Mock>>, uri: Uri) -> Response {
    m.hits
        .lock()
        .unwrap()
        .push((Instant::now(), uri.path().to_owned()));
    let r = m
        .script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| m.fallback.clone());
    respond(&r)
}

async fn route_lookup(State(m): State<Arc<Mock>>, Path(cs): Path<String>) -> Response {
    m.route_hits.lock().unwrap().push(cs.clone());
    tokio::time::sleep(m.route_delay).await;
    if cs == "JST520" {
        respond(&ok(
            r#"{"response":{"flightroute":{"callsign":"JST520","origin":{"iata_code":"SYD","icao_code":"YSSY"},"destination":{"iata_code":"MEL","icao_code":"YMML"}}}}"#,
        ))
    } else {
        respond(&Reply {
            status: 404,
            retry_after: None,
            body: r#"{"response":"unknown callsign"}"#.into(),
        })
    }
}

async fn serve(script: Vec<Reply>, fallback: Reply, route_delay: Duration) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock {
        script: Mutex::new(script.into()),
        fallback,
        hits: Mutex::new(vec![]),
        route_hits: Mutex::new(vec![]),
        route_delay,
    });
    let app = Router::new()
        .route("/v2/lat/{lat}/lon/{lon}/dist/{r}", get(positions))
        .route("/route/{cs}", get(route_lookup))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (base, mock)
}

struct Running {
    store: Store,
    health: Arc<SourceHealth>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start(base: &str, extra_cfg: &str, interval: Duration, cap: Duration) -> Running {
    let cfg: AircraftConfig = toml::from_str(&format!(
        "url = \"{base}/v2/lat/{{lat}}/lon/{{lon}}/dist/{{radius_nm}}\"\n\
         location = {{ lat = -33.9461, lon = 151.1772 }}\n\
         {extra_cfg}"
    ))
    .unwrap();
    let tuning = Tuning {
        interval,
        backoff_cap: cap,
        routes_base: format!("{base}/route"),
        route_timeout: Duration::from_secs(5),
        ..Tuning::from_config(&cfg)
    };
    let store = Store::new();
    let health = Arc::new(SourceHealth::default());
    let (stop, rx) = watch::channel(false);
    let ctx = SourceCtx {
        name: "aircraft".into(),
        store: store.writer("aircraft"),
        shutdown: Shutdown(rx),
        health: health.clone(),
    };
    let task = tokio::spawn(Box::new(AircraftSource::with_tuning(&cfg, tuning)).run(ctx));
    Running {
        store,
        health,
        stop,
        task,
    }
}

impl Running {
    async fn finish(self) {
        let _ = self.stop.send(true);
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .expect("source stops promptly")
            .unwrap()
            .unwrap();
    }
}

async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

fn text_of(store: &Store, key: &str) -> Option<String> {
    match store.snapshot().get(key).map(|e| e.value.clone()) {
        Some(Value::Text(t)) => Some(t),
        _ => None,
    }
}

#[tokio::test]
async fn polls_fixture_rounds_location_and_looks_up_routes_in_the_background() {
    // A slow route lookup must not delay positions.
    let (base, mock) = serve(vec![], ok(FI), Duration::from_millis(150)).await;
    let r = start(
        &base,
        "include_ground = true\nradius_nm = 10\n",
        Duration::from_millis(40),
        Duration::from_millis(200),
    );
    wait_for("positions", || {
        r.store.snapshot().contains_key("aircraft:count")
    })
    .await;
    let snap = r.store.snapshot();
    assert_eq!(
        snap["aircraft:count"].value,
        Value::Number {
            value: 5.0,
            unit: None
        }
    );
    assert_eq!(
        text_of(&r.store, "aircraft:nearest.callsign").unwrap(),
        "JST520"
    );
    assert_eq!(text_of(&r.store, "aircraft:nearest.type").unwrap(), "A320");
    // Measured from the exact configured point (-33.9461, 151.1772), not
    // the point the fixture was captured at (-33.94, 151.18, from which the
    // provider says SW): the aircraft is ~0.1 NM north of the exact point.
    assert_eq!(
        text_of(&r.store, "aircraft:nearest.bearing_compass").unwrap(),
        "N"
    );
    assert!(
        !snap.contains_key("aircraft:nearest.route"),
        "positions arrive before the slow route lookup finishes"
    );
    assert!(r.health.up.load(Ordering::Relaxed));

    wait_for("route", || {
        text_of(&r.store, "aircraft:nearest.route").as_deref() == Some("SYD→MEL")
    })
    .await;
    assert_eq!(text_of(&r.store, "aircraft:nearest.origin").unwrap(), "SYD");

    // Lookups run one after another in the background.
    wait_for("all lookups", || mock.route_hits.lock().unwrap().len() >= 4).await;
    // Let several more polls happen; the cache means one lookup per callsign.
    let seen = mock.hits.lock().unwrap().len();
    wait_for("more polls", || mock.hits.lock().unwrap().len() >= seen + 4).await;
    r.finish().await;
    let mut rh = mock.route_hits.lock().unwrap().clone();
    rh.sort();
    // Only the callsigned aircraft in the list (the vehicle has none).
    assert_eq!(rh, ["JST520", "QFA148", "SIA211", "SSM1"]);

    let hits = mock.hits.lock().unwrap();
    // The location in the request is rounded to 2 decimals, and the radius
    // widened by the rounding error (10 NM + 0.42, rounded up).
    assert!(
        hits.iter()
            .all(|(_, p)| p == "/v2/lat/-33.95/lon/151.18/dist/11"),
        "{:?}",
        hits[0]
    );
    // Never faster than the interval.
    for w in hits.windows(2) {
        assert!(
            w[1].0 - w[0].0 >= Duration::from_millis(38),
            "polled too fast"
        );
    }
}

#[tokio::test]
async fn ground_traffic_is_dropped_by_default_and_routes_can_be_off() {
    let (base, mock) = serve(vec![], ok(FI), Duration::ZERO).await;
    let r = start(
        &base,
        "routes = false\n",
        Duration::from_millis(30),
        Duration::from_millis(100),
    );
    wait_for("count", || {
        r.store.snapshot().contains_key("aircraft:count")
    })
    .await;
    // Everything in the fixture is on the ground.
    assert_eq!(
        r.store.snapshot()["aircraft:count"].value,
        Value::Number {
            value: 0.0,
            unit: None
        }
    );
    assert!(text_of(&r.store, "aircraft:nearest.callsign").is_none());
    r.finish().await;
    assert!(mock.route_hits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn readsb_style_response_works_end_to_end() {
    let (base, _mock) = serve(vec![], ok(READSB), Duration::ZERO).await;
    let r = start(
        &base,
        "routes = false\nmax_altitude_ft = 15000\nradius_nm = 20\n",
        Duration::from_millis(30),
        Duration::from_millis(100),
    );
    wait_for("count", || {
        r.store.snapshot().contains_key("aircraft:count")
    })
    .await;
    // Within 20 NM, airborne, below 15000 ft: QFA12, VOZ450 (about 8 NM) and
    // the aircraft with only alt_geom. The 35000 ft one and the ground one go.
    let snap = r.store.snapshot();
    assert_eq!(
        snap["aircraft:count"].value,
        Value::Number {
            value: 3.0,
            unit: None
        }
    );
    // The alt_geom-only aircraft (7c0a05, no callsign) is nearest.
    assert!(!snap.contains_key("aircraft:nearest.callsign"));
    assert_eq!(
        snap["aircraft:nearest.alt_ft"].value,
        Value::Number {
            value: 4200.0,
            unit: None
        }
    );
    r.finish().await;
}

#[tokio::test]
async fn honours_retry_after_on_429() {
    let throttled = Reply {
        status: 429,
        retry_after: Some("1"),
        body: "slow down".into(),
    };
    let (base, mock) = serve(vec![throttled], ok(FI), Duration::ZERO).await;
    let r = start(
        &base,
        "include_ground = true\nroutes = false\n",
        Duration::from_millis(20),
        Duration::from_millis(100),
    );
    wait_for("429", || !mock.hits.lock().unwrap().is_empty()).await;
    wait_for("error counted", || {
        r.health.errors.load(Ordering::Relaxed) == 1
    })
    .await;
    assert!(!r.health.up.load(Ordering::Relaxed));
    wait_for("recovery", || {
        r.store.snapshot().contains_key("aircraft:count")
    })
    .await;
    assert!(r.health.up.load(Ordering::Relaxed));
    r.finish().await;
    let hits = mock.hits.lock().unwrap();
    let gap = hits[1].0 - hits[0].0;
    assert!(gap >= Duration::from_millis(990), "gap {gap:?}");
    assert!(gap < Duration::from_millis(2500), "gap {gap:?}");
}

#[tokio::test]
async fn errors_back_off_exponentially_and_keep_old_values() {
    let fail = Reply {
        status: 500,
        retry_after: None,
        body: "oops".into(),
    };
    let script = vec![
        ok(FI),
        fail.clone(),
        fail.clone(),
        fail.clone(),
        fail.clone(),
    ];
    let (base, mock) = serve(script, ok(FI), Duration::ZERO).await;
    let r = start(
        &base,
        "include_ground = true\nroutes = false\n",
        Duration::from_millis(30),
        Duration::from_millis(200),
    );
    wait_for("first failure", || mock.hits.lock().unwrap().len() >= 3).await;
    // The old values are still there, and the source is marked down.
    let snap = r.store.snapshot();
    assert!(snap.contains_key("aircraft:count"));
    assert!(snap.contains_key("aircraft:nearest.callsign"));
    assert!(!r.health.up.load(Ordering::Relaxed));

    wait_for("recovery", || mock.hits.lock().unwrap().len() >= 7).await;
    assert_eq!(r.health.errors.load(Ordering::Relaxed), 4);
    r.finish().await;
    let hits = mock.hits.lock().unwrap();
    let gaps: Vec<u128> = hits
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).as_millis())
        .collect();
    // interval, then 2x, 4x, 8x (capped at 200) and 200 after the failures.
    assert!(gaps[0] >= 28, "{gaps:?}");
    assert!(gaps[1] >= 58, "{gaps:?}");
    assert!(gaps[2] >= 118, "{gaps:?}");
    assert!(gaps[3] >= 198, "{gaps:?}");
    assert!(gaps[4] >= 198, "{gaps:?}");
    assert!(gaps[3] < 600 && gaps[4] < 600, "{gaps:?}");
    // Back to the normal interval after the recovery.
    assert!(gaps[5] >= 28 && gaps[5] < 190, "{gaps:?}");
}

#[tokio::test]
async fn empty_poll_removes_nearest_keys() {
    let (base, _mock) = serve(vec![ok(READSB)], ok(r#"{"aircraft":[]}"#), Duration::ZERO).await;
    let r = start(
        &base,
        "routes = false\nradius_nm = 20\n",
        Duration::from_millis(30),
        Duration::from_millis(100),
    );
    wait_for("first poll", || {
        r.store.snapshot().contains_key("aircraft:nearest.alt_ft")
    })
    .await;
    wait_for("empty poll", || {
        let s = r.store.snapshot();
        s.get("aircraft:count").map(|e| e.value.clone())
            == Some(Value::Number {
                value: 0.0,
                unit: None,
            })
    })
    .await;
    let snap = r.store.snapshot();
    assert!(!snap.keys().any(|k| k.starts_with("aircraft:nearest.")));
    r.finish().await;
}

#[tokio::test]
async fn shutdown_interrupts_a_long_wait() {
    let throttled = Reply {
        status: 429,
        retry_after: Some("3600"),
        body: String::new(),
    };
    let (base, mock) = serve(vec![], throttled, Duration::ZERO).await;
    let r = start(
        &base,
        "routes = false\n",
        Duration::from_millis(20),
        Duration::from_millis(100),
    );
    wait_for("429", || mock.hits.lock().unwrap().len() == 1).await;
    r.finish().await;
}

#[test]
fn distances_use_the_exact_location_not_the_rounded_query_point() {
    // An aircraft exactly at the configured point: the provider (queried at
    // the rounded point) would report it ~0.4 NM away; we report 0.
    let exact = LatLon {
        lat: -33.9461,
        lon: 151.1772,
    };
    let body = br#"{"aircraft":[{"hex":"abc123","lat":-33.9461,"lon":151.1772,"alt_baro":1200,"dst":0.42,"dir":45.0}]}"#;
    let list = parse_response(body, exact).unwrap();
    assert!(list[0].dist_nm < 1e-6, "{}", list[0].dist_nm);
}

#[test]
fn query_radius_covers_the_rounding_error_and_caps_at_250() {
    assert_eq!(query_radius_nm(10, 2), 11); // +0.42 NM
    assert_eq!(query_radius_nm(10, 4), 11); // +0.004 NM, still rounded up
    assert_eq!(query_radius_nm(10, 1), 15); // +4.2 NM
    assert_eq!(query_radius_nm(249, 2), 250);
    assert_eq!(query_radius_nm(250, 0), 250);
}
