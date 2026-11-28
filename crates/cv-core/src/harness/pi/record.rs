//! The Pi record codec: one JSONL line is one entry, and the entries form a tree.
//!
//! The block codec is [`crate::harness::wire::anthropic`]'s `OPENCLAW` quirks — Pi's `message`
//! object is the same shape OpenClaw persists (`text` / `thinking` / `toolCall` / `image` blocks,
//! the same `toolCallId`/`toolName`/`isError` on a tool result). What is Pi's own is the TREE, the
//! COMPACTION PROJECTION, and branch-local `context_edit`s, and those are what this module
//! implements over bounded [`jsonl::Read`] + [`RangeSource`] byte slices.

use super::{parse_ts, ts_from_value};
use crate::harness::store::document::range::RangeSource;
use crate::harness::store::document::Fields;
use crate::harness::store::jsonl::{self, ranges};
use crate::harness::wire::{
    self,
    anthropic::{self, OPENCLAW},
};
use crate::ir::*;
use crate::json::{JsonView, ReadInput, Validation};
use crate::lazy::Text;
use crate::stream::{Demand, Flow, MessageSink, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Array text joined from several strings stays in the source above this many bytes when spans
/// are demanded, matching OpenClaw and Claude tool-result arrays.
const JOINED_OVER: u64 = 64 * 1024;

const NAV_TOP: &[&str] = &[
    "type",
    "id",
    "parentId",
    "timestamp",
    "cwd",
    "version",
    "parentSession",
    "model",
    "modelId",
    "name",
    "firstKeptEntryId",
    "targetId",
    "replacement",
    "message",
];

const NAV_MESSAGE: &[&str] = &["role", "model", "errorMessage", "stopReason", "timestamp", "content"];

const DECODE_TOP: &[&str] = &[
    "type",
    "id",
    "parentId",
    "timestamp",
    "name",
    "summary",
    "firstKeptEntryId",
    "tokensBefore",
    "fromHook",
    "systemMessage",
    "details",
    "modelId",
    "model",
    "provider",
    "thinkingLevel",
    "message",
];

const DECODE_BODY: &[&str] = &[
    "role",
    "model",
    "timestamp",
    "content",
    "toolCallId",
    "toolName",
    "isError",
    "details",
];

/// Header facts captured from the leading `session` entry without retaining the transcript.
#[derive(Default)]
struct Header {
    id: Option<String>,
    cwd: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
    forked_from: Option<String>,
    model: Option<String>,
    version: Option<i64>,
    range: Option<(u64, u64)>,
}

/// How a `context_edit` modifies its target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditAction {
    None,
    /// `replacement` is absent or `null`: drop `targetId` from the projected context.
    Drop,
    /// `replacement` is an object: swap `targetId`'s `content` while keeping its identity.
    Replace,
}

/// One entry's compact structural and discovery facts, read without materializing its body.
struct Entry {
    id: String,
    parent: Option<String>,
    kind: String,
    /// A compaction entry's `firstKeptEntryId`: the oldest entry it kept.
    kept_id: Option<String>,
    /// A `message` entry whose role is `system`: the checkpoint replaces these, so the projection
    /// drops them from the kept pre-compaction span.
    is_system: bool,
    /// Whether this entry produces a conversational turn (`Message::is_turn()`), excluding failed
    /// assistant error records.
    is_turn: bool,
    /// Model recorded on `model_change` or `assistant` entries.
    model: Option<String>,
    /// Title from a `session_info` entry (`name`).
    session_name: Option<String>,
    /// Bounded title preview of a `user` prompt (`wire::text_title`).
    user_title: Option<String>,
    /// A `context_edit` entry's `targetId`.
    target_id: Option<String>,
    /// What a `context_edit` does to its target.
    edit_action: EditAction,
    /// Bounded title preview of a `context_edit` object replacement's `content`.
    edit_title: Option<String>,
    /// Start offset of the JSON record line in the source.
    line_at: u64,
    /// Validated byte range `(offset, len)` of this JSON object in the source.
    range: (u64, u64),
}

struct Scan {
    entries: Vec<Entry>,
    header: Header,
    max_ts: Option<DateTime<Utc>>,
    skipped: u64,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

fn timestamp_view(value: Option<JsonView<'_>>) -> Result<Option<DateTime<Utc>>> {
    match value {
        Some(v) if v.is_string()? || v.as_number()?.is_some() => Ok(ts_from_value(v.resolve()?.as_ref())),
        _ => Ok(None),
    }
}

fn string_title(value: Option<JsonView<'_>>) -> Result<Option<String>> {
    match value {
        Some(v) if v.is_string()? => wire::text_title(Some(v)),
        _ => Ok(None),
    }
}

fn non_blank(view: Option<JsonView<'_>>) -> Result<bool> {
    let Some(view) = view else { return Ok(false) };
    let mut found = false;
    Ok(view.for_each_char(|c| {
        found = !c.is_whitespace();
        !found
    })? && found)
}

fn present(view: Option<JsonView<'_>>) -> Result<Option<Value>> {
    match view {
        Some(v) if !v.is_null()? => Ok(Some(v.resolve()?.into_owned())),
        _ => Ok(None),
    }
}

fn lazy(view: Option<JsonView<'_>>, spans: bool) -> Result<Text> {
    Ok(view
        .map(|v| v.lazy_text(spans))
        .transpose()?
        .flatten()
        .unwrap_or_default())
}

fn text_parts(content: Option<JsonView<'_>>, spans: bool) -> Result<Text> {
    let backing = wire::Backing::Over(if spans { JOINED_OVER } else { u64::MAX });
    wire::joined(content, wire::TEXT, backing, &mut |v| v.lazy_text(spans))
}

fn bump_max_ts(slot: &mut Option<DateTime<Utc>>, ts: Option<DateTime<Utc>>) {
    if let Some(t) = ts {
        *slot = Some(slot.map_or(t, |prev| prev.max(t)));
    }
}

/// Extract one entry's compact descriptor from its `JsonView`. Returns `Ok(false)` when `view` is
/// not a JSON object (counted as a skipped line).
fn scan_one(view: JsonView<'_>, line_at: u64, range: (u64, u64), scan: &mut Scan) -> Result<bool> {
    let Some(top) = Fields::parse(view, NAV_TOP)? else {
        return Ok(false);
    };
    let kind = top.string("type")?.unwrap_or_default();
    let id = top.string("id")?.unwrap_or_default();
    let parent = non_empty(top.string("parentId")?);
    let mut timestamp = top.string("timestamp")?.as_deref().and_then(parse_ts);

    if kind == "session" && scan.header.range.is_none() {
        scan.header = Header {
            id: non_empty(Some(id.clone())),
            cwd: non_empty(top.string("cwd")?).map(PathBuf::from),
            created_at: timestamp,
            forked_from: non_empty(top.string("parentSession")?),
            model: non_empty(top.string("model")?).or(non_empty(top.string("modelId")?)),
            version: top
                .get("version")
                .map(JsonView::as_number)
                .transpose()?
                .flatten()
                .and_then(|n| n.as_i64()),
            range: Some(range),
        };
    }

    let mut is_system = false;
    let mut is_turn = false;
    let mut model = None;
    let mut session_name = None;
    let mut user_title = None;
    let mut target_id = None;
    let mut edit_action = EditAction::None;
    let mut edit_title = None;

    match kind.as_str() {
        "message" => {
            if let Some(msg) = top
                .get("message")
                .map(|m| Fields::parse(m, NAV_MESSAGE))
                .transpose()?
                .flatten()
            {
                if timestamp.is_none() {
                    timestamp = timestamp_view(msg.get("timestamp"))?;
                }
                let role = msg.string("role")?;
                is_system = role.as_deref() == Some("system");
                model = non_empty(msg.string("model")?);
                match role.as_deref() {
                    Some("user") => {
                        is_turn = true;
                        user_title = wire::text_title(msg.get("content"))?;
                    }
                    Some("assistant") => {
                        is_turn = !wire::assistant_failed(&msg)?;
                    }
                    _ => {}
                }
            }
        }
        "model_change" => {
            model = non_empty(top.string("modelId")?).or(non_empty(top.string("model")?));
        }
        "session_info" => {
            session_name = string_title(top.get("name"))?;
        }
        "context_edit" => {
            target_id = non_empty(top.string("targetId")?);
            match top.get("replacement") {
                None => edit_action = EditAction::Drop,
                Some(r) if r.is_null()? => edit_action = EditAction::Drop,
                Some(r) if r.is_object()? => {
                    edit_action = EditAction::Replace;
                    edit_title = wire::text_title(r.get("content")?)?;
                }
                Some(_) => edit_action = EditAction::None,
            }
        }
        _ => {}
    }

    bump_max_ts(&mut scan.max_ts, timestamp);
    let kept_id = if kind == "compaction" {
        non_empty(top.string("firstKeptEntryId")?)
    } else {
        None
    };

    scan.entries.push(Entry {
        id,
        parent,
        kind,
        kept_id,
        is_system,
        is_turn,
        model,
        session_name,
        user_title,
        target_id,
        edit_action,
        edit_title,
        line_at,
        range,
    });
    Ok(true)
}

/// Pass 1 over an opened [`jsonl::Read`]: compact navigation descriptors + byte ranges.
fn scan_file(read: &jsonl::Read) -> Result<Scan> {
    let mut scan = Scan {
        entries: Vec::new(),
        header: Header::default(),
        max_ts: None,
        skipped: 0,
    };
    read.lines(0, |line| {
        match line.view {
            Some(view) => {
                let range = view
                    .source_range()?
                    .context("a file-backed Pi transcript line has a source range")?;
                if !scan_one(view, line.at, range, &mut scan)? {
                    scan.skipped += 1;
                }
            }
            None if !line.blank => {
                anyhow::ensure!(
                    ranges::valid_utf8(&mut ranges::reader(
                        read,
                        ranges::Range {
                            offset: line.at,
                            len: line.end - line.at,
                        },
                    )?)?,
                    "Pi transcript did not contain valid UTF-8"
                );
                scan.skipped += 1;
            }
            None => {}
        }
        Ok(Flow::Continue)
    })?;
    Ok(scan)
}

/// Pass 1 over an in-memory UTF-8 slice (`ingest`).
fn scan_slice(text: &str) -> Result<Scan> {
    let mut scan = Scan {
        entries: Vec::new(),
        header: Header::default(),
        max_ts: None,
        skipped: 0,
    };
    jsonl::slice(text.as_bytes(), |line| {
        match line.view {
            Some(view) => {
                let len = line.end.saturating_sub(line.at);
                if !scan_one(view, line.at, (line.at, len), &mut scan)? {
                    scan.skipped += 1;
                }
            }
            None if !line.blank => {
                scan.skipped += 1;
            }
            None => {}
        }
        Ok(Flow::Continue)
    })?;
    Ok(scan)
}

struct Projection {
    /// Active root-to-leaf parent chain indices (before compaction cutting).
    chain: Vec<usize>,
    /// Projected active entry indices in emission order.
    order: Vec<usize>,
    /// `target_entry_idx -> context_edit_entry_idx` for object replacements on `message` entries.
    replacements: HashMap<usize, usize>,
}

/// The projection: which entries the model context actually contains, in the order the model sees.
///
/// Pi's own rule (`session-manager.js buildSessionProjection`): the last entry is the default leaf;
/// walk its `parentId` chain to the root; the latest `compaction` on that chain sets the oldest
/// kept entry (`firstKeptEntryId`) and supplies the system prompt from that point on.
///
/// THE ORDER IS NOT FILE ORDER, and this is the seam that is easy to get wrong. Native yields:
///   1. the latest compaction FIRST (its checkpoint system message is the context's head);
///   2. the kept PRE-compaction entries, EXCLUDING system messages and older compactions;
///   3. the post-compaction entries.
///
/// An older retained compaction contributes no messages of its own. Emitting the kept span in file
/// order would put the compaction last and keep the pre-compaction system messages, which is a
/// different context from the one the model saw.
fn projection(entries: &[Entry]) -> Projection {
    if entries.is_empty() {
        return Projection {
            chain: Vec::new(),
            order: Vec::new(),
            replacements: HashMap::new(),
        };
    }
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.id.is_empty())
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();

    // The leaf: the last entry, then its parent chain back to the root.
    let mut chain = Vec::new();
    let mut at = Some(entries.len() - 1);
    let mut guard = 0usize;
    while let Some(i) = at {
        chain.push(i);
        guard += 1;
        if guard > entries.len() {
            break; // a cycle: refuse to loop, keep what we have
        }
        at = entries[i].parent.as_deref().and_then(|p| by_id.get(p).copied());
    }
    chain.reverse(); // root first

    let mut latest: Option<usize> = None;
    for (pos, &i) in chain.iter().enumerate() {
        if entries[i].kind == "compaction" {
            latest = Some(pos);
        }
    }
    let raw_order = match latest {
        None => chain.clone(),
        Some(comp_pos) => {
            let kept_id = entries[chain[comp_pos]].kept_id.as_deref();
            let kept_from = kept_id
                .filter(|id| !id.is_empty())
                .and_then(|id| chain.iter().position(|&i| entries[i].id == id))
                .unwrap_or(comp_pos);

            // Clamp ranges so unusual/malformed `kept_from >= comp_pos` or trailing compaction never panics.
            let mut out = Vec::with_capacity(chain.len().saturating_sub(kept_from) + 1);
            out.push(chain[comp_pos]);
            if kept_from < comp_pos {
                for &i in &chain[kept_from..comp_pos] {
                    if entries[i].kind != "compaction" && (entries[i].kind != "message" || !entries[i].is_system) {
                        out.push(i);
                    }
                }
            }
            if comp_pos + 1 < chain.len() {
                for &i in &chain[comp_pos + 1..] {
                    out.push(i);
                }
            }
            out
        }
    };

    let (order, replacements) = apply_context_edits(entries, &by_id, &chain, raw_order);
    Projection {
        chain,
        order,
        replacements,
    }
}

/// Apply the branch-local `context_edit` entries on `chain` to `out`.
///
/// Only edits ON THE CHAIN apply (an edit on an abandoned branch is not this session's context),
/// and the LAST edit naming a target wins. `EditAction::Drop` (`replacement: null`) drops the
/// target; `EditAction::Replace` swaps the content of a `message` target while preserving its
/// identity, role, timestamp, model, usage and tool call id. Object edits targeting a non-`message`
/// entry are ignored cleanly.
fn apply_context_edits(
    entries: &[Entry],
    by_id: &HashMap<&str, usize>,
    chain: &[usize],
    mut out: Vec<usize>,
) -> (Vec<usize>, HashMap<usize, usize>) {
    let mut edits: HashMap<&str, usize> = HashMap::new();
    for &i in chain {
        if entries[i].kind == "context_edit" && entries[i].edit_action != EditAction::None {
            if let Some(target) = entries[i].target_id.as_deref() {
                edits.insert(target, i);
            }
        }
    }
    if edits.is_empty() {
        return (out, HashMap::new());
    }
    out.retain(|&i| match edits.get(entries[i].id.as_str()) {
        Some(&edit) => entries[edit].edit_action != EditAction::Drop,
        None => true,
    });
    let in_out: HashSet<usize> = out.iter().copied().collect();
    let mut replacements: HashMap<usize, usize> = HashMap::new();
    for (&target, &edit) in &edits {
        if entries[edit].edit_action != EditAction::Replace {
            continue;
        }
        if let Some(&t) = by_id.get(target) {
            if entries[t].kind == "message" && in_out.contains(&t) {
                replacements.insert(t, edit);
            }
        }
    }
    (out, replacements)
}

/// Session title: last non-blank `session_info.name` on `chain`, else first active user prompt's
/// `wire::text_title` (respecting `context_edit` drops and replacements).
fn resolve_title(entries: &[Entry], proj: &Projection) -> (Option<String>, Option<String>) {
    let mut session_name: Option<String> = None;
    for &i in &proj.chain {
        if let Some(name) = &entries[i].session_name {
            session_name = Some(name.clone());
        }
    }
    if session_name.is_some() {
        return (session_name.clone(), session_name);
    }

    // Active edits along the chain so a `null`-dropped prompt never leaks into the title.
    let mut edits: HashMap<&str, usize> = HashMap::new();
    for &i in &proj.chain {
        if entries[i].kind == "context_edit" && entries[i].edit_action != EditAction::None {
            if let Some(target) = entries[i].target_id.as_deref() {
                edits.insert(target, i);
            }
        }
    }
    let prompt_title = proj.chain.iter().find_map(|&i| {
        if entries[i].kind != "message" {
            return None;
        }
        match edits.get(entries[i].id.as_str()).copied() {
            Some(edit) if entries[edit].edit_action == EditAction::Drop => None,
            Some(edit) if entries[edit].edit_action == EditAction::Replace => entries[edit].edit_title.clone(),
            _ => entries[i].user_title.clone(),
        }
    });
    (prompt_title, None)
}

/// Session-level model: the last `model_change` on `chain` (even before `firstKeptEntryId`),
/// else the header's `model`, else the first assistant `message.model` on `chain`.
fn resolve_model(header: &Header, entries: &[Entry], chain: &[usize]) -> Option<String> {
    let mut last_change: Option<&str> = None;
    let mut first_assistant: Option<&str> = None;
    for &i in chain {
        match entries[i].kind.as_str() {
            "model_change" => {
                if let Some(m) = entries[i].model.as_deref() {
                    last_change = Some(m);
                }
            }
            "message" if first_assistant.is_none() => {
                first_assistant = entries[i].model.as_deref();
            }
            _ => {}
        }
    }
    last_change
        .map(str::to_owned)
        .or_else(|| header.model.clone())
        .or_else(|| first_assistant.map(str::to_owned))
}

fn fallback_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string()
}

use crate::harness::file_mtime;

fn is_known_entry_kind(kind: &str) -> bool {
    matches!(
        kind,
        "session"
            | "message"
            | "compaction"
            | "model_change"
            | "thinking_level_change"
            | "context_edit"
            | "session_info"
    )
}

/// One session's discovery ref: bounded navigation scan, exact projected `is_turn` count.
pub(super) fn discover_one(path: &Path) -> Result<SessionRef> {
    let read = jsonl::Read::open(path)?;
    let scan = scan_file(&read)?;
    let proj = projection(&scan.entries);
    let (title, _) = resolve_title(&scan.entries, &proj);
    let id = scan.header.id.unwrap_or_else(|| fallback_id(path));
    let mut r = SessionRef::new(id, Harness::Pi, path.to_path_buf());
    r.cwd = scan.header.cwd;
    r.title = title;
    r.created_at = scan.header.created_at;
    r.updated_at = scan.max_ts.or_else(|| file_mtime(path));
    r.message_count = Some(proj.order.iter().filter(|&&i| scan.entries[i].is_turn).count());
    read.check()?;
    Ok(r)
}

/// Stream the projected (or complete) entries from an opened [`jsonl::Read`] into `sink`.
pub(super) fn stream(
    path: &Path,
    r: &SessionRef,
    opts: &Demand,
    sink: &mut dyn MessageSink,
    read: &jsonl::Read,
) -> Result<Session> {
    let scan = scan_file(read)?;
    let proj = projection(&scan.entries);
    let source = read.source();
    let mut s = build_session(Some(path), &r.id, r.updated_at, &scan, &proj, opts);
    if opts.tier == Tier::Complete {
        if let Some((h_off, h_len)) = scan.header.range {
            let raw = source.read(ReadInput::Range(h_off, h_len), Validation::Value, |v| {
                Ok(v.resolve()?.into_owned())
            })?;
            s.harness_extra_mut(Harness::Pi).insert("header_record".into(), raw);
        }
    }

    let chain_set: HashSet<usize> = proj.chain.iter().copied().collect();
    let order: Vec<usize> = if opts.tier == Tier::Complete {
        (0..scan.entries.len()).collect()
    } else {
        proj.order.clone()
    };
    let resolver =
        (opts.tier == Tier::Complete && !opts.spans).then(|| crate::lazy::Resolver::new(Some(path.to_path_buf())));
    let mut meta_sent = false;
    let mut header_version = scan.header.version;

    for &i in &order {
        let entry = &scan.entries[i];
        if entry.kind == "session" {
            continue;
        }
        let (offset, len) = entry.range;
        let replacement_idx = if opts.tier == Tier::Complete {
            None
        } else {
            proj.replacements.get(&i).copied()
        };
        let mut decoded = match replacement_idx {
            None => source.read(ReadInput::Range(offset, len), Validation::Value, |view| {
                decode_entry(view, None, opts)
            })?,
            Some(edit_idx) => {
                let (e_off, e_len) = scan.entries[edit_idx].range;
                source.read(ReadInput::Range(offset, len), Validation::Value, |view| {
                    source.read(ReadInput::Range(e_off, e_len), Validation::Value, |edit_view| {
                        let rep = match edit_view.get("replacement")? {
                            Some(r) if r.is_object()? => Some(r),
                            _ => None,
                        };
                        decode_entry(view, rep, opts)
                    })
                })?
            }
        };

        let active = opts.tier != Tier::Complete || chain_set.contains(&i);
        let stamp_offset = opts.offsets && replacement_idx.is_none();
        if emit_decoded(
            &mut decoded,
            active,
            stamp_offset.then_some(entry.line_at),
            &mut header_version,
            resolver.as_ref(),
            &mut s,
            &mut meta_sent,
            sink,
        )? == Flow::Stop
        {
            break;
        }
    }
    if !meta_sent {
        sink.meta(&s);
    }
    read.check()?;
    Ok(s)
}

/// Wasm-safe in-memory Pi transcript parser for [`crate::ingest`].
pub(super) fn parse_str(fallback_id: &str, text: &str) -> Result<Session> {
    let opts = Demand::RICH;
    let scan = scan_slice(text)?;
    let proj = projection(&scan.entries);
    let id = scan.header.id.clone().unwrap_or_else(|| fallback_id.to_string());
    let mut s = build_session(None, &id, None, &scan, &proj, &opts);
    let mut collect = crate::stream::CollectSink::default();
    {
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Pi, &mut collect);
        let mut meta_sent = false;
        let mut header_version = scan.header.version;
        for &i in &proj.order {
            let entry = &scan.entries[i];
            if entry.kind == "session" {
                continue;
            }
            let (offset, len) = entry.range;
            let slice = &text[offset as usize..(offset + len) as usize];
            let src = RangeSource::text(None, slice, 0, false);
            let Some(val) = src.accept(src.range(), Validation::Value)? else {
                continue;
            };
            let view = JsonView::of(&src, val);
            let mut decoded = match proj.replacements.get(&i).copied() {
                None => decode_entry(view, None, &opts)?,
                Some(edit_idx) => {
                    let (e_off, e_len) = scan.entries[edit_idx].range;
                    let e_slice = &text[e_off as usize..(e_off + e_len) as usize];
                    let e_src = RangeSource::text(None, e_slice, 0, false);
                    match e_src.accept(e_src.range(), Validation::Value)? {
                        Some(e_val) => {
                            let e_view = JsonView::of(&e_src, e_val);
                            let rep = match e_view.get("replacement")? {
                                Some(r) if r.is_object()? => Some(r),
                                _ => None,
                            };
                            decode_entry(view, rep, &opts)?
                        }
                        None => decode_entry(view, None, &opts)?,
                    }
                }
            };
            if emit_decoded(
                &mut decoded,
                true,
                None,
                &mut header_version,
                None,
                &mut s,
                &mut meta_sent,
                &mut spawns,
            )? == Flow::Stop
            {
                break;
            }
        }
        if !meta_sent {
            spawns.meta(&s);
        }
    }
    s.messages = collect.messages;
    Ok(s)
}

fn build_session(
    path: Option<&Path>,
    fallback_id: &str,
    ref_updated: Option<DateTime<Utc>>,
    scan: &Scan,
    proj: &Projection,
    opts: &Demand,
) -> Session {
    let id = scan.header.id.clone().unwrap_or_else(|| {
        if !fallback_id.is_empty() {
            fallback_id.to_string()
        } else {
            "session".to_string()
        }
    });
    let (title, session_name) = resolve_title(&scan.entries, proj);
    let mut s = Session::new(id, Harness::Pi);
    s.source_path = path.map(Path::to_path_buf);
    s.cwd = scan.header.cwd.clone();
    s.title = title;
    s.created_at = scan.header.created_at;
    s.updated_at = scan.max_ts.or(ref_updated).or_else(|| path.and_then(file_mtime));
    s.model = resolve_model(&scan.header, &scan.entries, &proj.chain);
    s.lineage.forked_from = scan.header.forked_from.clone();
    super::super::note_skipped_lines(&mut s, scan.skipped);

    if let Some(name) = session_name {
        s.harness_extra_mut(Harness::Pi)
            .insert("session_name".into(), Value::from(name));
    }
    if opts.tier != Tier::Complete {
        let mut dropped: Vec<String> = proj
            .order
            .iter()
            .map(|&i| scan.entries[i].kind.as_str())
            .filter(|k| !is_known_entry_kind(k))
            .map(str::to_string)
            .collect();
        if !dropped.is_empty() {
            dropped.sort();
            dropped.dedup();
            s.harness_extra_mut(Harness::Pi)
                .insert("dropped_entry_types".into(), serde_json::json!(dropped));
        }
    }
    s
}

#[derive(Default)]
struct DecodedEntry {
    messages: Vec<Message>,
    native: Option<crate::Json>,
}

#[allow(clippy::too_many_arguments)]
fn emit_decoded(
    decoded: &mut DecodedEntry,
    active: bool,
    line_offset: Option<u64>,
    header_version: &mut Option<i64>,
    resolver: Option<&crate::lazy::Resolver>,
    s: &mut Session,
    meta_sent: &mut bool,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    let msgs = &mut decoded.messages;
    for m in msgs.iter_mut() {
        if !active {
            m.harness_extra_mut(Harness::Pi)
                .insert("inactive_branch".into(), Value::Bool(true));
        }
        if let Some(at) = line_offset {
            m.extra.insert(crate::offsets::OFFSET_KEY.into(), Value::from(at));
        }
        if m.kind == MessageKind::SystemPrompt && s.system_prompt.is_none() {
            s.system_prompt = m.text();
        }
        if s.model.is_none() {
            if let Some(model) = m.model.as_deref().filter(|m| !m.is_empty()) {
                s.model = Some(model.to_string());
            }
        }
        if let Some(ver) = header_version.take() {
            m.harness_extra_mut(Harness::Pi)
                .insert("session_version".into(), Value::from(ver));
        }
    }
    if let Some(native) = decoded.native.take() {
        CompleteRecord::attach_replay(msgs, Harness::Pi, native, serde_json::Map::new());
    }
    for mut m in std::mem::take(msgs) {
        if let Some(res) = resolver {
            m.materialize(res)?;
        }
        if !*meta_sent {
            sink.meta(s);
            *meta_sent = true;
        }
        if sink.message(m) == Flow::Stop {
            return Ok(Flow::Stop);
        }
    }
    Ok(Flow::Continue)
}

/// Decode one validated entry view (plus an optional `context_edit` replacement object view) into
/// zero or more IR [`Message`]s.
fn decode_entry(view: JsonView<'_>, replacement: Option<JsonView<'_>>, opts: &Demand) -> Result<DecodedEntry> {
    let spans = opts.spans || opts.tier == Tier::Complete;
    let Some(top) = Fields::parse(view, DECODE_TOP)? else {
        return Ok(DecodedEntry::default());
    };
    let kind = top.string("type")?.unwrap_or_default();
    if kind == "session" {
        return Ok(DecodedEntry::default());
    }
    let id = non_empty(top.string("id")?);
    let parent = non_empty(top.string("parentId")?);
    let mut ts = top.string("timestamp")?.as_deref().and_then(parse_ts);
    if ts.is_none() {
        if let Some(inner) = top.get("message").map(|m| m.get("timestamp")).transpose()?.flatten() {
            ts = timestamp_view(Some(inner))?;
        }
    }

    let mut messages = match kind.as_str() {
        "message" => decode_message(&top, id.clone(), parent.clone(), ts, replacement, spans)?,
        "compaction" => decode_compaction(&top, id.clone(), parent.clone(), ts, spans)?,
        "model_change" | "thinking_level_change" => decode_model_change(&top, &kind, id.clone(), parent.clone(), ts)?,
        "session_info" => {
            if opts.tier == Tier::Complete {
                let name = string_title(top.get("name"))?;
                let mut m = Message::of_kind(Role::System, MessageKind::Notice, Origin::Harness);
                m.id = id.clone();
                m.parent_id = parent.clone();
                m.timestamp = ts;
                m.content.push(Block::from_text(
                    format!("[session named: {}]", name.as_deref().unwrap_or("")).into(),
                ));
                m.harness_extra_mut(Harness::Pi)
                    .insert("record_type".into(), Value::from("session_info"));
                vec![m]
            } else {
                Vec::new()
            }
        }
        "context_edit" => carrier_messages("context_edit", id.clone(), parent.clone(), ts, opts),
        other => carrier_messages(other, id.clone(), parent.clone(), ts, opts),
    };

    if opts.tier == Tier::Complete && messages.is_empty() {
        messages = carrier_messages(&kind, id, parent, ts, opts);
    }
    let native = if opts.tier == Tier::Complete && view.is_object()? {
        Some(view.to_json()?)
    } else {
        None
    };
    Ok(DecodedEntry { messages, native })
}

fn carrier_messages(
    record_type: &str,
    id: Option<String>,
    parent: Option<String>,
    ts: Option<DateTime<Utc>>,
    opts: &Demand,
) -> Vec<Message> {
    if opts.tier != Tier::Complete {
        return Vec::new();
    }
    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
    m.id = id;
    m.parent_id = parent;
    m.timestamp = ts;
    m.harness_extra_mut(Harness::Pi)
        .insert("record_type".into(), Value::from(record_type));
    vec![m]
}

fn decode_message(
    top: &Fields<'_>,
    id: Option<String>,
    parent: Option<String>,
    ts: Option<DateTime<Utc>>,
    replacement: Option<JsonView<'_>>,
    spans: bool,
) -> Result<Vec<Message>> {
    let Some(container) = top.get("message") else {
        return Ok(Vec::new());
    };
    let Some(body) = Fields::parse(container, DECODE_BODY)? else {
        return Ok(Vec::new());
    };
    let Some(role_str) = body.string("role")? else {
        return Ok(Vec::new());
    };
    let (role, kind, origin) = match role_str.as_str() {
        "user" => (Role::User, MessageKind::Prompt, Origin::Human),
        "assistant" => (Role::Assistant, MessageKind::Reply, Origin::Model),
        "toolResult" | "tool" => (Role::Tool, MessageKind::ToolResult, Origin::Harness),
        "system" => (Role::System, MessageKind::SystemPrompt, Origin::Harness),
        _ => return Ok(Vec::new()),
    };
    // System prompts also populate `Session::system_prompt` (owned), so keep them inline.
    let keep_source = spans && role_str != "system";
    let mut msg = Message::of_kind(role, kind, origin);
    msg.id = id;
    msg.parent_id = parent;
    msg.timestamp = ts;
    msg.model = non_empty(body.string("model")?);

    let replaced_content = match replacement {
        Some(rep) => rep.get("content")?,
        None => None,
    };
    let content = replaced_content.or_else(|| body.get("content"));

    if matches!(role_str.as_str(), "toolResult" | "tool") {
        msg.content.push(Block::ToolResult {
            tool_use_id: body.string("toolCallId")?.unwrap_or_default(),
            content: text_parts(content, keep_source)?,
            is_error: body
                .get("isError")
                .map(JsonView::as_bool)
                .transpose()?
                .flatten()
                .unwrap_or(false),
            tool_name: body.string("toolName")?,
            status: None,
            details: present(body.get("details"))?,
            extensions: Default::default(),
        });
        if let Some(items) = content.map(JsonView::array).transpose()?.flatten() {
            for item in items {
                let item = item?;
                let is_img = item
                    .get("type")?
                    .filter(|v| v.is_string().unwrap_or(false))
                    .map(|v| Ok::<_, anyhow::Error>(v.resolve()?.as_str() == Some("image")))
                    .transpose()?
                    .unwrap_or(false);
                if is_img {
                    if let Some(b) = block_from_view(item, keep_source)? {
                        msg.content.push(b);
                    }
                }
            }
        }
    } else {
        match content {
            Some(c) if c.is_string()? => msg.content.push(Block::from_text(lazy(Some(c), keep_source)?)),
            Some(c) => {
                if let Some(items) = c.array()? {
                    for item in items {
                        if let Some(b) = block_from_view(item?, keep_source)? {
                            msg.content.push(b);
                        }
                    }
                }
            }
            None => {}
        }
    }

    if role_str == "assistant" {
        if let Some(meta) = Fields::parse(container, wire::PI_ASSISTANT_FIELDS)? {
            wire::pi_assistant_metadata(&mut msg, &meta, Harness::Pi)?;
        }
    }
    if replacement.is_some() {
        msg.harness_extra_mut(Harness::Pi)
            .insert("context_edit".into(), Value::Bool(true));
    }
    Ok(vec![msg])
}

fn decode_compaction(
    top: &Fields<'_>,
    id: Option<String>,
    parent: Option<String>,
    ts: Option<DateTime<Utc>>,
    spans: bool,
) -> Result<Vec<Message>> {
    let mut b = Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness);
    b.id = id;
    b.parent_id = parent;
    b.timestamp = ts;

    let tokens_before = top
        .get("tokensBefore")
        .map(JsonView::as_number)
        .transpose()?
        .flatten()
        .and_then(|n| n.as_u64());
    let from_hook = top.get("fromHook").map(JsonView::as_bool).transpose()?.flatten();

    {
        let bag = b.harness_extra_mut(Harness::Pi);
        bag.insert("record_type".into(), Value::from("compaction"));
        for key in [
            "firstKeptEntryId",
            "tokensBefore",
            "fromHook",
            "systemMessage",
            "details",
        ] {
            if let Some(val) = present(top.get(key))? {
                bag.insert(snake(key), val);
            }
        }
        // Populate `compact_metadata` so `compaction.rs` and `context.rs` see `pre_tokens` /
        // `pre_compaction_tokens` (and hook trigger) without harness special-casing.
        if tokens_before.is_some() || from_hook == Some(true) {
            let mut cm = serde_json::Map::new();
            if let Some(pre) = tokens_before {
                cm.insert("preTokens".into(), Value::from(pre));
            }
            if from_hook == Some(true) {
                cm.insert("trigger".into(), Value::from("hook"));
            }
            bag.insert("compact_metadata".into(), Value::Object(cm));
        }
    }

    let mut out = vec![b];
    let summary = top.get("summary");
    if non_blank(summary)? {
        let mut sm = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
        sm.id = None;
        sm.parent_id = out[0].id.clone();
        sm.timestamp = ts;
        sm.content.push(Block::from_text(lazy(summary, spans)?));
        sm.harness_extra_mut(Harness::Pi)
            .insert("record_type".into(), Value::from("compaction"));
        out.push(sm);
    }
    Ok(out)
}

fn decode_model_change(
    top: &Fields<'_>,
    kind: &str,
    id: Option<String>,
    parent: Option<String>,
    ts: Option<DateTime<Utc>>,
) -> Result<Vec<Message>> {
    let mut m = Message::of_kind(Role::System, MessageKind::ModelChange, Origin::Harness);
    m.id = id;
    m.parent_id = parent;
    m.timestamp = ts;

    let model_id = top.string("modelId")?.or(top.string("model")?);
    let provider = top.string("provider")?;
    let thinking_level = top.string("thinkingLevel")?;

    if kind == "model_change" {
        m.model = non_empty(model_id.clone());
    }

    {
        let bag = m.harness_extra_mut(Harness::Pi);
        bag.insert("record_type".into(), Value::from(kind));
        if let Some(mid) = &model_id {
            bag.insert("model_id".into(), Value::from(mid.as_str()));
        }
        if let Some(p) = &provider {
            bag.insert("provider".into(), Value::from(p.as_str()));
        }
        if let Some(tl) = &thinking_level {
            bag.insert("thinking_level".into(), Value::from(tl.as_str()));
        }
    }

    let text = if kind == "model_change" {
        let model = model_id.as_deref().unwrap_or("");
        match provider.as_deref() {
            Some(p) if !p.is_empty() => format!("[model: {p}/{model}]"),
            _ => format!("[model: {model}]"),
        }
    } else {
        match thinking_level.as_deref() {
            Some(l) if !l.is_empty() => format!("[thinking level: {l}]"),
            _ => "[thinking level changed]".to_string(),
        }
    };
    m.content.push(Block::from_text(text.into()));
    Ok(vec![m])
}

/// One `(TextContent|ThinkingContent|ToolCall|ImageContent)` item through the shared view codec,
/// preserving `thinking.redacted` like OpenClaw (`openclaw/record.rs:183-191`).
fn block_from_view(item: JsonView<'_>, spans: bool) -> Result<Option<Block>> {
    let joined_over = if spans { JOINED_OVER } else { u64::MAX };
    let mut text = |v: JsonView<'_>| v.lazy_text(spans);
    let mut block = anthropic::block_from_view_with(item, &OPENCLAW, spans, &mut text, joined_over)?;
    if let Some(Block::Thinking {
        redacted, encrypted, ..
    }) = &mut block
    {
        if item.get("redacted")?.map(JsonView::as_bool).transpose()?.flatten() == Some(true) {
            *redacted = true;
            *encrypted = Some("redacted".into());
        }
    }
    Ok(block)
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
