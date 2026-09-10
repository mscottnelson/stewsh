//! Model Context Protocol over stdin and stdout, so an agent already running
//! in a session can read this queue and report into it without learning a CLI.
//!
//! Hand-rolled for the same reason SQLite is bundled and Alpine is vendored:
//! the protocol is newline-delimited JSON-RPC 2.0 and a handful of methods, and
//! an SDK would cost more than it saves. Every tool resolves through `agent`
//! and `actions`, so this surface cannot answer differently from the CLI or the
//! web view.
//!
//! What is deliberately absent is as much of the design as what is here. There
//! is no focus tool, because an agent must not seize the human's window; no
//! pin, snooze or resolve, because triage is the human's judgement; and no
//! rank, because ranking spends the human's model budget and belongs to the
//! key they press. An agent may describe and report. It does not decide.
use crate::{
    actions,
    agent::{self, Budget, DEFAULT_BUDGET_TOKENS},
    repo::Git,
    stream::Mode,
    Result, State,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

/// Revisions this server can speak. The client's own version is echoed back
/// when it is one of these; otherwise it is told what we do speak.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
StewardShell is the attention queue for this desk: the terminal panes, agent \
sessions and git worktrees the person you are working with has in flight.

Call stewsh_whoami first. It tells you which work stream this directory belongs \
to, whether a previous session left a handoff, and which other agents are \
already in the same stream — check that before you start work someone else is \
already doing.

Call stewsh_brief for the wider picture. Each stream reports `needs_human` and \
the `requests` codes behind it, which distinguish work that is asking for a \
person from work that is merely unfinished.

Report into the queue rather than only into your reply, so what you learned \
outlives this turn: stewsh_report `waiting` when you need a decision, `ready` \
when a turn is done, `failed` when something broke. stewsh_capture leaves a \
durable next action.

Triage stays with the human. This surface cannot pin, snooze, resolve, re-rank, \
or move their window.";

fn schema(properties: Value, required: Vec<&str>) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// `readOnlyHint` is the honest split here: three tools that only look, two
/// that write to the local store and nothing else.
fn tools() -> Value {
    json!([
        {
            "name": "stewsh_whoami",
            "description": "Which context and work stream this process is in, what handoff it \
                was left, and which other panes and agent sessions share the stream. Reports how \
                identity was resolved in `via`, and refuses to guess between siblings.",
            "inputSchema": schema(json!({
                "session_id": {"type": "string", "description":
                    "Your own session ID. Only needed when `via` comes back unresolved or \
                     ambiguous; otherwise it is read from the environment or the directory."},
            }), vec![]),
            "annotations": {"title": "Where am I", "readOnlyHint": true},
        },
        {
            "name": "stewsh_brief",
            "description": "The queue, shrunk to a token budget. Every stream carries \
                `needs_human` plus the `requests` codes that justify it, the two scores, git \
                state, and the last ranking's reasoning. `omitted` and `estimated_tokens` say \
                what was dropped to fit.",
            "inputSchema": schema(json!({
                "mode": {"type": "string", "enum": ["ranked", "active", "debt"], "description":
                    "ranked replays the last on-demand ranking (default), active leads with \
                     recent activity, debt with unfinished-ness and ignores recency."},
                "budget_tokens": {"type": "integer", "minimum": 200, "maximum": 20000,
                    "description": "Rough ceiling on the reply. Defaults to 1500."},
                "all": {"type": "boolean", "description":
                    "Include archived, snoozed and resolved work. Off by default."},
                "session_id": {"type": "string", "description": "Your own session ID, as in stewsh_whoami."},
            }), vec![]),
            "annotations": {"title": "Brief me", "readOnlyHint": true},
        },
        {
            "name": "stewsh_stream",
            "description": "One work stream in full: every member with its state and handoff, \
                the scoring reasons, and the recent event history merged across members. Read \
                this before touching work you did not start.",
            "inputSchema": schema(json!({
                "stream": {"type": "string", "description":
                    "A stream key such as `repo#branch`, a name fragment, or the `n` from a brief."},
            }), vec!["stream"]),
            "annotations": {"title": "Stream detail", "readOnlyHint": true},
        },
        {
            "name": "stewsh_capture",
            "description": "Save the one next action for this context, replacing any previous \
                one. Use it to leave behind what the next session needs to know. Capture never \
                marks work complete.",
            "inputSchema": schema(json!({
                "note": {"type": "string", "description":
                    "One concrete next action. Omit to clear the saved one."},
                "session_id": {"type": "string", "description": "Your own session ID, as in stewsh_whoami."},
            }), vec![]),
            "annotations": {
                "title": "Capture a next action", "readOnlyHint": false,
                "destructiveHint": false, "idempotentHint": true, "openWorldHint": false,
            },
        },
        {
            "name": "stewsh_report",
            "description": "Report your own state so the queue reflects it: `waiting` when you \
                need a decision from the human, `ready` when a turn is done and reviewable, \
                `failed` when something broke, `working` while you are mid-task. A reported \
                state outranks anything inferred from your transcript.",
            "inputSchema": schema(json!({
                "state": {"type": "string", "enum": State::labels(), "description":
                    "waiting, ready, failed, working, idle or unknown."},
                "summary": {"type": "string", "description":
                    "One or two sentences of handoff: what you need, or what you finished."},
                "agent": {"type": "string", "description": "Which harness you are, e.g. claude or codex."},
                "event_id": {"type": "string", "description":
                    "A stable ID for this report, so a retry after a failure is recorded once."},
                "session_id": {"type": "string", "description": "Your own session ID, as in stewsh_whoami."},
            }), vec!["state"]),
            "annotations": {
                "title": "Report my state", "readOnlyHint": false,
                "destructiveHint": false, "idempotentHint": true, "openWorldHint": false,
            },
        },
    ])
}

fn text(args: &Value, key: &str) -> Option<String> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn call(conn: &mut Connection, with_pr: bool, name: &str, args: &Value) -> Result<Value> {
    let t = chrono::Utc::now().timestamp();
    let mut git = Git::new(with_pr);
    let session = text(args, "session_id");
    let explicit = session.as_deref();
    match name {
        "stewsh_whoami" => agent::whoami(conn, &mut git, explicit, t),
        "stewsh_brief" => {
            let budget = Budget {
                mode: match args["mode"].as_str() {
                    Some(m) => crate::mode_of(m)?,
                    None => Mode::Ranked,
                },
                all: args["all"].as_bool().unwrap_or(false),
                tokens: args["budget_tokens"]
                    .as_u64()
                    .map_or(DEFAULT_BUDGET_TOKENS, |n| (n as usize).clamp(200, 20_000)),
            };
            agent::brief(conn, &mut git, explicit, &budget, t)
        }
        "stewsh_stream" => {
            let key = text(args, "stream").ok_or("stewsh_stream needs a stream")?;
            agent::detail(conn, &mut git, &key, t)
        }
        // Both writes resolve identity the strict way: an agent that cannot be
        // told apart from its siblings is told so, not filed under one of them.
        "stewsh_capture" => {
            let me = agent::identify(conn, &mut git, explicit, t)?;
            let id = me.writable()?;
            actions::capture(conn, &id, text(args, "note").as_deref(), &me.cwd, t)
        }
        "stewsh_report" => {
            let state = State::parse(args["state"].as_str().unwrap_or_default())?;
            let me = agent::identify(conn, &mut git, explicit, t)?;
            let id = me.writable()?;
            let (agent, summary, event_id) = (
                text(args, "agent"),
                text(args, "summary"),
                text(args, "event_id"),
            );
            actions::report(
                conn,
                &id,
                &actions::Report {
                    state,
                    agent: agent.as_deref(),
                    summary: summary.as_deref(),
                    event_id: event_id.as_deref(),
                },
                &me.cwd,
                t,
            )
        }
        other => Err(format!("unknown tool: {other}").into()),
    }
}

/// A tool failure comes back as content with `isError`, not as a JSON-RPC
/// error: the model needs to read what went wrong and try something else,
/// which a transport-level error denies it.
fn tool_result(conn: &mut Connection, with_pr: bool, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match call(conn, with_pr, name, &args) {
        Ok(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "structuredContent": value,
        }),
        Err(e) => json!({
            "content": [{"type": "text", "text": format!("stewsh: {}", e)}],
            "isError": true,
        }),
    }
}

fn respond(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn refuse(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One request in, at most one response out. A notification — anything with no
/// `id`, including `notifications/initialized` — is acted on and never answered.
fn handle(conn: &mut Connection, with_pr: bool, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request["method"].as_str().unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(json!({}));
    let id = id?;
    Some(match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = if PROTOCOL_VERSIONS.contains(&asked) {
                asked
            } else {
                PROTOCOL_VERSIONS[0]
            };
            respond(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stewsh", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "tools/list" => respond(id, json!({"tools": tools()})),
        "tools/call" => respond(id, tool_result(conn, with_pr, &params)),
        "ping" => respond(id, json!({})),
        other => refuse(id, -32601, format!("method not found: {other}")),
    })
}

/// Stdout is the protocol channel: nothing else in this process may write to
/// it while the loop runs, which is why `serve`'s greeting has no counterpart
/// here. A malformed line is reported and the session continues, because one
/// bad frame is not a reason to drop an agent's connection.
pub fn serve(mut conn: Connection, with_pr: bool) -> Result<()> {
    let input = io::stdin().lock();
    let mut output = io::stdout().lock();
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(&mut conn, with_pr, &request),
            Err(e) => Some(refuse(Value::Null, -32700, format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}
