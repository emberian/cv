//! End-to-end loopback verification for Antigravity live `CreateConversation` seed delivery,
//! dropped-reply single-send reconciliation, cold `IR -> Antigravity` seed emission, and
//! credential-free real-`agy` loopback gateway resume.

use cv_testkit::{claude, World};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

fn write_antigravity_compacted_fixture(app_root: &Path, id: &str, child_id: &str) -> PathBuf {
    let log = app_root
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript_full.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let sub_dir = app_root.join("brain").join(id).join(".system_generated/subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(
        sub_dir.join(format!("{child_id}.json")),
        json!({
            "conversationId": child_id,
            "spawnStepIndex": 1,
            "state": "SUBAGENT_STATE_IDLE"
        })
        .to_string(),
    )
    .unwrap();

    let child_log = app_root
        .join("brain")
        .join(child_id)
        .join(".system_generated/logs/transcript_full.jsonl");
    fs::create_dir_all(child_log.parent().unwrap()).unwrap();
    fs::write(
        &child_log,
        format!(
            "{}\n",
            json!({
                "type": "PLANNER_RESPONSE",
                "step_index": 0,
                "status": "DONE",
                "content": "LATE_CHILD_EVIDENCE: verified zero data loss",
                "created_at": "2026-11-24T01:02:03Z"
            })
        ),
    )
    .unwrap();

    let rows = [
        json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "status": "DONE",
            // JSONL steps carry no model; the reader takes it from the settings block.
            "content": "<USER_SETTINGS_CHANGE>Model: MODEL_PLACEHOLDER_M37</USER_SETTINGS_CHANGE>\nInitial pre-compaction architecture request.",
            "created_at": "2026-11-24T01:00:00Z"
        }),
        json!({
            "step_index": 1,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "model": "MODEL_PLACEHOLDER_M37",
            "content": "Launched background verification worker.",
            "tool_calls": [{"name": "invoke_subagent", "args": {"TypeName": "verify", "Role": "Verifier", "Prompt": "verify"}}],
            "created_at": "2026-11-24T01:00:05Z"
        }),
        json!({
            "step_index": 2,
            "type": "INVOKE_SUBAGENT",
            "status": "DONE",
            "content": json!({"conversationId": child_id}).to_string(),
            "created_at": "2026-11-24T01:00:10Z"
        }),
        json!({
            "step_index": 3,
            "type": "CHECKPOINT",
            "status": "DONE",
            "content": "Compaction summary replacing prefix.",
            "created_at": "2026-11-24T01:01:00Z"
        }),
        json!({
            "step_index": 4,
            "type": "USER_INPUT",
            "status": "DONE",
            "content": "POST_COMPACT_PROMPT_ONE: keep ```fence``` intact.",
            "created_at": "2026-11-24T01:03:00Z"
        }),
        json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "model": "MODEL_PLACEHOLDER_M37",
            "content": "Acknowledged post-compaction turn.",
            "created_at": "2026-11-24T01:03:05Z"
        }),
        json!({
            "step_index": 6,
            "type": "USER_INPUT",
            "status": "DONE",
            "content": "POST_COMPACT_PROMPT_TWO: also preserve final instruction.",
            "created_at": "2026-11-24T01:04:00Z"
        }),
    ];
    fs::write(&log, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
    log
}

#[test]
fn distill_uncompact_renders_post_compaction_prompts_after_recovered_children_and_enforces_max_bytes() {
    // #tooth: `distill --uncompact` must retain pre-compaction context + recovered late child
    // output AND render post-compaction human turns chronologically in the Markdown body (counting
    // toward `--max-bytes`), rather than mentioning them only on stderr.
    let w = World::new("distill-uncompact-excluded-prompts");
    let app_root = w.base.join("agy_root");
    let parent_id = "d10db8f7-0000-4000-8000-000000000001";
    let child_id = "d10db8f7-0000-4000-8000-000000000002";
    let log = write_antigravity_compacted_fixture(&app_root, parent_id, child_id);

    let env = [("ANTIGRAVITY_APP_DATA_DIR", app_root.to_str().unwrap())];
    let (ok, _, pack, err) = w.cv_full(&["distill", log.to_str().unwrap(), "--uncompact"], &env, None);
    assert!(ok, "stderr: {err}");

    let child_pos = pack
        .find("LATE_CHILD_EVIDENCE: verified zero data loss")
        .expect("recovered child text must appear in pack");
    let heading_pos = pack
        .find("## Post-Compaction User Turns (Excluded from Pre-Compaction Prefix)")
        .expect("excluded prompts heading must appear in pack");
    let p1_pos = pack
        .find("POST_COMPACT_PROMPT_ONE: keep ```fence``` intact.")
        .expect("first post-compaction human prompt must appear in pack");
    let p2_pos = pack
        .find("POST_COMPACT_PROMPT_TWO: also preserve final instruction.")
        .expect("second post-compaction human prompt must appear in pack");

    assert!(
        child_pos < heading_pos && heading_pos < p1_pos && p1_pos < p2_pos,
        "expected chronological order (recovered children -> heading -> prompt 1 -> prompt 2):\n{pack}"
    );
    assert!(
        pack.contains("````text\nPOST_COMPACT_PROMPT_ONE: keep ```fence``` intact.\n````"),
        "expected collision-proof 4-backtick fence around 3-backtick prompt:\n{pack}"
    );

    // Must count toward `--max-bytes`: setting cap just below `pack.len()` refuses emission.
    let tight = (pack.len() - 10).to_string();
    let (ok_tight, _, out_tight, err_tight) = w.cv_full(
        &["distill", log.to_str().unwrap(), "--uncompact", "--max-bytes", &tight],
        &env,
        None,
    );
    assert!(!ok_tight && out_tight.is_empty(), "expected refusal: {err_tight}");
    assert!(err_tight.contains("exceeding --max-bytes"), "{err_tight}");
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

enum MockBehavior {
    Created(String),
    DropAndWriteLocal(PathBuf, String),
    DropWithoutWrite,
    Refuse(u16, String),
}

fn try_read_http_request(stream: &mut TcpStream) -> std::io::Result<Option<CapturedRequest>> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut req_line = String::new();
    loop {
        req_line.clear();
        if reader.read_line(&mut req_line)? == 0 {
            return Ok(None);
        }
        if !req_line.trim().is_empty() {
            break;
        }
    }
    let path = req_line.split_whitespace().nth(1).unwrap_or("").to_owned();
    let mut headers = HashMap::new();
    let mut content_len = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_owned();
            if key == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }
    if headers
        .get("expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        writer.flush()?;
    }
    let mut buf = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| std::io::Error::other("bad chunk size"))?;
            if size == 0 {
                loop {
                    line.clear();
                    if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            let start = buf.len();
            buf.resize(start + size, 0);
            reader.read_exact(&mut buf[start..])?;
            line.clear();
            reader.read_line(&mut line)?;
        }
    } else if content_len > 0 {
        buf.resize(content_len, 0);
        reader.read_exact(&mut buf)?;
    }
    let body: Value = if buf.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&buf).unwrap_or(Value::Null)
    };
    Ok(Some(CapturedRequest { path, headers, body }))
}

fn read_http_request(stream: &mut TcpStream) -> CapturedRequest {
    try_read_http_request(stream)
        .expect("read http request")
        .expect("non-empty http request")
}

fn write_local_agy_seed_transcript(app_root: &Path, conv_id: &str, prompt_text: &str) {
    let log = app_root
        .join("brain")
        .join(conv_id)
        .join(".system_generated/logs/transcript_full.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(
        log,
        format!(
            "{}\n",
            json!({
                "step_index": 0,
                "type": "USER_INPUT",
                "status": "DONE",
                "content": prompt_text,
                "created_at": chrono::Utc::now().to_rfc3339(),
            })
        ),
    )
    .unwrap();
}

#[test]
fn live_create_conversation_delivery_and_dropped_reply_reconciliation() {
    // #tooth: live CreateConversation reserves `deliveries` before writing to the socket; a dropped
    // reply reconciles against local Antigravity storage with zero duplicate POSTs, and an
    // unpersisted drop stays `Uncertain` with zero duplicate POSTs unless `--force` is passed.
    let w = World::new("l2-live-create-conversation");
    let app_root = w.base.join("agy_app_data");
    fs::create_dir_all(&app_root).unwrap();

    let s1 = write_antigravity_compacted_fixture(
        &app_root,
        "11111111-0000-4000-8000-000000000001",
        "11111111-0000-4000-8000-000000000002",
    );
    let s2 = write_antigravity_compacted_fixture(
        &app_root,
        "22222222-0000-4000-8000-000000000001",
        "22222222-0000-4000-8000-000000000002",
    );
    let s3 = write_antigravity_compacted_fixture(
        &app_root,
        "33333333-0000-4000-8000-000000000001",
        "33333333-0000-4000-8000-000000000002",
    );
    let s4 = write_antigravity_compacted_fixture(
        &app_root,
        "44444444-0000-4000-8000-000000000001",
        "44444444-0000-4000-8000-000000000002",
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let post_count = Arc::new(AtomicUsize::new(0));
    let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let scripted: Arc<Mutex<Vec<MockBehavior>>> = Arc::new(Mutex::new(vec![
        MockBehavior::Created("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0001".into()),
        // Case 2: drop connection AND materialize local transcript -> reconciles to Created!
        MockBehavior::DropAndWriteLocal(app_root.clone(), "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0002".into()),
        // Case 3: drop connection WITHOUT local transcript -> Uncertain; `--force` retries and succeeds!
        MockBehavior::DropWithoutWrite,
        MockBehavior::Created("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0003".into()),
        // Case 4: HTTP 403 refusal -> Refused!
        MockBehavior::Refuse(403, "invalid csrf token".into()),
    ]));

    let post_count_srv = Arc::clone(&post_count);
    let captured_srv = Arc::clone(&captured);
    let scripted_srv = Arc::clone(&scripted);
    let server = thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let next = {
                let mut q = scripted_srv.lock().unwrap();
                if q.is_empty() {
                    break;
                }
                q.remove(0)
            };
            post_count_srv.fetch_add(1, Ordering::SeqCst);
            let req = read_http_request(&mut stream);
            let prompt_str = req.body["prompt"][0]["text"].as_str().unwrap_or_default().to_owned();
            captured_srv.lock().unwrap().push(req);
            match next {
                MockBehavior::Created(cid) => {
                    let body = json!({"conversation": {"id": cid}}).to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
                MockBehavior::DropAndWriteLocal(root, cid) => {
                    write_local_agy_seed_transcript(&root, &cid, &prompt_str);
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                MockBehavior::DropWithoutWrite => {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                MockBehavior::Refuse(code, msg) => {
                    let body = json!({"code": "permission_denied", "message": msg}).to_string();
                    let resp = format!(
                        "HTTP/1.1 {code} Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            }
            if scripted_srv.lock().unwrap().is_empty() {
                break;
            }
        }
    });

    let ls_url = format!("http://{addr}");
    let env = [
        ("ANTIGRAVITY_APP_DATA_DIR", app_root.to_str().unwrap()),
        ("JETSKI_APP_DATA_DIR", app_root.to_str().unwrap()),
        ("ANTIGRAVITY_LS_ADDRESS", ls_url.as_str()),
        ("ANTIGRAVITY_CSRF_TOKEN", "test-csrf-secret"),
    ];

    // Non-loopback target is refused before touching the network.
    let bad_env = [
        ("ANTIGRAVITY_APP_DATA_DIR", app_root.to_str().unwrap()),
        ("ANTIGRAVITY_LS_ADDRESS", "http://8.8.8.8:8080"),
        ("ANTIGRAVITY_CSRF_TOKEN", "test-csrf-secret"),
    ];
    let (ok_bad, _, _, err_bad) = w.cv_full(
        &[
            "distill",
            s1.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
        ],
        &bad_env,
        None,
    );
    assert!(!ok_bad && err_bad.contains("non-loopback"), "{err_bad}");
    assert_eq!(post_count.load(Ordering::SeqCst), 0);

    // 1. Happy-path CreateConversation delivery.
    let (ok1, _, out1, err1) = w.cv_full(
        &[
            "distill",
            s1.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--json",
        ],
        &env,
        None,
    );
    assert!(ok1, "stderr: {err1}");
    assert_eq!(post_count.load(Ordering::SeqCst), 1);
    let j1: Value = serde_json::from_str(&out1).unwrap();
    assert_eq!(j1["emitted"]["session_id"], "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0001");
    assert_eq!(j1["emitted"]["delivery"]["state"], "created");
    assert_eq!(j1["emitted"]["delivery"]["reconciled"], false);
    let tag1 = j1["emitted"]["delivery"]["tag"].as_str().unwrap();
    assert!(tag1.starts_with("cvseed-") && tag1.len() == 23, "{tag1}");

    {
        let reqs = captured.lock().unwrap();
        let r0 = &reqs[0];
        assert_eq!(r0.path, "/product.v1.ConversationService/CreateConversation");
        assert_eq!(
            r0.headers.get("connect-protocol-version").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            r0.headers.get("x-antigravity-csrf-token").map(String::as_str),
            Some("test-csrf-secret")
        );
        assert_eq!(r0.body["model"], "MODEL_PLACEHOLDER_M37");
        let sent_text = r0.body["prompt"][0]["text"].as_str().unwrap();
        assert!(sent_text.ends_with(tag1), "{sent_text}");
        assert!(sent_text.contains("LATE_CHILD_EVIDENCE"));
        assert!(sent_text.contains("POST_COMPACT_PROMPT_ONE"));
    }

    // Re-running s1 without `--force` returns the journaled conversation with ZERO extra POSTs!
    let (ok1_repeat, _, out1_repeat, _) = w.cv_full(
        &[
            "distill",
            s1.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--json",
        ],
        &env,
        None,
    );
    assert!(ok1_repeat);
    assert_eq!(
        post_count.load(Ordering::SeqCst),
        1,
        "must not re-send already created seed"
    );
    let j1_rep: Value = serde_json::from_str(&out1_repeat).unwrap();
    assert_eq!(j1_rep["emitted"]["session_id"], "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0001");
    assert_eq!(j1_rep["emitted"]["delivery"]["reconciled"], true);

    // 2. Dropped HTTP reply AFTER local conversation materialized -> reconciles to Created, and a
    // second invocation sends ZERO network POSTs!
    let (ok2, _, out2, err2) = w.cv_full(
        &[
            "distill",
            s2.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--json",
        ],
        &env,
        None,
    );
    assert!(ok2, "expected immediate local reconciliation on dropped reply: {err2}");
    assert_eq!(post_count.load(Ordering::SeqCst), 2);
    let j2: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(j2["emitted"]["session_id"], "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0002");
    assert_eq!(j2["emitted"]["delivery"]["reconciled"], true);

    let (ok2_repeat, _, _, _) = w.cv_full(
        &[
            "distill",
            s2.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--json",
        ],
        &env,
        None,
    );
    assert!(ok2_repeat);
    assert_eq!(
        post_count.load(Ordering::SeqCst),
        2,
        "dropped-reply seed must never double-send"
    );

    // 3. Dropped HTTP reply WITHOUT local conversation -> Uncertain; retry without `--force` sends
    // ZERO POSTs; retry WITH `--force` sends exactly 1 POST and succeeds!
    let (ok3_first, _, _, err3_first) = w.cv_full(
        &[
            "distill",
            s3.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--json",
        ],
        &env,
        None,
    );
    assert!(!ok3_first && err3_first.contains("uncertain"), "{err3_first}");
    assert_eq!(post_count.load(Ordering::SeqCst), 3);

    let (ok3_second, _, _, err3_second) = w.cv_full(
        &[
            "distill",
            s3.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--json",
        ],
        &env,
        None,
    );
    assert!(
        !ok3_second && err3_second.contains("prior delivery attempt did not settle"),
        "{err3_second}"
    );
    assert_eq!(
        post_count.load(Ordering::SeqCst),
        3,
        "uncertain seed must not re-send without --force"
    );

    let (ok3_force, _, out3_force, err3_force) = w.cv_full(
        &[
            "distill",
            s3.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
            "--force",
            "--json",
        ],
        &env,
        None,
    );
    assert!(ok3_force, "stderr: {err3_force}");
    assert_eq!(post_count.load(Ordering::SeqCst), 4);
    let j3: Value = serde_json::from_str(&out3_force).unwrap();
    assert_eq!(j3["emitted"]["session_id"], "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeee0003");

    // 4. HTTP 403 Refused -> recorded as refused; second call without `--force` sends ZERO POSTs.
    let (ok4, _, _, err4) = w.cv_full(
        &[
            "distill",
            s4.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
        ],
        &env,
        None,
    );
    assert!(!ok4 && err4.contains("HTTP 403"), "{err4}");
    assert_eq!(post_count.load(Ordering::SeqCst), 5);

    let (ok4_repeat, _, _, err4_repeat) = w.cv_full(
        &[
            "distill",
            s4.to_str().unwrap(),
            "--uncompact",
            "--session=antigravity",
            "--delivery=live",
        ],
        &env,
        None,
    );
    assert!(
        !ok4_repeat && err4_repeat.contains("previously refused"),
        "{err4_repeat}"
    );
    assert_eq!(post_count.load(Ordering::SeqCst), 5);

    server.join().unwrap();
}

#[test]
fn cold_antigravity_emit_strict_refusal_and_readback() {
    // #tooth: `port --harness antigravity --strict` refuses sessions whose tool calls or multi-turn
    // replies would be flattened instead of fabricating protobuf tool steps; non-strict emits a
    // verified 1-step `USER_INPUT` trajectory that round-trips cleanly through `cv show` and `cv ls`.
    let w = World::new("cold-antigravity-emit");
    w.write_session(
        "claudetools",
        &[
            claude::user(
                "claudetools",
                "u0",
                "2026-11-24T10:00:00Z",
                "Inspect src/lib.rs and run tests.",
            ),
            claude::assistant(
                "a1",
                "2026-11-24T10:00:05Z",
                "claude-sonnet-4-6",
                json!([claude::tool_use("tu1", "Bash", json!({"command": "cargo test --lib"}))]),
            ),
            claude::tool_result("r1", "2026-11-24T10:00:10Z", "tu1", "test result: ok. 12 passed"),
            claude::assistant(
                "a2",
                "2026-11-24T10:00:15Z",
                "claude-sonnet-4-6",
                json!([{"type": "text", "text": "All 12 unit tests passed."}]),
            ),
        ],
    );

    let strict_out = w.base.join("strict_agy_out");
    let (ok_strict, _, _, err_strict) = w.cv(&[
        "port",
        "claudetools",
        "--harness",
        "antigravity",
        "--out",
        strict_out.to_str().unwrap(),
        "--strict",
    ]);
    assert!(!ok_strict, "multi-turn tool session must fail --strict");
    assert!(err_strict.contains("--strict"), "{err_strict}");
    assert!(
        !strict_out.exists(),
        "retracted strict failure must leave no target dir"
    );

    let cold_out = w.base.join("cold_agy_out");
    let (ok_port, _, out_port, err_port) = w.cv(&[
        "port",
        "claudetools",
        "--harness",
        "antigravity",
        "--out",
        cold_out.to_str().unwrap(),
        "--json",
    ]);
    assert!(ok_port, "stderr: {err_port}");
    let ported: Value = serde_json::from_str(&out_port).unwrap();
    let new_id = ported["id"].as_str().unwrap();
    assert!(cold_out.join("conversations").join(format!("{new_id}.db")).is_file());
    assert!(cold_out.join("conversation_summaries.db").is_file());

    let agy_env = [
        ("ANTIGRAVITY_APP_DATA_DIR", cold_out.to_str().unwrap()),
        ("JETSKI_APP_DATA_DIR", cold_out.to_str().unwrap()),
    ];
    let (ok_ls, _, out_ls, err_ls) = w.cv_full(&["ls", "--harness", "antigravity", "--json"], &agy_env, None);
    assert!(ok_ls, "stderr: {err_ls}");
    let listed: Value = serde_json::from_str(&out_ls).unwrap();
    assert!(
        listed.as_array().unwrap().iter().any(|row| row["id"] == new_id),
        "expected {new_id} in `cv ls`: {listed}"
    );

    let (ok_show, _, out_show, err_show) = w.cv_full(&["show", new_id, "--json"], &agy_env, None);
    assert!(ok_show, "stderr: {err_show}");
    let shown: Value = serde_json::from_str(&out_show).unwrap();
    assert_eq!(shown["messages"].as_array().unwrap().len(), 1);
    let seed_body = shown["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(seed_body.contains("Inspect src/lib.rs and run tests."), "{seed_body}");
    assert!(seed_body.contains("All 12 unit tests passed."), "{seed_body}");

    // Single plain user prompt ports cleanly even under `--strict`.
    w.write_session(
        "claudeplain",
        &[claude::user(
            "claudeplain",
            "pu0",
            "2026-11-24T11:00:00Z",
            "Single plain prompt for lossless one-step seed.",
        )],
    );
    let plain_out = w.base.join("plain_agy_out");
    let (ok_plain, _, _, err_plain) = w.cv(&[
        "port",
        "claudeplain",
        "--harness",
        "antigravity",
        "--out",
        plain_out.to_str().unwrap(),
        "--strict",
    ]);
    assert!(ok_plain, "1-turn user prompt must pass --strict: {err_plain}");
}

#[cfg(target_os = "macos")]
fn find_agy_executable() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("CV_AGY_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("agy");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/agy");
        if local.is_file() {
            return Some(local);
        }
    }
    None
}

#[cfg(target_os = "macos")]
#[test]
fn real_agy_loopback_gateway_resumes_cold_rewind_and_emitted_seed() {
    // #tooth: when `agy` is installed on macOS, drive the real binary inside `sandbox-exec` against
    // a credential-free `127.0.0.1:0` `AGY_LLM_GATEWAY_URL` mock server to verify that both
    // `cv rewind --uncompact` (keeping `executor_metadata` row 0) and `cv port --harness antigravity`
    // produce trajectories that `agy --conversation <id>` genuinely loads and continues!
    let l2_required = std::env::var("CV_L2").ok().is_some_and(|v| {
        v.split(',')
            .map(str::trim)
            .any(|s| matches!(s, "1" | "all" | "antigravity"))
    });
    if !l2_required {
        eprintln!("real_agy_loopback_gateway: skipped: set CV_L2=1 (or antigravity) to run real binaries");
        return;
    }
    let agy_bin = find_agy_executable().expect("CV_L2 requested antigravity, but `agy` was not found");
    assert!(
        Path::new("/usr/bin/sandbox-exec").is_file(),
        "CV_L2 requested antigravity, but `/usr/bin/sandbox-exec` is absent"
    );

    let w = World::new("real-agy-l2-gateway");
    let raw_sb = w.base.join("agy_sandbox");
    fs::create_dir_all(&raw_sb).unwrap();
    let sb_base = raw_sb.canonicalize().unwrap();
    for sub in ["home", "config", "data", "state", "cache", "tmp", "project"] {
        fs::create_dir_all(sb_base.join(sub)).unwrap();
    }
    let profile = sb_base.join("sandbox.sb");
    fs::write(
        &profile,
        format!(
            "(version 1)(allow default)(deny file-write*)\
             (allow file-write* (subpath {})(literal \"/dev/null\")\
             (literal \"/dev/ptmx\")(regex #\"^/dev/ttys[0-9]+$\"))\
             (deny network*)(allow network* (remote ip \"localhost:*\")(local ip \"localhost:*\"))\
             (allow network* (remote unix-socket))",
            serde_json::to_string(sb_base.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();

    let gateway = TcpListener::bind("127.0.0.1:0").unwrap();
    let gw_port = gateway.local_addr().unwrap().port();
    let seen_requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_srv = Arc::clone(&seen_requests);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_srv = Arc::clone(&stop);

    gateway.set_nonblocking(true).unwrap();
    let gw_thread = thread::spawn(move || {
        let mut workers = Vec::new();
        while !stop_srv.load(Ordering::Relaxed) {
            match gateway.accept() {
                Ok((mut stream, _)) => {
                    let seen_conn = Arc::clone(&seen_srv);
                    workers.push(thread::spawn(move || {
                        let _ = stream.set_nonblocking(false);
                        let Ok(Some(req)) = try_read_http_request(&mut stream) else {
                            return;
                        };
                        if req.body.is_null() {
                            let list = json!({"models": []}).to_string();
                            let http = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                list.len(),
                                list
                            );
                            let _ = stream.write_all(http.as_bytes());
                            let _ = stream.flush();
                            return;
                        }
                        let sys = serde_json::to_string(&req.body.get("systemInstruction")).unwrap_or_default();
                        let reply_text = if sys.contains("title generator") {
                            "Gateway fixture".to_owned()
                        } else {
                            seen_conn.lock().unwrap().push(req.body.clone());
                            let body_dump = serde_json::to_string(&req.body["contents"]).unwrap_or_default();
                            if body_dump.contains("/compact") {
                                "GATEWAY_COMPACT_SUMMARY: pre-compaction context.".to_owned()
                            } else {
                                "GATEWAY_OK_REPLY".to_owned()
                            }
                        };
                        let candidate = json!({
                            "candidates": [{
                                "content": {"role": "model", "parts": [{"text": reply_text}]},
                                "finishReason": "STOP",
                                "index": 0
                            }],
                            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
                            "modelVersion": "loopback"
                        });
                        let (ctype, payload) = if req.path.contains(":generateContent")
                            && !req.path.contains(":streamGenerateContent")
                        {
                            ("application/json", candidate.to_string())
                        } else {
                            ("text/event-stream", format!("data: {candidate}\n\n"))
                        };
                        let http = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                        let _ = stream.write_all(http.as_bytes());
                        let _ = stream.flush();
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(15));
                }
                Err(_) => break,
            }
        }
        for w in workers {
            let _ = w.join();
        }
    });

    let run_agy = |app_dir: &Path, args: &[&str]| -> Result<Value, String> {
        let out = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-f")
            .arg(&profile)
            .arg(&agy_bin)
            .arg("--dangerously-skip-permissions")
            .args(args)
            .arg("--print-timeout")
            .arg("20s")
            .arg("--output-format")
            .arg("stream-json")
            .current_dir(sb_base.join("project"))
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            )
            .env("HOME", sb_base.join("home"))
            .env("TMPDIR", sb_base.join("tmp"))
            .env("TERM", "dumb")
            .env("XDG_CONFIG_HOME", sb_base.join("config"))
            .env("XDG_DATA_HOME", sb_base.join("data"))
            .env("XDG_STATE_HOME", sb_base.join("state"))
            .env("XDG_CACHE_HOME", sb_base.join("cache"))
            .env("ANTIGRAVITY_APP_DATA_DIR", app_dir)
            .env("ANTIGRAVITY_GEMINI_DIR", app_dir.parent().unwrap_or(sb_base.as_path()))
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "1")
            .env("AGY_CLI_MODEL_API_MAX_RETRIES", "0")
            .env("AGY_LLM_GATEWAY_URL", format!("http://127.0.0.1:{gw_port}"))
            .env("AGY_LLM_GATEWAY_API_KEY", "loopback-dummy")
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "agy exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v.get("event").and_then(Value::as_str) == Some("result") {
                    return Ok(v["result"].clone());
                }
            }
        }
        Err(format!("missing result event in stdout: {stdout}"))
    };

    let live_root = sb_base.join("home/.gemini/antigravity-cli");
    // 1. Create a real session via `agy`, compact it, and add a post-compaction turn.
    let first = match run_agy(&live_root, &["--print", "PRE_COMPACT_REAL_AGY_MARKER"]) {
        Ok(res) => res,
        Err(err) => {
            stop.store(true, Ordering::Relaxed);
            let _ = gw_thread.join();
            panic!("real agy failed under CV_L2: {err}");
        }
    };
    let orig_id = first["conversation_id"].as_str().unwrap().to_owned();
    run_agy(&live_root, &["--conversation", &orig_id, "--print", "/compact"]).unwrap();
    run_agy(
        &live_root,
        &["--conversation", &orig_id, "--print", "POST_COMPACT_TURN"],
    )
    .unwrap();

    // Cold-rewind `--uncompact` into a clean app-data root inside `sb_base` and verify `executor_metadata` row 0!
    let rewound_root = sb_base.join("rewound_gemini/antigravity-cli");
    let orig_db = live_root.join("conversations").join(format!("{orig_id}.db"));
    let (ok_rw, _, out_rw, err_rw) = w.cv(&[
        "rewind",
        orig_db.to_str().unwrap(),
        "--uncompact",
        "--out",
        rewound_root.to_str().unwrap(),
        "--json",
    ]);
    assert!(ok_rw, "rewind failed: {err_rw}");
    let rw_json: Value = serde_json::from_str(&out_rw).unwrap();
    let rewound_id = rw_json["new_id"].as_str().unwrap();
    assert_eq!(rw_json["executor_rows_kept"], 1, "{rw_json}");

    seen_requests.lock().unwrap().clear();
    let resumed_rw = run_agy(
        &rewound_root,
        &["--conversation", rewound_id, "--print", "VERIFY_AFTER_COLD_REWIND"],
    )
    .expect("real agy must resume cold-rewound successor");
    assert_eq!(resumed_rw["status"], "SUCCESS");
    assert_eq!(resumed_rw["conversation_id"], rewound_id);
    {
        let reqs = seen_requests.lock().unwrap();
        let dump = serde_json::to_string(&*reqs).unwrap();
        assert!(
            dump.contains("PRE_COMPACT_REAL_AGY_MARKER"),
            "resumed request must carry pre-compaction user marker: {dump}"
        );
        assert!(
            !dump.contains("GATEWAY_COMPACT_SUMMARY"),
            "uncompacted rewind must omit compaction checkpoint summary: {dump}"
        );
    }

    // 2. Port a Claude session via `cv port --harness antigravity --out <ported_root>` and resume it in real `agy`!
    w.write_session(
        "claude4agy",
        &[
            claude::user("claude4agy", "u0", "2026-11-24T12:00:00Z", "PORTED_CLAUDE_SEED_MARKER"),
            claude::assistant(
                "a1",
                "2026-11-24T12:00:05Z",
                "claude-sonnet-4-6",
                json!([{"type": "text", "text": "PORTED_ASSISTANT_CONTEXT"}]),
            ),
        ],
    );
    let ported_root = sb_base.join("ported_gemini/antigravity-cli");
    let (ok_pt, _, out_pt, err_pt) = w.cv(&[
        "port",
        "claude4agy",
        "--harness",
        "antigravity",
        "--out",
        ported_root.to_str().unwrap(),
        "--json",
    ]);
    assert!(ok_pt, "port failed: {err_pt}");
    let pt_json: Value = serde_json::from_str(&out_pt).unwrap();
    let ported_id = pt_json["id"].as_str().unwrap();

    seen_requests.lock().unwrap().clear();
    let resumed_pt = run_agy(
        &ported_root,
        &["--conversation", ported_id, "--print", "CONTINUE_PORTED_SESSION"],
    )
    .expect("real agy must resume cold-emitted 1-step seed trajectory");
    assert_eq!(resumed_pt["status"], "SUCCESS");
    assert_eq!(resumed_pt["conversation_id"], ported_id);
    {
        let reqs = seen_requests.lock().unwrap();
        let dump = serde_json::to_string(&*reqs).unwrap();
        assert!(
            dump.contains("PORTED_CLAUDE_SEED_MARKER") && dump.contains("PORTED_ASSISTANT_CONTEXT"),
            "real agy must send ported seed content to gateway: {dump}"
        );
    }

    stop.store(true, Ordering::Relaxed);
    let _ = gw_thread.join();
}
