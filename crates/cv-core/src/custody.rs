//! Read immutable local and replicated session custody through its committed manifests.
//!
//! An archived session is held in one of two shapes ([`CustodyKind`]). A single append-only
//! native file keeps its own bytes as a chain of pack extents, each archive pass adding only the
//! bytes appended since the last one; reading it back runs the harness's own adapter over the
//! chain by position ([`ExtentChain`]), so verbatim `_record` carriers survive the live file and
//! nothing is staged on disk. Every other store, and every
//! entry written before native custody existed, keeps one portable Session IR JSON projection.
use crate::packs::{pack_idx_path, pack_zst_path, PackManifest as PackFileManifest, PayloadAddr, SealedPack};
use crate::{Adapter, Demand, Discovered, Harness, MessageSink, Session, SessionKey, SessionRef};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-session custody metadata persisted inside `pack_manifests.index_meta["sessions"]` and
/// backed by the companion `.pack.idx.json` `RecordIndex`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchivedSessionEntry {
    pub producer_id: String,
    #[serde(default)]
    pub profile: PathBuf,
    pub harness: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    pub archived_at: DateTime<Utc>,
    /// Hex BLAKE3 digest of the serialized Session IR JSON stored in `addr`.
    pub content_blake3: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mtime_ns: Option<i128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<crate::source::Baseline>,
    /// The newest record this entry added: the IR projection, or a native chain's last extent.
    pub addr: PayloadAddr,
    #[serde(default, skip_serializing_if = "CustodyKind::is_projection")]
    pub custody: CustodyKind,
}

/// How an archived session's bytes are held.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustodyKind {
    /// One Session IR JSON record at `addr` (SQLite, multi-file and legacy entries).
    #[default]
    PortableProjection,
    /// The native file's bytes `[0, total_bytes)`, concatenated from `extents` in order.
    NativeExtents {
        extents: Vec<PayloadAddr>,
        total_bytes: u64,
        /// [`SourceCheckpoint::boundary_blake3`] of the archived prefix: an append-only
        /// successor still has it.
        prefix_blake3: String,
        /// The live file's name, kept so the adapter reads the same kind of file back.
        file_name: String,
        /// The names of up to [`NativeName::LAYOUT`] directories above it, outermost first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        layout: Vec<String>,
    },
}

impl CustodyKind {
    pub fn is_projection(&self) -> bool {
        matches!(self, Self::PortableProjection)
    }
}

/// Bytes sampled at each end of a prefix for [`SourceCheckpoint`].
const BOUNDARY_WINDOW: u64 = 4096;

/// A bounded witness that a file still starts with the bytes archived through `offset`: BLAKE3
/// of the offset, the first and the last [`BOUNDARY_WINDOW`] bytes before it. An in-place
/// rewrite or truncation changes it; an edit strictly inside a long prefix's middle does not
/// (the price of not rereading the prefix on every append).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCheckpoint {
    pub offset: u64,
    pub boundary_blake3: String,
}

impl SourceCheckpoint {
    pub fn read(file: &std::fs::File, offset: u64) -> std::io::Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let mut hash = blake3::Hasher::new();
        hash.update(&offset.to_le_bytes());
        let head = offset.min(BOUNDARY_WINDOW);
        let tail_at = offset.saturating_sub(BOUNDARY_WINDOW).max(head);
        let mut buf = vec![0u8; BOUNDARY_WINDOW as usize];
        let mut f = file;
        for (at, len) in [(0, head), (tail_at, offset - tail_at)] {
            f.seek(SeekFrom::Start(at))?;
            f.read_exact(&mut buf[..len as usize])?;
            hash.update(&buf[..len as usize]);
        }
        Ok(Self {
            offset,
            boundary_blake3: hash.finalize().to_hex().to_string(),
        })
    }
}

/// A native extent chain as one immutable byte string `[0, len)`, read by position straight
/// from its packs: no copy of the file is staged in memory or on disk. Each extent's logical
/// start is precomputed, so a read binary-searches its extent and decodes only the pack frame
/// holding it; the last [`Self::FRAMES`] decoded frames are kept, so a sequential pass decodes
/// each frame once. An extent's span must match its pack's record index when the chain opens,
/// and its whole BLAKE3 digest is checked the first time any of its bytes is read: a corrupt or
/// substituted extent fails that read instead of feeding an adapter.
pub struct ExtentChain {
    /// `(logical start, extent, index into packs)`, ascending; empty extents are left out.
    spans: Vec<(u64, PayloadAddr, usize)>,
    packs: Vec<SealedPack>,
    len: u64,
    state: std::sync::Mutex<ChainState>,
}

#[derive(Default)]
struct ChainState {
    verified: Vec<bool>,
    /// Recently decoded frames, newest last: `(pack, frame start, bytes)`.
    frames: std::collections::VecDeque<(usize, u64, std::sync::Arc<Vec<u8>>)>,
    /// Frames decoded so far, for tests of the cache.
    decoded: u64,
}

impl std::fmt::Debug for ExtentChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtentChain")
            .field("extents", &self.spans.len())
            .field("len", &self.len)
            .finish()
    }
}

impl ExtentChain {
    const FRAMES: usize = 2;

    /// Open the packs `extents` live in and check each extent's span against the record index
    /// of `key`'s anchor there.
    pub fn open(directory: &Path, key: &SessionKey, extents: &[PayloadAddr]) -> Result<Self> {
        let anchor = archive_anchor(key);
        let mut packs: Vec<SealedPack> = Vec::new();
        let mut spans = Vec::with_capacity(extents.len());
        let mut start = 0u64;
        for addr in extents {
            let at = match packs.iter().position(|p| p.pack_id() == addr.pack_id) {
                Some(at) => at,
                None => {
                    packs.push(SealedPack::open(directory, &addr.pack_id)?);
                    packs.len() - 1
                }
            };
            let span = packs[at]
                .record_index()
                .get(&anchor)
                .with_context(|| format!("missing custody extent {anchor} in {}", addr.pack_id))?;
            anyhow::ensure!(
                span.blake3_digest == addr.blake3_digest
                    && span.decoded_offset == addr.decoded_offset
                    && span.length == addr.length,
                "pack {} extent span mismatch for {anchor}",
                addr.pack_id
            );
            if addr.length == 0 {
                anyhow::ensure!(
                    addr.blake3_digest == *blake3::hash(&[]).as_bytes(),
                    "BLAKE3 digest mismatch in empty custody extent of pack {}",
                    addr.pack_id
                );
                continue;
            }
            spans.push((start, addr.clone(), at));
            start = start
                .checked_add(addr.length)
                .context("custody chain length overflows")?;
        }
        Ok(Self {
            state: std::sync::Mutex::new(ChainState {
                verified: vec![false; spans.len()],
                ..Default::default()
            }),
            spans,
            packs,
            len: start,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Pack frames decoded so far (a sequential pass decodes each once).
    pub fn frames_decoded(&self) -> u64 {
        self.state.lock().map_or(0, |s| s.decoded)
    }

    /// The decoded frame of `pack` holding `offset`, from the cache or decoded into it.
    fn frame(&self, state: &mut ChainState, pack: usize, offset: u64) -> Result<(u64, std::sync::Arc<Vec<u8>>)> {
        if let Some((_, start, bytes)) = state
            .frames
            .iter()
            .find(|(p, start, bytes)| *p == pack && *start <= offset && offset < start + bytes.len() as u64)
        {
            return Ok((*start, bytes.clone()));
        }
        let (start, bytes) = self.packs[pack].decoded_frame_at(offset)?;
        let bytes = std::sync::Arc::new(bytes);
        state.decoded += 1;
        if state.frames.len() == Self::FRAMES {
            state.frames.pop_front();
        }
        state.frames.push_back((pack, start, bytes.clone()));
        Ok((start, bytes))
    }

    /// Hash span `i` whole against its recorded digest, once, keeping its first
    /// [`Self::FRAMES`] frames in the cache so a sequential pass from byte 0 does not decode
    /// them a second time.
    fn verify(&self, state: &mut ChainState, i: usize) -> Result<()> {
        if state.verified[i] {
            return Ok(());
        }
        let (_, addr, pack) = &self.spans[i];
        let mut hash = blake3::Hasher::new();
        let (mut at, end) = (addr.decoded_offset, addr.decoded_offset + addr.length);
        let mut head_cached = 0usize;
        while at < end {
            if head_cached < Self::FRAMES {
                let (start, bytes) = self.frame(state, *pack, at)?;
                head_cached += 1;
                let from = (at - start) as usize;
                let to = bytes.len().min((end - start) as usize);
                anyhow::ensure!(from < to, "pack {} frame does not advance", addr.pack_id);
                hash.update(&bytes[from..to]);
                at = start + to as u64;
            } else if let Some((_, start, bytes)) = state
                .frames
                .iter()
                .find(|(p, start, bytes)| *p == *pack && *start <= at && at < start + bytes.len() as u64)
            {
                let start = *start;
                let from = (at - start) as usize;
                let to = bytes.len().min((end - start) as usize);
                anyhow::ensure!(from < to, "pack {} frame does not advance", addr.pack_id);
                hash.update(&bytes[from..to]);
                at = start + to as u64;
            } else {
                let (start, bytes) = self.packs[*pack].decoded_frame_at(at)?;
                state.decoded += 1;
                let from = (at - start) as usize;
                let to = bytes.len().min((end - start) as usize);
                anyhow::ensure!(from < to, "pack {} frame does not advance", addr.pack_id);
                hash.update(&bytes[from..to]);
                at = start + to as u64;
            }
        }
        anyhow::ensure!(
            *hash.finalize().as_bytes() == addr.blake3_digest,
            "BLAKE3 digest mismatch in custody extent of pack {}",
            addr.pack_id
        );
        state.verified[i] = true;
        Ok(())
    }

    /// Fill `out` from logical offset `at` until it is full or the chain ends.
    fn read_into(&self, out: &mut [u8], at: u64) -> Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("custody chain poisoned"))?;
        let mut filled = 0;
        while filled < out.len() && at + (filled as u64) < self.len {
            let pos = at + filled as u64;
            let i = self.spans.partition_point(|(start, ..)| *start <= pos) - 1;
            self.verify(&mut state, i)?;
            let (start, addr, pack) = &self.spans[i];
            let within = pos - start;
            let offset = addr.decoded_offset + within;
            let (frame_start, bytes) = self.frame(&mut state, *pack, offset)?;
            let from = (offset - frame_start) as usize;
            let n = (out.len() - filled)
                .min((addr.length - within) as usize)
                .min(bytes.len() - from);
            out[filled..filled + n].copy_from_slice(&bytes[from..from + n]);
            filled += n;
        }
        Ok(filled)
    }
}

impl crate::harness::store::bytes::Positioned for ExtentChain {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, out: &mut [u8], at: u64) -> std::io::Result<usize> {
        self.read_into(out, at).map_err(std::io::Error::other)
    }
}

impl crate::packs::PositionedRead for ExtentChain {
    fn byte_len(&self) -> std::io::Result<u64> {
        Ok(self.len)
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        let n = self.read_into(out, offset).map_err(std::io::Error::other)?;
        if n < out.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read past the end of a custody chain",
            ));
        }
        Ok(())
    }
}

/// A cursor over an [`ExtentChain`]: `Read` + `Seek`, sharing the chain's frame cache.
pub struct ExtentChainReader {
    chain: std::sync::Arc<ExtentChain>,
    position: u64,
}

impl ExtentChainReader {
    pub fn new(chain: std::sync::Arc<ExtentChain>) -> Self {
        Self { chain, position: 0 }
    }
}

impl std::io::Read for ExtentChainReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self
            .chain
            .read_into(out, self.position)
            .map_err(std::io::Error::other)?;
        self.position += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for ExtentChainReader {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            std::io::SeekFrom::Start(at) => Some(at),
            std::io::SeekFrom::End(delta) => self.chain.len.checked_add_signed(delta),
            std::io::SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        self.position = target.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start of a custody chain",
            )
        })?;
        Ok(self.position)
    }
}

/// The record anchor of a session's entries in an archive pack.
pub fn archive_anchor(key: &SessionKey) -> String {
    if key.profile.as_os_str().is_empty() {
        format!("{}:{}:{}", key.producer, key.harness, key.native_id)
    } else {
        key.selector()
    }
}

fn verify_archive_span(index: &crate::packs::RecordIndex, key: &SessionKey, addr: &PayloadAddr) -> Result<()> {
    let anchor = archive_anchor(key);
    // Packs written before keyed anchors name records differently; accept the record at the
    // entry's exact address, which the span check below still pins to the same digest.
    let span = index
        .get(&anchor)
        .or_else(|| index.record_at_offset(addr.decoded_offset))
        .with_context(|| format!("missing archive record {anchor} in {}", addr.pack_id))?;
    anyhow::ensure!(
        span.blake3_digest == addr.blake3_digest
            && span.decoded_offset == addr.decoded_offset
            && span.length == addr.length,
        "pack {} record span mismatch for {anchor}",
        addr.pack_id
    );
    Ok(())
}

pub type ArchiveSessionMap = BTreeMap<String, ArchivedSessionEntry>;
/// Fill pack ids that older manifests left implicit and check the entry's digest invariant.
fn normalize(entry: &mut ArchivedSessionEntry, physical_pack_id: &str) -> Result<()> {
    if entry.addr.pack_id.is_empty() {
        entry.addr.pack_id = physical_pack_id.to_owned();
    }
    if let CustodyKind::NativeExtents { extents, .. } = &mut entry.custody {
        for addr in extents.iter_mut().filter(|a| a.pack_id.is_empty()) {
            addr.pack_id = physical_pack_id.to_owned();
        }
        anyhow::ensure!(
            extents.last() == Some(&entry.addr),
            "native custody of {} does not end at its newest extent",
            entry.id
        );
    }
    anyhow::ensure!(
        entry.content_blake3 == entry.addr.digest_hex(),
        "archive digest mismatch for {}",
        entry.id
    );
    Ok(())
}

impl ArchivedSessionEntry {
    /// Every pack record this entry reads: the projection, or each extent of its chain.
    pub fn records(&self) -> &[PayloadAddr] {
        match &self.custody {
            CustodyKind::PortableProjection => std::slice::from_ref(&self.addr),
            CustodyKind::NativeExtents { extents, .. } => extents,
        }
    }

    pub fn key(&self) -> Result<SessionKey> {
        Ok(SessionKey {
            producer: self.producer_id.clone(),
            profile: self.profile.clone(),
            harness: Harness::parse(&self.harness).context("unknown archived harness")?,
            native_id: self.id.clone(),
        })
    }
}

/// Read-only map of session selectors to their latest archived revision across
/// all published `pack_manifests` (ascending commit order so newer packs supersede older ones).
/// Also validates any present `.pack.idx.json` record spans. Safe under `--frozen`.
pub fn archive_entries(home: &Path) -> Result<(ArchiveSessionMap, usize)> {
    committed_entries(home, true)
}

fn committed_entries(home: &Path, verify_indexes: bool) -> Result<(ArchiveSessionMap, usize)> {
    let manifests = crate::list_pack_manifests_frozen_safe(home).context("reading pack manifests from state.db")?;
    let packs_dir = home.join("packs");
    let mut distinct_packs = std::collections::BTreeSet::new();
    let mut idx_cache: std::collections::HashMap<String, Option<PackFileManifest>> = std::collections::HashMap::new();
    let mut by_key = BTreeMap::new();

    for m in manifests {
        distinct_packs.insert(m.pack_path.clone());
        let phys_pack_id = m
            .index_meta
            .get("physical_pack_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| m.pack_id.split_once("--r").map(|(base, _)| base))
            .unwrap_or(&m.pack_id)
            .to_string();

        if let Some(sessions_val) = m.index_meta.get("sessions") {
            let entries: Vec<ArchivedSessionEntry> = serde_json::from_value(sessions_val.clone())
                .with_context(|| format!("decoding index_meta.sessions for pack {}", m.pack_id))?;
            for mut entry in entries {
                normalize(&mut entry, &phys_pack_id)?;
                if verify_indexes {
                    let key = entry.key()?;
                    for addr in entry.records() {
                        if !idx_cache.contains_key(&addr.pack_id) {
                            let idx_file = pack_idx_path(&packs_dir, &addr.pack_id)?;
                            let loaded = if idx_file.is_file() {
                                let raw = std::fs::read(&idx_file)
                                    .with_context(|| format!("reading {}", idx_file.display()))?;
                                Some(PackFileManifest::from_json_slice(&raw)?)
                            } else {
                                None
                            };
                            idx_cache.insert(addr.pack_id.clone(), loaded);
                        }
                        if let Some(Some(idx)) = idx_cache.get(&addr.pack_id) {
                            verify_archive_span(&idx.record_index, &key, addr)?;
                        }
                    }
                }
                let key = entry.key()?.selector();
                by_key.insert(key, entry);
            }
        }
    }
    Ok((by_key, distinct_packs.len()))
}

/// The archived bytes of one session: its IR projection, or its native chain concatenated.
pub fn read_session_bytes(home: &Path, harness: &str, id: &str) -> Result<Option<Vec<u8>>> {
    let entries = archive_entries(home)?.0;
    let mut matches = entries
        .iter()
        .filter(|(key, e)| e.harness == harness && (e.id == id || key.as_str() == id));
    let Some((_, entry)) = matches.next() else {
        return Ok(None);
    };
    anyhow::ensure!(matches.next().is_none(), "ambiguous archived session {id}");
    let packs = home.join("packs");
    Ok(Some(match &entry.custody {
        CustodyKind::PortableProjection => SealedPack::open(&packs, &entry.addr.pack_id)?.read_payload(&entry.addr)?,
        CustodyKind::NativeExtents { extents, .. } => {
            let chain = std::sync::Arc::new(ExtentChain::open(&packs, &entry.key()?, extents)?);
            let mut out = Vec::with_capacity(usize::try_from(chain.len()).unwrap_or(0));
            std::io::Read::read_to_end(&mut ExtentChainReader::new(chain), &mut out)?;
            out
        }
    }))
}

pub struct Backing {
    pub reference: SessionRef,
    key: SessionKey,
    directory: PathBuf,
    addr: PayloadAddr,
    custody: CustodyKind,
    source: &'static str,
    owner_store: Option<crate::state::StateStore>,
}

impl Backing {
    /// Decoded byte length of the custody payload (`addr.length` for projections, `total_bytes` for native chains).
    pub fn byte_len(&self) -> u64 {
        match &self.custody {
            CustodyKind::PortableProjection => self.addr.length,
            CustodyKind::NativeExtents { total_bytes, .. } => *total_bytes,
        }
    }

    /// Read the native file with the harness's own adapter, its bytes served by position from
    /// the extent chain: the same parser, carriers and lifecycle as the live file, with nothing
    /// written anywhere. The adapter is handed a path that does not exist (the file's own name
    /// under the layout it was archived from, inside the packs directory), so a sibling it looks
    /// for beside the file is absent, and its open of that path reads the chain instead.
    fn stream_native(
        &self,
        extents: &[PayloadAddr],
        total_bytes: u64,
        native: &NativeName,
        demand: &Demand,
        sink: &mut dyn MessageSink,
    ) -> Result<Session> {
        let adapter = crate::harness::for_harness(self.key.harness)
            .with_context(|| format!("no adapter for {}", self.key.harness))?;
        let chain = ExtentChain::open(&self.directory, &self.key, extents)?;
        anyhow::ensure!(chain.len() == total_bytes, "custody chain length mismatch");
        let newest = &extents.last().context("empty native custody chain")?.pack_id;
        let source = crate::json::JsonSource::capture(&pack_zst_path(&self.directory, newest)?)?;
        let path = native.view_path(&self.directory)?;
        let native = SessionRef {
            id: self.key.native_id.clone(),
            path: path.clone(),
            ..self.reference.clone()
        };
        let stand_in = crate::harness::store::jsonl::Substitute {
            path,
            source,
            bytes: std::sync::Arc::new(crate::harness::store::bytes::ByteSource::positioned(chain)),
        };
        let mut session =
            crate::harness::store::jsonl::with_substitute(stand_in, || adapter.stream(&native, demand, sink))?;
        anyhow::ensure!(
            session.id == self.key.native_id && session.harness == self.key.harness,
            "custody record identity mismatch"
        );
        session.source_path = Some(self.reference.path.clone());
        let mut coverage = crate::coverage::ReadCoverage::of_session(&session).unwrap_or_default();
        coverage.sources.insert(self.source.to_string());
        coverage.attach(&mut session);
        Ok(session)
    }
}

/// Where a native file lived, as far as its adapter reads meaning into it: the file's name and
/// up to [`NativeName::LAYOUT`] parent directory names (Qwen tells a session from a sub-agent
/// transcript by `projects/<project>/chats/`).
pub struct NativeName<'a> {
    pub file_name: &'a str,
    pub layout: &'a [String],
}

impl NativeName<'_> {
    pub const LAYOUT: usize = 4;

    /// The parent directory names of `path` an archive keeps beside its file name.
    pub fn layout_of(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take(Self::LAYOUT)
            .map_while(|dir| dir.file_name()?.to_str().map(str::to_owned))
            .collect();
        names.reverse();
        names
    }

    /// `<packs>/.custody-view/<layout…>/<file name>`: a path no file is at.
    fn view_path(&self, packs: &Path) -> Result<PathBuf> {
        let mut path = packs.join(".custody-view");
        for name in self.layout.iter().map(String::as_str).chain([self.file_name]) {
            crate::publish::validate_output_id(name).context("archived native file name")?;
            anyhow::ensure!(name != "..", "archived native file name");
            path.push(name);
        }
        anyhow::ensure!(!path.exists(), "{} exists; custody views must not", path.display());
        Ok(path)
    }
}

impl Adapter for Backing {
    fn harness(&self) -> Harness {
        self.key.harness
    }
    fn roots(&self) -> Vec<PathBuf> {
        vec![]
    }
    fn discover(&self) -> Result<Discovered> {
        Ok(Default::default())
    }
    fn stream(&self, _: &SessionRef, demand: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        if let Some(store) = &self.owner_store {
            store.check_hub_pack_owners(&self.key.producer, &self.addr, &self.custody)?;
        }
        if let CustodyKind::NativeExtents {
            extents,
            total_bytes,
            file_name,
            layout,
            ..
        } = &self.custody
        {
            let name = NativeName { file_name, layout };
            return self.stream_native(extents, *total_bytes, &name, demand, sink);
        }
        let pack = SealedPack::open(&self.directory, &self.addr.pack_id)?;
        if self.source == "custody_pack" {
            verify_archive_span(pack.record_index(), &self.key, &self.addr)?;
        }
        let bytes = pack.read_payload(&self.addr)?;
        let mut session = crate::state::hub::snapshot::decode_packed_session(&bytes)?;
        anyhow::ensure!(
            session.id == self.key.native_id && session.harness == self.key.harness,
            "pack record identity mismatch"
        );
        session.source_path = Some(self.reference.path.clone());
        let mut coverage = crate::coverage::ReadCoverage::of_session(&session).unwrap_or_default();
        coverage.sources.insert(self.source.to_string());
        coverage.attach(&mut session);
        crate::stream::replay(session, sink)
    }
}

/// Read a native custody chain back as a whole session (the hub shipper's view of an archive
/// commit). Projections are read by their callers directly.
pub fn collect_native(directory: &Path, key: SessionKey, custody: CustodyKind) -> Result<Session> {
    let CustodyKind::NativeExtents { extents, .. } = &custody else {
        anyhow::bail!("not a native custody chain");
    };
    let addr = extents.last().context("empty native custody chain")?.clone();
    let reference = SessionRef::new(key.selector(), key.harness, pack_zst_path(directory, &addr.pack_id)?);
    let backing = Backing {
        reference: reference.clone(),
        key,
        directory: directory.to_owned(),
        addr,
        custody,
        source: "custody_pack",
        owner_store: None,
    };
    crate::stream::collect(&backing, &reference)
}

pub(crate) fn or_archived(
    live: Option<crate::FoundSession>,
    mut rows: Vec<Backing>,
    id: &str,
) -> Result<Option<crate::FoundSession>> {
    if let Some((r, adapter)) = live {
        if !is_pack_ref(&r) {
            return Ok(Some((r, adapter)));
        }
        if let Some(backed) = backing_for(&r)? {
            return Ok(Some((r, backed)));
        }
    }
    if rows.iter().any(|b| b.reference.id == id || b.key.native_id == id) {
        rows.retain(|b| b.reference.id == id || b.key.native_id == id);
    }
    anyhow::ensure!(
        rows.len() <= 1,
        "ambiguous session {id}; select one of {}",
        rows.iter()
            .map(|b| b.reference.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(rows
        .pop()
        .map(|b| (b.reference.clone(), Box::new(b) as Box<dyn Adapter>)))
}

/// The newest local custody entry per session: `custody_heads`, one row per archived session,
/// or on a store whose heads are not yet backfilled every committed manifest.
pub fn head_entries(home: &Path) -> Result<ArchiveSessionMap> {
    let Some(rows) = crate::state::StateStore::at_dir(home).custody_heads()? else {
        return Ok(committed_entries(home, false)?.0);
    };
    let mut by_key = BTreeMap::new();
    for row in rows {
        let mut entry: ArchivedSessionEntry = serde_json::from_value(row).context("decoding custody head")?;
        let physical = entry.addr.pack_id.clone();
        normalize(&mut entry, &physical)?;
        by_key.insert(entry.key()?.selector(), entry);
    }
    Ok(by_key)
}

pub fn backing_from_hub_head(
    hub_packs_dir: &Path,
    entry: crate::state::hub::SessionRevisionEntry,
) -> Result<Option<Backing>> {
    let home = hub_packs_dir
        .parent()
        .and_then(Path::parent)
        .context("hub pack directory has no store root")?;
    hub_revision_backing(&crate::state::StateStore::at_dir(home), hub_packs_dir, entry)
}

pub(crate) fn hub_revision_backing(
    store: &crate::state::StateStore,
    hub_packs_dir: &Path,
    entry: crate::state::hub::SessionRevisionEntry,
) -> Result<Option<Backing>> {
    let custody = entry.custody.clone().unwrap_or(CustodyKind::PortableProjection);
    let addr = match (&entry.payload_addr, &custody) {
        (Some(addr), _) => addr.clone(),
        (None, CustodyKind::NativeExtents { extents, .. }) => {
            let Some(last) = extents.last() else {
                return Ok(None);
            };
            last.clone()
        }
        (None, CustodyKind::PortableProjection) => return Ok(None),
    };
    let Some(key) = entry.session_key_struct() else {
        return Ok(None);
    };
    let selector = key.selector();
    let directory = hub_packs_dir.to_owned();
    let reference = SessionRef {
        id: selector,
        harness: key.harness,
        path: pack_zst_path(&directory, &addr.pack_id)?,
        cwd: entry.cwd.map(Into::into),
        title: entry.title,
        created_at: entry.created_at.and_then(|t| t.parse().ok()),
        updated_at: entry.updated_at.and_then(|t| t.parse().ok()),
        message_count: usize::try_from(entry.message_count).ok(),
    };
    Ok(Some(Backing {
        reference,
        key,
        directory,
        addr,
        custody,
        source: "hub_snapshot",
        owner_store: Some(store.clone()),
    }))
}

fn backing_from_local_head(home: &Path, mut entry: ArchivedSessionEntry) -> Result<(String, Backing)> {
    let physical = entry.addr.pack_id.clone();
    normalize(&mut entry, &physical)?;
    let key = entry.key()?;
    let selector = key.selector();
    let directory = home.join("packs");
    let reference = SessionRef {
        id: selector.clone(),
        harness: key.harness,
        path: pack_zst_path(&directory, &entry.addr.pack_id)?,
        cwd: entry.cwd.map(Into::into),
        title: entry.title,
        created_at: entry.created_at,
        updated_at: entry.updated_at,
        message_count: Some(entry.message_count),
    };
    Ok((
        selector,
        Backing {
            reference,
            key,
            directory,
            addr: entry.addr,
            custody: entry.custody,
            source: "custody_pack",
            owner_store: None,
        },
    ))
}

impl crate::state::StateStore {
    // Heads written before nested references were checked at admission can outlive an upgrade.
    pub(crate) fn check_hub_pack_owners(
        &self,
        producer: &str,
        addr: &PayloadAddr,
        custody: &CustodyKind,
    ) -> Result<()> {
        let extents = match custody {
            CustodyKind::NativeExtents { extents, .. } => extents.as_slice(),
            CustodyKind::PortableProjection => &[],
        };
        let ids: std::collections::BTreeSet<_> = std::iter::once(addr).chain(extents).map(|a| &a.pack_id).collect();
        let ids = serde_json::to_string(&ids)?;
        let allowed = self
            .read_in(|conn| {
                Ok(conn.query_row(
                    "SELECT NOT EXISTS (
               SELECT 1 FROM json_each(?1) ref LEFT JOIN hub_packs p ON p.pack_id = ref.value
               WHERE p.pack_id IS NULL OR (p.producer_id IS NOT NULL AND p.producer_id <> ?2))",
                    rusqlite::params![ids, producer],
                    |r| r.get::<_, bool>(0),
                )?)
            })?
            .unwrap_or(false);
        anyhow::ensure!(allowed, "unavailable or foreign pack in stored hub revision");
        Ok(())
    }

    /// Indexed `O(log N)` point lookup for a single session's [`Backing`] by canonical `selector`,
    /// reading only that session's row from `custody_heads` or `hub_session_heads` (optionally
    /// pinned to `as_of_gen`). Safe under `--frozen`.
    pub fn custody_backing_for(&self, selector: &str, as_of_gen: Option<u64>) -> Result<Option<Backing>> {
        let Some(home) = self.path().parent() else {
            return Ok(None);
        };
        if as_of_gen.is_none() {
            match self.custody_head_point(selector)? {
                Some(Some(val)) => {
                    let entry: ArchivedSessionEntry = serde_json::from_value(val).context("decoding custody head")?;
                    let (_, backing) = backing_from_local_head(home, entry)?;
                    return Ok(Some(backing));
                }
                Some(None) => {}
                None => {
                    if let Some(entry) = committed_entries(home, false)?.0.remove(selector) {
                        let (_, backing) = backing_from_local_head(home, entry)?;
                        return Ok(Some(backing));
                    }
                }
            }
        }
        let hub = crate::state::hub::HubStore::at_home(home);
        if let Some(entry) = hub.session_head_for(selector, as_of_gen)? {
            return backing_from_hub_head(hub.packs_dir(), entry);
        }
        Ok(None)
    }
}

pub fn inventory() -> Result<Vec<Backing>> {
    inventory_as_of(None)
}

pub fn inventory_as_of(as_of_gen: Option<u64>) -> Result<Vec<Backing>> {
    let Some(home) = crate::env::Env::current().cv_home() else {
        return Ok(vec![]);
    };
    let db_path = home.join("state.db");
    let meta = match std::fs::metadata(&db_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.len() == 0 {
        return Ok(vec![]);
    }
    let mut rows = BTreeMap::new();
    let hub = crate::state::hub::HubStore::at_home(&home);
    let hub_heads = match hub.heads_at_generation(as_of_gen) {
        Ok(h) => h,
        Err(crate::state::hub::HubError::State(crate::state::StateError::NewerSchema { .. })) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    for entry in hub_heads {
        if let Some(backing) = backing_from_hub_head(hub.packs_dir(), entry)? {
            rows.insert(backing.reference.id.clone(), backing);
        }
    }
    if as_of_gen.is_none() {
        for (_, entry) in head_entries(&home)? {
            let (selector, backing) = backing_from_local_head(&home, entry)?;
            rows.insert(selector, backing);
        }
    }
    Ok(rows.into_values().collect())
}

pub fn backing_for(r: &SessionRef) -> Result<Option<Box<dyn Adapter>>> {
    backing_for_as_of(r, None)
}

pub fn backing_for_as_of(r: &SessionRef, as_of_gen: Option<u64>) -> Result<Option<Box<dyn Adapter>>> {
    if !is_pack_ref(r) {
        return Ok(None);
    }
    let Some(home) = crate::env::Env::current().cv_home() else {
        return Ok(None);
    };
    let store = crate::state::StateStore::at_dir(&home);
    if as_of_gen.is_none() && r.path.starts_with(home.join("hub").join("packs")) {
        let hub = crate::state::hub::HubStore::at_home(&home);
        if let Some(entry) = hub.session_head_for(&r.id, None)? {
            if let Some(b) = backing_from_hub_head(hub.packs_dir(), entry)? {
                if b.reference.path == r.path {
                    return Ok(Some(Box::new(b)));
                }
            }
        }
    }
    let backing = store
        .custody_backing_for(&r.id, as_of_gen)?
        .filter(|b| as_of_gen.is_some() || b.reference.path == r.path)
        .context("custody manifest no longer contains the selected revision")?;
    Ok(Some(Box::new(backing)))
}

pub fn is_pack_ref(r: &SessionRef) -> bool {
    SessionKey::split_selector(&r.id).is_some() && r.path.to_string_lossy().ends_with(".pack.zst")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    fn key() -> SessionKey {
        SessionKey {
            producer: "producer-a".into(),
            profile: PathBuf::new(),
            harness: Harness::Claude,
            native_id: "chainsess".into(),
        }
    }

    /// A Claude transcript big enough to span many 4 KiB pack frames, with a native carrier.
    fn transcript() -> Vec<u8> {
        let mut body = String::new();
        body.push_str(&format!(
            "{}\n",
            serde_json::json!({"type": "file-history-snapshot", "messageId": "snap-1",
                               "snapshot": {"marker": "carrier-heron-19", "trackedFileBackups": {}}})
        ));
        for i in 0..120 {
            body.push_str(&format!(
                "{}\n",
                serde_json::json!({"type": "user", "uuid": format!("u{i}"), "parentUuid": null,
                                   "timestamp": "2026-01-03T10:00:00Z", "carrierNote": format!("carrier-ibis-{i}"),
                                   "message": {"role": "user",
                                               "content": format!("turn {i} {}", "wren ".repeat(60))}})
            ));
        }
        body.into_bytes()
    }

    /// `bytes` archived as a chain split at `cut`, its two extents in two packs.
    fn chain(dir: &Path, bytes: &[u8], cut: usize) -> Vec<PayloadAddr> {
        let anchor = archive_anchor(&key());
        let mut extents = Vec::new();
        for (i, part) in [&bytes[..cut], &bytes[cut..]].into_iter().enumerate() {
            let mut builder = crate::packs::PackBuilder::with_frame_size(dir, format!("pack-{i}"), 4096, 1).unwrap();
            let mut input = part;
            extents.push(builder.append_reader(&anchor, &mut input, part.len() as u64).unwrap());
            builder.seal(crate::publish::Claim::New).unwrap();
        }
        extents
    }

    /// #tooth A chain reads by position: any range across extent and frame boundaries equals
    /// the file's bytes, seeking lands exactly, and a sequential pass decodes each frame a
    /// bounded number of times instead of staging the file.
    #[test]
    fn extent_chain_reads_by_position_across_extents_and_seeks() {
        let dir = cv_testkit::temp_dir("custody-chain");
        let bytes = transcript();
        let extents = chain(&dir, &bytes, bytes.len() / 3 + 7);
        let chain = std::sync::Arc::new(ExtentChain::open(&dir, &key(), &extents).unwrap());
        assert_eq!(chain.len(), bytes.len() as u64);
        for (at, len) in [
            (0usize, 10usize),
            (4090, 20),
            (bytes.len() / 3, 64),
            (bytes.len() - 5, 5),
        ] {
            let mut out = vec![0; len];
            crate::packs::PositionedRead::read_exact_at(chain.as_ref(), at as u64, &mut out).unwrap();
            assert_eq!(out, bytes[at..at + len], "range {at}+{len}");
        }
        let mut reader = ExtentChainReader::new(chain.clone());
        reader.seek(SeekFrom::Start(bytes.len() as u64 / 2)).unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, bytes[bytes.len() / 2..]);
        assert_eq!(reader.seek(SeekFrom::End(-3)).unwrap(), bytes.len() as u64 - 3);

        let fresh = std::sync::Arc::new(ExtentChain::open(&dir, &key(), &extents).unwrap());
        let mut all = Vec::new();
        ExtentChainReader::new(fresh.clone()).read_to_end(&mut all).unwrap();
        assert_eq!(all, bytes);
        let frames = bytes.len().div_ceil(4096) as u64 + 2;
        assert!(
            fresh.frames_decoded() <= 2 * frames,
            "{} frames decoded for {frames} frames",
            fresh.frames_decoded()
        );

        // #tooth Whole-extent verification preserves the first FRAMES decoded frames so a
        // sequential read from byte 0 hits the cache instead of decoding frames 0..FRAMES twice.
        let two_frame = &bytes[..6000]; // 2 frames at 4096 B/frame
        let mut single_builder = crate::packs::PackBuilder::with_frame_size(&dir, "pack-two-frame", 4096, 1).unwrap();
        let mut input = two_frame;
        let single_addr = single_builder
            .append_reader(archive_anchor(&key()), &mut input, two_frame.len() as u64)
            .unwrap();
        single_builder.seal(crate::publish::Claim::New).unwrap();
        let two_frame_chain = std::sync::Arc::new(ExtentChain::open(&dir, &key(), &[single_addr]).unwrap());
        let mut two_frame_out = Vec::new();
        ExtentChainReader::new(two_frame_chain.clone())
            .read_to_end(&mut two_frame_out)
            .unwrap();
        assert_eq!(two_frame_out, two_frame);
        assert_eq!(
            two_frame_chain.frames_decoded(),
            2,
            "2-frame extent must decode each frame once (got {})",
            two_frame_chain.frames_decoded()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// #tooth An extent whose bytes do not hash to its recorded digest fails the first read
    /// that touches it.
    #[test]
    fn extent_chain_rejects_an_extent_with_another_digest() {
        let dir = cv_testkit::temp_dir("custody-chain-digest");
        let bytes = transcript();
        let mut extents = chain(&dir, &bytes, 100);
        extents[1].blake3_digest[0] ^= 1;
        assert!(
            ExtentChain::open(&dir, &key(), &extents).is_err(),
            "the span check refuses it"
        );
        // The same bytes under an index forged to agree with the wrong digest: the span check
        // passes, the content check does not.
        let idx = crate::packs::pack_idx_path(&dir, "pack-1").unwrap();
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&idx).unwrap()).unwrap();
        for span in manifest["record_index"]["records"].as_array_mut().unwrap() {
            span["blake3_digest"] = extents[1].digest_hex().into();
        }
        std::fs::write(&idx, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let chain = ExtentChain::open(&dir, &key(), &extents).unwrap();
        let mut out = vec![0; 8];
        assert!(crate::packs::PositionedRead::read_exact_at(&chain, 0, &mut out).is_ok());
        assert!(crate::packs::PositionedRead::read_exact_at(&chain, 200, &mut out).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    /// #tooth The harness adapter reads an archived native file straight from its chain: the
    /// path it opens is a view no file is at, no byte spill is created, no staging directory
    /// appears while it streams, and the verbatim carrier record survives.
    #[cfg(feature = "sqlite")]
    #[test]
    fn native_custody_streams_through_the_adapter_without_staging() {
        let dir = cv_testkit::temp_dir("custody-stream");
        let bytes = transcript();
        let extents = chain(&dir, &bytes, bytes.len() / 2);
        let key = key();
        let reference = SessionRef::new(key.selector(), Harness::Claude, pack_zst_path(&dir, "pack-1").unwrap());
        let backing = Backing {
            reference: reference.clone(),
            key,
            directory: dir.clone(),
            addr: extents[1].clone(),
            custody: CustodyKind::NativeExtents {
                extents,
                total_bytes: bytes.len() as u64,
                prefix_blake3: String::new(),
                file_name: "chainsess.jsonl".into(),
                layout: vec!["projects".into(), "-work-proj".into()],
            },
            source: "custody_pack",
            owner_store: None,
        };
        let staging = || {
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("cv-custody-"))
                .count()
        };
        let before = staging();
        let spills = crate::harness::store::bytes::spills_created();
        crate::harness::store::jsonl::OPENED.with(|o| o.borrow_mut().clear());
        let mut texts = Vec::new();
        let mut during = 0;
        let mut sink = |m: crate::Message| {
            during = during.max(staging());
            texts.push(serde_json::to_string(&m).unwrap());
            crate::Flow::Continue
        };
        let session = Adapter::stream(&backing, &reference, &Demand::RICH.with_records(), &mut sink).unwrap();
        assert_eq!(session.id, "chainsess");
        assert_eq!(texts.len(), 120, "every turn reads");
        assert!(texts.iter().any(|t| t.contains("turn 119 wren")));
        assert!(
            texts.iter().any(|t| t.contains("carrier-ibis-77")),
            "each message keeps its verbatim record"
        );
        assert_eq!(during, before, "no staging directory while streaming");
        assert_eq!(crate::harness::store::bytes::spills_created(), spills, "no byte spill");
        let opened = crate::harness::store::jsonl::OPENED.with(|o| o.borrow().clone());
        assert!(!opened.is_empty());
        for path in opened {
            assert!(path.starts_with(dir.join(".custody-view")), "{}", path.display());
            assert!(!path.exists(), "{} is a view, not a file", path.display());
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
