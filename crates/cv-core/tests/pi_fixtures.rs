//! The Pi reader against real structure-only fixtures.
//!
//! The fixtures are three native Pi 1.0.2 sessions from a certify hour, every string replaced by
//! `<str:N>` except short enum-like fields. They are NOT in this repository: they live at
//! `/work/1000/pi-session-fixtures/` and the test skips when that path is absent, so a checkout
//! without them still builds and passes.
//!
//! What is asserted is structure, never text: entry counts, the tree's integrity, tool pairing by
//! id, and the compaction projection (which entries the latest compaction kept).

use cv_core::harness::pi::Pi;
use cv_core::harness::Adapter;
use cv_core::ir::Harness;
use cv_core::stream::{Demand, MessageSink};
use std::path::PathBuf;

const FIXTURES: &str = "/work/1000/pi-session-fixtures";

/// The fixture directory, or a FAILURE when it is absent.
///
/// A test that returns early when its input is missing reports green while testing nothing, which is
/// the failure mode this file exists to avoid. The qualification job must set `PI_FIXTURES` to a
/// frozen input directory; a plain `cargo test` on a machine without the fixtures fails loudly here
/// rather than passing silently.
fn fixtures_dir() -> PathBuf {
    let dir = std::env::var_os("PI_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(FIXTURES));
    assert!(
        dir.is_dir(),
        "the Pi fixtures are required: set PI_FIXTURES to the frozen fixture directory (looked in {})",
        dir.display()
    );
    dir
}

fn fixture(name: &str) -> PathBuf {
    let p = fixtures_dir().join(name);
    assert!(p.is_file(), "the fixture {} is required but absent", p.display());
    p
}

/// A sink that keeps only what the assertions need: the message kinds and the tool ids.
#[derive(Default)]
struct Collect {
    kinds: Vec<String>,
    tool_calls: Vec<String>,
    tool_results: Vec<String>,
    compaction_kept: Vec<String>,
}

impl MessageSink for Collect {
    fn message(&mut self, m: cv_core::ir::Message) -> cv_core::stream::Flow {
        self.kinds.push(format!("{:?}", m.kind));
        let bag = m.harness_extra(Harness::Pi);
        for b in &m.content {
            match b {
                cv_core::ir::Block::ToolUse { id, .. } => self.tool_calls.push(id.clone()),
                // THE BLOCK'S OWN ID, not a Pi-only bag entry: this is what generic pairing and
                // every writer read, so the test must read it too.
                cv_core::ir::Block::ToolResult { tool_use_id, .. } => {
                    self.tool_results.push(tool_use_id.clone())
                }
                _ => {}
            }
        }
        if let Some(kept) = bag.and_then(|b| b.get("first_kept_entry_id")).and_then(|v| v.as_str()) {
            self.compaction_kept.push(kept.to_string());
        }
        cv_core::stream::Flow::Continue
    }
}

fn read(name: &str) -> Collect {
    let path = fixture(name);
    let adapter = Pi::with_roots(vec![path.parent().unwrap().to_path_buf()]);
    let refs = adapter.discover().expect("discovery").refs;
    let r = refs.iter().find(|r| r.path == path).expect("the fixture is discovered");
    let mut sink = Collect::default();
    adapter.stream(r, &Demand::LEAN, &mut sink).expect("stream");
    // WHAT WAS CONSUMED, so a green run says what it read rather than only that it passed.
    let entries = std::fs::read_to_string(&path)
        .expect("the fixture is readable")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    eprintln!(
        "pi fixture {}: {} entries on disk, {} messages emitted, {} tool calls, {} compactions",
        name,
        entries,
        sink.kinds.len(),
        sink.tool_calls.len(),
        sink.compaction_kept.len()
    );
    sink
}

#[test]
fn a_session_without_compaction_reads_whole() {
    let c = read("c18-agent-0.session.struct.jsonl");
    assert!(!c.kinds.is_empty(), "the session yields messages");
    assert!(c.compaction_kept.is_empty(), "agent 0 has no compaction");
    // tool calls and results pair by id, never by order
    let mut calls = c.tool_calls.clone();
    let mut results = c.tool_results.clone();
    calls.sort();
    results.sort();
    assert_eq!(calls, results, "every tool call has a result with the same id");
}

#[test]
fn a_compacted_session_keeps_from_the_boundary() {
    let c = read("c18-agent-6.session.struct.jsonl");
    assert_eq!(c.compaction_kept.len(), 1, "agent 6 has exactly one compaction");
    assert!(!c.compaction_kept[0].is_empty(), "the compaction names the first kept entry");
    // the projection starts at the compaction, so the boundary is the first message
    assert!(
        c.kinds.iter().any(|k| k == "CompactionBoundary"),
        "the compaction boundary is emitted"
    );
    assert!(
        c.kinds.iter().any(|k| k == "CompactionSummary"),
        "the compaction summary is emitted"
    );
    // THE PROJECTION'S SIZE, which is what a wrong boundary changes. The fixture has 318 entries;
    // the compaction sits at index 309 and names firstKeptEntryId at index 161, so the kept set is
    // 161..=309 - about 149 entries, not the 9 a boundary-at-the-compaction bug would keep.
    assert!(
        c.kinds.len() > 100,
        "the kept set is the compaction's kept span, not just the entries after it: got {} messages",
        c.kinds.len()
    );
    // THE ORDER, which is what a file-order emission gets wrong. Native yields the compaction FIRST,
    // then the kept pre-compaction entries, then the post-compaction ones. A file-order emission
    // would put the boundary near the END, after every kept entry.
    let boundary_at = c
        .kinds
        .iter()
        .position(|k| k == "CompactionBoundary")
        .expect("a boundary is emitted");
    assert_eq!(
        boundary_at, 0,
        "the compaction boundary is the FIRST message, not at position {boundary_at} (file order)"
    );
}

#[test]
fn both_compacted_fixtures_agree_on_shape() {
    let a = read("c18-agent-6.session.struct.jsonl");
    let b = read("c18-agent-13.session.struct.jsonl");
    assert_eq!(a.compaction_kept.len(), b.compaction_kept.len(), "both have one compaction");
    assert!(
        a.kinds.iter().filter(|k| *k == "CompactionBoundary").count() == 1,
        "one boundary per compaction"
    );
    assert!(
        b.kinds.iter().filter(|k| *k == "CompactionBoundary").count() == 1,
        "one boundary per compaction"
    );
}

/// The assistant turn's usage is mapped into the IR, not dropped. The fixture carries it in full;
/// this reads the first assistant message's usage from the file and asserts the emitted message
/// carries the same numbers.
#[test]
fn assistant_usage_is_mapped() {
    let path = fixture("c18-agent-0.session.struct.jsonl");
    // the first assistant message's native usage, read from the file itself
    let mut want: Option<(u64, u64)> = None;
    for line in std::fs::read_to_string(&path).expect("readable").lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let m = v.get("message").expect("a message");
        if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(u) = m.get("usage") {
            let i = u.get("input").and_then(|x| x.as_u64());
            let o = u.get("output").and_then(|x| x.as_u64());
            if let (Some(i), Some(o)) = (i, o) {
                want = Some((i, o));
                break;
            }
        }
    }
    let (want_in, want_out) = want.expect("the fixture has an assistant usage");

    let adapter = Pi::with_roots(vec![path.parent().unwrap().to_path_buf()]);
    let refs = adapter.discover().expect("discovery").refs;
    let r = refs.iter().find(|r| r.path == path).expect("discovered");
    let mut sink = UsageCollect::default();
    adapter.stream(r, &Demand::LEAN, &mut sink).expect("stream");
    let got = sink.first.expect("an assistant message carries usage");
    assert_eq!(got.0, Some(want_in), "input tokens are mapped");
    assert_eq!(got.1, Some(want_out), "output tokens are mapped");
}

/// Keeps the first usage it sees.
#[derive(Default)]
struct UsageCollect {
    first: Option<(Option<u64>, Option<u64>)>,
}

impl MessageSink for UsageCollect {
    fn message(&mut self, m: cv_core::ir::Message) -> cv_core::stream::Flow {
        if self.first.is_none() {
            if let Some(u) = &m.usage {
                self.first = Some((u.input_tokens, u.output_tokens));
            }
        }
        cv_core::stream::Flow::Continue
    }
}

#[test]
fn the_harness_is_pi() {
    let adapter = Pi::with_roots(vec![PathBuf::from("/nonexistent")]);
    assert_eq!(adapter.harness(), Harness::Pi);
}

/// `complete` must not drop history. The projection deliberately drops the entries before the
/// compaction's kept boundary; a full-history search or a full-graph no-op writer needs them, so
/// under `complete` the whole chain is emitted and the count matches the file's entry count.
#[test]
fn complete_emits_the_whole_chain_not_the_projection() {
    let path = fixture("c18-agent-6.session.struct.jsonl");
    let entries = std::fs::read_to_string(&path)
        .expect("the fixture is readable")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    let adapter = Pi::with_roots(vec![path.parent().unwrap().to_path_buf()]);
    let refs = adapter.discover().expect("discovery").refs;
    let r = refs.iter().find(|r| r.path == path).expect("the fixture is discovered");

    let mut projected = Collect::default();
    adapter.stream(r, &Demand::LEAN, &mut projected).expect("stream");
    let mut complete = Collect::default();
    adapter
        .stream(r, &Demand::COMPLETE, &mut complete)
        .expect("stream complete");

    assert!(
        complete.kinds.len() > projected.kinds.len(),
        "complete emits more than the projection: {} vs {}",
        complete.kinds.len(),
        projected.kinds.len()
    );
    // every entry contributes at least one message under complete (a compaction contributes two)
    assert!(
        complete.kinds.len() >= entries,
        "complete emits at least one message per entry: {} messages for {} entries",
        complete.kinds.len(),
        entries
    );
}
