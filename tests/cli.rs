use std::{fs, process::Command};

#[test]
fn persistent_triage_and_failure_recovery() {
    let root = std::env::temp_dir().join(format!("stewsh-test-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let db = root.join("state.db");
    let run = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_stewsh"))
            .arg("--db")
            .arg(&db)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    assert!(run(&["next"]).contains("No tracked"));
    run(&["track", "normal"]);
    run(&["track", "failed", "--command", "false", "--exit-code", "1"]);
    assert!(run(&["next"]).contains("failed"));
    run(&["capture", "Fix the deployment", "--session-id", "manual"]);
    run(&["track", "manual", "--exit-code", "0"]);
    assert!(run(&["next"]).contains("Fix the deployment"));
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE sessions SET last_active_interaction=0 WHERE id='manual'",
        [],
    )
    .unwrap();
    assert!(run(&["next"]).contains("failed"));
    run(&["track", "failed", "--command", "true", "--exit-code", "0"]);
    let priority: f64 = conn
        .query_row(
            "SELECT base_priority FROM sessions WHERE id='failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(priority, 1.0);
    assert_eq!(run(&["triage", "--limit", "1"]).lines().count(), 2);
    let bad = Command::new(env!("CARGO_BIN_EXE_stewsh"))
        .arg("--db")
        .arg(&db)
        .args(["capture", "", "--session-id", "manual"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    drop(conn);
    fs::remove_dir_all(root).unwrap();
}
