//! The JSON row shapes cv emits, in one place — so every door emits the same object.
//!
//! [`docs/INTERFACE-V2.md` §3](../../../docs/INTERFACE-V2.md) says a **session row** always has the
//! same keys (`id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes`)
//! and a **search row** is that row plus what the search itself contributed (`score`, `snippet`,
//! and the sub-agent provenance trio). The rule only holds if there is one implementation: `cv
//! search --json` and `cvd`'s `/api/search` are two doors onto the same index, and a consumer must
//! not be able to tell which one it came through. Both call these functions.
//!
//! Key ORDER is part of the shape here: the workspace enables serde_json's `preserve_order`, so
//! these objects serialize in insertion order. Overwriting an existing key (the index's title/cwd
//! winning over the catalog's) keeps its original position, which is why the overlays below read
//! as "fill in, then append".

use crate::ir::{Harness, SessionRef};
use serde_json::{json, Value};

/// THE session row (§3): the same nine keys `cv ls --json`, `cv search --json`, `cvd`'s
/// `/api/sessions` and the desktop app's `local_sessions` emit. `size_bytes` is passed in because
/// only the caller knows whether it already has the `stat` in hand.
pub fn session_row(r: &SessionRef, size_bytes: Option<u64>) -> Value {
    json!({
        "id": r.id,
        "harness": r.harness.as_str(),
        "path": r.path.to_string_lossy(),
        "cwd": r.cwd.as_ref().map(|p| p.to_string_lossy()),
        "title": r.title,
        "created_at": r.created_at.map(|t| t.to_rfc3339()),
        "updated_at": r.updated_at.map(|t| t.to_rfc3339()),
        "message_count": r.message_count,
        "size_bytes": size_bytes,
    })
}

/// Unix seconds (how the search index dates a hit) → the RFC 3339 string §3 promises.
fn rfc3339(secs: Option<i64>) -> Option<String> {
    secs.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.to_rfc3339())
}

/// One search hit, in the terms the index speaks (ids and unix seconds), ready to be shaped into
/// the §3 search row by [`SearchRow::to_json`]. It is deliberately *not* `cv_search::Hit`: cv-core
/// cannot depend on cv-search (cv-search depends on cv-core), and the daemon's live-scan fallback
/// has no `Hit` to hand either.
pub struct SearchRow<'a> {
    /// The session id the index matched — a top-level session, or `agent-<hex>` for a folded-in
    /// sub-agent transcript (`cv index --subagents`).
    pub id: &'a str,
    pub harness: &'a str,
    pub cwd: Option<&'a str>,
    pub title: Option<&'a str>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// BM25 for full-text, cosine similarity for semantic; `None` for a live scan, which ranks
    /// nothing.
    pub score: Option<f32>,
    pub snippet: &'a str,
    pub agent_id: Option<&'a str>,
    pub parent_id: Option<&'a str>,
    pub workflow: Option<&'a str>,
}

impl SearchRow<'_> {
    /// The §3 search row: the session row (filled from the catalog, so `path`, `message_count` and
    /// `size_bytes` are the same values `ls --json` gives — all null for a sub-agent lane, which
    /// the catalog does not list) with the hit's own title/cwd/dates laid over it, plus `score`,
    /// `snippet`, and the sub-agent provenance trio — always present, null for a top-level hit.
    pub fn to_json(&self) -> Value {
        let harness = Harness::parse(self.harness);
        let cataloged = crate::catalog::lookup(self.id, harness)
            .into_iter()
            .find(|r| r.id == self.id);
        let mut row = match &cataloged {
            Some(r) => session_row(r, std::fs::metadata(&r.path).ok().map(|m| m.len())),
            None => json!({
                "id": self.id,
                "harness": self.harness,
                "path": Value::Null,
                "cwd": Value::Null,
                "title": Value::Null,
                "created_at": Value::Null,
                "updated_at": Value::Null,
                "message_count": Value::Null,
                "size_bytes": Value::Null,
            }),
        };
        let obj = row.as_object_mut().expect("json object");
        // The index is what matched: its title/cwd/dates win when it has them.
        if let Some(c) = self.cwd {
            obj.insert("cwd".into(), json!(c));
        }
        if let Some(t) = self.title {
            obj.insert("title".into(), json!(t));
        }
        if let Some(t) = rfc3339(self.created_at) {
            obj.insert("created_at".into(), json!(t));
        }
        if let Some(t) = rfc3339(self.updated_at) {
            obj.insert("updated_at".into(), json!(t));
        }
        obj.insert("score".into(), json!(self.score));
        obj.insert("snippet".into(), json!(self.snippet));
        // Sub-agent provenance (an index built with `cv index --subagents` folds lane transcripts
        // in): the lane's own agent id (`cv show <agent_id>` resolves it), the top-level session
        // that spawned it, and the workflow run it belonged to.
        obj.insert("agent_id".into(), json!(self.agent_id));
        obj.insert("parent_id".into(), json!(self.parent_id));
        obj.insert("workflow".into(), json!(self.workflow));
        row
    }
}

/// The live-scan twin of [`SearchRow::to_json`]: the ref is in hand, so the row is exact; a live
/// scan has no score and never walks sub-agents (provenance is null).
pub fn live_search_row(r: &SessionRef, title: &str, snippet: &str) -> Value {
    let mut row = session_row(r, std::fs::metadata(&r.path).ok().map(|m| m.len()));
    let obj = row.as_object_mut().expect("json object");
    obj.insert("title".into(), json!(title));
    obj.insert("score".into(), Value::Null);
    obj.insert("snippet".into(), json!(snippet));
    obj.insert("agent_id".into(), Value::Null);
    obj.insert("parent_id".into(), Value::Null);
    obj.insert("workflow".into(), Value::Null);
    row
}

/// The keys a §3 session row carries, in order — for tests and `cv schema`.
pub const SESSION_ROW_KEYS: [&str; 9] = [
    "id",
    "harness",
    "path",
    "cwd",
    "title",
    "created_at",
    "updated_at",
    "message_count",
    "size_bytes",
];

/// The keys a §3 search row carries, in order: the session row plus what the search contributed.
pub const SEARCH_ROW_KEYS: [&str; 14] = [
    "id",
    "harness",
    "path",
    "cwd",
    "title",
    "created_at",
    "updated_at",
    "message_count",
    "size_bytes",
    "score",
    "snippet",
    "agent_id",
    "parent_id",
    "workflow",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn a_ref() -> SessionRef {
        SessionRef {
            id: "abc".into(),
            harness: Harness::Claude,
            path: "/x/abc.jsonl".into(),
            cwd: Some("/w".into()),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 3,
        }
    }

    #[test]
    fn session_row_is_the_contract_row() {
        let v = session_row(&a_ref(), Some(10));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, SESSION_ROW_KEYS, "§3 row keys, in order");
        assert!(v["title"].is_null(), "missing title is an explicit null");
        assert_eq!(v["size_bytes"], 10);
    }

    #[test]
    fn search_rows_carry_the_session_row_plus_the_search_fields() {
        // An uncataloged id (a sub-agent lane): the session-row half is all null, the search half
        // is fully populated — the shape never varies, only the values.
        let row = SearchRow {
            id: "agent-deadbeef",
            harness: "claude",
            cwd: Some("/w"),
            title: Some("a lane"),
            created_at: Some(1_700_000_000),
            updated_at: None,
            score: Some(1.5),
            snippet: "…",
            agent_id: Some("deadbeef"),
            parent_id: Some("parent"),
            workflow: Some("wf_1"),
        }
        .to_json();
        let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, SEARCH_ROW_KEYS, "§3 search row keys, in order");
        assert_eq!(row["created_at"], "2023-11-14T22:13:20+00:00");
        assert!(row["updated_at"].is_null());
        assert_eq!(row["agent_id"], "deadbeef");

        // The live-scan twin: same keys, explicit nulls where a live scan knows nothing.
        let live = live_search_row(&a_ref(), "titled", "snip");
        let keys: Vec<&str> = live.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, SEARCH_ROW_KEYS, "live rows are the same shape");
        assert!(live["score"].is_null());
        assert!(live["workflow"].is_null());
        assert_eq!(live["title"], "titled");
    }
}
