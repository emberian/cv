//! Atomic [`SnapshotPublicationManifest`] publication, contiguous frontier computation,
//! pinned fleet read consistency, and deterministic rebuild-from-raw verification.

use super::{canonical_json_bytes, encode_pair_key, EventId, HubError, HubStore, Producer};
use crate::ir::{Block, Harness, Session, SessionRef};
use crate::packs::{pack_idx_path, pack_zst_path, PackManifest, PayloadAddr, SealedPack};
use crate::query::{ExtFacts, Facts, FieldId, SessionQuery, Tri};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Version of the hub envelope-to-projection fold semantics committed in every
/// [`SnapshotPublicationManifest`]. Decoupled from [`crate::source::VERSION`] so adapter-level
/// transcript parser tweaks do not invalidate historical fleet snapshot IDs.
pub const HUB_SNAPSHOT_INTERPRETATION_VERSION: u32 = 1;

/// Maximum number of unpinned [`SnapshotPublicationManifest`] rows retained in `hub_snapshots`.
/// Explicitly pinned snapshots (`pinned = 1`) are exempt from retention pruning.
pub const MAX_RETAINED_HUB_SNAPSHOTS: usize = 256;

const fn default_interpretation_version() -> u32 {
    HUB_SNAPSHOT_INTERPRETATION_VERSION
}

fn is_empty_object(v: &Value) -> bool {
    matches!(v, Value::Null) || v.as_object().is_some_and(|m| m.is_empty())
}

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Revision-addressed session entry inside a hub generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRevisionEntry {
    pub producer_id: String,
    pub session_id: String,
    pub harness: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub message_count: u64,
    pub epoch: String,
    pub event_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_addr: Option<PayloadAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custody: Option<crate::custody::CustodyKind>,
    #[serde(default = "empty_json_object", skip_serializing_if = "is_empty_object")]
    pub metadata: Value,
}

impl SessionRevisionEntry {
    pub fn session_key_struct(&self) -> Option<crate::SessionKey> {
        let harness = Harness::parse(&self.harness)?;
        let native_id = self
            .metadata
            .get("native_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| self.session_id.clone());
        let profile = self
            .metadata
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into();
        Some(crate::SessionKey {
            producer: self.producer_id.clone(),
            profile,
            harness,
            native_id,
        })
    }

    pub fn selector_string(&self) -> String {
        match self.session_key_struct() {
            Some(k) => k.selector(),
            None => format!("{}:{}:{}", self.producer_id, self.harness, self.session_id),
        }
    }
}

/// Compact generation header persisted in `hub_generations` (`O(1)` per published batch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubGenerationRecord {
    pub generation: u64,
    pub publication_id: String,
    pub published_at: i64,
    pub frontier_digest: String,
    pub control_revision: u64,
    pub quarantine_count: u64,
    pub indexed_generation: u64,
    pub pinned: bool,
}

/// Immutable, monotonically numbered fleet read-consistency manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPublicationManifest {
    pub hub_generation: u64,
    pub publication_id: String,
    pub published_at: i64,
    #[serde(default = "default_interpretation_version")]
    pub interpretation_version: u32,
    #[serde(with = "super::origin_frontier_serde")]
    pub origin_frontier: BTreeMap<(String, String), u64>,
    pub pack_digests: BTreeMap<String, String>,
    pub session_revisions: BTreeMap<String, SessionRevisionEntry>,
    #[serde(default)]
    pub view_manifest_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub control_revision: u64,
    pub quarantine_count: u64,
}

/// Contiguous frontier and gap/lag status for one `(producer_id, epoch)` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierStatus {
    pub producer_id: String,
    pub epoch: String,
    pub contiguous_seq: u64,
    pub max_received_seq: u64,
    pub buffered_count: u64,
    pub lag_events: u64,
    pub missing_ranges: Vec<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_conflict_seq: Option<u64>,
    pub updated_at: i64,
}

/// One loud conflict quarantine entry recorded when a duplicate [`EventId`] arrives with a
/// mismatched digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRecord {
    pub quarantine_id: i64,
    pub event_id: EventId,
    pub op_id: String,
    pub existing_digest: String,
    pub conflicting_digest: String,
    pub quarantined_at: i64,
}

/// Summary of quarantined conflicting events on the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineSummary {
    pub count: u64,
    pub entries: Vec<QuarantineRecord>,
}

/// Operator resolution strategy for an active quarantine record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineResolution {
    AcceptExisting,
    AcceptIncoming,
}

impl QuarantineResolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AcceptExisting => "accept_existing",
            Self::AcceptIncoming => "accept_incoming",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "accept_existing" => Some(Self::AcceptExisting),
            "accept_incoming" => Some(Self::AcceptIncoming),
            _ => None,
        }
    }
}

/// Response returned after resolving a quarantine entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineResolveResponse {
    pub quarantine_id: i64,
    pub resolution: QuarantineResolution,
    pub snapshot: Option<SnapshotPublicationManifest>,
}

/// Response payload for `GET /api/hub/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubStatusResponse {
    pub hub: bool,
    pub producers: Vec<Producer>,
    pub frontiers: Vec<FrontierStatus>,
    pub quarantine: QuarantineSummary,
    pub mirrored_pack_count: u64,
    pub latest_snapshot: Option<SnapshotPublicationManifest>,
}

/// Response payload for `GET /api/hub/snapshot[?id=<publication_id>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubSnapshotResponse {
    pub snapshot: SnapshotPublicationManifest,
    pub frontiers: Vec<FrontierStatus>,
    pub quarantine: QuarantineSummary,
}

/// One fleet session listing row pinned to a [`SnapshotPublicationManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubSessionRow {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub producer_id: String,
    pub harness: String,
    pub revision: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: u64,
    pub size_bytes: Option<u64>,
    pub pack_mirrored: bool,
    pub epoch: String,
    pub event_seq: u64,
}

impl From<&SessionRevisionEntry> for HubSessionRow {
    fn from(entry: &SessionRevisionEntry) -> Self {
        Self {
            id: entry.metadata["native_id"].as_str().unwrap_or(&entry.session_id).into(),
            session_key: entry.metadata["profile"].as_str().map(|_| entry.session_id.clone()),
            producer_id: entry.producer_id.clone(),
            harness: entry.harness.clone(),
            revision: entry.revision.clone(),
            cwd: entry.cwd.clone(),
            title: entry.title.clone(),
            model: entry.model.clone(),
            created_at: entry.created_at.clone(),
            updated_at: entry.updated_at.clone(),
            message_count: entry.message_count,
            size_bytes: entry.payload_addr.as_ref().map(|a| a.length),
            pack_mirrored: entry.payload_addr.is_some(),
            epoch: entry.epoch.clone(),
            event_seq: entry.event_seq,
        }
    }
}

/// Paginated fleet sessions response pinned to one [`SnapshotPublicationManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubSessionsPage {
    pub snapshot_id: Option<String>,
    pub hub_generation: u64,
    pub total: usize,
    pub offset: usize,
    pub limit: Option<usize>,
    pub sessions: Vec<HubSessionRow>,
}

/// Result of hydrating one `(producer_id, session_id)` from a pinned [`SnapshotPublicationManifest`].
#[derive(Debug, Clone)]
pub enum HydratedHubSession {
    Hydrated {
        snapshot_id: String,
        hub_generation: u64,
        entry: SessionRevisionEntry,
        session: Box<Session>,
    },
    PayloadUnavailable {
        snapshot_id: String,
        hub_generation: u64,
        reason: &'static str,
        entry: SessionRevisionEntry,
    },
}

impl HydratedHubSession {
    pub fn to_json(&self) -> Result<Value, HubError> {
        match self {
            Self::Hydrated {
                snapshot_id,
                hub_generation,
                entry,
                session,
            } => {
                let mut val = serde_json::to_value(session)?;
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("payload_unavailable".into(), Value::Bool(false));
                    obj.insert("producer_id".into(), Value::String(entry.producer_id.clone()));
                    obj.insert("revision".into(), Value::String(entry.revision.clone()));
                    obj.insert("snapshot_id".into(), Value::String(snapshot_id.clone()));
                    obj.insert("hub_generation".into(), json!(*hub_generation));
                }
                Ok(val)
            }
            Self::PayloadUnavailable {
                snapshot_id,
                hub_generation,
                reason,
                entry,
            } => Ok(json!({
                "payload_unavailable": true,
                "reason": *reason,
                "snapshot_id": snapshot_id,
                "hub_generation": *hub_generation,
                "producer_id": entry.producer_id,
                "session_id": entry.session_id,
                "revision": entry.revision,
                "metadata": {
                    "id": entry.session_id,
                    "producer_id": entry.producer_id,
                    "harness": entry.harness,
                    "revision": entry.revision,
                    "title": entry.title,
                    "cwd": entry.cwd,
                    "model": entry.model,
                    "created_at": entry.created_at,
                    "updated_at": entry.updated_at,
                    "message_count": entry.message_count,
                    "epoch": entry.epoch,
                    "event_seq": entry.event_seq,
                    "extra": entry.metadata,
                }
            })),
        }
    }
}

fn view_digest(val: &Value) -> Result<String, HubError> {
    let canonical = canonical_json_bytes(val)?;
    let hex = blake3::hash(&canonical).to_hex();
    Ok(format!("view-{}", &hex.as_str()[..24]))
}

pub(crate) fn compute_view_manifest_ids(
    origin_frontier: &BTreeMap<(String, String), u64>,
    pack_digests: &BTreeMap<String, String>,
    session_revisions: &BTreeMap<String, SessionRevisionEntry>,
) -> Result<BTreeMap<String, String>, HubError> {
    let frontier_encoded: BTreeMap<String, u64> = origin_frontier
        .iter()
        .map(|((p, e), &seq)| (encode_pair_key(p, e), seq))
        .collect();
    let mut views = BTreeMap::new();
    views.insert("frontiers".into(), view_digest(&json!(frontier_encoded))?);
    views.insert("packs".into(), view_digest(&json!(pack_digests))?);
    views.insert("sessions".into(), view_digest(&json!(session_revisions))?);
    Ok(views)
}

/// Compute deterministic `frontier_digest` and `publication_id` in `O(|origins|)` from the
/// rolling per-origin `(contiguous_seq, frontier_hash)` commitments without scanning fleet sessions.
pub(crate) fn compute_frontier_digest(
    origin_hashes: &BTreeMap<(String, String), (u64, String)>,
) -> Result<String, HubError> {
    let encoded: BTreeMap<String, Value> = origin_hashes
        .iter()
        .map(|((p, e), (seq, h))| (encode_pair_key(p, e), json!([seq, h])))
        .collect();
    let bytes = canonical_json_bytes(&Value::Object(encoded.into_iter().collect()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub(crate) fn compute_generation_publication_id(
    interpretation_version: u32,
    frontier_digest: &str,
    control_revision: u64,
    quarantine_count: u64,
) -> Result<String, HubError> {
    let doc = json!({
        "domain": "cv-hub-gen-v2",
        "interpretation_version": interpretation_version,
        "frontier_digest": frontier_digest,
        "control_revision": control_revision,
        "quarantine_count": quarantine_count,
    });
    let canonical = canonical_json_bytes(&doc)?;
    let hex = blake3::hash(&canonical).to_hex();
    Ok(format!("pub-{}", &hex.as_str()[..24]))
}

fn roll_frontier_hash(prev: &str, seq: u64, event_digest: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cv-frontier-v2:");
    hasher.update(prev.as_bytes());
    hasher.update(b":");
    hasher.update(seq.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(event_digest.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Advance the contiguous origin frontier `F(p, e)` incrementally in `O(delta + buffered)` by
/// scanning only `seq > old_contiguous_seq` on `idx_hub_ingested_origin_seq`.
///
/// Returns `(fold_after_seq, new_contiguous_seq)`. If a pre-v3 row had `delta_seeded == 0`,
/// `fold_after_seq` is reset to `0` (and `control_events` cleared) so the first v3 delta pass
/// transparently seeds `hub_epoch_sessions` and `hub_frontier_packs`.
pub(crate) fn advance_frontier_in_tx(
    tx: &Transaction<'_>,
    producer_id: &str,
    epoch: &str,
    now_ms: i64,
) -> Result<(u64, u64), HubError> {
    let prev: Option<(u64, u64, Option<u64>, bool)> = tx
        .query_row(
            "SELECT contiguous_seq, max_received_seq, first_conflict_seq, delta_seeded
             FROM hub_frontiers
             WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    r.get::<_, i64>(3)? != 0,
                ))
            },
        )
        .optional()?;

    let (old_contiguous_seq, old_max_seq, was_seeded) = prev.map(|(c, m, _, s)| (c, m, s)).unwrap_or((0, 0, true));

    let first_conflict_seq: Option<u64> = tx
        .query_row(
            "SELECT MIN(seq)
             FROM hub_quarantine
             WHERE producer_id = ?1
               AND epoch = ?2
               AND resolved_at IS NULL
               AND committed_at_quarantine = 0
               AND seq > ?3",
            rusqlite::params![producer_id, epoch, old_contiguous_seq as i64],
            |r| r.get::<_, Option<i64>>(0),
        )?
        .map(|v| v as u64);

    let mut stmt = tx.prepare(
        "SELECT seq FROM hub_ingested_events
         WHERE producer_id = ?1 AND epoch = ?2 AND seq > ?3
         ORDER BY seq ASC",
    )?;
    let tail_seqs: Vec<u64> = stmt
        .query_map(rusqlite::params![producer_id, epoch, old_contiguous_seq as i64], |r| {
            Ok(r.get::<_, i64>(0)? as u64)
        })?
        .collect::<Result<Vec<_>, _>>()?;

    if prev.is_none() && tail_seqs.is_empty() && first_conflict_seq.is_none() {
        return Ok((0, 0));
    }

    let mut contiguous_seq = old_contiguous_seq;
    for &seq in &tail_seqs {
        if first_conflict_seq.is_some_and(|conflict| seq >= conflict) {
            break;
        }
        if seq == contiguous_seq + 1 {
            contiguous_seq = seq;
        } else if seq > contiguous_seq + 1 {
            break;
        }
    }

    let max_received_seq = tail_seqs
        .last()
        .copied()
        .unwrap_or(old_max_seq)
        .max(old_max_seq)
        .max(contiguous_seq);
    let buffered_count = tail_seqs.iter().filter(|&&s| s > contiguous_seq).count() as u64;

    if !was_seeded {
        tx.execute(
            "DELETE FROM hub_epoch_sessions WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
        )?;
        tx.execute(
            "DELETE FROM hub_frontier_packs WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
        )?;
    }

    tx.execute(
        "INSERT INTO hub_frontiers(
             producer_id, epoch, contiguous_seq, max_received_seq,
             buffered_count, first_conflict_seq, updated_at,
             control_events, delta_seeded, frontier_hash
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 1, '')
         ON CONFLICT(producer_id, epoch) DO UPDATE SET
             contiguous_seq = excluded.contiguous_seq,
             max_received_seq = excluded.max_received_seq,
             buffered_count = excluded.buffered_count,
             first_conflict_seq = excluded.first_conflict_seq,
             updated_at = excluded.updated_at,
             control_events = CASE WHEN hub_frontiers.delta_seeded = 0 THEN 0 ELSE hub_frontiers.control_events END,
             frontier_hash = CASE WHEN hub_frontiers.delta_seeded = 0 THEN '' ELSE hub_frontiers.frontier_hash END,
             delta_seeded = 1",
        rusqlite::params![
            producer_id,
            epoch,
            contiguous_seq as i64,
            max_received_seq as i64,
            buffered_count as i64,
            first_conflict_seq.map(|v| v as i64),
            now_ms,
        ],
    )?;

    let fold_after = if was_seeded { old_contiguous_seq } else { 0 };
    Ok((fold_after, contiguous_seq))
}

/// Recompute the contiguous origin frontier `F(p, e)` from scratch for `(producer_id, epoch)`
/// inside `tx` (used on loud conflict quarantine and `rebuild_from_raw`).
///
/// `F(p, e) = max { n : 1..=n all present in hub_ingested_events AND n < first_conflict_seq }`.
pub(crate) fn recompute_frontier_in_tx(
    tx: &Transaction<'_>,
    producer_id: &str,
    epoch: &str,
    now_ms: i64,
) -> Result<u64, HubError> {
    let first_conflict_seq: Option<u64> = tx
        .query_row(
            "SELECT MIN(seq)
             FROM hub_quarantine
             WHERE producer_id = ?1
               AND epoch = ?2
               AND resolved_at IS NULL
               AND committed_at_quarantine = 0",
            rusqlite::params![producer_id, epoch],
            |r| r.get::<_, Option<i64>>(0),
        )?
        .map(|v| v as u64);

    let mut stmt = tx.prepare(
        "SELECT seq FROM hub_ingested_events
         WHERE producer_id = ?1 AND epoch = ?2
         ORDER BY seq ASC",
    )?;
    let seqs: Vec<u64> = stmt
        .query_map(
            rusqlite::params![producer_id, epoch],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?
        .collect::<Result<Vec<_>, _>>()?;

    if seqs.is_empty() && first_conflict_seq.is_none() {
        tx.execute(
            "DELETE FROM hub_frontiers WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
        )?;
        return Ok(0);
    }

    let mut contiguous_seq = 0u64;
    for &seq in &seqs {
        if first_conflict_seq.is_some_and(|conflict| seq >= conflict) {
            break;
        }
        if seq == contiguous_seq + 1 {
            contiguous_seq = seq;
        } else if seq > contiguous_seq + 1 {
            break;
        }
    }

    let max_received_seq = seqs.last().copied().unwrap_or(0);
    let buffered_count = seqs.iter().filter(|&&s| s > contiguous_seq).count() as u64;

    tx.execute(
        "INSERT INTO hub_frontiers(
             producer_id, epoch, contiguous_seq, max_received_seq,
             buffered_count, first_conflict_seq, updated_at,
             control_events, delta_seeded
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 1)
         ON CONFLICT(producer_id, epoch) DO UPDATE SET
             contiguous_seq = excluded.contiguous_seq,
             max_received_seq = excluded.max_received_seq,
             buffered_count = excluded.buffered_count,
             first_conflict_seq = excluded.first_conflict_seq,
             updated_at = excluded.updated_at,
             delta_seeded = 1",
        rusqlite::params![
            producer_id,
            epoch,
            contiguous_seq as i64,
            max_received_seq as i64,
            buffered_count as i64,
            first_conflict_seq.map(|v| v as i64),
            now_ms,
        ],
    )?;

    Ok(contiguous_seq)
}

#[derive(Debug)]
enum ParsedEnvelopeOp {
    Ignore,
    Delete { session_id: String },
    Upsert(Box<SessionRevisionEntry>),
}

fn parse_rfc3339_ms(ts: Option<&str>) -> Option<i64> {
    let s = ts?.trim();
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
}

fn str_field<'a>(primary: &'a Value, secondary: Option<&'a Value>, key: &str) -> Option<String> {
    primary
        .get(key)
        .and_then(Value::as_str)
        .or_else(|| secondary.and_then(|s| s.get(key)).and_then(Value::as_str))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn scoped_session_id(producer: &str, harness: &str, native: &str, profile: Option<&str>) -> String {
    match (Harness::parse(harness), profile) {
        (Some(harness), Some(profile)) => crate::SessionKey {
            producer: producer.into(),
            profile: profile.into(),
            harness,
            native_id: native.into(),
        }
        .selector(),
        _ => native.to_string(),
    }
}

fn parse_envelope_op(
    producer_id: &str,
    epoch: &str,
    event_seq: u64,
    event_digest: &str,
    envelope: &Value,
    payload_addr: Option<PayloadAddr>,
) -> ParsedEnvelopeOp {
    let explicit_kind = envelope.get("kind").and_then(Value::as_str);
    let nested = envelope.get("session").filter(|v| v.is_object());
    match explicit_kind {
        Some("session_delete") => {
            if let Some(sid) = envelope
                .get("session_id")
                .or_else(|| envelope.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return ParsedEnvelopeOp::Delete {
                    session_id: scoped_session_id(
                        producer_id,
                        envelope["harness"].as_str().unwrap_or(""),
                        sid,
                        envelope["profile"].as_str(),
                    ),
                };
            }
            return ParsedEnvelopeOp::Ignore;
        }
        Some("session_upsert") | Some("session_archive") | Some("session_delta") => {}
        None if nested.is_some() => {}
        // Non-session envelopes (`filtered_out`, `receipt`, `hook_receipt`, `lane_start`,
        // `heartbeat`, `noop`, `task_event`, etc.) stay durably committed in `hub_ingested_events`
        // and advance `F(p, e)` without mutating or clobbering `hub_sessions`.
        _ => return ParsedEnvelopeOp::Ignore,
    }

    let Some(session_id) = str_field(envelope, nested, "session_id").or_else(|| str_field(envelope, nested, "id"))
    else {
        return ParsedEnvelopeOp::Ignore;
    };

    let harness = str_field(envelope, nested, "harness").unwrap_or_else(|| Harness::ALL[0].as_str().to_string());
    let title = str_field(envelope, nested, "title");
    let cwd = str_field(envelope, nested, "cwd");
    let model = str_field(envelope, nested, "model");
    let created_at = str_field(envelope, nested, "created_at");
    let updated_at = str_field(envelope, nested, "updated_at");

    let message_count = envelope
        .get("message_count")
        .and_then(Value::as_u64)
        .or_else(|| nested.and_then(|s| s.get("message_count")).and_then(Value::as_u64))
        .or_else(|| {
            nested
                .and_then(|s| s.get("messages"))
                .and_then(Value::as_array)
                .map(|m| m.len() as u64)
        })
        .unwrap_or(0);

    let revision = str_field(envelope, None, "revision")
        .or_else(|| payload_addr.as_ref().map(PayloadAddr::digest_hex))
        .unwrap_or_else(|| event_digest.to_string());

    let custody = envelope
        .get("custody")
        .and_then(|c| serde_json::from_value::<crate::custody::CustodyKind>(c.clone()).ok());

    let mut metadata = envelope
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let session_id = if let Some(profile) = envelope["profile"].as_str() {
        if !metadata.is_object() {
            metadata = json!({});
        }
        metadata["native_id"] = json!(session_id);
        metadata["profile"] = json!(profile);
        scoped_session_id(producer_id, &harness, &session_id, Some(profile))
    } else {
        session_id
    };
    ParsedEnvelopeOp::Upsert(Box::new(SessionRevisionEntry {
        producer_id: producer_id.to_string(),
        session_id,
        harness,
        revision,
        title,
        cwd,
        model,
        created_at,
        updated_at,
        message_count,
        epoch: epoch.to_string(),
        event_seq,
        payload_addr,
        custody,
        metadata,
    }))
}

/// Seed all pre-v3 or unhashed `hub_frontiers` rows (`delta_seeded = 0` or `frontier_hash = ''`
/// with `contiguous_seq > 0`) into `hub_epoch_sessions`, `hub_frontier_packs`,
/// `hub_frontiers.control_events`, and `hub_frontiers.frontier_hash`, returning any changed
/// session heads so the next generation publication incorporates them cleanly.
pub(crate) fn ensure_all_frontiers_seeded_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
) -> Result<Vec<(String, String, Option<SessionRevisionEntry>)>, HubError> {
    let unseeded: Vec<(String, String, u64)> = {
        let mut stmt = tx.prepare(
            "SELECT producer_id, epoch, contiguous_seq
             FROM hub_frontiers
             WHERE delta_seeded = 0 OR (contiguous_seq > 0 AND frontier_hash = '')
             ORDER BY producer_id ASC, epoch ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if unseeded.is_empty() {
        return Ok(Vec::new());
    }

    let mut touched_by_producer: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (producer_id, epoch, contiguous_seq) in unseeded {
        tx.execute(
            "DELETE FROM hub_epoch_sessions WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
        )?;
        tx.execute(
            "DELETE FROM hub_frontier_packs WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
        )?;
        tx.execute(
            "UPDATE hub_frontiers
             SET control_events = 0, frontier_hash = '', delta_seeded = 1, updated_at = ?3
             WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch, now_ms],
        )?;
        let touched = fold_origin_range_in_tx(tx, &producer_id, &epoch, 0, contiguous_seq)?;
        if !touched.is_empty() {
            touched_by_producer.entry(producer_id).or_default().extend(touched);
        }
    }
    let mut head_deltas = Vec::new();
    for (producer_id, session_ids) in &touched_by_producer {
        head_deltas.extend(resolve_producer_sessions_in_tx(tx, producer_id, session_ids)?);
    }
    Ok(head_deltas)
}

/// Fold events in `(after_seq, upto_seq]` for `(producer_id, epoch)` into `hub_epoch_sessions`,
/// `hub_frontier_packs`, and `hub_frontiers.control_events`, returning the set of `session_id`s
/// touched in this delta.
pub(crate) fn fold_origin_range_in_tx(
    tx: &Transaction<'_>,
    producer_id: &str,
    epoch: &str,
    after_seq: u64,
    upto_seq: u64,
) -> Result<BTreeSet<String>, HubError> {
    let mut touched_sessions = BTreeSet::new();
    if upto_seq <= after_seq {
        return Ok(touched_sessions);
    }

    let mut stmt = tx.prepare(
        "SELECT seq, event_digest, envelope_json, payload_addr_json
         FROM hub_ingested_events
         WHERE producer_id = ?1 AND epoch = ?2 AND seq > ?3 AND seq <= ?4
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![producer_id, epoch, after_seq as i64, upto_seq as i64],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )?;

    let existing_epoch_ts = |session_id: &str| -> Result<Option<i64>, HubError> {
        Ok(tx
            .query_row(
                "SELECT sort_ts_ms FROM hub_epoch_sessions
                 WHERE producer_id = ?1 AND session_id = ?2 AND epoch = ?3",
                rusqlite::params![producer_id, session_id, epoch],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .filter(|&ts| ts != i64::MIN))
    };

    let mut frontier_hash: String = tx
        .query_row(
            "SELECT frontier_hash FROM hub_frontiers WHERE producer_id = ?1 AND epoch = ?2",
            rusqlite::params![producer_id, epoch],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();

    let mut added_control_events = 0u64;
    for row in rows {
        let (seq, event_digest, env_str, payload_str) = row?;
        frontier_hash = roll_frontier_hash(&frontier_hash, seq, &event_digest);
        let envelope: Value = serde_json::from_str(&env_str)?;
        let payload_addr: Option<PayloadAddr> = match payload_str {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        };
        if let Some(ref addr) = payload_addr {
            tx.execute(
                "INSERT OR IGNORE INTO hub_frontier_packs(producer_id, epoch, pack_id)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![producer_id, epoch, addr.pack_id],
            )?;
        }
        if let Some(custody_val) = envelope.get("custody") {
            if let Ok(crate::custody::CustodyKind::NativeExtents { extents, .. }) =
                serde_json::from_value::<crate::custody::CustodyKind>(custody_val.clone())
            {
                for ext in extents {
                    tx.execute(
                        "INSERT OR IGNORE INTO hub_frontier_packs(producer_id, epoch, pack_id)
                         VALUES (?1, ?2, ?3)",
                        rusqlite::params![producer_id, epoch, ext.pack_id],
                    )?;
                }
            }
        }

        let op = parse_envelope_op(producer_id, epoch, seq, &event_digest, &envelope, payload_addr);
        match op {
            ParsedEnvelopeOp::Ignore => {
                added_control_events += 1;
            }
            ParsedEnvelopeOp::Delete { session_id } => {
                let sort_ts_ms = match parse_rfc3339_ms(envelope.get("updated_at").and_then(Value::as_str)) {
                    Some(ts) => ts,
                    None => existing_epoch_ts(&session_id)?.unwrap_or(i64::MIN),
                };
                tx.execute(
                    "INSERT OR REPLACE INTO hub_epoch_sessions(
                         producer_id, session_id, epoch, is_delete, sort_ts_ms, event_seq, entry_json
                     ) VALUES (?1, ?2, ?3, 1, ?4, ?5, NULL)",
                    rusqlite::params![producer_id, session_id, epoch, sort_ts_ms, seq as i64],
                )?;
                touched_sessions.insert(session_id);
            }
            ParsedEnvelopeOp::Upsert(mut entry) => {
                let session_id = entry.session_id.clone();
                // If this event is an incremental `session_delta` carrying `delta_extents`,
                // append those extents to the prior epoch/head `NativeExtents` chain!
                if envelope.get("kind").and_then(Value::as_str) == Some("session_delta") {
                    if let Some(delta_extents) = envelope
                        .get("delta_extents")
                        .and_then(|v| serde_json::from_value::<Vec<PayloadAddr>>(v.clone()).ok())
                    {
                        for ext in &delta_extents {
                            tx.execute(
                                "INSERT OR IGNORE INTO hub_frontier_packs(producer_id, epoch, pack_id)
                                 VALUES (?1, ?2, ?3)",
                                rusqlite::params![producer_id, epoch, ext.pack_id],
                            )?;
                        }
                        let prev_entry: Option<SessionRevisionEntry> = tx
                            .query_row(
                                "SELECT entry_json FROM hub_epoch_sessions
                                 WHERE producer_id = ?1 AND session_id = ?2 AND epoch = ?3 AND is_delete = 0",
                                rusqlite::params![producer_id, &session_id, epoch],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .optional()?
                            .flatten()
                            .and_then(|raw| serde_json::from_str(&raw).ok())
                            .or_else(|| {
                                read_head_entry_in_conn(tx, producer_id, &session_id, None)
                                    .ok()
                                    .flatten()
                            });
                        let mut extents = match prev_entry.and_then(|p| p.custody) {
                            Some(crate::custody::CustodyKind::NativeExtents { extents, .. }) => extents,
                            _ => Vec::new(),
                        };
                        for ext in delta_extents {
                            if !extents.contains(&ext) {
                                extents.push(ext);
                            }
                        }
                        let total_bytes: u64 = extents.iter().map(|e| e.length).sum();
                        let file_name = envelope
                            .get("file_name")
                            .and_then(Value::as_str)
                            .unwrap_or("session.jsonl")
                            .to_string();
                        let layout: Vec<String> = envelope
                            .get("layout")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        let prefix_blake3 = envelope
                            .get("prefix_blake3")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if let Some(last) = extents.last().cloned() {
                            entry.payload_addr = Some(last);
                        }
                        entry.custody = Some(crate::custody::CustodyKind::NativeExtents {
                            extents,
                            total_bytes,
                            prefix_blake3,
                            file_name,
                            layout,
                        });
                    }
                }
                let sort_ts_ms = match parse_rfc3339_ms(entry.updated_at.as_deref().or(entry.created_at.as_deref())) {
                    Some(ts) => ts,
                    None => existing_epoch_ts(&session_id)?.unwrap_or(i64::MIN),
                };
                let entry_json = serde_json::to_string(&*entry)?;
                tx.execute(
                    "INSERT OR REPLACE INTO hub_epoch_sessions(
                         producer_id, session_id, epoch, is_delete, sort_ts_ms, event_seq, entry_json
                     ) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
                    rusqlite::params![producer_id, session_id, epoch, sort_ts_ms, seq as i64, entry_json],
                )?;
                touched_sessions.insert(session_id);
            }
        }
    }

    tx.execute(
        "UPDATE hub_frontiers
         SET control_events = control_events + ?1,
             frontier_hash = ?2
         WHERE producer_id = ?3 AND epoch = ?4",
        rusqlite::params![added_control_events as i64, frontier_hash, producer_id, epoch],
    )?;

    Ok(touched_sessions)
}

/// Resolve the winning cross-epoch state for each `session_id` of `producer_id` from
/// `hub_epoch_sessions` using strict total order `(effective_ts_ms, epoch, event_seq)` and
/// materialize into `hub_sessions`.
///
/// Epochs that omitted explicit RFC3339 timestamps (`sort_ts_ms == i64::MIN`) inherit the highest
/// `sort_ts_ms` observed in earlier epochs (`epoch ASC`) for that `(producer_id, session_id)`, so
/// an untimestamped `session_delete` in a newer epoch supersedes prior epochs without permanently
/// outranking later timestamped epochs, independent of batch arrival order.
fn epoch_generation(epoch: &str) -> u8 {
    // Pre-0.19.1 writer epochs were random UUIDv4 (`xxxxxxxx-xxxx-4xxx-...`), 93.75% of which
    // sort lexicographically after 0.19.1 monotonic UUIDv7 (`019...`). Ordering UUIDv4 as
    // generation 0 before UUIDv7/other epochs (generation 1) ensures post-upgrade epochs
    // supersede 0.19.0 epochs on untimestamped deletes and same-ms ties regardless of batch order.
    if epoch.len() == 36 && epoch.as_bytes().get(14) == Some(&b'4') && uuid::Uuid::parse_str(epoch).is_ok() {
        0
    } else {
        1
    }
}

pub(crate) fn resolve_producer_sessions_in_tx(
    tx: &Transaction<'_>,
    producer_id: &str,
    session_ids: &BTreeSet<String>,
) -> Result<Vec<(String, String, Option<SessionRevisionEntry>)>, HubError> {
    let mut stmt = tx.prepare(
        "SELECT epoch, is_delete, sort_ts_ms, event_seq, entry_json
         FROM hub_epoch_sessions
         WHERE producer_id = ?1 AND session_id = ?2",
    )?;
    let mut head_deltas = Vec::new();
    for session_id in session_ids {
        let mut epoch_rows: Vec<(String, i64, i64, i64, Option<String>)> = stmt
            .query_map(rusqlite::params![producer_id, session_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        epoch_rows.sort_by(|a, b| {
            epoch_generation(&a.0)
                .cmp(&epoch_generation(&b.0))
                .then_with(|| a.0.cmp(&b.0))
        });

        struct SessionWinner {
            rank: (i64, u8, String, i64),
            is_delete: i64,
            entry_json: Option<String>,
        }
        let mut running_ts = i64::MIN;
        let mut winner: Option<SessionWinner> = None;
        for (epoch, is_delete, raw_ts, event_seq, entry_json) in epoch_rows {
            let effective_ts = if raw_ts == i64::MIN {
                running_ts
            } else {
                running_ts = running_ts.max(raw_ts);
                raw_ts
            };
            let generation = epoch_generation(&epoch);
            let rank = (effective_ts, generation, epoch, event_seq);
            if winner.as_ref().is_none_or(|best| rank > best.rank) {
                winner = Some(SessionWinner {
                    rank,
                    is_delete,
                    entry_json,
                });
            }
        }
        let winner = winner.map(|w| (w.is_delete, w.entry_json));
        let prev_live = read_head_entry_in_conn(tx, producer_id, session_id, None)?;

        match winner {
            None | Some((1, _)) | Some((_, None)) => {
                tx.execute(
                    "DELETE FROM hub_sessions WHERE producer_id = ?1 AND session_id = ?2",
                    rusqlite::params![producer_id, session_id],
                )?;
                if prev_live.is_some() {
                    head_deltas.push((producer_id.to_string(), session_id.clone(), None));
                }
            }
            Some((_, Some(entry_json))) => {
                let entry: SessionRevisionEntry = serde_json::from_str(&entry_json)?;
                let payload_json = match &entry.payload_addr {
                    Some(addr) => Some(serde_json::to_string(addr)?),
                    None => None,
                };
                let meta_json = serde_json::to_string(&entry.metadata)?;
                tx.execute(
                    "INSERT OR REPLACE INTO hub_sessions(
                         producer_id, session_id, harness, revision, title, cwd, model,
                         created_at, updated_at, message_count, epoch, event_seq,
                         payload_addr_json, metadata_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    rusqlite::params![
                        entry.producer_id,
                        entry.session_id,
                        entry.harness,
                        entry.revision,
                        entry.title,
                        entry.cwd,
                        entry.model,
                        entry.created_at,
                        entry.updated_at,
                        entry.message_count as i64,
                        entry.epoch,
                        entry.event_seq as i64,
                        payload_json,
                        meta_json,
                    ],
                )?;
                if prev_live.as_ref() != Some(&entry) {
                    head_deltas.push((producer_id.to_string(), session_id.clone(), Some(entry)));
                }
            }
        }
    }
    Ok(head_deltas)
}

/// Deterministically rebuild all conflict-free frontier state (`seq <= F(p, e)`) for `producer_id`
/// from scratch via [`fold_origin_range_in_tx`] + [`resolve_producer_sessions_in_tx`].
pub(crate) fn recompute_producer_sessions_in_tx(
    tx: &Transaction<'_>,
    producer_id: &str,
) -> Result<Vec<(String, String, Option<SessionRevisionEntry>)>, HubError> {
    let existing_live_ids: BTreeSet<String> = {
        let mut stmt =
            tx.prepare("SELECT session_id FROM hub_session_heads WHERE producer_id = ?1 AND gen_to IS NULL")?;
        let rows = stmt.query_map([producer_id], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<BTreeSet<_>, _>>()?
    };

    tx.execute("DELETE FROM hub_sessions WHERE producer_id = ?1", [producer_id])?;
    tx.execute("DELETE FROM hub_epoch_sessions WHERE producer_id = ?1", [producer_id])?;
    tx.execute("DELETE FROM hub_frontier_packs WHERE producer_id = ?1", [producer_id])?;
    tx.execute(
        "UPDATE hub_frontiers SET control_events = 0, frontier_hash = '', delta_seeded = 1 WHERE producer_id = ?1",
        [producer_id],
    )?;

    let epochs: Vec<(String, u64)> = {
        let mut stmt = tx.prepare(
            "SELECT epoch, contiguous_seq
             FROM hub_frontiers
             WHERE producer_id = ?1
             ORDER BY epoch ASC",
        )?;
        let rows = stmt.query_map([producer_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let mut all_touched = existing_live_ids;
    for (epoch, contiguous_seq) in epochs {
        let touched = fold_origin_range_in_tx(tx, producer_id, &epoch, 0, contiguous_seq)?;
        all_touched.extend(touched);
    }
    resolve_producer_sessions_in_tx(tx, producer_id, &all_touched)
}

/// Cap unpinned rows in `hub_generations` to `max_unpinned` newest generations while retaining all
/// pinned (`pinned = 1`) generations and every `hub_session_heads` interval covering any retained
/// or pinned generation.
pub(crate) fn prune_unpinned_generations_in_tx(tx: &Transaction<'_>, max_unpinned: usize) -> Result<usize, HubError> {
    let deleted = tx.execute(
        "DELETE FROM hub_generations
         WHERE pinned = 0
           AND generation NOT IN (
               SELECT generation
               FROM hub_generations
               WHERE pinned = 0
               ORDER BY generation DESC
               LIMIT ?1
           )",
        [max_unpinned as i64],
    )?;
    if deleted > 0 {
        tx.execute(
            "DELETE FROM hub_session_heads
             WHERE gen_to IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM hub_generations g
                   WHERE g.generation >= hub_session_heads.gen_from
                     AND g.generation < hub_session_heads.gen_to
               )",
            [],
        )?;
    }
    Ok(deleted)
}

fn upsert_head_at_gen_in_tx(
    tx: &Transaction<'_>,
    gen: u64,
    producer_id: &str,
    session_id: &str,
    entry_opt: Option<&SessionRevisionEntry>,
) -> Result<(), HubError> {
    tx.execute(
        "UPDATE hub_session_heads
         SET gen_to = ?1
         WHERE producer_id = ?2 AND session_id = ?3 AND gen_to IS NULL AND gen_from < ?1",
        rusqlite::params![gen as i64, producer_id, session_id],
    )?;
    // If replacing within the exact same generation (e.g. during rebuild reconciliation), delete the open row at `gen`.
    tx.execute(
        "DELETE FROM hub_session_heads
         WHERE producer_id = ?1 AND session_id = ?2 AND gen_from = ?3 AND gen_to IS NULL",
        rusqlite::params![producer_id, session_id, gen as i64],
    )?;
    if let Some(entry) = entry_opt {
        let selector = entry.selector_string();
        let payload_json = match &entry.payload_addr {
            Some(addr) => Some(serde_json::to_string(addr)?),
            None => None,
        };
        let custody_json = match &entry.custody {
            Some(c) => Some(serde_json::to_string(c)?),
            None => None,
        };
        let meta_json = serde_json::to_string(&entry.metadata)?;
        tx.execute(
            "INSERT INTO hub_session_heads(
                 producer_id, session_id, selector, gen_from, gen_to,
                 harness, revision, title, cwd, model, created_at, updated_at,
                 message_count, epoch, event_seq, payload_addr_json, custody_json, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                entry.producer_id,
                entry.session_id,
                selector,
                gen as i64,
                entry.harness,
                entry.revision,
                entry.title,
                entry.cwd,
                entry.model,
                entry.created_at,
                entry.updated_at,
                entry.message_count as i64,
                entry.epoch,
                entry.event_seq as i64,
                payload_json,
                custody_json,
                meta_json,
            ],
        )?;
    }
    Ok(())
}

/// Publish a new monotonic generation into `hub_generations` and update `hub_session_heads` in
/// `O(|head_deltas| + |origins|)` — zero fleet-wide table scan, zero monolithic `manifest_json`.
pub(crate) fn publish_generation_delta_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    extra_deltas: &[(String, String, Option<SessionRevisionEntry>)],
) -> Result<Option<HubGenerationRecord>, HubError> {
    let mut all_deltas = ensure_all_frontiers_seeded_in_tx(tx, now_ms)?;
    all_deltas.extend(extra_deltas.iter().cloned());

    let mut origin_frontier: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut origin_hashes: BTreeMap<(String, String), (u64, String)> = BTreeMap::new();
    let mut total_control_events = 0u64;
    {
        let mut stmt = tx.prepare(
            "SELECT producer_id, epoch, contiguous_seq, control_events, frontier_hash
             FROM hub_frontiers
             ORDER BY producer_id ASC, epoch ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (p, e, seq, ctrl, fhash) = row?;
            origin_frontier.insert((p.clone(), e.clone()), seq);
            origin_hashes.insert((p, e), (seq, fhash));
            total_control_events = total_control_events.saturating_add(ctrl);
        }
    }

    let quarantine_count: u64 = tx.query_row(
        "SELECT COUNT(*) FROM hub_quarantine WHERE resolved_at IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;

    if origin_frontier.is_empty() && quarantine_count == 0 {
        return latest_generation_in_conn(tx);
    }

    let interpretation_version = HUB_SNAPSHOT_INTERPRETATION_VERSION;
    let frontier_digest = compute_frontier_digest(&origin_hashes)?;
    let control_revision = (origin_frontier.len() as u64)
        .saturating_add(quarantine_count)
        .saturating_add(total_control_events);
    let publication_id = compute_generation_publication_id(
        interpretation_version,
        &frontier_digest,
        control_revision,
        quarantine_count,
    )?;

    if let Some(prev) = latest_generation_in_conn(tx)? {
        if prev.publication_id == publication_id
            || (prev.frontier_digest == frontier_digest
                && prev.control_revision == control_revision
                && prev.quarantine_count == quarantine_count
                && all_deltas.is_empty())
        {
            // Reconcile any deltas at `prev.generation` if rebuilding a wiped head projection.
            for (producer_id, session_id, entry_opt) in &all_deltas {
                upsert_head_at_gen_in_tx(tx, prev.generation, producer_id, session_id, entry_opt.as_ref())?;
            }
            return Ok(Some(prev));
        }
    }

    let existing_same_id: Option<i64> = tx
        .query_row(
            "SELECT generation FROM hub_generations WHERE publication_id = ?1",
            [&publication_id],
            |r| r.get(0),
        )
        .optional()?;

    let next_gen: u64 = tx.query_row(
        "SELECT COALESCE(MAX(generation), 0) + 1 FROM hub_generations",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;

    let final_pub_id = if existing_same_id.is_some() {
        format!("{publication_id}-g{next_gen}")
    } else {
        publication_id
    };

    for (producer_id, session_id, entry_opt) in &all_deltas {
        upsert_head_at_gen_in_tx(tx, next_gen, producer_id, session_id, entry_opt.as_ref())?;
    }

    let frontier_encoded: BTreeMap<String, u64> = origin_frontier
        .iter()
        .map(|((p, e), &seq)| (encode_pair_key(p, e), seq))
        .collect();
    let frontier_json = serde_json::to_string(&frontier_encoded)?;

    tx.execute(
        "INSERT INTO hub_generations(
             generation, publication_id, published_at, frontier_digest,
             control_revision, quarantine_count, indexed_generation, pinned, frontier_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)",
        rusqlite::params![
            next_gen as i64,
            final_pub_id,
            now_ms,
            frontier_digest,
            control_revision as i64,
            quarantine_count as i64,
            frontier_json,
        ],
    )?;
    prune_unpinned_generations_in_tx(tx, MAX_RETAINED_HUB_SNAPSHOTS)?;

    Ok(Some(HubGenerationRecord {
        generation: next_gen,
        publication_id: final_pub_id,
        published_at: now_ms,
        frontier_digest,
        control_revision,
        quarantine_count,
        indexed_generation: 0,
        pinned: false,
    }))
}

pub(crate) fn latest_generation_in_conn(conn: &Connection) -> Result<Option<HubGenerationRecord>, HubError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_generations')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT generation, publication_id, published_at, frontier_digest,
                    control_revision, quarantine_count, indexed_generation, pinned
             FROM hub_generations
             ORDER BY generation DESC
             LIMIT 1",
            [],
            |r| {
                Ok(HubGenerationRecord {
                    generation: r.get::<_, i64>(0)? as u64,
                    publication_id: r.get(1)?,
                    published_at: r.get(2)?,
                    frontier_digest: r.get(3)?,
                    control_revision: r.get::<_, i64>(4)? as u64,
                    quarantine_count: r.get::<_, i64>(5)? as u64,
                    indexed_generation: r.get::<_, i64>(6)? as u64,
                    pinned: r.get::<_, i64>(7)? != 0,
                })
            },
        )
        .optional()?)
}

fn map_head_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRevisionEntry> {
    let payload_str: Option<String> = r.get(12)?;
    let custody_str: Option<String> = r.get(13)?;
    let meta_str: String = r.get(14)?;
    let payload_addr = payload_str.and_then(|s| serde_json::from_str(&s).ok());
    let custody = custody_str.and_then(|s| serde_json::from_str(&s).ok());
    let metadata = serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({}));
    Ok(SessionRevisionEntry {
        producer_id: r.get(0)?,
        session_id: r.get(1)?,
        harness: r.get(2)?,
        revision: r.get(3)?,
        title: r.get(4)?,
        cwd: r.get(5)?,
        model: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        message_count: r.get::<_, i64>(9)? as u64,
        epoch: r.get(10)?,
        event_seq: r.get::<_, i64>(11)? as u64,
        payload_addr,
        custody,
        metadata,
    })
}

pub(crate) fn read_head_entry_in_conn(
    conn: &Connection,
    producer_id: &str,
    session_id: &str,
    as_of_gen: Option<u64>,
) -> Result<Option<SessionRevisionEntry>, HubError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_session_heads')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(None);
    }
    match as_of_gen {
        None => Ok(conn
            .query_row(
                "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                        created_at, updated_at, message_count, epoch, event_seq,
                        payload_addr_json, custody_json, metadata_json
                 FROM hub_session_heads
                 WHERE producer_id = ?1 AND session_id = ?2 AND gen_to IS NULL",
                rusqlite::params![producer_id, session_id],
                map_head_row,
            )
            .optional()?),
        Some(g) => Ok(conn
            .query_row(
                "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                        created_at, updated_at, message_count, epoch, event_seq,
                        payload_addr_json, custody_json, metadata_json
                 FROM hub_session_heads
                 WHERE producer_id = ?1 AND session_id = ?2
                   AND gen_from <= ?3 AND (gen_to IS NULL OR gen_to > ?3)
                 ORDER BY gen_from DESC
                 LIMIT 1",
                rusqlite::params![producer_id, session_id, g as i64],
                map_head_row,
            )
            .optional()?),
    }
}

pub(crate) fn materialize_snapshot_in_conn(
    conn: &Connection,
    gen_rec: &HubGenerationRecord,
) -> Result<SnapshotPublicationManifest, HubError> {
    let frontier_raw: String = conn
        .query_row(
            "SELECT frontier_json FROM hub_generations WHERE generation = ?1",
            [gen_rec.generation as i64],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "{}".into());
    let encoded_frontier: BTreeMap<String, u64> = serde_json::from_str(&frontier_raw).unwrap_or_default();
    let mut origin_frontier = BTreeMap::new();
    for (k, seq) in encoded_frontier {
        if let Ok(pair) = super::decode_pair_key(&k) {
            origin_frontier.insert(pair, seq);
        }
    }

    let mut session_revisions = BTreeMap::new();
    let mut referenced_packs = BTreeSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                    created_at, updated_at, message_count, epoch, event_seq,
                    payload_addr_json, custody_json, metadata_json
             FROM hub_session_heads
             WHERE gen_from <= ?1 AND (gen_to IS NULL OR gen_to > ?1)
             ORDER BY producer_id ASC, session_id ASC",
        )?;
        let rows = stmt.query_map([gen_rec.generation as i64], map_head_row)?;
        for row in rows {
            let entry = row?;
            if let Some(ref addr) = entry.payload_addr {
                referenced_packs.insert(addr.pack_id.clone());
            }
            if let Some(crate::custody::CustodyKind::NativeExtents { ref extents, .. }) = entry.custody {
                for ext in extents {
                    referenced_packs.insert(ext.pack_id.clone());
                }
            }
            let key = encode_pair_key(&entry.producer_id, &entry.session_id);
            session_revisions.insert(key, entry);
        }
    }

    let latest_gen = latest_generation_in_conn(conn)?.map(|g| g.generation);
    if latest_gen == Some(gen_rec.generation) {
        let mut stmt = conn.prepare("SELECT DISTINCT pack_id FROM hub_frontier_packs")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            referenced_packs.insert(r?);
        }
    }

    let mut pack_digests = BTreeMap::new();
    for pack_id in referenced_packs {
        if let Some(digest) = conn
            .query_row(
                "SELECT blake3_digest FROM hub_packs WHERE pack_id = ?1",
                [&pack_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            pack_digests.insert(pack_id, digest);
        }
    }

    let view_manifest_ids = compute_view_manifest_ids(&origin_frontier, &pack_digests, &session_revisions)?;
    Ok(SnapshotPublicationManifest {
        hub_generation: gen_rec.generation,
        publication_id: gen_rec.publication_id.clone(),
        published_at: gen_rec.published_at,
        interpretation_version: HUB_SNAPSHOT_INTERPRETATION_VERSION,
        origin_frontier,
        pack_digests,
        session_revisions,
        view_manifest_ids,
        control_revision: gen_rec.control_revision,
        quarantine_count: gen_rec.quarantine_count,
    })
}

pub(crate) fn latest_snapshot_in_conn(conn: &Connection) -> Result<Option<SnapshotPublicationManifest>, HubError> {
    let Some(gen_rec) = latest_generation_in_conn(conn)? else {
        return Ok(None);
    };
    Ok(Some(materialize_snapshot_in_conn(conn, &gen_rec)?))
}

fn missing_ranges_for(
    conn: &Connection,
    producer_id: &str,
    epoch: &str,
    contiguous_seq: u64,
) -> Result<Vec<(u64, u64)>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT seq FROM hub_ingested_events
         WHERE producer_id = ?1 AND epoch = ?2 AND seq > ?3
         ORDER BY seq ASC",
    )?;
    let seqs: Vec<u64> = stmt
        .query_map(rusqlite::params![producer_id, epoch, contiguous_seq as i64], |r| {
            Ok(r.get::<_, i64>(0)? as u64)
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut gaps = Vec::new();
    let mut expected = contiguous_seq + 1;
    for seq in seqs {
        if seq > expected {
            gaps.push((expected, seq - 1));
        }
        expected = seq.saturating_add(1);
    }
    Ok(gaps)
}

fn read_frontiers_in_conn(conn: &Connection) -> Result<Vec<FrontierStatus>, HubError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_frontiers')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT producer_id, epoch, contiguous_seq, max_received_seq,
                buffered_count, first_conflict_seq, updated_at
         FROM hub_frontiers
         ORDER BY producer_id ASC, epoch ASC",
    )?;
    struct RawFrontierRow {
        producer_id: String,
        epoch: String,
        contiguous_seq: u64,
        max_received_seq: u64,
        buffered_count: u64,
        first_conflict_seq: Option<u64>,
        updated_at: i64,
    }
    let raw_rows: Vec<RawFrontierRow> = stmt
        .query_map([], |r| {
            Ok(RawFrontierRow {
                producer_id: r.get(0)?,
                epoch: r.get(1)?,
                contiguous_seq: r.get::<_, i64>(2)? as u64,
                max_received_seq: r.get::<_, i64>(3)? as u64,
                buffered_count: r.get::<_, i64>(4)? as u64,
                first_conflict_seq: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                updated_at: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(raw_rows.len());
    for row in raw_rows {
        let missing_ranges = missing_ranges_for(conn, &row.producer_id, &row.epoch, row.contiguous_seq)?;
        let lag_events = row.max_received_seq.saturating_sub(row.contiguous_seq);
        out.push(FrontierStatus {
            producer_id: row.producer_id,
            epoch: row.epoch,
            contiguous_seq: row.contiguous_seq,
            max_received_seq: row.max_received_seq,
            buffered_count: row.buffered_count,
            lag_events,
            missing_ranges,
            first_conflict_seq: row.first_conflict_seq,
            updated_at: row.updated_at,
        });
    }
    Ok(out)
}

fn read_quarantine_in_conn(conn: &Connection) -> Result<QuarantineSummary, HubError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_quarantine')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(QuarantineSummary {
            count: 0,
            entries: Vec::new(),
        });
    }
    let count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM hub_quarantine WHERE resolved_at IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let mut stmt = conn.prepare(
        "SELECT quarantine_id, producer_id, epoch, seq, op_id,
                existing_digest, conflicting_digest, quarantined_at
         FROM hub_quarantine
         WHERE resolved_at IS NULL
         ORDER BY quarantine_id ASC",
    )?;
    let entries = stmt
        .query_map([], |r| {
            Ok(QuarantineRecord {
                quarantine_id: r.get(0)?,
                event_id: EventId::new(
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? as u64,
                ),
                op_id: r.get(4)?,
                existing_digest: r.get(5)?,
                conflicting_digest: r.get(6)?,
                quarantined_at: r.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(QuarantineSummary { count, entries })
}

pub(crate) fn decode_packed_session(bytes: &[u8]) -> Result<Session, HubError> {
    if let Ok(session) = serde_json::from_slice::<Session>(bytes) {
        return Ok(session);
    }
    let val: Value = serde_json::from_slice(bytes)?;
    if let Some(inner) = val.get("session") {
        let session: Session = serde_json::from_value(inner.clone())?;
        return Ok(session);
    }
    Err(HubError::Pack(
        "mirrored pack record does not decode to a valid Session IR".into(),
    ))
}

struct PinnedSessionExt<'a> {
    entry: &'a SessionRevisionEntry,
    session: Option<&'a Session>,
    hydrated_attempted: bool,
}

impl PinnedSessionExt<'_> {
    fn missing_session_tri(&self) -> Tri {
        if !self.hydrated_attempted && self.entry.payload_addr.is_some() {
            Tri::Unknown
        } else {
            Tri::False
        }
    }
}

impl ExtFacts for PinnedSessionExt<'_> {
    fn model(&self, needle: &str, exact: bool) -> anyhow::Result<Tri> {
        if let Some(m) = self.entry.model.as_deref() {
            let lower = m.to_lowercase();
            let matched = if exact { lower == needle } else { lower.contains(needle) };
            if matched {
                return Ok(Tri::True);
            }
        }
        Ok(match self.session {
            Some(_) => Tri::Unknown,
            None => self.missing_session_tri(),
        })
    }

    fn text(&self, needle: &str) -> anyhow::Result<Tri> {
        if self
            .entry
            .title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(needle))
        {
            return Ok(Tri::True);
        }
        let Some(session) = self.session else {
            return Ok(self.missing_session_tri());
        };
        if session
            .title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(needle))
        {
            return Ok(Tri::True);
        }
        for msg in &session.messages {
            for block in &msg.content {
                match block {
                    Block::Text { text, .. } | Block::Thinking { text, .. } => {
                        if text.to_lowercase().contains(needle) {
                            return Ok(Tri::True);
                        }
                    }
                    Block::ToolResult { content, .. } => {
                        if content.to_lowercase().contains(needle) {
                            return Ok(Tri::True);
                        }
                    }
                    Block::ToolUse { name, input, .. } => {
                        if name.to_lowercase().contains(needle) {
                            return Ok(Tri::True);
                        }
                        if let Ok(raw) = serde_json::to_string(input) {
                            if raw.to_lowercase().contains(needle) {
                                return Ok(Tri::True);
                            }
                        }
                    }
                    Block::Image { .. } | Block::File { .. } | Block::Opaque { .. } => {}
                }
            }
        }
        Ok(Tri::False)
    }

    fn tool(&self, name: &str) -> anyhow::Result<Tri> {
        let Some(session) = self.session else {
            return Ok(self.missing_session_tri());
        };
        let hit = session.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, Block::ToolUse { name: n, .. } if n.to_lowercase().contains(name)))
        });
        Ok(Tri::of(hit))
    }

    fn exact(&self, field: FieldId, needle: &str) -> anyhow::Result<Tri> {
        if field == FieldId::Tool {
            let Some(session) = self.session else {
                return Ok(self.missing_session_tri());
            };
            let hit = session.messages.iter().any(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, Block::ToolUse { name: n, .. } if n.to_lowercase() == needle))
            });
            return Ok(Tri::of(hit));
        }
        Ok(Tri::False)
    }
}

fn entry_to_session_ref(entry: &SessionRevisionEntry) -> SessionRef {
    let harness = Harness::parse(&entry.harness).unwrap_or(Harness::ALL[0]);
    let mut r = SessionRef::new(entry.session_id.clone(), harness, PathBuf::new());
    r.title = entry.title.clone();
    r.cwd = entry.cwd.as_deref().map(PathBuf::from);
    r.created_at = entry
        .created_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    r.updated_at = entry
        .updated_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    r.message_count = Some(entry.message_count as usize);
    r
}

impl HubStore {
    /// Read-only fast lookup of the latest `(generation, publication_id)`, returning `Ok(None)`
    /// when `state.db` does not exist or has no hub generations yet.
    pub fn latest_generation_fast(&self) -> Result<Option<(u64, String)>, HubError> {
        Ok(self
            .store
            .read_in(|conn| {
                let has_table: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_generations')",
                    [],
                    |r| r.get(0),
                )?;
                if !has_table {
                    return Ok(None);
                }
                Ok(conn
                    .query_row(
                        "SELECT generation, publication_id FROM hub_generations ORDER BY generation DESC LIMIT 1",
                        [],
                        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)),
                    )
                    .optional()?)
            })?
            .flatten())
    }

    /// Read-only fast lookup of the latest published `publication_id`.
    pub fn latest_publication_id_fast(&self) -> Result<Option<String>, HubError> {
        Ok(self.latest_generation_fast()?.map(|(_, pub_id)| pub_id))
    }

    /// Resolve an optional generation selector (`None` -> latest, `"2"` -> `generation = 2`,
    /// `"pub-..."` -> `publication_id`).
    pub fn resolve_generation(&self, spec: Option<&str>) -> Result<Option<HubGenerationRecord>, HubError> {
        let Some(conn) = self.store.open_readonly()? else {
            return match spec {
                Some(want) => Err(HubError::NotFound(format!("snapshot `{want}` not found"))),
                None => Ok(None),
            };
        };
        let has_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_generations')",
            [],
            |r| r.get(0),
        )?;
        if !has_table {
            return match spec {
                Some(want) => Err(HubError::NotFound(format!("snapshot `{want}` not found"))),
                None => Ok(None),
            };
        }
        match spec.map(str::trim).filter(|s| !s.is_empty()) {
            None => latest_generation_in_conn(&conn),
            Some(want) => {
                let by_gen = want.parse::<u64>().ok();
                let rec = if let Some(g) = by_gen {
                    conn.query_row(
                        "SELECT generation, publication_id, published_at, frontier_digest,
                                control_revision, quarantine_count, indexed_generation, pinned
                         FROM hub_generations
                         WHERE generation = ?1",
                        [g as i64],
                        |r| {
                            Ok(HubGenerationRecord {
                                generation: r.get::<_, i64>(0)? as u64,
                                publication_id: r.get(1)?,
                                published_at: r.get(2)?,
                                frontier_digest: r.get(3)?,
                                control_revision: r.get::<_, i64>(4)? as u64,
                                quarantine_count: r.get::<_, i64>(5)? as u64,
                                indexed_generation: r.get::<_, i64>(6)? as u64,
                                pinned: r.get::<_, i64>(7)? != 0,
                            })
                        },
                    )
                    .optional()?
                } else {
                    conn.query_row(
                        "SELECT generation, publication_id, published_at, frontier_digest,
                                control_revision, quarantine_count, indexed_generation, pinned
                         FROM hub_generations
                         WHERE publication_id = ?1",
                        [want],
                        |r| {
                            Ok(HubGenerationRecord {
                                generation: r.get::<_, i64>(0)? as u64,
                                publication_id: r.get(1)?,
                                published_at: r.get(2)?,
                                frontier_digest: r.get(3)?,
                                control_revision: r.get::<_, i64>(4)? as u64,
                                quarantine_count: r.get::<_, i64>(5)? as u64,
                                indexed_generation: r.get::<_, i64>(6)? as u64,
                                pinned: r.get::<_, i64>(7)? != 0,
                            })
                        },
                    )
                    .optional()?
                };
                match rec {
                    Some(r) => Ok(Some(r)),
                    None => Err(HubError::NotFound(format!("snapshot `{want}` not found"))),
                }
            }
        }
    }

    /// Indexed `O(log N)` point query for one session head by `selector` (or `(producer_id, session_id)`),
    /// optionally pinned to `as_of_gen`. Safe under `--frozen`.
    pub fn session_head_for(
        &self,
        selector: &str,
        as_of_gen: Option<u64>,
    ) -> Result<Option<SessionRevisionEntry>, HubError> {
        Ok(self
            .store
            .read_in(|conn| {
                let has_table: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_session_heads')",
                    [],
                    |r| r.get(0),
                )?;
                if !has_table {
                    return Ok(None);
                }
                let row = match as_of_gen {
                    None => conn
                        .query_row(
                            "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                                    created_at, updated_at, message_count, epoch, event_seq,
                                    payload_addr_json, custody_json, metadata_json
                             FROM hub_session_heads
                             WHERE selector = ?1 AND gen_to IS NULL",
                            [selector],
                            map_head_row,
                        )
                        .optional()?,
                    Some(g) => conn
                        .query_row(
                            "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                                    created_at, updated_at, message_count, epoch, event_seq,
                                    payload_addr_json, custody_json, metadata_json
                             FROM hub_session_heads
                             WHERE selector = ?1
                               AND gen_from <= ?2 AND (gen_to IS NULL OR gen_to > ?2)
                             ORDER BY gen_from DESC
                             LIMIT 1",
                            rusqlite::params![selector, g as i64],
                            map_head_row,
                        )
                        .optional()?,
                };
                Ok(row)
            })?
            .flatten())
    }

    /// List all session heads visible at `as_of_gen` (`None` = active `gen_to IS NULL` heads).
    /// Safe under `--frozen`.
    pub fn heads_at_generation(&self, as_of_gen: Option<u64>) -> Result<Vec<SessionRevisionEntry>, HubError> {
        if let Some(g) = as_of_gen {
            let _ = self.resolve_generation(Some(&g.to_string()))?;
        }
        Ok(self
            .store
            .read_in(|conn| {
                let has_table: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_session_heads')",
                    [],
                    |r| r.get(0),
                )?;
                if !has_table {
                    return Ok(Vec::new());
                }
                match as_of_gen {
                    None => {
                        let mut stmt = conn.prepare(
                            "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                                    created_at, updated_at, message_count, epoch, event_seq,
                                    payload_addr_json, custody_json, metadata_json
                             FROM hub_session_heads
                             WHERE gen_to IS NULL
                             ORDER BY updated_at DESC, producer_id ASC, session_id ASC",
                        )?;
                        let rows = stmt.query_map([], map_head_row)?;
                        Ok(rows.collect::<Result<Vec<_>, _>>()?)
                    }
                    Some(g) => {
                        let mut stmt = conn.prepare(
                            "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                                    created_at, updated_at, message_count, epoch, event_seq,
                                    payload_addr_json, custody_json, metadata_json
                             FROM hub_session_heads
                             WHERE gen_from <= ?1 AND (gen_to IS NULL OR gen_to > ?1)
                             ORDER BY updated_at DESC, producer_id ASC, session_id ASC",
                        )?;
                        let rows = stmt.query_map([g as i64], map_head_row)?;
                        Ok(rows.collect::<Result<Vec<_>, _>>()?)
                    }
                }
            })?
            .unwrap_or_default())
    }

    /// Return the latest published [`SnapshotPublicationManifest`], if any.
    pub fn latest_snapshot(&self) -> Result<Option<SnapshotPublicationManifest>, HubError> {
        self.get_snapshot(None)
    }

    /// Look up a [`SnapshotPublicationManifest`] by `publication_id` or numeric generation, or the
    /// latest published generation when `id` is `None`.
    pub fn get_snapshot(&self, id: Option<&str>) -> Result<Option<SnapshotPublicationManifest>, HubError> {
        let Some(gen_rec) = self.resolve_generation(id)? else {
            return Ok(None);
        };
        let Some(conn) = self.store.open_readonly()? else {
            return Ok(None);
        };
        Ok(Some(materialize_snapshot_in_conn(&conn, &gen_rec)?))
    }

    /// Build the `GET /api/hub/status` payload.
    pub fn hub_status(&self) -> Result<HubStatusResponse, HubError> {
        let producers = self.store.list_producers()?;
        let Some(conn) = self.store.open_readonly()? else {
            return Ok(HubStatusResponse {
                hub: true,
                producers,
                frontiers: Vec::new(),
                quarantine: QuarantineSummary {
                    count: 0,
                    entries: Vec::new(),
                },
                mirrored_pack_count: 0,
                latest_snapshot: None,
            });
        };
        let frontiers = read_frontiers_in_conn(&conn)?;
        let quarantine = read_quarantine_in_conn(&conn)?;
        let has_packs: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_packs')",
            [],
            |r| r.get(0),
        )?;
        let mirrored_pack_count = if has_packs {
            conn.query_row("SELECT COUNT(*) FROM hub_packs", [], |r| r.get::<_, i64>(0))? as u64
        } else {
            0
        };
        let latest_snapshot = latest_snapshot_in_conn(&conn)?;
        Ok(HubStatusResponse {
            hub: true,
            producers,
            frontiers,
            quarantine,
            mirrored_pack_count,
            latest_snapshot,
        })
    }

    /// Build the `GET /api/hub/snapshot[?id=<publication_id>]` payload.
    pub fn snapshot_response(&self, id: Option<&str>) -> Result<HubSnapshotResponse, HubError> {
        let snapshot = self
            .get_snapshot(id)?
            .ok_or_else(|| HubError::NotFound("no hub snapshot published yet".into()))?;
        let (frontiers, quarantine) = match self.store.open_readonly()? {
            Some(conn) => (read_frontiers_in_conn(&conn)?, read_quarantine_in_conn(&conn)?),
            None => (
                Vec::new(),
                QuarantineSummary {
                    count: 0,
                    entries: Vec::new(),
                },
            ),
        };
        Ok(HubSnapshotResponse {
            snapshot,
            frontiers,
            quarantine,
        })
    }

    /// Query fleet sessions pinned to `snapshot_id` (or the latest published snapshot when `None`).
    ///
    /// Multi-page reads (`offset` / `limit`) against a pinned `snapshot_id` read exclusively from
    /// that immutable [`SnapshotPublicationManifest`] and its content-addressed mirrored packs, so
    /// concurrent `ingest_batch` publications never cause page tearing.
    #[allow(clippy::too_many_arguments)]
    pub fn query_sessions(
        &self,
        snapshot_id: Option<&str>,
        producer: Option<&str>,
        harness: Option<&str>,
        cwd: Option<&str>,
        q: Option<&str>,
        offset: usize,
        limit: Option<usize>,
    ) -> Result<HubSessionsPage, HubError> {
        let want_harness = match harness {
            Some(h) => {
                Some(Harness::parse(h).ok_or_else(|| HubError::InvalidArgument(format!("unknown harness {h:?}")))?)
            }
            None => None,
        };
        let parsed_query = match q {
            Some(raw) if !raw.trim().is_empty() => Some(
                SessionQuery::parse(raw)
                    .map_err(|e| HubError::InvalidArgument(e.replace("`cv query`", "`cv schema`")))?,
            ),
            _ => None,
        };

        let Some(manifest) = self.get_snapshot(snapshot_id)? else {
            return Ok(HubSessionsPage {
                snapshot_id: None,
                hub_generation: 0,
                total: 0,
                offset,
                limit,
                sessions: Vec::new(),
            });
        };

        let mut entries: Vec<&SessionRevisionEntry> = Vec::with_capacity(manifest.session_revisions.len());
        let mut open_packs: BTreeMap<String, SealedPack> = BTreeMap::new();

        for entry in manifest.session_revisions.values() {
            if let Some(prod) = producer {
                if entry.producer_id != prod {
                    continue;
                }
            }
            if let Some(h) = want_harness {
                if Harness::parse(&entry.harness) != Some(h) {
                    continue;
                }
            }
            if let Some(cwd_needle) = cwd {
                if !entry.cwd.as_deref().is_some_and(|c| c.contains(cwd_needle)) {
                    continue;
                }
            }

            if let Some(ref sq) = parsed_query {
                let sref = entry_to_session_ref(entry);
                if !sq.prefilter(&sref) {
                    continue;
                }
                let cheap_ext = PinnedSessionExt {
                    entry,
                    session: None,
                    hydrated_attempted: false,
                };
                let cheap_tri = sq
                    .eval(&Facts {
                        r: &sref,
                        session: None,
                        ext: &cheap_ext,
                    })
                    .map_err(|e| HubError::InvalidArgument(e.to_string()))?;
                match cheap_tri {
                    Tri::True => {}
                    Tri::False => continue,
                    Tri::Unknown => {
                        let hydrated = match &entry.payload_addr {
                            Some(addr) => {
                                if !open_packs.contains_key(&addr.pack_id) {
                                    let pack = SealedPack::open(&self.packs_dir, &addr.pack_id)
                                        .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                                    open_packs.insert(addr.pack_id.clone(), pack);
                                }
                                let pack = open_packs.get(&addr.pack_id).expect("just inserted");
                                let bytes = pack.read_payload(addr).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                                Some(decode_packed_session(&bytes)?)
                            }
                            None => None,
                        };
                        let full_ext = PinnedSessionExt {
                            entry,
                            session: hydrated.as_ref(),
                            hydrated_attempted: true,
                        };
                        let matched = sq
                            .matches(&Facts {
                                r: &sref,
                                session: hydrated.as_ref(),
                                ext: &full_ext,
                            })
                            .map_err(|e| HubError::InvalidArgument(e.to_string()))?;
                        if !matched {
                            continue;
                        }
                    }
                }
            }

            entries.push(entry);
        }

        // Canonical fleet listing order: newest activity (`updated_at`, fallback `created_at`)
        // descending, breaking ties deterministically by `(producer_id, session_id)`.
        entries.sort_by(|a, b| {
            let ta = parse_rfc3339_ms(a.updated_at.as_deref().or(a.created_at.as_deref()));
            let tb = parse_rfc3339_ms(b.updated_at.as_deref().or(b.created_at.as_deref()));
            tb.cmp(&ta)
                .then_with(|| a.producer_id.cmp(&b.producer_id))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

        let total = entries.len();
        let sliced = entries.into_iter().skip(offset);
        let rows: Vec<HubSessionRow> = match limit {
            Some(n) => sliced.take(n).map(HubSessionRow::from).collect(),
            None => sliced.map(HubSessionRow::from).collect(),
        };

        Ok(HubSessionsPage {
            snapshot_id: Some(manifest.publication_id),
            hub_generation: manifest.hub_generation,
            total,
            offset,
            limit,
            sessions: rows,
        })
    }

    /// Hydrate a full [`Session`] IR for `(producer_id, session_id)` directly from the mirrored
    /// Zstd-Seekable `packs/` recorded in `snapshot_id` (or the latest published snapshot).
    ///
    /// Never reads live mutable transcripts; if the producer shipped metadata without a pack
    /// payload, returns [`HydratedHubSession::PayloadUnavailable`] with `reason = "pack_not_mirrored"`.
    pub fn hydrate_session(
        &self,
        producer_id: &str,
        session_id: &str,
        snapshot_id: Option<&str>,
    ) -> Result<(SnapshotPublicationManifest, HydratedHubSession), HubError> {
        let manifest = self
            .get_snapshot(snapshot_id)?
            .ok_or_else(|| HubError::NotFound("no hub snapshot published yet".into()))?;
        let key = encode_pair_key(producer_id, session_id);
        let mut matching = manifest.session_revisions.values().filter(|entry| {
            entry.producer_id == producer_id && entry.metadata["native_id"].as_str() == Some(session_id)
        });
        let entry = manifest
            .session_revisions
            .get(&key)
            .or_else(|| {
                let first = matching.next()?;
                matching.next().is_none().then_some(first)
            })
            .cloned()
            .ok_or_else(|| {
                HubError::NotFound(format!(
                    "session `{producer_id}/{session_id}` not found in snapshot `{}`",
                    manifest.publication_id
                ))
            })?;

        let Some(ref addr) = entry.payload_addr else {
            let unavailable = HydratedHubSession::PayloadUnavailable {
                snapshot_id: manifest.publication_id.clone(),
                hub_generation: manifest.hub_generation,
                reason: "pack_not_mirrored",
                entry,
            };
            return Ok((manifest, unavailable));
        };

        // Whole-pack integrity was verified at custody acceptance. Hydration verifies the
        // selected record against the digest pinned in this snapshot's PayloadAddr, decoding
        // only intersecting frames rather than hashing the entire immutable pack on every read.
        let pack = SealedPack::open(&self.packs_dir, &addr.pack_id)
            .map_err(|e| HubError::Pack(format!("opening mirrored pack `{}`: {e:#}", addr.pack_id)))?;
        let payload_bytes = pack
            .read_payload(addr)
            .map_err(|e| HubError::Pack(format!("reading payload from pack `{}`: {e:#}", addr.pack_id)))?;
        let session = decode_packed_session(&payload_bytes)?;

        let hydrated = HydratedHubSession::Hydrated {
            snapshot_id: manifest.publication_id.clone(),
            hub_generation: manifest.hub_generation,
            entry,
            session: Box::new(session),
        };
        Ok((manifest, hydrated))
    }

    /// Return active quarantine records.
    pub fn quarantine_summary(&self) -> Result<QuarantineSummary, HubError> {
        let Some(conn) = self.store.open_readonly()? else {
            return Ok(QuarantineSummary {
                count: 0,
                entries: Vec::new(),
            });
        };
        read_quarantine_in_conn(&conn)
    }

    /// Resolve a quarantine conflict entry via operator choice (`accept_existing` or `accept_incoming`).
    pub fn resolve_quarantine(
        &self,
        quarantine_id: i64,
        resolution: QuarantineResolution,
    ) -> Result<QuarantineResolveResponse, HubError> {
        if crate::env::frozen() {
            return Err(HubError::State(crate::state::StateError::Frozen));
        }
        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = Utc::now().timestamp_millis();

        type QuarRow = (String, String, u64, String, String, String, Option<String>, Option<i64>);
        let row: Option<QuarRow> = tx
            .query_row(
                "SELECT producer_id, epoch, seq, op_id, conflicting_digest,
                        conflicting_envelope, conflicting_payload, resolved_at
                 FROM hub_quarantine
                 WHERE quarantine_id = ?1",
                [quarantine_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get::<_, i64>(2)? as u64,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            producer_id,
            epoch,
            seq,
            op_id,
            conflicting_digest,
            conflicting_envelope,
            conflicting_payload,
            resolved_at,
        )) = row
        else {
            return Err(HubError::NotFound(format!(
                "quarantine entry #{quarantine_id} not found"
            )));
        };
        if resolved_at.is_some() {
            let snapshot = latest_snapshot_in_conn(&tx)?;
            tx.commit()?;
            return Ok(QuarantineResolveResponse {
                quarantine_id,
                resolution,
                snapshot,
            });
        }

        let mut head_deltas = Vec::new();
        match resolution {
            QuarantineResolution::AcceptExisting => {
                tx.execute(
                    "UPDATE hub_quarantine
                     SET resolved_at = ?2, resolution = ?3
                     WHERE quarantine_id = ?1",
                    rusqlite::params![quarantine_id, now_ms, resolution.as_str()],
                )?;
                let (fold_after, new_frontier) = advance_frontier_in_tx(&tx, &producer_id, &epoch, now_ms)?;
                let touched = fold_origin_range_in_tx(&tx, &producer_id, &epoch, fold_after, new_frontier)?;
                if !touched.is_empty() {
                    head_deltas.extend(resolve_producer_sessions_in_tx(&tx, &producer_id, &touched)?);
                }
            }
            QuarantineResolution::AcceptIncoming => {
                if let Some(ref addr_json) = conflicting_payload {
                    let addr: PayloadAddr = serde_json::from_str(addr_json)?;
                    let manifest_str: Option<String> = tx
                        .query_row(
                            "SELECT manifest_json FROM hub_packs WHERE pack_id = ?1",
                            [&addr.pack_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let Some(manifest_str) = manifest_str else {
                        return Err(HubError::MissingPackDependency {
                            pack_id: addr.pack_id.clone(),
                            detail: "quarantined incoming payload references unmirrored pack".into(),
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
                    let span_matches = manifest.record_index.records().iter().any(|span| {
                        span.decoded_offset == addr.decoded_offset
                            && span.length == addr.length
                            && span.blake3_digest == addr.blake3_digest
                    });
                    if !span_matches {
                        return Err(HubError::MissingPackDependency {
                            pack_id: addr.pack_id,
                            detail: "quarantined PayloadAddr span not present in pack manifest".into(),
                        });
                    }
                }
                tx.execute(
                    "INSERT INTO hub_ingested_events(
                         producer_id, epoch, seq, op_id, event_digest,
                         envelope_json, payload_addr_json, ingested_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(producer_id, epoch, seq) DO UPDATE SET
                         op_id = excluded.op_id,
                         event_digest = excluded.event_digest,
                         envelope_json = excluded.envelope_json,
                         payload_addr_json = excluded.payload_addr_json,
                         ingested_at = excluded.ingested_at",
                    rusqlite::params![
                        producer_id,
                        epoch,
                        seq as i64,
                        op_id,
                        conflicting_digest,
                        conflicting_envelope,
                        conflicting_payload,
                        now_ms,
                    ],
                )?;
                tx.execute(
                    "UPDATE hub_quarantine
                     SET resolved_at = ?2, resolution = ?3
                     WHERE quarantine_id = ?1",
                    rusqlite::params![quarantine_id, now_ms, resolution.as_str()],
                )?;
                recompute_frontier_in_tx(&tx, &producer_id, &epoch, now_ms)?;
                head_deltas.extend(recompute_producer_sessions_in_tx(&tx, &producer_id)?);
            }
        }

        let gen_rec = publish_generation_delta_in_tx(&tx, now_ms, &head_deltas)?;
        let snapshot = match gen_rec {
            Some(ref g) => Some(materialize_snapshot_in_conn(&tx, g)?),
            None => None,
        };
        tx.commit()?;
        Ok(QuarantineResolveResponse {
            quarantine_id,
            resolution,
            snapshot,
        })
    }

    /// Pin or unpin a published generation (`publication_id`) so retention pruning never evicts it.
    pub fn set_snapshot_pinned(&self, publication_id: &str, pinned: bool) -> Result<bool, HubError> {
        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE hub_generations SET pinned = ?1 WHERE publication_id = ?2",
            rusqlite::params![i64::from(pinned), publication_id],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Wipe mutable hub projections (`hub_frontiers`, `hub_sessions`, `hub_epoch_sessions`,
    /// `hub_frontier_packs`) while preserving raw inputs and published generation intervals.
    pub fn wipe_derived_views(&self) -> Result<(), HubError> {
        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "DELETE FROM hub_sessions;
             DELETE FROM hub_epoch_sessions;
             DELETE FROM hub_frontier_packs;
             DELETE FROM hub_frontiers;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reconstruct `hub_packs` (reconciling `packs/` on disk), `hub_frontiers`, `hub_sessions`,
    /// `hub_session_heads`, and the latest [`SnapshotPublicationManifest`] strictly from raw
    /// `hub_ingested_events`, `hub_quarantine`, and mirrored `packs/`.
    pub fn rebuild_from_raw(&self) -> Result<Option<SnapshotPublicationManifest>, HubError> {
        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = Utc::now().timestamp_millis();

        if self.packs_dir.is_dir() {
            for entry in std::fs::read_dir(&self.packs_dir)? {
                let entry = entry?;
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                let Some(pack_id) = name.strip_suffix(".pack.idx.json") else {
                    continue;
                };
                if pack_id.starts_with(".cv-") {
                    continue;
                }
                let zst_path = pack_zst_path(&self.packs_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                let idx_path = pack_idx_path(&self.packs_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                if !zst_path.is_file() || !idx_path.is_file() {
                    continue;
                }
                let idx_bytes = std::fs::read(&idx_path)?;
                let zst_bytes = std::fs::read(&zst_path)?;
                let manifest =
                    PackManifest::from_json_slice(&idx_bytes).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                let sealed = SealedPack::from_slices(zst_bytes.clone(), &idx_bytes)
                    .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                for span in manifest.record_index.records() {
                    let addr = span.clone().into_addr(&manifest.pack_id);
                    sealed
                        .read_payload(&addr)
                        .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                }
                let zst_digest = blake3::hash(&zst_bytes).to_hex().to_string();
                let idx_digest = blake3::hash(&idx_bytes).to_hex().to_string();
                let manifest_str = String::from_utf8(idx_bytes)
                    .map_err(|e| HubError::Pack(format!("pack manifest UTF-8 error: {e}")))?;
                tx.execute(
                    "INSERT INTO hub_packs(
                         pack_id, producer_id, byte_len, total_decoded_len, frame_count,
                         record_count, blake3_digest, idx_blake3_digest, pack_rel_path,
                         idx_rel_path, manifest_json, received_at
                     ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT(pack_id) DO NOTHING",
                    rusqlite::params![
                        pack_id,
                        zst_bytes.len() as i64,
                        manifest.total_decoded_len as i64,
                        manifest.frame_count as i64,
                        manifest.record_index.len() as i64,
                        zst_digest,
                        idx_digest,
                        format!("{pack_id}.pack.zst"),
                        format!("{pack_id}.pack.idx.json"),
                        manifest_str,
                        now_ms,
                    ],
                )?;
            }
        }

        tx.execute_batch(
            "DELETE FROM hub_sessions;
             DELETE FROM hub_epoch_sessions;
             DELETE FROM hub_frontier_packs;
             DELETE FROM hub_frontiers;",
        )?;

        let origins: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT producer_id, epoch FROM hub_ingested_events
                 UNION
                 SELECT producer_id, epoch FROM hub_quarantine
                 ORDER BY 1 ASC, 2 ASC",
            )?;
            let mapped = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let mut producers = BTreeSet::new();
        for (producer_id, epoch) in &origins {
            recompute_frontier_in_tx(&tx, producer_id, epoch, now_ms)?;
            producers.insert(producer_id.clone());
        }
        let mut head_deltas = Vec::new();
        for producer_id in &producers {
            tx.execute(
                "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
                 VALUES (?1, ?1, '{}', ?2, ?2)
                 ON CONFLICT(producer_id) DO NOTHING",
                rusqlite::params![producer_id, now_ms],
            )?;
            head_deltas.extend(recompute_producer_sessions_in_tx(&tx, producer_id)?);
        }

        let gen_rec = publish_generation_delta_in_tx(&tx, now_ms, &head_deltas)?;
        let rebuilt = match gen_rec {
            Some(ref g) => Some(materialize_snapshot_in_conn(&tx, g)?),
            None => None,
        };
        tx.commit()?;
        Ok(rebuilt)
    }
}
