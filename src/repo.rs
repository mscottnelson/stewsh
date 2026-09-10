//! Git context. The cheapest high-signal evidence we have: a branch with a
//! failing check and unpushed commits outranks a clean idle pane, and proving
//! that needs no model.
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Default)]
pub struct RepoInfo {
    pub toplevel: String,
    pub common_dir: String,
    pub repo: String,
    /// Host of the `origin` remote, so a tab can be matched against the place
    /// this repository actually lives rather than any site that names it.
    pub host: String,
    pub branch: String,
    /// Set only when this checkout is a linked worktree, not the main one.
    pub worktree: String,
    pub dirty: bool,
    pub ahead: i64,
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", dir])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[derive(Default)]
pub struct Git {
    cache: HashMap<String, Option<RepoInfo>>,
    prs: HashMap<String, Option<Value>>,
    pub with_pr: bool,
}

impl Git {
    pub fn new(with_pr: bool) -> Self {
        Self {
            with_pr,
            ..Default::default()
        }
    }

    pub fn info(&mut self, cwd: &str) -> Option<RepoInfo> {
        if cwd.is_empty() || !Path::new(cwd).is_dir() {
            return None;
        }
        if let Some(hit) = self.cache.get(cwd) {
            return hit.clone();
        }
        let value = self.probe(cwd);
        self.cache.insert(cwd.to_string(), value.clone());
        value
    }

    fn probe(&self, cwd: &str) -> Option<RepoInfo> {
        let toplevel = git(cwd, &["rev-parse", "--show-toplevel"])?;
        let common_dir = git(cwd, &["rev-parse", "--absolute-git-dir"])
            .and_then(|d| {
                git(
                    cwd,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .or(Some(d))
            })
            .unwrap_or_default();
        let git_dir = git(cwd, &["rev-parse", "--absolute-git-dir"]).unwrap_or_default();
        let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|b| b != "HEAD")
            .or_else(|| git(cwd, &["rev-parse", "--short", "HEAD"]).map(|s| format!("@{s}")))
            .unwrap_or_else(|| "unknown".into());
        let remote = git(cwd, &["remote", "get-url", "origin"]);
        let repo = remote
            .as_deref()
            .map(|url| {
                url.trim_end_matches('/')
                    .trim_end_matches(".git")
                    .rsplit(['/', ':'])
                    .next()
                    .unwrap_or("repo")
                    .to_string()
            })
            .unwrap_or_else(|| basename(&toplevel));
        let host = remote.as_deref().map(remote_host).unwrap_or_default();
        Some(RepoInfo {
            dirty: git(cwd, &["status", "--porcelain", "--untracked-files=no"]).is_some(),
            ahead: git(cwd, &["rev-list", "--count", "@{u}..HEAD"])
                .and_then(|c| c.parse().ok())
                .unwrap_or(0),
            worktree: if git_dir != common_dir {
                toplevel.clone()
            } else {
                String::new()
            },
            toplevel,
            common_dir,
            repo,
            host,
            branch,
        })
    }

    /// Open pull request for the branch checked out at `cwd`. Network call via
    /// `gh`, so it is opt-in and cached per checkout.
    pub fn pr(&mut self, cwd: &str) -> Option<Value> {
        if !self.with_pr {
            return None;
        }
        if let Some(hit) = self.prs.get(cwd) {
            return hit.clone();
        }
        let value = Command::new("gh")
            .args([
                "pr",
                "view",
                "--json",
                "number,state,isDraft,url,statusCheckRollup",
            ])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
            .map(|v| {
                let checks = v["statusCheckRollup"].as_array().cloned().unwrap_or_default();
                let count = |want: &str| {
                    checks
                        .iter()
                        .filter(|c| {
                            c["conclusion"].as_str().unwrap_or(c["state"].as_str().unwrap_or(""))
                                == want
                        })
                        .count()
                };
                let failing = count("FAILURE") + count("TIMED_OUT") + count("CANCELLED");
                let pending = checks
                    .iter()
                    .filter(|c| {
                        matches!(
                            c["status"].as_str().unwrap_or(""),
                            "IN_PROGRESS" | "QUEUED" | "PENDING"
                        )
                    })
                    .count();
                json!({
                    "number": v["number"], "state": v["state"], "draft": v["isDraft"],
                    "url": v["url"], "failing": failing, "pending": pending,
                    "checks": if failing > 0 { "failing" } else if pending > 0 { "pending" } else { "passing" },
                })
            });
        self.prs.insert(cwd.to_string(), value.clone());
        value
    }

    /// Every checkout of every repo we have seen, including worktrees with no
    /// open pane. Unfinished work with zero terminal evidence is still work.
    pub fn worktrees(&mut self, seeds: &[String]) -> Vec<RepoInfo> {
        let mut seen = HashMap::new();
        for seed in seeds {
            let Some(base) = self.info(seed) else {
                continue;
            };
            if seen.contains_key(&base.common_dir) {
                continue;
            }
            let listing = git(&base.toplevel, &["worktree", "list", "--porcelain"]);
            seen.insert(base.common_dir.clone(), ());
            let Some(listing) = listing else { continue };
            for block in listing.split("\n\n") {
                if let Some(path) = block
                    .lines()
                    .find_map(|l| l.strip_prefix("worktree "))
                    .filter(|p| Path::new(p).is_dir())
                {
                    self.info(path);
                }
            }
        }
        let mut out: Vec<RepoInfo> = self.cache.values().flatten().cloned().collect();
        out.sort_by(|a, b| a.toplevel.cmp(&b.toplevel));
        out.dedup_by(|a, b| a.toplevel == b.toplevel);
        out
    }
}

/// `git@github.com:acme/x.git` and `https://github.com/acme/x` both give
/// `github.com`.
pub fn remote_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let after_user = after_scheme
        .rsplit_once('@')
        .map(|(_, r)| r)
        .unwrap_or(after_scheme);
    after_user
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

pub fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// The stream a checkout belongs to. Branch is the work stream most of the time.
pub fn key_for(info: Option<&RepoInfo>, cwd: &str) -> (String, String) {
    match info {
        Some(i) => (
            format!("{}#{}", i.repo, i.branch),
            format!("{} · {}", i.repo, i.branch),
        ),
        None if !cwd.is_empty() => (format!("dir:{cwd}"), basename(cwd)),
        None => ("unassigned".into(), "Unassigned".into()),
    }
}
