//! Which store a session is read from as its files appear, disappear and diverge across
//! the CLI, desktop and IDE roots. Native rows are encoded from the 1.5.9 field
//! descriptors; all text is synthetic.
#![cfg(feature = "sqlite")]

use cv_core::harness::{antigravity::Antigravity, Parse};
use cv_core::{Adapter, Block, Session};
use cv_testkit::World;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

const ID: &str = "11111111-2222-4333-8444-555555555555";

fn bytes(field: u8, value: impl AsRef<[u8]>) -> Vec<u8> {
    let value = value.as_ref();
    assert!(field < 16 && value.len() < 128);
    [vec![(field << 3) | 2, value.len() as u8], value.to_vec()].concat()
}
/// One native prompt row: Step{type(1)=USER_INPUT, status(4)=DONE, user_input(19){items(3){text(1)}}}.
fn native(root: &Path, text: &str) -> PathBuf {
    let dir = root.join("conversations");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{ID}.db"));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
    let input = bytes(3, bytes(1, text));
    let payload = [vec![8, 14, 32, 3], vec![(19 << 3) | 2, 1, input.len() as u8], input].concat();
    conn.execute("INSERT INTO steps(idx, step_payload) VALUES (0, ?1)", [payload])
        .unwrap();
    path
}
fn log(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join("brain").join(ID).join(".system_generated/logs").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":text});
    fs::write(&path, format!("{record}\n")).unwrap();
    path
}
fn text(session: &Session) -> String {
    let resolver = cv_core::lazy::Resolver::new(session.source_path.clone());
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.try_resolve(&resolver).unwrap().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_log_removed_after_discovery_falls_back_to_the_surviving_store() {
    let world = World::in_process("agy-roots-log-removed", cv_core::env::Env::enter_isolated);
    let full = log(&world.base, "transcript_full.jsonl", "exported request");
    let compact = log(&world.base, "transcript.jsonl", "compact request");
    let database = native(&world.base, "native request");
    let adapter = Antigravity::with_home(&world.base);
    let reference = adapter.discover().unwrap().remove(0);
    assert_eq!(reference.path, full);

    // The writer rotates the full log away between discovery and the read.
    fs::remove_file(&full).unwrap();
    let session = adapter.parse(&reference).unwrap();
    assert_eq!(text(&session), "compact request");
    assert_eq!(session.source_path.as_deref(), Some(compact.as_path()));

    fs::remove_file(&compact).unwrap();
    let session = adapter.parse(&reference).unwrap();
    assert_eq!(text(&session), "native request");
    assert_eq!(session.source_path.as_deref(), Some(database.as_path()));

    // With nothing left the read fails; it does not succeed empty.
    fs::remove_file(&database).unwrap();
    assert!(adapter.parse(&reference).is_err());
}

#[test]
fn mirrored_roots_yield_one_session_from_the_fullest_readable_export() {
    let world = World::in_process("agy-roots-mirrors", cv_core::env::Env::enter_isolated);
    let gemini = world.base.join(".gemini");
    let (cli, desktop, ide) = (
        gemini.join("antigravity-cli"),
        gemini.join("antigravity"),
        gemini.join("antigravity-ide"),
    );
    // The CLI root has only a native store, the desktop mirror a compact export, the IDE
    // mirror the full export with diverged content.
    native(&cli, "cli native request");
    log(&desktop, "transcript.jsonl", "desktop compact request");
    let full = log(&ide, "transcript_full.jsonl", "ide full request");
    let adapter = Antigravity::with_home(&world.base);
    let found = adapter.discover().unwrap();
    let sessions: Vec<_> = found.into_iter().collect();
    assert_eq!(sessions.len(), 1, "one conversation id is one session");
    assert_eq!(sessions[0].path, full);
    let session = adapter.parse(&sessions[0]).unwrap();
    assert_eq!(text(&session), "ide full request");
    // The other root's native store is a different copy: it is not merged into this one.
    let coverage = cv_core::coverage::ReadCoverage::of_session(&session).unwrap();
    assert!(!coverage.sources.contains(cv_core::coverage::ReadCoverage::NATIVE_STORE));
}
