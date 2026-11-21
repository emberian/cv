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
        let planner = [
            number(1, 15),
            number(4, 3),
            bytes(5, bytes(1, number(1, 1_700_000_000))),
            bytes(
                20,
                [bytes(1, "Running"), bytes(7, call_a.clone()), bytes(7, call_b.clone())].concat(),
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
        let res_b = [
            number(1, 132),
            number(4, 3),
            bytes(5, [bytes(1, number(1, 1_700_000_002)), bytes(4, call_b)].concat()),
            bytes(140, bytes(2, bytes(1, "answered"))),
            bytes(147, bytes(1, bytes(22, []))),
        ]
        .concat();
        let db_path = database(&root, &[planner, res_a, res_b]);
        let adapter = Antigravity::with_home(&root);

        // Native-only at LEAN/FACTS keeps task_details, omits non-permission interaction,
        // and surfaces non-permission interaction + structured_result on RICH.
        let native_ref = SessionRef::new(ID, Harness::Antigravity, &db_path);
        for demand in [Demand::LEAN, Demand::FACTS] {
            let s = cv_core::stream::collect_with(&adapter, &native_ref, &demand).unwrap();
            assert_eq!(
                s.messages[1].extra["antigravity"]["task_details"]["id"],
                format!("{ID}/task-1")
            );
            assert!(s.messages[2]
                .extra
                .get("antigravity")
                .and_then(|v| v.get("interaction"))
                .is_none());
        }
        let rich = cv_core::stream::collect_with(&adapter, &native_ref, &Demand::RICH).unwrap();
        let cv_core::Block::ToolResult { details: Some(d), .. } = &rich.messages[1].content[0] else {
            panic!("expected structured result details");
        };
        assert_eq!(d["result"], json!({"exit": true}));
        assert_eq!(
            rich.messages[2].extra["antigravity"]["interaction"],
            json!({"kind": "unsupported", "native_field": 22})
        );
        assert!(ReadCoverage::of_session(&rich)
            .unwrap()
            .limitations
            .contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS));

        // Verified export merge also preserves task_details at LEAN and structured_result at RICH.
        let logs = root.join("brain").join(ID).join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let export = logs.join("transcript_full.jsonl");
        let lines = [
            json!({"step_index":0,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2023-11-14T22:13:20Z","content":"Running","tool_calls":[{"name":"run_command","args":{}},{"name":"ask_question","args":{}}]}),
            json!({"step_index":1,"type":"GENERIC","status":"DONE","created_at":"2023-11-14T22:13:21Z","content":"Created At: 2023-11-14T22:13:21Z\ntask started"}),
            json!({"step_index":2,"type":"GENERIC","status":"DONE","created_at":"2023-11-14T22:13:22Z","content":"Created At: 2023-11-14T22:13:22Z\nanswered"}),
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
}
