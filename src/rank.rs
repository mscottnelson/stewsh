//! On-demand agent ranking. Heat and debt are computed here and handed to the
//! model as authoritative; the model only adjudicates blockedness and ordering,
//! and its answer is stored so the queue does not move until asked again.
use crate::{
    model::{clean, Stream},
    Result,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const DEFAULT_RANKER: &str = "claude -p --model haiku";

const INSTRUCTIONS: &str = r#"You are ranking a developer's active work streams. Input is one JSON
document on stdin. Reply with ONE JSON object and nothing else.

The input gives you, per stream: `heat` (decayed recent activity, authoritative
for recency), `debt` (unfinished-ness), `score` (heat+debt), git state, and the
member panes and agent sessions with their states and last messages.

Your job is the part arithmetic cannot do: decide which streams are actually
blocked on this human right now, versus running fine unattended or already done.
Weigh a waiting agent, a failing check, or an unanswered question above raw heat.
Respect `heat` for recency: do not promote a cold stream without a stated reason.
Keep the previous ordering where nothing justifies a change.

Reply shape:
{"ranking":[{"stream":"<key>","why":"<one sentence>","next":"<one concrete action>",
"confidence":0.0-1.0}],"note":"<optional one line>"}

Include every stream key from the input exactly once. Be terse and concrete.
Name the pane or agent that is blocked. Never invent facts not in the input."#;

fn age(now: i64, at: i64) -> i64 {
    (now - at).max(0)
}

pub fn evidence(streams: &[Stream], now: i64, intent: Option<&str>, previous: &[String]) -> Value {
    let items: Vec<Value> = streams
        .iter()
        .map(|s| {
            let members: Vec<Value> = s
                .members
                .iter()
                .filter(|m| m.queued(now))
                .take(8)
                .map(|m| {
                    json!({
                        "id": m.id, "kind": m.kind, "agent": m.agent,
                        "state": m.state, "state_source": m.state_source,
                        "idle_seconds": age(now, m.last_active),
                        "note": m.note, "handoff": m.handoff,
                        "last_command": m.command, "exit_code": m.exit_code,
                        "location": m.location,
                    })
                })
                .collect();
            json!({
                "stream": s.key, "name": s.name, "repo": s.repo, "branch": s.branch,
                "worktree": s.worktree, "heat": (s.heat * 100.0).round() / 100.0,
                "heat_points": s.heat_points, "debt": s.debt, "score": s.score,
                "state": s.state, "dirty": s.dirty, "ahead": s.ahead, "pr": s.pr,
                "idle_seconds": age(now, s.last_active), "members": members,
            })
        })
        .collect();
    json!({"as_of": now, "intent": intent, "previous_order": previous, "streams": items})
}

/// Hash the parts that represent a real change of situation. Time-varying
/// values are excluded so an unchanged desk costs nothing to re-rank.
pub fn fingerprint(streams: &[Stream], intent: Option<&str>) -> String {
    let reduced: Vec<Value> = streams
        .iter()
        .map(|s| {
            json!({
                "k": s.key, "d": s.dirty, "a": s.ahead,
                "p": s.pr.as_ref().map(|p| p["checks"].clone()),
                "m": s.members.iter().map(|m| json!([m.id, m.state, m.revision, m.note, m.handoff]))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    format!(
        "{:x}",
        Sha256::digest(json!({"i": intent, "s": reduced}).to_string().as_bytes())
    )
}

fn invoke(command: &str, input: &str) -> Result<String> {
    let parts = shlex::split(command).ok_or("STEWSH_RANKER is not valid shell quoting")?;
    let (program, args) = parts.split_first().ok_or("STEWSH_RANKER is empty")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start ranker `{program}`: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("missing ranker stdin")?;
    let payload = input.to_string();
    let writer = thread::spawn(move || stdin.write_all(payload.as_bytes()));
    let stdout = child.stdout.take().ok_or("missing ranker stdout")?;
    let stderr = child.stderr.take().ok_or("missing ranker stderr")?;
    let out = thread::spawn(move || {
        let mut b = Vec::new();
        stdout.take(4 * 1024 * 1024).read_to_end(&mut b).map(|_| b)
    });
    let err = thread::spawn(move || {
        let mut b = Vec::new();
        stderr.take(64 * 1024).read_to_end(&mut b).map(|_| b)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = writer.join();
    let stdout = out.join().map_err(|_| "ranker output reader failed")??;
    let stderr = err.join().map_err(|_| "ranker error reader failed")??;
    let status = status.ok_or("ranker timed out after 120s; deterministic order kept")?;
    if !status.success() {
        return Err(format!(
            "ranker exited {}: {}",
            status.code().unwrap_or(-1),
            clean(&String::from_utf8_lossy(&stderr), 400)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Models wrap JSON in prose or fences often enough that this is not optional.
fn extract(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        if v.get("ranking").is_some() {
            return Ok(v);
        }
        // Harnesses such as `claude -p --output-format json` wrap the answer.
        if let Some(inner) = v["result"].as_str() {
            return extract(inner);
        }
    }
    let start = text.find('{').ok_or("ranker returned no JSON object")?;
    let end = text.rfind('}').ok_or("ranker returned no JSON object")?;
    if end <= start {
        return Err("ranker returned no JSON object".into());
    }
    let value: Value = serde_json::from_str(&text[start..=end])
        .map_err(|e| format!("ranker returned invalid JSON: {e}"))?;
    if value.get("ranking").is_none() {
        return Err("ranker response has no `ranking` array".into());
    }
    Ok(value)
}

/// Persist an ordering. Streams the ranker omitted keep deterministic order
/// behind the ones it placed, so the ordinals are always complete.
fn apply(conn: &Connection, streams: &[Stream], ranking: &[Value], now: i64) -> Result<Vec<Value>> {
    let mut ordered: Vec<Value> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    for entry in ranking {
        let Some(key) = entry["stream"].as_str() else {
            continue;
        };
        let Some(s) = streams.iter().find(|s| s.key == key || s.id == key) else {
            continue;
        };
        if used.contains(&s.id) {
            continue;
        }
        used.push(s.id.clone());
        ordered.push(json!({
            "stream": s.key, "id": s.id, "name": s.name,
            "why": entry["why"].as_str().unwrap_or(""),
            "next": entry["next"].as_str().unwrap_or(""),
            "confidence": entry["confidence"].as_f64().unwrap_or(0.5),
        }));
    }
    for s in streams.iter().filter(|s| !used.contains(&s.id)) {
        ordered.push(json!({
            "stream": s.key, "id": s.id, "name": s.name,
            "why": "Not placed by the ranker; deterministic order kept.",
            "next": "", "confidence": 0.0,
        }));
    }
    conn.execute("UPDATE streams SET ordinal=NULL", [])?;
    for (i, entry) in ordered.iter().enumerate() {
        conn.execute(
            "UPDATE streams SET ordinal=?2,why=?3,next_action=?4,confidence=?5,ranked_at=?6 WHERE id=?1",
            params![
                entry["id"].as_str().unwrap_or(""),
                i as i64 + 1,
                entry["why"].as_str(),
                entry["next"].as_str(),
                entry["confidence"].as_f64(),
                now
            ],
        )?;
    }
    Ok(ordered)
}

/// Phase one: decide whether a model call is needed at all. Cheap, and the
/// only phase before the subprocess that touches the database.
pub fn cached(
    conn: &Connection,
    streams: &[Stream],
    intent: Option<&str>,
    force: bool,
) -> Result<Option<Value>> {
    let hash = fingerprint(streams, intent);
    let last: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT at,input_hash,payload FROM rankings ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    match last {
        Some((at, prior, payload)) if prior == hash && !force => Ok(Some(json!({
            "ranking": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
            "ranked_at": at, "cached": true,
            "message": "Nothing changed since the last ranking; no model call made."
        }))),
        _ => Ok(None),
    }
}

pub fn ranker_command() -> String {
    std::env::var("STEWSH_RANKER").unwrap_or_else(|_| DEFAULT_RANKER.to_string())
}

/// What the ranker said, and which ranker said it.
pub struct Answer {
    pub ranking: Vec<Value>,
    pub fallback: Option<String>,
    pub note: String,
    pub command: String,
}

/// Phase two: the subprocess. Deliberately takes no database handle, so a slow
/// or hung ranker cannot freeze every other request behind the lock.
pub fn ask(streams: &[Stream], now: i64, intent: Option<&str>, command: &str) -> Answer {
    let previous: Vec<String> = {
        let mut p: Vec<&Stream> = streams.iter().filter(|s| s.ordinal.is_some()).collect();
        p.sort_by_key(|s| s.ordinal);
        p.iter().map(|s| s.key.clone()).collect()
    };
    let doc = evidence(streams, now, intent, &previous);
    let prompt = format!("{INSTRUCTIONS}\n\n{doc}\n");
    let command = command.to_string();
    match invoke(&command, &prompt).and_then(|t| extract(&t)) {
        Ok(v) => Answer {
            ranking: v["ranking"].as_array().cloned().unwrap_or_default(),
            fallback: None,
            note: v["note"].as_str().unwrap_or("").to_string(),
            command,
        },
        Err(e) => Answer {
            ranking: vec![],
            fallback: Some(e.to_string()),
            note: String::new(),
            command,
        },
    }
}

/// Phase three: persist the ordering.
pub fn commit(
    conn: &mut Connection,
    streams: &[Stream],
    intent: Option<&str>,
    answer: Answer,
    now: i64,
) -> Result<Value> {
    let hash = fingerprint(streams, intent);
    let tx = conn.transaction()?;
    let ordered = apply(&tx, streams, &answer.ranking, now)?;
    tx.execute(
        "INSERT INTO rankings(at,input_hash,model,fallback,payload) VALUES(?1,?2,?3,?4,?5)",
        params![
            now,
            hash,
            answer.command,
            answer.fallback.is_some(),
            serde_json::to_string(&ordered)?
        ],
    )?;
    tx.commit()?;
    Ok(json!({
        "ranking": ordered, "ranked_at": now, "cached": false,
        "model": answer.command, "note": answer.note, "fallback": answer.fallback,
    }))
}

pub fn run(
    conn: &mut Connection,
    streams: &[Stream],
    now: i64,
    intent: Option<&str>,
    force: bool,
) -> Result<Value> {
    if let Some(hit) = cached(conn, streams, intent, force)? {
        return Ok(hit);
    }
    let answer = ask(streams, now, intent, &ranker_command());
    commit(conn, streams, intent, answer, now)
}
