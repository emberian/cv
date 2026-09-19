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
//! message spans — `cv compaction <session>` lists them; `cv show <session> --pre-compaction` reads
//! the span before the first boundary.
//!
//! Memory discipline (the project value): detection rides the streaming parse through a
//! [`CompactionSink`]; it keeps only small per-boundary rows + (optionally) the summary text, never
//! the transcript. The summary itself is bounded (a few KB) so it's resolved eagerly.

use crate::ir::{Block, Message, Role, Session, SessionRef};
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
/// `keep_summaries` controls whether the (bounded) summary text is retained — `cv compaction` keeps
/// it; a count-only probe need not. Holds only the small [`Compaction`] rows.
pub struct CompactionSink {
    keep_summaries: bool,
    msg_idx: usize,
    boundaries: Vec<Compaction>,
    /// Pending boundaries whose summary hasn't streamed past yet, keyed by boundary uuid → index
    /// into `boundaries`.
    pending: std::collections::HashMap<String, usize>,
    /// Resolver for a lazily-spanned summary body (set on the streaming path; `None` for the
    /// already-materialized in-session path). A compaction summary can exceed the inline threshold.
    resolver: Option<crate::lazy::Resolver>,
}

impl CompactionSink {
    pub fn new(keep_summaries: bool) -> Self {
        CompactionSink {
            keep_summaries,
            msg_idx: 0,
            boundaries: Vec::new(),
            pending: std::collections::HashMap::new(),
            resolver: None,
        }
    }

    pub fn into_boundaries(self) -> Vec<Compaction> {
        self.boundaries
    }
}

impl MessageSink for CompactionSink {
    fn message(&mut self, m: Message) -> Flow {
        let idx = self.msg_idx;
        self.msg_idx += 1;

        // A compaction boundary: a System message whose `subtype` is `compact_boundary`.
        if m.role == Role::System && m.extra.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            let meta = m.extra.get("compactMetadata");
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
            return Flow::Continue;
        }

        // The paired summary: an `isCompactSummary` message whose `parentUuid` is a pending
        // boundary's uuid. (Falls back to "the most recent unpaired boundary" if parentUuid is
        // absent — older transcripts linked only by adjacency.)
        let is_summary = m.extra.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
        if is_summary {
            let slot = m.parent_id.as_deref().and_then(|p| self.pending.remove(p)).or_else(|| {
                // No/unknown parent link: attach to the latest boundary still awaiting a summary.
                let last = self.boundaries.iter().rposition(|c| c.summary_msg_idx.is_none())?;
                // Drop any stale pending entry for it so it isn't double-claimed.
                self.pending.retain(|_, v| *v != last);
                Some(last)
            });
            if let Some(i) = slot {
                self.boundaries[i].summary_msg_idx = Some(idx);
                if self.keep_summaries {
                    self.boundaries[i].summary = summary_text(&m, self.resolver.as_ref());
                }
            }
        }
        Flow::Continue
    }
}

/// The summary body of an `isCompactSummary` message: its concatenated text blocks, resolving a
/// lazy span (a long summary can exceed the inline threshold) against `resolver` when present.
fn summary_text(m: &Message, resolver: Option<&crate::lazy::Resolver>) -> Option<String> {
    let mut s = String::new();
    for b in &m.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            match resolver {
                Some(r) => s.push_str(&text.resolve(r)),
                None => s.push_str(text), // already-materialized (in-session) path
            }
        }
    }
    (!s.trim().is_empty()).then_some(s)
}

/// Detect every compaction in a session by streaming it (memory-safe on a multi-GB transcript).
/// `keep_summaries` retains each boundary's (bounded) summary text. Best-effort: a parse error
/// yields whatever boundaries were seen before it.
pub fn detect(r: &SessionRef, keep_summaries: bool) -> anyhow::Result<Vec<Compaction>> {
    let adapter =
        crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
    let mut sink = CompactionSink::new(keep_summaries);
    sink.resolver = Some(crate::lazy::Resolver::new(Some(r.path.clone())));
    // `lazy_extra`: the `subtype`/`compactMetadata`/`isCompactSummary` keys this detector keys on
    // live in `extra` — plain `lazy()` omits them. Giant message bodies still stay lazy on disk.
    adapter.stream(r, &crate::stream::ParseOptions::lazy_extra(), &mut sink)?;
    Ok(sink.into_boundaries())
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

/// A [`Session`]-level convenience (used by tests / whole-session consumers): detect compactions
/// from an already-parsed session's messages, without re-streaming. Mirrors [`CompactionSink`]'s
/// logic on `session.messages`.
pub fn detect_in_session(session: &Session, keep_summaries: bool) -> Vec<Compaction> {
    let mut sink = CompactionSink::new(keep_summaries);
    for m in &session.messages {
        let _ = sink.message(m.clone());
    }
    sink.into_boundaries()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;

    /// Build a system compact_boundary message with the given uuid + metadata.
    fn boundary(uuid: &str, trigger: &str, pre: u64) -> Message {
        let mut m = Message::new(Role::System);
        m.id = Some(uuid.to_string());
        m.content = vec![Block::Text {
            text: "Conversation compacted".into(),
        }];
        m.extra
            .insert("subtype".into(), Value::String("compact_boundary".into()));
        m.extra.insert(
            "compactMetadata".into(),
            serde_json::json!({"trigger": trigger, "preTokens": pre, "durationMs": 1234}),
        );
        m
    }

    /// Build an isCompactSummary user message whose parentUuid links to `parent`.
    fn summary(parent: &str, body: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.parent_id = Some(parent.to_string());
        m.content = vec![Block::Text { text: body.into() }];
        m.extra.insert("isCompactSummary".into(), Value::Bool(true));
        m
    }

    fn plain_user(body: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.content = vec![Block::Text { text: body.into() }];
        m
    }

    #[test]
    fn detects_boundaries_and_pairs_summaries_by_parent_uuid() {
        let session = Session {
            id: "s".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            source_path: None,
            extra: Default::default(),
            messages: vec![
                plain_user("hello"),                           // 0
                boundary("b1", "manual", 989063),              // 1
                summary("b1", "SUMMARY ONE covers the start"), // 2
                plain_user("more work"),                       // 3
                boundary("b2", "auto", 500000),                // 4
                summary("b2", "SUMMARY TWO"),                  // 5
                plain_user("after"),                           // 6
            ],
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };
        let comps = detect_in_session(&session, true);
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
            id: "s".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            source_path: None,
            extra: Default::default(),
            messages: vec![boundary("bx", "manual", 100), sum],
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };
        let comps = detect_in_session(&session, true);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(1));
        assert_eq!(comps[0].summary.as_deref(), Some("ADJACENT SUMMARY"));
    }

    #[test]
    fn keep_summaries_false_detects_but_drops_text() {
        let session = Session {
            id: "s".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            source_path: None,
            extra: Default::default(),
            messages: vec![boundary("b", "manual", 100), summary("b", "text")],
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };
        let comps = detect_in_session(&session, false);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(1));
        assert_eq!(comps[0].summary, None, "count-only mode keeps no summary text");
    }
}
