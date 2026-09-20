//! CLI glue for the query calculus: parse `-q` strings and resolve the one external predicate
//! cv-core cannot (`text:`, which needs the tantivy index) into the [`TextSets`] the core's
//! [`SessionFacts`] resolver consults. Everything else — the events/forest/compaction predicates
//! and the two-phase ref filter — lives in `cv_core::query::facts`, so `cv -q` and `cvd`'s
//! `/api/stats?q=` evaluate the same calculus. The reference itself is `cv schema`
//! (`cmd/schema.rs`).

use anyhow::{anyhow, Result};
use cv_core::query::{FieldId, SessionQuery};
use std::collections::HashSet;

pub(crate) use cv_core::query::{filter_refs, matches_full, TextSets};

/// Parse an optional `-q` string into a query, surfacing parse errors. The core's messages point
/// at the reference by its 0.10 name; the CLI seam re-points them at `cv schema`.
pub(crate) fn build(query: Option<String>) -> Result<Option<SessionQuery>> {
    match query {
        Some(q) => Ok(Some(
            SessionQuery::parse(&q).map_err(|e| anyhow!(e.replace("`cv query`", "`cv schema`")))?,
        )),
        None => Ok(None),
    }
}

/// Run one full-text search per distinct `text:` needle in the query. If there's no index, warn
/// once and treat every `text:` needle as matching nothing.
pub(crate) fn text_sets(query: &SessionQuery) -> TextSets {
    if query.needles(FieldId::Text).is_empty() {
        return TextSets::empty();
    }
    if !cv_search::default_tantivy_dir().exists() {
        eprintln!("cv query: text: needs a full-text index — run `cv index` (treating text: as no match)");
        return TextSets::resolve(query, |_| HashSet::new());
    }
    TextSets::resolve(query, |n| {
        // A generous cap: we want the full matching set, not a top-K ranking.
        cv_search::text_search(None, n, 100_000)
            .map(|hits| hits.into_iter().map(|h| h.id).collect())
            .unwrap_or_default()
    })
}
