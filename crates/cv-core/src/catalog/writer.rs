//! One owned catalog connection for a sweep's serial publication stage.
use crate::events::{EventSpool, Provenance};
use crate::source::Baseline;
use crate::{stream::Counted, SessionRef};
use anyhow::Result;

/// Reuses catalog/schema initialization and prepared event statements across sessions. Each
/// consumer's publication remains an independent transaction; no write lock spans source reads.
pub struct Writer {
    #[cfg(feature = "sqlite")]
    conn: rusqlite::Connection,
}

impl Writer {
    /// Open the shared catalog and initialize existing event/catalog schemas once.
    pub fn open() -> Option<Self> {
        if crate::env::frozen() {
            return None;
        }
        #[cfg(feature = "sqlite")]
        {
            crate::events::writer_connection().map(|conn| Self { conn })
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    /// Publish one completed event pass and its own captured source baseline atomically.
    pub fn record_events(
        &mut self,
        r: &SessionRef,
        from: usize,
        events: EventSpool,
        source: &Baseline,
        prov: &Provenance,
    ) -> Result<()> {
        #[cfg(feature = "sqlite")]
        {
            crate::events::record_spooled_in(&mut self.conn, r, from, events, source, prov)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            crate::events::record_spooled_with_source(r, from, events, source, prov)
        }
    }

    /// Counts retain their independent before-read dependencies and best-effort cache semantics.
    pub fn record_counted(&mut self, r: &SessionRef, source: &Baseline, counted: Counted) {
        #[cfg(feature = "sqlite")]
        {
            let source = source.with_reads(&counted.dependencies);
            if !crate::source::unchanged(r, &source) {
                return;
            }
            super::imp::note_counts_in(&mut self.conn, r, &source, Some(counted.turns), counted.facts, || {
                counted.counts
            });
        }
        #[cfg(not(feature = "sqlite"))]
        {
            counted.record_with_source(r, source);
        }
    }

    /// Persist refreshed query facts (e.g. `subagent_count` / `subagent_guard`) onto an already
    /// source-certified count record without re-parsing the session transcript.
    pub(crate) fn refresh_counted_facts(&mut self, r: &SessionRef, source: &Baseline, counted: super::Counted) {
        #[cfg(feature = "sqlite")]
        {
            if !crate::source::unchanged(r, source) {
                return;
            }
            let turns = counted.turns.map(|t| t as usize);
            let facts = counted.facts;
            let counts = counted.counts;
            super::imp::note_counts_in(&mut self.conn, r, source, turns, facts, || counts);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = (r, source, counted);
        }
    }
}
