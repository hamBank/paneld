//! Loopback HTTP server and source runner for poll source tests. Binds
//! `127.0.0.1:0`; no network, root or hardware needed.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use tokio::sync::watch;

use crate::sources::{Shutdown, Source, SourceCtx, SourceHealth};
use crate::store::Store;

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub retry_after: Option<&'static str>,
    pub location: Option<String>,
    pub body: String,
}

pub fn ok(body: &str) -> Reply {
    status(200, body)
}

pub fn status(code: u16, body: &str) -> Reply {
    Reply {
        status: code,
        retry_after: None,
        location: None,
        body: body.into(),
    }
}

/// One request the server saw.
#[derive(Clone, Debug)]
pub struct Hit {
    pub at: Instant,
    pub path: String,
    /// Decoded query parameters.
    pub query: HashMap<String, String>,
    pub headers: HeaderMap,
}

type Handler = Box<dyn Fn(&Hit, usize) -> Reply + Send + Sync>;

pub struct Server {
    handler: Handler,
    pub hits: Mutex<Vec<Hit>>,
}

async fn handle(State(s): State<Arc<Server>>, req: Request<Body>) -> Response {
    let uri = req.uri().clone();
    let query = reqwest::Url::parse(&format!("http://x{uri}"))
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default();
    let hit = Hit {
        at: Instant::now(),
        path: uri.path().to_owned(),
        query,
        headers: req.headers().clone(),
    };
    let n = {
        let mut hits = s.hits.lock().unwrap();
        hits.push(hit.clone());
        hits.len() - 1
    };
    let r = (s.handler)(&hit, n);
    let mut b = Response::builder().status(StatusCode::from_u16(r.status).unwrap());
    if let Some(ra) = r.retry_after {
        b = b.header("retry-after", ra);
    }
    if let Some(l) = r.location {
        b = b.header("location", l);
    }
    b.body(Body::from(r.body)).unwrap()
}

/// Serve every path with `f(hit, index_of_this_request)`. Returns the base
/// URL (`http://127.0.0.1:port`).
pub async fn serve_fn(
    f: impl Fn(&Hit, usize) -> Reply + Send + Sync + 'static,
) -> (String, Arc<Server>) {
    let server = Arc::new(Server {
        handler: Box::new(f),
        hits: Mutex::new(vec![]),
    });
    let app = Router::new().fallback(handle).with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (base, server)
}

/// Serve `script` in order, then `fallback` forever.
pub async fn serve(script: Vec<Reply>, fallback: Reply) -> (String, Arc<Server>) {
    let script = Mutex::new(VecDeque::from(script));
    serve_fn(move |_, _| {
        script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| fallback.clone())
    })
    .await
}

pub struct Running {
    pub store: Store,
    pub health: Arc<SourceHealth>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

pub fn run(name: &str, source: impl Source + 'static) -> Running {
    let store = Store::new();
    let health = Arc::new(SourceHealth::default());
    let (stop, rx) = watch::channel(false);
    let ctx = SourceCtx {
        name: name.into(),
        store: store.writer(name),
        shutdown: Shutdown(rx),
        health: health.clone(),
    };
    let task = tokio::spawn(Box::new(source).run(ctx));
    Running {
        store,
        health,
        stop,
        task,
    }
}

impl Running {
    pub async fn finish(self) {
        let _ = self.stop.send(true);
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .expect("source stops promptly")
            .unwrap()
            .unwrap();
    }

    pub fn errors(&self) -> u64 {
        self.health.errors.load(Ordering::Relaxed)
    }
    pub fn up(&self) -> bool {
        self.health.up.load(Ordering::Relaxed)
    }
}

pub async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}
