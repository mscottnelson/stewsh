mod actions;
mod harness;
mod iterm;
mod model;
mod rank;
mod repo;
mod serve;
mod store;
mod stream;

use chrono::{Local, TimeZone};
use clap::{Parser, Subcommand, ValueEnum};
use model::clean;
use repo::Git;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    env,
    error::Error,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};
use stream::Mode;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(
    name = "stewsh",
    version,
    about = "StewardShell — oversee your sessions and steward unfinished work"
)]
struct Cli {
    /// Override the local database (also STEWSH_DB).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Emit a versioned JSON response, with no interactive prompts.
    #[arg(long, global = true)]
    json: bool,
    /// Short database lock timeout for passive shell hooks.
    #[arg(long, global = true, hide = true)]
    passive: bool,
    /// Include pull request status via `gh`. The only outbound network call.
    #[arg(long, global = true)]
    pr: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum State {
    Unknown,
    Idle,
    Working,
    Waiting,
    Ready,
    Failed,
}
impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Enter the supervisory shell. Commands here never execute OS commands.
    Shell {
        #[arg(long)]
        no_sync: bool,
    },
    /// Discover existing iTerm2 panes (macOS); preserves user decisions.
    Sync,
    /// Record local shell activity; silent on success.
    Track {
        session_id: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        exit_code: Option<i32>,
        #[arg(long, value_enum)]
        state: Option<State>,
    },
    /// Show the attention queue, including reasons.
    Triage {
        #[arg(long,default_value_t=10,value_parser=clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        /// Include closed, snoozed and resolved contexts.
        #[arg(long)]
        all: bool,
        /// Only contexts dating from before today's local calendar date.
        #[arg(long)]
        older: bool,
        /// Match title, CWD, ID, agent, note or handoff.
        #[arg(long)]
        search: Option<String>,
    },
    /// Print the next context without consuming or reviewing it.
    Next,
    /// Inspect one context and its recent metadata history.
    Show {
        id: String,
    },
    /// Save your next action; capture never marks the work complete.
    Capture {
        note: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long, conflicts_with = "note")]
        clear: bool,
    },
    /// Report agent status/handoff; does not replace the user's next action.
    Report {
        #[arg(value_enum)]
        state: State,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        /// Deduplicate retried reports for this session.
        #[arg(long)]
        event_id: Option<String>,
    },
    /// Acknowledge the current revision; subsequent activity resurfaces it.
    Review {
        id: String,
    },
    /// Pin a context above routine observations; --off removes the pin.
    Pin {
        id: String,
        #[arg(long)]
        off: bool,
    },
    /// Defer until tomorrow at 9 a.m., or for --minutes N.
    Snooze {
        id: String,
        #[arg(long,value_parser=clap::value_parser!(u32).range(1..=525600))]
        minutes: Option<u32>,
    },
    Unsnooze {
        id: String,
    },
    /// Explicitly remove from the attention queue without closing the terminal.
    Resolve {
        id: String,
    },
    Reopen {
        id: String,
    },
    /// Mark a tracked shell closed; its unfinished context is retained.
    Close {
        id: String,
    },
    /// Select an existing iTerm2 pane without sending input.
    Focus {
        id: String,
    },
    /// Read the live visible screen (never persisted).
    Preview {
        id: String,
    },
    /// Serve the local web view (the default when run from a terminal).
    Serve {
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Do not open a browser window.
        #[arg(long)]
        no_open: bool,
    },
    /// Ranked work streams: the active queue.
    Queue {
        /// active is heat led, debt is the morning view, ranked replays the
        /// last agent ranking.
        #[arg(long, default_value = "ranked")]
        mode: String,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: u32,
    },
    /// Ask the configured agent to rank the queue. On demand, never automatic.
    Rank {
        /// What you are trying to get done; guides the ranker.
        #[arg(long)]
        intent: Option<String>,
        /// Re-rank even when nothing has changed since the last ranking.
        #[arg(long)]
        force: bool,
        /// Print the evidence document instead of calling the ranker.
        #[arg(long)]
        dry_run: bool,
    },
    /// Read Claude Code and Codex transcripts; no hook installation needed.
    Agents {
        #[arg(long, default_value_t = harness::DEFAULT_WINDOW_DAYS, value_parser = clap::value_parser!(i64).range(1..=365))]
        days: i64,
    },
    /// File a context under a work stream, creating the stream if it is new.
    Group {
        context: String,
        #[arg(long)]
        to: String,
    },
    /// Inspect or change work streams.
    Stream {
        /// list, pin, unpin, archive, unarchive, rename, review, snooze, resolve
        action: String,
        id: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Check local storage and integration setup.
    Doctor,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn cwd() -> String {
    env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn session(explicit: Option<String>) -> Result<String> {
    if let Some(id) = explicit {
        if !id.trim().is_empty() {
            return Ok(id);
        }
        return Err("session ID must not be empty".into());
    }
    if let Ok(id) = env::var("STEWSH_SESSION_ID") {
        if !id.trim().is_empty() {
            return Ok(id);
        }
    }
    if let Ok(id) = env::var("ITERM_SESSION_ID") {
        if let Some((_, uuid)) = id.rsplit_once(':') {
            return Ok(format!("iterm:{uuid}"));
        }
    }
    Err("no session identity; source shell/stewsh.zsh or pass --session-id ID".into())
}

/// Exact match only. An agent reporting a fresh ID must never silently attach
/// itself to an unrelated context that happens to share a prefix.
fn existing_or_new(conn: &Connection, id: String, t: i64) -> Result<String> {
    Ok(store::find_exact(conn, &id, t)?.map_or(id, |c| c.id))
}

fn validate_text(text: &str, label: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > 8192 {
        return Err(format!("{label} must be nonempty and at most 8192 bytes").into());
    }
    Ok(())
}

fn mode_of(raw: &str) -> Result<Mode> {
    match raw {
        "active" => Ok(Mode::Active),
        "debt" => Ok(Mode::Debt),
        "ranked" => Ok(Mode::Ranked),
        other => Err(format!("unknown mode: {other}; use active, debt or ranked").into()),
    }
}

fn execute(conn: &mut Connection, git: &mut Git, command: Commands, t: i64) -> Result<Value> {
    match command {
        Commands::Shell { .. } => Err("already in StewardShell; use queue or help".into()),
        Commands::Serve { .. } => Err("serve cannot run inside StewardShell".into()),
        Commands::Sync => {
            let mut out = iterm::sync(conn, t)?;
            out["regrouped"] = json!(stream::regroup(conn, git, t)?);
            Ok(out)
        }
        Commands::Agents { days } => {
            let mut out = harness::sync(conn, t, days)?;
            out["regrouped"] = json!(stream::regroup(conn, git, t)?);
            Ok(out)
        }
        Commands::Queue { mode, all, limit } => {
            let mut streams = stream::assemble(conn, git, t, mode_of(&mode)?, all)?;
            streams.truncate(limit as usize);
            Ok(json!({"streams": streams, "as_of": t, "mode": mode}))
        }
        Commands::Rank {
            intent,
            force,
            dry_run,
        } => {
            let streams = stream::assemble(conn, git, t, Mode::Active, false)?;
            if dry_run {
                return Ok(rank::evidence(&streams, t, intent.as_deref(), &[]));
            }
            if streams.is_empty() {
                return Ok(json!({"ranking": [], "message": "Nothing in the queue to rank."}));
            }
            rank::run(conn, &streams, t, intent.as_deref(), force)
        }
        Commands::Group { context, to } => {
            let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
            actions::group(conn, &streams, &context, &to, t)
        }
        Commands::Stream { action, id, name } => {
            let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
            if action == "list" {
                return Ok(json!({"streams": streams, "as_of": t, "mode": "ranked"}));
            }
            let id = id.ok_or("stream action needs a stream")?;
            actions::stream_action(conn, &streams, &id, &action, name.as_deref(), t)
        }
        Commands::Track {
            session_id,
            command,
            exit_code,
            state,
        } => {
            if let Some(c) = &command {
                validate_text(c, "command")?;
            }
            let tx = conn.transaction()?;
            store::ensure(&tx, &session_id, &cwd(), t)?;
            let state = state
                .map(State::label)
                .or_else(|| exit_code.map(|e| if e == 0 { "idle" } else { "failed" }))
                .or(if command.is_some() {
                    Some("working")
                } else {
                    None
                });
            let changed = state.is_some() || command.is_some();
            tx.execute("UPDATE sessions SET cwd=?2,command=COALESCE(?3,command),
              exit_code=CASE WHEN ?3 IS NOT NULL OR ?4 IS NOT NULL OR ?5='working' THEN ?4 ELSE exit_code END,
              state=COALESCE(?5,state),state_source=CASE WHEN ?5 IS NOT NULL THEN 'shell' ELSE state_source END,
              availability='open',last_active_interaction=CASE WHEN ?6 THEN ?7 ELSE last_active_interaction END,
              revision=revision+?6,base_priority=CASE WHEN ?4 IS NULL THEN base_priority WHEN ?4=0 THEN 1 ELSE 5 END WHERE id=?1",
              params![session_id,cwd(),command,exit_code,state,changed,t])?;
            if changed {
                store::event(
                    &tx,
                    &session_id,
                    t,
                    "tracked",
                    json!({"state":state,"exit_code":exit_code}),
                    None,
                )?;
            }
            tx.commit()?;
            Ok(json!({"id":session_id,"tracked":true}))
        }
        Commands::Triage {
            limit,
            all,
            older,
            search,
        } => {
            let search = search.unwrap_or_default().to_lowercase();
            let sessions: Vec<_> = store::rows(conn, t)?
                .into_iter()
                .filter(|s| all || s.queued(t))
                .filter(|s| !older || s.older)
                .filter(|s| {
                    format!(
                        "{} {} {} {} {} {}",
                        s.id,
                        s.name,
                        s.cwd,
                        s.agent,
                        s.note.as_deref().unwrap_or(""),
                        s.handoff.as_deref().unwrap_or("")
                    )
                    .to_lowercase()
                    .contains(&search)
                })
                .take(limit as usize)
                .collect();
            Ok(json!({"sessions":sessions,"as_of":t}))
        }
        Commands::Next => Ok(
            json!({"sessions":store::rows(conn,t)?.into_iter().filter(|s|s.queued(t)).take(1).collect::<Vec<_>>(),"as_of":t}),
        ),
        Commands::Show { id } => {
            let s = store::find(conn, &id, t)?;
            Ok(json!({"events":store::history(conn,&s.id)?,"session":s}))
        }
        Commands::Capture {
            note,
            session_id,
            clear,
        } => {
            let id = existing_or_new(conn, session(session_id)?, t)?;
            let tx = conn.transaction()?;
            store::ensure(&tx, &id, &cwd(), t)?;
            tx.commit()?;
            if clear {
                actions::context_action(conn, &id, "clear", None, None, t)
            } else {
                let note = note.as_deref().ok_or("provide a note or --clear")?;
                actions::context_action(conn, &id, "capture", Some(note), None, t)
            }
        }
        Commands::Report {
            state,
            session_id,
            agent,
            summary,
            event_id,
        } => {
            for (value, label) in [
                (&agent, "agent"),
                (&summary, "summary"),
                (&event_id, "event ID"),
            ] {
                if let Some(v) = value {
                    validate_text(v, label)?;
                }
            }
            let id = existing_or_new(conn, session(session_id)?, t)?;
            let tx = conn.transaction()?;
            store::ensure(&tx, &id, &cwd(), t)?;
            if let Some(key) = &event_id {
                if tx.execute(
                    "INSERT OR IGNORE INTO receipts(session_id,external_id) VALUES(?1,?2)",
                    params![id, key],
                )? == 0
                {
                    return Ok(
                        json!({"id":id,"duplicate":true,"message":"Report already recorded"}),
                    );
                }
            }
            tx.execute("UPDATE sessions SET state=?2,state_source='agent',agent=COALESCE(?3,agent),handoff=COALESCE(?4,handoff),revision=revision+1,last_active_interaction=?5 WHERE id=?1",params![id,state.label(),agent,summary,t])?;
            store::event(
                &tx,
                &id,
                t,
                "reported",
                json!({"state":state.label(),"agent":agent,"summary":summary}),
                event_id.as_deref(),
            )?;
            tx.commit()?;
            Ok(json!({"id":id,"message":"Agent report recorded"}))
        }
        Commands::Focus { id } | Commands::Preview { id } => {
            Err(format!("unexpected action dispatch for {id}").into())
        }
        Commands::Doctor => {
            let integrity: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
            let ranker =
                env::var("STEWSH_RANKER").unwrap_or_else(|_| rank::DEFAULT_RANKER.to_string());
            let program = shlex::split(&ranker)
                .and_then(|p| p.first().cloned())
                .unwrap_or_default();
            let ranker_found = std::process::Command::new("which")
                .arg(&program)
                .output()
                .is_ok_and(|o| o.status.success());
            Ok(json!({
                "storage": integrity,
                "schema_version": store::SCHEMA_VERSION,
                "contexts": store::rows(conn, t)?.len(),
                "streams": stream::assemble(conn, git, t, Mode::Ranked, true)?.len(),
                "iterm_supported": cfg!(target_os = "macos"),
                "shell_session": env::var("STEWSH_SESSION_ID").ok(),
                "ranker": ranker,
                "ranker_available": ranker_found,
                "message": "sync reads iTerm, agents reads Claude/Codex transcripts, rank calls the ranker",
            }))
        }
        change => {
            let (id, action) = match &change {
                Commands::Review { id } => (id.clone(), "review"),
                Commands::Pin { id, off } => (id.clone(), if *off { "unpin" } else { "pin" }),
                Commands::Snooze { id, .. } => (id.clone(), "snooze"),
                Commands::Unsnooze { id } => (id.clone(), "unsnooze"),
                Commands::Resolve { id } => (id.clone(), "resolve"),
                Commands::Reopen { id } => (id.clone(), "reopen"),
                Commands::Close { id } => (id.clone(), "close"),
                _ => unreachable!(),
            };
            let minutes = match &change {
                Commands::Snooze { minutes, .. } => *minutes,
                _ => None,
            };
            actions::context_action(conn, &id, action, None, minutes, t)
        }
    }
}

fn dispatch(conn: &mut Connection, git: &mut Git, command: Commands, t: i64) -> Result<Value> {
    match &command {
        Commands::Preview { id } => {
            let s = store::find(conn, id, t)?;
            let native = s
                .native_id
                .ok_or("no iTerm pane mapping; run sync or return to the shell manually")?;
            iterm::action(&native, "preview")
        }
        Commands::Focus { id } => {
            let id = id.clone();
            let streams = stream::assemble(conn, git, t, Mode::Ranked, true)?;
            actions::focus_stream(conn, &streams, &id, t)
        }
        _ => execute(conn, git, command, t),
    }
}

fn reasons_line(value: &Value) -> String {
    value["reasons"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .map(|r| {
                    format!(
                        "{:+}: {}",
                        r["points"].as_i64().unwrap_or(0),
                        r["text"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default()
}

fn show_streams(out: &mut impl Write, streams: &[Value], mode: &str) -> Result<()> {
    if streams.is_empty() {
        writeln!(
            out,
            "No work streams in this view. Try `stewsh sync`, `stewsh agents`, or --all."
        )?;
        return Ok(());
    }
    writeln!(out, "Work streams ({mode})\n")?;
    for (i, s) in streams.iter().enumerate() {
        let ordinal = s["ordinal"].as_i64().unwrap_or(i as i64 + 1);
        writeln!(
            out,
            "{ordinal:>3}. {:<52} {:>4}  heat {:<4} debt {}",
            clean(s["name"].as_str().unwrap_or(""), 52),
            s["score"],
            s["heat_points"],
            s["debt"]
        )?;
        let mut facts = vec![
            clean(s["key"].as_str().unwrap_or(""), 60),
            clean(s["state"].as_str().unwrap_or("unknown"), 20),
        ];
        if s["dirty"] == true {
            facts.push("uncommitted".into());
        }
        if let Some(a) = s["ahead"].as_i64().filter(|a| *a > 0) {
            facts.push(format!("{a} unpushed"));
        }
        if let Some(pr) = s["pr"].as_object() {
            facts.push(format!(
                "PR #{} {}",
                pr["number"],
                pr["checks"].as_str().unwrap_or("")
            ));
        }
        writeln!(out, "     {}", facts.join(" · "))?;
        if let Some(why) = s["why"].as_str().filter(|w| !w.is_empty()) {
            writeln!(out, "     {}", clean(why, 160))?;
        }
        if let Some(next) = s["next"].as_str().filter(|n| !n.is_empty()) {
            writeln!(out, "     → {}", clean(next, 160))?;
        }
        for m in s["members"].as_array().into_iter().flatten().take(4) {
            writeln!(
                out,
                "       · {:<40} {:<8} {}",
                clean(
                    m["name"].as_str().unwrap_or(m["id"].as_str().unwrap_or("")),
                    40
                ),
                clean(m["state"].as_str().unwrap_or(""), 8),
                clean(m["id"].as_str().unwrap_or(""), 46)
            )?;
        }
        writeln!(out)?;
    }
    writeln!(
        out,
        "focus <n> jumps to a stream's pane · rank re-orders on demand"
    )?;
    Ok(())
}

fn display(value: &Value, machine: bool) -> Result<()> {
    let mut out = io::stdout().lock();
    if machine {
        writeln!(
            out,
            "{}",
            json!({"schema_version":1,"ok":true,"data":value})
        )?;
        return Ok(());
    }
    if let Some(streams) = value.get("streams").and_then(Value::as_array) {
        return show_streams(
            &mut out,
            streams,
            value["mode"].as_str().unwrap_or("ranked"),
        );
    }
    if let Some(ranking) = value.get("ranking").and_then(Value::as_array) {
        if let Some(f) = value["fallback"].as_str() {
            writeln!(out, "Ranker unavailable: {}", clean(f, 300))?;
            writeln!(out, "Deterministic order kept.\n")?;
        } else if value["cached"] == true {
            writeln!(out, "Nothing changed since the last ranking; reusing it.\n")?;
        }
        for (i, r) in ranking.iter().enumerate() {
            writeln!(
                out,
                "{:>3}. {}",
                i + 1,
                clean(r["name"].as_str().unwrap_or(""), 70)
            )?;
            if let Some(why) = r["why"].as_str().filter(|w| !w.is_empty()) {
                writeln!(out, "     {}", clean(why, 200))?;
            }
            if let Some(next) = r["next"].as_str().filter(|n| !n.is_empty()) {
                writeln!(out, "     → {}", clean(next, 200))?;
            }
        }
        if let Some(note) = value["note"].as_str().filter(|n| !n.is_empty()) {
            writeln!(out, "\n{}", clean(note, 300))?;
        }
        return Ok(());
    }
    if let Some(sessions) = value.get("sessions").and_then(Value::as_array) {
        if sessions.is_empty() {
            writeln!(
                out,
                "No tracked contexts in this view. Use sync, agents, track, or triage --all."
            )?;
        }
        for s in sessions {
            let id = s["id"].as_str().unwrap_or("");
            let title = s["name"].as_str().filter(|v| !v.is_empty()).unwrap_or(id);
            let flags = format!(
                "{}{}{}",
                if s["resolved"] == true {
                    " · resolved"
                } else {
                    ""
                },
                if s["snoozed_until"].as_i64().unwrap_or(0) > now() {
                    " · snoozed"
                } else {
                    ""
                },
                if s["availability"] == "closed" {
                    " · closed"
                } else {
                    ""
                }
            );
            writeln!(
                out,
                "{:>3}  {} · {}{}",
                s["score"],
                clean(title, 70),
                s["state"].as_str().unwrap_or("unknown"),
                flags
            )?;
            writeln!(
                out,
                "     {}  [{}; {} evidence]  {}",
                clean(id, 100),
                clean(s["agent"].as_str().unwrap_or("terminal"), 60),
                clean(s["state_source"].as_str().unwrap_or("unknown"), 30),
                clean(s["cwd"].as_str().unwrap_or(""), 90)
            )?;
            let action = s["note"]
                .as_str()
                .or(s["handoff"].as_str())
                .or(s["command"].as_str())
                .unwrap_or("Review this session and capture a next action");
            writeln!(out, "     → {}", clean(action, 150))?;
            writeln!(out, "     {}", reasons_line(s))?;
        }
    } else if let Some(s) = value.get("session") {
        writeln!(
            out,
            "{}",
            clean(
                s["name"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .unwrap_or(s["id"].as_str().unwrap_or("")),
                120
            )
        )?;
        for (label, key) in [
            ("Session", "id"),
            ("Directory", "cwd"),
            ("Repository", "repo"),
            ("Branch", "branch"),
            ("Location", "location"),
            ("Availability", "availability"),
            ("State", "state"),
            ("Evidence source", "state_source"),
            ("Agent", "agent"),
            ("Age source", "age_source"),
        ] {
            let text = s[key].as_str().unwrap_or("");
            if !text.is_empty() {
                writeln!(out, "  {label}: {}", clean(text, 180))?;
            }
        }
        for (label, key) in [
            ("Created", "created_at"),
            ("Last activity", "last_active"),
            ("Last observed", "observed_at"),
            ("Reviewed", "reviewed_at"),
            ("Snoozed until", "snoozed_until"),
        ] {
            if let Some(t) = s[key].as_i64().filter(|t| *t > 0) {
                if let Some(date) = Local.timestamp_opt(t, 0).single() {
                    writeln!(out, "  {label}: {date}")?;
                }
            }
        }
        writeln!(
            out,
            "  Pinned: {} · Resolved: {} · Revision: {} / reviewed {}",
            s["pinned"], s["resolved"], s["revision"], s["reviewed_revision"]
        )?;
        writeln!(
            out,
            "\nNext action: {}",
            clean(
                s["note"].as_str().unwrap_or("No next action captured"),
                2000
            )
        )?;
        if let Some(handoff) = s["handoff"].as_str() {
            writeln!(out, "Agent handoff: {}", clean(handoff, 2000))?;
        }
        writeln!(
            out,
            "\nDebt: {} · Heat: {:.2}",
            s["score"],
            s["heat"].as_f64().unwrap_or(0.0)
        )?;
        writeln!(out, "  {}", reasons_line(s))?;
        writeln!(out, "\nRecent events (full details: show ID --json)")?;
        if let Some(events) = value["events"].as_array() {
            for event in events.iter().take(8) {
                writeln!(
                    out,
                    "  {}  {}  {}",
                    event["at"],
                    event["kind"].as_str().unwrap_or(""),
                    clean(&event["data"].to_string(), 200)
                )?;
            }
        }
    } else if let Some(text) = value.get("text").and_then(Value::as_str) {
        for line in text.lines() {
            writeln!(out, "{}", clean(line, 2000))?;
        }
    } else if value.get("tracked").is_some() {
        // Passive tracking is intentionally silent.
    } else {
        writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
    }
    Ok(())
}

fn shell(conn: &mut Connection, git: &mut Git, no_sync: bool) -> Result<()> {
    println!("StewardShell · your local attention queue\nCommands: queue, rank, focus, group, stream, show, capture, report, sync, agents\nUse help for options; quote notes with spaces. quit exits. No OS commands are executed here.\n");
    if !no_sync && cfg!(target_os = "macos") {
        match iterm::sync(conn, now()) {
            Ok(v) => display(&v, false)?,
            Err(e) => eprintln!("stewsh: {e}"),
        }
    }
    display(
        &execute(
            conn,
            git,
            Commands::Queue {
                mode: "ranked".into(),
                all: false,
                limit: 12,
            },
            now(),
        )?,
        false,
    )?;
    let interactive = io::stdin().is_terminal();
    loop {
        if interactive {
            print!("\nstewsh> ");
            io::stdout().flush()?;
        }
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let Some(args) = shlex::split(line.trim()) else {
            eprintln!("stewsh: unmatched quote or escape");
            continue;
        };
        if args.is_empty() {
            continue;
        }
        if args.len() == 1 && matches!(args[0].as_str(), "quit" | "exit") {
            break;
        }
        let parsed = Cli::try_parse_from(std::iter::once("stewsh".to_string()).chain(args));
        match parsed {
            Ok(cli) => {
                if cli.db.is_some() {
                    eprintln!("stewsh: launch a new stewsh process to change databases");
                    continue;
                }
                match dispatch(conn, git, cli.command.unwrap_or(Commands::Next), now()) {
                    Ok(v) => display(&v, cli.json)?,
                    Err(e) => emit_error(&*e, cli.json),
                }
            }
            Err(e) => {
                e.print()?;
            }
        }
    }
    Ok(())
}

fn emit_error(error: &dyn Error, machine: bool) {
    if machine {
        println!(
            "{}",
            json!({"schema_version":1,"ok":false,"error":{"message":error.to_string()}})
        );
    } else {
        eprintln!("stewsh: {}", clean(&error.to_string(), 1000));
    }
}

fn run(cli: Cli) -> Result<()> {
    let db = match cli
        .db
        .or_else(|| env::var_os("STEWSH_DB").map(PathBuf::from))
    {
        Some(path) => path,
        None => dirs::home_dir()
            .ok_or("cannot find home; pass --db")?
            .join(".config/stewsh/stewsh.db"),
    };
    let mut conn = store::open(&db, cli.passive)?;
    let mut git = Git::new(cli.pr);
    let command = cli.command.unwrap_or_else(|| {
        if cli.json || !io::stdin().is_terminal() {
            Commands::Queue {
                mode: "ranked".into(),
                all: false,
                limit: 12,
            }
        } else {
            // The web view is the product; the REPL stays for scripting.
            Commands::Serve {
                port: 7777,
                no_open: false,
            }
        }
    });
    match command {
        Commands::Shell { no_sync } => {
            if cli.json {
                return Err("shell is interactive; use queue --json for machine output".into());
            }
            shell(&mut conn, &mut git, no_sync)
        }
        Commands::Serve { port, no_open } => {
            if cli.json {
                return Err("serve is interactive; use queue --json for machine output".into());
            }
            serve::start(conn, port, cli.pr, !no_open)
        }
        command => display(&dispatch(&mut conn, &mut git, command, now())?, cli.json),
    }
}

fn main() {
    let machine = env::args().any(|a| a == "--json");
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            if machine && e.use_stderr() {
                emit_error(&e, true);
                std::process::exit(2);
            }
            e.exit();
        }
    };
    if let Err(e) = run(cli) {
        if e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        emit_error(&*e, machine);
        std::process::exit(1);
    }
}
