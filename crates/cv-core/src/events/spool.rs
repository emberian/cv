//! Bounded event retention between a source pass and atomic catalog publication. Without the
//! `sqlite` feature there is no catalog to publish to, so the spool counts rows and keeps none.
use super::{Event, EventSink};
#[cfg(feature = "sqlite")]
use crate::spill::{Ordered, Record};
use crate::stream::{Checkpoint, Flow, MessageSink};
use crate::{Message, Session};
#[cfg(feature = "sqlite")]
use anyhow::Context;
use anyhow::Result;
#[cfg(all(test, feature = "sqlite"))]
use std::fs;
#[cfg(feature = "sqlite")]
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[cfg(feature = "sqlite")]
#[derive(serde::Deserialize)]
struct DiskEvent {
    msg_idx: usize,
    ts: Option<i64>,
    kind: String,
    tool: Option<String>,
    target: Option<String>,
    detail: Option<String>,
    call: bool,
    #[serde(default)]
    hunk_hashes: Vec<u64>,
}

/// The kinds a spooled row may carry; decode admits exactly these.
#[cfg(feature = "sqlite")]
const KINDS: &[&str] = &["file_edit", "file_read", "command", "tool", "error"];

#[cfg(feature = "sqlite")]
fn decode(line: &str) -> Result<Event> {
    let row: DiskEvent = serde_json::from_str(line).context("decoding event spool row")?;
    let kind = KINDS
        .iter()
        .find(|kind| **kind == row.kind.as_str())
        .copied()
        .context("unknown event kind")?;
    Ok(Event {
        msg_idx: row.msg_idx,
        ts: row.ts,
        kind,
        tool: row.tool,
        target: row.target,
        detail: row.detail,
        call: row.call,
        hunk_hashes: row.hunk_hashes,
    })
}

#[cfg(feature = "sqlite")]
impl Record for Event {
    type ReadState = String;
    fn heap_bytes(&self) -> usize {
        [&self.tool, &self.target, &self.detail]
            .into_iter()
            .flatten()
            .map(String::capacity)
            .sum::<usize>()
            + self.hunk_hashes.capacity() * std::mem::size_of::<u64>()
    }
    fn write(&self, writer: &mut impl Write) -> Result<()> {
        serde_json::to_writer(&mut *writer, self).context("serializing event spool row")?;
        writer.write_all(b"\n").context("writing event spool row")
    }
    fn read(reader: &mut impl BufRead, line: &mut String) -> Result<Self> {
        line.clear();
        reader.read_line(line).context("reading event spool row")?;
        decode(line)
    }
}

/// A completed ordered event batch. Large batches share the bounded private spool with stats;
/// staged rows remain unpublished until the source pass completes successfully. A build without
/// the catalog (`sqlite`) has nowhere to publish them: it counts each row and retains none.
#[derive(Default)]
pub struct EventSpool {
    #[cfg(feature = "sqlite")]
    ordered: Ordered<Event>,
    #[cfg(not(feature = "sqlite"))]
    len: usize,
}
impl EventSpool {
    pub fn len(&self) -> usize {
        #[cfg(feature = "sqlite")]
        return self.ordered.len();
        #[cfg(not(feature = "sqlite"))]
        return self.len;
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn temporary_bytes(&self) -> Result<u64> {
        #[cfg(feature = "sqlite")]
        return self.ordered.spilled_bytes();
        #[cfg(not(feature = "sqlite"))]
        return Ok(0);
    }
    pub fn push(&mut self, event: Event) -> Result<()> {
        #[cfg(feature = "sqlite")]
        return self.ordered.push(event);
        #[cfg(not(feature = "sqlite"))]
        {
            drop(event);
            self.len += 1;
            Ok(())
        }
    }
    #[cfg(feature = "sqlite")]
    pub(super) fn rows(self) -> Result<impl Iterator<Item = Result<Event>>> {
        self.ordered.rows()
    }
    /// Stream staged event rows out of the bounded spool in extraction order.
    pub fn into_rows(self) -> Result<Box<dyn Iterator<Item = Result<Event>>>> {
        #[cfg(feature = "sqlite")]
        return Ok(Box::new(self.ordered.rows()?));
        #[cfg(not(feature = "sqlite"))]
        return Ok(Box::new(std::iter::empty()));
    }
}

/// Event extraction with bounded retention. Checkpoints have the same events layer as EventSink;
/// the rows are unpublished until the source pass completes successfully.
pub struct SpoolingSink {
    inner: EventSink,
    spool: EventSpool,
    latch: crate::stream::Latch,
}

impl SpoolingSink {
    pub fn new(cwd: Option<PathBuf>) -> Self {
        Self::with_inner(EventSink::new(cwd))
    }

    pub fn resume(cp: &Checkpoint) -> Option<Self> {
        EventSink::resume(cp).map(Self::with_inner)
    }

    /// [`EventSink::with_source`].
    pub fn with_source(mut self, source: PathBuf) -> Self {
        self.inner = self.inner.with_source(source);
        self
    }

    fn with_inner(inner: EventSink) -> Self {
        Self {
            inner,
            spool: EventSpool::default(),
            latch: Default::default(),
        }
    }

    /// Keep event checkpoints and dependency reads without extracting further event rows.
    pub fn without_events(mut self) -> Self {
        self.inner = self.inner.without_events();
        self
    }

    pub fn inner(&self) -> &EventSink {
        &self.inner
    }

    /// Observe without copying the message; failures keep the batch unpublished.
    pub fn observe_message(&mut self, message: &Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        if self.inner.observe_message(message) == Flow::Stop {
            self.latch.capture(&mut self.inner);
            // `EventSink` stops only once its latch holds a failure, so `capture` has latched
            // it; should it ever stop without one, the extraction (not the spool) is to blame.
            self.latch
                .fail(anyhow::anyhow!("event extraction stopped without naming an error"));
            return Flow::Stop;
        }
        for event in self.inner.events.drain(..) {
            if let Err(error) = self.spool.push(event) {
                self.latch.fail(error);
                return Flow::Stop;
            }
        }
        super::release_scratch(&mut self.inner.events);
        Flow::Continue
    }

    pub fn into_spool(mut self) -> Result<EventSpool> {
        match self.take_error() {
            Some(error) => Err(error),
            None => Ok(self.spool),
        }
    }
}

impl MessageSink for SpoolingSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take_or(&mut self.inner, "event spool")
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            self.inner.dependency(path);
        }
    }

    fn append_only_dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            self.inner.append_only_dependency(path);
        }
    }

    fn meta(&mut self, session: &Session) {
        if !self.latch.failed() {
            self.inner.meta(session);
        }
    }

    fn message(&mut self, message: Message) -> Flow {
        self.observe_message(&message)
    }

    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        !self.latch.failed() && self.inner.wants_checkpoint(offset)
    }

    fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
        !self.latch.failed() && self.inner.wants_final_checkpoint(offset)
    }

    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if !self.latch.failed() {
            self.inner.checkpoint(cp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    #[test]
    fn wide_patch_rows_spool_in_order_without_retaining_extraction_capacity() {
        let mut sink = SpoolingSink::new(None);
        let patch: String = (0..2000).map(|i| format!("*** Update File: file-{i}\n")).collect();
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(crate::Block::ToolUse {
            id: "wide".into(),
            name: "apply_patch".into(),
            namespace: None,
            input: serde_json::json!({"patch": patch}).into(),
        });
        assert_eq!(sink.observe_message(&message), Flow::Continue);
        assert!(sink.inner.events.capacity() <= 64);
        let spool = sink.into_spool().unwrap();
        let rows: Vec<_> = spool.rows().unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(rows.len(), 2000);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.target.as_deref(), Some(format!("file-{i}").as_str()));
        }
    }

    /// With nowhere to publish, a spool counts what it is handed and holds nothing: no row in
    /// memory, no byte on disk, however many arrive.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn a_spool_without_a_catalog_counts_rows_and_retains_none() {
        let mut spool = EventSpool::default();
        for i in 0..20_000 {
            spool.push(event(i, Some(format!("/repo/distinct/{i}.rs")))).unwrap();
        }
        assert_eq!(spool.len(), 20_000);
        assert_eq!(spool.temporary_bytes().unwrap(), 0);
        assert_eq!(std::mem::size_of::<EventSpool>(), std::mem::size_of::<usize>());
    }

    #[test]
    fn drained_spool_error_cannot_resume_delivery_or_publish_staged_rows() {
        let mut sink = SpoolingSink::new(None);
        sink.latch.fail(anyhow::anyhow!("temporary spool write failed"));
        assert_eq!(sink.take_error().unwrap().to_string(), "temporary spool write failed");
        assert_eq!(sink.message(Message::new(crate::Role::User)), Flow::Stop);
        assert_eq!(sink.observe_message(&Message::new(crate::Role::User)), Flow::Stop);
        assert!(!sink.wants_checkpoint(1));
        assert!(
            sink.into_spool().is_err(),
            "draining the diagnostic must not make staged output publishable"
        );
    }

    fn event(idx: usize, target: Option<String>) -> Event {
        Event {
            msg_idx: idx,
            ts: Some(idx as i64),
            kind: "file_edit",
            tool: Some("Edit".into()),
            target,
            detail: Some("unicode \u{1f9ed} \"quoted\"\nsecond line".into()),
            call: true,
            hunk_hashes: vec![0xdead_beef_cafe_babe],
        }
    }

    #[cfg(feature = "sqlite")]
    fn large_spool(start: usize, len: usize) -> EventSpool {
        let mut spool = EventSpool::default();
        for i in start..start + len {
            spool.push(event(i, Some(format!("/repo/distinct/{i}.rs")))).unwrap();
        }
        spool
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn spill_roundtrip_retains_bounded_memory_and_removes_owned_file() {
        let small = large_spool(0, 2);
        assert_eq!(small.temporary_bytes().unwrap(), 0);
        let rows: Vec<_> = small.rows().unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(
            rows,
            (0..2)
                .map(|i| event(i, Some(format!("/repo/distinct/{i}.rs"))))
                .collect::<Vec<_>>()
        );
        for count in [20_000, 200_000] {
            let spool = large_spool(0, count);
            assert_eq!(spool.len(), count);
            assert!(spool.temporary_bytes().unwrap() > 0);
            let mut rows = spool.rows().unwrap();
            for i in 0..count {
                assert_eq!(
                    rows.next().unwrap().unwrap(),
                    event(i, Some(format!("/repo/distinct/{i}.rs")))
                );
            }
            assert!(rows.next().is_none());
            drop(rows);
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn event_codec_rejects_corrupt_unknown_and_truncated_records() {
        use std::io::Cursor;
        assert!(Event::read(&mut Cursor::new(b"{broken\n"), &mut String::new()).is_err());
        assert!(decode(r#"{"msg_idx":0,"ts":null,"kind":"unknown","call":true}"#).is_err());
        let first = serde_json::to_vec(&event(0, Some("/repo/a.rs".into()))).unwrap();
        let mut reader = Cursor::new(first);
        assert!(Event::read(&mut reader, &mut String::new()).is_ok());
        assert!(
            Event::read(&mut reader, &mut String::new()).is_err(),
            "truncated prefix cannot produce another row"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn extraction_and_resume_share_the_legacy_event_checkpoint() {
        use crate::{Block, Role};
        let mut legacy = EventSink::new(Some("/repo".into()));
        let mut bounded = SpoolingSink::new(Some("/repo".into()));
        let dependency = std::env::temp_dir().join(format!("cv-absent-dependency-{}", uuid::Uuid::new_v4()));
        legacy.dependency(&dependency);
        bounded.dependency(&dependency);
        assert_eq!(
            serde_json::to_value(legacy.dependencies()).unwrap(),
            serde_json::to_value(bounded.inner().dependencies()).unwrap()
        );
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "edit".into(),
            name: "Edit".into(),
            input: serde_json::json!({"file_path":"a.rs"}).into(),
            namespace: None,
        });
        legacy.message(message.clone());
        bounded.message(message.clone());
        let mut expected = Checkpoint::default();
        let mut actual = Checkpoint::default();
        legacy.checkpoint(&mut expected);
        bounded.checkpoint(&mut actual);
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(&actual).unwrap()
        );
        let mut resumed = SpoolingSink::resume(&actual).unwrap();
        resumed.message(message.clone());
        legacy.message(message);
        let full: Vec<_> = bounded
            .into_spool()
            .unwrap()
            .rows()
            .unwrap()
            .chain(resumed.into_spool().unwrap().rows().unwrap())
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(full, legacy.into_events().unwrap());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn standalone_ingest_reports_sql_failure_and_keeps_prior_publication() {
        use crate::events::{events_for, ingest_ref, needs_ingest};
        use crate::{Harness, SessionRef};
        let world = crate::test_world("event-spool-ingest-error");
        let record = serde_json::json!({"type":"assistant","uuid":"a1","sessionId":"ingest", "message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/repo/old.rs"}}]}});
        let path = world.write_session("ingest", &[record]);
        let r = SessionRef::new("ingest", Harness::Claude, &path);
        assert_eq!(ingest_ref(&r).unwrap(), 1);
        let before = format!("{:?}", events_for("claude", "ingest", None).unwrap());
        let content = fs::read_to_string(&path).unwrap().replace("old.rs", "new-longer.rs");
        fs::write(&path, content).unwrap();
        let conn = crate::catalog::open_db().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'test'); END;",
        )
        .unwrap();
        assert!(ingest_ref(&r).is_err());
        assert_eq!(format!("{:?}", events_for("claude", "ingest", None).unwrap()), before);
        assert!(needs_ingest(&r));
        conn.execute_batch("DROP TRIGGER reject_event;").unwrap();
        assert_eq!(ingest_ref(&r).unwrap(), 1);
        assert_eq!(
            events_for("claude", "ingest", None).unwrap()[0].target.as_deref(),
            Some("/repo/new-longer.rs")
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn ordered_publication_replaces_the_suffix_and_rolls_back_failures() {
        use crate::events::{events_for, needs_ingest, record_spooled_with_source, Provenance};
        use crate::{Harness, SessionRef};
        let world = crate::test_world("event-spool-transaction");
        let path = world.proj.join("events.jsonl");
        fs::write(&path, b"original source").unwrap();
        let r = SessionRef::new("events", Harness::Claude, &path);
        let source = crate::source::Probe::default()
            .snapshot(&r)
            .with_reads(&Default::default());
        let mut original = EventSpool::default();
        for row in [
            event(0, None),
            event(2, Some("old".into())),
            event(2, Some("old".into())),
        ] {
            original.push(row).unwrap();
        }
        record_spooled_with_source(&r, 0, original, &source, &Provenance::default()).unwrap();
        assert_eq!(events_for("claude", "events", None).unwrap().len(), 3);
        let mut spool = EventSpool::default();
        spool.push(event(2, None)).unwrap();
        spool.push(event(2, Some("new".into()))).unwrap();
        spool.push(event(3, Some("new".into()))).unwrap();
        record_spooled_with_source(&r, 2, spool, &source, &Provenance::default()).unwrap();
        let rows = events_for("claude", "events", None).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].msg_idx, 0);
        assert_eq!(rows[3].msg_idx, 3);
        assert_eq!(rows[1].target, None);
        let before = format!("{rows:?}");
        assert!(!needs_ingest(&r));
        fs::write(&path, b"rewritten source, deliberately longer").unwrap();
        let changed = crate::source::Probe::default()
            .snapshot(&r)
            .with_reads(&Default::default());
        // An out-of-order row fails after the DELETE and an INSERT; neither rows nor freshness
        // may commit. A below-resume index fails by the same transactional path.
        for indices in [[4, 3], [1, 5]] {
            let mut spool = EventSpool::default();
            for i in indices {
                spool.push(event(i, Some("invalid".into()))).unwrap();
            }
            assert!(record_spooled_with_source(&r, 2, spool, &changed, &Provenance::default()).is_err());
            assert_eq!(format!("{:?}", events_for("claude", "events", None).unwrap()), before);
            assert!(needs_ingest(&r));
        }
        // Force a disk decoding failure after a valid row; all earlier INSERTs still roll back.
        let mut spool = large_spool(2, 3000);
        spool.ordered.append_corrupt(b"{broken\n").unwrap();
        assert!(record_spooled_with_source(&r, 2, spool, &changed, &Provenance::default()).is_err());
        assert_eq!(format!("{:?}", events_for("claude", "events", None).unwrap()), before);
        assert!(needs_ingest(&r));
        // Force an SQL INSERT failure, again after the suffix DELETE.
        let conn = crate::catalog::open_db().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'test'); END;",
        )
        .unwrap();
        let spool = large_spool(2, 3000);
        assert!(record_spooled_with_source(&r, 2, spool, &changed, &Provenance::default()).is_err());
        assert_eq!(format!("{:?}", events_for("claude", "events", None).unwrap()), before);
        assert!(needs_ingest(&r));
    }
}
