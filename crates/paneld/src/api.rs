//! HTTP control API (DESIGN.md §7.1).
//!
//! Every route sits on the shared [`Control`] handle (commands to the render
//! loop, status back) except `POST /api/data/{key}`, which writes straight
//! into the store as the source `http`.
//!
//! Errors are always `{ "error": "..." }` with a fitting status. When
//! `[http] token_env` is set, every `/api/*` route needs
//! `Authorization: Bearer <token>`; the preview routes are separate and stay
//! open.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value as Json_};

use crate::config::HttpConfig;
use crate::control::{Command, Control, NotifyRequest, Status};
use crate::sources::ha::{read_secret, Secret};
use crate::store::{StoreWriter, Value};

/// Largest accepted request body.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Most keys `POST /api/data` may hold at once.
pub const MAX_PUSHED_KEYS: usize = 1000;
/// Longest accepted key name (the part after `<prefix>:`).
pub const MAX_KEY_NAME_LEN: usize = 128;
/// The store source name pushed values are stamped with.
pub const PUSH_SOURCE: &str = "http";

/// What the router needs from `[http]`. `Debug` never shows the token.
#[derive(Debug, Clone)]
pub struct ApiSettings {
    /// Bearer token; `None` leaves the API open.
    pub token: Option<Secret>,
    /// Pushed keys must start with `<data_prefix>:`.
    pub data_prefix: String,
}

impl ApiSettings {
    /// Read the token named by `token_env` (an error naming the variable if
    /// it is unset or empty) and warn when the API is open on a
    /// non-loopback address.
    pub fn from_config(http: &HttpConfig) -> Result<ApiSettings, String> {
        let token = http
            .token_env
            .as_deref()
            .map(|var| read_secret(var, "the API bearer token (`[http] token_env`)"))
            .transpose()?;
        if token.is_none() && !http.bind.ip().is_loopback() {
            tracing::warn!(
                addr = %http.bind,
                "the control API is unauthenticated and not bound to loopback: anyone on the \
                 network can change the display; set [http] token_env"
            );
        }
        Ok(ApiSettings {
            token,
            data_prefix: http.data_prefix.clone(),
        })
    }
}

struct Api {
    control: Control,
    writer: StoreWriter,
    settings: ApiSettings,
}

type Shared = Arc<Api>;

/// The control API routes. `store` is where `POST /api/data` writes.
pub fn router(control: Control, store: &crate::store::Store, settings: ApiSettings) -> Router {
    let api = Arc::new(Api {
        control,
        writer: store.writer(PUSH_SOURCE),
        settings,
    });
    Router::new()
        .route("/api/state", get(state))
        .route("/api/notify", post(notify).delete(clear_notifications))
        .route("/api/data/{key}", post(push_data))
        .route("/api/scene/{name}", post(pin_scene))
        .route("/api/scene", delete(unpin_scene))
        .route("/api/brightness", post(set_brightness))
        .route("/api/power", post(set_power))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(api.clone(), authenticate))
        .with_state(api)
}

// ---------------------------------------------------------------------------
// Errors

/// An error response: a status and `{ "error": message }`.
#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// The render loop is gone.
fn unavailable(msg: String) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, msg)
}

fn accepted() -> (StatusCode, Json<Json_>) {
    (StatusCode::ACCEPTED, Json(json!({ "status": "accepted" })))
}

/// Parse a JSON body. The content type is not checked (`curl -d` sends a
/// form type), and every failure is a clear JSON error instead of axum's
/// plain-text rejection.
fn parse_body<T: DeserializeOwned>(
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<T, ApiError> {
    let bytes = body.map_err(|e| match e.status() {
        StatusCode::PAYLOAD_TOO_LARGE => ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("request body is larger than {MAX_BODY_BYTES} bytes"),
        ),
        s => ApiError(s, "could not read the request body".into()),
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}

// ---------------------------------------------------------------------------
// Auth

/// Compare two byte strings without stopping at the first difference. The
/// work depends on the longer length only, so the token's length is the one
/// thing a timing attacker could learn, which the header already shows.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// The token from `Authorization: Bearer <token>` (scheme is case-insensitive).
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

async fn authenticate(State(api): State<Shared>, req: Request, next: Next) -> Response {
    let Some(token) = &api.settings.token else {
        return next.run(req).await;
    };
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer)
        .is_some_and(|given| constant_time_eq(given.as_bytes(), token.expose().as_bytes()));
    if ok {
        return next.run(req).await;
    }
    let mut resp = ApiError(
        StatusCode::UNAUTHORIZED,
        "missing or invalid bearer token".into(),
    )
    .into_response();
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

// ---------------------------------------------------------------------------
// Routes

/// `GET /api/state`: the render loop's latest [`Status`].
async fn state(State(api): State<Shared>) -> Json<Status> {
    Json((*api.control.status()).clone())
}

/// `POST /api/notify`: queue a notification (202).
async fn notify(
    State(api): State<Shared>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let req: NotifyRequest = parse_body(body)?;
    let n = req
        .validate(&api.control.status().scenes)
        .map_err(ApiError::bad_request)?;
    api.control.send(Command::Notify(n)).map_err(unavailable)?;
    Ok(accepted())
}

/// `DELETE /api/notify`: drop every notification (202).
async fn clear_notifications(State(api): State<Shared>) -> Result<impl IntoResponse, ApiError> {
    api.control
        .send(Command::ClearNotifications)
        .map_err(unavailable)?;
    Ok(accepted())
}

/// `POST /api/scene/{name}`: pin a scene (202, or 404 for an unknown name).
async fn pin_scene(
    State(api): State<Shared>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !api.control.status().scenes.contains(&name) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no scene named {name:?}"),
        ));
    }
    api.control
        .send(Command::Pin(Some(name)))
        .map_err(unavailable)?;
    Ok(accepted())
}

/// `DELETE /api/scene`: release the pin (202).
async fn unpin_scene(State(api): State<Shared>) -> Result<impl IntoResponse, ApiError> {
    api.control.send(Command::Pin(None)).map_err(unavailable)?;
    Ok(accepted())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrightnessBody {
    value: u8,
}

/// `POST /api/brightness`: `{ "value": 0-255 }` (202).
async fn set_brightness(
    State(api): State<Shared>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let b: BrightnessBody = parse_body(body)?;
    api.control
        .send(Command::SetBrightness(b.value))
        .map_err(unavailable)?;
    Ok(accepted())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PowerBody {
    on: bool,
}

/// `POST /api/power`: `{ "on": true }` (202).
async fn set_power(
    State(api): State<Shared>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let b: PowerBody = parse_body(body)?;
    api.control
        .send(Command::SetPower(b.on))
        .map_err(unavailable)?;
    Ok(accepted())
}

/// Check a pushed key: `<data_prefix>:<name>` with a plain name.
fn check_key(key: &str, prefix: &str) -> Result<(), String> {
    if !crate::condition::is_valid_key(key) {
        return Err(format!(
            "{key:?} is not a valid key; keys look like `{prefix}:name`"
        ));
    }
    let name = key.strip_prefix(prefix).and_then(|r| r.strip_prefix(':'));
    let Some(name) = name else {
        return Err(format!(
            "pushed keys must start with `{prefix}:` (other prefixes belong to sources)"
        ));
    };
    if name.len() > MAX_KEY_NAME_LEN
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err(format!(
            "the part after `{prefix}:` must be 1 to {MAX_KEY_NAME_LEN} characters of letters, digits, '_', '.' and '-'"
        ));
    }
    Ok(())
}

/// What a pushed body means: set a value, or remove the key.
fn parse_push(body: &Json_) -> Result<Option<Value>, String> {
    let (value, unit) = match body {
        Json_::Object(m) => {
            if let Some(extra) = m.keys().find(|k| *k != "value" && *k != "unit") {
                return Err(format!(
                    "unknown field `{extra}`; an object body is {{ \"value\": ..., \"unit\": \"W\" }}"
                ));
            }
            let value = m
                .get("value")
                .ok_or("an object body needs a `value` field")?;
            let unit = match m.get("unit") {
                None | Some(Json_::Null) => None,
                Some(Json_::String(u)) => Some(u.as_str()),
                Some(_) => return Err("`unit` must be a string".into()),
            };
            (value, unit)
        }
        v => (v, None),
    };
    if matches!(value, Json_::Array(_) | Json_::Object(_)) {
        return Err("the value must be a number, string, boolean or null".into());
    }
    let parsed = crate::extract::to_value(value, unit);
    if unit.is_some() && !matches!(parsed, Some(Value::Number { .. }) | None) {
        return Err("`unit` only applies to numbers".into());
    }
    if parsed.is_none() && !value.is_null() {
        return Err("the number is not finite".into());
    }
    Ok(parsed)
}

/// `POST /api/data/{key}`: set (200) or, with `null`, remove a pushed value.
async fn push_data(
    State(api): State<Shared>,
    Path(key): Path<String>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    check_key(&key, &api.settings.data_prefix).map_err(ApiError::bad_request)?;
    let json: Json_ = parse_body(body)?;
    let value = parse_push(&json).map_err(ApiError::bad_request)?;

    // Never overwrite or remove another source's value (defence in depth:
    // config validation already keeps the prefix out of the sources'
    // namespaces), and bound how many keys can be pushed.
    let snap = api.writer.snapshot();
    match snap.get(&key) {
        Some(e) if &*e.source != PUSH_SOURCE => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("{key:?} is written by the source {:?}", &*e.source),
            ));
        }
        None if value.is_some()
            && snap.values().filter(|e| &*e.source == PUSH_SOURCE).count() >= MAX_PUSHED_KEYS =>
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("at most {MAX_PUSHED_KEYS} keys can be pushed; remove some with null"),
            ));
        }
        _ => {}
    }
    match value {
        Some(v) => api.writer.set(key, v),
        None => api.writer.remove(&key),
    }
    Ok(Json(json!({ "status": "ok" })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{channel, ControlLink};
    use crate::store::Store;
    use std::future::IntoFuture;
    use std::sync::mpsc::TryRecvError;

    struct Harness {
        addr: std::net::SocketAddr,
        link: ControlLink,
        store: Store,
        client: reqwest::Client,
    }

    async fn harness(token: Option<&str>) -> Harness {
        let (control, link, _out) = channel(vec!["home".into(), "alert".into()]);
        let store = Store::new();
        let settings = ApiSettings {
            token: token.map(Secret::for_test),
            data_prefix: "push".into(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router(control, &store, settings)).into_future());
        Harness {
            addr,
            link,
            store,
            client: crate::sources::http_client().build().unwrap(),
        }
    }

    impl Harness {
        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }
        async fn send(
            &self,
            method: reqwest::Method,
            path: &str,
            body: Option<&str>,
        ) -> (StatusCode, Json_) {
            let mut r = self.client.request(method, self.url(path));
            if let Some(b) = body {
                r = r.body(b.to_owned());
            }
            let resp = r.send().await.unwrap();
            let status = resp.status().as_u16();
            let json = resp.json().await.unwrap_or(Json_::Null);
            (StatusCode::from_u16(status).unwrap(), json)
        }
        async fn post(&self, path: &str, body: &str) -> (StatusCode, Json_) {
            self.send(reqwest::Method::POST, path, Some(body)).await
        }
        fn cmd(&self) -> Result<Command, TryRecvError> {
            self.link.commands.try_recv()
        }
    }

    fn error_of(j: &Json_) -> &str {
        j["error"]
            .as_str()
            .unwrap_or_else(|| panic!("no error in {j}"))
    }

    #[tokio::test]
    async fn state_returns_the_status_as_json() {
        let h = harness(None).await;
        let (s, body) = h.send(reqwest::Method::GET, "/api/state", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["scenes"], json!(["home", "alert"]));
        assert_eq!(body["on"], true);
        assert_eq!(body["brightness"], 255);
        assert_eq!(body["pinned"], Json_::Null);
    }

    #[tokio::test]
    async fn notify_queues_and_validates() {
        let h = harness(None).await;
        let (s, body) = h
            .post("/api/notify", r#"{"text": "Washing done", "priority": 5}"#)
            .await;
        assert_eq!(
            (s, &body["status"]),
            (StatusCode::ACCEPTED, &json!("accepted"))
        );
        match h.cmd().unwrap() {
            Command::Notify(n) => {
                assert_eq!(n.priority, 5);
                assert_eq!(
                    n.content,
                    crate::control::NotificationContent::Inline {
                        text: "Washing done".into(),
                        icon: None,
                        color: crate::color::Color::WHITE
                    }
                );
            }
            c => panic!("unexpected {c:?}"),
        }
        let (s, _) = h.post("/api/notify", r#"{"scene": "alert"}"#).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert!(matches!(h.cmd().unwrap(), Command::Notify(_)));

        for (body, want) in [
            (r#"{"scene": "nope"}"#, "no scene named"),
            (r#"{}"#, "one of `text` or `scene`"),
            (r##"{"text": "a", "colour": "#fff"}"##, "unknown field"),
            (r#"{"text": "a", "repeat": 0}"#, "`repeat` must be"),
            (r#"{"text": "#, "invalid JSON"),
            ("", "invalid JSON"),
            (r#"[1]"#, "invalid JSON"),
        ] {
            let (s, j) = h.post("/api/notify", body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
            assert!(error_of(&j).contains(want), "{body}: {j}");
        }
        assert!(matches!(h.cmd(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn clear_notifications_sends_the_command() {
        let h = harness(None).await;
        let (s, _) = h.send(reqwest::Method::DELETE, "/api/notify", None).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert_eq!(h.cmd().unwrap(), Command::ClearNotifications);
    }

    #[tokio::test]
    async fn pin_and_release_scene() {
        let h = harness(None).await;
        let (s, _) = h.post("/api/scene/alert", "").await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert_eq!(h.cmd().unwrap(), Command::Pin(Some("alert".into())));
        let (s, j) = h.post("/api/scene/nope", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(error_of(&j).contains("no scene named"));
        let (s, _) = h.send(reqwest::Method::DELETE, "/api/scene", None).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert_eq!(h.cmd().unwrap(), Command::Pin(None));
        assert!(matches!(h.cmd(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn brightness_and_power() {
        let h = harness(None).await;
        assert_eq!(
            h.post("/api/brightness", r#"{"value": 128}"#).await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(h.cmd().unwrap(), Command::SetBrightness(128));
        assert_eq!(
            h.post("/api/power", r#"{"on": false}"#).await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(h.cmd().unwrap(), Command::SetPower(false));
        for (path, body) in [
            ("/api/brightness", r#"{"value": 256}"#),
            ("/api/brightness", r#"{"value": -1}"#),
            ("/api/brightness", r#"{"value": "x"}"#),
            ("/api/brightness", r#"{}"#),
            ("/api/brightness", r#"{"value": 1, "extra": 1}"#),
            ("/api/power", r#"{"on": "yes"}"#),
            ("/api/power", r#"{}"#),
        ] {
            let (s, j) = h.post(path, body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{path} {body}");
            assert!(error_of(&j).starts_with("invalid JSON body"), "{j}");
        }
        assert!(matches!(h.cmd(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn dead_render_loop_is_503() {
        let h = harness(None).await;
        let Harness {
            addr,
            link,
            store,
            client,
        } = h;
        drop(link);
        let h = Harness {
            addr,
            link: channel(vec![]).1,
            store,
            client,
        };
        for (m, path, body) in [
            (
                reqwest::Method::POST,
                "/api/notify",
                Some(r#"{"text": "x"}"#),
            ),
            (reqwest::Method::DELETE, "/api/notify", None),
            (reqwest::Method::POST, "/api/scene/home", None),
            (reqwest::Method::DELETE, "/api/scene", None),
            (
                reqwest::Method::POST,
                "/api/brightness",
                Some(r#"{"value": 1}"#),
            ),
            (reqwest::Method::POST, "/api/power", Some(r#"{"on": true}"#)),
        ] {
            let (s, j) = h.send(m, path, body).await;
            assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert!(error_of(&j).contains("render loop"));
        }
    }

    #[tokio::test]
    async fn push_data_sets_and_removes() {
        let h = harness(None).await;
        let get = |k: &str| h.store.snapshot().get(k).cloned();

        assert_eq!(
            h.post("/api/data/push:power", "1500").await.0,
            StatusCode::OK
        );
        let e = get("push:power").unwrap();
        assert_eq!(&*e.source, "http");
        assert_eq!(
            e.value,
            Value::Number {
                value: 1500.0,
                unit: None
            }
        );

        h.post("/api/data/push:power", r#"{"value": 12.5, "unit": "W"}"#)
            .await;
        assert_eq!(
            get("push:power").unwrap().value,
            Value::Number {
                value: 12.5,
                unit: Some("W".into())
            }
        );
        h.post("/api/data/push:flag", "true").await;
        assert_eq!(get("push:flag").unwrap().value, Value::Bool(true));
        h.post("/api/data/push:msg", r#""hello""#).await;
        assert_eq!(get("push:msg").unwrap().value, Value::Text("hello".into()));
        // The content type is not required, and a percent-encoded colon works.
        assert_eq!(h.post("/api/data/push%3Aenc", "3").await.0, StatusCode::OK);
        assert!(get("push:enc").is_some());

        // null removes, also as { "value": null }; removing a missing key is fine.
        assert_eq!(
            h.post("/api/data/push:flag", "null").await.0,
            StatusCode::OK
        );
        assert!(get("push:flag").is_none());
        h.post("/api/data/push:msg", r#"{"value": null}"#).await;
        assert!(get("push:msg").is_none());
        assert_eq!(
            h.post("/api/data/push:never", "null").await.0,
            StatusCode::OK
        );
        assert!(get("push:power").is_some());
    }

    #[tokio::test]
    async fn push_data_validates() {
        let h = harness(None).await;
        for (path, body, want) in [
            ("/api/data/nocolon", "1", "not a valid key"),
            ("/api/data/ha:sensor.x", "1", "must start with `push:`"),
            ("/api/data/http:x", "1", "must start with `push:`"),
            ("/api/data/pushy:x", "1", "must start with `push:`"),
            ("/api/data/push:a%20b", "1", "letters, digits"),
            ("/api/data/push:x", "[1]", "number, string, boolean or null"),
            ("/api/data/push:x", r#"{"a": 1}"#, "unknown field"),
            ("/api/data/push:x", r#"{"unit": "W"}"#, "needs a `value`"),
            (
                "/api/data/push:x",
                r#"{"value": "hi", "unit": "W"}"#,
                "only applies to numbers",
            ),
            (
                "/api/data/push:x",
                r#"{"value": 1, "unit": 5}"#,
                "`unit` must be a string",
            ),
            ("/api/data/push:x", "nope", "invalid JSON"),
            ("/api/data/push:x", "", "invalid JSON"),
        ] {
            let (s, j) = h.post(path, body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{path} {body}");
            assert!(error_of(&j).contains(want), "{path} {body}: {j}");
        }
        let long = format!("/api/data/push:{}", "k".repeat(MAX_KEY_NAME_LEN + 1));
        assert_eq!(h.post(&long, "1").await.0, StatusCode::BAD_REQUEST);
        assert!(h.store.snapshot().is_empty());
    }

    #[tokio::test]
    async fn push_data_body_cap() {
        let h = harness(None).await;
        let big = format!("\"{}\"", "x".repeat(MAX_BODY_BYTES));
        let (s, j) = h.post("/api/data/push:big", &big).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(error_of(&j).contains("larger than"));
        assert!(h.store.snapshot().is_empty());
        // The cap covers the other body routes too.
        let big = format!(r#"{{"text": "{}"}}"#, "x".repeat(MAX_BODY_BYTES));
        assert_eq!(
            h.post("/api/notify", &big).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // Just under the cap works.
        let ok = format!("\"{}\"", "x".repeat(MAX_BODY_BYTES - 16));
        assert_eq!(h.post("/api/data/push:ok", &ok).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn push_data_never_touches_other_sources() {
        let h = harness(None).await;
        // A source wrote a key under the pushed prefix (a stale or reloaded
        // config): the push must not overwrite or remove it.
        h.store.writer("mqtt").set(
            "push:theirs",
            Value::Number {
                value: 1.0,
                unit: None,
            },
        );
        let (s, j) = h.post("/api/data/push:theirs", "2").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(error_of(&j).contains("mqtt"));
        assert_eq!(
            h.post("/api/data/push:theirs", "null").await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            h.store.snapshot()["push:theirs"].value,
            Value::Number {
                value: 1.0,
                unit: None
            }
        );
    }

    #[tokio::test]
    async fn push_data_key_count_is_capped() {
        let h = harness(None).await;
        let w = h.store.writer(PUSH_SOURCE);
        for i in 0..MAX_PUSHED_KEYS {
            w.set(format!("push:k{i}"), Value::Bool(true));
        }
        assert_eq!(
            h.post("/api/data/push:one-more", "1").await.0,
            StatusCode::CONFLICT
        );
        // Updating or removing existing keys still works.
        assert_eq!(h.post("/api/data/push:k0", "1").await.0, StatusCode::OK);
        assert_eq!(h.post("/api/data/push:k1", "null").await.0, StatusCode::OK);
        assert_eq!(
            h.post("/api/data/push:one-more", "1").await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn auth_off_needs_no_header() {
        let h = harness(None).await;
        assert_eq!(
            h.send(reqwest::Method::GET, "/api/state", None).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn auth_on_guards_every_route() {
        let h = harness(Some("s3cret-token")).await;
        let routes = [
            (reqwest::Method::GET, "/api/state"),
            (reqwest::Method::POST, "/api/notify"),
            (reqwest::Method::DELETE, "/api/notify"),
            (reqwest::Method::POST, "/api/data/push:x"),
            (reqwest::Method::POST, "/api/scene/home"),
            (reqwest::Method::DELETE, "/api/scene"),
            (reqwest::Method::POST, "/api/brightness"),
            (reqwest::Method::POST, "/api/power"),
        ];
        for (m, path) in &routes {
            for auth in [
                None,
                Some("Bearer wrong"),
                Some("Bearer s3cret-toke"),
                Some("Bearer s3cret-token2"),
                Some("s3cret-token"),
                Some("Basic s3cret-token"),
                Some("Bearer"),
            ] {
                let mut r = h.client.request(m.clone(), h.url(path)).body("1");
                if let Some(a) = auth {
                    r = r.header("Authorization", a);
                }
                let resp = r.send().await.unwrap();
                assert_eq!(resp.status(), 401, "{m} {path} {auth:?}");
                assert_eq!(resp.headers()["www-authenticate"], "Bearer");
                let j: Json_ = resp.json().await.unwrap();
                assert!(error_of(&j).contains("bearer token"));
                assert!(!j.to_string().contains("s3cret"));
            }
        }
        assert!(matches!(h.cmd(), Err(TryRecvError::Empty)));
        assert!(h.store.snapshot().is_empty());

        // The right token (any scheme case) gets through.
        for auth in ["Bearer s3cret-token", "bearer s3cret-token"] {
            let resp = h
                .client
                .get(h.url("/api/state"))
                .header("Authorization", auth)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{auth}");
        }
        let resp = h
            .client
            .post(h.url("/api/power"))
            .header("Authorization", "Bearer s3cret-token")
            .body(r#"{"on": true}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        assert_eq!(h.cmd().unwrap(), Command::SetPower(true));
    }

    #[test]
    fn constant_time_eq_compares_bytes() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"a\0", b"a"));
        assert!(!constant_time_eq(b"\0", b""));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bEaReR abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("abc"), None);
    }

    #[test]
    fn token_is_never_shown() {
        let s = ApiSettings {
            token: Some(Secret::for_test("s3cret-token")),
            data_prefix: "push".into(),
        };
        let shown = format!("{s:?} {:#?}", s.clone());
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn startup_reads_the_token_or_names_the_variable() {
        let mut http = HttpConfig::default();
        assert!(ApiSettings::from_config(&http).unwrap().token.is_none());

        http.token_env = Some("PANELD_TEST_API_TOKEN_UNSET".into());
        let e = ApiSettings::from_config(&http).unwrap_err();
        assert!(
            e.contains("PANELD_TEST_API_TOKEN_UNSET") && e.contains("not set"),
            "{e}"
        );

        std::env::set_var("PANELD_TEST_API_TOKEN_EMPTY", "  ");
        http.token_env = Some("PANELD_TEST_API_TOKEN_EMPTY".into());
        let e = ApiSettings::from_config(&http).unwrap_err();
        assert!(
            e.contains("PANELD_TEST_API_TOKEN_EMPTY") && e.contains("empty"),
            "{e}"
        );

        std::env::set_var("PANELD_TEST_API_TOKEN_SET", "s3cret-token\n");
        http.token_env = Some("PANELD_TEST_API_TOKEN_SET".into());
        let s = ApiSettings::from_config(&http).unwrap();
        assert_eq!(s.token.as_ref().unwrap().expose(), "s3cret-token");
        assert!(!format!("{s:?}").contains("s3cret"));
    }
}
