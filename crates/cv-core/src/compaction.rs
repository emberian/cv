//! Compaction detection: where a session's context window was rebuilt, and the summary that
//! carried it across.
//!
//! When a Claude Code session runs out of context (manually via `/compact`, or automatically), the
//! harness compacts: it writes a `system` record `subtype:"compact_boundary"` (carrying
//! `compactMetadata` — `trigger`, `preTokens`, `durationMs`, the preserved-segment uuids) and then
//! seeds the *next* context window with a single `user` message flagged `isCompactSummary:true`
//! whose body is the generated summary of everything before. The summary's `parentUuid` points at
//! the boundary's `uuid`, so the pair is linked unambiguously.
//!
//! This is **load-bearing**: the compaction summary is how a continued agent retrieves the
//! pre-compaction context it can no longer see. A session can compact many times (the reference
//! transcript: 11), so cv surfaces every boundary, the summary text, and the pre/post-compaction
//! message spans — `cv show <session> --kind compaction_boundary` lists them; `cv show <session> --pre-compaction` reads
//! the span before the first boundary.
//!
//! Memory discipline (the project value): detection rides the streaming parse through a
//! [`CompactionSink`]; it keeps only small per-boundary rows + (optionally) the summary text, never
//! the transcript. Requested full summaries are retained as output and may be arbitrarily large.

use crate::ir::{Block, Message, MessageKind, SessionRef};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use crate::stream::{Flow, MessageSink};
use serde_json::Value;

/// One compaction boundary in a session: where the window was rebuilt, why, and the summary that
/// seeded the new window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Compaction {
    /// 0-based index (in transcript message-stream order) of the `compact_boundary` record.
    pub boundary_msg_idx: usize,
    /// The boundary record's `uuid` (what the summary's `parentUuid` references).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_uuid: Option<String>,
    /// 0-based index of the `isCompactSummary` summary message that follows, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_msg_idx: Option<usize>,
    /// `compactMetadata.trigger`: `manual` (`/compact`) or `auto` (ran out of context).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// `compactMetadata.preTokens`: the context size that triggered the compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_tokens: Option<u64>,
    /// `compactMetadata.durationMs`: how long the compaction itself took.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The generated summary text (the seed of the post-compaction window). Resolved when the
    /// detector was asked to keep summaries; otherwise `None` (boundaries still detected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl Compaction {
    /// A short one-line descriptor for listings: `#N · trigger · preTokens · summary-head`.
    pub fn headline(&self, n: usize) -> String {
        let trig = self.trigger.as_deref().unwrap_or("?");
        let pre = self
            .pre_tokens
            .map(|t| format!("{}k ctx", t / 1000))
            .unwrap_or_else(|| "? ctx".into());
        format!("compaction #{n} · {trig} · {pre} · @msg {}", self.boundary_msg_idx)
    }
}

/// A [`MessageSink`] that detects every compaction boundary as the transcript streams past, pairing
/// each `compact_boundary` with its `isCompactSummary` summary (by `parentUuid == boundary.uuid`).
///
/// `keep_summaries` controls whether full summary text is retained — `cv show --kind compaction_*`
/// keeps it; a count-only probe need not. Requested summaries may exceed the boundary metadata.
pub struct CompactionSink {
    keep_summaries: bool,
    summary_preview: Option<usize>,
    msg_idx: usize,
    boundaries: Vec<Compaction>,
    /// Pending boundaries whose summary hasn't streamed past yet, keyed by boundary uuid → index
    /// into `boundaries`.
    pending: std::collections::HashMap<String, usize>,
    unpaired: std::collections::BTreeSet<usize>,
    /// Resolver for a lazily-spanned summary body (set on the streaming path; `None` for the
    /// already-materialized in-session path). A compaction summary can exceed the inline threshold.
    resolver: Option<crate::lazy::Resolver>,
    /// The scan's first failure, kept whole until someone is handed it: `try_message` returns it
    /// to the driver, `message` (which cannot) leaves it for `into_boundaries`.
    error: Option<anyhow::Error>,
    /// The scan failed, whether or not `error` has been handed out yet; every later call refuses.
    failed: bool,
}

impl CompactionSink {
    pub fn new(keep_summaries: bool) -> Self {
        CompactionSink {
            keep_summaries,
            summary_preview: None,
            msg_idx: 0,
            boundaries: Vec::new(),
            pending: std::collections::HashMap::new(),
            unpaired: std::collections::BTreeSet::new(),
            resolver: None,
            error: None,
            failed: false,
        }
    }

    #[cfg(feature = "sqlite")]
    pub(crate) fn with_source(mut self, path: &std::path::Path) -> Self {
        self.resolver = Some(crate::Resolver::new(Some(path.to_owned())));
        self
    }

    /// Finish a scan, rejecting partial results when a demanded summary failed.
    pub fn into_boundaries(self) -> anyhow::Result<Vec<Compaction>> {
        match (self.error, self.failed) {
            (Some(error), _) => Err(error),
            (None, true) => Err(anyhow::anyhow!(
                "compaction scan failed (its error was returned to the stream)"
            )),
            (None, false) => Ok(self.boundaries),
        }
    }
}

/// A harness-specific fact by key, searching every harness bag in `extra` (a compaction sink sees
/// messages from any harness and does not know which wrote them).
fn bag_value<'a>(m: &'a Message, key: &str) -> Option<&'a Value> {
    m.extra
        .values()
        .filter_map(Value::as_object)
        .find_map(|bag| bag.get(key))
}

impl MessageSink for CompactionSink {
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        self.observe(m).unwrap_or_else(|error| {
            self.failed = true;
            self.error = Some(error);
            Flow::Stop
        })
    }

    fn try_message(&mut self, m: Message) -> anyhow::Result<Flow> {
        if !self.failed {
            return self.observe(m).inspect_err(|_| self.failed = true);
        }
        Err(self
            .error
            .take()
            .unwrap_or_else(|| anyhow::anyhow!("compaction scan has already failed")))
    }
}

impl CompactionSink {
    fn observe(&mut self, m: Message) -> anyhow::Result<Flow> {
        let idx = self.msg_idx;
        self.msg_idx += 1;

        // A compaction boundary is a `kind`, whatever the harness. Its metadata (trigger, tokens
        // before, duration) is harness-specific and lives in that harness's `extra` bag.
        if m.kind == MessageKind::CompactionBoundary {
            let meta = bag_value(&m, "compactMetadata").or_else(|| bag_value(&m, "compact_metadata"));
            let comp = Compaction {
                boundary_msg_idx: idx,
                boundary_uuid: m.id.clone(),
                summary_msg_idx: None,
                trigger: meta
                    .and_then(|v| v.get("trigger"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                pre_tokens: meta.and_then(|v| v.get("preTokens")).and_then(Value::as_u64),
                duration_ms: meta.and_then(|v| v.get("durationMs")).and_then(Value::as_u64),
                summary: None,
            };
            if let Some(uuid) = &m.id {
                self.pending.insert(uuid.clone(), self.boundaries.len());
            }
            self.boundaries.push(comp);
            self.unpaired.insert(self.boundaries.len() - 1);
            return Ok(Flow::Continue);
        }

        // The paired summary: a `CompactionSummary` whose `parent_id` is a pending boundary's uuid.
        // (Falls back to "the most recent unpaired boundary" if the parent link is absent — older
        // transcripts linked only by adjacency.)
        let is_summary = m.kind == MessageKind::CompactionSummary;
        if is_summary {
            let slot = m.parent_id.as_deref().and_then(|p| self.pending.remove(p)).or_else(|| {
                // No/unknown parent link: attach to the latest boundary still awaiting a summary.
                let last = self.unpaired.pop_last()?;
                if let Some(id) = self.boundaries[last].boundary_uuid.as_ref() {
                    if self.pending.get(id) == Some(&last) {
                        self.pending.remove(id);
                    }
                }
                Some(last)
            });
            if let Some(i) = slot {
                self.unpaired.remove(&i);
                self.boundaries[i].summary_msg_idx = Some(idx);
                if self.keep_summaries {
                    self.boundaries[i].summary = summary_text(&m, self.resolver.as_ref(), self.summary_preview)?;
                }
            }
        }
        Ok(Flow::Continue)
    }
}

/// The summary body of an `isCompactSummary` message: its concatenated text blocks, resolving a
/// lazy span (a long summary can exceed the inline threshold) against `resolver` when present.
fn summary_text(
    m: &Message,
    resolver: Option<&crate::lazy::Resolver>,
    preview: Option<usize>,
) -> anyhow::Result<Option<String>> {
    let fallback = crate::lazy::Resolver::new(None);
    let resolver = resolver.unwrap_or(&fallback);
    let mut s = String::new();
    let mut prefix = preview.map(crate::projection::Head::bounded);
    let mut nonempty = false;
    let mut nonblank = false;
    for b in &m.content {
        if let Block::Text { text } = b {
            if nonempty {
                if let Some(prefix) = &mut prefix {
                    prefix.push("\n");
                } else {
                    s.push('\n');
                }
            }
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                nonempty |= !part.is_empty();
                nonblank |= !part.trim().is_empty();
                if let Some(prefix) = &mut prefix {
                    prefix.push(part);
                } else {
                    s.push_str(part);
                }
                Ok(())
            })?;
        }
    }
    Ok(nonblank.then(|| prefix.map_or(s, |prefix| prefix.finish())))
}

/// Detect every compaction by streaming, retaining boundary rows and requested summaries.
/// `keep_summaries` retains each boundary's full summary text as requested output. Parse errors
/// and demanded summary read errors are returned; partial results are not published.
pub fn detect(r: &SessionRef, keep_summaries: bool) -> anyhow::Result<Vec<Compaction>> {
    detect_with_preview(r, keep_summaries, None)
}

/// Detect boundaries with bounded summary previews matching `truncate(full_summary, max_chars)`.
/// All demanded summary chunks are checked, including a blank prefix with nonblank text later.
pub fn detect_preview(r: &SessionRef, max_chars: usize) -> anyhow::Result<Vec<Compaction>> {
    detect_with_preview(r, true, Some(max_chars))
}

fn detect_with_preview(
    r: &SessionRef,
    keep_summaries: bool,
    preview: Option<usize>,
) -> anyhow::Result<Vec<Compaction>> {
    let adapter =
        crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
    let mut sink = CompactionSink::new(keep_summaries);
    sink.summary_preview = preview;
    sink.resolver = Some(crate::lazy::Resolver::new(Some(r.path.clone())));
    // Boundary kinds and compactMetadata are reducer facts; summary bodies remain lazy until
    // requested. Adapters without a Facts projection conservatively retain their existing extra.
    adapter.try_stream(r, &crate::stream::Demand::FACTS.with_spans(), &mut sink)?;
    sink.into_boundaries()
}

/// The message-index span *before* the Nth compaction boundary (0-based `n`): `[start, boundary)`
/// where `start` is the message just after the previous boundary's summary (or 0 for the first).
/// This is the pre-compaction context a continued agent lost — readable via
/// `cv show <id> --range start-boundary`. `None` if there are fewer than `n+1` boundaries.
pub fn pre_compaction_span(boundaries: &[Compaction], n: usize) -> Option<(usize, usize)> {
    let b = boundaries.get(n)?;
    let start = if n == 0 {
        0
    } else {
        // After the previous boundary's summary (if any), else after the previous boundary itself.
        let prev = &boundaries[n - 1];
        prev.summary_msg_idx.map(|s| s + 1).unwrap_or(prev.boundary_msg_idx + 1)
    };
    Some((start.min(b.boundary_msg_idx), b.boundary_msg_idx))
}

/// The Nth boundary as `cv show --kind compaction_boundary --json` and `GET /api/session/{h}/{id}/compactions` emit it:
/// the [`Compaction`] struct's own serialization plus `pre_compaction_span` under that one name,
/// so the same fact never has two spellings depending on the door.
pub fn row(boundaries: &[Compaction], n: usize) -> Value {
    let mut v = serde_json::to_value(&boundaries[n]).expect("Compaction serializes to an object");
    if let Some(obj) = v.as_object_mut() {
        if let Some((s, e)) = pre_compaction_span(boundaries, n) {
            obj.insert("pre_compaction_span".into(), serde_json::json!([s, e]));
        }
    }
    v
}

/// Test convenience: detect compactions
/// from an already-parsed session's messages, without re-streaming. Mirrors [`CompactionSink`]'s
/// logic on `session.messages`, resolving retained lazy text against `session.source_path` and
/// returning demanded summary read errors.
pub fn detect_in_session(session: &crate::ir::Session, keep_summaries: bool) -> anyhow::Result<Vec<Compaction>> {
    let mut sink = CompactionSink::new(keep_summaries);
    sink.resolver = Some(crate::lazy::Resolver::new(session.source_path.clone()));
    for m in &session.messages {
        sink.try_message(m.clone())?;
    }
    sink.into_boundaries()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;

    #[test]
    fn bounded_summary_preview_matches_full_text_for_blank_and_multiblock_bodies() {
        for body in ["", "   ", "\nλ短\nsecond", "                                  late"] {
            let mut message = summary("b", body);
            message.content.push(Block::Text {
                text: "\ntrailing".into(),
            });
            let full = summary_text(&message, None, None).unwrap();
            for cap in [0, 1, 8, 200] {
                assert_eq!(
                    summary_text(&message, None, Some(cap)).unwrap(),
                    full.as_ref().map(|text| crate::ir::truncate(text, cap))
                );
            }
        }
    }

    use crate::ir::{Harness, Origin, Role};

    /// Build a compaction-boundary message (Claude shape) with the given uuid + metadata.
    fn boundary(uuid: &str, trigger: &str, pre: u64) -> Message {
        let mut m = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
        m.id = Some(uuid.to_string());
        m.content = vec![Block::Text {
            text: "Conversation compacted".into(),
        }];
        m.harness_extra_mut(Harness::Claude).insert(
            "compactMetadata".into(),
            serde_json::json!({"trigger": trigger, "preTokens": pre, "durationMs": 1234}),
        );
        m
    }

    /// Build a compaction-summary message whose parent links to `parent`.
    fn summary(parent: &str, body: &str) -> Message {
        let mut m = Message::of_kind(Role::User, MessageKind::CompactionSummary, Origin::Harness);
        m.parent_id = Some(parent.to_string());
        m.content = vec![Block::Text { text: body.into() }];
        m
    }

    fn plain_user(body: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.content = vec![Block::Text { text: body.into() }];
        m
    }

    #[test]
    fn streamed_facts_keep_boundary_metadata_and_requested_summary() {
        let world = crate::test_world("compaction-facts-detect");
        let path = world.base.join("session.jsonl");
        let body = "full summary🌙\n".repeat(8192);
        let lines = [
            serde_json::json!({"type":"system","uuid":"boundary","subtype":"compact_boundary","compactMetadata":{"trigger":"manual","preTokens":42,"durationMs":1234}}),
            serde_json::json!({"type":"user","uuid":"summary","parentUuid":"boundary","isCompactSummary":true,"message":{"role":"user","content":[{"type":"text","text":body}]}}),
        ];
        std::fs::write(
            &path,
            lines
                .iter()
                .map(|line| serde_json::to_string(line).unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let reference = SessionRef::new("facts-compaction", Harness::Claude, &path);
        for keep in [false, true] {
            let found = detect(&reference, keep).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].trigger.as_deref(), Some("manual"));
            assert_eq!(found[0].pre_tokens, Some(42));
            assert_eq!(found[0].duration_ms, Some(1234));
            assert_eq!(found[0].summary_msg_idx, Some(1));
            assert_eq!(found[0].summary.as_deref(), keep.then_some(body.as_str()));
        }
    }

    #[test]
    fn full_summaries_match_inline_span_and_source_without_truncation() {
        let world = crate::test_world("compaction-checked-summary");
        let path = world.base.join("summary.json");
        for body in [" \n\t".repeat(9000), "🌙\n\"\\".repeat(32768)] {
            let wire = serde_json::to_vec(&body).unwrap();
            std::fs::write(&path, &wire).unwrap();
            let source = crate::Json::source(&path, 0, wire.len() as u64)
                .unwrap()
                .with_view(|view| view.source_text())
                .unwrap()
                .unwrap();
            let span = crate::lazy::Text::Span(crate::lazy::Span {
                source: Some(path.clone()),
                offset: 1,
                len: wire.len() as u64 - 2,
                escaped: true,
            });
            let make = |text| {
                let mut session = Session::new("summary", Harness::Claude);
                let mut message = summary("b", "");
                message.content.push(Block::Text { text });
                message.content.push(Block::Thinking {
                    text: "ignored".into(),
                    signature: None,
                    encrypted: None,
                    redacted: false,
                });
                message.content.push(Block::Text { text: "".into() });
                session.messages = vec![boundary("b", "auto", 42), message];
                session
            };
            let inline = detect_in_session(&make(body.clone().into()), true).unwrap();
            for text in [span, source] {
                let lazy = detect_in_session(&make(text), true).unwrap();
                assert_eq!(
                    serde_json::to_value(lazy).unwrap(),
                    serde_json::to_value(&inline).unwrap()
                );
            }
            if !body.trim().is_empty() {
                assert_eq!(inline[0].summary.as_deref(), Some(format!("{body}\n").as_str()));
                assert!(inline[0].summary.as_ref().unwrap().len() > 128 * 1024);
            } else {
                assert!(inline[0].summary.is_none());
            }
        }
    }

    #[test]
    fn demanded_summary_failure_stops_and_rejects_partial_boundaries() {
        let world = crate::test_world("compaction-summary-mutation");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot provenance").unwrap();
        let capture = crate::json::JsonSource::capture(&path).unwrap();
        let backing = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let wire = serde_json::to_vec(&"summary🌙".repeat(8192)).unwrap();
        let (offset, len) = backing
            .append(|out| {
                out.write_all(&wire)?;
                Ok(())
            })
            .unwrap();
        let text = capture
            .read(ReadInput::Backing(backing, offset, len), Validation::Value, |view| {
                view.source_text()
            })
            .unwrap()
            .unwrap();
        let mut message = summary("b", "");
        message.content = vec![Block::Text { text }];
        let mut session = Session::new("source-summary", Harness::Claude);
        session.messages = vec![boundary("b", "auto", 42), message.clone()];
        std::fs::write(&path, "changed snapshot").unwrap();
        let mut preview_message = summary("b", &"visible".repeat(100));
        preview_message.content.push(session.messages[1].content[0].clone());
        assert!(
            summary_text(&preview_message, None, Some(8)).is_err(),
            "a complete preview must still reject a later demanded source failure"
        );
        assert!(detect_in_session(&session, true).is_err());
        let count_only = detect_in_session(&session, false).unwrap();
        assert_eq!(count_only[0].summary_msg_idx, Some(1));
        assert!(count_only[0].summary.is_none());
        let mut sink = CompactionSink::new(true);
        assert!(matches!(sink.message(session.messages[0].clone()), Flow::Continue));
        assert!(matches!(sink.message(message.clone()), Flow::Stop));
        assert!(matches!(sink.message(plain_user("after failure")), Flow::Stop));
        assert!(sink.into_boundaries().is_err());
        let mut sink = CompactionSink::new(true);
        sink.try_message(session.messages[0].clone()).unwrap();
        assert!(sink.try_message(message).is_err());
        assert!(sink.try_message(plain_user("after failure")).is_err());
        assert!(sink.into_boundaries().is_err());
        let missing = crate::lazy::Text::Span(crate::lazy::Span {
            source: Some(world.base.join("missing")),
            offset: 0,
            len: 8192,
            escaped: true,
        });
        if let Block::Text { text } = &mut session.messages[1].content[0] {
            *text = missing;
        }
        let mut preview_message = summary("b", &"visible".repeat(100));
        preview_message.content.push(session.messages[1].content[0].clone());
        assert!(
            summary_text(&preview_message, None, Some(8)).is_err(),
            "a complete preview must still reject a later demanded source failure"
        );
        assert!(detect_in_session(&session, true).is_err());
        assert_eq!(detect_in_session(&session, false).unwrap().len(), 1);
        // The driver is handed the failure itself (here an io::Error under context), not a
        // flattened copy that has lost its cause.
        if let Block::Text { text } = &mut session.messages[1].content[0] {
            *text = crate::lazy::Text::Span(crate::lazy::Span {
                source: Some(world.base.join("missing")),
                offset: 1,
                len: 8192,
                escaped: true,
            });
        }
        let mut sink = CompactionSink::new(true);
        sink.try_message(session.messages[0].clone()).unwrap();
        let streamed = sink.try_message(session.messages[1].clone()).unwrap_err();
        assert!(
            streamed.root_cause().downcast_ref::<std::io::Error>().is_some(),
            "{streamed:?}"
        );
        assert!(sink.into_boundaries().is_err());
    }

    #[test]
    fn detects_boundaries_and_pairs_summaries_by_parent_uuid() {
        let session = Session {
            messages: vec![
                plain_user("hello"),                           // 0
                boundary("b1", "manual", 989063),              // 1
                summary("b1", "SUMMARY ONE covers the start"), // 2
                plain_user("more work"),                       // 3
                boundary("b2", "auto", 500000),                // 4
                summary("b2", "SUMMARY TWO"),                  // 5
                plain_user("after"),                           // 6
            ],
            ..Session::new("s", Harness::Claude)
        };
        let comps = detect_in_session(&session, true).unwrap();
        assert_eq!(comps.len(), 2, "two boundaries");

        assert_eq!(comps[0].boundary_msg_idx, 1);
        assert_eq!(comps[0].boundary_uuid.as_deref(), Some("b1"));
        assert_eq!(comps[0].summary_msg_idx, Some(2));
        assert_eq!(comps[0].trigger.as_deref(), Some("manual"));
        assert_eq!(comps[0].pre_tokens, Some(989063));
        assert_eq!(comps[0].duration_ms, Some(1234));
        assert_eq!(comps[0].summary.as_deref(), Some("SUMMARY ONE covers the start"));

        assert_eq!(comps[1].boundary_msg_idx, 4);
        assert_eq!(comps[1].trigger.as_deref(), Some("auto"));
        assert_eq!(comps[1].summary_msg_idx, Some(5));

        // Pre-compaction spans: first is [0,1); second is after the first summary, [3,4).
        assert_eq!(pre_compaction_span(&comps, 0), Some((0, 1)));
        assert_eq!(pre_compaction_span(&comps, 1), Some((3, 4)));
        assert_eq!(pre_compaction_span(&comps, 2), None);
    }

    #[test]
    fn pairs_by_adjacency_when_parent_uuid_missing() {
        // Older transcripts: the summary carries no parentUuid — fall back to the latest unpaired
        // boundary.
        let mut sum = summary("ignored", "ADJACENT SUMMARY");
        sum.parent_id = None;
        let session = Session {
            messages: vec![boundary("bx", "manual", 100), sum],
            ..Session::new("s", Harness::Claude)
        };
        let comps = detect_in_session(&session, true).unwrap();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(1));
        assert_eq!(comps[0].summary.as_deref(), Some("ADJACENT SUMMARY"));
    }

    #[test]
    fn keep_summaries_false_detects_but_drops_text() {
        let session = Session {
            messages: vec![boundary("b", "manual", 100), summary("b", "text")],
            ..Session::new("s", Harness::Claude)
        };
        let comps = detect_in_session(&session, false).unwrap();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(1));
        assert_eq!(comps[0].summary, None, "count-only mode keeps no summary text");
    }
    #[test]
    fn queued_unlinked_summaries_pair_latest_once_and_preserve_duplicate_id_links() {
        let mut sink = CompactionSink::new(false);
        for i in 0..1000 {
            sink.try_message(boundary(&format!("b{i}"), "auto", 100)).unwrap();
        }
        for _ in 0..1000 {
            sink.try_message(summary("unknown", "summary")).unwrap();
        }
        assert!(sink.pending.is_empty() && sink.unpaired.is_empty());
        let rows = sink.into_boundaries().unwrap();
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.summary_msg_idx, Some(1999 - i));
        }
        let mut sink = CompactionSink::new(false);
        sink.try_message(boundary("duplicate", "auto", 100)).unwrap();
        sink.try_message(boundary("duplicate", "manual", 200)).unwrap();
        sink.try_message(summary("duplicate", "newest")).unwrap();
        sink.try_message(summary("unknown", "oldest")).unwrap();
        let rows = sink.into_boundaries().unwrap();
        assert_eq!(rows[0].summary_msg_idx, Some(3));
        assert_eq!(rows[1].summary_msg_idx, Some(2));
    }
}
