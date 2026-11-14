use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::sources::poll::testutil::{ok, run, serve, serve_fn, status, wait_for, Reply};
use crate::store::Store;

// Shapes from the documented examples
// (https://prometheus.io/docs/prometheus/latest/querying/api/).
const VECTOR_TWO: &str = r#"{"status":"success","data":{"resultType":"vector","result":[
  {"metric":{"__name__":"up","job":"prometheus","instance":"localhost:9090"},"value":[1435781451.781,"1"]},
  {"metric":{"__name__":"up","job":"node","instance":"localhost:9100"},"value":[1435781451.781,"0"]}]}}"#;
const VECTOR_ONE: &str = r#"{"status":"success","data":{"resultType":"vector","result":[
  {"metric":{"job":"node"},"value":[1435781451.781,"412.5"]}]}}"#;
const VECTOR_EMPTY: &str = r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#;
const VECTOR_NAN: &str = r#"{"status":"success","data":{"resultType":"vector","result":[
  {"metric":{},"value":[1435781451.781,"NaN"]}]}}"#;
const VECTOR_INF: &str = r#"{"status":"success","data":{"resultType":"vector","result":[
  {"metric":{},"value":[1435781451.781,"+Inf"]}]}}"#;
const SCALAR: &str =
    r#"{"status":"success","data":{"resultType":"scalar","result":[1435781460.781,"3.5"]}}"#;
const MATRIX: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[
  {"metric":{"__name__":"up","job":"prometheus"},"values":[[1435781430.781,"1"],[1435781445.781,"2"],[1435781460.781,"NaN"],[1435781475.781,"4"]]},
  {"metric":{"__name__":"up","job":"node"},"values":[[1435781430.781,"0"]]}]}}"#;
const ERROR: &str = r#"{"status":"error","errorType":"bad_data","error":"invalid parameter \"query\": 1:5: parse error: unexpected end of input"}"#;

fn cfg(url: &str, extra: &str) -> PromConfig {
    toml::from_str(&format!("url = \"{url}\"\n{extra}")).unwrap()
}

fn fast() -> (Timing, Limits) {
    (
        Timing {
            interval: Duration::from_millis(20),
            backoff_cap: Duration::from_millis(200),
        },
        Limits {
            retry_after_default: Duration::from_millis(50),
            ..Limits::default()
        },
    )
}

fn source(c: &PromConfig) -> PromSource {
    let (t, l) = fast();
    PromSource::new(c).unwrap().with_timing(t, l)
}

fn num(store: &Store, key: &str) -> Option<(f64, Option<String>)> {
    match store.snapshot().get(key).map(|e| e.value.clone()) {
        Some(Value::Number { value, unit }) => Some((value, unit)),
        _ => None,
    }
}

// ---- parsing ---------------------------------------------------------------------

#[test]
fn instant_results() {
    assert_eq!(
        parse_instant(VECTOR_ONE.as_bytes()),
        Ok(Instant::Number(412.5))
    );
    assert_eq!(parse_instant(SCALAR.as_bytes()), Ok(Instant::Number(3.5)));
    assert_eq!(parse_instant(VECTOR_EMPTY.as_bytes()), Ok(Instant::NoValue));
    assert_eq!(parse_instant(VECTOR_NAN.as_bytes()), Ok(Instant::NoValue));
    assert_eq!(parse_instant(VECTOR_INF.as_bytes()), Ok(Instant::NoValue));
    assert_eq!(parse_instant(VECTOR_TWO.as_bytes()), Ok(Instant::Many(2)));
    // A native histogram has `histogram`, not `value`.
    let hist = r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{},"histogram":[1.0,{"count":"1","sum":"2","buckets":[]}]}]}}"#;
    assert_eq!(parse_instant(hist.as_bytes()), Ok(Instant::NoValue));
}

#[test]
fn instant_errors() {
    let e = parse_instant(ERROR.as_bytes()).unwrap_err();
    assert!(e.starts_with("bad_data: invalid parameter"), "{e}");
    let e = parse_instant(MATRIX.as_bytes()).unwrap_err();
    assert!(e.contains("matrix"), "{e}");
    let e =
        parse_instant(br#"{"status":"success","data":{"resultType":"string","result":[1,"x"]}}"#)
            .unwrap_err();
    assert!(e.contains("string"), "{e}");
    assert!(parse_instant(b"<html>").unwrap_err().contains("not JSON"));
    assert!(parse_instant(b"{}").unwrap_err().contains("status"));
}

#[test]
fn range_results_use_the_first_series_oldest_first_and_skip_nan() {
    let pts = parse_range(MATRIX.as_bytes()).unwrap();
    let vals: Vec<f64> = pts.iter().map(|p| p.1).collect();
    assert_eq!(vals, vec![1.0, 2.0, 4.0]);
    assert!(pts.windows(2).all(|w| w[0].0 < w[1].0));
    assert_eq!(
        pts[0].0,
        UNIX_EPOCH + Duration::from_secs_f64(1435781430.781)
    );
    let empty = br#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#;
    assert!(parse_range(empty).unwrap().is_empty());
    assert!(parse_range(VECTOR_ONE.as_bytes())
        .unwrap_err()
        .contains("vector"));
    assert!(parse_range(ERROR.as_bytes()).is_err());
}

#[test]
fn long_server_errors_are_capped() {
    let long = "x".repeat(5000);
    let body = format!(r#"{{"status":"error","errorType":"t","error":"{long}"}}"#);
    assert!(parse_instant(body.as_bytes()).unwrap_err().len() <= 210);
}

// ---- end to end ------------------------------------------------------------------

fn by_query(hit: &crate::sources::poll::testutil::Hit) -> Reply {
    let q = hit.query.get("query").map(String::as_str).unwrap_or("");
    match (hit.path.as_str(), q) {
        ("/api/v1/query", "one") => ok(VECTOR_ONE),
        ("/api/v1/query", "scalar") => ok(SCALAR),
        ("/api/v1/query", "empty") => ok(VECTOR_EMPTY),
        ("/api/v1/query", "nan") => ok(VECTOR_NAN),
        ("/api/v1/query", "multi") => ok(VECTOR_TWO),
        ("/api/v1/query", "bad") => status(400, ERROR),
        ("/api/v1/query_range", "series") => ok(MATRIX),
        _ => status(404, "no"),
    }
}

#[tokio::test]
async fn end_to_end_results_and_request_shape() {
    std::env::set_var("PANELD_TEST_PROM_BEARER_A", "prom-secret-a");
    let (base, server) = serve_fn(|h, _| by_query(h)).await;
    let c = cfg(
        &format!("{base}/"),
        r#"bearer_env = "PANELD_TEST_PROM_BEARER_A"
           queries = { "p:one" = { query = "one", unit = "W" }, "p:scalar" = "scalar",
                       "p:empty" = "empty", "p:nan" = "nan", "p:multi" = "multi", "p:bad" = "bad" }
           range_queries = { "p:hist" = { query = "series", range_s = 3600, step_s = 60 } }"#,
    );
    let r = run("prom", source(&c));
    wait_for("results", || {
        num(&r.store, "p:scalar").is_some() && r.errors() >= 2
    })
    .await;
    wait_for("hist", || r.store.snapshot().contains_key("p:hist")).await;
    assert_eq!(num(&r.store, "p:one"), Some((412.5, Some("W".into()))));
    assert_eq!(num(&r.store, "p:scalar"), Some((3.5, None)));
    let snap = r.store.snapshot();
    for k in ["p:empty", "p:nan", "p:multi", "p:bad"] {
        assert!(!snap.contains_key(k), "{k} must not be written");
    }
    let Value::Series(s) = &snap["p:hist"].value else {
        panic!("not a series");
    };
    assert_eq!(
        s.iter().map(|p| p.1).collect::<Vec<_>>(),
        vec![1.0, 2.0, 4.0]
    );
    assert!(!r.up(), "problems mark the source down");

    // All six instant and the range query are sent in each round, with the
    // bearer token, and the range window is exactly range_s with the step.
    let hits = server.hits.lock().unwrap().clone();
    assert!(hits
        .iter()
        .all(|h| h.headers["authorization"] == "Bearer prom-secret-a"));
    let range = hits
        .iter()
        .find(|h| h.path == "/api/v1/query_range")
        .unwrap();
    let f = |k: &str| range.query[k].parse::<f64>().unwrap();
    assert!((f("end") - f("start") - 3600.0).abs() < 0.01);
    assert_eq!(f("step"), 60.0);
    r.finish().await;
}

#[tokio::test]
async fn a_key_that_becomes_ambiguous_keeps_its_old_value() {
    let calls = AtomicUsize::new(0);
    let (base, _s) = serve_fn(move |_, _| {
        if calls.fetch_add(1, Ordering::Relaxed) == 0 {
            ok(VECTOR_ONE)
        } else {
            ok(VECTOR_TWO)
        }
    })
    .await;
    let c = cfg(&base, r#"queries = { "p:v" = "x" }"#);
    let r = run("prom", source(&c));
    wait_for("value", || num(&r.store, "p:v").is_some()).await;
    wait_for("error", || r.errors() >= 1).await;
    assert_eq!(num(&r.store, "p:v").unwrap().0, 412.5);
    r.finish().await;
}

#[tokio::test]
async fn empty_vector_removes_an_existing_key() {
    let calls = AtomicUsize::new(0);
    let (base, _s) = serve_fn(move |_, _| {
        if calls.fetch_add(1, Ordering::Relaxed) == 0 {
            ok(VECTOR_ONE)
        } else {
            ok(VECTOR_EMPTY)
        }
    })
    .await;
    let c = cfg(&base, r#"queries = { "p:v" = "x" }"#);
    let r = run("prom", source(&c));
    wait_for("value", || num(&r.store, "p:v").is_some()).await;
    wait_for("removed", || num(&r.store, "p:v").is_none()).await;
    r.finish().await;
}

#[tokio::test]
async fn rate_limit_then_server_error_then_recovery() {
    let (base, server) = serve(
        vec![
            ok(VECTOR_ONE),
            Reply {
                retry_after: Some("1"),
                ..status(429, "")
            },
            status(500, "oops"),
        ],
        ok(SCALAR),
    )
    .await;
    let c = cfg(&base, r#"queries = { "p:v" = "x" }"#);
    let r = run("prom", source(&c));
    wait_for("first", || {
        num(&r.store, "p:v").is_some_and(|v| v.0 == 412.5)
    })
    .await;
    wait_for("recovered", || {
        num(&r.store, "p:v").is_some_and(|v| v.0 == 3.5)
    })
    .await;
    assert!(r.errors() >= 2);
    assert!(r.up());
    let hits = server.hits.lock().unwrap().clone();
    assert!(
        hits[2].at - hits[1].at >= Duration::from_millis(950),
        "Retry-After honoured"
    );
    assert!(
        hits[3].at - hits[2].at >= Duration::from_millis(35),
        "backoff after 500"
    );
    r.finish().await;
}

#[tokio::test]
async fn failed_round_keeps_old_values() {
    let calls = AtomicUsize::new(0);
    let (base, _s) = serve_fn(move |_, _| {
        if calls.fetch_add(1, Ordering::Relaxed) == 0 {
            ok(VECTOR_ONE)
        } else {
            status(503, "{}")
        }
    })
    .await;
    let c = cfg(&base, r#"queries = { "p:v" = "x" }"#);
    let r = run("prom", source(&c));
    wait_for("value", || num(&r.store, "p:v").is_some()).await;
    wait_for("error", || r.errors() >= 1).await;
    assert_eq!(num(&r.store, "p:v").unwrap().0, 412.5);
    r.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queries_run_concurrently_but_within_the_limit() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    // The handler is synchronous, so measure overlap with a blocking sleep
    // in the multi-threaded runtime.
    let (i, p) = (in_flight.clone(), peak.clone());
    let (base, _s) = serve_fn(move |_, _| {
        let now = i.fetch_add(1, Ordering::SeqCst) + 1;
        p.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(40));
        i.fetch_sub(1, Ordering::SeqCst);
        ok(SCALAR)
    })
    .await;
    let queries: String = (0..12)
        .map(|n| format!("\"p:k{n}\" = \"q{n}\", "))
        .collect();
    let c = cfg(&base, &format!("queries = {{ {queries} }}"));
    let r = run("prom", source(&c));
    wait_for("all keys", || r.store.snapshot().len() == 12).await;
    r.finish().await;
    let peak = peak.load(Ordering::SeqCst);
    assert!(peak <= CONCURRENCY, "peak {peak}");
}

// ---- secrets ---------------------------------------------------------------------

#[test]
fn bearer_token_never_appears_in_debug_or_errors() {
    std::env::set_var("PANELD_TEST_PROM_BEARER_B", "hunter2-prom-token");
    let c = cfg(
        "http://x",
        r#"bearer_env = "PANELD_TEST_PROM_BEARER_B"
           queries = { "p:v" = "x" }"#,
    );
    let s = PromSource::new(&c).unwrap();
    for t in [
        format!("{s:?}"),
        format!("{:?}", s.headers),
        format!("{c:?}"),
    ] {
        assert!(!t.contains("hunter2"), "leaked in {t}");
    }
    assert!(s.headers[AUTHORIZATION].is_sensitive());

    std::env::set_var("PANELD_TEST_PROM_BEARER_C", "bad\nvalue-hunter3");
    let c = cfg(
        "http://x",
        r#"bearer_env = "PANELD_TEST_PROM_BEARER_C"
           queries = { "p:v" = "x" }"#,
    );
    let e = format!("{:#}", PromSource::new(&c).unwrap_err());
    assert!(
        e.contains("PANELD_TEST_PROM_BEARER_C") && !e.contains("hunter3"),
        "{e}"
    );

    let c = cfg(
        "http://x",
        r#"bearer_env = "PANELD_TEST_PROM_BEARER_UNSET"
           queries = { "p:v" = "x" }"#,
    );
    let e = check_environment(&c).unwrap_err();
    assert!(e.contains("PANELD_TEST_PROM_BEARER_UNSET"), "{e}");
}
