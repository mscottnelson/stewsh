use chrono::{Local, TimeZone};
use serde::Serialize;
use serde_json::Value;

/// Activity heat halves every 90 minutes: recent work leads the active queue
/// without erasing yesterday's unfinished work, which `debt` still carries.
/// Heat is the sum of decayed events plus a decayed baseline for the context's
/// last activity, so a live session outranks a stale one even between syncs.
pub const HALF_LIFE_SECS: f64 = 5400.0;
/// Beyond this the decayed contribution is below one part in 65,000.
pub const HEAT_WINDOW_SECS: i64 = 86_400;

#[derive(Debug, Clone, Serialize)]
pub struct Context {
    pub id: String,
    pub cwd: String,
    pub command: Option<String>,
    pub note: Option<String>,
    pub exit_code: Option<i32>,
    pub created_at: i64,
    pub last_active: i64,
    pub name: String,
    pub source: String,
    pub availability: String,
    pub state: String,
    pub state_source: String,
    pub agent: String,
    pub handoff: Option<String>,
    pub pinned: bool,
    pub snoozed_until: i64,
    pub resolved: bool,
    pub revision: i64,
    pub reviewed_revision: i64,
    pub reviewed_at: Option<i64>,
    pub observed_at: Option<i64>,
    pub native_id: Option<String>,
    pub location: String,
    pub age_source: String,
    pub kind: String,
    pub repo: String,
    pub branch: String,
    pub worktree: String,
    pub external_ref: Option<String>,
    pub heat: f64,
    pub score: i32,
    pub reasons: Vec<Reason>,
    pub older: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reason {
    pub points: i32,
    pub code: &'static str,
    pub text: &'static str,
}

impl Context {
    /// Debt: how unfinished this is, independent of when it last moved.
    pub fn rank(&mut self, now: i64) {
        self.score = 0;
        self.reasons.clear();
        if self.kind == "tab" {
            // A browser tab is context to return to, not evidence of activity.
            self.heat = 0.0;
            return;
        }
        let today = Local.timestamp_opt(now, 0).single().map(|d| d.date_naive());
        let started = Local
            .timestamp_opt(self.created_at, 0)
            .single()
            .map(|d| d.date_naive());
        self.older = matches!((started, today), (Some(a), Some(b)) if a < b);
        let mut add = |points: i32, code: &'static str, text: &'static str| {
            self.score += points;
            self.reasons.push(Reason { points, code, text });
        };
        if self.pinned {
            add(100, "pinned", "Pinned by you");
        }
        if self.note.as_ref().is_some_and(|n| !n.is_empty()) {
            add(50, "next_action", "Saved next action");
        }
        match self.state.as_str() {
            "waiting" => add(45, "waiting", "Input may be needed"),
            "failed" => add(35, "failed", "Failure reported or suggested"),
            "ready" => add(25, "ready", "Ready for review"),
            "working" => add(-10, "working", "Work in progress"),
            _ => {}
        }
        if self.older && self.reviewed_revision < self.revision {
            add(30, "older", "Unreviewed work from before today");
        }
        if self.reviewed_revision == 0 {
            add(10, "never_reviewed", "Never reviewed");
        } else if self.reviewed_revision < self.revision {
            add(20, "changed", "Changed since review");
        } else {
            add(-60, "reviewed", "Reviewed; no new revision");
        }
        self.score = self.score.max(0);
    }

    pub fn queued(&self, now: i64) -> bool {
        !self.resolved && self.snoozed_until <= now && self.availability != "closed"
    }
}

/// A work stream: the unit the user actually prioritizes and focuses.
#[derive(Debug, Clone, Serialize)]
pub struct Stream {
    pub id: String,
    pub name: String,
    pub key: String,
    pub repo: String,
    pub host: String,
    pub branch: String,
    pub worktree: String,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: i64,
    pub dirty: bool,
    pub ahead: i64,
    pub pr: Option<Value>,
    pub ordinal: Option<i64>,
    pub why: Option<String>,
    pub next: Option<String>,
    pub confidence: Option<f64>,
    pub ranked_at: Option<i64>,
    pub members: Vec<Context>,
    pub heat: f64,
    pub heat_points: i32,
    pub debt: i32,
    pub score: i32,
    pub state: String,
    pub last_active: i64,
    pub reasons: Vec<Reason>,
}

impl Stream {
    /// Active score is heat-led with debt underneath; the debt view sorts on
    /// `debt` alone. Both are deterministic; the ranker only reorders.
    pub fn roll_up(&mut self, now: i64) {
        self.members.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.last_active.cmp(&a.last_active))
        });
        let live: Vec<&Context> = self
            .members
            .iter()
            .filter(|m| m.kind != "tab" && m.queued(now))
            .collect();
        // Hottest member leads and each next one counts half as much, so the
        // total converges to twice the hottest. Breadth can never saturate the
        // clamp on its own; only genuinely hot work gets there.
        let mut heats: Vec<f64> = live.iter().map(|m| m.heat).collect();
        heats.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        self.heat = heats
            .iter()
            .enumerate()
            .map(|(i, h)| h * 0.5f64.powi(i as i32))
            .sum();
        self.debt = live.iter().map(|m| m.score).max().unwrap_or(0);
        self.last_active = self
            .members
            .iter()
            .map(|m| m.last_active)
            .max()
            .unwrap_or(0);
        self.heat_points = ((self.heat * 45.0).round() as i32).clamp(0, 150);
        self.state = live
            .iter()
            .map(|m| m.state.as_str())
            .min_by_key(|s| match *s {
                "waiting" => 0,
                "failed" => 1,
                "ready" => 2,
                "working" => 3,
                "idle" => 4,
                _ => 5,
            })
            .unwrap_or("unknown")
            .to_string();
        self.reasons.clear();
        if self.heat_points > 0 {
            self.reasons.push(Reason {
                points: self.heat_points,
                code: "heat",
                text: "Recent activity",
            });
        }
        if self.debt > 0 {
            self.reasons.push(Reason {
                points: self.debt,
                code: "debt",
                text: "Unfinished work",
            });
        }
        if self.pinned {
            self.reasons.push(Reason {
                points: 100,
                code: "stream_pinned",
                text: "Stream pinned by you",
            });
        }
        // Repo evidence stands on its own: a worktree can be unfinished with no
        // pane open in it at all.
        let mut git_points = 0;
        let mut git = |points: i32, code: &'static str, text: &'static str| {
            git_points += points;
            self.reasons.push(Reason { points, code, text });
        };
        if self.dirty {
            git(10, "dirty", "Uncommitted changes");
        }
        if self.ahead > 0 {
            git(
                5 * self.ahead.clamp(1, 4) as i32,
                "ahead",
                "Commits not pushed",
            );
        }
        match self.pr.as_ref().and_then(|p| p["checks"].as_str()) {
            Some("failing") => git(40, "pr_failing", "Pull request checks failing"),
            Some("pending") => git(5, "pr_pending", "Pull request checks running"),
            _ => {}
        }
        self.score = self.heat_points + self.debt + i32::from(self.pinned) * 100 + git_points;
        self.debt += git_points;
    }

    pub fn queued(&self, now: i64) -> bool {
        // An uncommitted or unpushed worktree is unfinished work whether or not
        // any pane is still open on it, so this must not depend on membership.
        !self.archived
            && (self.dirty
                || self.ahead > 0
                || self
                    .members
                    .iter()
                    .any(|m| m.kind != "tab" && m.queued(now)))
    }
}

pub fn event_weight(kind: &str) -> f64 {
    match kind {
        // Deliberate user acts are the strongest evidence of current intent.
        "captured" | "focused" => 1.5,
        "tracked" => 1.0,
        "reported" | "harness" => 0.8,
        "reviewed" => 0.3,
        // A changed screen may only be a spinner.
        "observed" => 0.15,
        _ => 0.2,
    }
}

pub fn decay(now: i64, at: i64) -> f64 {
    0.5f64.powf((now - at).max(0) as f64 / HALF_LIFE_SECS)
}

pub fn clean(text: &str, width: usize) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                c
            }
        })
        .take(width)
        .collect()
}

/// Case is preserved: `repo#Feature-X` and `repo#feature-x` are different
/// branches and must not collapse into one stream. Lookup is case-insensitive,
/// so nothing downstream needs the lowercasing this used to do.
pub fn slug(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '#' | '-' | '_' | '.' | '/') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "stream".into()
    } else {
        s.chars().take(120).collect()
    }
}
