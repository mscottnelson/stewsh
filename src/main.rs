use clap::{Parser, Subcommand};
use rusqlite::{params, Connection};
use std::{env, error::Error, fs, path::PathBuf, time::Duration};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(
    name = "stewsh",
    version,
    about = "StewardShell: Your local TTY session secretary"
)]
struct Cli {
    /// Override ~/.config/stewsh/stewsh.db (useful for isolated workspaces).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Record a terminal interaction; successful tracking is silent.
    Track {
        session_id: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        exit_code: Option<i32>,
    },
    /// Show the most urgent terminal contexts.
    Triage {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
    },
    /// Attach a high-priority note to the current terminal.
    Capture {
        note: String,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Print the highest-priority context without removing it.
    Next,
}

#[derive(Debug)]
struct Context {
    id: String,
    cwd: String,
    command: Option<String>,
    note: Option<String>,
    exit_code: Option<i32>,
    last_active: i64,
    priority: f64,
}

fn score(priority: f64, last_active: i64, now: i64) -> f64 {
    let minutes = now.saturating_sub(last_active).max(0) as f64 / 60.0;
    (priority * 10.0) / (minutes + 1.0).powf(1.2)
}

fn database(path: Option<PathBuf>) -> Result<Connection> {
    let path = match path {
        Some(p) => p,
        None => dirs::home_dir()
            .ok_or("cannot determine home directory; use --db")?
            .join(".config/stewsh/stewsh.db"),
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    // Keep terminal metadata private on Unix, including SQLite sidecars.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY, cwd TEXT NOT NULL, command TEXT, exit_code INTEGER,
            creation_time INTEGER NOT NULL, last_active_interaction INTEGER NOT NULL,
            note TEXT, base_priority REAL NOT NULL DEFAULT 1
        );",
    )?;
    Ok(conn)
}

fn session(explicit: Option<String>) -> Result<String> {
    explicit
        .or_else(|| env::var("STEWSH_SESSION_ID").ok())
        .or_else(|| env::var("TTY").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no terminal session: source shell/stewsh.zsh, set STEWSH_SESSION_ID, or pass --session-id".into())
}

fn ranked(conn: &Connection, now: i64) -> Result<Vec<Context>> {
    let mut query = conn.prepare(
        "SELECT id,cwd,command,note,exit_code,last_active_interaction,base_priority FROM sessions",
    )?;
    let mut rows = query
        .query_map([], |r| {
            Ok(Context {
                id: r.get(0)?,
                cwd: r.get(1)?,
                command: r.get(2)?,
                note: r.get(3)?,
                exit_code: r.get(4)?,
                last_active: r.get(5)?,
                priority: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.sort_by(|a, b| {
        score(b.priority, b.last_active, now)
            .total_cmp(&score(a.priority, a.last_active, now))
            .then_with(|| b.last_active.cmp(&a.last_active))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(rows)
}

fn clean(text: &str, width: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(width)
        .collect()
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let now = chrono::Utc::now().timestamp();
    let cwd = env::current_dir()?.to_string_lossy().into_owned();
    let conn = database(cli.db)?;
    match cli.command {
        Commands::Track {
            session_id,
            command,
            exit_code,
        } => {
            if session_id.trim().is_empty() {
                return Err("session ID must not be empty".into());
            }
            conn.execute(
                "INSERT INTO sessions (id,cwd,command,exit_code,creation_time,last_active_interaction,base_priority)
                 VALUES (?1,?2,?3,?4,?5,?5,CASE WHEN COALESCE(?4,0)!=0 THEN 5 ELSE 1 END)
                 ON CONFLICT(id) DO UPDATE SET cwd=excluded.cwd,
                 command=COALESCE(?3,sessions.command),
                 exit_code=CASE WHEN ?3 IS NOT NULL OR ?4 IS NOT NULL THEN ?4 ELSE sessions.exit_code END,
                 last_active_interaction=?5,
                 base_priority=CASE WHEN sessions.note IS NOT NULL THEN 10
                   WHEN ?4 IS NOT NULL THEN CASE WHEN ?4!=0 THEN 5 ELSE 1 END
                   ELSE sessions.base_priority END",
                params![session_id,cwd,command,exit_code,now],
            )?;
        }
        Commands::Capture { note, session_id } => {
            if note.trim().is_empty() {
                return Err("note must not be empty".into());
            }
            let id = session(session_id)?;
            conn.execute(
                "INSERT INTO sessions (id,cwd,creation_time,last_active_interaction,note,base_priority)
                 VALUES (?1,?2,?3,?3,?4,10)
                 ON CONFLICT(id) DO UPDATE SET cwd=?2,last_active_interaction=?3,note=?4,base_priority=10",
                params![id,cwd,now,note],
            )?;
            println!("Captured for {}: {}", clean(&id, 80), clean(&note, 240));
        }
        Commands::Triage { limit } => {
            let rows = ranked(&conn, now)?;
            if rows.is_empty() {
                println!("No tracked contexts yet.");
                return Ok(());
            }
            println!(
                "{:>8}  {:<24}  {:<32}  CONTEXT",
                "SCORE", "SESSION", "DIRECTORY"
            );
            for row in rows.iter().take(limit as usize) {
                print_context(row, now);
            }
        }
        Commands::Next => {
            if let Some(row) = ranked(&conn, now)?.first() {
                print_context(row, now);
            } else {
                println!("No tracked contexts yet.");
            }
        }
    }
    Ok(())
}

fn print_context(row: &Context, now: i64) {
    let text = row
        .note
        .as_deref()
        .or(row.command.as_deref())
        .unwrap_or("Active session");
    let failure = row
        .exit_code
        .filter(|c| *c != 0)
        .map(|c| format!(" [exit {c}]"))
        .unwrap_or_default();
    println!(
        "{:>8.2}  {:<24}  {:<32}  {}{}",
        score(row.priority, row.last_active, now),
        clean(&row.id, 24),
        clean(&row.cwd, 32),
        clean(text, 100),
        failure
    );
}

fn main() {
    if let Err(error) = run() {
        eprintln!("stewsh: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urgency_and_decay() {
        assert_eq!(score(1.0, 100, 100), 10.0);
        assert!(score(10.0, 100, 100) > score(5.0, 100, 100));
        assert!(score(1.0, 100, 100) > score(1.0, 100, 160));
        assert_eq!(score(1.0, 200, 100), 10.0);
    }
    #[test]
    fn terminal_controls_are_removed() {
        assert_eq!(clean("a\n\u{1b}b", 4), "a  b");
    }
}
