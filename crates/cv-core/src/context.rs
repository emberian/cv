//! A small context-usage timeline over existing transcript records. No bodies are retained and
//! no token estimates are computed. Adapters normalize usage and suppress stale cumulative
//! snapshots; the same response-key rule as `stats` suppresses split response records.

use crate::ir::{Harness, Message, MessageKind, SessionRef, Usage};
use crate::stream::{Demand, Flow, MessageSink, Tier};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize)]
pub struct ContextRow {
    /// Original transcript index, suitable for `show --around`.
    pub index: usize,
    pub timestamp: Option<DateTime<Utc>>,
    pub kind: &'static str,
    pub model: Option<String>,
    /// The normalized disjoint usage components; absent values are not manufactured as zeroes.
    pub usage: Option<Usage>,
    /// `recorded` means saved usage, not independently measured request size. `estimated` marks
    /// the saved estimate written by `prune --revive`.
    pub usage_basis: Option<&'static str>,
    /// Sum of the recorded prompt components, absent if none of them was recorded.
    pub prompt_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub pre_compaction_tokens: Option<u64>,
    pub trigger: Option<String>,
}

/// Projected row ranges, as for other `show --kind` views. `around` is an original message
/// index and a count of context rows on each side, not a row number.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextWindow {
    pub start: usize,
    pub end: Option<usize>,
    pub last: Option<usize>,
    pub around: Option<(usize, usize)>,
}

pub fn window(rows: Vec<ContextRow>, selection: ContextWindow) -> Vec<ContextRow> {
    let (start, end) = if let Some((index, context)) = selection.around {
        let first = rows.partition_point(|row| row.index < index);
        let after = rows.partition_point(|row| row.index <= index);
        (
            first.saturating_sub(context),
            after.saturating_add(context).min(rows.len()),
        )
    } else if let Some(last) = selection.last {
        (rows.len().saturating_sub(last), rows.len())
    } else {
        (
            selection.start.min(rows.len()),
            selection.end.unwrap_or(rows.len()).min(rows.len()),
        )
    };
    rows.into_iter().skip(start).take(end.saturating_sub(start)).collect()
}

fn bag_value<'a>(message: &'a Message, key: &str) -> Option<&'a Value> {
    message
        .extra
        .values()
        .filter_map(Value::as_object)
        .find_map(|bag| bag.get(key))
}

struct ContextSink {
    harness: Harness,
    index: usize,
    rows: Vec<ContextRow>,
    seen: HashSet<String>,
    last_limit: Option<u64>,
}

impl ContextSink {
    fn new(harness: Harness) -> Self {
        Self {
            harness,
            index: 0,
            rows: Vec::new(),
            seen: HashSet::new(),
            last_limit: None,
        }
    }
}

impl MessageSink for ContextSink {
    fn tier(&self) -> Tier {
        Tier::Facts
    }

    fn message(&mut self, message: Message) -> Flow {
        let index = self.index;
        self.index += 1;
        let limit = bag_value(&message, "model_context_window").and_then(Value::as_u64);
        let limit_changed = limit.is_some() && limit != self.last_limit;
        if limit.is_some() {
            self.last_limit = limit;
        }
        let mut usage = message.usage.clone().filter(|usage| !usage.is_empty());
        if usage.is_some() {
            if let Some(key) = crate::stats::usage_key(self.harness, &message) {
                if !self.seen.insert(key) {
                    usage = None;
                }
            }
        }
        let boundary = message.kind == MessageKind::CompactionBoundary;
        if usage.is_none() && !boundary && !limit_changed {
            return Flow::Continue;
        }
        let metadata = bag_value(&message, "compactMetadata").or_else(|| bag_value(&message, "compact_metadata"));
        let prompt_tokens = usage.as_ref().and_then(|usage| {
            (usage.input_tokens.is_some() || usage.cache_read_tokens.is_some() || usage.cache_creation_tokens.is_some())
                .then(|| usage.prompt_tokens())
        });
        let usage_basis = usage.as_ref().map(|_| {
            if bag_value(&message, "_cv_orig_ctx").is_some() {
                "estimated"
            } else {
                "recorded"
            }
        });
        let pre_compaction_tokens = boundary
            .then(|| metadata.and_then(|v| v.get("preTokens")).and_then(Value::as_u64))
            .flatten();
        let trigger = boundary
            .then(|| {
                metadata
                    .and_then(|v| v.get("trigger"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
        self.rows.push(ContextRow {
            index,
            timestamp: message.timestamp,
            kind: if boundary {
                "compaction_boundary"
            } else if usage.is_some() {
                "usage"
            } else {
                "context_limit"
            },
            model: message.model,
            usage,
            usage_basis,
            prompt_tokens,
            context_window: limit,
            pre_compaction_tokens,
            trigger,
        });
        Flow::Continue
    }
}

/// Stream once and retain only numeric context rows. Text, media, tool bodies and compaction
/// summaries remain in the source. Empty output means no usable usage, limit or boundary records.
pub fn read(adapter: &dyn crate::Adapter, session: &SessionRef) -> Result<Vec<ContextRow>> {
    let mut sink = ContextSink::new(session.harness);
    adapter.try_stream(session, &Demand::FACTS.with_spans(), &mut sink)?;
    Ok(sink.rows)
}
