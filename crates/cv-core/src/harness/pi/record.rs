//! The Pi record codec: one JSONL line is one entry, and the entries form a tree.
//!
//! The block codec is [`crate::harness::wire::anthropic`]'s `OPENCLAW` quirks — Pi's `message`
//! object is the same shape OpenClaw persists (`text` / `thinking` / `toolCall` blocks, the same
//! `toolCallId`/`toolName`/`isError` on a tool result). What is Pi's own is the TREE and the
//! COMPACTION PROJECTION, and those are what this module implements.

use crate::harness::wire::anthropic::{self, OPENCLAW};
use super::parse_ts;
use crate::ir::*;
use crate::json::JsonSource;
use crate::stream::{Demand, Flow, MessageSink, Tier};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::Path;

/// One entry's structural facts, read without materializing its body.
struct Entry {
    id: String,
    parent: Option<String>,
    kind: String,
    /// A compaction entry's `firstKeptEntryId`: the oldest entry it kept.
    kept_id: Option<String>,
    /// A `message` entry whose role is `system`: the checkpoint replaces these, so the projection
    /// drops them from the kept pre-compaction span.
    is_system: bool,
    /// A `message` entry's role, so the turn count can be computed without reading the body.
    role: Option<String>,
    /// A `context_edit` entry's `targetId`: the entry it replaces or drops.
    target_id: Option<String>,
    /// A `context_edit` entry's `replacement` is null: the target is dropped, not replaced.
    replacement_is_null: bool,
    /// The line's byte offset, so the body is read only when the entry is emitted.
    offset: u64,
}

/// Read the entries' structure: id, parentId, type, and where each line sits.
fn entries(source: &JsonSource) -> Result<(Vec<Entry>, Option<Value>)> {
    let file = fs::File::open(source.path()).with_context(|| format!("reading {}", source.path().display()))?;
    let mut out = Vec::new();
    let mut header = None;
    let mut offset = 0u64;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        let len = line.len() as u64 + 1;   // the newline
        if line.trim().is_empty() {
            offset += len;
            continue;
        }
        let v: Value = serde_json::from_str(&line).with_context(|| format!("entry at byte {offset}"))?;
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let parent = v.get("parentId").and_then(|x| x.as_str()).map(str::to_string);
        let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("?").to_string();
        // Header metadata belongs to the session, even when compaction omits it from the context.
        if out.is_empty() && kind == "session" {
            header = Some(v.clone());
        }
        let kept_id = v.get("firstKeptEntryId").and_then(|x| x.as_str()).map(str::to_string);
        let is_system = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some("system");
        let role = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .map(str::to_string);
        let target_id = v.get("targetId").and_then(|x| x.as_str()).map(str::to_string);
        let replacement_is_null = v.get("replacement").is_none_or(Value::is_null);
        out.push(Entry { id, parent, kind, kept_id, is_system, role, target_id, replacement_is_null, offset });
        offset += len;
    }
    Ok((out, header))
}

/// The projection: which entries the model context actually contains, in the order the model sees.
///
/// Pi's own rule (`session-manager.js buildSessionProjection`): the last entry is the default leaf;
/// walk its `parentId` chain to the root; the latest `compaction` on that chain sets the oldest
/// kept entry (`firstKeptEntryId`) and supplies the system prompt from that point on.
///
/// THE ORDER IS NOT FILE ORDER, and this is the seam that is easy to get wrong. Native yields:
///   1. the latest compaction FIRST (its checkpoint system message is the context's head);
///   2. the kept PRE-compaction entries, EXCLUDING system messages (the checkpoint replaced them);
///   3. the post-compaction entries.
/// An older retained compaction contributes no messages of its own. Emitting the kept span in file
/// order would put the compaction last and keep the pre-compaction system messages, which is a
/// different context from the one the model saw.
fn projection(entries: &[Entry]) -> Vec<usize> {
    if entries.is_empty() {
        return Vec::new();
    }
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    // the leaf: the last entry, then its parent chain back to the root
    let mut chain = Vec::new();
    let mut at = Some(entries.len() - 1);
    let mut guard = 0usize;
    while let Some(i) = at {
        chain.push(i);
        guard += 1;
        if guard > entries.len() {
            break;   // a cycle: refuse to loop, keep what we have
        }
        at = entries[i].parent.as_deref().and_then(|p| by_id.get(p).copied());
    }
    chain.reverse();   // root first

    // THE LATEST COMPACTION ON THE CHAIN, and the boundary it names. The compaction entry sits at
    // the END of the chain (it is the newest entry), but `firstKeptEntryId` points BACK to the
    // oldest entry the compaction kept — so the kept set starts there, not at the compaction.
    let mut latest: Option<usize> = None;
    for (pos, &i) in chain.iter().enumerate() {
        if entries[i].kind == "compaction" {
            latest = Some(pos);
        }
    }
    let Some(comp_pos) = latest else {
        // no compaction: the whole chain is the context, with any context_edit applied
        return apply_context_edits(&entries, &chain, chain.clone());
    };
    let kept_id = entries[chain[comp_pos]].kept_id.as_deref();
    let kept_from = kept_id
        .and_then(|id| chain.iter().position(|&i| entries[i].id == id))
        .unwrap_or(comp_pos);

    // 1. the compaction first, 2. the kept pre-compaction entries without system messages,
    // 3. the post-compaction entries.
    //
    // THE RANGES MUST BE ORDERED. `kept_from` can be AFTER `comp_pos` when the compaction names a
    // kept entry that is newer than itself (a malformed or unusual file), and `comp_pos + 1` can be
    // past the end when the compaction is the last entry. Both would panic on a slice, so the
    // ranges are clamped rather than assumed.
    let mut out = Vec::with_capacity(chain.len().saturating_sub(kept_from) + 1);
    out.push(chain[comp_pos]);
    if kept_from < comp_pos {
        for &i in &chain[kept_from..comp_pos] {
            if entries[i].kind != "message" || !entries[i].is_system {
                out.push(i);
            }
        }
    }
    if comp_pos + 1 < chain.len() {
        for &i in &chain[comp_pos + 1..] {
            out.push(i);
        }
    }
    apply_context_edits(&entries, &chain, out)
}

/// Apply the branch-local `context_edit` entries to a projection.
///
/// A `context_edit` names a `targetId` and a `replacement`: `null` DROPS the target from the
/// context, an object REPLACES it. The edit itself is not a message — it changes what the model
/// sees, so the projection must apply it or the context it reports is not the one Pi sent.
///
/// Only edits ON THE CHAIN apply (a branch-local edit on an abandoned branch is not this
/// session's context), and the LAST edit naming a target wins, which is what an append-only log
/// means. An edit whose target is not in the projection is a no-op: it names an entry the
/// compaction already dropped.
fn apply_context_edits(entries: &[Entry], chain: &[usize], mut out: Vec<usize>) -> Vec<usize> {
    // the last edit per target, in chain order
    let mut edits: HashMap<&str, usize> = HashMap::new();
    for &i in chain {
        if entries[i].kind == "context_edit" {
            if let Some(target) = entries[i].target_id.as_deref() {
                edits.insert(target, i);
            }
        }
    }
    if edits.is_empty() {
        return out;
    }
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    out.retain(|&i| {
        // an entry is dropped when the last edit naming it has a null replacement
        match edits.get(entries[i].id.as_str()) {
            Some(&edit) => !entries[edit].replacement_is_null,
            None => true,
        }
    });
    // a replacement's content is emitted in the target's place, so the projection keeps the
    // target's position but the edit's body; the reader emits the edit entry itself for that.
    for (&target, &edit) in &edits {
        if entries[edit].replacement_is_null {
            continue;
        }
        if let Some(pos) = by_id.get(target).and_then(|t| out.iter().position(|&i| i == *t)) {
            out[pos] = edit;
        }
    }
    out
}

/// One session's metadata, without its messages.
pub(super) fn discover_one(path: &Path) -> Result<SessionRef> {
    let source = JsonSource::capture(path)?;
    let (entries, header) = entries(&source)?;
    let header = header.as_ref();
    let id = header
        .and_then(|h| h.get("id")).and_then(Value::as_str)
        .filter(|s| !s.is_empty()).map(str::to_string)
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string()
        });
    let mut r = SessionRef::new(id, Harness::Pi, path.to_path_buf());
    r.cwd = header.and_then(|h| h.get("cwd")).and_then(Value::as_str).map(Into::into);
    r.created_at = header.and_then(|h| h.get("timestamp")).and_then(Value::as_str).and_then(parse_ts);
    r.updated_at = fs::metadata(path).ok().and_then(|m| m.modified().ok()).map(Into::into);
    // THE CONVERSATIONAL TURN COUNT, which `ls` shows and every adapter must meet: user and
    // assistant messages only, never system/meta records or tool results. It is computed from the
    // same records the reader parses, not from a proxy like the file's line count.
    r.message_count = Some(
        entries
            .iter()
            .filter(|e| e.kind == "message" && !e.is_system)
            .filter(|e| matches!(e.role.as_deref(), Some("user") | Some("assistant")))
            .count(),
    );
    source.check()?;
    Ok(r)
}

/// Stream the projected entries into `sink`, IN PROJECTION ORDER.
///
/// A structural pass records offsets and header metadata; a body pass retains selected entries.
/// Bodies are emitted in projection order, never file order; header metadata is independent.
pub(super) fn stream(
    path: &Path,
    r: &SessionRef,
    opts: &Demand,
    sink: &mut dyn MessageSink,
    source: &JsonSource,
) -> Result<Session> {
    let (entries, header) = entries(source)?;
    let header = header.as_ref();
    // UNDER `complete`, THE WHOLE CHAIN IS EMITTED, not the projection. `complete` is the demand
    // that must not drop a field, and the projection deliberately drops inactive/summarized history
    // (the entries before the compaction's kept boundary). A full-history search or a full-graph
    // no-op writer cannot recover those from the projection, so it must see them. The projection is
    // what the MODEL saw; the chain is what the SESSION holds, and `complete` asks for the latter.
    let order = if opts.tier == Tier::Complete {
        let mut all: Vec<usize> = (0..entries.len()).collect();
        all.sort_by_key(|&i| entries[i].offset);   // file order, which is the chain's own order
        all
    } else {
        projection(&entries)
    };
    let mut s = Session::new(r.id.clone(), Harness::Pi);
    s.source_path = Some(path.to_path_buf());
    s.cwd = header.and_then(|h| h.get("cwd")).and_then(Value::as_str).map(Into::into);
    s.created_at = header.and_then(|h| h.get("timestamp")).and_then(Value::as_str).and_then(parse_ts);
    s.updated_at = r.updated_at;

    // RECORD WHAT THE READER DROPS, so a writer can refuse rather than silently lose it. A
    // `context_edit` is an append-only context edit the projection does not apply yet; an unknown
    // entry type is one this adapter has never seen. Either way the emitted messages are not the
    // whole file, and a writer that rebuilt the file from them would lose the difference. The
    // writer reads this list and refuses; a consumer that only reads is unaffected.
    let mut dropped: Vec<String> = Vec::new();
    for &i in &order {
        let kind = entries[i].kind.as_str();
        if !matches!(kind, "session" | "message" | "compaction" | "model_change" | "thinking_level_change") {
            dropped.push(kind.to_string());
        }
    }
    if !dropped.is_empty() {
        dropped.sort();
        dropped.dedup();
        s.extra.insert(
            "pi".into(),
            serde_json::json!({ "dropped_entry_types": dropped }),
        );
    }

    // read every line once, keyed by its offset
    let file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut at = 0u64;
    let mut bodies: HashMap<u64, Value> = HashMap::new();
    let wanted: HashSet<u64> = order.iter().map(|&i| entries[i].offset).collect();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if wanted.contains(&at) {
            let v: Value = serde_json::from_str(line.trim()).with_context(|| format!("entry at byte {at}"))?;
            bodies.insert(at, v);
        }
        at += n as u64;
    }

    // emit in the projection's order
    for &i in &order {
        let Some(v) = bodies.get(&entries[i].offset) else { continue };
        if emit_entry(v, &mut s, opts, sink)? == Flow::Stop {
            break;
        }
    }
    Ok(s)
}

/// One entry → its messages.
fn emit_entry(v: &Value, s: &mut Session, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Flow> {
    let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let parent = v.get("parentId").and_then(|x| x.as_str()).map(str::to_string);
    let ts = v.get("timestamp").and_then(|x| x.as_str()).and_then(parse_ts);

    match kind {
        "session" => Ok(Flow::Continue),
        "message" => {
            let m = v.get("message").cloned().unwrap_or(Value::Null);
            let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("user");
            let mut msg = Message::new(role_from(role));
            msg.id = Some(id);
            msg.parent_id = parent;
            msg.timestamp = ts;
            if role == "toolResult" {
                // A TOOL RESULT IS A MESSAGE ROLE, not a content block: `blocks_from` would turn its
                // content array into Text blocks and lose the call id. The block is pushed
                // explicitly, the same way OpenClaw's codec does (record.rs:511-537), so the shared
                // IR's `tool_use_id` is what generic pairing and every writer read.
                let content = m.get("content");
                let text = match content {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(items)) => items
                        .iter()
                        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(""),
                    _ => String::new(),
                };
                msg.content.push(Block::ToolResult {
                    tool_use_id: m
                        .get("toolCallId")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    content: crate::lazy::Text::Inline(text),
                    is_error: m.get("isError").and_then(|x| x.as_bool()).unwrap_or(false),
                    tool_name: m.get("toolName").and_then(|x| x.as_str()).map(str::to_string),
                    status: None,
                    details: m.get("details").filter(|d| !d.is_null()).cloned(),
                });
                // Text coercion cannot represent image parts; keep them through the shared codec,
                // as the OpenClaw envelope bridge does, without duplicating the result text.
                msg.content.extend(
                    blocks_from_value(content, opts.spans)
                        .into_iter()
                        .filter(|block| matches!(block, Block::Image { .. })),
                );
            } else {
                msg.content = blocks_from_value(m.get("content"), opts.spans);
            }
            if let Some(model) = m.get("model").and_then(|x| x.as_str()) {
                msg.model = Some(model.to_string());
            }
            if role == "assistant" {
                let fields = crate::harness::store::document::Fields::parse(
                    (&m).into(), crate::harness::wire::PI_ASSISTANT_FIELDS,
                )?.context("assistant metadata must be an object")?;
                crate::harness::wire::pi_assistant_metadata(&mut msg, &fields, Harness::Pi)?;
            }
            if opts.tier == Tier::Complete {
                msg.harness_extra_mut(Harness::Pi).insert("native".into(), v.clone());
            }
            Ok(sink.message(msg))
        }
        "compaction" => {
            // the boundary, then the summary — the same two-message shape OpenClaw's codec uses
            let mut b = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
            b.id = Some(id.clone());
            b.parent_id = parent.clone();
            b.timestamp = ts;
            b.kind = MessageKind::CompactionBoundary;
            let bag = b.harness_extra_mut(Harness::Pi);
            bag.insert("record_type".into(), Value::from("compaction"));
            for key in ["firstKeptEntryId", "tokensBefore", "fromHook"] {
                if let Some(x) = v.get(key) {
                    if !x.is_null() {
                        bag.insert(snake(key), x.clone());
                    }
                }
            }
            // the checkpoint's system prompt REPLACES the retained pre-compaction system messages
            if let Some(sm) = v.get("systemMessage") {
                bag.insert("system_message".into(), sm.clone());
            }
            // THE WHOLE NATIVE RECORD, so a writer can clone it rather than reconstruct it.
            if opts.tier == Tier::Complete {
                bag.insert("native".into(), v.clone());
            }
            b.content.push(Block::Text { text: crate::lazy::Text::Inline("[conversation compacted]".into()) });
            if sink.message(b) == Flow::Stop {
                return Ok(Flow::Stop);
            }
            if let Some(summary) = v.get("summary").and_then(|x| x.as_str()).filter(|t| !t.trim().is_empty()) {
                let mut sm = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
                sm.id = Some(id);
                sm.parent_id = parent;
                sm.timestamp = ts;
                sm.kind = MessageKind::CompactionSummary;
                sm.content.push(Block::Text { text: crate::lazy::Text::Inline(summary.to_string()) });
                sm.harness_extra_mut(Harness::Pi).insert("record_type".into(), Value::from("compaction"));
                return Ok(sink.message(sm));
            }
            Ok(Flow::Continue)
        }
        "model_change" | "thinking_level_change" => {
            let mut m = Message::of_kind(Role::System, MessageKind::ModelChange, Origin::Harness);
            m.id = Some(id);
            m.parent_id = parent;
            m.timestamp = ts;
            m.kind = MessageKind::ModelChange;
            let bag = m.harness_extra_mut(Harness::Pi);
            bag.insert("record_type".into(), Value::from(kind));
            if opts.tier == Tier::Complete {
                bag.insert("native".into(), v.clone());
            }
            for key in ["modelId", "provider", "thinkingLevel"] {
                if let Some(x) = v.get(key).and_then(|x| x.as_str()) {
                    bag.insert(snake(key), Value::from(x));
                }
            }
            // THE SESSION-LEVEL MODEL, so a header shows it rather than `?`. The last model_change
            // on the chain wins, which is what the session is running now.
            if kind == "model_change" {
                if let Some(model) = v.get("modelId").and_then(|x| x.as_str()).filter(|m| !m.is_empty()) {
                    s.model = Some(model.to_string());
                }
            }
            let text = match kind {
                "model_change" => {
                    let model = v.get("modelId").and_then(|x| x.as_str()).unwrap_or("");
                    match v.get("provider").and_then(|x| x.as_str()) {
                        Some(p) if !p.is_empty() => format!("[model: {p}/{model}]"),
                        _ => format!("[model: {model}]"),
                    }
                }
                _ => match v.get("thinkingLevel").and_then(|x| x.as_str()) {
                    Some(l) => format!("[thinking level: {l}]"),
                    None => "[thinking level changed]".to_string(),
                },
            };
            m.content.push(Block::Text { text: crate::lazy::Text::Inline(text) });
            Ok(sink.message(m))
        }
        "context_edit" => {
            // A REPLACEMENT IS EMITTED IN THE TARGET'S PLACE, as a user turn carrying the new
            // content: that is what the model sees where the target used to be. A null
            // replacement drops the target and emits nothing (the projection already removed it).
            let Some(replacement) = v.get("replacement").filter(|r| !r.is_null()) else {
                return Ok(Flow::Continue);
            };
            let mut msg = Message::new(Role::User);
            msg.id = Some(id);
            msg.parent_id = parent;
            msg.timestamp = ts;
            msg.content = blocks_from_value(replacement.get("content"), opts.spans);
            msg.harness_extra_mut(Harness::Pi).insert("context_edit".into(), Value::from(true));
            if opts.tier == Tier::Complete {
                msg.harness_extra_mut(Harness::Pi).insert("native".into(), v.clone());
            }
            Ok(sink.message(msg))
        }
        _ => Ok(Flow::Continue),
    }
}

/// Every block of a `content` value, through the shared view-based codec with Pi's quirks.
///
/// Pi persists the same `text`/`thinking`/`toolCall` block shapes OpenClaw does, so the codec is
/// [`anthropic::block_from_view_with`] with the `OPENCLAW` quirks — not a second parser. A bare
/// string is one text block.
fn blocks_from_value(content: Option<&Value>, spans: bool) -> Vec<Block> {
    let Some(content) = content else { return Vec::new() };
    if let Some(s) = content.as_str() {
        return vec![Block::Text { text: crate::lazy::Text::Inline(s.to_string()) }];
    }
    let Some(items) = content.as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for item in items {
        let view = crate::json::JsonView::from(item);
        let mut text = |v: crate::json::JsonView<'_>| v.lazy_text(spans);
        match anthropic::block_from_view_with(view, &OPENCLAW, spans, &mut text, u64::MAX) {
            Ok(Some(block)) => out.push(block),
            Ok(None) => {}
            Err(_) => {}
        }
    }
    out
}

fn role_from(role: &str) -> Role {
    match role {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "toolResult" | "tool" => Role::Tool,
        _ => Role::User,
    }
}

fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
