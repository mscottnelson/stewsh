use crate::{
    model::{decay, event_weight, Context, HEAT_WINDOW_SECS},
    Result,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{collections::HashMap, fs, path::Path, time::Duration};

pub const SCHEMA_VERSION: i32 = 2;

const V1: &str = "CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY, cwd TEXT NOT NULL, command TEXT, exit_code INTEGER,
    creation_time INTEGER NOT NULL, last_active_interaction INTEGER NOT NULL,
    note TEXT, base_priority REAL NOT NULL DEFAULT 1);
    ALTER TABLE sessions ADD COLUMN name TEXT NOT NULL DEFAULT '';
    ALTER TABLE sessions ADD COLUMN source TEXT NOT NULL DEFAULT 'shell';
    ALTER TABLE sessions ADD COLUMN availability TEXT NOT NULL DEFAULT 'unknown';
    ALTER TABLE sessions ADD COLUMN state TEXT NOT NULL DEFAULT 'unknown';
    ALTER TABLE sessions ADD COLUMN state_source TEXT NOT NULL DEFAULT 'shell';
    ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'terminal';
    ALTER TABLE sessions ADD COLUMN handoff TEXT;
    ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN snoozed_until INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN resolved INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE sessions ADD COLUMN reviewed_revision INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN reviewed_at INTEGER;
    ALTER TABLE sessions ADD COLUMN observed_at INTEGER;
    ALTER TABLE sessions ADD COLUMN native_id TEXT;
    ALTER TABLE sessions ADD COLUMN location TEXT NOT NULL DEFAULT '';
    ALTER TABLE sessions ADD COLUMN age_source TEXT NOT NULL DEFAULT 'First tracked';
    ALTER TABLE sessions ADD COLUMN fingerprint TEXT NOT NULL DEFAULT '';
    UPDATE sessions SET state='failed' WHERE exit_code IS NOT NULL AND exit_code != 0;
    CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
      at INTEGER NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL, external_id TEXT,
      UNIQUE(session_id, external_id));
    CREATE INDEX events_session ON events(session_id, seq);
    CREATE TABLE receipts (session_id TEXT NOT NULL, external_id TEXT NOT NULL, PRIMARY KEY(session_id,external_id));
    PRAGMA user_version=1;";

const V2: &str = "ALTER TABLE sessions ADD COLUMN kind TEXT NOT NULL DEFAULT 'pane';
    ALTER TABLE sessions ADD COLUMN repo TEXT NOT NULL DEFAULT '';
    ALTER TABLE sessions ADD COLUMN branch TEXT NOT NULL DEFAULT '';
    ALTER TABLE sessions ADD COLUMN worktree TEXT NOT NULL DEFAULT '';
    ALTER TABLE sessions ADD COLUMN external_ref TEXT;
    CREATE TABLE streams (
      id TEXT PRIMARY KEY, name TEXT NOT NULL, key TEXT NOT NULL DEFAULT '',
      repo TEXT NOT NULL DEFAULT '', branch TEXT NOT NULL DEFAULT '',
      worktree TEXT NOT NULL DEFAULT '', pinned INTEGER NOT NULL DEFAULT 0,
      archived INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
      dirty INTEGER NOT NULL DEFAULT 0, ahead INTEGER NOT NULL DEFAULT 0, pr TEXT,
      ordinal INTEGER, why TEXT, next_action TEXT, confidence REAL, ranked_at INTEGER);
    CREATE TABLE stream_members (
      stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
      context_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'pane',
      origin TEXT NOT NULL DEFAULT 'auto', PRIMARY KEY(stream_id, context_id));
    CREATE INDEX stream_members_context ON stream_members(context_id);
    CREATE TABLE rankings (
      id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, input_hash TEXT NOT NULL,
      model TEXT NOT NULL DEFAULT '', fallback INTEGER NOT NULL DEFAULT 0, payload TEXT NOT NULL);
    CREATE INDEX events_at ON events(at);
    PRAGMA user_version=2;";

pub fn open(path: &Path, passive: bool) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(if passive { 5 } else { 2000 }))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err("database is newer than this StewardShell; upgrade the binary".into());
    }
    if version < SCHEMA_VERSION {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Recheck under the write lock: another shell may have migrated already.
        let version: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            tx.execute_batch(V1)?;
        }
        if version < 2 {
            tx.execute_batch(V2)?;
        }
        tx.commit()?;
    }
    Ok(conn)
}

pub fn ensure(conn: &Connection, id: &str, cwd: &str, now: i64) -> Result<()> {
    if id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err("session ID must be nonempty, control-free, and at most 512 bytes".into());
    }
    conn.execute("INSERT OR IGNORE INTO sessions(id,cwd,creation_time,last_active_interaction) VALUES(?1,?2,?3,?3)",params![id,cwd,now])?;
    Ok(())
}

pub fn event(
    conn: &Connection,
    id: &str,
    now: i64,
    kind: &str,
    data: Value,
    external: Option<&str>,
) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO events(session_id,at,kind,data,external_id) VALUES(?1,?2,?3,?4,?5)",
        params![id, now, kind, data.to_string(), external],
    )?;
    if inserted > 0 {
        conn.execute("DELETE FROM events WHERE session_id=?1 AND seq NOT IN (SELECT seq FROM events WHERE session_id=?1 ORDER BY seq DESC LIMIT 200)",[id])?;
    }
    Ok(inserted > 0)
}

/// Decayed activity per context. One pass over the recent event window.
pub fn heat(conn: &Connection, now: i64) -> Result<HashMap<String, f64>> {
    let mut q =
        conn.prepare("SELECT session_id,at,kind FROM events WHERE at >= ?1 AND at <= ?2")?;
    let mut map = HashMap::new();
    let mut rows = q.query(params![now - HEAT_WINDOW_SECS, now])?;
    while let Some(r) = rows.next()? {
        let (id, at, kind): (String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        *map.entry(id).or_insert(0.0) += event_weight(&kind) * decay(now, at);
    }
    Ok(map)
}

const COLUMNS: &str = "id,cwd,command,note,exit_code,creation_time,last_active_interaction,name,\
    source,availability,state,state_source,agent,handoff,pinned,snoozed_until,resolved,revision,\
    reviewed_revision,reviewed_at,observed_at,native_id,location,age_source,kind,repo,branch,\
    worktree,external_ref";

pub fn rows(conn: &Connection, now: i64) -> Result<Vec<Context>> {
    let warmth = heat(conn, now)?;
    let mut q = conn.prepare(&format!("SELECT {COLUMNS} FROM sessions"))?;
    let mut rows = q
        .query_map([], |r| {
            Ok(Context {
                id: r.get(0)?,
                cwd: r.get(1)?,
                command: r.get(2)?,
                note: r.get(3)?,
                exit_code: r.get(4)?,
                created_at: r.get(5)?,
                last_active: r.get(6)?,
                name: r.get(7)?,
                source: r.get(8)?,
                availability: r.get(9)?,
                state: r.get(10)?,
                state_source: r.get(11)?,
                agent: r.get(12)?,
                handoff: r.get(13)?,
                pinned: r.get(14)?,
                snoozed_until: r.get(15)?,
                resolved: r.get(16)?,
                revision: r.get(17)?,
                reviewed_revision: r.get(18)?,
                reviewed_at: r.get(19)?,
                observed_at: r.get(20)?,
                native_id: r.get(21)?,
                location: r.get(22)?,
                age_source: r.get(23)?,
                kind: r.get(24)?,
                repo: r.get(25)?,
                branch: r.get(26)?,
                worktree: r.get(27)?,
                external_ref: r.get(28)?,
                heat: 0.0,
                score: 0,
                reasons: vec![],
                older: false,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for row in &mut rows {
        // Recorded events plus a baseline from how recently this context moved
        // at all. Without the baseline, heat would measure how often the user
        // ran `sync` rather than how recently the work was live.
        row.heat = warmth.get(&row.id).copied().unwrap_or(0.0) + decay(now, row.last_active);
        row.rank(now);
    }
    rows.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.last_active.cmp(&b.last_active))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(rows)
}

pub fn find(conn: &Connection, query: &str, now: i64) -> Result<Context> {
    let rows = rows(conn, now)?;
    if let Some(row) = rows.iter().find(|r| r.id == query) {
        return Ok(row.clone());
    }
    if query.trim().is_empty() {
        return Err("session ID must not be empty".into());
    }
    let matches: Vec<_> = rows
        .into_iter()
        .filter(|r| r.id.starts_with(query))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(format!("unknown session: {query}").into()),
        _ => Err(format!("ambiguous session prefix: {query}; use a longer ID").into()),
    }
}

/// Exact-match only: an agent reporting a fresh ID must never attach itself to
/// an unrelated context that happens to share a prefix.
pub fn find_exact(conn: &Connection, id: &str, now: i64) -> Result<Option<Context>> {
    Ok(rows(conn, now)?.into_iter().find(|r| r.id == id))
}

pub fn history(conn: &Connection, id: &str) -> Result<Vec<Value>> {
    let mut q = conn.prepare(
        "SELECT seq,at,kind,data FROM events WHERE session_id=?1 ORDER BY seq DESC LIMIT 30",
    )?;
    let events = q.query_map([id], |r| {
        let data: String = r.get(3)?;
        Ok(json!({"seq":r.get::<_,i64>(0)?,"at":r.get::<_,i64>(1)?,"kind":r.get::<_,String>(2)?,"data":serde_json::from_str::<Value>(&data).unwrap_or(Value::Null)}))
    })?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(events)
}
