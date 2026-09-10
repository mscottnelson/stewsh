//! Local web view. One binary, one page, no build step: the display is a
//! ranked list, which HTML has done well for thirty years.
use crate::{
    actions, iterm,
    model::clean,
    rank,
    repo::Git,
    stream::{self, Mode},
    Result,
};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct App {
    db: Arc<Mutex<Connection>>,
    with_pr: bool,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn fail(error: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"schema_version":1,"ok":false,"error":{"message":clean(&error,1000)}})),
    )
        .into_response()
}

fn ok(value: Value) -> Response {
    Json(json!({"schema_version":1,"ok":true,"data":value})).into_response()
}

/// Every handler's work is synchronous SQLite plus short subprocesses, so it
/// runs on a blocking thread rather than tying up the async runtime.
async fn blocking<F>(app: App, f: F) -> Response
where
    F: FnOnce(&mut Connection, &mut Git) -> Result<Value> + Send + 'static,
{
    // The error type is not Send, so it becomes a message before it crosses
    // the thread boundary.
    let handle = tokio::task::spawn_blocking(move || {
        let mut guard = match app.db.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        };
        let mut git = Git::new(app.with_pr);
        f(&mut guard, &mut git).map_err(|e| e.to_string())
    })
    .await;
    match handle {
        Ok(Ok(value)) => ok(value),
        Ok(Err(message)) => fail(message),
        Err(e) => fail(format!("worker failed: {e}")),
    }
}

fn parse_mode(raw: Option<&String>) -> Mode {
    match raw.map(String::as_str) {
        Some("debt") => Mode::Debt,
        Some("ranked") => Mode::Ranked,
        _ => Mode::Active,
    }
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("web/index.html"),
    )
}

async fn alpine() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_str!("web/alpine.js"),
    )
}

async fn queue(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mode = parse_mode(q.get("mode"));
    let all = q.get("all").is_some_and(|v| v == "1" || v == "true");
    blocking(app, move |conn, git| {
        let t = now();
        let streams = stream::assemble(conn, git, t, mode, all)?;
        Ok(json!({"streams": streams, "as_of": t,
                  "mode": q.get("mode").cloned().unwrap_or_else(|| "active".into())}))
    })
    .await
}

#[derive(Deserialize)]
struct SyncBody {
    #[serde(default)]
    iterm: bool,
    #[serde(default = "yes")]
    harness: bool,
    #[serde(default)]
    days: Option<i64>,
}
fn yes() -> bool {
    true
}

async fn sync(State(app): State<App>, Json(body): Json<SyncBody>) -> Response {
    blocking(app, move |conn, git| {
        let t = now();
        let mut out = json!({});
        if body.iterm {
            match iterm::sync(conn, t) {
                Ok(v) => out["iterm"] = v,
                Err(e) => out["iterm_error"] = json!(e.to_string()),
            }
        }
        if body.harness {
            match crate::harness::sync(
                conn,
                t,
                body.days.unwrap_or(crate::harness::DEFAULT_WINDOW_DAYS),
            ) {
                Ok(v) => out["harness"] = v,
                Err(e) => out["harness_error"] = json!(e.to_string()),
            }
        }
        out["regrouped"] = json!(stream::regroup(conn, git, t)?);
        Ok(out)
    })
    .await
}

#[derive(Deserialize)]
struct RankBody {
    intent: Option<String>,
    #[serde(default)]
    force: bool,
}

async fn rank_route(State(app): State<App>, Json(body): Json<RankBody>) -> Response {
    blocking(app, move |conn, git| {
        let t = now();
        let streams = stream::assemble(conn, git, t, Mode::Active, false)?;
        if streams.is_empty() {
            return Ok(json!({"ranking": [], "message": "Nothing in the queue to rank."}));
        }
        rank::run(conn, &streams, t, body.intent.as_deref(), body.force)
    })
    .await
}

#[derive(Deserialize)]
struct TargetBody {
    stream: String,
}

async fn focus(State(app): State<App>, Json(body): Json<TargetBody>) -> Response {
    blocking(app, move |conn, git| {
        let t = now();
        let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
        actions::focus_stream(conn, &streams, &body.stream, t)
    })
    .await
}

#[derive(Deserialize)]
struct ActionBody {
    id: String,
    action: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    minutes: Option<u32>,
}

async fn context_action(State(app): State<App>, Json(body): Json<ActionBody>) -> Response {
    blocking(app, move |conn, _| {
        actions::context_action(
            conn,
            &body.id,
            &body.action,
            body.value.as_deref(),
            body.minutes,
            now(),
        )
    })
    .await
}

async fn stream_action(State(app): State<App>, Json(body): Json<ActionBody>) -> Response {
    blocking(app, move |conn, git| {
        let t = now();
        let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
        actions::stream_action(
            conn,
            &streams,
            &body.id,
            &body.action,
            body.value.as_deref(),
            t,
        )
    })
    .await
}

#[derive(Deserialize)]
struct GroupBody {
    context: String,
    stream: String,
}

async fn group(State(app): State<App>, Json(body): Json<GroupBody>) -> Response {
    blocking(app, move |conn, git| {
        let t = now();
        let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
        actions::group(conn, &streams, &body.context, &body.stream, t)
    })
    .await
}

pub fn router(db: Arc<Mutex<Connection>>, with_pr: bool) -> Router {
    let app = App { db, with_pr };
    Router::new()
        .route("/", get(index))
        .route("/alpine.js", get(alpine))
        .route("/api/queue", get(queue))
        .route("/api/sync", post(sync))
        .route("/api/rank", post(rank_route))
        .route("/api/focus", post(focus))
        .route("/api/context", post(context_action))
        .route("/api/stream", post(stream_action))
        .route("/api/group", post(group))
        .with_state(app)
}

pub fn start(conn: Connection, port: u16, with_pr: bool, open_browser: bool) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // Loopback only. This is a personal desk, not a service.
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("cannot bind {addr}: {e}"))?;
        let bound = listener.local_addr()?;
        let url = format!("http://127.0.0.1:{}", bound.port());
        println!("StewardShell is at {url}   (ctrl-c to stop)");
        if open_browser {
            let _ = std::process::Command::new("open").arg(&url).status();
        }
        axum::serve(listener, router(Arc::new(Mutex::new(conn)), with_pr))
            .await
            .map_err(|e| format!("server stopped: {e}"))?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
