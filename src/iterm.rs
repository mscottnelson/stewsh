use crate::{store, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Pane {
    id: String,
    name: String,
    tty: String,
    cwd: String,
    text: String,
    prompt: bool,
    location: String,
}

pub fn script(source: &str, args: &[&str]) -> Result<String> {
    if !cfg!(target_os = "macos") {
        return Err(
            "iTerm2 integration requires macOS; shell tracking and agent reports work here".into(),
        );
    }
    let mut child = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", source])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(16 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() > Duration::from_secs(25) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = out.join().map_err(|_| "collector output reader failed")??;
    let stderr = err.join().map_err(|_| "collector error reader failed")??;
    let status = status.ok_or("iTerm2 collection timed out after 25s; prior snapshot preserved")?;
    if !status.success() {
        return Err(format!(
            "iTerm2: {}",
            crate::model::clean(&String::from_utf8_lossy(&stderr), 500)
        )
        .into());
    }
    Ok(String::from_utf8(stdout)?)
}

fn age(value: &str) -> Option<i64> {
    let (days, clock) = value
        .split_once('-')
        .map_or((0, value), |(d, c)| (d.parse::<i64>().unwrap_or(0), c));
    let mut seconds = days * 86400;
    for (i, p) in clock.split(':').rev().enumerate() {
        seconds += p.parse::<i64>().ok()? * 60_i64.pow(i as u32);
    }
    Some(seconds)
}

fn processes() -> HashMap<String, Vec<(String, i64)>> {
    let mut result = HashMap::<String, Vec<(String, i64)>>::new();
    if let Ok(out) = Command::new("ps")
        .args(["-axo", "tty=,etime=,comm="])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut p = line.split_whitespace();
            if let (Some(tty), Some(time), Some(cmd)) = (p.next(), p.next(), p.next()) {
                let name = cmd.rsplit('/').next().unwrap_or(cmd).to_string();
                result
                    .entry(format!("/dev/{tty}"))
                    .or_default()
                    .push((name, age(time).unwrap_or(0)));
            }
        }
    }
    result
}

fn signal(text: &str, prompt: bool) -> &'static str {
    let tail = text
        .lines()
        .rev()
        .filter(|s| !s.trim().is_empty())
        .take(12)
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    if [
        "[y/n]",
        "(y/n)",
        "allow once",
        "approval required",
        "do you want to",
        "would you like",
        "press enter",
        "permission required",
    ]
    .iter()
    .any(|p| tail.contains(p))
    {
        "waiting"
    } else if [
        "error:",
        "failed",
        "traceback",
        "permission denied",
        "fatal:",
    ]
    .iter()
    .any(|p| tail.contains(p))
    {
        "failed"
    } else if prompt {
        "idle"
    } else {
        "unknown"
    }
}

pub fn sync(conn: &mut Connection, now: i64) -> Result<Value> {
    let panes: Vec<Pane> = serde_json::from_str(&script(include_str!("iterm-collect.js"), &[])?)?;
    let procs = processes();
    let tx = conn.transaction()?;
    let mut ids = Vec::new();
    for pane in &panes {
        let id = format!("iterm:{}", pane.id);
        ids.push(id.clone());
        store::ensure(&tx, &id, &pane.cwd, now)?;
        let (fingerprint, old_state, old_state_source, old_availability): (
            String,
            String,
            String,
            String,
        ) = tx.query_row(
            "SELECT fingerprint,state,state_source,availability FROM sessions WHERE id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let digest = format!("{:x}", Sha256::digest(pane.text.trim().as_bytes()));
        let first_sight = fingerprint.is_empty();
        let changed = fingerprint != digest;
        // Discovering a pane is not the same as the pane being active. On first
        // sight we fall back to the shell's own age rather than claiming it just
        // moved, which would make every pane maximally hot on the first sync.
        let moved = changed && !first_sight;
        let ps = procs.get(&pane.tty).cloned().unwrap_or_default();
        let agents: Vec<_> = ["claude", "codex", "devin"]
            .into_iter()
            .filter(|a| ps.iter().any(|(p, _)| p.to_lowercase().contains(a)))
            .collect();
        let agent = if agents.len() == 1 {
            agents[0]
        } else if agents.len() > 1 {
            "multiple"
        } else {
            "terminal"
        };
        let shell_age = ps
            .iter()
            .filter(|(p, _)| {
                matches!(
                    p.as_str(),
                    "zsh" | "-zsh" | "bash" | "-bash" | "fish" | "sh"
                )
            })
            .map(|(_, a)| *a)
            .max();
        // Screen evidence is weak, but a pane visibly asking a question or
        // showing a failure is worth more than a stale shell state. The zsh
        // hook marks a pane `working` on every command, so without this an
        // agent's prompt would never surface in the very setup we recommend.
        // Only an explicit agent report keeps its veto.
        let observed = signal(&pane.text, pane.prompt);
        let weak = old_state_source == "screen" || old_state == "unknown";
        let overrides = old_state_source != "agent" && matches!(observed, "waiting" | "failed");
        let (state, state_source) = if weak || overrides {
            (observed, "screen")
        } else {
            (old_state.as_str(), old_state_source.as_str())
        };
        // A changed screen is activity, not a new revision: a spinner, a clock
        // or a log tail would otherwise un-review the pane on every sync and
        // make `review` useless on exactly the panes that matter. Only a
        // changed signal category or a reopened pane counts as a revision.
        let semantic = state != old_state || old_availability != "open";
        tx.execute("UPDATE sessions SET name=?2,source='iterm',native_id=?3,location=?4,availability='open',observed_at=?5,
          cwd=CASE WHEN ?6!='' THEN ?6 ELSE cwd END,agent=CASE WHEN state_source='agent' THEN agent ELSE ?7 END,
          fingerprint=?8,state=?9,state_source=?10,
          revision=revision+?11,
          last_active_interaction=CASE WHEN ?13=1 THEN ?5
            WHEN ?14=1 AND ?12 IS NOT NULL THEN MIN(last_active_interaction,?5-?12)
            ELSE last_active_interaction END,
          creation_time=CASE WHEN ?12 IS NOT NULL THEN MIN(creation_time,?5-?12) ELSE creation_time END,
          age_source=CASE WHEN ?12 IS NOT NULL THEN 'Local shell age estimate' ELSE age_source END WHERE id=?1",
          params![id,pane.name,pane.id,pane.location,now,pane.cwd,agent,digest,state,state_source,
                  i32::from(semantic),shell_age,i32::from(moved),i32::from(first_sight)])?;
        if changed || old_availability != "open" {
            store::event(
                &tx,
                &id,
                now,
                "observed",
                json!({"state":state,"source":state_source,"screen_changed":changed}),
                None,
            )?;
        }
    }
    let existing = store::rows(&tx, now)?;
    let mut closed = 0;
    for row in existing {
        if row.source == "iterm" && row.availability != "closed" && !ids.contains(&row.id) {
            tx.execute(
                "UPDATE sessions SET availability='closed',observed_at=?2 WHERE id=?1",
                params![row.id, now],
            )?;
            store::event(
                &tx,
                &row.id,
                now,
                "closed",
                json!({"reason":"Absent from complete iTerm snapshot"}),
                None,
            )?;
            closed += 1;
        }
    }
    tx.commit()?;
    Ok(json!({"open_panes":panes.len(),"newly_closed":closed,"observed_at":now}))
}

pub fn action(native: &str, action: &str) -> Result<Value> {
    serde_json::from_str(&script(include_str!("iterm-action.js"), &[native, action])?)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn macos_age() {
        assert_eq!(age("2-01:02:03"), Some(176523));
        assert_eq!(age("05:30"), Some(330));
    }
    #[test]
    fn evidence_is_conservative() {
        assert_eq!(signal("Do you want to continue? [y/n]", false), "waiting");
        assert_eq!(signal("hello", false), "unknown");
        assert_eq!(signal("cargo test\nerror: failed", false), "failed");
    }
}
