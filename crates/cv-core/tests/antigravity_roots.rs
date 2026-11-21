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

#[test]
fn resume_is_qualified_when_the_cli_root_does_not_hold_the_conversation() {
    let world = World::in_process("agy-roots-resume", cv_core::env::Env::enter_isolated);
    let gemini = world.home.join(".gemini");
    let (cli, ide) = (gemini.join("antigravity-cli"), gemini.join("antigravity-ide"));
    fs::create_dir_all(&cli).unwrap();
    // Only the IDE mirror has this conversation, and only as an export.
    log(&ide, "transcript_full.jsonl", "ide request");
    let adapter = Antigravity::with_home(&world.home);
    let reference = adapter.discover().unwrap().remove(0);
    let caveat = adapter
        .resume_caveat(&reference)
        .expect("agy cannot resume this session");
    assert!(caveat.contains("antigravity-cli/conversations"), "{caveat}");
    assert!(caveat.contains("antigravity-ide"), "{caveat}");

    // Once the CLI root holds the native conversation, the command stands unqualified,
    // wherever cv read the richer history from.
    native(&cli, "cli native request");
    assert_eq!(adapter.resume_caveat(&reference), None);
    let reference = adapter.discover().unwrap().remove(0);
    assert_eq!(adapter.resume_caveat(&reference), None);
}

#[test]
fn equal_rank_mirrors_prefer_recency_and_cross_root_native_resume_merges_verified_tail() {
    let world = World::in_process("agy-roots-recency-resume", cv_core::env::Env::enter_isolated);
    let gemini = world.home.join(".gemini");
    let (cli, ide) = (gemini.join("antigravity-cli"), gemini.join("antigravity-ide"));

    // Equal rank=4 logs: older CLI log yields to newer IDE log; equal mtime keeps CLI.
    let cli_full = log(&cli, "transcript_full.jsonl", "older cli export");
    cv_testkit::settle(&cli);
    let ide_full = log(&ide, "transcript_full.jsonl", "newer ide export");
    let adapter = Antigravity::with_home(&world.home);
    let found = adapter.discover().unwrap().remove(0);
    assert_eq!(found.path, ide_full);

    // Cross-root native resume: IDE has exported steps 0..1, CLI resumed natively with steps 0..2
    // (no brain log in CLI). Discovery selects IDE's full export, and parse() merges CLI's step 2.
    fs::remove_dir_all(cli.join("brain")).unwrap();
    let stamp = "2026-11-15T10:00:00Z";
    let sec = 1_794_736_800u64;
    let export_lines = [
        json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":stamp,"content":"shared prompt"}),
        json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":stamp,"content":"first reply"}),
    ];
    fs::write(
        &ide_full,
        export_lines.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();

    fn pb_varint(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (n & 127) as u8;
            n >>= 7;
            out.push(b | if n == 0 { 0 } else { 128 });
            if n == 0 {
                return out;
            }
        }
    }
    let pb_num = |f: u64, v: u64| [pb_varint(f << 3), pb_varint(v)].concat();
    let pb_bytes = |f: u64, v: &[u8]| [pb_varint((f << 3) | 2), pb_varint(v.len() as u64), v.to_vec()].concat();
    let meta = pb_bytes(1, &pb_num(1, sec));

    fs::create_dir_all(cli.join("conversations")).unwrap();
    let cli_db = cli.join("conversations").join(format!("{ID}.db"));
    let conn = rusqlite::Connection::open(&cli_db).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
    let s0 = [
        pb_num(1, 14),
        pb_num(4, 3),
        pb_bytes(5, &meta),
        pb_bytes(19, &pb_bytes(3, &pb_bytes(1, b"shared prompt"))),
    ]
    .concat();
    let s1 = [
        pb_num(1, 15),
        pb_num(4, 3),
        pb_bytes(5, &meta),
        pb_bytes(20, &pb_bytes(1, b"first reply")),
    ]
    .concat();
    let s2 = [
        pb_num(1, 15),
        pb_num(4, 3),
        pb_bytes(5, &meta),
        pb_bytes(20, &pb_bytes(1, b"resumed cli native tail")),
    ]
    .concat();
    for (idx, payload) in [(0i64, s0), (1, s1), (2, s2)] {
        conn.execute(
            "INSERT INTO steps(idx, step_payload) VALUES (?1, ?2)",
            rusqlite::params![idx, payload],
        )
        .unwrap();
    }
    drop(conn);

    let reference = adapter.discover().unwrap().remove(0);
    assert_eq!(reference.path, ide_full);
    let mut probe = cv_core::source::Probe::default();
    let baseline = probe.snapshot(&reference);

    let session = adapter.parse(&reference).unwrap();
    assert_eq!(text(&session), "shared prompt\nfirst reply\nresumed cli native tail");
    let coverage = cv_core::coverage::ReadCoverage::of_session(&session).unwrap();
    assert!(coverage
        .sources
        .contains(cv_core::coverage::ReadCoverage::EXPORTED_HISTORY));
    assert!(coverage.sources.contains(cv_core::coverage::ReadCoverage::NATIVE_STORE));
    assert!(!coverage
        .limitations
        .contains(cv_core::coverage::ReadCoverage::UNRECONCILED_HISTORY));

    // Appending step 3 in the cross-root CLI native DB invalidates the session's dependencies.
    let conn = rusqlite::Connection::open(&cli_db).unwrap();
    let s3 = [
        pb_num(1, 15),
        pb_num(4, 3),
        pb_bytes(5, &meta),
        pb_bytes(20, &pb_bytes(1, b"later cli native step")),
    ]
    .concat();
    conn.execute("INSERT INTO steps(idx, step_payload) VALUES (3, ?1)", [s3])
        .unwrap();
    drop(conn);
    let mut probe_after = cv_core::source::Probe::default();
    let after = probe_after.snapshot(&reference);
    assert!(!baseline.fresh(&after, &mut probe_after));
    let _ = cli_full;
}

#[test]
fn summary_wal_updates_invalidate_only_the_affected_session_via_row_certificates() {
    let world = World::in_process("agy-summary-certificates", cv_core::env::Env::enter_isolated);
    let app = world.home.join(".gemini/antigravity-cli");
    let id_b = "22222222-3333-4444-8555-666666666666";
    let id_c = "33333333-4444-4555-8666-777777777777";
    for (id, body) in [
        (ID, "first session"),
        (id_b, "second session"),
        (id_c, "absent from summaries"),
    ] {
        let p = app
            .join("brain")
            .join(id)
            .join(".system_generated/logs/transcript_full.jsonl");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let rec = json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":body});
        fs::write(p, format!("{rec}\n")).unwrap();
    }

    let sum_db = app.join("conversation_summaries.db");
    let writer = rusqlite::Connection::open(&sum_db).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE conversation_summaries (
                 conversation_id TEXT PRIMARY KEY,
                 title TEXT,
                 preview TEXT,
                 workspace_uris TEXT,
                 parent_conversation_id TEXT,
                 agent_name TEXT,
                 source TEXT,
                 project_id TEXT,
                 last_modified_time TEXT
             );",
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO conversation_summaries(conversation_id, title) VALUES (?1, 'Title A v1'), (?2, 'Title B v1')",
            rusqlite::params![ID, id_b],
        )
        .unwrap();

    let adapter = Antigravity::with_home(&world.home);
    let refs = adapter.discover().unwrap().refs;
    assert_eq!(refs.len(), 3);
    let find_ref = |target: &str| refs.iter().find(|r| r.id == target).unwrap().clone();
    let (ref_a, ref_b, ref_c) = (find_ref(ID), find_ref(id_b), find_ref(id_c));

    let mut probe = cv_core::source::Probe::default();
    let capture = |r: &cv_core::SessionRef, p: &mut cv_core::source::Probe| {
        let mut reads = cv_core::source::ReadSet::default();
        reads.record(&r.path);
        let mut b = p.snapshot(r).with_reads(&reads);
        b.rows = p.rows(r);
        assert!(b.rows.is_some(), "{} must have a summary certificate", r.id);
        b
    };
    let (base_a, base_b, base_c) = (
        capture(&ref_a, &mut probe),
        capture(&ref_b, &mut probe),
        capture(&ref_c, &mut probe),
    );
    assert_eq!(adapter.parse(&ref_a).unwrap().title.as_deref(), Some("Title A v1"));

    // Update ONLY session A in the shared WAL without touching any transcript file.
    writer
        .execute(
            "UPDATE conversation_summaries SET title = 'Title A v2' WHERE conversation_id = ?1",
            [ID],
        )
        .unwrap();

    let mut after_probe = cv_core::source::Probe::default();
    let (now_a, now_b, now_c) = (
        after_probe.snapshot(&ref_a),
        after_probe.snapshot(&ref_b),
        after_probe.snapshot(&ref_c),
    );
    // Strict file baseline moved for all three (shared `-wal` grew), so `fresh` is false...
    assert!(!base_a.fresh(&now_a, &mut after_probe));
    assert!(!base_b.fresh(&now_b, &mut after_probe));
    assert!(!base_c.fresh(&now_c, &mut after_probe));
    // ...while `rows_fresh` invalidates ONLY session A and admits B (present) and C (seeded absent)!
    assert!(!base_a.rows_fresh(&ref_a, &now_a, &mut after_probe));
    assert!(base_b.rows_fresh(&ref_b, &now_b, &mut after_probe));
    assert!(base_c.rows_fresh(&ref_c, &now_c, &mut after_probe));
    assert_eq!(adapter.parse(&ref_a).unwrap().title.as_deref(), Some("Title A v2"));
    drop(writer);
}
