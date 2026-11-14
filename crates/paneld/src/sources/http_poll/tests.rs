use std::time::Duration;

use super::*;
use crate::sources::poll::testutil::{ok, run, serve, serve_fn, status, wait_for, Reply};
use crate::store::{Store, Value};

const BODY: &str = r#"{
  "current": {"temp": 21.5, "icon": "sunny", "humidity": "63", "nothing": null, "rain": false},
  "list": [{"main": {"t": 3.25}}, {"main": {"t": 4}}],
  "meta": {"a.b": 7}
}"#;

fn cfg(url: &str, extra: &str) -> HttpPollConfig {
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
            retry_after_max: Duration::from_secs(60),
            ..Limits::default()
        },
    )
}

fn source(c: &HttpPollConfig) -> HttpPollSource {
    let (t, l) = fast();
    HttpPollSource::new(c).unwrap().with_timing(t, l)
}

fn num(store: &Store, key: &str) -> Option<(f64, Option<String>)> {
    match store.snapshot().get(key).map(|e| e.value.clone()) {
        Some(Value::Number { value, unit }) => Some((value, unit)),
        _ => None,
    }
}

fn text(store: &Store, key: &str) -> Option<String> {
    match store.snapshot().get(key).map(|e| e.value.clone()) {
        Some(Value::Text(t)) => Some(t),
        _ => None,
    }
}

#[test]
fn changes_cover_nested_paths_arrays_strings_missing_and_null() {
    let c = cfg(
        "http://x",
        r#"extract = {
            "w:temp" = { path = "$.current.temp", unit = "C" },
            "w:icon" = "$.current.icon",
            "w:hum" = "$.current.humidity",
            "w:null" = "$.current.nothing",
            "w:missing" = "$.current.nope.deeper",
            "w:rain" = "$.current.rain",
            "w:t1" = "$.list[1].main.t",
            "w:dotted" = '$.meta["a.b"]',
            "w:list" = "$.list"
        }"#,
    );
    let rules = HttpPollSource::new(&c).unwrap().rules;
    let root: Json = serde_json::from_str(BODY).unwrap();
    let changes = changes_for(&rules, &root);
    let get = |k: &str| {
        changes
            .iter()
            .find(|c| matches!(c, Change::Set(x, _) | Change::Remove(x) if x == k))
            .unwrap()
            .clone()
    };
    let n = |v: f64, unit: Option<&str>| {
        Change::Set(
            String::new(),
            Value::Number {
                value: v,
                unit: unit.map(Into::into),
            },
        )
    };
    let same = |c: Change, k: &str, expect: Change| {
        let rekey = |c: Change| match c {
            Change::Set(_, v) => Change::Set(k.into(), v),
            o => o,
        };
        assert_eq!(c, rekey(expect), "{k}");
    };
    same(get("w:temp"), "w:temp", n(21.5, Some("C")));
    same(get("w:hum"), "w:hum", n(63.0, None));
    same(get("w:t1"), "w:t1", n(4.0, None));
    same(get("w:dotted"), "w:dotted", n(7.0, None));
    assert_eq!(
        get("w:icon"),
        Change::Set("w:icon".into(), Value::Text("sunny".into()))
    );
    assert_eq!(
        get("w:rain"),
        Change::Set("w:rain".into(), Value::Bool(false))
    );
    assert_eq!(get("w:null"), Change::Remove("w:null".into()));
    assert_eq!(get("w:missing"), Change::Remove("w:missing".into()));
    let Change::Set(_, Value::Json(list)) = get("w:list") else {
        panic!("list should be Json");
    };
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(changes.len(), 9, "every key is written or removed, once");
}

#[tokio::test]
async fn polls_and_writes_in_one_batch_and_sends_headers() {
    std::env::set_var("PANELD_TEST_HTTP_POLL_KEY_A", "s3cret-a");
    let (base, server) = serve(vec![], ok(BODY)).await;
    let c = cfg(
        &format!("{base}/forecast?lat=1&appid=abc"),
        r#"extract = { "w:temp" = { path = "$.current.temp", unit = "C" }, "w:icon" = "$.current.icon" }
           headers = { "Accept" = "application/json" }
           header_env = { "X-Api-Key" = "PANELD_TEST_HTTP_POLL_KEY_A" }"#,
    );
    let r = run("weather", source(&c));
    wait_for("both keys", || r.store.snapshot().len() == 2).await;
    assert_eq!(num(&r.store, "w:temp"), Some((21.5, Some("C".into()))));
    assert_eq!(text(&r.store, "w:icon").as_deref(), Some("sunny"));
    assert!(r.up());
    let hit = server.hits.lock().unwrap()[0].clone();
    assert_eq!(hit.path, "/forecast");
    assert_eq!(hit.query["appid"], "abc", "query string is kept");
    assert_eq!(hit.headers["x-api-key"], "s3cret-a");
    assert_eq!(hit.headers["accept"], "application/json");
    r.finish().await;
}

#[tokio::test]
async fn missing_value_in_a_good_response_removes_the_key() {
    let (base, _s) = serve(
        vec![ok(r#"{"a": 1, "b": 2}"#)],
        ok(r#"{"a": 5, "b": null}"#),
    )
    .await;
    let c = cfg(&base, r#"extract = { "w:a" = "$.a", "w:b" = "$.b" }"#);
    let r = run("w", source(&c));
    wait_for("b removed", || {
        num(&r.store, "w:a").is_some_and(|(v, _)| v == 5.0) && num(&r.store, "w:b").is_none()
    })
    .await;
    r.finish().await;
}

#[tokio::test]
async fn failed_polls_keep_old_values_and_recover_after_429_and_500() {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let (base, server) =
        serve_fn(
            move |_, _| match calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) {
                0 => ok(r#"{"v": 1}"#),
                1 => Reply {
                    retry_after: Some("1"),
                    ..status(429, "slow down")
                },
                2 => status(500, "oops"),
                3 => ok("not json"),
                _ => ok(r#"{"v": 2}"#),
            },
        )
        .await;
    let c = cfg(&base, r#"extract = { "w:v" = "$.v" }"#);
    let r = run("w", source(&c));
    wait_for("first value", || num(&r.store, "w:v").is_some()).await;
    wait_for("error counted", || r.errors() >= 1).await;
    assert_eq!(num(&r.store, "w:v").unwrap().0, 1.0, "old value kept");
    wait_for("recovery", || {
        num(&r.store, "w:v").is_some_and(|(v, _)| v == 2.0)
    })
    .await;
    assert!(r.errors() >= 3, "429, 500 and bad JSON each count");
    assert!(r.up());

    let hits = server.hits.lock().unwrap().clone();
    // After the 429 with Retry-After: 1 nothing is sent for ~1 s.
    assert!(
        hits[2].at - hits[1].at >= Duration::from_millis(950),
        "Retry-After honoured: {:?}",
        hits[2].at - hits[1].at
    );
    // After the 500 the wait doubles (20 ms interval -> 40 ms), never below the interval.
    assert!(hits[3].at - hits[2].at >= Duration::from_millis(35));
    r.finish().await;
}

#[tokio::test]
async fn never_polls_faster_than_the_interval() {
    let (base, server) = serve(vec![], ok(r#"{"v": 1}"#)).await;
    let c = cfg(&base, r#"extract = { "w:v" = "$.v" }"#);
    let r = run("w", source(&c));
    wait_for("5 polls", || server.hits.lock().unwrap().len() >= 5).await;
    r.finish().await;
    let hits = server.hits.lock().unwrap().clone();
    for w in hits.windows(2) {
        assert!(w[1].at - w[0].at >= Duration::from_millis(19));
    }
}

#[tokio::test]
async fn does_not_follow_a_redirect_to_another_host() {
    std::env::set_var("PANELD_TEST_HTTP_POLL_KEY_R", "s3cret-r");
    let (other, other_server) = serve(vec![], ok(r#"{"v": 9}"#)).await;
    // 127.0.0.1 and localhost are different hosts.
    let target = format!("{}/steal", other.replace("127.0.0.1", "localhost"));
    let (base, _s) = serve_fn(move |_, _| Reply {
        location: Some(target.clone()),
        ..status(302, "")
    })
    .await;
    let c = cfg(
        &base,
        r#"extract = { "w:v" = "$.v" }
           header_env = { "X-Api-Key" = "PANELD_TEST_HTTP_POLL_KEY_R" }"#,
    );
    let r = run("w", source(&c));
    wait_for("error", || r.errors() >= 1).await;
    assert!(num(&r.store, "w:v").is_none());
    assert!(
        other_server.hits.lock().unwrap().is_empty(),
        "credentials must not travel"
    );
    r.finish().await;
}

#[test]
fn secrets_never_appear_in_debug_or_errors() {
    std::env::set_var("PANELD_TEST_HTTP_POLL_KEY_D", "hunter2-very-secret");
    let c = cfg(
        "http://x/?apikey=abc",
        r#"extract = { "w:v" = "$.v" }
           header_env = { "Authorization" = "PANELD_TEST_HTTP_POLL_KEY_D" }"#,
    );
    let s = HttpPollSource::new(&c).unwrap();
    for text in [
        format!("{s:?}"),
        format!("{:?}", s.headers),
        format!("{c:?}"),
    ] {
        assert!(!text.contains("hunter2"), "leaked in {text}");
    }
    // The header value itself is flagged sensitive.
    assert!(s.headers["authorization"].is_sensitive());

    // A value that is not a legal header value: the error names the variable only.
    std::env::set_var("PANELD_TEST_HTTP_POLL_KEY_E", "bad\nvalue-hunter3");
    let c = cfg(
        "http://x",
        r#"extract = { "w:v" = "$.v" }
           header_env = { "X-Key" = "PANELD_TEST_HTTP_POLL_KEY_E" }"#,
    );
    let e = format!("{:#}", HttpPollSource::new(&c).unwrap_err());
    assert!(
        e.contains("PANELD_TEST_HTTP_POLL_KEY_E") && !e.contains("hunter3"),
        "{e}"
    );

    // Unset variable: names it.
    let c = cfg(
        "http://x",
        r#"extract = { "w:v" = "$.v" }
           header_env = { "X-Key" = "PANELD_TEST_HTTP_POLL_UNSET" }"#,
    );
    assert!(check_environment(&c)
        .unwrap_err()
        .contains("PANELD_TEST_HTTP_POLL_UNSET"));
}

#[tokio::test]
async fn transport_errors_do_not_log_the_url() {
    // Nothing listens on this port: the failure text must not hold the URL.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let c = cfg(
        &format!("http://127.0.0.1:{port}/p?apikey=hunter4"),
        r#"extract = { "w:v" = "$.v" }"#,
    );
    let s = source(&c);
    let client = reqwest::Client::new();
    let store = Store::new();
    let o = poll_once(&client, &s, &store.writer("w")).await;
    let Outcome::Failed(m) = o else {
        panic!("{o:?}")
    };
    assert!(
        !m.contains("hunter4") && !m.contains("apikey") && !m.contains(&port.to_string()),
        "{m}"
    );
}
