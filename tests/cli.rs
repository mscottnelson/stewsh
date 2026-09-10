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
        c
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
    a.run(&["stream", "pin", "alpha"]);
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let alpha = streams.iter().find(|s| s["id"] == "alpha").unwrap();
    assert_eq!(alpha["pinned"], true);
    assert!(alpha["score"].as_i64().unwrap() >= 100);
    a.run(&["stream", "archive", "alpha"]);
    let visible = a.run(&["queue"])["streams"].as_array().unwrap().clone();
    assert!(visible.iter().all(|s| s["id"] != "alpha"));
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
    let _ = child.kill();
    let _ = child.wait();
    assert!(String::from_utf8_lossy(&page.stdout).contains("StewardShell"));
    let v: Value = serde_json::from_slice(&queue.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["streams"][0]["members"][0]["id"], "one");
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
         VALUES('alpha','tab:abc','tab','auto');",
    );
    a.run(&["track", "pane-two"]); // forces a regroup on the next assemble
    let streams = a.run(&["queue", "--all", "--limit", "200"])["streams"]
        .as_array()
        .unwrap()
        .clone();
    let alpha = streams.iter().find(|s| s["id"] == "alpha").unwrap();
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
