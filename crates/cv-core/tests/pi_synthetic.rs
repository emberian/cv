//! The Pi reader against hand-written synthetic sessions, always on.
//!
//! These do NOT need `PI_FIXTURES`: every session is built in the test from invented text, so a
//! bare checkout tests the reader. The real-corpus tests in `pi_fixtures.rs` skip with a reason
//! when the frozen input is absent; these never skip.
//!
//! What is covered: a plain session, a compacted one, a branched one (an abandoned branch must not
//! appear), a `context_edit` that drops its target, one that replaces it, and a malformed parent
//! link (the walk must not loop or panic).

use cv_core::harness::pi::Pi;
use cv_core::harness::Adapter;
use cv_core::ir::{Block, Harness, Message, MessageKind, Role};
use cv_core::stream::{Demand, Flow, MessageSink};
use std::path::PathBuf;

/// A sink that keeps the messages it is given.
#[derive(Default)]
struct Collect(Vec<Message>);

impl MessageSink for Collect {
    fn message(&mut self, m: Message) -> Flow {
        self.0.push(m);
        Flow::Continue
    }
}

/// Write `lines` as a session file in a fresh directory and return (dir, path).
fn session(name: &str, lines: &[&str]) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pi-synth-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the fixture dir");
    let path = dir.join(format!("{name}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").expect("write the session");
    (dir, path)
}

fn read(dir: &PathBuf, path: &PathBuf, demand: &Demand) -> Collect {
    let adapter = Pi::with_roots(vec![dir.clone()]);
    let refs = adapter.discover().expect("discovery").refs;
    let r = refs.iter().find(|r| r.path == *path).expect("the session is discovered");
    let mut sink = Collect::default();
    adapter.stream(r, demand, &mut sink).expect("stream");
    sink
}

/// The text of a message, joined from its text blocks and any tool result's content.
fn text(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.inline_str().unwrap_or_default().to_string()),
            Block::ToolResult { content, .. } => Some(content.inline_str().unwrap_or_default().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

const HEADER: &str = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-11-19T10:00:00.000Z","cwd":"/tmp/synth"}"#;

#[test]
fn a_plain_session_reads_whole() {
    let (dir, path) = session("plain", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    assert_eq!(c.0.len(), 2, "both turns are emitted");
    assert_eq!(text(&c.0[0]), "first");
    assert_eq!(text(&c.0[1]), "second");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_compacted_session_starts_at_the_boundary() {
    let (dir, path) = session("compacted", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"dropped by the compaction"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"user","content":[{"type":"text","text":"kept"}]}}"#,
        r#"{"type":"compaction","id":"c1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","summary":"the summary","firstKeptEntryId":"m2","tokensBefore":100}"#,
        r#"{"type":"message","id":"m3","parentId":"c1","timestamp":"2026-11-19T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"after"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    let kinds: Vec<String> = c.0.iter().map(|m| format!("{:?}", m.kind)).collect();
    assert_eq!(kinds[0], "CompactionBoundary", "the boundary is first");
    assert!(kinds.contains(&"CompactionSummary".to_string()), "the summary is emitted");
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(!texts.iter().any(|t| t.contains("dropped by the compaction")), "the pre-boundary turn is gone");
    assert!(texts.iter().any(|t| t == "kept"), "the kept turn survives");
    assert!(texts.iter().any(|t| t == "after"), "the post-compaction turn survives");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_abandoned_branch_does_not_appear() {
    let (dir, path) = session("branched", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"root"}]}}"#,
        // the abandoned branch
        r#"{"type":"message","id":"dead","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}"#,
        // the live branch, which is the last entry
        r#"{"type":"message","id":"live","parentId":"m1","timestamp":"2026-11-19T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"live"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(texts.iter().any(|t| t == "root"), "the shared root is in the context");
    assert!(texts.iter().any(|t| t == "live"), "the live branch is in the context");
    assert!(!texts.iter().any(|t| t == "abandoned"), "the abandoned branch is NOT in the context");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_context_edit_with_a_null_replacement_drops_its_target() {
    let (dir, path) = session("edit-drop", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"secret"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m1","replacement":null}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(!texts.iter().any(|t| t == "secret"), "the dropped target is not in the context");
    assert!(texts.iter().any(|t| t == "reply"), "the other turn survives");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_context_edit_with_content_replaces_its_target() {
    let (dir, path) = session("edit-replace", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"original"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m1","replacement":{"content":[{"type":"text","text":"rewritten"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert_eq!(c.0.len(), 2, "exactly two turns: the edit adds no message of its own");
    assert_eq!(texts, vec!["rewritten".to_string(), "reply".to_string()], "the target is replaced in place, not duplicated");
    assert!(!texts.iter().any(|t| t == "original"), "the original text is gone");
    // THE TARGET KEEPS ITS IDENTITY: the edit changes the content, not who said it.
    let edited = c.0.iter().find(|m| text(m) == "rewritten").expect("the edited turn");
    assert_eq!(edited.id.as_deref(), Some("m1"), "the target's id survives, not the edit's");
    assert_eq!(edited.role, Role::User, "the target's role survives");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_context_edit_replacing_an_assistant_turn_keeps_its_role_and_usage() {
    let (dir, path) = session("edit-assistant", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"ask"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"original answer"}],"usage":{"input":10,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":5,"cost":{"total":0.5}}}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m2","replacement":{"content":[{"type":"text","text":"rewritten answer"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    let edited = c.0.iter().find(|m| text(m) == "rewritten answer").expect("the edited turn");
    assert_eq!(edited.id.as_deref(), Some("m2"), "the assistant target's id survives");
    assert_eq!(edited.role, Role::Assistant, "an assistant turn stays an assistant turn");
    let usage = edited.usage.as_ref().expect("the target's usage survives the edit");
    assert_eq!(usage.output_tokens, Some(20), "the target's usage is not lost");
    assert_eq!(usage.reasoning_tokens, Some(5), "the target's reasoning count is not lost");
    assert!(!c.0.iter().any(|m| text(m) == "original answer"), "the original text is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_context_edit_replacing_a_tool_result_keeps_its_call_id() {
    let (dir, path) = session("edit-tool", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"run it"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"toolResult","toolCallId":"call-7","toolName":"bash","isError":false,"content":[{"type":"text","text":"original output"}]}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m2","replacement":{"content":[{"type":"text","text":"rewritten output"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::LEAN);
    let edited = c.0.iter().find(|m| text(m) == "rewritten output").expect("the edited tool result");
    assert_eq!(edited.id.as_deref(), Some("m2"), "the tool result's id survives");
    assert_eq!(edited.role, Role::Tool, "a tool result stays a tool result");
    let call = edited.content.iter().find_map(|b| match b {
        Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
        _ => None,
    }).expect("the tool result block survives");
    assert_eq!(call, "call-7", "the tool call id survives the edit");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn complete_keeps_the_original_content_and_the_edit_record() {
    let (dir, path) = session("edit-complete", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"original"}]}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","targetId":"m1","replacement":{"content":[{"type":"text","text":"rewritten"}]}}"#,
    ]);
    let c = read(&dir, &path, &Demand::COMPLETE);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(texts.iter().any(|t| t == "original"), "complete keeps the original content");
    assert!(!texts.iter().any(|t| t == "rewritten"), "complete does not apply the edit");
    let carrier = c.0.iter().find(|m| m.kind == MessageKind::Carrier).expect("the edit record is carried");
    assert_eq!(carrier.id.as_deref(), Some("e1"), "the carrier is the edit's own record");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_parent_link_does_not_loop_or_panic() {
    let (dir, path) = session("malformed", &[
        HEADER,
        // a cycle: m1 -> m2 -> m1
        r#"{"type":"message","id":"m1","parentId":"m2","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}"#,
    ]);
    // the walk must terminate; what it emits is less important than that it returns
    let c = read(&dir, &path, &Demand::LEAN);
    assert!(!c.0.is_empty(), "a cyclic chain still yields the entries it walked");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn complete_emits_the_whole_chain() {
    let (dir, path) = session("complete", &[
        HEADER,
        r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"dropped"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"user","content":[{"type":"text","text":"kept"}]}}"#,
        r#"{"type":"compaction","id":"c1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","summary":"s","firstKeptEntryId":"m2"}"#,
    ]);
    let lean = read(&dir, &path, &Demand::LEAN);
    let full = read(&dir, &path, &Demand::COMPLETE);
    assert!(full.0.len() > lean.0.len(), "complete emits more than the projection");
    let texts: Vec<String> = full.0.iter().map(text).collect();
    assert!(texts.iter().any(|t| t == "dropped"), "complete keeps the pre-boundary turn");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_harness_is_pi() {
    let adapter = Pi::with_roots(vec![PathBuf::from("/nonexistent")]);
    assert_eq!(adapter.harness(), Harness::Pi);
}
