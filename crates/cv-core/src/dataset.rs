//! Training-dataset export: render a [`Session`] into one JSONL record in a format that
//! fine-tuning toolchains ingest directly —
//!
//! - `chatml`   → `{"messages":[{"role","content"}]}` (OpenAI-style; the most portable)
//! - `sharegpt` → `{"conversations":[{"from","value"}]}`
//!
//! Unsloth Studio, TRL, and HuggingFace `datasets` load these with no bespoke adapter (Studio's
//! importer auto-detects both). One session → one line; [`RecordWriter`] accepts one message at a
//! time without retaining transcript history. Callers stage a record until filtering succeeds.
//!
//! Tool calls/results are folded into the message text as fenced `tool_call` / `tool_result`
//! blocks (v1: the model learns the tool-use *pattern* in-band, and it imports as plain chatml).
//! Reasoning is kept in a `<thinking>` wrapper — it's high-signal for distilling into smaller
//! models, and a downstream filter can strip it if a given run wants answer-only SFT.

#[cfg(test)]
use crate::json::{ReadInput, Validation};
mod deferred;

use crate::ir::{Block, Message, MessageKind, Role, Session};

/// The training-role label for a message, or `None` when the turn was never part of what the model
/// saw or said (a harness notice, an API error, a compaction boundary, a carrier record, …) and so
/// must not appear in a distillation corpus. Keyed off [`MessageKind`], not the harness: injected
/// context (system reminders, hook output, environment context) is labelled as user INPUT — that is
/// where the model saw it on the wire — while the system prompt and a compaction summary are
/// `system`. `chat` picks the ChatML spelling, otherwise ShareGPT's.
fn training_role(m: &Message, chat: bool) -> Option<&'static str> {
    if !m.is_model_visible() {
        return None;
    }
    Some(match m.kind {
        MessageKind::SystemPrompt | MessageKind::CompactionSummary => "system",
        MessageKind::InjectedContext | MessageKind::Prompt => {
            if chat {
                "user"
            } else {
                "human"
            }
        }
        MessageKind::Reply => {
            if chat {
                "assistant"
            } else {
                "gpt"
            }
        }
        MessageKind::ToolResult => "tool",
        // A sub-agent spawn or return trains as the role that carried it.
        MessageKind::SubagentSpawn | MessageKind::SubagentReturn => match m.role {
            Role::System => "system",
            Role::User => {
                if chat {
                    "user"
                } else {
                    "human"
                }
            }
            Role::Assistant => {
                if chat {
                    "assistant"
                } else {
                    "gpt"
                }
            }
            Role::Tool => "tool",
        },
        MessageKind::Notice
        | MessageKind::CompactionBoundary
        | MessageKind::ModelChange
        | MessageKind::Error
        | MessageKind::Branch
        | MessageKind::Carrier => return None,
    })
}

/// Chunk size for streaming a span's content into the JSONL writer (bounds peak per giant field).
const SPAN_CHUNK: usize = 1 << 20; // 1 MiB

/// Stream one message's blocks as training text to `emit`, resolving span content **in chunks** (so a
/// giant field is never materialized whole). Parts are separated by `\n\n`, with empty/redacted
/// text turns producing no part.
fn render_blocks_into<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    resolver: &crate::lazy::Resolver,
    emit: &mut dyn FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    render_blocks_impl(blocks, resolver, None, emit)
}

/// Per-block redaction context for [`render_blocks_impl`]: the options plus the running stats.
type RedactCtx<'a> = (&'a crate::redact::RedactOptions, &'a mut crate::redact::RedactStats);

/// The shared block renderer behind [`render_blocks_into`] (plain) and the redacting path of
/// [`write_record`].
///
/// Without `redact`, span content is chunk-resolved straight to `emit` (a giant field is never held
/// whole). With `redact`, each text field is scrubbed **per block** — resolve (a span must be
/// scanned whole to redact it), scrub, emit, drop — so the peak is one field, and nothing is cloned
/// when a field is clean ([`scrub_cow`](crate::redact::scrub_cow) borrows). Tool inputs materialize
/// only for structural scrubbing.
fn render_blocks_impl<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    resolver: &crate::lazy::Resolver,
    mut redact: Option<RedactCtx<'_>>,
    emit: &mut dyn FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut started = false;
    let sep = |started: &mut bool, emit: &mut dyn FnMut(&str) -> anyhow::Result<()>| {
        if *started {
            emit("\n\n")?;
        }
        *started = true;
        Ok::<_, anyhow::Error>(())
    };
    for b in blocks {
        match b {
            Block::Text { text } => {
                if let Some((opts, stats)) = redact.as_mut() {
                    let s = text.try_resolve(resolver)?;
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit)?;
                    emit(&crate::redact::scrub_cow(&s, opts, stats))?;
                } else if let Some(s) = text.inline_str() {
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit)?;
                    emit(s)?;
                } else {
                    sep(&mut started, emit)?;
                    text.try_for_each_chunk(resolver, SPAN_CHUNK, &mut *emit)?;
                }
            }
            Block::Thinking { text, redacted, .. } => {
                if *redacted {
                    continue;
                }
                if let Some((opts, stats)) = redact.as_mut() {
                    let s = text.try_resolve(resolver)?;
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit)?;
                    emit("<thinking>\n")?;
                    emit(&crate::redact::scrub_cow(&s, opts, stats))?;
                    emit("\n</thinking>")?;
                } else if let Some(s) = text.inline_str() {
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit)?;
                    emit("<thinking>\n")?;
                    emit(s)?;
                    emit("\n</thinking>")?;
                } else {
                    sep(&mut started, emit)?;
                    emit("<thinking>\n")?;
                    text.try_for_each_chunk(resolver, SPAN_CHUNK, &mut *emit)?;
                    emit("\n</thinking>")?;
                }
            }
            Block::ToolUse { name, input, .. } => {
                sep(&mut started, emit)?;
                emit("```tool_call\n")?;
                emit(name)?;
                emit(" ")?;
                struct TextWriter<'a>(&'a mut dyn FnMut(&str) -> anyhow::Result<()>);
                impl std::io::Write for TextWriter<'_> {
                    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                        let text = std::str::from_utf8(bytes).map_err(std::io::Error::other)?;
                        (self.0)(text).map_err(std::io::Error::other)?;
                        Ok(bytes.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                let mut writer = TextWriter(emit);
                if let Some((opts, stats)) = redact.as_mut() {
                    if crate::redact::json_needs_scrub(input, opts)? {
                        // Structural scrubbing explicitly owns only the demanded dirty tool value.
                        let mut owned = input.resolve()?.into_owned();
                        crate::redact::scrub_value(&mut owned, opts, stats);
                        serde_json::to_writer(&mut writer, &owned)?;
                    } else {
                        input.write_canonical(&mut writer)?;
                    }
                } else {
                    input.write_canonical(&mut writer)?;
                }
                emit("\n```")?;
            }
            Block::ToolResult { content, is_error, .. } => {
                sep(&mut started, emit)?;
                emit(if *is_error {
                    "```tool_result error\n"
                } else {
                    "```tool_result\n"
                })?;
                if let Some((opts, stats)) = redact.as_mut() {
                    let s = content.try_resolve(resolver)?;
                    emit(&crate::redact::scrub_cow(&s, opts, stats))?;
                } else if let Some(s) = content.inline_str() {
                    emit(s)?;
                } else {
                    content.try_for_each_chunk(resolver, SPAN_CHUNK, &mut *emit)?;
                }
                emit("\n```")?;
            }
            Block::Image { media_type, .. } => {
                sep(&mut started, emit)?;
                emit("[image")?;
                if let Some(m) = media_type.as_deref() {
                    emit(": ")?;
                    emit(m)?;
                }
                emit("]")?;
            }
            Block::File { path, mime, .. } => {
                sep(&mut started, emit)?;
                emit("[file: ")?;
                emit(path.as_deref().or(mime.as_deref()).unwrap_or("attachment"))?;
                emit("]")?;
            }
        }
    }
    Ok(())
}

/// Whether a message renders to no training text (so [`write_record`] drops it) — the
/// cheap, no-resolve predicate matching [`render_blocks_into`]'s emptiness (only empty/redacted text
/// turns are empty; any tool/image/file block, or any span, makes it non-empty).
fn message_is_empty(m: &Message) -> bool {
    !m.content.iter().any(|b| match b {
        Block::Text { text } => text.is_lazy() || text.inline_str().is_some_and(|s| !s.trim().is_empty()),
        Block::Thinking { text, redacted, .. } => {
            !*redacted && (text.is_lazy() || text.inline_str().is_some_and(|s| !s.trim().is_empty()))
        }
        _ => true,
    })
}

/// Session-level prompt copies use Message::text's plain-text convention. Compare decoded
/// chunks exactly, retaining no second prompt String; distinct system turns remain untouched.
fn represents_system_prompt(message: &Message, prompt: &str, resolver: &crate::Resolver) -> anyhow::Result<bool> {
    if message.kind != MessageKind::SystemPrompt || !message.is_model_visible() {
        return Ok(false);
    }
    let mut offset = 0usize;
    let mut equal = true;
    let mut compare = |part: &str| -> anyhow::Result<()> {
        let end = offset
            .checked_add(part.len())
            .ok_or_else(|| anyhow::anyhow!("system prompt length overflow"))?;
        equal &= prompt.as_bytes().get(offset..end) == Some(part.as_bytes());
        offset = end;
        Ok(())
    };
    let mut present = false;
    for block in &message.content {
        if let Block::Text { text } = block {
            if present {
                compare("\n")?;
            }
            text.try_for_each_chunk(resolver, SPAN_CHUNK, |part| {
                present |= !part.is_empty();
                compare(part)
            })?;
        }
    }
    Ok(equal && offset == prompt.len())
}

const fn record_header(format: Format) -> &'static [u8] {
    match format {
        Format::Chatml | Format::OpenAiTools => b"{\"messages\":[",
        Format::ShareGpt => b"{\"conversations\":[",
    }
}

/// `publish` skips a record header without knowing the format, so it reads into a buffer sized
/// for any of them — checked against the table above.
const RECORD_HEADER_BUF: usize = 32;
const _: () = assert!(record_header(Format::Chatml).len() <= RECORD_HEADER_BUF);
const _: () = assert!(record_header(Format::ShareGpt).len() <= RECORD_HEADER_BUF);
const _: () = assert!(record_header(Format::OpenAiTools).len() <= RECORD_HEADER_BUF);

/// Which export shape — `Chatml`, `ShareGpt`, or native `OpenAiTools` (`tool_calls` + `tool_call_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Chatml,
    ShareGpt,
    /// Structured OpenAI tool-calling schema (`tool_calls` array on assistant messages and
    /// `role: "tool", tool_call_id` messages for tool outputs) for modern SFT/RL trainers.
    OpenAiTools,
}

/// Canonical alias for [`Format`].
pub type DatasetFormat = Format;

impl Format {
    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Chatml => "chatml",
            Format::ShareGpt => "sharegpt",
            Format::OpenAiTools => "openai-tools",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chatml" => Some(Format::Chatml),
            "sharegpt" => Some(Format::ShareGpt),
            "openai-tools" | "openai_tools" | "openaitools" => Some(Format::OpenAiTools),
            _ => None,
        }
    }
}

/// Trajectory selection and rendering options for [`write_record_with_options`] and [`stage_record_with_options`].
#[derive(Debug, Clone)]
pub struct DatasetOptions {
    /// Walk `parent_id` backward from the session's newest model-visible leaf (and exclude
    /// sidechain / inactive-branch turns) so rewound edits and abandoned branches are omitted.
    pub active_branch_only: bool,
    /// Reject trajectories that end in an error record, an unanswered `ToolUse`, an unrecovered
    /// `is_error: true` `ToolResult`, or a non-assistant turn.
    pub clean_outcome_only: bool,
    /// Include `Block::Thinking` blocks (`<thinking>` in ChatML/ShareGPT, `reasoning_content` in
    /// `OpenAiTools`). Defaults to `true`.
    pub include_thinking: bool,
}

impl Default for DatasetOptions {
    fn default() -> Self {
        Self {
            active_branch_only: false,
            clean_outcome_only: false,
            include_thinking: true,
        }
    }
}

fn is_inactive_or_sidechain(m: &Message) -> bool {
    m.extra
        .get("isSidechain")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || m.extra
            .get("cv")
            .and_then(|v| v.get("inactive_branch"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// Select the message indices to export under `opts`, or `None` if `clean_outcome_only` rejects
/// the trajectory.
pub fn select_trajectory(messages: &[Message], opts: &DatasetOptions) -> Option<Vec<usize>> {
    let indices = if opts.active_branch_only {
        select_active_branch_indices(messages)
    } else {
        (0..messages.len()).collect()
    };
    if opts.clean_outcome_only && !trajectory_has_clean_outcome(messages, &indices) {
        return None;
    }
    Some(indices)
}

fn select_active_branch_indices(messages: &[Message]) -> Vec<usize> {
    let mut by_id: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (idx, m) in messages.iter().enumerate() {
        if is_inactive_or_sidechain(m) {
            continue;
        }
        if let Some(id) = m.id.as_deref().filter(|s| !s.is_empty()) {
            by_id.entry(id).or_insert(idx);
        }
    }
    let leaf = (0..messages.len()).rev().find(|&i| {
        let m = &messages[i];
        !is_inactive_or_sidechain(m)
            && (m.is_model_visible() || m.kind == MessageKind::Error)
            && m.id.as_deref().is_some_and(|s| !s.is_empty())
    });
    let Some(leaf_idx) = leaf else {
        return (0..messages.len())
            .filter(|&i| !is_inactive_or_sidechain(&messages[i]))
            .collect();
    };
    let has_parent_links = messages
        .iter()
        .any(|m| !is_inactive_or_sidechain(m) && m.parent_id.as_deref().is_some_and(|p| !p.is_empty()));
    if !has_parent_links {
        return (0..messages.len())
            .filter(|&i| !is_inactive_or_sidechain(&messages[i]))
            .collect();
    }

    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut cur = Some(leaf_idx);
    while let Some(idx) = cur {
        if !visited.insert(idx) {
            break;
        }
        chain.push(idx);
        cur = messages[idx]
            .parent_id
            .as_deref()
            .filter(|p| !p.is_empty())
            .and_then(|p| by_id.get(p).copied());
    }
    chain.reverse();
    if let Some((0, first)) = messages.iter().enumerate().next() {
        if first.kind == MessageKind::SystemPrompt
            && first.id.is_none()
            && !is_inactive_or_sidechain(first)
            && chain.first().copied() != Some(0)
        {
            chain.insert(0, 0);
        }
    }
    chain
}

fn trajectory_has_clean_outcome(messages: &[Message], indices: &[usize]) -> bool {
    let mut pending_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut pending_unnamed = 0usize;
    let mut unresolved_tool_error = false;
    let mut last_visible_role: Option<Role> = None;
    let mut saw_assistant = false;

    for &idx in indices {
        let m = &messages[idx];
        if m.kind == MessageKind::Error {
            return false;
        }
        if !m.is_model_visible() || message_is_empty(m) {
            continue;
        }
        last_visible_role = Some(m.role);
        let mut assistant_has_text = false;
        let mut assistant_called_tool = false;
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    if m.role == Role::Assistant
                        && (text.is_lazy() || text.inline_str().is_some_and(|s| !s.trim().is_empty()))
                    {
                        assistant_has_text = true;
                    }
                }
                Block::ToolUse { id, .. } => {
                    assistant_called_tool = true;
                    if id.is_empty() {
                        pending_unnamed += 1;
                    } else {
                        pending_ids.insert(id.as_str());
                    }
                }
                Block::ToolResult {
                    tool_use_id, is_error, ..
                } => {
                    if tool_use_id.is_empty() {
                        pending_unnamed = pending_unnamed.saturating_sub(1);
                    } else {
                        pending_ids.remove(tool_use_id.as_str());
                    }
                    if *is_error {
                        unresolved_tool_error = true;
                    }
                }
                _ => {}
            }
        }
        if m.role == Role::Assistant {
            saw_assistant = true;
            if assistant_has_text && !assistant_called_tool {
                unresolved_tool_error = false;
            }
        }
    }

    saw_assistant
        && last_visible_role == Some(Role::Assistant)
        && pending_ids.is_empty()
        && pending_unnamed == 0
        && !unresolved_tool_error
}

/// Write `session` as one JSONL record (no trailing newline) **streaming** — content (incl. giant
/// span fields) is rendered straight to `w` in chunks and JSON-escaped per chunk, so the whole record
/// is never held in memory. Returns `false` (writing nothing) when the session has no non-empty turns.
/// With `redact`, each demanded field is scrubbed before rendering. A source or writer error
/// may leave a prefix in `w`; publish the record only after this function succeeds.
pub fn write_record<W: std::io::Write + ?Sized>(
    session: &Session,
    w: &mut W,
    fmt: Format,
    redact: Option<&crate::redact::RedactOptions>,
) -> std::io::Result<bool> {
    write_record_with_options(session, w, fmt, redact, &DatasetOptions::default())
}

/// Like [`write_record`], applying [`DatasetOptions`] (active-branch selection, clean-outcome gate,
/// and optional thinking omission).
pub fn write_record_with_options<W: std::io::Write + ?Sized>(
    session: &Session,
    w: &mut W,
    fmt: Format,
    redact: Option<&crate::redact::RedactOptions>,
    opts: &DatasetOptions,
) -> std::io::Result<bool> {
    let Some(selected) = select_trajectory(&session.messages, opts) else {
        return Ok(false);
    };
    let mut record = RecordWriter::new(w, fmt, redact).with_include_thinking(opts.include_thinking);
    let resolver = session.resolver();
    if let Some(prompt) = session
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        let represented = selected
            .iter()
            .try_fold(false, |represented, &idx| {
                Ok::<_, anyhow::Error>(
                    represented || represents_system_prompt(&session.messages[idx], prompt, &resolver)?,
                )
            })
            .map_err(std::io::Error::other)?;
        if !represented {
            record.system_prompt(prompt)?;
        }
    }
    for idx in selected {
        record.message(&session.messages[idx], &resolver)?;
    }
    record.finish()
}

/// Incremental training record renderer. Retains formatting state only; each message is rendered
/// and dropped by the caller. Empty messages do not open the record. Errors can leave a prefix in
/// the writer, so callers must stage bytes until the complete source pass and its filters succeed.
pub struct RecordWriter<'a, W: std::io::Write + ?Sized> {
    writer: &'a mut W,
    fmt: Format,
    redact: Option<&'a crate::redact::RedactOptions>,
    include_thinking: bool,
    started: bool,
    synthetic_call_seq: usize,
    pending_synth_ids: std::collections::VecDeque<String>,
}

impl<'a, W: std::io::Write + ?Sized> RecordWriter<'a, W> {
    pub fn new(writer: &'a mut W, fmt: Format, redact: Option<&'a crate::redact::RedactOptions>) -> Self {
        Self {
            writer,
            fmt,
            redact,
            include_thinking: true,
            started: false,
            synthetic_call_seq: 0,
            pending_synth_ids: std::collections::VecDeque::new(),
        }
    }

    pub fn with_include_thinking(mut self, include_thinking: bool) -> Self {
        self.include_thinking = include_thinking;
        self
    }

    /// Render one model-visible, nonempty message, preserving the whole-session writer's key
    /// order, separators, redaction and role spelling. Returns whether this message contributed.
    pub fn message(&mut self, message: &Message, resolver: &crate::lazy::Resolver) -> std::io::Result<bool> {
        let chat = matches!(self.fmt, Format::Chatml | Format::OpenAiTools);
        let Some(role) = training_role(message, chat) else {
            return Ok(false);
        };
        if message_is_empty(message) {
            return Ok(false);
        }
        // The deferred eligibility check stays cheap so rejected queries never resolve body
        // sources. Accepted rendering must apply the same whitespace policy to lazy and owned
        // fields, including individual blocks inside an otherwise nonempty message.
        let mut blocks = Vec::new();
        for block in &message.content {
            let text = match block {
                Block::Text { text } => Some(text),
                Block::Thinking {
                    text, redacted: false, ..
                } if self.include_thinking => Some(text),
                Block::Thinking { .. } => continue,
                _ => None,
            };
            let keep = match text {
                Some(text) => {
                    if let Some(value) = text.inline_str() {
                        !value.trim().is_empty()
                    } else {
                        !crate::harness::store::document::text_head(text, resolver, 1, true)
                            .map_err(std::io::Error::other)?
                            .0
                            .is_empty()
                    }
                }
                None => true,
            };
            if keep {
                blocks.push(block);
            }
        }
        if blocks.is_empty() {
            return Ok(false);
        }
        if self.fmt == Format::OpenAiTools {
            return self.write_openai_tools_message(role, &blocks, resolver);
        }
        let redact = self.redact;
        self.write_message(role, |emit| {
            if let Some(opts) = redact {
                let mut stats = crate::redact::RedactStats::default();
                render_blocks_impl(blocks, resolver, Some((opts, &mut stats)), emit)
            } else {
                render_blocks_into(blocks, resolver, emit)
            }
        })
    }

    fn write_openai_tools_message(
        &mut self,
        role: &str,
        blocks: &[&Block],
        resolver: &crate::lazy::Resolver,
    ) -> std::io::Result<bool> {
        if role == "assistant" {
            let mut thinking: Vec<&crate::lazy::Text> = Vec::new();
            let mut content_blocks: Vec<&Block> = Vec::new();
            let mut calls: Vec<(&str, &str, &crate::json::Json)> = Vec::new();
            let mut extra_tool_results: Vec<&Block> = Vec::new();

            for &b in blocks {
                match b {
                    Block::Thinking {
                        text, redacted: false, ..
                    } => thinking.push(text),
                    Block::ToolUse { id, name, input, .. } => calls.push((id.as_str(), name.as_str(), input)),
                    Block::ToolResult { .. } => extra_tool_results.push(b),
                    Block::Text { .. } | Block::Image { .. } | Block::File { .. } => content_blocks.push(b),
                    Block::Thinking { .. } => {}
                }
            }
            if thinking.is_empty() && content_blocks.is_empty() && calls.is_empty() && extra_tool_results.is_empty() {
                return Ok(false);
            }
            let mut wrote = false;
            if !thinking.is_empty() || !content_blocks.is_empty() || !calls.is_empty() {
                self.open_item()?;
                wrote = true;
                self.writer.write_all(b"{\"role\":\"assistant\",\"content\":")?;
                if content_blocks.is_empty() {
                    self.writer.write_all(b"null")?;
                } else {
                    self.writer.write_all(b"\"")?;
                    let redact = self.redact;
                    let w = &mut self.writer;
                    let mut emit = |piece: &str| write_json_escaped(w, piece).map_err(anyhow::Error::from);
                    if let Some(opts) = redact {
                        let mut stats = crate::redact::RedactStats::default();
                        render_blocks_impl(
                            content_blocks.iter().copied(),
                            resolver,
                            Some((opts, &mut stats)),
                            &mut emit,
                        )
                        .map_err(std::io::Error::other)?;
                    } else {
                        render_blocks_into(content_blocks.iter().copied(), resolver, &mut emit)
                            .map_err(std::io::Error::other)?;
                    }
                    self.writer.write_all(b"\"")?;
                }

                if !thinking.is_empty() {
                    self.writer.write_all(b",\"reasoning_content\":\"")?;
                    for (idx, t) in thinking.iter().enumerate() {
                        if idx > 0 {
                            write_json_escaped(self.writer, "\n\n")?;
                        }
                        stream_text_escaped(self.writer, t, resolver, self.redact)?;
                    }
                    self.writer.write_all(b"\"")?;
                }

                if !calls.is_empty() {
                    self.writer.write_all(b",\"tool_calls\":[")?;
                    for (idx, (id, name, input)) in calls.iter().enumerate() {
                        if idx > 0 {
                            self.writer.write_all(b",")?;
                        }
                        let call_id: std::borrow::Cow<'_, str> = if id.is_empty() {
                            self.synthetic_call_seq += 1;
                            let gen = format!("call_{}", self.synthetic_call_seq);
                            self.pending_synth_ids.push_back(gen.clone());
                            std::borrow::Cow::Owned(gen)
                        } else {
                            std::borrow::Cow::Borrowed(*id)
                        };
                        self.writer.write_all(b"{\"id\":\"")?;
                        write_json_escaped(self.writer, &call_id)?;
                        self.writer
                            .write_all(b"\",\"type\":\"function\",\"function\":{\"name\":\"")?;
                        write_json_escaped(self.writer, name)?;
                        self.writer.write_all(b"\",\"arguments\":\"")?;
                        stream_json_arguments_escaped(self.writer, input, self.redact)?;
                        self.writer.write_all(b"\"}}")?;
                    }
                    self.writer.write_all(b"]")?;
                }

                self.writer.write_all(b"}")?;
            }
            for tr in extra_tool_results {
                wrote |= self.write_single_openai_tool_result(tr, resolver)?;
            }
            return Ok(wrote);
        }

        let mut tool_results: Vec<&Block> = Vec::new();
        let mut rest_blocks: Vec<&Block> = Vec::new();
        for &b in blocks {
            if matches!(b, Block::ToolResult { .. }) {
                tool_results.push(b);
            } else {
                rest_blocks.push(b);
            }
        }

        let mut wrote = false;
        for tr in tool_results {
            wrote |= self.write_single_openai_tool_result(tr, resolver)?;
        }
        if !rest_blocks.is_empty() {
            let text_role = if role == "tool" { "user" } else { role };
            let redact = self.redact;
            wrote |= self.write_message(text_role, |emit| {
                if let Some(opts) = redact {
                    let mut stats = crate::redact::RedactStats::default();
                    render_blocks_impl(rest_blocks, resolver, Some((opts, &mut stats)), emit)
                } else {
                    render_blocks_into(rest_blocks, resolver, emit)
                }
            })?;
        }
        Ok(wrote)
    }

    fn write_single_openai_tool_result(
        &mut self,
        block: &Block,
        resolver: &crate::lazy::Resolver,
    ) -> std::io::Result<bool> {
        let Block::ToolResult {
            tool_use_id,
            content,
            tool_name,
            ..
        } = block
        else {
            return Ok(false);
        };
        let call_id: std::borrow::Cow<'_, str> = if tool_use_id.is_empty() {
            std::borrow::Cow::Owned(self.pending_synth_ids.pop_front().unwrap_or_else(|| "call_0".into()))
        } else {
            std::borrow::Cow::Borrowed(tool_use_id.as_str())
        };
        self.open_item()?;
        self.writer.write_all(b"{\"role\":\"tool\",\"tool_call_id\":\"")?;
        write_json_escaped(self.writer, &call_id)?;
        self.writer.write_all(b"\"")?;
        if let Some(name) = tool_name.as_deref().filter(|n| !n.is_empty()) {
            self.writer.write_all(b",\"name\":\"")?;
            write_json_escaped(self.writer, name)?;
            self.writer.write_all(b"\"")?;
        }
        self.writer.write_all(b",\"content\":\"")?;
        stream_text_escaped(self.writer, content, resolver, self.redact)?;
        self.writer.write_all(b"\"}")?;
        Ok(true)
    }

    fn open_item(&mut self) -> std::io::Result<()> {
        if self.started {
            self.writer.write_all(b",")?;
        } else {
            self.writer.write_all(record_header(self.fmt))?;
            self.started = true;
        }
        Ok(())
    }

    fn system_prompt(&mut self, prompt: &str) -> std::io::Result<bool> {
        if prompt.trim().is_empty() {
            return Ok(false);
        }
        let redact = self.redact;
        self.write_message("system", |emit| {
            if let Some(opts) = redact {
                let mut stats = crate::redact::RedactStats::default();
                emit(&crate::redact::scrub_cow(prompt, opts, &mut stats))
            } else {
                emit(prompt)
            }
        })
    }

    fn write_message(
        &mut self,
        role: &str,
        render: impl FnOnce(&mut dyn FnMut(&str) -> anyhow::Result<()>) -> anyhow::Result<()>,
    ) -> std::io::Result<bool> {
        let (role_key, content_key) = match self.fmt {
            Format::Chatml | Format::OpenAiTools => ("role", "content"),
            Format::ShareGpt => ("from", "value"),
        };
        // Keep the established lexicographic outer keys and insertion-ordered tool JSON.
        let role_first = role_key < content_key;
        self.open_item()?;
        let w = &mut self.writer;
        if role_first {
            write!(w, "{{\"{role_key}\":\"{role}\",\"{content_key}\":\"")?;
        } else {
            write!(w, "{{\"{content_key}\":\"")?;
        }
        let mut emit = |piece: &str| write_json_escaped(w, piece).map_err(anyhow::Error::from);
        render(&mut emit).map_err(std::io::Error::other)?;
        if role_first {
            w.write_all(b"\"}")?;
        } else {
            write!(w, "\",\"{role_key}\":\"{role}\"}}")?;
        }
        Ok(true)
    }

    /// Close the JSON record, or write nothing when no message contributed. No trailing newline.
    pub fn finish(self) -> std::io::Result<bool> {
        if self.started {
            self.writer.write_all(b"]}")?;
        }
        Ok(self.started)
    }
}

fn stream_text_escaped<W: std::io::Write + ?Sized>(
    w: &mut W,
    text: &crate::lazy::Text,
    resolver: &crate::lazy::Resolver,
    redact: Option<&crate::redact::RedactOptions>,
) -> std::io::Result<()> {
    if let Some(opts) = redact {
        let resolved = text.try_resolve(resolver).map_err(std::io::Error::other)?;
        let mut stats = crate::redact::RedactStats::default();
        let scrubbed = crate::redact::scrub_cow(&resolved, opts, &mut stats);
        write_json_escaped(w, &scrubbed)
    } else if let Some(s) = text.inline_str() {
        write_json_escaped(w, s)
    } else {
        text.try_for_each_chunk(resolver, SPAN_CHUNK, |chunk| {
            write_json_escaped(w, chunk).map_err(anyhow::Error::from)
        })
        .map_err(std::io::Error::other)
    }
}

fn stream_json_arguments_escaped<W: std::io::Write + ?Sized>(
    w: &mut W,
    input: &crate::json::Json,
    redact: Option<&crate::redact::RedactOptions>,
) -> std::io::Result<()> {
    struct EscapedJsonBytes<'a, W: std::io::Write + ?Sized> {
        inner: &'a mut W,
        carry: [u8; 4],
        carry_len: usize,
    }
    impl<W: std::io::Write + ?Sized> std::io::Write for EscapedJsonBytes<'_, W> {
        fn write(&mut self, mut buf: &[u8]) -> std::io::Result<usize> {
            let total = buf.len();
            if self.carry_len > 0 {
                while self.carry_len < 4 && !buf.is_empty() {
                    self.carry[self.carry_len] = buf[0];
                    self.carry_len += 1;
                    buf = &buf[1..];
                    if let Ok(s) = std::str::from_utf8(&self.carry[..self.carry_len]) {
                        write_json_escaped(self.inner, s)?;
                        self.carry_len = 0;
                        break;
                    }
                }
                if self.carry_len == 4 {
                    let s = std::str::from_utf8(&self.carry).map_err(std::io::Error::other)?;
                    write_json_escaped(self.inner, s)?;
                    self.carry_len = 0;
                }
            }
            if !buf.is_empty() {
                match std::str::from_utf8(buf) {
                    Ok(s) => write_json_escaped(self.inner, s)?,
                    Err(err) => {
                        let valid = err.valid_up_to();
                        if valid > 0 {
                            let s = std::str::from_utf8(&buf[..valid]).map_err(std::io::Error::other)?;
                            write_json_escaped(self.inner, s)?;
                        }
                        if err.error_len().is_some() {
                            return Err(std::io::Error::other("invalid UTF-8 in tool JSON"));
                        }
                        let tail = &buf[valid..];
                        self.carry[..tail.len()].copy_from_slice(tail);
                        self.carry_len = tail.len();
                    }
                }
            }
            Ok(total)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.carry_len != 0 {
                return Err(std::io::Error::other("truncated UTF-8 in tool JSON"));
            }
            self.inner.flush()
        }
    }

    let mut esc = EscapedJsonBytes {
        inner: w,
        carry: [0; 4],
        carry_len: 0,
    };
    if let Some(opts) = redact {
        if crate::redact::json_needs_scrub(input, opts).map_err(std::io::Error::other)? {
            let mut owned = input.resolve().map_err(std::io::Error::other)?.into_owned();
            let mut stats = crate::redact::RedactStats::default();
            crate::redact::scrub_value(&mut owned, opts, &mut stats);
            serde_json::to_writer(&mut esc, &owned).map_err(std::io::Error::other)?;
        } else {
            input.write_canonical(&mut esc).map_err(std::io::Error::other)?;
        }
    } else {
        input.write_canonical(&mut esc).map_err(std::io::Error::other)?;
    }
    use std::io::Write as _;
    esc.flush()
}

/// An adapter failed before a complete dataset record was available. Corpus callers may report
/// this input and continue; demanded rendering, query and staging failures are returned directly.
#[derive(Debug)]
pub struct InputError(anyhow::Error);
impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for InputError {}

/// A complete, accepted record stored in bounded private staging. Publish only this successful
/// result; skipped records and failed passes never expose their staged prefix to corpus output.
/// An EOF-only metadata prompt may supply a leading item; publication joins its bounded stage
/// with the existing body rather than staging a second complete record.
pub struct StagedRecord {
    leading: Option<crate::harness::store::bytes::ByteSource>,
    body: crate::harness::store::bytes::ByteSource,
    skip_header: usize,
}
impl StagedRecord {
    /// Copy the JSONL record, including its newline, to the corpus destination.
    pub fn publish<W: std::io::Write + ?Sized>(&self, writer: &mut W) -> std::io::Result<()> {
        use std::io::Read;
        if let Some(leading) = &self.leading {
            leading.copy_to(writer)?;
        }
        if self.skip_header == 0 {
            self.body.copy_to(writer)?;
        } else {
            let mut body = self.body.reader()?;
            let mut header = [0; RECORD_HEADER_BUF];
            body.read_exact(&mut header[..self.skip_header])?;
            std::io::copy(&mut body, writer)?;
        }
        Ok(())
    }
}

/// Render and query one lazy stream without collecting its message history. Records are staged
/// until successful EOF, raw-message minimum and full query evaluation. Filtered histories stage
/// message projections with source descriptors and render only accepted records. Unfiltered
/// histories stage bytes directly. Rendered system-text projections use the same bounded spill
/// transport for exact deduplication against the final session-level prompt; distinct system
/// turns retain their order. Redaction still demands one complete text/tool field at a time;
/// ordinary rendering resolves giant payloads in chunks. Source metadata scales with distinct
/// captured source paths/identities, while message/byte retention spills under fixed budgets.
#[allow(clippy::too_many_arguments)]
pub fn stage_record(
    adapter: &dyn crate::harness::Adapter,
    r: &crate::ir::SessionRef,
    fmt: Format,
    redact: Option<&crate::redact::RedactOptions>,
    min_messages: usize,
    query: Option<&crate::SessionQuery>,
    text_sets: &crate::query::TextSets,
) -> anyhow::Result<Option<StagedRecord>> {
    stage_record_with_options(
        adapter,
        r,
        fmt,
        redact,
        min_messages,
        query,
        text_sets,
        &DatasetOptions::default(),
    )
}

/// Like [`stage_record`], applying [`DatasetOptions`] (active-branch selection, clean-outcome gate,
/// and optional thinking omission).
#[allow(clippy::too_many_arguments)]
pub fn stage_record_with_options(
    adapter: &dyn crate::harness::Adapter,
    r: &crate::ir::SessionRef,
    fmt: Format,
    redact: Option<&crate::redact::RedactOptions>,
    min_messages: usize,
    query: Option<&crate::SessionQuery>,
    text_sets: &crate::query::TextSets,
    opts: &DatasetOptions,
) -> anyhow::Result<Option<StagedRecord>> {
    use std::io::Write;
    let needs_trajectory_pass = opts.active_branch_only || opts.clean_outcome_only;
    struct Sink<'a> {
        record: RecordWriter<'a, std::io::BufWriter<crate::harness::store::bytes::ByteStage>>,
        deferred: Option<crate::spill::Ordered<deferred::DeferredMessage>>,
        systems: crate::spill::Ordered<deferred::DeferredMessage>,
        min_messages: usize,
        wait_eof: bool,
        needs_trajectory_pass: bool,
        sources: crate::json::SourceRegistry,
        resolver: crate::lazy::Resolver,
        facts: Option<crate::query::QueryReducer<'a>>,
        messages: usize,
        latch: crate::stream::Latch,
    }
    impl Sink<'_> {
        fn render(&mut self, mut message: Message) -> anyhow::Result<()> {
            let rendered = self.record.message(&message, &self.resolver)?;
            if rendered && message.kind == MessageKind::SystemPrompt {
                message.content.retain(|block| matches!(block, Block::Text { .. }));
                self.systems
                    .push(deferred::DeferredMessage::stage(message, &mut self.sources))?;
            }
            Ok(())
        }
    }
    impl crate::MessageSink for Sink<'_> {
        fn message(&mut self, message: Message) -> crate::Flow {
            self.messages += 1;
            let result = (|| {
                if let Some(facts) = &mut self.facts {
                    facts.observe(&message)?;
                }
                if let Some(deferred) = &mut self.deferred {
                    let chat = matches!(self.record.fmt, Format::Chatml | Format::OpenAiTools);
                    let keep = if self.needs_trajectory_pass {
                        training_role(&message, chat).is_some()
                            || message.kind == MessageKind::Error
                            || message.id.is_some()
                    } else {
                        training_role(&message, chat).is_some() && !message_is_empty(&message)
                    };
                    if keep {
                        deferred.push(deferred::DeferredMessage::stage(message, &mut self.sources))?;
                    }
                    if !self.wait_eof && self.messages >= self.min_messages {
                        for row in self.deferred.take().expect("deferred messages").rows()? {
                            let message = row?.restore(&self.sources)?;
                            self.render(message)?;
                        }
                    }
                } else {
                    self.render(message)?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            match result {
                Ok(()) => crate::Flow::Continue,
                Err(error) => {
                    self.latch.fail(error);
                    crate::Flow::Stop
                }
            }
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.latch.take("dataset")
        }
    }
    let wait_eof = query.is_some_and(|q| !q.is_empty()) || needs_trajectory_pass;
    let mut bytes = std::io::BufWriter::with_capacity(64 * 1024, crate::harness::store::bytes::ByteStage::new());
    let mut sink = Sink {
        record: RecordWriter::new(&mut bytes, fmt, redact).with_include_thinking(opts.include_thinking),
        deferred: (wait_eof || min_messages > 0).then(Default::default),
        systems: Default::default(),
        min_messages,
        wait_eof,
        needs_trajectory_pass,
        sources: Default::default(),
        resolver: crate::lazy::Resolver::new(Some(r.path.clone())),
        facts: query.map(crate::query::QueryReducer::new),
        messages: 0,
        latch: Default::default(),
    };
    let counted_query = query.is_some_and(crate::SessionQuery::needs_counted);
    let read = crate::stream::stream_counted(
        adapter,
        r,
        &crate::Demand::LEAN.with_spans(),
        &mut sink,
        crate::stream::Pass::RECORD,
    );
    let (session, counted) = match read {
        Ok(done) => (done.session, done.counted),
        Err(error) if sink.latch.failed() => return Err(error),
        Err(error) => return Err(InputError(error).into()),
    };
    let counts = counted.filter(|_| counted_query).map(|c| c.counts);
    if sink.messages < min_messages {
        return Ok(None);
    }
    if let Some(facts) = sink.facts.take() {
        if !facts.matches(r, &session, text_sets, counts)? {
            return Ok(None);
        }
    }
    if let Some(deferred) = sink.deferred.take() {
        if needs_trajectory_pass {
            let mut restored = Vec::new();
            for row in deferred.rows()? {
                restored.push(row?.restore(&sink.sources)?);
            }
            let Some(selected) = select_trajectory(&restored, opts) else {
                return Ok(None);
            };
            if selected.len() < min_messages {
                return Ok(None);
            }
            for idx in selected {
                sink.render(restored[idx].clone())?;
            }
        } else {
            for row in deferred.rows()? {
                let message = row?.restore(&sink.sources)?;
                sink.render(message)?;
            }
        }
    }
    let prompt = session
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty());
    let mut represented = false;
    if let Some(prompt) = prompt {
        for row in sink.systems.rows()? {
            let message = row?.restore(&sink.sources)?;
            if represents_system_prompt(&message, prompt, &sink.resolver)? {
                represented = true;
                break;
            }
        }
    }
    let has_body = sink.record.finish()?;
    if !has_body && (prompt.is_none() || represented) {
        return Ok(None);
    }
    if has_body {
        bytes.write_all(b"\n")?;
    }
    let leading = if let Some(prompt) = prompt.filter(|_| !represented) {
        let mut prefix = std::io::BufWriter::with_capacity(64 * 1024, crate::harness::store::bytes::ByteStage::new());
        {
            let mut record = RecordWriter::new(&mut prefix, fmt, redact).with_include_thinking(opts.include_thinking);
            record.system_prompt(prompt)?;
            if !has_body {
                record.finish()?;
            }
        }
        prefix.write_all(if has_body { b"," } else { b"\n" })?;
        Some(prefix.into_inner().map_err(|error| error.into_error())?.finish()?)
    } else {
        None
    };
    let skip_header = if has_body && leading.is_some() {
        record_header(fmt).len()
    } else {
        0
    };
    Ok(Some(StagedRecord {
        leading,
        body: bytes.into_inner().map_err(|error| error.into_error())?.finish()?,
        skip_header,
    }))
}

/// Write `s` to `w` as the *body* of a JSON string (no surrounding quotes), JSON-escaped exactly as
/// `serde_json` would. Escaping is context-free, so escaping each chunk and concatenating equals
/// escaping the whole — keeping streamed output byte-identical to a one-shot `serde_json::to_string`.
fn write_json_escaped<W: std::io::Write + ?Sized>(w: &mut W, s: &str) -> std::io::Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    let mut remaining = s;
    while !remaining.is_empty() {
        let mut end = remaining.len().min(SPAN_CHUNK);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let quoted = serde_json::to_string(&remaining[..end]).map_err(std::io::Error::other)?;
        w.write_all(&quoted.as_bytes()[1..quoted.len() - 1])?;
        remaining = &remaining[end..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Harness, Message};
    use serde_json::{json, Value};

    fn msg(role: Role, blocks: Vec<Block>) -> Message {
        Message {
            complete_record: None,
            id: None,
            parent_id: None,
            role,
            timestamp: None,
            model: None,
            content: blocks,
            usage: None,
            extra: serde_json::Map::new(),
            kind: crate::ir::MessageKind::for_role(role),
            origin: crate::ir::Origin::for_role(role),
        }
    }

    /// One record through the streaming writer — the only production path — parsed back.
    fn record(s: &Session, fmt: Format) -> Option<Value> {
        let mut buf = Vec::new();
        write_record(s, &mut buf, fmt, None)
            .unwrap()
            .then(|| serde_json::from_slice(&buf).unwrap())
    }

    fn session(messages: Vec<Message>) -> Session {
        Session {
            messages,
            ..Session::new("t", Harness::Claude)
        }
    }

    #[test]
    fn chatml_maps_roles_and_folds_tools() {
        let s = session(vec![
            msg(
                Role::User,
                vec![Block::Text {
                    text: "fix the bug".into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![
                    Block::Thinking {
                        text: "check the log".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::ToolUse {
                        id: "1".into(),
                        name: "Bash".into(),
                        input: (json!({"cmd": "grep x"})).into(),
                        namespace: None,
                    },
                ],
            ),
            msg(
                Role::Tool,
                vec![Block::ToolResult {
                    tool_use_id: "1".into(),
                    content: "found it".into(),
                    is_error: false,
                    tool_name: None,
                    status: None,
                    details: None,
                }],
            ),
        ]);
        let v = record(&s, Format::Chatml).expect("non-empty");
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].as_str().unwrap().contains("<thinking>"));
        assert!(msgs[1]["content"].as_str().unwrap().contains("```tool_call"));
        assert_eq!(msgs[2]["role"], "tool");
        assert!(msgs[2]["content"].as_str().unwrap().contains("found it"));
    }

    #[test]
    fn empty_session_writes_nothing() {
        assert!(record(&session(vec![]), Format::Chatml).is_none());
        let blank = session(vec![msg(Role::User, vec![Block::Text { text: "  ".into() }])]);
        assert!(record(&blank, Format::Chatml).is_none());
    }

    /// The per-block redacting renderer must produce exactly what "redact the whole session first,
    /// then render plain" produces — the old clone-and-materialize path it replaced.
    #[test]
    fn redacted_record_equals_redact_then_render() {
        let s = session(vec![
            msg(
                Role::User,
                vec![Block::Text {
                    text: "my key is sk-abcDEF1234567890ghijkl ok".into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![
                    Block::Thinking {
                        text: "they pasted sk-zzzzzzzzzzzzzzzzzzzz hmm".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::ToolUse {
                        id: "1".into(),
                        name: "Bash".into(),
                        input: (json!({"command": "export TOKEN=plain", "auth": "Bearer abcdef1234567890XYZ"})).into(),
                        namespace: None,
                    },
                ],
            ),
            msg(
                Role::Tool,
                vec![Block::ToolResult {
                    tool_use_id: "1".into(),
                    content: "leaked ghp_0123456789abcdefABCDEF0123456789abcd done".into(),
                    is_error: false,
                    tool_name: None,
                    status: None,
                    details: None,
                }],
            ),
            // A clean message: must render identically (and untouched) under redaction.
            msg(
                Role::User,
                vec![Block::Text {
                    text: "thanks, looks good".into(),
                }],
            ),
        ]);
        let opts = crate::redact::RedactOptions::default();

        let (scrubbed_session, _) = crate::redact::redact_with(&s, &opts).unwrap();
        let mut expect = Vec::new();
        write_record(&scrubbed_session, &mut expect, Format::Chatml, None).unwrap();

        let mut got = Vec::new();
        write_record(&s, &mut got, Format::Chatml, Some(&opts)).unwrap();

        let got = String::from_utf8(got).unwrap();
        assert_eq!(got, String::from_utf8(expect).unwrap());
        assert!(got.contains("[REDACTED:api_key]"));
        assert!(!got.contains("sk-abcDEF1234567890ghijkl"));
        assert!(!got.contains("ghp_0123456789abcdef"));
        assert!(got.contains("thanks, looks good"));
    }

    #[test]
    fn sharegpt_uses_from_value() {
        let s = session(vec![msg(Role::User, vec![Block::Text { text: "hi".into() }])]);
        let v = record(&s, Format::ShareGpt).unwrap();
        assert_eq!(v["conversations"][0]["from"], "human");
        assert_eq!(v["conversations"][0]["value"], "hi");
    }

    #[test]
    fn staged_records_match_collected_bytes_and_full_query_semantics() {
        let world = crate::test_world("staged-dataset-equivalence");
        let rich = "escaped\n\"🌙\" sk-abcDEF1234567890ghijkl".repeat(300);
        let path = world.write_session(
            "dataset",
            &[
                json!({"type":"user","uuid":"u","message":{"role":"user","content":"hello"}}),
                json!({"type":"assistant","uuid":"early","message":{"role":"assistant","model":"EarlyModel",
                "content":[{"type":"thinking","thinking":rich},
                    {"type":"tool_use","id":"call","name":"Edit","input":{"file_path":"a.rs","content":rich}}]}}),
                json!({"type":"user","uuid":"result","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call","is_error":true,"content":rich}]}}),
                json!({"type":"system","uuid":"notice","subtype":"api_error","error":{"message":"notice"}}),
                json!({"type":"assistant","uuid":"late","cwd":"/late/work","gitBranch":"FeatureBranch",
                "message":{"role":"assistant","model":"LateModel","content":"done"}}),
            ],
        );
        let r = crate::ir::SessionRef::new("dataset", Harness::Claude, path);
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let full = crate::stream::collect_with(adapter.as_ref(), &r, &crate::Demand::LEAN.with_spans()).unwrap();
        let opts = crate::redact::RedactOptions::default();
        for fmt in [Format::Chatml, Format::ShareGpt] {
            for redact in [None, Some(&opts)] {
                for expression in [
                    "",
                    "model:early AND model:late",
                    "git:feature",
                    "tool:edit AND has:errors",
                    "touched:/late/work/a.rs",
                    "NOT tool:edit",
                    "has:plan",
                    "NOT has:goal",
                    "text:hit",
                    "text:miss OR model:early",
                ] {
                    let query = crate::SessionQuery::parse(expression).unwrap();
                    let texts = crate::query::TextSets::resolve(&query, |needle| {
                        if needle == "hit" {
                            std::collections::HashSet::from([r.id.clone()])
                        } else {
                            Default::default()
                        }
                    });
                    for min in [0, full.messages.len(), full.messages.len() + 1] {
                        let mut expected = Vec::new();
                        let matched = full.messages.len() >= min
                            && crate::query::matches_full(&query, &r, &full, &texts).unwrap();
                        if matched && write_record(&full, &mut expected, fmt, redact).unwrap() {
                            expected.push(b'\n');
                        }
                        let staged =
                            stage_record(adapter.as_ref(), &r, fmt, redact, min, Some(&query), &texts).unwrap();
                        let mut actual = Vec::new();
                        if let Some(record) = staged {
                            record.publish(&mut actual).unwrap();
                        }
                        assert_eq!(actual, expected, "query={expression:?}, min={min}");
                    }
                }
            }
        }
    }

    #[test]
    fn incremental_record_spelling_is_byte_exact() {
        let mut notice = msg(
            Role::System,
            vec![Block::Text {
                text: "never trains".into(),
            }],
        );
        notice.kind = MessageKind::Notice;
        let messages = vec![
            notice,
            msg(Role::User, vec![Block::Text { text: " ".into() }]),
            msg(
                Role::User,
                vec![Block::Text {
                    text: "a\n\"🌙\"".into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![Block::ToolUse {
                    id: "t".into(),
                    name: "Tool".into(),
                    namespace: None,
                    input: json!({"z":2,"a":"quoted\t"}).into(),
                }],
            ),
        ];
        let resolver = crate::lazy::Resolver::new(None);
        for (fmt, expected) in [
            (
                Format::Chatml,
                r#"{"messages":[{"content":"a\n\"🌙\"","role":"user"},{"content":"```tool_call\nTool {\"z\":2,\"a\":\"quoted\\t\"}\n```","role":"assistant"}]}"#,
            ),
            (
                Format::ShareGpt,
                r#"{"conversations":[{"from":"human","value":"a\n\"🌙\""},{"from":"gpt","value":"```tool_call\nTool {\"z\":2,\"a\":\"quoted\\t\"}\n```"}]}"#,
            ),
        ] {
            let mut bytes = Vec::new();
            let mut record = RecordWriter::new(&mut bytes, fmt, None);
            for message in &messages {
                record.message(message, &resolver).unwrap();
            }
            assert!(record.finish().unwrap());
            assert_eq!(std::str::from_utf8(&bytes).unwrap(), expected);
        }
    }

    #[test]
    fn unreadable_demanded_text_aborts_plain_and_redacted_records() {
        let path = std::env::temp_dir().join(format!("cv-dataset-missing-{}", uuid::Uuid::new_v4()));
        let text = crate::lazy::Text::Span(crate::lazy::Span {
            source: Some(path),
            offset: 0,
            len: 8192,
            escaped: false,
        });
        let session = session(vec![msg(Role::User, vec![Block::Text { text }])]);
        let opts = crate::redact::RedactOptions::default();
        for redact in [None, Some(&opts)] {
            assert!(write_record(&session, &mut Vec::new(), Format::Chatml, redact).is_err());
        }
    }

    #[test]
    fn unreadable_demanded_tool_value_aborts_record() {
        let world = crate::test_world("dataset-json-failure");
        let path = world.base.join("argument.json");
        let bytes = br#"{"rich":"selected value"}"#;
        std::fs::write(&path, bytes).unwrap();
        let input = crate::json::Json::source(&path, 0, bytes.len() as u64).unwrap();
        std::fs::remove_file(&path).unwrap();
        let session = session(vec![msg(
            Role::Assistant,
            vec![Block::ToolUse {
                id: "call".into(),
                name: "Tool".into(),
                namespace: None,
                input,
            }],
        )]);
        let opts = crate::redact::RedactOptions::default();
        for redact in [None, Some(&opts)] {
            assert!(write_record(&session, &mut Vec::new(), Format::Chatml, redact).is_err());
        }
    }

    #[test]
    fn giant_inline_escaping_preserves_unicode_and_control_bytes() {
        let text = "🌙\n\"\t\\".repeat(SPAN_CHUNK / 4);
        let mut bytes = Vec::new();
        write_json_escaped(&mut bytes, &text).unwrap();
        let expected = serde_json::to_string(&text).unwrap();
        assert_eq!(bytes, expected.as_bytes()[1..expected.len() - 1]);
    }

    #[test]
    fn backed_text_survives_late_model_query_and_minimum_staging() {
        struct Backed(Vec<Message>);
        impl crate::harness::Adapter for Backed {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &crate::ir::SessionRef,
                _: &crate::Demand,
                sink: &mut dyn crate::MessageSink,
            ) -> anyhow::Result<Session> {
                for message in &self.0 {
                    sink.try_message(message.clone())?;
                }
                let mut session = Session::new(&r.id, r.harness);
                session.source_path = Some(r.path.clone());
                Ok(session)
            }
        }
        let world = crate::test_world("dataset-backed-text-late-model");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot").unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let backing = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let body = "🌙\n\"".repeat(8192);
        let wire = serde_json::to_vec(&body).unwrap();
        let (offset, len) = backing
            .append(|out| {
                out.write_all(&wire)?;
                Ok(())
            })
            .unwrap();
        let text = source
            .read(
                ReadInput::Backing(backing.clone(), offset, len),
                Validation::Value,
                |view| view.source_text(),
            )
            .unwrap()
            .unwrap();
        let mut first = Message::new(Role::User);
        first.content.push(Block::Text { text });
        let mut last = Message::new(Role::Assistant);
        last.content.push(Block::Text {
            text: "finished".into(),
        });
        last.model = Some("present".into());
        let adapter = Backed(vec![first, last]);
        drop(source);
        drop(backing);
        let reference = crate::ir::SessionRef::new("backed", Harness::Claude, &path);
        let mut full = Session::new("backed", Harness::Claude);
        full.source_path = Some(path);
        full.messages = adapter.0.clone();
        let mut expected = Vec::new();
        assert!(write_record(&full, &mut expected, Format::Chatml, None).unwrap());
        expected.push(b'\n');
        for (minimum, expression, matched) in [
            (0, "model:present", true),
            (2, "model:present", true),
            (3, "model:present", false),
            (0, "model:absent", false),
        ] {
            let query = crate::SessionQuery::parse(expression).unwrap();
            let staged = stage_record(
                &adapter,
                &reference,
                Format::Chatml,
                None,
                minimum,
                Some(&query),
                &Default::default(),
            )
            .unwrap();
            assert_eq!(staged.is_some(), matched);
            if let Some(record) = staged {
                let mut actual = Vec::new();
                record.publish(&mut actual).unwrap();
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn rejected_filters_never_demand_training_text() {
        struct MissingText(std::path::PathBuf);
        impl crate::harness::Adapter for MissingText {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &crate::ir::SessionRef,
                _: &crate::Demand,
                sink: &mut dyn crate::MessageSink,
            ) -> anyhow::Result<Session> {
                let mut message = msg(
                    Role::Assistant,
                    vec![Block::Text {
                        text: crate::lazy::Text::Span(crate::lazy::Span {
                            source: Some(self.0.clone()),
                            offset: 0,
                            len: 8192,
                            escaped: false,
                        }),
                    }],
                );
                message.model = Some("present".into());
                sink.try_message(message)?;
                let mut session = Session::new(&r.id, r.harness);
                session.source_path = Some(r.path.clone());
                Ok(session)
            }
        }
        let world = crate::test_world("dataset-filter-before-render");
        let path = world.write_session("filtered", &[]);
        let r = crate::ir::SessionRef::new("filtered", Harness::Claude, path);
        let adapter = MissingText(world.base.join("missing-text"));
        let miss = crate::SessionQuery::parse("model:absent").unwrap();
        let hit = crate::SessionQuery::parse("model:present").unwrap();
        let opts = crate::redact::RedactOptions::default();
        for redact in [None, Some(&opts)] {
            for (min, query) in [(2, None), (0, Some(&miss))] {
                assert!(
                    stage_record(&adapter, &r, Format::Chatml, redact, min, query, &Default::default())
                        .unwrap()
                        .is_none()
                );
            }
            let error = stage_record(&adapter, &r, Format::Chatml, redact, 0, Some(&hit), &Default::default())
                .err()
                .unwrap();
            assert!(!error.is::<InputError>(), "accepted payload failures remain fatal");
        }
    }

    #[test]
    fn late_adapter_failure_discards_staged_prefix() {
        struct Fails;
        impl crate::harness::Adapter for Fails {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                _: &crate::ir::SessionRef,
                _: &crate::Demand,
                sink: &mut dyn crate::MessageSink,
            ) -> anyhow::Result<Session> {
                sink.try_message(msg(
                    Role::User,
                    vec![Block::Text {
                        text: "valid prefix".into(),
                    }],
                ))?;
                anyhow::bail!("late input unavailable")
            }
        }
        let world = crate::test_world("staged-dataset-read-failure");
        let path = world.write_session("fail", &[]);
        let r = crate::ir::SessionRef::new("fail", Harness::Claude, path);
        let error = stage_record(&Fails, &r, Format::Chatml, None, 0, None, &Default::default())
            .err()
            .unwrap();
        assert!(error.is::<InputError>());
        assert!(error.to_string().contains("late input unavailable"));
        assert!(crate::catalog::AffordanceTable::load().counts(&r).is_none());
    }
    #[test]
    fn lazy_whitespace_fields_match_owned_turns_and_block_separators() {
        let world = cv_testkit::World::new("dataset-whitespace-parity");
        let whitespace = " \n\t\u{2003}".repeat(2048);
        let mut owned = Session::new("whitespace", Harness::Claude);
        owned.messages = vec![
            msg(
                Role::User,
                vec![Block::Text {
                    text: whitespace.clone().into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![
                    Block::Text {
                        text: whitespace.clone().into(),
                    },
                    Block::Thinking {
                        text: whitespace.clone().into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::Text {
                        text: format!("{whitespace}answer").into(),
                    },
                    Block::Thinking {
                        text: "reason".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                ],
            ),
            msg(
                Role::Tool,
                vec![Block::ToolResult {
                    tool_use_id: "t".into(),
                    content: whitespace.into(),
                    tool_name: None,
                    is_error: false,
                    status: None,
                    details: None,
                }],
            ),
        ];
        let mut lazy = owned.clone();
        for (mi, message) in lazy.messages.iter_mut().enumerate() {
            for (bi, block) in message.content.iter_mut().enumerate() {
                let text = match block {
                    Block::Text { text } | Block::Thinking { text, .. } => text,
                    Block::ToolResult { content, .. } => content,
                    _ => continue,
                };
                let wire = serde_json::to_vec(text.inline_str().unwrap()).unwrap();
                let path = world.base.join(format!("{mi}-{bi}.json"));
                std::fs::write(&path, &wire).unwrap();
                *text = crate::Json::source(&path, 0, wire.len() as u64)
                    .unwrap()
                    .with_view(|view| view.source_text())
                    .unwrap()
                    .unwrap();
            }
        }
        for format in [Format::Chatml, Format::ShareGpt] {
            for redact in [None, Some(crate::redact::RedactOptions::default())] {
                let mut expected = Vec::new();
                let mut actual = Vec::new();
                assert!(write_record(&owned, &mut expected, format, redact.as_ref()).unwrap());
                assert!(write_record(&lazy, &mut actual, format, redact.as_ref()).unwrap());
                assert_eq!(actual, expected);
            }
        }
        lazy.messages.truncate(1);
        let mut actual = Vec::new();
        assert!(!write_record(&lazy, &mut actual, Format::Chatml, None).unwrap());
        assert!(actual.is_empty());
    }
    struct SystemMetadataFixture {
        session: Session,
        initial_prompt: Option<String>,
    }
    impl crate::Adapter for SystemMetadataFixture {
        fn harness(&self) -> Harness {
            Harness::OpenSession
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> anyhow::Result<crate::Discovered> {
            Ok(Default::default())
        }
        fn stream(
            &self,
            reference: &crate::SessionRef,
            _: &crate::Demand,
            sink: &mut dyn crate::MessageSink,
        ) -> anyhow::Result<Session> {
            let mut meta = Session::new(reference.id.clone(), reference.harness);
            meta.system_prompt = self.initial_prompt.clone();
            meta.model = Some("present".into());
            sink.meta(&meta);
            for message in &self.session.messages {
                if sink.message(message.clone()) == crate::Flow::Stop {
                    break;
                }
            }
            meta.system_prompt = self.session.system_prompt.clone();
            Ok(meta)
        }
    }
    fn system_message(texts: &[&str]) -> Message {
        let mut message = msg(
            Role::System,
            texts.iter().map(|text| Block::Text { text: (*text).into() }).collect(),
        );
        message.kind = MessageKind::SystemPrompt;
        message
    }

    #[test]
    fn session_only_system_prompts_are_leading_and_equivalent_copies_do_not_duplicate() -> anyhow::Result<()> {
        let world = crate::test_world("dataset-system-metadata");
        let path = world.base.join("source");
        std::fs::write(&path, "stable source")?;
        let reference = crate::SessionRef::new("system-metadata", Harness::OpenSession, path);
        let user = msg(
            Role::User,
            vec![Block::Text {
                text: "user prompt".into(),
            }],
        );
        let cases = [
            ("metadata only", vec![user.clone()], 2),
            (
                "after",
                vec![system_message(&["before"]), user.clone(), system_message(&["after"])],
                3,
            ),
            ("metadata", vec![system_message(&["distinct"]), user.clone()], 3),
            (
                "first\nsecond",
                vec![system_message(&["", "first", "second"]), user.clone()],
                2,
            ),
            (" \n\t", vec![user.clone()], 1),
            ("only metadata", vec![], 1),
        ];
        for (prompt, messages, turns) in cases {
            let mut session = Session::new("system-metadata", Harness::OpenSession);
            session.system_prompt = Some(prompt.into());
            session.messages = messages;
            let fixture = SystemMetadataFixture {
                session,
                initial_prompt: Some("obsolete header prompt".into()),
            };
            for format in [Format::Chatml, Format::ShareGpt] {
                let mut expected = Vec::new();
                assert!(write_record(&fixture.session, &mut expected, format, None)?);
                let parsed: Value = serde_json::from_slice(&expected)?;
                let (array, role, content) = match format {
                    Format::Chatml | Format::OpenAiTools => ("messages", "role", "content"),
                    Format::ShareGpt => ("conversations", "from", "value"),
                };
                assert_eq!(parsed[array].as_array().unwrap().len(), turns);
                if !prompt.trim().is_empty()
                    && fixture
                        .session
                        .messages
                        .iter()
                        .all(|message| message.kind != MessageKind::SystemPrompt)
                {
                    assert_eq!(parsed[array][0][role], "system");
                    assert_eq!(parsed[array][0][content], prompt);
                }
                expected.push(b'\n');
                for query in [
                    None,
                    Some(crate::SessionQuery::parse("model:present").map_err(anyhow::Error::msg)?),
                ] {
                    let staged = stage_record(
                        &fixture,
                        &reference,
                        format,
                        None,
                        0,
                        query.as_ref(),
                        &Default::default(),
                    )?
                    .unwrap();
                    let mut actual = Vec::new();
                    staged.publish(&mut actual)?;
                    assert_eq!(actual, expected, "{prompt:?}");
                    assert!(!String::from_utf8(actual)?.contains("obsolete header prompt"));
                }
            }
        }
        // Session metadata is not a transcript message for minimum/query count selection.
        let mut session = Session::new("minimum", Harness::OpenSession);
        session.system_prompt = Some("metadata".into());
        session.messages.push(user);
        let fixture = SystemMetadataFixture {
            session,
            initial_prompt: None,
        };
        assert!(stage_record(&fixture, &reference, Format::Chatml, None, 2, None, &Default::default())?.is_none());
        Ok(())
    }

    #[test]
    fn checked_system_prompt_equivalence_spills_and_redacts_without_losing_changes() -> anyhow::Result<()> {
        let world = crate::test_world("dataset-system-source");
        let prompt = format!("{}\nprivate@example.com", "prompt λ ".repeat(512));
        let path = world.base.join("prompt.json");
        let wire = serde_json::to_vec(&prompt)?;
        std::fs::write(&path, &wire)?;
        let checked = crate::Json::source(&path, 0, wire.len() as u64)?
            .with_view(|view| view.source_text())?
            .unwrap();
        let mut session = Session::new("checked-system", Harness::OpenSession);
        session.system_prompt = Some(prompt.clone());
        let mut checked_message = msg(Role::System, vec![Block::Text { text: checked.clone() }]);
        checked_message.kind = MessageKind::SystemPrompt;
        for _ in 0..1200 {
            session.messages.push(checked_message.clone());
        }
        session.messages.push(system_message(&["distinct later system change"]));
        let fixture = SystemMetadataFixture {
            session,
            initial_prompt: None,
        };
        let reference = crate::SessionRef::new("checked-system", Harness::OpenSession, path.clone());
        let redact = crate::redact::RedactOptions::default();
        for format in [Format::Chatml, Format::ShareGpt] {
            let mut expected = Vec::new();
            assert!(write_record(&fixture.session, &mut expected, format, Some(&redact))?);
            expected.push(b'\n');
            let query = crate::SessionQuery::parse("model:present").map_err(anyhow::Error::msg)?;
            let staged = stage_record(
                &fixture,
                &reference,
                format,
                Some(&redact),
                0,
                Some(&query),
                &Default::default(),
            )?
            .unwrap();
            let mut actual = Vec::new();
            staged.publish(&mut actual)?;
            assert_eq!(actual, expected);
            assert!(!std::str::from_utf8(&actual)?.contains("private@example.com"));
            let value: Value = serde_json::from_slice(&actual)?;
            let (array, content) = match format {
                Format::Chatml | Format::OpenAiTools => ("messages", "content"),
                Format::ShareGpt => ("conversations", "value"),
            };
            assert_eq!(value[array].as_array().unwrap().len(), 1201);
            assert_eq!(value[array][1200][content], "distinct later system change");
        }
        std::fs::remove_file(&path)?;
        let rejected = crate::SessionQuery::parse("model:absent").map_err(anyhow::Error::msg)?;
        assert!(stage_record(
            &fixture,
            &reference,
            Format::Chatml,
            None,
            0,
            Some(&rejected),
            &Default::default()
        )?
        .is_none());
        assert!(stage_record(&fixture, &reference, Format::Chatml, None, 0, None, &Default::default()).is_err());
        Ok(())
    }
    #[test]
    fn lmstudio_metadata_only_system_prompt_reaches_dataset_output() -> anyhow::Result<()> {
        let world = crate::test_world("dataset-lmstudio-system");
        let path = world.base.join("conversation.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "systemPrompt":"Follow the house style.",
                "messages":[{"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"Write a function."}]}]}]
            }))?,
        )?;
        let adapter = crate::harness::lmstudio::LmStudio::with_root(&world.base);
        let reference = crate::SessionRef::from_path(&path, Harness::LmStudio);
        let collected = crate::stream::collect_with(&adapter, &reference, &crate::Demand::LEAN.with_spans())?;
        assert_eq!(collected.system_prompt.as_deref(), Some("Follow the house style."));
        assert!(collected
            .messages
            .iter()
            .all(|message| message.kind != MessageKind::SystemPrompt));
        let staged = stage_record(&adapter, &reference, Format::Chatml, None, 0, None, &Default::default())?.unwrap();
        let mut output = Vec::new();
        staged.publish(&mut output)?;
        let output: Value = serde_json::from_slice(&output)?;
        assert_eq!(
            output["messages"],
            json!([
                {"role":"system","content":"Follow the house style."},
                {"role":"user","content":"Write a function."},
            ])
        );
        Ok(())
    }
    #[test]
    fn giant_whitespace_source_text_and_thinking_do_not_open_training_turns() -> anyhow::Result<()> {
        use std::io::Write;
        let world = cv_testkit::World::new("dataset-giant-whitespace");
        let path = world.base.join("whitespace.json");
        let mut file = std::fs::File::create(&path)?;
        file.write_all(b"\"")?;
        for _ in 0..32 {
            file.write_all(&[b' '; 64 * 1024])?;
        }
        file.write_all(b"\"")?;
        drop(file);
        let text = crate::Json::source(&path, 0, std::fs::metadata(&path)?.len())?
            .with_view(|view| view.source_text())?
            .unwrap();
        assert!(matches!(text, crate::Text::Source(_)));
        let mut session = Session::new("giant-whitespace", Harness::Claude);
        session.messages.push(msg(
            Role::Assistant,
            vec![
                Block::Text { text: text.clone() },
                Block::Thinking {
                    text,
                    signature: None,
                    encrypted: None,
                    redacted: false,
                },
            ],
        ));
        for format in [Format::Chatml, Format::ShareGpt, Format::OpenAiTools] {
            let mut output = Vec::new();
            assert!(!write_record(&session, &mut output, format, None)?);
            assert!(output.is_empty());
        }
        Ok(())
    }

    #[test]
    fn openai_tools_format_emits_structured_tool_calls_and_matches_staged_stream() -> anyhow::Result<()> {
        let world = crate::test_world("dataset-openai-tools");
        let path = world.base.join("source");
        std::fs::write(&path, "stable")?;
        let reference = crate::SessionRef::new("oai-1", Harness::OpenSession, path);

        let mut s = Session::new("oai-1", Harness::OpenSession);
        s.system_prompt = Some("You are a concise coding agent.".into());
        s.messages = vec![
            msg(
                Role::User,
                vec![Block::Text {
                    text: "Run grep and summarize.".into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![
                    Block::Thinking {
                        text: "Need to inspect matches first.".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::ToolUse {
                        id: "call_grep_1".into(),
                        name: "Bash".into(),
                        input: json!({"cmd": "rg -n foo src/", "secret": "sk-abcDEF1234567890ghijkl"}).into(),
                        namespace: None,
                    },
                ],
            ),
            msg(
                Role::Tool,
                vec![Block::ToolResult {
                    tool_use_id: "call_grep_1".into(),
                    content: "src/lib.rs:10:foo".into(),
                    is_error: false,
                    tool_name: Some("Bash".into()),
                    status: None,
                    details: None,
                }],
            ),
            msg(
                Role::Assistant,
                vec![Block::Text {
                    text: "Found `foo` at `src/lib.rs:10`.".into(),
                }],
            ),
        ];

        assert_eq!(DatasetFormat::parse("openai-tools"), Some(Format::OpenAiTools));
        assert_eq!(Format::OpenAiTools.as_str(), "openai-tools");

        let redact = crate::redact::RedactOptions::default();
        let mut raw = Vec::new();
        assert!(write_record(&s, &mut raw, Format::OpenAiTools, Some(&redact))?);
        let parsed: Value = serde_json::from_slice(&raw)?;
        let msgs = parsed["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(
            msgs[0],
            json!({"role": "system", "content": "You are a concise coding agent."})
        );
        assert_eq!(msgs[1], json!({"role": "user", "content": "Run grep and summarize."}));
        assert_eq!(msgs[2]["role"], "assistant");
        assert!(
            msgs[2]["content"].is_null(),
            "textless tool call turn must emit content: null"
        );
        assert_eq!(msgs[2]["reasoning_content"], "Need to inspect matches first.");
        let tcs = msgs[2]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "call_grep_1");
        assert_eq!(tcs[0]["type"], "function");
        assert_eq!(tcs[0]["function"]["name"], "Bash");
        let args: Value = serde_json::from_str(tcs[0]["function"]["arguments"].as_str().unwrap())?;
        assert_eq!(args["cmd"], "rg -n foo src/");
        assert_eq!(args["secret"], "[REDACTED:api_key]");

        assert_eq!(
            msgs[3],
            json!({
                "role": "tool",
                "tool_call_id": "call_grep_1",
                "name": "Bash",
                "content": "src/lib.rs:10:foo"
            })
        );
        assert_eq!(
            msgs[4],
            json!({
                "role": "assistant",
                "content": "Found `foo` at `src/lib.rs:10`."
            })
        );

        // Verify exact byte parity with stage_record (both streaming and deferred query paths).
        let fixture = SystemMetadataFixture {
            session: s,
            initial_prompt: None,
        };
        let mut expected = raw;
        expected.push(b'\n');
        let query = crate::SessionQuery::parse("model:present").map_err(anyhow::Error::msg)?;
        let staged = stage_record(
            &fixture,
            &reference,
            Format::OpenAiTools,
            Some(&redact),
            2,
            Some(&query),
            &Default::default(),
        )?
        .unwrap();
        let mut actual = Vec::new();
        staged.publish(&mut actual)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn active_branch_and_clean_outcome_options_prune_rewinds_and_reject_broken_trajectories() -> anyhow::Result<()> {
        let mut u1 = msg(
            Role::User,
            vec![Block::Text {
                text: "v1 question".into(),
            }],
        );
        u1.id = Some("m1".into());

        // Abandoned branch off m1: failed tool call with error result.
        let mut abandoned_a = msg(
            Role::Assistant,
            vec![Block::ToolUse {
                id: "bad_call".into(),
                name: "Bash".into(),
                input: json!({"cmd": "false"}).into(),
                namespace: None,
            }],
        );
        abandoned_a.id = Some("m2_abandoned".into());
        abandoned_a.parent_id = Some("m1".into());

        let mut abandoned_tr = msg(
            Role::Tool,
            vec![Block::ToolResult {
                tool_use_id: "bad_call".into(),
                content: "exit 1".into(),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
            }],
        );
        abandoned_tr.id = Some("m3_abandoned".into());
        abandoned_tr.parent_id = Some("m2_abandoned".into());

        // Active winning branch off m1: clean assistant completion.
        let mut winning_a = msg(
            Role::Assistant,
            vec![Block::Text {
                text: "clean final answer".into(),
            }],
        );
        winning_a.id = Some("m2_win".into());
        winning_a.parent_id = Some("m1".into());

        let branched = session(vec![u1.clone(), abandoned_a.clone(), abandoned_tr.clone(), winning_a]);
        let opts = DatasetOptions {
            active_branch_only: true,
            clean_outcome_only: true,
            include_thinking: false,
        };
        let mut out = Vec::new();
        assert!(write_record_with_options(
            &branched,
            &mut out,
            Format::OpenAiTools,
            None,
            &opts
        )?);
        let v: Value = serde_json::from_slice(&out)?;
        assert_eq!(
            v["messages"],
            json!([
                {"role": "user", "content": "v1 question"},
                {"role": "assistant", "content": "clean final answer"}
            ])
        );

        // Session ending in an unrecovered tool error or dangling ToolUse is rejected by clean_outcome_only.
        let failed = session(vec![u1.clone(), abandoned_a.clone(), abandoned_tr]);
        let mut rejected = Vec::new();
        assert!(!write_record_with_options(
            &failed,
            &mut rejected,
            Format::OpenAiTools,
            None,
            &opts
        )?);
        assert!(rejected.is_empty());

        let dangling = session(vec![u1, abandoned_a]);
        assert!(!write_record_with_options(
            &dangling,
            &mut rejected,
            Format::OpenAiTools,
            None,
            &opts
        )?);
        Ok(())
    }
}
