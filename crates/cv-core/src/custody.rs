//! Read immutable local and replicated session custody through its committed manifests.
//!
//! An archived session is held in one of two shapes ([`CustodyKind`]). A single append-only
//! native file keeps its own bytes as a chain of pack extents, each archive pass adding only the
//! bytes appended since the last one; reading it back runs the harness's own adapter over the
//! chain, so verbatim `_record` carriers survive the live file. Every other store, and every
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

/// Sequential reader over a native extent chain. Pack frames decode one bounded chunk at a time
/// and every extent's BLAKE3 digest and record span are verified before its last byte is
/// returned, so a corrupt or substituted extent fails the read instead of feeding an adapter.
pub struct ExtentChainReader {
    directory: PathBuf,
    anchor: String,
    extents: std::vec::IntoIter<PayloadAddr>,
    current: Option<(PayloadAddr, u64, blake3::Hasher)>,
    pack: Option<SealedPack>,
    chunk: Vec<u8>,
    at: usize,
}

impl ExtentChainReader {
    const CHUNK: u64 = 1 << 20;

    pub fn new(directory: &Path, key: &SessionKey, extents: Vec<PayloadAddr>) -> Self {
        Self {
            directory: directory.to_owned(),
            anchor: archive_anchor(key),
            extents: extents.into_iter(),
            current: None,
            pack: None,
            chunk: Vec::new(),
            at: 0,
        }
    }

    fn refill(&mut self) -> Result<bool> {
        loop {
            if let Some((addr, read, hash)) = &mut self.current {
                if *read < addr.length {
                    let pack = self.pack.as_ref().context("extent pack not open")?;
                    let len = (addr.length - *read).min(Self::CHUNK);
                    self.chunk = pack.read_decoded_range(addr.decoded_offset + *read, len)?;
                    hash.update(&self.chunk);
                    *read += len;
                    self.at = 0;
                    if *read == addr.length {
                        anyhow::ensure!(
                            *hash.finalize().as_bytes() == addr.blake3_digest,
                            "BLAKE3 digest mismatch in custody extent of pack {}",
                            addr.pack_id
                        );
                    }
                    return Ok(true);
                }
            }
            let Some(next) = self.extents.next() else {
                return Ok(false);
            };
            if self.pack.as_ref().is_none_or(|p| p.pack_id() != next.pack_id) {
                self.pack = Some(SealedPack::open(&self.directory, &next.pack_id)?);
            }
            let pack = self.pack.as_ref().context("extent pack not open")?;
            let span = pack
                .record_index()
                .get(&self.anchor)
                .with_context(|| format!("missing custody extent {} in {}", self.anchor, next.pack_id))?;
            anyhow::ensure!(
                span.blake3_digest == next.blake3_digest
                    && span.decoded_offset == next.decoded_offset
                    && span.length == next.length,
                "pack {} extent span mismatch for {}",
                next.pack_id,
                self.anchor
            );
            self.current = Some((next, 0, blake3::Hasher::new()));
        }
    }
}

impl std::io::Read for ExtentChainReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.chunk.len() && !self.refill().map_err(std::io::Error::other)? {
            return Ok(0);
        }
        let n = out.len().min(self.chunk.len() - self.at);
        out[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
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
            let mut out = Vec::new();
            std::io::Read::read_to_end(
                &mut ExtentChainReader::new(&packs, &entry.key()?, extents.clone()),
                &mut out,
            )?;
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
}

/// A private directory holding one native file rebuilt from its extent chain, removed on drop.
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Backing {
    /// Rebuild the native file from its chain into a private temporary directory and read it
    /// with the harness's own adapter: the same parser, carriers and lifecycle as the live file.
    /// The chain streams through bounded chunks; nothing holds the whole file in memory.
    fn stream_native(
        &self,
        extents: &[PayloadAddr],
        file_name: &str,
        demand: &Demand,
        sink: &mut dyn MessageSink,
    ) -> Result<Session> {
        let adapter = crate::harness::for_harness(self.key.harness)
            .with_context(|| format!("no adapter for {}", self.key.harness))?;
        let staging = Staging(std::env::temp_dir().join(format!("cv-custody-{}", uuid::Uuid::new_v4().simple())));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&staging.0)?;
        crate::publish::validate_output_id(file_name).context("archived native file name")?;
        let path = staging.0.join(file_name);
        {
            let mut out = std::fs::File::create_new(&path)?;
            std::io::copy(
                &mut ExtentChainReader::new(&self.directory, &self.key, extents.to_vec()),
                &mut out,
            )?;
        }
        let native = SessionRef {
            id: self.key.native_id.clone(),
            path,
            ..self.reference.clone()
        };
        let mut session = adapter.stream(&native, demand, sink)?;
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
        if let CustodyKind::NativeExtents { extents, file_name, .. } = &self.custody {
            return self.stream_native(extents, file_name, demand, sink);
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
    };
    crate::stream::collect(&backing, &reference)
}

pub(crate) fn or_archived(
    live: Option<crate::FoundSession>,
    mut rows: Vec<Backing>,
    id: &str,
) -> Result<Option<crate::FoundSession>> {
    if live.is_some() {
        return Ok(live);
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

pub fn inventory() -> Result<Vec<Backing>> {
    let Some(home) = crate::env::Env::current().cv_home() else {
        return Ok(vec![]);
    };
    if !home.join("state.db").try_exists()? {
        return Ok(vec![]);
    }
    let mut rows = BTreeMap::new();
    let hub = crate::state::hub::HubStore::at_home(&home);
    if let Some(manifest) = hub.get_snapshot(None)? {
        for entry in manifest.session_revisions.into_values() {
            let Some(addr) = entry.payload_addr else { continue };
            let Some(harness) = Harness::parse(&entry.harness) else {
                continue;
            };
            let key = SessionKey {
                producer: entry.producer_id,
                profile: entry
                    .metadata
                    .get("profile")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .into(),
                harness,
                native_id: entry
                    .metadata
                    .get("native_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or(entry.session_id),
            };
            let selector = key.selector();
            let directory = hub.packs_dir().to_owned();
            let reference = SessionRef {
                id: selector.clone(),
                harness,
                path: pack_zst_path(&directory, &addr.pack_id)?,
                cwd: entry.cwd.map(Into::into),
                title: entry.title,
                created_at: entry.created_at.and_then(|t| t.parse().ok()),
                updated_at: entry.updated_at.and_then(|t| t.parse().ok()),
                message_count: usize::try_from(entry.message_count).ok(),
            };
            rows.insert(
                selector,
                Backing {
                    reference,
                    key,
                    directory,
                    addr,
                    custody: CustodyKind::PortableProjection,
                    source: "hub_snapshot",
                },
            );
        }
    }
    for (selector, e) in head_entries(&home)? {
        let key = e.key()?;
        let directory = home.join("packs");
        let reference = SessionRef {
            id: selector.clone(),
            harness: key.harness,
            path: pack_zst_path(&directory, &e.addr.pack_id)?,
            cwd: e.cwd.map(Into::into),
            title: e.title,
            created_at: e.created_at,
            updated_at: e.updated_at,
            message_count: Some(e.message_count),
        };
        rows.insert(
            selector,
            Backing {
                reference,
                key,
                directory,
                addr: e.addr,
                custody: e.custody,
                source: "custody_pack",
            },
        );
    }
    Ok(rows.into_values().collect())
}

pub fn backing_for(r: &SessionRef) -> Result<Option<Box<dyn Adapter>>> {
    if !is_pack_ref(r) {
        return Ok(None);
    }
    let backing = inventory()?
        .into_iter()
        .find(|b| b.reference.id == r.id && b.reference.path == r.path)
        .context("custody manifest no longer contains the selected revision")?;
    Ok(Some(Box::new(backing)))
}

pub fn is_pack_ref(r: &SessionRef) -> bool {
    SessionKey::split_selector(&r.id).is_some() && r.path.to_string_lossy().ends_with(".pack.zst")
}
