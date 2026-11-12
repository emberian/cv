//! One explicitly owned catalog connection for a read/discovery operation.
use super::{Publication, SessionRef};
use crate::Harness;

/// No transaction spans filesystem scans. Separate operations reopen the current catalog path;
/// this owner has no process-global connection or schema cache.
/// What a failed skipped-source write warns about ([`crate::env::warn_once`]).
#[cfg(feature = "sqlite")]
pub(crate) const SKIPPED_LIST: &str = "the skipped-source list `cv doctor` shows";

pub(crate) struct Reader {
    #[cfg(feature = "sqlite")]
    conn: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
    #[cfg(all(test, feature = "sqlite"))]
    counts_lookups: std::cell::Cell<usize>,
    #[cfg(all(test, feature = "sqlite"))]
    counts_rows: std::cell::Cell<usize>,
    #[cfg(all(test, feature = "sqlite"))]
    turn_snapshots: std::cell::Cell<usize>,
}

/// The no-sqlite fallbacks: `find`/`sessions` simply always take the scan path, so every
/// read answers "no catalog" and every write is nothing.
#[cfg(not(feature = "sqlite"))]
impl Reader {
    pub(crate) fn open() -> Option<Self> {
        None
    }

    /// Share this operation's initialized connection with scoped discovery workers.
    pub(crate) fn discovery_scope(&self) -> crate::discover_cache::read::Scope {
        crate::discover_cache::read::Context::current().enter()
    }

    pub(crate) fn counted_with_probe(
        &self,
        r: &SessionRef,
        source: &crate::source::Baseline,
        probe: &mut crate::source::Probe,
    ) -> Option<super::Counted> {
        let _ = (r, source, probe);
        None
    }

    pub(crate) fn refill_turns<'a>(&self, refs: impl Iterator<Item = &'a mut SessionRef>) {
        let _ = refs;
    }

    pub(crate) fn last_full_sync(&self) -> Option<i64> {
        None
    }

    pub(crate) fn take_rediscovery_notice(&self) -> bool {
        false
    }

    pub(crate) fn probe_stale(&self) -> Option<Vec<Harness>> {
        None
    }

    pub(crate) fn all_sessions(&self) -> Option<Vec<SessionRef>> {
        None
    }

    pub(crate) fn exact_session(&self, id: &str, harness: Option<Harness>, overrides: [bool; 2]) -> Option<SessionRef> {
        let _ = (id, harness, overrides);
        None
    }

    pub(crate) fn primary_sessions(&self) -> Vec<SessionRef> {
        Vec::new()
    }

    pub(crate) fn capture_primary_reads(&self, a: &dyn crate::Adapter) -> super::PrimaryReads {
        let _ = a;
        Default::default()
    }

    pub(crate) fn replace_harness(
        &mut self,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(std::path::PathBuf, super::Watched)],
        before: &super::PrimaryReads,
    ) {
        let _ = (h, refs, watches, before);
    }

    pub(crate) fn persist_discovery(&mut self, prune: bool) {
        crate::discover_cache::persist(prune);
    }

    pub(crate) fn record_skipped(&mut self, rows: &[(crate::Harness, &[crate::Skipped])]) {
        let _ = rows;
    }

    pub(crate) fn publish_all(&mut self, rows: &[Publication<'_>]) {
        let _ = rows;
    }
}

#[cfg(feature = "sqlite")]
impl Reader {
    pub(crate) fn open() -> Option<Self> {
        super::imp::open_db().map(|conn| Self::from_connection(std::sync::Arc::new(std::sync::Mutex::new(conn))))
    }

    #[cfg(feature = "sqlite")]
    pub(crate) fn from_connection(conn: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>) -> Self {
        Self {
            conn,
            #[cfg(all(test, feature = "sqlite"))]
            counts_lookups: std::cell::Cell::new(0),
            #[cfg(all(test, feature = "sqlite"))]
            counts_rows: std::cell::Cell::new(0),
            #[cfg(all(test, feature = "sqlite"))]
            turn_snapshots: std::cell::Cell::new(0),
        }
    }

    /// Share this operation's initialized connection with scoped discovery workers.
    pub(crate) fn discovery_scope(&self) -> crate::discover_cache::read::Scope {
        crate::discover_cache::read::Context::from_connection(self.conn.clone()).enter()
    }

    /// Demand one count record and its provenance in a single statement snapshot. Separate
    /// candidates may observe different commits; no fleet snapshot or transaction spans body
    /// reads/publication. Missing, stale, malformed or unreadable cache records are cache misses.
    pub(crate) fn counted_with_probe(
        &self,
        r: &SessionRef,
        source: &crate::source::Baseline,
        probe: &mut crate::source::Probe,
    ) -> Option<super::Counted> {
        #[cfg(test)]
        self.counts_lookups.set(self.counts_lookups.get() + 1);
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = super::imp::count_record_in(&conn, r)?;
        #[cfg(test)]
        self.counts_rows.set(self.counts_rows.get() + 1);
        let record = crate::source::Record::<super::Counted>::decode(&record)?;
        Some(record.admit_for(r, source, probe)?.clone())
    }

    #[cfg(test)]
    pub(crate) fn counts_with_probe(
        &self,
        r: &SessionRef,
        source: &crate::source::Baseline,
        probe: &mut crate::source::Probe,
    ) -> Option<crate::affordance::Counts> {
        Some(self.counted_with_probe(r, source, probe)?.counts)
    }

    /// Refill only source-certified exact counts. Cold/malformed candidates are rejected
    /// before constructing source snapshots, so metadata discovery does not inventory sidecars
    /// or tool-result directories merely to learn that no completed count exists.
    pub(crate) fn refill_turns<'a>(&self, refs: impl Iterator<Item = &'a mut SessionRef>) {
        let mut probe = crate::source::Probe::default();
        let mut candidates = 0;
        for r in refs.filter(|r| r.message_count.is_none()) {
            let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(record) = super::imp::count_record_in(&conn, r)
                .and_then(|record| crate::source::Record::<super::Counted>::decode(&record))
            else {
                continue;
            };
            let Some(turns) = record.payload.turns else {
                continue;
            };
            #[cfg(test)]
            self.turn_snapshots.set(self.turn_snapshots.get() + 1);
            let source = probe.snapshot(r);
            if record.admit_for(r, &source, &mut probe).is_some() {
                r.message_count = Some(turns as usize);
            }
            // Bound retained fleet auxiliary metadata while sharing common DB identities
            // within a batch. One exceptional certificate can still name many dependencies.
            candidates += 1;
            if candidates == 256 {
                probe = crate::source::Probe::default();
                candidates = 0;
            }
        }
    }

    #[cfg(all(test, feature = "sqlite"))]
    pub(crate) fn turn_snapshots(&self) -> usize {
        self.turn_snapshots.get()
    }

    #[cfg(all(test, feature = "sqlite"))]
    pub(crate) fn counts_work(&self) -> (usize, usize) {
        (self.counts_lookups.get(), self.counts_rows.get())
    }

    pub(crate) fn last_full_sync(&self) -> Option<i64> {
        super::imp::last_full_sync_in(&self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Whether this is the first full discovery since a newer cv upgraded the catalog.
    pub(crate) fn take_rediscovery_notice(&self) -> bool {
        super::imp::take_rediscovery_notice_in(&self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub(crate) fn probe_stale(&self) -> Option<Vec<Harness>> {
        super::imp::probe_stale_in(&self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub(crate) fn all_sessions(&self) -> Option<Vec<SessionRef>> {
        super::imp::all_sessions_in(&self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Search-row enrichment needs exactly one ID, never its prefix siblings. Preserve the
    /// existing first exact catalog copy while reusing this operation's initialized connection.
    pub(crate) fn exact_session(&self, id: &str, harness: Option<Harness>, overrides: [bool; 2]) -> Option<SessionRef> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        super::imp::exact_session_in(&conn, id, harness, overrides)
    }

    pub(crate) fn primary_sessions(&self) -> Vec<SessionRef> {
        super::imp::primary_refs_in(
            &self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            None,
        )
    }

    pub(crate) fn capture_primary_reads(&self, a: &dyn crate::Adapter) -> super::PrimaryReads {
        let refs = super::imp::primary_refs_in(
            &self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(a.harness()),
        );
        super::capture_primary_reads_from(a, &refs)
    }

    pub(crate) fn replace_harness(
        &mut self,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(std::path::PathBuf, super::Watched)],
        before: &super::PrimaryReads,
    ) {
        let sources = super::imp::prepare_sources(h, refs, before);
        super::imp::replace_prepared_in(&mut self.conn.lock().unwrap(), h, refs, watches, &sources);
    }

    pub(crate) fn persist_discovery(&mut self, prune: bool) {
        crate::discover_cache::persist_in(&mut self.conn.lock().unwrap(), prune);
    }

    /// Persist what each listed harness's discovery could not open (see
    /// [`crate::harness::Discovered`]). A catalog that cannot take the write keeps its old rows and
    /// says so once per process, so `cv doctor`'s list is never stale in silence.
    pub(crate) fn record_skipped(&mut self, rows: &[(crate::Harness, &[crate::Skipped])]) {
        if let Err(error) = super::imp::record_skipped_in(&mut self.conn.lock().unwrap(), rows) {
            crate::env::warn_once(SKIPPED_LIST, format_args!("{error:#}"));
        }
    }

    pub(crate) fn publish_all(&mut self, rows: &[Publication<'_>]) {
        let sources: Vec<_> = rows
            .iter()
            .map(|(h, refs, _, before)| super::imp::prepare_sources(*h, refs, before))
            .collect();
        super::imp::publish_all_in(&mut self.conn.lock().unwrap(), rows, &sources);
    }
}
