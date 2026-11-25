//! Streaming canonical unit projection (`π_B`) and feature census for `port --strict`.
//!
//! Each message is consumed once in O(1) retained memory per block: text and tool-result bodies
//! stream in 64 KiB chunks into BLAKE3, tool-use JSON arguments stream through a key-sorted,
//! number-normalized canonical BLAKE3 hasher, media blocks (`Image`, `File`) hash their projected
//! metadata/references in block order, and tool-call identifiers are canonicalized into an
//! injective encounter-order bijection `ρ` so renamed IDs round-trip cleanly while crossed or
//! collapsed `tool_use_id` links fail immediately.

use super::*;
use crate::harness::claude::emit::SystemFate;
use crate::harness::claude::skills::SkillsPolicy;
use crate::json::JsonView;
use crate::lazy::{Resolver, Text};
use crate::stream::{Flow, MessageSink};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Maximum UTF-8 preview bytes kept per canonical unit for human-readable `Divergence` diagnostics.
const PREVIEW_MAX: usize = 64;

/// Canonical turn role in the ordered unit stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::emit) enum UnitRole {
    System,
    User,
    Assistant,
}

impl UnitRole {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Source or reparsed coordinates (`turn[i].block[j]`) for diagnostic pinpointing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::emit) struct UnitLoc {
    pub turn: usize,
    pub block: usize,
}

impl std::fmt::Display for UnitLoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "turn[{}].block[{}]", self.turn, self.block)
    }
}

/// One canonical semantic item in the ordered stream `π_B(S)` / `units_B(reparsed)`.
#[derive(Debug, Clone)]
pub(in crate::emit) struct Unit {
    pub loc: UnitLoc,
    pub kind: UnitKind,
}

#[derive(Debug, Clone)]
pub(in crate::emit) enum UnitKind {
    Turn {
        role: UnitRole,
    },
    Text {
        hash: [u8; 32],
        byte_len: u64,
        preview: String,
    },
    Thinking {
        text_hash: Option<[u8; 32]>,
        sig_hash: Option<[u8; 32]>,
        enc_hash: Option<[u8; 32]>,
        preview: String,
    },
    ToolUse {
        call_rho: u32,
        name: String,
        input_hash: [u8; 32],
        input_preview: String,
    },
    ToolResult {
        call_rho: u32,
        content_hash: [u8; 32],
        content_len: u64,
        is_error: Option<bool>,
        preview: String,
    },
    Image {
        hash: [u8; 32],
        preview: String,
    },
    File {
        hash: [u8; 32],
        preview: String,
    },
    Opaque {
        block_type: String,
        hash: [u8; 32],
    },
}

/// Semantic unit equality compares canonical BLAKE3 digests, lengths, roles, and `ρ`-ordinals
/// (never bounded display previews, which exist solely for human diagnostics).
impl PartialEq for UnitKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Turn { role: a }, Self::Turn { role: b }) => a == b,
            (
                Self::Text {
                    hash: ha, byte_len: la, ..
                },
                Self::Text {
                    hash: hb, byte_len: lb, ..
                },
            ) => ha == hb && la == lb,
            (
                Self::Thinking {
                    text_hash: ta,
                    sig_hash: sa,
                    enc_hash: ea,
                    ..
                },
                Self::Thinking {
                    text_hash: tb,
                    sig_hash: sb,
                    enc_hash: eb,
                    ..
                },
            ) => ta == tb && sa == sb && ea == eb,
            (
                Self::ToolUse {
                    call_rho: ra,
                    name: na,
                    input_hash: ia,
                    ..
                },
                Self::ToolUse {
                    call_rho: rb,
                    name: nb,
                    input_hash: ib,
                    ..
                },
            ) => ra == rb && na == nb && ia == ib,
            (
                Self::ToolResult {
                    call_rho: ra,
                    content_hash: ca,
                    content_len: la,
                    is_error: ea,
                    ..
                },
                Self::ToolResult {
                    call_rho: rb,
                    content_hash: cb,
                    content_len: lb,
                    is_error: eb,
                    ..
                },
            ) => ra == rb && ca == cb && la == lb && ea == eb,
            (Self::Image { hash: ha, .. }, Self::Image { hash: hb, .. }) => ha == hb,
            (Self::File { hash: ha, .. }, Self::File { hash: hb, .. }) => ha == hb,
            (
                Self::Opaque {
                    block_type: ta,
                    hash: ha,
                },
                Self::Opaque {
                    block_type: tb,
                    hash: hb,
                },
            ) => ta == tb && ha == hb,
            _ => false,
        }
    }
}

impl Eq for UnitKind {}

impl UnitKind {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Turn { .. } => "turn",
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::ToolUse { .. } => "tool_use",
            Self::ToolResult { .. } => "tool_result",
            Self::Image { .. } => "image",
            Self::File { .. } => "file",
            Self::Opaque { .. } => "opaque",
        }
    }

    pub(super) fn summary(&self) -> String {
        match self {
            Self::Turn { role } => format!("turn({})", role.as_str()),
            Self::Text { byte_len, preview, .. } => format!("text({byte_len}B {preview:?})"),
            Self::Thinking {
                text_hash,
                sig_hash,
                enc_hash,
                preview,
            } => format!(
                "thinking(text={}, sig={}, enc={}, {preview:?})",
                text_hash.is_some(),
                sig_hash.is_some(),
                enc_hash.is_some(),
            ),
            Self::ToolUse {
                call_rho,
                name,
                input_preview,
                ..
            } => format!("tool_use(ρ#{call_rho} {name} {input_preview})"),
            Self::ToolResult {
                call_rho,
                content_len,
                is_error,
                preview,
                ..
            } => match is_error {
                Some(err) => format!("tool_result(ρ#{call_rho} err={err} {content_len}B {preview:?})"),
                None => format!("tool_result(ρ#{call_rho} {content_len}B {preview:?})"),
            },
            Self::Image { preview, .. } => format!("image({preview})"),
            Self::File { preview, .. } => format!("file({preview})"),
            Self::Opaque { block_type, .. } => format!("opaque({block_type})"),
        }
    }
}

/// Complete projection output: the historical `Features` census plus the ordered canonical unit
/// stream `π_B` and `--skills` expected-strip accounting.
#[derive(Default, Debug)]
pub(in crate::emit) struct Projected {
    pub features: Features,
    pub skills_stripped: usize,
    pub units: Vec<Unit>,
}

/// Stateful injective canonicalizer `ρ` mapping raw tool-call IDs to encounter-order ordinals.
#[derive(Default, Debug, Clone)]
struct RhoMap {
    ids: HashMap<String, u32>,
    next: u32,
}

impl RhoMap {
    fn intern(&mut self, id: &str) -> u32 {
        if let Some(&rho) = self.ids.get(id) {
            return rho;
        }
        let rho = self.next;
        self.next += 1;
        self.ids.insert(id.to_owned(), rho);
        rho
    }
}

/// Streaming summary of one text leaf (never buffers full body).
#[derive(Clone, Debug)]
struct ScannedText {
    hash: [u8; 32],
    byte_len: u64,
    non_blank: bool,
    first_non_blank: Option<char>,
    starts_ignored_gemini: bool,
    preview: String,
}

/// Incremental multi-segment text hasher (`BLAKE3(s_0 || sep || s_1 || ...)` in O(1) memory).
#[derive(Clone)]
struct JoinedTextHasher {
    hasher: blake3::Hasher,
    byte_len: u64,
    non_blank: bool,
    preview: String,
    preview_done: bool,
    started: bool,
}

impl JoinedTextHasher {
    fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            byte_len: 0,
            non_blank: false,
            preview: String::new(),
            preview_done: false,
            started: false,
        }
    }

    fn push_str(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.hasher.update(chunk.as_bytes());
        self.byte_len += chunk.len() as u64;
        if !self.non_blank && chunk.chars().any(|c| !c.is_whitespace()) {
            self.non_blank = true;
        }
        append_preview_stateful(&mut self.preview, &mut self.preview_done, chunk);
    }

    fn push_text(&mut self, text: &Text, resolver: &Resolver, sep: &str) -> Result<()> {
        let mut local_any = false;
        text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
            if !chunk.is_empty() {
                if !local_any && self.started && !sep.is_empty() {
                    self.push_str(sep);
                }
                local_any = true;
                self.push_str(chunk);
            }
            Ok(())
        })?;
        if local_any {
            self.started = true;
        }
        Ok(())
    }

    /// Mirrors `codex/emit/writer.rs::message_string`: once `started` is true, each subsequent
    /// text-bearing block prepends `\n` before streaming its chunks (even if that block is `""`).
    fn push_codex_message_block(&mut self, text: &Text, resolver: &Resolver) -> Result<()> {
        if self.started {
            self.push_str("\n");
        }
        let mut non_empty = false;
        text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
            if !chunk.is_empty() {
                non_empty = true;
                self.push_str(chunk);
            }
            Ok(())
        })?;
        self.started |= non_empty;
        Ok(())
    }

    fn finish(self) -> ([u8; 32], u64, bool, String) {
        (
            *self.hasher.finalize().as_bytes(),
            self.byte_len,
            self.non_blank,
            self.preview,
        )
    }
}

fn append_preview(preview: &mut String, chunk: &str) {
    let mut done = false;
    append_preview_stateful(preview, &mut done, chunk);
}

fn append_preview_stateful(preview: &mut String, done: &mut bool, chunk: &str) {
    if *done || preview.len() >= PREVIEW_MAX {
        *done = true;
        return;
    }
    let slice = if preview.is_empty() {
        chunk.trim_start_matches([' ', '\t', '\r', '\n'])
    } else {
        chunk
    };
    for ch in slice.chars() {
        if matches!(ch, '\n' | '\r') {
            if !preview.is_empty() {
                *done = true;
                break;
            }
            continue;
        }
        if preview.len() + ch.len_utf8() > PREVIEW_MAX {
            *done = true;
            break;
        }
        preview.push(ch);
    }
}

fn scan_text(text: &Text, resolver: &Resolver) -> Result<ScannedText> {
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0u64;
    let mut first_non_blank: Option<char> = None;
    let mut lead = String::new();
    let mut preview = String::new();
    let mut preview_done = false;
    text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
        if chunk.is_empty() {
            return Ok(());
        }
        hasher.update(chunk.as_bytes());
        byte_len += chunk.len() as u64;
        if first_non_blank.is_none() {
            let trimmed = chunk.trim_start();
            if let Some(ch) = trimmed.chars().next() {
                first_non_blank = Some(ch);
                for c in trimmed.chars().take(20) {
                    lead.push(c);
                }
            }
        } else if lead.len() < 20 {
            for c in chunk.chars().take(20 - lead.len()) {
                lead.push(c);
            }
        }
        append_preview_stateful(&mut preview, &mut preview_done, chunk);
        Ok(())
    })?;
    let starts_ignored_gemini = matches!(first_non_blank, Some('/' | '?'))
        || lead.starts_with("<session_context>")
        || lead.starts_with("<hook_context>");
    Ok(ScannedText {
        hash: *hasher.finalize().as_bytes(),
        byte_len,
        non_blank: first_non_blank.is_some(),
        first_non_blank,
        starts_ignored_gemini,
        preview,
    })
}

/// Preserve the first nonblank character classification consumed by `is_client_command`:
/// `/` and `?` are commands, `x` is other nonblank text, and empty is blank.
pub(super) fn marker(text: &Text, resolver: &Resolver) -> Result<String> {
    Ok(match crate::emit::thinking::first_character(text, resolver, true)? {
        Some(ch @ ('/' | '?')) => ch.to_string(),
        Some(_) => "x".into(),
        None => String::new(),
    })
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn hash_pair(a: &str, b: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(a.len() as u64).to_le_bytes());
    h.update(a.as_bytes());
    h.update(&(b.len() as u64).to_le_bytes());
    h.update(b.as_bytes());
    *h.finalize().as_bytes()
}

/// Canonical JSON BLAKE3 digest: object keys sorted by UTF-8 bytes, duplicate keys resolved last-wins,
/// numbers canonicalized (`-0.0 == 0.0 == 0`, exact integral floats matched to integers, 64-bit integer
/// precision preserved), strings streamed in chunks without full-string allocation.
fn hash_canonical_json_view(view: JsonView<'_>, hasher: &mut blake3::Hasher) -> Result<()> {
    if view.is_null()? {
        hasher.update(b"N");
        return Ok(());
    }
    if let Some(b) = view.as_bool()? {
        hasher.update(if b { b"T" } else { b"F" });
        return Ok(());
    }
    if let Some(n) = view.as_number()? {
        if let Some(i) = n.as_i64() {
            hasher.update(b"I");
            hasher.update(&(i as i128).to_le_bytes());
        } else if let Some(u) = n.as_u64() {
            hasher.update(b"I");
            hasher.update(&(u as i128).to_le_bytes());
        } else if let Some(f) = n.as_f64() {
            if f == 0.0 {
                hasher.update(b"I");
                hasher.update(&0i128.to_le_bytes());
            } else if f.is_finite() && f.fract() == 0.0 && f >= (i64::MIN as f64) && f < (i64::MAX as f64) {
                hasher.update(b"I");
                hasher.update(&((f as i64) as i128).to_le_bytes());
            } else {
                hasher.update(b"D");
                hasher.update(&f.to_bits().to_le_bytes());
            }
        }
        return Ok(());
    }
    let mut str_len = 0u64;
    let is_str = view.for_each_str_chunk(|chunk| {
        if str_len == 0 {
            hasher.update(b"S");
        }
        hasher.update(chunk.as_bytes());
        str_len += chunk.len() as u64;
        Ok(())
    })?;
    if is_str {
        if str_len == 0 {
            hasher.update(b"S");
        }
        hasher.update(&[0]);
        hasher.update(&str_len.to_le_bytes());
        return Ok(());
    }
    if let Some(arr) = view.array()? {
        hasher.update(b"[");
        let mut count = 0u64;
        for item in arr {
            hash_canonical_json_view(item?, hasher)?;
            count += 1;
        }
        hasher.update(b"]");
        hasher.update(&count.to_le_bytes());
        return Ok(());
    }
    let mut entries: Vec<(String, [u8; 32])> = Vec::new();
    let is_obj = view.for_each_entry(|key, val| {
        let mut child = blake3::Hasher::new();
        hash_canonical_json_view(val, &mut child)?;
        let digest = *child.finalize().as_bytes();
        if let Some(existing) = entries.iter_mut().find(|(k, _)| k == key) {
            existing.1 = digest;
        } else {
            entries.push((key.to_owned(), digest));
        }
        Ok(())
    })?;
    if is_obj {
        entries.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
        hasher.update(b"{");
        for (k, d) in &entries {
            hasher.update(&(k.len() as u64).to_le_bytes());
            hasher.update(k.as_bytes());
            hasher.update(d);
        }
        hasher.update(b"}");
        hasher.update(&(entries.len() as u64).to_le_bytes());
    }
    Ok(())
}

fn empty_object_hash() -> Result<[u8; 32]> {
    let empty = serde_json::json!({});
    let mut h = blake3::Hasher::new();
    hash_canonical_json_view(JsonView::from(&empty), &mut h)?;
    Ok(*h.finalize().as_bytes())
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum OrphanShell {
    #[default]
    None,
    OpenCode,
    Gemini,
    Codex,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum EmptyTurnDrop {
    #[default]
    None,
    Qwen,
    LmStudioUnlessUsage,
}

/// Per-harness emitter/re-parser wire behavior table (exhaustive over [`Harness::ALL`]).
#[derive(Clone, Copy, Default)]
struct TargetProfile {
    openai_args: bool,
    codex_custom_raw_wrap: bool,
    folds_tool_result: bool,
    renders_tools_as_text: bool,
    assistant_splits_per_block: bool,
    separated_assistant: bool,
    join_thinking: bool,
    skip_blank_text: bool,
    files_after_tool_calls: bool,
    drops_empty_turn: EmptyTurnDrop,
    omits_system_prompt_rows: bool,
    empty_assistant_default_turn: bool,
    reroles_non_prompt_user: bool,
    orphan_result_shell: OrphanShell,
    native_thinking_as_text: bool,
    drops_foreign_encrypted: bool,
    codex_message_join: bool,
    claude_non_user_file_text: bool,
    opencode_non_assistant_unavail_text: bool,
    strips_claude_skills: bool,
}

fn target_profile(h: Harness) -> TargetProfile {
    let base = TargetProfile::default();
    match h {
        Harness::Claude => TargetProfile {
            empty_assistant_default_turn: true,
            claude_non_user_file_text: true,
            strips_claude_skills: true,
            ..base
        },
        Harness::Codex => TargetProfile {
            openai_args: true,
            codex_custom_raw_wrap: true,
            assistant_splits_per_block: true,
            orphan_result_shell: OrphanShell::Codex,
            codex_message_join: true,
            ..base
        },
        Harness::Grok => TargetProfile {
            openai_args: true,
            separated_assistant: true,
            join_thinking: true,
            drops_foreign_encrypted: true,
            ..base
        },
        Harness::OpenCode => TargetProfile {
            folds_tool_result: true,
            orphan_result_shell: OrphanShell::OpenCode,
            opencode_non_assistant_unavail_text: true,
            ..base
        },
        Harness::Gemini => TargetProfile {
            folds_tool_result: true,
            separated_assistant: true,
            orphan_result_shell: OrphanShell::Gemini,
            ..base
        },
        Harness::Hermes => TargetProfile {
            openai_args: true,
            separated_assistant: true,
            join_thinking: true,
            ..base
        },
        Harness::LmStudio => TargetProfile {
            renders_tools_as_text: true,
            drops_empty_turn: EmptyTurnDrop::LmStudioUnlessUsage,
            ..base
        },
        Harness::Continue => TargetProfile {
            openai_args: true,
            separated_assistant: true,
            files_after_tool_calls: true,
            native_thinking_as_text: true,
            ..base
        },
        Harness::Qwen => TargetProfile {
            skip_blank_text: true,
            drops_empty_turn: EmptyTurnDrop::Qwen,
            omits_system_prompt_rows: true,
            reroles_non_prompt_user: true,
            ..base
        },
        Harness::OpenClaw
        | Harness::Cline
        | Harness::Roo
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Pi
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => base,
    }
}

/// Hash a `ToolUse.input` modulo target wire canonicalization rules:
/// - OpenAI `function.arguments` stores (`Codex`, `Grok`, `Hermes`, `Continue`) decode `null` or `""`
///   arguments to `{}`;
/// - `Codex` `custom_tool_call` (`exec` / `apply_patch` with a raw string input `"s"`) decodes back
///   as `{"raw": "s"}`.
fn hash_tool_input(target: Harness, name: &str, input: &crate::Json) -> Result<([u8; 32], String)> {
    let prof = target_profile(target);
    input.with_view(|view| {
        let (mut preview, truncated) = view.canonical_prefix(PREVIEW_MAX)?;
        if truncated {
            preview.push('…');
        }
        if prof.codex_custom_raw_wrap && matches!(name, "exec" | "apply_patch") && view.is_string()? {
            let mut str_hasher = blake3::Hasher::new();
            hash_canonical_json_view(view, &mut str_hasher)?;
            let val_digest = *str_hasher.finalize().as_bytes();
            let mut obj_hasher = blake3::Hasher::new();
            obj_hasher.update(b"{");
            obj_hasher.update(&3u64.to_le_bytes());
            obj_hasher.update(b"raw");
            obj_hasher.update(&val_digest);
            obj_hasher.update(b"}");
            obj_hasher.update(&1u64.to_le_bytes());
            return Ok((*obj_hasher.finalize().as_bytes(), preview));
        }
        if prof.openai_args {
            let empty_or_null = if view.is_null()? {
                true
            } else if view.is_string()? {
                let mut blank = true;
                view.for_each_str_chunk(|c| {
                    if !c.trim().is_empty() {
                        blank = false;
                    }
                    Ok(())
                })?;
                blank
            } else {
                false
            };
            if empty_or_null {
                return Ok((empty_object_hash()?, "{}".into()));
            }
        }
        let mut h = blake3::Hasher::new();
        hash_canonical_json_view(view, &mut h)?;
        Ok((*h.finalize().as_bytes(), preview))
    })
}

/// Canonical projection of a `Block::Image` onto `(hash, preview)` or degraded text on `target`.
enum ImageProjection {
    Media { hash: [u8; 32], preview: String },
    DegradedText(String),
    Dropped,
}

fn finish_image_media(
    media_type: Option<&str>,
    data_ref: Option<&str>,
    trunc_80: bool,
    mime_from_uri_only: bool,
) -> ImageProjection {
    let norm_ref: std::borrow::Cow<'_, str> = match data_ref {
        Some(r) if trunc_80 && r.chars().count() > 80 => std::borrow::Cow::Owned(r.chars().take(80).collect()),
        Some(r) => std::borrow::Cow::Borrowed(r),
        None => std::borrow::Cow::Borrowed(""),
    };
    let uri_mime = || {
        data_ref
            .and_then(|r| r.strip_prefix("data:"))
            .and_then(|rest| rest.split_once(';'))
            .map(|(m, _)| m)
            .filter(|m| !m.is_empty())
    };
    let norm_mime = if mime_from_uri_only {
        uri_mime().unwrap_or("")
    } else {
        media_type.filter(|m| !m.is_empty()).or_else(uri_mime).unwrap_or("")
    };
    let mut prev = String::new();
    append_preview(&mut prev, if !norm_ref.is_empty() { &norm_ref } else { norm_mime });
    ImageProjection::Media {
        hash: hash_pair(norm_mime, &norm_ref),
        preview: prev,
    }
}

fn project_image(
    target: Harness,
    role: UnitRole,
    is_source: bool,
    media_type: Option<&str>,
    data_ref: Option<&str>,
) -> ImageProjection {
    match target {
        Harness::LmStudio => {
            if media_type.is_some() || data_ref.is_some() {
                let r = data_ref.unwrap_or("");
                let mut prev = String::new();
                append_preview(&mut prev, if r.is_empty() { "image" } else { r });
                ImageProjection::Media {
                    hash: hash_pair("", r),
                    preview: prev,
                }
            } else {
                ImageProjection::Dropped
            }
        }
        Harness::Codex => {
            if role == UnitRole::Assistant {
                ImageProjection::Dropped
            } else {
                finish_image_media(media_type, data_ref, false, true)
            }
        }
        Harness::Hermes => finish_image_media(media_type, data_ref, false, true),
        Harness::Continue => {
            if data_ref.is_none() {
                ImageProjection::Dropped
            } else {
                finish_image_media(media_type, data_ref, false, true)
            }
        }
        Harness::OpenCode => {
            if is_source && role != UnitRole::Assistant {
                if let Some(unavail) = opencode_attachment_unavailable("Image", media_type, data_ref) {
                    return ImageProjection::DegradedText(unavail);
                }
            }
            finish_image_media(media_type, data_ref, false, false)
        }
        Harness::Gemini | Harness::Qwen => {
            if let Some((uri_mime, b64)) = data_ref
                .and_then(|r| r.strip_prefix("data:"))
                .and_then(|rest| rest.split_once(";base64,"))
            {
                let eff_mime = if !uri_mime.is_empty() {
                    uri_mime
                } else {
                    media_type
                        .filter(|m| !m.is_empty())
                        .unwrap_or("application/octet-stream")
                };
                let canonical_uri = format!("data:{eff_mime};base64,{b64}");
                let mut prev = String::new();
                append_preview(&mut prev, &canonical_uri);
                return ImageProjection::Media {
                    hash: hash_pair(eff_mime, &canonical_uri),
                    preview: prev,
                };
            }
            if is_source {
                let label = data_ref
                    .filter(|s| !s.is_empty())
                    .or_else(|| media_type.filter(|s| !s.is_empty()));
                let text = match label {
                    Some(l) => format!("[image: {l}]"),
                    None => "[image]".to_owned(),
                };
                return ImageProjection::DegradedText(text);
            }
            finish_image_media(media_type, data_ref, false, false)
        }
        Harness::OpenClaw => finish_image_media(media_type, data_ref, true, false),
        Harness::Claude | Harness::Cline | Harness::Roo => finish_image_media(media_type, data_ref, false, false),
        Harness::Grok
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Pi
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => {
            if target_holds(target, Field::Image) {
                finish_image_media(media_type, data_ref, false, false)
            } else {
                ImageProjection::Dropped
            }
        }
    }
}

/// Canonical projection of a `Block::File` onto `(hash, preview)` or degraded text on `target`.
enum FileProjection {
    Media { hash: [u8; 32], preview: String },
    DegradedText(String),
    Dropped,
}

fn project_file(
    target: Harness,
    role: UnitRole,
    is_source: bool,
    mime: Option<&str>,
    path: Option<&str>,
    source: Option<&str>,
) -> FileProjection {
    let label_fallback = || path.or(source).unwrap_or("?");
    match target {
        Harness::Codex => {
            if is_source {
                return FileProjection::DegradedText(format!("[file: {}]", label_fallback()));
            }
            FileProjection::Dropped
        }
        Harness::Claude | Harness::Cline | Harness::Roo => {
            if role == UnitRole::User {
                let m = mime.unwrap_or("");
                let s = source.unwrap_or("");
                let p = path.unwrap_or("");
                let mut h = blake3::Hasher::new();
                for part in [m, p, s] {
                    h.update(&(part.len() as u64).to_le_bytes());
                    h.update(part.as_bytes());
                }
                let mut prev = String::new();
                append_preview(&mut prev, if !p.is_empty() { p } else { s });
                FileProjection::Media {
                    hash: *h.finalize().as_bytes(),
                    preview: prev,
                }
            } else if is_source {
                FileProjection::DegradedText(format!("[file: {}]", label_fallback()))
            } else {
                FileProjection::Dropped
            }
        }
        Harness::OpenClaw => {
            if is_source {
                FileProjection::DegradedText(format!("[file: {}]", label_fallback()))
            } else {
                FileProjection::Dropped
            }
        }
        Harness::OpenCode => {
            let uri = source.or(path);
            if is_source && role != UnitRole::Assistant {
                if let Some(unavail) = opencode_attachment_unavailable("File", mime, uri) {
                    return FileProjection::DegradedText(unavail);
                }
            }
            let u = uri.unwrap_or("");
            let m = mime.unwrap_or("");
            let mut prev = String::new();
            append_preview(&mut prev, u);
            FileProjection::Media {
                hash: hash_pair(m, u),
                preview: prev,
            }
        }
        Harness::Gemini | Harness::Qwen => {
            let u = source.or(path).unwrap_or("");
            let m = mime.unwrap_or("");
            let mut prev = String::new();
            append_preview(&mut prev, u);
            FileProjection::Media {
                hash: hash_pair(m, u),
                preview: prev,
            }
        }
        Harness::Continue => {
            let u = path.or(source).unwrap_or("");
            let m = mime.unwrap_or("");
            let mut prev = String::new();
            append_preview(&mut prev, u);
            FileProjection::Media {
                hash: hash_pair(m, u),
                preview: prev,
            }
        }
        Harness::LmStudio => {
            if path.is_some() || source.is_some() {
                let u = source.or(path).unwrap_or("");
                let mut prev = String::new();
                append_preview(&mut prev, u);
                FileProjection::Media {
                    hash: hash_pair("", u),
                    preview: prev,
                }
            } else {
                FileProjection::Dropped
            }
        }
        Harness::Hermes
        | Harness::Grok
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Pi
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => {
            if target_holds(target, Field::File) {
                let u = source.or(path).unwrap_or("");
                let m = mime.unwrap_or("");
                let mut prev = String::new();
                append_preview(&mut prev, u);
                FileProjection::Media {
                    hash: hash_pair(m, u),
                    preview: prev,
                }
            } else {
                FileProjection::Dropped
            }
        }
    }
}

/// Pre-scanned block facts retained only until `finish` resolves forward references (`answered` /
/// folded `ToolResult`s). Every payload is already reduced to its 32-byte BLAKE3 digest + small
/// preview, so retaining `ScannedMessage` across a 10,000-turn transcript uses O(turns), never O(bytes).
#[derive(Clone, Debug)]
enum ScannedBlock {
    Text {
        loc: UnitLoc,
        scanned: ScannedText,
    },
    Thinking {
        loc: UnitLoc,
        text: ScannedText,
        codex_summary: Option<ScannedText>,
        signature: Option<([u8; 32], usize)>,
        encrypted: Option<([u8; 32], usize)>,
        redacted: bool,
    },
    ToolUse {
        loc: UnitLoc,
        id: String,
        name: String,
        input_hash: [u8; 32],
        input_preview: String,
        lmstudio_text: Option<([u8; 32], u64, String)>,
    },
    ToolResult {
        loc: UnitLoc,
        tool_use_id: String,
        content: ScannedText,
        tool_name: Option<String>,
        is_error: bool,
        has_details: bool,
        lmstudio_text: Option<([u8; 32], u64, String)>,
    },
    Image {
        loc: UnitLoc,
        media_type: Option<String>,
        data_ref: Option<String>,
    },
    File {
        loc: UnitLoc,
        mime: Option<String>,
        path: Option<String>,
        source: Option<String>,
    },
    Opaque {
        loc: UnitLoc,
        block_type: String,
        hash: [u8; 32],
    },
}

type JoinedTurnText = ([u8; 32], u64, bool, String);

#[derive(Clone, Debug)]
struct ScannedMessage {
    turn_idx: usize,
    written_from: Option<Harness>,
    role: Role,
    kind: MessageKind,
    origin: crate::ir::Origin,
    synthesized: bool,
    carrier: bool,
    claude_raw_content: bool,
    skills_stripped: bool,
    has_id: bool,
    timestamp: Option<DateTime<Utc>>,
    has_model: bool,
    usage: Option<crate::ir::Usage>,
    hermes_verbatim_reasoning: Option<([u8; 32], Option<[u8; 32]>, String)>,
    joined_thinking: Option<([u8; 32], String)>,
    joined_text: Option<JoinedTurnText>,
    lmstudio_non_selected_variant: bool,
    blocks: Vec<ScannedBlock>,
}

/// Fidelity projection context derived from [`EmitOptions`].
#[derive(Clone, Default)]
pub(super) struct FidelityContext {
    pub skills_policy: SkillsPolicy,
    pub skills_config: Option<PathBuf>,
    pub effective_cwd: Option<PathBuf>,
}

pub(super) struct Accumulator {
    target: Harness,
    thinking: ThinkingMode,
    ctx: FidelityContext,
    resolver: Resolver,
    latch: crate::stream::Latch,
    written_from: Option<Harness>,
    messages: Vec<ScannedMessage>,
    turn_counter: usize,
}

impl Accumulator {
    pub(super) fn new(
        target: Harness,
        thinking: ThinkingMode,
        resolver: Resolver,
        written_from: Option<Harness>,
    ) -> Self {
        Self::with_context(target, thinking, FidelityContext::default(), resolver, written_from)
    }

    pub(super) fn with_context(
        target: Harness,
        thinking: ThinkingMode,
        ctx: FidelityContext,
        resolver: Resolver,
        written_from: Option<Harness>,
    ) -> Self {
        Self {
            target,
            thinking,
            ctx,
            resolver,
            latch: Default::default(),
            written_from,
            messages: Vec::new(),
            turn_counter: 0,
        }
    }

    pub(super) fn note(&mut self, message: &Message) -> Result<()> {
        self.note_from(message, self.written_from)
    }

    pub(super) fn note_from(&mut self, message: &Message, written_from: Option<Harness>) -> Result<()> {
        let turn_idx = self.turn_counter;
        self.turn_counter += 1;
        let prof = target_profile(self.target);
        let (codex_summary, hermes_verbatim_reasoning, lmstudio_non_selected_variant) =
            target_bag_facts(self.target, written_from, self.thinking, message, &self.resolver)?;

        let skills_stripped = written_from.is_some()
            && prof.strips_claude_skills
            && self.ctx.skills_policy != SkillsPolicy::Keep
            && crate::harness::claude::skills::strips_rendered(
                message,
                self.ctx.skills_policy,
                self.ctx.skills_config.as_deref(),
                self.ctx.effective_cwd.as_deref(),
            );

        let joined_thinking =
            if prof.join_thinking && message.role == Role::Assistant && hermes_verbatim_reasoning.is_none() {
                compute_joined_thinking(self.target, self.thinking, message, &self.resolver)?
            } else {
                None
            };

        let join_sep = turn_text_join_separator(self.target, written_from, message);
        let joined_text = match join_sep {
            Some(sep) => compute_joined_turn_text(
                self.target,
                self.thinking,
                written_from.is_some(),
                message,
                sep,
                &self.resolver,
            )?,
            None => None,
        };

        let mut blocks = Vec::with_capacity(message.content.len());
        let mut used_codex_summary = false;
        for (block_idx, block) in message.content.iter().enumerate() {
            let loc = UnitLoc {
                turn: turn_idx,
                block: block_idx,
            };
            blocks.push(match block {
                Block::Text { text, .. } => ScannedBlock::Text {
                    loc,
                    scanned: scan_text(text, &self.resolver)?,
                },
                Block::Thinking {
                    text,
                    signature,
                    encrypted,
                    redacted,
                    ..
                } => ScannedBlock::Thinking {
                    loc,
                    text: scan_text(text, &self.resolver)?,
                    codex_summary: if !used_codex_summary {
                        used_codex_summary = true;
                        codex_summary.clone()
                    } else {
                        None
                    },
                    signature: signature.as_deref().map(|s| (hash_bytes(s.as_bytes()), s.len())),
                    encrypted: encrypted.as_deref().map(|e| (hash_bytes(e.as_bytes()), e.len())),
                    redacted: *redacted,
                },
                Block::ToolUse { id, name, input, .. } => {
                    let (input_hash, input_preview) = hash_tool_input(self.target, name, input)?;
                    let lmstudio_text = if prof.renders_tools_as_text && written_from.is_some() {
                        let mut compact = Vec::new();
                        input.write_canonical(&mut compact)?;
                        let rendered = format!("[tool call: {name} {}]", String::from_utf8_lossy(&compact));
                        let mut preview = String::new();
                        append_preview(&mut preview, &rendered);
                        Some((hash_bytes(rendered.as_bytes()), rendered.len() as u64, preview))
                    } else {
                        None
                    };
                    ScannedBlock::ToolUse {
                        loc,
                        id: id.clone(),
                        name: name.clone(),
                        input_hash,
                        input_preview,
                        lmstudio_text,
                    }
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    tool_name,
                    is_error,
                    details,
                    ..
                } => {
                    let lmstudio_text = if prof.renders_tools_as_text && written_from.is_some() {
                        let mut j = JoinedTextHasher::new();
                        j.push_str("[tool result: ");
                        j.push_text(content, &self.resolver, "")?;
                        j.push_str("]");
                        let (h, len, _, prev) = j.finish();
                        Some((h, len, prev))
                    } else {
                        None
                    };
                    ScannedBlock::ToolResult {
                        loc,
                        tool_use_id: tool_use_id.clone(),
                        content: scan_text(content, &self.resolver)?,
                        tool_name: tool_name.clone(),
                        is_error: *is_error,
                        has_details: details.is_some(),
                        lmstudio_text,
                    }
                }
                Block::Image {
                    media_type, data_ref, ..
                } => ScannedBlock::Image {
                    loc,
                    media_type: media_type.clone(),
                    data_ref: data_ref.clone(),
                },
                Block::File { mime, path, source, .. } => ScannedBlock::File {
                    loc,
                    mime: mime.clone(),
                    path: path.clone(),
                    source: source.clone(),
                },
                Block::Opaque { block_type, value } => {
                    let mut h = blake3::Hasher::new();
                    h.update(&(block_type.len() as u64).to_le_bytes());
                    h.update(block_type.as_bytes());
                    hash_canonical_json_view(JsonView::from(value), &mut h)?;
                    ScannedBlock::Opaque {
                        loc,
                        block_type: block_type.clone(),
                        hash: *h.finalize().as_bytes(),
                    }
                }
            });
        }

        let claude_raw_content = written_from.is_some_and(|from| source_has_raw_content_override(from, message));

        self.messages.push(ScannedMessage {
            turn_idx,
            written_from,
            role: message.role,
            kind: message.kind,
            origin: message.origin,
            synthesized: message.is_synthesized(),
            carrier: message.is_carrier(),
            claude_raw_content,
            skills_stripped,
            has_id: message.id.is_some(),
            timestamp: message.timestamp,
            has_model: message.model.is_some(),
            usage: message.usage.clone(),
            hermes_verbatim_reasoning,
            joined_thinking,
            joined_text,
            lmstudio_non_selected_variant,
            blocks,
        });
        Ok(())
    }

    pub(super) fn finish(self, metadata: &Session) -> Result<Projected> {
        let mut features = Features::default();
        let mut skills_stripped = 0usize;
        let rules = write_rules(self.target);
        let session_has_model = metadata.model.is_some();

        // Collect answered call IDs (for Claude's `answered_calls_only`), called IDs, and all
        // `ToolResult`s per `tool_use_id` in encounter sequence (never overwriting duplicates).
        let mut answered = HashSet::<&str>::new();
        let mut called = HashSet::<&str>::new();
        let mut folded_results: HashMap<&str, Vec<&ScannedBlock>> = HashMap::new();
        for msg in &self.messages {
            for b in &msg.blocks {
                match b {
                    ScannedBlock::ToolUse { id, .. } => {
                        called.insert(id.as_str());
                    }
                    ScannedBlock::ToolResult { tool_use_id, .. } => {
                        answered.insert(tool_use_id.as_str());
                        if msg.role == Role::Tool {
                            folded_results.entry(tool_use_id.as_str()).or_default().push(b);
                        }
                    }
                    _ => {}
                }
            }
        }

        let is_orphan_call = |b: &ScannedBlock| -> bool {
            rules.answered_calls_only
                && matches!(b, ScannedBlock::ToolUse { id, .. } if !answered.contains(id.as_str()))
        };

        // 1. Populate feature counts (exact parity with capability table + `SkillsPolicy` stripping).
        for msg in &self.messages {
            if msg.skills_stripped {
                skills_stripped += 1;
                continue;
            }
            let fate = msg
                .written_from
                .zip(rules.system_fate)
                .map(|(source, f)| f(source, &stub_message_for_system_fate(msg)));
            let unrepresentable = scanned_unrepresentable(self.target, self.thinking, msg);
            if !msg.carrier && unrepresentable {
                features.dropped += 1;
            }
            let bookkeeping =
                msg.kind == MessageKind::Carrier || (msg.carrier && msg.blocks.is_empty() && !msg.synthesized);
            if bookkeeping || unrepresentable || (!msg.blocks.is_empty() && msg.blocks.iter().all(&is_orphan_call)) {
                continue;
            }
            let (role, kind) = if rules.reroles_client_commands && scanned_is_client_command(msg) {
                ("system", MessageKind::InjectedContext.as_str())
            } else if fate == Some(SystemFate::Special(MessageKind::CompactionSummary)) {
                ("user", msg.kind.as_str())
            } else {
                (role_str(msg.role), msg.kind.as_str())
            };
            *features.role_counts.entry(role).or_default() += 1;
            *features.kind_counts.entry(kind).or_default() += 1;

            let tool_turn = msg.role == Role::Tool;
            if msg.timestamp.is_some() {
                if tool_turn {
                    features.timestamps_tool += 1;
                } else {
                    features.timestamps += 1;
                }
            }
            if msg.has_id {
                if tool_turn {
                    features.ids_tool += 1;
                } else {
                    features.ids += 1;
                }
            }
            if let (Role::Assistant, Some(usage)) = (msg.role, &msg.usage) {
                features.usage += 1;
                if !target_holds(self.target, Field::UsageCacheCreation) && usage.cache_creation_tokens.is_some() {
                    features.unheld_cache_creation += 1;
                }
                if usage.cost_usd.is_some() {
                    features.cost += 1;
                }
            }
            if msg.has_model || session_has_model {
                features.model_eff += 1;
            }
            for b in msg.blocks.iter().filter(|b| !is_orphan_call(b)) {
                match b {
                    ScannedBlock::Text { .. } => features.text += 1,
                    ScannedBlock::ToolUse { .. } => features.tool_use += 1,
                    ScannedBlock::Opaque { .. } => features.opaque += 1,
                    ScannedBlock::Image { .. } => features.image += 1,
                    ScannedBlock::File { .. } => features.file += 1,
                    ScannedBlock::ToolResult {
                        tool_name,
                        is_error,
                        has_details,
                        ..
                    } => {
                        features.tool_result += 1;
                        if tool_name.as_deref().is_some_and(|n| !n.is_empty()) {
                            features.results_with_name += 1;
                        }
                        if *is_error {
                            features.results_is_error += 1;
                        }
                        if *has_details {
                            features.results_with_details += 1;
                        }
                    }
                    ScannedBlock::Thinking {
                        text,
                        signature,
                        encrypted,
                        ..
                    } => {
                        if text.non_blank {
                            features.thinking_text += 1;
                        }
                        if signature.is_some() {
                            features.thinking_sig += 1;
                        }
                        if encrypted.is_some() {
                            features.thinking_enc += 1;
                        }
                    }
                }
            }
        }
        if session_has_model {
            features.model_eff = features.role_counts.values().sum();
        }

        // 2. Build canonical ordered unit stream (`π_B(S)` on source side, `units_B(R)` on reparsed side).
        let units = build_canonical_units(
            self.target,
            self.thinking,
            &self.messages,
            &answered,
            &called,
            &folded_results,
        )?;
        Ok(Projected {
            features,
            skills_stripped,
            units,
        })
    }
}

impl MessageSink for Accumulator {
    fn message(&mut self, message: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        match self.note(&message) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.latch.fail(error);
                Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("fidelity projection")
    }
    fn meta(&mut self, session: &Session) {
        self.resolver = session.resolver();
    }
}

/// The features of `session` as `target`'s writer is about to write it (used by source validation
/// and the `fidelity-features.txt` census golden).
pub(in crate::emit) fn source_features(target: Harness, thinking: ThinkingMode, session: &Session) -> Result<Features> {
    Ok(source_projected_with_opts(
        target,
        session,
        &EmitOptions {
            thinking,
            ..Default::default()
        },
    )?
    .features)
}

pub(in crate::emit) fn source_projected_with_opts(
    target: Harness,
    session: &Session,
    opts: &EmitOptions,
) -> Result<Projected> {
    let sources = crate::splice::message_harnesses(session);
    let ctx = FidelityContext {
        skills_policy: opts.skills_policy,
        skills_config: opts.skills_config.clone(),
        effective_cwd: crate::emit::effective_cwd(session, opts),
    };
    let mut accumulator =
        Accumulator::with_context(target, opts.thinking, ctx, session.resolver(), Some(session.harness));
    for (i, message) in session.messages.iter().enumerate() {
        let from = sources
            .as_ref()
            .and_then(|s| s.get(i).copied())
            .unwrap_or(session.harness);
        accumulator.note_from(message, Some(from))?;
    }
    accumulator.finish(session)
}

/// The projection of `session` as `target`'s reader read it back.
#[cfg(test)]
pub(in crate::emit) fn read_projected(target: Harness, thinking: ThinkingMode, session: &Session) -> Result<Projected> {
    let mut accumulator = Accumulator::new(target, thinking, session.resolver(), None);
    for message in &session.messages {
        accumulator.note(message)?;
    }
    accumulator.finish(session)
}

type HermesVerbatimReasoning = ([u8; 32], Option<[u8; 32]>, String);

fn target_bag_facts(
    target: Harness,
    written_from: Option<Harness>,
    thinking: ThinkingMode,
    message: &Message,
    resolver: &Resolver,
) -> Result<(Option<ScannedText>, Option<HermesVerbatimReasoning>, bool)> {
    match target {
        Harness::Codex => {
            let summary = written_from
                .is_some()
                .then(|| {
                    message
                        .harness_extra(Harness::Codex)
                        .and_then(|bag| bag.get("reasoning_summary"))
                        .and_then(Value::as_str)
                })
                .flatten()
                .map(|s| scan_text(&Text::Inline(s.to_owned()), resolver))
                .transpose()?;
            Ok((summary, None, false))
        }
        Harness::Hermes => {
            let reasoning = if written_from.is_some() && thinking != ThinkingMode::Drop {
                extract_hermes_verbatim_reasoning(target, message)
            } else {
                None
            };
            Ok((None, reasoning, false))
        }
        Harness::LmStudio => {
            let variant = message.kind == MessageKind::Branch
                && message
                    .harness_extra(Harness::LmStudio)
                    .and_then(|b| b.get("variant"))
                    .is_some();
            Ok((None, None, variant))
        }
        Harness::Claude
        | Harness::Grok
        | Harness::OpenCode
        | Harness::OpenClaw
        | Harness::Gemini
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Qwen
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Pi
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => Ok((None, None, false)),
    }
}

fn source_has_raw_content_override(from: Harness, message: &Message) -> bool {
    match from {
        Harness::Claude => message.harness_extra(Harness::Claude).is_some_and(|ex| {
            ex.contains_key(&format!("{}content", crate::harness::claude::MESSAGE_EXTRA_PREFIX))
                || ex.get("message").and_then(|v| v.get("content")).is_some()
        }),
        Harness::Codex
        | Harness::Grok
        | Harness::OpenCode
        | Harness::OpenClaw
        | Harness::Gemini
        | Harness::Hermes
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Qwen
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Pi
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => false,
    }
}

fn turn_text_join_separator(target: Harness, written_from: Option<Harness>, message: &Message) -> Option<&'static str> {
    let has_images = message.content.iter().any(|b| matches!(b, Block::Image { .. }));
    match target {
        Harness::Grok => matches!(message.role, Role::System | Role::User | Role::Assistant).then_some(""),
        Harness::Hermes => {
            (!has_images && matches!(message.role, Role::System | Role::User | Role::Assistant)).then_some("\n")
        }
        Harness::Codex => matches!(message.role, Role::System | Role::User).then_some("\n"),
        Harness::OpenCode => matches!(message.role, Role::System | Role::User).then_some(""),
        Harness::Claude => {
            let special = written_from
                .and_then(|src| write_rules(target).system_fate.map(|f| f(src, message)))
                .is_some_and(|fate| matches!(fate, SystemFate::Special(_)));
            special.then_some("")
        }
        Harness::Qwen => matches!(message.kind, MessageKind::Error | MessageKind::Notice).then_some(""),
        Harness::OpenClaw
        | Harness::Gemini
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Pi
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => None,
    }
}

fn stub_message_for_system_fate(msg: &ScannedMessage) -> Message {
    let mut m = if msg.synthesized {
        Message::synthesized(msg.kind, msg.origin)
    } else {
        Message::new(msg.role)
    };
    m.role = msg.role;
    m.kind = msg.kind;
    m.origin = msg.origin;
    if msg.carrier {
        m.extra
            .insert(crate::ir::CARRIER_KEY.into(), Value::Object(Default::default()));
    }
    m
}

fn scanned_is_client_command(msg: &ScannedMessage) -> bool {
    msg.kind == MessageKind::Prompt
        && msg
            .blocks
            .iter()
            .filter_map(|b| match b {
                ScannedBlock::Text { scanned, .. } if scanned.non_blank => scanned.first_non_blank,
                _ => None,
            })
            .next()
            .is_some_and(|ch| matches!(ch, '/' | '?'))
}

fn scanned_is_gemini_ignored_user(msg: &ScannedMessage) -> bool {
    msg.kind == MessageKind::Prompt
        && msg
            .blocks
            .iter()
            .filter_map(|b| match b {
                ScannedBlock::Text { scanned, .. } if scanned.non_blank => Some(scanned.starts_ignored_gemini),
                _ => None,
            })
            .next()
            .unwrap_or(false)
}

fn scanned_thinking_fate(target: Harness, thinking: ThinkingMode, b: &ScannedBlock) -> ThinkingFate {
    let ScannedBlock::Thinking {
        text,
        signature,
        encrypted,
        redacted,
        ..
    } = b
    else {
        return ThinkingFate::Keep;
    };
    crate::emit::thinking_fate_with_presence(
        target,
        thinking,
        text.non_blank,
        signature.as_ref().map(|_| "x"),
        encrypted.as_ref().map(|_| "x"),
        *redacted,
    )
}

fn scanned_unrepresentable(target: Harness, thinking: ThinkingMode, msg: &ScannedMessage) -> bool {
    if msg.blocks.is_empty() {
        return false;
    }
    for b in &msg.blocks {
        let survives = match b {
            ScannedBlock::Text { .. } => true,
            ScannedBlock::Thinking { .. } => scanned_thinking_fate(target, thinking, b) != ThinkingFate::Drop,
            ScannedBlock::ToolUse { .. } => target_holds(target, Field::ToolUse),
            ScannedBlock::ToolResult { .. } => target_holds(target, Field::ToolResult),
            ScannedBlock::Image { .. } => target_holds(target, Field::Image),
            ScannedBlock::File { .. } => target_holds(target, Field::File),
            ScannedBlock::Opaque { .. } => target_holds(target, Field::Opaque),
        };
        if survives {
            return false;
        }
    }
    true
}

#[cfg(feature = "sqlite")]
fn extract_hermes_verbatim_reasoning(target: Harness, msg: &Message) -> Option<([u8; 32], Option<[u8; 32]>, String)> {
    use crate::harness::hermes::{RAW_REASONING_CONTENT_KEY, RAW_REASONING_KEY};
    let hbag = msg.harness_extra(target)?;
    let has_raw = hbag.get(RAW_REASONING_KEY).and_then(Value::as_str).is_some()
        || hbag.get(RAW_REASONING_CONTENT_KEY).and_then(Value::as_str).is_some()
        || hbag.get("reasoning_details").is_some();
    if !has_raw {
        return None;
    }
    for b in &msg.content {
        if let Block::Thinking { text, encrypted, .. } = b {
            if let Some(s) = text.inline_str() {
                let mut prev = String::new();
                append_preview(&mut prev, s);
                let text_hash = (!s.trim().is_empty()).then(|| hash_bytes(s.as_bytes()));
                let enc_hash = encrypted.as_deref().map(|e| hash_bytes(e.as_bytes()));
                if let Some(th) = text_hash {
                    return Some((th, enc_hash, prev));
                }
            }
        }
    }
    None
}

#[cfg(not(feature = "sqlite"))]
fn extract_hermes_verbatim_reasoning(_target: Harness, _msg: &Message) -> Option<([u8; 32], Option<[u8; 32]>, String)> {
    None
}

fn opencode_attachment_unavailable(label: &str, mime: Option<&str>, reference: Option<&str>) -> Option<String> {
    let valid = match (mime.filter(|m| !m.is_empty()), reference.filter(|r| !r.is_empty())) {
        (_, Some(r)) if r.starts_with("data:") => r
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(';'))
            .is_some_and(|(m, tail)| !m.is_empty() && tail.starts_with("base64,")),
        (Some(_), Some(r)) if r.starts_with("file:") || r.starts_with("http://") || r.starts_with("https://") => true,
        _ => false,
    };
    (!valid).then(|| {
        format!(
            "[{label} attachment unavailable: {}]",
            reference.unwrap_or("no embedded data")
        )
    })
}

/// Build the canonical ordered unit stream `π_B` for either the source (`written_from.is_some()`)
/// or the reparsed session (`written_from.is_none()`), covering `System`, `User`, `Assistant`, and
/// `Tool` turns plus `Text`, `Thinking`, `ToolUse`, `ToolResult`, `Image`, and `File` blocks.
fn build_canonical_units(
    target: Harness,
    thinking: ThinkingMode,
    messages: &[ScannedMessage],
    answered: &HashSet<&str>,
    called: &HashSet<&str>,
    folded_results: &HashMap<&str, Vec<&ScannedBlock>>,
) -> Result<Vec<Unit>> {
    let rules = write_rules(target);
    let prof = target_profile(target);
    let folds = prof.folds_tool_result;
    let empty_obj = empty_object_hash()?;
    let mut rho = RhoMap::default();
    let mut out = Vec::new();

    for msg in messages {
        if msg.skills_stripped || msg.lmstudio_non_selected_variant {
            continue;
        }
        let is_source = msg.written_from.is_some();
        let fate = msg
            .written_from
            .zip(rules.system_fate)
            .map(|(source, f)| f(source, &stub_message_for_system_fate(msg)));
        if matches!(fate, Some(SystemFate::Unwritten(_))) {
            continue;
        }
        // Qwen rebuilds its system prompt at launch (`qwen/emit.rs:318`) and never writes `SystemPrompt` rows.
        if is_source && prof.omits_system_prompt_rows && msg.kind == MessageKind::SystemPrompt {
            continue;
        }
        let unrepresentable = scanned_unrepresentable(target, thinking, msg);
        let bookkeeping =
            msg.kind == MessageKind::Carrier || (msg.carrier && msg.blocks.is_empty() && !msg.synthesized);
        let is_orphan_call = |b: &ScannedBlock| -> bool {
            is_source
                && rules.answered_calls_only
                && matches!(b, ScannedBlock::ToolUse { id, .. } if !answered.contains(id.as_str()))
        };
        // Claude's generated-content `default_empty()` fallback (`claude/emit/stream.rs:226`) emits
        // `[{"type":"text","text":""}]` when non-portable `ThinkingFate::Drop` empties a non-carrier
        // Assistant turn with no raw `message.content` override; Claude's reader parses that back
        // as an empty Assistant turn (`UnitKind::Turn { role: Assistant }`).
        if is_source
            && prof.empty_assistant_default_turn
            && thinking != ThinkingMode::Portable
            && msg.role == Role::Assistant
            && !msg.carrier
            && !msg.claude_raw_content
            && unrepresentable
            && msg.blocks.iter().all(|b| matches!(b, ScannedBlock::Thinking { .. }))
        {
            out.push(Unit {
                loc: UnitLoc {
                    turn: msg.turn_idx,
                    block: 0,
                },
                kind: UnitKind::Turn {
                    role: UnitRole::Assistant,
                },
            });
            continue;
        }
        if bookkeeping || unrepresentable || (!msg.blocks.is_empty() && msg.blocks.iter().all(is_orphan_call)) {
            continue;
        }

        match msg.role {
            Role::System => {
                // Claude `CompactionSummary` writes as a `user` record (`isCompactSummary: true`).
                let unit_role = if fate == Some(SystemFate::Special(MessageKind::CompactionSummary)) {
                    UnitRole::User
                } else {
                    UnitRole::System
                };
                project_turn_units(
                    target,
                    thinking,
                    is_source,
                    unit_role,
                    msg,
                    &is_orphan_call,
                    &mut rho,
                    &mut out,
                );
            }
            Role::User => {
                // Gemini & Qwen re-role client commands / context preambles to `System`/`InjectedContext`;
                // Qwen also writes non-`Prompt` User turns (`InjectedContext`, `SubagentReturn`,
                // `CompactionSummary`) as `system`-provenance rows that re-parse as `Role::System`.
                let unit_role = if is_source
                    && ((rules.reroles_client_commands && scanned_is_gemini_ignored_user(msg))
                        || (prof.reroles_non_prompt_user && msg.kind != MessageKind::Prompt))
                {
                    UnitRole::System
                } else {
                    UnitRole::User
                };
                project_turn_units(
                    target,
                    thinking,
                    is_source,
                    unit_role,
                    msg,
                    &is_orphan_call,
                    &mut rho,
                    &mut out,
                );
            }
            Role::Assistant => {
                project_turn_units(
                    target,
                    thinking,
                    is_source,
                    UnitRole::Assistant,
                    msg,
                    &is_orphan_call,
                    &mut rho,
                    &mut out,
                );
                // On stores that fold `ToolResult` onto its originating `ToolUse` (`OpenCode`, `Gemini`),
                // the reader synthesizes the `Role::Tool` message immediately after this `Assistant`
                // turn in call order. Emit every paired source `ToolResult` in sequence (preserving duplicates
                // so a lost duplicate `ToolResult` fails unit equivalence as well as count delta).
                if is_source && folds {
                    for b in &msg.blocks {
                        if let ScannedBlock::ToolUse { id, .. } = b {
                            if let Some(res_list) = folded_results.get(id.as_str()) {
                                for res in res_list {
                                    push_tool_result_unit(target, res, &mut rho, &mut out);
                                }
                            }
                        }
                    }
                }
            }
            Role::Tool => {
                if prof.renders_tools_as_text {
                    if is_source {
                        project_lmstudio_tool_turn(msg, &mut out);
                    }
                    continue;
                }
                if !target_holds(target, Field::ToolResult) {
                    continue;
                }
                for b in &msg.blocks {
                    let ScannedBlock::ToolResult {
                        loc,
                        tool_use_id,
                        tool_name,
                        ..
                    } = b
                    else {
                        continue;
                    };
                    if is_source && folds {
                        if called.contains(tool_use_id.as_str()) {
                            continue;
                        }
                        // Orphan result: OpenCode & Gemini synthesize an Assistant call shell for it.
                        let shell_name = if prof.orphan_result_shell == OrphanShell::Gemini {
                            tool_name.as_deref().unwrap_or("tool").to_owned()
                        } else {
                            tool_name.clone().unwrap_or_default()
                        };
                        let call_rho = rho.intern(tool_use_id);
                        out.push(Unit {
                            loc: *loc,
                            kind: UnitKind::Turn {
                                role: UnitRole::Assistant,
                            },
                        });
                        out.push(Unit {
                            loc: *loc,
                            kind: UnitKind::ToolUse {
                                call_rho,
                                name: shell_name,
                                input_hash: empty_obj,
                                input_preview: "{}".into(),
                            },
                        });
                    } else if is_source
                        && prof.orphan_result_shell == OrphanShell::Codex
                        && !called.contains(tool_use_id.as_str())
                    {
                        // Codex synthesizes a `function_call` / `custom_tool_call` shell before an orphan output.
                        let shell_name = tool_name.clone().unwrap_or_else(|| "unknown".into());
                        let call_rho = rho.intern(tool_use_id);
                        let (shell_hash, shell_prev) = if matches!(tool_name.as_deref(), Some("exec" | "apply_patch")) {
                            let val = serde_json::json!({"raw": ""});
                            let mut h = blake3::Hasher::new();
                            hash_canonical_json_view(JsonView::from(&val), &mut h)?;
                            (*h.finalize().as_bytes(), val.to_string())
                        } else {
                            (empty_obj, "{}".into())
                        };
                        out.push(Unit {
                            loc: *loc,
                            kind: UnitKind::Turn {
                                role: UnitRole::Assistant,
                            },
                        });
                        out.push(Unit {
                            loc: *loc,
                            kind: UnitKind::ToolUse {
                                call_rho,
                                name: shell_name,
                                input_hash: shell_hash,
                                input_preview: shell_prev,
                            },
                        });
                    }
                    push_tool_result_unit(target, b, &mut rho, &mut out);
                }
            }
        }
    }
    Ok(out)
}

fn push_tool_result_unit(target: Harness, b: &ScannedBlock, rho: &mut RhoMap, out: &mut Vec<Unit>) {
    let ScannedBlock::ToolResult {
        loc,
        tool_use_id,
        content,
        is_error,
        ..
    } = b
    else {
        return;
    };
    let call_rho = rho.intern(tool_use_id);
    let held_err = target_holds(target, Field::IsError).then_some(*is_error);
    out.push(Unit {
        loc: *loc,
        kind: UnitKind::ToolResult {
            call_rho,
            content_hash: content.hash,
            content_len: content.byte_len,
            is_error: held_err,
            preview: content.preview.clone(),
        },
    });
}

fn project_lmstudio_tool_turn(msg: &ScannedMessage, out: &mut Vec<Unit>) {
    let mut rendered: Vec<(UnitLoc, [u8; 32], u64, String)> = Vec::new();
    for b in &msg.blocks {
        if let ScannedBlock::ToolResult {
            loc,
            lmstudio_text: Some((h, len, prev)),
            ..
        } = b
        {
            rendered.push((*loc, *h, *len, prev.clone()));
        }
    }
    if rendered.is_empty() {
        return;
    }
    out.push(Unit {
        loc: rendered[0].0,
        kind: UnitKind::Turn { role: UnitRole::User },
    });
    for (loc, hash, byte_len, preview) in rendered {
        if byte_len > 0 {
            out.push(Unit {
                loc,
                kind: UnitKind::Text {
                    hash,
                    byte_len,
                    preview,
                },
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn project_turn_units(
    target: Harness,
    thinking: ThinkingMode,
    is_source: bool,
    role: UnitRole,
    msg: &ScannedMessage,
    is_orphan_call: &impl Fn(&ScannedBlock) -> bool,
    rho: &mut RhoMap,
    out: &mut Vec<Unit>,
) {
    let prof = target_profile(target);
    let separated_assistant = role == UnitRole::Assistant && prof.separated_assistant;
    let join_texts = msg.joined_text.is_some();
    let join_thinking = role == UnitRole::Assistant && prof.join_thinking;
    // Qwen skips blank text on ordinary parts and `Notice` records (`Error` keeps empty `""`, which is 0B anyway).
    let skip_blank_text = prof.skip_blank_text;

    enum PendingItem {
        Text {
            loc: UnitLoc,
            hash: [u8; 32],
            byte_len: u64,
            preview: String,
        },
        Thinking {
            loc: UnitLoc,
            text_hash: Option<[u8; 32]>,
            sig_hash: Option<[u8; 32]>,
            enc_hash: Option<[u8; 32]>,
            preview: String,
        },
        ToolUse {
            loc: UnitLoc,
            id: String,
            name: String,
            input_hash: [u8; 32],
            input_preview: String,
        },
        Image {
            loc: UnitLoc,
            hash: [u8; 32],
            preview: String,
        },
        File {
            loc: UnitLoc,
            hash: [u8; 32],
            preview: String,
        },
        Opaque {
            loc: UnitLoc,
            block_type: String,
            hash: [u8; 32],
        },
    }

    let mut items: Vec<PendingItem> = Vec::new();
    for b in msg.blocks.iter().filter(|b| !is_orphan_call(b)) {
        match b {
            ScannedBlock::Text { loc, scanned } => {
                if scanned.byte_len == 0 || (skip_blank_text && !scanned.non_blank) {
                    continue;
                }
                items.push(PendingItem::Text {
                    loc: *loc,
                    hash: scanned.hash,
                    byte_len: scanned.byte_len,
                    preview: scanned.preview.clone(),
                });
            }
            ScannedBlock::Thinking {
                loc,
                text,
                codex_summary,
                signature,
                encrypted,
                redacted,
            } => {
                if is_source && prof.native_thinking_as_text && thinking == ThinkingMode::Native {
                    if text.non_blank {
                        items.push(PendingItem::Text {
                            loc: *loc,
                            hash: text.hash,
                            byte_len: text.byte_len,
                            preview: text.preview.clone(),
                        });
                    }
                    continue;
                }
                let fate = scanned_thinking_fate(target, thinking, b);
                match fate {
                    ThinkingFate::Drop => {}
                    ThinkingFate::AsText => {
                        if text.byte_len > 0 && (!skip_blank_text || text.non_blank) {
                            items.push(PendingItem::Text {
                                loc: *loc,
                                hash: text.hash,
                                byte_len: text.byte_len,
                                preview: text.preview.clone(),
                            });
                        }
                    }
                    ThinkingFate::Placeholder => {
                        let ph = crate::emit::thinking_placeholder(
                            signature.as_ref().map(|(_, len)| "x".repeat(*len)).as_deref(),
                            encrypted.as_ref().map(|(_, len)| "x".repeat(*len)).as_deref(),
                            *redacted,
                        );
                        let mut prev = String::new();
                        append_preview(&mut prev, &ph);
                        items.push(PendingItem::Text {
                            loc: *loc,
                            hash: hash_bytes(ph.as_bytes()),
                            byte_len: ph.len() as u64,
                            preview: prev,
                        });
                    }
                    ThinkingFate::Keep => {
                        if skip_blank_text && !text.non_blank {
                            continue;
                        }
                        let eff_text = codex_summary.as_ref().unwrap_or(text);
                        let text_hash = (holds(target, thinking, Field::ThinkingText) && eff_text.non_blank)
                            .then_some(eff_text.hash);
                        let sig_hash = holds(target, thinking, Field::ThinkingSignature)
                            .then(|| signature.map(|(h, _)| h))
                            .flatten();
                        let enc_hash = holds(target, thinking, Field::ThinkingEncrypted)
                            .then(|| {
                                if is_source && prof.drops_foreign_encrypted && msg.written_from != Some(target) {
                                    None
                                } else {
                                    encrypted.map(|(h, _)| h)
                                }
                            })
                            .flatten();
                        if text_hash.is_some() || sig_hash.is_some() || enc_hash.is_some() {
                            items.push(PendingItem::Thinking {
                                loc: *loc,
                                text_hash,
                                sig_hash,
                                enc_hash,
                                preview: eff_text.preview.clone(),
                            });
                        }
                    }
                }
            }
            ScannedBlock::ToolUse {
                loc,
                id,
                name,
                input_hash,
                input_preview,
                lmstudio_text,
            } => {
                if role != UnitRole::Assistant {
                    continue;
                }
                if prof.renders_tools_as_text {
                    if let Some((h, len, prev)) = lmstudio_text {
                        items.push(PendingItem::Text {
                            loc: *loc,
                            hash: *h,
                            byte_len: *len,
                            preview: prev.clone(),
                        });
                    }
                    continue;
                }
                if target_holds(target, Field::ToolUse) {
                    items.push(PendingItem::ToolUse {
                        loc: *loc,
                        id: id.clone(),
                        name: name.clone(),
                        input_hash: *input_hash,
                        input_preview: input_preview.clone(),
                    });
                }
            }
            ScannedBlock::Image {
                loc,
                media_type,
                data_ref,
            } => match project_image(target, role, is_source, media_type.as_deref(), data_ref.as_deref()) {
                ImageProjection::Media { hash, preview } => items.push(PendingItem::Image {
                    loc: *loc,
                    hash,
                    preview,
                }),
                ImageProjection::DegradedText(text) => {
                    let mut prev = String::new();
                    append_preview(&mut prev, &text);
                    items.push(PendingItem::Text {
                        loc: *loc,
                        hash: hash_bytes(text.as_bytes()),
                        byte_len: text.len() as u64,
                        preview: prev,
                    });
                }
                ImageProjection::Dropped => {}
            },
            ScannedBlock::File {
                loc,
                mime,
                path,
                source,
            } => match project_file(
                target,
                role,
                is_source,
                mime.as_deref(),
                path.as_deref(),
                source.as_deref(),
            ) {
                FileProjection::Media { hash, preview } => items.push(PendingItem::File {
                    loc: *loc,
                    hash,
                    preview,
                }),
                FileProjection::DegradedText(text) => {
                    let mut prev = String::new();
                    append_preview(&mut prev, &text);
                    items.push(PendingItem::Text {
                        loc: *loc,
                        hash: hash_bytes(text.as_bytes()),
                        byte_len: text.len() as u64,
                        preview: prev,
                    });
                }
                FileProjection::Dropped => {}
            },
            ScannedBlock::Opaque { loc, block_type, hash } => {
                if target_holds(target, Field::Opaque) {
                    items.push(PendingItem::Opaque {
                        loc: *loc,
                        block_type: block_type.clone(),
                        hash: *hash,
                    });
                }
            }
            ScannedBlock::ToolResult { .. } => {}
        }
    }

    // Check whether joined_text overrides empty check (e.g. Qwen Notice skips blank joined text).
    let has_joined_text = msg
        .joined_text
        .as_ref()
        .is_some_and(|(_, len, non_blank, _)| *len > 0 && (!skip_blank_text || *non_blank));

    // Synthesized / control `System` markers with zero surviving content blocks (`CompactionBoundary`,
    // `ModelChange`, `Branch`, OpenClaw `session_info`) carry no body units; every `System` turn with
    // surviving content (`SystemPrompt`, `Notice`, `InjectedContext`, `CompactionSummary`, `Error`,
    // `SubagentReturn`) emits its `Turn(System)` + hashed content blocks.
    if role == UnitRole::System && items.is_empty() && !has_joined_text {
        return;
    }

    let lmstudio_has_usage = prof.drops_empty_turn == EmptyTurnDrop::LmStudioUnlessUsage
        && role == UnitRole::Assistant
        && msg
            .usage
            .as_ref()
            .is_some_and(|u| u.input_tokens.is_some() || u.output_tokens.is_some());
    if items.is_empty() && !has_joined_text && prof.drops_empty_turn != EmptyTurnDrop::None && !lmstudio_has_usage {
        return;
    }

    let turn_loc = UnitLoc {
        turn: msg.turn_idx,
        block: 0,
    };
    // Codex writes one rollout item per Assistant block (`reasoning`, `agent_message`, `function_call`),
    // and its reader parses each item back as an individual `Role::Assistant` turn.
    if prof.assistant_splits_per_block && role == UnitRole::Assistant {
        if items.is_empty() {
            out.push(Unit {
                loc: turn_loc,
                kind: UnitKind::Turn { role },
            });
            return;
        }
        for item in &items {
            let (loc, kind) = match item {
                PendingItem::Text {
                    loc,
                    hash,
                    byte_len,
                    preview,
                } => (
                    *loc,
                    UnitKind::Text {
                        hash: *hash,
                        byte_len: *byte_len,
                        preview: preview.clone(),
                    },
                ),
                PendingItem::Thinking {
                    loc,
                    text_hash,
                    sig_hash,
                    enc_hash,
                    preview,
                } => (
                    *loc,
                    UnitKind::Thinking {
                        text_hash: *text_hash,
                        sig_hash: *sig_hash,
                        enc_hash: *enc_hash,
                        preview: preview.clone(),
                    },
                ),
                PendingItem::ToolUse {
                    loc,
                    id,
                    name,
                    input_hash,
                    input_preview,
                } => {
                    let call_rho = rho.intern(id);
                    (
                        *loc,
                        UnitKind::ToolUse {
                            call_rho,
                            name: name.clone(),
                            input_hash: *input_hash,
                            input_preview: input_preview.clone(),
                        },
                    )
                }
                PendingItem::Image { loc, hash, preview } => (
                    *loc,
                    UnitKind::Image {
                        hash: *hash,
                        preview: preview.clone(),
                    },
                ),
                PendingItem::File { loc, hash, preview } => (
                    *loc,
                    UnitKind::File {
                        hash: *hash,
                        preview: preview.clone(),
                    },
                ),
                PendingItem::Opaque { loc, block_type, hash } => (
                    *loc,
                    UnitKind::Opaque {
                        block_type: block_type.clone(),
                        hash: *hash,
                    },
                ),
            };
            out.push(Unit {
                loc,
                kind: UnitKind::Turn { role },
            });
            out.push(Unit { loc, kind });
        }
        return;
    }

    out.push(Unit {
        loc: turn_loc,
        kind: UnitKind::Turn { role },
    });

    let emit_thinking = |items: &[PendingItem], out: &mut Vec<Unit>| {
        if join_thinking {
            if let Some((th, eh, prev)) = &msg.hermes_verbatim_reasoning {
                out.push(Unit {
                    loc: turn_loc,
                    kind: UnitKind::Thinking {
                        text_hash: Some(*th),
                        sig_hash: None,
                        enc_hash: *eh,
                        preview: prev.clone(),
                    },
                });
                return;
            }
            let thoughts: Vec<_> = items
                .iter()
                .filter_map(|i| match i {
                    PendingItem::Thinking {
                        loc,
                        text_hash,
                        sig_hash,
                        enc_hash,
                        preview,
                    } => Some((*loc, *text_hash, *sig_hash, *enc_hash, preview)),
                    _ => None,
                })
                .collect();
            if thoughts.len() == 1 {
                let (loc, text_hash, sig_hash, enc_hash, preview) = thoughts[0];
                out.push(Unit {
                    loc,
                    kind: UnitKind::Thinking {
                        text_hash,
                        sig_hash,
                        enc_hash,
                        preview: preview.clone(),
                    },
                });
                return;
            }
            if thoughts.len() > 1 {
                let first_enc = thoughts.iter().find_map(|(_, _, _, e, _)| *e);
                let first_sig = thoughts.iter().find_map(|(_, _, s, _, _)| *s);
                let loc = thoughts[0].0;
                let (text_hash, preview) = match &msg.joined_thinking {
                    Some((th, prev)) if holds(target, thinking, Field::ThinkingText) => (Some(*th), prev.clone()),
                    _ => (thoughts[0].1, thoughts[0].4.clone()),
                };
                out.push(Unit {
                    loc,
                    kind: UnitKind::Thinking {
                        text_hash,
                        sig_hash: first_sig,
                        enc_hash: first_enc,
                        preview,
                    },
                });
                return;
            }
        }
        for item in items {
            if let PendingItem::Thinking {
                loc,
                text_hash,
                sig_hash,
                enc_hash,
                preview,
            } = item
            {
                out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::Thinking {
                        text_hash: *text_hash,
                        sig_hash: *sig_hash,
                        enc_hash: *enc_hash,
                        preview: preview.clone(),
                    },
                });
            }
        }
    };

    let emit_content_and_media = |items: &[PendingItem], out: &mut Vec<Unit>| {
        if join_texts {
            if let Some((hash, byte_len, non_blank, preview)) = &msg.joined_text {
                if *byte_len > 0 && (!skip_blank_text || *non_blank) {
                    let loc = items
                        .iter()
                        .find_map(|i| match i {
                            PendingItem::Text { loc, .. } => Some(*loc),
                            _ => None,
                        })
                        .unwrap_or(turn_loc);
                    out.push(Unit {
                        loc,
                        kind: UnitKind::Text {
                            hash: *hash,
                            byte_len: *byte_len,
                            preview: preview.clone(),
                        },
                    });
                }
            }
            for item in items {
                match item {
                    PendingItem::Image { loc, hash, preview } => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::Image {
                            hash: *hash,
                            preview: preview.clone(),
                        },
                    }),
                    // Continue writes `contextItems` (`File`) after `toolCalls`; handled below.
                    PendingItem::File { loc, hash, preview } if !prof.files_after_tool_calls => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::File {
                            hash: *hash,
                            preview: preview.clone(),
                        },
                    }),
                    PendingItem::Opaque { loc, block_type, hash } => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::Opaque {
                            block_type: block_type.clone(),
                            hash: *hash,
                        },
                    }),
                    _ => {}
                }
            }
        } else {
            for item in items {
                match item {
                    PendingItem::Text {
                        loc,
                        hash,
                        byte_len,
                        preview,
                    } => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::Text {
                            hash: *hash,
                            byte_len: *byte_len,
                            preview: preview.clone(),
                        },
                    }),
                    PendingItem::Image { loc, hash, preview } => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::Image {
                            hash: *hash,
                            preview: preview.clone(),
                        },
                    }),
                    PendingItem::File { loc, hash, preview } if !prof.files_after_tool_calls => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::File {
                            hash: *hash,
                            preview: preview.clone(),
                        },
                    }),
                    PendingItem::Opaque { loc, block_type, hash } => out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::Opaque {
                            block_type: block_type.clone(),
                            hash: *hash,
                        },
                    }),
                    _ => {}
                }
            }
        }
    };

    let emit_tool_uses = |items: &[PendingItem], rho: &mut RhoMap, out: &mut Vec<Unit>| {
        for item in items {
            if let PendingItem::ToolUse {
                loc,
                id,
                name,
                input_hash,
                input_preview,
            } = item
            {
                let call_rho = rho.intern(id);
                out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::ToolUse {
                        call_rho,
                        name: name.clone(),
                        input_hash: *input_hash,
                        input_preview: input_preview.clone(),
                    },
                });
            }
        }
    };

    let emit_continue_files = |items: &[PendingItem], out: &mut Vec<Unit>| {
        if prof.files_after_tool_calls {
            for item in items {
                if let PendingItem::File { loc, hash, preview } = item {
                    out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::File {
                            hash: *hash,
                            preview: preview.clone(),
                        },
                    });
                }
            }
        }
    };

    if separated_assistant || join_texts || prof.files_after_tool_calls {
        emit_thinking(&items, out);
        emit_content_and_media(&items, out);
        emit_tool_uses(&items, rho, out);
        emit_continue_files(&items, out);
    } else {
        for item in &items {
            match item {
                PendingItem::Text {
                    loc,
                    hash,
                    byte_len,
                    preview,
                } => out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::Text {
                        hash: *hash,
                        byte_len: *byte_len,
                        preview: preview.clone(),
                    },
                }),
                PendingItem::Thinking {
                    loc,
                    text_hash,
                    sig_hash,
                    enc_hash,
                    preview,
                } => out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::Thinking {
                        text_hash: *text_hash,
                        sig_hash: *sig_hash,
                        enc_hash: *enc_hash,
                        preview: preview.clone(),
                    },
                }),
                PendingItem::ToolUse {
                    loc,
                    id,
                    name,
                    input_hash,
                    input_preview,
                } => {
                    let call_rho = rho.intern(id);
                    out.push(Unit {
                        loc: *loc,
                        kind: UnitKind::ToolUse {
                            call_rho,
                            name: name.clone(),
                            input_hash: *input_hash,
                            input_preview: input_preview.clone(),
                        },
                    });
                }
                PendingItem::Image { loc, hash, preview } => out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::Image {
                        hash: *hash,
                        preview: preview.clone(),
                    },
                }),
                PendingItem::File { loc, hash, preview } => out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::File {
                        hash: *hash,
                        preview: preview.clone(),
                    },
                }),
                PendingItem::Opaque { loc, block_type, hash } => out.push(Unit {
                    loc: *loc,
                    kind: UnitKind::Opaque {
                        block_type: block_type.clone(),
                        hash: *hash,
                    },
                }),
            }
        }
    }
}

fn compute_joined_thinking(
    target: Harness,
    thinking: ThinkingMode,
    message: &Message,
    resolver: &Resolver,
) -> Result<Option<([u8; 32], String)>> {
    let mut joiner = JoinedTextHasher::new();
    let mut kept_count = 0usize;
    for b in &message.content {
        if let Block::Thinking { text, .. } = b {
            let fate = crate::emit::thinking::fate(b, target, thinking, resolver)?;
            if fate == ThinkingFate::Keep {
                kept_count += 1;
                joiner.push_text(text, resolver, "\n\n")?;
            }
        }
    }
    if kept_count > 1 && joiner.non_blank {
        let (h, _, _, prev) = joiner.finish();
        Ok(Some((h, prev)))
    } else {
        Ok(None)
    }
}

fn compute_joined_turn_text(
    target: Harness,
    thinking: ThinkingMode,
    is_source: bool,
    message: &Message,
    sep: &str,
    resolver: &Resolver,
) -> Result<Option<JoinedTurnText>> {
    let mut joiner = JoinedTextHasher::new();
    let prof = target_profile(target);
    let codex_msg = is_source && prof.codex_message_join && matches!(message.role, Role::User | Role::System);
    for b in &message.content {
        match b {
            Block::Text { text, .. } => {
                if codex_msg {
                    joiner.push_codex_message_block(text, resolver)?;
                } else {
                    joiner.push_text(text, resolver, sep)?;
                }
            }
            Block::Thinking { .. } => {
                if let Some(val) = crate::emit::thinking::text(b, target, thinking, resolver)? {
                    if codex_msg {
                        joiner.push_codex_message_block(&val, resolver)?;
                    } else {
                        joiner.push_text(&val, resolver, sep)?;
                    }
                }
            }
            Block::File { path, source, .. }
                if is_source && prof.claude_non_user_file_text && message.role != Role::User =>
            {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                joiner.push_text(&Text::Inline(format!("[file: {label}]")), resolver, sep)?;
            }
            _ => {}
        }
    }
    if is_source && prof.opencode_non_assistant_unavail_text && message.role != Role::Assistant {
        for b in &message.content {
            let unavail = match b {
                Block::Image {
                    media_type, data_ref, ..
                } => opencode_attachment_unavailable("Image", media_type.as_deref(), data_ref.as_deref()),
                Block::File { mime, path, source, .. } => {
                    opencode_attachment_unavailable("File", mime.as_deref(), source.as_deref().or(path.as_deref()))
                }
                _ => None,
            };
            if let Some(u) = unavail {
                joiner.push_text(&Text::Inline(u), resolver, sep)?;
            }
        }
    }
    let (h, len, non_blank, prev) = joiner.finish();
    Ok(Some((h, len, non_blank, prev)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_preserves_future_answers_final_model_and_client_commands() -> Result<()> {
        let mut session = Session::new("projection", Harness::Claude);
        let mut call = Message::new(Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "later".into(),
            name: "tool".into(),
            input: serde_json::json!({"large": "ignored"}).into(),
            namespace: None,
            extensions: Default::default(),
        });
        let mut orphan = call.clone();
        if let Block::ToolUse { id, .. } = &mut orphan.content[0] {
            *id = "orphan".into();
        }
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "later".into(),
            content: "output".into(),
            tool_name: Some("tool".into()),
            is_error: true,
            status: None,
            details: Some(Value::Null),
            extensions: Default::default(),
        });
        let mut command = Message::new(Role::User);
        command.content = vec![Block::from_text(" \n".into()), Block::from_text(" ?help".into())];
        command.model = Some("explicit".into());
        let mut signed = Message::new(Role::Assistant);
        signed.content.push(Block::Thinking {
            text: "\u{2003}".into(),
            signature: Some(String::new()),
            encrypted: None,
            redacted: true,
            extensions: Default::default(),
        });
        let mut carrier = Message::new(Role::System);
        carrier.kind = MessageKind::Carrier;
        session.messages = vec![result, orphan, command, signed, carrier, call];
        let mut golden = String::new();
        for model in [None, Some("late-default".into())] {
            session.model = model;
            for target in Harness::ALL {
                for thinking in [ThinkingMode::Native, ThinkingMode::Text, ThinkingMode::Drop] {
                    golden.push_str(&format!(
                        "{:?}/{target}/{thinking} {:?}\n",
                        session.model,
                        source_features(target, thinking, &session)?
                    ));
                }
            }
        }
        assert_eq!(
            golden,
            include_str!("../../../tests/fixtures/emit-frozen/fidelity-features.txt")
        );
        session.model = None;
        let facts = source_features(Harness::Claude, ThinkingMode::Native, &session)?;
        assert_eq!(facts.tool_use, 1, "only the answered call survives");
        assert_eq!(facts.tool_result, 1);
        assert_eq!(facts.results_with_name, 1);
        assert_eq!(facts.results_is_error, 1);
        assert_eq!(facts.results_with_details, 1);
        assert_eq!(facts.thinking_text, 0, "Unicode whitespace is blank");
        assert_eq!(facts.model_eff, 1, "the command has an explicit model");
        session.model = Some("late-default".into());
        let facts = source_features(Harness::Claude, ThinkingMode::Native, &session)?;
        assert_eq!(facts.model_eff, facts.role_counts.values().sum::<usize>());
        assert!(!facts.kind_counts.contains_key("carrier"));
        Ok(())
    }

    #[test]
    fn canonical_json_hash_sorts_keys_normalizes_numbers_and_catches_single_byte_flips() -> Result<()> {
        let a: crate::Json = serde_json::json!({"z": 1, "a": {"k2": -0.0, "k1": "hello"}}).into();
        let b: crate::Json = serde_json::json!({"a": {"k1": "hello", "k2": 0}, "z": 1.0}).into();
        let c: crate::Json = serde_json::json!({"a": {"k1": "hellp", "k2": 0}, "z": 1}).into();
        let (ha, _) = hash_tool_input(Harness::Claude, "Bash", &a)?;
        let (hb, _) = hash_tool_input(Harness::Claude, "Bash", &b)?;
        let (hc, _) = hash_tool_input(Harness::Claude, "Bash", &c)?;
        assert_eq!(ha, hb, "reordered keys and -0.0/0/1.0/1 canonicalize identically");
        assert_ne!(ha, hc, "1-byte string value flip changes BLAKE3 digest");
        Ok(())
    }
}
