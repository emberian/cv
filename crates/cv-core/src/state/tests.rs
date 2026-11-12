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

    // Forward migration v1 -> v2 preserves every existing authoritative row.
    let mut extended = schema::MIGRATIONS.to_vec();
    extended.push((
        2,
        "v2_test_extension",
        "CREATE TABLE IF NOT EXISTS custom_v2_notes (note_id TEXT PRIMARY KEY, body TEXT NOT NULL);",
    ));
    {
        let mut conn = store.open_writable().unwrap();
        apply_migrations_with(&mut conn, 2, &extended).unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), 2);
    }

    // Opening a v2 store with a v1 binary (`STATE_SCHEMA_VERSION == 1`) fails closed without dropping data.
    let err = store.open_writable().unwrap_err();
    assert!(
        matches!(err, StateError::NewerSchema { found: 2, supported: 1 }),
        "unexpected error: {err:?}"
    );
    let ro_err = store.open_readonly().unwrap_err();
    assert!(matches!(ro_err, StateError::NewerSchema { found: 2, supported: 1 }));

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
