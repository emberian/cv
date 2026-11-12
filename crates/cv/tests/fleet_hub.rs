//! End-to-end Fleet Hub integration suite (`CV_SERVE_HUB=1`):
//!
//! 1. Out-of-order batch delivery (`5..=8` before `1..=4`) holds `F(p, e) == 0` until `1..=4`
//!    lands, then jumps to `8` (`Snapshot` #1 -> #2), plus `publish-before-reference` (`424`).
//! 2. Duplicate batch & pack replay is 100% idempotent; conflicting pack upload returns `409`.
//! 3. Loud conflict quarantine (`gpt-6-astra` Gate): same `EventId` with different digest returns
//!    `409 Conflict`, writes `hub_quarantine`, and freezes `F(p, e)` below the conflict.
//! 4. Sleeping/offline producer host + pinned `?snapshot=` read consistency + tear-free multi-page
//!    pagination + metadata-only `pack_not_mirrored` fallback + `X-Cv-Snapshot` header stamping.
//! 5. `[replication.<target>]` configurable privacy filter & scrubbing pipeline + Two-Stage Ack
//!    Accounting (`TransportDeliveryAck` vs durable `CustodyAck` gating `is_pack_evictable`) +
//!    full wire/disk zero-leak audit.
//! 6. Rebuild Parity Invariant (`gpt-6-astra` Gate): wipe derived hub views (`hub_snapshots`,
//!    `hub_sessions`, `hub_frontiers`, and `hub_packs` rows) and rebuild from raw
//!    `hub_ingested_events` + `hub_quarantine` + `hub/packs/` -> bit-for-bit identical snapshot
//!    frontier, manifest, and hydrated sessions.

use chrono::{TimeZone, Utc};
use cv_core::config::Config;
use cv_core::env::Env;
use cv_core::ir::{Block, Harness, Message, Principal, Role, Session};
use cv_core::packs::{PackBuilder, SealedPack};
use cv_core::publish::Claim;
use cv_core::state::hub::{
    capture_discovered_sessions, capture_session_to_store, encode_pair_key, seal_local_inbox_to_pack, HttpClient,
    HubSessionsPage, HubSnapshotResponse, HubStatusResponse, HubStore, IngestBatchRequest, IngestEventRecord,
    ShipConfig, Shipper, PACK_STREAM_CONTENT_TYPE,
};
use cv_core::state::{EventId, RecorderServer, StateStore};
use cv_testkit::World;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_hub_serve(w: &World, token: Option<&str>) -> (String, u16, Reaper) {
    let mut cmd = w.cv_command();
    cmd.args(["serve", "--port", "0"]);
    if let Some(tok) = token {
        cmd.args(["--token", tok]);
    }
    cmd.env("CV_SERVE_HUB", "1");

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cv serve should spawn");

    let stderr = child.stderr.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut sink);
    });
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let banner_port = |line: &str| -> Option<u16> {
        let rest = line.split("http://").nth(1)?;
        let hostport = rest.split_whitespace().next()?;
        hostport.rsplit(':').next()?.parse().ok()
    };

    let mut reaper = Reaper(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    let port = loop {
        if let Some(status) = reaper.0.try_wait().expect("try_wait cv serve") {
            panic!("cv serve exited at startup ({status}); stderr: {seen:?}");
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if line.starts_with("cv serve on ") {
                    if let Some(p) = banner_port(&line) {
                        break p;
                    }
                }
                seen.push(line);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
        assert!(
            Instant::now() < deadline,
            "cv serve never printed banner; stderr: {seen:?}"
        );
    };
    std::thread::spawn(move || while rx.recv().is_ok() {});

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if let Some(status) = reaper.0.try_wait().expect("try_wait cv serve") {
            panic!("cv serve died after banner ({status})");
        }
        assert!(Instant::now() < deadline, "cv serve never accepted on :{port}");
        std::thread::sleep(Duration::from_millis(20));
    }

    (format!("http://127.0.0.1:{port}"), port, reaper)
}

fn msg_text(role: Role, text: &str) -> Message {
    let mut m = Message::new(role);
    m.content.push(Block::Text { text: text.into() });
    m
}

fn make_session(id: &str, cwd: &str, title: &str, user_text: &str, asst_text: &str, minute: u32) -> Session {
    let mut s = Session::new(id, Harness::Claude);
    s.cwd = Some(cwd.into());
    s.title = Some(title.into());
    s.model = Some("claude-sonnet-4-6".into());
    let t0 = Utc.with_ymd_and_hms(2026, 6, 15, 12, minute, 0).unwrap();
    let t1 = Utc.with_ymd_and_hms(2026, 6, 15, 12, minute, 30).unwrap();
    s.created_at = Some(t0);
    s.updated_at = Some(t1);
    let mut m1 = msg_text(Role::User, user_text);
    m1.timestamp = Some(t0);
    let mut m2 = msg_text(Role::Assistant, asst_text);
    m2.timestamp = Some(t1);
    s.messages = vec![m1, m2];
    s
}

fn build_pack_with_sessions(
    dir: &Path,
    pack_id: &str,
    sessions: &[Session],
) -> Vec<(Session, cv_core::packs::PayloadAddr)> {
    fs::create_dir_all(dir).unwrap();
    let mut builder = PackBuilder::create(dir, pack_id).unwrap();
    let mut out = Vec::with_capacity(sessions.len());
    for (idx, s) in sessions.iter().enumerate() {
        let bytes = serde_json::to_vec(s).unwrap();
        let addr = builder
            .append_record(format!("session:{}:{}:{idx}", s.harness.as_str(), s.id), &bytes)
            .unwrap();
        out.push((s.clone(), addr));
    }
    builder.seal(Claim::New).unwrap();
    out
}

fn make_event(
    producer: &str,
    epoch: &str,
    seq: u64,
    session: &Session,
    addr: Option<cv_core::packs::PayloadAddr>,
) -> IngestEventRecord {
    let env = json!({
        "kind": "session_upsert",
        "session_id": session.id,
        "harness": session.harness.as_str(),
        "title": session.title,
        "cwd": session.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "model": session.model,
        "created_at": session.created_at.map(|t| t.to_rfc3339()),
        "updated_at": session.updated_at.map(|t| t.to_rfc3339()),
        "message_count": session.messages.len(),
    });
    IngestEventRecord::new(
        EventId::new(producer, epoch, seq),
        format!("op:{producer}:{epoch}:{seq}"),
        env,
        addr,
    )
    .unwrap()
}

/// 1. Out-of-order batch delivery (`5..=8` before `1..=4`) holds `F(p, e) == 0` until `1..=4`
///    lands, then advances to `8` and publishes `Snapshot` #1 -> #2; `publish-before-reference`
///    rejects unuploaded packs with `424 Failed Dependency` before mutating state.
#[test]
fn fleet_hub_out_of_order_frontier_and_publish_before_reference() {
    let hub_w = World::new("hub-ooo");
    let scratch_packs = hub_w.base.join("producer-packs");
    let (hub_url, _port, _reaper) = spawn_hub_serve(&hub_w, Some("ooo-token"));

    let shipper = Shipper::new(
        ShipConfig::new("hub", &hub_url, "prod-alpha")
            .with_token("ooo-token")
            .with_host_label("alpha-host"),
    )
    .unwrap();
    shipper.enroll().unwrap();

    let sessions: Vec<Session> = (1u32..=8)
        .map(|i| {
            make_session(
                &format!("sess-{i}"),
                "/work/alpha",
                &format!("Session #{i}"),
                &format!("question {i}"),
                &format!("answer {i}"),
                i,
            )
        })
        .collect();
    let packed = build_pack_with_sessions(&scratch_packs, "pack-alpha-1to8", &sessions);
    let events: Vec<IngestEventRecord> = packed
        .iter()
        .enumerate()
        .map(|(idx, (s, addr))| make_event("prod-alpha", "ep-1", (idx + 1) as u64, s, Some(addr.clone())))
        .collect();

    // `publish-before-reference`: referencing `pack-alpha-1to8` BEFORE uploading it MUST fail 424
    // and leave hub frontiers & snapshots untouched!
    let err = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: events[4..8].to_vec(),
        })
        .unwrap_err();
    match err {
        cv_core::state::hub::HubError::Transport { status, message } => {
            assert_eq!(status, Some(424), "expected 424 Failed Dependency: {message}");
            assert!(message.contains("pack-alpha-1to8"), "{message}");
        }
        other => panic!("expected Transport(424), got {other:?}"),
    }
    let (status_raw, _) = shipper.client().get("/api/hub/status").unwrap();
    let st0: HubStatusResponse = status_raw.json().unwrap();
    assert!(st0.frontiers.is_empty(), "failed batch must not mutate frontiers");
    assert!(st0.latest_snapshot.is_none(), "failed batch must not publish snapshot");

    // Upload the pack first (`publish-before-reference` satisfied).
    let (up_resp, _) = shipper.upload_pack_files(&scratch_packs, "pack-alpha-1to8").unwrap();
    assert!(!up_resp.replayed);
    assert!(up_resp.durable_fsynced);

    // Deliver out-of-order batch `seq = 5..=8` BEFORE `1..=4`.
    let (batch1_resp, _) = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: events[4..8].to_vec(),
        })
        .unwrap();
    assert_eq!(batch1_resp.accepted_count, 4);
    assert_eq!(
        batch1_resp
            .origin_frontier
            .get(&("prod-alpha".into(), "ep-1".into()))
            .copied(),
        Some(0),
        "F(prod-alpha, ep-1) must stay 0 while 1..=4 are missing"
    );
    let snap1 = batch1_resp.snapshot.expect("Snapshot #1 published");
    assert_eq!(snap1.hub_generation, 1);
    assert!(
        snap1.session_revisions.is_empty(),
        "buffered future events 5..=8 must NOT leak into Snapshot #1"
    );

    let (st1_raw, _) = shipper.client().get("/api/hub/status").unwrap();
    assert_eq!(st1_raw.header("X-Cv-Snapshot"), Some(snap1.publication_id.as_str()));
    assert_eq!(st1_raw.header("X-Cv-Hub-Generation"), Some("1"));
    let st1: HubStatusResponse = st1_raw.json().unwrap();
    assert_eq!(st1.frontiers.len(), 1);
    assert_eq!(st1.frontiers[0].contiguous_seq, 0);
    assert_eq!(st1.frontiers[0].max_received_seq, 8);
    assert_eq!(st1.frontiers[0].buffered_count, 4);
    assert_eq!(st1.frontiers[0].missing_ranges, vec![(1, 4)]);

    let (sess1_raw, _) = shipper.client().get("/api/hub/sessions").unwrap();
    let sess1_page: HubSessionsPage = sess1_raw.json().unwrap();
    assert_eq!(sess1_page.total, 0);

    // Now deliver the missing prefix `seq = 1..=4`: frontier jumps straight from `0` -> `8`!
    let (batch2_resp, _) = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: events[0..4].to_vec(),
        })
        .unwrap();
    assert_eq!(batch2_resp.accepted_count, 4);
    assert_eq!(
        batch2_resp
            .origin_frontier
            .get(&("prod-alpha".into(), "ep-1".into()))
            .copied(),
        Some(8)
    );
    let snap2 = batch2_resp.snapshot.expect("Snapshot #2 published");
    assert_eq!(snap2.hub_generation, 2);
    assert_ne!(snap2.publication_id, snap1.publication_id);
    assert_eq!(snap2.session_revisions.len(), 8);

    let (st2_raw, _) = shipper.client().get("/api/hub/status").unwrap();
    let st2: HubStatusResponse = st2_raw.json().unwrap();
    assert_eq!(st2.frontiers[0].contiguous_seq, 8);
    assert_eq!(st2.frontiers[0].buffered_count, 0);
    assert!(st2.frontiers[0].missing_ranges.is_empty());

    let (sess2_raw, _) = shipper.client().get("/api/hub/sessions").unwrap();
    assert_eq!(sess2_raw.header("X-Cv-Snapshot"), Some(snap2.publication_id.as_str()));
    assert_eq!(sess2_raw.header("X-Cv-Hub-Generation"), Some("2"));
    let sess2_page: HubSessionsPage = sess2_raw.json().unwrap();
    assert_eq!(sess2_page.total, 8);
}

/// 2. Duplicate batch & pack replay is 100% idempotent; conflicting pack digest for an existing
///    `pack_id` fails loudly with `409 Conflict`.
#[test]
fn fleet_hub_duplicate_replay_and_pack_conflict() {
    let hub_w = World::new("hub-idempotent");
    let scratch_packs = hub_w.base.join("packs");
    let (hub_url, _port, _reaper) = spawn_hub_serve(&hub_w, Some("idem-token"));

    let shipper = Shipper::new(
        ShipConfig::new("hub", &hub_url, "prod-idem")
            .with_token("idem-token")
            .with_host_label("idem-box"),
    )
    .unwrap();
    shipper.enroll().unwrap();

    let sessions: Vec<Session> = (1u32..=4)
        .map(|i| make_session(&format!("idem-{i}"), "/work/idem", "Idem", "u", "a", i))
        .collect();
    let packed = build_pack_with_sessions(&scratch_packs, "pack-idem-1", &sessions);
    let events: Vec<IngestEventRecord> = packed
        .iter()
        .enumerate()
        .map(|(idx, (s, addr))| make_event("prod-idem", "ep-1", (idx + 1) as u64, s, Some(addr.clone())))
        .collect();

    let (up1, _) = shipper.upload_pack_files(&scratch_packs, "pack-idem-1").unwrap();
    assert!(!up1.replayed);

    // Identical pack retry via POST `/api/hub/upload_pack` is a 200 idempotent replay.
    let (up2, _) = shipper.upload_pack_files(&scratch_packs, "pack-idem-1").unwrap();
    assert!(up2.replayed);
    assert_eq!(up2.pack.blake3_digest, up1.pack.blake3_digest);

    // Identical pack retry via PUT `/api/hub/packs/<pack_id>` is also a 200 idempotent replay.
    let idx_bytes = fs::read(scratch_packs.join("pack-idem-1.pack.idx.json")).unwrap();
    let zst_bytes = fs::read(scratch_packs.join("pack-idem-1.pack.zst")).unwrap();
    let mut stream_body = idx_bytes.clone();
    stream_body.extend_from_slice(&zst_bytes);
    let idx_len_str = idx_bytes.len().to_string();
    let (put_resp, _) = shipper
        .client()
        .send(
            "PUT",
            "/api/hub/packs/pack-idem-1",
            Some(PACK_STREAM_CONTENT_TYPE),
            &[("X-Cv-Pack-Idx-Len", idx_len_str.as_str())],
            &stream_body,
        )
        .unwrap();
    assert_eq!(put_resp.status, 200);
    let put_parsed: cv_core::state::hub::UploadPackResponse = put_resp.json().unwrap();
    assert!(put_parsed.replayed);

    // Uploading different content under the existing `pack-idem-1` name MUST fail 409 Conflict!
    let alt_dir = hub_w.base.join("alt-packs");
    let alt_sess = vec![make_session("alt-x", "/work/alt", "Different", "diff", "bytes", 9)];
    build_pack_with_sessions(&alt_dir, "pack-idem-1", &alt_sess);
    let conflict_err = shipper.upload_pack_files(&alt_dir, "pack-idem-1").unwrap_err();
    match conflict_err {
        cv_core::state::hub::HubError::Transport { status, message } => {
            assert_eq!(status, Some(409), "expected 409 PackConflict: {message}");
            let v: Value = serde_json::from_str(&message).unwrap();
            assert_eq!(v["pack_id"], "pack-idem-1");
            assert_eq!(v["existing_digest"], up1.pack.blake3_digest);
            assert_ne!(v["incoming_digest"], up1.pack.blake3_digest);
        }
        other => panic!("expected Transport(409), got {other:?}"),
    }

    // First ingest commits `1..=4` and publishes generation 1.
    let (ing1, _) = shipper
        .send_ingest_batch(&IngestBatchRequest { events: events.clone() })
        .unwrap();
    assert_eq!(ing1.accepted_count, 4);
    assert_eq!(ing1.replayed_count, 0);
    let snap1 = ing1.snapshot.unwrap();
    assert_eq!(snap1.hub_generation, 1);

    // Full duplicate replay & partial subset replay are 100% idempotent (200 OK, generation stays 1).
    let (ing2, _) = shipper
        .send_ingest_batch(&IngestBatchRequest { events: events.clone() })
        .unwrap();
    assert_eq!(ing2.accepted_count, 0);
    assert_eq!(ing2.replayed_count, 4);
    assert_eq!(ing2.snapshot.as_ref().unwrap().hub_generation, 1);
    assert_eq!(ing2.snapshot.as_ref().unwrap().publication_id, snap1.publication_id);

    let (ing3, _) = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: events[1..3].to_vec(),
        })
        .unwrap();
    assert_eq!(ing3.accepted_count, 0);
    assert_eq!(ing3.replayed_count, 2);
    assert_eq!(ing3.snapshot.as_ref().unwrap().hub_generation, 1);
}

/// 3. Loud Conflict Quarantine (`gpt-6-astra` Gate): same `EventId` arriving with a different
///    digest returns `409 Conflict` naming `EventId` + both digests, writes `hub_quarantine`,
///    never overwrites committed state, and freezes `F(p, e)` below the conflict (`seq - 1`).
#[test]
fn fleet_hub_conflict_quarantine_freezes_frontier() {
    let hub_w = World::new("hub-quarantine");
    let scratch_packs = hub_w.base.join("packs");
    let (hub_url, _port, _reaper) = spawn_hub_serve(&hub_w, Some("quar-token"));

    let shipper = Shipper::new(
        ShipConfig::new("hub", &hub_url, "prod-quar")
            .with_token("quar-token")
            .with_host_label("quar-box"),
    )
    .unwrap();
    shipper.enroll().unwrap();

    let sessions: Vec<Session> = (1u32..=6)
        .map(|i| make_session(&format!("qs-{i}"), "/work/q", &format!("Q{i}"), "u", "a", i))
        .collect();
    let packed = build_pack_with_sessions(&scratch_packs, "pack-quar-1", &sessions);
    shipper.upload_pack_files(&scratch_packs, "pack-quar-1").unwrap();

    let events: Vec<IngestEventRecord> = packed
        .iter()
        .enumerate()
        .map(|(idx, (s, addr))| make_event("prod-quar", "ep-1", (idx + 1) as u64, s, Some(addr.clone())))
        .collect();

    // Commit `1..=3` cleanly (`F == 3`) and buffer `5..=6` (`F` stays `3`).
    let (r1, _) = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: vec![
                events[0].clone(),
                events[1].clone(),
                events[2].clone(),
                events[4].clone(),
                events[5].clone(),
            ],
        })
        .unwrap();
    assert_eq!(
        r1.origin_frontier.get(&("prod-quar".into(), "ep-1".into())).copied(),
        Some(3)
    );

    // Inject a conflicting `seq = 4` first? Even better: test BOTH a conflict on a buffered/next
    // slot (`seq = 4` committed vs forged `seq = 4`) so `F` stays frozen at `3` even after `1..=6`
    // are all present!
    // Step A: commit legitimate `seq = 4` -> `F` advances to `6`. Wait: let's test conflict at
    // `seq = 4` where `seq = 4` was buffered out-of-order on `ep-2` AND conflict on `ep-1` at `seq = 4`!
    let (r_ep1_4, _) = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: vec![events[3].clone()],
        })
        .unwrap();
    assert_eq!(
        r_ep1_4
            .origin_frontier
            .get(&("prod-quar".into(), "ep-1".into()))
            .copied(),
        Some(6)
    );

    // Now send a forged conflicting event for `(prod-quar, ep-1, seq = 4)` with a different digest!
    let forged_env = json!({
        "kind": "session_upsert",
        "session_id": "forged-hijack",
        "harness": "claude",
        "title": "MUTATED HISTORY",
        "message_count": 99,
    });
    let forged_ev =
        IngestEventRecord::new(EventId::new("prod-quar", "ep-1", 4), "op:forged:4", forged_env, None).unwrap();
    assert_ne!(forged_ev.digest, events[3].digest);

    let err = shipper
        .send_ingest_batch(&IngestBatchRequest {
            events: vec![forged_ev.clone()],
        })
        .unwrap_err();
    match err {
        cv_core::state::hub::HubError::Transport { status, message } => {
            assert_eq!(status, Some(409), "expected 409 Conflict: {message}");
            let body: Value = serde_json::from_str(&message).unwrap();
            assert_eq!(body["quarantined"], true, "{body}");
            assert_eq!(body["event_id"]["producer_id"], "prod-quar");
            assert_eq!(body["event_id"]["epoch"], "ep-1");
            assert_eq!(body["event_id"]["seq"], 4);
            assert_eq!(body["existing_digest"], events[3].digest);
            assert_eq!(body["conflicting_digest"], forged_ev.digest);
            assert_eq!(body["quarantine_count"], 1);
            assert_eq!(
                body["origin_frontier"]["prod-quar/ep-1"], 3,
                "frontier MUST freeze below conflict seq=4 (at 3): {body}"
            );
        }
        other => panic!("expected Transport(409), got {other:?}"),
    }

    // Verify `GET /api/hub/status`: quarantine recorded, `first_conflict_seq == Some(4)`,
    // `contiguous_seq == 3`, and `"forged-hijack"` NEVER entered `hub_sessions`!
    let (st_raw, _) = shipper.client().get("/api/hub/status").unwrap();
    let st: HubStatusResponse = st_raw.json().unwrap();
    assert_eq!(st.quarantine.count, 1);
    assert_eq!(st.quarantine.entries[0].event_id, EventId::new("prod-quar", "ep-1", 4));
    assert_eq!(st.quarantine.entries[0].existing_digest, events[3].digest);
    assert_eq!(st.quarantine.entries[0].conflicting_digest, forged_ev.digest);
    assert_eq!(st.frontiers[0].contiguous_seq, 3);
    assert_eq!(st.frontiers[0].first_conflict_seq, Some(4));

    let (sess_raw, _) = shipper.client().get("/api/hub/sessions").unwrap();
    let page: HubSessionsPage = sess_raw.json().unwrap();
    let ids: Vec<&str> = page.sessions.iter().map(|r| r.id.as_str()).collect();
    assert!(!ids.contains(&"forged-hijack"), "forged event must never materialize");
    assert_eq!(page.total, 3, "only conflict-free prefix 1..=3 remains visible");
}

/// 4. Multi-producer live hub: sleeping/offline laptop hydration from mirrored Zstd-Seekable
///    `hub/packs/`, pinned `?snapshot=<pub_id>` immutability across new turns, tear-free
///    multi-page `?snapshot=` pagination, `X-Cv-Snapshot` stamping on `/api/*`, and metadata-only
///    `pack_not_mirrored` structured fallback.
#[test]
fn fleet_hub_offline_hydration_pinned_snapshots_and_pagination() {
    let hub_w = World::new("hub-offline-pinned");
    let (hub_url, _port, _reaper) = spawn_hub_serve(&hub_w, Some("fleet-secret"));

    // Unauthenticated & query-token POST requests are rejected 401.
    let unauth_client = HttpClient::new(&hub_url, None).unwrap();
    assert_eq!(unauth_client.get("/api/hub/status").unwrap().0.status, 401);
    assert_eq!(
        unauth_client
            .post_json("/api/hub/enroll?token=fleet-secret", &json!({"producer_id": "sneaky"}))
            .unwrap()
            .0
            .status,
        401,
        "query ?token= must never authenticate hub POST writes"
    );

    // Producer A (`laptop-alpha`): write real Claude transcripts in its own isolated World,
    // discover & capture them into local `StateStore`, sync to hub -> publishes `pub_1`.
    let pub_1 = {
        let prod_a = World::in_process("prod-laptop-alpha", Env::enter_isolated);
        prod_a.write_session(
            "alpha-pelican",
            &[
                json!({"type": "ai-title", "aiTitle": "Pelican migration on laptop-alpha"}),
                json!({"type": "user", "uuid": "u1", "timestamp": "2026-07-01T10:00:00Z",
                       "cwd": "/work/pelican",
                       "message": {"role": "user", "content": "fix pelican parser edge case"}}),
                json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-07-01T10:01:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "tool_use", "id": "tu1", "name": "Edit",
                            "input": {"file_path": "src/pelican.rs"}},
                           {"type": "text", "text": "patched pelican parser cleanly"}]}}),
            ],
        );
        prod_a.write_session(
            "alpha-albatross",
            &[
                json!({"type": "user", "uuid": "u2", "timestamp": "2026-07-01T11:00:00Z",
                       "cwd": "/work/albatross",
                       "message": {"role": "user", "content": "albatross benchmark"}}),
                json!({"type": "assistant", "uuid": "a2", "timestamp": "2026-07-01T11:02:00Z",
                       "message": {"role": "assistant", "content": [{"type": "text", "text": "42 ns/iter"}]}}),
            ],
        );

        let store_a = StateStore::at_dir(&prod_a.cv_home);
        let local_packs_a = prod_a.cv_home.join("packs");
        let receipts = capture_discovered_sessions(&store_a, &local_packs_a).unwrap();
        assert_eq!(receipts.len(), 2);

        let shipper_a = Shipper::new(
            ShipConfig::new("hub", &hub_url, "laptop-alpha")
                .with_token("fleet-secret")
                .with_host_label("alpha.local"),
        )
        .unwrap();
        let rep_a = shipper_a.sync(&store_a, &local_packs_a).unwrap();
        assert_eq!(rep_a.events_shipped, 2);
        assert_eq!(rep_a.hub_generation, Some(1));

        // Simulate Producer A going completely offline / disk unmounted: dropping `World` removes
        // `base_a` (transcripts, local `state.db`, local `packs/`), and we remove any remnant if present.
        let base_a = prod_a.base.clone();
        drop(prod_a);
        if base_a.exists() {
            fs::remove_dir_all(&base_a).unwrap();
        }
        assert!(!base_a.exists(), "Producer A's disk must be completely gone");

        rep_a.hub_snapshot_id.expect("pub_1")
    };

    // Producer B (`laptop-beta`) ships 3 sessions -> publishes `pub_2` (total 5 fleet sessions).
    let store_b_dir = hub_w.base.join("prod-b-state");
    let packs_b_dir = hub_w.base.join("prod-b-packs");
    let store_b = StateStore::at_dir(&store_b_dir);
    for (idx, name) in ["beta-one", "beta-two", "beta-three"].iter().enumerate() {
        let s = make_session(
            name,
            "/work/beta",
            &format!("Beta {name}"),
            "initial beta prompt",
            "initial beta reply",
            (idx as u32) + 10,
        );
        capture_session_to_store(&store_b, &packs_b_dir, &s, true).unwrap();
    }
    let shipper_b = Shipper::new(
        ShipConfig::new("hub", &hub_url, "laptop-beta")
            .with_token("fleet-secret")
            .with_host_label("beta.local"),
    )
    .unwrap();
    let rep_b1 = shipper_b.sync(&store_b, &packs_b_dir).unwrap();
    assert_eq!(rep_b1.hub_generation, Some(2));
    let pub_2 = rep_b1.hub_snapshot_id.expect("pub_2");
    assert_ne!(pub_1, pub_2);

    // Producer B now appends a new turn to `beta-one` (4 messages instead of 2) AND adds `beta-four`
    // -> publishes `pub_3` (generation 3).
    let mut updated_beta_one = make_session(
        "beta-one",
        "/work/beta",
        "Beta beta-one (v2)",
        "initial beta prompt",
        "initial beta reply",
        40,
    );
    updated_beta_one
        .messages
        .push(msg_text(Role::User, "follow-up turn on beta-one"));
    updated_beta_one
        .messages
        .push(msg_text(Role::Assistant, "second reply on beta-one"));
    capture_session_to_store(&store_b, &packs_b_dir, &updated_beta_one, true).unwrap();
    let beta_four = make_session("beta-four", "/work/beta", "Beta four", "q4", "a4", 45);
    capture_session_to_store(&store_b, &packs_b_dir, &beta_four, true).unwrap();
    let rep_b2 = shipper_b.sync(&store_b, &packs_b_dir).unwrap();
    assert_eq!(rep_b2.hub_generation, Some(3));
    let pub_3 = rep_b2.hub_snapshot_id.expect("pub_3");

    let client = shipper_b.client();

    // A. Sleeping/Offline Producer Hydration: `laptop-alpha` is deleted from disk, yet
    //    `GET /api/hub/session/laptop-alpha/alpha-pelican?snapshot=<pub_1>` hydrates the full IR
    //    (including tool calls and assistant text) from mirrored Zstd-Seekable `hub/packs/`!
    let (hyd_raw, _) = client
        .get(&format!("/api/hub/session/laptop-alpha/alpha-pelican?snapshot={pub_1}"))
        .unwrap();
    assert_eq!(hyd_raw.status, 200);
    assert_eq!(hyd_raw.header("X-Cv-Snapshot"), Some(pub_1.as_str()));
    assert_eq!(hyd_raw.header("X-Cv-Hub-Generation"), Some("1"));
    let hyd_json: Value = hyd_raw.json().unwrap();
    assert_eq!(hyd_json["payload_unavailable"], false);
    assert_eq!(hyd_json["id"], "alpha-pelican");
    assert_eq!(hyd_json["producer_id"], "laptop-alpha");
    assert_eq!(hyd_json["snapshot_id"], pub_1);
    let hyd_str = serde_json::to_string(&hyd_json["messages"]).unwrap();
    assert!(hyd_str.contains("patched pelican parser cleanly"), "{hyd_str}");
    assert!(hyd_str.contains("src/pelican.rs"), "{hyd_str}");

    // B. Pinned `?snapshot=` Immutability:
    //    - `beta-one` at `?snapshot=<pub_2>` STILL has 2 messages and title `"Beta beta-one"`,
    //      whereas unpinned (latest `pub_3`) has 4 messages and title `"Beta beta-one (v2)"`!
    let (b1_v2_raw, _) = client
        .get(&format!("/api/hub/session/laptop-beta/beta-one?snapshot={pub_2}"))
        .unwrap();
    assert_eq!(b1_v2_raw.status, 200);
    assert_eq!(b1_v2_raw.header("X-Cv-Snapshot"), Some(pub_2.as_str()));
    assert_eq!(b1_v2_raw.header("X-Cv-Hub-Generation"), Some("2"));
    let b1_v2: Value = b1_v2_raw.json().unwrap();
    assert_eq!(b1_v2["title"], "Beta beta-one");
    assert_eq!(b1_v2["messages"].as_array().unwrap().len(), 2);

    let (b1_v3_raw, _) = client.get("/api/hub/session/laptop-beta/beta-one").unwrap();
    assert_eq!(b1_v3_raw.header("X-Cv-Snapshot"), Some(pub_3.as_str()));
    assert_eq!(b1_v3_raw.header("X-Cv-Hub-Generation"), Some("3"));
    let b1_v3: Value = b1_v3_raw.json().unwrap();
    assert_eq!(b1_v3["title"], "Beta beta-one (v2)");
    assert_eq!(b1_v3["messages"].as_array().unwrap().len(), 4);

    // C. Tear-Free Multi-Page Pagination on Pinned `?snapshot=<pub_2>` even while `pub_3` is live:
    let mut paged_ids = Vec::new();
    for offset in [0, 2, 4] {
        let (p_raw, _) = client
            .get(&format!("/api/hub/sessions?snapshot={pub_2}&limit=2&offset={offset}"))
            .unwrap();
        assert_eq!(p_raw.status, 200);
        assert_eq!(p_raw.header("X-Cv-Snapshot"), Some(pub_2.as_str()));
        assert_eq!(p_raw.header("X-Cv-Hub-Generation"), Some("2"));
        let page: HubSessionsPage = p_raw.json().unwrap();
        assert_eq!(page.total, 5, "pub_2 must stay frozen at 5 sessions");
        for row in page.sessions {
            if row.id == "beta-one" {
                assert_eq!(row.message_count, 2, "pinned page must show pub_2 message_count");
            }
            paged_ids.push(format!("{}/{}", row.producer_id, row.id));
        }
    }
    assert_eq!(paged_ids.len(), 5);
    assert!(
        !paged_ids.iter().any(|k| k.ends_with("/beta-four")),
        "beta-four from pub_3 must not tear into pub_2 pages"
    );

    // Query calculus (`?q=text:pelican` and `?q=tool:edit`) over mirrored packs works even with Producer A offline!
    let (q_raw, _) = client.get("/api/hub/sessions?q=text%3Apelican%20tool%3Aedit").unwrap();
    let q_page: HubSessionsPage = q_raw.json().unwrap();
    assert_eq!(q_page.total, 1);
    assert_eq!(q_page.sessions[0].id, "alpha-pelican");

    // D. `/api/*` endpoints stamp `X-Cv-Snapshot: <latest_publication_id>` and expose it via CORS!
    let (health_raw, _) = client
        .send("GET", "/api/health", None, &[("Origin", "http://localhost:5173")], &[])
        .unwrap();
    assert_eq!(health_raw.status, 200);
    assert_eq!(health_raw.header("X-Cv-Snapshot"), Some(pub_3.as_str()));
    let exposed = health_raw.header("Access-Control-Expose-Headers").unwrap_or_default();
    assert!(
        exposed.contains("X-Cv-Snapshot") && exposed.contains("X-Cv-Hub-Generation"),
        "CORS must expose hub snapshot headers: {exposed}"
    );

    // E. Metadata-Only Producer (`include_pack_payload = false`): returns structured 200 OK
    //    `{ "payload_unavailable": true, "reason": "pack_not_mirrored", "metadata": ... }`!
    let meta_store = StateStore::at_dir(hub_w.base.join("meta-only-state"));
    let meta_packs = hub_w.base.join("meta-only-packs");
    let meta_sess = make_session(
        "meta-only-1",
        "/work/meta",
        "Metadata Only Session",
        "no pack shipped",
        "ok",
        50,
    );
    capture_session_to_store(&meta_store, &meta_packs, &meta_sess, false).unwrap();
    let shipper_meta = Shipper::new(
        ShipConfig::new("hub", &hub_url, "meta-producer")
            .with_token("fleet-secret")
            .with_host_label("meta.local"),
    )
    .unwrap();
    let rep_meta = shipper_meta.sync(&meta_store, &meta_packs).unwrap();
    assert!(rep_meta.uploaded_pack_id.is_none());

    let (unavail_raw, _) = client.get("/api/hub/session/meta-producer/meta-only-1").unwrap();
    assert_eq!(unavail_raw.status, 200);
    let unavail: Value = unavail_raw.json().unwrap();
    assert_eq!(unavail["payload_unavailable"], true, "{unavail}");
    assert_eq!(unavail["reason"], "pack_not_mirrored", "{unavail}");
    assert_eq!(unavail["metadata"]["title"], "Metadata Only Session", "{unavail}");
    assert_eq!(unavail["metadata"]["message_count"], 2, "{unavail}");
}

/// 5. `[replication.<target>]` Configurable Privacy Filter & Scrubbing Pipeline + Two-Stage Ack
///    Accounting (`TransportDeliveryAck` vs durable `CustodyAck` gating `is_pack_evictable`) +
///    exhaustive hub disk/pack zero-leak audit.
#[test]
fn fleet_hub_privacy_scrubbing_and_two_stage_custody_acks() {
    let hub_w = World::new("hub-privacy");
    let (hub_url, _port, _reaper) = spawn_hub_serve(&hub_w, Some("priv-token"));

    let prod_w = World::in_process("prod-privacy", Env::enter_isolated);
    let home_str = prod_w.home.to_string_lossy().into_owned();
    let allowed_repo = prod_w.home.join("dev/acme-service");
    let denied_cwd = prod_w.home.join("secret-stealth/omega");

    let cfg_toml = format!(
        r#"
[replication.team-hub]
private_ips = true
hostnames = true
ssh_principals = true
rewrite_home = true
rewrite_repo = true
principal = {{ mode = "hash", salt = "fleet-salt-99" }}

[replication.team-hub.classes]
api_keys = true
private_keys = true
jwts = true
emails = true
blobs = true
assignments = true

[[replication.team-hub.path_rewrites]]
pattern = "/Volumes/CorpVault"
replacement = "$VAULT"

[replication.team-hub.filter]
allow_cwds = ["{home_str}/dev/*"]
deny_cwds = ["{home_str}/secret-stealth/*"]
harnesses = ["claude"]
tags = ["team-share"]
"#
    );
    let parsed_cfg: Config = toml::from_str(&cfg_toml).unwrap();
    cv_core::config::save(&parsed_cfg).unwrap();

    // Session 1: Admitted (`cwd` under `~/dev/*`, harness `claude`, tag `team-share`), packed with
    // realistic secrets, PEM keys, `$HOME` & `$REPO` & `/Volumes/CorpVault` paths, hostnames, IPs,
    // SSH principals, and user principal!
    let api_secret = "sk-ant-api03-Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2Ji1Hg0Fe9Dc8Ba7Zy6Xw5Vu4Ts3Rq2";
    let pem_block = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ==\n-----END OPENSSH PRIVATE KEY-----";
    let mut s_admitted = make_session(
        "sess-admitted",
        &allowed_repo.to_string_lossy(),
        &format!("Fix deploy on build-node-07.internal.corp in {home_str}/dev/acme-service"),
        &format!(
            "Read {home_str}/.ssh/config and {}/src/auth.rs and /Volumes/CorpVault/prod.env; \
             connect via deploy@bastion.internal.corp (10.42.88.19) or devbox.tailnet.ts.net \
             using key {api_secret} and\n{pem_block}",
            allowed_repo.display()
        ),
        &format!(
            "Updated {}/src/auth.rs; contacted owner alice.sec@acme.example",
            allowed_repo.display()
        ),
        10,
    );
    s_admitted.principal = Some(Principal::String("alice.sec@acme.example".into()));
    s_admitted.extra.insert("cv".into(), json!({ "tags": ["team-share"] }));
    s_admitted.messages[1].content.push(Block::ToolUse {
        id: "tu-secret".into(),
        name: "Bash".into(),
        input: serde_json::from_value(json!({
            "command": format!("ANTHROPIC_API_KEY={api_secret} ssh deploy@10.42.88.19 cat {home_str}/.netrc")
        }))
        .unwrap(),
        namespace: None,
    });

    // Session 2: Denied by `deny_cwds` (`~/secret-stealth/omega`).
    let mut s_denied_cwd = make_session(
        "sess-denied-cwd",
        &denied_cwd.to_string_lossy(),
        "Top Secret Stealth",
        "CANARY_STEALTH_CWD_NEVER_SEND_9999",
        "classified reply",
        15,
    );
    s_denied_cwd
        .extra
        .insert("cv".into(), json!({ "tags": ["team-share"] }));

    // Session 3: Denied by missing `team-share` tag.
    let mut s_denied_tag = make_session(
        "sess-denied-tag",
        &allowed_repo.to_string_lossy(),
        "Untagged Personal",
        "CANARY_UNTAGGED_PRIVATE_NEVER_SEND_8888",
        "personal note",
        20,
    );
    s_denied_tag
        .extra
        .insert("cv".into(), json!({ "tags": ["personal-only"] }));

    // Session 4: Denied by harness (`codex` not in `harnesses = ["claude"]`).
    let mut s_denied_harness = make_session(
        "sess-denied-harness",
        &allowed_repo.to_string_lossy(),
        "Codex Run",
        "CANARY_CODEX_HARNESS_NEVER_SEND_7777",
        "codex reply",
        25,
    );
    s_denied_harness.harness = Harness::Codex;
    s_denied_harness
        .extra
        .insert("cv".into(), json!({ "tags": ["team-share"] }));

    let store = StateStore::at_dir(&prod_w.cv_home);
    let local_packs = prod_w.cv_home.join("packs");
    let recorder = RecorderServer::start(store.clone(), "prod-sec").unwrap();
    for s in [&s_admitted, &s_denied_cwd, &s_denied_tag, &s_denied_harness] {
        capture_session_to_store(&store, &local_packs, s, true).unwrap();
    }
    recorder.shutdown();

    // Seal all 4 local commits into a single local `pack_manifests` entry so we can test
    // Two-Stage Ack Accounting & `is_pack_evictable`!
    let sealed_local = seal_local_inbox_to_pack(&store, &local_packs, Some("local-consolidated-1"))
        .unwrap()
        .expect("sealed local pack");
    assert_eq!(sealed_local.record_count, 4);
    assert!(
        !store.is_pack_evictable("local-consolidated-1", &["team-hub"]).unwrap(),
        "pack MUST NOT be evictable before durable hub CustodyAck"
    );

    let shipper = Shipper::new(
        ShipConfig::new("team-hub", &hub_url, "prod-sec")
            .with_token("priv-token")
            .with_host_label("alice-mbp.internal.corp"),
    )
    .unwrap();

    // Stage-1 ONLY check: `enroll` gives a `TransportDeliveryAck`, NOT a durable `CustodyAck`!
    let (enroll_resp, transport_ack) = shipper.enroll().unwrap();
    assert_eq!(transport_ack.status, 200);
    assert!(
        !enroll_resp.producer.host_label.contains("internal.corp"),
        "enrolled host_label must also be scrubbed: {:?}",
        enroll_resp.producer.host_label
    );
    assert!(
        !store.is_pack_evictable("local-consolidated-1", &["team-hub"]).unwrap(),
        "Stage-1 transport ack alone must NOT mark local pack evictable"
    );

    // Full sync: scrubs & filters locally, uploads scrubbed pack, ingests batch, verifies hub
    // pack BLAKE3 + `durable_fsynced`, and records Stage-2 `CustodyAck`!
    let report = shipper.sync(&store, &local_packs).unwrap();
    assert_eq!(report.events_examined, 4);
    assert_eq!(report.events_shipped, 4);
    assert_eq!(report.sessions_admitted, 1);
    assert_eq!(report.sessions_filtered_out, 3);
    assert!(
        report.redactions_applied >= 6,
        "expected redactions, got {}",
        report.redactions_applied
    );
    assert!(report.transport_acks.len() >= 3);
    assert_eq!(report.durable_custody_acks.len(), 1);
    assert_eq!(report.durable_custody_acks[0].contiguous_seq, 4);

    // Stage-2 durable `CustodyAck` now permits local pack eviction for `["team-hub"]`, while an
    // unacked second replication target (`dr-vault`) still blocks eviction!
    assert!(
        store.is_pack_evictable("local-consolidated-1", &["team-hub"]).unwrap(),
        "local pack must become evictable after verified durable hub CustodyAck"
    );
    assert!(
        !store
            .is_pack_evictable("local-consolidated-1", &["team-hub", "dr-vault"])
            .unwrap(),
        "unacked second target must still retain local pack"
    );

    // Verify hub sessions & hydrated scrubbed payload!
    let (page_raw, _) = shipper.client().get("/api/hub/sessions").unwrap();
    let page: HubSessionsPage = page_raw.json().unwrap();
    assert_eq!(page.total, 1, "only admitted session may appear on hub");
    assert_eq!(page.sessions[0].id, "sess-admitted");
    assert_eq!(page.sessions[0].cwd.as_deref(), Some("$REPO"));

    let (hyd_raw, _) = shipper.client().get("/api/hub/session/prod-sec/sess-admitted").unwrap();
    assert_eq!(hyd_raw.status, 200);
    let hyd: Value = hyd_raw.json().unwrap();
    let hyd_text = serde_json::to_string(&hyd).unwrap();
    assert!(hyd_text.contains("$REPO/src/auth.rs"), "{hyd_text}");
    assert!(hyd_text.contains("$HOME/.ssh/config"), "{hyd_text}");
    assert!(hyd_text.contains("$VAULT/prod.env"), "{hyd_text}");
    assert!(hyd_text.contains("[REDACTED:"), "{hyd_text}");
    let principal_str = hyd["principal"].as_str().expect("hashed principal string");
    assert!(principal_str.starts_with("sha256:"), "principal={principal_str}");

    // Exhaustive wire/disk zero-leak audit across hub `state.db`, WAL, and decompressed `hub/packs/*`!
    let forbidden_substrings = [
        api_secret,
        "Zx9Yw8Vu7Ts6Rq5",
        "BEGIN OPENSSH PRIVATE KEY",
        "b3BlbnNzaC1rZXktdjE",
        home_str.as_str(),
        "/Volumes/CorpVault",
        "build-node-07.internal.corp",
        "bastion.internal.corp",
        "devbox.tailnet.ts.net",
        "alice-mbp.internal.corp",
        "10.42.88.19",
        "alice.sec@acme.example",
        "CANARY_STEALTH_CWD_NEVER_SEND_9999",
        "CANARY_UNTAGGED_PRIVATE_NEVER_SEND_8888",
        "CANARY_CODEX_HARNESS_NEVER_SEND_7777",
        "sess-denied-cwd",
        "sess-denied-tag",
        "sess-denied-harness",
    ];

    let mut hub_corpora: Vec<(String, Vec<u8>)> = Vec::new();
    for db_file in ["state.db", "state.db-wal"] {
        let p = hub_w.cv_home.join(db_file);
        if p.exists() {
            hub_corpora.push((db_file.into(), fs::read(&p).unwrap()));
        }
    }
    let hub_packs_dir = hub_w.cv_home.join("hub/packs");
    for entry in fs::read_dir(&hub_packs_dir).unwrap() {
        let path = entry.unwrap().path();
        let fname = path.file_name().unwrap().to_string_lossy().into_owned();
        hub_corpora.push((fname.clone(), fs::read(&path).unwrap()));
        if let Some(pack_id) = fname.strip_suffix(".pack.idx.json") {
            let sealed = SealedPack::open(&hub_packs_dir, pack_id).unwrap();
            for span in sealed.record_index().records() {
                let addr = span.clone().into_addr(pack_id);
                let decoded = sealed.read_payload(&addr).unwrap();
                hub_corpora.push((format!("decoded:{pack_id}:{}", span.anchor), decoded));
            }
        }
    }
    assert!(!hub_corpora.is_empty());

    for (source_label, raw_bytes) in &hub_corpora {
        let lossy = String::from_utf8_lossy(raw_bytes);
        for &secret in &forbidden_substrings {
            assert!(
                !lossy.contains(secret),
                "PRIVACY LEAK: forbidden substring {secret:?} found on hub in {source_label}"
            );
        }
    }
}

/// 6. Rebuild Parity Invariant (`gpt-6-astra` Gate): wipe derived hub views (`hub_snapshots`,
///    `hub_sessions`, `hub_frontiers`, plus `hub_packs` table rows) and rebuild strictly from raw
///    `hub_ingested_events` + `hub_quarantine` + `hub/packs/` on disk -> resulting `Snapshot`
///    frontier, manifest (`publication_id`, `pack_digests`, `session_revisions`), and hydrated
///    sessions match incremental ingest bit-for-bit!
#[test]
fn fleet_hub_rebuild_from_raw_matches_incremental_bit_for_bit() {
    let hub_w = World::new("hub-rebuild-parity");
    let scratch = hub_w.base.join("producer-packs");
    let (hub_url, _port, _reaper) = spawn_hub_serve(&hub_w, Some("rebuild-token"));

    let shipper_a = Shipper::new(
        ShipConfig::new("hub", &hub_url, "prod-a")
            .with_token("rebuild-token")
            .with_host_label("node-a"),
    )
    .unwrap();
    let shipper_b = Shipper::new(
        ShipConfig::new("hub", &hub_url, "prod-b")
            .with_token("rebuild-token")
            .with_host_label("node-b"),
    )
    .unwrap();
    shipper_a.enroll().unwrap();
    shipper_b.enroll().unwrap();

    // Producer A: out-of-order arrival (`3, 4` before `1, 2`), plus a session revision update on
    // `a-sess-1` at `seq = 3` and a `session_delete` on `a-sess-2` at `seq = 4`!
    let a_s1_v1 = make_session("a-sess-1", "/work/a", "A1 v1", "hello", "world", 5);
    let mut a_s1_v2 = make_session("a-sess-1", "/work/a", "A1 v2 (final)", "hello", "world", 12);
    a_s1_v2.messages.push(msg_text(Role::User, "extra turn"));
    a_s1_v2.messages.push(msg_text(Role::Assistant, "extra answer"));
    let a_s2 = make_session("a-sess-2", "/work/a", "A2 ephemeral", "temp", "gone", 8);
    let a_packed = build_pack_with_sessions(
        &scratch,
        "pack-rebuild-a",
        &[a_s1_v1.clone(), a_s2.clone(), a_s1_v2.clone()],
    );
    shipper_a.upload_pack_files(&scratch, "pack-rebuild-a").unwrap();

    let ev_a1 = make_event("prod-a", "ep-1", 1, &a_packed[0].0, Some(a_packed[0].1.clone()));
    let ev_a2 = make_event("prod-a", "ep-1", 2, &a_packed[1].0, Some(a_packed[1].1.clone()));
    let ev_a3 = make_event("prod-a", "ep-1", 3, &a_packed[2].0, Some(a_packed[2].1.clone()));
    let ev_a4 = IngestEventRecord::new(
        EventId::new("prod-a", "ep-1", 4),
        "op:prod-a:delete:a-sess-2",
        json!({
            "kind": "session_delete",
            "session_id": "a-sess-2",
            "updated_at": "2026-06-15T12:20:00Z",
        }),
        None,
    )
    .unwrap();

    // Send `3, 4` first, then `1, 2`.
    shipper_a
        .send_ingest_batch(&IngestBatchRequest {
            events: vec![ev_a3, ev_a4],
        })
        .unwrap();
    shipper_a
        .send_ingest_batch(&IngestBatchRequest {
            events: vec![ev_a1, ev_a2],
        })
        .unwrap();

    // Producer B: 2 packed sessions (`seq = 1, 2`), 1 metadata-only session (`seq = 3`), a gap at
    // `seq = 5` (buffered beyond `seq = 4` missing? Wait: let's also include a quarantined conflict
    // at `seq = 4` after `seq = 1..=3` + buffered `seq = 5` so rebuild tests partial frontiers &
    // quarantine parity too!).
    let b_s1 = make_session("b-sess-1", "/work/b", "B1", "ub1", "ab1", 14);
    let b_s2 = make_session("b-sess-2", "/work/b", "B2", "ub2", "ab2", 16);
    let b_s5 = make_session("b-sess-5-buffered", "/work/b", "B5 buffered", "ub5", "ab5", 25);
    let b_packed = build_pack_with_sessions(&scratch, "pack-rebuild-b", &[b_s1.clone(), b_s2.clone(), b_s5.clone()]);
    shipper_b.upload_pack_files(&scratch, "pack-rebuild-b").unwrap();

    let ev_b1 = make_event("prod-b", "ep-1", 1, &b_packed[0].0, Some(b_packed[0].1.clone()));
    let ev_b2 = make_event("prod-b", "ep-1", 2, &b_packed[1].0, Some(b_packed[1].1.clone()));
    let b_meta = make_session("b-sess-meta", "/work/b", "B3 meta", "m", "m", 18);
    let ev_b3 = make_event("prod-b", "ep-1", 3, &b_meta, None);
    let ev_b4_orig = IngestEventRecord::new(
        EventId::new("prod-b", "ep-1", 4),
        "op:prod-b:4:orig",
        json!({"kind": "heartbeat"}),
        None,
    )
    .unwrap();
    let ev_b5 = make_event("prod-b", "ep-1", 5, &b_packed[2].0, Some(b_packed[2].1.clone()));

    shipper_b
        .send_ingest_batch(&IngestBatchRequest {
            events: vec![ev_b1, ev_b2, ev_b3, ev_b4_orig, ev_b5],
        })
        .unwrap();

    // Inject conflict at `(prod-b, ep-1, seq = 4)` -> freezes `prod-b` frontier at `3` (`seq = 5`
    // stays above the conflict and excluded!).
    let ev_b4_conflict = IngestEventRecord::new(
        EventId::new("prod-b", "ep-1", 4),
        "op:prod-b:4:conflict",
        json!({"kind": "session_upsert", "session_id": "bad", "harness": "claude"}),
        None,
    )
    .unwrap();
    let conflict_res = shipper_b.send_ingest_batch(&IngestBatchRequest {
        events: vec![ev_b4_conflict],
    });
    assert!(conflict_res.is_err());

    let client = shipper_a.client();

    // Capture incremental ground truth from live HTTP endpoints BEFORE wiping!
    let (status_before_raw, _) = client.get("/api/hub/status").unwrap();
    let status_before: HubStatusResponse = status_before_raw.json().unwrap();
    let snap_before = status_before
        .latest_snapshot
        .clone()
        .expect("latest snapshot before wipe");

    let (snap_ep_before_raw, _) = client.get("/api/hub/snapshot").unwrap();
    let snap_ep_before: HubSnapshotResponse = snap_ep_before_raw.json().unwrap();
    assert_eq!(snap_ep_before.snapshot, snap_before);

    let (sessions_before_raw, _) = client.get("/api/hub/sessions").unwrap();
    let sessions_before: HubSessionsPage = sessions_before_raw.json().unwrap();
    assert_eq!(sessions_before.total, 4); // a-sess-1 (v2), b-sess-1, b-sess-2, b-sess-meta

    let mut hydrated_before: BTreeMap<String, Value> = BTreeMap::new();
    for row in &sessions_before.sessions {
        let key = encode_pair_key(&row.producer_id, &row.id);
        let (h_raw, _) = client
            .get(&format!("/api/hub/session/{}/{}", row.producer_id, row.id))
            .unwrap();
        assert_eq!(h_raw.status, 200);
        let mut val: Value = h_raw.json().unwrap();
        // Normalize `hub_generation` (rebuild starts fresh generation 1, while `publication_id` and
        // every content/frontier/session field MUST match bit-for-bit!).
        if let Some(obj) = val.as_object_mut() {
            obj.remove("hub_generation");
        }
        hydrated_before.insert(key, val);
    }

    // Wipe derived views (`hub_snapshots`, `hub_sessions`, `hub_frontiers`) AND clear `hub_packs`
    // rows to force full reconciliation from raw `hub_ingested_events` + `hub_quarantine` + `hub/packs/`!
    let hub_store = HubStore::at_home(&hub_w.cv_home);
    hub_store.wipe_derived_views().unwrap();
    {
        let conn = hub_store.state_store().open_writable().unwrap();
        conn.execute("DELETE FROM hub_packs", []).unwrap();
    }

    let (wiped_sessions_raw, _) = client.get("/api/hub/sessions").unwrap();
    let wiped_sessions: HubSessionsPage = wiped_sessions_raw.json().unwrap();
    assert_eq!(wiped_sessions.total, 0, "derived sessions must be empty after wipe");

    // Rebuild strictly from raw `hub_ingested_events` + `hub_quarantine` + `hub/packs/`!
    let rebuilt_snap = hub_store
        .rebuild_from_raw()
        .unwrap()
        .expect("rebuilt snapshot manifest");

    // Bit-for-bit manifest parity!
    assert_eq!(
        rebuilt_snap.publication_id, snap_before.publication_id,
        "rebuilt publication_id MUST match incremental ingest bit-for-bit"
    );
    assert_eq!(rebuilt_snap.origin_frontier, snap_before.origin_frontier);
    assert_eq!(rebuilt_snap.pack_digests, snap_before.pack_digests);
    assert_eq!(rebuilt_snap.session_revisions, snap_before.session_revisions);
    assert_eq!(rebuilt_snap.quarantine_count, snap_before.quarantine_count);

    // Live HTTP queries after rebuild match incremental ingest bit-for-bit!
    let (status_after_raw, _) = client.get("/api/hub/status").unwrap();
    let status_after: HubStatusResponse = status_after_raw.json().unwrap();
    assert_eq!(status_after.quarantine, status_before.quarantine);
    assert_eq!(status_after.frontiers.len(), status_before.frontiers.len());
    for (fa, fb) in status_after.frontiers.iter().zip(status_before.frontiers.iter()) {
        assert_eq!(fa.producer_id, fb.producer_id);
        assert_eq!(fa.epoch, fb.epoch);
        assert_eq!(fa.contiguous_seq, fb.contiguous_seq);
        assert_eq!(fa.max_received_seq, fb.max_received_seq);
        assert_eq!(fa.buffered_count, fb.buffered_count);
        assert_eq!(fa.lag_events, fb.lag_events);
        assert_eq!(fa.missing_ranges, fb.missing_ranges);
        assert_eq!(fa.first_conflict_seq, fb.first_conflict_seq);
    }

    let (sessions_after_raw, _) = client.get("/api/hub/sessions").unwrap();
    let sessions_after: HubSessionsPage = sessions_after_raw.json().unwrap();
    assert_eq!(sessions_after.snapshot_id, sessions_before.snapshot_id);
    assert_eq!(sessions_after.sessions, sessions_before.sessions);

    for row in &sessions_after.sessions {
        let key = encode_pair_key(&row.producer_id, &row.id);
        let (h_raw, _) = client
            .get(&format!("/api/hub/session/{}/{}", row.producer_id, row.id))
            .unwrap();
        assert_eq!(h_raw.status, 200);
        let mut val: Value = h_raw.json().unwrap();
        if let Some(obj) = val.as_object_mut() {
            obj.remove("hub_generation");
        }
        assert_eq!(
            &val,
            hydrated_before.get(&key).unwrap(),
            "rebuilt hydrated session `{key}` drifted from incremental ingest"
        );
    }
}

#[test]
fn fleet_hub_non_session_envelopes_json_scrubbing_and_pack_bounds() {
    let hub_w = World::in_process("fleet-hub-non-session-bounds", Env::enter_isolated);
    let hub_home = hub_w.cv_home.clone();
    let (hub_url, _port, _srv) = spawn_hub_serve(&hub_w, Some("fleet-secret"));
    let client = HttpClient::new(&hub_url, Some("fleet-secret".into())).unwrap();

    // 1. Verify `upload_pack_stream` enforces uncompressed total & per-record caps (`400 Bad Request`)
    //    before `SeekableReader::read_range` allocates.
    let stage_dir = hub_home.join("bomb-stage");
    let s = make_session("bomb-seed", "/work/public", "Seed", "hi", "ok", 1);
    let _ = build_pack_with_sessions(&stage_dir, "pack-bomb-check", &[s]);
    let idx_bytes = fs::read(cv_core::packs::pack_idx_path(&stage_dir, "pack-bomb-check").unwrap()).unwrap();
    let zst_bytes = fs::read(cv_core::packs::pack_zst_path(&stage_dir, "pack-bomb-check").unwrap()).unwrap();
    let probe_shipper = Shipper::new(ShipConfig::new("hub", &hub_url, "probe").with_token("fleet-secret")).unwrap();
    let (seed_up, _) = probe_shipper.upload_pack_files(&stage_dir, "pack-bomb-check").unwrap();
    let zst_hex = seed_up.pack.blake3_digest;
    let mut manifest_json: Value = serde_json::from_slice(&idx_bytes).unwrap();

    // Case A: `total_decoded_len` > 1 GiB
    manifest_json["total_decoded_len"] = json!(2_u64 * 1024 * 1024 * 1024);
    manifest_json["pack_id"] = json!("pack-bomb-total");
    let bomb_total_idx = serde_json::to_vec(&manifest_json).unwrap();
    let mut payload_total = Vec::with_capacity(bomb_total_idx.len() + zst_bytes.len());
    payload_total.extend_from_slice(&bomb_total_idx);
    payload_total.extend_from_slice(&zst_bytes);
    let (resp_total, _) = client
        .send(
            "PUT",
            "/api/hub/packs/pack-bomb-total",
            Some(PACK_STREAM_CONTENT_TYPE),
            &[
                ("X-Cv-Pack-Idx-Len", &bomb_total_idx.len().to_string()),
                ("X-Cv-Pack-Digest", &zst_hex),
            ],
            &payload_total,
        )
        .unwrap();
    assert_eq!(resp_total.status, 400);
    assert!(
        String::from_utf8_lossy(&resp_total.body).contains("exceeds max allowed"),
        "expected total_decoded_len cap error, got: {}",
        String::from_utf8_lossy(&resp_total.body)
    );

    // Case B: per-record `span.length` > 256 MiB (while `total_decoded_len` <= 1 GiB)
    let mut manifest_rec: Value = serde_json::from_slice(&idx_bytes).unwrap();
    manifest_rec["pack_id"] = json!("pack-bomb-rec");
    manifest_rec["total_decoded_len"] = json!(512_u64 * 1024 * 1024);
    manifest_rec["record_index"]["records"][0]["length"] = json!(300_u64 * 1024 * 1024);
    let bomb_rec_idx = serde_json::to_vec(&manifest_rec).unwrap();
    let mut payload_rec = Vec::with_capacity(bomb_rec_idx.len() + zst_bytes.len());
    payload_rec.extend_from_slice(&bomb_rec_idx);
    payload_rec.extend_from_slice(&zst_bytes);
    let (resp_rec, _) = client
        .send(
            "PUT",
            "/api/hub/packs/pack-bomb-rec",
            Some(PACK_STREAM_CONTENT_TYPE),
            &[
                ("X-Cv-Pack-Idx-Len", &bomb_rec_idx.len().to_string()),
                ("X-Cv-Pack-Digest", &zst_hex),
            ],
            &payload_rec,
        )
        .unwrap();
    assert_eq!(resp_rec.status, 400);
    assert!(
        String::from_utf8_lossy(&resp_rec.body).contains("exceeds max allowed"),
        "expected per-record length cap error, got: {}",
        String::from_utf8_lossy(&resp_rec.body)
    );

    // 2. Verify non-session `capture_inbox` envelopes (`receipt`, `lane_start`) + recursive JSON
    //    `Value` leaf scrubbing (`producer_metadata`, quoted secrets in `metadata`, `session.cwd`
    //    `$REPO` rewrite, and non-clobbering of existing `hub_sessions`).
    let prod_store_dir = hub_home.join("prod-mixed-state");
    let prod_packs_dir = hub_home.join("prod-mixed-packs");
    fs::create_dir_all(&prod_packs_dir).unwrap();
    let prod_store = StateStore::at_dir(&prod_store_dir);
    let recorder = RecorderServer::start(prod_store.clone(), "prod-mixed").unwrap();

    let quoted_secret = "sk-ant-api03-quotedsecretvalue1234567890abcdef";
    let enroll_secret = "sk-ant-api03-enrollsecretvalue9876543210fedcba";

    // Event 1: non-session hook receipt with NO `session_id` (must not create `"unknown"` session).
    let env_1 = json!({
        "kind": "lane_start",
        "cwd": "/work/public/sub",
        "note": format!("lane booted on macbook-secret.local with key \"{quoted_secret}\""),
        "principal": "alice-raw-principal",
    });
    prod_store.append("op-hook-1", "dig-hook-1", &env_1).unwrap();

    // Event 2: real session `sess-keep` in `/work/public` with metadata containing quotes + `/work/public` path.
    let sess = make_session(
        "sess-keep",
        "/work/public",
        "Preserved Session Title",
        "keep my messages intact",
        "42",
        5,
    );
    let mut builder = PackBuilder::create(&prod_packs_dir, "local-mixed-pack").unwrap();
    let local_addr = builder
        .append_record("sess-keep", &serde_json::to_vec(&sess).unwrap())
        .unwrap();
    builder.seal(Claim::New).unwrap();
    let env_2 = json!({
        "kind": "session_upsert",
        "session_id": "sess-keep",
        "harness": sess.harness.as_str(),
        "title": sess.title,
        "cwd": "/work/public",
        "message_count": 2,
        "metadata": {
            "quoted_blob": format!("header=\"{quoted_secret}\" tail=/work/public/src/main.rs"),
            "principal": "alice-raw-principal",
        }
    });
    prod_store
        .append_request(
            &cv_core::state::AppendRequest::new("op-sess-keep", "dig-sess-keep", env_2)
                .with_payload_ref(serde_json::to_string(&local_addr).unwrap()),
        )
        .unwrap();

    // Event 3: trailing `receipt` referencing `session_id: "sess-keep"` with NO payload — MUST NOT
    // clobber `sess-keep`'s `title`, `message_count`, or `payload_addr`!
    let env_3 = json!({
        "kind": "receipt",
        "session_id": "sess-keep",
        "cwd": "/work/public",
        "detail": "post-tool hook receipt at /Users/ember/.ssh/config",
    });
    prod_store
        .append("op-receipt-after", "dig-receipt-after", &env_3)
        .unwrap();

    // Event 4: non-session envelope in denied `/work/secret-ops` — MUST be filtered out!
    let env_4 = json!({
        "kind": "receipt",
        "cwd": "/work/secret-ops",
        "detail": "top-secret-denied-hook-marker",
    });
    prod_store.append("op-denied-hook", "dig-denied-hook", &env_4).unwrap();

    recorder.shutdown();

    let toml_cfg = r#"
[replication.hub]
private_ips = true
hostnames = true
rewrite_home = true
rewrite_repo = true
principal = "hash"

[replication.hub.filter]
allow_cwds = ["/work/public"]
deny_cwds = ["/work/secret-ops"]
"#;
    let parsed_cfg: Config = toml::from_str(toml_cfg).unwrap();
    let profile = cv_core::state::hub::resolve_replication_profile(&parsed_cfg, "hub");

    let mut ship_cfg = ShipConfig::new("hub", &hub_url, "prod-mixed")
        .with_token("fleet-secret")
        .with_host_label("macbook-secret.local")
        .with_profile(profile);
    ship_cfg.producer_metadata = json!({
        "owner_token": format!("bearer=\"{enroll_secret}\""),
        "home_hint": "/Users/ember/dev",
        "principal": "alice-raw-principal",
    });

    let shipper = Shipper::new(ship_cfg).unwrap();
    let rep = shipper.sync(&prod_store, &prod_packs_dir).unwrap();
    assert_eq!(rep.events_shipped, 4);
    assert_eq!(rep.sessions_admitted, 1);
    assert_eq!(rep.sessions_filtered_out, 1);

    // Check hub sessions: ONLY `sess-keep` exists (no `"unknown"` row), and its title,
    // message_count (2), metadata (`$REPO/src/main.rs`), and mirrored pack hydration stayed intact!
    let (page_raw, _) = client.get("/api/hub/sessions").unwrap();
    let page: HubSessionsPage = page_raw.json().unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.sessions[0].id, "sess-keep");
    assert_eq!(page.sessions[0].title.as_deref(), Some("Preserved Session Title"));
    assert_eq!(page.sessions[0].message_count, 2);

    let (snap_raw, _) = client.get("/api/hub/snapshot").unwrap();
    let snap: HubSnapshotResponse = snap_raw.json().unwrap();
    assert_eq!(
        snap.frontiers[0].contiguous_seq, 4,
        "all 4 events (including non-session receipts) must advance F(p, e)"
    );
    let rev_key = encode_pair_key("prod-mixed", "sess-keep");
    let rev = snap.snapshot.session_revisions.get(&rev_key).unwrap();
    let rev_meta_str = serde_json::to_string(&rev.metadata).unwrap();
    assert!(
        rev_meta_str.contains("$REPO/src/main.rs"),
        "session metadata must preserve JSON structure and rewrite session.cwd to $REPO: {rev_meta_str}"
    );
    assert!(!rev_meta_str.contains(quoted_secret));
    assert!(!rev_meta_str.contains("alice-raw-principal"));

    let (hyd_raw, _) = client.get("/api/hub/session/prod-mixed/sess-keep").unwrap();
    assert_eq!(hyd_raw.status, 200);
    let hyd_json: Value = hyd_raw.json().unwrap();
    assert_eq!(hyd_json["messages"].as_array().unwrap().len(), 2);

    // Check enrolled producer metadata & raw hub sqlite bytes: zero leaks anywhere!
    let (status_raw, _) = client.get("/api/hub/status").unwrap();
    let status: HubStatusResponse = status_raw.json().unwrap();
    let status_text = serde_json::to_string(&status).unwrap();
    let db_bytes = fs::read(hub_home.join("state.db")).unwrap();
    let db_lossy = String::from_utf8_lossy(&db_bytes);
    for forbidden in [
        quoted_secret,
        enroll_secret,
        "macbook-secret.local",
        "/Users/ember",
        "alice-raw-principal",
        "/work/secret-ops",
        "top-secret-denied-hook-marker",
    ] {
        assert!(
            !status_text.contains(forbidden),
            "hub status leaked {forbidden:?}: {status_text}"
        );
        assert!(!db_lossy.contains(forbidden), "hub state.db leaked {forbidden:?}");
    }
}

/// 8. The hub verifies what it counts: an event whose declared digest does not commit to its own
///    `(op_id, envelope, payload_addr)` is refused (`400`) before anything is stored, and a frozen
///    hub refuses a pack upload before creating `hub/packs/` or staging a temp file.
#[test]
fn fleet_hub_rejects_unverified_event_digests_and_frozen_pack_uploads() {
    let hub_w = World::new("hub-verify");
    let hub = HubStore::at_home(&hub_w.cv_home);

    let s = make_session("verify-1", "/work/v", "V1", "u", "a", 1);
    let honest = make_event("prod-v", "ep-1", 1, &s, None);

    // Same event, envelope altered after the digest was computed (or a digest simply made up).
    let mut tampered = honest.clone();
    tampered.envelope["title"] = json!("tampered in transit");
    let err = hub
        .ingest_batch(&IngestBatchRequest { events: vec![tampered] })
        .unwrap_err();
    assert!(
        matches!(err, cv_core::state::hub::HubError::InvalidArgument(ref m) if m.contains("declares digest")),
        "{err:?}"
    );
    let forged = IngestEventRecord::with_digest(
        EventId::new("prod-v", "ep-1", 1),
        honest.op_id.clone(),
        "0".repeat(64),
        honest.envelope.clone(),
        None,
    );
    assert!(hub.ingest_batch(&IngestBatchRequest { events: vec![forged] }).is_err());
    let status = hub.hub_status().unwrap();
    assert!(
        status.frontiers.is_empty() && status.quarantine.count == 0,
        "a refused event must leave no frontier, quarantine, or stored row: {status:?}"
    );

    // The honest record still lands, and its stored digest is the one the hub computed.
    let ok = hub
        .ingest_batch(&IngestBatchRequest {
            events: vec![honest.clone()],
        })
        .unwrap();
    assert_eq!(ok.accepted_count, 1);
    assert_eq!(
        ok.origin_frontier.get(&("prod-v".into(), "ep-1".into())).copied(),
        Some(1)
    );

    // A frozen hub refuses a pack upload without creating the mirror directory or a staged file.
    let frozen_w = World::new("hub-frozen-upload");
    let frozen_hub = HubStore::at_home(&frozen_w.cv_home);
    let packs_dir = frozen_w.cv_home.join("hub/packs");
    assert!(!packs_dir.exists());
    {
        let _frozen = Env::from_vars([("CV_FROZEN", "1")]).enter();
        let err = frozen_hub
            .upload_pack_bytes("pack-frozen", Some("prod-v"), None, b"{}", b"not a pack")
            .unwrap_err();
        assert!(
            matches!(
                err,
                cv_core::state::hub::HubError::State(cv_core::state::StateError::Frozen)
            ),
            "{err:?}"
        );
    }
    assert!(
        !packs_dir.exists() && !frozen_w.cv_home.join("hub").exists(),
        "--frozen pack upload must not create hub/packs"
    );
}
