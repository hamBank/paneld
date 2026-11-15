//! HTTP control API (DESIGN.md §7.1). M4 groundwork: `GET /api/state`; the
//! remaining routes (`notify`, `data`, `scene`, `brightness`, bearer token)
//! are added on top of the same [`Control`] handle.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::control::{Control, Status};

/// The control API routes.
pub fn router(control: Control) -> Router {
    Router::new()
        .route("/api/state", get(state))
        .with_state(control)
}

/// `GET /api/state`: the render loop's latest [`Status`].
async fn state(State(control): State<Control>) -> Json<Status> {
    Json((*control.status()).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::IntoFuture;

    #[tokio::test]
    async fn state_returns_the_status_as_json() {
        let (control, _link, _out) = crate::control::channel(vec!["home".into()]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router(control)).into_future());
        let body: serde_json::Value = crate::sources::http_client()
            .build()
            .unwrap()
            .get(format!("http://{addr}/api/state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["scenes"], serde_json::json!(["home"]));
        assert_eq!(body["on"], true);
        assert_eq!(body["brightness"], 255);
        assert_eq!(body["pinned"], serde_json::Value::Null);
    }
}
