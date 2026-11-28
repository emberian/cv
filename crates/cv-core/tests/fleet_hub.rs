//! End-to-end multi-producer delta replication, spool/seal & pull topologies,
//! `O(delta)` native extent shipping, and `AuthorityEnvelope` soundness tests.

use chrono::{TimeZone, Utc};
use cv_core::custody::CustodyKind;
use cv_core::env::Env;
use cv_core::harness::Parse as _;
use cv_core::ir::{Block, Harness, Message, Principal, Role, Session};
use cv_core::packs::PackBuilder;
use cv_core::publish::Claim;
use cv_core::redact::{PrincipalPolicy, ScrubProfile};
use cv_core::state::hub::{
    export_delta_stream, export_to_spool, AuthorityQueryContext, AuthorityStatus, HubError, HubStore,
    IngestBatchRequest, IngestEventRecord, PlaceState,
};
use cv_core::state::{AppendRequest, EventId, PackManifest as StatePackManifest, StateStore};
use cv_testkit::World;
use serde_json::json;
use std::fs;
use std::io::Cursor;

fn make_session(id: &str, cwd: &str, title: &str, user_msg: &str, asst_msg: &str) -> Session {
    let mut s = Session::new(id, Harness::Claude);
    s.cwd = Some(cwd.into());
    s.title = Some(title.into());
    s.model = Some("claude-sonnet-4-6".into());
    s.created_at = Some(Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap());
    s.updated_at = Some(Utc.with_ymd_and_hms(2026, 7, 10, 12, 5, 0).unwrap());
    let mut m1 = Message::new(Role::User);
    m1.timestamp = s.created_at;
    m1.content.push(Block::from_text(user_msg.into()));
    let mut m2 = Message::new(Role::Assistant);
    m2.timestamp = s.updated_at;
    m2.model = s.model.clone();
    m2.content.push(Block::from_text(asst_msg.into()));
    s.messages = vec![m1, m2];
    s
}

#[test]
fn multi_producer_delta_extents_sealed_spool_pull_and_authority_soundness() {
    let hub_w = World::in_process("core-fleet-delta-authority", Env::enter_isolated);
    let hub = HubStore::at_home(&hub_w.cv_home);

    // ── Producer 1 (`prod-push`): 200 KiB initial native JSONL session + 4 KiB append ──
    let prod1_dir = hub_w.base.join("prod1");
    let prod1_store = StateStore::at_dir(&prod1_dir);
    let prod1_packs = prod1_dir.join("packs");
    fs::create_dir_all(&prod1_packs).unwrap();

    let fake_home = hub_w.home.to_string_lossy().into_owned();
    let mut initial_jsonl = Vec::new();
    for i in 0..400 {
        let line = json!({
            "type": "user",
            "uuid": format!("u-{i}"),
            "sessionId": "native-big",
            "timestamp": "2026-07-10T12:00:00Z",
            "cwd": format!("{fake_home}/work/service"),
            "message": {
                "role": "user",
                "content": format!("initial turn {i} padding {}", "x".repeat(420))
            }
        });
        initial_jsonl.extend_from_slice(serde_json::to_string(&line).unwrap().as_bytes());
        initial_jsonl.push(b'\n');
    }
    assert!(initial_jsonl.len() > 200 * 1024);

    let key_local = cv_core::SessionKey {
        producer: "local".into(),
        profile: Default::default(),
        harness: Harness::Claude,
        native_id: "native-big".into(),
    };
    let anchor_local = cv_core::custody::archive_anchor(&key_local);
    let mut b1 = PackBuilder::create(&prod1_packs, "p1-init").unwrap();
    let init_addr = b1.append_record(&anchor_local, &initial_jsonl).unwrap();
    b1.seal(Claim::New).unwrap();

    let init_custody = CustodyKind::NativeExtents {
        extents: vec![init_addr.clone()],
        total_bytes: init_addr.length,
        prefix_blake3: init_addr.digest_hex(),
        file_name: "native-big.jsonl".into(),
        layout: Vec::new(),
    };
    let init_env = json!({
        "kind": "session_archive",
        "producer_id": "local",
        "harness": "claude",
        "session_id": "native-big",
        "title": "Large Native Session",
        "cwd": format!("{fake_home}/work/service"),
        "created_at": "2026-07-10T12:00:00Z",
        "updated_at": "2026-07-10T12:01:00Z",
        "message_count": 400,
        "custody": init_custody,
        "metadata": {
            "nested": {
                "kind": "sk-ant-api03-Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2Ji1Hg0Fe9Dc8Ba7Zy6Xw5Vu4Ts3Rq2",
                "harness": format!("{fake_home}/.ssh/id_ed25519"),
                "principal": "alice@corp.internal"
            }
        }
    });
    let r1 = prod1_store
        .append_request(
            &AppendRequest::new("op:p1:1", init_addr.digest_hex(), init_env.clone())
                .with_payload_ref(serde_json::to_string(&init_addr).unwrap()),
        )
        .unwrap();
    prod1_store
        .publish_pack_range(&StatePackManifest {
            pack_id: "p1-init".into(),
            min_commit_pos: r1.local_commit_pos,
            max_commit_pos: r1.local_commit_pos,
            record_count: 1,
            byte_len: fs::metadata(prod1_packs.join("p1-init.pack.zst")).unwrap().len(),
            digest: "blake3:init".into(),
            pack_path: "packs/p1-init.pack.zst".into(),
            index_meta: json!({
                "physical_pack_id": "p1-init",
                "commits": [{
                    "local_commit_pos": r1.local_commit_pos,
                    "producer_id": "local",
                    "epoch": r1.event_id.epoch,
                    "seq": r1.event_id.seq,
                    "op_id": "op:p1:1",
                    "envelope": init_env,
                    "payload_addr": init_addr,
                }]
            }),
            published_at: 1000,
        })
        .unwrap();

    let mut profile_p1 = ScrubProfile {
        rewrite_home: true,
        principal: PrincipalPolicy::Hash {
            salt: Some("s1".into()),
        },
        ..Default::default()
    };
    profile_p1.filter.deny_cwds = vec![format!("{fake_home}/secret/*").into()];

    // Export initial commit via `--frozen`-safe `export_delta_stream("0")` into Hub -> Gen 1!
    let mut stream_1 = Vec::new();
    let exp1 = export_delta_stream(
        &prod1_store,
        &prod1_packs,
        "prod-push",
        "box-1",
        &profile_p1,
        "0",
        &mut stream_1,
    )
    .unwrap();
    assert_eq!(exp1.events_exported, 1);
    let ing1 = hub
        .ingest_delta_stream(Cursor::new(&stream_1), Some("prod-push"))
        .unwrap();
    assert_eq!(ing1.hub_generation, Some(1));

    // Verify nested `"kind"` and `"harness"` inside metadata WERE scrubbed, while root `"kind"` / `"harness"` survived,
    // and principal hash uses full 64-hex `blake3:`!
    let heads_g1 = hub.heads_at_generation(Some(1)).unwrap();
    assert_eq!(heads_g1.len(), 1);
    let meta_str = serde_json::to_string(&heads_g1[0].metadata).unwrap();
    assert!(
        !meta_str.contains("sk-ant-api03") && !meta_str.contains(&fake_home),
        "nested kind/harness must be scrubbed: {meta_str}"
    );
    let p_hash = heads_g1[0].metadata["nested"]["principal"].as_str().unwrap();
    assert!(p_hash.starts_with("blake3:") && p_hash.len() == 71, "{p_hash}");

    // Now append ~4 KiB (`session_delta`) to `native-big` and delete `p1-init.pack.zst` locally to
    // PROVE `export_delta_stream` reads ONLY the 4 KiB delta pack (`O(delta)`), NEVER `p1-init`!
    fs::remove_file(prod1_packs.join("p1-init.pack.zst")).unwrap();
    let mut delta_jsonl = Vec::new();
    for i in 0..8 {
        let line = json!({
            "type": "assistant",
            "uuid": format!("a-delta-{i}"),
            "sessionId": "native-big",
            "timestamp": "2026-07-10T12:10:00Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": format!("delta narwhal turn {i} {}", "y".repeat(400))}]
            }
        });
        delta_jsonl.extend_from_slice(serde_json::to_string(&line).unwrap().as_bytes());
        delta_jsonl.push(b'\n');
    }
    assert!(delta_jsonl.len() > 3500 && delta_jsonl.len() < 6000);
    let mut b2 = PackBuilder::create(&prod1_packs, "p1-delta").unwrap();
    let delta_addr = b2.append_record(&anchor_local, &delta_jsonl).unwrap();
    b2.seal(Claim::New).unwrap();

    let delta_env = json!({
        "kind": "session_delta",
        "producer_id": "local",
        "harness": "claude",
        "session_id": "native-big",
        "title": "Large Native Session (with delta)",
        "cwd": format!("{fake_home}/work/service"),
        "created_at": "2026-07-10T12:00:00Z",
        "updated_at": "2026-07-10T12:10:00Z",
        "message_count": 408,
        "file_name": "native-big.jsonl",
        "delta_extents": [delta_addr],
    });
    let r2 = prod1_store
        .append_request(
            &AppendRequest::new("op:p1:2", delta_addr.digest_hex(), delta_env.clone())
                .with_payload_ref(serde_json::to_string(&delta_addr).unwrap()),
        )
        .unwrap();
    prod1_store
        .publish_pack_range(&StatePackManifest {
            pack_id: "p1-delta".into(),
            min_commit_pos: r2.local_commit_pos,
            max_commit_pos: r2.local_commit_pos,
            record_count: 1,
            byte_len: fs::metadata(prod1_packs.join("p1-delta.pack.zst")).unwrap().len(),
            digest: "blake3:delta".into(),
            pack_path: "packs/p1-delta.pack.zst".into(),
            index_meta: json!({
                "physical_pack_id": "p1-delta",
                "commits": [{
                    "local_commit_pos": r2.local_commit_pos,
                    "producer_id": "local",
                    "epoch": r2.event_id.epoch,
                    "seq": r2.event_id.seq,
                    "op_id": "op:p1:2",
                    "envelope": delta_env,
                    "payload_addr": delta_addr,
                    "delta_extents": [delta_addr],
                }]
            }),
            published_at: 2000,
        })
        .unwrap();

    let wm_after_1 = hub.pull_watermark_for("prod-push").unwrap();
    let mut stream_2 = Vec::new();
    let exp2 = export_delta_stream(
        &prod1_store,
        &prod1_packs,
        "prod-push",
        "box-1",
        &profile_p1,
        &wm_after_1,
        &mut stream_2,
    )
    .unwrap();
    assert_eq!(exp2.events_exported, 1);
    assert!(
        stream_2.len() < 8192,
        "4 KiB append must ship O(delta) bytes (< 8 KiB), got {} bytes",
        stream_2.len()
    );
    let ing2 = hub
        .ingest_delta_stream(Cursor::new(&stream_2), Some("prod-push"))
        .unwrap();
    assert_eq!(ing2.hub_generation, Some(2));

    // ── Producer 2 (`sbx-sealed`): Credential-free `--spool --seal` ephemeral sandbox ──
    let sbx_dir = hub_w.base.join("sbx");
    let sbx_store = StateStore::at_dir(&sbx_dir);
    let sbx_packs = sbx_dir.join("packs");
    let spool_dir = hub_w.base.join("spool-out");
    let mut sbx_sess = make_session("sbx-1", "/workspace/app", "Sandbox Task", "run tests", "all green");
    sbx_sess.principal = Some(Principal::String("ci-bot".into()));
    cv_core::state::hub::capture_session_to_store(&sbx_store, &sbx_packs, &sbx_sess, true).unwrap();

    let mut sbx_profile = ScrubProfile::default();
    sbx_profile.classes.api_keys = false;
    sbx_profile.classes.private_keys = false;
    sbx_profile.classes.jwts = false;
    sbx_profile.classes.emails = false;
    sbx_profile.classes.blobs = false;
    sbx_profile.classes.assignments = false;
    let spool_rep = export_to_spool(
        &sbx_store,
        &sbx_packs,
        &spool_dir,
        "sbx-sealed",
        "ephemeral-pod-9",
        &sbx_profile,
        true,
    )
    .unwrap();
    assert!(spool_rep.sealed);
    assert_eq!(spool_rep.events_spooled, 1);

    let spool_ing1 = hub.ingest_spool_dir(&spool_dir).unwrap();
    assert_eq!(spool_ing1.events_accepted, 1);
    assert_eq!(spool_ing1.hub_generation, Some(3));
    // Re-ingesting the same spool directory is 100% idempotent!
    let spool_ing2 = hub.ingest_spool_dir(&spool_dir).unwrap();
    assert_eq!(spool_ing2.events_accepted, 0);
    assert_eq!(spool_ing2.events_replayed, 1);
    assert_eq!(spool_ing2.hub_generation, Some(3));

    // ── Pinned `--as-of G` repeatability across G=1, G=2, G=3 ──
    let sel_p1 = heads_g1[0].selector_string();
    let b_at_1 = hub
        .state_store()
        .custody_backing_for(&sel_p1, Some(1))
        .unwrap()
        .unwrap();
    let parsed_at_1 = b_at_1.parse(&b_at_1.reference).unwrap();
    assert_eq!(parsed_at_1.messages.len(), 400, "Gen 1 must stay frozen at 400 turns");

    let b_at_2 = hub
        .state_store()
        .custody_backing_for(&sel_p1, Some(2))
        .unwrap()
        .unwrap();
    let parsed_at_2 = b_at_2.parse(&b_at_2.reference).unwrap();
    assert_eq!(
        parsed_at_2.messages.len(),
        408,
        "Gen 2 chains initial extent + 8-turn delta extent"
    );

    // ── Open-World Soundness (`AuthorityEnvelope`) ──
    // 1) `sbx-sealed` is `Sealed` at `contiguous_seq == producer_head_seq == 1`: even with `max_staleness_ms = 0`,
    //    it is `Complete` forever without heartbeats!
    let env_sealed = hub
        .check_authority(&AuthorityQueryContext {
            place_glob: Some("sbx-*"),
            max_staleness_ms: Some(0),
            ..Default::default()
        })
        .expect("Sealed place must pass max-staleness=0 forever without heartbeats");
    assert_eq!(env_sealed.status, AuthorityStatus::Complete);
    assert_eq!(env_sealed.places[0].state, PlaceState::Sealed);

    // 2) Querying `cwd = "<fake_home>/secret/vault"` against `prod-push` (whose `deny_cwds` excludes `<fake_home>/secret/*`)
    //    downgrades `status` to `Partial` naming `excluded_by_filter_cwd`!
    let secret_cwd = format!("{fake_home}/secret/vault");
    let env_filtered = hub
        .compute_authority(&AuthorityQueryContext {
            place_glob: Some("prod-push"),
            cwd_filter: Some(&secret_cwd),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(env_filtered.status, AuthorityStatus::Partial);
    assert!(env_filtered
        .omissions
        .iter()
        .any(|o| o.reason == "excluded_by_filter_cwd"));

    // 3) Enrolling an unsealed disconnected producer `prod-stale` with old contact timestamp downgrades
    //    fleet authority to `Partial` and fails closed under `check_authority(max_staleness_ms: Some(1_000))`!
    hub.enroll_producer("prod-stale", "stale-laptop", &json!({})).unwrap();
    let stale_err = hub
        .check_authority(&AuthorityQueryContext {
            place_glob: Some("prod-stale"),
            max_staleness_ms: Some(1_000),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(stale_err.authority.status, AuthorityStatus::Partial);
    assert!(stale_err.message.contains("prod-stale"));

    // 4) Conflicting `seq = 1` packet against `sbx-sealed` is quarantined WITHOUT rewinding `contiguous_seq`
    //    or erasing `sbx-1` from `hub_session_heads`!
    let conflict_ev = IngestEventRecord::new(
        EventId::new("sbx-sealed", &env_sealed.places[0].epoch, 1),
        "op:forged:1",
        json!({"kind": "session_upsert", "session_id": "forged", "harness": "claude"}),
        None,
    )
    .unwrap();
    assert!(matches!(
        hub.ingest_batch(&IngestBatchRequest {
            events: vec![conflict_ev]
        }),
        Err(HubError::EventConflict(_))
    ));
    let heads_after_conflict = hub.heads_at_generation(None).unwrap();
    assert_eq!(
        heads_after_conflict.len(),
        2,
        "committed sessions must survive seq=1 quarantine intact"
    );

    // 5) Unified HTTP read API (`route_json`): `/api/places`, pinned `/api/sessions?as_of=1&authority=1`,
    //    and fail-closed `503` on `/api/sessions?place=prod-stale&max_staleness=1s`!
    let (st_places, body_places) = cv_core::api::route_json(&["api", "places"], "");
    assert_eq!(st_places, 200, "{body_places}");
    assert!(body_places["places"].is_array() && body_places["authority"].is_object());

    let (st_asof1, body_asof1) = cv_core::api::route_json(&["api", "sessions"], "as_of=1&authority=1");
    assert_eq!(st_asof1, 200, "{body_asof1}");
    assert_eq!(body_asof1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(body_asof1["authority"]["generation"], 1);

    let (st_stale, body_stale) = cv_core::api::route_json(&["api", "sessions"], "place=prod-stale&max_staleness=1s");
    assert_eq!(
        st_stale, 503,
        "violated max_staleness must return HTTP 503: {body_stale}"
    );
    assert_eq!(body_stale["authority"]["status"], "partial");
}
