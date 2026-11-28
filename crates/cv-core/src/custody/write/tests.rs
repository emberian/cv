use super::*;

#[test]
fn interleaved_non_archive_inbox_rows_survive_contiguous_pack_publish_and_retry() {
    let world = crate::test_world("archive-interleaved-inbox");
    let archive = Archive {
        home: world.cv_home.clone(),
    };
    let store = archive.state_store();
    let producer = store.default_producer().to_string();

    // Build one physical sealed pack containing two session records (`s-one` and `s-two`).
    let packs_dir = archive.packs_dir();
    let pack_id = "pack-interleaved-test";
    let mut builder = PackBuilder::create(&packs_dir, pack_id).unwrap();
    let payload_1 = br#"{"id":"s-one","harness":"claude","messages":[]}"#;
    let payload_2 = br#"{"id":"s-two","harness":"claude","messages":[]}"#;
    let addr_1 = builder
        .append_record(record_anchor(&producer, "claude", "s-one"), payload_1)
        .unwrap();
    let addr_2 = builder
        .append_record(record_anchor(&producer, "claude", "s-two"), payload_2)
        .unwrap();
    let pfm = builder.seal(Claim::New).unwrap();
    let zst_bytes = std::fs::read(pack_zst_path(&packs_dir, pack_id).unwrap()).unwrap();
    let rel_pack_path = format!("packs/{pack_id}.pack.zst");
    let pack_blake3 = crate::blake3_hex(&zst_bytes);
    let now = Utc::now();

    // Simulate an earlier failed/aborted pack (`pack-old`) that left `s-one` at `pos = 1`,
    // followed by a published pack at `pos = 2`, then our new sync pass appending `s-one`
    // (`pos = 3`), a concurrent `cv hook receipt` interleaving at `pos = 4`, and `s-two`
    // appending at `pos = 5`.
    let stale_req = AppendRequest::new(
        format!("archive:pack-old:{producer}:claude:s-one"),
        crate::blake3_hex(payload_1),
        json!({"kind": "session_archive", "session_id": "s-one"}),
    );
    let pos_stale = store.append_request(&stale_req).unwrap().local_commit_pos;
    assert_eq!(pos_stale, 1);

    let mid_req = AppendRequest::new(
        format!("archive:pack-mid:{producer}:claude:s-mid"),
        "deadbeef",
        json!({"kind": "session_archive", "session_id": "s-mid"}),
    );
    let pos_mid = store.append_request(&mid_req).unwrap().local_commit_pos;
    assert_eq!(pos_mid, 2);
    store
        .publish_pack_range(&StatePackManifest {
            pack_id: "pack-mid".into(),
            min_commit_pos: 2,
            max_commit_pos: 2,
            record_count: 1,
            byte_len: 1,
            digest: "blake3:00".into(),
            pack_path: "packs/pack-mid.pack.zst".into(),
            index_meta: json!({"sessions": []}),
            published_at: now.timestamp_millis(),
        })
        .unwrap();

    // Pack-scoped `op_id` gets a fresh `local_commit_pos = 3` (no retry wedge across `pos = 2`!).
    let r1 = store
        .append_request(&AppendRequest::new(
            format!("archive:{pack_id}:{producer}:claude:s-one"),
            crate::blake3_hex(payload_1),
            json!({"kind": "session_archive", "session_id": "s-one"}),
        ))
        .unwrap();
    assert_eq!(r1.local_commit_pos, 3);

    // Concurrent `cv hook receipt` lands right in the middle at `pos = 4`!
    let hook_rec = store
        .append(
            "receipt:claude:live-sess:1",
            "hash-1",
            &json!({"kind": "tool_receipt", "session_id": "live-sess", "seq": 1}),
        )
        .unwrap();
    assert_eq!(hook_rec.local_commit_pos, 4);

    let r2 = store
        .append_request(&AppendRequest::new(
            format!("archive:{pack_id}:{producer}:claude:s-two"),
            crate::blake3_hex(payload_2),
            json!({"kind": "session_archive", "session_id": "s-two"}),
        ))
        .unwrap();
    assert_eq!(r2.local_commit_pos, 5);

    let entry_1 = ArchivedSessionEntry {
        profile: Default::default(),
        producer_id: producer.clone(),
        harness: "claude".into(),
        id: "s-one".into(),
        title: Some("One".into()),
        cwd: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
        archived_at: now,
        content_blake3: crate::blake3_hex(payload_1),
        source_len: None,
        source_mtime_ns: None,
        baseline: None,
        addr: addr_1,
        custody: Default::default(),
    };
    let entry_2 = ArchivedSessionEntry {
        id: "s-two".into(),
        title: Some("Two".into()),
        content_blake3: crate::blake3_hex(payload_2),
        addr: addr_2,
        custody: Default::default(),
        ..entry_1.clone()
    };

    publish_contiguous_archive_runs(
        &store,
        pack_id,
        &rel_pack_path,
        zst_bytes.len() as u64,
        &pack_blake3,
        &pfm,
        now.timestamp_millis(),
        vec![
            CommittedArchiveRecord {
                local_commit_pos: r1.local_commit_pos,
                epoch: r1.event_id.epoch,
                seq: r1.event_id.seq,
                op_id: format!("archive:{pack_id}:{producer}:claude:s-one"),
                envelope: json!({"kind": "session_archive", "session_id": "s-one"}),
                entry: entry_1,
            },
            CommittedArchiveRecord {
                local_commit_pos: r2.local_commit_pos,
                epoch: r2.event_id.epoch,
                seq: r2.event_id.seq,
                op_id: format!("archive:{pack_id}:{producer}:claude:s-two"),
                envelope: json!({"kind": "session_archive", "session_id": "s-two"}),
                entry: entry_2,
            },
        ],
    )
    .unwrap();

    // Interleaved `pos = 4` (`tool_receipt`) and unreferenced `pos = 1` MUST remain in `capture_inbox`,
    // while `pos = 3` and `pos = 5` were cleanly drained into `pack_manifests`!
    let remaining = store.read_inbox_range(1, 10).unwrap();
    let remaining_pos: Vec<i64> = remaining.iter().map(|e| e.local_commit_pos).collect();
    assert_eq!(remaining_pos, vec![1, 4]);
    assert_eq!(remaining[1].op_id, "receipt:claude:live-sess:1");

    // `entries()` reads both sub-runs (`pack_id` and `pack_id--r1`), validates `.pack.idx.json`,
    // and counts distinct physical `pack_path`s (`pack-mid` + `pack-interleaved-test` = 2).
    let (rows, pack_count) = archive.entries().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(pack_count, 2);
    assert!(crate::custody::read_session_bytes(&archive.home, "claude", "s-one")
        .unwrap()
        .is_some());
    assert!(crate::custody::read_session_bytes(&archive.home, "claude", "s-two")
        .unwrap()
        .is_some());
}

#[test]
fn watch_candidates_and_sync_refs_track_directory_sessions_and_multi_session_exports() {
    use crate::Adapter as _;
    let world = crate::test_world("archive-watch-dir-and-multi");
    let archive = Archive::resolve(Some(world.cv_home.join("archive"))).unwrap();

    // 1. Directory-backed `kimi-code` session (`r.path` is `.../session_kc1`, transcript is `agents/main/wire.jsonl`).
    let kc_home = world.base.join("kimi-code-home");
    let sess_dir = kc_home.join("sessions/wd1/session_kc1");
    let wire_path = sess_dir.join("agents/main/wire.jsonl");
    let state_path = sess_dir.join("state.json");
    std::fs::create_dir_all(wire_path.parent().unwrap()).unwrap();
    std::fs::write(
        &wire_path,
        concat!(
            r#"{"type":"metadata","protocol_version":"1.0"}"#,
            "\n",
            r#"{"timestamp":1710000001.0,"message":{"type":"TurnBegin","payload":{"user_input":"first prompt"}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let kc = crate::harness::kimi_code::KimiCode::with_home(&kc_home);
    let kc_refs = kc.discover().unwrap().refs;
    assert_eq!(kc_refs.len(), 1);
    assert!(kc_refs[0].path.is_dir());

    assert_eq!(archive.sync_refs(&kc_refs).unwrap().archived, 1);

    let prod = archive.state_store().default_producer().to_string();
    let mut seen = WatchBaselines::after_sync(&archive, &kc_refs, &SyncSummary::default()).unwrap();

    // Idle poll on directory-backed `kimi-code` emits ZERO candidates (zero session parses!).
    assert!(
        seen.candidates(kc.discover().unwrap().refs).is_empty(),
        "idle kimi-code poll must emit zero watch candidates"
    );
    // Idle `sync_refs` ALSO skips via `prev.baseline.fresh` with zero new packs.
    let idle_sync = archive.sync_refs(&kc_refs).unwrap();
    assert_eq!(idle_sync.archived, 0);
    assert_eq!(idle_sync.skipped, 1);

    // Creating/editing companion `state.json` (without touching `wire.jsonl`!) must surface in watch candidates.
    std::fs::write(&state_path, r#"{"title":"Updated via state.json"}"#).unwrap();
    let kc_state_cands = seen.candidates(kc.discover().unwrap().refs);
    assert_eq!(
        kc_state_cands.len(),
        1,
        "companion state.json mutation must surface in watch candidates"
    );
    assert!(
        seen.candidates(kc.discover().unwrap().refs).is_empty(),
        "subsequent idle poll after state.json update must be quiet again"
    );

    // Appending to nested `agents/main/wire.jsonl` does NOT touch `session_kc1`'s directory mtime,
    // yet both `WatchBaselines::candidates` and `Archive::sync_refs` must detect and archive the growth!
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&wire_path).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":1710000002.0,"message":{{"type":"ContentPart","payload":{{"type":"text","text":"second reply"}}}}}}"#
        )
        .unwrap();
    }
    let kc_refs_after = kc.discover().unwrap().refs;
    let kc_cands = seen.candidates(kc_refs_after);
    assert_eq!(
        kc_cands.len(),
        1,
        "nested wire.jsonl append must surface in watch candidates"
    );
    assert_eq!(
        archive.sync_refs(&kc_cands).unwrap().archived,
        1,
        "sync_refs must re-archive grown nested wire.jsonl"
    );

    // 2. Multi-session `chatgpt-export` (`conversations.json` shared by two sessions `cg-1` and `cg-2`).
    let exp_file = world.base.join("conversations.json");
    std::fs::write(&exp_file, b"[]").unwrap();
    let make_export_refs = |path: &Path| {
        vec![
            SessionRef {
                id: "cg-1".into(),
                harness: Harness::ChatGptExport,
                path: path.to_path_buf(),
                cwd: None,
                title: Some("One".into()),
                created_at: None,
                updated_at: None,
                message_count: Some(1),
            },
            SessionRef {
                id: "cg-2".into(),
                harness: Harness::ChatGptExport,
                path: path.to_path_buf(),
                cwd: None,
                title: Some("Two".into()),
                created_at: None,
                updated_at: None,
                message_count: Some(1),
            },
        ]
    };
    let mut exp_seen = WatchBaselines::default();
    assert_eq!(exp_seen.candidates(make_export_refs(&exp_file)).len(), 2);
    assert!(
        exp_seen.candidates(make_export_refs(&exp_file)).is_empty(),
        "idle multi-session export must emit zero watch candidates"
    );
    std::fs::write(&exp_file, b"[{}, {}]").unwrap();
    let exp_cands = exp_seen.candidates(make_export_refs(&exp_file));
    assert_eq!(
        exp_cands.len(),
        2,
        "updating shared conversations.json must emit ALL sibling sessions, not just the first"
    );
    assert!(
        exp_seen.candidates(make_export_refs(&exp_file)).is_empty(),
        "subsequent idle poll on updated conversations.json must emit zero candidates"
    );

    // 3. Prove `Archive::sync_refs` and `WatchBaselines::candidates` skip idle Claude sessions
    //    strictly from `Baseline` without invoking `stream_counted`, while catching companion
    //    task directory edits (`<claude_config>/tasks/<sid>/1.json`).
    let claude_cfg = world.base.join("claude-cfg");
    let claude_proj = claude_cfg.join("projects/proj-a");
    std::fs::create_dir_all(&claude_proj).unwrap();
    // Deliberately write truncated/unparseable bytes that fail `stream_counted` if parsed,
    // and seed an `ArchivedSessionEntry` with its current `Baseline`.
    let unparsed_claude = claude_proj.join("sess-no-parse.jsonl");
    std::fs::write(&unparsed_claude, b"\xff\xfe\xfd").unwrap();
    let claude_ref = SessionRef::new("sess-no-parse", Harness::Claude, &unparsed_claude);
    let mut claude_seen = WatchBaselines::default();
    assert_eq!(claude_seen.candidates(vec![claude_ref.clone()]).len(), 1);
    assert!(
        claude_seen.candidates(vec![claude_ref.clone()]).is_empty(),
        "idle Claude session must emit zero watch candidates"
    );

    let mut probe = crate::source::Probe::default();
    let captured_base = probe
        .snapshot(&claude_ref)
        .with_reads(&crate::source::ReadSet::default());
    let store = archive.state_store();
    let pack_id = "pack-claude-baseline-proof";
    let mut builder = PackBuilder::create(&archive.packs_dir(), pack_id).unwrap();
    let dummy_ir = br#"{"id":"sess-no-parse","harness":"claude","messages":[]}"#;
    let addr = builder.append_record(claude_ref.session_selector(), dummy_ir).unwrap();
    let pfm = builder.seal(Claim::New).unwrap();
    let zst = std::fs::read(pack_zst_path(&archive.packs_dir(), pack_id).unwrap()).unwrap();
    let append = store
        .append_request(&AppendRequest::new(
            format!("archive:{pack_id}:{prod}:claude:sess-no-parse"),
            crate::blake3_hex(dummy_ir),
            json!({"kind": "session_archive", "session_id": "sess-no-parse"}),
        ))
        .unwrap();
    publish_contiguous_archive_runs(
        &store,
        pack_id,
        &format!("packs/{pack_id}.pack.zst"),
        zst.len() as u64,
        &crate::blake3_hex(&zst),
        &pfm,
        Utc::now().timestamp_millis(),
        vec![CommittedArchiveRecord {
            local_commit_pos: append.local_commit_pos,
            epoch: append.event_id.epoch,
            seq: append.event_id.seq,
            op_id: format!("archive:{pack_id}:{prod}:claude:sess-no-parse"),
            envelope: json!({"kind": "session_archive", "session_id": "sess-no-parse"}),
            entry: ArchivedSessionEntry {
                profile: crate::SessionKey::local(&claude_ref).profile,
                producer_id: prod.clone(),
                harness: "claude".into(),
                id: "sess-no-parse".into(),
                title: None,
                cwd: None,
                created_at: None,
                updated_at: None,
                message_count: 0,
                archived_at: Utc::now(),
                content_blake3: crate::blake3_hex(dummy_ir),
                source_len: Some(3),
                source_mtime_ns: None,
                baseline: Some(captured_base),
                addr,
                custody: Default::default(),
            },
        }],
    )
    .unwrap();

    // Because `Baseline::fresh` holds, `sync_refs` skips `sess-no-parse` WITHOUT opening or
    // parsing `\xff\xfe\xfd`!
    let skipped_proof = archive.sync_refs(std::slice::from_ref(&claude_ref)).unwrap();
    assert_eq!(skipped_proof.skipped, 1);
    assert_eq!(skipped_proof.archived, 0);
    assert!(
        skipped_proof.failed.is_empty(),
        "unchanged Claude baseline must skip before stream_counted: {:?}",
        skipped_proof.failed
    );

    // Creating a companion task file `<claude_cfg>/tasks/sess-no-parse/1.json` invalidates
    // `Baseline::fresh` on both `WatchBaselines::candidates` and `Archive::sync_refs`!
    let task_file = claude_cfg.join("tasks/sess-no-parse/1.json");
    std::fs::create_dir_all(task_file.parent().unwrap()).unwrap();
    std::fs::write(&task_file, r#"{"id":"1","subject":"Check companion"}"#).unwrap();
    assert_eq!(
        claude_seen.candidates(vec![claude_ref.clone()]).len(),
        1,
        "Claude companion tasks/<sid> edit must surface in watch candidates"
    );
    let invalidated = archive.sync_refs(std::slice::from_ref(&claude_ref)).unwrap();
    assert_eq!(
        invalidated.skipped, 0,
        "Claude companion tasks/<sid> edit must invalidate sync_refs baseline fast-path"
    );
    claude_seen.reconcile(std::slice::from_ref(&claude_ref), &invalidated);
    assert!(
        claude_seen.candidates(vec![claude_ref.clone()]).is_empty(),
        "reconciled baseline after companion edit must stay quiet on idle poll"
    );

    // 4. Prove a failed `sync_refs` read (e.g. corrupt zstd rollout) is evicted from
    //    `seen_baseline` by `WatchBaselines::reconcile` so the next poll retries it instead of
    //    permanently dropping the session.
    let bad_zst = world.base.join("corrupt-rollout.jsonl.zst");
    std::fs::write(&bad_zst, b"not-valid-zstd-frame").unwrap();
    let bad_ref = SessionRef::new("codex-corrupt", Harness::Codex, &bad_zst);
    let mut fail_seen = WatchBaselines::default();
    let first_cands = fail_seen.candidates(vec![bad_ref.clone()]);
    assert_eq!(first_cands.len(), 1);
    let failed_batch = archive.sync_refs(&first_cands).unwrap();
    assert_eq!(failed_batch.failed.len(), 1);
    fail_seen.reconcile(&first_cands, &failed_batch);
    assert_eq!(
        fail_seen.candidates(vec![bad_ref.clone()]).len(),
        1,
        "failed sync_refs session must be evicted from seen_baseline and retried on the next poll"
    );
}

/// #tooth Concurrent JSONL growth between `native_delta` and `stream_counted` never stamps
/// `baseline.primary.len = len_t2` over a `[0..to_t1)` pack: the next sync captures `[to_t1..len_t2)`
/// as a delta extent instead of skipping it forever.
#[test]
fn tooth_concurrent_append_between_native_delta_and_baseline_is_captured_on_next_sync() {
    let world = crate::test_world("archive-concurrent-append-gap");
    let path = world.write_session(
        "sess-gap",
        &[json!({
            "type": "user", "uuid": "u1", "sessionId": "sess-gap",
            "timestamp": "2026-06-01T10:00:00Z",
            "message": {"role": "user", "content": "first turn before append"}
        })],
    );
    let r = SessionRef::new("sess-gap", Harness::Claude, &path);
    let archive = Archive {
        home: world.cv_home.clone(),
    };
    let late_line = format!(
        "{}\n",
        json!({
            "type": "assistant", "uuid": "a1", "sessionId": "sess-gap",
            "timestamp": "2026-06-01T10:00:05Z",
            "message": {"role": "assistant", "content": "appended-between-delta-and-stream"}
        })
    );
    let target_path = path.clone();
    let line_copy = late_line.clone();
    AFTER_NATIVE_DELTA.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(move |p| {
            if p == target_path {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
                f.write_all(line_copy.as_bytes()).unwrap();
            }
        }));
    });
    let pass1 = archive.sync_refs(std::slice::from_ref(&r)).unwrap();
    AFTER_NATIVE_DELTA.with(|cell| *cell.borrow_mut() = None);
    assert_eq!(pass1.archived, 1);

    // Pass 2 runs with NO further writes to `path`: it MUST NOT skip `[to_t1..len_t2)`!
    let mut watch = WatchBaselines::after_sync(&archive, std::slice::from_ref(&r), &pass1).unwrap();
    let candidates = watch.candidates(vec![r.clone()]);
    assert_eq!(
        candidates.len(),
        1,
        "watch must capture the append made during its initial sync"
    );
    let pass2 = archive.sync_refs(&candidates).unwrap();
    assert_eq!(
        pass2.archived, 1,
        "second sync must capture lines appended between native_delta and baseline: {pass2:?}"
    );
    assert_eq!(pass2.archived_entries[0].message_count, 2);
    match &pass2.archived_entries[0].custody {
        crate::custody::CustodyKind::NativeExtents { extents, .. } => {
            assert_eq!(extents.len(), 2, "second sync chains only the missing delta extent");
            assert_eq!(extents[1].length, late_line.len() as u64);
        }
        other => panic!("expected NativeExtents, got {other:?}"),
    }

    // Pass 3 is now quiescent and skips cleanly.
    let pass3 = archive.sync_refs(std::slice::from_ref(&r)).unwrap();
    assert_eq!((pass3.archived, pass3.skipped), (0, 1));

    let expected = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let recovered = crate::custody::read_session_bytes(&archive.home, "claude", "sess-gap")
        .unwrap()
        .unwrap();
    assert_eq!(recovered, expected);
}

/// #tooth Vanished or truncated live files at pack time record per-session failures without
/// aborting sibling sessions in the batch or leaving orphan spans in `.pack.zst`, and native
/// `message_count` counts turns (`Message::is_turn`), excluding tool-result-only user records.
#[test]
fn tooth_vanished_or_truncated_native_file_isolates_failure_without_orphan_spans_and_counts_turns() {
    let world = crate::test_world("archive-vanish-and-turn-count");
    let vanish_path = world.write_session(
        "sess-vanish",
        &[json!({
            "type": "user", "uuid": "v1", "sessionId": "sess-vanish",
            "timestamp": "2026-06-01T10:00:00Z",
            "message": {"role": "user", "content": "will vanish before pack"}
        })],
    );
    let trunc_path = world.write_session(
        "sess-trunc",
        &[json!({
            "type": "user", "uuid": "t1", "sessionId": "sess-trunc",
            "timestamp": "2026-06-01T10:00:00Z",
            "message": {"role": "user", "content": "will truncate before pack"}
        })],
    );
    let ok_path = world.write_session(
        "sess-turns",
        &[
            json!({
                "type": "user", "uuid": "u1", "sessionId": "sess-turns",
                "timestamp": "2026-06-01T10:00:00Z",
                "message": {"role": "user", "content": "run a check"}
            }),
            json!({
                "type": "assistant", "uuid": "a1", "sessionId": "sess-turns",
                "timestamp": "2026-06-01T10:00:01Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "true"}}
                ]}
            }),
            json!({
                "type": "user", "uuid": "u2", "sessionId": "sess-turns",
                "timestamp": "2026-06-01T10:00:02Z",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": "ok"}
                ]}
            }),
            json!({
                "type": "assistant", "uuid": "a2", "sessionId": "sess-turns",
                "timestamp": "2026-06-01T10:00:03Z",
                "message": {"role": "assistant", "content": "all green"}
            }),
        ],
    );
    let refs = vec![
        SessionRef::new("sess-vanish", Harness::Claude, &vanish_path),
        SessionRef::new("sess-trunc", Harness::Claude, &trunc_path),
        SessionRef::new("sess-turns", Harness::Claude, &ok_path),
    ];
    let v_target = vanish_path.clone();
    let t_target = trunc_path.clone();
    BEFORE_PACK_STAGE.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(move |p| {
            if p == v_target {
                let _ = std::fs::remove_file(p);
            } else if p == t_target {
                std::fs::write(p, b"short\n").unwrap();
            }
        }));
    });
    let archive = Archive {
        home: world.cv_home.clone(),
    };
    let summary = archive.sync_refs(&refs).unwrap();
    BEFORE_PACK_STAGE.with(|cell| *cell.borrow_mut() = None);

    assert_eq!(summary.archived, 1, "healthy sibling must still archive: {summary:?}");
    assert_eq!(
        summary.failed.len(),
        2,
        "vanished and truncated files fail per-session: {summary:?}"
    );
    // Tool-use + tool-result exchange has 3 turns (u1 prompt, a1 tool call reply, a2 reply) —
    // u2 is a tool-result-only user record, NOT a turn (`DATAPATHS.md` counting rule).
    let expected_turns = crate::stream::stream_counted(
        crate::harness::for_harness(Harness::Claude).unwrap().as_ref(),
        &refs[2],
        &crate::Demand::LEAN.with_spans(),
        &mut crate::stream::Discard,
        crate::stream::Pass::default(),
    )
    .unwrap()
    .counted
    .unwrap()
    .turns;
    assert_eq!(summary.archived_entries[0].message_count, expected_turns);

    // Verify zero orphan spans in the sealed `.pack.idx.json`: `total_decoded_len` equals the
    // single committed session's length!
    let pack_id = summary.pack_id.unwrap();
    let idx_raw = std::fs::read(crate::packs::pack_idx_path(&archive.packs_dir(), &pack_id).unwrap()).unwrap();
    let pfm = PackFileManifest::from_json_slice(&idx_raw).unwrap();
    assert_eq!(
        pfm.total_decoded_len,
        std::fs::metadata(&ok_path).unwrap().len(),
        "failed pre-append checks must leave zero orphan bytes in .pack.zst"
    );
}
