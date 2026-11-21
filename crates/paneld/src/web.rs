//! The web control app and the Host guard (DESIGN.md §7.4).
//!
//! The app is three embedded files served at `/`, `/app` and `/app/*`, outside
//! the API's auth layer like `/preview`. [`guard_host`] protects every route
//! against DNS rebinding when the server listens on loopback.

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

const INDEX_HTML: &str = include_str!("../assets/web/index.html");
const APP_CSS: &str = include_str!("../assets/web/app.css");
const APP_JS: &str = include_str!("../assets/web/app.js");

/// No inline code, no other origins, no framing.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, CSP),
        ],
        body,
    )
        .into_response()
}

/// `GET /`, `GET /app`, `GET /app/app.css` and `GET /app/app.js`.
pub fn app_router() -> Router {
    let index = || async { asset("text/html; charset=utf-8", INDEX_HTML) };
    Router::new()
        .route("/", get(index))
        .route("/app", get(index))
        .route(
            "/app/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/app/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
}

/// Whether a `Host` header value names the loopback interface:
/// `localhost`, `127.0.0.1` or `[::1]`, with any port. A missing header
/// (HTTP/1.0) is allowed.
pub fn loopback_host_allowed(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return true;
    };
    matches!(
        crate::api::split_authority(host),
        Some((h, _)) if matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]")
    )
}

async fn reject_foreign_host(req: Request, next: Next) -> Response {
    let host = crate::api::request_host(&req);
    // A Host header that is not valid text is as bad as a foreign one.
    let unreadable = host.is_none() && req.headers().contains_key(header::HOST);
    if unreadable || !loopback_host_allowed(host.as_deref()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "host not allowed" })),
        )
            .into_response();
    }
    next.run(req).await
}

/// Wrap the whole app. When the server is bound to a loopback address, only
/// `Host` values naming loopback pass, so a DNS-rebinding page cannot reach
/// the API with its own hostname. Other bind addresses get no check.
pub fn guard_host(app: Router, bind: std::net::SocketAddr) -> Router {
    if bind.ip().is_loopback() {
        app.layer(middleware::from_fn(reject_foreign_host))
    } else {
        app
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::IntoFuture;

    async fn serve(app: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app).into_future());
        addr
    }

    fn client() -> reqwest::Client {
        crate::sources::http_client().build().unwrap()
    }

    #[tokio::test]
    async fn app_routes_serve_the_embedded_files() {
        let addr = serve(app_router()).await;
        let c = client();
        let get = |p: &str| c.get(format!("http://{addr}{p}")).send();

        let mut bodies = Vec::new();
        for (path, ct) in [
            ("/", "text/html; charset=utf-8"),
            ("/app", "text/html; charset=utf-8"),
            ("/app/app.css", "text/css; charset=utf-8"),
            ("/app/app.js", "text/javascript; charset=utf-8"),
        ] {
            let r = get(path).await.unwrap();
            assert_eq!(r.status(), 200, "{path}");
            let h = r.headers();
            assert_eq!(h["content-type"], ct, "{path}");
            assert_eq!(h["cache-control"], "no-cache", "{path}");
            assert_eq!(h["x-content-type-options"], "nosniff", "{path}");
            assert_eq!(h["content-security-policy"], CSP, "{path}");
            bodies.push(r.text().await.unwrap());
        }
        assert_eq!(bodies[0], bodies[1]);
        assert!(!bodies[0].is_empty() && !bodies[2].is_empty() && !bodies[3].is_empty());
        assert_eq!(
            CSP,
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
             connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        );
        assert_eq!(get("/app/other.js").await.unwrap().status(), 404);
    }

    #[tokio::test]
    async fn without_the_app_routes_the_paths_are_404() {
        // `main.rs` simply does not merge the router when `[http] app = false`.
        let addr = serve(Router::new()).await;
        for p in ["/", "/app", "/app/app.css", "/app/app.js"] {
            let r = client()
                .get(format!("http://{addr}{p}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{p}");
        }
    }

    #[test]
    fn loopback_hosts() {
        for ok in [
            "localhost",
            "localhost:8080",
            "LOCALHOST:1",
            "127.0.0.1",
            "127.0.0.1:8080",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(loopback_host_allowed(Some(ok)), "{ok}");
        }
        assert!(loopback_host_allowed(None));
        for bad in [
            "",
            "evil.example",
            "evil.example:8080",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "127.0.0.2:8080",
            "::1",
            "[::2]",
            "localhost:",
            "localhost:99999",
            "localhost:80@evil.example",
            "user@localhost",
            "0.0.0.0:8080",
        ] {
            assert!(!loopback_host_allowed(Some(bad)), "{bad}");
        }
    }

    async fn status_with_host(addr: std::net::SocketAddr, path: &str, host: &str) -> u16 {
        client()
            .get(format!("http://{addr}{path}"))
            .header("Host", host)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn loopback_bind_checks_the_host_on_every_route() {
        let app = Router::new()
            .merge(app_router())
            .route("/metrics", get(|| async { "m" }));
        let addr = serve(guard_host(app, "127.0.0.1:8080".parse().unwrap())).await;
        for path in ["/", "/app/app.js", "/metrics"] {
            assert_eq!(status_with_host(addr, path, "localhost:8080").await, 200);
            assert_eq!(status_with_host(addr, path, "127.0.0.1:8080").await, 200);
            assert_eq!(status_with_host(addr, path, "[::1]:8080").await, 200);
            let r = client()
                .get(format!("http://{addr}{path}"))
                .header("Host", "evil.example")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{path}");
            let j: serde_json::Value = r.json().await.unwrap();
            assert_eq!(j, json!({ "error": "host not allowed" }));
        }
        // The server's real address (127.0.0.1:<port>) is fine without an override.
        let r = client()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn ipv6_loopback_bind_also_checks() {
        let app = Router::new().route("/", get(|| async { "x" }));
        let addr = serve(guard_host(app, "[::1]:8080".parse().unwrap())).await;
        assert_eq!(status_with_host(addr, "/", "evil.example").await, 403);
    }

    #[tokio::test]
    async fn other_binds_do_not_check_the_host() {
        for bind in ["0.0.0.0:8080", "192.168.1.5:8080", "[::]:8080"] {
            let app = Router::new().route("/", get(|| async { "x" }));
            let addr = serve(guard_host(app, bind.parse().unwrap())).await;
            assert_eq!(
                status_with_host(addr, "/", "evil.example").await,
                200,
                "{bind}"
            );
        }
    }
}
