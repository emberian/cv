//! The Pi reader against hand-written synthetic sessions, always on.
//!
//! Covers plain, compacted, multiply-compacted, and branched sessions, chained `context_edit`s,
//! complete-mode `_record` carriers and `inactive_branch` tagging, `model_change` propagation,
//! and malformed cyclic parent links.

use cv_core::env::Env;
use cv_core::harness::pi::Pi;
use cv_core::harness::Adapter;
use cv_core::ir::{Block, Harness, Message, MessageKind, Role, Session, CARRIER_KEY};
use cv_core::stream::{Demand, Flow, MessageSink};
use cv_testkit::World;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Collect(Vec<Message>);

impl MessageSink for Collect {
    fn message(&mut self, m: Message) -> Flow {
        self.0.push(m);
        Flow::Continue
    }
}

fn session(world: &World, name: &str, lines: &[&str]) -> (PathBuf, PathBuf) {
    let dir = world.base.join(name);
    std::fs::create_dir_all(&dir).expect("the fixture dir");
    let path = dir.join(format!("{name}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").expect("write the session");
    (dir, path)
}

fn read(dir: &Path, path: &Path, demand: &Demand) -> (Session, Collect) {
    let adapter = Pi::with_roots(vec![dir.to_path_buf()]);
    let refs = adapter.discover().expect("discovery").refs;
    let r = refs.iter().find(|r| r.path == path).expect("the session is discovered");
    let mut sink = Collect::default();
    let s = adapter.stream(r, demand, &mut sink).expect("stream");
    (s, sink)
}

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

const HEADER: &str =
    r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-11-19T10:00:00.000Z","cwd":"/tmp/synth"}"#;

#[test]
fn a_plain_session_reads_whole() {
    let world = World::in_process("pi-synth-plain", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "plain",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    assert_eq!(c.0.len(), 2, "both turns are emitted");
    assert_eq!(text(&c.0[0]), "first");
    assert_eq!(text(&c.0[1]), "second");
}

#[test]
fn a_compacted_session_starts_at_the_boundary() {
    let world = World::in_process("pi-synth-compacted", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "compacted",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"dropped by the compaction"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"user","content":[{"type":"text","text":"kept"}]}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","summary":"the summary","firstKeptEntryId":"m2","tokensBefore":100}"#,
            r#"{"type":"message","id":"m3","parentId":"c1","timestamp":"2026-11-19T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"after"}]}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    let kinds: Vec<String> = c.0.iter().map(|m| format!("{:?}", m.kind)).collect();
    assert_eq!(kinds[0], "CompactionBoundary", "the boundary is first");
    assert!(
        kinds.contains(&"CompactionSummary".to_string()),
        "the summary is emitted"
    );
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(
        !texts.iter().any(|t| t.contains("dropped by the compaction")),
        "the pre-boundary turn is gone"
    );
    assert!(texts.iter().any(|t| t == "kept"), "the kept turn survives");
    assert!(texts.iter().any(|t| t == "after"), "the post-compaction turn survives");
}

#[test]
fn multiple_compactions_emit_only_latest_boundary_and_keep_pre_compaction_model_change() {
    let world = World::in_process("pi-synth-multi-compact", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "multi-compact",
        &[
            HEADER,
            // Pre-compaction model_change before firstKeptEntryId ("m2"): must still set s.model!
            r#"{"type":"model_change","id":"mc0","parentId":"s1","timestamp":"2026-11-19T10:00:00.500Z","provider":"anthropic","modelId":"claude-opus-4-6"}"#,
            r#"{"type":"message","id":"sys0","parentId":"mc0","timestamp":"2026-11-19T10:00:00.700Z","message":{"role":"system","content":"initial system prompt"}}"#,
            r#"{"type":"message","id":"m1","parentId":"sys0","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":"dropped"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"user","content":"kept across both"}}"#,
            r#"{"type":"message","id":"sys1","parentId":"m2","timestamp":"2026-11-19T10:00:02.500Z","message":{"role":"system","content":"mid system prompt"}}"#,
            // Earlier compaction inside the span that c2's firstKeptEntryId ("m2") retains:
            r#"{"type":"compaction","id":"c1","parentId":"sys1","timestamp":"2026-11-19T10:00:03.000Z","summary":"older summary","firstKeptEntryId":"m2","tokensBefore":200}"#,
            r#"{"type":"message","id":"m3","parentId":"c1","timestamp":"2026-11-19T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"between compactions"}]}}"#,
            r#"{"type":"model_change","id":"mc1","parentId":"m3","timestamp":"2026-11-19T10:00:04.500Z","provider":"openai","modelId":"gpt-5.4"}"#,
            // Latest compaction whose firstKeptEntryId ("m2") precedes c1:
            r#"{"type":"compaction","id":"c2","parentId":"mc1","timestamp":"2026-11-19T10:00:05.000Z","summary":"latest summary","firstKeptEntryId":"m2","tokensBefore":500}"#,
            r#"{"type":"message","id":"m4","parentId":"c2","timestamp":"2026-11-19T10:00:06.000Z","message":{"role":"user","content":"after latest"}}"#,
        ],
    );
    let (s, c) = read(&dir, &path, &Demand::LEAN);
    assert_eq!(s.model.as_deref(), Some("gpt-5.4"));
    let boundaries: Vec<_> =
        c.0.iter()
            .filter(|m| m.kind == MessageKind::CompactionBoundary)
            .collect();
    assert_eq!(boundaries.len(), 1, "older retained compaction c1 must emit 0 messages");
    assert_eq!(boundaries[0].id.as_deref(), Some("c2"));
    let summaries: Vec<_> =
        c.0.iter()
            .filter(|m| m.kind == MessageKind::CompactionSummary)
            .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(text(summaries[0]), "latest summary");
    assert!(!c.0.iter().any(|m| m.kind == MessageKind::SystemPrompt));

    let model_change =
        c.0.iter()
            .find(|m| m.kind == MessageKind::ModelChange)
            .expect("mc1 retained");
    assert_eq!(model_change.model.as_deref(), Some("gpt-5.4"));

    // Also test when the ONLY model_change precedes firstKeptEntryId:
    let (dir2, path2) = session(
        &world,
        "pre-compact-mc",
        &[
            HEADER,
            r#"{"type":"model_change","id":"mc0","parentId":"s1","provider":"anthropic","modelId":"claude-opus-4-6"}"#,
            r#"{"type":"message","id":"m1","parentId":"mc0","message":{"role":"user","content":"kept"}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"m1","summary":"sum","firstKeptEntryId":"m1"}"#,
        ],
    );
    let (s2, _) = read(&dir2, &path2, &Demand::LEAN);
    assert_eq!(
        s2.model.as_deref(),
        Some("claude-opus-4-6"),
        "s.model must find model_change on chain even before firstKeptEntryId"
    );
}

#[test]
fn an_abandoned_branch_does_not_appear_in_lean_and_is_marked_inactive_in_complete() {
    let world = World::in_process("pi-synth-branched", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "branched",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"root"}]}}"#,
            r#"{"type":"message","id":"dead","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}"#,
            r#"{"type":"message","id":"live","parentId":"m1","timestamp":"2026-11-19T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"live"}]}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert_eq!(texts, vec!["root".to_string(), "live".to_string()]);

    let (_, full) = read(&dir, &path, &Demand::COMPLETE);
    let dead = full.0.iter().find(|m| m.id.as_deref() == Some("dead")).unwrap();
    assert_eq!(
        dead.harness_extra(Harness::Pi).and_then(|b| b.get("inactive_branch")),
        Some(&Value::Bool(true))
    );
    let live = full.0.iter().find(|m| m.id.as_deref() == Some("live")).unwrap();
    assert!(live
        .harness_extra(Harness::Pi)
        .and_then(|b| b.get("inactive_branch"))
        .is_none());
}

#[test]
fn a_context_edit_with_a_null_replacement_drops_its_target() {
    let world = World::in_process("pi-synth-edit-drop", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "edit-drop",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"secret"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}"#,
            r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m1","replacement":null}"#,
        ],
    );
    let (s, c) = read(&dir, &path, &Demand::LEAN);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(!texts.iter().any(|t| t == "secret"));
    assert!(texts.iter().any(|t| t == "reply"));
    assert!(
        s.title.is_none(),
        "a null-dropped user prompt must not leak into Session::title"
    );
    assert!(
        s.harness_extra(Harness::Pi)
            .and_then(|b| b.get("dropped_entry_types"))
            .is_none(),
        "context_edit must not be flagged in dropped_entry_types"
    );
}

#[test]
fn a_context_edit_with_content_replaces_its_target() {
    let world = World::in_process("pi-synth-edit-replace", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "edit-replace",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"original"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}"#,
            r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m1","replacement":{"content":[{"type":"text","text":"rewritten"}]}}"#,
        ],
    );
    let (s, c) = read(&dir, &path, &Demand::LEAN);
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert_eq!(c.0.len(), 2);
    assert_eq!(texts, vec!["rewritten".to_string(), "reply".to_string()]);
    assert_eq!(s.title.as_deref(), Some("rewritten"));
    let edited = &c.0[0];
    assert_eq!(edited.id.as_deref(), Some("m1"));
    assert_eq!(edited.role, Role::User);
}

#[test]
fn chained_context_edits_abandoned_edits_and_non_message_targets() {
    let world = World::in_process("pi-synth-edit-chains", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "edit-chains",
        &[
            HEADER,
            // Empty-id noise entry must not collide in by_id:
            r#"{"type":"session_info","id":"","parentId":"s1","name":""}"#,
            r#"{"type":"message","id":"pre","parentId":"s1","message":{"role":"user","content":"pre-compaction dropped"}}"#,
            r#"{"type":"message","id":"m1","parentId":"pre","message":{"role":"user","content":"u1-orig"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","message":{"role":"user","content":"u2-orig"}}"#,
            r#"{"type":"message","id":"m3","parentId":"m2","message":{"role":"user","content":"u3-orig"}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"m3","summary":"keep m1..m3","firstKeptEntryId":"m1"}"#,
            // Abandoned branch edit on m1 (must NOT apply):
            r#"{"type":"context_edit","id":"dead-edit","parentId":"c1","targetId":"m1","replacement":null}"#,
            // Active chain:
            // m1: replace -> replace (second wins)
            r#"{"type":"context_edit","id":"e1","parentId":"c1","targetId":"m1","replacement":{"content":"u1-v1"}}"#,
            r#"{"type":"context_edit","id":"e2","parentId":"e1","targetId":"m1","replacement":{"content":"u1-v2"}}"#,
            // m2: replace -> null (dropped)
            r#"{"type":"context_edit","id":"e3","parentId":"e2","targetId":"m2","replacement":{"content":"u2-v1"}}"#,
            r#"{"type":"context_edit","id":"e4","parentId":"e3","targetId":"m2","replacement":null}"#,
            // m3: null -> replace (revived with new content)
            r#"{"type":"context_edit","id":"e5","parentId":"e4","targetId":"m3","replacement":null}"#,
            r#"{"type":"context_edit","id":"e6","parentId":"e5","targetId":"m3","replacement":{"content":"u3-revived"}}"#,
            // Edit targeting pre-compaction dropped entry `pre` (no-op):
            r#"{"type":"context_edit","id":"e7","parentId":"e6","targetId":"pre","replacement":{"content":"pre-revived"}}"#,
            // Object edit targeting non-message entry `c1` (must be ignored cleanly):
            r#"{"type":"context_edit","id":"e8","parentId":"e7","targetId":"c1","replacement":{"content":"bogus"}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    let user_texts: Vec<String> = c.0.iter().filter(|m| m.role == Role::User).map(text).collect();
    assert_eq!(user_texts, vec!["u1-v2".to_string(), "u3-revived".to_string()]);
    let summary = c.0.iter().find(|m| m.kind == MessageKind::CompactionSummary).unwrap();
    assert_eq!(
        text(summary),
        "keep m1..m3",
        "compaction summary untouched by object edit"
    );
}

#[test]
fn a_context_edit_replacing_an_assistant_turn_keeps_its_role_and_usage() {
    let world = World::in_process("pi-synth-edit-assistant", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "edit-assistant",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"ask"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"original answer"}],"usage":{"input":10,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":5,"cost":{"total":0.5}}}}"#,
            r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m2","replacement":{"content":[{"type":"text","text":"rewritten answer"}]}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    let edited =
        c.0.iter()
            .find(|m| text(m) == "rewritten answer")
            .expect("the edited turn");
    assert_eq!(edited.id.as_deref(), Some("m2"));
    assert_eq!(edited.role, Role::Assistant);
    let usage = edited.usage.as_ref().expect("the target's usage survives the edit");
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.reasoning_tokens, Some(5));
    assert!(!c.0.iter().any(|m| text(m) == "original answer"));
}

#[test]
fn a_context_edit_replacing_a_tool_result_keeps_its_call_id() {
    let world = World::in_process("pi-synth-edit-tool", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "edit-tool",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"run it"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"toolResult","toolCallId":"call-7","toolName":"bash","isError":false,"content":[{"type":"text","text":"original output"}]}}"#,
            r#"{"type":"context_edit","id":"e1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","targetId":"m2","replacement":{"content":[{"type":"text","text":"rewritten output"}]}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    let edited =
        c.0.iter()
            .find(|m| text(m) == "rewritten output")
            .expect("the edited tool result");
    assert_eq!(edited.id.as_deref(), Some("m2"));
    assert_eq!(edited.role, Role::Tool);
    let call = edited
        .content
        .iter()
        .find_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .expect("the tool result block survives");
    assert_eq!(call, "call-7");
}

#[test]
fn complete_keeps_original_content_carries_edit_and_unknown_records_under_carrier_key() {
    let world = World::in_process("pi-synth-complete-carrier", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "edit-complete",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"original"}]}}"#,
            r#"{"type":"context_edit","id":"e1","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","targetId":"m1","replacement":{"content":[{"type":"text","text":"rewritten"}]}}"#,
            r#"{"type":"brand_new_entry","id":"u1","parentId":"e1","timestamp":"2026-11-19T10:00:03.000Z","customField":42}"#,
        ],
    );
    let (lean_s, _) = read(&dir, &path, &Demand::LEAN);
    assert_eq!(
        lean_s.extra["pi"]["dropped_entry_types"],
        json!(["brand_new_entry"]),
        "lean records unknown entry types without flagging context_edit"
    );

    let (full_s, c) = read(&dir, &path, &Demand::COMPLETE);
    assert!(full_s
        .harness_extra(Harness::Pi)
        .and_then(|b| b.get("dropped_entry_types"))
        .is_none());
    let texts: Vec<String> = c.0.iter().map(text).collect();
    assert!(texts.iter().any(|t| t == "original"));
    assert!(!texts.iter().any(|t| t == "rewritten"));

    let edit_carrier =
        c.0.iter()
            .find(|m| m.id.as_deref() == Some("e1"))
            .expect("the edit record is carried");
    assert_eq!(edit_carrier.kind, MessageKind::Carrier);
    assert_eq!(edit_carrier.extra["pi"]["record_type"], "context_edit");
    assert_eq!(
        serde_json::to_value(edit_carrier).unwrap()["extra"][CARRIER_KEY]["targetId"],
        "m1"
    );

    let unknown_carrier =
        c.0.iter()
            .find(|m| m.id.as_deref() == Some("u1"))
            .expect("unknown entry type is carried under COMPLETE");
    assert_eq!(unknown_carrier.kind, MessageKind::Carrier);
    assert_eq!(unknown_carrier.extra["pi"]["record_type"], "brand_new_entry");
    assert_eq!(
        serde_json::to_value(unknown_carrier).unwrap()["extra"][CARRIER_KEY]["customField"],
        42
    );

    let user_wire = serde_json::to_value(&c.0[0]).unwrap();
    assert_eq!(user_wire["extra"][CARRIER_KEY]["id"], "m1");
    assert!(c.0[0]
        .harness_extra(Harness::Pi)
        .is_none_or(|b| !b.contains_key("native")));
}

#[test]
fn a_malformed_parent_link_does_not_loop_or_panic() {
    let world = World::in_process("pi-synth-cycle", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "malformed",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"m2","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}"#,
        ],
    );
    let (_, c) = read(&dir, &path, &Demand::LEAN);
    assert!(!c.0.is_empty());
}

#[test]
fn complete_emits_the_whole_chain() {
    let world = World::in_process("pi-synth-complete", Env::enter_isolated);
    let (dir, path) = session(
        &world,
        "complete",
        &[
            HEADER,
            r#"{"type":"message","id":"m1","parentId":"s1","timestamp":"2026-11-19T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"dropped"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-19T10:00:02.000Z","message":{"role":"user","content":[{"type":"text","text":"kept"}]}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"m2","timestamp":"2026-11-19T10:00:03.000Z","summary":"s","firstKeptEntryId":"m2"}"#,
        ],
    );
    let (_, lean) = read(&dir, &path, &Demand::LEAN);
    let (_, full) = read(&dir, &path, &Demand::COMPLETE);
    assert!(full.0.len() > lean.0.len());
    let texts: Vec<String> = full.0.iter().map(text).collect();
    assert!(texts.iter().any(|t| t == "dropped"));
}

#[test]
fn the_harness_is_pi() {
    let adapter = Pi::with_roots(vec![PathBuf::from("/nonexistent")]);
    assert_eq!(adapter.harness(), Harness::Pi);
}

#[test]
fn pi_v3_tools_classify_extract_events_hunk_fingerprints_and_compact_labels() {
    use cv_core::tools::classify::{
        compact_label, extract_diff_hunk_hashes, hash_normalized_line, is_large_payload_tool,
        visit_tool_ops_with_hunks, ClassifyOpts, ToolOp,
    };

    let world = World::in_process("pi-synth-tools-events", Env::enter_isolated);
    let cwd = world.base.join("repo");
    std::fs::create_dir_all(&cwd).unwrap();

    let big_pad = "x".repeat(70_000);
    let lines = [
        json!({
            "type": "session",
            "version": 3,
            "id": "pi-tools-v3",
            "timestamp": "2026-11-19T11:00:00.000Z",
            "cwd": cwd,
        })
        .to_string(),
        json!({
            "type": "message",
            "id": "u1",
            "parentId": "pi-tools-v3",
            "timestamp": "2026-11-19T11:00:01.000Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "inspect, edit, and test the crate"}]
            }
        })
        .to_string(),
        json!({
            "type": "message",
            "id": "a1",
            "parentId": "u1",
            "timestamp": "2026-11-19T11:00:02.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "toolCall",
                        "id": "c-ls",
                        "name": "ls",
                        "arguments": {"path": "src"}
                    },
                    {
                        "type": "toolCall",
                        "id": "c-find",
                        "name": "find",
                        "arguments": {"pattern": "*.rs", "path": "src"}
                    },
                    {
                        "type": "toolCall",
                        "id": "c-grep",
                        "name": "grep",
                        "arguments": {"pattern": "fn parse_header", "path": "src"}
                    },
                    {
                        "type": "toolCall",
                        "id": "c-read",
                        "name": "read",
                        "arguments": {"path": "src/lib.rs", "offset": 1, "limit": 80}
                    },
                    {
                        "type": "toolCall",
                        "id": "c-write",
                        "name": "write",
                        "arguments": {
                            "path": "src/generated.rs",
                            "content": "pub fn generated_seed() -> u64 {\n    0xdead_beef\n}\n",
                            "pad": big_pad,
                        }
                    },
                    {
                        "type": "toolCall",
                        "id": "c-edit-single",
                        "name": "edit",
                        "arguments": {
                            "path": "src/lib.rs",
                            "oldText": "let limit = 16;",
                            "newText": "let limit = compute_pi_limit(input);\nassert!(limit > 0);",
                            "pad": big_pad,
                        }
                    },
                    {
                        "type": "toolCall",
                        "id": "c-edit-multi",
                        "name": "edit",
                        "arguments": {
                            "path": "src/parser.rs",
                            "edits": [
                                {
                                    "oldText": "fn alpha() {}",
                                    "newText": "pub fn alpha_v3() -> bool {\n    true\n}"
                                },
                                {
                                    "oldText": "fn beta() {}",
                                    "newText": "pub fn beta_v3() -> usize {\n    42\n}"
                                }
                            ]
                        }
                    },
                    {
                        "type": "toolCall",
                        "id": "c-bash",
                        "name": "bash",
                        "arguments": {"command": "cargo test --lib", "timeout": 30}
                    }
                ]
            }
        })
        .to_string(),
        json!({
            "type": "message",
            "id": "r-err",
            "parentId": "a1",
            "timestamp": "2026-11-19T11:00:03.000Z",
            "message": {
                "role": "toolResult",
                "toolCallId": "c-bash",
                "toolName": "bash",
                "isError": true,
                "content": [{"type": "text", "text": "test parser::alpha failed"}]
            }
        })
        .to_string(),
    ];
    let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (dir, path) = session(&world, "pi-tools-v3", &line_refs);

    assert!(is_large_payload_tool(Some(Harness::Pi), "write"));
    assert!(is_large_payload_tool(Some(Harness::Pi), "edit"));

    let adapter = Pi::with_roots(vec![dir]);
    let refs = adapter.discover().expect("discover").refs;
    let r = refs.iter().find(|r| r.path == path).expect("session ref");

    // Stream with spans so >64 KiB write/edit arguments exercise lazy Json::Source views.
    let mut spanned = Collect::default();
    let s = adapter
        .stream(r, &Demand::LEAN.with_spans(), &mut spanned)
        .expect("stream with spans");
    let asst = spanned
        .0
        .iter()
        .find(|m| m.id.as_deref() == Some("a1"))
        .expect("assistant message");

    let mut labels = Vec::new();
    let mut classified = Vec::new();
    for block in &asst.content {
        if let Block::ToolUse { name, input, .. } = block {
            if matches!(name.as_str(), "write" | "edit") && input.resolve().unwrap().get("pad").is_some() {
                assert!(
                    matches!(input, cv_core::Json::Source(_)),
                    "{name} >64 KiB must stay lazy"
                );
            }
            let label = input
                .with_view(|v| compact_label(Some(Harness::Pi), name, v, 80))
                .unwrap();
            labels.push(label);
            input
                .with_view(|v| {
                    visit_tool_ops_with_hunks(Some(Harness::Pi), name, v, ClassifyOpts::FULL, |sem, hunks| {
                        classified.push((sem.into_owned(), hunks));
                    })
                })
                .unwrap();
        }
    }

    assert_eq!(
        labels,
        vec![
            "ls · src",
            "find · *.rs",
            "grep · fn parse_header",
            "read · src/lib.rs",
            "write · src/generated.rs",
            "edit · src/lib.rs",
            "edit · src/parser.rs",
            "bash · cargo test --lib",
        ]
    );

    assert_eq!(classified.len(), 8);
    assert_eq!(
        classified[0].0.op,
        ToolOp::Search {
            path: Some("src".into()),
            query: None
        }
    );
    assert_eq!(
        classified[1].0.op,
        ToolOp::Search {
            path: Some("src".into()),
            query: Some("*.rs".into())
        }
    );
    assert_eq!(
        classified[2].0.op,
        ToolOp::Search {
            path: Some("src".into()),
            query: Some("fn parse_header".into())
        }
    );
    assert_eq!(
        classified[3].0.op,
        ToolOp::FileRead {
            path: "src/lib.rs".into()
        }
    );
    assert_eq!(
        classified[4].0.op,
        ToolOp::FileEdit {
            path: "src/generated.rs".into(),
            is_full_write: true
        }
    );
    assert_eq!(
        classified[5].0.op,
        ToolOp::FileEdit {
            path: "src/lib.rs".into(),
            is_full_write: false
        }
    );
    assert_eq!(
        classified[6].0.op,
        ToolOp::FileEdit {
            path: "src/parser.rs".into(),
            is_full_write: false
        }
    );
    assert_eq!(
        classified[7].0.op,
        ToolOp::Command {
            cmd: Some("cargo test --lib".into()),
            workdir: None,
            background: false
        }
    );

    // Verify hunk fingerprints for write, single-hunk edit (oldText/newText), and multi-hunk edit (edits[]).
    let single_rebased_diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -12,1 +44,2 @@
-    let limit = 16;
+        let limit = compute_pi_limit(input);
+        assert!(limit > 0);
";
    assert_eq!(
        classified[5].1,
        extract_diff_hunk_hashes(single_rebased_diff),
        "Pi single edit newText hashes must match re-indented git diff"
    );
    assert!(classified[4].1.contains(&hash_normalized_line("0xdead_beef").unwrap()));
    assert!(classified[6]
        .1
        .contains(&hash_normalized_line("pub fn alpha_v3() -> bool {").unwrap()));
    assert!(classified[6]
        .1
        .contains(&hash_normalized_line("pub fn beta_v3() -> usize {").unwrap()));

    // Verify ToolHistogram stream vs whole-session parity.
    let hist_stream = cv_core::tools::ToolHistogram::from_stream(&adapter, r).unwrap();
    let hist_msgs = cv_core::tools::ToolHistogram::from_messages(spanned.0.iter(), s.cwd.as_deref()).unwrap();
    assert_eq!(
        serde_json::to_value(&hist_stream).unwrap(),
        serde_json::to_value(&hist_msgs).unwrap()
    );
    assert_eq!(hist_stream.total_calls, 8);
    assert_eq!(hist_stream.total_errors, 1);
    assert_eq!(hist_stream.tools["ls"].reads, 1);
    assert_eq!(hist_stream.tools["find"].reads, 1);
    assert_eq!(hist_stream.tools["grep"].reads, 1);
    assert_eq!(hist_stream.tools["read"].reads, 1);
    assert_eq!(hist_stream.tools["write"].edits, 1);
    assert_eq!(hist_stream.tools["edit"].edits, 2);
    assert_eq!(hist_stream.tools["bash"].commands, 1);
    assert_eq!(hist_stream.tools["bash"].errors, 1);

    // Verify end-to-end EventSink / SQLite catalog ingestion when sqlite is enabled.
    #[cfg(feature = "sqlite")]
    {
        let canonical_cwd = cv_core::events::absolutize(cwd.to_str().unwrap(), None);
        let n = cv_core::events::ingest_ref(r).unwrap();
        assert_eq!(n, 9, "8 tool calls + 1 error result");
        let rows = cv_core::events::events_for("pi", "pi-tools-v3", None).unwrap();
        assert_eq!(rows.len(), 9);
        let touched = cv_core::events::sessions_touching(&format!("{canonical_cwd}/src/lib.rs"), false).unwrap();
        assert_eq!(touched.len(), 1);
        assert_eq!((touched[0].reads, touched[0].edits), (1, 1));
        let hunks = cv_core::events::edit_hunks_for("pi", "pi-tools-v3").unwrap();
        assert_eq!(hunks.len(), 3, "write + 2 edit calls persist hunk hashes");
        assert!(hunks.iter().all(|(_, _, hashes)| !hashes.is_empty()));
    }
}
