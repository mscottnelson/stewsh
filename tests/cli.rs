use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};
static COUNTER: AtomicUsize = AtomicUsize::new(0);
struct App {
    root: PathBuf,
    db: PathBuf,
}
impl App {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "stewsh-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let db = root.join("state.db");
        Self { root, db }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_stewsh"));
        c.arg("--db").arg(&self.db);
        // The suite resolves identity, so the developer's own shell session
        // must not leak in: inside iTerm, or a stewsh-hooked zsh, these are set.
        c.env_remove("STEWSH_SESSION_ID")
            .env_remove("ITERM_SESSION_ID");
        c
    }
    /// Run from a chosen directory, because the working directory is itself an
    /// input to identity and grouping.
    fn run_in(&self, dir: &std::path::Path, args: &[&str]) -> Value {
        let out = self
            .command()
            .current_dir(dir)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{:?}: {} {}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let envelope: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(envelope["ok"], true);
        envelope["data"].clone()
    }
    /// One MCP session: every request in, stdin closed, every reply back. The
    /// server exits when stdin ends, so this needs no timeout.
    fn mcp(&self, requests: &[&str]) -> Vec<Value> {
        let mut child = self
            .command()
            .current_dir(&self.root)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            for request in requests {
                writeln!(stdin, "{request}").unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn run(&self, args: &[&str]) -> Value {
        let out = self.command().arg("--json").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{:?}: {} {}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let envelope: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["ok"], true);
        envelope["data"].clone()
    }
    fn fail(&self, args: &[&str]) -> Value {
        let out = self.command().arg("--json").args(args).output().unwrap();
        assert!(!out.status.success());
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["ok"], false);
        v
    }
    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
    fn show(&self, id: &str) -> Value {
        self.run(&["show", id])["session"].clone()
    }
}
impl Drop for App {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn old_work_and_next_actions_do_not_decay() {
    let a = App::new();
    a.run(&["track", "recent"]);
    a.run(&["track", "yesterday"]);
    a.sql("UPDATE sessions SET creation_time=creation_time-172800,last_active_interaction=last_active_interaction-172800 WHERE id='yesterday'");
    assert_eq!(a.run(&["next"])["sessions"][0]["id"], "yesterday");
    a.run(&["capture", "Review this patch", "--session-id", "yesterday"]);
    a.sql("UPDATE sessions SET last_active_interaction=0 WHERE id='yesterday'");
    a.run(&["track", "failure", "--exit-code", "1"]);
    assert_eq!(a.run(&["next"])["sessions"][0]["id"], "yesterday");
    a.run(&["pin", "recent"]);
    assert_eq!(a.run(&["next"])["sessions"][0]["id"], "recent");
    a.run(&["pin", "recent", "--off"]);
    assert_eq!(a.run(&["next"])["sessions"][0]["id"], "yesterday");
    assert_eq!(
        a.run(&["triage", "--older"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn review_uses_revisions_even_with_same_second_events() {
    let a = App::new();
    a.run(&["track", "s", "--exit-code", "1"]);
    let initial = a.show("s")["score"].as_i64().unwrap();
    a.run(&["review", "s"]);
    let reviewed = a.show("s");
    assert!(reviewed["score"].as_i64().unwrap() < initial);
    a.run(&["track", "s", "--state", "working"]);
    let changed = a.show("s");
    assert!(changed["revision"].as_i64().unwrap() > changed["reviewed_revision"].as_i64().unwrap());
    a.run(&["track", "s", "--exit-code", "0"]);
    assert_eq!(a.show("s")["state"], "idle");
}

#[test]
fn snooze_resolution_and_closed_are_distinct() {
    let a = App::new();
    a.run(&["capture", "Keep me", "--session-id", "s"]);
    a.run(&["snooze", "s", "--minutes", "60"]);
    assert!(a.run(&["next"])["sessions"].as_array().unwrap().is_empty());
    a.run(&["report", "waiting", "--session-id", "s"]);
    assert!(a.run(&["next"])["sessions"].as_array().unwrap().is_empty());
    a.sql("UPDATE sessions SET snoozed_until=1");
    assert_eq!(a.run(&["next"])["sessions"][0]["id"], "s");
    a.run(&["resolve", "s"]);
    a.run(&["track", "s", "--exit-code", "1"]);
    assert!(a.run(&["next"])["sessions"].as_array().unwrap().is_empty());
    a.run(&["reopen", "s"]);
    a.run(&["close", "s"]);
    assert!(a.run(&["next"])["sessions"].as_array().unwrap().is_empty());
    let s = a.run(&["triage", "--all"])["sessions"][0].clone();
    assert_eq!(s["resolved"], false);
    assert_eq!(s["availability"], "closed");
    assert_eq!(s["note"], "Keep me");
}

#[test]
fn reports_preserve_intent_and_deduplicate_retries() {
    let a = App::new();
    a.run(&[
        "capture",
        "Review security implications",
        "--session-id",
        "s",
    ]);
    let args = [
        "report",
        "ready",
        "--session-id",
        "s",
        "--agent",
        "codex",
        "--summary",
        "Tests passed; inspect auth changes",
        "--event-id",
        "turn-1",
    ];
    a.run(&args);
    let revision = a.show("s")["revision"].clone();
    assert_eq!(a.run(&args)["duplicate"], true);
    let s = a.show("s");
    assert_eq!(s["revision"], revision);
    assert_eq!(s["note"], "Review security implications");
    assert_eq!(s["state"], "ready");
    assert_eq!(s["resolved"], false);
    a.run(&["capture", "--clear", "--session-id", "s"]);
    assert!(a.show("s")["note"].is_null());
}

#[test]
fn migration_preserves_v01_data() {
    let a = App::new();
    a.sql("CREATE TABLE sessions(id TEXT PRIMARY KEY,cwd TEXT NOT NULL,command TEXT,exit_code INTEGER,creation_time INTEGER NOT NULL,last_active_interaction INTEGER NOT NULL,note TEXT,base_priority REAL NOT NULL DEFAULT 1); INSERT INTO sessions VALUES('legacy','/work','cargo test',1,100,200,'Find the failure',10)");
    let s = a.show("legacy");
    assert_eq!(s["note"], "Find the failure");
    assert_eq!(s["command"], "cargo test");
    assert_eq!(s["state"], "failed");
    assert_eq!(s["created_at"], 100);
    a.run(&["doctor"]);
    assert_eq!(a.show("legacy")["note"], "Find the failure");
}

#[test]
fn invalid_inputs_and_ambiguous_ids_do_not_mutate() {
    let a = App::new();
    a.run(&["track", "same-a"]);
    a.run(&["track", "same-b"]);
    a.fail(&["review", "same"]);
    a.fail(&["resolve", "missing"]);
    a.fail(&["capture", "", "--session-id", "same-a"]);
    a.fail(&["snooze", "same-a", "--minutes", "0"]);
    a.fail(&["report", "bogus", "--session-id", "same-a"]);
    assert_eq!(a.show("same-a")["resolved"], false);
    a.run(&["review", "same-a"]);
    assert!(a.show("same-a")["reviewed_at"].is_i64());
}

#[test]
fn interactive_shell_uses_same_commands_and_quoting() {
    let a = App::new();
    let mut child = a
        .command()
        .args(["shell", "--no-sync"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"track demo\ncapture 'Check the migration' --session-id demo\nreport waiting --session-id demo --summary 'Need a decision'\ntriage\nquit\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Check the migration"));
    assert_eq!(a.show("demo")["state"], "waiting");
}

#[test]
fn terminal_controls_are_not_executed_by_text_output() {
    let a = App::new();
    a.run(&[
        "capture",
        "hello\u{1b}]52;c;secret\u{7}",
        "--session-id",
        "s",
    ]);
    let out = a.command().arg("triage").output().unwrap();
    assert!(!out.stdout.contains(&0x1b));
    assert!(!out.stdout.contains(&7));
}

#[test]
fn future_schema_is_rejected_without_rewriting() {
    let a = App::new();
    a.sql("PRAGMA user_version=99");
    a.fail(&["doctor"]);
    let c = rusqlite::Connection::open(&a.db).unwrap();
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 99);
}

#[test]
fn zsh_hooks_are_idempotent_private_and_preserve_exit_status() {
    if Command::new("zsh").arg("--version").output().is_err() {
        return;
    }
    let a = App::new();
    let script = r#"
source "$STEWSH_TEST_SCRIPT"
source "$STEWSH_TEST_SCRIPT"
[[ ${#preexec_functions} == 1 && ${#precmd_functions} == 1 ]] || exit 70
_stewsh_preexec 'false secret-command-argument'
false
_stewsh_precmd
code=$?
[[ $code == 1 ]] || exit 71
_stewsh_preexec 'stewsh triage'
[[ -z ${_stewsh_pending-} ]] || exit 72
"#;
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_stewsh"));
    let out = Command::new("zsh")
        .args(["-f", "-i", "-c", script])
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("STEWSH_DB", &a.db)
        .env("ITERM_SESSION_ID", "w1t1p1:HOOK-TEST")
        .env(
            "STEWSH_TEST_SCRIPT",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("shell/stewsh.zsh"),
        )
        .env_remove("STEWSH_RECORD_COMMANDS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = a.show("iterm:HOOK-TEST");
    assert!(s["command"].is_null());
    assert_eq!(s["exit_code"], 1);
    assert_eq!(s["availability"], "open");
}

#[test]
fn agent_identity_is_exact_while_humans_may_use_prefixes() {
    let a = App::new();
    a.run(&["track", "long-session-id"]);
    // An exact ID attaches to the existing context.
    a.run(&["capture", "Finish this", "--session-id", "long-session-id"]);
    a.run(&["report", "waiting", "--session-id", "long-session-id"]);
    assert_eq!(a.show("long-session-id")["note"], "Finish this");
    assert_eq!(
        a.run(&["triage", "--all"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A prefix must never let an agent adopt somebody else's context: it gets
    // its own, and the original note is untouched.
    a.run(&["report", "failed", "--session-id", "long-s"]);
    assert_eq!(a.show("long-session-id")["state"], "waiting");
    assert_eq!(a.show("long-s")["state"], "failed");
    // Humans typing IDs still get prefix resolution, and ambiguity still fails.
    a.run(&["track", "long-second-id"]);
    a.fail(&["show", "long-se"]);
    assert_eq!(a.show("long-sec")["id"], "long-second-id");
}

#[test]
fn heat_decays_while_debt_does_not() {
    let a = App::new();
    let now = chrono::Utc::now().timestamp();
    a.run(&["track", "cold", "--command", "cargo build"]);
    a.run(&["track", "warm", "--command", "cargo build"]);
    // Age the cold context's activity by six hours: four half-lives.
    a.sql(&format!(
        "UPDATE events SET at={} WHERE session_id='cold';
         UPDATE sessions SET last_active_interaction={} WHERE id='cold';",
        now - 21_600,
        now - 21_600
    ));
    let streams = a.run(&["queue", "--mode", "active", "--all"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let heat = |id: &str| -> f64 {
        streams
            .iter()
            .flat_map(|s| s["members"].as_array().unwrap().clone())
            .find(|m| m["id"] == id)
            .map(|m| m["heat"].as_f64().unwrap())
            .unwrap()
    };
    assert!(heat("warm") > heat("cold") * 8.0, "recency must lead");
    // Debt ignores age entirely: both are equally unfinished.
    let debt = |id: &str| -> i64 {
        streams
            .iter()
            .flat_map(|s| s["members"].as_array().unwrap().clone())
            .find(|m| m["id"] == id)
            .map(|m| m["score"].as_i64().unwrap())
            .unwrap()
    };
    assert!(debt("cold") >= debt("warm"));
}

#[test]
fn streams_group_by_repo_and_manual_grouping_wins() {
    let a = App::new();
    a.run(&["track", "one"]);
    a.run(&["track", "two"]);
    a.run(&["group", "one", "--to", "Rate bump arc"]);
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let arc = streams
        .iter()
        .find(|s| s["name"] == "Rate bump arc")
        .expect("manual stream exists");
    assert_eq!(arc["members"].as_array().unwrap().len(), 1);
    assert_eq!(arc["members"][0]["id"], "one");
    // Regrouping runs on every sync and must not undo the manual choice.
    let home = a.root.join("empty-home");
    fs::create_dir_all(&home).unwrap();
    assert!(a
        .command()
        .arg("--json")
        .args(["agents", "--days", "1"])
        .env("HOME", &home)
        .output()
        .unwrap()
        .status
        .success());
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let arc = streams
        .iter()
        .find(|s| s["name"] == "Rate bump arc")
        .unwrap();
    assert_eq!(arc["members"][0]["id"], "one");
}

#[test]
fn stream_pin_and_archive_change_the_queue() {
    let a = App::new();
    a.run(&["track", "one"]);
    a.run(&["group", "one", "--to", "Alpha"]);
    a.run(&["stream", "pin", "alpha"]); // lookup stays case-insensitive
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let alpha = streams.iter().find(|s| s["id"] == "Alpha").unwrap();
    assert_eq!(alpha["pinned"], true);
    assert!(alpha["score"].as_i64().unwrap() >= 100);
    a.run(&["stream", "archive", "alpha"]);
    let visible = a.run(&["queue"])["streams"].as_array().unwrap().clone();
    assert!(visible.iter().all(|s| s["id"] != "Alpha"));
}

#[test]
fn ranker_failure_keeps_deterministic_order_and_reports_why() {
    let a = App::new();
    a.run(&["track", "one", "--command", "cargo test"]);
    let out = a
        .command()
        .arg("--json")
        .args(["rank"])
        .env("STEWSH_RANKER", "/nonexistent/ranker-binary")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "a broken ranker must not fail the command");
    assert!(v["data"]["fallback"].is_string());
    // Every stream still gets an ordinal, so `focus 1` keeps working.
    assert!(v["data"]["ranking"][0]["stream"].is_string());
    let streams = a.run(&["queue", "--mode", "ranked", "--all"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(streams[0]["ordinal"], 1);
}

#[test]
fn ranking_is_cached_until_the_situation_changes() {
    let a = App::new();
    a.run(&["track", "one", "--command", "cargo test"]);
    let ranker = a.root.join("ranker.sh");
    fs::write(
        &ranker,
        r#"#!/bin/sh
cat > /dev/null
echo call >> "$STEWSH_TEST_CALLS"
printf '{"ranking":[]}'
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ranker, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let calls = a.root.join("calls");
    let rank = || {
        a.command()
            .arg("--json")
            .arg("rank")
            .env("STEWSH_RANKER", &ranker)
            .env("STEWSH_TEST_CALLS", &calls)
            .output()
            .unwrap()
    };
    rank();
    let first = fs::read_to_string(&calls)
        .unwrap_or_default()
        .lines()
        .count();
    let second_out = rank();
    let second: Value = serde_json::from_slice(&second_out.stdout).unwrap();
    assert_eq!(
        second["data"]["cached"], true,
        "unchanged desk must not re-call"
    );
    assert_eq!(
        fs::read_to_string(&calls)
            .unwrap_or_default()
            .lines()
            .count(),
        first
    );
    // A real change invalidates the cache.
    a.run(&["capture", "New action", "--session-id", "one"]);
    let third_out = rank();
    let third: Value = serde_json::from_slice(&third_out.stdout).unwrap();
    assert_ne!(third["data"]["cached"], true);
}

#[test]
fn rank_dry_run_exposes_the_evidence_without_calling_a_model() {
    let a = App::new();
    a.run(&["track", "one", "--command", "cargo test"]);
    a.run(&["capture", "Ship it", "--session-id", "one"]);
    let doc = a.run(&["rank", "--dry-run", "--intent", "shipping"]);
    assert_eq!(doc["intent"], "shipping");
    let stream = &doc["streams"][0];
    assert!(stream["heat"].is_number() && stream["debt"].is_number());
    assert_eq!(stream["members"][0]["note"], "Ship it");
}

#[test]
fn harness_reads_claude_transcripts_without_hooks() {
    let a = App::new();
    let home = a.root.join("home");
    let project = home.join(".claude/projects/-tmp-demo");
    fs::create_dir_all(&project).unwrap();
    let cwd = a.root.to_string_lossy().into_owned();
    fs::write(
        project.join("abc-123.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"type":"ai-title","aiTitle":"Rate bump arc","sessionId":"abc-123"}),
            serde_json::json!({"type":"user","cwd":cwd,"gitBranch":"feat/x",
                "timestamp":"2026-09-09T10:00:00.000Z","message":{"content":"go"}}),
            serde_json::json!({"type":"assistant","cwd":cwd,"gitBranch":"feat/x",
                "timestamp":"2026-09-09T10:01:00.000Z",
                "message":{"content":[{"text":"Tests pass; review the migration."}]}}),
        ),
    )
    .unwrap();
    let out = a
        .command()
        .arg("--json")
        .args(["agents", "--days", "365"])
        .env("HOME", &home)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["data"]["agent_sessions"], 1);
    let s = a.show("claude:abc-123");
    assert_eq!(s["name"], "Rate bump arc");
    assert_eq!(s["agent"], "claude");
    assert_eq!(s["kind"], "agent");
    // Turn ended on the assistant, so the work is ready for a human look.
    assert_eq!(s["state"], "ready");
    assert_eq!(s["state_source"], "harness");
    assert_eq!(s["handoff"], "Tests pass; review the migration.");
}

#[test]
fn web_view_serves_the_queue_over_loopback() {
    let a = App::new();
    a.run(&["track", "one", "--command", "cargo test"]);
    let mut child = a
        .command()
        .args(["serve", "--port", "0", "--no-open"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    {
        use std::io::{BufRead, BufReader};
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        reader.read_line(&mut line).unwrap();
    }
    let url = line
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .expect("server prints its URL")
        .to_string();
    let page = Command::new("curl").args(["-sf", &url]).output().unwrap();
    let queue = Command::new("curl")
        .args(["-sf", &format!("{url}/api/queue")])
        .output()
        .unwrap();
    // The agent-facing reads are on the same server, from the same builders.
    let brief = Command::new("curl")
        .args(["-sf", &format!("{url}/api/brief?mode=debt&budget=900")])
        .output()
        .unwrap();
    let whoami = Command::new("curl")
        .args(["-sf", &format!("{url}/api/whoami?session_id=one")])
        .output()
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(String::from_utf8_lossy(&page.stdout).contains("StewardShell"));
    let v: Value = serde_json::from_slice(&queue.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["streams"][0]["members"][0]["id"], "one");
    let v: Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(v["data"]["mode"], "debt");
    assert_eq!(v["data"]["budget_tokens"], 900);
    assert_eq!(v["data"]["queue"][0]["members"][0]["id"], "one");
    let v: Value = serde_json::from_slice(&whoami.stdout).unwrap();
    assert_eq!(v["data"]["via"], "argument");
    assert_eq!(v["data"]["context"]["id"], "one");
}

#[cfg(target_os = "macos")]
#[test]
fn complete_snapshots_preserve_user_state_and_failed_sync_keeps_availability() {
    use std::os::unix::fs::PermissionsExt;
    let a = App::new();
    let mock = a.root.join("osascript");
    fs::write(&mock, "#!/bin/sh\ncat \"$STEWSH_TEST_PANES\"\n").unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = a.root.join("panes.json");
    fs::write(&fixture,r#"[{"id":"pane-one","name":"Test pane","tty":"/dev/no-test-tty","cwd":"/work","text":"Do you want to continue? [y/n]","prompt":false,"location":"Window 1"}]"#).unwrap();
    let sync = || {
        a.command()
            .args(["--json", "sync"])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    a.root.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("STEWSH_TEST_PANES", &fixture)
            .output()
            .unwrap()
    };
    assert!(sync().status.success());
    assert_eq!(a.show("iterm:pane-one")["state"], "waiting");
    a.run(&[
        "capture",
        "Keep my intention",
        "--session-id",
        "iterm:pane-one",
    ]);
    a.run(&["pin", "iterm:pane-one"]);
    a.run(&["review", "iterm:pane-one"]);
    let revision = a.show("iterm:pane-one")["revision"].clone();
    assert!(sync().status.success());
    assert_eq!(a.show("iterm:pane-one")["revision"], revision);
    a.run(&[
        "report",
        "working",
        "--session-id",
        "iterm:pane-one",
        "--agent",
        "claude",
    ]);
    assert!(sync().status.success());
    let s = a.show("iterm:pane-one");
    assert_eq!(s["state"], "working");
    assert_eq!(s["pinned"], true);
    assert_eq!(s["note"], "Keep my intention");
    fs::write(&fixture, "invalid snapshot").unwrap();
    assert!(!sync().status.success());
    assert_eq!(a.show("iterm:pane-one")["availability"], "open");
    fs::write(&fixture, "[]").unwrap();
    assert!(sync().status.success());
    let s = a.show("iterm:pane-one");
    assert_eq!(s["availability"], "closed");
    assert_eq!(s["resolved"], false);
}

#[cfg(target_os = "macos")]
#[test]
fn a_changed_screen_is_activity_but_not_a_new_revision() {
    use std::os::unix::fs::PermissionsExt;
    let a = App::new();
    let mock = a.root.join("osascript");
    fs::write(&mock, "#!/bin/sh\ncat \"$STEWSH_TEST_PANES\"\n").unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = a.root.join("panes.json");
    let pane = |text: &str| {
        format!(
            r#"[{{"id":"p1","name":"Build","tty":"/dev/no-test-tty","cwd":"/work","text":"{text}","prompt":false,"location":"Window 1"}}]"#
        )
    };
    let sync = || {
        a.command()
            .args(["--json", "sync"])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    a.root.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("STEWSH_TEST_PANES", &fixture)
            .output()
            .unwrap()
    };
    fs::write(&fixture, pane("building |")).unwrap();
    assert!(sync().status.success());
    a.run(&["review", "iterm:p1"]);
    let reviewed = a.show("iterm:p1");
    assert_eq!(reviewed["revision"], reviewed["reviewed_revision"]);

    // A spinner advancing changes the screen fingerprint. That is activity,
    // and it must not silently cancel the review the user just made.
    fs::write(&fixture, pane("building /")).unwrap();
    assert!(sync().status.success());
    let after = a.show("iterm:p1");
    assert_eq!(
        after["revision"], reviewed["revision"],
        "spinner bumped revision"
    );
    assert_eq!(after["revision"], after["reviewed_revision"]);
    assert!(
        after["heat"].as_f64().unwrap() > 0.0,
        "still counted as activity"
    );

    // A changed signal category is a real revision and does resurface it.
    fs::write(&fixture, pane("error: build failed")).unwrap();
    assert!(sync().status.success());
    let changed = a.show("iterm:p1");
    assert_eq!(changed["state"], "failed");
    assert!(changed["revision"].as_i64().unwrap() > reviewed["revision"].as_i64().unwrap());
    assert!(changed["revision"].as_i64().unwrap() > changed["reviewed_revision"].as_i64().unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn focusing_a_stream_selects_its_pane_and_counts_as_intent() {
    use std::os::unix::fs::PermissionsExt;
    let a = App::new();
    let mock = a.root.join("osascript");
    // Records the arguments it was asked to act on, then answers like iTerm.
    fs::write(
        &mock,
        "#!/bin/sh\nif [ -n \"$STEWSH_TEST_PANES\" ] && [ $# -le 4 ]; then cat \"$STEWSH_TEST_PANES\"; \
         else echo \"$5 $6\" >> \"$STEWSH_TEST_FOCUS\"; printf '{\"focused\":true}'; fi\n",
    )
    .unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = a.root.join("panes.json");
    fs::write(&fixture, r#"[{"id":"p1","name":"Build","tty":"/dev/no-test-tty","cwd":"/work","text":"ok","prompt":true,"location":"Window 2 / Tab 1"}]"#).unwrap();
    let focus_log = a.root.join("focus.log");
    let path = format!(
        "{}:{}",
        a.root.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    assert!(a
        .command()
        .args(["--json", "sync"])
        .env("PATH", &path)
        .env("STEWSH_TEST_PANES", &fixture)
        .output()
        .unwrap()
        .status
        .success());
    a.run(&["group", "iterm:p1", "--to", "Build work"]);
    // An agent session in the same stream must not be chosen over the pane.
    a.run(&[
        "report",
        "ready",
        "--session-id",
        "agent-only",
        "--agent",
        "claude",
    ]);
    a.run(&["group", "agent-only", "--to", "Build work"]);

    let out = a
        .command()
        .args(["--json", "focus", "Build work"])
        .env("PATH", &path)
        .env("STEWSH_TEST_FOCUS", &focus_log)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["data"]["focused"], "iterm:p1");
    assert_eq!(v["data"]["location"], "Window 2 / Tab 1");
    assert!(fs::read_to_string(&focus_log).unwrap().contains("p1 focus"));
    // Choosing to go somewhere is the clearest statement of intent there is.
    let events = a.run(&["show", "iterm:p1"])["events"]
        .as_array()
        .unwrap()
        .clone();
    assert!(events.iter().any(|e| e["kind"] == "focused"));
}

#[test]
fn regrouping_does_not_orphan_browser_tabs() {
    let a = App::new();
    a.run(&["track", "pane-one"]);
    a.run(&["group", "pane-one", "--to", "Alpha"]);
    // Stand in for the browser adapter: a tab has no working directory, so a
    // regroup keyed on cwd would file it under "unassigned".
    a.sql(
        "INSERT INTO sessions(id,cwd,creation_time,last_active_interaction,kind,source,name,external_ref)
         VALUES('tab:abc','',1,1,'tab','browser','PR 12','https://github.com/acme/x/pull/12');
         INSERT INTO stream_members(stream_id,context_id,role,origin)
         VALUES('Alpha','tab:abc','tab','auto');",
    );
    a.run(&["track", "pane-two"]); // forces a regroup on the next assemble
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let alpha = streams.iter().find(|s| s["id"] == "Alpha").unwrap();
    let ids: Vec<&str> = alpha["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"tab:abc"), "tab was orphaned by regrouping");
    // And a tab contributes nothing to the queue's scoring.
    let tab = a.show("tab:abc");
    assert_eq!(tab["score"], 0);
    assert_eq!(tab["heat"].as_f64().unwrap(), 0.0);
}

#[test]
fn a_large_transcript_survives_a_split_character_at_the_tail_offset() {
    let a = App::new();
    let home = a.root.join("home");
    let project = home.join(".claude/projects/-tmp-big");
    fs::create_dir_all(&project).unwrap();
    let cwd = a.root.to_string_lossy().into_owned();
    let trailer = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"ai-title","aiTitle":"Long session","sessionId":"big-1"}),
        serde_json::json!({"type":"assistant","cwd":cwd,"gitBranch":"main",
            "timestamp":"2026-09-09T10:01:00.000Z",
            "message":{"content":[{"text":"still here"}]}}),
    );
    // Place the 256 KiB tail offset inside a run of two-byte characters, on a
    // continuation byte. That is what made a strict UTF-8 read fail, empty the
    // buffer, and silently drop a live session. The padding follows the run
    // rather than leading it: the offset is measured from the end, so only a
    // byte between the run and EOF moves which half of a character it lands
    // on. One such byte flips parity, so two attempts suffice.
    let path = project.join("big-1.jsonl");
    let build = |shim: usize| {
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"gitBranch\":\"main\",\"message\":{{\"content\":\"{}{}\"}}}}\n{trailer}",
            "\u{e9}".repeat(160_000),
            "x".repeat(shim)
        )
    };
    let body = (0..2)
        .map(build)
        .find(|b| {
            let bytes = b.as_bytes();
            bytes.len() > 262_144 && bytes[bytes.len() - 262_144] & 0b1100_0000 == 0b1000_0000
        })
        .expect("one of the two paddings splits a character at the tail offset");
    fs::write(&path, &body).unwrap();

    let out = a
        .command()
        .arg("--json")
        .args(["agents", "--days", "365"])
        .env("HOME", &home)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        v["data"]["agent_sessions"], 1,
        "split character dropped the session"
    );
    let s = a.show("claude:big-1");
    assert_eq!(s["name"], "Long session");
    assert_eq!(
        s["availability"], "open",
        "a live session was closed as absent"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn screen_evidence_still_reaches_a_pane_the_shell_hook_marked_working() {
    use std::os::unix::fs::PermissionsExt;
    let a = App::new();
    let mock = a.root.join("osascript");
    fs::write(&mock, "#!/bin/sh\ncat \"$STEWSH_TEST_PANES\"\n").unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = a.root.join("panes.json");
    let pane = |text: &str| {
        format!(
            r#"[{{"id":"p9","name":"Agent","tty":"/dev/no-test-tty","cwd":"/work","text":"{text}","prompt":false,"location":"W1"}}]"#
        )
    };
    let sync = || {
        a.command()
            .args(["--json", "sync"])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    a.root.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("STEWSH_TEST_PANES", &fixture)
            .output()
            .unwrap()
    };
    fs::write(&fixture, pane("ready")).unwrap();
    assert!(sync().status.success());
    // The zsh hook marks the pane working on every command it runs.
    a.run(&["track", "iterm:p9", "--state", "working"]);
    assert_eq!(a.show("iterm:p9")["state_source"], "shell");

    // The agent then asks a question. That must reach the queue.
    fs::write(&fixture, pane("Do you want to proceed? [y/n]")).unwrap();
    assert!(sync().status.success());
    let s = a.show("iterm:p9");
    assert_eq!(s["state"], "waiting", "screen evidence was discarded");
    assert_eq!(s["state_source"], "screen");

    // An explicit agent report still outranks the screen.
    a.run(&[
        "report",
        "working",
        "--session-id",
        "iterm:p9",
        "--agent",
        "claude",
    ]);
    fs::write(&fixture, pane("error: something failed")).unwrap();
    assert!(sync().status.success());
    let s = a.show("iterm:p9");
    assert_eq!(s["state"], "working", "an agent report must keep its veto");
    assert_eq!(s["state_source"], "agent");
}

/// Starts the web view and returns its base URL plus the child to kill.
#[cfg(unix)]
fn serve(a: &App) -> (String, std::process::Child) {
    use std::io::{BufRead, BufReader};
    let mut child = a
        .command()
        .args(["serve", "--port", "0", "--no-open"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .expect("server prints its URL")
        .to_string();
    (url, child)
}

#[cfg(unix)]
#[test]
fn the_web_view_answers_only_to_its_own_loopback_address() {
    let a = App::new();
    a.run(&["track", "one"]);
    let (url, mut child) = serve(&a);
    let code = |args: &[&str]| -> String {
        let out = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let ours = code(&[&format!("{url}/api/queue")]);
    // A rebound browser still sends the attacker's name in Host.
    let foreign = code(&["-H", "Host: evil.example.com", &format!("{url}/api/queue")]);
    let foreign_write = code(&[
        "-X",
        "POST",
        "-H",
        "Host: evil.example.com",
        "-H",
        "content-type: application/json",
        "-d",
        r#"{"id":"one","action":"pin"}"#,
        &format!("{url}/api/context"),
    ]);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(ours, "200");
    assert_eq!(foreign, "403", "a foreign Host must be refused");
    assert_eq!(foreign_write, "403", "a foreign Host must not mutate state");
    assert_eq!(a.show("one")["pinned"], false);
}

#[cfg(unix)]
#[test]
fn a_slow_ranker_does_not_freeze_the_rest_of_the_web_view() {
    use std::os::unix::fs::PermissionsExt;
    let a = App::new();
    a.run(&["track", "one", "--command", "cargo test"]);
    let started = a.root.join("ranker-started");
    let ranker = a.root.join("slow-ranker.sh");
    fs::write(
        &ranker,
        r#"#!/bin/sh
cat > /dev/null
touch "$STEWSH_TEST_STARTED"
sleep 5
printf '{"ranking":[]}'
"#,
    )
    .unwrap();
    fs::set_permissions(&ranker, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = {
        use std::io::{BufRead, BufReader};
        let mut c = a
            .command()
            .args(["serve", "--port", "0", "--no-open"])
            .env("STEWSH_RANKER", &ranker)
            .env("STEWSH_TEST_STARTED", &started)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(c.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .split_whitespace()
            .find(|w| w.starts_with("http://"))
            .expect("server prints its URL")
            .to_string();
        (c, url)
    };
    let (ref mut server, ref url) = child;
    let mut ranking = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-d",
            "{}",
            &format!("{url}/api/rank"),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // Wait until the ranker subprocess is genuinely running.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !started.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let saw_start = started.exists();
    // The lock must be free while the ranker thinks, so this returns promptly
    // rather than waiting out the five-second subprocess.
    let queue = Command::new("curl")
        .args(["-s", "--max-time", "3", &format!("{url}/api/queue")])
        .output()
        .unwrap();
    let _ = ranking.wait();
    let _ = server.kill();
    let _ = server.wait();
    assert!(saw_start, "the stub ranker never ran");
    assert!(
        queue.status.success(),
        "the queue blocked behind the ranker subprocess"
    );
    let v: Value = serde_json::from_slice(&queue.stdout).unwrap();
    assert_eq!(v["ok"], true);
}

#[test]
fn repository_evidence_keeps_a_stream_visible_after_its_panes_are_resolved() {
    let a = App::new();
    a.run(&["track", "one"]);
    // Stand in for a dirty worktree with no live pane left on it.
    a.sql(
        "INSERT INTO streams(id,name,key,repo,branch,worktree,created_at,dirty,ahead)
         VALUES('r1#wip','r1 wip','r1#wip','r1','wip','/tmp/wt',1,1,2);
         INSERT INTO stream_members(stream_id,context_id,role,origin)
         VALUES('r1#wip','one','pane','manual');",
    );
    a.run(&["resolve", "one"]);
    // A browser tab attached to the stream must not mask the git evidence
    // either: it makes the member list non-empty without being real work.
    a.sql(
        "INSERT INTO sessions(id,cwd,creation_time,last_active_interaction,kind,source,agent,name)
         VALUES('tab:zz','',1,1,'tab','browser','Google Chrome','PR page');
         INSERT INTO stream_members(stream_id,context_id,role,origin)
         VALUES('r1#wip','tab:zz','tab','auto');",
    );
    let debt = a.run(&["queue", "--mode", "debt", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let wip = debt
        .iter()
        .find(|s| s["id"] == "r1#wip")
        .expect("uncommitted work stays in the queue after its pane is resolved");
    assert_eq!(wip["dirty"], true);
    assert!(wip["debt"].as_i64().unwrap() >= 10);
}

#[test]
fn branches_differing_only_in_case_stay_separate_streams() {
    let a = App::new();
    a.run(&["track", "one"]);
    a.run(&["track", "two"]);
    // Two real branches whose derived keys differ only in case. Lowercasing the
    // slug merged them, and the upsert then let each overwrite the other's git
    // facts on every regroup.
    a.sql(
        "INSERT INTO streams(id,name,key,repo,host,branch,worktree,created_at,dirty,ahead)
         VALUES('r#Feature-X','r Feature-X','r#Feature-X','r','github.com','Feature-X','',1,1,0),
               ('r#feature-x','r feature-x','r#feature-x','r','github.com','feature-x','',1,0,3);
         INSERT INTO stream_members(stream_id,context_id,role,origin)
         VALUES('r#Feature-X','one','pane','manual'),('r#feature-x','two','pane','manual');",
    );
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let upper = streams.iter().find(|s| s["id"] == "r#Feature-X").unwrap();
    let lower = streams.iter().find(|s| s["id"] == "r#feature-x").unwrap();
    assert_eq!(upper["members"][0]["id"], "one");
    assert_eq!(lower["members"][0]["id"], "two");
    // Their git facts stay their own rather than overwriting each other.
    assert_eq!(upper["dirty"], true);
    assert_eq!(lower["ahead"], 3);
    // A case-insensitive lookup still reaches one of them rather than failing.
    assert!(a.run(&["stream", "pin", "r#Feature-X"])["action"] == "pin");
}

#[test]
fn a_printed_row_number_is_the_one_focus_resolves() {
    let a = App::new();
    for id in ["alpha", "beta", "gamma"] {
        a.run(&["track", id]);
        a.run(&["group", id, "--to", id]);
    }
    // A stale ranking must not renumber a differently sorted view.
    a.sql("UPDATE streams SET ordinal=3 WHERE id='alpha'; UPDATE streams SET ordinal=1 WHERE id='gamma';");
    for mode in ["active", "debt", "ranked"] {
        let streams = a.run(&["queue", "--mode", mode, "--limit", "200"])["streams"]
            .as_array()
            .unwrap()
            .clone();
        for (i, s) in streams.iter().enumerate() {
            let out = a
                .command()
                .arg("--json")
                .args(["focus", &(i + 1).to_string(), "--mode", mode, "--all"])
                .output()
                .unwrap();
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            // Focus itself needs a real pane; what matters is which stream it
            // resolved the number to, which the error names either way.
            let named = v["data"]["stream"].as_str().unwrap_or_default().to_string()
                + v["error"]["message"].as_str().unwrap_or_default();
            assert!(
                named.contains(s["name"].as_str().unwrap()),
                "row {} in {mode} resolved to something else: {named}",
                i + 1
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn an_unreadable_browser_never_deletes_the_tabs_it_cannot_see() {
    use std::os::unix::fs::PermissionsExt;
    let a = App::new();
    let mock = a.root.join("osascript");
    fs::write(&mock, "#!/bin/sh\ncat \"$STEWSH_TEST_TABS\"\n").unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = a.root.join("tabs.json");
    let path = format!(
        "{}:{}",
        a.root.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let tabs = || {
        a.command()
            .args(["--json", "tabs"])
            .env("PATH", &path)
            .env("STEWSH_TEST_TABS", &fixture)
            .output()
            .unwrap()
    };
    // Chrome has the tab; Safari is not running.
    fs::write(
        &fixture,
        r#"{"browsers":[{"name":"Google Chrome","ok":true,"reason":"","tabs":[
           {"browser":"Google Chrome","url":"https://github.com/acme/thing/pull/1",
            "title":"PR 1","location":"W1"}]},
           {"name":"Safari","ok":false,"reason":"not running","tabs":[]}]}"#,
    )
    .unwrap();
    assert!(tabs().status.success());
    // A Safari tab the user had captured a note on, from an earlier run.
    a.sql(
        "INSERT INTO sessions(id,cwd,creation_time,last_active_interaction,kind,source,agent,name,note,availability)
         VALUES('tab:safari1','',1,1,'tab','browser','Safari','Design doc','come back to this','open');",
    );
    assert!(tabs().status.success());
    let kept = a.show("tab:safari1");
    assert_eq!(kept["note"], "come back to this", "a note was destroyed");
    assert_eq!(
        kept["availability"], "open",
        "an unreadable browser closed its tabs"
    );

    // Now Safari is readable and genuinely has no such tab: closed, not erased.
    fs::write(
        &fixture,
        r#"{"browsers":[{"name":"Google Chrome","ok":true,"reason":"","tabs":[]},
           {"name":"Safari","ok":true,"reason":"","tabs":[]}]}"#,
    )
    .unwrap();
    assert!(tabs().status.success());
    let gone = a.show("tab:safari1");
    assert_eq!(gone["availability"], "closed");
    assert_eq!(
        gone["note"], "come back to this",
        "closing must not erase the note"
    );

    // Every browser unreadable is an error, not an empty authoritative snapshot.
    fs::write(
        &fixture,
        r#"{"browsers":[{"name":"Google Chrome","ok":false,"reason":"not running","tabs":[]},
           {"name":"Safari","ok":false,"reason":"not running","tabs":[]}]}"#,
    )
    .unwrap();
    assert!(!tabs().status.success());
}

/// `stewsh_whoami` is the primitive the human surface never needed: an agent
/// asking where it is. The ladder must say how it resolved, and must refuse to
/// break a tie rather than file a handoff into a sibling's context.
#[test]
fn whoami_says_how_it_resolved_and_refuses_to_guess_between_siblings() {
    let a = App::new();
    let work = a.root.join("work");
    fs::create_dir_all(&work).unwrap();
    a.run_in(&work, &["track", "pane-one"]);

    // One recorded session in this directory is enough to name the context.
    let me = a.run_in(&work, &["whoami"]);
    assert_eq!(me["via"], "cwd");
    assert_eq!(me["context"]["id"], "pane-one");
    assert!(me["hint"].is_null(), "a resolved identity needs no hint");

    // Two is not: the tie is reported, never broken.
    a.run_in(&work, &["track", "pane-two"]);
    let me = a.run_in(&work, &["whoami"]);
    assert!(me["context"].is_null());
    assert_eq!(me["ambiguous"].as_array().unwrap().len(), 2);
    assert!(me["hint"].as_str().unwrap().contains("pass session_id"));
    // The stream is still resolvable from the directory even when the context
    // is not, which is the common case for an agent harness.
    assert!(me["stream"]["stream"].as_str().unwrap().contains("work"));

    // An explicit ID outranks everything and names its siblings.
    let me = a.run_in(&work, &["whoami", "--session-id", "pane-two"]);
    assert_eq!(me["via"], "argument");
    assert_eq!(me["context"]["id"], "pane-two");
    assert_eq!(me["siblings"][0]["id"], "pane-one");

    // A shell hook's session ID is authoritative before its first write, so a
    // declared-but-unrecorded identity is still writable, not an error.
    let out = a
        .command()
        .current_dir(&work)
        .arg("--json")
        .args(["whoami"])
        .env("STEWSH_SESSION_ID", "brand-new")
        .output()
        .unwrap();
    let me: Value = serde_json::from_slice(&out.stdout).unwrap();
    let me = &me["data"];
    assert_eq!(me["via"], "stewsh_session_id");
    assert_eq!(me["declared"], "brand-new");
    assert!(me["context"].is_null());
    assert!(me["hint"].is_null());
}

/// The brief is the one payload with a budget, because an agent pays for every
/// field it reads. It must fit, and it must say what fitting cost.
#[test]
fn a_brief_fits_its_budget_and_names_what_it_dropped() {
    let a = App::new();
    let note = "Read the migration, decide whether the backfill runs before or \
                after the cutover, then tell the other session which it is.";
    for i in 0..14 {
        let id = format!("ctx-{i}");
        a.run(&["track", &id]);
        a.run(&["capture", note, "--session-id", &id]);
    }
    // Distinct directories make distinct streams; nothing has assembled yet, so
    // the first brief's self-healing regroup files them by these values.
    a.sql("UPDATE sessions SET cwd='/tmp/stewsh-work-'||id");

    let full = a.run(&["brief", "--mode", "debt", "--budget", "20000"]);
    assert_eq!(full["counts"]["streams"], 14);
    assert_eq!(full["omitted"]["streams"], 0);
    assert_eq!(full["queue"].as_array().unwrap().len(), 14);

    // Breadth is paid for out of detail: every stream keeps its row while the
    // member rosters go, and only a budget too small for the bare listing
    // starts dropping streams.
    let thinned = a.run(&["brief", "--mode", "debt", "--budget", "1200"]);
    assert_eq!(thinned["omitted"]["streams"], 0);
    assert_eq!(thinned["queue"].as_array().unwrap().len(), 14);
    assert!(thinned["omitted"]["members"].as_u64().unwrap() > 0);
    assert!(thinned["estimated_tokens"].as_u64().unwrap() <= 1200);
    assert!(
        thinned["estimated_tokens"].as_u64().unwrap()
            > full["estimated_tokens"].as_u64().unwrap() / 4,
        "a budget should be spent, not merely respected"
    );

    let small = a.run(&["brief", "--mode", "debt", "--budget", "400"]);
    let tokens = small["estimated_tokens"].as_u64().unwrap();
    assert!(tokens <= 400, "brief overran its budget at {tokens} tokens");
    assert!(small["omitted"]["streams"].as_u64().unwrap() > 0);
    // Still an answer, not a truncated one: the count is of the whole desk.
    assert_eq!(small["counts"]["streams"], 14);
    assert!(small["queue"].as_array().unwrap().len() < 14);
    // Shrinking drops detail in a stated order, never the leading stream.
    assert_eq!(small["queue"][0]["n"], 1);
    assert_eq!(small["queue"][0]["stream"], full["queue"][0]["stream"]);
}

/// The verdict an agent actually wants, and the distinction the scores alone
/// cannot express: a dirty tree is unfinished, not a question.
#[test]
fn the_agent_surface_separates_asking_for_a_human_from_merely_unfinished() {
    let a = App::new();
    a.run(&["track", "quiet"]);
    a.run(&["track", "broken", "--exit-code", "1"]);
    a.sql("UPDATE sessions SET cwd='/tmp/stewsh-'||id");
    let of = |brief: &Value, key: &str| -> Value {
        brief["queue"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["stream"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("no stream {key}"))
    };

    let brief = a.run(&["brief", "--mode", "debt"]);
    assert_eq!(brief["counts"]["needs_human"], 1);
    let broken = of(&brief, "dir:/tmp/stewsh-broken");
    assert_eq!(broken["needs_human"], true);
    assert_eq!(broken["requests"][0], "failed");
    let quiet = of(&brief, "dir:/tmp/stewsh-quiet");
    assert_eq!(quiet["needs_human"], false);
    assert!(quiet["requests"].is_null(), "no codes means no request");

    // Repo evidence raises debt without turning the stream into a question.
    a.sql("UPDATE streams SET dirty=1,ahead=3 WHERE key='dir:/tmp/stewsh-quiet'");
    let brief = a.run(&["brief", "--mode", "debt"]);
    let quiet = of(&brief, "dir:/tmp/stewsh-quiet");
    assert_eq!(quiet["needs_human"], false);
    assert!(quiet["debt"].as_i64().unwrap() > 0);
    assert_eq!(quiet["git"]["dirty"], true);
    assert_eq!(quiet["git"]["ahead"], 3);

    // A saved next action is a question; a captured note is the human's own ask.
    a.run(&[
        "capture",
        "Decide the cutover order",
        "--session-id",
        "quiet",
    ]);
    let brief = a.run(&["brief", "--mode", "debt"]);
    assert_eq!(of(&brief, "dir:/tmp/stewsh-quiet")["needs_human"], true);
    assert_eq!(brief["counts"]["needs_human"], 2);
}

/// The MCP surface is a protocol adapter and nothing more: it must write
/// through the same action layer the CLI does, and land in the same store.
#[test]
fn mcp_speaks_json_rpc_over_stdio_and_writes_through_the_same_actions() {
    let a = App::new();
    a.run(&["track", "claude:in-session"]);
    let replies = a.mcp(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"stewsh_report","arguments":{"state":"waiting","summary":"Need the cutover order","agent":"claude","session_id":"claude:in-session","event_id":"turn-1"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"stewsh_report","arguments":{"state":"ready","summary":"Different text, same event","session_id":"claude:in-session","event_id":"turn-1"}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"stewsh_capture","arguments":{"note":"Review the backfill","session_id":"claude:in-session"}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"stewsh_brief","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"stewsh_report","arguments":{"state":"nonsense"}}}"#,
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"stewsh_capture","arguments":{"note":"whose context?"}}}"#,
        r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#,
        "definitely not json",
    ]);

    // A notification is acted on and never answered: eleven lines in, and the
    // nine requests carrying an ID plus the unparseable line make ten replies.
    assert_eq!(replies.len(), 10, "{replies:#?}");
    let by_id = |id: i64| -> Value {
        replies
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no reply for {id}"))
    };
    let content = |id: i64| -> String {
        by_id(id)["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let init = by_id(1);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "stewsh");
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("stewsh_whoami"));

    let listed = by_id(2);
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "stewsh_whoami",
            "stewsh_brief",
            "stewsh_stream",
            "stewsh_capture",
            "stewsh_report"
        ]
    );
    // Triage stays the human's: no tool may focus, pin, snooze, resolve or rank.
    for forbidden in ["focus", "pin", "snooze", "resolve", "rank", "review"] {
        assert!(
            !names.iter().any(|n| n.contains(forbidden)),
            "the agent surface must not expose {forbidden}"
        );
    }

    assert!(content(3).contains("Agent report recorded"));
    // Same event ID, different text: recorded once, exactly as the CLI does.
    assert!(content(4).contains("already recorded"));
    let s = a.show("claude:in-session");
    assert_eq!(s["state"], "waiting");
    assert_eq!(s["state_source"], "agent");
    assert_eq!(s["handoff"], "Need the cutover order");
    assert_eq!(s["note"], "Review the backfill");

    // A structured mirror of the text, so a client need not re-parse it.
    let brief = &by_id(6)["result"]["structuredContent"];
    assert_eq!(brief["counts"]["needs_human"], 1);
    assert!(brief["estimated_tokens"].as_u64().unwrap() > 0);

    // A tool failure is readable content, not a dropped connection.
    assert_eq!(by_id(7)["result"]["isError"], true);
    assert!(content(7).contains("unknown state"));
    assert_eq!(by_id(8)["result"]["isError"], true);
    assert!(content(8).contains("session_id"));
    // Protocol-level faults stay protocol-level.
    assert_eq!(by_id(9)["error"]["code"], -32601);
    assert_eq!(
        replies.iter().find(|r| r["id"].is_null()).unwrap()["error"]["code"],
        -32700
    );
}

/// The MCP tool's `state` enum and the CLI's accepted values are one list in
/// the source. This is the gate that keeps them one list in practice.
#[test]
fn the_reported_states_are_the_same_list_on_both_surfaces() {
    let a = App::new();
    let replies = a.mcp(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#]);
    let tools = replies[0]["result"]["tools"].as_array().unwrap();
    let report = tools.iter().find(|t| t["name"] == "stewsh_report").unwrap();
    let states: Vec<String> = report["inputSchema"]["properties"]["state"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(states.len() >= 6, "{states:?}");
    for state in &states {
        a.run(&["report", state, "--session-id", "probe"]);
        assert_eq!(a.show("probe")["state"], state.as_str());
    }
    a.fail(&["report", "sideways", "--session-id", "probe"]);
}
