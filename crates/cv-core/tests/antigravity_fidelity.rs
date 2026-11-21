//! Independent synthetic coverage contracts across native reads and public consumer surfaces.
use cv_core::harness::antigravity::Antigravity;
use cv_core::{Harness, SessionRef};
use cv_testkit::World;
use std::fs;

const ID: &str = "bbbbbbbb-2222-4333-8444-555555555555";

#[test]
fn empty_exports_and_empty_tool_lists_do_not_invent_inferred_tool_associations() {
    use cv_core::coverage::ReadCoverage;
    for export in [
        "",
        r#"{"step_index":0,"type":"PLANNER_RESPONSE","status":"DONE","content":"hello","tool_calls":[]}"#,
    ] {
        let session = cv_core::harness::antigravity::parse_str(ID, export, None).unwrap();
        let coverage = ReadCoverage::of_session(&session).expect("export source is known even when empty");
        assert!(coverage.sources.contains(ReadCoverage::EXPORTED_HISTORY));
        assert!(!coverage.limitations.contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
    }
}

#[cfg(not(feature = "sqlite"))]
#[test]
fn portable_native_database_read_is_an_explicit_unsupported_error() {
    use cv_core::harness::Parse;
    use cv_core::Adapter;
    let world = World::in_process("agy-portable-coverage", cv_core::env::Env::enter_isolated);
    let root = world.home.join(".gemini/antigravity-cli");
    let dir = root.join("conversations");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{ID}.db"));
    fs::write(&path, b"SQLite format 3\0synthetic database bytes").unwrap();
    let adapter = Antigravity::with_home(&root);
    let discovered = adapter.discover().unwrap();
    assert!(discovered
        .skipped
        .iter()
        .any(|skipped| skipped.path == path && skipped.why.to_lowercase().contains("sqlite")));
    let error = adapter
        .parse(&SessionRef::new(ID, Harness::Antigravity, path))
        .expect_err("portable build must not interpret native DB bytes as empty JSONL");
    let error = format!("{error:#}").to_lowercase();
    assert!(error.contains("sqlite"), "{error}");
}

#[cfg(feature = "sqlite")]
mod native {
    use super::*;
    use cv_core::coverage::ReadCoverage;
    use cv_core::{Adapter, Demand, MessageSink};
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    fn varint(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (n & 127) as u8;
            n >>= 7;
            out.push(byte | if n == 0 { 0 } else { 128 });
            if n == 0 {
                return out;
            }
        }
    }
    fn number(field: u64, value: u64) -> Vec<u8> {
        [varint(field << 3), varint(value)].concat()
    }
    fn bytes(field: u64, value: impl AsRef<[u8]>) -> Vec<u8> {
        let value = value.as_ref();
        [varint((field << 3) | 2), varint(value.len() as u64), value.to_vec()].concat()
    }
    fn database(root: &Path, rows: &[Vec<u8>]) -> PathBuf {
        let directory = root.join("conversations");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{ID}.db"));
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER DEFAULT 0);",
        )
        .unwrap();
        for (index, row) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO steps(idx,step_payload) VALUES (?1,?2)",
                rusqlite::params![index as i64, row],
            )
            .unwrap();
        }
        path
    }
    fn api(suffix: &str) -> Value {
        api_query(suffix, "")
    }
    fn api_query(suffix: &str, query: &str) -> Value {
        let (status, reply) = cv_core::api::route_json(&["api", "session", "antigravity", ID, suffix], query);
        assert_eq!(status, 200, "{reply}");
        reply
    }
    fn evidence(value: &Value) -> ReadCoverage {
        serde_json::from_value(value.clone()).expect("assessed coverage metadata")
    }

    #[test]
    fn empty_and_unknown_native_history_have_sources_without_invented_missing_data() {
        for unknown in [false, true] {
            let world = World::in_process("agy-source-coverage", cv_core::env::Env::enter_isolated);
            let root = world.home.join(".gemini/antigravity-cli");
            let rows = if unknown {
                vec![[number(1, 9999), number(4, 3), bytes(9999, "future stored content")].concat()]
            } else {
                vec![]
            };
            let path = database(&root, &rows);
            let adapter = Antigravity::with_home(&root);
            let reference = SessionRef::new(ID, Harness::Antigravity, path);
            for demand in [Demand::LEAN, Demand::FACTS, Demand::COMPLETE] {
                let session = cv_core::stream::collect_with(&adapter, &reference, &demand).unwrap();
                let coverage =
                    ReadCoverage::of_session(&session).expect("native source must be assessed even with no rows");
                assert!(coverage.sources.contains(ReadCoverage::NATIVE_STORE));
                assert!(!coverage.limitations.contains(ReadCoverage::MISSING_PAYLOADS));
                if unknown {
                    assert!(coverage.limitations.contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS));
                }
            }
            if unknown {
                for (suffix, pointer) in [
                    ("head", "/extra/cv/read_coverage"),
                    ("messages", "/session/extra/cv/read_coverage"),
                ] {
                    let reply = api(suffix);
                    assert!(evidence(reply.pointer(pointer).unwrap())
                        .sources
                        .contains(ReadCoverage::NATIVE_STORE));
                }
            } else {
                // An empty 0-row native shell DB is omitted from discovery while remaining
                // directly readable via `collect_with` above.
                let discovered = adapter.discover().unwrap();
                assert!(discovered.refs.is_empty());
                assert!(discovered.skipped.is_empty());
                let (status, _) = cv_core::api::route_json(&["api", "session", "antigravity", ID, "head"], "");
                assert_eq!(status, 404);
            }
        }

        // A `.db` in `conversations/` without the `steps` schema is reported in `Discovered::skipped`.
        let world = World::in_process("agy-missing-steps-schema", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        fs::create_dir_all(root.join("conversations")).unwrap();
        let bad_db = root.join("conversations").join(format!("{ID}.db"));
        rusqlite::Connection::open(&bad_db)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
            .unwrap();
        let discovered = Antigravity::with_home(&root).discover().unwrap();
        assert!(discovered.refs.is_empty());
        assert_eq!(discovered.skipped.len(), 1);
        assert_eq!(discovered.skipped[0].path, bad_db);
    }

    #[test]
    fn matching_native_tail_remains_visible_beside_nonempty_historical_export() {
        let world = World::in_process("agy-native-tail", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let prompt = "retained exported prompt";
        let tail = "new native reply after the exported prefix";
        database(
            &root,
            &[
                [number(1, 14), number(4, 3), bytes(19, bytes(3, bytes(1, prompt)))].concat(),
                [number(1, 15), number(4, 3), bytes(20, bytes(1, tail))].concat(),
            ],
        );
        let logs = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let export = logs.join("transcript_full.jsonl");
        fs::write(
            &export,
            format!(
                "{}\n",
                json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":prompt})
            ),
        )
        .unwrap();
        let adapter = Antigravity::with_home(&root);
        let reference = SessionRef::new(ID, Harness::Antigravity, export);
        for demand in [Demand::LEAN, Demand::FACTS, Demand::COMPLETE] {
            let session = cv_core::stream::collect_with(&adapter, &reference, &demand).unwrap();
            let messages = serde_json::to_value(&session.messages).unwrap();
            let texts = messages
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().unwrap())
                .filter(|block| block["type"] == "text")
                .map(|block| block["text"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(texts, [prompt, tail]);
            let coverage = ReadCoverage::of_session(&session).unwrap();
            assert!(coverage.sources.contains(ReadCoverage::EXPORTED_HISTORY));
            assert!(coverage.sources.contains(ReadCoverage::NATIVE_STORE));
        }
        let reply = api("messages");
        assert_eq!(reply["messages"][0]["content"][0]["text"], prompt);
        assert_eq!(reply["messages"][1]["content"][0]["text"], tail);
        assert_eq!(api("head")["total"], 2);
    }

    #[test]
    fn conflicting_latest_overlap_keeps_export_and_reports_unreconciled_native_tail() {
        let world = World::in_process("agy-conflicting-tail", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let prompt = "shared earlier prompt";
        let exported_answer = "retained original answer";
        let newer_tail = "continuation of a conflicting native revision";
        database(
            &root,
            &[
                [number(1, 14), number(4, 3), bytes(19, bytes(3, bytes(1, prompt)))].concat(),
                [
                    number(1, 15),
                    number(4, 3),
                    bytes(20, bytes(1, "different native answer at reused index")),
                ]
                .concat(),
                [number(1, 15), number(4, 3), bytes(20, bytes(1, newer_tail))].concat(),
            ],
        );
        let logs = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let export = logs.join("transcript_full.jsonl");
        fs::write(
            &export,
            format!(
                "{}\n{}\n",
                json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":prompt}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","content":exported_answer})
            ),
        )
        .unwrap();
        let adapter = Antigravity::with_home(&root);
        let reference = SessionRef::new(ID, Harness::Antigravity, export);
        let session = cv_core::stream::collect_with(&adapter, &reference, &Demand::COMPLETE).unwrap();
        let coverage = ReadCoverage::of_session(&session).unwrap();
        assert!(coverage.limitations.contains(ReadCoverage::UNRECONCILED_HISTORY));
        assert!(!coverage.limitations.contains(ReadCoverage::MISSING_HISTORY));
        assert!(!coverage.limitations.contains(ReadCoverage::MISSING_PAYLOADS));
        let reply = api("messages");
        assert_eq!(reply["messages"].as_array().unwrap().len(), 2);
        assert_eq!(reply["messages"][1]["content"][0]["text"], exported_answer);
        assert!(evidence(&reply["session"]["extra"]["cv"]["read_coverage"])
            .limitations
            .contains(ReadCoverage::UNRECONCILED_HISTORY));
        assert_eq!(api("head")["total"], 2);
    }

    #[test]
    fn newer_native_indices_without_shared_history_are_not_blindly_appended() {
        let world = World::in_process("agy-unanchored-tail", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let path = database(
            &root,
            &[[
                number(1, 15),
                number(4, 3),
                bytes(20, bytes(1, "unanchored native reply")),
            ]
            .concat()],
        );
        let connection = rusqlite::Connection::open(path).unwrap();
        connection.execute("UPDATE steps SET idx=1", []).unwrap();
        drop(connection);
        let logs = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript_full.jsonl"),
            format!(
                "{}\n",
                json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"exported prompt"})
            ),
        )
        .unwrap();
        let reply = api("messages");
        assert_eq!(reply["messages"].as_array().unwrap().len(), 1);
        assert_eq!(reply["messages"][0]["content"][0]["text"], "exported prompt");
        assert!(evidence(&reply["session"]["extra"]["cv"]["read_coverage"])
            .limitations
            .contains(ReadCoverage::UNRECONCILED_HISTORY));
    }

    #[test]
    fn binary_planner_and_call_signatures_survive_rich_api_metadata() {
        let world = World::in_process("agy-binary-signatures", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let call = [
            bytes(1, "native-call"),
            bytes(2, "run_command"),
            bytes(3, "{}"),
            bytes(7, [0xff, 0, 0x80]),
        ]
        .concat();
        let response = [
            bytes(1, "answer"),
            bytes(3, "thinking"),
            bytes(4, "legacy textual signature"),
            bytes(14, [0, 0xff, 0x80, b'\n']),
            bytes(7, call),
        ]
        .concat();
        database(&root, &[[number(1, 15), number(4, 3), bytes(20, response)].concat()]);
        let reply = api_query("messages", "extra=1");
        let details = reply["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|message| message.pointer("/extra/antigravity/planner_details"))
            .expect("rich metadata preserves recorded binary signatures");
        assert_eq!(
            details["thinking_signature"],
            json!({"encoding":"base64","data":"AP+ACg=="})
        );
        assert_eq!(details["tool_calls"][0]["id"], "native-call");
        assert_eq!(
            details["tool_calls"][0]["thinking_signature"],
            json!({"encoding":"base64","data":"/wCA"})
        );
        assert!(reply["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .any(|block| block["type"] == "thinking" && block["signature"] == "legacy textual signature"));
    }

    #[test]
    fn media_and_cleared_payload_coverage_reach_api_doctor_and_markdown() {
        let world = World::in_process("agy-media-coverage", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let uri = "https://example.invalid/image.png";
        let attachment = [bytes(1, "image/png"), bytes(5, uri)].concat();
        let inline = [bytes(1, "image/png"), bytes(2, "stored inline image bytes")].concat();
        // raw_thinking (16) is a field no decoder reads: the step reports it.
        let response = [
            bytes(1, "A reply with media"),
            bytes(20, attachment),
            bytes(20, inline),
            bytes(16, "raw thinking"),
        ]
        .concat();
        let path = database(
            &root,
            &[
                [number(1, 15), number(4, 3), bytes(20, response)].concat(),
                [number(1, 132), number(4, 5), bytes(140, [])].concat(),
            ],
        );
        let adapter = Antigravity::with_home(&root);
        let reference = SessionRef::new(ID, Harness::Antigravity, path);
        let session = cv_core::stream::collect_with(&adapter, &reference, &Demand::COMPLETE).unwrap();
        let coverage = ReadCoverage::of_session(&session).unwrap();
        assert!(coverage.limitations.contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS));
        assert!(coverage.limitations.contains(ReadCoverage::MISSING_PAYLOADS));
        assert!(
            !coverage.limitations.contains(ReadCoverage::MISSING_ATTACHMENTS),
            "stored inline bytes are present"
        );
        let reply = api("messages");
        assert_eq!(evidence(&reply["session"]["extra"]["cv"]["read_coverage"]), coverage);
        assert!(reply["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .any(|block| block["type"] == "image" && block["data_ref"] == uri));
        assert!(reply["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .any(|block| block["type"] == "image"
                && block["data_ref"] == "data:image/png;base64,c3RvcmVkIGlubGluZSBpbWFnZSBieXRlcw=="));
        let mut report = cv_core::doctor::Report::default();
        let mut pass = report.session_with_harness(None, Some(Harness::Antigravity));
        let shell = adapter.try_stream(&reference, &Demand::FACTS, &mut pass).unwrap();
        pass.meta(&shell);
        pass.finish().unwrap();
        assert_eq!(report.to_json()["read_coverage"][0]["coverage"], json!(coverage));
        let mut markdown = Vec::new();
        cv_core::render::write_markdown(&session, &mut markdown, &session.resolver()).unwrap();
        let markdown = String::from_utf8(markdown).unwrap();
        assert!(markdown.contains("some recorded payloads are unavailable"));
        assert!(markdown.contains("some native fields are not interpreted"));
    }

    #[test]
    fn lean_and_verified_reads_preserve_task_details_structured_results_and_interaction_tiers() {
        let world = World::in_process("agy-lean-task-interaction", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let call_a = [bytes(1, "call-task"), bytes(2, "run_command"), bytes(3, "{}")].concat();
        let call_b = [bytes(1, "call-q"), bytes(2, "ask_question"), bytes(3, "{}")].concat();
        let call_c = [bytes(1, "call-u"), bytes(2, "custom_tool"), bytes(3, "{}")].concat();
        let planner = [
            number(1, 15),
            number(4, 3),
            bytes(5, bytes(1, number(1, 1_700_000_000))),
            bytes(
                20,
                [
                    bytes(1, "Running"),
                    bytes(7, call_a.clone()),
                    bytes(7, call_b.clone()),
                    bytes(7, call_c.clone()),
                ]
                .concat(),
            ),
        ]
        .concat();
        let entry = [bytes(1, "exit"), bytes(2, number(4, 1))].concat();
        let res_a = [
            number(1, 132),
            number(4, 3),
            bytes(5, [bytes(1, number(1, 1_700_000_001)), bytes(4, call_a)].concat()),
            bytes(
                140,
                bytes(2, [bytes(1, "task started"), bytes(10, bytes(1, entry))].concat()),
            ),
            bytes(148, [bytes(1, format!("{ID}/task-1")), number(6, 1)].concat()),
        ]
        .concat();
        let opt = [bytes(1, "opt-a"), bytes(2, "Proceed")].concat();
        let answered = [bytes(1, "Continue?"), bytes(2, opt), bytes(4, "opt-a")].concat();
        let res_b = [
            number(1, 132),
            number(4, 3),
            bytes(5, [bytes(1, number(1, 1_700_000_002)), bytes(4, call_b)].concat()),
            bytes(140, bytes(2, bytes(1, "answered"))),
            bytes(147, bytes(2, bytes(22, bytes(1, answered)))),
        ]
        .concat();
        let res_c = [
            number(1, 132),
            number(4, 3),
            bytes(5, [bytes(1, number(1, 1_700_000_003)), bytes(4, call_c)].concat()),
            bytes(140, bytes(2, bytes(1, "future"))),
            bytes(147, bytes(1, bytes(99, []))),
        ]
        .concat();
        let db_path = database(&root, &[planner, res_a, res_b, res_c]);
        let adapter = Antigravity::with_home(&root);

        // Native-only at LEAN/FACTS keeps task_details, omits non-permission interaction,
        // and surfaces ask_question + unsupported interaction + structured_result on RICH.
        let native_ref = SessionRef::new(ID, Harness::Antigravity, &db_path);
        for demand in [Demand::LEAN, Demand::FACTS] {
            let s = cv_core::stream::collect_with(&adapter, &native_ref, &demand).unwrap();
            assert_eq!(
                s.messages[1].extra["antigravity"]["task_details"]["id"],
                format!("{ID}/task-1")
            );
            for idx in [2, 3] {
                assert!(s.messages[idx]
                    .extra
                    .get("antigravity")
                    .and_then(|v| v.get("interaction"))
                    .is_none());
            }
        }
        let rich = cv_core::stream::collect_with(&adapter, &native_ref, &Demand::RICH).unwrap();
        let cv_core::Block::ToolResult { details: Some(d), .. } = &rich.messages[1].content[0] else {
            panic!("expected structured result details");
        };
        assert_eq!(d["result"], json!({"exit": true}));
        assert_eq!(
            rich.messages[2].extra["antigravity"]["interaction"],
            json!({
                "kind": "ask_question",
                "questions": [{
                    "question": "Continue?",
                    "options": [{"id": "opt-a", "text": "Proceed"}],
                    "selected_option_ids": ["opt-a"]
                }]
            })
        );
        assert_eq!(
            rich.messages[3].extra["antigravity"]["interaction"],
            json!({"kind": "unsupported", "native_field": 99})
        );
        assert!(ReadCoverage::of_session(&rich)
            .unwrap()
            .limitations
            .contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS));

        // Verified export merge also preserves task_details at LEAN and structured_result + ask_question at RICH.
        let logs = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let export = logs.join("transcript_full.jsonl");
        let lines = [
            json!({"step_index":0,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2023-11-14T22:13:20Z","content":"Running","tool_calls":[{"name":"run_command","args":{}},{"name":"ask_question","args":{}},{"name":"custom_tool","args":{}}]}),
            json!({"step_index":1,"type":"GENERIC","status":"DONE","created_at":"2023-11-14T22:13:21Z","content":"Created At: 2023-11-14T22:13:21Z\ntask started"}),
            json!({"step_index":2,"type":"GENERIC","status":"DONE","created_at":"2023-11-14T22:13:22Z","content":"Created At: 2023-11-14T22:13:22Z\nanswered"}),
            json!({"step_index":3,"type":"GENERIC","status":"DONE","created_at":"2023-11-14T22:13:23Z","content":"Created At: 2023-11-14T22:13:23Z\nfuture"}),
        ];
        fs::write(&export, lines.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let exp_ref = SessionRef::new(ID, Harness::Antigravity, &export);
        let lean_merged = cv_core::stream::collect_with(&adapter, &exp_ref, &Demand::LEAN).unwrap();
        assert_eq!(
            lean_merged.messages[1].extra["antigravity"]["task_details"]["id"],
            format!("{ID}/task-1")
        );
        let rich_merged = cv_core::stream::collect_with(&adapter, &exp_ref, &Demand::RICH).unwrap();
        let cv_core::Block::ToolResult { details: Some(d), .. } = &rich_merged.messages[1].content[0] else {
            panic!("expected merged structured result");
        };
        assert_eq!(d["result"], json!({"exit": true}));
        assert_eq!(
            rich_merged.messages[2].extra["antigravity"]["interaction"]["kind"],
            "ask_question"
        );

        // Oversized >256 KiB pbtxt annotation is safely ignored.
        let annotations = root.join("annotations");
        fs::create_dir_all(&annotations).unwrap();
        fs::write(
            annotations.join(format!("{ID}.pbtxt")),
            format!("title: \"{}\"\n", "x".repeat(300 * 1024)),
        )
        .unwrap();
        let discovered = adapter.discover().unwrap();
        assert!(discovered[0].title.as_deref().is_none_or(|t| t.len() < 1024));
    }

    #[test]
    fn legacy_image_data_interactions_side_columns_and_direct_subagent_spawns_compose() {
        let world = World::in_process("agy-legacy-fidelity-e2e", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let child_id = "dddddddd-2222-4333-8444-555555555555";
        let dir = root.join("conversations");
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join(format!("{ID}.db"));
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER,
                status INTEGER,
                metadata BLOB,
                error_details BLOB,
                permissions BLOB,
                task_details BLOB,
                render_info BLOB,
                step_payload BLOB,
                step_format INTEGER DEFAULT 0
            );",
        )
        .unwrap();

        // Step 0: USER_INPUT with legacy ImageData (6) alongside Media (9) -> single media array, 0 unread fields.
        let legacy_img = [
            bytes(1, "c3RvcmVkIGlubGluZSBpbWFnZSBieXRlcw=="),
            bytes(2, "image/jpeg"),
            bytes(3, "legacy prompt screenshot"),
        ]
        .concat();
        let modern_media = [bytes(1, "image/png"), bytes(5, "https://example.invalid/a.png")].concat();
        let user_step = [
            number(1, 14),
            number(4, 3),
            bytes(
                19,
                [
                    bytes(3, bytes(1, "Inspect legacy screenshot")),
                    bytes(6, legacy_img),
                    bytes(9, modern_media),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (0, 14, 3, ?1)",
            [&user_step],
        )
        .unwrap();

        // Step 1: PLANNER_RESPONSE issuing file edit + direct invoke_subagent.
        let call_file = [bytes(1, "call-file"), bytes(2, "write_file"), bytes(3, "{}")].concat();
        let call_spawn = [bytes(1, "call-spawn-143"), bytes(2, "invoke_subagent"), bytes(3, "{}")].concat();
        let planner = [
            number(1, 15),
            number(4, 3),
            bytes(
                20,
                [
                    bytes(1, "Editing and spawning"),
                    bytes(7, call_file.clone()),
                    bytes(7, call_spawn.clone()),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (1, 15, 3, ?1)",
            [&planner],
        )
        .unwrap();

        // Step 2: Cleared GENERIC row whose metadata, permissions, task_details, and render_info live in SQLite side columns.
        let side_meta = [bytes(1, number(1, 1_700_000_010)), bytes(4, call_file)].concat();
        let side_perm = bytes(2, bytes(1, [bytes(1, "file"), bytes(2, "/work/out.txt")].concat()));
        let side_task = [bytes(1, format!("{ID}/task-side")), number(6, 0)].concat();
        let side_render = bytes(2, "recovered from SQLite side column");
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, metadata, permissions, task_details, render_info, step_payload)
             VALUES (2, 132, 5, ?1, ?2, ?3, ?4, X'')",
            rusqlite::params![side_meta, side_perm, side_task, side_render],
        )
        .unwrap();

        // Step 3: Direct CortexStepInvokeSubagent (143) naming `child_id` via `conversation_id (5)`.
        let spawn_step = [
            number(1, 127),
            number(4, 3),
            bytes(5, bytes(4, call_spawn)),
            bytes(143, [bytes(5, child_id), number(11, 1)].concat()),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (3, 127, 3, ?1)",
            [&spawn_step],
        )
        .unwrap();
        drop(conn);

        // Child DB + subagent descriptor so `Antigravity::forest` exercises `native::spawn_calls` on `step.last(143)`.
        let child_db = dir.join(format!("{child_id}.db"));
        let child_conn = rusqlite::Connection::open(&child_db).unwrap();
        child_conn
            .execute_batch(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER DEFAULT 0);",
            )
            .unwrap();
        let child_prompt = [
            number(1, 14),
            number(4, 3),
            bytes(19, bytes(3, bytes(1, "Child brief"))),
        ]
        .concat();
        child_conn
            .execute("INSERT INTO steps(idx, step_payload) VALUES (0, ?1)", [&child_prompt])
            .unwrap();
        drop(child_conn);

        let subagents_dir = root.join("brain").join(ID).join(".system_generated/subagents");
        fs::create_dir_all(&subagents_dir).unwrap();
        fs::write(
            subagents_dir.join(format!("{child_id}.json")),
            json!({
                "conversationId": child_id,
                "state": "SUBAGENT_STATE_ALIVE",
                "subagentDescriptor": {"typeName": "helper", "role": "Check"}
            })
            .to_string(),
        )
        .unwrap();

        let adapter = Antigravity::with_home(&root);
        let parent_ref = SessionRef::new(ID, Harness::Antigravity, &db_path);
        let session = cv_core::stream::collect_with(&adapter, &parent_ref, &Demand::RICH).unwrap();
        let coverage = ReadCoverage::of_session(&session).unwrap();
        assert!(
            !coverage.limitations.contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS),
            "all modeled legacy ImageData, side columns, and direct steps should have zero unread fields: {coverage:?}"
        );
        assert!(coverage.limitations.contains(ReadCoverage::MISSING_PAYLOADS));

        let images: Vec<_> = session.messages[0]
            .content
            .iter()
            .filter_map(|b| match b {
                cv_core::Block::Image { media_type, data_ref } => Some((media_type.as_deref(), data_ref.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            images,
            vec![
                (Some("image/png"), Some("https://example.invalid/a.png")),
                (
                    Some("image/jpeg"),
                    Some("data:image/jpeg;base64,c3RvcmVkIGlubGluZSBpbWFnZSBieXRlcw==")
                ),
            ]
        );
        assert_eq!(
            session.messages[2].extra["antigravity"]["task_details"]["id"],
            format!("{ID}/task-side")
        );
        assert_eq!(
            session.messages[2].extra["antigravity"]["interaction"]["target"],
            "/work/out.txt"
        );
        let cv_core::Block::ToolResult {
            tool_use_id, content, ..
        } = &session.messages[2].content[0]
        else {
            panic!("expected recovered side-column tool result");
        };
        assert_eq!(tool_use_id, "call-file");
        assert_eq!(
            content.try_resolve(&session.resolver()).unwrap().as_ref(),
            "recovered from SQLite side column"
        );

        let forest = adapter
            .forest(&parent_ref, cv_core::SubagentDemand::Rich, None, &|_, _| true)
            .unwrap();
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].session.id, child_id);
        assert_eq!(forest[0].tool_use_id.as_deref(), Some("call-spawn-143"));
    }

    #[test]
    fn cross_seam_reconciled_direct_steps_raw_summaries_swarm_board_events_and_affordances_compose() {
        let world = World::in_process("agy-p3-cross-seam-e2e", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let killed_worker = "cccccccc-3333-4444-8555-666666666666";

        let dir = root.join("conversations");
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join(format!("{ID}.db"));
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER,
                status INTEGER,
                metadata BLOB,
                error_details BLOB,
                permissions BLOB,
                task_details BLOB,
                render_info BLOB,
                step_payload BLOB,
                step_format INTEGER DEFAULT 0
            );",
        )
        .unwrap();

        let meta_ts = |sec: u64, src: u64, call: Option<&[u8]>| -> Vec<u8> {
            let mut m = [bytes(1, number(1, sec)), number(3, src)].concat();
            if let Some(call) = call {
                m.extend(bytes(4, call));
            }
            m
        };

        // Step 0: USER_INPUT (2026-11-20T00:00:00Z = 1_795_132_800).
        let prompt_text = "Audit Pass 3 cross-seam state";
        let step0 = [
            number(1, 14),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_800, 4, None)),
            bytes(19, bytes(3, bytes(1, prompt_text))),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (0, 14, 3, ?1)",
            [&step0],
        )
        .unwrap();

        // Step 1: PLANNER_RESPONSE issuing run_command, view_file (AbsolutePath), ask_question, and cleared run_command.
        let call_run = [
            bytes(1, "call-run"),
            bytes(2, "run_command"),
            bytes(3, r#"{"CommandLine":"cargo check"}"#),
        ]
        .concat();
        let call_view = [
            bytes(1, "call-view"),
            bytes(2, "view_file"),
            bytes(3, r#"{"AbsolutePath":"/workspace/p3/README.md"}"#),
        ]
        .concat();
        let call_ask = [
            bytes(1, "call-ask"),
            bytes(2, "ask_question"),
            bytes(
                3,
                r#"{"questions":[{"question":"Ship Pass 3?","options":["Yes","No"]}]}"#,
            ),
        ]
        .concat();
        let call_cleared = [
            bytes(1, "call-cleared"),
            bytes(2, "run_command"),
            bytes(3, r#"{"CommandLine":"pytest"}"#),
        ]
        .concat();
        let step1 = [
            number(1, 15),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_801, 2, None)),
            bytes(
                20,
                [
                    bytes(1, "Checking workspace and asking user"),
                    bytes(7, call_run.clone()),
                    bytes(7, call_view.clone()),
                    bytes(7, call_ask.clone()),
                    bytes(7, call_cleared.clone()),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (1, 15, 3, ?1)",
            [&step1],
        )
        .unwrap();

        // Step 2: Direct RUN_COMMAND (overlaps with partial JSONL export).
        let step2 = [
            number(1, 21),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_802, 2, Some(&call_run))),
            bytes(
                28,
                [
                    bytes(2, "/workspace/p3"),
                    number(6, 0),
                    bytes(21, bytes(1, "Finished check in 0.4s")),
                    bytes(31, "zsh"),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (2, 21, 3, ?1)",
            [&step2],
        )
        .unwrap();

        // Step 3 (native-only tail): Direct VIEW_FILE (14) with legacy ImageData (14).
        let img_data = [bytes(1, "aGVsbG8="), bytes(2, "image/png"), bytes(3, "diagram")].concat();
        let step3 = [
            number(1, 8),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_803, 2, Some(&call_view))),
            bytes(
                14,
                [
                    bytes(1, "file:///workspace/p3/README.md"),
                    number(2, 1),
                    number(3, 10),
                    bytes(4, "# Pass 3\nAll green"),
                    bytes(14, img_data),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (3, 8, 3, ?1)",
            [&step3],
        )
        .unwrap();

        // Step 4 (native-only tail): Direct ASK_QUESTION (154) with RequestedInteraction (56) + CompletedInteraction (147).
        let opt_yes = [bytes(1, "yes"), bytes(2, "Yes")].concat();
        let opt_no = [bytes(1, "no"), bytes(2, "No")].concat();
        let req_q = [
            bytes(1, "Ship Pass 3?"),
            bytes(2, opt_yes.clone()),
            bytes(2, opt_no.clone()),
            number(3, 0),
        ]
        .concat();
        let ans_q = [
            bytes(1, "Ship Pass 3?"),
            bytes(2, opt_yes),
            bytes(2, opt_no),
            bytes(4, "yes"),
        ]
        .concat();
        let req_56 = bytes(22, bytes(1, req_q));
        let completed_147 = [bytes(1, req_56.clone()), bytes(2, bytes(22, bytes(1, ans_q.clone())))].concat();
        let step4 = [
            number(1, 138),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_804, 2, Some(&call_ask))),
            bytes(56, req_56),
            bytes(147, completed_147),
            bytes(154, bytes(1, ans_q)),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (4, 138, 3, ?1)",
            [&step4],
        )
        .unwrap();

        // Step 5 (native-only tail): CLEARED row (status = 5, step_type = 21 RUN_COMMAND) with empty step_payload,
        // while SQLite side columns `steps.metadata` and `steps.render_info` hold call ID + terminal output.
        let side_meta = meta_ts(1_795_132_805, 2, Some(&call_cleared));
        let terminal_block = [
            bytes(1, "pytest"),
            bytes(2, "/workspace/p3"),
            number(3, 0),
            bytes(4, "12 passed in 0.1s"),
        ]
        .concat();
        let side_render = bytes(10, bytes(3, terminal_block));
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, metadata, render_info, step_payload)
             VALUES (5, 21, 5, ?1, ?2, X'')",
            rusqlite::params![side_meta, side_render],
        )
        .unwrap();
        drop(conn);

        // Partial JSONL export holding only steps 0..=2 (omitting tool call IDs so reconciliation supplies them).
        let logs_dir = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs_dir).unwrap();
        let export_path = logs_dir.join("transcript_full.jsonl");
        let export_lines = [
            json!({
                "step_index": 0,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-20T00:00:00Z",
                "content": format!("<USER_REQUEST>\n{prompt_text}\n</USER_REQUEST>")
            }),
            json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-20T00:00:01Z",
                "content": "Checking workspace and asking user",
                "tool_calls": [
                    {"name": "run_command", "args": {"CommandLine": "cargo check"}},
                    {"name": "view_file", "args": {"AbsolutePath": "/workspace/p3/README.md"}},
                    {"name": "ask_question", "args": {"questions": [{"question": "Ship Pass 3?", "options": ["Yes", "No"]}]}},
                    {"name": "run_command", "args": {"CommandLine": "pytest"}}
                ]
            }),
            json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "RUN_COMMAND",
                "status": "DONE",
                "created_at": "2026-11-20T00:00:02Z",
                "content": "Created At: 2026-11-20T00:00:02Z\nFinished check in 0.4s"
            }),
        ];
        fs::write(
            &export_path,
            export_lines.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();

        // 1.5.13 conversation_summaries.db with CascadeTrajectorySummary (`raw_summary`) for parent and killed worker.
        let parent_raw = [
            bytes(1, "Cross-seam summary"),
            bytes(7, number(1, 1_795_132_800)),
            bytes(
                9,
                [
                    bytes(1, "file:///workspace/p3"),
                    bytes(2, "file:///workspace"),
                    bytes(4, "p3/integration"),
                ]
                .concat(),
            ),
            bytes(15, bytes(1, "Cross-Seam P3 Title")),
            bytes(17, [bytes(6, ID), bytes(18, "proj-p3")].concat()),
            number(23, 0),
        ]
        .concat();
        let worker_raw = [
            bytes(1, "Killed worker summary"),
            bytes(7, number(1, 1_795_132_810)),
            bytes(15, bytes(1, "Summary Worker Title")),
            bytes(
                17,
                [
                    bytes(5, ID),
                    bytes(6, ID),
                    bytes(8, [bytes(1, "verifier"), bytes(2, "Audit seams")].concat()),
                ]
                .concat(),
            ),
            number(23, 1),
        ]
        .concat();
        let sum_db = root.join("conversation_summaries.db");
        {
            let sconn = rusqlite::Connection::open(&sum_db).unwrap();
            sconn
                .execute_batch(
                    "CREATE TABLE conversation_summaries (
                        conversation_id TEXT PRIMARY KEY,
                        killed INTEGER,
                        status TEXT,
                        group_id TEXT,
                        raw_summary BLOB
                    );",
                )
                .unwrap();
            sconn
                .execute(
                    "INSERT INTO conversation_summaries VALUES (?1, 0, 'IDLE', 'grp-p3', ?2)",
                    rusqlite::params![ID, parent_raw],
                )
                .unwrap();
            sconn
                .execute(
                    "INSERT INTO conversation_summaries VALUES (?1, 0, 'KILLED', 'grp-p3', ?2)",
                    rusqlite::params![killed_worker, worker_raw],
                )
                .unwrap();
        }

        // Swarm members.json + board.jsonl for unavailable killed worker recovery.
        let swarm_dir = root.join("brain").join(ID).join(".system_generated/swarm");
        fs::create_dir_all(&swarm_dir).unwrap();
        fs::write(
            swarm_dir.join("members.json"),
            json!({
                "title": "Pass 3 Cross-Seam Swarm",
                "members": {
                    "coordinator": {"conversation_id": ID, "role": "coordinator"},
                    "seam-verifier": {"conversation_id": killed_worker, "role": "worker", "phase": "RUNNING", "killed": false}
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            swarm_dir.join("board.jsonl"),
            concat!(
                "{\"seq\":1,\"agent\":\"coordinator\",\"text\":\"@seam-verifier Verify Pass 3 seams\",\"kind\":\"brief\"}\n",
                "{\"seq\":2,\"agent\":\"seam-verifier\",\"text\":\"@coordinator [done] all 3 Pass 3 seams verified clean\"}\n"
            ),
        )
        .unwrap();

        let adapter = Antigravity::with_home(&root);
        let exp_ref = SessionRef::new(ID, Harness::Antigravity, &export_path);
        let session = cv_core::stream::collect_with(&adapter, &exp_ref, &Demand::RICH).unwrap();

        // 1. Header & coverage: raw_summary + swarm_title + reconciled native tail with zero unread fields or inferred tool IDs.
        assert_eq!(session.title.as_deref(), Some("Cross-Seam P3 Title"));
        assert_eq!(session.cwd.as_deref(), Some(Path::new("/workspace/p3")));
        assert_eq!(
            session.git.as_ref().and_then(|g| g.branch.as_deref()),
            Some("p3/integration")
        );
        assert_eq!(session.extra["antigravity"]["swarm_title"], "Pass 3 Cross-Seam Swarm");
        let coverage = ReadCoverage::of_session(&session).unwrap();
        assert!(coverage.sources.contains(ReadCoverage::EXPORTED_HISTORY));
        assert!(coverage.sources.contains(ReadCoverage::NATIVE_STORE));
        assert!(coverage.limitations.contains(ReadCoverage::MISSING_PAYLOADS));
        assert!(!coverage.limitations.contains(ReadCoverage::UNRECONCILED_HISTORY));
        assert!(!coverage.limitations.contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS));
        assert!(!coverage.limitations.contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
        assert_eq!(session.messages.len(), 6);

        // 2. Step 2 (`RUN_COMMAND` in export) merged native call ID + typed payload from native direct step.
        let cv_core::Block::ToolResult {
            tool_use_id: id2,
            details: Some(det2),
            ..
        } = &session.messages[2].content[0]
        else {
            panic!("expected merged RUN_COMMAND tool result");
        };
        assert_eq!(id2, "call-run");
        assert_eq!(det2["payload"]["type"], "CortexStepRunCommand");
        assert_eq!(det2["payload"]["shell_name"], "zsh");
        assert_eq!(session.messages[2].extra["antigravity"]["step_type"], "RUN_COMMAND");

        // 3. Step 3 (`VIEW_FILE` in native tail) projected content, payload, and legacy `ImageData` attachment.
        let cv_core::Block::ToolResult {
            tool_use_id: id3,
            details: Some(det3),
            ..
        } = &session.messages[3].content[0]
        else {
            panic!("expected VIEW_FILE tool result");
        };
        assert_eq!(id3, "call-view");
        assert_eq!(det3["payload"]["path"], "file:///workspace/p3/README.md");
        assert!(session.messages[3].content.iter().any(|b| matches!(
            b,
            cv_core::Block::Image { media_type: Some(mt), data_ref: Some(dr) }
            if mt == "image/png" && dr == "data:image/png;base64,aGVsbG8="
        )));

        // 4. Step 4 (`ASK_QUESTION` in native tail) projected both `CortexStepAskQuestion` payload and `56`/`147` interaction.
        assert_eq!(session.messages[4].extra["antigravity"]["step_type"], "ASK_QUESTION");
        assert_eq!(
            session.messages[4].extra["antigravity"]["interaction"],
            json!({
                "kind": "ask_question",
                "questions": [{
                    "question": "Ship Pass 3?",
                    "options": [{"id": "yes", "text": "Yes"}, {"id": "no", "text": "No"}],
                    "selected_option_ids": ["yes"]
                }]
            })
        );

        // 5. Step 5 (`CLEARED` empty-payload row) recovered `RUN_COMMAND` type, `call-cleared` ID, and terminal output from SQLite side columns.
        assert_eq!(session.messages[5].extra["antigravity"]["step_type"], "RUN_COMMAND");
        let cv_core::Block::ToolResult {
            tool_use_id: id5,
            content: body5,
            ..
        } = &session.messages[5].content[0]
        else {
            panic!("expected side-column CLEARED tool result");
        };
        assert_eq!(id5, "call-cleared");
        let resolved5 = body5.try_resolve(&session.resolver()).unwrap().into_owned();
        assert!(
            resolved5.contains("$ pytest") && resolved5.contains("12 passed in 0.1s"),
            "unexpected recovered terminal output: {resolved5}"
        );

        // 6. EventExtractor (`EventSink`): `view_file` `AbsolutePath` -> `file_read`.
        let mut sink = cv_core::events::EventSink::new(session.cwd.clone()).with_source(export_path.clone());
        for m in &session.messages {
            assert_eq!(sink.observe_message(m), cv_core::Flow::Continue);
        }
        let events = sink.into_events().unwrap();
        assert!(
            events.iter().any(|e| {
                e.kind == "file_read"
                    && e.tool.as_deref() == Some("view_file")
                    && e.target.as_deref() == Some("/workspace/p3/README.md")
            }),
            "expected view_file AbsolutePath -> file_read in {events:?}"
        );

        // 7. Affordances (`affordance::of_session`): `ask_question` -> `Question` / `"ask"`.
        let affordances = cv_core::affordance::of_session(&session).unwrap();
        assert!(
            affordances.iter().any(|a| {
                a.kind == cv_core::affordance::Kind::Question
                    && a.action == "ask"
                    && a.tool.as_deref() == Some("ask_question")
                    && a.summary.contains("Ship Pass 3?")
            }),
            "expected Question/ask affordance in {affordances:?}"
        );

        // 8. Lanes (`lanes::lanes_of`): `raw_summary` `killed = true` + `swarm/board.jsonl` `last_text` recovery.
        let lanes = cv_core::lanes::lanes_of(&exp_ref).unwrap();
        let worker_lane = lanes.iter().find(|l| l.session_id == killed_worker).unwrap();
        assert_eq!(worker_lane.status.as_str(), "killed");
        assert_eq!(
            worker_lane.last_text.as_deref(),
            Some("@coordinator [done] all 3 Pass 3 seams verified clean")
        );
    }

    #[test]
    fn formatted_direct_steps_late_partial_exports_text_columns_and_elicitation_affordances_stay_clean() {
        let world = World::in_process("agy-p3-auditor-regressions", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let dir = root.join("conversations");
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join(format!("{ID}.db"));
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // TEXT-typed `step_type` and `status` columns must never fail `native::open` with InvalidColumnType.
        conn.execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type TEXT,
                status TEXT,
                metadata BLOB,
                step_payload BLOB,
                step_format INTEGER DEFAULT 0
            );",
        )
        .unwrap();

        let meta_ts = |sec: u64, src: u64, call: Option<&[u8]>| -> Vec<u8> {
            let mut m = [bytes(1, number(1, sec)), number(3, src)].concat();
            if let Some(call) = call {
                m.extend(bytes(4, call));
            }
            m
        };

        let call_view = [bytes(1, "call-v"), bytes(2, "view_file"), bytes(3, "{}")].concat();
        let call_dir = [bytes(1, "call-d"), bytes(2, "list_directory"), bytes(3, "{}")].concat();
        let call_ask = [bytes(1, "call-q"), bytes(2, "ask_question"), bytes(3, "{}")].concat();
        let call_elic = [bytes(1, "call-e"), bytes(2, "mcp_tool"), bytes(3, "{}")].concat();
        let call_appr = [bytes(1, "call-a"), bytes(2, "run_command"), bytes(3, "{}")].concat();

        // Step 0: Native-only PLANNER_RESPONSE (export starts late at step 1!).
        let step0 = [
            number(1, 15),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_900, 2, None)),
            bytes(
                20,
                [
                    bytes(1, "Native-only planner head"),
                    bytes(7, call_view.clone()),
                    bytes(7, call_dir.clone()),
                    bytes(7, call_ask.clone()),
                    bytes(7, call_elic.clone()),
                    bytes(7, call_appr.clone()),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (0, 'PLANNER_RESPONSE', 'DONE', ?1)",
            [&step0],
        )
        .unwrap();

        // Step 1: VIEW_FILE with unnumbered native content + legacy ImageData (exported with line numbers + missing media URI).
        let step1 = [
            number(1, 8),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_901, 2, Some(&call_view))),
            bytes(
                14,
                [
                    bytes(1, "file:///workspace/a.rs"),
                    bytes(4, "fn main() {}"),
                    bytes(14, [bytes(1, "aGVsbG8="), bytes(2, "image/png")].concat()),
                ]
                .concat(),
            ),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (1, '8', '3', ?1)",
            [&step1],
        )
        .unwrap();

        // Step 2: LIST_DIRECTORY (native JSON content vs human-readable exported listing).
        let step2 = [
            number(1, 9),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_902, 2, Some(&call_dir))),
            bytes(15, [bytes(1, "/workspace"), bytes(2, "src")].concat()),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (2, '9', '3', ?1)",
            [&step2],
        )
        .unwrap();

        // Step 3: Contentless ASK_QUESTION (exported with "Created At: ..." header).
        let ans_q = [bytes(1, "Proceed?"), bytes(4, "yes")].concat();
        let step3 = [
            number(1, 138),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_903, 2, Some(&call_ask))),
            bytes(147, bytes(2, bytes(22, bytes(1, ans_q.clone())))),
            bytes(154, bytes(1, ans_q)),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (3, '138', '3', ?1)",
            [&step3],
        )
        .unwrap();

        // Step 4: Verified prompt anchor so native head + tail reconcile cleanly.
        let step4 = [
            number(1, 14),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_904, 4, None)),
            bytes(19, bytes(3, bytes(1, "Continue"))),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (4, '14', '3', ?1)",
            [&step4],
        )
        .unwrap();

        // Step 5 (native-only): GENERIC with elicitation `action = 1` ("accepted" / even if "approved") -> NOT Approval affordance.
        let elic = [
            bytes(1, bytes(20, [bytes(1, "mcp-srv"), bytes(3, "Auth?")].concat())),
            bytes(2, bytes(20, number(1, 1))),
        ]
        .concat();
        let step5 = [
            number(1, 132),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_905, 2, Some(&call_elic))),
            bytes(140, bytes(2, bytes(1, "ok"))),
            bytes(147, elic),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (5, '132', '3', ?1)",
            [&step5],
        )
        .unwrap();

        // Step 6 (native-only): GENERIC with targetless `approval_interaction` (23) -> `Approval/approve` with trimmed `"approval"` summary.
        let appr = [bytes(1, bytes(23, [])), bytes(2, bytes(23, number(1, 1)))].concat();
        let step6 = [
            number(1, 132),
            number(4, 3),
            bytes(5, meta_ts(1_795_132_906, 2, Some(&call_appr))),
            bytes(140, bytes(2, bytes(1, "approved"))),
            bytes(147, appr),
        ]
        .concat();
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, step_payload) VALUES (6, '132', '3', ?1)",
            [&step6],
        )
        .unwrap();
        drop(conn);

        // Hostile TEXT `raw_summary` in `conversation_summaries.db` is ignored via `typeof(raw_summary) = 'blob'`.
        let sum_db = root.join("conversation_summaries.db");
        {
            let sconn = rusqlite::Connection::open(&sum_db).unwrap();
            sconn
                .execute_batch(
                    "CREATE TABLE conversation_summaries (
                        conversation_id TEXT PRIMARY KEY,
                        title TEXT,
                        raw_summary BLOB
                    );",
                )
                .unwrap();
            sconn
                .execute(
                    "INSERT INTO conversation_summaries VALUES (?1, 'Safe Title', 'hostile-text-not-blob')",
                    [ID],
                )
                .unwrap();
        }

        // Export starts at step 1 with formatted `VIEW_FILE` (+ missing linked media), `LIST_DIRECTORY`, `ASK_QUESTION`, and `USER_INPUT`.
        let logs_dir = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs_dir).unwrap();
        let export_path = logs_dir.join("transcript_full.jsonl");
        let export_lines = [
            json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "VIEW_FILE",
                "status": "DONE",
                "created_at": "2026-11-20T00:01:41Z",
                "content": "Showing lines 1-1 of 1\n1: fn main() {}",
                "media": [{"mime_type": "image/png", "uri": "file:///nonexistent/missing-view.png"}]
            }),
            json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "LIST_DIRECTORY",
                "status": "DONE",
                "created_at": "2026-11-20T00:01:42Z",
                "content": "Directory /workspace:\n  src/"
            }),
            json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "ASK_QUESTION",
                "status": "DONE",
                "created_at": "2026-11-20T00:01:43Z",
                "content": "Created At: 2026-11-20T00:01:43Z"
            }),
            json!({
                "step_index": 4,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-20T00:01:44Z",
                "content": "<USER_REQUEST>\nContinue\n</USER_REQUEST>"
            }),
        ];
        fs::write(
            &export_path,
            export_lines.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();

        let adapter = Antigravity::with_home(&root);
        let exp_ref = SessionRef::new(ID, Harness::Antigravity, &export_path);
        let session = cv_core::stream::collect_with(&adapter, &exp_ref, &Demand::RICH).unwrap();
        let coverage = ReadCoverage::of_session(&session).unwrap();
        assert!(!coverage.limitations.contains(ReadCoverage::UNRECONCILED_HISTORY));
        assert!(!coverage.limitations.contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
        assert!(!coverage.limitations.contains(ReadCoverage::MISSING_ATTACHMENTS));
        assert_eq!(session.title.as_deref(), Some("Safe Title"));
        assert_eq!(session.messages.len(), 7);

        // Step 1 (`VIEW_FILE`) recovered `ImageData` and `call-v`.
        let cv_core::Block::ToolResult { tool_use_id: id1, .. } = &session.messages[1].content[0] else {
            panic!("expected VIEW_FILE");
        };
        assert_eq!(id1, "call-v");
        assert!(session.messages[1].content.iter().any(|b| matches!(
            b,
            cv_core::Block::Image { data_ref: Some(dr), .. } if dr == "data:image/png;base64,aGVsbG8="
        )));

        // Step 2 (`LIST_DIRECTORY`) kept `structured_result` despite human-formatted exported text.
        let cv_core::Block::ToolResult {
            tool_use_id: id2,
            details: Some(det2),
            ..
        } = &session.messages[2].content[0]
        else {
            panic!("expected LIST_DIRECTORY details");
        };
        assert_eq!(id2, "call-d");
        assert_eq!(det2["result"], json!({"path": "/workspace", "entries": ["src"]}));

        // Step 3 (`ASK_QUESTION`) kept `payload` and `interaction` despite contentless native step.
        let cv_core::Block::ToolResult {
            tool_use_id: id3,
            details: Some(det3),
            ..
        } = &session.messages[3].content[0]
        else {
            panic!("expected ASK_QUESTION details");
        };
        assert_eq!(id3, "call-q");
        assert_eq!(det3["payload"]["type"], "CortexStepAskQuestion");
        assert_eq!(
            session.messages[3].extra["antigravity"]["interaction"]["kind"],
            "ask_question"
        );

        // Affordances: elicitation is NOT an `Approval`, and targetless approval trims to `"approval"`.
        let affordances = cv_core::affordance::of_session(&session).unwrap();
        let approvals: Vec<_> = affordances
            .iter()
            .filter(|a| a.kind == cv_core::affordance::Kind::Approval)
            .collect();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].summary, "approval");
    }
}
