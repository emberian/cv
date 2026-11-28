//! Durable authoritative domain tables on `state.db`: producers, writer epochs, capture inbox,
//! pack manifests & custody acknowledgements, lane runs & endpoints, hash-chained tool receipts,
//! and session lineage edges.

use super::{recorder::default_host_label, DurableReceipt, EventId, StateError, StateStore};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashSet, VecDeque};

/// Enrolled producer record (`producers` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub producer_id: String,
    pub host_label: String,
    pub metadata: Value,
    pub enrolled_at: i64,
    pub updated_at: i64,
}

/// Exclusive writer activation epoch (`writer_epochs` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterEpoch {
    pub epoch: String,
    pub producer_id: String,
    pub mode: String,
    pub pid: Option<i64>,
    pub proc_start_time: Option<i64>,
    pub activated_at: i64,
    pub closed_at: Option<i64>,
}

/// Unpacked commit record residing in the bounded `capture_inbox` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureInboxRow {
    pub local_commit_pos: i64,
    pub producer_id: String,
    pub epoch: String,
    pub seq: u64,
    pub op_id: String,
    pub envelope: Value,
    pub payload_ref: Option<String>,
    pub committed_at: i64,
}

/// Immutable archive pack manifest (`pack_manifests` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    pub pack_id: String,
    pub min_commit_pos: i64,
    pub max_commit_pos: i64,
    pub record_count: u64,
    pub byte_len: u64,
    pub digest: String,
    pub pack_path: String,
    pub index_meta: Value,
    pub published_at: i64,
}

/// Remote or vault custody watermark (`custody_acks` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyAck {
    pub target_hub: String,
    pub producer_id: String,
    pub epoch: String,
    pub contiguous_seq: u64,
    pub max_commit_pos: Option<i64>,
    pub pack_id: Option<String>,
    pub acked_at: i64,
}

/// Indexed local replication outbox commit (`custody_commits` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyCommitRow {
    pub local_commit_pos: i64,
    pub producer_id: String,
    pub epoch: String,
    pub seq: u64,
    pub op_id: String,
    pub envelope: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_addr: Option<crate::packs::PayloadAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_extents: Option<Vec<crate::packs::PayloadAddr>>,
    pub committed_at: i64,
}

/// Per-target outbound sequence cursor (`ship_cursors` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipCursorRow {
    pub target_name: String,
    pub producer_id: String,
    pub epoch: String,
    pub contiguous_seq: u64,
    pub updated_at: i64,
}

/// Durable sub-agent / lane process lifecycle record (`lane_runs` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRun {
    pub run_id: String,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub harness: Option<String>,
    pub pid: Option<i64>,
    pub proc_start_time: Option<i64>,
    pub boot_id: Option<String>,
    pub transcript_path: Option<String>,
    pub agent_transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub started_at: i64,
    pub exited_at: Option<i64>,
    pub exit_status: Option<String>,
    pub last_text: Option<String>,
}

/// Binding between a live or historical lane run and a logical task/inbox endpoint (`lane_endpoints`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneEndpointBinding {
    pub run_id: String,
    pub endpoint: String,
    pub bound_at: i64,
}

/// Durable hash-chained tool receipt (`receipts` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRecord {
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub harness: String,
    pub agent_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    pub kind: String,
    pub digest_sha256: String,
    pub prev_hash: String,
    pub receipt_hash: String,
    pub body: Value,
}

/// First-class cross-session lineage edge (`lineage_edges` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageEdge {
    pub src_session_id: String,
    pub dst_session_id: String,
    pub edge_kind: String,
    pub harness: Option<String>,
    pub tool_use_id: Option<String>,
    pub metadata: Value,
    pub created_at: i64,
}

/// Domain prefix for `cv-receipt-v1` canonical SHA-256 chain hashes.
pub const RECEIPT_DOMAIN: &str = "cv-receipt-v1";

/// A single offered command and the observed extent of its five-turn window.
pub struct Whisper {
    pub session_id: String,
    pub kind: String,
    pub subject_key: String,
    pub subject_rev: String,
    pub command: String,
    pub emitted_at: i64,
    pub followed_up_at: Option<i64>,
    pub unfollowed: bool,
}

impl StateStore {
    #[cfg(unix)]
    pub(super) fn offer_whisper(
        &self,
        harness: &str,
        session: &str,
        kind: &str,
        subject: &str,
        revision: &str,
        command: &str,
    ) -> Result<bool, StateError> {
        // Keep the dedup keys for the recipient's lifetime. On saturation stop
        // offering instead of evicting a key that could cause repeat delivery.
        Ok(self.open_writable_core()?.execute(
            "INSERT INTO whispers(session_id,kind,subject_key,subject_rev,command,status,harness)
             SELECT ?1,?2,?3,?4,?5,'offered',?6
             WHERE (SELECT COUNT(*) FROM whispers WHERE session_id=?1) < 256
             ON CONFLICT DO NOTHING",
            rusqlite::params![session, kind, subject, revision, command, harness],
        )? == 1)
    }

    #[cfg(unix)]
    pub(super) fn settle_offer(
        &self,
        session: &str,
        kind: &str,
        subject: &str,
        revision: &str,
        emitted_at: Option<i64>,
    ) -> Result<(), StateError> {
        let conn = self.open_writable_core()?;
        if let Some(ts) = emitted_at {
            conn.execute(
                "UPDATE whispers SET status='emitted',emitted_at=?5
                WHERE session_id=?1 AND kind=?2 AND subject_key=?3 AND subject_rev=?4 AND status='offered'",
                rusqlite::params![session, kind, subject, revision, ts],
            )?;
        } else {
            conn.execute("DELETE FROM whispers WHERE session_id=?1 AND kind=?2 AND subject_key=?3 AND subject_rev=?4 AND status='offered'",
                [session,kind,subject,revision])?;
        }
        Ok(())
    }

    pub fn session_whispers(&self, harness: &str, session: &str) -> Result<Vec<Whisper>, StateError> {
        Ok(self
            .read_in(|conn| {
                if super::current_schema_version(conn)? < 4 {
                    return Ok(Vec::new());
                }
                let mut stmt = conn.prepare(
                    "SELECT kind, subject_key, subject_rev, command, emitted_at, followed_up_at, status
                    FROM whispers WHERE session_id=?1 AND harness=?2 AND emitted_at IS NOT NULL ORDER BY emitted_at",
                )?;
                let rows = stmt.query_map([session, harness], |r| {
                    Ok(Whisper {
                        session_id: session.to_owned(),
                        kind: r.get(0)?,
                        subject_key: r.get(1)?,
                        subject_rev: r.get(2)?,
                        command: r.get(3)?,
                        emitted_at: r.get(4)?,
                        followed_up_at: r.get(5)?,
                        unfollowed: r.get::<_, String>(6)? == "unfollowed",
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })?
            .unwrap_or_default())
    }

    pub fn finish_whisper_window(&self, w: &Whisper) -> Result<(), StateError> {
        self.open_writable_core()?.execute(
            "UPDATE whispers SET status=?5, followed_up_at=?6
            WHERE session_id=?1 AND kind=?2 AND subject_key=?3 AND subject_rev=?4 AND status='emitted'",
            rusqlite::params![
                w.session_id,
                w.kind,
                w.subject_key,
                w.subject_rev,
                if w.followed_up_at.is_some() {
                    "followed_up"
                } else {
                    "unfollowed"
                },
                w.followed_up_at
            ],
        )?;
        Ok(())
    }

    pub fn session_baggage(&self, harness: &str, session: &str) -> Result<Option<crate::hooks::Baggage>, StateError> {
        Ok(self
            .read_in(|conn| {
                if super::current_schema_version(conn)? < 4 {
                    return Ok(None);
                }
                let raw: Option<String> = conn
                    .query_row(
                        "SELECT baggage FROM session_baggage WHERE harness=?1 AND session_id=?2",
                        [harness, session],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(raw.as_deref().and_then(crate::hooks::Baggage::parse))
            })?
            .flatten())
    }
}

pub(super) fn record_baggage(
    conn: &rusqlite::Connection,
    harness: &str,
    session: &str,
    raw: &str,
) -> Result<(), StateError> {
    if crate::hooks::Baggage::parse(raw).is_some() && !session.is_empty() {
        conn.execute(
            "INSERT INTO session_baggage(harness,session_id,baggage) VALUES(?1,?2,?3)
            ON CONFLICT(harness,session_id) DO NOTHING",
            [harness, session, raw],
        )?;
    }
    Ok(())
}

impl StateStore {
    // ───────────────────────── Producers & Writer Epochs ─────────────────────────

    /// Enroll or update a producer identity in `producers`.
    pub fn enroll_producer(
        &self,
        producer_id: &str,
        host_label: &str,
        metadata: &Value,
    ) -> Result<Producer, StateError> {
        if producer_id.trim().is_empty() {
            return Err(StateError::InvalidArgument("producer_id must not be empty".into()));
        }
        let host = if host_label.trim().is_empty() {
            default_host_label()
        } else {
            host_label.to_string()
        };
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let meta_json = serde_json::to_string(metadata)?;
        tx.execute(
            "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(producer_id) DO UPDATE SET
                 host_label = excluded.host_label,
                 metadata_json = excluded.metadata_json,
                 updated_at = excluded.updated_at",
            rusqlite::params![producer_id, host, meta_json, now_ms],
        )?;
        let row = tx.query_row(
            "SELECT producer_id, host_label, metadata_json, enrolled_at, updated_at
             FROM producers WHERE producer_id = ?1",
            [producer_id],
            row_to_producer,
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Look up an enrolled producer by `producer_id`.
    pub fn get_producer(&self, producer_id: &str) -> Result<Option<Producer>, StateError> {
        Ok(self
            .read_in(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT producer_id, host_label, metadata_json, enrolled_at, updated_at
                         FROM producers WHERE producer_id = ?1",
                        [producer_id],
                        row_to_producer,
                    )
                    .optional()?)
            })?
            .flatten())
    }

    /// List all enrolled producers ordered by `enrolled_at`.
    pub fn list_producers(&self) -> Result<Vec<Producer>, StateError> {
        Ok(self.read_in(Self::producers_in)?.unwrap_or_default())
    }

    pub(super) fn producers_in(conn: &rusqlite::Connection) -> Result<Vec<Producer>, StateError> {
        let mut stmt = conn.prepare(
            "SELECT producer_id, host_label, metadata_json, enrolled_at, updated_at
             FROM producers ORDER BY enrolled_at ASC, producer_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_producer)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Allocate a fresh random 128-bit UUIDv4 writer epoch for `producer_id`.
    pub fn activate_epoch(&self, producer_id: &str, mode: &str) -> Result<WriterEpoch, StateError> {
        if producer_id.trim().is_empty() {
            return Err(StateError::InvalidArgument("producer_id must not be empty".into()));
        }
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
             VALUES (?1, ?2, '{}', ?3, ?3)
             ON CONFLICT(producer_id) DO NOTHING",
            rusqlite::params![producer_id, default_host_label(), now_ms],
        )?;
        let epoch = allocate_monotonic_epoch(&tx, producer_id)?;
        let (pid, proc_start_time) = current_process_identity();
        tx.execute(
            "INSERT INTO writer_epochs(epoch, producer_id, mode, pid, proc_start_time, activated_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            rusqlite::params![epoch, producer_id, mode, pid, proc_start_time, now_ms],
        )?;
        tx.commit()?;
        Ok(WriterEpoch {
            epoch,
            producer_id: producer_id.to_string(),
            mode: mode.to_string(),
            pid,
            proc_start_time,
            activated_at: now_ms,
            closed_at: None,
        })
    }

    /// Mark an active writer epoch closed.
    pub fn close_epoch(&self, epoch: &str) -> Result<bool, StateError> {
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let updated = tx.execute(
            "UPDATE writer_epochs SET closed_at = COALESCE(closed_at, ?1) WHERE epoch = ?2",
            rusqlite::params![now_ms, epoch],
        )?;
        tx.commit()?;
        Ok(updated > 0)
    }

    /// Read a single `writer_epochs` record by `epoch`.
    pub fn get_epoch(&self, epoch: &str) -> Result<Option<WriterEpoch>, StateError> {
        Ok(self
            .read_in(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT epoch, producer_id, mode, pid, proc_start_time, activated_at, closed_at
                         FROM writer_epochs WHERE epoch = ?1",
                        [epoch],
                        |r| {
                            Ok(WriterEpoch {
                                epoch: r.get(0)?,
                                producer_id: r.get(1)?,
                                mode: r.get(2)?,
                                pid: r.get(3)?,
                                proc_start_time: r.get(4)?,
                                activated_at: r.get(5)?,
                                closed_at: r.get(6)?,
                            })
                        },
                    )
                    .optional()?)
            })?
            .flatten())
    }

    // ───────────────────────── Operation Bindings & Capture Inbox ─────────────────────────

    /// Look up an idempotency binding by `op_id`.
    pub fn get_operation_binding(&self, op_id: &str) -> Result<Option<(String, DurableReceipt)>, StateError> {
        Ok(self
            .read_in(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT request_digest, producer_id, epoch, seq, local_commit_pos
                         FROM operation_bindings WHERE op_id = ?1",
                        [op_id],
                        row_to_operation_binding,
                    )
                    .optional()?)
            })?
            .flatten())
    }

    /// List durable operation bindings in `[min_pos, max_pos]` ordered by `local_commit_pos`.
    pub fn list_operation_bindings_range(
        &self,
        min_pos: i64,
        max_pos: i64,
    ) -> Result<Vec<(String, DurableReceipt)>, StateError> {
        if min_pos > max_pos {
            return Ok(Vec::new());
        }
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT op_id, producer_id, epoch, seq, local_commit_pos
                     FROM operation_bindings
                     WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2
                     ORDER BY local_commit_pos ASC",
                )?;
                let rows = stmt.query_map(rusqlite::params![min_pos, max_pos], row_to_operation_binding)?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Read retained rows in `capture_inbox` within `[min_pos, max_pos]` ordered by `local_commit_pos`.
    pub fn read_inbox_range(&self, min_pos: i64, max_pos: i64) -> Result<Vec<CaptureInboxRow>, StateError> {
        if min_pos > max_pos {
            return Ok(Vec::new());
        }
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT local_commit_pos, producer_id, epoch, seq, op_id, envelope_json, payload_ref, committed_at
                     FROM capture_inbox
                     WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2
                     ORDER BY local_commit_pos ASC",
                )?;
                let rows = stmt.query_map(rusqlite::params![min_pos, max_pos], |r| {
                    let env_str: String = r.get(5)?;
                    let seq: i64 = r.get(3)?;
                    Ok(CaptureInboxRow {
                        local_commit_pos: r.get(0)?,
                        producer_id: r.get(1)?,
                        epoch: r.get(2)?,
                        seq: seq as u64,
                        op_id: r.get(4)?,
                        envelope: serde_json::from_str(&env_str).unwrap_or(Value::Null),
                        payload_ref: r.get(6)?,
                        committed_at: r.get(7)?,
                    })
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Count unpacked rows currently retained in `capture_inbox`.
    pub fn inbox_count(&self) -> Result<u64, StateError> {
        Ok(self
            .read_in(|conn| {
                let n: i64 = conn.query_row("SELECT COUNT(*) FROM capture_inbox", [], |r| r.get(0))?;
                Ok(n as u64)
            })?
            .unwrap_or(0))
    }

    // ───────────────────────── Pack Manifests & Custody Acks ─────────────────────────

    /// Atomically publish `manifest` in `pack_manifests` and transfer logical ownership of
    /// `[min_commit_pos, max_commit_pos]` out of `capture_inbox` in a single `BEGIN IMMEDIATE`
    /// transaction (`publish-before-reference`).
    ///
    /// Returns the number of `capture_inbox` rows transferred (`0` on idempotent retry of an
    /// already-published identical manifest).
    pub fn publish_pack_range(&self, manifest: &PackManifest) -> Result<usize, StateError> {
        if manifest.pack_id.trim().is_empty() {
            return Err(StateError::InvalidArgument("pack_id must not be empty".into()));
        }
        if manifest.digest.trim().is_empty() {
            return Err(StateError::InvalidArgument("pack digest must not be empty".into()));
        }
        if manifest.min_commit_pos > manifest.max_commit_pos {
            return Err(StateError::PackRangeMismatch(format!(
                "min_commit_pos ({}) > max_commit_pos ({})",
                manifest.min_commit_pos, manifest.max_commit_pos
            )));
        }

        let mut conn = self.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotent retry check for same `pack_id`.
        if let Some((min_p, max_p, rec_c, dig)) = tx
            .query_row(
                "SELECT min_commit_pos, max_commit_pos, record_count, digest
                 FROM pack_manifests WHERE pack_id = ?1",
                [&manifest.pack_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        {
            if min_p == manifest.min_commit_pos
                && max_p == manifest.max_commit_pos
                && rec_c as u64 == manifest.record_count
                && dig == manifest.digest
            {
                tx.commit()?;
                return Ok(0);
            }
            return Err(StateError::PackRangeMismatch(format!(
                "pack_id `{}` already exists with different bounds or digest",
                manifest.pack_id
            )));
        }

        // Refuse overlapping commit ranges with any other published pack.
        let overlap: Option<String> = tx
            .query_row(
                "SELECT pack_id FROM pack_manifests
                 WHERE min_commit_pos <= ?2 AND max_commit_pos >= ?1
                 LIMIT 1",
                rusqlite::params![manifest.min_commit_pos, manifest.max_commit_pos],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other_id) = overlap {
            return Err(StateError::PackRangeMismatch(format!(
                "range [{}..={}] overlaps already-published pack `{other_id}`",
                manifest.min_commit_pos, manifest.max_commit_pos
            )));
        }

        // Verify all claimed records currently reside in `capture_inbox`.
        let in_inbox: i64 = tx.query_row(
            "SELECT COUNT(*) FROM capture_inbox
             WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2",
            rusqlite::params![manifest.min_commit_pos, manifest.max_commit_pos],
            |r| r.get(0),
        )?;
        if in_inbox as u64 != manifest.record_count {
            return Err(StateError::PackRangeMismatch(format!(
                "expected {} inbox rows in [{}..={}], found {}",
                manifest.record_count, manifest.min_commit_pos, manifest.max_commit_pos, in_inbox
            )));
        }

        let heads_were_fresh = custody_heads_fresh(&tx)?;
        let commits_were_fresh = custody_commits_fresh(&tx)?;
        let index_meta_json = serde_json::to_string(&manifest.index_meta)?;
        tx.execute(
            "INSERT INTO pack_manifests(
                 pack_id, min_commit_pos, max_commit_pos, record_count, byte_len,
                 digest, pack_path, index_meta_json, published_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                manifest.pack_id,
                manifest.min_commit_pos,
                manifest.max_commit_pos,
                manifest.record_count as i64,
                manifest.byte_len as i64,
                manifest.digest,
                manifest.pack_path,
                index_meta_json,
                manifest.published_at,
            ],
        )?;

        let deleted = tx.execute(
            "DELETE FROM capture_inbox
             WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2",
            rusqlite::params![manifest.min_commit_pos, manifest.max_commit_pos],
        )?;

        // Heads that folded every manifest before this one take it incrementally; heads another
        // binary left behind are rebuilt from every manifest, this one included.
        if heads_were_fresh {
            upsert_custody_heads(&tx, &manifest.pack_id, manifest.min_commit_pos, &manifest.index_meta)?;
            mark_custody_heads(&tx)?;
        } else {
            rebuild_custody_heads(&tx)?;
        }

        let db_dir = self.db_path.parent();
        if commits_were_fresh {
            upsert_custody_commits(&tx, manifest, db_dir)?;
            mark_custody_commits(&tx)?;
        } else {
            rebuild_custody_commits(&tx, db_dir)?;
        }

        tx.commit()?;
        Ok(deleted)
    }

    /// The newest custody entry of every archived session (`custody_heads`), as stored JSON.
    /// `None` when the table is absent, its watermark no longer matches `pack_manifests` (a
    /// store no writable open has converged yet, or one a binary without the table published
    /// to), or `state.db` carries a newer `user_version`: the caller then reads `pack_manifests`
    /// instead. Read-only and `--frozen` safe; on a checkpointed store without `-wal`/`-journal`
    /// frames, `--frozen` reads `state.db` directly via `immutable=1` without copying to `TMPDIR`.
    pub fn custody_heads(&self) -> Result<Option<Vec<Value>>, StateError> {
        match self.read_compatible_in(|conn| {
            if super::current_schema_version(conn)? > super::STATE_SCHEMA_VERSION {
                return Ok(None);
            }
            read_custody_heads_in(conn)
        })? {
            None => Ok(Some(Vec::new())),
            Some(heads) => Ok(heads),
        }
    }

    /// List all published pack manifests ordered by `min_commit_pos`.
    ///
    /// Uses [`Self::read_compatible_in`] so stores stamped with a newer `user_version` still fall
    /// back to reading `pack_manifests` cleanly when `custody_heads` returns `Ok(None)`.
    pub fn list_pack_manifests(&self) -> Result<Vec<PackManifest>, StateError> {
        Ok(self
            .read_compatible_in(|conn| {
                if !conn.table_exists(None, "pack_manifests")? {
                    return Ok(Vec::new());
                }
                let mut stmt = conn.prepare(&format!(
                    "SELECT {PACK_MANIFEST_COLS}
                     FROM pack_manifests
                     ORDER BY min_commit_pos ASC, pack_id ASC"
                ))?;
                let rows = stmt.query_map([], row_to_pack_manifest)?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Look up a single published pack manifest by `pack_id`.
    pub fn get_pack_manifest(&self, pack_id: &str) -> Result<Option<PackManifest>, StateError> {
        Ok(self
            .read_in(|conn| {
                conn.query_row(
                    &format!("SELECT {PACK_MANIFEST_COLS} FROM pack_manifests WHERE pack_id = ?1"),
                    [pack_id],
                    row_to_pack_manifest,
                )
                .optional()
                .map_err(StateError::from)
            })?
            .flatten())
    }

    /// Record a monotonic custody acknowledgement watermark for `(target_hub, producer_id, epoch)`.
    pub fn record_custody_ack(&self, ack: &CustodyAck) -> Result<(), StateError> {
        if ack.target_hub.trim().is_empty() || ack.producer_id.trim().is_empty() || ack.epoch.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "custody ack target_hub, producer_id, and epoch must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO custody_acks(
                 target_hub, producer_id, epoch, contiguous_seq, max_commit_pos, pack_id, acked_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(target_hub, producer_id, epoch) DO UPDATE SET
                 contiguous_seq = MAX(custody_acks.contiguous_seq, excluded.contiguous_seq),
                 max_commit_pos = CASE
                     WHEN excluded.max_commit_pos IS NULL THEN custody_acks.max_commit_pos
                     WHEN custody_acks.max_commit_pos IS NULL THEN excluded.max_commit_pos
                     ELSE MAX(custody_acks.max_commit_pos, excluded.max_commit_pos)
                 END,
                 pack_id = COALESCE(excluded.pack_id, custody_acks.pack_id),
                 acked_at = MAX(custody_acks.acked_at, excluded.acked_at)",
            rusqlite::params![
                ack.target_hub,
                ack.producer_id,
                ack.epoch,
                ack.contiguous_seq as i64,
                ack.max_commit_pos,
                ack.pack_id,
                ack.acked_at,
            ],
        )?;
        tx.execute(
            "INSERT INTO ship_cursors(target_name, producer_id, epoch, contiguous_seq, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(target_name, producer_id, epoch) DO UPDATE SET
                 contiguous_seq = MAX(ship_cursors.contiguous_seq, excluded.contiguous_seq),
                 updated_at = MAX(ship_cursors.updated_at, excluded.updated_at)",
            rusqlite::params![
                ack.target_hub,
                ack.producer_id,
                ack.epoch,
                ack.contiguous_seq as i64,
                ack.acked_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// List custody acknowledgements recorded for `target_hub`.
    pub fn custody_acks_for(&self, target_hub: &str) -> Result<Vec<CustodyAck>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT target_hub, producer_id, epoch, contiguous_seq, max_commit_pos, pack_id, acked_at
                     FROM custody_acks WHERE target_hub = ?1
                     ORDER BY producer_id ASC, epoch ASC",
                )?;
                let rows = stmt.query_map([target_hub], |r| {
                    Ok(CustodyAck {
                        target_hub: r.get(0)?,
                        producer_id: r.get(1)?,
                        epoch: r.get(2)?,
                        contiguous_seq: r.get::<_, i64>(3)? as u64,
                        max_commit_pos: r.get(4)?,
                        pack_id: r.get(5)?,
                        acked_at: r.get(6)?,
                    })
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Never-delete-before-custody gate: returns `true` iff `pack_id` exists and every hub in
    /// `required_hubs` has acknowledged `pack_id` directly or has a contiguous sequence watermark
    /// covering every `(producer_id, epoch)` bound in `[manifest.min_commit_pos..=manifest.max_commit_pos]`.
    /// If `required_hubs` is empty, local custody alone applies (`true`).
    pub fn is_pack_evictable(&self, pack_id: &str, required_hubs: &[&str]) -> Result<bool, StateError> {
        Ok(self
            .read_in(|conn| {
                let Some((min_pos, max_pos)): Option<(i64, i64)> = conn
                    .query_row(
                        "SELECT min_commit_pos, max_commit_pos FROM pack_manifests WHERE pack_id = ?1",
                        [pack_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?
                else {
                    return Ok(false);
                };
                if required_hubs.is_empty() {
                    return Ok(true);
                }

                let mut stmt = conn.prepare(
                    "SELECT producer_id, epoch, MAX(seq)
                     FROM operation_bindings
                     WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2
                     GROUP BY producer_id, epoch",
                )?;
                let epoch_reqs: Vec<(String, String, i64)> = stmt
                    .query_map(rusqlite::params![min_pos, max_pos], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                for &hub in required_hubs {
                    let direct_pack_acked: bool = conn.query_row(
                        "SELECT EXISTS(
                             SELECT 1 FROM custody_acks
                             WHERE target_hub = ?1 AND pack_id = ?2
                         )",
                        rusqlite::params![hub, pack_id],
                        |r| r.get(0),
                    )?;
                    if direct_pack_acked {
                        continue;
                    }

                    if epoch_reqs.is_empty() {
                        let pos_acked: bool = conn.query_row(
                            "SELECT EXISTS(
                                 SELECT 1 FROM custody_acks
                                 WHERE target_hub = ?1
                                   AND max_commit_pos IS NOT NULL
                                   AND max_commit_pos >= ?2
                             )",
                            rusqlite::params![hub, max_pos],
                            |r| r.get(0),
                        )?;
                        if !pos_acked {
                            return Ok(false);
                        }
                        continue;
                    }

                    for (prod_id, epoch, req_seq) in &epoch_reqs {
                        let seq_acked: bool = conn.query_row(
                            "SELECT EXISTS(
                                 SELECT 1 FROM custody_acks
                                 WHERE target_hub = ?1
                                   AND epoch = ?3
                                   AND contiguous_seq >= ?4
                                   AND (
                                       producer_id = ?2
                                       OR EXISTS(
                                           SELECT 1 FROM writer_epochs w
                                           WHERE w.epoch = ?3 AND w.producer_id = ?2
                                       )
                                   )
                             )",
                            rusqlite::params![hub, prod_id, epoch, req_seq],
                            |r| r.get(0),
                        )?;
                        if !seq_acked {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            })?
            .unwrap_or(false))
    }

    // ───────────────────────── Lane Runs & Endpoint Bindings ─────────────────────────

    /// Record a sub-agent / lane run birth (`lane_runs`).
    pub fn record_lane_start(&self, run: &LaneRun) -> Result<(), StateError> {
        if run.run_id.trim().is_empty() || run.session_id.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "lane run_id and session_id must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO lane_runs(
                 run_id, session_id, agent_id, agent_type, harness, pid, proc_start_time,
                 boot_id, transcript_path, agent_transcript_path, cwd, started_at,
                 exited_at, exit_status, last_text
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(run_id) DO UPDATE SET
                 agent_id = COALESCE(excluded.agent_id, lane_runs.agent_id),
                 agent_type = COALESCE(excluded.agent_type, lane_runs.agent_type),
                 harness = COALESCE(excluded.harness, lane_runs.harness),
                 pid = COALESCE(excluded.pid, lane_runs.pid),
                 proc_start_time = COALESCE(excluded.proc_start_time, lane_runs.proc_start_time),
                 boot_id = COALESCE(excluded.boot_id, lane_runs.boot_id),
                 transcript_path = COALESCE(excluded.transcript_path, lane_runs.transcript_path),
                 agent_transcript_path = COALESCE(excluded.agent_transcript_path, lane_runs.agent_transcript_path),
                 cwd = COALESCE(excluded.cwd, lane_runs.cwd),
                 exited_at = COALESCE(excluded.exited_at, lane_runs.exited_at),
                 exit_status = COALESCE(excluded.exit_status, lane_runs.exit_status),
                 last_text = COALESCE(excluded.last_text, lane_runs.last_text)",
            rusqlite::params![
                run.run_id,
                run.session_id,
                run.agent_id,
                run.agent_type,
                run.harness,
                run.pid,
                run.proc_start_time,
                run.boot_id,
                run.transcript_path,
                run.agent_transcript_path,
                run.cwd,
                run.started_at,
                run.exited_at,
                run.exit_status,
                run.last_text,
            ],
        )?;
        if let Some(raw) = crate::env::var("CV_BAGGAGE")
            .ok()
            .filter(|s| crate::hooks::Baggage::parse(s).is_some())
        {
            tx.execute(
                "UPDATE lane_runs SET baggage=?2 WHERE run_id=?1 AND baggage IS NULL",
                rusqlite::params![run.run_id, raw],
            )?;
            if let Some(harness) = run.harness.as_deref() {
                let child = run
                    .agent_transcript_path
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).file_stem())
                    .and_then(|s| s.to_str())
                    .filter(|s| s.starts_with("agent-"))
                    .or(run.agent_id.as_deref())
                    .unwrap_or(&run.session_id);
                record_baggage(&tx, harness, child, &raw)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record a lane run's exit timestamp, status, and final summary text.
    pub fn record_lane_exit(
        &self,
        run_id: &str,
        exited_at: i64,
        exit_status: Option<&str>,
        last_text: Option<&str>,
    ) -> Result<bool, StateError> {
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            "UPDATE lane_runs
             SET exited_at = ?2,
                 exit_status = COALESCE(?3, exit_status),
                 last_text = COALESCE(?4, last_text)
             WHERE run_id = ?1",
            rusqlite::params![run_id, exited_at, exit_status, last_text],
        )?;
        tx.commit()?;
        Ok(updated > 0)
    }

    /// Fetch a `LaneRun` by `run_id`.
    pub fn get_lane_run(&self, run_id: &str) -> Result<Option<LaneRun>, StateError> {
        Ok(self
            .read_in(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT run_id, session_id, agent_id, agent_type, harness, pid, proc_start_time,
                                boot_id, transcript_path, agent_transcript_path, cwd, started_at,
                                exited_at, exit_status, last_text
                         FROM lane_runs WHERE run_id = ?1",
                        [run_id],
                        row_to_lane_run,
                    )
                    .optional()?)
            })?
            .flatten())
    }

    /// List all `LaneRun`s recorded for `session_id`, oldest first.
    pub fn list_lane_runs_for_session(&self, session_id: &str) -> Result<Vec<LaneRun>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT run_id, session_id, agent_id, agent_type, harness, pid, proc_start_time,
                            boot_id, transcript_path, agent_transcript_path, cwd, started_at,
                            exited_at, exit_status, last_text
                     FROM lane_runs WHERE session_id = ?1
                     ORDER BY started_at ASC, run_id ASC",
                )?;
                let rows = stmt.query_map([session_id], row_to_lane_run)?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Bind a `(run_id, endpoint)` pair in `lane_endpoints`.
    pub fn bind_lane_endpoint(&self, run_id: &str, endpoint: &str) -> Result<LaneEndpointBinding, StateError> {
        if run_id.trim().is_empty() || endpoint.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "run_id and endpoint must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO lane_endpoints(run_id, endpoint, bound_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(run_id, endpoint) DO UPDATE SET bound_at = excluded.bound_at",
            rusqlite::params![run_id, endpoint, now_ms],
        )?;
        tx.commit()?;
        Ok(LaneEndpointBinding {
            run_id: run_id.to_string(),
            endpoint: endpoint.to_string(),
            bound_at: now_ms,
        })
    }

    /// List endpoints bound to `run_id`.
    pub fn endpoints_for_run(&self, run_id: &str) -> Result<Vec<LaneEndpointBinding>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT run_id, endpoint, bound_at
                     FROM lane_endpoints WHERE run_id = ?1
                     ORDER BY bound_at ASC, endpoint ASC",
                )?;
                let rows = stmt.query_map([run_id], |r| {
                    Ok(LaneEndpointBinding {
                        run_id: r.get(0)?,
                        endpoint: r.get(1)?,
                        bound_at: r.get(2)?,
                    })
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Find the most recently started `LaneRun` bound to `endpoint`.
    pub fn latest_run_for_endpoint(&self, endpoint: &str) -> Result<Option<LaneRun>, StateError> {
        Ok(self
            .read_in(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT r.run_id, r.session_id, r.agent_id, r.agent_type, r.harness, r.pid,
                                r.proc_start_time, r.boot_id, r.transcript_path, r.agent_transcript_path,
                                r.cwd, r.started_at, r.exited_at, r.exit_status, r.last_text
                         FROM lane_endpoints e
                         JOIN lane_runs r ON r.run_id = e.run_id
                         WHERE e.endpoint = ?1
                         ORDER BY e.bound_at DESC, r.started_at DESC
                         LIMIT 1",
                        [endpoint],
                        row_to_lane_run,
                    )
                    .optional()?)
            })?
            .flatten())
    }

    // ───────────────────────── Hash-Chained Receipts ─────────────────────────

    /// Append a hash-chained tool receipt for `session_id` transactionally inside `state.db`.
    ///
    /// Eliminates `session_id` filesystem path traversal and the 64 KiB mid-UTF-8 `read_to_string`
    /// tail-slicing bug by reading `prev_hash` and `seq` directly from the indexed `receipts` table
    /// inside `BEGIN IMMEDIATE`.
    pub fn append_receipt(
        &self,
        session_id: &str,
        harness: &str,
        body: Map<String, Value>,
    ) -> Result<ReceiptRecord, StateError> {
        if session_id.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "receipt session_id must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rec = insert_receipt_in_tx(&tx, session_id, harness, body)?;
        tx.commit()?;
        Ok(rec)
    }

    /// Atomically commit both a hash-chained `receipts` row and its `capture_inbox` /
    /// `operation_bindings` record inside a single `BEGIN IMMEDIATE` SQLite transaction.
    ///
    /// If either write fails (for example an `OperationConflict` or SQLite error), the entire
    /// transaction rolls back so `receipts` and `capture_inbox` can never diverge on crash/error.
    pub fn append_receipt_with_inbox(
        &self,
        session_id: &str,
        harness: &str,
        body: Map<String, Value>,
    ) -> Result<(ReceiptRecord, super::DurableReceipt), StateError> {
        if session_id.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "receipt session_id must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rec = insert_receipt_in_tx(&tx, session_id, harness, body)?;
        let op_id = format!("receipt:{}:{}:{}", harness, rec.session_id, rec.seq);
        let envelope = json!({
            "kind": "tool_receipt",
            "session_id": rec.session_id,
            "harness": rec.harness,
            "seq": rec.seq,
            "tool_use_id": rec.tool_use_id,
            "tool_name": rec.tool_name,
            "receipt_kind": rec.kind,
            "digest_sha256": rec.digest_sha256,
            "prev_hash": rec.prev_hash,
            "receipt_hash": rec.receipt_hash,
        });
        let req = super::AppendRequest::new(op_id, rec.receipt_hash.clone(), envelope);
        let producer = self.default_producer.as_str();
        let mut serverless_epoch = tx
            .query_row(
                "SELECT epoch FROM writer_epochs
                 WHERE producer_id = ?1 AND mode = 'serverless' AND closed_at IS NULL
                 ORDER BY activated_at DESC, epoch DESC LIMIT 1",
                [producer],
                |r| r.get(0),
            )
            .optional()?;
        let durable = super::recorder::commit_append_in_tx(&tx, producer, &mut serverless_epoch, "serverless", &req)?;
        tx.commit()?;
        Ok((rec, durable))
    }

    /// Return all receipts recorded for `session_id` in chain order (`seq = 1..=N`).
    pub fn receipts_for_session(&self, session_id: &str) -> Result<Vec<ReceiptRecord>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT session_id, seq, ts, harness, agent_id, tool_use_id, tool_name,
                            kind, digest_sha256, prev_hash, receipt_hash, body_json
                     FROM receipts WHERE session_id = ?1
                     ORDER BY seq ASC",
                )?;
                let rows = stmt.query_map([session_id], |r| {
                    let body_str: String = r.get(11)?;
                    Ok(ReceiptRecord {
                        session_id: r.get(0)?,
                        seq: r.get::<_, i64>(1)? as u64,
                        ts: r.get(2)?,
                        harness: r.get(3)?,
                        agent_id: r.get(4)?,
                        tool_use_id: r.get(5)?,
                        tool_name: r.get(6)?,
                        kind: r.get(7)?,
                        digest_sha256: r.get(8)?,
                        prev_hash: r.get(9)?,
                        receipt_hash: r.get(10)?,
                        body: serde_json::from_str(&body_str).unwrap_or(Value::Null),
                    })
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Cryptographically verify the entire receipt chain of `session_id` (`seq = 1..=N`).
    ///
    /// Returns the number of verified links, or an error naming the exact broken link.
    pub fn verify_receipt_chain(&self, session_id: &str) -> Result<usize, StateError> {
        let chain = self.receipts_for_session(session_id)?;
        verify_receipt_records(session_id, &chain)
    }

    // ───────────────────────── Lineage Edges ─────────────────────────

    /// Persist a directed cross-session lineage edge (`src_session_id -> dst_session_id`).
    pub fn record_lineage_edge(&self, edge: &LineageEdge) -> Result<(), StateError> {
        if edge.src_session_id.trim().is_empty()
            || edge.dst_session_id.trim().is_empty()
            || edge.edge_kind.trim().is_empty()
        {
            return Err(StateError::InvalidArgument(
                "lineage src_session_id, dst_session_id, and edge_kind must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let meta_json = serde_json::to_string(&edge.metadata)?;
        tx.execute(
            "INSERT INTO lineage_edges(
                 src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(src_session_id, dst_session_id, edge_kind) DO UPDATE SET
                 harness = COALESCE(excluded.harness, lineage_edges.harness),
                 tool_use_id = COALESCE(excluded.tool_use_id, lineage_edges.tool_use_id),
                 metadata_json = excluded.metadata_json,
                 created_at = excluded.created_at",
            rusqlite::params![
                edge.src_session_id,
                edge.dst_session_id,
                edge.edge_kind,
                edge.harness,
                edge.tool_use_id,
                meta_json,
                edge.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Outgoing lineage edges from `session_id` (`src_session_id = session_id`).
    pub fn lineage_from(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
                     FROM lineage_edges WHERE src_session_id = ?1
                     ORDER BY created_at ASC, dst_session_id ASC, edge_kind ASC",
                )?;
                let rows = stmt.query_map([session_id], row_to_lineage_edge)?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Incoming lineage edges into `session_id` (`dst_session_id = session_id`).
    pub fn lineage_to(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
                     FROM lineage_edges WHERE dst_session_id = ?1
                     ORDER BY created_at ASC, src_session_id ASC, edge_kind ASC",
                )?;
                let rows = stmt.query_map([session_id], row_to_lineage_edge)?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?
            .unwrap_or_default())
    }

    /// Transitive descendants reachable from `session_id` (cycle-safe breadth-first walk).
    pub fn lineage_descendants(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        Ok(self
            .read_in(|conn| {
                lineage_bfs(
                    conn,
                    session_id,
                    "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
                     FROM lineage_edges WHERE src_session_id = ?1
                     ORDER BY created_at ASC, dst_session_id ASC, edge_kind ASC",
                    |edge| &edge.dst_session_id,
                )
            })?
            .unwrap_or_default())
    }

    /// Transitive ancestors leading to `session_id` (cycle-safe breadth-first walk).
    pub fn lineage_ancestors(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        Ok(self
            .read_in(|conn| {
                lineage_bfs(
                    conn,
                    session_id,
                    "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
                     FROM lineage_edges WHERE dst_session_id = ?1
                     ORDER BY created_at ASC, src_session_id ASC, edge_kind ASC",
                    |edge| &edge.src_session_id,
                )
            })?
            .unwrap_or_default())
    }
}

const PACK_MANIFEST_COLS: &str =
    "pack_id, min_commit_pos, max_commit_pos, record_count, byte_len, digest, pack_path, index_meta_json, published_at";

fn row_to_pack_manifest(r: &rusqlite::Row<'_>) -> rusqlite::Result<PackManifest> {
    let meta_str: String = r.get(7)?;
    Ok(PackManifest {
        pack_id: r.get(0)?,
        min_commit_pos: r.get(1)?,
        max_commit_pos: r.get(2)?,
        record_count: r.get::<_, i64>(3)? as u64,
        byte_len: r.get::<_, i64>(4)? as u64,
        digest: r.get(5)?,
        pack_path: r.get(6)?,
        index_meta: serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({})),
        published_at: r.get(8)?,
    })
}

fn row_to_producer(r: &rusqlite::Row<'_>) -> rusqlite::Result<Producer> {
    let meta_str: String = r.get(2)?;
    Ok(Producer {
        producer_id: r.get(0)?,
        host_label: r.get(1)?,
        metadata: serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({})),
        enrolled_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn row_to_operation_binding(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, DurableReceipt)> {
    let key: String = r.get(0)?;
    let prod: String = r.get(1)?;
    let epoch: String = r.get(2)?;
    let seq: i64 = r.get(3)?;
    let pos: i64 = r.get(4)?;
    Ok((
        key,
        DurableReceipt {
            event_id: EventId::new(prod, epoch, seq as u64),
            local_commit_pos: pos,
            replayed: true,
        },
    ))
}

fn lineage_bfs(
    conn: &rusqlite::Connection,
    session_id: &str,
    sql: &str,
    next_id: impl Fn(&LineageEdge) -> &str,
) -> Result<Vec<LineageEdge>, StateError> {
    let mut stmt = conn.prepare(sql)?;
    let mut visited_nodes = HashSet::new();
    visited_nodes.insert(session_id.to_string());
    let mut queue = VecDeque::new();
    queue.push_back(session_id.to_string());
    let mut edges = Vec::new();
    while let Some(cur) = queue.pop_front() {
        let rows = stmt.query_map([&cur], row_to_lineage_edge)?;
        for edge in rows {
            let edge = edge?;
            let neighbor = next_id(&edge);
            if visited_nodes.insert(neighbor.to_owned()) {
                queue.push_back(neighbor.to_owned());
            }
            edges.push(edge);
        }
    }
    Ok(edges)
}

fn row_to_lane_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<LaneRun> {
    Ok(LaneRun {
        run_id: r.get(0)?,
        session_id: r.get(1)?,
        agent_id: r.get(2)?,
        agent_type: r.get(3)?,
        harness: r.get(4)?,
        pid: r.get(5)?,
        proc_start_time: r.get(6)?,
        boot_id: r.get(7)?,
        transcript_path: r.get(8)?,
        agent_transcript_path: r.get(9)?,
        cwd: r.get(10)?,
        started_at: r.get(11)?,
        exited_at: r.get(12)?,
        exit_status: r.get(13)?,
        last_text: r.get(14)?,
    })
}

fn row_to_lineage_edge(r: &rusqlite::Row<'_>) -> rusqlite::Result<LineageEdge> {
    let meta_str: String = r.get(5)?;
    Ok(LineageEdge {
        src_session_id: r.get(0)?,
        dst_session_id: r.get(1)?,
        edge_kind: r.get(2)?,
        harness: r.get(3)?,
        tool_use_id: r.get(4)?,
        metadata: serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({})),
        created_at: r.get(6)?,
    })
}

/// Canonical JSON serialization (lexicographically sorted object keys, zero extra whitespace).
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// Compute `sha256("cv-receipt-v1\n{prev}\n{canonical_json(body_without_h)}")`.
pub fn compute_receipt_hash(prev: &str, body_without_h: &Value) -> String {
    let payload = format!("{RECEIPT_DOMAIN}\n{prev}\n{}", canonical_json(body_without_h));
    crate::digest::sha256_hex(payload.as_bytes())
}

/// Current OS `(pid, proc_start_time)` identity for process-reuse-safe liveness checks.
pub fn current_process_identity() -> (Option<i64>, Option<i64>) {
    let pid = std::process::id() as i64;
    (Some(pid), proc_start_time_of(pid))
}

/// Query OS process start timestamp for `pid`, returning `None` if `pid` does not exist or is
/// inaccessible. Comparing `(pid, proc_start_time)` prevents recycled PIDs from appearing live.
pub fn proc_start_time_of(pid: i64) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
                size,
            )
        };
        if ret == size {
            let start_us = (info.pbi_start_tvsec as i128) * 1_000_000 + (info.pbi_start_tvusec as i128);
            return i64::try_from(start_us).ok();
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(") ")?.1;
        // Field 3 is index 0 of `after_comm`, so field 22 (`starttime`) is index 19.
        after_comm.split_whitespace().nth(19)?.parse::<i64>().ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// Check whether `run` corresponds to a currently live OS process matching `(pid, proc_start_time)`.
pub fn is_lane_run_alive(run: &LaneRun) -> bool {
    if let Some(recorded) = &run.boot_id {
        if crate::lanes::current_boot_id().as_ref() != Some(recorded) {
            return false;
        }
    }
    if run.exited_at.is_some() {
        return false;
    }
    let Some(pid) = run.pid.filter(|&p| p > 0) else {
        return false;
    };
    #[cfg(unix)]
    {
        let alive_signal = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if alive_signal != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EPERM) {
                return false;
            }
        }
        if let Some(expected_start) = run.proc_start_time {
            return proc_start_time_of(pid) == Some(expected_start);
        }
        true
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

pub(crate) fn verify_receipt_records(session_id: &str, chain: &[ReceiptRecord]) -> Result<usize, StateError> {
    let mut expected_prev = String::new();
    for (expected_seq, rec) in (1u64..).zip(chain.iter()) {
        if rec.seq != expected_seq {
            return Err(StateError::InvalidArgument(format!(
                "receipt chain gap in session `{session_id}`: expected seq {expected_seq}, found {}",
                rec.seq
            )));
        }
        if rec.prev_hash != expected_prev {
            return Err(StateError::InvalidArgument(format!(
                "receipt chain prev_hash mismatch at seq {}: expected `{expected_prev}`, found `{}`",
                rec.seq, rec.prev_hash
            )));
        }
        let mut without_h = rec.body.clone();
        if let Some(obj) = without_h.as_object_mut() {
            obj.remove("h");
        }
        let computed = compute_receipt_hash(&expected_prev, &without_h);
        if computed != rec.receipt_hash {
            return Err(StateError::InvalidArgument(format!(
                "receipt chain tampered at seq {}: computed `{computed}` != stored `{}`",
                rec.seq, rec.receipt_hash
            )));
        }
        expected_prev = rec.receipt_hash.clone();
    }
    Ok(chain.len())
}

fn insert_receipt_in_tx(
    tx: &rusqlite::Connection,
    session_id: &str,
    harness: &str,
    mut body: Map<String, Value>,
) -> Result<ReceiptRecord, StateError> {
    let prev_row: Option<(i64, String)> = tx
        .query_row(
            "SELECT seq, receipt_hash FROM receipts
             WHERE session_id = ?1
             ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let (seq, prev_hash) = match prev_row {
        Some((last_seq, last_hash)) => ((last_seq + 1) as u64, last_hash),
        None => (1u64, String::new()),
    };

    let ts = body
        .get("ts")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    body.entry("v").or_insert_with(|| json!(1));
    body.insert("session".into(), json!(session_id));
    body.insert("ts".into(), json!(ts));
    body.insert("prev".into(), json!(prev_hash));

    let kind = body.get("kind").and_then(Value::as_str).unwrap_or("pre").to_string();
    let agent_id = body.get("agent").and_then(Value::as_str).map(str::to_string);
    let tool_use_id = body.get("tool_use_id").and_then(Value::as_str).map(str::to_string);
    let tool_name = body.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
    let digest_sha256 = body
        .get("input_sha256")
        .or_else(|| body.get("response_sha256"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let unsigned = Value::Object(body.clone());
    let receipt_hash = compute_receipt_hash(&prev_hash, &unsigned);
    body.insert("h".into(), json!(receipt_hash));
    let full_body = Value::Object(body);
    let body_json = serde_json::to_string(&full_body)?;

    tx.execute(
        "INSERT INTO receipts(
             session_id, seq, ts, harness, agent_id, tool_use_id, tool_name,
             kind, digest_sha256, prev_hash, receipt_hash, body_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            session_id,
            seq as i64,
            ts,
            harness,
            agent_id,
            tool_use_id,
            tool_name,
            kind,
            digest_sha256,
            prev_hash,
            receipt_hash,
            body_json,
        ],
    )?;

    if let Some(call) = tool_use_id.as_deref() {
        crate::events::workspace::record(
            tx,
            harness,
            session_id,
            agent_id.as_deref().unwrap_or(""),
            call,
            &full_body,
        )?;
    }
    Ok(ReceiptRecord {
        session_id: session_id.to_string(),
        seq,
        ts,
        harness: harness.to_string(),
        agent_id,
        tool_use_id,
        tool_name,
        kind,
        digest_sha256,
        prev_hash,
        receipt_hash,
        body: full_body,
    })
}

impl StateStore {
    /// Return or atomically initialize a persistent, privacy-safe replication `producer_id`
    /// (`prod-<12hex>`) stored in `producers.metadata_json["ship_producer_id"]` for this store so
    /// multiple machines shipping with default settings never collide under `"local"`.
    pub fn ensure_ship_producer_id(&self) -> Result<String, StateError> {
        let mut conn = self.open_writable_core()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prod_key = self.default_producer();
        let existing_meta: Option<String> = tx
            .query_row(
                "SELECT metadata_json FROM producers WHERE producer_id = ?1",
                [prod_key],
                |r| r.get(0),
            )
            .optional()?;

        let mut meta_obj = existing_meta
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();

        if let Some(existing_id) = meta_obj
            .get("ship_producer_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(existing_id.to_string());
        }

        let generated = format!("prod-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        meta_obj.insert("ship_producer_id".into(), Value::String(generated.clone()));
        let meta_json = serde_json::to_string(&Value::Object(meta_obj))?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(producer_id) DO UPDATE SET
                 metadata_json = excluded.metadata_json,
                 updated_at = excluded.updated_at",
            rusqlite::params![prod_key, default_host_label(), meta_json, now_ms],
        )?;
        tx.commit()?;
        Ok(generated)
    }
}

/// Allocate a time-ordered RFC-9562 UUIDv7 writer epoch that is strictly greater (lexicographically)
/// than every prior UUIDv7 epoch recorded for `producer_id`, even across same-millisecond bursts
/// or wall-clock skew.
pub(crate) fn allocate_monotonic_epoch(
    conn: &rusqlite::Connection,
    producer_id: &str,
) -> Result<String, rusqlite::Error> {
    let candidate = uuid::Uuid::now_v7();
    let max_prev: Option<String> = conn
        .query_row(
            "SELECT MAX(epoch) FROM writer_epochs
             WHERE producer_id = ?1 AND length(epoch) = 36 AND substr(epoch, 15, 1) = '7'",
            [producer_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();

    if let Some(prev_str) = max_prev {
        if let Ok(prev_uuid) = uuid::Uuid::parse_str(&prev_str) {
            if candidate.as_u128() <= prev_uuid.as_u128() {
                let next_key = unpack_uuid_v7_key122(prev_uuid.as_u128()).saturating_add(1);
                return Ok(uuid::Uuid::from_u128(repack_uuid_v7_key122(next_key)).to_string());
            }
        }
    }
    Ok(candidate.to_string())
}

fn unpack_uuid_v7_key122(u: u128) -> u128 {
    let ts48 = (u >> 80) & 0xffff_ffff_ffff;
    let rand_a12 = (u >> 64) & 0x0fff;
    let rand_b62 = u & 0x3fff_ffff_ffff_ffff;
    (ts48 << 74) | (rand_a12 << 62) | rand_b62
}

fn repack_uuid_v7_key122(key122: u128) -> u128 {
    let ts48 = (key122 >> 74) & 0xffff_ffff_ffff;
    let rand_a12 = (key122 >> 62) & 0x0fff;
    let rand_b62 = key122 & 0x3fff_ffff_ffff_ffff;
    (ts48 << 80) | (0x7u128 << 76) | (rand_a12 << 64) | (0x2u128 << 62) | rand_b62
}

/// Upsert the custody entries a manifest's `index_meta.sessions` carries, keeping the one with
/// the highest commit position per session selector. A manifest without sessions (hub inbox
/// seals) changes nothing.
fn upsert_custody_heads(
    tx: &rusqlite::Transaction<'_>,
    pack_id: &str,
    commit_pos: i64,
    index_meta: &Value,
) -> Result<(), StateError> {
    let Some(sessions) = index_meta.get("sessions").and_then(Value::as_array) else {
        return Ok(());
    };
    let physical = crate::custody::physical_pack_id(pack_id, index_meta);
    for entry in sessions {
        let mut entry = entry.clone();
        if entry["addr"]["pack_id"].as_str().is_some_and(str::is_empty) {
            entry["addr"]["pack_id"] = json!(physical);
        }
        let text = |k: &str| entry.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(harness) = crate::Harness::parse(&text("harness")) else {
            continue;
        };
        let selector = crate::SessionKey {
            producer: text("producer_id"),
            profile: text("profile").into(),
            harness,
            native_id: text("id"),
        }
        .selector();
        tx.execute(
            "INSERT INTO custody_heads(selector, commit_pos, pack_id, entry_json) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(selector) DO UPDATE SET
                 commit_pos = excluded.commit_pos, pack_id = excluded.pack_id, entry_json = excluded.entry_json
             WHERE excluded.commit_pos >= custody_heads.commit_pos",
            rusqlite::params![selector, commit_pos, physical, serde_json::to_string(&entry)?],
        )?;
    }
    Ok(())
}

/// Read `custody_heads` rows from `conn` when the table is present and fresh.
fn read_custody_heads_in(conn: &rusqlite::Connection) -> Result<Option<Vec<Value>>, StateError> {
    if !custody_heads_fresh(conn)? {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT entry_json FROM custody_heads ORDER BY selector")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    rows.iter()
        .map(|raw| serde_json::from_str(raw).map_err(StateError::from))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

impl StateStore {
    /// Point-query a single session's local `custody_heads` JSON entry by `selector` in `O(log N)`
    /// without scanning other sessions. Returns `Ok(None)` when `state.db` is absent, `custody_heads`
    /// is not fresh (`Ok(Some(None))` when fresh and absent), or under `--frozen` on an empty home.
    pub(crate) fn custody_head_point(&self, selector: &str) -> Result<Option<Option<Value>>, StateError> {
        self.read_in(|conn| {
            if !custody_heads_fresh(conn)? {
                return Ok(None);
            }
            let raw: Option<String> = conn
                .query_row(
                    "SELECT entry_json FROM custody_heads WHERE selector = ?1",
                    [selector],
                    |r| r.get(0),
                )
                .optional()?;
            match raw {
                Some(s) => Ok(Some(Some(serde_json::from_str(&s)?))),
                None => Ok(Some(None)),
            }
        })
        .map(|opt| opt.flatten())
    }
}

fn manifest_watermark_fresh(conn: &rusqlite::Connection, table: &str, through: &str) -> Result<bool, StateError> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) = 2 FROM sqlite_schema WHERE type = 'table' AND name IN (?1, ?2)",
        [table, through],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let fresh: Option<bool> = conn
        .query_row(
            &format!(
                "SELECT w.manifests = m.n AND w.through = m.pos
                 FROM {through} w,
                      (SELECT COUNT(*) AS n, COALESCE(MAX(min_commit_pos), 0) AS pos FROM pack_manifests) m
                 WHERE w.id = 1"
            ),
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(fresh == Some(true))
}

fn mark_manifest_watermark(tx: &rusqlite::Transaction<'_>, through: &str) -> Result<(), StateError> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO {through}(id, manifests, through)
             SELECT 1, COUNT(*), COALESCE(MAX(min_commit_pos), 0) FROM pack_manifests"
        ),
        [],
    )?;
    Ok(())
}

/// Fast-path check for [`super::schema::converge`]: verifies all four custody/watermark tables
/// exist and both watermarks match `pack_manifests` in one `pack_manifests` aggregation.
pub(super) fn custody_tables_fresh(conn: &rusqlite::Connection) -> Result<bool, StateError> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) = 4 FROM sqlite_schema
         WHERE type = 'table'
           AND name IN ('custody_heads', 'custody_heads_through', 'custody_commits', 'custody_commits_through')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let fresh: Option<bool> = conn
        .query_row(
            "SELECT h.manifests = m.n AND h.through = m.pos
                AND c.manifests = m.n AND c.through = m.pos
             FROM custody_heads_through h,
                  custody_commits_through c,
                  (SELECT COUNT(*) AS n, COALESCE(MAX(min_commit_pos), 0) AS pos FROM pack_manifests) m
             WHERE h.id = 1 AND c.id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(fresh == Some(true))
}

/// Whether `custody_heads` exists and folds exactly the manifests `pack_manifests` holds now (its
/// watermark's count and newest position). False on a store without the tables.
pub(crate) fn custody_heads_fresh(conn: &rusqlite::Connection) -> Result<bool, StateError> {
    manifest_watermark_fresh(conn, "custody_heads", "custody_heads_through")
}

/// Record that `custody_heads` folds every manifest `pack_manifests` holds now.
fn mark_custody_heads(tx: &rusqlite::Transaction<'_>) -> Result<(), StateError> {
    mark_manifest_watermark(tx, "custody_heads_through")
}

/// Refold `custody_heads` from every published manifest in commit order, then mark it fresh.
/// Runs once per upgrade (or after a binary without the table published), never per append.
pub(super) fn rebuild_custody_heads(tx: &rusqlite::Transaction<'_>) -> Result<(), StateError> {
    tx.execute("DELETE FROM custody_heads", [])?;
    let mut stmt = tx.prepare(
        "SELECT pack_id, min_commit_pos, index_meta_json FROM pack_manifests
         ORDER BY min_commit_pos ASC, pack_id ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (pack_id, pos, meta) = row?;
        let meta: Value = serde_json::from_str(&meta).unwrap_or_else(|_| json!({}));
        upsert_custody_heads(tx, &pack_id, pos, &meta)?;
    }
    drop(stmt);
    mark_custody_heads(tx)
}

/// Whether `custody_commits` exists and folds every published `pack_manifests` row.
pub(super) fn custody_commits_fresh(conn: &rusqlite::Connection) -> Result<bool, StateError> {
    manifest_watermark_fresh(conn, "custody_commits", "custody_commits_through")
}

fn mark_custody_commits(tx: &rusqlite::Transaction<'_>) -> Result<(), StateError> {
    mark_manifest_watermark(tx, "custody_commits_through")
}

#[allow(clippy::too_many_arguments)]
fn upsert_commit_row(
    tx: &rusqlite::Transaction<'_>,
    pos: i64,
    producer_id: &str,
    epoch: &str,
    seq: u64,
    op_id: &str,
    envelope: &Value,
    payload_addr: Option<&crate::packs::PayloadAddr>,
    delta_extents: Option<&[crate::packs::PayloadAddr]>,
    committed_at: i64,
) -> Result<(), StateError> {
    let env_json = serde_json::to_string(envelope)?;
    let addr_json = match payload_addr {
        Some(a) => Some(serde_json::to_string(a)?),
        None => None,
    };
    let delta_json = match delta_extents {
        Some(d) if !d.is_empty() => Some(serde_json::to_string(d)?),
        _ => envelope
            .get("delta_extents")
            .filter(|v| v.is_array())
            .map(serde_json::to_string)
            .transpose()?,
    };
    tx.execute(
        "INSERT INTO custody_commits(
             local_commit_pos, producer_id, epoch, seq, op_id,
             envelope_json, payload_addr_json, delta_extents_json, committed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(local_commit_pos) DO UPDATE SET
             producer_id = excluded.producer_id,
             epoch = excluded.epoch,
             seq = excluded.seq,
             op_id = excluded.op_id,
             envelope_json = excluded.envelope_json,
             payload_addr_json = COALESCE(excluded.payload_addr_json, custody_commits.payload_addr_json),
             delta_extents_json = COALESCE(excluded.delta_extents_json, custody_commits.delta_extents_json)",
        rusqlite::params![
            pos,
            producer_id,
            epoch,
            seq as i64,
            op_id,
            env_json,
            addr_json,
            delta_json,
            committed_at,
        ],
    )?;
    Ok(())
}

fn upsert_custody_commits(
    tx: &rusqlite::Transaction<'_>,
    pm: &PackManifest,
    db_dir: Option<&std::path::Path>,
) -> Result<(), StateError> {
    let mut handled = false;

    if let Some(arr) = pm.index_meta.get("commits").and_then(Value::as_array) {
        for val in arr {
            let pos = val
                .get("local_commit_pos")
                .and_then(Value::as_i64)
                .unwrap_or(pm.min_commit_pos);
            let producer_id = val.get("producer_id").and_then(Value::as_str).unwrap_or("local");
            let epoch = val.get("epoch").and_then(Value::as_str).unwrap_or("");
            let seq = val.get("seq").and_then(Value::as_u64).unwrap_or(1);
            let op_id = val.get("op_id").and_then(Value::as_str).unwrap_or("");
            let envelope = val.get("envelope").cloned().unwrap_or_else(|| json!({}));
            let payload_addr = val
                .get("payload_addr")
                .and_then(|a| serde_json::from_value::<crate::packs::PayloadAddr>(a.clone()).ok());
            let delta_extents = val
                .get("delta_extents")
                .and_then(|d| serde_json::from_value::<Vec<crate::packs::PayloadAddr>>(d.clone()).ok());
            if !epoch.is_empty() {
                upsert_commit_row(
                    tx,
                    pos,
                    producer_id,
                    epoch,
                    seq,
                    op_id,
                    &envelope,
                    payload_addr.as_ref(),
                    delta_extents.as_deref(),
                    pm.published_at,
                )?;
            }
        }
        handled = true;
    }

    if !handled {
        if let Some(value) = pm.index_meta.get("commits_addr") {
            if let Ok(addr) = serde_json::from_value::<crate::packs::PayloadAddr>(value.clone()) {
                let raw_parent = std::path::Path::new(&pm.pack_path).parent();
                let fallback_dir = db_dir.map(|d| d.join("packs"));
                let dir_opt = match raw_parent {
                    Some(p) if !p.as_os_str().is_empty() && p.is_absolute() && p.exists() => Some(p.to_path_buf()),
                    _ => fallback_dir.filter(|p| p.exists()),
                };
                if let Some(dir) = dir_opt {
                    if let Ok(pack) = crate::packs::SealedPack::open(&dir, &addr.pack_id) {
                        if let Ok(bytes) = pack.read_payload(&addr) {
                            if let Ok(entries) = serde_json::from_slice::<Vec<Value>>(&bytes) {
                                for val in entries {
                                    let pos = val
                                        .get("local_commit_pos")
                                        .and_then(Value::as_i64)
                                        .unwrap_or(pm.min_commit_pos);
                                    let producer_id = val.get("producer_id").and_then(Value::as_str).unwrap_or("local");
                                    let epoch = val.get("epoch").and_then(Value::as_str).unwrap_or("");
                                    let seq = val.get("seq").and_then(Value::as_u64).unwrap_or(1);
                                    let op_id = val.get("op_id").and_then(Value::as_str).unwrap_or("");
                                    let envelope = val.get("envelope").cloned().unwrap_or_else(|| json!({}));
                                    let payload_addr = val.get("payload_addr").and_then(|a| {
                                        serde_json::from_value::<crate::packs::PayloadAddr>(a.clone()).ok()
                                    });
                                    if !epoch.is_empty() {
                                        upsert_commit_row(
                                            tx,
                                            pos,
                                            producer_id,
                                            epoch,
                                            seq,
                                            op_id,
                                            &envelope,
                                            payload_addr.as_ref(),
                                            None,
                                            pm.published_at,
                                        )?;
                                    }
                                }
                                handled = true;
                            }
                        }
                    }
                }
            }
        }
    }

    if !handled {
        // Legacy pre-commits pack manifest: reconstruct from `operation_bindings` + `index_meta.sessions`.
        let mut binding_by_pos: std::collections::BTreeMap<i64, (String, String, String, u64)> =
            std::collections::BTreeMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT local_commit_pos, op_id, producer_id, epoch, seq
                 FROM operation_bindings
                 WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2
                 ORDER BY local_commit_pos ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![pm.min_commit_pos, pm.max_commit_pos], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)? as u64,
                ))
            })?;
            for r in rows {
                let (pos, op_id, prod, ep, seq) = r?;
                binding_by_pos.insert(pos, (op_id, prod, ep, seq));
            }
        }
        let phys_pack_id = crate::custody::physical_pack_id(&pm.pack_id, &pm.index_meta);
        if let Some(sessions) = pm.index_meta.get("sessions").and_then(Value::as_array) {
            for (idx, sess_val) in sessions.iter().enumerate() {
                let pos = pm.min_commit_pos + idx as i64;
                let (op_id, producer_id, epoch, seq) = match binding_by_pos.get(&pos) {
                    Some((op_id, prod, ep, seq)) => (op_id.clone(), prod.clone(), ep.clone(), *seq),
                    None => {
                        let prod = sess_val
                            .get("producer_id")
                            .and_then(Value::as_str)
                            .unwrap_or("local")
                            .to_string();
                        let harness = sess_val.get("harness").and_then(Value::as_str).unwrap_or("unknown");
                        let sid = sess_val
                            .get("id")
                            .or_else(|| sess_val.get("session_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        (
                            format!("archive:{}:{prod}:{harness}:{sid}", pm.pack_id),
                            prod,
                            format!("legacy-{}", pm.pack_id),
                            (idx + 1) as u64,
                        )
                    }
                };
                let mut payload_addr = sess_val
                    .get("addr")
                    .and_then(|a| serde_json::from_value::<crate::packs::PayloadAddr>(a.clone()).ok());
                if let Some(ref mut addr) = payload_addr {
                    if addr.pack_id.is_empty() {
                        addr.pack_id = phys_pack_id.to_string();
                    }
                }
                let mut envelope = json!({
                    "kind": "session_archive",
                    "producer_id": producer_id,
                    "harness": sess_val.get("harness").cloned().unwrap_or(Value::Null),
                    "session_id": sess_val.get("id").or_else(|| sess_val.get("session_id")).cloned().unwrap_or(Value::Null),
                    "title": sess_val.get("title").cloned().unwrap_or(Value::Null),
                    "cwd": sess_val.get("cwd").cloned().unwrap_or(Value::Null),
                    "created_at": sess_val.get("created_at").cloned().unwrap_or(Value::Null),
                    "updated_at": sess_val.get("updated_at").cloned().unwrap_or(Value::Null),
                    "message_count": sess_val.get("message_count").cloned().unwrap_or(json!(0)),
                    "content_blake3": sess_val.get("content_blake3").cloned().unwrap_or(Value::Null),
                    "pack_id": payload_addr.as_ref().map(|a| a.pack_id.as_str()).unwrap_or(phys_pack_id),
                    "decoded_offset": payload_addr.as_ref().map(|a| a.decoded_offset).unwrap_or(0),
                    "length": payload_addr.as_ref().map(|a| a.length).unwrap_or(0),
                });
                if let Some(prof) = sess_val.get("profile") {
                    envelope["profile"] = prof.clone();
                }
                if let Some(cust) = sess_val.get("custody") {
                    envelope["custody"] = cust.clone();
                }
                upsert_commit_row(
                    tx,
                    pos,
                    &producer_id,
                    &epoch,
                    seq,
                    &op_id,
                    &envelope,
                    payload_addr.as_ref(),
                    None,
                    pm.published_at,
                )?;
            }
        } else {
            for (pos, (op_id, prod, ep, seq)) in binding_by_pos {
                upsert_commit_row(
                    tx,
                    pos,
                    &prod,
                    &ep,
                    seq,
                    &op_id,
                    &json!({ "kind": "noop" }),
                    None,
                    None,
                    pm.published_at,
                )?;
            }
        }
    }

    Ok(())
}

pub(super) fn rebuild_custody_commits(
    tx: &rusqlite::Transaction<'_>,
    db_dir: Option<&std::path::Path>,
) -> Result<(), StateError> {
    tx.execute("DELETE FROM custody_commits", [])?;
    let manifests: Vec<PackManifest> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT {PACK_MANIFEST_COLS}
             FROM pack_manifests
             ORDER BY min_commit_pos ASC, pack_id ASC"
        ))?;
        let rows = stmt.query_map([], row_to_pack_manifest)?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for pm in &manifests {
        upsert_custody_commits(tx, pm, db_dir)?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO custody_commits(
             local_commit_pos, producer_id, epoch, seq, op_id,
             envelope_json, payload_addr_json, delta_extents_json, committed_at
         )
         SELECT local_commit_pos, producer_id, epoch, seq, op_id,
                envelope_json, payload_ref, NULL, committed_at
         FROM capture_inbox",
        [],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO ship_cursors(target_name, producer_id, epoch, contiguous_seq, updated_at)
         SELECT target_hub, producer_id, epoch, contiguous_seq, acked_at
         FROM custody_acks",
        [],
    )?;
    mark_custody_commits(tx)
}

impl StateStore {
    /// Read outbound `ship_cursors` watermarks for `target_name` (merged with `custody_acks`).
    /// Safe under `--frozen`.
    pub fn ship_cursors_for(&self, target_name: &str) -> Result<Vec<ShipCursorRow>, StateError> {
        Ok(self
            .read_in(|conn| {
                let mut by_key: std::collections::BTreeMap<(String, String), ShipCursorRow> =
                    std::collections::BTreeMap::new();
                let has_acks: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'custody_acks')",
                    [],
                    |r| r.get(0),
                )?;
                if has_acks {
                    let mut stmt = conn.prepare(
                        "SELECT producer_id, epoch, contiguous_seq, acked_at
                         FROM custody_acks WHERE target_hub = ?1",
                    )?;
                    let rows = stmt.query_map([target_name], |r| {
                        Ok(ShipCursorRow {
                            target_name: target_name.to_string(),
                            producer_id: r.get(0)?,
                            epoch: r.get(1)?,
                            contiguous_seq: r.get::<_, i64>(2)? as u64,
                            updated_at: r.get(3)?,
                        })
                    })?;
                    for row in rows {
                        let r = row?;
                        by_key.insert((r.producer_id.clone(), r.epoch.clone()), r);
                    }
                }
                let has_cursors: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'ship_cursors')",
                    [],
                    |r| r.get(0),
                )?;
                if has_cursors {
                    let mut stmt = conn.prepare(
                        "SELECT producer_id, epoch, contiguous_seq, updated_at
                         FROM ship_cursors WHERE target_name = ?1",
                    )?;
                    let rows = stmt.query_map([target_name], |r| {
                        Ok(ShipCursorRow {
                            target_name: target_name.to_string(),
                            producer_id: r.get(0)?,
                            epoch: r.get(1)?,
                            contiguous_seq: r.get::<_, i64>(2)? as u64,
                            updated_at: r.get(3)?,
                        })
                    })?;
                    for row in rows {
                        let r = row?;
                        by_key
                            .entry((r.producer_id.clone(), r.epoch.clone()))
                            .and_modify(|prev| {
                                if r.contiguous_seq > prev.contiguous_seq {
                                    prev.contiguous_seq = r.contiguous_seq;
                                }
                                if r.updated_at > prev.updated_at {
                                    prev.updated_at = r.updated_at;
                                }
                            })
                            .or_insert(r);
                    }
                }
                Ok(by_key.into_values().collect())
            })?
            .unwrap_or_default())
    }

    /// Monotonically advance `ship_cursors` for `(target_name, producer_id, epoch)`.
    pub fn advance_ship_cursor(
        &self,
        target_name: &str,
        producer_id: &str,
        epoch: &str,
        contiguous_seq: u64,
        updated_at: i64,
    ) -> Result<(), StateError> {
        let mut conn = self.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO ship_cursors(target_name, producer_id, epoch, contiguous_seq, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(target_name, producer_id, epoch) DO UPDATE SET
                 contiguous_seq = MAX(ship_cursors.contiguous_seq, excluded.contiguous_seq),
                 updated_at = MAX(ship_cursors.updated_at, excluded.updated_at)",
            rusqlite::params![target_name, producer_id, epoch, contiguous_seq as i64, updated_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Query unsent commits strictly after `acked_by_epoch` (`epoch -> contiguous_seq`) using the
    /// indexed `custody_commits` outbox (`O(|delta|)` instead of scanning historical pack manifests).
    /// Returns `Ok(None)` only when reading `--frozen` against a pre-v8 database whose
    /// `custody_commits` table has not been populated yet.
    pub fn pending_custody_commits(
        &self,
        acked_by_epoch: &std::collections::BTreeMap<String, u64>,
        limit: usize,
    ) -> Result<Option<Vec<CustodyCommitRow>>, StateError> {
        if limit == 0 {
            return Ok(Some(Vec::new()));
        }
        self.read_in(|conn| {
            if !custody_commits_fresh(conn)? {
                return Ok(None);
            }
            let epochs: Vec<(String, String)> = {
                let mut stmt =
                    conn.prepare("SELECT DISTINCT producer_id, epoch FROM custody_commits ORDER BY epoch ASC")?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let mut out = Vec::new();
            for (prod_id, epoch) in epochs {
                if out.len() >= limit {
                    break;
                }
                let acked = acked_by_epoch.get(&epoch).copied().unwrap_or(0);
                let rem = (limit - out.len()) as i64;
                let mut stmt = conn.prepare(
                    "SELECT local_commit_pos, producer_id, epoch, seq, op_id,
                            envelope_json, payload_addr_json, delta_extents_json, committed_at
                     FROM custody_commits
                     WHERE producer_id = ?1 AND epoch = ?2 AND seq > ?3
                     ORDER BY seq ASC
                     LIMIT ?4",
                )?;
                let rows = stmt.query_map(rusqlite::params![prod_id, epoch, acked as i64, rem], |r| {
                    let env_str: String = r.get(5)?;
                    let addr_str: Option<String> = r.get(6)?;
                    let delta_str: Option<String> = r.get(7)?;
                    Ok(CustodyCommitRow {
                        local_commit_pos: r.get(0)?,
                        producer_id: r.get(1)?,
                        epoch: r.get(2)?,
                        seq: r.get::<_, i64>(3)? as u64,
                        op_id: r.get(4)?,
                        envelope: serde_json::from_str(&env_str).unwrap_or_else(|_| json!({})),
                        payload_addr: addr_str.and_then(|s| serde_json::from_str(&s).ok()),
                        delta_extents: delta_str.and_then(|s| serde_json::from_str(&s).ok()),
                        committed_at: r.get(8)?,
                    })
                })?;
                for row in rows {
                    out.push(row?);
                }
            }
            out.sort_by_key(|c| c.local_commit_pos);
            Ok(Some(out))
        })
        .map(|opt| opt.unwrap_or(Some(Vec::new())))
    }

    /// Indexed `O(log N)` lookup for the highest `local_commit_pos` in `epoch` with `seq <= upto_seq`.
    pub fn max_commit_pos_at_or_below(&self, epoch: &str, upto_seq: u64) -> Result<Option<i64>, StateError> {
        Ok(self
            .read_in(|conn| {
                let has_commits: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'custody_commits')",
                    [],
                    |r| r.get(0),
                )?;
                if has_commits {
                    let pos: Option<i64> = conn.query_row(
                        "SELECT MAX(local_commit_pos) FROM custody_commits WHERE epoch = ?1 AND seq <= ?2",
                        rusqlite::params![epoch, upto_seq as i64],
                        |r| r.get(0),
                    )?;
                    if pos.is_some() {
                        return Ok(pos);
                    }
                }
                let pos: Option<i64> = conn.query_row(
                    "SELECT MAX(local_commit_pos) FROM operation_bindings WHERE epoch = ?1 AND seq <= ?2",
                    rusqlite::params![epoch, upto_seq as i64],
                    |r| r.get(0),
                )?;
                Ok(pos)
            })?
            .flatten())
    }
}
