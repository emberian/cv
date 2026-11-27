//! Fold, splice, distill, and continuation lineage detection and chain collapsing.
//!
//! Tools (and `cv` itself via `distill --session`, `splice --session`, `rewind`) often fold or
//! branch a conversation by writing a new session file carrying most of its parent's history.
//! Without lineage collapsing, search and `cv pack` surface every copy of the same conversation.
//!
//! Safety guardrail: [`detect_opening_text`] and [`derived_from_path`] inspect **only** session
//! metadata records and the opening two conversational messages (`msg_idx < 2`), never arbitrary
//! UUIDs mentioned later in a transcript.

use crate::ir::{Block, Harness, Message, Session, SessionRef};
use crate::lazy::Resolver;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

/// Maximum bytes read from the head of a transcript file when probing derivation metadata.
const HEAD_PROBE_BYTES: usize = 32 * 1024;
/// Maximum bytes inspected inside an opening message's text for continuation/fold framing.
const OPENING_TEXT_BYTES: usize = 2048;
/// Maximum JSONL lines scanned in the head probe before giving up.
const MAX_HEAD_LINES: usize = 24;

/// Return the parent session ID that `session` continues or was derived/forked from, if recorded.
pub fn derived_from_session(session: &Session) -> Option<&str> {
    fn non_self<'a>(s: &'a str, own: &str) -> Option<&'a str> {
        (!s.is_empty() && s != own).then_some(s)
    }
    let own = session.id.as_str();
    if let Some(c) = session.lineage.continues.as_deref().and_then(|s| non_self(s, own)) {
        return Some(c);
    }
    if let Some(f) = session.lineage.forked_from.as_deref().and_then(|s| non_self(s, own)) {
        return Some(f);
    }
    if let Some(cv) = session.cv_extra() {
        if let Some(c) = cv
            .get("continues")
            .and_then(Value::as_str)
            .and_then(|s| non_self(s, own))
        {
            return Some(c);
        }
        if let Some(src) = cv
            .get("distill")
            .and_then(|d| d.get("source_id"))
            .and_then(Value::as_str)
            .and_then(|s| non_self(s, own))
        {
            return Some(src);
        }
        if let Some(seed) = cv.get("seed").and_then(Value::as_object) {
            if seed.get("kind").and_then(Value::as_str) != Some("pack") {
                if let Some(src) = seed
                    .get("source_id")
                    .and_then(Value::as_str)
                    .and_then(|s| non_self(s, own))
                {
                    return Some(src);
                }
            }
        }
        if let Some(src) = cv
            .get("loom")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|span| span.get("source_id"))
            .and_then(Value::as_str)
            .and_then(|s| non_self(s, own))
        {
            return Some(src);
        }
    }
    None
}

/// Detect explicit continuation/fold/distill/splice parent provenance on an opening message
/// (`msg_idx` 0 or 1). Callers MUST only invoke this on message 0 or 1.
pub fn detect_opening_message(msg: &Message, resolver: &Resolver, own_id: Option<&str>) -> Option<String> {
    if let Some(cv) = msg.cv_extra() {
        if let Some(seed) = cv.get("seed").and_then(Value::as_object) {
            if seed.get("kind").and_then(Value::as_str) != Some("pack") {
                if let Some(src) = seed.get("source_id").and_then(Value::as_str) {
                    if !src.is_empty() && own_id != Some(src) {
                        return Some(src.to_owned());
                    }
                }
            }
        }
    }
    for block in &msg.content {
        if let Block::Text { text, .. } = block {
            if let Ok(prefix) = text.try_resolve_prefix(resolver, OPENING_TEXT_BYTES as u64) {
                if let Some(found) = detect_opening_text(&prefix, own_id) {
                    return Some(found);
                }
            }
            break;
        }
    }
    None
}

/// Inspect the opening bytes of message 0/1 text for explicit continuation/fold provenance naming
/// a parent session ID (`[cv distill] ... Source: \`claude:<id>\``, `[cv splice] ...`,
/// `"continued from ... <uuid>"`, `"source session: <uuid>"`, etc.).
pub fn detect_opening_text(text: &str, own_id: Option<&str>) -> Option<String> {
    let end = crate::scan::floor_char(text, text.len().min(OPENING_TEXT_BYTES));
    let head = &text[..end];
    if head.trim().is_empty() {
        return None;
    }
    let accept = |candidate: String| (!candidate.is_empty() && own_id != Some(candidate.as_str())).then_some(candidate);

    // 1. `cv distill` / `# Distilled context —` opening pack header (`Source: \`<harness>:<id>\``).
    if head.contains("[cv distill") || head.starts_with("# Distilled context —") {
        if let Some(after_src) = find_ascii_case_insensitive(head, "source:") {
            let window = slice_window(head, after_src, 200);
            if let Some(id) = extract_harness_prefixed_id(window).or_else(|| extract_uuid(window)) {
                if let Some(ok) = accept(id) {
                    return Some(ok);
                }
            }
        }
        if let Some(after_tag) = find_ascii_case_insensitive(head, "[cv distill") {
            let window = slice_window(head, after_tag, 240);
            if let Some(id) = extract_harness_prefixed_id(window).or_else(|| extract_uuid(window)) {
                if let Some(ok) = accept(id) {
                    return Some(ok);
                }
            }
        }
    }

    // 2. `[cv splice]` marker.
    if let Some(after_splice) = find_ascii_case_insensitive(head, "[cv splice") {
        let window = slice_window(head, after_splice, 240);
        if let Some(id) = extract_harness_prefixed_id(window).or_else(|| extract_uuid(window)) {
            if let Some(ok) = accept(id) {
                return Some(ok);
            }
        }
    }

    // 3. Explicit continuation / fold / compaction provenance phrases in opening message head.
    const CUES: &[&str] = &[
        "continued from",
        "continuation of session",
        "continuation of conversation",
        "continuing session",
        "folded from",
        "compacted from",
        "spliced from",
        "distilled from",
        "derived from",
        "source session:",
        "parent session:",
        "previous session:",
    ];
    for cue in CUES {
        if let Some(after_cue) = find_ascii_case_insensitive(head, cue) {
            let window = slice_window(head, after_cue, 240);
            if let Some(id) = extract_harness_prefixed_id(window).or_else(|| extract_uuid(window)) {
                if let Some(ok) = accept(id) {
                    return Some(ok);
                }
            }
        }
    }
    None
}

fn slice_window(s: &str, start: usize, max_len: usize) -> &str {
    let end = crate::scan::floor_char(s, s.len().min(start.saturating_add(max_len)));
    &s[start..end]
}

/// Return byte offset immediately AFTER the first ASCII case-insensitive match of `needle` in `haystack`.
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || hb.len() < nb.len() {
        return None;
    }
    (0..=hb.len() - nb.len())
        .find(|&i| hb[i..i + nb.len()].eq_ignore_ascii_case(nb))
        .map(|i| i + nb.len())
}

/// Extract `<known_harness>:<session_id>` (e.g. `` `claude:wombatsess` `` or `claude:550e8400-...`).
fn extract_harness_prefixed_id(s: &str) -> Option<String> {
    for (colon_idx, _) in s.match_indices(':') {
        let before = &s[..colon_idx];
        let h_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .map_or(0, |p| p + 1);
        let h_cand = &before[h_start..];
        if h_cand.is_empty() || Harness::parse(h_cand).is_none() {
            continue;
        }
        // Reject URL schemes (`http://`, `https://`, `file://`).
        let after = &s[colon_idx + 1..];
        if after.starts_with('/') {
            continue;
        }
        let id_len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'.'))
            .count();
        let id = after[..id_len].trim_end_matches('.');
        if id.len() >= 3 {
            return Some(id.to_owned());
        }
    }
    None
}

/// Extract a canonical 36-byte UUID (`8-4-4-4-12` hex digits) bounded by non-identifier bytes.
pub fn extract_uuid(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 36 {
        return None;
    }
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    for i in 0..=b.len() - 36 {
        if i > 0 && (is_word(b[i - 1]) || b[i - 1] == b'-') {
            continue;
        }
        if i + 36 < b.len() && (is_word(b[i + 36]) || b[i + 36] == b'-') {
            continue;
        }
        if is_uuid_bytes(&b[i..i + 36]) {
            return Some(s[i..i + 36].to_owned());
        }
    }
    None
}

fn is_uuid_bytes(b: &[u8]) -> bool {
    if b.len() != 36 {
        return false;
    }
    for (idx, &ch) in b.iter().enumerate() {
        let expect_dash = matches!(idx, 8 | 13 | 18 | 23);
        if expect_dash {
            if ch != b'-' {
                return false;
            }
        } else if !ch.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Probe `SessionRef` for explicit fold/splice/distill/rewind/continuation parent session ID.
/// Reads at most a small head slice of the backing transcript (or sibling `.rewind.json`), falling
/// back to recorded `lineage_edges` in `state.db` (`continues` / `fork` / `rewind` / `splice` / `distill`).
pub fn derived_from_ref(r: &SessionRef) -> Option<String> {
    if let Some(found) = derived_from_path(&r.path, Some(r.id.as_str())) {
        return Some(found);
    }
    crate::lanes::session_lineage(&r.id)
        .ancestors
        .into_iter()
        .find(|link| {
            link.dst_session_id == r.id
                && link.src_session_id != r.id
                && matches!(
                    link.edge_kind.as_str(),
                    "continues" | "fork" | "rewind" | "splice" | "distill"
                )
        })
        .map(|link| link.src_session_id)
}

/// Check sibling `<stem>.rewind.json` provenance sidecar written by `cv rewind`.
pub fn derived_from_rewind_sidecar(path: &Path, own_id: Option<&str>) -> Option<String> {
    let accept = |s: &str| (!s.is_empty() && own_id != Some(s)).then(|| s.to_owned());
    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        let rewind_sidecar = path.with_extension("rewind.json");
        if let Ok(file) = std::fs::File::open(&rewind_sidecar) {
            let mut buf = Vec::with_capacity(2048);
            if file.take(8192).read_to_end(&mut buf).is_ok() {
                if let Ok(val) = serde_json::from_slice::<Value>(&buf) {
                    if val.get("format").and_then(Value::as_str) == Some("cv-rewind") {
                        if let Some(src_id) = val
                            .get("source")
                            .and_then(|s| s.get("id"))
                            .and_then(Value::as_str)
                            .and_then(accept)
                        {
                            return Some(src_id);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Effective conversational turn count for `SessionRef` when ranking members of a derivation chain:
/// returns `r.message_count` when known, otherwise counts conversational turns in `.jsonl`/`.json`
/// transcripts up to 16 MiB so fast head/tail discovery never treats a 5-turn distilled copy as
/// equal in length to its 40-turn parent.
pub fn effective_message_count(r: &SessionRef) -> Option<usize> {
    if r.message_count.is_some() {
        return r.message_count;
    }
    const MAX_SCAN: u64 = 16 * 1024 * 1024;
    let file = std::fs::File::open(&r.path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 || len > MAX_SCAN {
        return None;
    }
    let mut buf = Vec::with_capacity((len as usize).min(64 * 1024));
    file.take(MAX_SCAN).read_to_end(&mut buf).ok()?;
    if buf.starts_with(b"SQLite format 3") {
        return None;
    }
    let text = std::str::from_utf8(&buf).ok()?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') && !trimmed.lines().nth(1).is_some_and(|l| l.trim_start().starts_with('{')) {
        let doc: Value = serde_json::from_str(trimmed).ok()?;
        return doc.get("messages").and_then(Value::as_array).map(Vec::len);
    }
    let mut count = 0usize;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(line) {
            if is_conversational_jsonl_record(&val) {
                count += 1;
            }
        }
    }
    Some(count)
}

/// Head-only probe of a transcript path for explicit derivation lineage.
pub fn derived_from_path(path: &Path, own_id: Option<&str>) -> Option<String> {
    if let Some(src_id) = derived_from_rewind_sidecar(path, own_id) {
        return Some(src_id);
    }

    // 2. Read bounded head of the primary file.
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(4096);
    file.take(HEAD_PROBE_BYTES as u64).read_to_end(&mut buf).ok()?;
    if buf.is_empty() || buf.starts_with(b"SQLite format 3") {
        return None;
    }
    let valid_end = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(e) => e.valid_up_to(),
    };
    let head = std::str::from_utf8(&buf[..valid_end]).ok()?;
    derived_from_head_str(head, own_id)
}

/// Inspect a transcript head slice (JSONL or OpenSession/JSON document) for explicit lineage.
pub fn derived_from_head_str(head: &str, own_id: Option<&str>) -> Option<String> {
    let trimmed = head.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let accept = |s: &str| (!s.is_empty() && own_id != Some(s)).then(|| s.to_owned());

    // Whole-JSON document (e.g. OpenSession `.json`) when it carries `"messages"` or `"open_session"`.
    if trimmed.starts_with('{') && !trimmed.lines().nth(1).is_some_and(|l| l.trim_start().starts_with('{')) {
        if let Ok(doc) = serde_json::from_str::<Value>(trimmed) {
            if doc.get("messages").is_some() || doc.get("open_session").is_some() {
                if let Some(found) = derived_from_json_object(&doc, own_id) {
                    return Some(found);
                }
                if let Some(msgs) = doc.get("messages").and_then(Value::as_array) {
                    for m in msgs.iter().take(2) {
                        if let Some(found) = extract_json_message_derivation(m, own_id) {
                            return Some(found);
                        }
                    }
                }
                return None;
            }
        }
    }

    // JSONL head: inspect at most `MAX_HEAD_LINES` lines and stop after 2 conversational turns.
    let mut turns_seen = 0usize;
    for (line_idx, raw_line) in head.lines().take(MAX_HEAD_LINES).enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(val) => {
                if let Some(found) = derived_from_json_object(&val, own_id) {
                    return Some(found);
                }
                if is_conversational_jsonl_record(&val) {
                    if turns_seen < 2 {
                        if let Some(found) = extract_json_message_derivation(&val, own_id) {
                            return Some(found);
                        }
                    }
                    turns_seen += 1;
                    if turns_seen >= 2 {
                        break;
                    }
                }
            }
            Err(_) if line_idx <= 3 && turns_seen < 2 => {
                // Truncated giant opening record (> 32 KiB): check raw JSON string head safely.
                if let Some(unescaped) = rough_opening_text_from_partial_json(line) {
                    if let Some(found) = detect_opening_text(&unescaped, own_id).and_then(|s| accept(&s)) {
                        return Some(found);
                    }
                }
                break;
            }
            Err(_) => {}
        }
    }
    None
}

fn derived_from_json_object(val: &Value, own_id: Option<&str>) -> Option<String> {
    let obj = val.as_object()?;
    let accept = |s: &str| (!s.is_empty() && own_id != Some(s)).then(|| s.to_owned());

    // Direct continuation / derivation keys on a metadata record.
    for key in [
        "continuedFrom",
        "continued_from",
        "derivedFrom",
        "derived_from",
        "forked_from_id",
        "forkedFromId",
    ] {
        if let Some(id) = obj.get(key).and_then(Value::as_str).and_then(accept) {
            return Some(id);
        }
    }
    // Claude `fork-context-ref` record.
    if obj.get("type").and_then(Value::as_str) == Some("fork-context-ref") {
        if let Some(id) = obj.get("parentSessionId").and_then(Value::as_str).and_then(accept) {
            return Some(id);
        }
    }
    // Codex `session_meta` (`payload.forked_from_id` / `payload.continued_from`).
    if let Some(payload) = obj.get("payload").and_then(Value::as_object) {
        for key in [
            "continuedFrom",
            "continued_from",
            "derivedFrom",
            "derived_from",
            "forked_from_id",
        ] {
            if let Some(id) = payload.get(key).and_then(Value::as_str).and_then(accept) {
                return Some(id);
            }
        }
    }
    // IR / OpenSession `lineage` object.
    if let Some(lin) = obj.get("lineage").and_then(Value::as_object) {
        for key in ["continues", "forked_from"] {
            if let Some(id) = lin.get(key).and_then(Value::as_str).and_then(accept) {
                return Some(id);
            }
        }
    }
    // `extra.cv` provenance bag.
    if let Some(cv) = obj.get("extra").and_then(|e| e.get("cv")).and_then(Value::as_object) {
        if let Some(id) = cv.get("continues").and_then(Value::as_str).and_then(accept) {
            return Some(id);
        }
        if let Some(id) = cv
            .get("distill")
            .and_then(|d| d.get("source_id"))
            .and_then(Value::as_str)
            .and_then(accept)
        {
            return Some(id);
        }
        if let Some(seed) = cv.get("seed").and_then(Value::as_object) {
            if seed.get("kind").and_then(Value::as_str) != Some("pack") {
                if let Some(id) = seed.get("source_id").and_then(Value::as_str).and_then(accept) {
                    return Some(id);
                }
            }
        }
        if let Some(id) = cv
            .get("loom")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|s| s.get("source_id"))
            .and_then(Value::as_str)
            .and_then(accept)
        {
            return Some(id);
        }
    }
    None
}

fn is_conversational_jsonl_record(val: &Value) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("user" | "assistant") => true,
        Some("response_item") => obj
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|t| t == "message"),
        None => obj
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| matches!(r, "user" | "assistant")),
        _ => false,
    }
}

fn extract_json_message_derivation(val: &Value, own_id: Option<&str>) -> Option<String> {
    if let Some(found) = derived_from_json_object(val, own_id) {
        return Some(found);
    }
    let mut buf = String::new();
    collect_message_text(val, &mut buf);
    if buf.is_empty() {
        return None;
    }
    detect_opening_text(&buf, own_id)
}

fn collect_message_text(val: &Value, out: &mut String) {
    if out.len() >= OPENING_TEXT_BYTES {
        return;
    }
    let push = |s: &str, out: &mut String| {
        if out.len() >= OPENING_TEXT_BYTES || s.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let rem = OPENING_TEXT_BYTES.saturating_sub(out.len());
        out.push_str(&s[..crate::scan::floor_char(s, s.len().min(rem))]);
    };
    let Some(obj) = val.as_object() else {
        return;
    };
    for container in [Some(val), obj.get("message"), obj.get("payload")]
        .into_iter()
        .flatten()
    {
        if let Some(content) = container.get("content") {
            match content {
                Value::String(s) => push(s, out),
                Value::Array(items) => {
                    for item in items {
                        if let Some(s) = item.get("text").and_then(Value::as_str) {
                            push(s, out);
                        } else if let Some(s) = item.as_str() {
                            push(s, out);
                        }
                        if out.len() >= OPENING_TEXT_BYTES {
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn rough_opening_text_from_partial_json(line: &str) -> Option<String> {
    for marker in ["\"text\":\"", "\"content\":\""] {
        if let Some(pos) = line.find(marker) {
            let rest = &line[pos + marker.len()..];
            let cap = crate::scan::floor_char(rest, rest.len().min(OPENING_TEXT_BYTES));
            return Some(rest[..cap].replace("\\n", "\n").replace("\\\"", "\""));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Chain collapsing (`collapse_derived_chains`)
// ---------------------------------------------------------------------------

/// Metadata of one candidate session for [`collapse_derived_chains`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainMeta {
    pub harness: String,
    pub id: String,
    pub derived_from: Option<String>,
    pub message_count: Option<usize>,
    pub updated_at: Option<i64>,
    pub size_bytes: Option<u64>,
}

/// Record of a derived session copy collapsed into a fuller representative of the same chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollapsedCopy {
    pub harness: String,
    pub dropped_id: String,
    pub kept_id: String,
}

/// Group connected derivation chains (`A <- B <- C`, handling branching folds and cycles safely)
/// and select the **fullest** member of each chain in `items`:
/// 1. Largest `message_count` (fullest transcript).
/// 2. Active chain tip (`true` when no other candidate derives from this session).
/// 3. Newest `updated_at` timestamp.
/// 4. Largest `size_bytes`.
/// 5. Best original rank in `items`.
///
/// Each surviving chain representative is placed at the earliest index its chain occupied in
/// `items`, preserving recall/search ranking.
pub fn collapse_derived_chains<T, F>(items: Vec<T>, meta_of: F) -> (Vec<T>, Vec<CollapsedCopy>)
where
    F: FnMut(&T) -> ChainMeta,
{
    collapse_derived_chains_with_lookup(items, meta_of, |_, _| None)
}

/// Like [`collapse_derived_chains`], with an optional `lookup_parent(harness, id) -> Option<parent_id>`
/// to bridge intermediate sessions that were not themselves in `items`. Cycle-safe up to 16 hops.
pub fn collapse_derived_chains_with_lookup<T, F, L>(
    items: Vec<T>,
    mut meta_of: F,
    mut lookup_parent: L,
) -> (Vec<T>, Vec<CollapsedCopy>)
where
    F: FnMut(&T) -> ChainMeta,
    L: FnMut(&str, &str) -> Option<String>,
{
    if items.len() <= 1 {
        return (items, Vec::new());
    }

    let metas: Vec<ChainMeta> = items
        .iter()
        .map(|item| {
            let mut m = meta_of(item);
            m.derived_from = m.derived_from.filter(|p| !p.is_empty() && p != &m.id);
            m
        })
        .collect();

    // Fast path: if no candidate has `derived_from`, no chain can collapse.
    if metas.iter().all(|m| m.derived_from.is_none()) {
        return (items, Vec::new());
    }

    // Union-Find over `(harness, id)` strings, including virtual parent nodes not in `items`.
    let mut node_ids: HashMap<(String, String), usize> = HashMap::with_capacity(metas.len() * 2);
    let mut uf_parent: Vec<usize> = Vec::with_capacity(metas.len() * 2);
    let mut intern = |h: &str, id: &str, uf_parent: &mut Vec<usize>| -> usize {
        let key = (h.to_owned(), id.to_owned());
        if let Some(&idx) = node_ids.get(&key) {
            idx
        } else {
            let idx = uf_parent.len();
            uf_parent.push(idx);
            node_ids.insert(key, idx);
            idx
        }
    };
    fn find(uf: &mut [usize], mut x: usize) -> usize {
        while uf[x] != x {
            uf[x] = uf[uf[x]];
            x = uf[x];
        }
        x
    }
    fn union(uf: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(uf, a), find(uf, b));
        if ra != rb {
            uf[rb] = ra;
        }
    }

    let in_items: HashSet<(&str, &str)> = metas.iter().map(|m| (m.harness.as_str(), m.id.as_str())).collect();
    let mut has_child_in_items: HashSet<(String, String)> = HashSet::new();

    let item_nodes: Vec<usize> = metas
        .iter()
        .map(|m| intern(&m.harness, &m.id, &mut uf_parent))
        .collect();

    for (i, m) in metas.iter().enumerate() {
        let Some(first_parent) = m.derived_from.as_deref() else {
            continue;
        };
        has_child_in_items.insert((m.harness.clone(), first_parent.to_owned()));
        let mut cur_node = item_nodes[i];
        let mut next_parent = Some(first_parent.to_owned());
        let mut visited = HashSet::new();
        visited.insert(m.id.clone());
        let mut hops = 0usize;
        while let Some(pid) = next_parent.take() {
            if pid.is_empty() || !visited.insert(pid.clone()) || hops >= 16 {
                break;
            }
            let p_node = intern(&m.harness, &pid, &mut uf_parent);
            union(&mut uf_parent, cur_node, p_node);
            cur_node = p_node;
            hops += 1;
            if in_items.contains(&(m.harness.as_str(), pid.as_str())) {
                // Already in `items`; its own iteration links its parent.
                break;
            }
            if let Some(grand) = lookup_parent(&m.harness, &pid) {
                has_child_in_items.insert((m.harness.clone(), grand.clone()));
                next_parent = Some(grand);
            }
        }
    }

    // Group candidate indices by connected component root.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, &node) in item_nodes.iter().enumerate() {
        let root = find(&mut uf_parent, node);
        groups.entry(root).or_default().push(idx);
    }

    // Pick winner for each component and place at the component's earliest rank (`members[0]`).
    let mut slots: Vec<(usize, usize)> = Vec::with_capacity(groups.len());
    let mut collapsed = Vec::new();
    for members in groups.into_values() {
        let best_rank = members[0];
        if members.len() == 1 {
            slots.push((best_rank, members[0]));
            continue;
        }
        let winner = *members
            .iter()
            .max_by_key(|&&idx| {
                let m = &metas[idx];
                let is_tip = !has_child_in_items.contains(&(m.harness.clone(), m.id.clone()));
                (
                    m.message_count.unwrap_or(0),
                    is_tip,
                    m.updated_at,
                    m.size_bytes,
                    std::cmp::Reverse(idx),
                )
            })
            .expect("non-empty component");
        let kept_id = metas[winner].id.clone();
        for &idx in &members {
            if idx != winner && metas[idx].id != kept_id {
                collapsed.push((
                    best_rank,
                    idx,
                    CollapsedCopy {
                        harness: metas[idx].harness.clone(),
                        dropped_id: metas[idx].id.clone(),
                        kept_id: kept_id.clone(),
                    },
                ));
            }
        }
        slots.push((best_rank, winner));
    }

    slots.sort_unstable_by_key(|&(rank, _)| rank);
    collapsed.sort_unstable_by_key(|&(rank, idx, _)| (rank, idx));

    let mut indexed_items: Vec<Option<T>> = items.into_iter().map(Some).collect();
    let kept = slots
        .into_iter()
        .filter_map(|(_, winner)| indexed_items[winner].take())
        .collect();
    let dropped = collapsed.into_iter().map(|(_, _, c)| c).collect();
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_explicit_opening_continuations_and_ignores_unrelated_uuids() {
        let parent = "550e8400-e29b-41d4-a716-446655440000";
        let own = "660e8400-e29b-41d4-a716-446655440001";

        assert_eq!(
            detect_opening_text(
                &format!("This session is being continued from a previous conversation ({parent})."),
                Some(own),
            )
            .as_deref(),
            Some(parent)
        );
        assert_eq!(
            detect_opening_text(
                "[cv distill — resumed lane]\n# Distilled context — parser\n\nSource: `claude:wombatsess` · 42 messages",
                Some("newsess"),
            )
            .as_deref(),
            Some("wombatsess")
        );
        assert_eq!(
            detect_opening_text(&format!("[cv splice] grafted from {parent}"), Some(own)).as_deref(),
            Some(parent)
        );
        assert_eq!(
            detect_opening_text(&format!("Source session: {parent}\nSummary follows"), Some(own)).as_deref(),
            Some(parent)
        );

        // Self-reference never creates a self-derivation.
        assert_eq!(detect_opening_text(&format!("Continued from {own}"), Some(own)), None);

        // Ordinary prompt mentioning a UUID without continuation framing MUST NOT match.
        assert_eq!(
            detect_opening_text(
                &format!("Please debug why user record {parent} returns 404 in production"),
                Some(own),
            ),
            None
        );
    }

    #[test]
    fn head_probe_stops_after_two_turns_and_ignores_deep_transcript_mentions() {
        let deep_uuid = "99999999-e29b-41d4-a716-446655440000";
        let jsonl = format!(
            concat!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n",
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}\n",
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"continued from {deep}\"}}}}\n"
            ),
            deep = deep_uuid
        );
        assert_eq!(derived_from_head_str(&jsonl, Some("own")), None);

        // Turn 0 continuation DOES match.
        let opening_jsonl = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"Continued from session {deep_uuid}\"}}}}\n"
        );
        assert_eq!(
            derived_from_head_str(&opening_jsonl, Some("own")).as_deref(),
            Some(deep_uuid)
        );
    }

    #[test]
    fn collapse_derived_chains_picks_fullest_handles_cycles_and_preserves_rank() {
        #[derive(Clone, Debug)]
        struct Row {
            id: &'static str,
            from: Option<&'static str>,
            msgs: Option<usize>,
            ts: Option<i64>,
            size: Option<u64>,
        }
        let meta = |r: &Row| ChainMeta {
            harness: "claude".into(),
            id: r.id.into(),
            derived_from: r.from.map(str::to_owned),
            message_count: r.msgs,
            updated_at: r.ts,
            size_bytes: r.size,
        };

        // Chain 1: `fold2` (rank 0, 6 msgs) -> `fold1` (10 msgs) -> `orig` (rank 3, 50 msgs).
        // Unrelated `other` at rank 1. `orig` should win Chain 1 AND stay at rank 0 ahead of `other`!
        let rows = vec![
            Row {
                id: "fold2",
                from: Some("fold1"),
                msgs: Some(6),
                ts: Some(300),
                size: Some(1000),
            },
            Row {
                id: "other",
                from: None,
                msgs: Some(12),
                ts: Some(250),
                size: Some(2000),
            },
            Row {
                id: "fold1",
                from: Some("orig"),
                msgs: Some(10),
                ts: Some(200),
                size: Some(3000),
            },
            Row {
                id: "orig",
                from: None,
                msgs: Some(50),
                ts: Some(100),
                size: Some(9000),
            },
        ];
        let (kept, dropped) = collapse_derived_chains(rows, meta);
        let kept_ids: Vec<&str> = kept.iter().map(|r| r.id).collect();
        assert_eq!(kept_ids, ["orig", "other"]);
        assert_eq!(
            dropped,
            vec![
                CollapsedCopy {
                    harness: "claude".into(),
                    dropped_id: "fold2".into(),
                    kept_id: "orig".into(),
                },
                CollapsedCopy {
                    harness: "claude".into(),
                    dropped_id: "fold1".into(),
                    kept_id: "orig".into(),
                },
            ]
        );

        // Equal message_count tie-breaker: active chain tip (`tip`) beats superseded parent (`base`).
        let equal_msgs = vec![
            Row {
                id: "base",
                from: None,
                msgs: Some(20),
                ts: Some(100),
                size: Some(5000),
            },
            Row {
                id: "tip",
                from: Some("base"),
                msgs: Some(20),
                ts: Some(200),
                size: Some(5000),
            },
        ];
        let (kept, dropped) = collapse_derived_chains(equal_msgs, meta);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "tip");
        assert_eq!(dropped[0].dropped_id, "base");
        assert_eq!(dropped[0].kept_id, "tip");

        // Pathological cycle `c1 <-> c2` terminates cleanly and picks newest/largest.
        let cyclic = vec![
            Row {
                id: "c1",
                from: Some("c2"),
                msgs: Some(8),
                ts: Some(100),
                size: Some(500),
            },
            Row {
                id: "c2",
                from: Some("c1"),
                msgs: Some(8),
                ts: Some(200),
                size: Some(600),
            },
        ];
        let (kept, dropped) = collapse_derived_chains(cyclic, meta);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "c2");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].dropped_id, "c1");
    }
}
