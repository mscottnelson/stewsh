//! The agent-facing view, over the same store, scoring and action layer the
//! human surface uses.
//!
//! The human asks "what deserves my attention". An agent asks a first-person
//! question the human view has no way to answer: where am I, what did the last
//! session leave me, and which of these streams is actually asking for a
//! person. So this module adds three things the human surface does not need:
//! an identity ladder that says how it resolved, a needs-human verdict derived
//! from the existing reason codes, and a token budget it admits to, because
//! every field an agent reads is a field it pays for.
use crate::{
    model::{clean, Context, Stream},
    repo::{key_for, Git},
    store,
    stream::{self, Mode},
    Result,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::env;

/// Rough, and deliberately so: four bytes per token is close enough to keep a
/// brief inside a context window. The payload reports the estimate as an
/// estimate rather than pretending to have tokenized anything.
pub const BYTES_PER_TOKEN: usize = 4;
pub const DEFAULT_BUDGET_TOKENS: usize = 1500;

/// Descending detail per stream: (members, prose characters). Breadth comes
/// first and detail takes what breadth leaves, because knowing every stream
/// exists and which of them want a person beats knowing five of them deeply —
/// depth on one stream is a `detail` call away. The budget is spent, not merely
/// respected.
const LEVELS: [(usize, usize); 5] = [(8, 400), (4, 240), (2, 160), (1, 120), (0, 80)];

/// A brief listing more streams than this is not a brief. Bounding the growth
/// search also bounds its cost, which is one serialization per candidate.
const MAX_STREAMS: usize = 40;

/// Drop what carries no information. Nulls and empty strings cost tokens and
/// say nothing; `false` is kept, because absence and "no" are not the same
/// claim to an agent deciding whether to act.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, compact(v)))
                .filter(|(_, v)| match v {
                    Value::Null => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Array(a) => !a.is_empty(),
                    Value::Object(o) => !o.is_empty(),
                    _ => true,
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(compact).collect()),
        other => other,
    }
}

fn estimate(value: &Value) -> usize {
    value.to_string().len() / BYTES_PER_TOKEN
}

fn prose(text: Option<&str>, width: usize) -> Value {
    match text
        .map(|t| clean(t, width))
        .filter(|t| !t.trim().is_empty())
    {
        Some(t) => json!(t),
        None => Value::Null,
    }
}

fn member_view(m: &Context, now: i64, width: usize) -> Value {
    json!({
        "id": m.id,
        "kind": m.kind,
        "agent": if m.agent == "terminal" { Value::Null } else { json!(m.agent) },
        "state": m.state,
        "state_source": m.state_source,
        "idle_seconds": (now - m.last_active).max(0),
        "note": prose(m.note.as_deref(), width),
        "handoff": prose(m.handoff.as_deref(), width),
        "last_command": prose(m.command.as_deref(), 120),
        "exit_code": m.exit_code,
    })
}

fn git_view(s: &Stream) -> Value {
    json!({
        "dirty": s.dirty,
        "ahead": if s.ahead > 0 { json!(s.ahead) } else { Value::Null },
        "pr": s.pr.as_ref().map(|p| json!({
            "number": p["number"], "checks": p["checks"],
        })),
    })
}

fn stream_view(s: &Stream, n: usize, now: i64, members: usize, width: usize) -> Value {
    let requests = s.requests(now);
    json!({
        "n": n,
        "stream": s.key,
        "name": s.name,
        "needs_human": !requests.is_empty(),
        "requests": requests,
        "score": s.score,
        "heat": s.heat_points,
        "debt": s.debt,
        "state": s.state,
        "idle_seconds": (now - s.last_active).max(0),
        "repo": s.repo,
        "branch": s.branch,
        "worktree": s.worktree,
        "git": git_view(s),
        "why": prose(s.why.as_deref(), width),
        "next": prose(s.next.as_deref(), width),
        "members": s.members.iter().filter(|m| m.queued(now)).take(members)
            .map(|m| member_view(m, now, width)).collect::<Vec<_>>(),
    })
}

/// How an identity was arrived at, so the caller knows how much to trust it.
/// Ordered strongest first; this is also the resolution order.
#[derive(Clone, Copy, PartialEq)]
pub enum Via {
    Argument,
    ShellSession,
    ItermSession,
    Cwd,
    None,
}

impl Via {
    pub fn label(self) -> &'static str {
        match self {
            Self::Argument => "argument",
            Self::ShellSession => "stewsh_session_id",
            Self::ItermSession => "iterm_session_id",
            Self::Cwd => "cwd",
            Self::None => "unresolved",
        }
    }
}

/// The declared half of the ladder: an explicit ID, then the environment. This
/// is what `capture` and `report` have always used, and it lives here so the
/// CLI and the MCP server cannot disagree about who is speaking.
pub fn declared(explicit: Option<&str>) -> Option<(String, Via)> {
    if let Some(id) = explicit.map(str::trim).filter(|id| !id.is_empty()) {
        return Some((id.to_string(), Via::Argument));
    }
    if let Ok(id) = env::var("STEWSH_SESSION_ID") {
        if !id.trim().is_empty() {
            return Some((id, Via::ShellSession));
        }
    }
    if let Ok(id) = env::var("ITERM_SESSION_ID") {
        if let Some((_, uuid)) = id.rsplit_once(':') {
            return Some((format!("iterm:{uuid}"), Via::ItermSession));
        }
    }
    None
}

pub struct Identity {
    /// The ID the caller or the environment claims, recorded or not. A shell
    /// hook's session ID is authoritative for that shell even before the first
    /// write creates the row.
    pub declared: Option<String>,
    pub context: Option<Context>,
    pub stream: Option<Stream>,
    pub via: Via,
    /// Contexts that matched the working directory equally well. A tie is
    /// reported, never broken: an agent must not adopt a sibling's context.
    pub ambiguous: Vec<String>,
    pub cwd: String,
}

impl Identity {
    /// The context ID a write may land on. Refuses a tie and refuses to invent
    /// one from a directory, which is the same rule `report` already follows
    /// for prefixes: guessing wrong writes a handoff into someone else's work.
    pub fn writable(&self) -> Result<String> {
        if let Some(c) = &self.context {
            return Ok(c.id.clone());
        }
        if let Some(id) = &self.declared {
            return Ok(id.clone());
        }
        Err(if self.ambiguous.is_empty() {
            "no session identity: set STEWSH_SESSION_ID, run `stewsh agents` so this \
             directory has a recorded session, or pass session_id"
                .into()
        } else {
            format!(
                "{} sessions share this directory ({}); pass session_id to say which is yours",
                self.ambiguous.len(),
                self.ambiguous.join(", ")
            )
            .into()
        })
    }
}

/// Resolve who and where this process is. A declared ID wins outright. Failing
/// that, the working directory places the *stream* even when it cannot name the
/// context, which is the common case for an MCP server: the harness gives it a
/// project directory but no session identity.
pub fn identify(
    conn: &mut Connection,
    git: &mut Git,
    explicit: Option<&str>,
    t: i64,
) -> Result<Identity> {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
    let of_context = |c: &Context| {
        streams
            .iter()
            .find(|s| s.members.iter().any(|m| m.id == c.id))
            .cloned()
    };
    let of_cwd = |git: &mut Git, cwd: &str| {
        let info = git.info(cwd);
        let (key, _) = key_for(info.as_ref(), cwd);
        streams.iter().find(|s| s.key == key).cloned()
    };
    // A declared ID short-circuits the ladder even when no row exists for it
    // yet: it is a claim, and the first write will create it. What the
    // directory still supplies in that case is the stream.
    if let Some((id, via)) = declared(explicit) {
        let context = store::find_exact(conn, &id, t)?;
        let stream = match context.as_ref().and_then(of_context) {
            Some(s) => Some(s),
            None => of_cwd(git, &cwd),
        };
        return Ok(Identity {
            declared: Some(id),
            context,
            stream,
            via,
            ambiguous: vec![],
            cwd,
        });
    }
    // An agent session recorded in this directory is the best remaining guess,
    // and only when there is exactly one of them.
    let mut candidates: Vec<Context> = store::rows(conn, t)?
        .into_iter()
        .filter(|c| c.cwd == cwd && c.kind != "tab" && c.availability != "closed")
        .collect();
    if candidates.iter().any(|c| c.kind == "agent") {
        candidates.retain(|c| c.kind == "agent");
    }
    let stream = match candidates.first().and_then(of_context) {
        Some(s) => Some(s),
        None => of_cwd(git, &cwd),
    };
    let (context, ambiguous) = match candidates.len() {
        1 => (candidates.pop(), vec![]),
        0 => (None, vec![]),
        _ => (
            None,
            candidates.iter().map(|c| c.id.clone()).take(6).collect(),
        ),
    };
    let via = if context.is_some() || !ambiguous.is_empty() {
        Via::Cwd
    } else {
        Via::None
    };
    Ok(Identity {
        declared: None,
        context,
        stream,
        via,
        ambiguous,
        cwd,
    })
}

/// Where am I, and what was I handed. The first call an agent should make.
pub fn whoami(
    conn: &mut Connection,
    git: &mut Git,
    explicit: Option<&str>,
    t: i64,
) -> Result<Value> {
    let me = identify(conn, git, explicit, t)?;
    let siblings: Vec<Value> = me
        .stream
        .as_ref()
        .map(|s| {
            s.members
                .iter()
                .filter(|m| m.queued(t))
                .filter(|m| Some(&m.id) != me.context.as_ref().map(|c| &c.id))
                .take(8)
                .map(|m| member_view(m, t, 240))
                .collect()
        })
        .unwrap_or_default();
    Ok(compact(json!({
        "as_of": t,
        "via": me.via.label(),
        "cwd": me.cwd,
        "context": me.context.as_ref().map(|c| member_view(c, t, 2000)),
        // Only worth saying when it is not already the context above.
        "declared": me.context.as_ref().map_or(json!(me.declared), |_| Value::Null),
        "ambiguous": me.ambiguous,
        "stream": me.stream.as_ref().map(|s| json!({
            "stream": s.key, "name": s.name, "repo": s.repo, "branch": s.branch,
            "worktree": s.worktree, "needs_human": !s.requests(t).is_empty(),
            "requests": s.requests(t), "state": s.state, "git": git_view(s),
            "why": prose(s.why.as_deref(), 400), "next": prose(s.next.as_deref(), 400),
        })),
        "siblings": siblings,
        "hint": match me.writable() {
            Ok(_) => Value::Null,
            Err(e) => json!(e.to_string()),
        },
    })))
}

/// The first-person block of a brief. Deliberately thinner than `whoami`:
/// a sibling count rather than the roster, so the brief spends its budget on
/// the queue and an agent that wants the roster asks for it.
fn you_view(me: &Identity, t: i64, width: usize) -> Value {
    let mine = me.context.as_ref().map(|c| &c.id);
    json!({
        "via": me.via.label(),
        "context": me.context.as_ref().map(|c| json!({
            "id": c.id, "state": c.state,
            "note": prose(c.note.as_deref(), width),
            "handoff": prose(c.handoff.as_deref(), width),
        })),
        "declared": me.context.as_ref().map_or(json!(me.declared), |_| Value::Null),
        "ambiguous": me.ambiguous,
        "stream": me.stream.as_ref().map(|s| json!(s.key)),
        "siblings": me.stream.as_ref().map_or(0, |s| s.members.iter()
            .filter(|m| m.queued(t) && Some(&m.id) != mine).count()),
        "hint": match me.writable() {
            Ok(_) => Value::Null,
            Err(e) => json!(e.to_string()),
        },
    })
}

pub struct Budget {
    pub mode: Mode,
    pub all: bool,
    pub tokens: usize,
}

/// The situation, shrunk to fit. `queue` is the whole answer: each stream says
/// whether it is asking for a human and which codes say so, so an agent filters
/// rather than re-deriving the policy that produced the scores.
pub fn brief(
    conn: &mut Connection,
    git: &mut Git,
    explicit: Option<&str>,
    budget: &Budget,
    t: i64,
) -> Result<Value> {
    let me = identify(conn, git, explicit, t)?;
    let streams = stream::assemble(conn, git, t, budget.mode, budget.all)?;
    let waiting = streams
        .iter()
        .flat_map(|s| s.members.iter())
        .filter(|m| m.queued(t) && m.kind == "agent" && m.state == "waiting")
        .count();
    let asking = streams.iter().filter(|s| !s.requests(t).is_empty()).count();
    let total_members: usize = streams
        .iter()
        .map(|s| s.members.iter().filter(|m| m.queued(t)).count())
        .sum();
    let build = |(streams_cap, members, width): (usize, usize, usize)| {
        let queue: Vec<Value> = streams
            .iter()
            .take(streams_cap)
            .enumerate()
            .map(|(i, s)| stream_view(s, i + 1, t, members, width))
            .collect();
        let shown_members: usize = streams
            .iter()
            .take(streams_cap)
            .map(|s| {
                s.members
                    .iter()
                    .filter(|m| m.queued(t))
                    .count()
                    .min(members)
            })
            .sum();
        compact(json!({
            "as_of": t,
            "mode": match budget.mode {
                Mode::Active => "active", Mode::Debt => "debt", Mode::Ranked => "ranked",
            },
            "you": you_view(&me, t, width),
            "counts": {
                "streams": streams.len(), "needs_human": asking, "agents_waiting": waiting,
            },
            "queue": queue,
            "omitted": {
                "streams": streams.len().saturating_sub(streams_cap),
                "members": total_members.saturating_sub(shown_members),
            },
        }))
    };
    let reach = streams.len().min(MAX_STREAMS);
    let fits = |shape: (usize, usize, usize)| estimate(&build(shape)) <= budget.tokens;
    let (bare_members, bare_width) = *LEVELS.last().ok_or("no detail level configured")?;
    let bare = (reach, bare_members, bare_width);
    let chosen = if fits(bare) {
        // Every stream is affordable, so upgrade detail as far as it goes.
        LEVELS
            .iter()
            .map(|&(members, width)| (reach, members, width))
            .find(|&shape| fits(shape))
            .unwrap_or(bare)
    } else {
        // It is not. Trade streams away one at a time, least urgent first,
        // rather than thinning what the leading ones say about themselves.
        let mut cap = 1;
        while cap < reach && fits((cap + 1, bare_members, bare_width)) {
            cap += 1;
        }
        (cap, bare_members, bare_width)
    };
    // A desk too cramped for even one stream still gets an answer, with the
    // overage stated rather than the payload silently blown.
    let mut out = build(chosen);
    let tokens = estimate(&out);
    out["estimated_tokens"] = json!(tokens);
    out["budget_tokens"] = json!(budget.tokens);
    if tokens > budget.tokens {
        out["over_budget"] = json!(true);
    }
    Ok(out)
}

/// One stream in full, with the recent history of its members merged. This is
/// what an agent reads before touching work it did not start: who else is in
/// here, what state they are in, and what has happened lately.
pub fn detail(conn: &mut Connection, git: &mut Git, query: &str, t: i64) -> Result<Value> {
    let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
    let s = stream::find(&streams, query)?;
    let mut events: Vec<Value> = Vec::new();
    for m in &s.members {
        for mut e in store::history(conn, &m.id)? {
            e["context"] = json!(m.id);
            events.push(e);
        }
    }
    events.sort_by_key(|e| std::cmp::Reverse(e["at"].as_i64().unwrap_or(0)));
    events.truncate(12);
    let mut view = stream_view(&s, 1, t, s.members.len(), 2000);
    view["reasons"] = json!(s.reasons);
    view["events"] = json!(events);
    view["ranked_at"] = json!(s.ranked_at);
    view["pinned"] = json!(s.pinned);
    Ok(compact(view))
}
