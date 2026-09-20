//! Corpus-wide statistics over a set of discovered sessions — what `cv stats` prints and what
//! `cvd`'s `/api/stats` serves.
//!
//! The numbers come from the catalog row alone (harness, message count, cwd, dates), so a whole
//! fleet costs one catalog read and no transcript parsing. Both doors call [`CorpusStats::compute`]
//! and [`CorpusStats::to_json`] so the dashboard's Stats view and `cv stats --json` cannot drift —
//! the dashboard used to accumulate its own totals from whichever sessions the user had clicked,
//! and said so in the UI.

use crate::ir::SessionRef;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;

/// How many cwds `to_json` (and `cv stats`' table) keeps.
pub const TOP_CWDS: usize = 10;

/// The aggregate. `by_harness` and `top_cwds` are sorted by count descending, ties broken by name
/// ascending, so the order is stable across runs and across doors.
#[derive(Debug, Clone, Default)]
pub struct CorpusStats {
    pub sessions: usize,
    pub messages: usize,
    pub by_harness: Vec<(&'static str, usize)>,
    /// Every cwd, home-relative (`~/dev/x`), `(no cwd)` for sessions that record none. The full
    /// list; [`CorpusStats::to_json`] and the table take the first [`TOP_CWDS`].
    pub top_cwds: Vec<(String, usize)>,
    pub earliest_created: Option<DateTime<Utc>>,
    pub latest_updated: Option<DateTime<Utc>>,
}

impl CorpusStats {
    pub fn compute(refs: &[SessionRef]) -> CorpusStats {
        let mut per_harness: HashMap<&'static str, usize> = HashMap::new();
        let mut per_cwd: HashMap<String, usize> = HashMap::new();
        let mut stats = CorpusStats {
            sessions: refs.len(),
            ..CorpusStats::default()
        };
        for r in refs {
            *per_harness.entry(r.harness.as_str()).or_default() += 1;
            stats.messages += r.message_count;
            let cwd = r.cwd.as_deref().map(home_rel).unwrap_or_else(|| "(no cwd)".into());
            *per_cwd.entry(cwd).or_default() += 1;
            if let Some(c) = r.created_at {
                stats.earliest_created = Some(stats.earliest_created.map_or(c, |m| m.min(c)));
            }
            if let Some(u) = r.updated_at {
                stats.latest_updated = Some(stats.latest_updated.map_or(u, |m| m.max(u)));
            }
        }
        stats.by_harness = per_harness.into_iter().collect();
        stats.by_harness.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        stats.top_cwds = per_cwd.into_iter().collect();
        stats.top_cwds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        stats
    }

    /// The `cv stats --json` payload, whose top-level keys are exactly `sessions, messages,
    /// by_harness, top_cwds, earliest_created, latest_updated`. `by_harness` is an object in
    /// descending-count order (the workspace's serde_json keeps insertion order).
    pub fn to_json(&self) -> Value {
        let by_harness: serde_json::Map<String, Value> =
            self.by_harness.iter().map(|(h, n)| (h.to_string(), json!(n))).collect();
        let top_cwds: Vec<Value> = self
            .top_cwds
            .iter()
            .take(TOP_CWDS)
            .map(|(c, n)| json!({ "cwd": c, "sessions": n }))
            .collect();
        json!({
            "sessions": self.sessions,
            "messages": self.messages,
            "by_harness": by_harness,
            "top_cwds": top_cwds,
            "earliest_created": self.earliest_created.map(|d| d.to_rfc3339()),
            "latest_updated": self.latest_updated.map(|d| d.to_rfc3339()),
        })
    }
}

/// `~/dev/x` for a path under `$HOME`, the full path otherwise. `$HOME` (not `dirs::home_dir`) so
/// a test or a daemon running under a redirected home agrees with the CLI.
fn home_rel(p: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

/// The keys `cv stats --json` / `GET /api/stats` carry, in order.
pub const STATS_KEYS: [&str; 6] = [
    "sessions",
    "messages",
    "by_harness",
    "top_cwds",
    "earliest_created",
    "latest_updated",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;

    fn r(id: &str, h: Harness, msgs: usize, cwd: Option<&str>, created: &str, updated: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: h,
            path: format!("/x/{id}.jsonl").into(),
            cwd: cwd.map(Into::into),
            title: None,
            created_at: DateTime::parse_from_rfc3339(created)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            updated_at: DateTime::parse_from_rfc3339(updated)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            message_count: msgs,
        }
    }

    #[test]
    fn totals_ranking_and_json_keys() {
        let refs = vec![
            r(
                "a",
                Harness::Claude,
                3,
                Some("/w/one"),
                "2026-01-01T00:00:00Z",
                "2026-01-02T00:00:00Z",
            ),
            r(
                "b",
                Harness::Codex,
                5,
                Some("/w/one"),
                "2025-06-01T00:00:00Z",
                "2026-03-01T00:00:00Z",
            ),
            r(
                "c",
                Harness::Codex,
                7,
                None,
                "2026-02-01T00:00:00Z",
                "2026-02-02T00:00:00Z",
            ),
        ];
        let s = CorpusStats::compute(&refs);
        assert_eq!((s.sessions, s.messages), (3, 15));
        // Descending count, ties by name.
        assert_eq!(s.by_harness, vec![("codex", 2), ("claude", 1)]);
        assert_eq!(s.top_cwds[0], ("/w/one".to_string(), 2));
        assert!(s.top_cwds.iter().any(|(c, n)| c == "(no cwd)" && *n == 1));

        let v = s.to_json();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, STATS_KEYS, "stats payload keys, in order");
        let harnesses: Vec<&str> = v["by_harness"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(harnesses, ["codex", "claude"], "by_harness stays rank-ordered");
        assert_eq!(v["earliest_created"], "2025-06-01T00:00:00+00:00");
        assert_eq!(v["latest_updated"], "2026-03-01T00:00:00+00:00");
    }

    #[test]
    fn an_empty_corpus_is_zeroes_and_nulls_not_an_error() {
        let v = CorpusStats::compute(&[]).to_json();
        assert_eq!(v["sessions"], 0);
        assert_eq!(v["messages"], 0);
        assert!(v["by_harness"].as_object().unwrap().is_empty());
        assert!(v["top_cwds"].as_array().unwrap().is_empty());
        assert!(v["earliest_created"].is_null() && v["latest_updated"].is_null());
    }
}
