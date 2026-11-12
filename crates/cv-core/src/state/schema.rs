//! Authoritative `state.db` schema and forward-only migration runner.
//!
//! Unlike `catalog.db` (which is derived and rebuildable from external transcripts), `state.db`
//! holds `cv`-owned authoritative facts: enrolled producers, writer epochs, idempotency operation
//! bindings, the bounded capture inbox, pack manifests & custody acknowledgements, task events,
//! retired-agent records, lane runs/endpoints, hash-chained tool receipts, and session lineage
//! edges. Schema evolution is strictly forward-migrated via [`STATE_SCHEMA_VERSION`] and NEVER
//! coupled to [`crate::source::VERSION`].

use super::StateError;
use rusqlite::Connection;

/// Current authoritative `state.db` schema version.
pub const STATE_SCHEMA_VERSION: i64 = 3;

/// Ordered forward migrations `(target_version, name, ddl)`.
pub const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "v1_authoritative_state_spine", MIGRATION_V1),
    (2, "v2_fleet_hub", MIGRATION_V2),
    (3, "v3_hub_delta_and_snapshot_bounds", MIGRATION_V3),
];

const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS producers (
    producer_id   TEXT PRIMARY KEY,
    host_label    TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    enrolled_at   INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS writer_epochs (
    epoch           TEXT PRIMARY KEY,
    producer_id     TEXT NOT NULL REFERENCES producers(producer_id),
    mode            TEXT NOT NULL,
    pid             INTEGER,
    proc_start_time INTEGER,
    activated_at    INTEGER NOT NULL,
    closed_at       INTEGER
);
CREATE INDEX IF NOT EXISTS idx_writer_epochs_producer
    ON writer_epochs(producer_id, activated_at);

CREATE TABLE IF NOT EXISTS epoch_counters (
    producer_id TEXT NOT NULL,
    epoch       TEXT NOT NULL,
    last_seq    INTEGER NOT NULL,
    PRIMARY KEY(producer_id, epoch)
);

CREATE TABLE IF NOT EXISTS operation_bindings (
    op_id            TEXT PRIMARY KEY,
    request_digest   TEXT NOT NULL,
    producer_id      TEXT NOT NULL,
    epoch            TEXT NOT NULL,
    seq              INTEGER NOT NULL,
    local_commit_pos INTEGER NOT NULL,
    committed_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_op_bindings_commit_pos
    ON operation_bindings(local_commit_pos);

CREATE TABLE IF NOT EXISTS capture_inbox (
    local_commit_pos INTEGER PRIMARY KEY AUTOINCREMENT,
    producer_id      TEXT NOT NULL,
    epoch            TEXT NOT NULL,
    seq              INTEGER NOT NULL,
    op_id            TEXT NOT NULL UNIQUE,
    envelope_json    TEXT NOT NULL,
    payload_ref      TEXT,
    committed_at     INTEGER NOT NULL,
    UNIQUE(producer_id, epoch, seq)
);
CREATE INDEX IF NOT EXISTS idx_capture_inbox_epoch_seq
    ON capture_inbox(producer_id, epoch, seq);

CREATE TABLE IF NOT EXISTS pack_manifests (
    pack_id         TEXT PRIMARY KEY,
    min_commit_pos  INTEGER NOT NULL,
    max_commit_pos  INTEGER NOT NULL,
    record_count    INTEGER NOT NULL,
    byte_len        INTEGER NOT NULL,
    digest          TEXT NOT NULL,
    pack_path       TEXT NOT NULL,
    index_meta_json TEXT NOT NULL DEFAULT '{}',
    published_at    INTEGER NOT NULL,
    CHECK (min_commit_pos <= max_commit_pos),
    CHECK (record_count >= 0),
    CHECK (byte_len >= 0)
);
CREATE INDEX IF NOT EXISTS idx_pack_manifests_range
    ON pack_manifests(min_commit_pos, max_commit_pos);

CREATE TABLE IF NOT EXISTS custody_acks (
    target_hub      TEXT NOT NULL,
    producer_id     TEXT NOT NULL,
    epoch           TEXT NOT NULL,
    contiguous_seq  INTEGER NOT NULL,
    max_commit_pos  INTEGER,
    pack_id         TEXT,
    acked_at        INTEGER NOT NULL,
    PRIMARY KEY(target_hub, producer_id, epoch)
);

CREATE TABLE IF NOT EXISTS task_events (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id  TEXT NOT NULL UNIQUE,
    task_id   TEXT NOT NULL,
    ts        TEXT NOT NULL,
    by_actor  TEXT NOT NULL,
    raw_json  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_events_task_id
    ON task_events(task_id, seq);

CREATE TABLE IF NOT EXISTS task_folds (
    slot         INTEGER PRIMARY KEY CHECK (slot = 1),
    upto_seq     INTEGER NOT NULL,
    model_json   TEXT NOT NULL,
    applied_json TEXT NOT NULL,
    garbage_json TEXT NOT NULL,
    refused_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_sync_meta (
    slot         INTEGER PRIMARY KEY CHECK (slot = 1),
    log_len      INTEGER NOT NULL,
    log_lines    INTEGER NOT NULL,
    file_id_json TEXT
);

CREATE TABLE IF NOT EXISTS retired_agents (
    agent_id   TEXT PRIMARY KEY,
    ts         TEXT NOT NULL,
    successor  TEXT,
    session_id TEXT,
    undone     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS lane_runs (
    run_id                TEXT PRIMARY KEY,
    session_id            TEXT NOT NULL,
    agent_id              TEXT,
    agent_type            TEXT,
    harness               TEXT,
    pid                   INTEGER,
    proc_start_time       INTEGER,
    boot_id               TEXT,
    transcript_path       TEXT,
    agent_transcript_path TEXT,
    cwd                   TEXT,
    started_at            INTEGER NOT NULL,
    exited_at             INTEGER,
    exit_status           TEXT,
    last_text             TEXT
);
CREATE INDEX IF NOT EXISTS idx_lane_runs_session
    ON lane_runs(session_id, started_at);
CREATE INDEX IF NOT EXISTS idx_lane_runs_agent
    ON lane_runs(agent_id);

CREATE TABLE IF NOT EXISTS lane_endpoints (
    run_id   TEXT NOT NULL REFERENCES lane_runs(run_id) ON DELETE CASCADE,
    endpoint TEXT NOT NULL,
    bound_at INTEGER NOT NULL,
    PRIMARY KEY(run_id, endpoint)
);
CREATE INDEX IF NOT EXISTS idx_lane_endpoints_endpoint
    ON lane_endpoints(endpoint);

CREATE TABLE IF NOT EXISTS receipts (
    session_id    TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    ts            TEXT NOT NULL,
    harness       TEXT NOT NULL,
    agent_id      TEXT,
    tool_use_id   TEXT,
    tool_name     TEXT NOT NULL,
    kind          TEXT NOT NULL,
    digest_sha256 TEXT NOT NULL,
    prev_hash     TEXT NOT NULL,
    receipt_hash  TEXT NOT NULL,
    body_json     TEXT NOT NULL,
    PRIMARY KEY(session_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_receipts_hash
    ON receipts(receipt_hash);

CREATE TABLE IF NOT EXISTS lineage_edges (
    src_session_id TEXT NOT NULL,
    dst_session_id TEXT NOT NULL,
    edge_kind      TEXT NOT NULL,
    harness        TEXT,
    tool_use_id    TEXT,
    metadata_json  TEXT NOT NULL DEFAULT '{}',
    created_at     INTEGER NOT NULL,
    PRIMARY KEY(src_session_id, dst_session_id, edge_kind)
);
CREATE INDEX IF NOT EXISTS idx_lineage_src
    ON lineage_edges(src_session_id);
CREATE INDEX IF NOT EXISTS idx_lineage_dst
    ON lineage_edges(dst_session_id);
"#;

const MIGRATION_V2: &str = r#"
CREATE TABLE IF NOT EXISTS hub_packs (
    pack_id            TEXT PRIMARY KEY,
    producer_id        TEXT,
    byte_len           INTEGER NOT NULL CHECK (byte_len >= 0),
    total_decoded_len  INTEGER NOT NULL CHECK (total_decoded_len >= 0),
    frame_count        INTEGER NOT NULL CHECK (frame_count >= 0),
    record_count       INTEGER NOT NULL CHECK (record_count >= 0),
    blake3_digest      TEXT NOT NULL,
    idx_blake3_digest  TEXT NOT NULL,
    pack_rel_path      TEXT NOT NULL,
    idx_rel_path       TEXT NOT NULL,
    manifest_json      TEXT NOT NULL,
    received_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS hub_ingested_events (
    ingest_seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    producer_id       TEXT NOT NULL,
    epoch             TEXT NOT NULL,
    seq               INTEGER NOT NULL CHECK (seq >= 1),
    op_id             TEXT NOT NULL,
    event_digest      TEXT NOT NULL,
    envelope_json     TEXT NOT NULL,
    payload_addr_json TEXT,
    ingested_at       INTEGER NOT NULL,
    UNIQUE(producer_id, epoch, seq)
);
CREATE INDEX IF NOT EXISTS idx_hub_ingested_origin_seq
    ON hub_ingested_events(producer_id, epoch, seq);

CREATE TABLE IF NOT EXISTS hub_quarantine (
    quarantine_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    producer_id            TEXT NOT NULL,
    epoch                  TEXT NOT NULL,
    seq                    INTEGER NOT NULL CHECK (seq >= 1),
    op_id                  TEXT NOT NULL,
    existing_digest        TEXT NOT NULL,
    conflicting_digest     TEXT NOT NULL,
    conflicting_envelope   TEXT NOT NULL,
    conflicting_payload    TEXT,
    quarantined_at         INTEGER NOT NULL,
    UNIQUE(producer_id, epoch, seq, conflicting_digest)
);
CREATE INDEX IF NOT EXISTS idx_hub_quarantine_origin_seq
    ON hub_quarantine(producer_id, epoch, seq);

CREATE TABLE IF NOT EXISTS hub_frontiers (
    producer_id        TEXT NOT NULL,
    epoch              TEXT NOT NULL,
    contiguous_seq     INTEGER NOT NULL CHECK (contiguous_seq >= 0),
    max_received_seq   INTEGER NOT NULL CHECK (max_received_seq >= 0),
    buffered_count     INTEGER NOT NULL CHECK (buffered_count >= 0),
    first_conflict_seq INTEGER,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY(producer_id, epoch)
);

CREATE TABLE IF NOT EXISTS hub_sessions (
    producer_id       TEXT NOT NULL,
    session_id        TEXT NOT NULL,
    harness           TEXT NOT NULL,
    revision          TEXT NOT NULL,
    title             TEXT,
    cwd               TEXT,
    model             TEXT,
    created_at        TEXT,
    updated_at        TEXT,
    message_count     INTEGER NOT NULL DEFAULT 0,
    epoch             TEXT NOT NULL,
    event_seq         INTEGER NOT NULL,
    payload_addr_json TEXT,
    metadata_json     TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY(producer_id, session_id)
);
CREATE INDEX IF NOT EXISTS idx_hub_sessions_updated
    ON hub_sessions(updated_at DESC, producer_id ASC, session_id ASC);

CREATE TABLE IF NOT EXISTS hub_snapshots (
    hub_generation INTEGER PRIMARY KEY,
    publication_id TEXT NOT NULL UNIQUE,
    published_at   INTEGER NOT NULL,
    manifest_json  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_hub_snapshots_pub_id
    ON hub_snapshots(publication_id);
"#;

const MIGRATION_V3: &str = r#"
ALTER TABLE hub_snapshots ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE hub_frontiers ADD COLUMN control_events INTEGER NOT NULL DEFAULT 0;
ALTER TABLE hub_frontiers ADD COLUMN delta_seeded INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS hub_epoch_sessions (
    producer_id TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    epoch       TEXT NOT NULL,
    is_delete   INTEGER NOT NULL CHECK (is_delete IN (0, 1)),
    sort_ts_ms  INTEGER NOT NULL,
    event_seq   INTEGER NOT NULL,
    entry_json  TEXT,
    PRIMARY KEY(producer_id, session_id, epoch)
);

CREATE TABLE IF NOT EXISTS hub_frontier_packs (
    producer_id TEXT NOT NULL,
    epoch       TEXT NOT NULL,
    pack_id     TEXT NOT NULL,
    PRIMARY KEY(producer_id, epoch, pack_id)
);
"#;

/// Read `PRAGMA user_version` from `conn`.
pub fn current_schema_version(conn: &Connection) -> Result<i64, StateError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Ensure `conn` is migrated forward to [`STATE_SCHEMA_VERSION`].
///
/// Fast-paths when `PRAGMA user_version == STATE_SCHEMA_VERSION` so serverless hot-path appends
/// pay zero DDL overhead. Refuses `user_version > STATE_SCHEMA_VERSION` rather than clobbering a
/// newer binary's authoritative state.
pub fn ensure_migrated(conn: &mut Connection) -> Result<(), StateError> {
    apply_migrations_with(conn, STATE_SCHEMA_VERSION, MIGRATIONS)
}

/// Apply forward migrations up to `target_version` inside an `IMMEDIATE` transaction.
pub fn apply_migrations_with(
    conn: &mut Connection,
    target_version: i64,
    migrations: &[(i64, &str, &str)],
) -> Result<(), StateError> {
    let found = current_schema_version(conn)?;
    if found > target_version {
        return Err(StateError::NewerSchema {
            found,
            supported: target_version,
        });
    }
    if found == target_version {
        return Ok(());
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > target_version {
        return Err(StateError::NewerSchema {
            found: current,
            supported: target_version,
        });
    }
    if current == target_version {
        tx.commit()?;
        return Ok(());
    }

    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );",
    )?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    for &(ver, name, sql) in migrations {
        if ver <= current || ver > target_version {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![ver, name, now_ms],
        )?;
        tx.pragma_update(None, "user_version", ver)?;
    }

    tx.commit()?;
    Ok(())
}
