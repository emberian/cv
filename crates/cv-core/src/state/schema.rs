//! Authoritative `state.db` schema and forward-only migration runner.
//!
//! Unlike `catalog.db` (which is derived and rebuildable from external transcripts), `state.db`
//! holds `cv`-owned authoritative facts: enrolled producers, writer epochs, idempotency operation
//! bindings, the bounded capture inbox, pack manifests & custody acknowledgements, cached task
//! folds, retired-agent records, lane runs/endpoints, hash-chained tool receipts, and session
//! lineage edges. Schema evolution is strictly forward-migrated via [`STATE_SCHEMA_VERSION`] and
//! NEVER coupled to [`crate::source::VERSION`].

use super::StateError;
use rusqlite::Connection;

/// Current authoritative `state.db` schema version.
pub const STATE_SCHEMA_VERSION: i64 = 11;

/// Ordered forward migrations `(target_version, name, ddl)`.
pub const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "v1_authoritative_state_spine", MIGRATION_V1),
    (2, "v2_fleet_hub", MIGRATION_V2),
    (3, "v3_hub_delta_and_snapshot_bounds", MIGRATION_V3),
    (4, "v4_hook_observations", MIGRATION_V4),
    (5, "v5_file_observations", MIGRATION_V5),
    (6, "v6_hub_producer_credentials", MIGRATION_V6),
    (7, "v7_hub_generational_mvcc", MIGRATION_V7),
    (8, "v8_custody_outbox_and_ship_cursors", MIGRATION_V8),
    (
        9,
        "v9_generation_authority",
        "ALTER TABLE hub_generations ADD COLUMN authority_json TEXT;
        CREATE UNIQUE INDEX hub_open_session_head ON hub_session_heads(producer_id,session_id) WHERE gen_to IS NULL;
        CREATE TABLE hub_pull_sources(target TEXT PRIMARY KEY, producer_id TEXT NOT NULL);",
    ),
    (
        10,
        "v10_hub_index_changes",
        r#"
        CREATE TABLE hub_index_changes(selector TEXT PRIMARY KEY, generation INTEGER NOT NULL);
        CREATE INDEX hub_index_changes_by_generation ON hub_index_changes(generation,selector);
        INSERT INTO hub_index_changes SELECT selector, MAX(COALESCE(gen_to,gen_from)) FROM hub_session_heads GROUP BY selector;
        CREATE TRIGGER hub_index_open AFTER INSERT ON hub_session_heads BEGIN
            INSERT INTO hub_index_changes VALUES(NEW.selector,NEW.gen_from)
            ON CONFLICT(selector) DO UPDATE SET generation=MAX(generation,excluded.generation);
        END;
        CREATE TRIGGER hub_index_close AFTER UPDATE OF gen_to ON hub_session_heads WHEN NEW.gen_to IS NOT NULL BEGIN
            INSERT INTO hub_index_changes VALUES(OLD.selector,NEW.gen_to)
            ON CONFLICT(selector) DO UPDATE SET generation=MAX(generation,excluded.generation);
        END;
        UPDATE hub_generations SET indexed_generation=0;
    "#,
    ),
    (
        11,
        "v11_drop_zombie_task_events",
        "DROP INDEX IF EXISTS idx_task_events_task_id;
        DROP TABLE IF EXISTS task_events;
        DROP TABLE IF EXISTS task_sync_meta;",
    ),
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

CREATE TABLE IF NOT EXISTS task_folds (
    slot         INTEGER PRIMARY KEY CHECK (slot = 1),
    upto_seq     INTEGER NOT NULL,
    model_json   TEXT NOT NULL,
    applied_json TEXT NOT NULL,
    garbage_json TEXT NOT NULL,
    refused_json TEXT NOT NULL
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

const MIGRATION_V4: &str = r#"
CREATE TABLE whispers (
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    subject_rev TEXT NOT NULL,
    command TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('offered','emitted','followed_up','unfollowed')),
    emitted_at INTEGER,
    followed_up_at INTEGER,
    PRIMARY KEY(session_id, kind, subject_key, subject_rev)
) WITHOUT ROWID;
ALTER TABLE lane_runs ADD COLUMN baggage TEXT;
CREATE TABLE session_baggage (
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    baggage TEXT NOT NULL,
    PRIMARY KEY(harness, session_id)
) WITHOUT ROWID;
"#;

/// File versions observed by tool receipts. `horizons` records, per table, the newest start time
/// of any expired row deleted from it, so readers can tell forgotten history from absent history.
const MIGRATION_V5: &str = r#"
CREATE TABLE IF NOT EXISTS file_versions (
    harness TEXT NOT NULL, session TEXT NOT NULL, agent TEXT NOT NULL, call TEXT NOT NULL,
    path TEXT NOT NULL, edit INTEGER NOT NULL, base TEXT NOT NULL, blob TEXT NOT NULL,
    lo INTEGER, hi INTEGER,
    UNIQUE(harness, session, agent, call, path)
);
CREATE INDEX IF NOT EXISTS file_versions_writer ON file_versions(harness, session, agent, path);
CREATE INDEX IF NOT EXISTS file_versions_edits ON file_versions(path, edit);
CREATE TABLE IF NOT EXISTS pending_file_versions (
    harness TEXT NOT NULL, session TEXT NOT NULL, agent TEXT NOT NULL, call TEXT NOT NULL,
    path TEXT NOT NULL, edit INTEGER NOT NULL, base TEXT NOT NULL, lo INTEGER, hi INTEGER,
    ambiguous INTEGER NOT NULL DEFAULT 0, started INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY(harness, session, agent, call, path)
);
CREATE INDEX IF NOT EXISTS pending_file_path ON pending_file_versions(path, edit, started);
CREATE INDEX IF NOT EXISTS pending_file_age ON pending_file_versions(started);
CREATE TABLE IF NOT EXISTS horizons (
    name TEXT PRIMARY KEY,
    newest_forgotten_time INTEGER NOT NULL
) WITHOUT ROWID;
-- Task projections gained replay-derived fields; cached folds rebuild from the log.
DELETE FROM task_folds;
"#;

const MIGRATION_V6: &str = r#"
CREATE TABLE IF NOT EXISTS hub_producer_credentials (
    producer_id  TEXT PRIMARY KEY,
    token_blake3 TEXT NOT NULL,
    scopes_json  TEXT NOT NULL DEFAULT '[]',
    created_at   INTEGER NOT NULL,
    revoked_at   INTEGER
);

ALTER TABLE hub_quarantine ADD COLUMN committed_at_quarantine INTEGER NOT NULL DEFAULT 0;
ALTER TABLE hub_quarantine ADD COLUMN resolved_at INTEGER;
ALTER TABLE hub_quarantine ADD COLUMN resolution TEXT;
"#;

const MIGRATION_V7: &str = r#"
ALTER TABLE hub_frontiers ADD COLUMN frontier_hash TEXT NOT NULL DEFAULT '';

CREATE TABLE IF NOT EXISTS hub_generations (
    generation         INTEGER PRIMARY KEY,
    publication_id     TEXT NOT NULL UNIQUE,
    published_at       INTEGER NOT NULL,
    frontier_digest    TEXT NOT NULL,
    control_revision   INTEGER NOT NULL,
    quarantine_count   INTEGER NOT NULL,
    indexed_generation INTEGER NOT NULL DEFAULT 0,
    pinned             INTEGER NOT NULL DEFAULT 0,
    frontier_json      TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_hub_generations_pub_id
    ON hub_generations(publication_id);

CREATE TABLE IF NOT EXISTS hub_session_heads (
    producer_id       TEXT NOT NULL,
    session_id        TEXT NOT NULL,
    selector          TEXT NOT NULL,
    gen_from          INTEGER NOT NULL,
    gen_to            INTEGER,
    harness           TEXT NOT NULL,
    revision          TEXT NOT NULL,
    title             TEXT,
    cwd               TEXT,
    model             TEXT,
    created_at        TEXT,
    updated_at        TEXT,
    message_count     INTEGER NOT NULL,
    epoch             TEXT NOT NULL,
    event_seq         INTEGER NOT NULL,
    payload_addr_json TEXT,
    custody_json      TEXT,
    metadata_json     TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY(producer_id, session_id, gen_from)
);
CREATE INDEX IF NOT EXISTS idx_hub_session_heads_live_updated
    ON hub_session_heads(gen_to, updated_at DESC, producer_id ASC, session_id ASC);
CREATE INDEX IF NOT EXISTS idx_hub_session_heads_selector
    ON hub_session_heads(selector, gen_to);

INSERT OR IGNORE INTO hub_generations(
    generation, publication_id, published_at, frontier_digest,
    control_revision, quarantine_count, indexed_generation, pinned, frontier_json
)
SELECT hub_generation, publication_id, published_at, '', 0, 0, 0, pinned, '{}'
FROM hub_snapshots;

INSERT OR IGNORE INTO hub_session_heads(
    producer_id, session_id, selector, gen_from, gen_to,
    harness, revision, title, cwd, model, created_at, updated_at,
    message_count, epoch, event_seq, payload_addr_json, custody_json, metadata_json
)
SELECT
    producer_id,
    session_id,
    CASE
        WHEN instr(session_id, ':') > 0 THEN session_id
        ELSE producer_id || ':' || harness || ':' || session_id
    END,
    COALESCE((SELECT MAX(generation) FROM hub_generations), 1),
    NULL,
    harness, revision, title, cwd, model, created_at, updated_at,
    message_count, epoch, event_seq, payload_addr_json, NULL, metadata_json
FROM hub_sessions;

"#;

const MIGRATION_V8: &str = r#"
CREATE TABLE IF NOT EXISTS custody_commits (
    local_commit_pos   INTEGER PRIMARY KEY,
    producer_id        TEXT NOT NULL,
    epoch              TEXT NOT NULL,
    seq                INTEGER NOT NULL CHECK (seq >= 1),
    op_id              TEXT NOT NULL,
    envelope_json      TEXT NOT NULL,
    payload_addr_json  TEXT,
    delta_extents_json TEXT,
    committed_at       INTEGER NOT NULL,
    UNIQUE(producer_id, epoch, seq)
);
CREATE INDEX IF NOT EXISTS idx_custody_commits_epoch_seq
    ON custody_commits(producer_id, epoch, seq);

CREATE TABLE IF NOT EXISTS custody_commits_through (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    manifests INTEGER NOT NULL,
    through   INTEGER NOT NULL
);

CREATE TRIGGER IF NOT EXISTS trg_capture_inbox_to_custody_commits
AFTER INSERT ON capture_inbox
BEGIN
    INSERT INTO custody_commits(
        local_commit_pos, producer_id, epoch, seq, op_id,
        envelope_json, payload_addr_json, delta_extents_json, committed_at
    ) VALUES (
        NEW.local_commit_pos, NEW.producer_id, NEW.epoch, NEW.seq, NEW.op_id,
        NEW.envelope_json, NEW.payload_ref, NULL, NEW.committed_at
    )
    ON CONFLICT(local_commit_pos) DO UPDATE SET
        envelope_json = excluded.envelope_json,
        payload_addr_json = COALESCE(excluded.payload_addr_json, custody_commits.payload_addr_json);
END;

INSERT OR IGNORE INTO custody_commits(
    local_commit_pos, producer_id, epoch, seq, op_id,
    envelope_json, payload_addr_json, delta_extents_json, committed_at
)
SELECT local_commit_pos, producer_id, epoch, seq, op_id,
       envelope_json, payload_ref, NULL, committed_at
FROM capture_inbox;

CREATE TABLE IF NOT EXISTS ship_cursors (
    target_name    TEXT NOT NULL,
    producer_id    TEXT NOT NULL,
    epoch          TEXT NOT NULL,
    contiguous_seq INTEGER NOT NULL CHECK (contiguous_seq >= 0),
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY(target_name, producer_id, epoch)
);

INSERT OR IGNORE INTO ship_cursors(target_name, producer_id, epoch, contiguous_seq, updated_at)
SELECT target_hub, producer_id, epoch, contiguous_seq, acked_at
FROM custody_acks;
"#;

/// Tables that converge by presence rather than by `user_version`: every writable open creates
/// what is missing, whatever version numbering other migrations use, so a store at any version
/// (including one whose numbers another branch of migrations claimed) gains them exactly once.
///
/// `custody_heads` holds the newest custody entry per session selector, so reading the custody
/// inventory costs one row per archived session rather than every historical manifest.
/// `custody_heads_through` is its one-row watermark: the `pack_manifests` count and newest
/// position it folds. A reader that sees another signature (the table is absent, or a binary
/// without it published since) reads `pack_manifests` instead; the next writable open rebuilds.
const CONVERGED: &str = r#"
CREATE TABLE IF NOT EXISTS custody_heads (
    selector    TEXT PRIMARY KEY,
    commit_pos  INTEGER NOT NULL,
    pack_id     TEXT NOT NULL,
    entry_json  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS custody_heads_through (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    manifests INTEGER NOT NULL,
    through   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS custody_commits (
    local_commit_pos   INTEGER PRIMARY KEY,
    producer_id        TEXT NOT NULL,
    epoch              TEXT NOT NULL,
    seq                INTEGER NOT NULL CHECK (seq >= 1),
    op_id              TEXT NOT NULL,
    envelope_json      TEXT NOT NULL,
    payload_addr_json  TEXT,
    delta_extents_json TEXT,
    committed_at       INTEGER NOT NULL,
    UNIQUE(producer_id, epoch, seq)
);
CREATE INDEX IF NOT EXISTS idx_custody_commits_epoch_seq
    ON custody_commits(producer_id, epoch, seq);
CREATE TABLE IF NOT EXISTS custody_commits_through (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    manifests INTEGER NOT NULL,
    through   INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS trg_capture_inbox_to_custody_commits
AFTER INSERT ON capture_inbox
BEGIN
    INSERT INTO custody_commits(
        local_commit_pos, producer_id, epoch, seq, op_id,
        envelope_json, payload_addr_json, delta_extents_json, committed_at
    ) VALUES (
        NEW.local_commit_pos, NEW.producer_id, NEW.epoch, NEW.seq, NEW.op_id,
        NEW.envelope_json, NEW.payload_ref, NULL, NEW.committed_at
    )
    ON CONFLICT(local_commit_pos) DO UPDATE SET
        envelope_json = excluded.envelope_json,
        payload_addr_json = COALESCE(excluded.payload_addr_json, custody_commits.payload_addr_json);
END;
CREATE TABLE IF NOT EXISTS ship_cursors (
    target_name    TEXT NOT NULL,
    producer_id    TEXT NOT NULL,
    epoch          TEXT NOT NULL,
    contiguous_seq INTEGER NOT NULL CHECK (contiguous_seq >= 0),
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY(target_name, producer_id, epoch)
);
CREATE TABLE IF NOT EXISTS hub_producer_credentials (
    producer_id  TEXT PRIMARY KEY,
    token_blake3 TEXT NOT NULL,
    scopes_json  TEXT NOT NULL DEFAULT '[]',
    created_at   INTEGER NOT NULL,
    revoked_at   INTEGER
);
CREATE TABLE IF NOT EXISTS hub_generations (
    generation         INTEGER PRIMARY KEY,
    publication_id     TEXT NOT NULL UNIQUE,
    published_at       INTEGER NOT NULL,
    frontier_digest    TEXT NOT NULL,
    control_revision   INTEGER NOT NULL,
    quarantine_count   INTEGER NOT NULL,
    indexed_generation INTEGER NOT NULL DEFAULT 0,
    pinned             INTEGER NOT NULL DEFAULT 0,
    frontier_json      TEXT NOT NULL DEFAULT '{}',
    authority_json     TEXT
);
CREATE INDEX IF NOT EXISTS idx_hub_generations_pub_id
    ON hub_generations(publication_id);
CREATE TABLE IF NOT EXISTS hub_session_heads (
    producer_id       TEXT NOT NULL,
    session_id        TEXT NOT NULL,
    selector          TEXT NOT NULL,
    gen_from          INTEGER NOT NULL,
    gen_to            INTEGER,
    harness           TEXT NOT NULL,
    revision          TEXT NOT NULL,
    title             TEXT,
    cwd               TEXT,
    model             TEXT,
    created_at        TEXT,
    updated_at        TEXT,
    message_count     INTEGER NOT NULL,
    epoch             TEXT NOT NULL,
    event_seq         INTEGER NOT NULL,
    payload_addr_json TEXT,
    custody_json      TEXT,
    metadata_json     TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY(producer_id, session_id, gen_from)
);
CREATE INDEX IF NOT EXISTS idx_hub_session_heads_live_updated
    ON hub_session_heads(gen_to, updated_at DESC, producer_id ASC, session_id ASC);
CREATE INDEX IF NOT EXISTS idx_hub_session_heads_selector
    ON hub_session_heads(selector, gen_to);
CREATE UNIQUE INDEX IF NOT EXISTS hub_open_session_head
    ON hub_session_heads(producer_id, session_id) WHERE gen_to IS NULL;
CREATE TABLE IF NOT EXISTS hub_pull_sources (
    target      TEXT PRIMARY KEY,
    producer_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS hub_index_changes (
    selector   TEXT PRIMARY KEY,
    generation INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS hub_index_changes_by_generation
    ON hub_index_changes(generation, selector);
CREATE TRIGGER IF NOT EXISTS hub_index_open AFTER INSERT ON hub_session_heads BEGIN
    INSERT INTO hub_index_changes VALUES(NEW.selector, NEW.gen_from)
    ON CONFLICT(selector) DO UPDATE SET generation = MAX(generation, excluded.generation);
END;
CREATE TRIGGER IF NOT EXISTS hub_index_close AFTER UPDATE OF gen_to ON hub_session_heads WHEN NEW.gen_to IS NOT NULL BEGIN
    INSERT INTO hub_index_changes VALUES(OLD.selector, NEW.gen_to)
    ON CONFLICT(selector) DO UPDATE SET generation = MAX(generation, excluded.generation);
END;
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
    ensure_migrated_with(conn, STATE_SCHEMA_VERSION, MIGRATIONS)
}

/// [`apply_migrations_with`], then [`converge`].
pub fn ensure_migrated_with(
    conn: &mut Connection,
    target_version: i64,
    migrations: &[(i64, &str, &str)],
) -> Result<(), StateError> {
    apply_migrations_with(conn, target_version, migrations)?;
    converge(conn)
}

/// Create the [`CONVERGED`] tables a store lacks and rebuild `custody_heads` when its watermark
/// no longer matches `pack_manifests`. One read on the fast path; independent of `user_version`.
pub fn converge(conn: &mut Connection) -> Result<(), StateError> {
    if super::tables::custody_heads_fresh(conn)? && super::tables::custody_commits_fresh(conn)? {
        return Ok(());
    }
    let db_dir = conn
        .path()
        .and_then(|p| std::path::Path::new(p).parent().map(std::path::Path::to_path_buf));
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(CONVERGED)?;
    if !super::tables::custody_heads_fresh(&tx)? {
        super::tables::rebuild_custody_heads(&tx)?;
    }
    if !super::tables::custody_commits_fresh(&tx)? {
        super::tables::rebuild_custody_commits(&tx, db_dir.as_deref())?;
    }
    tx.commit()?;
    Ok(())
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
        if name == "v7_hub_generational_mvcc" {
            super::hub::snapshot::migrate_snapshot_history(&tx)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![ver, name, now_ms],
        )?;
        tx.pragma_update(None, "user_version", ver)?;
    }

    tx.commit()?;
    Ok(())
}
