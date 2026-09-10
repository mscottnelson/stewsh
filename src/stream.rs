//! Work streams: the unit the user prioritizes and focuses. A stream owns
//! panes, agent sessions and (later) windows. Its key is derived from repo
//! context, because a branch or worktree is the work stream most of the time.
use crate::{
    model::{slug, Context, Stream},
    repo::{key_for, Git},
    store, Result,
};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const UNASSIGNED: &str = "unassigned";

pub fn ensure(conn: &Connection, s: &Stream, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO streams(id,name,key,repo,branch,worktree,created_at,dirty,ahead,pr)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(id) DO UPDATE SET repo=excluded.repo,branch=excluded.branch,
           worktree=excluded.worktree,dirty=excluded.dirty,ahead=excluded.ahead,
           pr=COALESCE(excluded.pr,streams.pr),key=excluded.key",
        params![
            s.id,
            s.name,
            s.key,
            s.repo,
            s.branch,
            s.worktree,
            now,
            s.dirty,
            s.ahead,
            s.pr.as_ref().map(|p| p.to_string())
        ],
    )?;
    Ok(())
}

pub fn blank(id: &str, name: &str) -> Stream {
    Stream {
        id: id.into(),
        name: name.into(),
        key: id.into(),
        repo: String::new(),
        branch: String::new(),
        worktree: String::new(),
        pinned: false,
        archived: false,
        created_at: 0,
        dirty: false,
        ahead: 0,
        pr: None,
        ordinal: None,
        why: None,
        next: None,
        confidence: None,
        ranked_at: None,
        members: vec![],
        heat: 0.0,
        heat_points: 0,
        debt: 0,
        score: 0,
        state: "unknown".into(),
        last_active: 0,
        reasons: vec![],
    }
}

/// Manual membership always wins; automatic grouping never overrides a choice
/// the user made explicitly.
pub fn assign(conn: &Connection, stream_id: &str, context_id: &str, origin: &str) -> Result<()> {
    if origin == "auto" {
        let manual: i64 = conn.query_row(
            "SELECT COUNT(*) FROM stream_members WHERE context_id=?1 AND origin='manual'",
            [context_id],
            |r| r.get(0),
        )?;
        if manual > 0 {
            return Ok(());
        }
    }
    conn.execute(
        "DELETE FROM stream_members WHERE context_id=?1 AND stream_id!=?2",
        params![context_id, stream_id],
    )?;
    let role: String = conn
        .query_row("SELECT kind FROM sessions WHERE id=?1", [context_id], |r| {
            r.get(0)
        })
        .unwrap_or_else(|_| "pane".into());
    conn.execute(
        "INSERT INTO stream_members(stream_id,context_id,role,origin) VALUES(?1,?2,?3,?4)
         ON CONFLICT(stream_id,context_id) DO UPDATE SET origin=excluded.origin,role=excluded.role",
        params![stream_id, context_id, role, origin],
    )?;
    Ok(())
}

/// Derive each context's repo facts and file it under the matching stream.
pub fn regroup(conn: &mut Connection, git: &mut Git, now: i64) -> Result<usize> {
    let contexts = store::rows(conn, now)?;
    let mut seeds: Vec<String> = contexts
        .iter()
        .filter(|c| !c.cwd.is_empty())
        .map(|c| c.cwd.clone())
        .collect();
    seeds.sort();
    seeds.dedup();
    let checkouts = git.worktrees(&seeds);
    let tx = conn.transaction()?;
    let mut touched = 0;
    // Every known checkout becomes a stream, even with no pane open in it.
    for info in &checkouts {
        let (key, name) = key_for(Some(info), &info.toplevel);
        let mut s = blank(&slug(&key), &name);
        s.key = key;
        s.repo = info.repo.clone();
        s.branch = info.branch.clone();
        s.worktree = info.worktree.clone();
        s.dirty = info.dirty;
        s.ahead = info.ahead;
        s.pr = git.pr(&info.toplevel);
        ensure(&tx, &s, now)?;
    }
    for c in &contexts {
        if c.kind == "tab" {
            // A tab is placed by URL, not by a working directory it lacks.
            continue;
        }
        let info = git.info(&c.cwd);
        let (key, name) = key_for(info.as_ref(), &c.cwd);
        let id = slug(&key);
        let mut s = blank(&id, &name);
        s.key = key;
        if let Some(i) = &info {
            s.repo = i.repo.clone();
            s.branch = i.branch.clone();
            s.worktree = i.worktree.clone();
            s.dirty = i.dirty;
            s.ahead = i.ahead;
            s.pr = git.pr(&i.toplevel);
        }
        ensure(&tx, &s, now)?;
        tx.execute(
            "UPDATE sessions SET repo=?2,branch=?3,worktree=?4 WHERE id=?1",
            params![
                c.id,
                info.as_ref().map(|i| i.repo.clone()).unwrap_or_default(),
                info.as_ref().map(|i| i.branch.clone()).unwrap_or_default(),
                info.as_ref()
                    .map(|i| i.worktree.clone())
                    .unwrap_or_default()
            ],
        )?;
        assign(&tx, &id, &c.id, "auto")?;
        touched += 1;
    }
    tx.commit()?;
    Ok(touched)
}

fn load(conn: &Connection, now: i64) -> Result<Vec<Stream>> {
    let contexts: HashMap<String, Context> = store::rows(conn, now)?
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    let mut placed: HashSet<String> = HashSet::new();
    let mut q = conn.prepare("SELECT stream_id,context_id FROM stream_members")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let (stream, context): (String, String) = (r.get(0)?, r.get(1)?);
        if contexts.contains_key(&context) {
            placed.insert(context.clone());
            members.entry(stream).or_default().push(context);
        }
    }
    let mut q = conn.prepare(
        "SELECT id,name,key,repo,branch,worktree,pinned,archived,created_at,dirty,ahead,pr,
         ordinal,why,next_action,confidence,ranked_at FROM streams",
    )?;
    let mut streams = q
        .query_map([], |r| {
            let pr: Option<String> = r.get(11)?;
            Ok(Stream {
                id: r.get(0)?,
                name: r.get(1)?,
                key: r.get(2)?,
                repo: r.get(3)?,
                branch: r.get(4)?,
                worktree: r.get(5)?,
                pinned: r.get(6)?,
                archived: r.get(7)?,
                created_at: r.get(8)?,
                dirty: r.get(9)?,
                ahead: r.get(10)?,
                pr: pr.and_then(|p| serde_json::from_str::<Value>(&p).ok()),
                ordinal: r.get(12)?,
                why: r.get(13)?,
                next: r.get(14)?,
                confidence: r.get(15)?,
                ranked_at: r.get(16)?,
                members: vec![],
                heat: 0.0,
                heat_points: 0,
                debt: 0,
                score: 0,
                state: "unknown".into(),
                last_active: 0,
                reasons: vec![],
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for s in &mut streams {
        s.members = members
            .remove(&s.id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| contexts.get(&id).cloned())
            .collect();
    }
    let orphans: Vec<Context> = contexts
        .into_values()
        .filter(|c| !placed.contains(&c.id))
        .collect();
    if !orphans.is_empty() {
        let mut s = blank(UNASSIGNED, "Unassigned");
        s.members = orphans;
        streams.push(s);
    }
    for s in &mut streams {
        s.roll_up(now);
    }
    Ok(streams)
}

/// Sort mode for the queue. Active is heat-led; debt is the morning view.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Active,
    Debt,
    Ranked,
}

pub fn assemble(
    conn: &mut Connection,
    git: &mut Git,
    now: i64,
    mode: Mode,
    all: bool,
) -> Result<Vec<Stream>> {
    let mut streams = load(conn, now)?;
    // Self-heal: a context nobody has filed yet triggers one bounded regroup.
    if streams.iter().any(|s| s.id == UNASSIGNED) {
        regroup(conn, git, now)?;
        streams = load(conn, now)?;
    }
    streams.retain(|s| all || (s.queued(now) && !(s.members.is_empty() && s.score == 0)));
    match mode {
        Mode::Active => streams.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.last_active.cmp(&a.last_active))
                .then_with(|| a.id.cmp(&b.id))
        }),
        Mode::Debt => streams.sort_by(|a, b| {
            b.debt
                .cmp(&a.debt)
                .then_with(|| a.last_active.cmp(&b.last_active))
                .then_with(|| a.id.cmp(&b.id))
        }),
        Mode::Ranked => streams.sort_by(|a, b| {
            a.ordinal
                .unwrap_or(i64::MAX)
                .cmp(&b.ordinal.unwrap_or(i64::MAX))
                .then_with(|| b.score.cmp(&a.score))
                .then_with(|| a.id.cmp(&b.id))
        }),
    }
    Ok(streams)
}

/// Accept a stream id, a unique id prefix, a name fragment, or the ordinal
/// printed by the last ranking, so `focus 2` works straight from the queue.
pub fn find(streams: &[Stream], query: &str) -> Result<Stream> {
    let q = query.trim().trim_start_matches('#');
    if q.is_empty() {
        return Err("stream must not be empty".into());
    }
    if let Ok(n) = q.parse::<i64>() {
        if let Some(s) = streams.iter().find(|s| s.ordinal == Some(n)) {
            return Ok(s.clone());
        }
        if let Some(s) = streams.get((n - 1).max(0) as usize).filter(|_| n >= 1) {
            return Ok(s.clone());
        }
    }
    if let Some(s) = streams.iter().find(|s| s.id == q) {
        return Ok(s.clone());
    }
    let lower = q.to_lowercase();
    let matches: Vec<_> = streams
        .iter()
        .filter(|s| {
            s.id.starts_with(&lower)
                || s.key.to_lowercase().contains(&lower)
                || s.name.to_lowercase().contains(&lower)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(format!("unknown stream: {query}").into()),
        _ => Err(format!("ambiguous stream: {query}; use a longer name or the ordinal").into()),
    }
}
