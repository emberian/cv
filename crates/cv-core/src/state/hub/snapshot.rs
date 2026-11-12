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
use std::collections::BTreeMap;
use std::path::PathBuf;

fn is_empty_object(v: &Value) -> bool {
    matches!(v, Value::Null) || v.as_object().is_some_and(|m| m.is_empty())
}

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Revision-addressed session entry inside a [`SnapshotPublicationManifest`].
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
    #[serde(default = "empty_json_object", skip_serializing_if = "is_empty_object")]
    pub metadata: Value,
}

/// Immutable, monotonically numbered fleet read-consistency manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPublicationManifest {
    pub hub_generation: u64,
    pub publication_id: String,
    pub published_at: i64,
    #[serde(with = "super::origin_frontier_serde")]
    pub origin_frontier: BTreeMap<(String, String), u64>,
    pub pack_digests: BTreeMap<String, String>,
    pub session_revisions: BTreeMap<String, SessionRevisionEntry>,
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
            id: entry.session_id.clone(),
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

/// Compute deterministic `publication_id` from a snapshot's canonical content.
pub(crate) fn compute_publication_id(
    origin_frontier: &BTreeMap<(String, String), u64>,
    pack_digests: &BTreeMap<String, String>,
    session_revisions: &BTreeMap<String, SessionRevisionEntry>,
    quarantine_count: u64,
) -> Result<String, HubError> {
    let frontier_encoded: BTreeMap<String, u64> = origin_frontier
        .iter()
        .map(|((p, e), &seq)| (encode_pair_key(p, e), seq))
        .collect();
    let doc = json!({
        "domain": "cv-hub-snapshot-v1",
        "origin_frontier": frontier_encoded,
        "pack_digests": pack_digests,
        "session_revisions": session_revisions,
        "quarantine_count": quarantine_count,
    });
    let canonical = canonical_json_bytes(&doc)?;
    let hex = blake3::hash(&canonical).to_hex();
    Ok(format!("pub-{}", &hex.as_str()[..24]))
}

/// Recompute the contiguous origin frontier `F(p, e)` for `(producer_id, epoch)` inside `tx`.
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
            "SELECT MIN(seq) FROM hub_quarantine WHERE producer_id = ?1 AND epoch = ?2",
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
             buffered_count, first_conflict_seq, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(producer_id, epoch) DO UPDATE SET
             contiguous_seq = excluded.contiguous_seq,
             max_received_seq = excluded.max_received_seq,
             buffered_count = excluded.buffered_count,
             first_conflict_seq = excluded.first_conflict_seq,
             updated_at = excluded.updated_at",
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
                    session_id: sid.to_string(),
                };
            }
            return ParsedEnvelopeOp::Ignore;
        }
        Some("session_upsert") => {}
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

    let metadata = envelope
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

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
        metadata,
    }))
}

/// Deterministically fold all conflict-free frontier events (`seq <= F(p, e)`) for `producer_id`
/// into `hub_sessions`.
///
/// Order-independent: events are ordered canonically by `(epoch, seq)` within each epoch and across
/// epochs by `(updated_at_ms, epoch, seq)`, so out-of-order batch arrival and full rebuilds from
/// `hub_ingested_events` produce bit-for-bit identical rows.
pub(crate) fn recompute_producer_sessions_in_tx(tx: &Transaction<'_>, producer_id: &str) -> Result<(), HubError> {
    tx.execute("DELETE FROM hub_sessions WHERE producer_id = ?1", [producer_id])?;

    let mut stmt = tx.prepare(
        "SELECT e.epoch, e.seq, e.event_digest, e.envelope_json, e.payload_addr_json
         FROM hub_ingested_events e
         JOIN hub_frontiers f
           ON f.producer_id = e.producer_id AND f.epoch = e.epoch
         WHERE e.producer_id = ?1 AND e.seq <= f.contiguous_seq
         ORDER BY e.epoch ASC, e.seq ASC",
    )?;

    struct Candidate {
        epoch: String,
        seq: u64,
        sort_ts_ms: i64,
        op: ParsedEnvelopeOp,
    }

    let mut candidates = Vec::new();
    let rows = stmt.query_map([producer_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? as u64,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;

    for row in rows {
        let (epoch, seq, event_digest, env_str, payload_str) = row?;
        let envelope: Value = serde_json::from_str(&env_str)?;
        let payload_addr: Option<PayloadAddr> = match payload_str {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        };
        let op = parse_envelope_op(producer_id, &epoch, seq, &event_digest, &envelope, payload_addr);
        let sort_ts_ms = match &op {
            ParsedEnvelopeOp::Upsert(entry) => {
                parse_rfc3339_ms(entry.updated_at.as_deref().or(entry.created_at.as_deref())).unwrap_or(i64::MIN)
            }
            ParsedEnvelopeOp::Delete { .. } => {
                parse_rfc3339_ms(envelope.get("updated_at").and_then(Value::as_str)).unwrap_or(i64::MAX)
            }
            ParsedEnvelopeOp::Ignore => continue,
        };
        candidates.push(Candidate {
            epoch,
            seq,
            sort_ts_ms,
            op,
        });
    }

    // Within a single `(producer_id, epoch)`, `seq` is strictly causal. Across distinct epochs of
    // the same producer, sort by `(sort_ts_ms, epoch, seq)` so newer epochs supersede older ones
    // deterministically regardless of network arrival order.
    let multi_epoch = {
        let first_ep = candidates.first().map(|c| c.epoch.as_str());
        candidates.iter().any(|c| Some(c.epoch.as_str()) != first_ep)
    };
    if multi_epoch {
        candidates.sort_by(|a, b| {
            if a.epoch == b.epoch {
                a.seq.cmp(&b.seq)
            } else {
                (a.sort_ts_ms, &a.epoch, a.seq).cmp(&(b.sort_ts_ms, &b.epoch, b.seq))
            }
        });
    }

    let mut folded: BTreeMap<String, SessionRevisionEntry> = BTreeMap::new();
    for cand in candidates {
        match cand.op {
            ParsedEnvelopeOp::Ignore => {}
            ParsedEnvelopeOp::Delete { session_id } => {
                folded.remove(&session_id);
            }
            ParsedEnvelopeOp::Upsert(entry) => {
                folded.insert(entry.session_id.clone(), *entry);
            }
        }
    }

    for entry in folded.values() {
        let payload_json = match &entry.payload_addr {
            Some(addr) => Some(serde_json::to_string(addr)?),
            None => None,
        };
        let meta_json = serde_json::to_string(&entry.metadata)?;
        tx.execute(
            "INSERT INTO hub_sessions(
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
    }

    Ok(())
}

/// Build the current coherent [`SnapshotPublicationManifest`] from `tx` and publish a new
/// monotonic `hub_generation` row into `hub_snapshots` iff the coherent state changed since the
/// latest published snapshot (or if no snapshot has been published yet and the hub has frontier or
/// quarantine state).
pub(crate) fn maybe_publish_snapshot_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
) -> Result<Option<SnapshotPublicationManifest>, HubError> {
    let mut origin_frontier: BTreeMap<(String, String), u64> = BTreeMap::new();
    {
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
            origin_frontier.insert((p, e), seq);
        }
    }

    let quarantine_count: u64 = tx.query_row("SELECT COUNT(*) FROM hub_quarantine", [], |r| r.get::<_, i64>(0))? as u64;

    if origin_frontier.is_empty() && quarantine_count == 0 {
        return latest_snapshot_in_conn(tx);
    }

    // Collect every pack referenced by conflict-free frontier events (`seq <= contiguous_seq`).
    let mut referenced_packs = std::collections::BTreeSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT e.payload_addr_json
             FROM hub_ingested_events e
             JOIN hub_frontiers f
               ON f.producer_id = e.producer_id AND f.epoch = e.epoch
             WHERE e.seq <= f.contiguous_seq AND e.payload_addr_json IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let raw = row?;
            let addr: PayloadAddr = serde_json::from_str(&raw)?;
            referenced_packs.insert(addr.pack_id);
        }
    }

    let mut pack_digests: BTreeMap<String, String> = BTreeMap::new();
    for pack_id in referenced_packs {
        if let Some(digest) = tx
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

    let mut session_revisions: BTreeMap<String, SessionRevisionEntry> = BTreeMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT producer_id, session_id, harness, revision, title, cwd, model,
                    created_at, updated_at, message_count, epoch, event_seq,
                    payload_addr_json, metadata_json
             FROM hub_sessions
             ORDER BY producer_id ASC, session_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let payload_str: Option<String> = r.get(12)?;
            let meta_str: String = r.get(13)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, i64>(9)? as u64,
                r.get::<_, String>(10)?,
                r.get::<_, i64>(11)? as u64,
                payload_str,
                meta_str,
            ))
        })?;
        for row in rows {
            let (
                producer_id,
                session_id,
                harness,
                revision,
                title,
                cwd,
                model,
                created_at,
                updated_at,
                message_count,
                epoch,
                event_seq,
                payload_str,
                meta_str,
            ) = row?;
            let payload_addr = match payload_str {
                Some(s) => Some(serde_json::from_str(&s)?),
                None => None,
            };
            let metadata = serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({}));
            let key = encode_pair_key(&producer_id, &session_id);
            session_revisions.insert(
                key,
                SessionRevisionEntry {
                    producer_id,
                    session_id,
                    harness,
                    revision,
                    title,
                    cwd,
                    model,
                    created_at,
                    updated_at,
                    message_count,
                    epoch,
                    event_seq,
                    payload_addr,
                    metadata,
                },
            );
        }
    }

    let publication_id = compute_publication_id(&origin_frontier, &pack_digests, &session_revisions, quarantine_count)?;

    if let Some(prev) = latest_snapshot_in_conn(tx)? {
        if prev.publication_id == publication_id {
            return Ok(Some(prev));
        }
    }

    // If an earlier generation already published this exact content-addressed `publication_id`
    // (for example before and after a revert/delete), disambiguate with the generation suffix so
    // `hub_snapshots.publication_id` stays globally unique while single-pass and rebuilt latest
    // snapshots remain identical.
    let existing_same_id: Option<i64> = tx
        .query_row(
            "SELECT hub_generation FROM hub_snapshots WHERE publication_id = ?1",
            [&publication_id],
            |r| r.get(0),
        )
        .optional()?;

    let next_gen: u64 = tx.query_row(
        "SELECT COALESCE(MAX(hub_generation), 0) + 1 FROM hub_snapshots",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;

    let final_pub_id = if existing_same_id.is_some() {
        format!("{publication_id}-g{next_gen}")
    } else {
        publication_id
    };

    let manifest = SnapshotPublicationManifest {
        hub_generation: next_gen,
        publication_id: final_pub_id.clone(),
        published_at: now_ms,
        origin_frontier,
        pack_digests,
        session_revisions,
        quarantine_count,
    };
    let manifest_json = serde_json::to_string(&manifest)?;
    tx.execute(
        "INSERT INTO hub_snapshots(hub_generation, publication_id, published_at, manifest_json)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![next_gen as i64, final_pub_id, now_ms, manifest_json],
    )?;

    Ok(Some(manifest))
}

pub(crate) fn latest_snapshot_in_conn(conn: &Connection) -> Result<Option<SnapshotPublicationManifest>, HubError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_snapshots')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(None);
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT manifest_json FROM hub_snapshots ORDER BY hub_generation DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
        None => Ok(None),
    }
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
    let count: u64 = conn.query_row("SELECT COUNT(*) FROM hub_quarantine", [], |r| r.get::<_, i64>(0))? as u64;
    let mut stmt = conn.prepare(
        "SELECT quarantine_id, producer_id, epoch, seq, op_id,
                existing_digest, conflicting_digest, quarantined_at
         FROM hub_quarantine
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

fn decode_packed_session(bytes: &[u8]) -> Result<Session, HubError> {
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
                    Block::Text { text } | Block::Thinking { text, .. } => {
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
                    Block::Image { .. } | Block::File { .. } => {}
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
    /// Read-only fast lookup of the latest published `publication_id`, returning `Ok(None)` when
    /// `state.db` does not exist or has no hub snapshots yet (never creates directories or files).
    pub fn latest_publication_id_fast(&self) -> Result<Option<String>, HubError> {
        let Some(conn) = self.store.open_readonly()? else {
            return Ok(None);
        };
        let has_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_snapshots')",
            [],
            |r| r.get(0),
        )?;
        if !has_table {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT publication_id FROM hub_snapshots ORDER BY hub_generation DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Return the latest published [`SnapshotPublicationManifest`], if any.
    pub fn latest_snapshot(&self) -> Result<Option<SnapshotPublicationManifest>, HubError> {
        let Some(conn) = self.store.open_readonly()? else {
            return Ok(None);
        };
        latest_snapshot_in_conn(&conn)
    }

    /// Look up a [`SnapshotPublicationManifest`] by `publication_id`, or the latest published
    /// snapshot when `id` is `None`.
    pub fn get_snapshot(&self, id: Option<&str>) -> Result<Option<SnapshotPublicationManifest>, HubError> {
        let Some(conn) = self.store.open_readonly()? else {
            return match id {
                Some(want) => Err(HubError::NotFound(format!("snapshot `{want}` not found"))),
                None => Ok(None),
            };
        };
        let has_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_snapshots')",
            [],
            |r| r.get(0),
        )?;
        if !has_table {
            return match id {
                Some(want) => Err(HubError::NotFound(format!("snapshot `{want}` not found"))),
                None => Ok(None),
            };
        }
        match id {
            Some(want) => {
                let raw: Option<String> = conn
                    .query_row(
                        "SELECT manifest_json FROM hub_snapshots WHERE publication_id = ?1",
                        [want],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(json_str) = raw else {
                    return Err(HubError::NotFound(format!("snapshot `{want}` not found")));
                };
                Ok(Some(serde_json::from_str(&json_str)?))
            }
            None => latest_snapshot_in_conn(&conn),
        }
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
        let entry = manifest.session_revisions.get(&key).cloned().ok_or_else(|| {
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

        // Verify the pack whole-file digest matches what `SnapshotPublicationManifest` pinned.
        if let Some(expected_pack_digest) = manifest.pack_digests.get(&addr.pack_id) {
            let zst_path =
                pack_zst_path(&self.packs_dir, &addr.pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            let zst_bytes = std::fs::read(&zst_path).map_err(|e| {
                HubError::Pack(format!(
                    "reading mirrored pack {} for hydration: {e}",
                    zst_path.display()
                ))
            })?;
            let actual_pack_digest = blake3::hash(&zst_bytes).to_hex().to_string();
            if &actual_pack_digest != expected_pack_digest {
                return Err(HubError::Pack(format!(
                    "mirrored pack `{}` digest mismatch against snapshot `{}`: expected {expected_pack_digest}, got {actual_pack_digest}",
                    addr.pack_id, manifest.publication_id
                )));
            }
        }

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

    /// Wipe all derived hub views (`hub_frontiers`, `hub_sessions`, `hub_snapshots`) while leaving
    /// raw authoritative inputs (`hub_ingested_events`, `hub_quarantine`, `hub_packs`, `packs/`)
    /// untouched — used by rebuild recovery and the `Rebuild Parity Invariant` gate.
    pub fn wipe_derived_views(&self) -> Result<(), HubError> {
        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "DELETE FROM hub_snapshots;
             DELETE FROM hub_sessions;
             DELETE FROM hub_frontiers;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reconstruct `hub_packs` (reconciling `packs/` on disk), `hub_frontiers`, `hub_sessions`,
    /// and the latest [`SnapshotPublicationManifest`] strictly from raw `hub_ingested_events`,
    /// `hub_quarantine`, and mirrored `packs/`.
    pub fn rebuild_from_raw(&self) -> Result<Option<SnapshotPublicationManifest>, HubError> {
        let mut conn = self.store.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = Utc::now().timestamp_millis();

        // Reconcile `hub_packs` from sealed `.pack.idx.json` + `.pack.zst` files in `self.packs_dir`
        // so rebuild works even if `hub_packs` rows were cleared alongside derived tables.
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
            "DELETE FROM hub_snapshots;
             DELETE FROM hub_sessions;
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

        let mut producers = std::collections::BTreeSet::new();
        for (producer_id, epoch) in &origins {
            recompute_frontier_in_tx(&tx, producer_id, epoch, now_ms)?;
            producers.insert(producer_id.clone());
        }
        for producer_id in &producers {
            recompute_producer_sessions_in_tx(&tx, producer_id)?;
        }

        let rebuilt = maybe_publish_snapshot_in_tx(&tx, now_ms)?;
        tx.commit()?;
        Ok(rebuilt)
    }
}
