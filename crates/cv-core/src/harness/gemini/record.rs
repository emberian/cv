//! The Gemini message codec: one validated `ConversationRecord` message view and the reader's
//! demand in, 0..2 IR messages out. Both recording eras feed it (each element of a document's
//! `messages[]`, each effective JSONL record) and so do checkpoints' parts; mapped, positioned
//! and parsed bytes differ only in the [`JsonView`] they hand it. The demand chooses which fields
//! stay in the source, never which code runs.

use super::*;
use crate::harness::store::{document::Fields, text};
use crate::harness::wire::genai::{self, Dialect, Part};
use crate::json::{JsonSource, JsonView};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use crate::lazy::{Resolver, Text};

/// Every top-level key any message kind reads, selected in one pass.
const TOP: &[&str] = &[
    "type",
    "id",
    "timestamp",
    "model",
    "tokens",
    "thoughts",
    "displayContent",
    "rewindTo",
    "content",
    "toolCalls",
];

/// What a read asks of the codec. `source` is set when spans are demanded and the bytes come
/// from a captured file: large strings and arguments then stay in it, and composite result or
/// thought text is generated into its decoded backing. Without it every field is owned.
#[derive(Clone, Copy)]
pub(super) struct Demand<'a> {
    pub harness: Harness,
    pub source: Option<&'a JsonSource>,
    pub keep_image_data: bool,
    pub resolver: &'a Resolver,
}

impl<'a> Demand<'a> {
    /// The read's demand over its captured `source`: under spans, large fields stay in it;
    /// discovery skips decoding inline base64 image payloads.
    pub fn of(harness: Harness, demand: &stream::Demand, source: &'a JsonSource, resolver: &'a Resolver) -> Self {
        Demand {
            harness,
            source: demand.spans.then_some(source),
            keep_image_data: demand.tier != crate::stream::Tier::Discovery,
            resolver,
        }
    }

    fn spans(self) -> bool {
        self.source.is_some()
    }

    /// One message through the codec. `published` remembers that a delivered message held a
    /// source descriptor, so the reader checks the source once more at the end.
    pub fn decode(
        self,
        view: JsonView<'_>,
        st: &mut EmitState,
        sink: &mut dyn MessageSink,
        published: &mut bool,
    ) -> Result<Flow> {
        message(view, self, st, &mut Out { sink, published })
    }
}

/// The sink, rechecking the source after a slow callback handed a source descriptor (a consumer
/// must not go on with a descriptor of a source that has changed); the reader's EOF check covers
/// the fast ones.
struct Out<'s> {
    sink: &'s mut dyn MessageSink,
    published: &'s mut bool,
}

impl Out<'_> {
    fn deliver(&mut self, m: Message, d: Demand<'_>) -> Result<Flow> {
        let sourced = m.content.iter().any(|b| match b {
            Block::Text { text } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => {
                matches!(text, Text::Source(_))
            }
            Block::ToolUse { input, .. } => matches!(input, crate::Json::Source(_)),
            _ => false,
        });
        let started = std::time::Instant::now();
        let flow = self.sink.message(m);
        if sourced {
            *self.published = true;
            if let Some(source) = d.source {
                source.check_after(started)?;
            }
        }
        Ok(flow)
    }
}

/// Translate one message into 0..2 IR messages (an assistant turn may be followed by a
/// [`Role::Tool`] turn for its results), threading [`EmitState`] across messages. Errors are
/// source failures, never malformed messages: those carry nothing.
fn message(view: JsonView<'_>, d: Demand<'_>, st: &mut EmitState, out: &mut Out<'_>) -> Result<Flow> {
    let Some(rec) = Fields::parse(view, TOP)? else {
        return Ok(Flow::Continue);
    };
    let mty = rec.string("type")?;
    let mty = mty.as_deref().unwrap_or("user");
    let ts = rec.string("timestamp")?.as_deref().and_then(parse_ts);
    let id = rec.string("id")?;

    match mty {
        "gemini" | "model" | "assistant" => {
            let mut m = Message::new(Role::Assistant);
            m.id = id;
            m.timestamp = ts;
            // The first model seen becomes `Session::model`; every turn carries the model its
            // record states, so a read that starts mid-file reports the same model a full one does.
            if let Some(model) = rec.string("model")? {
                if st.session_model.is_none() {
                    st.session_model = Some(model.clone());
                }
                m.model = Some(model);
            }
            m.usage = rec.get("tokens").map(tokens).transpose()?.flatten();

            // Thoughts → Thinking blocks (subject + description). `thoughts_listed` records that a
            // `thoughts[]` list with entries was present — whether or not any yielded text —
            // because then `content` carries the same thoughts as parts, which are dropped below.
            let mut thoughts_listed = false;
            if let Some(thoughts) = rec.get("thoughts").map(JsonView::array).transpose()?.flatten() {
                for thought in thoughts {
                    thoughts_listed = true;
                    if let Some(text) = d.thought(thought?)? {
                        m.content.push(Block::Thinking {
                            text,
                            signature: None,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
            }

            // Assistant content: string, or array of Parts (text / inlineData / functionCall). A
            // record re-synced from the API history (`updateMessagesFromHistory`, every `$set.messages`
            // since 0.46.0) carries its thoughts and calls twice: as `thought`/`functionCall` parts
            // in `content` AND in `thoughts[]`/`toolCalls[]`. The structured fields win; the parts
            // are kept only when the fields are absent (older records, checkpoints).
            let calls = match rec.get("toolCalls").map(JsonView::array).transpose()?.flatten() {
                Some(calls) => calls
                    .map(|call| {
                        Fields::parse(
                            call?,
                            &["id", "name", "agentId", "status", "args", "result", "resultDisplay"],
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
                None => Vec::new(),
            };
            let mut call_ids = std::collections::HashSet::new();
            for call in calls.iter().flatten() {
                if let Some(id) = call.string("id")? {
                    call_ids.insert(id);
                }
            }
            let mut parts = Vec::new();
            if let Some(content) = rec.get("content") {
                content_blocks(content, d, &mut parts)?;
            }
            m.content.extend(parts.into_iter().filter(|b| match b {
                Block::Thinking { .. } => !thoughts_listed,
                Block::ToolUse { id, .. } => !call_ids.contains(id.as_str()),
                _ => true,
            }));

            // Tool calls (gemini-cli ToolCallRecord) → ToolUse + paired ToolResult. A call with an
            // `agentId` ran a sub-agent (`invoke_agent`), whose own recording is
            // `chats/<this session>/<agentId>.jsonl`: the turn is a SubagentSpawn, its result a
            // SubagentReturn.
            let mut tool_results: Vec<Block> = Vec::new();
            let mut agent_ids: Vec<Value> = Vec::new();
            for call in &calls {
                let string = |key| call.as_ref().map_or(Ok(None), |c| c.string(key));
                let field = |key| call.as_ref().and_then(|c| c.get(key));
                let name = string("name")?.unwrap_or_else(|| "tool".into());
                let call_id = string("id")?.unwrap_or_else(|| format!("{name}-call"));
                let input = match field("args") {
                    Some(args) => args.argument(d.spans())?,
                    None => Value::Null.into(),
                };
                if let Some(agent) = string("agentId")? {
                    agent_ids.push(Value::String(agent));
                }
                m.content.push(Block::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input,
                    namespace: None,
                });
                let status = string("status")?;
                let is_error = status.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("error"));
                let content = match field("result").map(|v| d.result(v)).transpose()?.flatten() {
                    Some(text) => Some(text),
                    None => field("resultDisplay")
                        .map(|v| v.lazy_text(d.spans()))
                        .transpose()?
                        .flatten(),
                }
                .unwrap_or_default();
                // A tool that completed with EMPTY output still ran and still answered — the
                // turn is real. Requiring non-empty text dropped it, and with it the whole Tool
                // turn, so a `ToolSearch` that found nothing vanished from a round trip. A call
                // with no `status` yet has not completed and still contributes no result.
                if !content.is_empty() || status.is_some() {
                    st.answered.insert(call_id.clone());
                    tool_results.push(Block::ToolResult {
                        tool_use_id: call_id,
                        content,
                        is_error,
                        tool_name: Some(name),
                        status,
                        details: None,
                    });
                }
            }

            let spawned = !agent_ids.is_empty();
            if spawned {
                m.kind = MessageKind::SubagentSpawn;
                m.harness_extra_mut(d.harness)
                    .insert("subagent_ids".into(), Value::Array(agent_ids));
            }
            if !m.content.is_empty() && out.deliver(m, d)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
            // Emit tool results as a separate Tool-role turn (mirrors how Gemini feeds
            // functionResponses back as a user turn, but kept distinct in the IR).
            if !tool_results.is_empty() {
                let mut tm = Message::new(Role::Tool);
                if spawned {
                    tm.kind = MessageKind::SubagentReturn;
                    tm.origin = Origin::Subagent;
                }
                tm.timestamp = ts;
                tm.content = tool_results;
                if out.deliver(tm, d)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
            }
        }
        "info" | "error" | "warning" => {
            // Harness notices (`info`/`warning`) and errors the model never answered (`error`):
            // System turns, searchable, typed by kind; the record type is a Gemini fact.
            let kind = if mty == "error" {
                MessageKind::Error
            } else {
                MessageKind::Notice
            };
            let mut m = Message::of_kind(Role::System, kind, Origin::Harness);
            m.id = id;
            m.timestamp = ts;
            m.harness_extra_mut(d.harness)
                .insert("record_type".into(), Value::String(mty.to_string()));
            if let Some(content) = rec.get("content") {
                content_blocks(content, d, &mut m.content)?;
            }
            if !m.content.is_empty() && out.deliver(m, d)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        "rewind" => {
            // A `$rewindTo` control record (see `RecordingReplay::ingest`): what follows does not
            // continue what precedes. Kept as a Branch marker so a reader sees the cut; it is not a
            // message record, so it carries no id or timestamp of its own.
            let target = rec.string("rewindTo")?.unwrap_or_default();
            let mut m = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
            m.content.push(Block::Text {
                text: format!("[rewound to {target}]").into(),
            });
            let bag = m.harness_extra_mut(d.harness);
            bag.insert("record_type".into(), Value::String("rewind".into()));
            bag.insert("rewind_to".into(), Value::String(target));
            if out.deliver(m, d)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        _ => {
            let mut blocks = Vec::new();
            let tool_response = match rec.get("content") {
                Some(content) => content_blocks(content, d, &mut blocks)?,
                None => false,
            };
            if tool_response {
                // A `user` record of bare `functionResponse` parts: the tool output fed back to the
                // model (see [`EmitState::answered`]). A copy of results already emitted is dropped;
                // otherwise (no `toolCalls[]` result to pair with) it is the Tool turn itself.
                blocks.retain(|b| match b {
                    Block::ToolResult { tool_use_id, .. } => st.answered.insert(tool_use_id.clone()),
                    _ => true,
                });
                if !blocks.is_empty() {
                    let mut m = Message::new(Role::Tool);
                    m.id = id;
                    m.timestamp = ts;
                    m.content = blocks;
                    if out.deliver(m, d)? == Flow::Stop {
                        return Ok(Flow::Stop);
                    }
                }
                return Ok(Flow::Continue);
            }
            // user (and anything else) → a human prompt, or context gemini-cli injected.
            let mut m = Message::new(Role::User);
            m.id = id;
            m.timestamp = ts;
            m.content = blocks;
            // A BeforeAgent hook's `additionalContext` rides in the SAME record as the prompt, as a
            // trailing `<hook_context>` part (0.46.0, 0.63.0): split it off into its own Hook turn.
            let mut hook = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Hook);
            if m.content.len() > 1 {
                let (hooked, rest): (Vec<Block>, Vec<Block>) = std::mem::take(&mut m.content)
                    .into_iter()
                    .partition(|b| matches!(b, Block::Text { text } if d.is_hook(text)));
                m.content = rest;
                hook.content = hooked;
                if m.content.is_empty() {
                    m.content = std::mem::take(&mut hook.content);
                }
            }
            d.user_facts(&mut m, st);
            // `displayContent`: what the user typed when it differs from what the model got (a
            // custom `/command` before expansion, an `@file` prompt before the file was inlined).
            if let Some(typed) = rec
                .get("displayContent")
                .map(display_text)
                .transpose()?
                .filter(|t| !t.is_empty())
            {
                m.harness_extra_mut(d.harness)
                    .insert("display_text".into(), Value::String(typed));
            }
            if !m.content.is_empty() && out.deliver(m, d)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
            if !hook.content.is_empty() {
                hook.timestamp = ts;
                if out.deliver(hook, d)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
            }
        }
    }
    Ok(Flow::Continue)
}

/// Gemini's `tokens.input` is the API's `promptTokenCount`, which *includes* `cached`; the IR's
/// disjoint `input_tokens` is the remainder (the emitter adds `cached` back).
fn tokens(v: JsonView<'_>) -> Result<Option<Usage>> {
    let Some(fields) = Fields::parse(v, &["input", "output", "cached"])? else {
        return Ok(None);
    };
    let get = |k: &str| -> Result<Option<u64>> {
        Ok(fields
            .get(k)
            .map(JsonView::as_number)
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64()))
    };
    let cached = get("cached")?;
    Ok(Some(Usage {
        input_tokens: get("input")?.map(|i| i.saturating_sub(cached.unwrap_or(0))),
        output_tokens: get("output")?,
        cache_read_tokens: cached,
        ..Usage::default()
    }))
}

/// Append IR blocks for a Gemini `content` value, a `PartListUnion`: a bare string, a single Part
/// object, or an array of Parts. True when it is a nonempty array of `functionResponse` parts
/// only (a tool-response turn).
pub(super) fn content_blocks(view: JsonView<'_>, d: Demand<'_>, out: &mut Vec<Block>) -> Result<bool> {
    if let Some(text) = view.lazy_text(d.spans())? {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
    } else if let Some(parts) = view.array()? {
        let mut present = false;
        let mut tool_response = true;
        for part in parts {
            present = true;
            tool_response &= part_block(part?, d, out)?;
        }
        return Ok(present && tool_response);
    } else {
        part_block(view, d, out)?;
    }
    Ok(false)
}

/// One Part: text / inlineData / fileData / functionCall / functionResponse. True when it has a
/// `functionResponse` key, whatever its value or which field won dispatch.
fn part_block(view: JsonView<'_>, d: Demand<'_>, out: &mut Vec<Block>) -> Result<bool> {
    if let Some(text) = view.lazy_text(d.spans())? {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
        return Ok(false);
    }
    let selected = genai::select_with_images(view, Dialect::Gemini, d.keep_image_data)?;
    match selected.part {
        Some(Part::Call(fc)) => {
            if let Some(fc) = Fields::parse(fc, &["name", "id", "args"])? {
                let name = fc.string("name")?.unwrap_or_else(|| "tool".into());
                let id = fc.string("id")?.unwrap_or_else(|| format!("{name}-call"));
                out.push(Block::ToolUse {
                    id,
                    name,
                    input: match fc.get("args") {
                        Some(args) => args.argument(d.spans())?,
                        None => Value::Null.into(),
                    },
                    namespace: None,
                });
            }
        }
        Some(Part::Response(fr)) => {
            if let Some(fr) = Fields::parse(fr, &["name", "id", "response"])? {
                let name = fr.string("name")?.unwrap_or_else(|| "tool".into());
                let id = fr.string("id")?.unwrap_or_else(|| format!("{name}-call"));
                out.push(Block::ToolResult {
                    tool_use_id: id,
                    content: match fr.get("response") {
                        Some(response) => d.text(value_chunk(response)?)?,
                        None => Text::default(),
                    },
                    is_error: false,
                    tool_name: Some(name),
                    status: None,
                    details: None,
                });
            }
        }
        Some(Part::Media(block)) => out.push(block),
        Some(Part::Text { value, thought, .. }) => {
            if let Some(text) = value.lazy_text(d.spans())? {
                if !text.is_empty() {
                    out.push(genai::text_block(text, thought, None));
                }
            }
        }
        None => {}
    }
    Ok(selected.response_present)
}

/// One piece of a tool result or thought: nothing, a string, or a value shown as compact JSON.
#[derive(Clone, Copy)]
enum Chunk<'a> {
    Empty,
    String(JsonView<'a>),
    Json(JsonView<'a>),
}

impl Chunk<'_> {
    fn present(self) -> Result<bool> {
        Ok(match self {
            Self::Empty => false,
            Self::Json(_) => true,
            Self::String(value) => match value.source_range()? {
                Some((_, len)) => len > 2,
                None => value.resolve()?.as_str().is_some_and(|s| !s.is_empty()),
            },
        })
    }
}

/// Display text of a value, unwrapping the common `{output}` / `{error}` wrapper.
fn value_chunk(view: JsonView<'_>) -> Result<Chunk<'_>> {
    if view.is_string()? {
        return Ok(Chunk::String(view));
    }
    if view.is_null()? {
        return Ok(Chunk::Empty);
    }
    if let Some(fields) = Fields::parse(view, &["output", "error"])? {
        for key in ["output", "error"] {
            if let Some(value) = fields.get(key) {
                if value.is_string()? {
                    return Ok(Chunk::String(value));
                }
            }
        }
    }
    Ok(Chunk::Json(view))
}

/// A result part: a `functionResponse`'s response, a text part's text, or the part itself.
fn part_chunk(view: JsonView<'_>, text_part: bool) -> Result<Chunk<'_>> {
    if let Some(fields) = Fields::parse(view, &["functionResponse", "text"])? {
        if let Some(response) = fields.get("functionResponse") {
            return response
                .get("response")?
                .map(value_chunk)
                .transpose()
                .map(|v| v.unwrap_or(Chunk::Empty));
        }
        if text_part {
            if let Some(value) = fields.get("text") {
                if value.is_string()? {
                    return Ok(Chunk::String(value));
                }
            }
        }
    }
    value_chunk(view)
}

impl Demand<'_> {
    fn text(self, chunk: Chunk<'_>) -> Result<Text> {
        match (chunk, self.source) {
            (Chunk::Empty, _) => Ok(Text::default()),
            (Chunk::String(value), Some(_)) => Ok(value.source_text()?.unwrap_or_default()),
            (Chunk::String(value), None) => Ok(value.text(false)?.unwrap_or_default()),
            (Chunk::Json(_), _) => self.join(&[chunk], "", ""),
        }
    }

    /// `head`, then the chunks separated by `between`: generated into the source's decoded
    /// backing under spans, owned otherwise.
    fn join(self, chunks: &[Chunk<'_>], head: &str, between: &str) -> Result<Text> {
        if let Some(source) = self.source {
            return text::append(source, &source.decoded_backing()?, |writer| {
                writer.push(head)?;
                for (i, chunk) in chunks.iter().enumerate() {
                    if i != 0 {
                        writer.push(between)?;
                    }
                    match *chunk {
                        Chunk::Empty => {}
                        Chunk::String(value) => {
                            writer.push_string(value)?;
                        }
                        Chunk::Json(value) => writer.push_json(value)?,
                    }
                }
                Ok(())
            });
        }
        let mut out = head.to_owned();
        for (i, chunk) in chunks.iter().enumerate() {
            if i != 0 {
                out.push_str(between);
            }
            match *chunk {
                Chunk::Empty => {}
                Chunk::String(value) => out.push_str(value.resolve()?.as_str().unwrap_or_default()),
                Chunk::Json(value) => out.push_str(&value.resolve()?.to_string()),
            }
        }
        Ok(Text::Inline(out))
    }

    /// A thought's text: `**subject** description`, or whichever of the two is nonempty.
    fn thought(self, thought: JsonView<'_>) -> Result<Option<Text>> {
        let string = |key| -> Result<Chunk<'_>> {
            Ok(match thought.get(key)? {
                Some(value) if value.is_string()? => Chunk::String(value),
                _ => Chunk::Empty,
            })
        };
        let subject = string("subject")?;
        let description = string("description")?;
        Ok(match (subject.present()?, description.present()?) {
            (false, false) => None,
            (true, false) => Some(self.text(subject)?),
            (false, true) => Some(self.text(description)?),
            (true, true) => Some(self.join(&[subject, description], "**", "** ")?),
        })
    }

    /// Human-readable text of a `toolCall.result` (a `PartListUnion`, typically a list of
    /// `{functionResponse:{response:{output}}}` parts): present pieces joined by newlines.
    fn result(self, value: JsonView<'_>) -> Result<Option<Text>> {
        if value.is_string()? {
            return self.text(Chunk::String(value)).map(Some);
        }
        if let Some(parts) = value.array()? {
            let mut present = Vec::new();
            for part in parts {
                let chunk = part_chunk(part?, true)?;
                if chunk.present()? {
                    present.push(chunk);
                }
            }
            return match present.as_slice() {
                [] => Ok(None),
                [one] => self.text(*one).map(Some),
                many => self.join(many, "", "\n").map(Some),
            };
        }
        if value.is_object()? {
            self.text(part_chunk(value, false)?).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// The text of a `displayContent` (`PartListUnion`): a string, or its parts' `text` joined.
fn display_text(view: JsonView<'_>) -> Result<String> {
    if view.is_string()? {
        return Ok(view.resolve()?.as_str().unwrap_or_default().to_owned());
    }
    let mut text = String::new();
    let mut append = |view: JsonView<'_>| -> Result<()> {
        let string = if view.is_string()? {
            Some(view)
        } else {
            view.get("text")?
        };
        if let Some(string) = string {
            string.for_each_str_chunk(|chunk| {
                text.push_str(chunk);
                Ok(())
            })?;
        }
        Ok(())
    };
    if let Some(parts) = view.array()? {
        for part in parts {
            append(part?)?;
        }
    } else if view.is_object()? {
        append(view)?;
    }
    Ok(text)
}

impl Demand<'_> {
    /// First non-whitespace characters, and the title's first 81 characters. Scanning has
    /// bounded state even for a pasted field that begins with megabytes of whitespace.
    fn inspect<'t>(self, texts: impl Iterator<Item = &'t Text>) -> TextInfo {
        let mut info = TextInfo::default();
        let mut present = false;
        for text in texts {
            if present {
                info.feed("\n");
            }
            match text {
                Text::Inline(s) => info.feed(s),
                Text::Source(source) => {
                    let mut prefix = info.clone();
                    let determined = source
                        .try_prefix_with_truncation(4096)
                        .ok()
                        .is_some_and(|(text, truncated)| {
                            prefix.feed(&text);
                            prefix.complete() || !truncated
                        });
                    if determined {
                        info = prefix;
                    } else {
                        // Leading whitespace can hide the classification marker beyond the
                        // bounded prefix. Preserve the complete fallback for that case.
                        let _ = source.try_for_each_chunk(4096, |part| {
                            info.feed(part);
                            Ok(())
                        });
                    }
                }
                Text::Span(sp) => {
                    // Ordinary prompts need only a short prefix. Fall back to a bounded scan
                    // when leading whitespace hides the classification marker beyond it.
                    let mut prefix = info.clone();
                    prefix.feed(&self.resolver.try_resolve_prefix(sp, 4096).unwrap_or_default());
                    if prefix.complete() || sp.len <= 4096 {
                        info = prefix;
                    } else {
                        let _ = self.resolver.try_for_each_chunk(sp, 4096, |s| {
                            info.feed(s);
                            Ok(())
                        });
                    }
                }
            }
            present |= !text.is_empty();
            if info.complete() {
                break;
            }
        }
        info
    }

    fn is_hook(self, text: &Text) -> bool {
        self.inspect(std::iter::once(text))
            .trimmed
            .starts_with("<hook_context>")
    }

    /// Type a user turn by its text, and take the first real prompt as the fallback title.
    fn user_facts(self, m: &mut Message, st: &mut EmitState) {
        let info = self.inspect(m.content.iter().filter_map(|b| match b {
            Block::Text { text } => Some(text),
            _ => None,
        }));
        type_user_turn_from(m, &info.trimmed);
        if st.title.is_none() && info.has_text && !is_ignored_user_content(&info.trimmed) {
            st.title = Some(info.head.finish());
        }
    }
}

#[derive(Clone)]
struct TextInfo {
    head: crate::projection::Head,
    trimmed: String,
    trimmed_chars: usize,
    has_text: bool,
    nonwhite: bool,
}

impl Default for TextInfo {
    fn default() -> Self {
        Self {
            head: crate::projection::Head::bounded(crate::projection::Head::TITLE),
            trimmed: String::new(),
            trimmed_chars: 0,
            has_text: false,
            nonwhite: false,
        }
    }
}

impl TextInfo {
    fn complete(&self) -> bool {
        self.head.done() && self.trimmed_chars == 64
    }

    fn feed(&mut self, s: &str) {
        self.has_text |= !s.is_empty();
        for c in s.chars() {
            self.head.push_char(c);
            if !self.nonwhite && !c.is_whitespace() {
                self.nonwhite = true;
            }
            if self.nonwhite && self.trimmed_chars < 64 {
                self.trimmed.push(c);
                self.trimmed_chars += 1;
            }
            if self.complete() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_input_is_made_disjoint_from_cached() {
        // Gemini's `input` is `promptTokenCount`, which includes `cached`.
        let v = serde_json::json!({"input": 1000, "output": 40, "cached": 600});
        let u = tokens((&v).into()).unwrap().unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (Some(400), Some(600), Some(40))
        );
        assert_eq!(u.prompt_tokens(), 1000, "the prompt the model read is unchanged");
    }

    #[test]
    fn retained_prompt_inspection_matches_owned_prefix_and_whitespace_fallback() {
        let dir = cv_testkit::temp_dir("gemini-inspect-prefix");
        let path = dir.join("text.json");
        let resolver = Resolver::new(None);
        let d = Demand {
            harness: Harness::Gemini,
            source: None,
            keep_image_data: true,
            resolver: &resolver,
        };
        for body in [
            "ordinary é\n".repeat(8192),
            format!("{}<hook_context>context", " ".repeat(8192)),
            " ".repeat(8192),
            "short".into(),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            let json = crate::Json::source(&path, 0, bytes.len() as u64).unwrap();
            let text = json.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
            let actual = d.inspect(std::iter::once(&text));
            let mut expected = TextInfo::default();
            expected.feed(&body);
            assert_eq!(actual.head, expected.head);
            assert_eq!(actual.trimmed, expected.trimmed);
            assert_eq!(actual.has_text, expected.has_text);
            assert_eq!(d.is_hook(&text), body.trim_start().starts_with("<hook_context>"));
            std::fs::write(&path, "{} ").unwrap();
            if matches!(text, Text::Source(_)) {
                assert!(text.try_resolve(&resolver).is_err());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod genai_contracts {
    use super::*;
    use crate::harness::wire::genai_frozen as frozen;
    #[test]
    fn decoder_matches_independent_frozen_parts() -> Result<()> {
        let world = crate::test_world("gemini-genai-frozen");
        let path = world.base.join("parts.json");
        let resolver = Resolver::new(Some(path.clone()));
        let mut rows = Vec::new();
        for (case, parts) in frozen::cases().into_iter().enumerate() {
            let value = parts;
            let bytes = serde_json::to_vec(&value)?;
            std::fs::write(&path, &bytes)?;
            let source = JsonSource::capture(&path)?;
            for demand in 0..2 {
                let (blocks, flag) = source.read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                    let d = Demand {
                        harness: Harness::Gemini,
                        source: if demand == 0 { None } else { Some(&source) },
                        keep_image_data: true,
                        resolver: &resolver,
                    };
                    let mut blocks = Vec::new();
                    let flag = content_blocks(view, d, &mut blocks)?;
                    Ok((blocks, flag))
                })?;
                rows.push(serde_json::json!({"case":case,"demand":demand,"source_sha256":crate::digest::sha256_hex(&bytes),"result":frozen::row(blocks,flag,&resolver)?}));
            }
            assert_eq!(std::fs::read(&path)?, bytes, "decoder changed source");
        }
        frozen::assert_frozen(
            rows,
            include_str!("../../../tests/fixtures/wire/gemini-parts.jsonl"),
            "tests/fixtures/wire/gemini-parts.jsonl",
        )
    }
}
