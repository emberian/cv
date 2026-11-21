//! The Pi reader against real structure-only fixtures when present.
//!
//! The fixtures are three native Pi 1.0.2 sessions from a certify hour, every string replaced by
//! `<str:N>` except short enum-like fields. They are external qualification inputs (at
//! `/work/1000/pi-session-fixtures/`, or wherever `PI_FIXTURES` points). When `PI_FIXTURES` is
//! unset and the default qualification directory is absent, these tests skip cleanly so a bare
//! checkout passes; when `PI_FIXTURES` is explicitly set to a missing path, they fail loudly.

use cv_core::harness::pi::Pi;
use cv_core::harness::Adapter;
use cv_core::ir::Harness;
use cv_core::stream::{Demand, MessageSink};
use std::path::{Path, PathBuf};

const FIXTURES: &str = "/work/1000/pi-session-fixtures";

fn fixtures_dir() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PI_FIXTURES").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(explicit);
        assert!(
            dir.is_dir(),
            "PI_FIXTURES was set to {}, which is not a directory",
            dir.display()
        );
        return Some(dir);
    }
    let default = Path::new(FIXTURES);
    default.is_dir().then(|| default.to_path_buf())
}

fn fixture(name: &str) -> Option<PathBuf> {
    let dir = fixtures_dir()?;
    let p = dir.join(name);
    assert!(p.is_file(), "the fixture {} is required but absent", p.display());
    Some(p)
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
                cv_core::ir::Block::ToolResult { tool_use_id, .. } => self.tool_results.push(tool_use_id.clone()),
                _ => {}
            }
        }
        if let Some(kept) = bag.and_then(|b| b.get("first_kept_entry_id")).and_then(|v| v.as_str()) {
            self.compaction_kept.push(kept.to_string());
        }
        cv_core::stream::Flow::Continue
    }
}

fn read(name: &str) -> Option<Collect> {
    let path = fixture(name)?;
    let adapter = Pi::with_roots(vec![path.parent().unwrap().to_path_buf()]);
    let refs = adapter.discover().expect("discovery").refs;
    let r = refs.iter().find(|r| r.path == path).expect("the fixture is discovered");
    let mut sink = Collect::default();
    adapter.stream(r, &Demand::LEAN, &mut sink).expect("stream");
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
    Some(sink)
}

#[test]
fn a_session_without_compaction_reads_whole() {
    let Some(c) = read("c18-agent-0.session.struct.jsonl") else {
        return;
    };
    assert!(!c.kinds.is_empty(), "the session yields messages");
    assert!(c.compaction_kept.is_empty(), "agent 0 has no compaction");
    let mut calls = c.tool_calls.clone();
    let mut results = c.tool_results.clone();
    calls.sort();
    results.sort();
    assert_eq!(calls, results, "every tool call has a result with the same id");
}

#[test]
fn a_compacted_session_keeps_from_the_boundary() {
    let Some(c) = read("c18-agent-6.session.struct.jsonl") else {
        return;
    };
    assert_eq!(c.compaction_kept.len(), 1, "agent 6 has exactly one compaction");
    assert!(
        !c.compaction_kept[0].is_empty(),
        "the compaction names the first kept entry"
    );
    assert!(
        c.kinds.iter().any(|k| k == "CompactionBoundary"),
        "the compaction boundary is emitted"
    );
    assert!(
        c.kinds.iter().any(|k| k == "CompactionSummary"),
        "the compaction summary is emitted"
    );
    assert!(
        c.kinds.len() > 100,
        "the kept set is the compaction's kept span, not just the entries after it: got {} messages",
        c.kinds.len()
    );
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
    let (Some(a), Some(b)) = (
        read("c18-agent-6.session.struct.jsonl"),
        read("c18-agent-13.session.struct.jsonl"),
    ) else {
        return;
    };
    assert_eq!(
        a.compaction_kept.len(),
        b.compaction_kept.len(),
        "both have one compaction"
    );
    assert_eq!(
        a.kinds.iter().filter(|k| *k == "CompactionBoundary").count(),
        1,
        "one boundary per compaction"
    );
    assert_eq!(
        b.kinds.iter().filter(|k| *k == "CompactionBoundary").count(),
        1,
        "one boundary per compaction"
    );
}

#[test]
fn assistant_usage_is_mapped() {
    let Some(path) = fixture("c18-agent-0.session.struct.jsonl") else {
        return;
    };
    let mut want: Option<(u64, u64)> = None;
    for line in std::fs::read_to_string(&path).expect("readable").lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
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

#[test]
fn complete_emits_the_whole_chain_not_the_projection() {
    let Some(path) = fixture("c18-agent-6.session.struct.jsonl") else {
        return;
    };
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
    assert!(
        complete.kinds.len() >= entries,
        "complete emits at least one message per entry: {} messages for {} entries",
        complete.kinds.len(),
        entries
    );
}
