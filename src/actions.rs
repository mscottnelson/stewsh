//! Actions shared by the CLI, the web view and the MCP server, so no surface
//! can drift from the others.
use crate::{
    browser, iterm,
    model::{slug, Context, Stream},
    store, stream, Result, State,
};
use chrono::{Local, TimeZone};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub fn tomorrow(t: i64) -> Result<i64> {
    let date = Local
        .timestamp_opt(t, 0)
        .single()
        .ok_or("invalid current time")?
        .date_naive()
        .succ_opt()
        .ok_or("invalid tomorrow")?;
    let datetime = date.and_hms_opt(9, 0, 0).ok_or("invalid snooze time")?;
    Local
        .from_local_datetime(&datetime)
        .earliest()
        .map(|d| d.timestamp())
        .ok_or_else(|| "9 a.m. does not exist locally; use --minutes".into())
}

fn validate(text: &str, label: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > 8192 {
        return Err(format!("{label} must be nonempty and at most 8192 bytes").into());
    }
    Ok(())
}

/// Bring one member to the front, whatever kind of thing it is.
fn raise(c: &Context) -> Result<Value> {
    if let Some(native) = &c.native_id {
        return iterm::action(native, "focus");
    }
    if c.kind == "tab" {
        let url = c
            .external_ref
            .as_deref()
            .ok_or("this tab has no recorded URL; run tabs to refresh")?;
        return browser::focus(url);
    }
    Err(match c.kind.as_str() {
        "agent" => format!(
            "{} is an agent session with no window; its transcript is {}",
            c.name,
            c.external_ref.as_deref().unwrap_or("not recorded")
        ),
        _ => format!(
            "{} has no iTerm pane to focus; run sync, or return to it yourself",
            if c.name.is_empty() { &c.id } else { &c.name }
        ),
    }
    .into())
}

/// Jump to a stream's primary pane. Focusing is a declaration of intent, so it
/// counts as activity and warms the stream it lands on.
pub fn focus_stream(
    conn: &mut Connection,
    streams: &[Stream],
    query: &str,
    t: i64,
) -> Result<Value> {
    let s = match stream::find(streams, query) {
        Ok(s) => s,
        // A bare context ID should still work from either surface.
        Err(e) => match store::find(conn, query, t) {
            Ok(c) => {
                let result = raise(&c)?;
                store::event(conn, &c.id, t, "focused", json!({"via": "context"}), None)?;
                return Ok(json!({"focused": c.id, "result": result}));
            }
            Err(_) => return Err(e),
        },
    };
    let mut candidates: Vec<_> = s.members.iter().filter(|m| m.queued(t)).collect();
    if candidates.is_empty() {
        candidates = s.members.iter().collect();
    }
    // A terminal pane is the destination when there is one; a tab is the next
    // best thing; an agent transcript is neither, so say so plainly.
    let target = candidates
        .iter()
        .find(|m| m.native_id.is_some())
        .or_else(|| candidates.iter().find(|m| m.kind == "tab"))
        .copied();
    let Some(target) = target else {
        let lives = if s.worktree.is_empty() {
            s.repo.as_str()
        } else {
            s.worktree.as_str()
        };
        return Err(format!(
            "stream {} has no pane or tab to focus; its work lives at {}",
            s.name,
            if lives.is_empty() {
                "no recorded location"
            } else {
                lives
            }
        )
        .into());
    };
    let result = raise(target)?;
    store::event(
        conn,
        &target.id,
        t,
        "focused",
        json!({"stream": s.key}),
        None,
    )?;
    Ok(json!({
        "focused": target.id, "stream": s.key, "location": target.location,
        "others": candidates.iter().filter(|m| m.id != target.id)
            .map(|m| json!({"id": m.id, "name": m.name, "kind": m.kind})).collect::<Vec<_>>(),
        "result": result,
    }))
}

pub fn context_action(
    conn: &mut Connection,
    query: &str,
    action: &str,
    value: Option<&str>,
    minutes: Option<u32>,
    t: i64,
) -> Result<Value> {
    let tx = conn.transaction()?;
    let c = store::find(&tx, query, t)?;
    let out = apply_context(&tx, &c, action, value, minutes, t)?;
    tx.commit()?;
    Ok(out)
}

/// The action itself, on a caller-owned transaction, so a stream-wide fan-out
/// is one atomic unit instead of one transaction per member.
fn apply_context(
    tx: &Connection,
    c: &crate::model::Context,
    action: &str,
    value: Option<&str>,
    minutes: Option<u32>,
    t: i64,
) -> Result<Value> {
    let (sql, data): (&str, Value) = match action {
        "pin" => ("UPDATE sessions SET pinned=1 WHERE id=?1", json!({})),
        "unpin" => ("UPDATE sessions SET pinned=0 WHERE id=?1", json!({})),
        "review" => (
            "UPDATE sessions SET reviewed_revision=revision,reviewed_at=?2 WHERE id=?1",
            json!({"revision": c.revision}),
        ),
        "resolve" => ("UPDATE sessions SET resolved=1 WHERE id=?1", json!({})),
        "reopen" => ("UPDATE sessions SET resolved=0 WHERE id=?1", json!({})),
        "close" => (
            "UPDATE sessions SET availability='closed' WHERE id=?1",
            json!({}),
        ),
        "unsnooze" => ("UPDATE sessions SET snoozed_until=0 WHERE id=?1", json!({})),
        "snooze" => {
            let until = match minutes {
                Some(m) => t + i64::from(m) * 60,
                None => tomorrow(t)?,
            };
            tx.execute(
                "UPDATE sessions SET snoozed_until=?2 WHERE id=?1",
                params![c.id, until],
            )?;
            store::event(tx, &c.id, t, "snoozed", json!({"until": until}), None)?;
            return Ok(json!({"id": c.id, "action": "snoozed", "until": until}));
        }
        "capture" => {
            let note = value.ok_or("capture needs a note")?;
            validate(note, "note")?;
            tx.execute(
                "UPDATE sessions SET note=?2,revision=revision+1,last_active_interaction=?3 WHERE id=?1",
                params![c.id, note, t],
            )?;
            store::event(tx, &c.id, t, "captured", json!({"note": note}), None)?;
            return Ok(json!({"id": c.id, "action": "captured"}));
        }
        "clear" => {
            tx.execute(
                "UPDATE sessions SET note=NULL,revision=revision+1 WHERE id=?1",
                [&c.id],
            )?;
            store::event(tx, &c.id, t, "captured", json!({"note": null}), None)?;
            return Ok(json!({"id": c.id, "action": "cleared"}));
        }
        other => return Err(format!("unknown action: {other}").into()),
    };
    if action == "review" {
        tx.execute(sql, params![c.id, t])?;
    } else {
        tx.execute(sql, [&c.id])?;
    }
    store::event(tx, &c.id, t, action, data, None)?;
    Ok(json!({"id": c.id, "action": action}))
}

pub fn stream_action(
    conn: &mut Connection,
    streams: &[Stream],
    query: &str,
    action: &str,
    value: Option<&str>,
    t: i64,
) -> Result<Value> {
    let s = stream::find(streams, query)?;
    let sql = match action {
        "pin" => "UPDATE streams SET pinned=1 WHERE id=?1",
        "unpin" => "UPDATE streams SET pinned=0 WHERE id=?1",
        "archive" => "UPDATE streams SET archived=1 WHERE id=?1",
        "unarchive" => "UPDATE streams SET archived=0 WHERE id=?1",
        "rename" => {
            let name = value.ok_or("rename needs a name")?;
            validate(name, "name")?;
            conn.execute(
                "UPDATE streams SET name=?2 WHERE id=?1",
                params![s.id, name],
            )?;
            return Ok(json!({"id": s.id, "action": "renamed", "name": name}));
        }
        "snooze" | "resolve" | "review" => {
            // One transaction for the whole fan-out: a failure part way through
            // must not leave some members changed and the caller told it failed.
            // The members are already loaded, so no per-member rescan either.
            let tx = conn.transaction()?;
            let mut touched = Vec::new();
            for m in s.members.iter().filter(|m| m.kind != "tab" && m.queued(t)) {
                apply_context(&tx, m, action, None, None, t)?;
                touched.push(m.id.clone());
            }
            tx.commit()?;
            return Ok(json!({"id": s.id, "action": action, "contexts": touched}));
        }
        other => return Err(format!("unknown stream action: {other}").into()),
    };
    conn.execute(sql, [&s.id])?;
    Ok(json!({"id": s.id, "action": action}))
}

/// Save a next action, creating the context when the ID is new. Capture never
/// marks work complete: it is the note the human or the agent wants to find
/// next time, and nothing else.
pub fn capture(
    conn: &mut Connection,
    id: &str,
    note: Option<&str>,
    cwd: &str,
    t: i64,
) -> Result<Value> {
    let tx = conn.transaction()?;
    store::ensure(&tx, id, cwd, t)?;
    tx.commit()?;
    match note {
        Some(note) => context_action(conn, id, "capture", Some(note), None, t),
        None => context_action(conn, id, "clear", None, None, t),
    }
}

/// An agent's report on itself. Agent-sourced state outranks anything inferred
/// from a transcript or a screen, and `event_id` makes a retried report
/// idempotent, because an agent that failed mid-turn will send it again.
pub struct Report<'a> {
    pub state: State,
    pub agent: Option<&'a str>,
    pub summary: Option<&'a str>,
    /// A stable ID for one report, so an agent that retries after failing
    /// mid-turn is recorded once.
    pub event_id: Option<&'a str>,
}

pub fn report(conn: &mut Connection, id: &str, r: &Report, cwd: &str, t: i64) -> Result<Value> {
    let Report {
        state,
        agent,
        summary,
        event_id,
    } = *r;
    for (value, label) in [
        (&agent, "agent"),
        (&summary, "summary"),
        (&event_id, "event ID"),
    ] {
        if let Some(v) = value {
            validate(v, label)?;
        }
    }
    let tx = conn.transaction()?;
    store::ensure(&tx, id, cwd, t)?;
    if let Some(key) = event_id {
        if tx.execute(
            "INSERT OR IGNORE INTO receipts(session_id,external_id) VALUES(?1,?2)",
            params![id, key],
        )? == 0
        {
            return Ok(json!({"id": id, "duplicate": true, "message": "Report already recorded"}));
        }
    }
    tx.execute(
        "UPDATE sessions SET state=?2,state_source='agent',agent=COALESCE(?3,agent),
         handoff=COALESCE(?4,handoff),revision=revision+1,last_active_interaction=?5 WHERE id=?1",
        params![id, state.label(), agent, summary, t],
    )?;
    store::event(
        &tx,
        id,
        t,
        "reported",
        json!({"state": state.label(), "agent": agent, "summary": summary}),
        event_id,
    )?;
    tx.commit()?;
    Ok(json!({"id": id, "state": state.label(), "message": "Agent report recorded"}))
}

/// Manual grouping. Creates the stream when the name is new, and the membership
/// is marked manual so automatic regrouping will not undo the user's choice.
pub fn group(
    conn: &mut Connection,
    streams: &[Stream],
    context: &str,
    target: &str,
    t: i64,
) -> Result<Value> {
    let c = store::find(conn, context, t)?;
    let s = match stream::find(streams, target) {
        Ok(s) => s,
        Err(_) => {
            validate(target, "stream name")?;
            let fresh = stream::blank(&slug(target), target);
            stream::ensure(conn, &fresh, t)?;
            fresh
        }
    };
    stream::assign(conn, &s.id, &c.id, "manual")?;
    store::event(conn, &c.id, t, "grouped", json!({"stream": s.id}), None)?;
    Ok(json!({"context": c.id, "stream": s.id, "name": s.name}))
}
