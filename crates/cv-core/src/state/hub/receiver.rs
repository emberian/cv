//! Fleet Hub receiver: producer enrollment, streaming Zstd-Seekable pack upload with BLAKE3 &
//! `FrameIndex` verification, `publish-before-reference` batch ingestion, out-of-order buffering
//! with gap-free contiguous `(producer, epoch)` frontiers, and loud `409 Conflict` quarantine.

use super::snapshot::{
    advance_frontier_in_tx, fold_origin_range_in_tx, latest_snapshot_in_conn, maybe_publish_snapshot_in_tx,
    recompute_frontier_in_tx, recompute_producer_sessions_in_tx, resolve_producer_sessions_in_tx,
    SnapshotPublicationManifest,
};
use super::{canonical_json_bytes, compute_event_digest, EventId, HubError, HubStore, Producer};
use crate::packs::{
    pack_idx_path, pack_zst_path, sweep_orphan_pack_temps, PackManifest, PayloadAddr, SeekableReader,
    DEFAULT_ORPHAN_TMP_MAX_AGE,
};
use crate::publish::{self, Claim, Set, Staged};
use chrono::Utc;
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};

/// Defensive uncompressed total size cap on uploaded packs (1 GiB) to block multi-frame RLE zstd
/// bombs before `SeekableReader::read_range` allocates.
pub const MAX_PACK_DECODED_BYTES: u64 = 1024 * 1024 * 1024;
/// Defensive record count cap per uploaded `.pack.idx.json` manifest.
pub const MAX_PACK_RECORDS: usize = 100_000;
/// Defensive per-record uncompressed byte cap (256 MiB) before `SeekableReader::read_range`.
pub const MAX_PACK_RECORD_BYTES: u64 = 256 * 1024 * 1024;

/// Records are ordered and disjoint after PackManifest validation. Keep one decoded frame
/// across records so both memory and decompression work are bounded by the pack, not N records.
fn verify_record_digests<S: crate::harness::store::zst::PositionedRead>(
    reader: &SeekableReader<S>,
    manifest: &PackManifest,
) -> Result<(), HubError> {
    let mut cached_frame = None;
    let mut decoded = Vec::new();
    for span in manifest.record_index.records() {
        let mut hasher = blake3::Hasher::new();
        let mut offset = span.decoded_offset;
        let end = offset + span.length; // checked by PackManifest::from_json_slice
        while offset < end {
            let idx = reader
                .index()
                .frame_for_offset(offset)
                .ok_or_else(|| HubError::Pack("record offset has no frame".into()))?;
            if cached_frame != Some(idx) {
                decoded = reader.decode_frame(idx)?;
                cached_frame = Some(idx);
            }
            let frame = &reader.index().frames()[idx];
            let start = (offset - frame.decoded_offset) as usize;
            let take = (end - offset).min((decoded.len() - start) as u64) as usize;
            hasher.update(&decoded[start..start + take]);
            offset += take as u64;
        }
        if *hasher.finalize().as_bytes() != span.blake3_digest {
            return Err(HubError::Pack(format!(
                "BLAKE3 mismatch on record {:?} inside pack {:?}",
                span.anchor, manifest.pack_id
            )));
        }
    }
    Ok(())
}

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Request payload for `POST /api/hub/enroll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub producer_id: String,
    #[serde(default)]
    pub host_label: String,
    #[serde(default = "empty_json_object")]
    pub metadata: Value,
}

/// Response payload for `POST /api/hub/enroll`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub producer: Producer,
}

/// Durable pack custody record stored in `hub_packs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirroredPackRecord {
    pub pack_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_id: Option<String>,
    pub byte_len: u64,
    pub total_decoded_len: u64,
    pub frame_count: usize,
    pub record_count: usize,
    pub blake3_digest: String,
    pub idx_blake3_digest: String,
    pub received_at: i64,
}

/// Response payload for `POST /api/hub/upload_pack` and `PUT /api/hub/packs/<pack_id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPackResponse {
    pub pack: MirroredPackRecord,
    pub replayed: bool,
    pub durable_fsynced: bool,
}

/// Single event record in an [`IngestBatchRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestEventRecord {
    pub event_id: EventId,
    pub op_id: String,
    #[serde(rename = "digest", alias = "event_digest")]
    pub digest: String,
    pub envelope: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_addr: Option<PayloadAddr>,
}

impl IngestEventRecord {
    /// Construct an [`IngestEventRecord`] computing its canonical BLAKE3 `digest`.
    pub fn new(
        event_id: EventId,
        op_id: impl Into<String>,
        envelope: Value,
        payload_addr: Option<PayloadAddr>,
    ) -> Result<Self, serde_json::Error> {
        let op_id = op_id.into();
        let digest = compute_event_digest(&op_id, &envelope, payload_addr.as_ref())?;
        Ok(Self {
            event_id,
            op_id,
            digest,
            envelope,
            payload_addr,
        })
    }

    /// Construct with an explicit caller-supplied `digest` (useful for deterministic fault tests).
    pub fn with_digest(
        event_id: EventId,
        op_id: impl Into<String>,
        digest: impl Into<String>,
        envelope: Value,
        payload_addr: Option<PayloadAddr>,
    ) -> Self {
        Self {
            event_id,
            op_id: op_id.into(),
            digest: digest.into(),
            envelope,
            payload_addr,
        }
    }
}

/// Request payload for `POST /api/hub/ingest_batch`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestBatchRequest {
    pub events: Vec<IngestEventRecord>,
}

/// Per-`(producer_id, epoch)` durable custody watermark returned by `ingest_batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubCustodyWatermark {
    pub producer_id: String,
    pub epoch: String,
    pub contiguous_seq: u64,
    pub max_received_seq: u64,
    pub buffered_count: u64,
    pub acked_at: i64,
}

/// Response payload for `POST /api/hub/ingest_batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestBatchResponse {
    pub accepted_count: usize,
    pub replayed_count: usize,
    pub durable_fsynced: bool,
    pub verified_pack_digests: BTreeMap<String, String>,
    pub custody_acks: Vec<HubCustodyWatermark>,
    #[serde(with = "super::origin_frontier_serde")]
    pub origin_frontier: BTreeMap<(String, String), u64>,
    pub snapshot: Option<SnapshotPublicationManifest>,
}

fn canonical_record_signature(
    op_id: &str,
    envelope: &Value,
    payload_addr: Option<&PayloadAddr>,
) -> Result<Vec<u8>, serde_json::Error> {
    let payload_val = match payload_addr {
        Some(addr) => serde_json::to_value(addr)?,
        None => Value::Null,
    };
    canonical_json_bytes(&json!({
        "op_id": op_id,
        "envelope": envelope,
        "payload_addr": payload_val,
    }))
}

impl HubStore {
    /// Stream a Zstd-Seekable `.pack.zst` + companion `.pack.idx.json` into staged temp files in
    /// `$CLUSTERVISION_HOME/hub/packs/`, verify whole-file BLAKE3 + Zstd-Seekable `FrameIndex` +
    /// per-record BLAKE3 spans, publish atomically via [`publish::Set`], and record durable hub
    /// pack custody in `hub_packs`.
    ///
    /// - Retrying an already-mirrored `(pack_id, blake3_digest)` returns `replayed: true` (`200 OK`).
    /// - Re-uploading an existing `pack_id` with a different digest fails loudly with
    ///   [`HubError::PackConflict`] (`409 Conflict`).
    pub fn upload_pack_stream(
        &self,
        pack_id: &str,
        producer_id: Option<&str>,
        expected_blake3_digest: Option<&str>,
        idx_bytes: &[u8],
        mut zst_reader: impl Read,
    ) -> Result<UploadPackResponse, HubError> {
        // Refuse before creating `hub/packs/` or staging the upload: the first `state.db` write
        // would refuse later, but only after the directory and a temp file already exist.
        if crate::env::frozen() {
            return Err(HubError::State(crate::state::StateError::Frozen));
        }
        publish::validate_output_id(pack_id).map_err(|e| HubError::InvalidArgument(e.to_string()))?;
        let zst_dest = pack_zst_path(&self.packs_dir, pack_id).map_err(|e| HubError::InvalidArgument(e.to_string()))?;
        let idx_dest = pack_idx_path(&self.packs_dir, pack_id).map_err(|e| HubError::InvalidArgument(e.to_string()))?;

        std::fs::create_dir_all(&self.packs_dir)?;
        let _ = sweep_orphan_pack_temps(&self.packs_dir, DEFAULT_ORPHAN_TMP_MAX_AGE);

        // Stream `.pack.zst` into a staged temp file while incrementally computing whole-file BLAKE3.
        let mut staged_zst =
            Staged::beside(&zst_dest, "hub-pack-zst", 0o644).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let mut hasher = blake3::Hasher::new();
        let mut byte_len = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = zst_reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            staged_zst.write_all(&buf[..n])?;
            byte_len += n as u64;
        }
        staged_zst.flush()?;
        let actual_zst_digest = hasher.finalize().to_hex().to_string();

        if let Some(expected) = expected_blake3_digest.map(str::trim).filter(|s| !s.is_empty()) {
            if !expected.eq_ignore_ascii_case(&actual_zst_digest) {
                return Err(HubError::Pack(format!(
                    "pack `{pack_id}` whole-file BLAKE3 mismatch: declared {expected}, computed {actual_zst_digest}"
                )));
            }
        }

        let idx_blake3_digest = blake3::hash(idx_bytes).to_hex().to_string();
        // Both files are immutable commitments: replay cannot silently change the record index.
        // Check existing durable `hub_packs` entry before touching published paths.
        {
            let conn = self.store.open_writable()?;
            let existing: Option<MirroredPackRecord> = conn
                .query_row(
                    "SELECT pack_id, producer_id, byte_len, total_decoded_len, frame_count,
                            record_count, blake3_digest, idx_blake3_digest, received_at
                     FROM hub_packs WHERE pack_id = ?1",
                    [pack_id],
                    |r| {
                        Ok(MirroredPackRecord {
                            pack_id: r.get(0)?,
                            producer_id: r.get(1)?,
                            byte_len: r.get::<_, i64>(2)? as u64,
                            total_decoded_len: r.get::<_, i64>(3)? as u64,
                            frame_count: r.get::<_, i64>(4)? as usize,
                            record_count: r.get::<_, i64>(5)? as usize,
                            blake3_digest: r.get(6)?,
                            idx_blake3_digest: r.get(7)?,
                            received_at: r.get(8)?,
                        })
                    },
                )
                .optional()?;

            if let Some(record) = existing {
                if record.blake3_digest != actual_zst_digest {
                    return Err(HubError::PackConflict {
                        pack_id: pack_id.to_string(),
                        existing_digest: record.blake3_digest,
                        incoming_digest: actual_zst_digest,
                    });
                }
                if record.idx_blake3_digest != idx_blake3_digest {
                    return Err(HubError::PackConflict {
                        pack_id: pack_id.to_string(),
                        existing_digest: record.idx_blake3_digest,
                        incoming_digest: idx_blake3_digest,
                    });
                }
                if zst_dest.is_file() && idx_dest.is_file() {
                    return Ok(UploadPackResponse {
                        pack: record,
                        replayed: true,
                        durable_fsynced: true,
                    });
                }
            }
        }

        // Parse & validate `.pack.idx.json`, Zstd-Seekable `FrameIndex`, and every `RecordSpan` BLAKE3!
        let manifest = PackManifest::from_json_slice(idx_bytes)
            .map_err(|e| HubError::Pack(format!("invalid `.pack.idx.json` for `{pack_id}`: {e:#}")))?;
        if manifest.pack_id != pack_id {
            return Err(HubError::Pack(format!(
                "pack manifest id {:?} does not match upload pack_id {:?}",
                manifest.pack_id, pack_id
            )));
        }
        if manifest.total_decoded_len > MAX_PACK_DECODED_BYTES {
            return Err(HubError::Pack(format!(
                "pack `{pack_id}` total_decoded_len {} exceeds max allowed {MAX_PACK_DECODED_BYTES}",
                manifest.total_decoded_len
            )));
        }
        if manifest.record_index.len() > MAX_PACK_RECORDS {
            return Err(HubError::Pack(format!(
                "pack `{pack_id}` record count {} exceeds max allowed {MAX_PACK_RECORDS}",
                manifest.record_index.len()
            )));
        }
        for span in manifest.record_index.records() {
            if span.length > MAX_PACK_RECORD_BYTES {
                return Err(HubError::Pack(format!(
                    "record {:?} in pack `{pack_id}` length {} exceeds max allowed {MAX_PACK_RECORD_BYTES}",
                    span.anchor, span.length
                )));
            }
            let Some(end) = span.decoded_offset.checked_add(span.length) else {
                return Err(HubError::Pack(format!(
                    "record {:?} in pack `{pack_id}` offset+length overflow",
                    span.anchor
                )));
            };
            if end > manifest.total_decoded_len {
                return Err(HubError::Pack(format!(
                    "record {:?} in pack `{pack_id}` end {} exceeds total_decoded_len {}",
                    span.anchor, end, manifest.total_decoded_len
                )));
            }
        }

        let verify_file = staged_zst.file().try_clone()?;
        let reader = SeekableReader::open(verify_file).map_err(|e| {
            HubError::Pack(format!(
                "invalid Zstd-Seekable frame table in uploaded pack `{pack_id}`: {e:#}"
            ))
        })?;
        if reader.index().total_decoded_len() > MAX_PACK_DECODED_BYTES {
            return Err(HubError::Pack(format!(
                "pack `{pack_id}` seekable decoded length {} exceeds max allowed {MAX_PACK_DECODED_BYTES}",
                reader.index().total_decoded_len()
            )));
        }
        if reader.index().total_decoded_len() != manifest.total_decoded_len {
            return Err(HubError::Pack(format!(
                "pack `{pack_id}` seekable decoded length {} != manifest {}",
                reader.index().total_decoded_len(),
                manifest.total_decoded_len
            )));
        }
        if reader.index().len() != manifest.frame_count {
            return Err(HubError::Pack(format!(
                "pack `{pack_id}` seekable frame_count {} != manifest {}",
                reader.index().len(),
                manifest.frame_count
            )));
        }
        verify_record_digests(&reader, &manifest)?;

        let manifest_json_str = String::from_utf8(idx_bytes.to_vec())
            .map_err(|e| HubError::Pack(format!("pack manifest is not UTF-8: {e}")))?;

        let mut staged_idx =
            Staged::beside(&idx_dest, "hub-pack-idx", 0o644).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        staged_idx.write_all(idx_bytes)?;
        staged_idx.flush()?;

        // If a prior crash published the exact same files before DB commit, check digest or publish fresh.
        let claim = if zst_dest.exists() || idx_dest.exists() {
            if zst_dest.is_file() {
                let existing_bytes = std::fs::read(&zst_dest)?;
                let existing_hex = blake3::hash(&existing_bytes).to_hex().to_string();
                if existing_hex != actual_zst_digest {
                    return Err(HubError::PackConflict {
                        pack_id: pack_id.to_string(),
                        existing_digest: existing_hex,
                        incoming_digest: actual_zst_digest,
                    });
                }
            }
            Claim::Replace
        } else {
            Claim::New
        };

        let mut set = Set::new();
        set.add(staged_zst, &zst_dest, claim);
        set.add(staged_idx, &idx_dest, claim);
        set.publish()
            .map_err(|e| HubError::Pack(format!("publishing pack `{pack_id}`: {e:#}")))?;

        let now_ms = Utc::now().timestamp_millis();
        let prod_opt = producer_id.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);

        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((existing_dig,)) = tx
            .query_row(
                "SELECT blake3_digest FROM hub_packs WHERE pack_id = ?1",
                [pack_id],
                |r| Ok((r.get::<_, String>(0)?,)),
            )
            .optional()?
        {
            if existing_dig != actual_zst_digest {
                return Err(HubError::PackConflict {
                    pack_id: pack_id.to_string(),
                    existing_digest: existing_dig,
                    incoming_digest: actual_zst_digest,
                });
            }
            let record = MirroredPackRecord {
                pack_id: pack_id.to_string(),
                producer_id: prod_opt,
                byte_len,
                total_decoded_len: manifest.total_decoded_len,
                frame_count: manifest.frame_count,
                record_count: manifest.record_index.len(),
                blake3_digest: actual_zst_digest,
                idx_blake3_digest,
                received_at: now_ms,
            };
            tx.commit()?;
            return Ok(UploadPackResponse {
                pack: record,
                replayed: true,
                durable_fsynced: true,
            });
        }

        tx.execute(
            "INSERT INTO hub_packs(
                 pack_id, producer_id, byte_len, total_decoded_len, frame_count,
                 record_count, blake3_digest, idx_blake3_digest, pack_rel_path,
                 idx_rel_path, manifest_json, received_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                pack_id,
                prod_opt,
                byte_len as i64,
                manifest.total_decoded_len as i64,
                manifest.frame_count as i64,
                manifest.record_index.len() as i64,
                actual_zst_digest,
                idx_blake3_digest,
                format!("{pack_id}.pack.zst"),
                format!("{pack_id}.pack.idx.json"),
                manifest_json_str,
                now_ms,
            ],
        )?;
        tx.commit()?;

        Ok(UploadPackResponse {
            pack: MirroredPackRecord {
                pack_id: pack_id.to_string(),
                producer_id: prod_opt,
                byte_len,
                total_decoded_len: manifest.total_decoded_len,
                frame_count: manifest.frame_count,
                record_count: manifest.record_index.len(),
                blake3_digest: actual_zst_digest,
                idx_blake3_digest,
                received_at: now_ms,
            },
            replayed: false,
            durable_fsynced: true,
        })
    }

    /// Convenience wrapper over [`Self::upload_pack_stream`] for in-memory slices.
    pub fn upload_pack_bytes(
        &self,
        pack_id: &str,
        producer_id: Option<&str>,
        expected_blake3_digest: Option<&str>,
        idx_bytes: &[u8],
        zst_bytes: &[u8],
    ) -> Result<UploadPackResponse, HubError> {
        self.upload_pack_stream(
            pack_id,
            producer_id,
            expected_blake3_digest,
            idx_bytes,
            io::Cursor::new(zst_bytes),
        )
    }

    /// Ingest a batch of replicated events under strict `publish-before-reference`, out-of-order
    /// gap-free frontier progression, idempotency, and loud `409 Conflict` quarantine.
    pub fn ingest_batch(&self, req: &IngestBatchRequest) -> Result<IngestBatchResponse, HubError> {
        for rec in &req.events {
            if rec.event_id.producer_id.trim().is_empty() {
                return Err(HubError::InvalidArgument(
                    "event_id.producer_id must not be empty".into(),
                ));
            }
            if rec.event_id.epoch.trim().is_empty() {
                return Err(HubError::InvalidArgument("event_id.epoch must not be empty".into()));
            }
            if rec.event_id.seq == 0 {
                return Err(HubError::InvalidArgument(
                    "event_id.seq must be >= 1 (1-based sequence)".into(),
                ));
            }
            if rec.op_id.trim().is_empty() {
                return Err(HubError::InvalidArgument("op_id must not be empty".into()));
            }
            if rec.digest.trim().is_empty() {
                return Err(HubError::InvalidArgument("event digest must not be empty".into()));
            }
            // The frontier counts events that were received *and verified*: a declared digest that
            // does not commit to the bytes actually delivered is refused before anything is stored,
            // so `hub_ingested_events.event_digest` is always the commitment of its own row.
            let computed = compute_event_digest(&rec.op_id, &rec.envelope, rec.payload_addr.as_ref())?;
            if !rec.digest.trim().eq_ignore_ascii_case(&computed) {
                return Err(HubError::InvalidArgument(format!(
                    "event {}/{}/#{} declares digest `{}` but its (op_id, envelope, payload_addr) hash to `{computed}`",
                    rec.event_id.producer_id, rec.event_id.epoch, rec.event_id.seq, rec.digest
                )));
            }
            if i64::try_from(rec.event_id.seq).is_err() {
                return Err(HubError::InvalidArgument(
                    "event_id.seq exceeds the storable sequence range".into(),
                ));
            }
        }

        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = Utc::now().timestamp_millis();

        // 1. Enforce `publish-before-reference` BEFORE mutating any table!
        let mut verified_pack_digests: BTreeMap<String, String> = BTreeMap::new();
        let mut loaded_manifests: BTreeMap<String, PackManifest> = BTreeMap::new();

        for rec in &req.events {
            let Some(ref addr) = rec.payload_addr else {
                continue;
            };
            if !loaded_manifests.contains_key(&addr.pack_id) {
                let row: Option<(String, String)> = tx
                    .query_row(
                        "SELECT blake3_digest, manifest_json FROM hub_packs WHERE pack_id = ?1",
                        [&addr.pack_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((pack_digest, manifest_str)) = row else {
                    return Err(HubError::MissingPackDependency {
                        pack_id: addr.pack_id.clone(),
                        detail: "pack not yet uploaded to hub (`publish-before-reference` violation)".into(),
                    });
                };
                let zst_path =
                    pack_zst_path(&self.packs_dir, &addr.pack_id).map_err(|e| HubError::MissingPackDependency {
                        pack_id: addr.pack_id.clone(),
                        detail: e.to_string(),
                    })?;
                let idx_path =
                    pack_idx_path(&self.packs_dir, &addr.pack_id).map_err(|e| HubError::MissingPackDependency {
                        pack_id: addr.pack_id.clone(),
                        detail: e.to_string(),
                    })?;
                if !zst_path.is_file() || !idx_path.is_file() {
                    return Err(HubError::MissingPackDependency {
                        pack_id: addr.pack_id.clone(),
                        detail: "mirrored pack files missing on disk".into(),
                    });
                }
                let manifest = PackManifest::from_json_slice(manifest_str.as_bytes()).map_err(|e| {
                    HubError::MissingPackDependency {
                        pack_id: addr.pack_id.clone(),
                        detail: format!("corrupt stored pack manifest: {e:#}"),
                    }
                })?;
                verified_pack_digests.insert(addr.pack_id.clone(), pack_digest);
                loaded_manifests.insert(addr.pack_id.clone(), manifest);
            }

            let manifest = loaded_manifests.get(&addr.pack_id).expect("loaded above");
            let span_matches = manifest.record_index.records().iter().any(|span| {
                span.decoded_offset == addr.decoded_offset
                    && span.length == addr.length
                    && span.blake3_digest == addr.blake3_digest
            });
            if !span_matches {
                return Err(HubError::MissingPackDependency {
                    pack_id: addr.pack_id.clone(),
                    detail: format!(
                        "PayloadAddr (offset={}, len={}, digest={}) not present in pack manifest",
                        addr.decoded_offset,
                        addr.length,
                        addr.digest_hex()
                    ),
                });
            }
        }

        if req.events.is_empty() {
            let origin_frontier = current_origin_frontier_in_tx(&tx)?;
            let snapshot = latest_snapshot_in_conn(&tx)?;
            tx.commit()?;
            return Ok(IngestBatchResponse {
                accepted_count: 0,
                replayed_count: 0,
                durable_fsynced: true,
                verified_pack_digests,
                custody_acks: Vec::new(),
                origin_frontier,
                snapshot,
            });
        }

        // 2. Deduplicate identical intra-batch retries & detect conflicts against both intra-batch
        //    entries and committed `hub_ingested_events`.
        struct CheckedRecord<'a> {
            rec: &'a IngestEventRecord,
            sig: Vec<u8>,
        }

        let mut unique_in_batch: Vec<CheckedRecord<'_>> = Vec::with_capacity(req.events.len());
        let mut seen_coords: BTreeMap<(String, String, u64), usize> = BTreeMap::new();
        let mut replayed_count = 0usize;

        for rec in &req.events {
            let sig = canonical_record_signature(&rec.op_id, &rec.envelope, rec.payload_addr.as_ref())?;
            let coord = (
                rec.event_id.producer_id.clone(),
                rec.event_id.epoch.clone(),
                rec.event_id.seq,
            );
            if let Some(&first_idx) = seen_coords.get(&coord) {
                let first = &unique_in_batch[first_idx];
                if first.rec.digest == rec.digest && first.sig == sig {
                    replayed_count += 1;
                    continue;
                }
                let conflicting_digest = if first.rec.digest != rec.digest {
                    rec.digest.clone()
                } else {
                    compute_event_digest(&rec.op_id, &rec.envelope, rec.payload_addr.as_ref())?
                };
                return self.quarantine_and_fail(
                    tx,
                    &rec.event_id,
                    &rec.op_id,
                    &first.rec.digest,
                    &conflicting_digest,
                    &rec.envelope,
                    rec.payload_addr.as_ref(),
                    now_ms,
                );
            }
            seen_coords.insert(coord, unique_in_batch.len());
            unique_in_batch.push(CheckedRecord { rec, sig });
        }

        let mut to_insert: Vec<&IngestEventRecord> = Vec::new();
        for item in &unique_in_batch {
            let rec = item.rec;
            let existing: Option<(String, String, String, Option<String>)> = tx
                .query_row(
                    "SELECT op_id, event_digest, envelope_json, payload_addr_json
                     FROM hub_ingested_events
                     WHERE producer_id = ?1 AND epoch = ?2 AND seq = ?3",
                    rusqlite::params![rec.event_id.producer_id, rec.event_id.epoch, rec.event_id.seq as i64],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;

            if let Some((ex_op_id, ex_digest, ex_env_str, ex_addr_str)) = existing {
                let ex_env: Value = serde_json::from_str(&ex_env_str)?;
                let ex_addr: Option<PayloadAddr> = match ex_addr_str {
                    Some(s) => Some(serde_json::from_str(&s)?),
                    None => None,
                };
                let ex_sig = canonical_record_signature(&ex_op_id, &ex_env, ex_addr.as_ref())?;
                if ex_digest == rec.digest && ex_sig == item.sig {
                    replayed_count += 1;
                    continue;
                }
                let conflicting_digest = if ex_digest != rec.digest {
                    rec.digest.clone()
                } else {
                    let computed = compute_event_digest(&rec.op_id, &rec.envelope, rec.payload_addr.as_ref())?;
                    if computed != ex_digest {
                        computed
                    } else {
                        format!("payload-mismatch-{}", blake3::hash(&item.sig).to_hex())
                    }
                };
                return self.quarantine_and_fail(
                    tx,
                    &rec.event_id,
                    &rec.op_id,
                    &ex_digest,
                    &conflicting_digest,
                    &rec.envelope,
                    rec.payload_addr.as_ref(),
                    now_ms,
                );
            }

            to_insert.push(rec);
        }

        // 3. Conflict-free batch: enroll producers if unseen, insert events, advance gap-free
        //    frontiers, materialize session state, and publish atomic `SnapshotPublicationManifest`!
        let mut touched_origins: BTreeSet<(String, String)> = BTreeSet::new();
        let mut touched_producers: BTreeSet<String> = BTreeSet::new();
        for item in &unique_in_batch {
            touched_origins.insert((item.rec.event_id.producer_id.clone(), item.rec.event_id.epoch.clone()));
            touched_producers.insert(item.rec.event_id.producer_id.clone());
        }

        for producer_id in &touched_producers {
            tx.execute(
                "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
                 VALUES (?1, ?1, '{}', ?2, ?2)
                 ON CONFLICT(producer_id) DO NOTHING",
                rusqlite::params![producer_id, now_ms],
            )?;
        }

        for rec in &to_insert {
            let env_json = serde_json::to_string(&rec.envelope)?;
            let addr_json = match &rec.payload_addr {
                Some(addr) => Some(serde_json::to_string(addr)?),
                None => None,
            };
            tx.execute(
                "INSERT INTO hub_ingested_events(
                     producer_id, epoch, seq, op_id, event_digest,
                     envelope_json, payload_addr_json, ingested_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    rec.event_id.producer_id,
                    rec.event_id.epoch,
                    rec.event_id.seq as i64,
                    rec.op_id,
                    rec.digest,
                    env_json,
                    addr_json,
                    now_ms,
                ],
            )?;
        }

        let mut touched_sessions_by_producer: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (producer_id, epoch) in &touched_origins {
            let (fold_after, new_frontier) = advance_frontier_in_tx(&tx, producer_id, epoch, now_ms)?;
            let delta_sessions = fold_origin_range_in_tx(&tx, producer_id, epoch, fold_after, new_frontier)?;
            if !delta_sessions.is_empty() {
                touched_sessions_by_producer
                    .entry(producer_id.clone())
                    .or_default()
                    .extend(delta_sessions);
            }
        }
        for (producer_id, session_ids) in &touched_sessions_by_producer {
            resolve_producer_sessions_in_tx(&tx, producer_id, session_ids)?;
        }

        let snapshot = maybe_publish_snapshot_in_tx(&tx, now_ms)?;
        let origin_frontier = current_origin_frontier_in_tx(&tx)?;

        let mut custody_acks = Vec::with_capacity(touched_origins.len());
        for (producer_id, epoch) in &touched_origins {
            let (contiguous_seq, max_received_seq, buffered_count) = tx.query_row(
                "SELECT contiguous_seq, max_received_seq, buffered_count
                 FROM hub_frontiers
                 WHERE producer_id = ?1 AND epoch = ?2",
                rusqlite::params![producer_id, epoch],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                    ))
                },
            )?;
            custody_acks.push(HubCustodyWatermark {
                producer_id: producer_id.clone(),
                epoch: epoch.clone(),
                contiguous_seq,
                max_received_seq,
                buffered_count,
                acked_at: now_ms,
            });
        }

        tx.commit()?;

        Ok(IngestBatchResponse {
            accepted_count: to_insert.len(),
            replayed_count,
            durable_fsynced: true,
            verified_pack_digests,
            custody_acks,
            origin_frontier,
            snapshot,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn quarantine_and_fail(
        &self,
        tx: rusqlite::Transaction<'_>,
        event_id: &EventId,
        op_id: &str,
        existing_digest: &str,
        conflicting_digest: &str,
        conflicting_envelope: &Value,
        conflicting_payload: Option<&PayloadAddr>,
        now_ms: i64,
    ) -> Result<IngestBatchResponse, HubError> {
        let env_json = serde_json::to_string(conflicting_envelope)?;
        let payload_json = match conflicting_payload {
            Some(addr) => Some(serde_json::to_string(addr)?),
            None => None,
        };
        tx.execute(
            "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
             VALUES (?1, ?1, '{}', ?2, ?2)
             ON CONFLICT(producer_id) DO NOTHING",
            rusqlite::params![event_id.producer_id, now_ms],
        )?;
        tx.execute(
            "INSERT INTO hub_quarantine(
                 producer_id, epoch, seq, op_id, existing_digest,
                 conflicting_digest, conflicting_envelope, conflicting_payload, quarantined_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(producer_id, epoch, seq, conflicting_digest) DO NOTHING",
            rusqlite::params![
                event_id.producer_id,
                event_id.epoch,
                event_id.seq as i64,
                op_id,
                existing_digest,
                conflicting_digest,
                env_json,
                payload_json,
                now_ms,
            ],
        )?;

        recompute_frontier_in_tx(&tx, &event_id.producer_id, &event_id.epoch, now_ms)?;
        recompute_producer_sessions_in_tx(&tx, &event_id.producer_id)?;
        let _ = maybe_publish_snapshot_in_tx(&tx, now_ms)?;

        let origin_frontier = current_origin_frontier_in_tx(&tx)?;
        let quarantine_count: u64 =
            tx.query_row("SELECT COUNT(*) FROM hub_quarantine", [], |r| r.get::<_, i64>(0))? as u64;

        tx.commit()?;

        Err(HubError::EventConflict(Box::new(super::EventConflictDetail {
            event_id: event_id.clone(),
            existing_digest: existing_digest.to_string(),
            conflicting_digest: conflicting_digest.to_string(),
            origin_frontier,
            quarantine_count,
        })))
    }
}

fn current_origin_frontier_in_tx(tx: &rusqlite::Transaction<'_>) -> Result<BTreeMap<(String, String), u64>, HubError> {
    let mut out = BTreeMap::new();
    let mut stmt = tx.prepare(
        "SELECT producer_id, epoch, contiguous_seq
         FROM hub_frontiers
         ORDER BY producer_id ASC, epoch ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? as u64,
        ))
    })?;
    for row in rows {
        let (p, e, seq) = row?;
        out.insert((p, e), seq);
    }
    Ok(out)
}

#[cfg(test)]
mod astra_tests {
    use super::*;
    #[test]
    fn adjacent_records_decode_each_frame_only_once() {
        let world = cv_testkit::World::new("hub-frame-verification");
        let mut builder = crate::packs::PackBuilder::with_frame_size(&world.cv_home, "tiny", 64, 1).unwrap();
        for i in 0..1000 {
            builder.append_record(format!("r{i}"), b"x").unwrap();
        }
        let manifest = builder.seal(Claim::New).unwrap();
        let file = std::fs::File::open(pack_zst_path(&world.cv_home, "tiny").unwrap()).unwrap();
        let reader = SeekableReader::open(file).unwrap();
        verify_record_digests(&reader, &manifest).unwrap();
        assert_eq!(reader.frames_decoded(), reader.index().len() as u64);
    }
}
