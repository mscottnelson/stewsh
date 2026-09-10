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
    let path = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut best: Option<&Stream> = None;
    for s in streams.iter().filter(|s| !s.repo.is_empty() && !s.archived) {
        let repo = s.repo.to_lowercase();
        if !segments.iter().any(|seg| *seg == repo) {
            continue;
        }
        // A URL naming the branch as well pins the tab to that exact stream.
        let branch = s.branch.to_lowercase();
        let exact = !branch.is_empty() && path.contains(&branch);
        if exact {
            return Some(s);
        }
        if best.is_none_or(|b| s.score > b.score) {
            best = Some(s);
        }
    }
    best
}

pub fn sync(conn: &mut Connection, streams: &[Stream], now: i64) -> Result<Value> {
    let tabs: Vec<Tab> = serde_json::from_str(&script(include_str!("browser-collect.js"), &[])?)?;
    let mut attached = 0;
    let tx = conn.transaction()?;
    let mut live = Vec::new();
    for tab in &tabs {
        let Some(target) = stream_for(&tab.url, streams) else {
            continue;
        };
        let id = id_for(&tab.url);
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
        stream::assign(&tx, &target.id, &id, "auto")?;
        attached += 1;
    }
    // A closed tab should disappear rather than linger as a dead link.
    let mut q = tx.prepare("SELECT id FROM sessions WHERE source='browser'")?;
    let known: Vec<String> = q
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(q);
    let mut gone = 0;
    for id in known.iter().filter(|id| !live.contains(id)) {
        tx.execute("DELETE FROM stream_members WHERE context_id=?1", [id])?;
        tx.execute("DELETE FROM events WHERE session_id=?1", [id])?;
        tx.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        gone += 1;
    }
    tx.commit()?;
    Ok(json!({"tabs_seen": tabs.len(), "attached": attached, "closed": gone}))
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
        // Unrelated browsing never joins a stream.
        assert!(stream_for("https://news.example.com/story", &streams).is_none());
        // A repository URL joins that repository's most pressing stream.
        assert_eq!(
            stream_for("https://github.com/acme/taxengine/pull/505", &streams)
                .unwrap()
                .id,
            "taxengine#feat"
        );
        // Naming the branch pins it exactly, whatever the scores say.
        assert_eq!(
            stream_for(
                "https://github.com/acme/taxengine/tree/feat/rates",
                &streams
            )
            .unwrap()
            .id,
            "taxengine#feat"
        );
        // A repository we do not track is still left alone.
        assert!(stream_for("https://github.com/acme/unknown/pull/1", &streams).is_none());
    }
}
