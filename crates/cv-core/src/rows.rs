//! The JSON row shapes cv emits, in one place — so every door emits the same object.
//!
//! [`docs/INTERFACE-V2.md` §3](../../../docs/INTERFACE-V2.md) says a **session row** always has the
//! same keys (`id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes`)
//! and a **search row** is that row plus what the search itself contributed (`score`, `snippet`,
//! and the sub-agent provenance trio). The rule only holds if there is one implementation: `cv
//! search --json` and `cv serve`'s `/api/search` are two doors onto the same index, and a consumer must
//! not be able to tell which one it came through. Both call these functions.
//!
//! Key ORDER is part of the shape here: the workspace enables serde_json's `preserve_order`, so
//! these objects serialize in insertion order. Overwriting an existing key (the index's title/cwd
//! winning over the catalog's) keeps its original position, which is why the overlays below read
//! as "fill in, then append".

use crate::events::{EventRow, TouchedSession};
use crate::ir::{Harness, SessionRef};
use serde_json::{json, Value};

/// THE session row (§3): the same nine keys `cv ls --json`, `cv search --json`, `cv serve`'s
/// `/api/sessions` and the desktop app's `api` command emit. `size_bytes` is passed in because
/// only the caller knows whether it already has the `stat` in hand. `message_count` is `null`
/// for a session no full read or index has counted yet; `0` is a session with no turns.
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

/// One search-response enrichment operation. Reuses catalog initialization and the exact-ID
/// statement, without a daemon cache or a retained second collection of hit metadata.
pub struct SearchRowContext {
    catalog: std::cell::OnceCell<Option<crate::catalog::Reader>>,
    /// One probe per search response: chunked hits repeat a session's path within a response.
    probe: std::cell::RefCell<crate::source::Probe>,
}

impl SearchRowContext {
    pub fn new() -> Self {
        Self {
            catalog: std::cell::OnceCell::new(),
            probe: std::cell::RefCell::new(crate::source::Probe::default()),
        }
    }
}

impl Default for SearchRowContext {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchRow<'_> {
    /// The §3 search row: the session row (filled from the catalog, so `path`, `message_count` and
    /// `size_bytes` are the same values `ls --json` gives — all null for a sub-agent lane, which
    /// the catalog does not list) with the hit's own title/cwd/dates laid over it, plus `score`,
    /// `snippet`, and the sub-agent provenance trio — always present, null for a top-level hit.
    /// A response's hits share one operation-owned exact catalog reader (`context`).
    pub fn to_json_with(&self, context: &SearchRowContext) -> Value {
        let cataloged = context
            .catalog
            .get_or_init(crate::catalog::Reader::open)
            .as_ref()
            .and_then(|reader| {
                reader.exact_session(
                    self.id,
                    Harness::parse(self.harness),
                    [self.cwd.is_some(), self.title.is_some()],
                )
            });
        let reference = cataloged.as_ref();
        // Index cwd/title and valid dates win when present; the catalog supplies their fallbacks
        // plus canonical identity, path and count. Out-of-range index dates use the catalog too.
        let created = rfc3339(self.created_at).or_else(|| reference.and_then(|r| r.created_at).map(|t| t.to_rfc3339()));
        let updated = rfc3339(self.updated_at).or_else(|| reference.and_then(|r| r.updated_at).map(|t| t.to_rfc3339()));
        json!({
            "id": reference.map_or(self.id, |r| r.id.as_str()),
            "harness": reference.map_or(self.harness, |r| r.harness.as_str()),
            "path": reference.map(|r| r.path.to_string_lossy()),
            "cwd": self.cwd.map(std::borrow::Cow::Borrowed).or_else(|| reference.and_then(|r| r.cwd.as_ref()).map(|p| p.to_string_lossy())),
            "title": self.title.or_else(|| reference.and_then(|r| r.title.as_deref())),
            "created_at": created,
            "updated_at": updated,
            "message_count": reference.map(|r| r.message_count),
            "size_bytes": reference.and_then(|r| context.probe.borrow_mut().ref_size(r)),
            "score": self.score,
            "snippet": self.snippet,
            "agent_id": self.agent_id,
            "parent_id": self.parent_id,
            "workflow": self.workflow,
        })
    }
}

/// The live-scan twin of [`SearchRow::to_json`]: the ref is in hand, so the row is exact; a live
/// scan has no score and never walks sub-agents (provenance is null).
pub fn live_search_row(r: &SessionRef, title: &str, snippet: &str) -> Value {
    let mut row = session_row(r, crate::source::ref_size(r));
    let obj = row.as_object_mut().expect("json object");
    obj.insert("title".into(), json!(title));
    obj.insert("score".into(), Value::Null);
    obj.insert("snippet".into(), json!(snippet));
    obj.insert("agent_id".into(), Value::Null);
    obj.insert("parent_id".into(), Value::Null);
    obj.insert("workflow".into(), Value::Null);
    row
}

/// One extracted tool event as `cv events --json` and `GET /api/session/{h}/{id}/events` emit it:
/// the catalog row with `ts` spelled RFC 3339 (the catalog stores unix seconds) and the sub-agent
/// provenance trio always present (null for a top-level session).
pub fn event_row(e: &EventRow) -> Value {
    json!({
        "msg_idx": e.msg_idx,
        "ts": rfc3339(e.ts),
        "kind": e.kind,
        "tool": e.tool,
        "target": e.target,
        "detail": e.detail,
        "agent_id": e.agent_id,
        "parent_id": e.parent_id,
        "workflow": e.workflow,
    })
}

/// One `cv touched --json` / `GET /api/touched` row: who touched the file, how often, when last
/// (RFC 3339), and which lane of which workflow if it was a sub-agent.
pub fn touched_row(t: &TouchedSession) -> Value {
    json!({
        "harness": t.harness,
        "session_id": t.session_id,
        "title": t.title,
        "edits": t.edits,
        "reads": t.reads,
        "last_ts": rfc3339(t.last_ts),
        "agent_id": t.agent_id,
        "parent_id": t.parent_id,
        "workflow": t.workflow,
    })
}

/// `head` then `tail`, as one array: the §3 rows are `SessionRef::FIELDS` plus what each row adds.
const fn joined<const N: usize>(head: &[&'static str], tail: &[&'static str]) -> [&'static str; N] {
    let mut out = [""; N];
    let mut i = 0;
    while i < head.len() {
        out[i] = head[i];
        i += 1;
    }
    let mut j = 0;
    while j < tail.len() {
        out[i + j] = tail[j];
        j += 1;
    }
    assert!(i + j == N, "the row's key count is its parts' sum");
    out
}

/// The keys a §3 session row carries, in order: [`SessionRef::FIELDS`] plus `size_bytes`.
pub const SESSION_ROW_KEYS: [&str; SessionRef::FIELDS.len() + 1] = joined(SessionRef::FIELDS, &["size_bytes"]);

/// The keys a sub-agent forest row carries (`/api/session/{harness}/{id}/subagents`), in order:
/// the session row plus the sidecar enrichment the forest view groups by.
pub const SUBAGENT_ROW_KEYS: [&str; SESSION_ROW_KEYS.len() + 7] = joined(
    &SESSION_ROW_KEYS,
    &[
        "agent_id",
        "agent_type",
        "description",
        "tool_use_id",
        "workflow",
        "result_status",
        "result_summary",
    ],
);

/// One sub-agent of a forest as its row: [`session_row`] of its transcript, then the
/// [`SUBAGENT_ROW_KEYS`] it adds, each an explicit `null` when the sidecar did not say.
pub fn subagent_row(s: &crate::AgentRef, size_bytes: Option<u64>) -> Value {
    let crate::AgentRef {
        session,
        agent_type,
        description,
        tool_use_id,
        workflow,
        result_status,
        result_summary,
    } = s;
    let mut row = session_row(session, size_bytes);
    let map = row.as_object_mut().expect("session_row is an object");
    let added = [
        json!(s.agent_id()),
        json!(agent_type),
        json!(description),
        json!(tool_use_id),
        json!(workflow),
        json!(result_status),
        json!(result_summary),
    ];
    for (key, value) in SUBAGENT_ROW_KEYS[SESSION_ROW_KEYS.len()..].iter().zip(added) {
        map.insert((*key).into(), value);
    }
    row
}

/// The keys a §3 search row carries, in order: the session row plus what the search contributed.
pub const SEARCH_ROW_KEYS: [&str; SESSION_ROW_KEYS.len() + 5] = joined(
    &SESSION_ROW_KEYS,
    &["score", "snippet", "agent_id", "parent_id", "workflow"],
);

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
            message_count: Some(3),
        }
    }

    #[test]
    fn session_row_is_the_contract_row() {
        let v = session_row(&a_ref(), Some(10));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, SESSION_ROW_KEYS, "§3 row keys, in order");
        assert!(v["title"].is_null(), "missing title is an explicit null");
        assert_eq!(v["message_count"], 3);
        assert_eq!(v["size_bytes"], 10);
        let uncounted = SessionRef {
            message_count: None,
            ..a_ref()
        };
        assert!(
            session_row(&uncounted, None)["message_count"].is_null(),
            "not counted yet is null, never 0"
        );
    }

    #[test]
    fn subagent_rows_carry_the_session_row_plus_the_sidecar_fields() {
        let agent = crate::AgentRef {
            session: SessionRef {
                id: "agent-a1".into(),
                ..a_ref()
            },
            agent_type: Some("Explore".into()),
            description: None,
            tool_use_id: Some("toolu_1".into()),
            workflow: None,
            result_status: None,
            result_summary: None,
        };
        let row = subagent_row(&agent, None);
        let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, SUBAGENT_ROW_KEYS);
        assert_eq!(row["agent_id"], "a1");
        assert!(row["description"].is_null());
    }

    #[test]
    fn search_rows_carry_the_session_row_plus_the_search_fields() {
        let _world = crate::test_world("rows-search-contract");
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
        .to_json_with(&SearchRowContext::new());
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
