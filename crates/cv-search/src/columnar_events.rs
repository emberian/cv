//! Derived immutable columnar `events` segments backed by `tantivy-columnar`, `tantivy-fst`,
//! and `roaring` bitmaps, with session/source revision replacement masks and 64-bit edit hunk
//! fingerprints.
//!
//! ### Segment Architecture
//! Each `.clev` segment is an immutable multi-section file containing:
//! 1. **`tantivy-columnar` table**: dictionary-encoded `Str` columns (`harness`, `session`, `kind`,
//!    `tool`, `model`, `detail`), bitpacked numeric columns (`msg_idx`, `ts`, `input_tokens`,
//!    `output_tokens`, `duration_ms`, `target_ord`), `Bool` `call`, and multivalued bitpacked `u64`
//!    `hunk_hash` fingerprints.
//! 2. **`tantivy-fst` target dictionaries**: a forward `Map` (`target -> target_ord`) for O(|path|)
//!    exact lookups (`sessions_editing`), paired with a reversed-byte `Map`
//!    (`reverse(target) -> target_ord`) turning repo-relative suffix queries (`.../src/ir.rs`) into
//!    exact FST prefix ranges without scanning unrelated targets, plus an O(1) offset string table.
//! 3. **`RoaringBitmap` hot-dimension postings** (`u32` segment-local row IDs): per `kind`,
//!    `harness`, `session`, `tool`, `target_ord`, and 64-bit `hunk_hash`.
//! 4. **Row-group min/max metadata** (`ROW_GROUP_ROWS = 4096`): `(ts_min, ts_max, msg_idx_min,
//!    msg_idx_max)` per group for fast range/suffix pruning.
//!
//! ### Revision & Suffix Replacement Semantics (`cv_core::events` parity)
//! Every atomic `manifest.json` commit pairs segment metadata with per-segment `superseded`
//! `RoaringBitmap` visibility masks:
//! - **Full replacement (`from == 0`)** (Gemini rewind, branch switch, full re-ingest): masks out
//!   all prior row IDs of `(harness, session_id)` via its `by_session` posting bitmap without
//!   re-decoding or rewriting any unrelated session.
//! - **Suffix replacement (`from > 0`)** (resumed append or rewind-and-append): checks output
//!   authority parity, then scans only `(harness, session_id)`'s row IDs in row groups with
//!   `msg_idx_max >= from`, masking out rows where `msg_idx >= from` before publishing the new
//!   delta segment so prefix rows `< from` are neither rewritten nor double-counted.

use anyhow::{Context, Result};
use cv_core::events::{EditEvent, EditHunkEvent, Event, EventRow, Provenance, SessionEditHunkRow, TouchedSession};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tantivy_columnar::{Column, ColumnType, ColumnarReader, ColumnarWriter, DynamicColumn, StrColumn};
use tantivy_fst::{IntoStreamer, Map, MapBuilder, Streamer};

const CLEV_MAGIC: &[u8; 8] = b"CVCLEV\x01\x00";
const CLEV_FOOTER_MAGIC: &[u8; 4] = b"CLV1";
const CLEV_FOOTER_BYTES: usize = 64;
const MANIFEST_VERSION: u32 = 1;
const ROW_GROUP_ROWS: u32 = 4096;
const MAX_HUNK_HASHES_PER_EDIT: usize = 64;
const BATCH_FLUSH_ROWS: usize = 16_384;

/// Additional optional row metrics carried into columnar event rows when available.
#[derive(Debug, Clone, Default)]
pub struct ColumnarRowMetrics {
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
}

/// Session-level metadata stored in the columnar manifest to satisfy catalog joins (`title`, `cwd`,
/// `created_at`, `updated_at`) without requiring a separate SQLite join when standalone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetaSnapshot {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub model: Option<String>,
}

impl SessionMetaSnapshot {
    pub fn from_ref(r: &cv_core::SessionRef) -> Self {
        Self {
            title: r.title.clone(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            model: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AuthoritySnapshot {
    parent_id: Option<String>,
    agent_id: Option<String>,
    workflow: Option<String>,
    cwd: Option<String>,
}

impl AuthoritySnapshot {
    fn of(r: &cv_core::SessionRef, prov: &Provenance) -> Self {
        Self {
            parent_id: prov.parent_id.clone(),
            agent_id: prov.agent_id.clone(),
            workflow: prov.workflow.clone(),
            cwd: r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// Min/max statistics for a fixed-size block of rows inside a segment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RowGroupStats {
    pub start_row: u32,
    pub end_row: u32,
    pub ts_min: Option<i64>,
    pub ts_max: Option<i64>,
    pub msg_idx_min: u64,
    pub msg_idx_max: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentManifestEntry {
    file: String,
    num_rows: u32,
    /// Serialized `RoaringBitmap` of superseded row IDs in this segment.
    superseded_bytes: Vec<u8>,
    row_groups: Vec<RowGroupStats>,
}

impl SegmentManifestEntry {
    fn superseded(&self) -> Result<RoaringBitmap> {
        if self.superseded_bytes.is_empty() {
            return Ok(RoaringBitmap::new());
        }
        RoaringBitmap::deserialize_from(Cursor::new(&self.superseded_bytes))
            .context("deserializing segment superseded bitmap")
    }

    fn set_superseded(&mut self, bm: &RoaringBitmap) -> Result<()> {
        self.superseded_bytes.clear();
        if !bm.is_empty() {
            bm.serialize_into(&mut self.superseded_bytes)
                .context("serializing segment superseded bitmap")?;
        }
        Ok(())
    }

    fn dead_count(&self) -> u64 {
        self.superseded().map_or(0, |bm| bm.len())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionState {
    harness: String,
    session_id: String,
    authority: AuthoritySnapshot,
    meta: SessionMetaSnapshot,
    baseline: cv_core::source::Baseline,
    /// Ordered list of segment filenames currently holding potentially live rows for this session.
    segments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ColumnarManifest {
    version: u32,
    interpretation_version: u32,
    segments: Vec<SegmentManifestEntry>,
    sessions: BTreeMap<String, SessionState>,
}

impl Default for ColumnarManifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            interpretation_version: cv_core::source::VERSION,
            segments: Vec::new(),
            sessions: BTreeMap::new(),
        }
    }
}

impl ColumnarManifest {
    fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join("manifest.json");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let manifest: Self = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
        if manifest.version != MANIFEST_VERSION || manifest.interpretation_version != cv_core::source::VERSION {
            return Ok(None);
        }
        Ok(Some(manifest))
    }

    fn save_atomic(&self, dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).context("serializing columnar events manifest")?;
        atomic_write_file(dir, "manifest.json", &bytes).context("persisting columnar events manifest")
    }
}

fn session_key(harness: &str, session_id: &str) -> String {
    format!("{harness}\0{session_id}")
}

fn atomic_write_file(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp_path = dir.join(format!(".tmp-{file_name}-{}", uuid::Uuid::new_v4().simple()));
    let res = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
        std::io::Write::write_all(&mut f, bytes).with_context(|| format!("writing {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, dir.join(file_name)).with_context(|| format!("renaming {file_name} into place"))?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    res
}

fn fnv1a_32(body: &[u8], footer_head: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in body.iter().chain(footer_head.iter()) {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    for i in (0..prefix.len()).rev() {
        if prefix[i] < 0xFF {
            let mut out = prefix[..=i].to_vec();
            out[i] += 1;
            return Some(out);
        }
    }
    None
}

fn absolutize(p: &str, cwd: Option<&Path>) -> String {
    if Path::new(p).is_absolute() {
        return p.to_string();
    }
    let trimmed = p.trim_start_matches("./");
    match cwd {
        Some(c) => c.join(trimmed).to_string_lossy().into_owned(),
        None => trimmed.to_string(),
    }
}

// ---- Segment Row Input & On-Disk Encoding ----------------------------------------------------

#[derive(Debug, Clone)]
struct StagedRow {
    harness: String,
    session_id: String,
    msg_idx: u64,
    ts: Option<i64>,
    kind: String,
    tool: Option<String>,
    target: Option<String>,
    detail: Option<String>,
    call: bool,
    hunk_hashes: Vec<u64>,
    metrics: ColumnarRowMetrics,
}

#[derive(Serialize, Deserialize)]
struct PostingsDirectory {
    by_kind: BTreeMap<String, (u64, u32)>,
    by_harness: BTreeMap<String, (u64, u32)>,
    by_session: BTreeMap<String, (u64, u32)>,
    by_tool: BTreeMap<String, (u64, u32)>,
    by_target_ord: Vec<(u64, u32)>,
    by_hunk_hash: Vec<(u64, u64, u32)>,
}

fn register_schema(writer: &mut ColumnarWriter) {
    for str_col in ["harness", "session", "kind", "tool", "model", "detail"] {
        writer.record_column_type(str_col, ColumnType::Str, false);
    }
    for u64_col in [
        "msg_idx",
        "input_tokens",
        "output_tokens",
        "duration_ms",
        "target_ord",
        "hunk_hash",
    ] {
        writer.record_column_type(u64_col, ColumnType::U64, false);
    }
    writer.record_column_type("ts", ColumnType::I64, false);
    writer.record_column_type("call", ColumnType::Bool, false);
}

fn write_clev_segment(dir: &Path, rows: &[StagedRow]) -> Result<(String, Vec<RowGroupStats>)> {
    anyhow::ensure!(!rows.is_empty(), "cannot write empty columnar event segment");
    anyhow::ensure!(
        rows.len() <= u32::MAX as usize,
        "columnar event segment exceeds u32 row capacity"
    );
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    // 1. Build sorted unique target dictionary + forward & reversed FSTs.
    let mut unique_targets: Vec<String> = rows.iter().filter_map(|r| r.target.clone()).collect();
    unique_targets.sort_unstable();
    unique_targets.dedup();

    let mut fwd_builder = MapBuilder::memory();
    for (ord, t) in unique_targets.iter().enumerate() {
        fwd_builder
            .insert(t.as_bytes(), ord as u64)
            .context("inserting into forward target FST")?;
    }
    let forward_fst_bytes = fwd_builder.into_inner().context("finishing forward target FST")?;

    let mut rev_entries: Vec<(Vec<u8>, u64)> = unique_targets
        .iter()
        .enumerate()
        .map(|(ord, t)| {
            let mut rev = t.as_bytes().to_vec();
            rev.reverse();
            (rev, ord as u64)
        })
        .collect();
    rev_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut rev_builder = MapBuilder::memory();
    for (rev_key, ord) in rev_entries {
        rev_builder
            .insert(&rev_key, ord)
            .context("inserting into reversed target FST")?;
    }
    let reverse_fst_bytes = rev_builder.into_inner().context("finishing reversed target FST")?;

    let forward_fst_view = Map::from_bytes(forward_fst_bytes.clone()).context("opening forward FST")?;

    // Target string blob + u32 end-offsets for O(1) `ord -> &str` lookup.
    let mut target_offsets = Vec::with_capacity(unique_targets.len() + 1);
    let mut target_blob = Vec::new();
    target_offsets.push(0u32);
    for t in &unique_targets {
        target_blob.extend_from_slice(t.as_bytes());
        target_offsets.push(target_blob.len() as u32);
    }

    // 2. Populate `ColumnarWriter`, `RoaringBitmap` postings, and `RowGroupStats`.
    let mut col_writer = ColumnarWriter::default();
    register_schema(&mut col_writer);

    let mut by_kind: BTreeMap<String, RoaringBitmap> = BTreeMap::new();
    let mut by_harness: BTreeMap<String, RoaringBitmap> = BTreeMap::new();
    let mut by_session: BTreeMap<String, RoaringBitmap> = BTreeMap::new();
    let mut by_tool: BTreeMap<String, RoaringBitmap> = BTreeMap::new();
    let mut by_target_ord: Vec<RoaringBitmap> = (0..unique_targets.len()).map(|_| RoaringBitmap::new()).collect();
    let mut by_hunk_hash: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
    let mut row_groups: Vec<RowGroupStats> = Vec::new();

    for (idx, r) in rows.iter().enumerate() {
        let row_id = idx as u32;
        col_writer.record_str(row_id, "harness", &r.harness);
        col_writer.record_str(row_id, "session", &r.session_id);
        col_writer.record_str(row_id, "kind", &r.kind);
        if let Some(ref tool) = r.tool {
            col_writer.record_str(row_id, "tool", tool);
            by_tool.entry(tool.clone()).or_default().insert(row_id);
        }
        if let Some(ref model) = r.metrics.model {
            col_writer.record_str(row_id, "model", model);
        }
        if let Some(ref detail) = r.detail {
            col_writer.record_str(row_id, "detail", detail);
        }
        col_writer.record_numerical(row_id, "msg_idx", r.msg_idx);
        if let Some(ts) = r.ts {
            col_writer.record_numerical(row_id, "ts", ts);
        }
        if let Some(in_tok) = r.metrics.input_tokens {
            col_writer.record_numerical(row_id, "input_tokens", in_tok);
        }
        if let Some(out_tok) = r.metrics.output_tokens {
            col_writer.record_numerical(row_id, "output_tokens", out_tok);
        }
        if let Some(dur) = r.metrics.duration_ms {
            col_writer.record_numerical(row_id, "duration_ms", dur);
        }
        col_writer.record_bool(row_id, "call", r.call);

        if let Some(ref target) = r.target {
            if let Some(ord) = forward_fst_view.get(target.as_bytes()) {
                col_writer.record_numerical(row_id, "target_ord", ord);
                by_target_ord[ord as usize].insert(row_id);
            }
        }
        for &h in &r.hunk_hashes {
            col_writer.record_numerical(row_id, "hunk_hash", h);
            by_hunk_hash.entry(h).or_default().insert(row_id);
        }

        by_kind.entry(r.kind.clone()).or_default().insert(row_id);
        by_harness.entry(r.harness.clone()).or_default().insert(row_id);
        by_session
            .entry(session_key(&r.harness, &r.session_id))
            .or_default()
            .insert(row_id);

        let group_idx = (row_id / ROW_GROUP_ROWS) as usize;
        if group_idx >= row_groups.len() {
            row_groups.push(RowGroupStats {
                start_row: row_id,
                end_row: row_id + 1,
                ts_min: r.ts,
                ts_max: r.ts,
                msg_idx_min: r.msg_idx,
                msg_idx_max: r.msg_idx,
            });
        } else {
            let g = &mut row_groups[group_idx];
            g.end_row = row_id + 1;
            g.msg_idx_min = g.msg_idx_min.min(r.msg_idx);
            g.msg_idx_max = g.msg_idx_max.max(r.msg_idx);
            if let Some(ts) = r.ts {
                g.ts_min = Some(g.ts_min.map_or(ts, |cur| cur.min(ts)));
                g.ts_max = Some(g.ts_max.map_or(ts, |cur| cur.max(ts)));
            }
        }
    }

    let mut col_bytes = Vec::new();
    col_writer
        .serialize(rows.len() as u32, &mut col_bytes)
        .context("serializing tantivy-columnar payload")?;

    // 3. Assemble binary `.clev` segment.
    let mut buf = Vec::with_capacity(col_bytes.len() + forward_fst_bytes.len() * 2 + 4096);
    buf.extend_from_slice(CLEV_MAGIC);
    buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(unique_targets.len() as u32).to_le_bytes());

    let col_offset = buf.len() as u64;
    buf.extend_from_slice(&col_bytes);
    let col_len = col_bytes.len() as u64;

    let fwd_fst_offset = buf.len() as u64;
    buf.extend_from_slice(&forward_fst_bytes);
    let fwd_fst_len = forward_fst_bytes.len() as u64;

    let rev_fst_offset = buf.len() as u64;
    buf.extend_from_slice(&reverse_fst_bytes);
    let rev_fst_len = reverse_fst_bytes.len() as u64;

    let targets_offset = buf.len() as u64;
    for &off in &target_offsets {
        buf.extend_from_slice(&off.to_le_bytes());
    }
    buf.extend_from_slice(&target_blob);

    // Serialize RoaringBitmap postings + directory.
    let mut postings_dir = PostingsDirectory {
        by_kind: BTreeMap::new(),
        by_harness: BTreeMap::new(),
        by_session: BTreeMap::new(),
        by_tool: BTreeMap::new(),
        by_target_ord: Vec::with_capacity(by_target_ord.len()),
        by_hunk_hash: Vec::with_capacity(by_hunk_hash.len()),
    };

    let write_bm = |bm: &RoaringBitmap, out: &mut Vec<u8>| -> Result<(u64, u32)> {
        let off = out.len() as u64;
        bm.serialize_into(&mut *out).context("serializing roaring posting")?;
        Ok((off, (out.len() as u64 - off) as u32))
    };

    for (k, bm) in &by_kind {
        postings_dir.by_kind.insert(k.clone(), write_bm(bm, &mut buf)?);
    }
    for (k, bm) in &by_harness {
        postings_dir.by_harness.insert(k.clone(), write_bm(bm, &mut buf)?);
    }
    for (k, bm) in &by_session {
        postings_dir.by_session.insert(k.clone(), write_bm(bm, &mut buf)?);
    }
    for (k, bm) in &by_tool {
        postings_dir.by_tool.insert(k.clone(), write_bm(bm, &mut buf)?);
    }
    for bm in &by_target_ord {
        postings_dir.by_target_ord.push(write_bm(bm, &mut buf)?);
    }
    for (&h, bm) in &by_hunk_hash {
        let (off, len) = write_bm(bm, &mut buf)?;
        postings_dir.by_hunk_hash.push((h, off, len));
    }

    let dir_offset = buf.len() as u64;
    serde_json::to_writer(&mut buf, &postings_dir).context("serializing postings directory")?;
    let dir_len = buf.len() as u64 - dir_offset;

    let mut footer = [0u8; CLEV_FOOTER_BYTES];
    footer[0..8].copy_from_slice(&col_offset.to_le_bytes());
    footer[8..16].copy_from_slice(&col_len.to_le_bytes());
    footer[16..24].copy_from_slice(&fwd_fst_offset.to_le_bytes());
    footer[24..32].copy_from_slice(&fwd_fst_len.to_le_bytes());
    footer[32..40].copy_from_slice(&rev_fst_offset.to_le_bytes());
    footer[40..48].copy_from_slice(&targets_offset.to_le_bytes());
    footer[48..56].copy_from_slice(&dir_offset.to_le_bytes());
    let crc = fnv1a_32(&buf, &footer[..56]);
    footer[56..60].copy_from_slice(&crc.to_le_bytes());
    footer[60..64].copy_from_slice(CLEV_FOOTER_MAGIC);
    let _ = (rev_fst_len, dir_len); // bounded directly by adjacent offsets & footer_start
    buf.extend_from_slice(&footer);

    let file_name = format!("seg-{}.clev", uuid::Uuid::new_v4().simple());
    atomic_write_file(dir, &file_name, &buf).context("persisting columnar event segment")?;
    Ok((file_name, row_groups))
}

struct OpenedColumns {
    harness: Option<StrColumn>,
    session: Option<StrColumn>,
    kind: Option<StrColumn>,
    tool: Option<StrColumn>,
    model: Option<StrColumn>,
    detail: Option<StrColumn>,
    msg_idx: Option<Column<u64>>,
    ts: Option<Column<i64>>,
    input_tokens: Option<Column<u64>>,
    output_tokens: Option<Column<u64>>,
    duration_ms: Option<Column<u64>>,
    target_ord: Option<Column<u64>>,
    hunk_hash: Option<Column<u64>>,
    call: Option<Column<bool>>,
}

struct ClevReader {
    bytes: Vec<u8>,
    footer_start: usize,
    num_rows: u32,
    num_targets: usize,
    targets_offset: usize,
    targets_blob_offset: usize,
    forward_fst: Map<Vec<u8>>,
    reverse_fst: Map<Vec<u8>>,
    postings: PostingsDirectory,
    cols: OpenedColumns,
}

impl ClevReader {
    fn open(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(
            bytes.len() >= 16 + CLEV_FOOTER_BYTES && &bytes[..8] == CLEV_MAGIC,
            "invalid columnar segment header in {}",
            path.display()
        );
        let footer_start = bytes.len() - CLEV_FOOTER_BYTES;
        let footer = &bytes[footer_start..];
        anyhow::ensure!(
            &footer[60..64] == CLEV_FOOTER_MAGIC,
            "invalid columnar segment footer in {}",
            path.display()
        );
        let expected_crc = u32::from_le_bytes(footer[56..60].try_into().unwrap());
        let actual_crc = fnv1a_32(&bytes[..footer_start], &footer[..56]);
        anyhow::ensure!(
            expected_crc == actual_crc,
            "corrupt columnar segment checksum in {}",
            path.display()
        );

        let num_rows = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let num_targets = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;

        let to_usize = |v: u64, what: &str| -> Result<usize> {
            usize::try_from(v).with_context(|| format!("overflowing {what} in {}", path.display()))
        };
        let checked_range = |off: usize, len: usize, limit: usize, what: &str| -> Result<&[u8]> {
            let end = off
                .checked_add(len)
                .with_context(|| format!("overflowing {what} range in {}", path.display()))?;
            anyhow::ensure!(
                end <= limit,
                "out-of-bounds {what} slice ({off}..{end} > {limit}) in {}",
                path.display()
            );
            Ok(&bytes[off..end])
        };

        let col_offset = to_usize(u64::from_le_bytes(footer[0..8].try_into().unwrap()), "col_offset")?;
        let col_len = to_usize(u64::from_le_bytes(footer[8..16].try_into().unwrap()), "col_len")?;
        let fwd_fst_offset = to_usize(u64::from_le_bytes(footer[16..24].try_into().unwrap()), "fwd_fst_offset")?;
        let fwd_fst_len = to_usize(u64::from_le_bytes(footer[24..32].try_into().unwrap()), "fwd_fst_len")?;
        let rev_fst_offset = to_usize(u64::from_le_bytes(footer[32..40].try_into().unwrap()), "rev_fst_offset")?;
        let targets_offset = to_usize(u64::from_le_bytes(footer[40..48].try_into().unwrap()), "targets_offset")?;
        let dir_offset = to_usize(u64::from_le_bytes(footer[48..56].try_into().unwrap()), "dir_offset")?;

        let col_slice = checked_range(col_offset, col_len, footer_start, "columnar")?;
        let fwd_slice = checked_range(fwd_fst_offset, fwd_fst_len, footer_start, "forward_fst")?;
        anyhow::ensure!(
            rev_fst_offset <= targets_offset && targets_offset <= dir_offset && dir_offset <= footer_start,
            "invalid section ordering in {}",
            path.display()
        );
        let offsets_bytes = num_targets
            .checked_add(1)
            .and_then(|n| n.checked_mul(4))
            .context("overflowing target offset table size")?;
        let targets_blob_offset = targets_offset
            .checked_add(offsets_bytes)
            .context("overflowing targets_blob_offset")?;
        anyhow::ensure!(
            targets_blob_offset <= dir_offset,
            "out-of-bounds target offset table in {}",
            path.display()
        );

        let columnar = ColumnarReader::open(col_slice.to_vec()).context("opening tantivy-columnar reader")?;
        let forward_fst = Map::from_bytes(fwd_slice.to_vec()).context("opening forward target FST")?;
        let reverse_fst =
            Map::from_bytes(bytes[rev_fst_offset..targets_offset].to_vec()).context("opening reversed target FST")?;
        let postings: PostingsDirectory =
            serde_json::from_slice(&bytes[dir_offset..footer_start]).context("decoding postings directory")?;

        let open_str = |name: &str| -> Result<Option<StrColumn>> {
            let handles = columnar.read_columns(name)?;
            let Some(h) = handles.first() else { return Ok(None) };
            match h.open()? {
                DynamicColumn::Str(c) => Ok(Some(c)),
                _ => Ok(None),
            }
        };
        let open_u64 = |name: &str| -> Result<Option<Column<u64>>> {
            let handles = columnar.read_columns(name)?;
            let Some(h) = handles.first() else { return Ok(None) };
            match h.open()? {
                DynamicColumn::U64(c) => Ok(Some(c)),
                _ => Ok(None),
            }
        };
        let open_i64 = |name: &str| -> Result<Option<Column<i64>>> {
            let handles = columnar.read_columns(name)?;
            let Some(h) = handles.first() else { return Ok(None) };
            match h.open()? {
                DynamicColumn::I64(c) => Ok(Some(c)),
                _ => Ok(None),
            }
        };
        let open_bool = |name: &str| -> Result<Option<Column<bool>>> {
            let handles = columnar.read_columns(name)?;
            let Some(h) = handles.first() else { return Ok(None) };
            match h.open()? {
                DynamicColumn::Bool(c) => Ok(Some(c)),
                _ => Ok(None),
            }
        };

        let cols = OpenedColumns {
            harness: open_str("harness")?,
            session: open_str("session")?,
            kind: open_str("kind")?,
            tool: open_str("tool")?,
            model: open_str("model")?,
            detail: open_str("detail")?,
            msg_idx: open_u64("msg_idx")?,
            ts: open_i64("ts")?,
            input_tokens: open_u64("input_tokens")?,
            output_tokens: open_u64("output_tokens")?,
            duration_ms: open_u64("duration_ms")?,
            target_ord: open_u64("target_ord")?,
            hunk_hash: open_u64("hunk_hash")?,
            call: open_bool("call")?,
        };

        Ok(Self {
            bytes,
            footer_start,
            num_rows,
            num_targets,
            targets_offset,
            targets_blob_offset,
            forward_fst,
            reverse_fst,
            postings,
            cols,
        })
    }

    fn decode_bm(&self, slice: Option<(u64, u32)>) -> Result<RoaringBitmap> {
        let Some((off, len)) = slice else {
            return Ok(RoaringBitmap::new());
        };
        let off = usize::try_from(off).context("overflowing columnar roaring offset")?;
        let len = len as usize;
        let end = off.checked_add(len).context("overflowing columnar roaring range")?;
        anyhow::ensure!(end <= self.footer_start, "columnar roaring slice out of bounds");
        RoaringBitmap::deserialize_from(Cursor::new(&self.bytes[off..end]))
            .context("deserializing columnar roaring bitmap")
    }

    fn by_session(&self, key: &str) -> Result<RoaringBitmap> {
        self.decode_bm(self.postings.by_session.get(key).copied())
    }

    fn by_kind(&self, kind: &str) -> Result<RoaringBitmap> {
        self.decode_bm(self.postings.by_kind.get(kind).copied())
    }

    fn by_harness(&self, harness: &str) -> Result<RoaringBitmap> {
        self.decode_bm(self.postings.by_harness.get(harness).copied())
    }

    fn by_target_ord(&self, ord: u64) -> Result<RoaringBitmap> {
        let Ok(idx) = usize::try_from(ord) else {
            return Ok(RoaringBitmap::new());
        };
        self.decode_bm(self.postings.by_target_ord.get(idx).copied())
    }

    fn by_hunk_hash(&self, hash: u64) -> Result<RoaringBitmap> {
        let idx = self.postings.by_hunk_hash.binary_search_by_key(&hash, |&(h, _, _)| h);
        match idx {
            Ok(i) => {
                let (_, off, len) = self.postings.by_hunk_hash[i];
                self.decode_bm(Some((off, len)))
            }
            Err(_) => Ok(RoaringBitmap::new()),
        }
    }

    fn target_str(&self, ord: u64) -> Option<String> {
        let i = usize::try_from(ord).ok()?;
        if i >= self.num_targets {
            return None;
        }
        let p0 = self.targets_offset.checked_add(i.checked_mul(4)?)?;
        let p1 = p0.checked_add(4)?;
        let p2 = p0.checked_add(8)?;
        let start = u32::from_le_bytes(self.bytes.get(p0..p1)?.try_into().ok()?) as usize;
        let end = u32::from_le_bytes(self.bytes.get(p1..p2)?.try_into().ok()?) as usize;
        if start > end {
            return None;
        }
        let b_start = self.targets_blob_offset.checked_add(start)?;
        let b_end = self.targets_blob_offset.checked_add(end)?;
        if b_end > self.footer_start {
            return None;
        }
        let s = std::str::from_utf8(self.bytes.get(b_start..b_end)?).ok()?;
        Some(s.to_string())
    }

    fn matching_target_ords(&self, exact_keys: &[&str], suffix: Option<&str>) -> Vec<u64> {
        let mut ords = Vec::new();
        for &exact in exact_keys {
            if let Some(ord) = self.forward_fst.get(exact.as_bytes()) {
                ords.push(ord);
            }
        }
        if let Some(sfx) = suffix {
            let mut rev_prefix = sfx.as_bytes().to_vec();
            rev_prefix.reverse();
            let mut stream = match next_prefix(&rev_prefix) {
                Some(upper) => self.reverse_fst.range().ge(&rev_prefix).lt(&upper).into_stream(),
                None => self.reverse_fst.range().ge(&rev_prefix).into_stream(),
            };
            while let Some((_k, ord)) = stream.next() {
                ords.push(ord);
            }
        }
        ords.sort_unstable();
        ords.dedup();
        ords
    }

    fn read_str(col: Option<&StrColumn>, row_id: u32) -> Result<Option<String>> {
        let Some(col) = col else { return Ok(None) };
        let Some(ord) = col.term_ords(row_id).next() else {
            return Ok(None);
        };
        let mut s = String::new();
        if col.ord_to_str(ord, &mut s)? {
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }

    fn materialize_row(&self, row_id: u32) -> Result<StagedRow> {
        let harness = Self::read_str(self.cols.harness.as_ref(), row_id)?.unwrap_or_default();
        let session_id = Self::read_str(self.cols.session.as_ref(), row_id)?.unwrap_or_default();
        let kind = Self::read_str(self.cols.kind.as_ref(), row_id)?.unwrap_or_default();
        let tool = Self::read_str(self.cols.tool.as_ref(), row_id)?;
        let model = Self::read_str(self.cols.model.as_ref(), row_id)?;
        let detail = Self::read_str(self.cols.detail.as_ref(), row_id)?;
        let msg_idx = self.cols.msg_idx.as_ref().and_then(|c| c.first(row_id)).unwrap_or(0);
        let ts = self.cols.ts.as_ref().and_then(|c| c.first(row_id));
        let input_tokens = self.cols.input_tokens.as_ref().and_then(|c| c.first(row_id));
        let output_tokens = self.cols.output_tokens.as_ref().and_then(|c| c.first(row_id));
        let duration_ms = self.cols.duration_ms.as_ref().and_then(|c| c.first(row_id));
        let target = self
            .cols
            .target_ord
            .as_ref()
            .and_then(|c| c.first(row_id))
            .and_then(|ord| self.target_str(ord));
        let hunk_hashes = self
            .cols
            .hunk_hash
            .as_ref()
            .map(|c| c.values_for_doc(row_id).collect::<Vec<u64>>())
            .unwrap_or_default();
        let call = self.cols.call.as_ref().and_then(|c| c.first(row_id)).unwrap_or(false);

        Ok(StagedRow {
            harness,
            session_id,
            msg_idx,
            ts,
            kind,
            tool,
            target,
            detail,
            call,
            hunk_hashes,
            metrics: ColumnarRowMetrics {
                model,
                input_tokens,
                output_tokens,
                duration_ms,
            },
        })
    }
}

// ---- Public Store & Bounded Batch Recorder ---------------------------------------------------

/// Derived columnar event store managing immutable `.clev` segments and revision visibility masks.
pub struct ColumnarEventStore {
    dir: PathBuf,
    manifest: ColumnarManifest,
}

impl ColumnarEventStore {
    /// Open (or initialize in-memory if not yet created) the columnar event store at `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let manifest = ColumnarManifest::load(&dir)?.unwrap_or_default();
        Ok(Self { dir, manifest })
    }

    /// Clear all segments and manifest state on a full rebuild.
    pub fn clear(&mut self) -> Result<()> {
        if cv_core::env::frozen() {
            return Ok(());
        }
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|ext| ext == "clev") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.manifest = ColumnarManifest::default();
        self.manifest.save_atomic(&self.dir)
    }

    /// Whether any live (non-superseded) event rows exist in the store.
    pub fn has_events(&self) -> bool {
        self.manifest
            .segments
            .iter()
            .any(|s| (s.num_rows as u64) > s.dead_count())
    }

    /// Number of active segment files currently tracked by the manifest.
    pub fn segment_count(&self) -> usize {
        self.manifest.segments.len()
    }

    /// Row-group min/max metadata across active segments.
    pub fn row_groups(&self) -> Vec<(String, Vec<RowGroupStats>)> {
        self.manifest
            .segments
            .iter()
            .map(|s| (s.file.clone(), s.row_groups.clone()))
            .collect()
    }

    /// Whether session `r` under `prov` needs event re-ingestion relative to its stored baseline.
    pub fn needs_ingest(&self, r: &cv_core::SessionRef, prov: &Provenance) -> bool {
        let key = session_key(r.harness.as_str(), &r.id);
        let Some(state) = self.manifest.sessions.get(&key) else {
            return true;
        };
        if state.authority != AuthoritySnapshot::of(r, prov) {
            return true;
        }
        let mut probe = cv_core::source::Probe::default();
        let now = probe.snapshot(r);
        !(state.baseline.fresh(&now, &mut probe) || state.baseline.rows_fresh(r, &now, &mut probe))
    }

    /// Record one session's events from `from` (0 = full replacement / rewind; >0 = suffix
    /// replacement from `msg_idx >= from`), updating superseded bitmasks and committing atomically.
    pub fn record_session_events(
        &mut self,
        r: &cv_core::SessionRef,
        from: usize,
        events: impl IntoIterator<Item = Result<Event>>,
        source: &cv_core::source::Baseline,
        prov: &Provenance,
        meta_override: Option<SessionMetaSnapshot>,
    ) -> Result<()> {
        if cv_core::env::frozen() {
            return Ok(());
        }
        let mut batch = self.begin_batch();
        batch.stage_session(r, from, events, source, prov, meta_override)?;
        batch.commit()
    }

    /// Remove a specific `(harness, session_id)` and mask out all of its rows across segments.
    pub fn remove_session(&mut self, harness: &str, session_id: &str) -> Result<bool> {
        if cv_core::env::frozen() {
            return Ok(false);
        }
        let removed = self.mask_session_all(harness, session_id)?;
        if removed {
            self.prune_fully_dead_segments()?;
            self.manifest.save_atomic(&self.dir)?;
        }
        Ok(removed)
    }

    /// Remove any session whose `session_id` is in `missing_ids` (used by `fts::sweep` reaping).
    pub fn remove_sessions_by_id(&mut self, missing_ids: &[String]) -> Result<usize> {
        if cv_core::env::frozen() || missing_ids.is_empty() {
            return Ok(0);
        }
        let missing_set: std::collections::HashSet<&str> = missing_ids.iter().map(String::as_str).collect();
        let targets: Vec<(String, String)> = self
            .manifest
            .sessions
            .values()
            .filter(|s| missing_set.contains(s.session_id.as_str()))
            .map(|s| (s.harness.clone(), s.session_id.clone()))
            .collect();
        let mut count = 0usize;
        for (h, id) in targets {
            if self.mask_session_all(&h, &id)? {
                count += 1;
            }
        }
        if count > 0 {
            self.prune_fully_dead_segments()?;
            self.manifest.save_atomic(&self.dir)?;
        }
        Ok(count)
    }

    /// Whether a resumed event batch (`from > 0`) for `r` under `prov` matches the stored
    /// output authority in this columnar store.
    pub fn can_resume(&self, r: &cv_core::SessionRef, prov: &Provenance) -> bool {
        let key = session_key(r.harness.as_str(), &r.id);
        self.manifest
            .sessions
            .get(&key)
            .is_some_and(|existing| existing.authority == AuthoritySnapshot::of(r, prov))
    }

    /// Begin a bounded multi-session publication batch (flushes `.clev` segments every 16k rows
    /// and commits `manifest.json` atomically once at [`ColumnarBatch::commit`]).
    pub fn begin_batch(&mut self) -> ColumnarBatch<'_> {
        ColumnarBatch {
            store: self,
            staged_rows: Vec::new(),
            pending_session_segs: BTreeMap::new(),
            new_segment_files: Vec::new(),
            dirty: false,
            committed: false,
        }
    }

    fn mask_session_all(&mut self, harness: &str, session_id: &str) -> Result<bool> {
        let key = session_key(harness, session_id);
        let Some(old_state) = self.manifest.sessions.remove(&key) else {
            return Ok(false);
        };
        self.mask_segments_for_session(&key, &old_state.segments, 0)?;
        Ok(true)
    }

    fn mask_segments_for_session(&mut self, s_key: &str, seg_files: &[String], from_msg_idx: usize) -> Result<()> {
        if seg_files.is_empty() {
            return Ok(());
        }
        let seg_set: std::collections::HashSet<&str> = seg_files.iter().map(String::as_str).collect();
        for seg_entry in &mut self.manifest.segments {
            if !seg_set.contains(seg_entry.file.as_str()) {
                continue;
            }
            if from_msg_idx > 0 && seg_entry.row_groups.iter().all(|g| g.msg_idx_max < from_msg_idx as u64) {
                continue;
            }
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let session_rows = reader.by_session(s_key)?;
            if session_rows.is_empty() {
                continue;
            }
            let mut sup = seg_entry.superseded()?;
            if from_msg_idx == 0 {
                sup |= session_rows;
            } else {
                let candidate_rows = session_rows - &sup;
                let Some(ref msg_col) = reader.cols.msg_idx else {
                    continue;
                };
                for row_id in candidate_rows {
                    let group_idx = (row_id / ROW_GROUP_ROWS) as usize;
                    if let Some(g) = seg_entry.row_groups.get(group_idx) {
                        if g.msg_idx_max < from_msg_idx as u64 {
                            continue;
                        }
                    }
                    if msg_col.first(row_id).is_some_and(|m| m >= from_msg_idx as u64) {
                        sup.insert(row_id);
                    }
                }
            }
            seg_entry.set_superseded(&sup)?;
        }
        Ok(())
    }

    fn prune_fully_dead_segments(&mut self) -> Result<Vec<String>> {
        let mut dead_files = Vec::new();
        let mut kept = Vec::with_capacity(self.manifest.segments.len());
        for seg in std::mem::take(&mut self.manifest.segments) {
            if seg.num_rows > 0 && seg.dead_count() >= seg.num_rows as u64 {
                dead_files.push(seg.file);
            } else {
                kept.push(seg);
            }
        }
        self.manifest.segments = kept;
        if !dead_files.is_empty() {
            let dead_set: std::collections::HashSet<&str> = dead_files.iter().map(String::as_str).collect();
            for st in self.manifest.sessions.values_mut() {
                st.segments.retain(|f| !dead_set.contains(f.as_str()));
            }
        }
        Ok(dead_files)
    }

    /// Compact fragmented segments (rewriting live `!superseded` rows when dead ratio > 50%, or
    /// unconditionally when `force = true` and any segment has dead rows or multiple segments exist).
    pub fn compact(&mut self, force: bool) -> Result<bool> {
        if cv_core::env::frozen() || self.manifest.segments.is_empty() {
            return Ok(false);
        }
        let total_rows: u64 = self.manifest.segments.iter().map(|s| s.num_rows as u64).sum();
        let dead_rows: u64 = self.manifest.segments.iter().map(|s| s.dead_count()).sum();
        let should_compact = if force {
            dead_rows > 0 || self.manifest.segments.len() > 1
        } else {
            self.manifest.segments.len() > 1 && total_rows > 0 && dead_rows * 2 > total_rows
        };
        if !should_compact {
            return Ok(false);
        }

        let mut surviving_rows = Vec::new();
        let mut old_files = Vec::new();
        let mut touched_sessions: BTreeMap<String, bool> = BTreeMap::new();

        for seg in std::mem::take(&mut self.manifest.segments) {
            let reader = ClevReader::open(&self.dir.join(&seg.file))?;
            let sup = seg.superseded()?;
            for row_id in 0..reader.num_rows {
                if sup.contains(row_id) {
                    continue;
                }
                let row = reader.materialize_row(row_id)?;
                touched_sessions.insert(session_key(&row.harness, &row.session_id), true);
                surviving_rows.push(row);
            }
            old_files.push(seg.file);
        }

        for st in self.manifest.sessions.values_mut() {
            st.segments.clear();
        }

        if !surviving_rows.is_empty() {
            let (seg_file, row_groups) = write_clev_segment(&self.dir, &surviving_rows)?;
            let num_rows = surviving_rows.len() as u32;
            self.manifest.segments.push(SegmentManifestEntry {
                file: seg_file.clone(),
                num_rows,
                superseded_bytes: Vec::new(),
                row_groups,
            });
            for s_key in touched_sessions.keys() {
                if let Some(st) = self.manifest.sessions.get_mut(s_key) {
                    st.segments.push(seg_file.clone());
                }
            }
        }

        self.manifest.save_atomic(&self.dir)?;
        for f in old_files {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
        Ok(true)
    }

    /// Standalone sync entry point mirroring `cv_core::events::ingest_all`: streams changed
    /// sessions (plus subagent forests when `subagents = true`) into derived columnar segments.
    pub fn sync_sessions(&mut self, refs: &[cv_core::SessionRef], force: bool, subagents: bool) -> Result<usize> {
        if cv_core::env::frozen() {
            return Ok(0);
        }
        let _sources = cv_core::source::read_scope();
        let mut candidates: Vec<(cv_core::SessionRef, Provenance)> =
            refs.iter().cloned().map(|r| (r, Provenance::default())).collect();
        if subagents {
            for r in refs {
                for sub in cv_core::subagent_tree_filtered_of(r, cv_core::SubagentDemand::Sessions, None, |_, _| true)?
                {
                    let prov = Provenance {
                        parent_id: Some(r.id.clone()),
                        agent_id: Some(sub.agent_id().to_string()),
                        workflow: sub.workflow.clone(),
                    };
                    candidates.push((sub.session, prov));
                }
            }
        }

        // Deduplicate by (harness, id) preferring existing/newest copy, matching events.rs.
        let mut dedup: HashMap<(cv_core::Harness, String), (cv_core::SessionRef, Provenance)> = HashMap::new();
        for (r, prov) in candidates {
            let key = (r.harness, r.id.clone());
            match dedup.entry(key) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((r, prov));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let (old_r, _) = e.get();
                    if r.preferred_copy_cmp(old_r).is_lt() {
                        e.insert((r, prov));
                    }
                }
            }
        }
        let mut ordered: Vec<_> = dedup.into_values().collect();
        ordered.sort_by(|(a, _), (b, _)| a.harness.as_str().cmp(b.harness.as_str()).then_with(|| a.id.cmp(&b.id)));

        let mut ingested = 0usize;
        let mut probe = cv_core::source::Probe::default();
        let mut batch = self.begin_batch();
        for (r, prov) in ordered {
            if !force && !batch.store.needs_ingest(&r, &prov) {
                continue;
            }
            let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
                continue;
            };
            let before = probe.snapshot(&r);
            let mut sink = cv_core::events::SpoolingSink::new(r.cwd.clone()).with_source(r.path.clone());
            let session_meta = adapter.try_stream(&r, &cv_core::Demand::LEAN.with_spans(), &mut sink)?;
            let source = before.with_reads(sink.inner().dependencies());
            let spool = sink.into_spool()?;
            let meta_snap = SessionMetaSnapshot {
                title: session_meta.title.or_else(|| r.title.clone()),
                cwd: r
                    .cwd
                    .as_ref()
                    .or(session_meta.cwd.as_ref())
                    .map(|p| p.display().to_string()),
                created_at: r.created_at.or(session_meta.created_at).map(|t| t.timestamp()),
                updated_at: r.updated_at.or(session_meta.updated_at).map(|t| t.timestamp()),
                model: session_meta.model,
            };
            batch.stage_session(&r, 0, spool.into_rows()?, &source, &prov, Some(meta_snap))?;
            ingested += 1;
        }
        batch.commit()?;
        Ok(ingested)
    }

    // ---- Queries (1:1 Parity with SQLite `cv_core::events`) ----------------------------------

    /// All persisted events of one session in transcript order (`ORDER BY msg_idx, kind`),
    /// optionally filtered by `kind`.
    pub fn events_for(&self, harness: &str, session_id: &str, kind: Option<&str>) -> Result<Vec<EventRow>> {
        let s_key = session_key(harness, session_id);
        let Some(state) = self.manifest.sessions.get(&s_key) else {
            return Ok(Vec::new());
        };
        let seg_lookup: HashMap<&str, (usize, &SegmentManifestEntry)> = self
            .manifest
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| (s.file.as_str(), (i, s)))
            .collect();

        let mut collected: Vec<((i64, String, usize, u32), EventRow)> = Vec::new();
        for seg_file in &state.segments {
            let Some(&(seg_ord, seg_entry)) = seg_lookup.get(seg_file.as_str()) else {
                continue;
            };
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let sup = seg_entry.superseded()?;
            let mut rows = reader.by_session(&s_key)? - &sup;
            if let Some(k) = kind {
                rows &= reader.by_kind(k)?;
            }
            for row_id in rows {
                let r = reader.materialize_row(row_id)?;
                let msg_idx = r.msg_idx as i64;
                let row_kind = r.kind.clone();
                collected.push((
                    (msg_idx, row_kind, seg_ord, row_id),
                    EventRow {
                        harness: state.harness.clone(),
                        session_id: state.session_id.clone(),
                        msg_idx,
                        ts: r.ts,
                        kind: r.kind,
                        tool: r.tool,
                        target: r.target,
                        detail: r.detail,
                        parent_id: state.authority.parent_id.clone(),
                        agent_id: state.authority.agent_id.clone(),
                        workflow: state.authority.workflow.clone(),
                    },
                ));
            }
        }
        collected.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(collected.into_iter().map(|(_, row)| row).collect())
    }

    /// Sessions whose `file_edit` / `file_read` events touched `path` (by lexical absolutization
    /// or repo-relative suffix `.../<path>`), newest activity first (`ORDER BY last_ts DESC, session_id`).
    pub fn sessions_touching(&self, path: &str, edits_only: bool) -> Result<Vec<TouchedSession>> {
        let abs = absolutize(path, cv_core::env::current_dir().as_deref());
        let trimmed = path.trim_start_matches("./");
        let suffix = if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("/{trimmed}")
        };
        self.touching_inner(&abs, Some(&suffix), edits_only)
    }

    /// Sessions that edited exact path `abs` (`edits_only = true`, zero target-table scan via forward FST).
    pub fn sessions_editing(&self, abs: &str) -> Result<Vec<TouchedSession>> {
        self.touching_inner(abs, None, true)
    }

    fn catalog_session_lookup(&self) -> HashMap<(String, String), SessionMetaSnapshot> {
        let mut out = HashMap::new();
        for r in cv_core::cataloged_sessions() {
            let entry = out
                .entry((r.harness.as_str().to_string(), r.id.clone()))
                .or_insert_with(|| SessionMetaSnapshot::from_ref(&r));
            if let Some(t) = r.title {
                if entry.title.as_deref().is_none_or(|cur| t.as_str() > cur) {
                    entry.title = Some(t);
                }
            }
        }
        out
    }

    fn touching_inner(&self, abs: &str, suffix: Option<&str>, edits_only: bool) -> Result<Vec<TouchedSession>> {
        struct Acc {
            edits: i64,
            reads: i64,
            last_ts: Option<i64>,
        }
        let mut grouped: BTreeMap<(String, String), Acc> = BTreeMap::new();

        for seg_entry in &self.manifest.segments {
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let ords = reader.matching_target_ords(&[abs], suffix);
            if ords.is_empty() {
                continue;
            }
            let mut target_bm = RoaringBitmap::new();
            for ord in ords {
                target_bm |= reader.by_target_ord(ord)?;
            }
            let file_kinds = reader.by_kind("file_edit")? | reader.by_kind("file_read")?;
            let sup = seg_entry.superseded()?;
            let matched_rows = (target_bm & file_kinds) - sup;
            for row_id in matched_rows {
                let r = reader.materialize_row(row_id)?;
                let entry = grouped.entry((r.harness, r.session_id)).or_insert(Acc {
                    edits: 0,
                    reads: 0,
                    last_ts: None,
                });
                if r.kind == "file_edit" {
                    entry.edits += 1;
                } else if r.kind == "file_read" {
                    entry.reads += 1;
                }
                if let Some(ts) = r.ts {
                    entry.last_ts = Some(entry.last_ts.map_or(ts, |cur| cur.max(ts)));
                }
            }
        }

        let cat = self.catalog_session_lookup();
        let mut out = Vec::with_capacity(grouped.len());
        for ((harness, session_id), acc) in grouped {
            if edits_only && acc.edits == 0 {
                continue;
            }
            let s_key = session_key(&harness, &session_id);
            let state = self.manifest.sessions.get(&s_key);
            let title = cat
                .get(&(harness.clone(), session_id.clone()))
                .and_then(|m| m.title.clone())
                .or_else(|| state.and_then(|s| s.meta.title.clone()));
            out.push(TouchedSession {
                harness,
                session_id,
                title,
                edits: acc.edits,
                reads: acc.reads,
                last_ts: acc.last_ts,
                parent_id: state.and_then(|s| s.authority.parent_id.clone()),
                agent_id: state.and_then(|s| s.authority.agent_id.clone()),
                workflow: state.and_then(|s| s.authority.workflow.clone()),
            });
        }

        // SQLite `ORDER BY last_ts DESC, es.session_id`: `Some` > `None`, descending `last_ts`, tie-break `session_id` ASC.
        out.sort_by(|a, b| {
            b.last_ts
                .cmp(&a.last_ts)
                .then_with(|| a.session_id.cmp(&b.session_id))
                .then_with(|| a.harness.cmp(&b.harness))
        });
        Ok(out)
    }

    /// Every `file_edit` event on `(abs, rel)` in chronological order (`ORDER BY ts, msg_idx`),
    /// grouped identically to `cv_core::events::edits_touching_file`.
    pub fn edits_touching_file(&self, abs: &str, rel: &str) -> Result<Vec<EditEvent>> {
        Ok(self
            .edits_with_hunks_touching_file(abs, rel)?
            .into_iter()
            .map(|e| e.edit)
            .collect())
    }

    /// Every `file_edit` event on `(abs, rel)` paired with its normalized 64-bit hunk hashes,
    /// matching `cv_core::events::edits_with_hunks_touching_file` 1:1.
    pub fn edits_with_hunks_touching_file(&self, abs: &str, rel: &str) -> Result<Vec<EditHunkEvent>> {
        let suffix = format!("/{rel}");
        let cat = self.catalog_session_lookup();
        let mut raw: Vec<(EditSortKey, EditHunkEvent)> = Vec::new();

        for (seg_ord, seg_entry) in self.manifest.segments.iter().enumerate() {
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let ords = reader.matching_target_ords(&[abs, rel], Some(&suffix));
            if ords.is_empty() {
                continue;
            }
            let mut target_bm = RoaringBitmap::new();
            for ord in ords {
                target_bm |= reader.by_target_ord(ord)?;
            }
            let edit_rows = (target_bm & reader.by_kind("file_edit")?) - seg_entry.superseded()?;
            for row_id in edit_rows {
                let r = reader.materialize_row(row_id)?;
                let item = self.build_edit_hunk_event(r, &cat);
                let sort_key = (item.edit.ts, item.edit.msg_idx, seg_ord, row_id);
                raw.push((sort_key, item));
            }
        }

        raw.sort_by_key(|a| a.0);
        Ok(merge_edit_hunk_events(raw.into_iter().map(|(_, item)| item)))
    }

    /// Persisted `(msg_idx, target, hunk_hashes)` rows for `file_edit` events of one session.
    pub fn edit_hunks_for(&self, harness: &str, session_id: &str) -> Result<Vec<SessionEditHunkRow>> {
        let s_key = session_key(harness, session_id);
        let Some(state) = self.manifest.sessions.get(&s_key) else {
            return Ok(Vec::new());
        };
        let seg_lookup: HashMap<&str, (usize, &SegmentManifestEntry)> = self
            .manifest
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| (s.file.as_str(), (i, s)))
            .collect();

        let mut rows_out: Vec<((i64, usize, u32), SessionEditHunkRow)> = Vec::new();
        for seg_file in &state.segments {
            let Some(&(seg_ord, seg_entry)) = seg_lookup.get(seg_file.as_str()) else {
                continue;
            };
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let rows = (reader.by_session(&s_key)? & reader.by_kind("file_edit")?) - seg_entry.superseded()?;
            for row_id in rows {
                let r = reader.materialize_row(row_id)?;
                let msg_idx = r.msg_idx as i64;
                rows_out.push(((msg_idx, seg_ord, row_id), (msg_idx, r.target, r.hunk_hashes)));
            }
        }
        rows_out.sort_by_key(|a| a.0);
        Ok(rows_out.into_iter().map(|(_, item)| item).collect())
    }

    /// Structural blame lookup: all live `file_edit` rows across the corpus whose normalized
    /// 64-bit hunk/line fingerprint postings contain `hunk_hash`.
    pub fn edits_matching_hunk(&self, hunk_hash: u64) -> Result<Vec<EditHunkEvent>> {
        let cat = self.catalog_session_lookup();
        let mut raw: Vec<(EditSortKey, EditHunkEvent)> = Vec::new();
        for (seg_ord, seg_entry) in self.manifest.segments.iter().enumerate() {
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let rows = reader.by_hunk_hash(hunk_hash)? - seg_entry.superseded()?;
            for row_id in rows {
                let r = reader.materialize_row(row_id)?;
                if r.kind != "file_edit" {
                    continue;
                }
                let item = self.build_edit_hunk_event(r, &cat);
                let sort_key = (item.edit.ts, item.edit.msg_idx, seg_ord, row_id);
                raw.push((sort_key, item));
            }
        }
        raw.sort_by_key(|a| a.0);
        Ok(merge_edit_hunk_events(raw.into_iter().map(|(_, item)| item)))
    }

    /// Tool invocation counts (`call = true` rows, or all non-null `tool` rows) grouped by tool
    /// name descending by count, optionally filtered to one `harness`.
    pub fn tool_counts(&self, harness: Option<&str>) -> Result<Vec<(String, u64)>> {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for seg_entry in &self.manifest.segments {
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let sup = seg_entry.superseded()?;
            let harness_mask = match harness {
                Some(h) => Some(reader.by_harness(h)?),
                None => None,
            };
            for (tool_name, &(off, len)) in &reader.postings.by_tool {
                let mut bm = reader.decode_bm(Some((off, len)))? - &sup;
                if let Some(ref hm) = harness_mask {
                    bm &= hm;
                }
                if let Some(ref call_col) = reader.cols.call {
                    let c = bm
                        .into_iter()
                        .filter(|&row_id| call_col.first(row_id) == Some(true))
                        .count() as u64;
                    if c > 0 {
                        *counts.entry(tool_name.clone()).or_default() += c;
                    }
                }
            }
        }
        let mut out: Vec<(String, u64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Exact [`cv_core::tools::ToolHistogram`] (`calls`, `edits`, `reads`, `commands`, `errors`,
    /// `total_calls`, `total_errors`) evaluated directly over `roaring` postings and the `call`
    /// column without re-streaming transcripts or materializing row strings.
    pub fn tool_histogram(
        &self,
        harness: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<cv_core::tools::ToolHistogram> {
        let mut hist = cv_core::tools::ToolHistogram::default();
        for seg_entry in &self.manifest.segments {
            let reader = ClevReader::open(&self.dir.join(&seg_entry.file))?;
            let sup = seg_entry.superseded()?;

            let scope_mask: Option<RoaringBitmap> = match (harness, session_id) {
                (Some(h), Some(sid)) => Some(reader.by_session(&session_key(h, sid))? - &sup),
                (Some(h), None) => Some(reader.by_harness(h)? - &sup),
                (None, Some(sid)) => {
                    let mut m = RoaringBitmap::new();
                    let suffix = format!("\0{sid}");
                    for (k, &(off, len)) in &reader.postings.by_session {
                        if k.ends_with(&suffix) {
                            m |= reader.decode_bm(Some((off, len)))?;
                        }
                    }
                    Some(m - &sup)
                }
                (None, None) => None,
            };
            if scope_mask.as_ref().is_some_and(RoaringBitmap::is_empty) {
                continue;
            }

            let apply_scope = |mut bm: RoaringBitmap| -> RoaringBitmap {
                bm -= &sup;
                if let Some(ref sm) = scope_mask {
                    bm &= sm;
                }
                bm
            };

            let error_rows = apply_scope(reader.by_kind("error")?);
            let edit_rows = apply_scope(reader.by_kind("file_edit")?);
            let read_rows = apply_scope(reader.by_kind("file_read")?);
            let cmd_rows = apply_scope(reader.by_kind("command")?);
            hist.total_errors += error_rows.len() as usize;

            for (tool_name, &(off, len)) in &reader.postings.by_tool {
                let bm = apply_scope(reader.decode_bm(Some((off, len)))?);
                if bm.is_empty() {
                    continue;
                }
                let err_bm = &bm & &error_rows;
                let non_err = &bm - &err_bm;
                let calls = match reader.cols.call {
                    Some(ref call_col) => non_err
                        .iter()
                        .filter(|&row_id| call_col.first(row_id) == Some(true))
                        .count(),
                    None => 0,
                };
                let edits = (&non_err & &edit_rows).len() as usize;
                let reads = (&non_err & &read_rows).len() as usize;
                let commands = (&non_err & &cmd_rows).len() as usize;
                let errors = err_bm.len() as usize;
                hist.total_calls += calls;
                let entry = hist.tools.entry(tool_name.clone()).or_default();
                entry.calls += calls;
                entry.edits += edits;
                entry.reads += reads;
                entry.commands += commands;
                entry.errors += errors;
            }
        }
        Ok(hist)
    }

    fn build_edit_hunk_event(
        &self,
        r: StagedRow,
        cat: &HashMap<(String, String), SessionMetaSnapshot>,
    ) -> EditHunkEvent {
        let s_key = session_key(&r.harness, &r.session_id);
        let state = self.manifest.sessions.get(&s_key);
        let cat_meta = cat.get(&(r.harness.clone(), r.session_id.clone()));
        let title = cat_meta
            .and_then(|m| m.title.clone())
            .or_else(|| state.and_then(|s| s.meta.title.clone()));
        let cwd = cat_meta
            .and_then(|m| m.cwd.clone())
            .or_else(|| state.and_then(|s| s.meta.cwd.clone()));
        let created_at = cat_meta
            .and_then(|m| m.created_at)
            .or_else(|| state.and_then(|s| s.meta.created_at));
        let updated_at = cat_meta
            .and_then(|m| m.updated_at)
            .or_else(|| state.and_then(|s| s.meta.updated_at));
        let parent_id = state.and_then(|s| s.authority.parent_id.clone());

        EditHunkEvent {
            edit: EditEvent {
                harness: r.harness,
                session_id: r.session_id,
                msg_idx: r.msg_idx as i64,
                ts: r.ts,
                target: r.target,
                title,
                cwd,
                created_at,
                updated_at,
                parent_id,
            },
            hunk_hashes: r.hunk_hashes,
        }
    }
}

type EditSortKey = (Option<i64>, i64, usize, u32);
type EditMergeKey = (String, String, i64, Option<i64>, Option<String>);

fn merge_edit_hunk_events(items: impl IntoIterator<Item = EditHunkEvent>) -> Vec<EditHunkEvent> {
    let mut merged: Vec<EditHunkEvent> = Vec::new();
    let mut key_to_idx: HashMap<EditMergeKey, usize> = HashMap::new();
    for item in items {
        let key = (
            item.edit.harness.clone(),
            item.edit.session_id.clone(),
            item.edit.msg_idx,
            item.edit.ts,
            item.edit.target.clone(),
        );
        if let Some(&idx) = key_to_idx.get(&key) {
            let dst = &mut merged[idx].hunk_hashes;
            for h in item.hunk_hashes {
                if dst.len() < MAX_HUNK_HASHES_PER_EDIT && !dst.contains(&h) {
                    dst.push(h);
                }
            }
        } else {
            key_to_idx.insert(key, merged.len());
            merged.push(item);
        }
    }
    merged
}

/// Bounded multi-session batch writer over a [`ColumnarEventStore`].
pub struct ColumnarBatch<'a> {
    store: &'a mut ColumnarEventStore,
    staged_rows: Vec<StagedRow>,
    pending_session_segs: BTreeMap<String, Vec<String>>,
    new_segment_files: Vec<String>,
    dirty: bool,
    committed: bool,
}

impl ColumnarBatch<'_> {
    /// Whether `r` under `prov` has matching stored prefix authority in this batch/store.
    pub fn can_resume(&self, r: &cv_core::SessionRef, prov: &Provenance) -> bool {
        self.store.can_resume(r, prov)
    }

    /// Mask and drop any stored entry for `r` so a future `sync_sessions` re-extracts from `0`
    /// rather than retaining stale rows when an out-of-band SQLite write bypassed `cv-search`.
    pub fn invalidate_session(&mut self, r: &cv_core::SessionRef) -> Result<()> {
        if cv_core::env::frozen() {
            return Ok(());
        }
        let s_key = session_key(r.harness.as_str(), &r.id);
        if self.pending_session_segs.contains_key(&s_key) && !self.staged_rows.is_empty() {
            self.flush_chunk()?;
        }
        if self.store.mask_session_all(r.harness.as_str(), &r.id)? {
            self.dirty = true;
        }
        Ok(())
    }

    /// Stage a session's event rows starting at `from` into the batch, streaming rows in bounded
    /// `BATCH_FLUSH_ROWS` chunks without retaining whole-session event vectors in memory.
    pub fn stage_session(
        &mut self,
        r: &cv_core::SessionRef,
        from: usize,
        events: impl IntoIterator<Item = Result<Event>>,
        source: &cv_core::source::Baseline,
        prov: &Provenance,
        meta_override: Option<SessionMetaSnapshot>,
    ) -> Result<()> {
        if cv_core::env::frozen() {
            return Ok(());
        }
        let harness = r.harness.as_str().to_string();
        let s_key = session_key(&harness, &r.id);
        let auth = AuthoritySnapshot::of(r, prov);

        if from > 0 {
            let matches_auth = self
                .store
                .manifest
                .sessions
                .get(&s_key)
                .is_some_and(|existing| existing.authority == auth);
            if !matches_auth {
                anyhow::bail!("event prefix belongs to another or unknown output authority");
            }
        }

        // Flush any earlier buffered rows for this same session if re-staged in the same batch.
        if self.pending_session_segs.contains_key(&s_key) && !self.staged_rows.is_empty() {
            self.flush_chunk()?;
        }

        // Apply superseded bitmask to prior committed segments (and any already-flushed batch segments).
        if let Some(existing) = self.store.manifest.sessions.get(&s_key).cloned() {
            self.store.mask_segments_for_session(&s_key, &existing.segments, from)?;
        }

        let prior_kept_segs = if from == 0 {
            Vec::new()
        } else {
            self.store
                .manifest
                .sessions
                .get(&s_key)
                .map_or_else(Vec::new, |s| s.segments.clone())
        };

        let meta = meta_override.unwrap_or_else(|| SessionMetaSnapshot::from_ref(r));
        let row_model = meta.model.clone();
        self.store.manifest.sessions.insert(
            s_key.clone(),
            SessionState {
                harness: harness.clone(),
                session_id: r.id.clone(),
                authority: auth,
                meta,
                baseline: source.clone(),
                segments: prior_kept_segs,
            },
        );
        self.dirty = true;

        let mut previous_idx: Option<usize> = None;
        for ev in events {
            let ev = ev?;
            if ev.msg_idx < from || previous_idx.is_some_and(|prev| ev.msg_idx < prev) {
                anyhow::bail!("event batch is not ordered from the resume index");
            }
            previous_idx = Some(ev.msg_idx);
            self.pending_session_segs.entry(s_key.clone()).or_default();
            self.staged_rows.push(StagedRow {
                harness: harness.clone(),
                session_id: r.id.clone(),
                msg_idx: ev.msg_idx as u64,
                ts: ev.ts,
                kind: ev.kind.to_string(),
                tool: ev.tool,
                target: ev.target,
                detail: ev.detail,
                call: ev.call,
                hunk_hashes: ev.hunk_hashes,
                metrics: ColumnarRowMetrics {
                    model: row_model.clone(),
                    ..Default::default()
                },
            });
            if self.staged_rows.len() >= BATCH_FLUSH_ROWS {
                self.flush_chunk()?;
            }
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> Result<()> {
        if self.staged_rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.staged_rows);
        let mut sessions_in_chunk = std::collections::HashSet::new();
        for r in &rows {
            sessions_in_chunk.insert(session_key(&r.harness, &r.session_id));
        }
        let (seg_file, row_groups) = write_clev_segment(&self.store.dir, &rows)?;
        self.new_segment_files.push(seg_file.clone());
        self.store.manifest.segments.push(SegmentManifestEntry {
            file: seg_file.clone(),
            num_rows: rows.len() as u32,
            superseded_bytes: Vec::new(),
            row_groups,
        });
        for s_key in sessions_in_chunk {
            if let Some(st) = self.store.manifest.sessions.get_mut(&s_key) {
                if !st.segments.contains(&seg_file) {
                    st.segments.push(seg_file.clone());
                }
            }
        }
        self.pending_session_segs.clear();
        Ok(())
    }

    /// Checkpoint-commit all buffered rows so far while keeping this batch open for subsequent
    /// sessions in a long-running sweep.
    pub fn checkpoint_commit(&mut self) -> Result<()> {
        if cv_core::env::frozen() || !self.dirty {
            return Ok(());
        }
        self.flush_chunk()?;
        let dead_files = self.store.prune_fully_dead_segments()?;
        if !self.store.compact(false)? {
            self.store.manifest.save_atomic(&self.store.dir)?;
        }
        self.new_segment_files.clear();
        self.dirty = false;
        for f in dead_files {
            let _ = std::fs::remove_file(self.store.dir.join(f));
        }
        Ok(())
    }

    /// Flush remaining buffered rows, prune fully superseded segments, maybe compact, and commit
    /// `manifest.json` atomically.
    pub fn commit(mut self) -> Result<()> {
        self.checkpoint_commit()?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for ColumnarBatch<'_> {
    fn drop(&mut self) {
        if !self.committed && self.dirty {
            for f in &self.new_segment_files {
                let _ = std::fs::remove_file(self.store.dir.join(f));
            }
            if let Ok(reloaded) = ColumnarManifest::load(&self.store.dir) {
                self.store.manifest = reloaded.unwrap_or_default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::{Harness, SessionRef};
    use serde_json::json;

    struct ScratchDir(PathBuf);
    impl ScratchDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("cv-clev-test-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_claude_session(path: &Path, id: &str, cwd: &str, lines: &[serde_json::Value]) -> SessionRef {
        let mut s = String::new();
        for l in lines {
            s.push_str(&serde_json::to_string(l).unwrap());
            s.push('\n');
        }
        std::fs::write(path, s).unwrap();
        let mut r = SessionRef::new(id, Harness::Claude, path);
        r.cwd = Some(PathBuf::from(cwd));
        r
    }

    fn assert_event_rows_eq(a: &[EventRow], b: &[EventRow]) {
        let proj = |rows: &[EventRow]| -> Vec<_> {
            rows.iter()
                .map(|r| {
                    (
                        r.harness.clone(),
                        r.session_id.clone(),
                        r.msg_idx,
                        r.ts,
                        r.kind.clone(),
                        r.tool.clone(),
                        r.target.clone(),
                        r.detail.clone(),
                        r.parent_id.clone(),
                        r.agent_id.clone(),
                        r.workflow.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(proj(a), proj(b));
    }

    fn assert_touched_eq(a: &[TouchedSession], b: &[TouchedSession]) {
        let proj = |rows: &[TouchedSession]| -> Vec<_> {
            rows.iter()
                .map(|r| {
                    (
                        r.harness.clone(),
                        r.session_id.clone(),
                        r.edits,
                        r.reads,
                        r.last_ts,
                        r.parent_id.clone(),
                        r.agent_id.clone(),
                        r.workflow.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(proj(a), proj(b));
    }

    fn assert_edit_hunks_eq(a: &[EditHunkEvent], b: &[EditHunkEvent]) {
        let proj = |rows: &[EditHunkEvent]| -> Vec<_> {
            rows.iter()
                .map(|r| {
                    (
                        r.edit.harness.clone(),
                        r.edit.session_id.clone(),
                        r.edit.msg_idx,
                        r.edit.ts,
                        r.edit.target.clone(),
                        r.edit.parent_id.clone(),
                        r.hunk_hashes.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(proj(a), proj(b));
    }

    #[test]
    fn columnar_events_match_sqlite_oracle_across_rewinds_suffix_replacements_and_compaction() {
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        let col_dir = home.join("events_columnar");
        let sess_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();

        // Session A: initial 3 messages with Read, Edit (with normalized hunk hash), and Bash command.
        let path_a = sess_dir.join("sess-a.jsonl");
        let s_a = write_claude_session(
            &path_a,
            "sess-a",
            "/repo/proj",
            &[
                json!({
                    "type": "assistant",
                    "sessionId": "sess-a",
                    "timestamp": "2026-10-01T10:00:00Z",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "t1",
                            "name": "Read",
                            "input": { "file_path": "src/lib.rs" }
                        }]
                    }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "sess-a",
                    "timestamp": "2026-10-01T10:01:00Z",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "t2",
                            "name": "Edit",
                            "input": {
                                "file_path": "src/lib.rs",
                                "old_string": "fn old() {}",
                                "new_string": "pub fn new_api() -> usize {\n    42\n}"
                            }
                        }]
                    }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "sess-a",
                    "timestamp": "2026-10-01T10:02:00Z",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "t3",
                            "name": "Edit",
                            "input": {
                                "file_path": "src/vanished_later.rs",
                                "old_string": "a",
                                "new_string": "let rewind_victim = 999;"
                            }
                        }]
                    }
                }),
            ],
        );

        // Session B: unrelated session touching src/lib.rs and src/other.rs with subagent provenance.
        let path_b = sess_dir.join("sess-b.jsonl");
        let s_b = write_claude_session(
            &path_b,
            "sess-b",
            "/worktree-2/proj",
            &[
                json!({
                    "type": "assistant",
                    "sessionId": "sess-b",
                    "timestamp": "2026-10-01T11:00:00Z",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {
                                "type": "tool_use",
                                "id": "tb1",
                                "name": "Write",
                                "input": {
                                    "file_path": "src/lib.rs",
                                    "content": "pub fn second_checkout_edit() {}\n"
                                }
                            },
                            {
                                "type": "tool_use",
                                "id": "tb2",
                                "name": "Bash",
                                "input": { "command": "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: src/other.rs\n@@\n-old\n+new_line_1\n*** Add File: src/extra.rs\n+pub const K: usize = 1;\n*** End Patch\nEOF" }
                            }
                        ]
                    }
                }),
                json!({
                    "type": "user",
                    "sessionId": "sess-b",
                    "timestamp": "2026-10-01T11:01:00Z",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": "tb2",
                            "is_error": true,
                            "content": "exit code 1"
                        }]
                    }
                }),
            ],
        );
        let prov_b = Provenance {
            parent_id: Some("parent-root".into()),
            agent_id: Some("worker-7".into()),
            workflow: Some("wf-alpha".into()),
        };

        let mut store = ColumnarEventStore::open(&col_dir).unwrap();

        let ingest_both =
            |store: &mut ColumnarEventStore, r: &SessionRef, from: usize, events: Vec<Event>, prov: &Provenance| {
                let snap = cv_core::source::Probe::default()
                    .snapshot(r)
                    .with_reads(&Default::default());
                cv_core::events::record_events_with_source(r, from, events.iter().cloned().map(Ok), &snap, prov)
                    .unwrap();
                store
                    .record_session_events(r, from, events.into_iter().map(Ok), &snap, prov, None)
                    .unwrap();
            };

        let extract_all = |r: &SessionRef| -> Vec<Event> {
            let ad = cv_core::harness::for_harness(r.harness).unwrap();
            let mut sink = cv_core::events::EventSink::new(r.cwd.clone()).with_source(r.path.clone());
            ad.try_stream(r, &cv_core::Demand::LEAN.with_spans(), &mut sink)
                .unwrap();
            sink.into_events().unwrap()
        };

        let ev_a0 = extract_all(&s_a);
        let ev_b0 = extract_all(&s_b);
        ingest_both(&mut store, &s_a, 0, ev_a0.clone(), &Provenance::default());
        ingest_both(&mut store, &s_b, 0, ev_b0.clone(), &prov_b);

        let assert_hist_eq = |actual: &cv_core::tools::ToolHistogram, expected: &cv_core::tools::ToolHistogram| {
            assert_eq!(actual.tools, expected.tools);
            assert_eq!(actual.total_calls, expected.total_calls);
            assert_eq!(actual.total_errors, expected.total_errors);
        };

        let verify_oracle = |store: &ColumnarEventStore, live_a: &[Event], live_b: &[Event]| {
            for (h, sid) in [("claude", "sess-a"), ("claude", "sess-b")] {
                for kind in [
                    None,
                    Some("file_edit"),
                    Some("file_read"),
                    Some("command"),
                    Some("error"),
                ] {
                    assert_event_rows_eq(
                        &store.events_for(h, sid, kind).unwrap(),
                        &cv_core::events::events_for(h, sid, kind).unwrap(),
                    );
                }
                assert_eq!(
                    store.edit_hunks_for(h, sid).unwrap(),
                    cv_core::events::edit_hunks_for(h, sid).unwrap()
                );
            }
            for path in [
                "src/lib.rs",
                "/repo/proj/src/lib.rs",
                "src/vanished_later.rs",
                "src/other.rs",
            ] {
                for edits_only in [false, true] {
                    assert_touched_eq(
                        &store.sessions_touching(path, edits_only).unwrap(),
                        &cv_core::events::sessions_touching(path, edits_only).unwrap(),
                    );
                }
            }
            assert_touched_eq(
                &store.sessions_editing("/repo/proj/src/lib.rs").unwrap(),
                &cv_core::events::sessions_editing("/repo/proj/src/lib.rs").unwrap(),
            );
            assert_edit_hunks_eq(
                &store
                    .edits_with_hunks_touching_file("/repo/proj/src/lib.rs", "src/lib.rs")
                    .unwrap(),
                &cv_core::events::edits_with_hunks_touching_file("/repo/proj/src/lib.rs", "src/lib.rs").unwrap(),
            );
            assert_edit_hunks_eq(
                &store
                    .edits_with_hunks_touching_file("/repo/proj/src/vanished_later.rs", "src/vanished_later.rs")
                    .unwrap(),
                &cv_core::events::edits_with_hunks_touching_file(
                    "/repo/proj/src/vanished_later.rs",
                    "src/vanished_later.rs",
                )
                .unwrap(),
            );

            // Verify 1:1 `ToolHistogram` + `tool_counts` parity against `ToolHistogram::from_events`
            // (including compound multi-file `Bash` `apply_patch` where `call=false` edit rows must
            // increment `edits` without inflating `calls`, plus `error` rows!).
            let exp_hist_a = cv_core::tools::ToolHistogram::from_events(live_a);
            let exp_hist_b = cv_core::tools::ToolHistogram::from_events(live_b);
            let mut exp_hist_all = exp_hist_a.clone();
            exp_hist_all.merge(&exp_hist_b);
            assert_hist_eq(
                &store.tool_histogram(Some("claude"), Some("sess-a")).unwrap(),
                &exp_hist_a,
            );
            assert_hist_eq(
                &store.tool_histogram(Some("claude"), Some("sess-b")).unwrap(),
                &exp_hist_b,
            );
            assert_hist_eq(&store.tool_histogram(None, None).unwrap(), &exp_hist_all);
            let expected_ranked_calls: Vec<(String, u64)> = exp_hist_all
                .ranked()
                .into_iter()
                .filter(|(_, c)| c.calls > 0)
                .map(|(name, c)| (name.to_string(), c.calls as u64))
                .collect();
            assert_eq!(store.tool_counts(None).unwrap(), expected_ranked_calls);
        };

        // 1. Initial parity + 64-bit hunk hash structural blame lookup!
        verify_oracle(&store, &ev_a0, &ev_b0);
        let diff_hashes = cv_core::tools::classify::extract_diff_hunk_hashes(
            "@@ -10,1 +10,3 @@\n-fn old() {}\n+pub fn new_api() -> usize {\n+    42\n+}\n",
        );
        assert!(!diff_hashes.is_empty());
        let matched_hunk = store.edits_matching_hunk(diff_hashes[0]).unwrap();
        assert_eq!(matched_hunk.len(), 1);
        assert_eq!(matched_hunk[0].edit.session_id, "sess-a");

        // UNLINK Session B's source transcript on disk BEFORE mutating Session A!
        // Proves that suffix replacement and full rewinds on Session A NEVER re-decode Session B!
        std::fs::remove_file(&path_b).unwrap();

        // 2. Suffix replacement (`from = 2`): replace msg_idx >= 2 of Session A (`vanished_later.rs`
        //    is replaced by an edit to `src/replaced_suffix.rs` + a second edit at msg_idx = 3),
        //    proving prefix 0..2 is NOT double-counted and old suffix msg_idx=2 vanishes!
        let replacement_suffix = vec![
            Event {
                msg_idx: 2,
                ts: Some(1_790_849_000),
                kind: "file_edit",
                tool: Some("Edit".into()),
                target: Some("/repo/proj/src/lib.rs".into()),
                detail: None,
                call: true,
                hunk_hashes: vec![0xDEAD_BEEF_CAFE_0001, (1u64 << 63) | 77], // includes MSB=1 u64!
            },
            Event {
                msg_idx: 3,
                ts: Some(1_790_849_060),
                kind: "command",
                tool: Some("Bash".into()),
                target: Some("cargo clippy".into()),
                detail: None,
                call: true,
                hunk_hashes: Vec::new(),
            },
        ];
        let mut live_a1: Vec<Event> = ev_a0.iter().filter(|e| e.msg_idx < 2).cloned().collect();
        live_a1.extend(replacement_suffix.iter().cloned());
        ingest_both(&mut store, &s_a, 2, replacement_suffix, &Provenance::default());
        verify_oracle(&store, &live_a1, &ev_b0);
        assert!(
            store
                .sessions_touching("src/vanished_later.rs", false)
                .unwrap()
                .is_empty(),
            "superseded suffix target must vanish completely"
        );
        assert_eq!(
            store.edits_matching_hunk((1u64 << 63) | 77).unwrap().len(),
            1,
            "high-bit u64 hunk hash must round-trip through tantivy-columnar U64 column"
        );

        // 3. Authority mismatch on `from > 0` must fail cleanly (matching SQLite events.rs).
        let bad_prov = Provenance {
            workflow: Some("wrong-authority".into()),
            ..Default::default()
        };
        let snap = cv_core::source::Probe::default()
            .snapshot(&s_a)
            .with_reads(&Default::default());
        assert!(store
            .record_session_events(&s_a, 1, std::iter::empty(), &snap, &bad_prov, None)
            .is_err());

        // 4. Full Gemini-style rewind (`from = 0` shrinking Session A down to ONLY msg_idx = 0 Read):
        //    ALL edits from Session A vanish while Session B's rows remain intact without decoding B!
        let rewound_a = vec![Event {
            msg_idx: 0,
            ts: Some(1_790_848_800),
            kind: "file_read",
            tool: Some("Read".into()),
            target: Some("/repo/proj/src/lib.rs".into()),
            detail: None,
            call: true,
            hunk_hashes: Vec::new(),
        }];
        ingest_both(&mut store, &s_a, 0, rewound_a.clone(), &Provenance::default());
        verify_oracle(&store, &rewound_a, &ev_b0);
        assert!(store.edits_matching_hunk(diff_hashes[0]).unwrap().is_empty());

        // 5. Force compaction across segments and re-verify 1:1 parity on a fresh reopened store!
        assert!(store.compact(true).unwrap());
        assert_eq!(store.segment_count(), 1);
        let reopened = ColumnarEventStore::open(&col_dir).unwrap();
        verify_oracle(&reopened, &rewound_a, &ev_b0);
    }

    #[test]
    fn columnar_events_ride_along_with_fts_index_sessions_across_append_and_rewrite() {
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        let tantivy_dir = crate::default_tantivy_dir();
        let sess_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();

        let p1 = sess_dir.join("ride-1.jsonl");
        let r1 = write_claude_session(
            &p1,
            "ride-1",
            "/repo/ride",
            &[
                json!({
                    "type": "user",
                    "sessionId": "ride-1",
                    "timestamp": "2026-10-02T09:00:00Z",
                    "message": { "role": "user", "content": "inspect parser" }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "ride-1",
                    "timestamp": "2026-10-02T09:01:00Z",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "u1",
                            "name": "Edit",
                            "input": {
                                "file_path": "src/parser.rs",
                                "old_string": "let x = 1;",
                                "new_string": "let x = 2;\nlet y = 3;"
                            }
                        }]
                    }
                }),
            ],
        );

        // 1. Initial `index_sessions` populates both SQLite catalog and derived columnar segments.
        crate::index_sessions(Some(tantivy_dir.clone()), std::slice::from_ref(&r1), false, false).unwrap();
        let col = ColumnarEventStore::open(crate::default_columnar_events_dir()).unwrap();
        assert!(col.has_events());
        assert_event_rows_eq(
            &col.events_for("claude", "ride-1", None).unwrap(),
            &cv_core::events::events_for("claude", "ride-1", None).unwrap(),
        );
        assert_touched_eq(
            &col.sessions_touching("src/parser.rs", false).unwrap(),
            &cv_core::events::sessions_touching("src/parser.rs", false).unwrap(),
        );

        // 2. Append a new turn to `ride-1.jsonl` so `fts::sweep` exercises incremental resume (`from > 0`).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&p1).unwrap();
            writeln!(
                f,
                "{}",
                json!({
                    "type": "assistant",
                    "sessionId": "ride-1",
                    "timestamp": "2026-10-02T09:05:00Z",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "u2",
                            "name": "Read",
                            "input": { "file_path": "src/lexer.rs" }
                        }]
                    }
                })
            )
            .unwrap();
        }
        crate::index_sessions(Some(tantivy_dir.clone()), std::slice::from_ref(&r1), false, false).unwrap();
        let col = ColumnarEventStore::open(crate::default_columnar_events_dir()).unwrap();
        assert_event_rows_eq(
            &col.events_for("claude", "ride-1", None).unwrap(),
            &cv_core::events::events_for("claude", "ride-1", None).unwrap(),
        );
        assert_touched_eq(
            &col.sessions_touching("src/lexer.rs", false).unwrap(),
            &cv_core::events::sessions_touching("src/lexer.rs", false).unwrap(),
        );

        // 3. Truncate/rewrite `ride-1.jsonl` (smaller file -> full `from = 0` replacement) and verify
        //    old `src/parser.rs` & `src/lexer.rs` touches disappear from columnar store too!
        let r1_rewritten = write_claude_session(
            &p1,
            "ride-1",
            "/repo/ride",
            &[json!({
                "type": "assistant",
                "sessionId": "ride-1",
                "timestamp": "2026-10-02T09:10:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "u3",
                        "name": "Read",
                        "input": { "file_path": "src/fresh.rs" }
                    }]
                }
            })],
        );
        crate::index_sessions(Some(tantivy_dir), std::slice::from_ref(&r1_rewritten), false, false).unwrap();
        let col = ColumnarEventStore::open(crate::default_columnar_events_dir()).unwrap();
        assert_event_rows_eq(
            &col.events_for("claude", "ride-1", None).unwrap(),
            &cv_core::events::events_for("claude", "ride-1", None).unwrap(),
        );
        assert!(col.sessions_touching("src/parser.rs", false).unwrap().is_empty());
        assert!(col.sessions_touching("src/lexer.rs", false).unwrap().is_empty());
        assert_eq!(col.sessions_touching("src/fresh.rs", false).unwrap().len(), 1);
    }

    #[test]
    fn columnar_events_reject_forged_segment_bounds_cleanly() {
        let tmp = ScratchDir::new();
        let rows = vec![StagedRow {
            harness: "claude".into(),
            session_id: "s1".into(),
            msg_idx: 0,
            ts: Some(100),
            kind: "file_read".into(),
            tool: Some("Read".into()),
            target: Some("/repo/a.rs".into()),
            detail: None,
            call: true,
            hunk_hashes: Vec::new(),
            metrics: ColumnarRowMetrics::default(),
        }];
        let (seg_name, _) = write_clev_segment(tmp.path(), &rows).unwrap();
        let seg_path = tmp.path().join(seg_name);
        let orig = std::fs::read(&seg_path).unwrap();
        let footer_start = orig.len() - CLEV_FOOTER_BYTES;

        // Case A: forge overflowing `col_len = u64::MAX` with recomputed valid FNV-1a checksum.
        let mut forged = orig.clone();
        forged[footer_start + 8..footer_start + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        let crc = fnv1a_32(&forged[..footer_start], &forged[footer_start..footer_start + 56]);
        forged[footer_start + 56..footer_start + 60].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&seg_path, &forged).unwrap();
        assert!(ClevReader::open(&seg_path).is_err());

        // Case B: forge huge `num_targets = u32::MAX` with recomputed valid FNV-1a checksum.
        let mut forged2 = orig;
        forged2[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let crc2 = fnv1a_32(&forged2[..footer_start], &forged2[footer_start..footer_start + 56]);
        forged2[footer_start + 56..footer_start + 60].copy_from_slice(&crc2.to_le_bytes());
        std::fs::write(&seg_path, &forged2).unwrap();
        assert!(ClevReader::open(&seg_path).is_err());
    }
}
