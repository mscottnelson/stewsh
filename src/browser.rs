//! Browser tabs as stream members. A tab is context you want back when you
//! return to a piece of work, but it carries no activity signal, so it can
//! receive focus and must never drive the queue.
use crate::{iterm::script, model::Stream, store, stream, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Tab {
    browser: String,
    url: String,
    title: String,
    location: String,
}

#[derive(Deserialize)]
struct Report {
    name: String,
    ok: bool,
    reason: String,
    tabs: Vec<Tab>,
}

#[derive(Deserialize)]
struct Snapshot {
    browsers: Vec<Report>,
}

pub fn id_for(url: &str) -> String {
    format!(
        "tab:{:.16}",
        format!("{:x}", Sha256::digest(url.as_bytes()))
    )
}

/// A tab joins a stream only when its URL names a repository StewardShell
/// already knows. Everything else is left alone rather than filling the queue
/// with whatever happens to be open.
fn stream_for<'a>(url: &str, streams: &'a [Stream]) -> Option<&'a Stream> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = after_scheme.split(['?', '#']).next().unwrap_or_default();
    let host = path
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or_else(|| path.split('/').next().unwrap_or_default())
        .to_lowercase();
    let segments: Vec<String> = path
        .split('/')
        .skip(1)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut candidates: Vec<(usize, i32, &Stream)> = streams
        .iter()
        .filter(|s| !s.repo.is_empty() && !s.archived)
        // The tab must be on the host this repository actually lives on. A
        // documentation site that happens to have the repository name in its
        // path is somebody else's website.
        .filter(|s| !s.host.is_empty() && host == s.host.to_lowercase())
        .filter(|s| segments.iter().any(|seg| *seg == s.repo.to_lowercase()))
        .map(|s| {
            // Branches are matched as whole path segments, so `main` no longer
            // matches inside `maintenance`, and `feat/rates` matches as a pair.
            let branch: Vec<String> = s
                .branch
                .to_lowercase()
                .split('/')
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect();
            let depth = if !branch.is_empty()
                && segments.len() >= branch.len()
                && segments
                    .windows(branch.len())
                    .any(|w| w == branch.as_slice())
            {
                branch.len()
            } else {
                0
            };
            (depth, s.debt, s)
        })
        .collect();
    // Longest branch match wins; then the most unfinished stream. Debt rather
    // than score, because score carries heat and would make a repository-only
    // URL drift between streams as the day goes on. Id last, for determinism.
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.id.cmp(&b.2.id))
    });
    candidates.first().map(|(_, _, s)| *s)
}

pub fn sync(conn: &mut Connection, streams: &[Stream], now: i64) -> Result<Value> {
    let snapshot: Snapshot =
        serde_json::from_str(&script(include_str!("browser-collect.js"), &[])?)?;
    let readable: Vec<&Report> = snapshot.browsers.iter().filter(|b| b.ok).collect();
    if readable.is_empty() {
        let why: Vec<String> = snapshot
            .browsers
            .iter()
            .map(|b| format!("{}: {}", b.name, b.reason))
            .collect();
        return Err(format!("no readable browser ({})", why.join("; ")).into());
    }
    let mut attached = 0;
    let mut seen = 0;
    let tx = conn.transaction()?;
    let mut live = Vec::new();
    for tab in readable.iter().flat_map(|b| b.tabs.iter()) {
        seen += 1;
        let id = id_for(&tab.url);
        // Recorded whether or not it matches a stream: it is genuinely open, so
        // it must not be treated as gone on the reconcile below.
        live.push(id.clone());
        store::ensure(&tx, &id, "", now)?;
        let name = if tab.title.is_empty() {
            tab.url.clone()
        } else {
            tab.title.clone()
        };
        tx.execute(
            "UPDATE sessions SET kind='tab',source='browser',agent=?2,name=?3,external_ref=?4,
             location=?5,availability='open',observed_at=?6,state='idle',state_source='browser'
             WHERE id=?1",
            params![id, tab.browser, name, tab.url, tab.location, now],
        )?;
        if let Some(target) = stream_for(&tab.url, streams) {
            stream::assign(&tx, &target.id, &id, "auto")?;
            attached += 1;
        }
    }
    // Reconcile only the browsers we could actually read, and mark rows closed
    // rather than deleting them: a tab may carry a note the user wrote.
    let names: Vec<&str> = readable.iter().map(|b| b.name.as_str()).collect();
    let mut q = tx.prepare(
        "SELECT id,agent FROM sessions WHERE source='browser' AND availability!='closed'",
    )?;
    let known: Vec<(String, String)> = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(q);
    let mut closed = 0;
    for (id, browser) in known
        .iter()
        .filter(|(id, browser)| names.contains(&browser.as_str()) && !live.contains(id))
    {
        let _ = browser;
        tx.execute(
            "UPDATE sessions SET availability='closed',observed_at=?2 WHERE id=?1",
            params![id, now],
        )?;
        closed += 1;
    }
    tx.commit()?;
    Ok(json!({
        "tabs_seen": seen, "attached": attached, "closed": closed,
        "browsers": snapshot.browsers.iter()
            .map(|b| json!({"name": b.name, "ok": b.ok, "reason": b.reason}))
            .collect::<Vec<_>>(),
    }))
}

pub fn focus(url: &str) -> Result<Value> {
    script(include_str!("browser-action.js"), &[url]).and_then(|out| {
        serde_json::from_str(&out).map_err(|e| format!("browser focus failed: {e}").into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::blank;

    fn stream_with(id: &str, repo: &str, branch: &str, score: i32) -> Stream {
        let mut s = blank(id, id);
        s.repo = repo.into();
        s.host = "github.com".into();
        s.branch = branch.into();
        s.score = score;
        s
    }

    #[test]
    fn tabs_attach_only_to_repositories_we_know() {
        let streams = vec![
            stream_with("taxengine#main", "taxengine", "main", 10),
            stream_with("taxengine#feat", "taxengine", "feat/rates", 90),
            stream_with("other#main", "other", "main", 5),
        ];
        assert!(stream_for("https://news.example.com/story", &streams).is_none());
        assert_eq!(
            stream_for("https://github.com/acme/taxengine/pull/505", &streams)
                .unwrap()
                .id,
            "taxengine#feat"
        );
        assert_eq!(
            stream_for(
                "https://github.com/acme/taxengine/tree/feat/rates",
                &streams
            )
            .unwrap()
            .id,
            "taxengine#feat"
        );
        assert!(stream_for("https://github.com/acme/unknown/pull/1", &streams).is_none());
    }

    #[test]
    fn matching_is_by_path_segment_and_is_order_independent() {
        let forward = vec![
            stream_with("repo#main", "repo", "main", 10),
            stream_with("repo#feat", "repo", "feat/maintenance", 10),
        ];
        let reversed: Vec<Stream> = forward.iter().rev().cloned().collect();
        let url = "https://github.com/acme/repo/blob/feat/maintenance/notes.md";
        // `main` must not match inside `maintenance`, and the answer must not
        // depend on the order rows came back from the database.
        for set in [&forward, &reversed] {
            assert_eq!(stream_for(url, set).unwrap().id, "repo#feat");
        }
        // A domain that merely contains the branch or repo name is not a match.
        assert!(stream_for("https://docs.example.com/en/repo/start", &forward).is_none());
        assert!(stream_for("https://main.example.com/acme/repo/x", &forward).is_none());
    }

    #[test]
    fn a_repository_only_url_does_not_drift_as_heat_changes() {
        // Score carries heat, so tie-breaking on it moved tabs between streams
        // during the day. Debt is the stable signal.
        let mut cold = stream_with("repo#a", "repo", "a", 0);
        let mut hot = stream_with("repo#b", "repo", "b", 0);
        cold.debt = 40;
        hot.debt = 10;
        hot.score = 500;
        let streams = vec![cold, hot];
        assert_eq!(
            stream_for("https://github.com/acme/repo/issues", &streams)
                .unwrap()
                .id,
            "repo#a"
        );
    }
}
