//! Actions shared by the CLI and the web view, so both surfaces cannot drift.
use crate::{
    browser, iterm,
    model::{slug, Context, Stream},
    store, stream, Result,
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
    Err(format!(
        "{} is an agent session with no window; its transcript is {}",
        c.name,
        c.external_ref.as_deref().unwrap_or("not recorded")
    )
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
            store::event(&tx, &c.id, t, "snoozed", json!({"until": until}), None)?;
            tx.commit()?;
            return Ok(json!({"id": c.id, "action": "snoozed", "until": until}));
        }
        "capture" => {
            let note = value.ok_or("capture needs a note")?;
            validate(note, "note")?;
            tx.execute(
                "UPDATE sessions SET note=?2,revision=revision+1,last_active_interaction=?3 WHERE id=?1",
                params![c.id, note, t],
            )?;
            store::event(&tx, &c.id, t, "captured", json!({"note": note}), None)?;
            tx.commit()?;
            return Ok(json!({"id": c.id, "action": "captured"}));
        }
        "clear" => {
            tx.execute(
                "UPDATE sessions SET note=NULL,revision=revision+1 WHERE id=?1",
                [&c.id],
            )?;
            store::event(&tx, &c.id, t, "captured", json!({"note": null}), None)?;
            tx.commit()?;
            return Ok(json!({"id": c.id, "action": "cleared"}));
        }
        other => return Err(format!("unknown action: {other}").into()),
    };
    if action == "review" {
        tx.execute(sql, params![c.id, t])?;
    } else {
        tx.execute(sql, [&c.id])?;
    }
    store::event(&tx, &c.id, t, action, data, None)?;
    tx.commit()?;
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
            // Stream-level verbs fan out to the members they actually describe.
            let mut touched = Vec::new();
            for m in s.members.iter().filter(|m| m.queued(t)) {
                context_action(conn, &m.id, action, None, None, t)?;
                touched.push(m.id.clone());
            }
            return Ok(json!({"id": s.id, "action": action, "contexts": touched}));
        }
        other => return Err(format!("unknown stream action: {other}").into()),
    };
    conn.execute(sql, [&s.id])?;
    Ok(json!({"id": s.id, "action": action}))
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
            let mut fresh = stream::blank(&slug(target), target);
            fresh.key = slug(target);
            stream::ensure(conn, &fresh, t)?;
            fresh
        }
    };
    stream::assign(conn, &s.id, &c.id, "manual")?;
    store::event(conn, &c.id, t, "grouped", json!({"stream": s.id}), None)?;
    Ok(json!({"context": c.id, "stream": s.id, "name": s.name}))
}
