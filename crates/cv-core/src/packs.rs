//! Sealed Zstandard-Seekable multi-record `packs/` for content-addressed payloads and archived
//! transcripts.
//!
//! Instead of creating one CAS inode per small payload (which stalls HDDs and NAS mounts with
//! directory/inode metadata storms), multiple records/payloads are packed sequentially into sealed
//! immutable `.pack.zst` archives (~2 MiB independent Zstandard frames + trailing `0x184D2A5E`
//! skippable seek table with truncated XXH64 frame checksums) and addressed by
//! [`PayloadAddr`] = `(pack_id, decoded_offset, length, blake3_digest)`.
//!
//! Each sealed pack maintains three strictly distinct indexes (`docs/design/CODEC.md`):
//! 1. [`FrameIndex`]: `decoded_offset -> compressed_frame` (stored in `.pack.zst`'s seek table).
//! 2. [`RecordIndex`]: `anchor -> RecordSpan` (`decoded_offset`, `length`, `blake3_digest`).
//! 3. [`CodecCheckpointIndex`]: `anchor -> CodecCheckpoint` (settled `continuation_state` +
//!    codec version + external dependencies).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::harness::store::zst::{
    FrameEntry, FrameIndex, PositionedRead, SeekableReader, SeekableWriter, DEFAULT_SEEKABLE_FRAME_SIZE,
};
use crate::io_admission::{byte_source_policy, ByteSourcePolicy, IoAdmission};
use crate::publish::{self, Claim, Set, Staged};

/// Format version of the companion `.pack.idx.json` manifest.
pub const PACK_INDEX_VERSION: u32 = 1;

/// Address of one immutable payload/record inside a sealed multi-record pack:
/// `(pack_id, decoded_offset, length, blake3_digest)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PayloadAddr {
    pub pack_id: String,
    pub decoded_offset: u64,
    pub length: u64,
    #[serde(with = "hex_blake3")]
    pub blake3_digest: [u8; 32],
}

impl PayloadAddr {
    pub fn digest_hex(&self) -> String {
        encode_hex_32(&self.blake3_digest)
    }
}

/// One record/payload entry in [`RecordIndex`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSpan {
    pub anchor: String,
    pub decoded_offset: u64,
    pub length: u64,
    #[serde(with = "hex_blake3")]
    pub blake3_digest: [u8; 32],
}

impl RecordSpan {
    pub fn into_addr(self, pack_id: impl Into<String>) -> PayloadAddr {
        PayloadAddr {
            pack_id: pack_id.into(),
            decoded_offset: self.decoded_offset,
            length: self.length,
            blake3_digest: self.blake3_digest,
        }
    }
}

/// `anchor -> (decoded_offset, length, blake3_digest)` index for a sealed pack.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordIndex {
    records: Vec<RecordSpan>,
    #[serde(skip)]
    by_anchor: BTreeMap<String, usize>,
}

impl RecordIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, span: RecordSpan) -> Result<()> {
        anyhow::ensure!(
            !self.by_anchor.contains_key(&span.anchor),
            "duplicate record anchor {:?} in pack",
            span.anchor
        );
        if let Some(prev) = self.records.last() {
            let prev_end = prev
                .decoded_offset
                .checked_add(prev.length)
                .context("previous record end overflow")?;
            anyhow::ensure!(
                span.decoded_offset >= prev_end,
                "record {:?} offset {} precedes previous record end {}",
                span.anchor,
                span.decoded_offset,
                prev_end
            );
        }
        let idx = self.records.len();
        self.by_anchor.insert(span.anchor.clone(), idx);
        self.records.push(span);
        Ok(())
    }

    pub fn get(&self, anchor: &str) -> Option<&RecordSpan> {
        let idx = self.by_anchor.get(anchor).copied().or_else(|| {
            // Fallback linear scan if deserialized directly without `rebuild_lookup`.
            self.records.iter().position(|r| r.anchor == anchor)
        })?;
        self.records.get(idx)
    }

    /// Binary-search the record containing `decoded_offset`.
    pub fn record_at_offset(&self, decoded_offset: u64) -> Option<&RecordSpan> {
        let idx = self
            .records
            .partition_point(|r| r.decoded_offset.saturating_add(r.length) <= decoded_offset);
        let rec = self.records.get(idx)?;
        (decoded_offset >= rec.decoded_offset && decoded_offset < rec.decoded_offset.saturating_add(rec.length))
            .then_some(rec)
    }

    pub fn records(&self) -> &[RecordSpan] {
        &self.records
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Settled parser/codec continuation checkpoint (`anchor -> continuation_state`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCheckpoint {
    pub anchor: String,
    pub decoded_offset: u64,
    pub codec_version: u32,
    /// Checkpoints may only be persisted at settled record boundaries (`docs/design/CODEC.md`).
    pub settled: bool,
    /// External sidecar/parent dependencies whose identities must still match to resume.
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub continuation_state: serde_json::Value,
}

/// `anchor -> CodecCheckpoint` index, separate from [`FrameIndex`] and [`RecordIndex`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCheckpointIndex {
    checkpoints: Vec<CodecCheckpoint>,
    #[serde(skip)]
    by_anchor: BTreeMap<String, usize>,
}

impl CodecCheckpointIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn rebuild_lookup(&mut self) {
        self.by_anchor.clear();
        for (idx, cp) in self.checkpoints.iter().enumerate() {
            self.by_anchor.insert(cp.anchor.clone(), idx);
        }
    }

    /// Insert a settled codec checkpoint; refuses unsettled continuation state (`CODEC.md`).
    pub fn insert(&mut self, checkpoint: CodecCheckpoint) -> Result<()> {
        anyhow::ensure!(
            checkpoint.settled,
            "refusing to persist unsettled codec continuation state at anchor {:?}",
            checkpoint.anchor
        );
        anyhow::ensure!(
            !self.by_anchor.contains_key(&checkpoint.anchor),
            "duplicate codec checkpoint anchor {:?}",
            checkpoint.anchor
        );
        if let Some(prev) = self.checkpoints.last() {
            anyhow::ensure!(
                checkpoint.decoded_offset >= prev.decoded_offset,
                "codec checkpoint offset {} regresses before {}",
                checkpoint.decoded_offset,
                prev.decoded_offset
            );
        }
        let idx = self.checkpoints.len();
        self.by_anchor.insert(checkpoint.anchor.clone(), idx);
        self.checkpoints.push(checkpoint);
        Ok(())
    }

    pub fn get(&self, anchor: &str) -> Option<&CodecCheckpoint> {
        let idx = self
            .by_anchor
            .get(anchor)
            .copied()
            .or_else(|| self.checkpoints.iter().position(|c| c.anchor == anchor))?;
        self.checkpoints.get(idx)
    }

    /// Latest settled checkpoint at or before `target_decoded_offset` matching `codec_version` and
    /// whose `dependencies` are satisfied by `valid_dep`.
    pub fn latest_at_or_before(
        &self,
        target_decoded_offset: u64,
        codec_version: u32,
        mut valid_dep: impl FnMut(&str) -> bool,
    ) -> Option<&CodecCheckpoint> {
        let upper = self
            .checkpoints
            .partition_point(|cp| cp.decoded_offset <= target_decoded_offset);
        self.checkpoints[..upper]
            .iter()
            .rev()
            .find(|cp| cp.settled && cp.codec_version == codec_version && cp.dependencies.iter().all(|d| valid_dep(d)))
    }

    pub fn checkpoints(&self) -> &[CodecCheckpoint] {
        &self.checkpoints
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }
}

/// Companion `.pack.idx.json` manifest persisted atomically alongside `<pack_id>.pack.zst`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    pub version: u32,
    pub pack_id: String,
    pub total_decoded_len: u64,
    pub frame_count: usize,
    pub record_index: RecordIndex,
    pub codec_checkpoints: CodecCheckpointIndex,
}

impl PackManifest {
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self> {
        let mut manifest: Self = serde_json::from_slice(bytes).context("parsing pack manifest JSON")?;
        anyhow::ensure!(
            manifest.version == PACK_INDEX_VERSION,
            "unsupported pack index version {} (expected {})",
            manifest.version,
            PACK_INDEX_VERSION
        );
        // Deserialization bypasses RecordIndex::insert; reapply its ordering, uniqueness,
        // and bounds invariants before any consumer trusts offsets or verifies records.
        let mut checked = RecordIndex::new();
        for span in std::mem::take(&mut manifest.record_index.records) {
            let end = span
                .decoded_offset
                .checked_add(span.length)
                .context("record end overflow")?;
            anyhow::ensure!(end <= manifest.total_decoded_len, "record extends beyond pack");
            checked.insert(span)?;
        }
        manifest.record_index = checked;
        manifest.codec_checkpoints.rebuild_lookup();
        Ok(manifest)
    }
}

pub fn pack_zst_path(dir: &Path, pack_id: &str) -> Result<PathBuf> {
    publish::validate_output_id(pack_id)?;
    Ok(dir.join(format!("{pack_id}.pack.zst")))
}

pub fn pack_idx_path(dir: &Path, pack_id: &str) -> Result<PathBuf> {
    publish::validate_output_id(pack_id)?;
    Ok(dir.join(format!("{pack_id}.pack.idx.json")))
}

/// Default retention grace period (15 minutes) before unreferenced `.cv-*.tmp` staged pack files
/// left behind by hard crashes (`SIGKILL` / power loss) are swept from a pack directory.
pub const DEFAULT_ORPHAN_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Sweep stale `.cv-*.tmp` staged files in `dir` older than `max_age`.
///
/// Never follows symlinks and never touches live staged writers younger than `max_age` or any
/// published `.pack.zst` / `.pack.idx.json` artifact. Safe no-op if `dir` does not exist or under
/// `--frozen`.
pub fn sweep_orphan_pack_temps(dir: &Path, max_age: std::time::Duration) -> io::Result<usize> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (dir, max_age);
        Ok(0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if crate::env::frozen() || !dir.is_dir() {
            return Ok(0);
        }
        let now = std::time::SystemTime::now();
        let mut removed = 0usize;
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else { continue };
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !(name.starts_with(".cv-") && name.ends_with(".tmp")) {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if now.duration_since(mtime).is_ok_and(|age| age >= max_age) && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Builder streaming multiple records/payloads into one Zstandard-Seekable pack staged in `dir`,
/// published atomically via [`publish::Set`] (`Staged` temp write + `fsync` + rename + dir sync).
pub struct PackBuilder {
    dir: PathBuf,
    pack_id: String,
    zst_dest: PathBuf,
    idx_dest: PathBuf,
    writer: SeekableWriter<io::BufWriter<Staged>>,
    record_index: RecordIndex,
    codec_checkpoints: CodecCheckpointIndex,
}

impl PackBuilder {
    pub fn create(dir: &Path, pack_id: impl Into<String>) -> Result<Self> {
        Self::with_frame_size(dir, pack_id, DEFAULT_SEEKABLE_FRAME_SIZE, 3)
    }

    pub fn with_frame_size(
        dir: &Path,
        pack_id: impl Into<String>,
        frame_size: usize,
        compression_level: i32,
    ) -> Result<Self> {
        let pack_id = pack_id.into();
        let zst_dest = pack_zst_path(dir, &pack_id)?;
        let idx_dest = pack_idx_path(dir, &pack_id)?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating pack directory {}", dir.display()))?;
        let _ = sweep_orphan_pack_temps(dir, DEFAULT_ORPHAN_TMP_MAX_AGE);
        let staged_zst = Staged::beside(&zst_dest, "pack-zst", 0o600)?;
        let buffered = io::BufWriter::with_capacity(64 * 1024, staged_zst);
        let writer = SeekableWriter::with_options(buffered, frame_size, compression_level);
        Ok(Self {
            dir: dir.to_path_buf(),
            pack_id,
            zst_dest,
            idx_dest,
            writer,
            record_index: RecordIndex::new(),
            codec_checkpoints: CodecCheckpointIndex::new(),
        })
    }

    pub fn pack_id(&self) -> &str {
        &self.pack_id
    }

    pub fn decoded_offset(&self) -> u64 {
        self.writer.decoded_offset()
    }

    /// Append one record/payload under `anchor`, returning its immutable [`PayloadAddr`].
    pub fn append_record(&mut self, anchor: impl Into<String>, payload: &[u8]) -> Result<PayloadAddr> {
        let anchor = anchor.into();
        let decoded_offset = self.writer.decoded_offset();
        let length = payload.len() as u64;
        let blake3_digest = *blake3::hash(payload).as_bytes();
        self.writer
            .write_all(payload)
            .with_context(|| format!("writing record {anchor:?} to pack {}", self.pack_id))?;
        let span = RecordSpan {
            anchor,
            decoded_offset,
            length,
            blake3_digest,
        };
        self.record_index.insert(span.clone())?;
        Ok(span.into_addr(self.pack_id.clone()))
    }

    /// Record a settled codec continuation checkpoint at or before the current decoded offset.
    pub fn record_checkpoint(&mut self, checkpoint: CodecCheckpoint) -> Result<()> {
        anyhow::ensure!(
            checkpoint.decoded_offset <= self.writer.decoded_offset(),
            "checkpoint decoded_offset {} exceeds written pack bytes {}",
            checkpoint.decoded_offset,
            self.writer.decoded_offset()
        );
        self.codec_checkpoints.insert(checkpoint)
    }

    /// Seal the seekable zstd frames + trailing `0x184D2A5E` seek table, stage the companion
    /// `.pack.idx.json`, and publish both atomically via [`publish::Set`].
    pub fn seal(self, claim: Claim) -> Result<PackManifest> {
        let (mut buffered_zst, frame_index) = self
            .writer
            .finish()
            .with_context(|| format!("finishing seekable zstd pack {}", self.pack_id))?;
        buffered_zst.flush()?;
        let staged_zst = buffered_zst
            .into_inner()
            .map_err(|e| anyhow::Error::new(e.into_error()))?;

        let manifest = PackManifest {
            version: PACK_INDEX_VERSION,
            pack_id: self.pack_id.clone(),
            total_decoded_len: frame_index.total_decoded_len(),
            frame_count: frame_index.len(),
            record_index: self.record_index,
            codec_checkpoints: self.codec_checkpoints,
        };
        let manifest_bytes = serde_json::to_vec(&manifest).context("serializing pack manifest")?;
        let mut staged_idx = Staged::beside(&self.idx_dest, "pack-idx", 0o600)?;
        staged_idx.write_all(&manifest_bytes)?;
        staged_idx.flush()?;

        // Publish `.pack.zst` first, then `.pack.idx.json`, as one atomic `publish::Set`.
        let mut set = Set::new();
        set.add(staged_zst, &self.zst_dest, claim);
        set.add(staged_idx, &self.idx_dest, claim);
        set.publish()?;

        IoAdmission::global().register_sealed_immutable_dir(&self.dir);
        Ok(manifest)
    }
}

enum PackBacking {
    File {
        path: PathBuf,
        file: std::fs::File,
    },
    #[cfg(feature = "mmap")]
    Mapped {
        path: PathBuf,
        mmap: memmap2::Mmap,
    },
    Memory(Vec<u8>),
}

impl PositionedRead for PackBacking {
    fn byte_len(&self) -> io::Result<u64> {
        match self {
            Self::File { file, .. } => file.byte_len(),
            #[cfg(feature = "mmap")]
            Self::Mapped { mmap, .. } => mmap.byte_len(),
            Self::Memory(bytes) => bytes.byte_len(),
        }
    }

    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        match self {
            Self::File { path, file } => IoAdmission::global().admit_path(path, || file.read_exact_at(offset, out)),
            #[cfg(feature = "mmap")]
            Self::Mapped { path, mmap } => IoAdmission::global().admit_path(path, || mmap.read_exact_at(offset, out)),
            Self::Memory(bytes) => bytes.read_exact_at(offset, out),
        }
    }
}

/// Reader over a sealed immutable pack (`FrameIndex` + `RecordIndex` + `CodecCheckpointIndex`),
/// verifying BLAKE3 payload digests and XXH64 frame checksums on random-access reads.
pub struct SealedPack {
    manifest: PackManifest,
    reader: SeekableReader<PackBacking>,
    policy_used: ByteSourcePolicy,
}

impl SealedPack {
    /// Open `<dir>/<pack_id>.pack.{zst,idx.json}` using [`byte_source_policy`] (`Mapped` on local
    /// storage when `mmap` is enabled, `Buffered` positioned reads on network mounts or without
    /// `mmap`).
    pub fn open(dir: &Path, pack_id: &str) -> Result<Self> {
        let zst_path = pack_zst_path(dir, pack_id)?;
        let idx_path = pack_idx_path(dir, pack_id)?;
        let idx_bytes =
            std::fs::read(&idx_path).with_context(|| format!("reading pack index {}", idx_path.display()))?;
        let manifest = PackManifest::from_json_slice(&idx_bytes)?;
        anyhow::ensure!(
            manifest.pack_id == pack_id,
            "pack index id {:?} mismatches requested pack {:?}",
            manifest.pack_id,
            pack_id
        );

        let file =
            std::fs::File::open(&zst_path).with_context(|| format!("opening sealed pack {}", zst_path.display()))?;
        let policy = byte_source_policy(&zst_path);
        let (backing, actual_policy) = match policy {
            #[cfg(feature = "mmap")]
            ByteSourcePolicy::Mapped => {
                let len = file.metadata()?.len();
                if len == 0 {
                    (PackBacking::Memory(Vec::new()), ByteSourcePolicy::Mapped)
                } else {
                    // SAFETY: `zst_path` is a sealed immutable `.pack.zst` published via `Staged` rename.
                    match unsafe { memmap2::MmapOptions::new().map(&file) } {
                        Ok(mmap) => (
                            PackBacking::Mapped {
                                path: zst_path.clone(),
                                mmap,
                            },
                            ByteSourcePolicy::Mapped,
                        ),
                        Err(_) => (
                            PackBacking::File {
                                path: zst_path.clone(),
                                file,
                            },
                            ByteSourcePolicy::Buffered,
                        ),
                    }
                }
            }
            _ => (
                PackBacking::File {
                    path: zst_path.clone(),
                    file,
                },
                ByteSourcePolicy::Buffered,
            ),
        };

        let reader = SeekableReader::open(backing)
            .with_context(|| format!("validating zstd seek table of {}", zst_path.display()))?;
        anyhow::ensure!(
            reader.index().total_decoded_len() == manifest.total_decoded_len,
            "pack {} seek table decoded length {} mismatches manifest {}",
            pack_id,
            reader.index().total_decoded_len(),
            manifest.total_decoded_len
        );
        Ok(Self {
            manifest,
            reader,
            policy_used: actual_policy,
        })
    }

    /// Construct an in-memory `SealedPack` from slices (`wasm32` / browser archives).
    pub fn from_slices(zst_bytes: Vec<u8>, idx_bytes: &[u8]) -> Result<Self> {
        let manifest = PackManifest::from_json_slice(idx_bytes)?;
        let reader =
            SeekableReader::open(PackBacking::Memory(zst_bytes)).context("validating in-memory zstd seekable pack")?;
        anyhow::ensure!(
            reader.index().total_decoded_len() == manifest.total_decoded_len,
            "in-memory pack decoded length mismatch"
        );
        Ok(Self {
            manifest,
            reader,
            policy_used: ByteSourcePolicy::Slice,
        })
    }

    pub fn pack_id(&self) -> &str {
        &self.manifest.pack_id
    }

    pub fn byte_source_policy(&self) -> ByteSourcePolicy {
        self.policy_used
    }

    pub fn frame_index(&self) -> &FrameIndex {
        self.reader.index()
    }

    pub fn record_index(&self) -> &RecordIndex {
        &self.manifest.record_index
    }

    pub fn codec_checkpoints(&self) -> &CodecCheckpointIndex {
        &self.manifest.codec_checkpoints
    }

    pub fn frames_decoded(&self) -> u64 {
        self.reader.frames_decoded()
    }

    /// Read an arbitrary decoded byte range without record-level BLAKE3 digest verification
    /// (frame-level XXH64 checksums are still strictly verified!).
    pub fn read_decoded_range(&self, decoded_offset: u64, length: u64) -> Result<Vec<u8>> {
        self.reader.read_range(decoded_offset, length).with_context(|| {
            format!(
                "reading range {decoded_offset}..+{} from pack {}",
                length,
                self.pack_id()
            )
        })
    }

    /// Read a content-addressed [`PayloadAddr`] and verify its 32-byte BLAKE3 digest.
    pub fn read_payload(&self, addr: &PayloadAddr) -> Result<Vec<u8>> {
        anyhow::ensure!(
            addr.pack_id == self.manifest.pack_id,
            "payload pack_id {:?} mismatches open pack {:?}",
            addr.pack_id,
            self.manifest.pack_id
        );
        let bytes = self.read_decoded_range(addr.decoded_offset, addr.length)?;
        let actual = *blake3::hash(&bytes).as_bytes();
        anyhow::ensure!(
            actual == addr.blake3_digest,
            "BLAKE3 digest mismatch in pack {} at offset {}: got {}, expected {}",
            addr.pack_id,
            addr.decoded_offset,
            encode_hex_32(&actual),
            addr.digest_hex()
        );
        Ok(bytes)
    }

    /// Read a record by its logical `anchor` key, verifying its BLAKE3 digest.
    pub fn read_anchor(&self, anchor: &str) -> Result<Option<Vec<u8>>> {
        let Some(span) = self.manifest.record_index.get(anchor) else {
            return Ok(None);
        };
        let addr = span.clone().into_addr(&self.manifest.pack_id);
        self.read_payload(&addr).map(Some)
    }
}

fn encode_hex_32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

fn decode_hex_32(s: &str) -> std::result::Result<[u8; 32], &'static str> {
    if s.len() != 64 {
        return Err("BLAKE3 hex digest must be 64 hex characters");
    }
    let mut out = [0u8; 32];
    let raw = s.as_bytes();
    for i in 0..32 {
        let hi = (raw[2 * i] as char).to_digit(16).ok_or("invalid hex character")? as u8;
        let lo = (raw[2 * i + 1] as char).to_digit(16).ok_or("invalid hex character")? as u8;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

mod hex_blake3 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::encode_hex_32(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(deserializer)?;
        super::decode_hex_32(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_indexes_roundtrip_without_zstd_feature() -> Result<()> {
        let mut rec_idx = RecordIndex::new();
        let digest = *blake3::hash(b"hello").as_bytes();
        rec_idx.insert(RecordSpan {
            anchor: "a0".into(),
            decoded_offset: 0,
            length: 5,
            blake3_digest: digest,
        })?;
        assert_eq!(rec_idx.record_at_offset(4).unwrap().anchor, "a0");
        assert!(rec_idx.record_at_offset(5).is_none());

        let mut cp_idx = CodecCheckpointIndex::new();
        cp_idx.insert(CodecCheckpoint {
            anchor: "a0".into(),
            decoded_offset: 5,
            codec_version: 1,
            settled: true,
            dependencies: vec!["sidecar:1".into()],
            continuation_state: serde_json::json!({"turn": 1}),
        })?;

        let manifest = PackManifest {
            version: PACK_INDEX_VERSION,
            pack_id: "p0".into(),
            total_decoded_len: 5,
            frame_count: 1,
            record_index: rec_idx,
            codec_checkpoints: cp_idx,
        };
        let encoded = serde_json::to_vec(&manifest)?;
        let decoded = PackManifest::from_json_slice(&encoded)?;
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.record_index.get("a0").unwrap().blake3_digest, digest);
        assert_eq!(decoded.codec_checkpoints.get("a0").unwrap().codec_version, 1);
        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn sealed_pack_packs_many_records_into_one_archive_with_three_indexes_and_blake3_verification() -> Result<()> {
        let _world = crate::test_world("sealed-packs-e2e");
        let packs_dir = crate::env::Env::current().cv_home().unwrap().join("packs");

        // Build a pack with 4 KiB frame size so 200 records span multiple seekable frames,
        // while producing only ONE `.pack.zst` + ONE `.pack.idx.json` on disk (2 inodes, not 200!).
        let mut builder = PackBuilder::with_frame_size(&packs_dir, "pack-alpha", 4096, 1)?;
        let mut addrs = Vec::new();
        for i in 0..200usize {
            let payload = format!(
                "{{\"seq\":{i},\"text\":\"record-{i}-{}\"}}\n",
                "payload-body-".repeat((i % 7) + 3)
            );
            let addr = builder.append_record(format!("rec:{i:04}"), payload.as_bytes())?;
            addrs.push((i, payload, addr));

            if i % 50 == 49 {
                builder.record_checkpoint(CodecCheckpoint {
                    anchor: format!("rec:{i:04}"),
                    decoded_offset: builder.decoded_offset(),
                    codec_version: 2,
                    settled: true,
                    dependencies: vec![format!("dep:{}", i / 50)],
                    continuation_state: serde_json::json!({"seen": i + 1}),
                })?;
            }
        }

        // Unsettled checkpoint MUST be rejected (`CODEC.md` invariant).
        assert!(builder
            .record_checkpoint(CodecCheckpoint {
                anchor: "unsettled".into(),
                decoded_offset: builder.decoded_offset(),
                codec_version: 2,
                settled: false,
                dependencies: vec![],
                continuation_state: serde_json::json!({}),
            })
            .is_err());

        let manifest = builder.seal(Claim::New)?;
        assert_eq!(manifest.record_index.len(), 200);
        assert_eq!(manifest.codec_checkpoints.len(), 4);
        assert!(manifest.frame_count >= 5);

        // Verify inode discipline: only 2 files in `packs_dir`, zero leftover `.cv-*` temporaries!
        let files: Vec<_> = std::fs::read_dir(&packs_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2, "expected 2 files, got {files:?}");
        assert!(files.iter().all(|f| !f.starts_with(".cv-")));

        // Claim::New refuses overwriting an already-sealed immutable pack.
        let dup = PackBuilder::with_frame_size(&packs_dir, "pack-alpha", 4096, 1)?;
        assert!(dup.seal(Claim::New).is_err());

        // Open sealed pack: local sealed pack uses `Mapped` when `mmap` is enabled!
        let pack = SealedPack::open(&packs_dir, "pack-alpha")?;
        #[cfg(all(feature = "mmap", not(target_family = "wasm")))]
        assert_eq!(pack.byte_source_policy(), ByteSourcePolicy::Mapped);

        // Reading 1 small record out of the middle decodes ONLY its intersecting 4 KiB frame(s)!
        let (_, ref expected_137, ref addr_137) = addrs[137];
        let actual_137 = pack.read_payload(addr_137)?;
        assert_eq!(actual_137, expected_137.as_bytes());
        assert!(
            pack.frames_decoded() <= 2,
            "expected <= 2 frames decoded for single record, got {}",
            pack.frames_decoded()
        );

        // Anchor lookup + BLAKE3 verification works for every record.
        assert_eq!(pack.read_anchor("rec:0000")?.unwrap(), addrs[0].1.as_bytes());
        assert!(pack.read_anchor("rec:9999")?.is_none());

        // Forged BLAKE3 digest fails closed!
        let mut tampered_addr = addr_137.clone();
        tampered_addr.blake3_digest[0] ^= 0x01;
        assert!(pack.read_payload(&tampered_addr).is_err());

        // CodecCheckpointIndex respects codec_version and dependency satisfaction.
        let target_off = addrs[160].2.decoded_offset;
        let cp_all_valid = pack
            .codec_checkpoints()
            .latest_at_or_before(target_off, 2, |_| true)
            .unwrap();
        assert_eq!(cp_all_valid.anchor, "rec:0149");

        // If `dep:2` (anchor rec:0149) invalidated, falls back to earlier valid settled checkpoint `rec:0099`!
        let cp_dep_filtered = pack
            .codec_checkpoints()
            .latest_at_or_before(target_off, 2, |dep| dep != "dep:2")
            .unwrap();
        assert_eq!(cp_dep_filtered.anchor, "rec:0099");
        assert!(pack
            .codec_checkpoints()
            .latest_at_or_before(target_off, 99, |_| true)
            .is_none());

        // Also verify `SealedPack::from_slices` (`Slice` policy used on wasm32)!
        let zst_raw = std::fs::read(pack_zst_path(&packs_dir, "pack-alpha")?)?;
        let idx_raw = std::fs::read(pack_idx_path(&packs_dir, "pack-alpha")?)?;
        let mem_pack = SealedPack::from_slices(zst_raw, &idx_raw)?;
        assert_eq!(mem_pack.byte_source_policy(), ByteSourcePolicy::Slice);
        assert_eq!(mem_pack.read_payload(addr_137)?, expected_137.as_bytes());

        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn dropped_unpublished_pack_builder_leaves_zero_temp_files() -> Result<()> {
        let world = cv_testkit::World::new("pack-drop-cleanup");
        let dir = world.base.join("packs");
        {
            let mut builder = PackBuilder::create(&dir, "aborted-pack")?;
            builder.append_record("r1", b"unsealed data")?;
            // Dropped here without `seal()`!
        }
        let entries: Vec<_> = std::fs::read_dir(&dir)?.collect();
        assert!(entries.is_empty(), "aborted PackBuilder left files: {entries:?}");
        Ok(())
    }
    #[test]
    fn manifest_rejects_overlapping_and_duplicate_record_spans() {
        let mut manifest = PackManifest {
            version: PACK_INDEX_VERSION,
            pack_id: "test".into(),
            total_decoded_len: 4,
            frame_count: 1,
            record_index: RecordIndex::new(),
            codec_checkpoints: CodecCheckpointIndex::default(),
        };
        manifest.record_index.records = vec![
            RecordSpan {
                anchor: "a".into(),
                decoded_offset: 0,
                length: 3,
                blake3_digest: [0; 32],
            },
            RecordSpan {
                anchor: "b".into(),
                decoded_offset: 2,
                length: 2,
                blake3_digest: [0; 32],
            },
        ];
        assert!(PackManifest::from_json_slice(&serde_json::to_vec(&manifest).unwrap()).is_err());
        manifest.record_index.records[0].length = 2;
        manifest.record_index.records[1].anchor = "a".into();
        assert!(PackManifest::from_json_slice(&serde_json::to_vec(&manifest).unwrap()).is_err());
    }

    #[test]
    fn sweep_orphan_pack_temps_removes_expired_staged_temps_only() -> Result<()> {
        let world = cv_testkit::World::new("pack-orphan-sweep");
        let dir = world.base.join("packs");
        std::fs::create_dir_all(&dir)?;

        let stale_tmp = dir.join(".cv-pack-zst-stale.tmp");
        let fresh_tmp = dir.join(".cv-pack-zst-fresh.tmp");
        let keep_pack = dir.join("pack-keep.pack.zst");
        std::fs::write(&stale_tmp, b"partial crash bytes")?;
        std::fs::write(&fresh_tmp, b"active writer bytes")?;
        std::fs::write(&keep_pack, b"sealed pack bytes")?;

        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&stale_tmp)?
            .set_modified(past)?;

        let swept = sweep_orphan_pack_temps(&dir, DEFAULT_ORPHAN_TMP_MAX_AGE)?;
        assert_eq!(swept, 1);
        assert!(!stale_tmp.exists(), "expired .cv-*.tmp should be swept");
        assert!(fresh_tmp.exists(), "fresh .cv-*.tmp writer must stay untouched");
        assert!(keep_pack.exists(), "published .pack.zst must stay untouched");
        Ok(())
    }

    #[cfg(all(unix, feature = "zstd"))]
    #[test]
    fn sealed_pack_artifacts_are_owner_only_0600() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let world = cv_testkit::World::new("pack-mode-0600");
        let dir = world.base.join("packs");
        let mut builder = PackBuilder::create(&dir, "private-pack")?;
        builder.append_record("r1", b"secret session transcript")?;
        builder.seal(Claim::New)?;

        let zst_mode = std::fs::metadata(pack_zst_path(&dir, "private-pack")?)?
            .permissions()
            .mode()
            & 0o777;
        let idx_mode = std::fs::metadata(pack_idx_path(&dir, "private-pack")?)?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(zst_mode, 0o600);
        assert_eq!(idx_mode, 0o600);
        Ok(())
    }
}
