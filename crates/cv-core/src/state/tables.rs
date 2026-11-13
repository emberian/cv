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
        let mut conn = self.open_writable()?;
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
            |r| {
                let meta_str: String = r.get(2)?;
                Ok(Producer {
                    producer_id: r.get(0)?,
                    host_label: r.get(1)?,
                    metadata: serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({})),
                    enrolled_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            },
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Look up an enrolled producer by `producer_id`.
    pub fn get_producer(&self, producer_id: &str) -> Result<Option<Producer>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT producer_id, host_label, metadata_json, enrolled_at, updated_at
                 FROM producers WHERE producer_id = ?1",
                [producer_id],
                |r| {
                    let meta_str: String = r.get(2)?;
                    Ok(Producer {
                        producer_id: r.get(0)?,
                        host_label: r.get(1)?,
                        metadata: serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({})),
                        enrolled_at: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// List all enrolled producers ordered by `enrolled_at`.
    pub fn list_producers(&self) -> Result<Vec<Producer>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT producer_id, host_label, metadata_json, enrolled_at, updated_at
             FROM producers ORDER BY enrolled_at ASC, producer_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let meta_str: String = r.get(2)?;
            Ok(Producer {
                producer_id: r.get(0)?,
                host_label: r.get(1)?,
                metadata: serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({})),
                enrolled_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Allocate a fresh random 128-bit UUIDv4 writer epoch for `producer_id`.
    pub fn activate_epoch(&self, producer_id: &str, mode: &str) -> Result<WriterEpoch, StateError> {
        if producer_id.trim().is_empty() {
            return Err(StateError::InvalidArgument("producer_id must not be empty".into()));
        }
        let mut conn = self.open_writable()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
             VALUES (?1, ?2, '{}', ?3, ?3)
             ON CONFLICT(producer_id) DO NOTHING",
            rusqlite::params![producer_id, default_host_label(), now_ms],
        )?;
        let epoch = uuid::Uuid::new_v4().to_string();
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
        let mut conn = self.open_writable()?;
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
        let Some(conn) = self.open_readonly()? else {
            return Ok(None);
        };
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
    }

    // ───────────────────────── Operation Bindings & Capture Inbox ─────────────────────────

    /// Look up an idempotency binding by `op_id`.
    pub fn get_operation_binding(&self, op_id: &str) -> Result<Option<(String, DurableReceipt)>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT request_digest, producer_id, epoch, seq, local_commit_pos
                 FROM operation_bindings WHERE op_id = ?1",
                [op_id],
                |r| {
                    let digest: String = r.get(0)?;
                    let prod: String = r.get(1)?;
                    let epoch: String = r.get(2)?;
                    let seq: i64 = r.get(3)?;
                    let pos: i64 = r.get(4)?;
                    Ok((
                        digest,
                        DurableReceipt {
                            event_id: EventId::new(prod, epoch, seq as u64),
                            local_commit_pos: pos,
                            replayed: true,
                        },
                    ))
                },
            )
            .optional()?)
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
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT op_id, producer_id, epoch, seq, local_commit_pos
             FROM operation_bindings
             WHERE local_commit_pos >= ?1 AND local_commit_pos <= ?2
             ORDER BY local_commit_pos ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![min_pos, max_pos], |r| {
            let op_id: String = r.get(0)?;
            let prod: String = r.get(1)?;
            let epoch: String = r.get(2)?;
            let seq: i64 = r.get(3)?;
            let pos: i64 = r.get(4)?;
            Ok((
                op_id,
                DurableReceipt {
                    event_id: EventId::new(prod, epoch, seq as u64),
                    local_commit_pos: pos,
                    replayed: true,
                },
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Read retained rows in `capture_inbox` within `[min_pos, max_pos]` ordered by `local_commit_pos`.
    pub fn read_inbox_range(&self, min_pos: i64, max_pos: i64) -> Result<Vec<CaptureInboxRow>, StateError> {
        if min_pos > max_pos {
            return Ok(Vec::new());
        }
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
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
    }

    /// Count unpacked rows currently retained in `capture_inbox`.
    pub fn inbox_count(&self) -> Result<u64, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(0);
        };
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM capture_inbox", [], |r| r.get(0))?;
        Ok(n as u64)
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

        tx.commit()?;
        Ok(deleted)
    }

    /// List all published pack manifests ordered by `min_commit_pos`.
    pub fn list_pack_manifests(&self) -> Result<Vec<PackManifest>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT pack_id, min_commit_pos, max_commit_pos, record_count, byte_len,
                    digest, pack_path, index_meta_json, published_at
             FROM pack_manifests
             ORDER BY min_commit_pos ASC, pack_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
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
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
        tx.commit()?;
        Ok(())
    }

    /// List custody acknowledgements recorded for `target_hub`.
    pub fn custody_acks_for(&self, target_hub: &str) -> Result<Vec<CustodyAck>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
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
    }

    /// Never-delete-before-custody gate: returns `true` iff `pack_id` exists and every hub in
    /// `required_hubs` has acknowledged `max_commit_pos >= manifest.max_commit_pos` (or `pack_id`
    /// directly). If `required_hubs` is empty, local custody alone applies (`true`).
    pub fn is_pack_evictable(&self, pack_id: &str, required_hubs: &[&str]) -> Result<bool, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(false);
        };
        let Some(max_pos): Option<i64> = conn
            .query_row(
                "SELECT max_commit_pos FROM pack_manifests WHERE pack_id = ?1",
                [pack_id],
                |r| r.get(0),
            )
            .optional()?
        else {
            return Ok(false);
        };
        for &hub in required_hubs {
            let acked: bool = conn.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM custody_acks
                     WHERE target_hub = ?1
                       AND (pack_id = ?2 OR (max_commit_pos IS NOT NULL AND max_commit_pos >= ?3))
                 )",
                rusqlite::params![hub, pack_id, max_pos],
                |r| r.get(0),
            )?;
            if !acked {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ───────────────────────── Lane Runs & Endpoint Bindings ─────────────────────────

    /// Record a sub-agent / lane run birth (`lane_runs`).
    pub fn record_lane_start(&self, run: &LaneRun) -> Result<(), StateError> {
        if run.run_id.trim().is_empty() || run.session_id.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "lane run_id and session_id must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable()?;
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
        let mut conn = self.open_writable()?;
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
        let Some(conn) = self.open_readonly()? else {
            return Ok(None);
        };
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
    }

    /// List all `LaneRun`s recorded for `session_id`, oldest first.
    pub fn list_lane_runs_for_session(&self, session_id: &str) -> Result<Vec<LaneRun>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, agent_id, agent_type, harness, pid, proc_start_time,
                    boot_id, transcript_path, agent_transcript_path, cwd, started_at,
                    exited_at, exit_status, last_text
             FROM lane_runs WHERE session_id = ?1
             ORDER BY started_at ASC, run_id ASC",
        )?;
        let rows = stmt.query_map([session_id], row_to_lane_run)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Bind a `(run_id, endpoint)` pair in `lane_endpoints`.
    pub fn bind_lane_endpoint(&self, run_id: &str, endpoint: &str) -> Result<LaneEndpointBinding, StateError> {
        if run_id.trim().is_empty() || endpoint.trim().is_empty() {
            return Err(StateError::InvalidArgument(
                "run_id and endpoint must not be empty".into(),
            ));
        }
        let mut conn = self.open_writable()?;
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
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
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
    }

    /// Find the most recently started `LaneRun` bound to `endpoint`.
    pub fn latest_run_for_endpoint(&self, endpoint: &str) -> Result<Option<LaneRun>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(None);
        };
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
        let mut conn = self.open_writable()?;
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
        let mut conn = self.open_writable()?;
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
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
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
    }

    /// Cryptographically verify the entire receipt chain of `session_id` (`seq = 1..=N`).
    ///
    /// Returns the number of verified links, or an error naming the exact broken link.
    pub fn verify_receipt_chain(&self, session_id: &str) -> Result<usize, StateError> {
        let chain = self.receipts_for_session(session_id)?;
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
        let mut conn = self.open_writable()?;
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
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
             FROM lineage_edges WHERE src_session_id = ?1
             ORDER BY created_at ASC, dst_session_id ASC, edge_kind ASC",
        )?;
        let rows = stmt.query_map([session_id], row_to_lineage_edge)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Incoming lineage edges into `session_id` (`dst_session_id = session_id`).
    pub fn lineage_to(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
             FROM lineage_edges WHERE dst_session_id = ?1
             ORDER BY created_at ASC, src_session_id ASC, edge_kind ASC",
        )?;
        let rows = stmt.query_map([session_id], row_to_lineage_edge)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Transitive descendants reachable from `session_id` (cycle-safe breadth-first walk).
    pub fn lineage_descendants(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
             FROM lineage_edges WHERE src_session_id = ?1
             ORDER BY created_at ASC, dst_session_id ASC, edge_kind ASC",
        )?;
        let mut visited_nodes = HashSet::new();
        visited_nodes.insert(session_id.to_string());
        let mut queue = VecDeque::new();
        queue.push_back(session_id.to_string());
        let mut edges = Vec::new();
        while let Some(cur) = queue.pop_front() {
            let rows = stmt.query_map([&cur], row_to_lineage_edge)?;
            for edge in rows {
                let edge = edge?;
                if visited_nodes.insert(edge.dst_session_id.clone()) {
                    queue.push_back(edge.dst_session_id.clone());
                }
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    /// Transitive ancestors leading to `session_id` (cycle-safe breadth-first walk).
    pub fn lineage_ancestors(&self, session_id: &str) -> Result<Vec<LineageEdge>, StateError> {
        let Some(conn) = self.open_readonly()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
             FROM lineage_edges WHERE dst_session_id = ?1
             ORDER BY created_at ASC, src_session_id ASC, edge_kind ASC",
        )?;
        let mut visited_nodes = HashSet::new();
        visited_nodes.insert(session_id.to_string());
        let mut queue = VecDeque::new();
        queue.push_back(session_id.to_string());
        let mut edges = Vec::new();
        while let Some(cur) = queue.pop_front() {
            let rows = stmt.query_map([&cur], row_to_lineage_edge)?;
            for edge in rows {
                let edge = edge?;
                if visited_nodes.insert(edge.src_session_id.clone()) {
                    queue.push_back(edge.src_session_id.clone());
                }
                edges.push(edge);
            }
        }
        Ok(edges)
    }
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
        let mut conn = self.open_writable()?;
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
