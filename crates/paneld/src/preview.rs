//! Preview sink: serves the latest frame over HTTP (DESIGN.md §7.1).
//!
//! Routes: `GET /preview.png` (current frame, scaled) and `GET /preview`
//! (a page that keeps reloading the PNG). Frames are shown without any
//! post-processing, i.e. the "true" colours the scene asked for.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::config::PreviewOutput;
use crate::engine::FrameReceiver;

#[derive(Clone)]
struct PreviewState {
    frames: FrameReceiver,
    scale: u32,
    grid: bool,
    refresh_ms: u32,
    /// HTML credits shown under the image (data providers' terms).
    footer: String,
}

/// Routes for the preview sink. `fps` sets the page's refresh rate (capped
/// at 10 Hz, polling PNGs faster than that is pointless).
pub fn router(
    frames: FrameReceiver,
    cfg: &PreviewOutput,
    fps: u32,
    attributions: &[&str],
) -> Router {
    let state = PreviewState {
        frames,
        scale: cfg.scale,
        grid: cfg.grid,
        refresh_ms: 1000 / fps.clamp(1, 10),
        footer: attributions.join(" · "),
    };
    Router::new()
        .route("/preview.png", get(png))
        .route("/preview", get(page))
        .with_state(state)
}

async fn png(State(s): State<PreviewState>) -> Response {
    let frame = s.frames.borrow().clone();
    match tokio::task::spawn_blocking(move || crate::png_io::encode(&frame, s.scale, s.grid)).await
    {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "encoding preview failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn page(State(s): State<PreviewState>) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>paneld preview</title>
<style>body{{background:#111;margin:0;display:grid;place-items:center;min-height:100vh}}
img{{image-rendering:pixelated;max-width:100vw}}
footer{{color:#888;font:12px sans-serif}}footer a{{color:#aaa}}</style></head>
<body><img id="f" src="preview.png" alt="current frame">{footer}
<script>
const img = document.getElementById("f");
let busy = false;
setInterval(() => {{
  if (busy) return;
  busy = true;
  const next = new Image();
  next.onload = () => {{ img.src = next.src; busy = false; }};
  next.onerror = () => {{ busy = false; }};
  next.src = "preview.png?t=" + Date.now();
}}, {refresh});
</script></body></html>"#,
        refresh = s.refresh_ms,
        footer = if s.footer.is_empty() {
            String::new()
        } else {
            format!("<footer>{}</footer>", s.footer)
        }
    ))
}
