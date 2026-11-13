//! Exact code, symbol, and substring search via non-positional byte n-gram `RoaringBitmap`
//! segments paired with streaming exact verification against the matching decoded source revision.
//!
//! ### Candidate Algebra
//! Both index construction and query planning map UTF-8 text through the context-free monoid
//! homomorphism:
//!
//! `canon(s) = flat_map_{c in s.chars()} c.to_lowercase().map(ς -> σ)`
//!
//! Because `canon(a + b) == canon(a) + canon(b)` holds unconditionally across chunk, block, and
//! piece boundaries, streaming n-gram extraction needs only a 2-byte carry (`tail: [u8; 2]`) to
//! emit every unigram (`u8`), bigram (`u16`), and trigram (`[u8; 3]`) of each session's scoped
//! text stream without materializing full transcripts in memory.
//!
//! Furthermore, for every non-empty query `q`:
//! - If `q` is a case-sensitive substring of `doc`, `canon(q)` is a contiguous byte slice of
//!   `canon(doc)`.
//! - If `q.to_lowercase()` is a substring of `doc.to_lowercase()` (including Unicode contextual
//!   Greek final sigma), `canon(q)` is a contiguous byte slice of `canon(doc)`.
//!
//! Candidate `doc_id`s from `RoaringBitmap` intersection are therefore a 100% lossless superset
//! for both case-sensitive and case-insensitive modes, and every candidate is verified in O(window)
//! memory against the session's matching decoded source revision (`cv_core::source::Baseline`).

use crate::{Found, Hit, LiveRead, Note, Scope, SearchFilter, Source, Suffix, Unreadable};
use anyhow::{Context, Result};
use cv_core::projection::Searchable;
use cv_core::scan::{ceil_char, floor_char, lowercase_source_range, snippet, Excerpt};
use cv_core::stream::{Demand, Flow, MessageSink};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};

const SEG_MAGIC: &[u8; 8] = b"CVTRG\x00\x01\x00";
const FOOTER_MAGIC: &[u8; 4] = b"TRG1";
const FOOTER_BYTES: usize = 64;
const MANIFEST_VERSION: u32 = 1;
const PIECE_BYTES: usize = 4096;
const LEAD_BYTES: usize = 40;
const TRAIL_BYTES: usize = 120;
const PREVIEW_CHARS: usize = 240;

/// Options governing an exact trigram/substring search.
#[derive(Clone, Copy, Debug)]
pub struct TrigramOptions<'a> {
    pub filter: SearchFilter<'a>,
    pub case_sensitive: bool,
    pub live: LiveRead,
    pub limit: usize,
}

impl Default for TrigramOptions<'_> {
    fn default() -> Self {
        Self {
            filter: SearchFilter::default(),
            case_sensitive: false,
            live: LiveRead::All,
            limit: 20,
        }
    }
}

/// Summary of a trigram index synchronization pass.
#[derive(Debug, Default, Clone)]
pub struct TrigramSynced {
    pub total: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub compacted: bool,
    pub unreadable: Vec<Unreadable>,
}

/// Result of an exact trigram search, including candidate-pruning telemetry.
#[derive(Debug)]
pub struct TrigramFound {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub suffix: Suffix,
    pub notes: Vec<Note>,
    /// Segment-local session candidates surviving metadata + n-gram bitmap intersection.
    pub candidate_docs: usize,
    /// Candidates whose source revision was streamed through the exact verifier.
    pub verified_docs: usize,
}

impl TrigramFound {
    /// Convert into the unified [`Found`] search envelope.
    pub fn into_found(self) -> Found {
        Found {
            hits: self.hits,
            source: Source::Index,
            notes: self.notes,
            total: self.total,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentDocMeta {
    id: String,
    harness: String,
    path: PathBuf,
    cwd: Option<String>,
    title: String,
    preview: String,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    parent_id: Option<String>,
    agent_id: Option<String>,
    workflow: Option<String>,
    mtime: i64,
    size: i64,
    baseline: cv_core::source::Baseline,
}

impl SegmentDocMeta {
    fn to_session_ref(&self) -> Option<cv_core::SessionRef> {
        let harness = cv_core::Harness::parse(&self.harness)?;
        let mut r = cv_core::SessionRef::new(self.id.clone(), harness, self.path.clone());
        r.cwd = self.cwd.as_ref().map(PathBuf::from);
        r.title = Some(self.title.clone());
        r.created_at = self.created_at.and_then(|ts| chrono::DateTime::from_timestamp(ts, 0));
        r.updated_at = self.updated_at.and_then(|ts| chrono::DateTime::from_timestamp(ts, 0));
        Some(r)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentEntry {
    file: String,
    num_sessions: u32,
    /// Hex/base64-free compact roaring bytes stored as byte array or u32 list of deleted session ords.
    deleted_ords: Vec<u32>,
}

impl SegmentEntry {
    fn deleted_slot_bitmap(&self) -> RoaringBitmap {
        let mut bm = RoaringBitmap::new();
        for &ord in &self.deleted_ords {
            if ord < self.num_sessions {
                if let Some(base) = ord.checked_mul(2) {
                    bm.insert(base);
                    if let Some(ctx) = base.checked_add(1) {
                        bm.insert(ctx);
                    }
                }
            }
        }
        bm
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionPointer {
    segment: String,
    ord: u32,
    mtime: i64,
    size: i64,
    path: PathBuf,
    folded: bool,
    baseline: cv_core::source::Baseline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    interpretation_version: u32,
    has_subagents: bool,
    segments: Vec<SegmentEntry>,
    sessions: HashMap<String, SessionPointer>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            interpretation_version: cv_core::source::VERSION,
            has_subagents: false,
            segments: Vec::new(),
            sessions: HashMap::new(),
        }
    }
}

impl Manifest {
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
        for seg in &manifest.segments {
            anyhow::ensure!(
                seg.num_sessions <= u32::MAX / 2,
                "corrupt trigram manifest {}: segment {} num_sessions {} exceeds slot limit",
                path.display(),
                seg.file,
                seg.num_sessions
            );
            for &ord in &seg.deleted_ords {
                anyhow::ensure!(
                    ord < seg.num_sessions,
                    "corrupt trigram manifest {}: segment {} deleted ord {} >= num_sessions {}",
                    path.display(),
                    seg.file,
                    ord,
                    seg.num_sessions
                );
            }
        }
        Ok(Some(manifest))
    }

    fn save_atomic(&self, dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serializing trigram manifest")?;
        atomic_write_file(dir, "manifest.json", &bytes).context("persisting trigram manifest")
    }
}

fn atomic_write_file(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp_path = dir.join(format!(".tmp-{file_name}-{}", uuid::Uuid::new_v4().simple()));
    let write_res = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
        std::io::Write::write_all(&mut f, bytes).with_context(|| format!("writing {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, dir.join(file_name))
            .with_context(|| format!("renaming {} into place", file_name))?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_res
}

// ---- Canonical Byte N-Gram Extraction --------------------------------------------------------

fn canon_append(text: &str, out: &mut Vec<u8>) {
    if text.is_ascii() {
        out.extend(text.bytes().map(|b| b.to_ascii_lowercase()));
        return;
    }
    let mut utf8 = [0u8; 4];
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push((ch as u8).to_ascii_lowercase());
        } else {
            for lc in ch.to_lowercase() {
                let norm = if lc == '\u{03C2}' { '\u{03C3}' } else { lc };
                out.extend_from_slice(norm.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }
}

fn canon_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    canon_append(text, &mut out);
    out
}

#[inline]
fn pack_bigram(b0: u8, b1: u8) -> u16 {
    u16::from_be_bytes([b0, b1])
}

#[inline]
fn pack_trigram(b0: u8, b1: u8, b2: u8) -> u32 {
    ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32)
}

struct GramAccumulator {
    unigrams: [u64; 4],
    bigrams: Vec<u16>,
    trigrams: Vec<u32>,
    tail: [u8; 2],
    tail_len: u8,
    scratch: Vec<u8>,
}

impl Default for GramAccumulator {
    fn default() -> Self {
        Self {
            unigrams: [0; 4],
            bigrams: Vec::new(),
            trigrams: Vec::new(),
            tail: [0; 2],
            tail_len: 0,
            scratch: Vec::with_capacity(PIECE_BYTES + 16),
        }
    }
}

impl GramAccumulator {
    fn reset_boundary(&mut self) {
        self.tail_len = 0;
    }

    fn feed_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut rem = text;
        while !rem.is_empty() {
            let take = floor_char(rem, PIECE_BYTES.min(rem.len()));
            let take = if take == 0 {
                rem.chars().next().map_or(rem.len(), char::len_utf8)
            } else {
                take
            };
            self.scratch.clear();
            for i in 0..(self.tail_len as usize) {
                self.scratch.push(self.tail[i]);
            }
            let prefix_len = self.tail_len as usize;
            canon_append(&rem[..take], &mut self.scratch);
            rem = &rem[take..];

            let bytes = &self.scratch;
            for &b in &bytes[prefix_len..] {
                self.unigrams[(b >> 6) as usize] |= 1u64 << (b & 63);
            }
            if bytes.len() >= 2 {
                let start = prefix_len.saturating_sub(1);
                for i in start..(bytes.len() - 1) {
                    self.bigrams.push(pack_bigram(bytes[i], bytes[i + 1]));
                }
            }
            if bytes.len() >= 3 {
                let start = prefix_len.saturating_sub(2);
                for i in start..(bytes.len() - 2) {
                    self.trigrams.push(pack_trigram(bytes[i], bytes[i + 1], bytes[i + 2]));
                }
            }
            match bytes.len() {
                0 => self.tail_len = 0,
                1 => {
                    self.tail[0] = bytes[0];
                    self.tail_len = 1;
                }
                n => {
                    self.tail[0] = bytes[n - 2];
                    self.tail[1] = bytes[n - 1];
                    self.tail_len = 2;
                }
            }
            if self.bigrams.len() >= 8192 {
                self.bigrams.sort_unstable();
                self.bigrams.dedup();
            }
            if self.trigrams.len() >= 32768 {
                self.trigrams.sort_unstable();
                self.trigrams.dedup();
            }
        }
    }

    fn finish(&mut self) {
        self.bigrams.sort_unstable();
        self.bigrams.dedup();
        self.trigrams.sort_unstable();
        self.trigrams.dedup();
        self.scratch = Vec::new();
    }
}

#[derive(Default)]
struct PreviewAcc {
    chars: usize,
    word_open: bool,
    out: String,
}

impl PreviewAcc {
    fn push(&mut self, text: &str) {
        if self.chars >= PREVIEW_CHARS {
            return;
        }
        for c in text.chars() {
            if self.chars >= PREVIEW_CHARS {
                break;
            }
            if c.is_whitespace() {
                self.word_open = false;
            } else {
                if !self.word_open && !self.out.is_empty() {
                    self.out.push(' ');
                    self.chars += 1;
                }
                if self.chars < PREVIEW_CHARS {
                    self.out.push(c);
                    self.chars += 1;
                }
                self.word_open = true;
            }
        }
    }
}

struct IndexGramSink {
    primary_path: PathBuf,
    cwd: Option<String>,
    disc_title: Option<String>,
    meta_title: Option<String>,
    first_prompt: Option<String>,
    copied_before: Option<chrono::DateTime<chrono::Utc>>,
    resolver: cv_core::Resolver,
    dependencies: cv_core::source::ReadSet,
    in_context: bool,
    exchange: GramAccumulator,
    context: GramAccumulator,
    preview: PreviewAcc,
    err: Option<anyhow::Error>,
    failed: bool,
}

impl IndexGramSink {
    fn new(r: &cv_core::SessionRef) -> Self {
        Self {
            primary_path: cv_core::source::primary_path(r),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            disc_title: r.title.clone(),
            meta_title: None,
            first_prompt: None,
            copied_before: None,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
            dependencies: Default::default(),
            in_context: false,
            exchange: GramAccumulator::default(),
            context: GramAccumulator::default(),
            preview: PreviewAcc::default(),
            err: None,
            failed: false,
        }
    }

    fn push_piece(&mut self, text: &str) {
        if self.in_context {
            self.context.feed_str(text);
        } else {
            self.preview.push(text);
            self.exchange.feed_str(text);
        }
    }

    fn record_text(&mut self, text: &cv_core::Text) {
        match text {
            cv_core::Text::Span(sp) => {
                if let Some(path) = sp.source.as_ref().filter(|p| p.as_path() != self.primary_path) {
                    self.dependencies.record(path);
                }
            }
            cv_core::Text::Source(source) => {
                if source.path() != self.primary_path {
                    self.dependencies.record(source.path());
                }
                if let Some(reads) = source.dependencies() {
                    self.dependencies.merge(reads);
                }
            }
            cv_core::Text::Inline(_) => {}
        }
    }

    fn resolved_title(&self, returned_meta_title: Option<&str>) -> String {
        let meta = returned_meta_title
            .or(self.meta_title.as_deref())
            .or(self.disc_title.as_deref());
        cv_core::label_from(meta, self.first_prompt.as_deref())
    }
}

impl Searchable for IndexGramSink {
    fn full(&self) -> bool {
        self.failed || self.err.is_some()
    }

    fn str(&mut self, s: &str) {
        self.push_piece(s);
    }

    fn text(
        &mut self,
        text: &cv_core::Text,
        resolver: &cv_core::Resolver,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if let Some(inline) = text.inline_str() {
            self.push_piece(inline);
            return Ok(());
        }
        self.record_text(text);
        text.try_for_each_chunk(resolver, PIECE_BYTES, |part| {
            self.push_piece(part);
            anyhow::ensure!(!self.failed && self.err.is_none(), "trigram sink failed");
            Ok(())
        })
        .context("resolving trigram text")
    }

    fn json(&mut self, input: &cv_core::Json, _: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if let cv_core::Json::Source(range) = input {
            if range.path() != self.primary_path {
                self.dependencies.record(range.path());
            }
            if let Some(reads) = range.dependencies() {
                self.dependencies.merge(reads);
            }
        }
        struct JsonWriter<'a>(&'a mut IndexGramSink);
        impl std::io::Write for JsonWriter<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let s =
                    std::str::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                self.0.push_piece(s);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        input
            .write_canonical(&mut JsonWriter(self))
            .context("serializing tool input for trigram index")
    }

    fn after_block(&mut self, block: &cv_core::Block) -> Result<()> {
        if let Some(path) = block.persisted_output_path() {
            self.dependencies.record(Path::new(path));
            let read = cv_core::lazy::read_pieces_checked(Path::new(path), PIECE_BYTES, |piece| {
                self.push_piece(piece);
                Ok(())
            })
            .context("reading persisted output for trigram index")?;
            match read {
                cv_core::lazy::PersistedHead::Unreadable(reason) => {
                    anyhow::bail!("reading persisted output {path}: {reason}")
                }
                cv_core::lazy::PersistedHead::Text(_) => self.push_piece("\n"),
                cv_core::lazy::PersistedHead::Absent => {}
            }
        }
        Ok(())
    }
}

impl MessageSink for IndexGramSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.failed |= self.err.is_some();
        self.err
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("trigram sink failed")))
    }

    fn dependency(&mut self, path: &Path) {
        self.dependencies.record(path);
    }

    fn append_only_dependency(&mut self, path: &Path) {
        self.dependencies.record_append_only(path);
    }

    fn meta(&mut self, s: &cv_core::Session) {
        self.meta_title = s.title.clone();
        if self.cwd.is_none() {
            self.cwd = s.cwd.as_ref().map(|p| p.display().to_string());
        }
    }

    fn message(&mut self, m: cv_core::Message) -> Flow {
        if self.failed || self.err.is_some() {
            return Flow::Stop;
        }
        let r = std::mem::replace(&mut self.resolver, cv_core::Resolver::new(None));
        if self.first_prompt.is_none() && m.role == cv_core::Role::User {
            for b in &m.content {
                if let cv_core::Block::Text { text } = b {
                    self.record_text(text);
                }
            }
            match m.prompt_title(&r) {
                Ok(title) => self.first_prompt = title,
                Err(e) => self.err = Some(e.context("reading trigram label")),
            }
        }
        let copied = matches!((m.timestamp, self.copied_before), (Some(at), Some(before)) if at < before);
        if !copied && self.err.is_none() {
            self.in_context = !m.kind.is_exchange();
            if let Err(e) = cv_core::projection::searchable(&m.content, &r, self, &mut || Ok(())) {
                if self.err.is_none() {
                    self.err = Some(e);
                }
            }
        }
        self.resolver = r;
        if self.failed || self.err.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

struct ExtractedDoc {
    meta: SegmentDocMeta,
    exchange: GramAccumulator,
    context: GramAccumulator,
}

fn extract_session_grams(r: &cv_core::SessionRef, prov: &cv_core::events::Provenance) -> Result<ExtractedDoc> {
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("unknown harness {}", r.harness))?;
    let mut probe = cv_core::source::Probe::default();
    let before = probe.snapshot(r);
    let (mtime, size) = before.primary.map_or((0, -1), |id| (id.mtime_ns as i64, id.len as i64));

    let mut sink = IndexGramSink::new(r);
    sink.copied_before = adapter.copied_before(r, &mut sink);
    let session_meta = adapter
        .try_stream(r, &Demand::LEAN.with_spans(), &mut sink)
        .with_context(|| format!("indexing trigram session {}:{}", r.harness, r.id))?;
    if let Some(err) = sink.take_error() {
        return Err(err);
    }
    let title = sink.resolved_title(session_meta.title.as_deref());
    if !title.is_empty() {
        sink.exchange.reset_boundary();
        sink.exchange.feed_str(&title);
    }
    sink.exchange.finish();
    sink.context.finish();

    let baseline = before.with_reads(&sink.dependencies);
    let meta = SegmentDocMeta {
        id: r.id.clone(),
        harness: r.harness.as_str().to_string(),
        path: r.path.clone(),
        cwd: sink.cwd.or_else(|| session_meta.cwd.map(|p| p.display().to_string())),
        title,
        preview: sink.preview.out,
        created_at: r.created_at.or(session_meta.created_at).map(|t| t.timestamp()),
        updated_at: r.updated_at.or(session_meta.updated_at).map(|t| t.timestamp()),
        parent_id: prov.parent_id.clone(),
        agent_id: prov.agent_id.clone(),
        workflow: prov.workflow.clone(),
        mtime,
        size,
        baseline,
    };
    Ok(ExtractedDoc {
        meta,
        exchange: sink.exchange,
        context: sink.context,
    })
}

// ---- Segment Binary Writer & Reader ----------------------------------------------------------

fn write_segment(dir: &Path, docs: Vec<ExtractedDoc>) -> Result<String> {
    anyhow::ensure!(!docs.is_empty(), "cannot write empty trigram segment");
    anyhow::ensure!(
        docs.len() <= (u32::MAX as usize) / 2,
        "segment session count exceeds u32 slot capacity"
    );
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut unigrams: Vec<RoaringBitmap> = (0..256).map(|_| RoaringBitmap::new()).collect();
    let mut bigrams: BTreeMap<u16, RoaringBitmap> = BTreeMap::new();
    let mut trigrams: BTreeMap<u32, RoaringBitmap> = BTreeMap::new();
    let mut metas = Vec::with_capacity(docs.len());

    for (idx, doc) in docs.into_iter().enumerate() {
        let ord = idx as u32;
        for (slot, grams) in [(ord * 2, doc.exchange), (ord * 2 + 1, doc.context)] {
            for word_idx in 0..4 {
                let mut bits = grams.unigrams[word_idx];
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    unigrams[word_idx * 64 + bit].insert(slot);
                    bits &= bits - 1;
                }
            }
            for bg in grams.bigrams {
                bigrams.entry(bg).or_default().insert(slot);
            }
            for tg in grams.trigrams {
                trigrams.entry(tg).or_default().insert(slot);
            }
        }
        metas.push(doc.meta);
    }

    let mut buf = Vec::with_capacity(64 * 1024);
    buf.extend_from_slice(SEG_MAGIC);
    buf.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved alignment

    // Unigrams: 256 * 12 bytes directory + payloads
    let unigram_dir_offset = buf.len() as u64;
    let unigram_dir_bytes = 256 * 12;
    buf.resize(buf.len() + unigram_dir_bytes, 0);
    for (b, bm) in unigrams.iter().enumerate() {
        if bm.is_empty() {
            continue;
        }
        let off = buf.len() as u64;
        bm.serialize_into(&mut buf).context("serializing unigram roaring")?;
        let len = (buf.len() as u64 - off) as u32;
        let entry_pos = unigram_dir_offset as usize + b * 12;
        buf[entry_pos..entry_pos + 8].copy_from_slice(&off.to_le_bytes());
        buf[entry_pos + 8..entry_pos + 12].copy_from_slice(&len.to_le_bytes());
    }

    // Bigrams: num_bigrams * 16 bytes (u16 gram, u16 pad, u64 off, u32 len) + payloads
    let bigram_dir_offset = buf.len() as u64;
    let num_bigrams = bigrams.len() as u32;
    buf.resize(buf.len() + (num_bigrams as usize) * 16, 0);
    for (idx, (gram, bm)) in bigrams.iter().enumerate() {
        let off = buf.len() as u64;
        bm.serialize_into(&mut buf).context("serializing bigram roaring")?;
        let len = (buf.len() as u64 - off) as u32;
        let entry_pos = bigram_dir_offset as usize + idx * 16;
        buf[entry_pos..entry_pos + 2].copy_from_slice(&gram.to_le_bytes());
        buf[entry_pos + 4..entry_pos + 12].copy_from_slice(&off.to_le_bytes());
        buf[entry_pos + 12..entry_pos + 16].copy_from_slice(&len.to_le_bytes());
    }

    // Trigrams: num_trigrams * 16 bytes (u32 gram, u32 len, u64 off) + payloads
    let trigram_dir_offset = buf.len() as u64;
    let num_trigrams = trigrams.len() as u32;
    buf.resize(buf.len() + (num_trigrams as usize) * 16, 0);
    for (idx, (gram, bm)) in trigrams.iter().enumerate() {
        let off = buf.len() as u64;
        bm.serialize_into(&mut buf).context("serializing trigram roaring")?;
        let len = (buf.len() as u64 - off) as u32;
        let entry_pos = trigram_dir_offset as usize + idx * 16;
        buf[entry_pos..entry_pos + 4].copy_from_slice(&gram.to_le_bytes());
        buf[entry_pos + 4..entry_pos + 8].copy_from_slice(&len.to_le_bytes());
        buf[entry_pos + 8..entry_pos + 16].copy_from_slice(&off.to_le_bytes());
    }

    // Metadata JSON
    let meta_offset = buf.len() as u64;
    serde_json::to_writer(&mut buf, &metas).context("serializing segment metadata")?;
    let meta_len = buf.len() as u64 - meta_offset;

    // Footer (64 bytes)
    let mut footer = [0u8; FOOTER_BYTES];
    footer[0..8].copy_from_slice(&unigram_dir_offset.to_le_bytes());
    footer[8..16].copy_from_slice(&bigram_dir_offset.to_le_bytes());
    footer[16..20].copy_from_slice(&num_bigrams.to_le_bytes());
    footer[24..32].copy_from_slice(&trigram_dir_offset.to_le_bytes());
    footer[32..36].copy_from_slice(&num_trigrams.to_le_bytes());
    footer[40..48].copy_from_slice(&meta_offset.to_le_bytes());
    footer[48..56].copy_from_slice(&meta_len.to_le_bytes());
    let crc = fnv1a_32(&buf, &footer[..56]);
    footer[56..60].copy_from_slice(&crc.to_le_bytes());
    footer[60..64].copy_from_slice(FOOTER_MAGIC);
    buf.extend_from_slice(&footer);

    let file_name = format!("seg-{}.trg", uuid::Uuid::new_v4().simple());
    atomic_write_file(dir, &file_name, &buf).context("persisting trigram segment")?;
    Ok(file_name)
}

fn fnv1a_32(body: &[u8], footer_head: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in body.iter().chain(footer_head.iter()) {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

struct SegmentReader {
    bytes: Vec<u8>,
    num_sessions: u32,
    unigram_dir_offset: usize,
    bigram_dir_offset: usize,
    num_bigrams: usize,
    trigram_dir_offset: usize,
    num_trigrams: usize,
    metas: Vec<SegmentDocMeta>,
}

impl SegmentReader {
    fn open(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(
            bytes.len() >= 16 + FOOTER_BYTES && &bytes[..8] == SEG_MAGIC,
            "invalid trigram segment header in {}",
            path.display()
        );
        let footer_start = bytes.len() - FOOTER_BYTES;
        let footer = &bytes[footer_start..];
        anyhow::ensure!(
            &footer[60..64] == FOOTER_MAGIC,
            "invalid trigram segment footer in {}",
            path.display()
        );
        let expected_crc = u32::from_le_bytes(footer[56..60].try_into().unwrap());
        let actual_crc = fnv1a_32(&bytes[..footer_start], &footer[..56]);
        anyhow::ensure!(
            expected_crc == actual_crc,
            "corrupt trigram segment checksum in {}",
            path.display()
        );

        let to_usize = |v: u64, what: &str| -> Result<usize> {
            usize::try_from(v).with_context(|| format!("overflowing {what} in {}", path.display()))
        };
        let check_dir = |off: usize, len: usize, what: &str| -> Result<()> {
            let end = off
                .checked_add(len)
                .with_context(|| format!("overflowing {what} range in {}", path.display()))?;
            anyhow::ensure!(
                end <= footer_start,
                "out-of-bounds {what} directory ({off}..{end} > {footer_start}) in {}",
                path.display()
            );
            Ok(())
        };

        let num_sessions = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let unigram_dir_offset = to_usize(
            u64::from_le_bytes(footer[0..8].try_into().unwrap()),
            "unigram_dir_offset",
        )?;
        let bigram_dir_offset = to_usize(
            u64::from_le_bytes(footer[8..16].try_into().unwrap()),
            "bigram_dir_offset",
        )?;
        let num_bigrams = u32::from_le_bytes(footer[16..20].try_into().unwrap()) as usize;
        let trigram_dir_offset = to_usize(
            u64::from_le_bytes(footer[24..32].try_into().unwrap()),
            "trigram_dir_offset",
        )?;
        let num_trigrams = u32::from_le_bytes(footer[32..36].try_into().unwrap()) as usize;
        let meta_offset = to_usize(u64::from_le_bytes(footer[40..48].try_into().unwrap()), "meta_offset")?;
        let meta_len = to_usize(u64::from_le_bytes(footer[48..56].try_into().unwrap()), "meta_len")?;

        check_dir(unigram_dir_offset, 256 * 12, "unigram")?;
        check_dir(
            bigram_dir_offset,
            num_bigrams
                .checked_mul(16)
                .context("overflowing bigram directory size")?,
            "bigram",
        )?;
        check_dir(
            trigram_dir_offset,
            num_trigrams
                .checked_mul(16)
                .context("overflowing trigram directory size")?,
            "trigram",
        )?;
        check_dir(meta_offset, meta_len, "metadata")?;

        let metas: Vec<SegmentDocMeta> = serde_json::from_slice(&bytes[meta_offset..meta_offset + meta_len])
            .context("decoding trigram segment metadata")?;
        anyhow::ensure!(
            metas.len() == num_sessions as usize,
            "trigram segment metadata count mismatch"
        );
        Ok(Self {
            bytes,
            num_sessions,
            unigram_dir_offset,
            bigram_dir_offset,
            num_bigrams,
            trigram_dir_offset,
            num_trigrams,
            metas,
        })
    }

    fn decode_bitmap(&self, off: usize, len: usize) -> Result<RoaringBitmap> {
        let end = off.checked_add(len).context("overflowing trigram posting range")?;
        anyhow::ensure!(
            end <= self.bytes.len() - FOOTER_BYTES,
            "trigram posting slice out of bounds"
        );
        RoaringBitmap::deserialize_from(Cursor::new(&self.bytes[off..end]))
            .context("deserializing trigram roaring bitmap")
    }

    fn unigram(&self, b: u8) -> Result<RoaringBitmap> {
        let pos = self.unigram_dir_offset + (b as usize) * 12;
        let off = u64::from_le_bytes(self.bytes[pos..pos + 8].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(self.bytes[pos + 8..pos + 12].try_into().unwrap()) as usize;
        if len == 0 {
            return Ok(RoaringBitmap::new());
        }
        self.decode_bitmap(off, len)
    }

    fn bigram(&self, target: u16) -> Result<RoaringBitmap> {
        let mut lo = 0usize;
        let mut hi = self.num_bigrams;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let pos = self.bigram_dir_offset + mid * 16;
            let gram = u16::from_le_bytes(self.bytes[pos..pos + 2].try_into().unwrap());
            match gram.cmp(&target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let off = u64::from_le_bytes(self.bytes[pos + 4..pos + 12].try_into().unwrap()) as usize;
                    let len = u32::from_le_bytes(self.bytes[pos + 12..pos + 16].try_into().unwrap()) as usize;
                    return self.decode_bitmap(off, len);
                }
            }
        }
        Ok(RoaringBitmap::new())
    }

    fn trigram_slice(&self, target: u32) -> Option<(usize, usize)> {
        let mut lo = 0usize;
        let mut hi = self.num_trigrams;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let pos = self.trigram_dir_offset + mid * 16;
            let gram = u32::from_le_bytes(self.bytes[pos..pos + 4].try_into().unwrap());
            match gram.cmp(&target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let len = u32::from_le_bytes(self.bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
                    let off = u64::from_le_bytes(self.bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
                    return Some((off, len));
                }
            }
        }
        None
    }

    fn query_slots(&self, cq: &[u8], deleted_slots: &RoaringBitmap) -> Result<RoaringBitmap> {
        let mut slots = match cq.len() {
            0 => return Ok(RoaringBitmap::new()),
            1 => self.unigram(cq[0])?,
            2 => self.bigram(pack_bigram(cq[0], cq[1]))?,
            _ => {
                let mut grams: Vec<u32> = (0..=cq.len() - 3)
                    .map(|i| pack_trigram(cq[i], cq[i + 1], cq[i + 2]))
                    .collect();
                grams.sort_unstable();
                grams.dedup();

                let mut slices = Vec::with_capacity(grams.len());
                for g in grams {
                    let Some(slice) = self.trigram_slice(g) else {
                        return Ok(RoaringBitmap::new());
                    };
                    slices.push(slice);
                }
                // Smallest serialized roaring payload first so rare trigrams prune early.
                slices.sort_unstable_by_key(|&(_, len)| len);
                let (first_off, first_len) = slices[0];
                let mut acc = self.decode_bitmap(first_off, first_len)?;
                acc -= deleted_slots;
                if acc.is_empty() {
                    return Ok(acc);
                }
                for &(off, len) in &slices[1..] {
                    let bm = self.decode_bitmap(off, len)?;
                    acc &= bm;
                    if acc.is_empty() {
                        break;
                    }
                }
                return Ok(acc);
            }
        };
        slots -= deleted_slots;
        Ok(slots)
    }
}

// ---- Exact Streaming Substring Verifier ------------------------------------------------------

struct ExactStreamMatcher {
    needle: String,
    needle_folded: String,
    case_sensitive: bool,
    matched: bool,
    excerpt: Option<(u64, Excerpt)>,
    tail: String,
    tail_at: u64,
    keep: usize,
    stage: Vec<u8>,
}

impl ExactStreamMatcher {
    fn new(needle: &str, case_sensitive: bool) -> Self {
        let needle_folded = if case_sensitive {
            String::new()
        } else {
            needle.to_lowercase()
        };
        let max_len = needle.len().max(needle_folded.len());
        let guard = 4 * max_len + 8;
        Self {
            needle: needle.to_string(),
            needle_folded,
            case_sensitive,
            matched: false,
            excerpt: None,
            tail: String::new(),
            tail_at: 0,
            keep: LEAD_BYTES + guard,
            stage: Vec::new(),
        }
    }

    fn done(&self) -> bool {
        self.matched
            && self
                .excerpt
                .as_ref()
                .is_some_and(|(_, e)| e.text.len() >= e.at + e.len + TRAIL_BYTES)
    }

    fn feed_bytes(&mut self, bytes: &[u8]) -> u64 {
        if self.done() {
            return 0;
        }
        let mut accepted = 0u64;
        for part in bytes.chunks(PIECE_BYTES) {
            accepted += part.len() as u64;
            self.stage.extend_from_slice(part);
            if self.stage.len() >= PIECE_BYTES {
                self.flush(false);
                if self.done() {
                    break;
                }
            }
        }
        accepted
    }

    fn flush(&mut self, end: bool) {
        if self.stage.is_empty() {
            return;
        }
        let valid = match std::str::from_utf8(&self.stage) {
            Ok(_) => self.stage.len(),
            Err(e) if e.error_len().is_none() && !end => e.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(&self.stage).into_owned();
                self.stage.clear();
                self.piece(&text);
                return;
            }
        };
        if valid == 0 {
            return;
        }
        let rest = self.stage.split_off(valid);
        let staged = std::mem::replace(&mut self.stage, rest);
        let text = String::from_utf8(staged).unwrap_or_default();
        self.piece(&text);
    }

    fn piece(&mut self, piece: &str) {
        if let Some((_, ref mut ex)) = self.excerpt {
            let wanted = ex.at + ex.len + TRAIL_BYTES;
            if ex.text.len() < wanted {
                let take = ceil_char(piece, wanted - ex.text.len());
                ex.text.push_str(&piece[..take]);
            }
            return;
        }
        let mut window = std::mem::take(&mut self.tail);
        window.push_str(piece);

        let found_range = if self.case_sensitive {
            memchr::memmem::find(window.as_bytes(), self.needle.as_bytes()).map(|pos| pos..pos + self.needle.len())
        } else {
            let low = window.to_lowercase();
            low.find(self.needle_folded.as_str())
                .map(|pos| lowercase_source_range(&window, pos..pos + self.needle_folded.len()))
        };

        if let Some(range) = found_range {
            self.matched = true;
            let from = floor_char(&window, range.start.saturating_sub(LEAD_BYTES));
            let at = range.start - from;
            let len = range.len();
            let wanted = at + len + TRAIL_BYTES;
            let to = ceil_char(&window, from + wanted);
            let ex = Excerpt {
                text: window[from..to].to_string(),
                at,
                len,
            };
            self.excerpt = Some((self.tail_at + range.start as u64, ex));
            return;
        }

        let from = ceil_char(&window, window.len().saturating_sub(self.keep));
        window.drain(..from);
        self.tail_at += from as u64;
        self.tail = window;
    }

    fn finish(&mut self) -> Option<Excerpt> {
        self.flush(true);
        self.excerpt.clone().map(|(_, ex)| ex)
    }
}

struct VerifyStreamAdapter<'a> {
    matcher: &'a mut ExactStreamMatcher,
    primary_path: &'a Path,
    dependencies: &'a mut cv_core::source::ReadSet,
    metered: &'a std::cell::Cell<u64>,
}

impl VerifyStreamAdapter<'_> {
    fn feed(&mut self, bytes: &[u8]) {
        let accepted = self.matcher.feed_bytes(bytes);
        self.metered.set(self.metered.get().saturating_add(accepted));
    }
}

impl Searchable for VerifyStreamAdapter<'_> {
    fn full(&self) -> bool {
        self.matcher.done()
    }

    fn str(&mut self, s: &str) {
        self.feed(s.as_bytes());
    }

    fn text(
        &mut self,
        text: &cv_core::Text,
        resolver: &cv_core::Resolver,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        match text {
            cv_core::Text::Span(sp) => {
                if let Some(path) = sp.source.as_ref().filter(|p| p.as_path() != self.primary_path) {
                    self.dependencies.record(path);
                }
            }
            cv_core::Text::Source(source) => {
                if source.path() != self.primary_path {
                    self.dependencies.record(source.path());
                }
                if let Some(reads) = source.dependencies() {
                    self.dependencies.merge(reads);
                }
            }
            cv_core::Text::Inline(_) => {}
        }
        cv_core::stream::stopped(text.try_for_each_chunk(resolver, PIECE_BYTES, |part| {
            check()?;
            self.feed(part.as_bytes());
            if self.matcher.done() {
                Err(cv_core::stream::Stop.into())
            } else {
                Ok(())
            }
        }))
        .map(|_| ())
    }

    fn json(&mut self, json: &cv_core::Json, check: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if let cv_core::Json::Source(range) = json {
            if range.path() != self.primary_path {
                self.dependencies.record(range.path());
            }
            if let Some(reads) = range.dependencies() {
                self.dependencies.merge(reads);
            }
        }
        struct Writer<'a, 'b> {
            matcher: &'a mut ExactStreamMatcher,
            metered: &'a std::cell::Cell<u64>,
            check: &'a mut (dyn FnMut() -> Result<()> + 'b),
        }
        impl std::io::Write for Writer<'_, '_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                (self.check)().map_err(std::io::Error::other)?;
                let accepted = self.matcher.feed_bytes(bytes);
                self.metered.set(self.metered.get().saturating_add(accepted));
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        json.write_canonical(&mut Writer {
            matcher: self.matcher,
            metered: self.metered,
            check,
        })
    }

    fn after_block(&mut self, block: &cv_core::Block) -> Result<()> {
        if self.matcher.done() {
            return Ok(());
        }
        if let Some(path) = block.persisted_output_path() {
            self.dependencies.record(Path::new(path));
            let read = cv_core::lazy::read_pieces_checked(Path::new(path), PIECE_BYTES, |piece| {
                self.feed(piece.as_bytes());
                if self.matcher.done() {
                    Err(cv_core::stream::Stop.into())
                } else {
                    Ok(())
                }
            });
            let read = match read {
                Ok(r) => r,
                Err(e) if e.is::<cv_core::stream::Stop>() => return Ok(()),
                Err(e) => return Err(e).context("reading persisted output during trigram verification"),
            };
            match read {
                cv_core::lazy::PersistedHead::Unreadable(reason) => {
                    anyhow::bail!("reading persisted output {path}: {reason}")
                }
                cv_core::lazy::PersistedHead::Text(_) => self.feed(b"\n"),
                cv_core::lazy::PersistedHead::Absent => {}
            }
        }
        Ok(())
    }
}

struct VerifySink {
    primary_path: PathBuf,
    need_title: bool,
    need_cwd: bool,
    first_prompt: Option<String>,
    meta_title: Option<String>,
    cwd: Option<String>,
    copied_before: Option<chrono::DateTime<chrono::Utc>>,
    resolver: cv_core::Resolver,
    dependencies: cv_core::source::ReadSet,
    exchange: Option<ExactStreamMatcher>,
    context: Option<ExactStreamMatcher>,
    meter_limit: Option<u64>,
    metered_bytes: u64,
    over_budget: bool,
    err: Option<anyhow::Error>,
    failed: bool,
}

impl VerifySink {
    fn new(
        r: &cv_core::SessionRef,
        needle: &str,
        scope: Scope,
        case_sensitive: bool,
        need_title: bool,
        need_cwd: bool,
        meter_limit: Option<u64>,
    ) -> Self {
        let exchange =
            matches!(scope, Scope::Exchange | Scope::All).then(|| ExactStreamMatcher::new(needle, case_sensitive));
        let context =
            matches!(scope, Scope::Context | Scope::All).then(|| ExactStreamMatcher::new(needle, case_sensitive));
        Self {
            primary_path: cv_core::source::primary_path(r),
            need_title,
            need_cwd,
            first_prompt: None,
            meta_title: None,
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            copied_before: None,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
            dependencies: Default::default(),
            exchange,
            context,
            meter_limit,
            metered_bytes: 0,
            over_budget: false,
            err: None,
            failed: false,
        }
    }

    fn any_done(&self) -> bool {
        self.exchange.as_ref().is_some_and(ExactStreamMatcher::done)
            || self.context.as_ref().is_some_and(ExactStreamMatcher::done)
    }
}

impl MessageSink for VerifySink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.failed |= self.err.is_some();
        self.err
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("trigram verifier failed")))
    }

    fn dependency(&mut self, path: &Path) {
        self.dependencies.record(path);
    }

    fn append_only_dependency(&mut self, path: &Path) {
        self.dependencies.record_append_only(path);
    }

    fn meta(&mut self, s: &cv_core::Session) {
        self.meta_title = s.title.clone();
        if self.cwd.is_none() {
            self.cwd = s.cwd.as_ref().map(|p| p.display().to_string());
        }
    }

    fn message(&mut self, m: cv_core::Message) -> Flow {
        if self.failed || self.err.is_some() || self.over_budget {
            return Flow::Stop;
        }
        let r = std::mem::replace(&mut self.resolver, cv_core::Resolver::new(None));
        if self.need_title && self.first_prompt.is_none() && m.role == cv_core::Role::User {
            match m.prompt_title(&r) {
                Ok(title) => self.first_prompt = title,
                Err(e) => {
                    self.err = Some(e);
                    self.failed = true;
                    self.resolver = r;
                    return Flow::Stop;
                }
            }
        }
        let copied = matches!((m.timestamp, self.copied_before), (Some(at), Some(before)) if at < before);
        if !copied {
            let target = if m.kind.is_exchange() {
                self.exchange.as_mut()
            } else {
                self.context.as_mut()
            };
            if let Some(matcher) = target {
                let limit = self.meter_limit;
                let metered_cell = std::cell::Cell::new(self.metered_bytes);
                let over_cell = std::cell::Cell::new(self.over_budget);
                let mut adapter = VerifyStreamAdapter {
                    matcher,
                    primary_path: &self.primary_path,
                    dependencies: &mut self.dependencies,
                    metered: &metered_cell,
                };
                let res = cv_core::projection::searchable(&m.content, &r, &mut adapter, &mut || {
                    if let Some(max_b) = limit {
                        if metered_cell.get() > max_b {
                            over_cell.set(true);
                            anyhow::bail!("over trigram live-read budget");
                        }
                    }
                    Ok(())
                });
                self.metered_bytes = metered_cell.get();
                self.over_budget |= over_cell.get();
                if let Some(max_b) = self.meter_limit {
                    if self.metered_bytes > max_b {
                        self.over_budget = true;
                    }
                }
                if let Err(e) = res {
                    if !self.over_budget {
                        self.err = Some(e);
                        self.failed = true;
                    }
                    self.resolver = r;
                    return Flow::Stop;
                }
            }
        }
        self.resolver = r;
        if self.any_done()
            && (!self.need_title || self.first_prompt.is_some())
            && (!self.need_cwd || self.cwd.is_some())
        {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

struct VerifiedMatch {
    title: String,
    cwd: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    snippet: String,
    metered_bytes: u64,
}

enum VerifyOutcome {
    Matched(VerifiedMatch),
    NoMatch { metered_bytes: u64 },
    Stale,
}

#[allow(clippy::too_many_arguments)]
fn verify_session_exact(
    r: &cv_core::SessionRef,
    known_title: Option<&str>,
    required_cwd: Option<&str>,
    expected_baseline: Option<&cv_core::source::Baseline>,
    needle: &str,
    scope: Scope,
    case_sensitive: bool,
    meter_limit: Option<u64>,
    _probe: &mut cv_core::source::Probe,
) -> Result<VerifyOutcome> {
    let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
        return Ok(VerifyOutcome::NoMatch { metered_bytes: 0 });
    };
    if let Some(expected) = expected_baseline {
        let mut pre_probe = cv_core::source::Probe::default();
        let before = pre_probe.snapshot(r);
        if !expected.fresh(&before, &mut pre_probe) && !expected.rows_fresh(r, &before, &mut pre_probe) {
            return Ok(VerifyOutcome::Stale);
        }
    }

    let need_title = known_title.is_none();
    let need_cwd = required_cwd.is_some() && r.cwd.is_none();
    let mut sink = VerifySink::new(r, needle, scope, case_sensitive, need_title, need_cwd, meter_limit);
    sink.copied_before = adapter.copied_before(r, &mut sink);
    let stream_res = adapter
        .try_stream(r, &Demand::LEAN.with_spans(), &mut sink)
        .with_context(|| format!("verifying {}:{}", r.harness, r.id));
    if sink.over_budget {
        anyhow::bail!("over_budget:{}", sink.metered_bytes);
    }
    let session_meta = stream_res?;
    if let Some(err) = sink.take_error() {
        return Err(err);
    }

    if let Some(expected) = expected_baseline {
        let mut post_probe = cv_core::source::Probe::default();
        let after = post_probe.snapshot(r);
        if !expected.fresh(&after, &mut post_probe) && !expected.rows_fresh(r, &after, &mut post_probe) {
            return Ok(VerifyOutcome::Stale);
        }
    }

    let resolved_cwd = sink.cwd.or_else(|| session_meta.cwd.map(|p| p.display().to_string()));
    if let Some(cwd_sub) = required_cwd {
        if !resolved_cwd.as_deref().is_some_and(|c| c.contains(cwd_sub)) {
            return Ok(VerifyOutcome::NoMatch {
                metered_bytes: sink.metered_bytes,
            });
        }
    }
    let created_at = r.created_at.or(session_meta.created_at).map(|t| t.timestamp());
    let updated_at = r.updated_at.or(session_meta.updated_at).map(|t| t.timestamp());

    let title = match known_title {
        Some(t) => t.to_string(),
        None => {
            let m_title = session_meta
                .title
                .as_deref()
                .or(sink.meta_title.as_deref())
                .or(r.title.as_deref());
            cv_core::label_from(m_title, sink.first_prompt.as_deref())
        }
    };

    let body_excerpt = sink
        .exchange
        .as_mut()
        .and_then(ExactStreamMatcher::finish)
        .or_else(|| sink.context.as_mut().and_then(ExactStreamMatcher::finish));

    if let Some(ex) = body_excerpt {
        return Ok(VerifyOutcome::Matched(VerifiedMatch {
            title,
            cwd: resolved_cwd,
            created_at,
            updated_at,
            snippet: snippet(&ex.text, ex.at, ex.len),
            metered_bytes: sink.metered_bytes,
        }));
    }

    if matches!(scope, Scope::Exchange | Scope::All) && !title.is_empty() {
        let mut title_matcher = ExactStreamMatcher::new(needle, case_sensitive);
        title_matcher.feed_bytes(title.as_bytes());
        if let Some(ex) = title_matcher.finish() {
            return Ok(VerifyOutcome::Matched(VerifiedMatch {
                snippet: snippet(&ex.text, ex.at, ex.len),
                title,
                cwd: resolved_cwd,
                created_at,
                updated_at,
                metered_bytes: sink.metered_bytes,
            }));
        }
    }

    Ok(VerifyOutcome::NoMatch {
        metered_bytes: sink.metered_bytes,
    })
}

// ---- Discovery, Folding & Synchronization ----------------------------------------------------

fn expand_sessions(
    refs: &[cv_core::SessionRef],
    subagents: bool,
) -> Result<Vec<(cv_core::SessionRef, cv_core::events::Provenance)>> {
    let mut chosen: HashMap<String, (cv_core::SessionRef, cv_core::events::Provenance)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    let mut insert = |r: cv_core::SessionRef, prov: cv_core::events::Provenance| match chosen.get_mut(&r.id) {
        None => {
            order.push(r.id.clone());
            chosen.insert(r.id.clone(), (r, prov));
        }
        Some((old, old_prov)) => {
            if (!r.path.exists())
                .cmp(&(!old.path.exists()))
                .then_with(|| r.preferred_copy_cmp(old))
                .then_with(|| prov.parent_id.cmp(&old_prov.parent_id))
                .is_lt()
            {
                *old = r;
                *old_prov = prov;
            }
        }
    };

    for r in refs {
        insert(r.clone(), cv_core::events::Provenance::default());
        if subagents {
            for sub in cv_core::subagent_tree_filtered_of(r, cv_core::SubagentDemand::Sessions, None, |_, _| true)? {
                let prov = cv_core::events::Provenance {
                    parent_id: Some(r.id.clone()),
                    agent_id: Some(sub.agent_id().to_string()),
                    workflow: sub.workflow.clone(),
                };
                insert(sub.session, prov);
            }
        }
    }

    Ok(order.into_iter().filter_map(|id| chosen.remove(&id)).collect())
}

fn acquire_trigram_writer_lock(dir: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let lock_path = dir.join(".trigram-writer.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(file)
}

fn is_not_found_err(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|ioe| ioe.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Stat-only probe answering whether `sync_all` at `dir` has work to do:
/// - `None` when `manifest.json` does not exist or cannot be read;
/// - `Some(true)` when the corpus moved (new, modified, vanished, or missing folded subagents);
/// - `Some(false)` when every session in the corpus matches `manifest.json`.
pub fn pass_needed(dir: &Path, subagents: bool) -> Option<bool> {
    let _sources = cv_core::source::read_scope();
    let manifest = Manifest::load(dir).ok()??;
    if subagents && !manifest.has_subagents {
        return Some(true);
    }
    let refs = cv_core::sessions();
    let candidates = expand_sessions(&refs, subagents).ok()?;
    let mut probe = cv_core::source::Probe::default();
    let mut seen_ids = HashSet::with_capacity(candidates.len());
    for (r, _) in candidates {
        seen_ids.insert(r.id.clone());
        let is_fresh = manifest.sessions.get(&r.id).is_some_and(|ptr| {
            let now = probe.snapshot(&r);
            ptr.path == r.path
                && (ptr.baseline.fresh(&now, &mut probe) || ptr.baseline.rows_fresh(&r, &now, &mut probe))
        });
        if !is_fresh {
            return Some(true);
        }
    }
    if manifest
        .sessions
        .iter()
        .any(|(id, ptr)| !seen_ids.contains(id) && (subagents || !ptr.folded))
    {
        return Some(true);
    }
    Some(false)
}

/// Discover all sessions and synchronize the trigram index at `dir`.
pub fn sync_all(
    dir: &Path,
    rebuild: bool,
    subagents: bool,
    strict: bool,
) -> Result<(TrigramSynced, Vec<cv_core::SessionRef>)> {
    let _sources = cv_core::source::read_scope();
    let refs = cv_core::sessions();
    let synced = sync_sessions(dir, &refs, rebuild, subagents)?;
    if strict {
        if let Some(first) = synced.unreadable.first() {
            anyhow::bail!(
                "{} unreadable session(s) during trigram sync (first: {} ({}): {})",
                synced.unreadable.len(),
                first.id,
                first.harness,
                first.reason
            );
        }
    }
    Ok((synced, refs))
}

/// Incrementally synchronize `refs` into the trigram segment store at `dir`.
pub fn sync_sessions(
    dir: &Path,
    refs: &[cv_core::SessionRef],
    rebuild: bool,
    subagents: bool,
) -> Result<TrigramSynced> {
    let _sources = cv_core::source::read_scope();
    let _writer_lock = acquire_trigram_writer_lock(dir)?;

    let mut prior_rebuild_files = Vec::new();
    let mut manifest = if rebuild {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|ext| ext == "trg") {
                    if let Some(name) = e.file_name().to_str() {
                        prior_rebuild_files.push(name.to_string());
                    }
                }
            }
        }
        Manifest::default()
    } else {
        Manifest::load(dir)?.unwrap_or_default()
    };

    let candidates = expand_sessions(refs, subagents)?;
    let total = candidates.len();
    let mut probe = cv_core::source::Probe::default();
    let mut seen_ids = HashSet::with_capacity(candidates.len());
    let mut to_index = Vec::new();
    let mut unchanged = 0usize;

    for (r, prov) in candidates {
        seen_ids.insert(r.id.clone());
        let is_fresh = manifest.sessions.get(&r.id).is_some_and(|ptr| {
            let now = probe.snapshot(&r);
            ptr.path == r.path
                && (ptr.baseline.fresh(&now, &mut probe) || ptr.baseline.rows_fresh(&r, &now, &mut probe))
        });
        if is_fresh {
            unchanged += 1;
        } else {
            to_index.push((r, prov));
        }
    }

    let mut seg_deleted: HashMap<String, HashSet<u32>> = manifest
        .segments
        .iter()
        .map(|s| (s.file.clone(), s.deleted_ords.iter().copied().collect()))
        .collect();

    let mut removed = 0usize;
    let vanished: Vec<String> = manifest
        .sessions
        .iter()
        .filter(|(id, ptr)| !seen_ids.contains(*id) && (subagents || !ptr.folded))
        .map(|(id, _)| id.clone())
        .collect();
    for id in vanished {
        if let Some(old) = manifest.sessions.remove(&id) {
            if let Some(del) = seg_deleted.get_mut(&old.segment) {
                del.insert(old.ord);
            }
            removed += 1;
        }
    }

    let mut extracted = Vec::with_capacity(to_index.len());
    let mut unreadable = Vec::new();
    for (r, prov) in to_index {
        match extract_session_grams(&r, &prov) {
            Ok(doc) => {
                if let Some(old) = manifest.sessions.remove(&r.id) {
                    if let Some(del) = seg_deleted.get_mut(&old.segment) {
                        del.insert(old.ord);
                    }
                }
                extracted.push((doc, prov.parent_id.is_some()));
            }
            Err(err) => {
                if let Some(old) = manifest.sessions.remove(&r.id) {
                    if let Some(del) = seg_deleted.get_mut(&old.segment) {
                        del.insert(old.ord);
                    }
                }
                unreadable.push(Unreadable {
                    id: r.id,
                    harness: r.harness.as_str().to_string(),
                    reason: cv_core::ir::truncate(&format!("{err:#}"), 512),
                });
            }
        }
    }

    let indexed = extracted.len();
    if !extracted.is_empty() {
        let mut docs = Vec::with_capacity(extracted.len());
        let mut ptr_templates = Vec::with_capacity(extracted.len());
        for (idx, (doc, folded)) in extracted.into_iter().enumerate() {
            ptr_templates.push((
                doc.meta.id.clone(),
                idx as u32,
                doc.meta.mtime,
                doc.meta.size,
                doc.meta.path.clone(),
                folded,
                doc.meta.baseline.clone(),
            ));
            docs.push(doc);
        }
        let seg_file = write_segment(dir, docs)?;
        let num_sessions = ptr_templates.len() as u32;
        manifest.segments.push(SegmentEntry {
            file: seg_file.clone(),
            num_sessions,
            deleted_ords: Vec::new(),
        });
        seg_deleted.insert(seg_file.clone(), HashSet::new());
        for (id, ord, mtime, size, path, folded, baseline) in ptr_templates {
            manifest.sessions.insert(
                id,
                SessionPointer {
                    segment: seg_file.clone(),
                    ord,
                    mtime,
                    size,
                    path,
                    folded,
                    baseline,
                },
            );
        }
    }

    let mut dropped_files = Vec::new();
    let mut kept_segments = Vec::with_capacity(manifest.segments.len());
    for mut seg in manifest.segments {
        let mut del: Vec<u32> = seg_deleted.remove(&seg.file).unwrap_or_default().into_iter().collect();
        del.sort_unstable();
        if del.len() as u32 >= seg.num_sessions {
            dropped_files.push(seg.file);
        } else {
            seg.deleted_ords = del;
            kept_segments.push(seg);
        }
    }
    manifest.segments = kept_segments;
    manifest.has_subagents = subagents || manifest.sessions.values().any(|p| p.folded);

    let (compacted, compacted_old_files) = maybe_compact_segments(dir, &mut manifest)?;
    manifest.save_atomic(dir)?;
    let active_files: HashSet<&str> = manifest.segments.iter().map(|s| s.file.as_str()).collect();
    for f in prior_rebuild_files
        .into_iter()
        .chain(dropped_files)
        .chain(compacted_old_files)
    {
        if !active_files.contains(f.as_str()) {
            let _ = std::fs::remove_file(dir.join(f));
        }
    }

    Ok(TrigramSynced {
        total,
        indexed,
        unchanged,
        removed,
        compacted,
        unreadable,
    })
}

fn maybe_compact_segments(dir: &Path, manifest: &mut Manifest) -> Result<(bool, Vec<String>)> {
    let total_rows: usize = manifest.segments.iter().map(|s| s.num_sessions as usize).sum();
    let dead_rows: usize = manifest.segments.iter().map(|s| s.deleted_ords.len()).sum();
    if manifest.segments.len() <= 1 || total_rows == 0 || dead_rows * 2 <= total_rows {
        return Ok((false, Vec::new()));
    }

    // Re-extract live sessions of fragmented segments (> 50% dead overall).
    let mut live_docs = Vec::new();
    let mut old_files = Vec::new();
    let mut surviving_segments = Vec::new();

    for seg in std::mem::take(&mut manifest.segments) {
        if seg.deleted_ords.is_empty() {
            surviving_segments.push(seg);
            continue;
        }
        let seg_path = dir.join(&seg.file);
        let Ok(reader) = SegmentReader::open(&seg_path) else {
            surviving_segments.push(seg);
            continue;
        };
        let dead: HashSet<u32> = seg.deleted_ords.iter().copied().collect();
        let mut all_rebuilt = true;
        let mut rebuilt_for_seg = Vec::new();
        for (idx, meta) in reader.metas.iter().enumerate() {
            let ord = idx as u32;
            if dead.contains(&ord) {
                continue;
            }
            let Some(r) = meta.to_session_ref() else {
                all_rebuilt = false;
                break;
            };
            let prov = cv_core::events::Provenance {
                parent_id: meta.parent_id.clone(),
                agent_id: meta.agent_id.clone(),
                workflow: meta.workflow.clone(),
            };
            match extract_session_grams(&r, &prov) {
                Ok(doc) => rebuilt_for_seg.push((doc, meta.parent_id.is_some())),
                Err(_) => {
                    all_rebuilt = false;
                    break;
                }
            }
        }
        if all_rebuilt {
            old_files.push(seg.file);
            live_docs.extend(rebuilt_for_seg);
        } else {
            surviving_segments.push(seg);
        }
    }

    manifest.segments = surviving_segments;
    if live_docs.is_empty() && old_files.is_empty() {
        return Ok((false, Vec::new()));
    }
    if !live_docs.is_empty() {
        let mut docs = Vec::with_capacity(live_docs.len());
        let mut ptrs = Vec::with_capacity(live_docs.len());
        for (idx, (doc, folded)) in live_docs.into_iter().enumerate() {
            ptrs.push((
                doc.meta.id.clone(),
                idx as u32,
                doc.meta.mtime,
                doc.meta.size,
                doc.meta.path.clone(),
                folded,
                doc.meta.baseline.clone(),
            ));
            docs.push(doc);
        }
        let seg_file = write_segment(dir, docs)?;
        manifest.segments.push(SegmentEntry {
            file: seg_file.clone(),
            num_sessions: ptrs.len() as u32,
            deleted_ords: Vec::new(),
        });
        for (id, ord, mtime, size, path, folded, baseline) in ptrs {
            manifest.sessions.insert(
                id,
                SessionPointer {
                    segment: seg_file.clone(),
                    ord,
                    mtime,
                    size,
                    path,
                    folded,
                    baseline,
                },
            );
        }
    }
    Ok((true, old_files))
}

// ---- Query Execution (Indexed Segments + Live Unindexed Tail) --------------------------------

fn sort_hits(hits: &mut [Hit]) {
    hits.sort_by(|a, b| {
        let ta = a.updated_at.or(a.created_at).unwrap_or(i64::MIN);
        let tb = b.updated_at.or(b.created_at).unwrap_or(i64::MIN);
        tb.cmp(&ta).then_with(|| a.id.cmp(&b.id))
    });
}

/// Search the trigram index at `dir` plus any live unindexed/modified sessions in `refs`.
pub fn search(
    dir: &Path,
    query: &str,
    opts: TrigramOptions<'_>,
    refs: &[cv_core::SessionRef],
    allowed_ids: Option<&HashSet<String>>,
) -> Result<TrigramFound> {
    let _sources = cv_core::source::read_scope();
    let mut manifest = Manifest::load(dir)?;
    if manifest.is_none() && opts.live == LiveRead::Frozen {
        anyhow::bail!(
            "no trigram index at {}; build it with `cv index --exact`",
            dir.display()
        );
    }
    let mut suffix = Suffix {
        live: opts.live,
        no_index: manifest.is_none(),
        ..Default::default()
    };

    let harness_filter = match opts.filter.harness {
        Some(h) => match cv_core::Harness::parse(h) {
            Some(parsed) => Some(parsed.as_str()),
            None => {
                return Ok(TrigramFound {
                    hits: Vec::new(),
                    total: 0,
                    suffix,
                    notes: Vec::new(),
                    candidate_docs: 0,
                    verified_docs: 0,
                });
            }
        },
        None => None,
    };

    if allowed_ids.is_some_and(HashSet::is_empty) || query.is_empty() {
        return Ok(TrigramFound {
            hits: Vec::new(),
            total: 0,
            suffix,
            notes: Vec::new(),
            candidate_docs: 0,
            verified_docs: 0,
        });
    }

    let cq = canon_bytes(query);
    let mut probe = cv_core::source::Probe::default();

    // 1. Identify stale / unindexed sessions in `refs` (widening folded subagents when the
    //    manifest holds `--subagents`) against `manifest`.
    let fold_subagents = manifest.as_ref().is_some_and(|m| m.has_subagents);
    let candidates = expand_sessions(refs, fold_subagents)?;
    let newest_indexed = manifest
        .as_ref()
        .and_then(|m| m.sessions.values().filter(|p| p.size >= 0).map(|p| p.mtime).max())
        .unwrap_or(0);
    let mut newest_on_disk = 0i64;
    let mut stale_refs: Vec<(cv_core::SessionRef, cv_core::events::Provenance, i64, Option<u64>)> = Vec::new();
    let mut stale_ids = HashSet::new();

    for (r, prov) in candidates {
        let snap = probe.snapshot(&r);
        let mtime = snap.primary.map_or(0, |id| id.mtime_ns as i64);
        newest_on_disk = newest_on_disk.max(mtime);

        let is_fresh = manifest
            .as_ref()
            .and_then(|m| m.sessions.get(&r.id))
            .is_some_and(|ptr| {
                ptr.path == r.path
                    && (ptr.baseline.fresh(&snap, &mut probe) || ptr.baseline.rows_fresh(&r, &snap, &mut probe))
            });

        if !is_fresh {
            stale_ids.insert(r.id.clone());
        }

        if let Some(h) = harness_filter {
            if r.harness.as_str() != h {
                continue;
            }
        }
        if let Some(cwd_sub) = opts.filter.cwd {
            // Only fast-reject when discovery already knows `r.cwd`; if `r.cwd` is `None`,
            // defer to the indexed segment metadata (if fresh) or `VerifySink` (if stale/live).
            if let Some(ref rcwd) = r.cwd {
                if !rcwd.to_string_lossy().contains(cwd_sub) {
                    continue;
                }
            }
        }
        if let Some(ids) = allowed_ids {
            let admitted = ids.contains(&r.id) || prov.parent_id.as_ref().is_some_and(|p| ids.contains(p));
            if !admitted {
                continue;
            }
        }

        if !is_fresh {
            suffix.stale += 1;
            let known_cost = cv_core::harness::for_harness(r.harness).and_then(|ad| {
                if ad.sqlite_source(&r) == Some(true) {
                    None
                } else {
                    Some(snap.primary_len())
                }
            });
            stale_refs.push((r, prov, mtime, known_cost));
        }
    }
    suffix.behind_ns = newest_on_disk.saturating_sub(newest_indexed);

    // Also reap/shadow any indexed top-level session that was deleted on disk when `refs` is non-empty
    // or when checking on-disk existence during verification.
    let mut hits: Vec<Hit> = Vec::new();
    let mut candidate_docs = 0usize;
    let mut verified_docs = 0usize;

    // 2. Live tail verification for stale/unindexed sessions within `opts.live` budget.
    let mut remaining = match opts.live {
        LiveRead::All => u64::MAX,
        LiveRead::Within(budget, _) => budget,
        LiveRead::Frozen => 0,
    };
    if opts.live != LiveRead::Frozen && !stale_refs.is_empty() {
        stale_refs.sort_by_key(|(r, _, mtime, _)| std::cmp::Reverse((*mtime, r.updated_at)));
        for (mut r, prov, _, known_cost) in stale_refs {
            let is_folded = prov.parent_id.is_some();
            match known_cost {
                Some(cost) if cost > remaining => {
                    suffix.cut += 1;
                    suffix.cut_bytes += cost;
                    continue;
                }
                Some(cost) => {
                    verified_docs += 1;
                    match verify_session_exact(
                        &r,
                        None,
                        opts.filter.cwd,
                        None,
                        query,
                        opts.filter.scope,
                        opts.case_sensitive,
                        None,
                        &mut probe,
                    ) {
                        Ok(VerifyOutcome::Matched(m)) => {
                            remaining -= cost;
                            suffix.read += 1;
                            suffix.bytes += cost;
                            if r.cwd.is_none() {
                                r.cwd = m.cwd.as_deref().map(PathBuf::from);
                            }
                            hits.push(Hit {
                                id: r.id.clone(),
                                harness: r.harness.as_str().to_string(),
                                cwd: m.cwd,
                                title: Some(m.title),
                                score: 1.0,
                                snippet: m.snippet,
                                created_at: m.created_at,
                                updated_at: m.updated_at,
                                parent_id: prov.parent_id,
                                agent_id: prov.agent_id,
                                workflow: prov.workflow,
                                session: (!is_folded).then_some(r),
                            });
                        }
                        Ok(VerifyOutcome::NoMatch { .. }) | Ok(VerifyOutcome::Stale) => {
                            remaining -= cost;
                            suffix.read += 1;
                            suffix.bytes += cost;
                        }
                        Err(_) => {
                            suffix.failed += 1;
                        }
                    }
                }
                None => {
                    // Metered shared SQLite source: once the live-read budget is exhausted, cut
                    // immediately instead of letting each subsequent SQLite session stream a first
                    // chunk against `Some(0)`.
                    if opts.live != LiveRead::All && remaining == 0 {
                        suffix.cut += 1;
                        continue;
                    }
                    verified_docs += 1;
                    match verify_session_exact(
                        &r,
                        None,
                        opts.filter.cwd,
                        None,
                        query,
                        opts.filter.scope,
                        opts.case_sensitive,
                        (opts.live != LiveRead::All).then_some(remaining),
                        &mut probe,
                    ) {
                        Ok(VerifyOutcome::Matched(m)) => {
                            let spent = m.metered_bytes.min(remaining);
                            remaining -= spent;
                            suffix.read += 1;
                            suffix.bytes += spent;
                            if r.cwd.is_none() {
                                r.cwd = m.cwd.as_deref().map(PathBuf::from);
                            }
                            hits.push(Hit {
                                id: r.id.clone(),
                                harness: r.harness.as_str().to_string(),
                                cwd: m.cwd,
                                title: Some(m.title),
                                score: 1.0,
                                snippet: m.snippet,
                                created_at: m.created_at,
                                updated_at: m.updated_at,
                                parent_id: prov.parent_id,
                                agent_id: prov.agent_id,
                                workflow: prov.workflow,
                                session: (!is_folded).then_some(r),
                            });
                        }
                        Ok(VerifyOutcome::NoMatch { metered_bytes }) => {
                            let spent = metered_bytes.min(remaining);
                            remaining -= spent;
                            suffix.read += 1;
                            suffix.bytes += spent;
                        }
                        Ok(VerifyOutcome::Stale) => {
                            suffix.read += 1;
                        }
                        Err(e) => {
                            if let Some(rest) = e.to_string().strip_prefix("over_budget:") {
                                let metered = rest.parse::<u64>().unwrap_or(0);
                                let spent = metered.min(remaining);
                                remaining -= spent;
                                suffix.bytes += spent;
                                suffix.cut += 1;
                                suffix.cut_bytes += metered;
                            } else {
                                suffix.failed += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. Query immutable segments and verify candidate revisions (retrying once if a concurrent
    //    compactor swapped `manifest.json` and unlinked an older `.trg` segment mid-scan).
    let live_hits_len = hits.len();
    let live_verified_docs = verified_docs;
    let live_suffix = suffix.clone();
    let live_remaining = remaining;
    for attempt in 0..2 {
        let seg_res = (|| -> Result<()> {
            if let Some(ref mf) = manifest {
                for seg_entry in &mf.segments {
                    let reader = SegmentReader::open(&dir.join(&seg_entry.file))?;
                    let deleted_slots = seg_entry.deleted_slot_bitmap();
                    let slots = reader.query_slots(&cq, &deleted_slots)?;
                    if slots.is_empty() {
                        continue;
                    }
                    for ord in 0..reader.num_sessions {
                        let matches_scope = match opts.filter.scope {
                            Scope::Exchange => slots.contains(ord * 2),
                            Scope::Context => slots.contains(ord * 2 + 1),
                            Scope::All => slots.contains(ord * 2) || slots.contains(ord * 2 + 1),
                        };
                        if !matches_scope {
                            continue;
                        }
                        let meta = &reader.metas[ord as usize];
                        if stale_ids.contains(&meta.id) {
                            continue;
                        }
                        if let Some(h) = harness_filter {
                            if meta.harness != h {
                                continue;
                            }
                        }
                        if let Some(cwd_sub) = opts.filter.cwd {
                            if !meta.cwd.as_deref().is_some_and(|c| c.contains(cwd_sub)) {
                                continue;
                            }
                        }
                        if let Some(ids) = allowed_ids {
                            let admitted =
                                ids.contains(&meta.id) || meta.parent_id.as_ref().is_some_and(|p| ids.contains(p));
                            if !admitted {
                                continue;
                            }
                        }
                        candidate_docs += 1;
                        let Some(doc_ref) = meta.to_session_ref() else {
                            continue;
                        };
                        verified_docs += 1;
                        match verify_session_exact(
                            &doc_ref,
                            Some(&meta.title),
                            None,
                            Some(&meta.baseline),
                            query,
                            opts.filter.scope,
                            opts.case_sensitive,
                            None,
                            &mut probe,
                        ) {
                            Ok(VerifyOutcome::Matched(m)) => {
                                hits.push(Hit {
                                    id: meta.id.clone(),
                                    harness: meta.harness.clone(),
                                    cwd: meta.cwd.clone(),
                                    title: Some(m.title),
                                    score: 1.0,
                                    snippet: m.snippet,
                                    created_at: meta.created_at,
                                    updated_at: meta.updated_at,
                                    parent_id: meta.parent_id.clone(),
                                    agent_id: meta.agent_id.clone(),
                                    workflow: meta.workflow.clone(),
                                    session: None,
                                });
                            }
                            Ok(VerifyOutcome::Stale) => {
                                // Session mutated between step 1 baseline check and step 3 segment
                                // verification: fall back to live verification under `opts.live`
                                // instead of silently dropping it from results!
                                suffix.stale += 1;
                                let mut live_probe = cv_core::source::Probe::default();
                                let live_snap = live_probe.snapshot(&doc_ref);
                                if live_snap.primary.is_none() {
                                    // Deleted mid-scan: neither live nor cut.
                                } else if opts.live == LiveRead::Frozen {
                                    suffix.cut += 1;
                                } else {
                                    let known_cost = cv_core::harness::for_harness(doc_ref.harness).and_then(|ad| {
                                        if ad.sqlite_source(&doc_ref) == Some(true) {
                                            None
                                        } else {
                                            Some(live_snap.primary_len())
                                        }
                                    });
                                    if known_cost.is_none() && opts.live != LiveRead::All && remaining == 0 {
                                        suffix.cut += 1;
                                    } else if let Some(cost) = known_cost.filter(|&c| c > remaining) {
                                        suffix.cut += 1;
                                        suffix.cut_bytes += cost;
                                    } else {
                                        match verify_session_exact(
                                            &doc_ref,
                                            None,
                                            opts.filter.cwd,
                                            None,
                                            query,
                                            opts.filter.scope,
                                            opts.case_sensitive,
                                            known_cost
                                                .is_none()
                                                .then(|| (opts.live != LiveRead::All).then_some(remaining))
                                                .flatten(),
                                            &mut live_probe,
                                        ) {
                                            Ok(VerifyOutcome::Matched(m)) => {
                                                let spent = known_cost.unwrap_or(m.metered_bytes).min(remaining);
                                                remaining -= spent;
                                                suffix.read += 1;
                                                suffix.bytes += spent;
                                                hits.push(Hit {
                                                    id: meta.id.clone(),
                                                    harness: meta.harness.clone(),
                                                    cwd: m.cwd.or_else(|| meta.cwd.clone()),
                                                    title: Some(m.title),
                                                    score: 1.0,
                                                    snippet: m.snippet,
                                                    created_at: m.created_at.or(meta.created_at),
                                                    updated_at: m.updated_at.or(meta.updated_at),
                                                    parent_id: meta.parent_id.clone(),
                                                    agent_id: meta.agent_id.clone(),
                                                    workflow: meta.workflow.clone(),
                                                    session: (meta.parent_id.is_none()).then_some(doc_ref),
                                                });
                                            }
                                            Ok(VerifyOutcome::NoMatch { metered_bytes }) => {
                                                let spent = known_cost.unwrap_or(metered_bytes).min(remaining);
                                                remaining -= spent;
                                                suffix.read += 1;
                                                suffix.bytes += spent;
                                            }
                                            Ok(VerifyOutcome::Stale) => {
                                                suffix.read += 1;
                                            }
                                            Err(e) => {
                                                if let Some(rest) = e.to_string().strip_prefix("over_budget:") {
                                                    let metered = rest.parse::<u64>().unwrap_or(0);
                                                    let spent = metered.min(remaining);
                                                    remaining -= spent;
                                                    suffix.bytes += spent;
                                                    suffix.cut += 1;
                                                    suffix.cut_bytes += metered;
                                                } else {
                                                    suffix.failed += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(VerifyOutcome::NoMatch { .. }) | Err(_) => {}
                        }
                    }
                }
            }
            Ok(())
        })();
        match seg_res {
            Ok(()) => break,
            Err(err) if attempt == 0 && is_not_found_err(&err) => {
                hits.truncate(live_hits_len);
                candidate_docs = 0;
                verified_docs = live_verified_docs;
                suffix = live_suffix.clone();
                remaining = live_remaining;
                manifest = Manifest::load(dir)?;
            }
            Err(err) => return Err(err),
        }
    }

    sort_hits(&mut hits);
    let total = hits.len();
    if hits.len() > opts.limit {
        hits.truncate(opts.limit);
    }

    let mut notes = Vec::new();
    notes.extend(suffix.read_line().map(Note::Live));
    notes.extend(suffix.unread_line().map(Note::Stale));
    if hits.is_empty() && !suffix.no_index && !manifest.as_ref().is_some_and(|m| m.has_subagents) {
        notes.push(Note::NoSubagents);
    }

    Ok(TrigramFound {
        hits,
        total,
        suffix,
        notes,
        candidate_docs,
        verified_docs,
    })
}

/// Return matching session IDs for an exact substring query across indexed segments + live tail.
pub fn search_ids(
    dir: &Path,
    query: &str,
    scope: Scope,
    case_sensitive: bool,
    live: LiveRead,
    refs: &[cv_core::SessionRef],
) -> Result<(HashSet<String>, Suffix)> {
    let found = search(
        dir,
        query,
        TrigramOptions {
            filter: SearchFilter {
                harness: None,
                cwd: None,
                scope,
            },
            case_sensitive,
            live,
            limit: usize::MAX,
        },
        refs,
        None,
    )?;
    let ids = found.hits.into_iter().map(|h| h.id).collect();
    Ok((ids, found.suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::{Harness, SessionRef};
    use serde_json::json;

    struct ScratchDir(PathBuf);
    impl ScratchDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("cv-trigram-test-{}", uuid::Uuid::new_v4().simple()));
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

    fn write_claude_jsonl(path: &Path, id: &str, cwd: &str, lines: &[serde_json::Value]) -> SessionRef {
        let mut out = String::new();
        for line in lines {
            out.push_str(&serde_json::to_string(line).unwrap());
            out.push('\n');
        }
        std::fs::write(path, out).unwrap();
        let mut r = SessionRef::new(id, Harness::Claude, path);
        r.cwd = Some(PathBuf::from(cwd));
        r
    }

    /// Ground-truth brute-force exact substring oracle: materializes the full scoped text streams
    /// of every current session on disk (plus folded subagents when enabled) and tests substring
    /// containment directly with `str::contains` / `str::to_lowercase().contains`.
    fn brute_force_oracle(
        refs: &[SessionRef],
        subagents: bool,
        query: &str,
        scope: Scope,
        case_sensitive: bool,
        harness: Option<&str>,
        cwd: Option<&str>,
    ) -> HashSet<String> {
        if query.is_empty() {
            return HashSet::new();
        }
        let expanded = expand_sessions(refs, subagents).unwrap();
        let mut out = HashSet::new();
        for (r, _prov) in expanded {
            if !r.path.exists() {
                continue;
            }
            if let Some(h) = harness {
                if r.harness.as_str() != h {
                    continue;
                }
            }
            let Some((exchange_body, context_body, title, parsed_cwd)) = materialize_scoped_streams(&r) else {
                continue;
            };
            if let Some(c) = cwd {
                let eff_cwd = r.cwd.as_ref().map(|p| p.display().to_string()).or(parsed_cwd);
                if !eff_cwd.as_deref().is_some_and(|p| p.contains(c)) {
                    continue;
                }
            }
            let matches_str = |hay: &str| -> bool {
                if case_sensitive {
                    hay.contains(query)
                } else {
                    hay.to_lowercase().contains(&query.to_lowercase())
                }
            };
            let hit = match scope {
                Scope::Exchange => matches_str(&exchange_body) || matches_str(&title),
                Scope::Context => matches_str(&context_body),
                Scope::All => matches_str(&exchange_body) || matches_str(&context_body) || matches_str(&title),
            };
            if hit {
                out.insert(r.id);
            }
        }
        out
    }

    fn materialize_scoped_streams(r: &SessionRef) -> Option<(String, String, String, Option<String>)> {
        struct FullCollector {
            exchange: String,
            context: String,
            in_context: bool,
            first_prompt: Option<String>,
            meta_title: Option<String>,
            meta_cwd: Option<String>,
            copied_before: Option<chrono::DateTime<chrono::Utc>>,
            resolver: cv_core::Resolver,
        }
        impl Searchable for FullCollector {
            fn str(&mut self, s: &str) {
                if self.in_context {
                    self.context.push_str(s);
                } else {
                    self.exchange.push_str(s);
                }
            }
            fn text(
                &mut self,
                text: &cv_core::Text,
                resolver: &cv_core::Resolver,
                _: &mut dyn FnMut() -> Result<()>,
            ) -> Result<()> {
                text.try_for_each_chunk(resolver, 4096, |part| {
                    Searchable::str(self, part);
                    Ok(())
                })
            }
            fn json(&mut self, json: &cv_core::Json, _: &mut dyn FnMut() -> Result<()>) -> Result<()> {
                let mut s = String::new();
                cv_core::projection::append_json(&mut s, json)?;
                Searchable::str(self, &s);
                Ok(())
            }
            fn after_block(&mut self, block: &cv_core::Block) -> Result<()> {
                if let Some(path) = block.persisted_output_path() {
                    let read = cv_core::lazy::read_pieces_checked(Path::new(path), 4096, |piece| {
                        Searchable::str(self, piece);
                        Ok(())
                    })?;
                    if matches!(read, cv_core::lazy::PersistedHead::Text(_)) {
                        Searchable::str(self, "\n");
                    }
                }
                Ok(())
            }
        }
        impl MessageSink for FullCollector {
            fn meta(&mut self, s: &cv_core::Session) {
                self.meta_title = s.title.clone();
                if self.meta_cwd.is_none() {
                    self.meta_cwd = s.cwd.as_ref().map(|p| p.display().to_string());
                }
            }
            fn message(&mut self, m: cv_core::Message) -> Flow {
                let res = std::mem::replace(&mut self.resolver, cv_core::Resolver::new(None));
                if self.first_prompt.is_none() && m.role == cv_core::Role::User {
                    self.first_prompt = m.prompt_title(&res).ok().flatten();
                }
                let copied = matches!((m.timestamp, self.copied_before), (Some(at), Some(before)) if at < before);
                if !copied {
                    self.in_context = !m.kind.is_exchange();
                    let _ = cv_core::projection::searchable(&m.content, &res, self, &mut || Ok(()));
                }
                self.resolver = res;
                Flow::Continue
            }
        }

        let adapter = cv_core::harness::for_harness(r.harness)?;
        let mut col = FullCollector {
            exchange: String::new(),
            context: String::new(),
            in_context: false,
            first_prompt: None,
            meta_title: None,
            meta_cwd: None,
            copied_before: None,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
        };
        col.copied_before = adapter.copied_before(r, &mut col);
        let meta = adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut col).ok()?;
        let title = cv_core::label_from(
            meta.title
                .as_deref()
                .or(col.meta_title.as_deref())
                .or(r.title.as_deref()),
            col.first_prompt.as_deref(),
        );
        let cwd = col.meta_cwd.or_else(|| meta.cwd.map(|p| p.display().to_string()));
        Some((col.exchange, col.context, title, cwd))
    }

    #[test]
    fn trigram_matches_brute_force_oracle_across_symbols_lengths_boundaries_cases_and_live_tail() {
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        let index_dir = tmp.path().join("trigram");
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Session 1: code symbols, CLI flags, Rust paths, hex SHA, Unicode Greek sigma & lambda.
        let s1 = write_claude_jsonl(
            &sessions_dir.join("s1.jsonl"),
            "sess-code",
            "/repo/alpha",
            &[
                json!({
                    "type": "user",
                    "sessionId": "sess-code",
                    "uuid": "u1",
                    "message": {
                        "role": "user",
                        "content": "Check FileId::admits and pass --live-bytes=65536 on commit c9120a2e6bb24f01"
                    }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "sess-code",
                    "uuid": "a1",
                    "message": {
                        "role": "assistant",
                        "content": "Updated crates/cv-search/src/fts/live.rs with snake_case_ident, λx.x, and ΟΣ token."
                    }
                }),
            ],
        );

        // Session 2: giant multi-piece content where needle straddles exact 4096-byte piece boundaries!
        let pad = "a".repeat(PIECE_BYTES - 5);
        let boundary_payload = format!("{pad}CrossChunk::BoundarySymbol_42 trailing text");
        let s2 = write_claude_jsonl(
            &sessions_dir.join("s2.jsonl"),
            "sess-boundary",
            "/repo/alpha",
            &[json!({
                "type": "user",
                "sessionId": "sess-boundary",
                "uuid": "u2",
                "message": { "role": "user", "content": boundary_payload }
            })],
        );

        // Session 3: Context-only instruction block vs Exchange turn (verifies zero cross-scope leakage).
        let s3 = write_claude_jsonl(
            &sessions_dir.join("s3.jsonl"),
            "sess-scoped",
            "/repo/beta",
            &[
                json!({
                    "type": "user",
                    "sessionId": "sess-scoped",
                    "uuid": "u3-meta",
                    "isMeta": true,
                    "message": { "role": "user", "content": "ONLY_IN_CONTEXT_TOKEN --frozen-gate ABC_" }
                }),
                json!({
                    "type": "user",
                    "sessionId": "sess-scoped",
                    "uuid": "u3-ex",
                    "message": { "role": "user", "content": "_DEF exchange_only_marker {x: 1}" }
                }),
            ],
        );

        // Session 4: Trigram false-positive trap ("abXcd" + "bcYde" has trigrams "abc", "bcd", "cde" if unordered,
        // specifically "abcd" has "abc" and "bcd": doc has "abcX" and "Ybcd", so BOTH trigrams exist in the doc
        // WITHOUT "abcd" occurring as a contiguous substring!).
        let s4 = write_claude_jsonl(
            &sessions_dir.join("s4.jsonl"),
            "sess-trap",
            "/repo/beta",
            &[json!({
                "type": "user",
                "sessionId": "sess-trap",
                "uuid": "u4",
                "message": {
                    "role": "user",
                    "content": "trigram collision trap: FileId::ZZZ and ZZZ::admits and --live_ZZZ plus ZZZ-bytes"
                }
            })],
        );

        let mut refs = vec![s1.clone(), s2.clone(), s3.clone(), s4.clone()];
        let synced = sync_sessions(&index_dir, &refs, true, false).unwrap();
        assert_eq!(synced.indexed, 4);
        assert_eq!(synced.unreadable.len(), 0);

        // Session 5: Live unindexed tail session added AFTER sync_sessions!
        let s5 = write_claude_jsonl(
            &sessions_dir.join("s5.jsonl"),
            "sess-live-tail",
            "/repo/alpha",
            &[json!({
                "type": "user",
                "sessionId": "sess-live-tail",
                "uuid": "u5",
                "message": {
                    "role": "user",
                    "content": "live tail addition: TantivyColumnar::open_v2 and --live-bytes=65536"
                }
            })],
        );
        refs.push(s5.clone());

        // Mutate Session 1 on disk to REMOVE c9120a2e6bb24f01 and ADD d4e5f6a7b8c90123 (tests stale shadowing!).
        let s1_mutated = write_claude_jsonl(
            &sessions_dir.join("s1.jsonl"),
            "sess-code",
            "/repo/alpha",
            &[json!({
                "type": "user",
                "sessionId": "sess-code",
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": "Check FileId::admits and pass --live-bytes=65536 on commit d4e5f6a7b8c90123 with λx.x and ΟΣ "
                }
            })],
        );
        refs[0] = s1_mutated;

        let queries = [
            // (a) Code identifiers, flags, Rust paths, file paths, hex SHAs
            "FileId::admits",
            "fileid::admits",
            "--live-bytes",
            "--live-bytes=65536",
            "c9120a2e6bb24f01", // removed by mutation -> must be 0 hits!
            "d4e5f6a7b8c90123", // added by mutation -> must match live!
            "TantivyColumnar::open_v2",
            // (b) 1-char and 2-char queries (ASCII & multi-byte UTF-8)
            "{",
            "x",
            "λ",
            "::",
            "--",
            "ΟΣ",
            "ος",
            // (c) Cross-chunk boundary symbol
            "CrossChunk::BoundarySymbol_42",
            "aaCrossChunk::Boundary",
            // (d) Scope isolation & cross-scope non-leakage ("ABC__DEF" must NEVER match!)
            "ONLY_IN_CONTEXT_TOKEN",
            "exchange_only_marker",
            "ABC__DEF",
            // (e) Trigram collision trap (has all trigrams of "ZZZ::admits" vs "FileId::admits")
            "ZZZ::admits",
        ];

        for query in queries {
            for scope in [Scope::Exchange, Scope::Context, Scope::All] {
                for case_sensitive in [false, true] {
                    let expected = brute_force_oracle(&refs, false, query, scope, case_sensitive, None, None);
                    let found = search(
                        &index_dir,
                        query,
                        TrigramOptions {
                            filter: SearchFilter {
                                harness: None,
                                cwd: None,
                                scope,
                            },
                            case_sensitive,
                            live: LiveRead::All,
                            limit: 100,
                        },
                        &refs,
                        None,
                    )
                    .unwrap();
                    let actual: HashSet<String> = found.hits.iter().map(|h| h.id.clone()).collect();
                    assert_eq!(
                        actual, expected,
                        "oracle mismatch for query={query:?} scope={scope:?} case_sensitive={case_sensitive}"
                    );
                }
            }
        }

        // Verify candidate pruning: querying "CrossChunk::BoundarySymbol_42" only verifies 1 indexed candidate (+ 2 stale live)!
        let pruned = search(
            &index_dir,
            "CrossChunk::BoundarySymbol_42",
            TrigramOptions {
                live: LiveRead::Frozen,
                case_sensitive: true,
                ..Default::default()
            },
            &refs,
            None,
        )
        .unwrap();
        assert_eq!(pruned.candidate_docs, 1);
        assert_eq!(pruned.hits.len(), 1);
        assert_eq!(pruned.hits[0].id, "sess-boundary");
        // And under --frozen, the 2 stale sessions are reported in Suffix!
        assert_eq!(pruned.suffix.stale, 2);
        assert_eq!(pruned.suffix.read, 0);

        // Re-sync incrementally and verify compaction + 0 stale sessions afterward!
        let resynced = sync_sessions(&index_dir, &refs, false, false).unwrap();
        assert_eq!(resynced.indexed, 2); // s1 (mutated) + s5 (new)
        assert_eq!(resynced.unchanged, 3); // s2, s3, s4

        let post_sync = search(
            &index_dir,
            "d4e5f6a7b8c90123",
            TrigramOptions {
                live: LiveRead::Frozen,
                case_sensitive: true,
                ..Default::default()
            },
            &refs,
            None,
        )
        .unwrap();
        assert_eq!(post_sync.suffix.stale, 0);
        assert_eq!(post_sync.candidate_docs, 1);
        assert_eq!(post_sync.hits.len(), 1);
        assert_eq!(post_sync.hits[0].id, "sess-code");
    }

    #[test]
    fn trigram_compacts_fragmented_segments_folds_subagents_and_enforces_live_budget() {
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        let index_dir = tmp.path().join("trigram");
        let sessions_dir = tmp.path().join("projects").join("-proj");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Parent Claude session + nested subagent transcript under <parent>/subagents/agent-w1.jsonl
        let parent_id = "11111111-1111-1111-1111-111111111111";
        let parent_path = sessions_dir.join(format!("{parent_id}.jsonl"));
        let parent = write_claude_jsonl(
            &parent_path,
            parent_id,
            "/proj",
            &[json!({
                "type": "user",
                "sessionId": parent_id,
                "uuid": "u-p",
                "message": { "role": "user", "content": "parent orchestrator symbol ParentRoot::run" }
            })],
        );
        let sub_dir = sessions_dir.join(parent_id).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(
            sub_dir.join("agent-worker42.jsonl"),
            format!(
                "{}\n",
                json!({
                    "type": "user",
                    "sessionId": parent_id,
                    "agentId": "worker42",
                    "uuid": "u-sub",
                    "message": { "role": "user", "content": "subagent exclusive needle SubAgent::deep_verify_99" }
                })
            ),
        )
        .unwrap();

        // Sync with `subagents = true`, then do a plain refresh `subagents = false`: subagent hit must survive!
        let s_sub = sync_sessions(&index_dir, std::slice::from_ref(&parent), true, true).unwrap();
        assert_eq!(s_sub.indexed, 2);
        let s_plain = sync_sessions(&index_dir, std::slice::from_ref(&parent), false, false).unwrap();
        assert_eq!(s_plain.removed, 0);

        let mut allow_parent = HashSet::new();
        allow_parent.insert(parent_id.to_string());
        let sub_hit = search(
            &index_dir,
            "SubAgent::deep_verify_99",
            TrigramOptions {
                case_sensitive: true,
                live: LiveRead::Frozen,
                ..Default::default()
            },
            std::slice::from_ref(&parent),
            Some(&allow_parent),
        )
        .unwrap();
        assert_eq!(sub_hit.hits.len(), 1);
        assert_eq!(sub_hit.hits[0].parent_id.as_deref(), Some(parent_id));
        assert_eq!(sub_hit.hits[0].agent_id.as_deref(), Some("worker42"));

        // Create multiple segments and update >50% of sessions to trigger segment compaction.
        let mut batch = vec![parent.clone()];
        for i in 0..4 {
            let r = write_claude_jsonl(
                &sessions_dir.join(format!("extra-{i}.jsonl")),
                &format!("extra-{i}"),
                "/proj",
                &[json!({
                    "type": "user",
                    "sessionId": format!("extra-{i}"),
                    "uuid": "u",
                    "message": { "role": "user", "content": format!("SegInitial::item_{i}") }
                })],
            );
            batch.push(r);
            sync_sessions(&index_dir, &batch, false, false).unwrap();
        }
        // Rewrite extra-0..3 so old single-doc/multi-doc segments exceed 50% dead ratio and compact.
        for i in 0..4 {
            batch[i + 1] = write_claude_jsonl(
                &sessions_dir.join(format!("extra-{i}.jsonl")),
                &format!("extra-{i}"),
                "/proj",
                &[json!({
                    "type": "user",
                    "sessionId": format!("extra-{i}"),
                    "uuid": "u2",
                    "message": { "role": "user", "content": format!("SegCompacted::item_{i}") }
                })],
            );
        }
        let post_rewrite = sync_sessions(&index_dir, &batch, false, false).unwrap();
        assert_eq!(post_rewrite.indexed, 4);
        // Old symbol is completely gone; new symbol is indexed and verified.
        let gone = search(
            &index_dir,
            "SegInitial::item_0",
            TrigramOptions::default(),
            &batch,
            None,
        )
        .unwrap();
        assert!(gone.hits.is_empty());
        let kept = search(
            &index_dir,
            "SegCompacted::item_3",
            TrigramOptions::default(),
            &batch,
            None,
        )
        .unwrap();
        assert_eq!(kept.hits.len(), 1);

        // LiveRead::Within budget test: add two unindexed sessions (small newest, large older) and budget only the small one.
        let older_large = write_claude_jsonl(
            &sessions_dir.join("unindexed-large.jsonl"),
            "unindexed-large",
            "/proj",
            &[json!({
                "type": "user",
                "sessionId": "unindexed-large",
                "uuid": "ul",
                "message": { "role": "user", "content": format!("BudgetNeedle::shared {}", "z".repeat(4096)) }
            })],
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        let newer_small = write_claude_jsonl(
            &sessions_dir.join("unindexed-small.jsonl"),
            "unindexed-small",
            "/proj",
            &[json!({
                "type": "user",
                "sessionId": "unindexed-small",
                "uuid": "us",
                "message": { "role": "user", "content": "BudgetNeedle::shared tiny" }
            })],
        );
        let small_len = std::fs::metadata(&newer_small.path).unwrap().len();
        batch.push(older_large);
        batch.push(newer_small);

        let budgeted = search(
            &index_dir,
            "BudgetNeedle::shared",
            TrigramOptions {
                live: LiveRead::Within(small_len + 64, crate::Budget::Flag),
                case_sensitive: true,
                ..Default::default()
            },
            &batch,
            None,
        )
        .unwrap();
        assert_eq!(budgeted.hits.len(), 1);
        assert_eq!(budgeted.hits[0].id, "unindexed-small");
        assert_eq!(budgeted.suffix.stale, 2);
        assert_eq!(budgeted.suffix.read, 1);
        assert_eq!(budgeted.suffix.cut, 1);
    }

    #[test]
    fn trigram_segment_reader_rejects_forged_directory_bounds_cleanly() {
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        let sess = write_claude_jsonl(
            &tmp.path().join("s.jsonl"),
            "s",
            "/proj",
            &[json!({
                "type": "user",
                "sessionId": "s",
                "uuid": "u1",
                "message": { "role": "user", "content": "FileId::admits" }
            })],
        );
        let idx_dir = tmp.path().join("trigram");
        sync_sessions(&idx_dir, &[sess], true, false).unwrap();
        let seg_path = std::fs::read_dir(&idx_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "trg"))
            .unwrap();
        let orig = std::fs::read(&seg_path).unwrap();
        let footer_start = orig.len() - FOOTER_BYTES;

        // Forge `trigram_dir_offset = u64::MAX - 8` with valid recomputed FNV-1a checksum.
        let mut forged = orig.clone();
        forged[footer_start + 24..footer_start + 32].copy_from_slice(&(u64::MAX - 8).to_le_bytes());
        let crc = fnv1a_32(&forged[..footer_start], &forged[footer_start..footer_start + 56]);
        forged[footer_start + 56..footer_start + 60].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&seg_path, &forged).unwrap();
        assert!(SegmentReader::open(&seg_path).is_err());

        // Forge `num_trigrams = u32::MAX` with valid recomputed FNV-1a checksum.
        let mut forged2 = orig;
        forged2[footer_start + 32..footer_start + 36].copy_from_slice(&u32::MAX.to_le_bytes());
        let crc2 = fnv1a_32(&forged2[..footer_start], &forged2[footer_start..footer_start + 56]);
        forged2[footer_start + 56..footer_start + 60].copy_from_slice(&crc2.to_le_bytes());
        std::fs::write(&seg_path, &forged2).unwrap();
        assert!(SegmentReader::open(&seg_path).is_err());
    }

    #[test]
    fn trigram_folded_subagent_live_tail_and_parsed_cwd_filter_match_oracle() {
        use std::io::Write;
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        let index_dir = tmp.path().join("trigram");
        let sessions_dir = tmp.path().join("projects").join("-proj");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Part A: Folded subagents modified or newly spawned AFTER `sync_sessions(..., true)`.
        let parent_id = "22222222-2222-2222-2222-222222222222";
        let parent_path = sessions_dir.join(format!("{parent_id}.jsonl"));
        let parent = write_claude_jsonl(
            &parent_path,
            parent_id,
            "/repo/alpha",
            &[json!({
                "type": "user",
                "sessionId": parent_id,
                "uuid": "up",
                "message": { "role": "user", "content": "ParentRoot::orchestrate" }
            })],
        );
        let sub_dir = sessions_dir.join(parent_id).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub1_path = sub_dir.join("agent-sub1.jsonl");
        std::fs::write(
            &sub1_path,
            format!(
                "{}\n",
                json!({
                    "type": "user",
                    "sessionId": parent_id,
                    "agentId": "sub1",
                    "cwd": "/repo/alpha",
                    "uuid": "u-s1-1",
                    "message": { "role": "user", "content": "Sub1::turn_one initial finding" }
                })
            ),
        )
        .unwrap();

        let synced = sync_sessions(&index_dir, std::slice::from_ref(&parent), true, true).unwrap();
        assert_eq!(synced.indexed, 2);

        // Append a second turn to `sub1` AND spawn `sub2` on disk without re-indexing.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&sub1_path).unwrap();
            writeln!(
                f,
                "{}",
                json!({
                    "type": "assistant",
                    "sessionId": parent_id,
                    "agentId": "sub1",
                    "cwd": "/repo/alpha",
                    "uuid": "a-s1-2",
                    "message": { "role": "assistant", "content": "Sub1::turn_two appended after sync" }
                })
            )
            .unwrap();
        }
        let sub2_path = sub_dir.join("agent-sub2.jsonl");
        std::fs::write(
            &sub2_path,
            format!(
                "{}\n",
                json!({
                    "type": "user",
                    "sessionId": parent_id,
                    "agentId": "sub2",
                    "cwd": "/repo/alpha",
                    "uuid": "u-s2-1",
                    "message": { "role": "user", "content": "Sub2::fresh_spawn brand new worker" }
                })
            ),
        )
        .unwrap();

        for (needle, expected_agent) in [
            ("Sub1::turn_one", "sub1"),
            ("Sub1::turn_two", "sub1"),
            ("Sub2::fresh_spawn", "sub2"),
        ] {
            let expected = brute_force_oracle(
                std::slice::from_ref(&parent),
                true,
                needle,
                Scope::Exchange,
                true,
                None,
                None,
            );
            let got = search(
                &index_dir,
                needle,
                TrigramOptions {
                    case_sensitive: true,
                    ..Default::default()
                },
                std::slice::from_ref(&parent),
                None,
            )
            .unwrap();
            let got_ids: HashSet<String> = got.hits.iter().map(|h| h.id.clone()).collect();
            assert_eq!(got_ids, expected, "subagent live tail mismatch on {needle:?}");
            assert_eq!(got.hits.len(), 1);
            assert_eq!(got.hits[0].parent_id.as_deref(), Some(parent_id));
            assert_eq!(got.hits[0].agent_id.as_deref(), Some(expected_agent));
            assert!(
                got.hits[0].session.is_none(),
                "folded subagent hit must preserve subagent provenance instead of top-level live session"
            );
        }

        // Part B: SessionRef with `r.cwd = None` whose JSONL sets `"cwd": "/repo/target-only"`.
        let p_target = sessions_dir.join("cwd-target.jsonl");
        let mut r_target = write_claude_jsonl(
            &p_target,
            "cwd-target",
            "/repo/target-only",
            &[json!({
                "type": "user",
                "sessionId": "cwd-target",
                "cwd": "/repo/target-only",
                "uuid": "ut1",
                "message": { "role": "user", "content": "CwdDeferred::shared_needle turn1" }
            })],
        );
        r_target.cwd = None;

        let p_other = sessions_dir.join("cwd-other.jsonl");
        let mut r_other = write_claude_jsonl(
            &p_other,
            "cwd-other",
            "/repo/unrelated",
            &[json!({
                "type": "user",
                "sessionId": "cwd-other",
                "cwd": "/repo/unrelated",
                "uuid": "uo1",
                "message": { "role": "user", "content": "CwdDeferred::shared_needle turn1" }
            })],
        );
        r_other.cwd = None;

        let cwd_refs = vec![r_target.clone(), r_other];
        let opts_cwd = TrigramOptions {
            filter: SearchFilter {
                cwd: Some("target-only"),
                ..Default::default()
            },
            case_sensitive: true,
            ..Default::default()
        };

        // 1) Unindexed live tail with `r.cwd == None`:
        let live_res = search(&index_dir, "CwdDeferred::shared_needle", opts_cwd, &cwd_refs, None).unwrap();
        let oracle_ids = brute_force_oracle(
            &cwd_refs,
            false,
            "CwdDeferred::shared_needle",
            Scope::Exchange,
            true,
            None,
            Some("target-only"),
        );
        assert_eq!(
            live_res.hits.iter().map(|h| h.id.clone()).collect::<HashSet<_>>(),
            oracle_ids
        );
        assert_eq!(live_res.hits.len(), 1);
        assert_eq!(live_res.hits[0].id, "cwd-target");
        assert_eq!(live_res.hits[0].cwd.as_deref(), Some("/repo/target-only"));

        // 2) Synced into segment, then appended on disk while `r.cwd` remains `None`:
        let cwd_idx = tmp.path().join("trigram-cwd");
        sync_sessions(&cwd_idx, &cwd_refs, true, false).unwrap();
        let seg_res = search(&cwd_idx, "CwdDeferred::shared_needle", opts_cwd, &cwd_refs, None).unwrap();
        assert_eq!(seg_res.suffix.stale, 0);
        assert_eq!(seg_res.hits.len(), 1);
        assert_eq!(seg_res.hits[0].id, "cwd-target");

        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p_target).unwrap();
            writeln!(
                f,
                "{}",
                json!({
                    "type": "assistant",
                    "sessionId": "cwd-target",
                    "cwd": "/repo/target-only",
                    "uuid": "at2",
                    "message": { "role": "assistant", "content": "CwdDeferred::turn2_after_append" }
                })
            )
            .unwrap();
        }
        let appended_res = search(&cwd_idx, "CwdDeferred::turn2_after_append", opts_cwd, &cwd_refs, None).unwrap();
        assert_eq!(appended_res.hits.len(), 1);
        assert_eq!(appended_res.hits[0].id, "cwd-target");
        assert_eq!(appended_res.hits[0].cwd.as_deref(), Some("/repo/target-only"));
    }

    #[test]
    fn frozen_trigram_search_without_manifest_bails_and_concurrent_writers_serialize() {
        let tmp = ScratchDir::new();
        let empty_idx = tmp.path().join("unbuilt-trigram");
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let r1 = write_claude_jsonl(
            &sessions_dir.join("s1.jsonl"),
            "s1",
            "/repo/frozen",
            &[json!({
                "type": "user",
                "sessionId": "s1",
                "cwd": "/repo/frozen",
                "uuid": "u1",
                "message": { "role": "user", "content": "FrozenNeedle::alpha" }
            })],
        );
        let r2 = write_claude_jsonl(
            &sessions_dir.join("s2.jsonl"),
            "s2",
            "/repo/frozen",
            &[json!({
                "type": "user",
                "sessionId": "s2",
                "cwd": "/repo/frozen",
                "uuid": "u2",
                "message": { "role": "user", "content": "FrozenNeedle::beta" }
            })],
        );

        let err = search(
            &empty_idx,
            "FrozenNeedle",
            TrigramOptions {
                live: LiveRead::Frozen,
                ..Default::default()
            },
            std::slice::from_ref(&r1),
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("cv index --exact"),
            "frozen search without manifest must fail naming `cv index --exact`, got: {err:#}"
        );

        // Concurrent `sync_sessions` writers on the same directory serialize via `.trigram-writer.lock`.
        let shared_idx = tmp.path().join("shared-trigram");
        let all_refs = vec![r1, r2];
        std::thread::scope(|s| {
            let idx_a = &shared_idx;
            let refs_a = &all_refs;
            let h1 = s.spawn(move || sync_sessions(idx_a, refs_a, false, false).unwrap());
            let h2 = s.spawn(move || sync_sessions(idx_a, refs_a, false, false).unwrap());
            h1.join().unwrap();
            h2.join().unwrap();
        });

        let res = search(
            &shared_idx,
            "FrozenNeedle::",
            TrigramOptions {
                live: LiveRead::Frozen,
                case_sensitive: true,
                ..Default::default()
            },
            &all_refs,
            None,
        )
        .unwrap();
        assert_eq!(res.hits.len(), 2);
    }

    #[test]
    fn trigram_rejects_overflow_deleted_ords_debits_nonmatching_sqlite_live_bytes_and_recovers_mid_scan_stale() {
        use cv_core::Adapter as _;
        let tmp = ScratchDir::new();
        let home = tmp.path().join("cv-home");
        std::fs::create_dir_all(&home).unwrap();
        let _env = cv_core::env::Env::from_vars([
            ("CLUSTERVISION_HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_CACHE", home.to_str().unwrap()),
        ])
        .enter();

        // 1. Overflow `deleted_ords` (`u32::MAX`) cannot panic in `deleted_slot_bitmap` or `Manifest::load`.
        let forged_seg = SegmentEntry {
            file: "seg-0.trg".into(),
            num_sessions: u32::MAX,
            deleted_ords: vec![u32::MAX, u32::MAX - 1],
        };
        assert!(forged_seg.deleted_slot_bitmap().is_empty());

        let bad_idx = tmp.path().join("bad-manifest-idx");
        std::fs::create_dir_all(&bad_idx).unwrap();
        let bad_manifest = serde_json::json!({
            "version": MANIFEST_VERSION,
            "interpretation_version": cv_core::source::VERSION,
            "has_subagents": false,
            "segments": [{
                "file": "seg-0.trg",
                "num_sessions": u32::MAX,
                "deleted_ords": [u32::MAX]
            }],
            "sessions": {}
        });
        std::fs::write(
            bad_idx.join("manifest.json"),
            serde_json::to_vec(&bad_manifest).unwrap(),
        )
        .unwrap();
        assert!(Manifest::load(&bad_idx).is_err());

        // 2. Metered SQLite `--live-bytes` debits non-matching sessions (`NoMatch`) so budget is enforced!
        let db_path = tmp.path().join("hermes-live.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, started_at REAL, ended_at REAL, title TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL);",
        )
        .unwrap();
        let large_miss = "x".repeat(4_000);
        conn.execute(
            "INSERT INTO sessions(id, started_at, ended_at, title) VALUES ('s-miss', 100.0, 200.0, 'Miss')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, role, content, timestamp) VALUES ('s-miss', 'user', ?1, 200.0)",
            [&large_miss],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions(id, started_at, ended_at, title) VALUES ('s-older', 90.0, 100.0, 'Older')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(session_id, role, content, timestamp) VALUES ('s-older', 'user', ?1, 100.0)",
            [&format!(" MeteredNeedle::target {}", "y".repeat(4_000))],
        )
        .unwrap();
        drop(conn);

        let hermes = cv_core::harness::hermes::Hermes::with_db(db_path);
        let h_refs = hermes.discover().unwrap().refs;
        assert_eq!(h_refs.len(), 2);

        let sql_idx = tmp.path().join("sql-trigram");
        let budgeted = search(
            &sql_idx,
            "MeteredNeedle::target",
            TrigramOptions {
                live: LiveRead::Within(5_000, crate::fts::Budget::Flag),
                ..Default::default()
            },
            &h_refs,
            None,
        )
        .unwrap();
        assert!(
            budgeted.suffix.bytes >= 4_000,
            "non-matching metered SQLite session must debit suffix.bytes, got {:?}",
            budgeted.suffix
        );
        assert_eq!(
            budgeted.suffix.cut, 1,
            "after non-matching 4KB session debits a 5KB budget, second 4KB SQLite session must be cut: {:?}",
            budgeted.suffix
        );

        // 2b. Multiple oversized SQLite sessions (`over_budget`) debit `remaining` to 0 on the first
        //     session and cut all subsequent SQLite sessions immediately without opening/streaming them!
        let over_db = tmp.path().join("hermes-over-budget.db");
        let over_conn = rusqlite::Connection::open(&over_db).unwrap();
        over_conn
            .execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, started_at REAL, ended_at REAL, title TEXT);
                 CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL);",
            )
            .unwrap();
        let huge_chunk = "z".repeat(200_000);
        for (idx, ts) in [(1, 300.0), (2, 200.0), (3, 100.0)] {
            let sid = format!("over-{idx}");
            over_conn
                .execute(
                    "INSERT INTO sessions(id, started_at, ended_at, title) VALUES (?1, ?2, ?2, ?1)",
                    rusqlite::params![sid, ts],
                )
                .unwrap();
            over_conn
                .execute(
                    "INSERT INTO messages(session_id, role, content, timestamp) VALUES (?1, 'user', ?2, ?3)",
                    rusqlite::params![sid, huge_chunk, ts],
                )
                .unwrap();
        }
        drop(over_conn);
        let over_refs = cv_core::harness::hermes::Hermes::with_db(over_db)
            .discover()
            .unwrap()
            .refs;
        assert_eq!(over_refs.len(), 3);
        let over_res = search(
            &sql_idx,
            "MeteredNeedle::target",
            TrigramOptions {
                live: LiveRead::Within(1_000, crate::fts::Budget::Flag),
                ..Default::default()
            },
            &over_refs,
            None,
        )
        .unwrap();
        assert_eq!(
            over_res.verified_docs, 1,
            "once first over_budget SQLite session exhausts remaining=1000 to 0, remaining 2 must cut before streaming"
        );
        assert_eq!(over_res.suffix.cut, 3);
        assert_eq!(over_res.suffix.bytes, 1_000);

        // 3. Segment TOCTOU: when an indexed session's file mutates without `refs` flagging it in step 1
        //    (e.g. `refs` is empty on daemon/cached search after indexing), step 3 detects `Stale` and
        //    falls back to live verification instead of silently dropping the hit!
        let mut_path = tmp.path().join("mutated.jsonl");
        let mut_ref = write_claude_jsonl(
            &mut_path,
            "mut-1",
            "/repo/toctou",
            &[json!({
                "type": "user",
                "sessionId": "mut-1",
                "cwd": "/repo/toctou",
                "uuid": "u1",
                "message": { "role": "user", "content": "ToctouNeedle::survives" }
            })],
        );
        let toctou_idx = tmp.path().join("toctou-trigram");
        sync_sessions(&toctou_idx, std::slice::from_ref(&mut_ref), false, false).unwrap();

        // Append a new turn AFTER indexing, then search with `&[]` (`refs` empty, as when searching
        // without re-running full discovery) so step 3 itself observes `VerifyOutcome::Stale`.
        write_claude_jsonl(
            &mut_path,
            "mut-1",
            "/repo/toctou",
            &[
                json!({
                    "type": "user",
                    "sessionId": "mut-1",
                    "cwd": "/repo/toctou",
                    "uuid": "u1",
                    "message": { "role": "user", "content": "ToctouNeedle::survives" }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "mut-1",
                    "cwd": "/repo/toctou",
                    "uuid": "u2",
                    "message": { "role": "assistant", "content": "appended after sync" }
                }),
            ],
        );
        let toctou_res = search(
            &toctou_idx,
            "ToctouNeedle::survives",
            TrigramOptions::default(),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            toctou_res.hits.len(),
            1,
            "stale segment entry must fall back to live read, not vanish"
        );
        assert_eq!(toctou_res.suffix.stale, 1);
        assert_eq!(toctou_res.suffix.read, 1);
    }
}
