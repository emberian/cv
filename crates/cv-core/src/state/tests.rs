use super::schema::apply_migrations_with;
use super::tables::{current_process_identity, is_lane_run_alive};
use super::*;
use serde_json::{json, Map};
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::time::Instant;

fn tmp_state_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cv-state-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn bundled_sqlite_has_wal_reset_fix() {
    assert!(
        sqlite_version_number() >= 3_051_003,
        "expected bundled SQLite >= 3.51.3, got {} ({})",
        sqlite_version(),
        sqlite_version_number()
    );
}

#[test]
fn schema_migrations_are_forward_only_and_independent_of_source_version() {
    let dir = tmp_state_dir("migrations");
    let store = StateStore::at_dir(&dir);

    let prod = store
        .enroll_producer("prod-alpha", "mbp-local", &json!({"os": "macos"}))
        .unwrap();
    assert_eq!(prod.producer_id, "prod-alpha");

    let r1 = store.append("op-1", "sha256:aaa", &json!({"hello": "world"})).unwrap();
    assert_eq!(r1.local_commit_pos, 1);
    assert!(!r1.replayed);

    // Verify genuine v1 -> v2 migration preserves every v1 row and adds all v2 hub tables.
    let v1_dir = tmp_state_dir("migrate-v1-to-v2");
    {
        let mut raw_conn = rusqlite::Connection::open(v1_dir.join("state.db")).unwrap();
        apply_migrations_with(&mut raw_conn, 1, &schema::MIGRATIONS[..1]).unwrap();
        assert_eq!(current_schema_version(&raw_conn).unwrap(), 1);
        assert!(
            !raw_conn.table_exists(None, "task_events").unwrap(),
            "fresh v1 schema must not create zombie task_events"
        );
        // Simulate a historical pre-cleanup v1 store carrying stale task_events + task_sync_meta.
        raw_conn
            .execute_batch(
                "CREATE TABLE task_events (seq INTEGER PRIMARY KEY AUTOINCREMENT, raw_json TEXT NOT NULL);
                 CREATE INDEX idx_task_events_task_id ON task_events(seq);
                 CREATE TABLE task_sync_meta (slot INTEGER PRIMARY KEY);",
            )
            .unwrap();
        raw_conn
            .execute(
                "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
                 VALUES ('legacy-v1-prod', 'v1-host', '{}', 100, 100)",
                [],
            )
            .unwrap();
    }
    let v1_migrated = StateStore::at_dir(&v1_dir);
    {
        let conn = v1_migrated.open_writable().unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
        assert!(
            !conn.table_exists(None, "task_events").unwrap(),
            "forward migration must drop stale task_events"
        );
        assert!(
            !conn.table_exists(None, "task_sync_meta").unwrap(),
            "forward migration must drop stale task_sync_meta"
        );
        for name in [
            "hub_packs",
            "hub_ingested_events",
            "hub_quarantine",
            "hub_frontiers",
            "hub_epoch_sessions",
            "hub_frontier_packs",
            "hub_generations",
            "hub_session_heads",
            "hub_producer_credentials",
            "hub_pull_sources",
        ] {
            assert!(conn.table_exists(None, name).unwrap(), "missing migrated table {name}");
        }
        assert!(conn.column_exists(None, "hub_generations", "authority_json").unwrap());
    }
    assert_eq!(
        v1_migrated.get_producer("legacy-v1-prod").unwrap().unwrap().host_label,
        "v1-host"
    );
    std::fs::remove_dir_all(&v1_dir).ok();

    // #tooth Upgrade retained manifests including a subsequent deletion. Both cuts survive.
    let legacy = StateStore::at_dir(tmp_state_dir("retained-generations"));
    {
        let mut conn = rusqlite::Connection::open(legacy.path()).unwrap();
        apply_migrations_with(&mut conn, 6, &schema::MIGRATIONS[..6]).unwrap();
        let entry = json!({"producer_id":"p","session_id":"s","harness":"claude","revision":"first","epoch":"e","event_seq":1,"message_count":3});
        for (gen, revisions) in [(1, json!({"p/s":entry})), (2, json!({}))] {
            let manifest = json!({"hub_generation":gen,"publication_id":format!("pub-{gen}"),"published_at":gen,
                "origin_frontier":{"p/e":gen},"session_revisions":revisions,"pack_digests":{},"quarantine_count":0});
            conn.execute(
                "INSERT INTO hub_snapshots(hub_generation,publication_id,published_at,manifest_json,pinned)
                VALUES(?1,?2,?1,?3,1)",
                rusqlite::params![gen, format!("pub-{gen}"), manifest.to_string()],
            )
            .unwrap();
        }
    }
    drop(legacy.open_writable().unwrap());
    let hub = hub::HubStore::at_home(legacy.path().parent().unwrap());
    assert_eq!(hub.heads_at_generation(Some(1)).unwrap()[0].session_id, "s");
    assert!(hub.heads_at_generation(Some(2)).unwrap().is_empty());
    let old = hub
        .compute_authority(&hub::AuthorityQueryContext {
            as_of_gen: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(old.places[0].contiguous_seq, 1);
    assert_eq!(old.status, hub::AuthorityStatus::Partial);
    // Forward migration to STATE_SCHEMA_VERSION + 1 preserves every existing authoritative row.
    let next_ver = STATE_SCHEMA_VERSION + 1;
    let mut extended = schema::MIGRATIONS.to_vec();
    extended.push((
        next_ver,
        "v_next_test_extension",
        "CREATE TABLE IF NOT EXISTS custom_next_notes (note_id TEXT PRIMARY KEY, body TEXT NOT NULL);",
    ));
    {
        let mut conn = store.open_writable().unwrap();
        apply_migrations_with(&mut conn, next_ver, &extended).unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), next_ver);
    }

    // Opening a newer-schema store fails closed without dropping data.
    let err = store.open_writable().unwrap_err();
    assert!(
        matches!(
            err,
            StateError::NewerSchema { found, supported }
                if found == next_ver && supported == STATE_SCHEMA_VERSION
        ),
        "unexpected error: {err:?}"
    );
    let ro_err = store.open_readonly().unwrap_err();
    assert!(matches!(
        ro_err,
        StateError::NewerSchema { found, supported }
            if found == next_ver && supported == STATE_SCHEMA_VERSION
    ));

    // Resetting `user_version` back to 1 confirms nothing was dropped or corrupted.
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.pragma_update(None, "user_version", STATE_SCHEMA_VERSION).unwrap();
    }
    let (digest, replay_receipt) = store.get_operation_binding("op-1").unwrap().unwrap();
    assert_eq!(digest, "sha256:aaa");
    assert_eq!(replay_receipt.local_commit_pos, 1);
    assert_eq!(
        store.get_producer("prod-alpha").unwrap().unwrap().host_label,
        "mbp-local"
    );

    // Rebuilding `catalog.db` in the same `CLUSTERVISION_HOME` never touches `state.db`.
    {
        let _env = crate::env::Env::from_vars([("CLUSTERVISION_HOME", dir.as_os_str())]).enter();
        let cat_path = dir.join("catalog.db");
        let cat_conn = rusqlite::Connection::open(&cat_path).unwrap();
        cat_conn.pragma_update(None, "user_version", 1i64).unwrap();
        drop(cat_conn);
        let _rebuilt = crate::catalog::Writer::open().unwrap();
    }
    assert_eq!(store.inbox_count().unwrap(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn serverless_append_enforces_idempotency_conflict_rejection_and_fast_commit() {
    let dir = tmp_state_dir("serverless");
    let store = StateStore::at_dir(&dir).with_producer("node-1");

    let first = store
        .append("op-idempotent", "digest-v1", &json!({"kind": "turn", "n": 1}))
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(first.local_commit_pos, 1);
    assert_eq!(first.event_id.producer_id, "node-1");
    assert_eq!(first.event_id.seq, 1);

    // Retry with identical `(op_id, req_digest)` returns the exact original receipt without duplicating.
    let retry = store
        .append("op-idempotent", "digest-v1", &json!({"kind": "turn", "n": 1}))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.event_id, first.event_id);
    assert_eq!(retry.local_commit_pos, first.local_commit_pos);
    assert_eq!(store.inbox_count().unwrap(), 1);

    // Reusing `op-idempotent` with different bytes (`digest-v2`) is rejected as a conflict.
    let conflict = store
        .append("op-idempotent", "digest-v2", &json!({"kind": "turn", "n": 999}))
        .unwrap_err();
    assert!(
        matches!(
            conflict,
            StateError::OperationConflict {
                ref op_id,
                ref existing_digest,
                ref incoming_digest,
            } if op_id == "op-idempotent" && existing_digest == "digest-v1" && incoming_digest == "digest-v2"
        ),
        "unexpected error: {conflict:?}"
    );
    assert_eq!(store.inbox_count().unwrap(), 1);

    // Warm serverless commits run directly via `BEGIN IMMEDIATE` without discovery scans.
    let mut durations_us = Vec::with_capacity(9);
    for i in 0..9 {
        let t0 = Instant::now();
        let r = store
            .append(
                &format!("op-lat-{i}"),
                &format!("dig-{i}"),
                &json!({"seq": i, "msg": "fast"}),
            )
            .unwrap();
        durations_us.push(t0.elapsed().as_micros());
        assert!(!r.replayed);
    }
    durations_us.sort_unstable();
    let median_us = durations_us[durations_us.len() / 2];
    // Allow generous CI slack if disk fsync spikes, while asserting hot-path median stays tight.
    assert!(
        median_us < 25_000,
        "serverless commit unexpectedly slow: median={median_us}us samples={durations_us:?}"
    );
    assert_eq!(store.inbox_count().unwrap(), 10);

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn unix_socket_enforces_0600_mode_and_handles_paths_longer_than_sun_len() {
    use std::os::unix::fs::PermissionsExt;

    // 1. Short path: direct `<dir>/state.sock` with mode 0600.
    let short_dir = PathBuf::from(format!("/tmp/cvs-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&short_dir).unwrap();
    let short_store = StateStore::at_dir(&short_dir);
    let server = RecorderServer::start(short_store.clone(), "ipc-short").unwrap();
    let mode = std::fs::symlink_metadata(server.socket_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "socket must be owner-only 0600, got {mode:o}");
    let r = short_store.append("op-short", "d1", &json!({"ok": true})).unwrap();
    assert_eq!(r.event_id.epoch, server.epoch());
    server.shutdown();
    assert!(!short_store.socket_path().exists());
    std::fs::remove_dir_all(&short_dir).ok();

    // 2. Deep path (> 130 bytes, exceeding macOS 104-byte SUN_LEN): falls back to 0700 runtime dir.
    let deep_root = tmp_state_dir("deep");
    let deep_dir = deep_root
        .join("segment_alpha_0123456789_0123456789")
        .join("segment_beta_0123456789_0123456789")
        .join("segment_gamma_0123456789_0123456789");
    std::fs::create_dir_all(&deep_dir).unwrap();
    let deep_store = StateStore::at_dir(&deep_dir);
    assert!(deep_dir.join("state.sock").as_os_str().len() > 110);

    let deep_server = RecorderServer::start(deep_store.clone(), "ipc-deep").unwrap();
    let sock_meta = std::fs::symlink_metadata(deep_server.socket_path()).unwrap();
    assert_eq!(sock_meta.permissions().mode() & 0o777, 0o600);
    let r2 = deep_store.append("op-deep", "d2", &json!({"deep": true})).unwrap();
    assert_eq!(r2.event_id.epoch, deep_server.epoch());
    deep_server.shutdown();
    std::fs::remove_dir_all(&deep_root).ok();
}

#[cfg(unix)]
#[test]
fn crash_before_and_after_commit_handoff_preserves_exact_once_receipts() {
    let dir = tmp_state_dir("crash-handoff");
    let store = StateStore::at_dir(&dir).with_producer("fallback-prod");

    // 1. Owner crashes BEFORE `tx.commit()`: statement rolls back, socket drops without reply,
    // stale socket file remains on disk, client falls over to serverless and commits exactly once.
    let server1 = RecorderServer::start(store.clone(), "owner-1").unwrap();
    let epoch1 = server1.epoch().to_string();
    server1.set_fault_mode(FaultMode::CrashBeforeCommit);

    let receipt_pre = store
        .append("op-crash-before", "dig-before", &json!({"phase": "pre"}))
        .unwrap();
    assert!(
        !receipt_pre.replayed,
        "uncommitted batch rolled back, so serverless fallback performs the first commit"
    );
    assert_ne!(
        receipt_pre.event_id.epoch, epoch1,
        "rolled-back owner epoch must not own the serverless fallback commit"
    );
    assert_eq!(store.inbox_count().unwrap(), 1);
    drop(server1);

    // 2. Owner crashes AFTER `tx.commit()` (before sending socket reply): WAL has committed under
    // `epoch2`, socket drops, client retries seamlessly under ownership lock and gets the ORIGINAL
    // `epoch2` receipt idempotently (`replayed == true`) with zero duplicate inbox rows!
    let server2 = RecorderServer::start(store.clone(), "owner-2").unwrap();
    let epoch2 = server2.epoch().to_string();
    server2.set_fault_mode(FaultMode::CrashAfterCommitBeforeReply);

    let receipt_post = store
        .append("op-crash-after", "dig-after", &json!({"phase": "post"}))
        .unwrap();
    assert!(
        receipt_post.replayed,
        "already-committed WAL row must be recovered via operation_bindings on fallback"
    );
    assert_eq!(receipt_post.event_id.epoch, epoch2);
    assert_eq!(receipt_post.event_id.producer_id, "owner-2");
    assert_eq!(receipt_post.event_id.seq, 1);
    assert_eq!(store.inbox_count().unwrap(), 2);

    // Conflicting retry against the recovered post-crash `op_id` still fails closed.
    let conflict = store
        .append("op-crash-after", "tampered-digest", &json!({"phase": "evil"}))
        .unwrap_err();
    assert!(matches!(conflict, StateError::OperationConflict { .. }));
    assert_eq!(store.inbox_count().unwrap(), 2);

    drop(server2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fifty_concurrent_clients_survive_ipc_to_serverless_to_ipc_handoff() {
    let dir = tmp_state_dir("50-clients");
    let store = StateStore::at_dir(&dir).with_producer("serverless-peer");

    let server_a = RecorderServer::start(store.clone(), "ipc-owner-a").unwrap();
    let epoch_a = server_a.epoch().to_string();

    let n_clients = 50usize;
    let ops_per_client = 4usize;
    let barrier = Arc::new(Barrier::new(n_clients + 1));

    let results: Vec<Vec<DurableReceipt>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(n_clients);
        for client_idx in 0..n_clients {
            let client_store = store.clone();
            let b = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                b.wait();
                let mut my_receipts = Vec::with_capacity(ops_per_client);
                for k in 0..ops_per_client {
                    let op_id = format!("c{client_idx:02}-op{k}");
                    let digest = format!("sha256:{client_idx:02}:{k}");
                    let r = client_store
                        .append(&op_id, &digest, &json!({"client": client_idx, "k": k}))
                        .unwrap();
                    // Immediately verify idempotent retry and conflict rejection under contention!
                    if k == 1 {
                        let dup = client_store
                            .append(&op_id, &digest, &json!({"client": client_idx, "k": k}))
                            .unwrap();
                        assert!(dup.replayed);
                        assert_eq!(dup.event_id, r.event_id);
                        assert_eq!(dup.local_commit_pos, r.local_commit_pos);

                        let bad = client_store
                            .append(&op_id, "sha256:wrong", &json!({"forged": true}))
                            .unwrap_err();
                        assert!(matches!(bad, StateError::OperationConflict { .. }));
                    }
                    my_receipts.push(r);
                }
                my_receipts
            }));
        }

        // Release all 50 clients simultaneously, shut down `server_a` mid-flight so in-flight
        // callers transition to serverless `BEGIN IMMEDIATE`, then spin up `server_b` (`epoch_b`)!
        barrier.wait();
        std::thread::sleep(Duration::from_millis(3));
        server_a.shutdown();
        std::thread::sleep(Duration::from_millis(3));
        let server_b = RecorderServer::start(store.clone(), "ipc-owner-b").unwrap();
        assert_ne!(server_b.epoch(), epoch_a);

        let collected: Vec<Vec<DurableReceipt>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        server_b.shutdown();
        collected
    });

    let total_expected = n_clients * ops_per_client;
    let flat: Vec<DurableReceipt> = results.into_iter().flatten().collect();
    assert_eq!(flat.len(), total_expected);

    let mut positions: Vec<i64> = flat.iter().map(|r| r.local_commit_pos).collect();
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(
        positions,
        (1..=(total_expected as i64)).collect::<Vec<_>>(),
        "every logical operation must receive a unique contiguous local_commit_pos"
    );

    let event_ids: std::collections::HashSet<EventId> = flat.iter().map(|r| r.event_id.clone()).collect();
    assert_eq!(
        event_ids.len(),
        total_expected,
        "every committed operation must receive a unique (producer_id, epoch, seq)"
    );
    assert_eq!(store.inbox_count().unwrap(), total_expected as u64);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn publish_pack_range_is_atomic_preserves_epoch_monotonicity_and_enforces_custody() {
    let dir = tmp_state_dir("packs");
    let store = StateStore::at_dir(&dir);
    let server = RecorderServer::start(store.clone(), "packer-node").unwrap();

    for i in 1..=5 {
        let r = store
            .append(&format!("op-{i}"), &format!("d-{i}"), &json!({"i": i}))
            .unwrap();
        assert_eq!(r.local_commit_pos, i);
        assert_eq!(r.event_id.seq, i as u64);
    }
    server.shutdown();

    // Claiming uncommitted position 6 fails closed without deleting anything from `capture_inbox`.
    let bad_manifest = PackManifest {
        pack_id: "pack-bad".into(),
        min_commit_pos: 1,
        max_commit_pos: 6,
        record_count: 6,
        byte_len: 1024,
        digest: "sha256:bad".into(),
        pack_path: "packs/bad.pack.zst".into(),
        index_meta: json!({}),
        published_at: 1_700_000_000_000,
    };
    assert!(matches!(
        store.publish_pack_range(&bad_manifest).unwrap_err(),
        StateError::PackRangeMismatch(_)
    ));
    assert_eq!(store.inbox_count().unwrap(), 5);

    // Publish `[1..=3]` atomically: removes positions 1..=3 from inbox and registers manifest.
    let good_manifest = PackManifest {
        pack_id: "pack-001".into(),
        min_commit_pos: 1,
        max_commit_pos: 3,
        record_count: 3,
        byte_len: 4096,
        digest: "sha256:pack001".into(),
        pack_path: "packs/pack-001.zst".into(),
        index_meta: json!({"frames": 1}),
        published_at: 1_700_000_000_100,
    };
    assert_eq!(store.publish_pack_range(&good_manifest).unwrap(), 3);
    assert_eq!(store.inbox_count().unwrap(), 2);
    let remaining = store.read_inbox_range(1, 10).unwrap();
    assert_eq!(
        remaining.iter().map(|r| r.local_commit_pos).collect::<Vec<_>>(),
        vec![4, 5]
    );

    // Idempotent retry of the identical manifest returns `Ok(0)`.
    assert_eq!(store.publish_pack_range(&good_manifest).unwrap(), 0);

    // Overlapping pack `[3..=5]` is rejected.
    let overlap_manifest = PackManifest {
        pack_id: "pack-002".into(),
        min_commit_pos: 3,
        max_commit_pos: 5,
        record_count: 3,
        byte_len: 2048,
        digest: "sha256:pack002".into(),
        pack_path: "packs/pack-002.zst".into(),
        index_meta: json!({}),
        published_at: 1_700_000_000_200,
    };
    assert!(matches!(
        store.publish_pack_range(&overlap_manifest).unwrap_err(),
        StateError::PackRangeMismatch(_)
    ));

    // Custody ack gate: cannot evict `pack-001` before `vault-primary` acknowledges >= max_commit_pos 3.
    assert!(!store.is_pack_evictable("pack-001", &["vault-primary"]).unwrap());
    store
        .record_custody_ack(&CustodyAck {
            target_hub: "vault-primary".into(),
            producer_id: "packer-node".into(),
            epoch: remaining[0].epoch.clone(),
            contiguous_seq: 2,
            max_commit_pos: Some(2),
            pack_id: None,
            acked_at: 100,
        })
        .unwrap();
    assert!(!store.is_pack_evictable("pack-001", &["vault-primary"]).unwrap());

    store
        .record_custody_ack(&CustodyAck {
            target_hub: "vault-primary".into(),
            producer_id: "packer-node".into(),
            epoch: remaining[0].epoch.clone(),
            contiguous_seq: 4,
            max_commit_pos: Some(4),
            pack_id: Some("pack-001".into()),
            acked_at: 200,
        })
        .unwrap();
    // Out-of-order stale ack (`contiguous_seq: 1`) must NOT regress the watermark!
    store
        .record_custody_ack(&CustodyAck {
            target_hub: "vault-primary".into(),
            producer_id: "packer-node".into(),
            epoch: remaining[0].epoch.clone(),
            contiguous_seq: 1,
            max_commit_pos: Some(1),
            pack_id: None,
            acked_at: 150,
        })
        .unwrap();
    let acks = store.custody_acks_for("vault-primary").unwrap();
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].contiguous_seq, 4);
    assert_eq!(acks[0].max_commit_pos, Some(4));
    assert!(store.is_pack_evictable("pack-001", &["vault-primary"]).unwrap());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn frozen_mode_prevents_all_writes_and_reads_cleanly_with_or_without_db() {
    let base = tmp_state_dir("frozen");
    let missing_dir = base.join("never-created");
    let missing_store = StateStore::at_dir(&missing_dir);

    {
        let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
        assert_eq!(missing_store.inbox_count().unwrap(), 0);
        assert!(missing_store.list_producers().unwrap().is_empty());
        assert!(matches!(
            missing_store.append("op-1", "d-1", &json!({})).unwrap_err(),
            StateError::Frozen
        ));
        assert!(
            !missing_dir.exists(),
            "--frozen must never create missing state directories"
        );
    }

    let live_store = StateStore::at_dir(&base);
    live_store.append("op-live", "d-live", &json!({"x": 1})).unwrap();

    {
        let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
        assert_eq!(live_store.inbox_count().unwrap(), 1);
        assert!(live_store.get_operation_binding("op-live").unwrap().is_some());
        assert!(matches!(
            live_store.append("op-blocked", "d-blocked", &json!({})).unwrap_err(),
            StateError::Frozen
        ));
        assert!(matches!(
            live_store.enroll_producer("p", "h", &json!({})).unwrap_err(),
            StateError::Frozen
        ));
    }

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn receipts_resist_path_traversal_large_multibyte_utf8_and_detect_tampering() {
    let dir = tmp_state_dir("receipts");
    let store = StateStore::at_dir(&dir);

    // Malicious `session_id` with `../` traversal stays strictly inside parameterized SQLite rows.
    let malicious_session = "../../outside-escape/evil";
    let mut b1 = Map::new();
    b1.insert("kind".into(), json!("pre"));
    b1.insert("tool".into(), json!("Bash"));
    b1.insert("tool_use_id".into(), json!("tu-1"));
    // > 70 KiB of 4-byte UTF-8 emojis so a legacy 64 KiB `SeekFrom::End` + `read_to_string` would
    // slice mid-codepoint and fail with invalid UTF-8!
    b1.insert("note".into(), json!("🦀🌙".repeat(10_000)));
    b1.insert("input_sha256".into(), json!("aa11"));

    let rec1 = store.append_receipt(malicious_session, "claude", b1).unwrap();
    assert_eq!(rec1.seq, 1);
    assert_eq!(rec1.prev_hash, "");
    assert!(!dir.parent().unwrap().join("outside-escape").exists());

    let mut b2 = Map::new();
    b2.insert("kind".into(), json!("post"));
    b2.insert("tool".into(), json!("Bash"));
    b2.insert("tool_use_id".into(), json!("tu-1"));
    b2.insert("response_sha256".into(), json!("bb22"));

    let rec2 = store.append_receipt(malicious_session, "claude", b2).unwrap();
    assert_eq!(rec2.seq, 2);
    assert_eq!(rec2.prev_hash, rec1.receipt_hash);

    assert_eq!(store.verify_receipt_chain(malicious_session).unwrap(), 2);

    // Tamper with `body_json` in seq 1 via raw SQLite and confirm `verify_receipt_chain` catches it.
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE receipts SET body_json = replace(body_json, 'aa11', 'ff99') WHERE seq = 1",
            [],
        )
        .unwrap();
    }
    let err = store.verify_receipt_chain(malicious_session).unwrap_err().to_string();
    assert!(err.contains("tampered at seq 1"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lane_runs_verify_pid_start_time_liveness_and_lineage_handles_cycles() {
    let dir = tmp_state_dir("lanes-lineage");
    let store = StateStore::at_dir(&dir);

    let (pid, start_time) = current_process_identity();
    let live_run = LaneRun {
        run_id: "run-live".into(),
        session_id: "sess-1".into(),
        agent_id: Some("agent-1".into()),
        agent_type: Some("worker".into()),
        harness: Some("claude".into()),
        pid,
        proc_start_time: start_time,
        boot_id: None,
        transcript_path: None,
        agent_transcript_path: None,
        cwd: Some("/work".into()),
        started_at: 1_000,
        exited_at: None,
        exit_status: None,
        last_text: None,
    };
    store.record_lane_start(&live_run).unwrap();
    store.bind_lane_endpoint("run-live", "agent:worker-1").unwrap();
    assert!(is_lane_run_alive(&store.get_lane_run("run-live").unwrap().unwrap()));
    assert_eq!(
        store.latest_run_for_endpoint("agent:worker-1").unwrap().unwrap().run_id,
        "run-live"
    );

    // Simulate PID reuse: same live `pid`, different `proc_start_time` -> must NOT report alive!
    if let Some(actual_start) = start_time {
        let recycled = LaneRun {
            run_id: "run-recycled-pid".into(),
            proc_start_time: Some(actual_start.saturating_add(999_999)),
            ..live_run.clone()
        };
        assert!(
            !is_lane_run_alive(&recycled),
            "recycled PID with mismatched proc_start_time must report dead"
        );
    }

    assert!(store
        .record_lane_exit("run-live", 2_000, Some("ok"), Some("finished"))
        .unwrap());
    assert!(!is_lane_run_alive(&store.get_lane_run("run-live").unwrap().unwrap()));

    // Lineage edges with diamond + cycle terminate deterministically.
    for (src, dst, kind) in [
        ("s1", "s2", "parent_subagent"),
        ("s2", "s3", "distilled_from"),
        ("s1", "s3", "spliced_from"),
        ("s3", "s4", "adopted_into"),
        ("s4", "s2", "rewound_from"),
    ] {
        store
            .record_lineage_edge(&LineageEdge {
                src_session_id: src.into(),
                dst_session_id: dst.into(),
                edge_kind: kind.into(),
                harness: Some("claude".into()),
                tool_use_id: None,
                metadata: json!({}),
                created_at: 100,
            })
            .unwrap();
    }
    let desc = store.lineage_descendants("s1").unwrap();
    let reached: std::collections::BTreeSet<&str> = desc.iter().map(|e| e.dst_session_id.as_str()).collect();
    assert_eq!(reached.into_iter().collect::<Vec<_>>(), vec!["s2", "s3", "s4"]);

    let anc = store.lineage_ancestors("s4").unwrap();
    let origins: std::collections::BTreeSet<&str> = anc.iter().map(|e| e.src_session_id.as_str()).collect();
    assert_eq!(origins.into_iter().collect::<Vec<_>>(), vec!["s1", "s2", "s3", "s4"]);

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn batch_savepoint_rolls_back_failed_item_and_deep_tmpdir_stays_within_sun_len() {
    use std::os::unix::ffi::OsStrExt;

    let dir = tmp_state_dir("savepoint-sunlen");
    let store = StateStore::at_dir(&dir);

    // 1. Deep `TMPDIR` (> 80 bytes) plus deep `state.db` (> 96 bytes) must fall back to `/tmp/cv-usock-{uid}/...`
    //    keeping the socket path strictly <= 96 bytes (well under macOS's 104-byte `SUN_LEN`).
    let deep_db = dir.join("a".repeat(90)).join("state.db");
    let deep_store = StateStore::at(&deep_db);
    let deep_tmpdir = std::path::PathBuf::from(format!("/var/folders/{}", "x".repeat(80)));
    let fallback_sock = deep_store.socket_path_with_temp_dir(&deep_tmpdir);
    assert!(
        fallback_sock.to_string_lossy().starts_with("/tmp/cv-usock-"),
        "expected /tmp/cv-usock-* fallback, got {}",
        fallback_sock.display()
    );
    assert!(
        fallback_sock.as_os_str().as_bytes().len() <= 96,
        "socket path {} exceeded 96 bytes ({})",
        fallback_sock.display(),
        fallback_sock.as_os_str().as_bytes().len()
    );

    // 2. Per-item savepoint in `execute_batch`: if an item fails in SQLite AFTER incrementing
    //    `epoch_counters`, its savepoint rolls back cleanly without leaving a sequence gap or
    //    partial producer/counter state.
    {
        let conn = store.open_writable().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER abort_poison_op BEFORE INSERT ON capture_inbox
             WHEN NEW.op_id = 'poison-op'
             BEGIN
                 SELECT RAISE(ABORT, 'simulated capture_inbox failure');
             END;",
        )
        .unwrap();
    }

    let server = RecorderServer::start(store.clone(), "node-a").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for (op, dig) in [("good-1", "d1"), ("poison-op", "dp"), ("good-2", "d2")] {
        let st = store.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            st.append(op, dig, &json!({"op": op}))
        }));
    }
    let res: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let r1 = res[0].as_ref().expect("good-1 must commit");
    assert!(res[1].is_err(), "poison-op must fail and roll back its savepoint");
    let r2 = res[2].as_ref().expect("good-2 must commit");

    // Both `good-1` and `good-2` must get contiguous sequences `{1, 2}` (NEVER `3`), proving
    // `poison-op`'s `epoch_counters` increment rolled back cleanly inside the batch!
    let mut seqs = [r1.event_id.seq, r2.event_id.seq];
    seqs.sort_unstable();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(store.inbox_count().unwrap(), 2);

    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

fn dir_listing(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut names: Vec<(String, u64)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().unwrap().len(),
            )
        })
        .collect();
    names.sort();
    names
}

#[test]
fn frozen_reads_of_a_checkpointed_store_create_no_wal_or_shm_sidecars() {
    let base = tmp_state_dir("frozen-sidecars");
    let store = StateStore::at_dir(&base);
    store.append("op-live", "d-live", &json!({"x": 1})).unwrap();
    // The last writable connection closed: SQLite checkpointed and removed `-wal`/`-shm`.
    assert!(!base.join("state.db-wal").exists() && !base.join("state.db-shm").exists());
    let before = dir_listing(&base);
    let bytes_before = std::fs::read(base.join("state.db")).unwrap();

    {
        let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
        assert_eq!(store.inbox_count().unwrap(), 1);
        assert!(store.get_operation_binding("op-live").unwrap().is_some());
        assert!(store.list_producers().unwrap().len() <= 1);
        assert!(store.open_readonly().unwrap().is_some());
    }

    assert_eq!(
        dir_listing(&base),
        before,
        "--frozen reads must not create state.db-wal / state.db-shm"
    );
    assert_eq!(std::fs::read(base.join("state.db")).unwrap(), bytes_before);
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn frozen_reads_see_committed_wal_frames_without_touching_the_live_store() {
    let base = tmp_state_dir("frozen-live-wal");
    let store = StateStore::at_dir(&base);
    // A live writer holds the store open, so committed rows sit in `-wal` (not yet checkpointed).
    let writer = store.open_writable().unwrap();
    store.append("op-wal", "d-wal", &json!({"x": 2})).unwrap();
    assert!(std::fs::metadata(base.join("state.db-wal")).is_ok_and(|m| m.len() > 0));
    assert!(base.join("state.db-shm").exists());
    let before = dir_listing(&base);
    let shm_bytes_before = std::fs::read(base.join("state.db-shm")).unwrap();
    let shm_mtime_before = std::fs::metadata(base.join("state.db-shm"))
        .unwrap()
        .modified()
        .unwrap();

    {
        let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
        assert_eq!(
            store.inbox_count().unwrap(),
            1,
            "frozen read must see WAL-committed rows"
        );
        assert!(store.get_operation_binding("op-wal").unwrap().is_some());
    }

    assert_eq!(
        dir_listing(&base),
        before,
        "--frozen reads must not grow or reset the live WAL"
    );
    assert_eq!(
        std::fs::read(base.join("state.db-shm")).unwrap(),
        shm_bytes_before,
        "--frozen reads must not write reader marks into state.db-shm"
    );
    assert_eq!(
        std::fs::metadata(base.join("state.db-shm"))
            .unwrap()
            .modified()
            .unwrap(),
        shm_mtime_before,
        "--frozen reads must preserve state.db-shm mtime"
    );
    drop(writer);
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn readonly_sandbox_wal_store_reads_retirements_and_scales_concurrently() {
    use std::os::unix::fs::PermissionsExt;

    let live = tmp_state_dir("sandbox-wal-live");
    let store = StateStore::at_dir(&live);
    let writer = store.open_writable().unwrap();
    {
        let _seed = crate::env::Env::from_vars([("CLUSTERVISION_HOME", live.as_os_str())]).enter();
        crate::retired::retire("agent-sandboxed", Some("agent-successor"), Some("sess-parent")).unwrap();
    }
    assert!(std::fs::metadata(live.join("state.db-wal")).is_ok_and(|m| m.len() > 0));

    // Copy `state.db`, `state.db-wal`, and `state.db-shm` into a directory locked down to 0444/0555
    // (simulating a Seatbelt / read-only sandbox with uncheckpointed WAL retirements).
    let ro_dir = tmp_state_dir("sandbox-wal-ro");
    for name in ["state.db", "state.db-wal", "state.db-shm"] {
        let src = live.join(name);
        if src.exists() {
            let dst = ro_dir.join(name);
            std::fs::copy(&src, &dst).unwrap();
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
    }
    drop(writer);
    std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    // 1. Non-frozen `retired::current()` inside a read-only sandbox still sees WAL retirements.
    {
        let _env = crate::env::Env::from_vars([("CLUSTERVISION_HOME", ro_dir.as_os_str())]).enter();
        let map = crate::retired::current().unwrap();
        assert_eq!(
            map.get("sandboxed").and_then(|r| r.successor.as_deref()),
            Some("successor")
        );
    }

    // 2. 8 concurrent `--frozen` subagents read the WAL-backed read-only store cleanly without contention.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let ro_copy = ro_dir.clone();
            std::thread::spawn(move || {
                let _env = crate::env::Env::from_vars([
                    ("CLUSTERVISION_HOME", ro_copy.to_str().unwrap()),
                    ("CV_FROZEN", "true"),
                ])
                .enter();
                let map = crate::retired::current().unwrap();
                assert_eq!(
                    map.get("sandboxed").and_then(|r| r.successor.as_deref()),
                    Some("successor")
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // Restore permissions so cleanup succeeds.
    std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    for entry in std::fs::read_dir(&ro_dir).unwrap().flatten() {
        let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
    }
    std::fs::remove_dir_all(&live).ok();
    std::fs::remove_dir_all(&ro_dir).ok();
}

#[test]
fn frozen_reads_fold_a_retained_wal_without_shm_in_a_private_copy() {
    let live = tmp_state_dir("frozen-retained-live");
    let store = StateStore::at_dir(&live);
    let writer = store.open_writable().unwrap();
    store.append("op-wal", "d-wal", &json!({"x": 3})).unwrap();
    // A copy of the store taken while a writer was live: database + WAL frames, no `-shm`.
    let copied = tmp_state_dir("frozen-retained-copy");
    std::fs::copy(live.join("state.db"), copied.join("state.db")).unwrap();
    std::fs::copy(live.join("state.db-wal"), copied.join("state.db-wal")).unwrap();
    drop(writer);
    let before = dir_listing(&copied);
    let wal_before = std::fs::read(copied.join("state.db-wal")).unwrap();
    assert!(!wal_before.is_empty());

    {
        let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
        let copy = StateStore::at_dir(&copied);
        assert_eq!(
            copy.inbox_count().unwrap(),
            1,
            "rows committed only to the WAL must be read"
        );
        assert!(copy.get_operation_binding("op-wal").unwrap().is_some());
    }

    assert_eq!(
        dir_listing(&copied),
        before,
        "--frozen must not recover the WAL in place"
    );
    assert_eq!(std::fs::read(copied.join("state.db-wal")).unwrap(), wal_before);
    std::fs::remove_dir_all(&live).ok();
    std::fs::remove_dir_all(&copied).ok();
}

#[cfg(unix)]
#[test]
fn ipc_owner_reads_large_and_slow_frames_on_blocking_accepted_sockets() {
    use std::io::{BufRead, BufReader, Write};
    let dir = tmp_state_dir("ipc-large-frame");
    let store = StateStore::at_dir(&dir).with_producer("serverless-peer");
    let server = RecorderServer::start(store.clone(), "ipc-owner").unwrap();
    let epoch = server.epoch().to_string();

    // 1. A frame far larger than a Unix socket buffer: the owner must keep reading until the
    // newline instead of dropping the connection on the first short (would-block) read.
    let big = "x".repeat(4 * 1024 * 1024);
    let started = Instant::now();
    let receipt = store.append("op-big", "dig-big", &json!({"blob": big})).unwrap();
    assert_eq!(
        receipt.event_id.epoch, epoch,
        "the live IPC owner must commit the large frame"
    );
    assert!(!receipt.replayed);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "large frame took {:?}: the owner dropped it and the client spun on retries",
        started.elapsed()
    );

    // 2. A client that connects and only then (after a pause) sends its request line.
    let mut stream = std::os::unix::net::UnixStream::connect(server.socket_path()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    let req = AppendRequest::new("op-slow", "dig-slow", json!({"slow": true}));
    let mut line = serde_json::to_vec(&req).unwrap();
    line.push(b'\n');
    stream.write_all(&line).unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    assert!(
        reply.contains("Committed") && reply.contains(&epoch),
        "slow client must get a durable receipt from the owner, got {reply:?}"
    );

    assert_eq!(store.inbox_count().unwrap(), 2);
    drop(server);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn serverless_epochs_are_reused_per_producer_across_store_reopens() {
    let dir = tmp_state_dir("epoch-reuse");
    let mut first = None;
    for i in 0..20 {
        let store = StateStore::at_dir(&dir).with_producer("producer-a");
        let receipt = store.append(&format!("op-{i}"), "digest", &json!({})).unwrap();
        assert_eq!(receipt.event_id.seq, i + 1);
        assert_eq!(
            first.get_or_insert(receipt.event_id.epoch.clone()),
            &receipt.event_id.epoch
        );
    }
    let store = StateStore::at_dir(&dir).with_producer("producer-b");
    let other = store.append("other", "digest", &json!({})).unwrap();
    assert_ne!(first.unwrap(), other.event_id.epoch);
    assert_eq!(other.event_id.seq, 1);
    let conn = store.open_readonly().unwrap().unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM writer_epochs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
    drop(conn);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn direct_lane_liveness_rejects_previous_boot() {
    let run = LaneRun {
        run_id: "test".into(),
        session_id: "test".into(),
        agent_id: None,
        agent_type: None,
        harness: None,
        pid: Some(i64::from(std::process::id())),
        proc_start_time: super::tables::proc_start_time_of(i64::from(std::process::id())),
        boot_id: Some("previous-boot".into()),
        transcript_path: None,
        agent_transcript_path: None,
        cwd: None,
        started_at: 0,
        exited_at: None,
        exit_status: None,
        last_text: None,
    };
    assert!(!is_lane_run_alive(&run));
}

#[test]
fn append_receipt_with_inbox_commits_or_rolls_back_atomically_in_one_tx() {
    let dir = tmp_state_dir("atomic-receipt");
    let store = StateStore::at_dir(&dir);

    // Pre-bind `receipt:claude:sess-conflict:1` with a different digest so the inbox half fails.
    store
        .append("receipt:claude:sess-conflict:1", "wrong-digest", &json!({"x": 1}))
        .unwrap();

    let mut bad_body = serde_json::Map::new();
    bad_body.insert("kind".into(), json!("pre"));
    bad_body.insert("tool".into(), json!("Bash"));
    bad_body.insert("input_sha256".into(), json!("deadbeef"));
    let err = store
        .append_receipt_with_inbox("sess-conflict", "claude", bad_body)
        .unwrap_err();
    assert!(matches!(err, StateError::OperationConflict { .. }), "{err:?}");
    assert!(
        store.receipts_for_session("sess-conflict").unwrap().is_empty(),
        "failed capture_inbox append must roll back the receipts row in the same transaction"
    );

    // Clean session commits both `receipts` and `capture_inbox` together.
    let mut ok_body = serde_json::Map::new();
    ok_body.insert("kind".into(), json!("pre"));
    ok_body.insert("tool".into(), json!("Bash"));
    ok_body.insert("input_sha256".into(), json!("cafebabe"));
    let (rec, durable) = store.append_receipt_with_inbox("sess-ok", "claude", ok_body).unwrap();
    assert_eq!(rec.seq, 1);
    assert!(!durable.replayed);
    assert_eq!(store.receipts_for_session("sess-ok").unwrap().len(), 1);
    assert!(store
        .get_operation_binding("receipt:claude:sess-ok:1")
        .unwrap()
        .is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn writer_epochs_are_strictly_monotonic_uuid_v7_even_under_future_clock_skew() {
    let dir = tmp_state_dir("monotonic-epoch-v7");
    let store = StateStore::at_dir(&dir);

    let mut epochs = Vec::new();
    for _ in 0..32 {
        let ep = store.activate_epoch("prod-mono", "serverless").unwrap().epoch;
        let parsed = uuid::Uuid::parse_str(&ep).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
        epochs.push(ep);
    }
    for pair in epochs.windows(2) {
        assert!(
            pair[0] < pair[1],
            "expected strictly increasing UUIDv7 epochs: {} < {}",
            pair[0],
            pair[1]
        );
    }

    // Simulate a future-skewed UUIDv7 epoch already present in `writer_epochs`, then ensure
    // subsequent allocations still strictly advance past it while remaining valid UUIDv7.
    let conn = store.open_writable().unwrap();
    conn.execute(
        "INSERT INTO writer_epochs(epoch, producer_id, mode, pid, proc_start_time, activated_at, closed_at)
         VALUES ('ffffffff-ffff-7eff-bf00-000000000001', 'prod-mono', 'serverless', NULL, NULL, 0, NULL)",
        [],
    )
    .unwrap();
    let after_skew = super::tables::allocate_monotonic_epoch(&conn, "prod-mono").unwrap();
    drop(conn);
    let parsed_after = uuid::Uuid::parse_str(&after_skew).unwrap();
    assert_eq!(parsed_after.get_version_num(), 7);
    assert!(
        after_skew.as_str() > "ffffffff-ffff-7eff-bf00-000000000001",
        "allocated epoch {after_skew} did not advance past future-skewed epoch"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn is_pack_evictable_requires_custody_for_every_epoch_in_pack_range() {
    let dir = tmp_state_dir("evict-multi-epoch");
    let store = StateStore::at_dir(&dir).with_producer("prod-1");

    // Epoch 1 appends pos 1..=2 (`seq` 1..=2).
    let r1 = store.append("e1-op1", "d1", &json!({"n": 1})).unwrap();
    let r2 = store.append("e1-op2", "d2", &json!({"n": 2})).unwrap();
    assert_eq!((r1.local_commit_pos, r2.local_commit_pos), (1, 2));
    store.close_epoch(&r1.event_id.epoch).unwrap();

    // Epoch 2 appends pos 3..=4 (`seq` 1..=2).
    let r3 = store.append("e2-op1", "d3", &json!({"n": 3})).unwrap();
    let r4 = store.append("e2-op2", "d4", &json!({"n": 4})).unwrap();
    assert_ne!(r1.event_id.epoch, r3.event_id.epoch);
    assert!(r1.event_id.epoch < r3.event_id.epoch);
    assert_eq!((r3.local_commit_pos, r4.local_commit_pos), (3, 4));

    store
        .publish_pack_range(&PackManifest {
            pack_id: "pack-epoch-1".into(),
            min_commit_pos: 1,
            max_commit_pos: 2,
            record_count: 2,
            byte_len: 32,
            digest: "blake3:e1".into(),
            pack_path: "packs/pack-epoch-1.pack.zst".into(),
            index_meta: json!({}),
            published_at: 100,
        })
        .unwrap();
    store
        .publish_pack_range(&PackManifest {
            pack_id: "pack-epoch-2".into(),
            min_commit_pos: 3,
            max_commit_pos: 4,
            record_count: 2,
            byte_len: 32,
            digest: "blake3:e2".into(),
            pack_path: "packs/pack-epoch-2.pack.zst".into(),
            index_meta: json!({}),
            published_at: 101,
        })
        .unwrap();

    // Acknowledge ONLY epoch 2 (`max_commit_pos: 4`). Epoch 1 has zero custody acks!
    store
        .record_custody_ack(&CustodyAck {
            target_hub: "hub-a".into(),
            producer_id: "prod-1".into(),
            epoch: r3.event_id.epoch.clone(),
            contiguous_seq: 2,
            max_commit_pos: Some(4),
            pack_id: None,
            acked_at: 200,
        })
        .unwrap();

    assert!(
        !store.is_pack_evictable("pack-epoch-1", &["hub-a"]).unwrap(),
        "epoch 2 ack must NOT unlock unacknowledged epoch 1 pack!"
    );
    assert!(store.is_pack_evictable("pack-epoch-2", &["hub-a"]).unwrap());

    // Once epoch 1 reaches contiguous_seq >= 2, pack-epoch-1 becomes evictable too.
    store
        .record_custody_ack(&CustodyAck {
            target_hub: "hub-a".into(),
            producer_id: "prod-1".into(),
            epoch: r1.event_id.epoch.clone(),
            contiguous_seq: 2,
            max_commit_pos: Some(2),
            pack_id: None,
            acked_at: 201,
        })
        .unwrap();
    assert!(store.is_pack_evictable("pack-epoch-1", &["hub-a"]).unwrap());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn is_pack_evictable_matches_custom_replication_producer_id_via_writer_epochs() {
    let dir = tmp_state_dir("evict-custom-producer");
    let store = StateStore::at_dir(&dir).with_producer("local-default-prod");

    let r1 = store.append("op-1", "d1", &json!({"n": 1})).unwrap();
    store.close_epoch(&r1.event_id.epoch).unwrap();
    let r2 = store.append("op-2", "d2", &json!({"n": 2})).unwrap();
    assert_ne!(r1.event_id.epoch, r2.event_id.epoch);

    store
        .publish_pack_range(&PackManifest {
            pack_id: "pack-1".into(),
            min_commit_pos: 1,
            max_commit_pos: 1,
            record_count: 1,
            byte_len: 16,
            digest: "blake3:p1".into(),
            pack_path: "packs/pack-1.pack.zst".into(),
            index_meta: json!({}),
            published_at: 100,
        })
        .unwrap();
    store
        .publish_pack_range(&PackManifest {
            pack_id: "pack-2".into(),
            min_commit_pos: 2,
            max_commit_pos: 2,
            record_count: 1,
            byte_len: 16,
            digest: "blake3:p2".into(),
            pack_path: "packs/pack-2.pack.zst".into(),
            index_meta: json!({}),
            published_at: 101,
        })
        .unwrap();

    // Simulate `Shipper::sync` configured with custom `--producer-id custom-fleet-box`:
    // `custody_acks.producer_id` is `"custom-fleet-box"` rather than `"local-default-prod"`.
    store
        .record_custody_ack(&CustodyAck {
            target_hub: "team-hub".into(),
            producer_id: "custom-fleet-box".into(),
            epoch: r1.event_id.epoch.clone(),
            contiguous_seq: 1,
            max_commit_pos: Some(1),
            pack_id: None,
            acked_at: 200,
        })
        .unwrap();

    assert!(
        store.is_pack_evictable("pack-1", &["team-hub"]).unwrap(),
        "acknowledged pack shipped under custom --producer-id must be evictable"
    );
    assert!(
        !store.is_pack_evictable("pack-2", &["team-hub"]).unwrap(),
        "unacknowledged second epoch must remain non-evictable"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hub_v7_epoch_supersedes_pre_upgrade_v4_epoch_on_untimestamped_delete_in_any_arrival_order() {
    use super::hub::{compute_event_digest, HubStore, IngestBatchRequest, IngestEventRecord};

    // High-hex UUIDv4 epoch from 0.19.0 (`'f' > '0'`) vs post-upgrade 0.19.1 UUIDv7 (`"019..."`).
    let v4_epoch = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
    let v7_epoch = "019a7800-0000-7000-8000-000000000001";
    assert!(v4_epoch > v7_epoch, "lexicographically v4 sorts after v7");

    let upsert_env = json!({
        "kind": "session_upsert",
        "session_id": "sess-upgraded",
        "harness": "claude",
        "title": "Pre-upgrade session",
        "updated_at": "2026-11-12T10:00:00Z",
        "message_count": 3,
    });
    let upsert_rec = IngestEventRecord {
        event_id: EventId::new("prod-up", v4_epoch, 1),
        op_id: "op-v4-upsert".into(),
        digest: compute_event_digest("op-v4-upsert", &upsert_env, None).unwrap(),
        envelope: upsert_env,
        payload_addr: None,
    };

    // Untimestamped `session_delete` emitted after upgrading to 0.19.1 (`v7_epoch`).
    let delete_env = json!({
        "kind": "session_delete",
        "session_id": "sess-upgraded",
        "harness": "claude",
    });
    let delete_rec = IngestEventRecord {
        event_id: EventId::new("prod-up", v7_epoch, 1),
        op_id: "op-v7-delete".into(),
        digest: compute_event_digest("op-v7-delete", &delete_env, None).unwrap(),
        envelope: delete_env,
        payload_addr: None,
    };

    for v4_first in [true, false] {
        let dir = tmp_state_dir(if v4_first { "hub-v4-then-v7" } else { "hub-v7-then-v4" });
        let hub = HubStore::at_home(&dir);
        let first_batch = if v4_first {
            vec![upsert_rec.clone()]
        } else {
            vec![delete_rec.clone()]
        };
        let second_batch = if v4_first {
            vec![delete_rec.clone()]
        } else {
            vec![upsert_rec.clone()]
        };

        hub.ingest_batch(&IngestBatchRequest { events: first_batch }).unwrap();
        let resp = hub.ingest_batch(&IngestBatchRequest { events: second_batch }).unwrap();
        assert!(resp.publication_id.is_some());
        let snap = hub.latest_snapshot().unwrap().expect("snapshot published");
        assert!(
            !snap.session_revisions.contains_key("prod-up:sess-upgraded"),
            "0.19.1 UUIDv7 delete must supersede 0.19.0 UUIDv4 upsert (v4_first={v4_first}): {:?}",
            snap.session_revisions.keys().collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn frozen_checkpoint_connection_remains_a_snapshot_after_later_writes() {
    let base = tmp_state_dir("frozen-checkpoint-lifetime");
    let store = StateStore::at_dir(&base);
    store.append("before", "before", &json!({})).unwrap();
    let conn = {
        let _frozen = crate::env::Env::default().with_frozen().enter();
        store.open_readonly().unwrap().unwrap()
    };
    store.append("after", "after", &json!({})).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM capture_inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "a frozen connection must not follow later checkpoints");
    drop(conn);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn private_state_snapshot_admits_wal_growth_and_refuses_a_restarted_log() {
    let base = tmp_state_dir("frozen-copy-retry");
    let store = StateStore::at_dir(&base);
    let writer = store.open_writable().unwrap();
    store.append("before", "before", &json!({})).unwrap();
    let rows = |conn: &super::StateReadConnection| -> i64 {
        conn.query_row("SELECT count(*) FROM capture_inbox", [], |r| r.get(0))
            .unwrap()
    };

    // A commit appended to the same log generation after the copy leaves the copy a committed
    // state: one attempt, and the snapshot is the store as of the copy.
    let mut attempts = 0;
    let conn = super::open_private_folded_copy_with_check(&store.db_path, || {
        attempts += 1;
        store.append("after", "after", &json!({})).unwrap();
    })
    .unwrap();
    assert_eq!((attempts, rows(&conn)), (1, 1));
    drop(conn);

    // A checkpoint that restarts the log between the copies can pair an old database with a new
    // log. That attempt is discarded and the retry reads every commit.
    let restart = |id: &str| {
        writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        store.append(id, id, &json!({})).unwrap();
    };
    let mut attempts = 0;
    let conn = super::open_private_folded_copy_with_check(&store.db_path, || {
        attempts += 1;
        if attempts == 1 {
            restart("restarted");
        }
    })
    .unwrap();
    assert_eq!((attempts, rows(&conn)), (2, 3));
    drop(conn);

    let mut attempts = 0;
    let result = super::open_private_folded_copy_with_check(&store.db_path, || {
        attempts += 1;
        restart(&format!("racing-{attempts}"));
    });
    assert!(result.is_err(), "the final retry must not accept a restarted log");
    assert_eq!(attempts, 32);
    drop(writer);
    std::fs::remove_dir_all(base).unwrap();
}

struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Frozen opens race a writer that commits, checkpoints into the database file and restarts the
/// log. Each transaction (two inserts and two deletes over a bounded window) is either wholly in
/// a snapshot or absent from it.
#[test]
fn frozen_snapshots_are_committed_states_under_commit_and_checkpoint_churn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const WINDOW: i64 = 40;
    let base = tmp_state_dir("frozen-copy-churn");
    let store = StateStore::at_dir(&base);
    store
        .open_writable()
        .unwrap()
        .execute_batch(
            "CREATE TABLE left_side(n INTEGER PRIMARY KEY, pad BLOB);
             CREATE TABLE right_side(n INTEGER PRIMARY KEY, pad BLOB);",
        )
        .unwrap();
    let stop = AtomicBool::new(false);
    let (mut opened, mut refused, mut newest) = (0usize, 0usize, 0i64);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let writer = store.open_writable().unwrap();
            // Durability is not under test; fast commits race the copies harder.
            writer.execute_batch("PRAGMA synchronous = OFF;").unwrap();
            let mut n = 0i64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                // Several pages per commit, so a torn copy would show as a mismatch.
                writer
                    .execute_batch(&format!(
                        "BEGIN IMMEDIATE;
                         INSERT INTO left_side VALUES ({n}, randomblob(6000));
                         DELETE FROM left_side WHERE n <= {n} - {WINDOW};
                         INSERT INTO right_side VALUES ({n}, randomblob(6000));
                         DELETE FROM right_side WHERE n <= {n} - {WINDOW};
                         COMMIT;"
                    ))
                    .unwrap();
                match n % 11 {
                    0 => writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap(),
                    5 => writer.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").unwrap(),
                    _ => {}
                }
            }
        });
        // A failed assertion must still stop the writer, or the scope never joins.
        let _stop = StopOnDrop(&stop);
        let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
        for round in 0..200u32 {
            let conn = match store.open_readonly() {
                Ok(conn) => conn.unwrap(),
                Err(_) => {
                    refused += 1;
                    continue;
                }
            };
            opened += 1;
            let side = |table: &str| -> (i64, i64, i64) {
                conn.query_row(
                    &format!("SELECT count(*), coalesce(min(n), 0), coalesce(max(n), 0) FROM {table}"),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
            };
            let (count, oldest, latest) = side("left_side");
            assert_eq!(
                side("right_side"),
                (count, oldest, latest),
                "half of a transaction is visible"
            );
            assert_eq!(
                count,
                latest.min(WINDOW),
                "rows are missing from the window ending at {latest}"
            );
            assert!(
                latest >= newest,
                "a later snapshot went back in time: {latest} < {newest}"
            );
            newest = latest;
            if round.is_multiple_of(20) {
                let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
                assert_eq!(check, "ok");
            }
        }
    });
    eprintln!("frozen churn: {opened} snapshots, {refused} refused, {newest} commits seen");
    assert!(opened >= 150, "{refused} of 200 frozen opens were refused under churn");
    assert!(newest > 0);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn frozen_state_backing_is_owned_until_connection_closes() {
    let world = crate::test_world("state-read-owner");
    let temporary = world.base.join("private-snapshots");
    std::fs::create_dir(&temporary).unwrap();
    let store = StateStore::at(world.cv_home.join("state.db"));
    store.append("before", "before", &json!({})).unwrap();
    let before = std::fs::read(store.path()).unwrap();
    let before_identity = crate::source::FileId::at(store.path());
    let conn = {
        let _frozen = crate::env::Env::from_vars([("TMPDIR", temporary.as_os_str())])
            .with_frozen()
            .enter();
        store.open_readonly().unwrap().unwrap()
    };
    let backing = conn._snapshot.as_ref().unwrap().path().to_owned();
    #[cfg(windows)]
    assert!(
        backing.join("state.db").exists(),
        "Windows keeps the open backing until close"
    );
    #[cfg(unix)]
    assert!(
        !backing.exists(),
        "Unix must preserve immediate unlink for crash-safe cleanup"
    );
    let count: i64 = conn
        .query_row("SELECT count(*) FROM capture_inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert!(conn.execute("DELETE FROM capture_inbox", []).is_err());
    drop(conn);
    assert!(
        !backing.exists(),
        "close SQLite before removing its backing, including on Windows"
    );
    assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert_eq!(crate::source::FileId::at(store.path()), before_identity);
}

#[test]
fn rejected_state_snapshot_releases_its_private_backing() {
    let world = crate::test_world("state-read-owner-failure");
    let temporary = world.base.join("private-snapshots");
    std::fs::create_dir(&temporary).unwrap();
    let store = StateStore::at(world.cv_home.join("state.db"));
    std::fs::write(store.path(), b"not a SQLite database").unwrap();
    let _frozen = crate::env::Env::from_vars([("TMPDIR", temporary.as_os_str())])
        .with_frozen()
        .enter();
    assert!(store.open_readonly().is_err());
    assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
    assert_eq!(std::fs::read(store.path()).unwrap(), b"not a SQLite database");
}

/// #tooth A v4 store (hook observations, no file versions) upgrades in place to v5 and then
/// admits a `pre` receipt carrying file versions; expiry advances the forgotten-time horizon.
#[test]
fn state_v4_upgrades_to_v5_and_records_pre_receipt_files() {
    let dir = tmp_state_dir("migrate-v4-to-v5");
    {
        let mut raw = rusqlite::Connection::open(dir.join("state.db")).unwrap();
        apply_migrations_with(&mut raw, 4, &schema::MIGRATIONS[..4]).unwrap();
        assert_eq!(current_schema_version(&raw).unwrap(), 4);
        raw.execute(
            "INSERT INTO task_folds(slot, upto_seq, model_json, applied_json, garbage_json, refused_json)
             VALUES (1, 0, '{}', '{}', '[]', '[]')",
            [],
        )
        .unwrap();
    }
    let store = StateStore::at_dir(&dir);
    let file = |blob: &str| json!({"path": "/w/src/lib.rs", "blob": blob, "edit": true, "span": [0, 4]});
    let body = json!({"kind": "pre", "tool": "Edit", "tool_use_id": "call-1", "files": [file("b0")]});
    store
        .append_receipt_with_inbox("sess-v4", "codex", body.as_object().unwrap().clone())
        .unwrap();
    let conn = store.open_writable().unwrap();
    assert_eq!(current_schema_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
    let folds: i64 = conn
        .query_row("SELECT COUNT(*) FROM task_folds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(folds, 0, "v5 drops cached task folds so new projection fields replay");
    let pending: (String, String, i64, i64) = conn
        .query_row(
            "SELECT call, base, lo, hi FROM pending_file_versions WHERE session='sess-v4'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(pending, ("call-1".into(), "b0".into(), 0, 4));

    // An unanswered call is superseded by the same writer's next call on the same path, and an
    // expired row moves the horizon forward without ever moving it back.
    conn.execute(
        "UPDATE pending_file_versions SET started = unixepoch() - 90000 WHERE call='call-1'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pending_file_versions(harness,session,agent,call,path,edit,base,started)
         VALUES ('codex','other','','old','/w/x',1,'b',unixepoch() - 100000)",
        [],
    )
    .unwrap();
    // A peer's edit that never answered, twenty minutes old, no longer blinds the path.
    conn.execute(
        "INSERT INTO pending_file_versions(harness,session,agent,call,path,edit,base,started)
         VALUES ('codex','peer','','quiet','/w/src/lib.rs',1,'b0',unixepoch() - 1200)",
        [],
    )
    .unwrap();
    drop(conn);
    let body = json!({"kind": "pre", "tool": "Edit", "tool_use_id": "call-2", "files": [file("b1")]});
    store
        .append_receipt_with_inbox("sess-v4", "codex", body.as_object().unwrap().clone())
        .unwrap();
    let conn = store.open_writable().unwrap();
    let calls: Vec<String> = conn
        .prepare("SELECT call FROM pending_file_versions ORDER BY call")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(calls, ["call-2", "quiet"]);
    let ambiguous: i64 = conn
        .query_row(
            "SELECT ambiguous FROM pending_file_versions WHERE call='call-2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ambiguous, 0);
    let horizon: i64 = conn
        .query_row(
            "SELECT newest_forgotten_time FROM horizons WHERE name='pending_file_versions'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!((now - 90010..=now - 89990).contains(&horizon), "{horizon} vs {now}");
    std::fs::remove_dir_all(&dir).ok();
}

/// #tooth `custody_heads` converges by presence, not by `user_version`: a store whose numbers
/// other migrations already claimed (here 4 and 5) and that lacks the table gains it, filled
/// from every published manifest, on its next writable open; under `--frozen` a store without
/// the table, or with a stale one, reads as "fall back to the manifests", never as an error.
#[test]
fn custody_heads_converge_on_a_store_whose_version_numbers_are_taken() {
    let dir = tmp_state_dir("custody-converge");
    let store = StateStore::at_dir(&dir);
    let server = RecorderServer::start(store.clone(), "converge-node").unwrap();
    for i in 1..=2 {
        store
            .append(&format!("op-{i}"), &format!("d-{i}"), &json!({"i": i}))
            .unwrap();
    }
    server.shutdown();
    let entry = json!({
        "producer_id": "prod-a", "harness": "claude", "id": "s1", "message_count": 2,
        "archived_at": "2026-11-24T00:00:00Z", "content_blake3": "00".repeat(32),
        "addr": {"pack_id": "", "decoded_offset": 0, "length": 4, "blake3_digest": "00".repeat(32)}
    });
    store
        .publish_pack_range(&PackManifest {
            pack_id: "pack-c1".into(),
            min_commit_pos: 1,
            max_commit_pos: 2,
            record_count: 2,
            byte_len: 10,
            digest: "sha256:c1".into(),
            pack_path: "packs/pack-c1.pack.zst".into(),
            index_meta: json!({"sessions": [entry]}),
            published_at: 1,
        })
        .unwrap();
    let heads = store
        .custody_heads()
        .unwrap()
        .expect("heads are fresh after a publication");
    assert_eq!(heads.len(), 1);
    assert_eq!(
        heads[0]["addr"]["pack_id"], "pack-c1",
        "the physical pack id is filled in"
    );

    // Another branch of migrations took higher versions; this store never had the table.
    let v_next = STATE_SCHEMA_VERSION + 1;
    let v_after = STATE_SCHEMA_VERSION + 2;
    let taken = |conn: &rusqlite::Connection| {
        conn.execute_batch(
            "DROP TABLE custody_heads; DROP TABLE custody_heads_through;
             CREATE TABLE IF NOT EXISTS future_a(id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS future_b(id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", v_after).unwrap();
    };
    let tables = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('custody_heads', 'custody_heads_through')",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let mut later = schema::MIGRATIONS.to_vec();
    later.push((
        v_next,
        "v_next_elsewhere",
        "CREATE TABLE IF NOT EXISTS future_a(id INTEGER PRIMARY KEY);",
    ));
    later.push((
        v_after,
        "v_after_elsewhere",
        "CREATE TABLE IF NOT EXISTS future_b(id INTEGER PRIMARY KEY);",
    ));
    {
        let mut conn = rusqlite::Connection::open(store.path()).unwrap();
        taken(&conn);
        assert_eq!(tables(&conn), 0);
        schema::ensure_migrated_with(&mut conn, v_after, &later).unwrap();
        assert_eq!(
            current_schema_version(&conn).unwrap(),
            v_after,
            "the version is left alone"
        );
        assert_eq!(tables(&conn), 2);
        let selector: String = conn
            .query_row("SELECT selector FROM custody_heads", [], |r| r.get(0))
            .unwrap();
        assert!(!selector.is_empty(), "the head was rebuilt from the published manifest");
        // Converging again is a no-op read.
        schema::ensure_migrated_with(&mut conn, v_after, &later).unwrap();
        assert_eq!(tables(&conn), 2);
    }

    // Store stamped with a much newer `user_version = 99`: `custody_heads` returns `Ok(None)`,
    // and under `--frozen` both `head_entries` and `inventory` fall back to `pack_manifests`
    // without erroring on `NewerSchema`.
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 99;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    }
    assert_eq!(
        store.custody_heads().unwrap(),
        None,
        "newer schema falls back to manifests"
    );
    {
        let _frozen = crate::env::Env::from_vars([("CLUSTERVISION_HOME", dir.as_os_str())])
            .with_frozen()
            .enter();
        assert_eq!(
            store.custody_heads().unwrap(),
            None,
            "frozen newer schema falls back to manifests"
        );
        assert_eq!(crate::custody::head_entries(&dir).unwrap().len(), 1);
        assert_eq!(crate::custody::inventory().unwrap().len(), 1);
    }

    // The same store at this binary's version without `custody_heads`: frozen reads fall back,
    // and on a checkpointed store without `-wal`/`-journal`, neither `custody_heads` nor
    // `inventory` copies `state.db` into `TMPDIR`.
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.execute_batch(
            "DROP TABLE custody_heads; DROP TABLE custody_heads_through;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", STATE_SCHEMA_VERSION).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let _ = std::fs::remove_file(dir.join("state.db-wal"));
    let _ = std::fs::remove_file(dir.join("state.db-shm"));
    {
        let _frozen = crate::env::Env::default().with_frozen().enter();
        assert_eq!(store.custody_heads().unwrap(), None, "absent table: read the manifests");
    }
    drop(store.open_writable().unwrap());
    assert_eq!(store.custody_heads().unwrap().map(|h| h.len()), Some(1));
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let _ = std::fs::remove_file(dir.join("state.db-wal"));
    let _ = std::fs::remove_file(dir.join("state.db-shm"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked_tmp = dir.join("locked-tmp");
        std::fs::create_dir_all(&locked_tmp).unwrap();
        std::fs::set_permissions(&locked_tmp, std::fs::Permissions::from_mode(0o500)).unwrap();
        let _frozen = crate::env::Env::from_vars([
            ("CLUSTERVISION_HOME", dir.as_os_str()),
            ("TMPDIR", locked_tmp.as_os_str()),
        ])
        .with_frozen()
        .enter();
        let heads = store.custody_heads();
        let inv = crate::custody::inventory();
        std::fs::set_permissions(&locked_tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            heads.unwrap().map(|h| h.len()),
            Some(1),
            "checkpointed frozen read must not copy to TMPDIR"
        );
        assert_eq!(
            inv.unwrap().len(),
            1,
            "checkpointed frozen inventory must not copy to TMPDIR"
        );
    }

    // A manifest published by a binary that does not keep the heads leaves them stale: frozen
    // readers fall back, the next writable open rebuilds.
    {
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        conn.execute(
            "INSERT INTO pack_manifests(pack_id, min_commit_pos, max_commit_pos, record_count, byte_len,
                 digest, pack_path, index_meta_json, published_at)
             VALUES ('pack-c2', 3, 3, 0, 1, 'sha256:c2', 'packs/pack-c2.pack.zst', ?1, 2)",
            [
                json!({"sessions": [json!({"producer_id": "prod-a", "harness": "claude", "id": "s2",
                "message_count": 1, "archived_at": "2026-11-24T00:00:00Z",
                "content_blake3": "00".repeat(32),
                "addr": {"pack_id": "", "decoded_offset": 0, "length": 4,
                         "blake3_digest": "00".repeat(32)}})]})
                .to_string(),
            ],
        )
        .unwrap();
    }
    {
        let _frozen = crate::env::Env::default().with_frozen().enter();
        assert_eq!(store.custody_heads().unwrap(), None, "stale heads: read the manifests");
    }
    drop(store.open_writable().unwrap());
    assert_eq!(store.custody_heads().unwrap().map(|h| h.len()), Some(2));
    std::fs::remove_dir_all(&dir).ok();
}

/// #tooth Checkpointed `--frozen` bounded queries across `state/tables.rs` read `state.db` in place
/// via `immutable=1` without copying `state.db` into `TMPDIR`.
#[test]
#[cfg(unix)]
fn frozen_bounded_table_queries_read_checkpointed_state_without_tmpdir_copies() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp_state_dir("frozen-zero-copy-tables");
    let store = StateStore::at_dir(&dir);
    let prod = store.enroll_producer("prod-z", "host-z", &json!({})).unwrap();
    let ep = store.activate_epoch(&prod.producer_id, "serverless").unwrap();
    let receipt = store.append("op-z", "dig-z", &json!({"k": 1})).unwrap();
    store
        .record_lane_start(&tables::LaneRun {
            run_id: "run-z".into(),
            session_id: "sess-z".into(),
            agent_id: Some("agent-z".into()),
            agent_type: None,
            harness: Some("claude".into()),
            pid: None,
            proc_start_time: None,
            boot_id: None,
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: 100,
            exited_at: Some(200),
            exit_status: Some("ok".into()),
            last_text: None,
        })
        .unwrap();
    store.bind_lane_endpoint("run-z", "lane:z").unwrap();
    store
        .append_receipt("sess-z", "claude", json!({"tool": "Read"}).as_object().unwrap().clone())
        .unwrap();
    store
        .record_lineage_edge(&tables::LineageEdge {
            src_session_id: "sess-p".into(),
            dst_session_id: "sess-z".into(),
            edge_kind: "continues".into(),
            harness: Some("claude".into()),
            tool_use_id: None,
            metadata: json!({}),
            created_at: 1,
        })
        .unwrap();
    store.checkpoint_wal(true).unwrap();
    let _ = std::fs::remove_file(dir.join("state.db-wal"));
    let _ = std::fs::remove_file(dir.join("state.db-shm"));

    let locked_tmp = dir.join("locked-tmp");
    std::fs::create_dir_all(&locked_tmp).unwrap();
    std::fs::set_permissions(&locked_tmp, std::fs::Permissions::from_mode(0o500)).unwrap();
    let checks = (|| -> Result<(), StateError> {
        let _frozen = crate::env::Env::from_vars([
            ("CLUSTERVISION_HOME", dir.as_os_str()),
            ("TMPDIR", locked_tmp.as_os_str()),
        ])
        .with_frozen()
        .enter();
        assert_eq!(store.session_whispers("claude", "sess-z")?.len(), 0);
        assert_eq!(store.session_baggage("claude", "sess-z")?, None);
        assert_eq!(store.get_producer("prod-z")?.unwrap().host_label, "host-z");
        assert!(!store.list_producers()?.is_empty());
        assert_eq!(store.get_epoch(&ep.epoch)?.unwrap().producer_id, "prod-z");
        assert_eq!(store.get_operation_binding("op-z")?.unwrap().0, "dig-z");
        assert_eq!(
            store.list_operation_bindings_range(1, receipt.local_commit_pos)?.len(),
            1
        );
        assert_eq!(store.read_inbox_range(1, receipt.local_commit_pos)?.len(), 1);
        assert_eq!(store.inbox_count()?, 1);
        assert_eq!(store.custody_acks_for("hub-z")?.len(), 0);
        assert!(!store.is_pack_evictable("missing", &[])?);
        assert_eq!(store.get_lane_run("run-z")?.unwrap().session_id, "sess-z");
        assert_eq!(store.list_lane_runs_for_session("sess-z")?.len(), 1);
        assert_eq!(store.endpoints_for_run("run-z")?.len(), 1);
        assert_eq!(store.latest_run_for_endpoint("lane:z")?.unwrap().run_id, "run-z");
        assert_eq!(store.receipts_for_session("sess-z")?.len(), 1);
        assert_eq!(store.lineage_from("sess-p")?.len(), 1);
        assert_eq!(store.lineage_to("sess-z")?.len(), 1);
        assert_eq!(store.lineage_descendants("sess-p")?.len(), 1);
        assert_eq!(store.lineage_ancestors("sess-z")?.len(), 1);
        Ok(())
    })();
    std::fs::set_permissions(&locked_tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
    checks.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
