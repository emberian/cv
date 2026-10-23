//! CLI glue for the query calculus: parse `-q` strings and resolve the one external predicate
//! cv-core cannot (`text:`, which needs the tantivy index) into the [`TextSets`] the core's
//! [`SessionFacts`] resolver consults. Everything else — the events/forest/compaction predicates
//! and the two-phase ref filter — lives in `cv_core::query::facts`, so `cv -q` and `cv serve`'s
//! `/api/stats?q=` evaluate the same calculus. The reference itself is `cv schema`
//! (`cmd/schema.rs`).

use crate::util::usage;
use anyhow::{Context, Result};
use cv_core::query::{FieldId, SessionQuery};

pub(crate) use cv_core::query::{filter_refs, TextSets};

/// Parse an optional `-q` string into a query. A malformed query is a caller mistake (exit 2,
/// like a bad window or a removed flag). Core diagnostics point at `cv schema`.
pub(crate) fn build(query: Option<String>) -> Result<Option<SessionQuery>> {
    match query {
        Some(q) => match SessionQuery::parse(&q) {
            Ok(parsed) => Ok(Some(parsed)),
            Err(e) => usage(e),
        },
        None => Ok(None),
    }
}

/// Resolve every matching ID per distinct `text:` needle: the index plus the live read of what it
/// has not seen (all of it unless `$CV_LIVE_BYTES` or the config bounds it, none under `--frozen`),
/// as `cv search` answers. A session left
/// unread (unreadable, or `--frozen`) keeps what the index last saw of it (a note on stderr says
/// how many). Without an index the live read must cover every session, or there is no answer: a
/// usage error (exit 2) pointing at `cv index`, never a partial membership (which `NOT text:`
/// would turn into "everything unread").
pub(crate) fn text_sets(query: &SessionQuery) -> Result<TextSets> {
    let live = cv_search::LiveRead::of(None).or_else(|e| usage(format!("{e:#}")))?;
    text_sets_within(query, live)
}

fn text_sets_within(query: &SessionQuery, live: cv_search::LiveRead) -> Result<TextSets> {
    let needles = query.needles(FieldId::Text);
    if needles.is_empty() {
        return Ok(TextSets::empty());
    }
    if live == cv_search::LiveRead::Frozen && !cv_search::default_tantivy_dir().exists() {
        return usage("`text:` needs a full-text index; build it with `cv index`");
    }
    let borrowed: Vec<_> = needles.iter().map(String::as_str).collect();
    let (matches, suffix) = cv_search::text_search_ids_suffixed(None, &borrowed, live, &cv_core::sessions())
        .context("opening full-text query index")?;
    if suffix.no_index && suffix.unread() > 0 {
        return usage(format!(
            "`text:` needs a full-text index: {} of {} session(s) were not read live; build it with `cv index`",
            suffix.unread(),
            suffix.stale
        ));
    }
    if let Some(line) = suffix.unread_line() {
        eprintln!("(note: text: {line})");
    }
    let mut sets = std::collections::HashMap::new();
    for (needle, result) in needles.into_iter().zip(matches) {
        sets.insert(
            needle.clone(),
            result.with_context(|| format!("resolving text:{needle:?}"))?,
        );
    }
    Ok(TextSets::resolve(query, |needle| {
        sets.remove(needle).unwrap_or_default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_index_and_missing_index_have_distinct_query_outcomes() {
        let world = crate::test_world("query-text-index-errors");
        let path = world.write_session(
            "lone",
            &[serde_json::json!({"type":"user", "message":{"role":"user", "content":"needle"}})],
        );
        let query = SessionQuery::parse("NOT text:needle").unwrap();
        // No index, but the live read covers every session: the membership is exact.
        let sets = text_sets(&query).unwrap();
        assert!(sets.contains("needle", "lone"), "{}", path.display());
        // A live read that cannot cover them all has no answer.
        let absent = text_sets_within(&query, cv_search::LiveRead::Within(0, cv_search::Budget::Flag)).unwrap_err();
        assert!(absent.downcast_ref::<crate::util::UsageError>().is_some(), "{absent:#}");
        assert!(format!("{absent:#}").contains("cv index"), "{absent:#}");
        std::fs::create_dir_all(cv_search::default_tantivy_dir()).unwrap();
        std::fs::write(cv_search::default_tantivy_dir().join("meta.json"), "not an index").unwrap();
        assert!(
            text_sets(&query).is_err(),
            "existing unreadable index became false membership"
        );
    }

    #[test]
    fn text_sets_batch_distinct_needles_without_losing_membership() {
        let world = crate::test_world("query_text_sets_batch");
        let refs: Vec<_> = [("first", "alpha"), ("second", "beta")]
            .into_iter()
            .map(|(id, content)| {
                let path = world.write_session(
                    id,
                    &[serde_json::json!({
                        "type":"user", "message":{"role":"user", "content":content}
                    })],
                );
                cv_core::SessionRef::new(id, cv_core::Harness::Claude, path)
            })
            .collect();
        cv_search::index_sessions(None, refs, true, false).unwrap();
        let query = SessionQuery::parse("text:alpha OR text:beta OR text:alpha").unwrap();
        let sets = text_sets(&query).unwrap();
        assert!(sets.contains("alpha", "first"));
        assert!(!sets.contains("alpha", "second"));
        assert!(sets.contains("beta", "second"));
        assert!(!sets.contains("beta", "first"));
        let invalid = SessionQuery::parse("NOT text:\"(\"").unwrap();
        assert!(
            text_sets(&invalid).is_err(),
            "invalid full-text syntax became an empty false fact"
        );
    }
}
