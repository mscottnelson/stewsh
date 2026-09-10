//! Zero-install observation of agent harnesses by reading the session
//! transcripts they already write. No hook installation, and it sees sessions
//! that started before StewardShell existed.
use crate::{store, Result};
use chrono::DateTime;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// Only the tail of a transcript is read; some run to many megabytes.
const TAIL_BYTES: u64 = 256 * 1024;
pub const DEFAULT_WINDOW_DAYS: i64 = 3;

#[derive(Debug, Default)]
pub struct Observation {
    pub id: String,
    pub agent: &'static str,
    pub cwd: String,
    pub branch: String,
    pub name: String,
    pub handoff: Option<String>,
    pub state: &'static str,
    pub last_active: i64,
    pub path: String,
}

fn tail(path: &Path) -> Option<Vec<Value>> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = String::new();
    file.take(TAIL_BYTES + 4096).read_to_string(&mut buf).ok();
    let mut lines: Vec<&str> = buf.lines().collect();
    // A mid-record first line is expected whenever we seeked into the file.
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Some(
        lines
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect(),
    )
}

fn mtime(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn stamp(value: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|d| d.timestamp())
}

fn text_of(message: &Value) -> Option<String> {
    match &message["content"] {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let joined: Vec<&str> = parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .take(4)
                .collect();
            (!joined.is_empty()).then(|| joined.join(" "))
        }
        _ => None,
    }
}

fn jsonl_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth > 0 {
            jsonl_files(&path, depth - 1, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

fn claude(home: &Path, since: i64) -> Vec<Observation> {
    let mut files = Vec::new();
    jsonl_files(&home.join(".claude/projects"), 1, &mut files);
    files
        .into_iter()
        .filter(|p| mtime(p) >= since)
        .filter_map(|path| {
            let records = tail(&path)?;
            let mut o = Observation {
                id: format!("claude:{}", path.file_stem()?.to_string_lossy()),
                agent: "claude",
                last_active: mtime(&path),
                path: path.to_string_lossy().into_owned(),
                state: "idle",
                ..Default::default()
            };
            for r in &records {
                if let Some(cwd) = r["cwd"].as_str() {
                    o.cwd = cwd.to_string();
                }
                if let Some(branch) = r["gitBranch"].as_str() {
                    o.branch = branch.to_string();
                }
                match r["type"].as_str() {
                    Some("ai-title") => {
                        if let Some(t) = r["aiTitle"].as_str() {
                            o.name = t.to_string();
                        }
                    }
                    Some("last-prompt") => {
                        if let Some(t) = r["lastPrompt"].as_str() {
                            o.handoff = Some(t.chars().take(400).collect());
                        }
                    }
                    // Sidechain records belong to subagents, not this session.
                    Some(kind @ ("user" | "assistant"))
                        if !r["isSidechain"].as_bool().unwrap_or(false) =>
                    {
                        o.state = if kind == "assistant" {
                            "ready"
                        } else {
                            "working"
                        };
                        if let Some(at) = stamp(&r["timestamp"]) {
                            o.last_active = at;
                        }
                        if kind == "assistant" {
                            if let Some(t) = text_of(&r["message"]) {
                                o.handoff = Some(t.chars().take(400).collect());
                            }
                        }
                    }
                    _ => {}
                }
            }
            (!o.cwd.is_empty()).then_some(o)
        })
        .collect()
}

fn codex(home: &Path, since: i64) -> Vec<Observation> {
    let mut files = Vec::new();
    jsonl_files(&home.join(".codex/sessions"), 4, &mut files);
    files
        .into_iter()
        .filter(|p| mtime(p) >= since)
        .filter_map(|path| {
            let records = tail(&path)?;
            let mut o = Observation {
                agent: "codex",
                last_active: mtime(&path),
                path: path.to_string_lossy().into_owned(),
                state: "working",
                ..Default::default()
            };
            // The rollout filename carries the session id when the meta record
            // has already scrolled out of the tail window.
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            o.id = format!(
                "codex:{}",
                stem.rsplit_once("-Z-").map_or(stem.as_str(), |(_, id)| id)
            );
            for r in &records {
                if r["type"] == "session_meta" {
                    if let Some(cwd) = r["payload"]["cwd"].as_str() {
                        o.cwd = cwd.to_string();
                    }
                    if let Some(id) = r["payload"]["session_id"].as_str() {
                        o.id = format!("codex:{id}");
                    }
                }
                if let Some(at) = stamp(&r["timestamp"]) {
                    o.last_active = at;
                }
                match r["payload"]["type"].as_str().or(r["type"].as_str()) {
                    Some("task_complete") => o.state = "ready",
                    Some("user_message") => o.state = "working",
                    _ => {}
                }
            }
            if o.cwd.is_empty() {
                // Meta scrolled out of the window; fall back to the newest
                // record that carries a working directory.
                o.cwd = records
                    .iter()
                    .rev()
                    .find_map(|r| r["payload"]["cwd"].as_str())
                    .unwrap_or_default()
                    .to_string();
            }
            (!o.cwd.is_empty()).then_some(o)
        })
        .collect()
}

pub fn observe(home: &Path, since: i64) -> Vec<Observation> {
    let mut all = claude(home, since);
    all.extend(codex(home, since));
    all.sort_by_key(|o| std::cmp::Reverse(o.last_active));
    all
}

/// Record harness observations. Never touches the user's note, pin, snooze or
/// resolution: an agent reports, it does not decide.
pub fn sync(conn: &mut Connection, now: i64, days: i64) -> Result<Value> {
    let home = dirs::home_dir().ok_or("cannot find home directory")?;
    let since = now - days.max(1) * 86_400;
    let found = observe(&home, since);
    let tx = conn.transaction()?;
    let mut fresh = 0;
    for o in &found {
        store::ensure(&tx, &o.id, &o.cwd, o.last_active)?;
        let name = if o.name.is_empty() {
            crate::repo::basename(&o.cwd)
        } else {
            o.name.clone()
        };
        tx.execute(
            "UPDATE sessions SET kind='agent',source='harness',agent=?2,name=?3,cwd=?4,
             external_ref=?5,availability='open',observed_at=?6,
             state=CASE WHEN state_source='agent' THEN state ELSE ?7 END,
             state_source=CASE WHEN state_source='agent' THEN state_source ELSE 'harness' END,
             handoff=COALESCE(?8,handoff),
             last_active_interaction=MAX(last_active_interaction,?9),
             creation_time=MIN(creation_time,?9)
             WHERE id=?1",
            params![
                o.id,
                o.agent,
                name,
                o.cwd,
                o.path,
                now,
                o.state,
                o.handoff,
                o.last_active
            ],
        )?;
        // Deduplicated on the transcript's own timestamp, so heat rises only
        // when the session actually advanced since the last sync.
        let advanced = store::event(
            &tx,
            &o.id,
            o.last_active,
            "harness",
            json!({"state": o.state, "agent": o.agent}),
            Some(&format!("{}@{}", o.agent, o.last_active)),
        )?;
        if advanced {
            tx.execute(
                "UPDATE sessions SET revision=revision+1 WHERE id=?1",
                [&o.id],
            )?;
            fresh += 1;
        }
    }
    // Sessions that fell out of the window are no longer live; their unresolved
    // notes stay recoverable under the all-contexts view.
    let ids: Vec<String> = found.iter().map(|o| o.id.clone()).collect();
    let mut stale = 0;
    let mut q =
        tx.prepare("SELECT id FROM sessions WHERE source='harness' AND availability!='closed'")?;
    let existing: Vec<String> = q
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(q);
    for id in existing.iter().filter(|id| !ids.contains(id)) {
        tx.execute(
            "UPDATE sessions SET availability='closed',observed_at=?2 WHERE id=?1",
            params![id, now],
        )?;
        stale += 1;
    }
    tx.commit()?;
    Ok(json!({"agent_sessions": found.len(), "advanced": fresh, "aged_out": stale}))
}
