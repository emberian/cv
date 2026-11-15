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
        let (status, reply) = cv_core::api::route_json(&["api", "session", "antigravity", ID, suffix], "");
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
            for (suffix, pointer) in [
                ("head", "/extra/cv/read_coverage"),
                ("messages", "/session/extra/cv/read_coverage"),
            ] {
                let reply = api(suffix);
                assert!(evidence(reply.pointer(pointer).unwrap())
                    .sources
                    .contains(ReadCoverage::NATIVE_STORE));
            }
        }
    }

    #[test]
    fn media_and_cleared_payload_coverage_reach_api_doctor_and_markdown() {
        let world = World::in_process("agy-media-coverage", cv_core::env::Env::enter_isolated);
        let root = world.home.join(".gemini/antigravity-cli");
        let uri = "https://example.invalid/image.png";
        let attachment = [bytes(1, "image/png"), bytes(5, uri)].concat();
        let inline = [bytes(1, "image/png"), bytes(2, "stored inline image bytes")].concat();
        let response = [bytes(1, "A reply with media"), bytes(20, attachment), bytes(20, inline)].concat();
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
}
