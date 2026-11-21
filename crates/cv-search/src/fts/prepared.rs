//! Read-only repeated queries over one pinned index snapshot. No discovery or source reads.
use super::*;

/// A pinned index snapshot. New commits become visible only after [`Self::refresh`].
/// Source freshness is not assessed: results describe indexed revisions, including
/// sources since changed or removed, and exclude sessions never indexed.
pub struct PreparedIndex {
    path: PathBuf,
    index: Index,
    fields: Fields,
    searcher: tantivy::Searcher,
    snapshot_id: String,
    setup_micros: u64,
}

/// Logical work performed by this query, not OS I/O, page faults or physical bytes.
#[derive(Debug, serde::Serialize)]
pub struct QueryWork {
    pub source_bytes: u64,
    pub discovery_sessions: usize,
    pub live_snippet_reads: usize,
    /// Not instrumented: `null` is deliberately different from zero index I/O.
    pub index_bytes: Option<u64>,
    /// Top-level ranking/count passes; metadata ID lookups are counted separately.
    pub top_level_query_passes: usize,
    /// Distinct matching sessions whose metadata was evaluated for a filter.
    pub predicate_sessions: usize,
    /// Metadata ID lookup queries, including winner materialization without filters.
    pub metadata_lookup_queries: u64,
    /// Calls to the stored-document reader (including selective headers), not bytes.
    pub stored_document_reads: u64,
    pub returned_hits: usize,
    pub evaluation_micros: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct IndexedAnswer {
    pub hits: Vec<Hit>,
    pub total: usize,
    /// Opaque identifier of this reader lifetime, not a source revision or index hash.
    pub snapshot_id: String,
    pub indexed_documents: u64,
    pub source_freshness_assessed: bool,
    pub work: QueryWork,
}

impl PreparedIndex {
    pub fn open(path: &Path) -> Result<Self> {
        let started = std::time::Instant::now();
        let (index, fields, searcher) = query_index(path)?;
        Ok(Self {
            path: path.to_owned(),
            index,
            fields,
            searcher,
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            setup_micros: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        })
    }

    /// Reopen atomically: a failed open leaves the previous usable snapshot intact.
    pub fn refresh(&mut self) -> Result<()> {
        *self = Self::open(&self.path)?;
        Ok(())
    }

    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }
    pub fn indexed_documents(&self) -> u64 {
        self.searcher.num_docs()
    }
    /// Wall-clock reader setup, paid once at open/refresh, not per query.
    pub fn setup_micros(&self) -> u64 {
        self.setup_micros
    }

    /// The existing exact ID-membership query, reused by the public batch helper.
    pub fn matching_ids(&self, query: &str, scope: crate::Scope) -> Result<HashSet<String>> {
        matching_ids(&self.index, &self.fields, &self.searcher, query, scope)
    }

    /// Rank and count indexed sessions. Filters use metadata from this same snapshot;
    /// an allowlist admits a folded child when its own or its parent's ID is allowed.
    /// Snippets are stored previews, never live transcript excerpts.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        filter: crate::SearchFilter<'_>,
        allowed_ids: Option<std::sync::Arc<HashSet<String>>>,
    ) -> Result<IndexedAnswer> {
        let started = std::time::Instant::now();
        let parsed = parse_text_query(&self.index, &self.fields, query, filter.scope)?;
        let filtered = filtered(filter) || allowed_ids.is_some();
        let decisions = std::sync::Arc::new(MetadataDecisions::new_allowed(
            &self.searcher,
            &self.fields,
            filter,
            allowed_ids,
        ));
        let accept = filtered.then(|| {
            let decisions = decisions.clone();
            std::sync::Arc::new(move |id: &str| decisions.resolve(id).map(|d| d.accepted))
                as crate::session_collector::SessionFilter
        });
        let candidates = rank(
            &self.fields,
            &self.searcher,
            &*parsed,
            limit,
            accept.clone(),
            &self.searcher,
        )?;
        let mut query_passes = 1;
        let total = if candidates.len() < limit {
            candidates.len()
        } else {
            query_passes += 1;
            let ids = self
                .searcher
                .search(&*parsed, &crate::session_collector::SessionIds(self.fields.id))?;
            let mut count = 0;
            for id in ids {
                if match &accept {
                    Some(accept) => accept(&id)?,
                    None => true,
                } {
                    count += 1;
                }
            }
            count
        };
        let hits = candidates
            .into_iter()
            .map(|candidate| indexed_hit(&self.searcher, &self.fields, candidate, &decisions))
            .collect::<Result<Vec<_>>>()?;
        let predicate_sessions = decisions.cache.lock().unwrap_or_else(|e| e.into_inner()).len();
        let work = QueryWork {
            source_bytes: 0,
            discovery_sessions: 0,
            live_snippet_reads: 0,
            index_bytes: None,
            top_level_query_passes: query_passes,
            metadata_lookup_queries: decisions.metadata_queries.load(std::sync::atomic::Ordering::Relaxed),
            predicate_sessions,
            stored_document_reads: decisions.stored_reads.load(std::sync::atomic::Ordering::Relaxed),
            returned_hits: hits.len(),
            evaluation_micros: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        };
        Ok(IndexedAnswer {
            hits,
            total,
            snapshot_id: self.snapshot_id.clone(),
            indexed_documents: self.searcher.num_docs(),
            source_freshness_assessed: false,
            work,
        })
    }
}
