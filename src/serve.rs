//! Local web view. One binary, one page, no build step: the display is a
//! ranked list, which HTML has done well for thirty years.
use crate::{
    actions,
    agent::{self, Budget, DEFAULT_BUDGET_TOKENS},
    iterm,
    model::clean,
    rank,
    repo::Git,
    stream::{self, Mode},
    Result,
};
use axum::{
    extract::{Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
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

fn lock(app: &App) -> std::sync::MutexGuard<'_, Connection> {
    // A panicking handler must not take the whole queue down with it; the
    // connection itself is still usable.
    app.db.lock().unwrap_or_else(|poison| poison.into_inner())
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
        let mut guard = lock(&app);
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
    let name = q.get("mode").cloned().unwrap_or_else(|| "ranked".into());
    let all = q.get("all").is_some_and(|v| v == "1" || v == "true");
    blocking(app, move |conn, git| {
        let t = now();
        // One mode parser for both surfaces: the web used to accept a mode the
        // CLI rejects, sort by Active anyway, and echo the bogus name back.
        let streams = stream::assemble(conn, git, t, crate::mode_of(&name)?, all)?;
        Ok(json!({"streams": streams, "as_of": t, "mode": name}))
    })
    .await
}

/// The agent-facing reads, over loopback, for an agent that can reach a URL
/// but not spawn a subprocess. Same builders the CLI and the MCP server call.
async fn brief(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let name = q.get("mode").cloned().unwrap_or_else(|| "ranked".into());
    let all = q.get("all").is_some_and(|v| v == "1" || v == "true");
    let tokens = q
        .get("budget")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(DEFAULT_BUDGET_TOKENS, |n| n.clamp(200, 20_000));
    let session = q.get("session_id").cloned();
    blocking(app, move |conn, git| {
        let budget = Budget {
            mode: crate::mode_of(&name)?,
            all,
            tokens,
        };
        agent::brief(conn, git, session.as_deref(), &budget, now())
    })
    .await
}

async fn whoami(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let session = q.get("session_id").cloned();
    blocking(app, move |conn, git| {
        agent::whoami(conn, git, session.as_deref(), now())
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
    #[serde(default)]
    tabs: bool,
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
        if body.tabs {
            let streams = stream::assemble(conn, git, t, Mode::Active, true)?;
            match crate::browser::sync(conn, &streams, t) {
                Ok(v) => out["tabs"] = v,
                Err(e) => out["tabs_error"] = json!(e.to_string()),
            }
        }
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

/// Three phases, and the database lock is held for only the first and third.
/// Holding it across a ranker that may take two minutes would freeze the page.
async fn rank_route(State(app): State<App>, Json(body): Json<RankBody>) -> Response {
    let handle = tokio::task::spawn_blocking(move || {
        let t = now();
        let intent = body.intent.as_deref();
        let (streams, early) = {
            let mut guard = lock(&app);
            let mut git = Git::new(app.with_pr);
            let streams = stream::assemble(&mut guard, &mut git, t, Mode::Active, false)
                .map_err(|e| e.to_string())?;
            if streams.is_empty() {
                let done = json!({"ranking": [], "message": "Nothing in the queue to rank."});
                (streams, Some(done))
            } else {
                let hit = rank::cached(&guard, &streams, intent, body.force)
                    .map_err(|e| e.to_string())?;
                (streams, hit)
            }
        };
        if let Some(done) = early {
            return Ok(done);
        }
        let answer = rank::ask(&streams, t, intent, &rank::ranker_command());
        let mut guard = lock(&app);
        rank::commit(&mut guard, &streams, intent, answer, t).map_err(|e| e.to_string())
    })
    .await;
    match handle {
        Ok(Ok(value)) => ok(value),
        Ok(Err(message)) => fail(message),
        Err(e) => fail(format!("worker failed: {e}")),
    }
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

fn is_loopback_host(host: &str, port: u16) -> bool {
    let (name, given) = match host.rsplit_once(':') {
        // An IPv6 literal keeps its brackets; a bare one has no port.
        Some((n, p)) if !n.ends_with('[') && n.contains('[') == n.contains(']') => (n, Some(p)),
        _ => (host, None),
    };
    let named = matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1");
    let ported = match given {
        Some(p) => p.parse::<u16>() == Ok(port),
        None => false,
    };
    named && ported
}

/// A browser that has been rebound to 127.0.0.1 still sends the attacker's
/// hostname in `Host`, so checking it is what actually closes DNS rebinding.
async fn loopback_only(State(port): State<u16>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !is_loopback_host(&host, port) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"schema_version":1,"ok":false,"error":{
                "message":"StewardShell answers only to its own loopback address"}})),
        )
            .into_response();
    }
    next.run(req).await
}

pub fn router(db: Arc<Mutex<Connection>>, with_pr: bool, port: u16) -> Router {
    let app = App { db, with_pr };
    Router::new()
        .route("/", get(index))
        .route("/alpine.js", get(alpine))
        .route("/api/queue", get(queue))
        .route("/api/brief", get(brief))
        .route("/api/whoami", get(whoami))
        .route("/api/sync", post(sync))
        .route("/api/rank", post(rank_route))
        .route("/api/focus", post(focus))
        .route("/api/context", post(context_action))
        .route("/api/stream", post(stream_action))
        .route("/api/group", post(group))
        .with_state(app)
        .layer(middleware::from_fn_with_state(port, loopback_only))
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
        axum::serve(
            listener,
            router(Arc::new(Mutex::new(conn)), with_pr, bound.port()),
        )
        .await
        .map_err(|e| format!("server stopped: {e}"))?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
