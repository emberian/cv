//! Per-message byte offsets — the seekable-session store (REARCH Phase 2).
//!
//! `cv show --range A-B` (and `cv blame --show`) on a 37k-message JSONL session used to stream
//! from byte 0, parsing everything before A just to throw it away. This module records, during
//! the ingest pass `cv index` already makes, the **byte offset of the record line that produced
//! each message** — so a windowed read can [`stream_range`] straight to message A and parse only
//! B−A messages.
//!
//! ## Recording
//!
//! [`MessageSink::message`] carries no byte offset, and most adapters don't know one. The two
//! adapters whose sessions are actually huge — claude and codex JSONL, which already track byte
//! positions on their span-producing lazy paths — stamp the offset into
//! `Message.extra[`[`OFFSET_KEY`]`]` under [`ParseOptions::lazy_offsets`]. An [`OffsetSink`]
//! riding the indexer's tee harvests the stamps; sessions whose messages aren't stamped (other
//! harnesses, codex legacy `.json`, non-mmap builds) simply get no seekable row and windowed
//! reads fall back to today's stream-and-skip. Graceful, never wrong.
//!
//! ## Storage
//!
//! One row per session in the shared `catalog.db` ([`crate::catalog`]): file signature
//! `(mtime_ns, size)` as the staleness guard, a small replay-metadata header, and the offsets as
//! a single BLOB of `msg_count × u64-LE`. A blob beats per-message rows because the lookup is
//! always "one session, one index": SQLite's `substr()` slices the needed 8-byte entries without
//! shipping the rest, there is exactly one row to maintain per session, and the corpus cost stays
//! 8 bytes/message (~40 MB for 5M messages) with zero per-row b-tree overhead — a `(session,
//! msg_idx, off)` table measured ~3× that in the events catalog before it was interned. Fixed
//! width (vs delta/varint) keeps the read O(1) at `idx*8` instead of an O(n) decode.
//!
//! ## Seeking
//!
//! [`stream_range`] validates the file signature (any mismatch → `Ok(false)` → caller falls back
//! to a full stream — stale offsets must never produce wrong output), seeks to `offsets[start]`,
//! and drives the adapter's record-level replay (`pub(crate)` cooperation fns inside
//! `harness::claude` / `harness::codex`). Replay correctness rests on each record line parsing
//! independently of skipped prefix state; the one codex exception — `turn_context` model-change
//! notes, which compare against the previous model — is excluded at record time (a session with
//! a model change gets a `seekable=0` row and falls back; they're rare).
//!
//! Session **metadata** for a seek-read can't come from re-parsing the head (that's the cost we're
//! avoiding), so the row stores the snapshot the recording stream's `meta()` delivered (codex:
//! title/model/cwd from `session_meta`/`turn_context`, plus *when* it arrived) and, for adapters
//! that emit no meta (claude), the first message-carried model. `stream_range` replays that as one
//! `meta()` call so header-rendering sinks see exactly what the full stream would have shown them.

use crate::ir::{Harness, Message, Session, SessionRef};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// `Message.extra` key the claude/codex adapters stamp with the message's source byte offset
/// (the start of the JSON record line that produced it) under [`ParseOptions::lazy_offsets`].
pub const OFFSET_KEY: &str = "cv_byte_offset";

/// How many preceding entries [`stream_range`] inspects to count messages sharing the seek
/// line (a record that yields several messages). Both supported adapters emit ≤1 message per
/// record today, so the count is 0 in practice; the window keeps the general case honest.
const SHARED_LINE_WINDOW: usize = 8;

/// Whether `h`'s adapter can stamp byte offsets (and be replayed from one). Exactly the
/// adapters with `pub(crate)` seek cooperation below.
pub fn supported(h: Harness) -> bool {
    matches!(h, Harness::Claude | Harness::Codex)
}

/// File `(mtime_ns, size)` — the staleness signature guarding every recorded blob.
/// `(0, 0)` when unreadable, which is never treated as fresh (mirrors
/// [`events::file_mtime_ns`](crate::events::file_mtime_ns) semantics).
pub fn file_sig(path: &Path) -> (i64, i64) {
    let Ok(md) = std::fs::metadata(path) else { return (0, 0) };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    (mtime, md.len() as i64)
}

/// Encode per-message offsets as the stored BLOB: `len × u64-LE`.
pub fn encode_offsets(offs: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(offs.len() * 8);
    for o in offs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out
}

/// Read entry `idx` of an encoded blob (the inverse of [`encode_offsets`] at one index).
pub fn offset_at(blob: &[u8], idx: usize) -> Option<u64> {
    let at = idx.checked_mul(8)?;
    let bytes: [u8; 8] = blob.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Snapshot of the one `meta()` call the recording stream delivered: the fields header-rendering
/// sinks read, plus how many messages preceded it (so replay can order it the same way).
#[derive(Debug, Clone)]
struct MetaSnap {
    /// Messages already streamed when `meta()` arrived (0 = before the body, the common case).
    idx: usize,
    title: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
}

/// A [`MessageSink`] that harvests the per-message byte-offset stamps (plus the replay metadata
/// [`stream_range`] needs) as a session streams past — designed to ride the indexer's
/// [`TeeSink`](crate::stream::TeeSink) so recording costs no extra pass. Feed it a stream made
/// with [`ParseOptions::lazy_offsets`], then hand it to [`record`].
#[derive(Default)]
pub struct OffsetSink {
    offs: Vec<u64>,
    /// Every message so far was stamped with a monotonic offset. One miss disables collection
    /// (the session can't be seeked) but the sink keeps counting for the row's bookkeeping.
    stamped: bool,
    count: usize,
    meta: Option<MetaSnap>,
    first_model: Option<(usize, String)>,
    /// A codex mid-session model change was seen. Its system note is reconstructed from the
    /// *previous* model — state a mid-file replay doesn't have — so the session is unseekable.
    hazard: bool,
}

impl OffsetSink {
    pub fn new() -> Self {
        OffsetSink {
            stamped: true,
            ..Default::default()
        }
    }

    /// Whether the stream produced a usable offset for every message (and no replay hazard).
    pub fn seekable(&self) -> bool {
        self.stamped && !self.hazard
    }

    /// Messages seen (= entries recorded when [`seekable`](OffsetSink::seekable)).
    pub fn message_count(&self) -> usize {
        self.count
    }

    /// The harvested offsets (empty once a message arrives unstamped).
    pub fn offsets(&self) -> &[u64] {
        &self.offs
    }
}

impl MessageSink for OffsetSink {
    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(MetaSnap {
                idx: self.count,
                title: s.title.clone(),
                model: s.model.clone(),
                cwd: s.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            });
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        let idx = self.count;
        self.count += 1;
        if self.stamped {
            match m.extra.get(OFFSET_KEY).and_then(Value::as_u64) {
                // Offsets must be non-decreasing (messages stream in file order); anything else
                // means a stamping bug — refuse to record rather than risk a wrong seek.
                Some(off) if self.offs.last().is_none_or(|&l| l <= off) => self.offs.push(off),
                _ => {
                    self.stamped = false;
                    self.offs = Vec::new();
                }
            }
        }
        if self.first_model.is_none() {
            if let Some(md) = &m.model {
                self.first_model = Some((idx, md.clone()));
            }
        }
        // Harness facts are nested (`extra["codex"]`), never flat — reading the flat key here
        // silently disabled hazard detection and let a model-changing codex session look seekable.
        if m.harness_extra(crate::ir::Harness::Codex)
            .and_then(|b| b.get("codex_event"))
            .and_then(Value::as_str)
            == Some("turn_context")
        {
            self.hazard = true;
        }
        Flow::Continue
    }
}

/// The session-metadata `meta()` call [`stream_range`] replays before (or, when the recording
/// stream delivered it late, after) the window. Stored snapshot when the stream emitted one;
/// otherwise synthesized to mirror the [`SessionRef`] — a no-op for meta-applying sinks — plus
/// the model the skipped prefix would have provided (claude stamps `Message::model` per
/// assistant turn; a window past the first one must still render the header's model).
#[cfg(feature = "sqlite")]
fn meta_session(r: &SessionRef, row: &db::SeekRow, start: usize) -> Session {
    let mut s = Session {
        id: r.id.clone(),
        harness: r.harness,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };
    if row.meta_idx.is_some() {
        s.title = row.meta_title.clone();
        s.model = row.meta_model.clone();
        s.cwd = row.meta_cwd.clone().map(std::path::PathBuf::from);
    } else {
        s.title = r.title.clone();
        s.cwd = r.cwd.clone();
        // Strictly before `start`: a model named by a message INSIDE the window arrives with that
        // message, so only the skipped prefix's model has to be replayed here.
        s.model = match (row.first_model_idx, &row.first_model) {
            (Some(i), Some(m)) if i < start => Some(m.clone()),
            _ => None,
        };
    }
    s
}

/// Forwards a replayed stream to `sink` as exactly the window `[start, start+remaining)`:
/// drops the `skip` leading messages that share message `start`'s record line, then stops
/// after `remaining` (when bounded).
struct WindowSink<'a> {
    inner: &'a mut dyn MessageSink,
    skip: usize,
    remaining: Option<usize>,
}

impl MessageSink for WindowSink<'_> {
    fn meta(&mut self, s: &Session) {
        self.inner.meta(s);
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.remaining == Some(0) {
            return Flow::Stop;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return Flow::Continue;
        }
        let flow = self.inner.message(m);
        if let Some(rem) = &mut self.remaining {
            *rem -= 1;
            if *rem == 0 {
                return Flow::Stop;
            }
        }
        flow
    }
}

/// Stream messages `[start, end)` of session `r` into `sink` by **seeking** to message `start`'s
/// recorded byte offset instead of parsing the file from the top.
///
/// Returns `Ok(false)` — with `sink` untouched — whenever the seek path isn't available, so the
/// caller falls back to a full stream: no recorded offsets, a stale file signature (the file
/// changed since recording — never risk wrong output), an unsupported harness, `start == 0`
/// (a full stream is already optimal), or an unopenable file (let the full path surface the
/// error its usual way). `Ok(true)` means the sink received one `meta()` call (see
/// [`meta_session`]) plus exactly the window's messages, identical to what a full stream with
/// the same [`ParseOptions`] would have delivered for those indices.
#[cfg(feature = "sqlite")]
pub fn stream_range(
    r: &SessionRef,
    start: usize,
    end: Option<usize>,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<bool> {
    if start == 0 || !supported(r.harness) {
        return Ok(false);
    }
    let (mtime_ns, size) = file_sig(&r.path);
    if mtime_ns == 0 {
        return Ok(false);
    }
    let Some(row) = db::seek_row(r, mtime_ns, size, start) else {
        return Ok(false);
    };
    let meta = meta_session(r, &row, start);
    let remaining = end.map(|e| e.saturating_sub(start));

    // Nothing (or an empty window) to replay: the header metadata is still owed.
    if start >= row.msg_count || remaining == Some(0) {
        sink.meta(&meta);
        return Ok(true);
    }

    // The seek target plus how many same-line messages precede `start` (records that yield
    // several messages). If every inspected entry shares the line and more could precede it,
    // the skip can't be bounded — fall back.
    let Some(&target) = row.window.last() else {
        return Ok(false);
    };
    let skip = row.window[..row.window.len() - 1]
        .iter()
        .rev()
        .take_while(|&&o| o == target)
        .count();
    if skip == row.window.len() - 1 && row.window_start > 0 {
        return Ok(false);
    }

    match r.harness {
        Harness::Claude => {
            use std::io::Seek;
            let Ok(mut f) = std::fs::File::open(&r.path) else {
                return Ok(false);
            };
            // Post-open size recheck (mirrors the codex arm below): the file may have changed
            // between the stat that keyed `seek_row` and this open — stale offsets, fall back.
            match f.metadata() {
                Ok(m) if m.len() as i64 == size && target < m.len() => {}
                _ => return Ok(false), // raced a file change since the stat — stale, fall back
            }
            if f.seek(std::io::SeekFrom::Start(target)).is_err() {
                return Ok(false);
            }
            // claude itself never emits meta; deliver the synthetic one first (see meta_session).
            sink.meta(&meta);
            let mut win = WindowSink {
                inner: sink,
                skip,
                remaining,
            };
            crate::harness::claude::stream_reader_from(
                &r.id,
                std::io::BufReader::new(f),
                Some(r.path.clone()),
                target,
                opts,
                &mut win,
            );
            Ok(true)
        }
        #[cfg(feature = "mmap")]
        Harness::Codex => {
            let Ok(file) = std::fs::File::open(&r.path) else {
                return Ok(false);
            };
            let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else {
                return Ok(false);
            };
            if map.len() as i64 != size || target >= map.len() as u64 {
                return Ok(false); // raced a file change since the stat — stale, fall back
            }
            // `has_events` is a head property (bounded detector) — one extra small read.
            let has_events = {
                let Ok(head) = std::fs::File::open(&r.path) else {
                    return Ok(false);
                };
                crate::harness::codex::detect_has_events(std::io::BufReader::new(head))
            };
            // Replay meta in recorded order: before the window if the stream delivered it by
            // message `start`, else after (codex emits meta as soon as the model is known —
            // EOF at the latest — and the full path does the same relative to a window).
            let meta_first = row.meta_idx.is_none_or(|mi| mi <= start);
            if meta_first {
                sink.meta(&meta);
            }
            // Model in effect at the seek point (so in-window `turn_context` records compare
            // against the right baseline; recorded sessions have no model *changes* by the
            // hazard rule, so the first/meta model is the model everywhere).
            let seed_model = if meta_first { row.meta_model.clone() } else { None };
            {
                let mut win = WindowSink {
                    inner: sink,
                    skip,
                    remaining,
                };
                crate::harness::codex::stream_spans_from(
                    &map,
                    target,
                    Some(r.path.clone()),
                    has_events,
                    seed_model,
                    opts.offsets,
                    &mut win,
                );
            }
            if !meta_first {
                sink.meta(&meta);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Without sqlite there is nowhere to record offsets — every windowed read falls back.
#[cfg(not(feature = "sqlite"))]
pub fn stream_range(
    _r: &SessionRef,
    _start: usize,
    _end: Option<usize>,
    _opts: &ParseOptions,
    _sink: &mut dyn MessageSink,
) -> Result<bool> {
    Ok(false)
}

#[cfg(feature = "sqlite")]
mod db {
    use super::*;
    use rusqlite::{params, Connection, OptionalExtension};

    /// Offsets-table schema generation. The catalog's `PRAGMA user_version` belongs to the event
    /// tables ([`crate::events`]), so this module keeps its own one-row stamp; like the rest of
    /// the catalog the table is a pure cache — migration is drop + lazy re-record on the next
    /// `cv index`.
    const OFFSETS_SCHEMA_VERSION: i64 = 1;

    /// Open the shared catalog db and ensure the offsets table exists (self-healing on a version
    /// bump). `None` on any error; callers degrade to "no offsets".
    fn open() -> Option<Connection> {
        let conn = crate::catalog::open_db()?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS msg_offsets_schema(version INTEGER NOT NULL)")
            .ok()?;
        let version: Option<i64> = conn
            .query_row("SELECT version FROM msg_offsets_schema", [], |r| r.get(0))
            .optional()
            .ok()?;
        if version != Some(OFFSETS_SCHEMA_VERSION) {
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS msg_offsets;
                 DELETE FROM msg_offsets_schema;
                 INSERT INTO msg_offsets_schema VALUES({OFFSETS_SCHEMA_VERSION});"
            ))
            .ok()?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS msg_offsets(
                 harness         TEXT NOT NULL,
                 id              TEXT NOT NULL,
                 path            TEXT NOT NULL,
                 mtime_ns        INTEGER NOT NULL,
                 size            INTEGER NOT NULL,
                 msg_count       INTEGER NOT NULL,
                 -- 0 = recorded-but-unseekable (unstamped messages or a replay hazard): keeps
                 -- the incremental skip working without retrying such sessions every index run.
                 seekable        INTEGER NOT NULL,
                 offsets         BLOB NOT NULL,
                 meta_idx        INTEGER,
                 meta_title      TEXT,
                 meta_model      TEXT,
                 meta_cwd        TEXT,
                 first_model_idx INTEGER,
                 first_model     TEXT
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_msg_offsets_key
                 ON msg_offsets(harness, id, path);",
        )
        .ok()?;
        Some(conn)
    }

    /// Persist what `sink` harvested for session `r`, keyed by the file signature **captured
    /// before the stream started** (so a file appended mid-pass reads as stale, never wrong).
    /// An unseekable result still writes a `seekable=0` row so the incremental skip-table stops
    /// re-streaming the session every run. Best-effort like the rest of the catalog.
    pub fn record(r: &SessionRef, sink: &OffsetSink, mtime_ns: i64, size: i64) {
        if mtime_ns == 0 {
            return;
        }
        let Some(conn) = open() else { return };
        let seekable = sink.seekable();
        let blob = if seekable {
            encode_offsets(&sink.offs)
        } else {
            Vec::new()
        };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO msg_offsets
             (harness,id,path,mtime_ns,size,msg_count,seekable,offsets,
              meta_idx,meta_title,meta_model,meta_cwd,first_model_idx,first_model)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                r.harness.as_str(),
                r.id,
                r.path.to_string_lossy(),
                mtime_ns,
                size,
                sink.count as i64,
                seekable,
                blob,
                sink.meta.as_ref().map(|m| m.idx as i64),
                sink.meta.as_ref().and_then(|m| m.title.clone()),
                sink.meta.as_ref().and_then(|m| m.model.clone()),
                sink.meta.as_ref().and_then(|m| m.cwd.clone()),
                sink.first_model.as_ref().map(|(i, _)| *i as i64),
                sink.first_model.as_ref().map(|(_, m)| m.clone()),
            ],
        );
    }

    /// `(harness, id, path) → (mtime_ns, size)` — the loaded skip-table rows.
    type SigRows = std::collections::HashMap<(String, String, String), (i64, i64)>;

    /// The whole offsets skip-table loaded with one query, for bulk ingest passes (the
    /// per-session alternative would re-run the catalog DDL per call, ~2,400 times a sweep).
    /// When the catalog can't be opened every check returns `false`: don't re-stream the corpus
    /// into a broken db forever (same degrade as the event catalog's).
    pub struct SyncTable {
        rows: Option<SigRows>,
    }

    impl SyncTable {
        pub fn load() -> SyncTable {
            let load = || -> Option<SigRows> {
                let conn = open()?;
                let mut stmt = conn
                    .prepare("SELECT harness, id, path, mtime_ns, size FROM msg_offsets")
                    .ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            (
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ),
                            (row.get::<_, i64>(3)?, row.get::<_, i64>(4)?),
                        ))
                    })
                    .ok()?;
                Some(rows.flatten().collect())
            };
            SyncTable { rows: load() }
        }

        /// Whether `r` needs an offsets (re)record: no row, or a signature mismatch.
        pub fn needs_record(&self, r: &SessionRef, mtime_ns: i64, size: i64) -> bool {
            let Some(rows) = &self.rows else { return false };
            let key = (
                r.harness.as_str().to_string(),
                r.id.clone(),
                r.path.to_string_lossy().into_owned(),
            );
            !(rows.get(&key) == Some(&(mtime_ns, size)) && mtime_ns != 0)
        }
    }

    /// One seek lookup, fetched in a single query.
    pub(super) struct SeekRow {
        pub msg_count: usize,
        pub meta_idx: Option<usize>,
        pub meta_title: Option<String>,
        pub meta_model: Option<String>,
        pub meta_cwd: Option<String>,
        pub first_model_idx: Option<usize>,
        pub first_model: Option<String>,
        /// Decoded `offsets[window_start..=start]` (clamped to `msg_count` entries) — the seek
        /// target plus enough context to count same-line predecessors.
        pub window: Vec<u64>,
        pub window_start: usize,
    }

    /// Fetch the seekable row for `r` **iff** its stored signature matches `(mtime_ns, size)`
    /// (staleness is decided inside the query). `substr()` slices only the needed window out of
    /// the BLOB, so a 37k-message session ships ≤72 bytes of offsets, not 300 KB.
    pub(super) fn seek_row(r: &SessionRef, mtime_ns: i64, size: i64, start: usize) -> Option<SeekRow> {
        let conn = open()?;
        let window_start = start.saturating_sub(SHARED_LINE_WINDOW);
        // SQLite substr is 1-based; clamp the slice to the blob (start ≥ msg_count reads short).
        let from = (window_start * 8 + 1) as i64;
        let len = ((start - window_start + 1) * 8) as i64;
        let (mut row, blob) = conn
            .query_row(
                "SELECT msg_count, meta_idx, meta_title, meta_model, meta_cwd,
                        first_model_idx, first_model, substr(offsets, ?5, ?6)
                 FROM msg_offsets
                 WHERE harness=?1 AND id=?2 AND path=?3
                   AND mtime_ns=?4 AND size=?7 AND seekable=1",
                params![
                    r.harness.as_str(),
                    r.id,
                    r.path.to_string_lossy(),
                    mtime_ns,
                    from,
                    len,
                    size,
                ],
                |row| {
                    Ok((
                        SeekRow {
                            msg_count: row.get::<_, i64>(0)? as usize,
                            meta_idx: row.get::<_, Option<i64>>(1)?.map(|v| v as usize),
                            meta_title: row.get(2)?,
                            meta_model: row.get(3)?,
                            meta_cwd: row.get(4)?,
                            first_model_idx: row.get::<_, Option<i64>>(5)?.map(|v| v as usize),
                            first_model: row.get(6)?,
                            window: Vec::new(),
                            window_start,
                        },
                        row.get::<_, Vec<u8>>(7)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        if start < row.msg_count {
            // An in-range start must yield the full window; anything else is a corrupt blob.
            if blob.len() != (start - window_start + 1) * 8 {
                return None;
            }
            row.window = blob
                .chunks_exact(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                .collect();
        }
        Some(row)
    }
}

#[cfg(feature = "sqlite")]
pub use db::{record, SyncTable};

/// No-op fallbacks without sqlite: recording vanishes, nothing is ever stale (mirrors
/// [`crate::events`]'s degrade).
#[cfg(not(feature = "sqlite"))]
mod db_stub {
    use super::*;
    pub fn record(_r: &SessionRef, _sink: &OffsetSink, _mtime_ns: i64, _size: i64) {}
    pub struct SyncTable;
    impl SyncTable {
        pub fn load() -> SyncTable {
            SyncTable
        }
        pub fn needs_record(&self, _r: &SessionRef, _mtime_ns: i64, _size: i64) -> bool {
            false
        }
    }
}
#[cfg(not(feature = "sqlite"))]
pub use db_stub::{record, SyncTable};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    #[test]
    fn blob_roundtrips_and_reads_at_index() {
        let offs = [0u64, 17, 17, 4096, u64::MAX - 3];
        let blob = encode_offsets(&offs);
        assert_eq!(blob.len(), offs.len() * 8);
        for (i, &o) in offs.iter().enumerate() {
            assert_eq!(offset_at(&blob, i), Some(o));
        }
        assert_eq!(offset_at(&blob, offs.len()), None);
        assert_eq!(offset_at(&[], 0), None);
    }

    fn stamped(off: u64, model: Option<&str>) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.model = model.map(str::to_string);
        m.extra.insert(OFFSET_KEY.into(), serde_json::json!(off));
        m
    }

    #[test]
    fn sink_harvests_stamps_meta_and_first_model() {
        let mut sink = OffsetSink::new();
        assert_eq!(sink.message(stamped(0, None)), Flow::Continue);
        // Meta arriving after one message records its position.
        let mut s = Session {
            id: "x".into(),
            harness: Harness::Codex,
            cwd: Some("/w".into()),
            title: None,
            created_at: None,
            updated_at: None,
            model: Some("gpt-x".into()),
            git: None,
            messages: vec![],
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };
        MessageSink::meta(&mut sink, &s);
        s.model = Some("other".to_string()); // later meta calls are ignored
        MessageSink::meta(&mut sink, &s);
        sink.message(stamped(120, Some("claude-x")));
        sink.message(stamped(120, None)); // equal offsets (same line) are fine
        assert!(sink.seekable());
        assert_eq!(sink.offsets(), &[0, 120, 120]);
        assert_eq!(sink.message_count(), 3);
        let meta = sink.meta.as_ref().unwrap();
        assert_eq!((meta.idx, meta.model.as_deref()), (1, Some("gpt-x")));
        assert_eq!(sink.first_model, Some((1, "claude-x".into())));
    }

    #[test]
    fn sink_disables_on_unstamped_or_regressing_offsets() {
        // A message with no stamp ⇒ unseekable.
        let mut sink = OffsetSink::new();
        sink.message(stamped(5, None));
        sink.message(Message::new(Role::User));
        assert!(!sink.seekable());
        assert!(sink.offsets().is_empty());
        assert_eq!(sink.message_count(), 2, "bookkeeping continues past the disable");

        // Offsets going backwards ⇒ unseekable (stamping bug guard).
        let mut sink = OffsetSink::new();
        sink.message(stamped(100, None));
        sink.message(stamped(50, None));
        assert!(!sink.seekable());
    }

    #[test]
    fn sink_flags_codex_model_change_hazard() {
        let mut sink = OffsetSink::new();
        let mut note = stamped(40, None);
        note.harness_extra_mut(crate::ir::Harness::Codex)
            .insert("codex_event".into(), serde_json::json!("turn_context"));
        sink.message(stamped(0, None));
        sink.message(note);
        assert!(!sink.seekable(), "a model-change note must mark the session unseekable");
        assert_eq!(sink.offsets(), &[0, 40], "offsets themselves are still well-formed");
    }
}
