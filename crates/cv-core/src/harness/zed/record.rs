//! The one decoder of a Zed thread document, over a view of the (decompressed) `data` cell.
//! Messages are sniffed per message, not per thread, so a hand-migrated or mixed blob still
//! parses. The span demand decides what is resolved, never which codec runs.
use super::*;
use crate::harness::store::document::{text_is_blank, Fields};
use crate::json::{JsonSource, JsonView};
use crate::lazy::{Resolver, Text};

/// Thread-level keys the session reads; the rest of the document is never decoded.
const METADATA: &[&str] = &[
    "title",
    "summary",
    "updated_at",
    "initial_project_snapshot",
    "model",
    "version",
    "profile",
    "completion_mode",
    "detailed_summary_state",
    "detailed_summary",
    "cumulative_token_usage",
    "subagent_context",
    "imported",
];

#[cfg(test)]
thread_local! { pub(super) static DECODED_MESSAGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// The thread's metadata (owned; small) and its `messages` member, as a view.
pub(super) fn header(view: JsonView<'_>) -> Result<(Value, Option<JsonView<'_>>)> {
    let mut keys = METADATA.to_vec();
    keys.push("messages");
    let Some(fields) = Fields::parse(view, &keys)? else {
        return Ok((Value::Object(Default::default()), None));
    };
    Ok((Value::Object(fields.project(METADATA)?), fields.get("messages")))
}

/// Convert thread messages to IR, stopping on [`Flow::Stop`]. Spans retain long text and
/// tool arguments in the cell; complete reads supply counts independently of discovery.
pub(super) fn messages(
    view: JsonView<'_>,
    source: &JsonSource,
    spans: bool,
    f: &mut dyn FnMut(Message) -> Flow,
) -> Result<()> {
    let Some(messages) = view.array()? else {
        return Ok(());
    };
    // Tool-call ids → names, so legacy tool_results (which carry no name) get labeled.
    let mut names = HashMap::new();
    for message in messages {
        #[cfg(test)]
        DECODED_MESSAGES.with(|n| n.set(n.get() + 1));
        let Some(fields) = Fields::parse(
            message?,
            &[
                "role",
                "id",
                "segments",
                "text",
                "tool_uses",
                "context",
                "is_hidden",
                "creases",
                "tool_results",
                "User",
                "Agent",
                "Compaction",
            ],
        )?
        else {
            continue;
        };
        if legacy(&fields, source, spans, &mut names, f)? == Flow::Stop
            || modern(&fields, source, spans, f)? == Flow::Stop
        {
            break;
        }
    }
    Ok(())
}

fn text(fields: &Fields<'_>, key: &str, spans: bool) -> Result<Option<Text>> {
    fields
        .get(key)
        .map(|v| v.lazy_text(spans))
        .transpose()
        .map(Option::flatten)
}
/// One emptiness rule per type: `Text`'s inherent `is_empty` (span-safe `len`), `String`'s own.
trait NonEmpty {
    fn is_nonempty(&self) -> bool;
}
impl NonEmpty for Text {
    fn is_nonempty(&self) -> bool {
        !self.is_empty()
    }
}
impl NonEmpty for String {
    fn is_nonempty(&self) -> bool {
        !self.is_empty()
    }
}
fn nonempty<T: NonEmpty>(text: Option<T>) -> Option<T> {
    text.filter(|text| text.is_nonempty())
}
fn nonnull(fields: &Fields<'_>, key: &str) -> Result<Option<Value>> {
    let Some(value) = fields.get(key) else { return Ok(None) };
    if value.is_null()? {
        return Ok(None);
    }
    Ok(Some(value.resolve()?.into_owned()))
}
fn remember(block: &Block, names: &mut HashMap<String, String>) {
    if let Block::ToolUse { id, name, .. } = block {
        if !id.is_empty() && !name.is_empty() {
            names.insert(id.clone(), name.clone());
        }
    }
}
fn tool_use(view: JsonView<'_>, spans: bool) -> Result<Block> {
    let fields = Fields::parse(view, &["id", "name", "input"])?;
    let string = |key| -> Result<String> {
        Ok(fields
            .as_ref()
            .map(|v| v.string(key))
            .transpose()?
            .flatten()
            .unwrap_or_default())
    };
    Ok(Block::ToolUse {
        id: string("id")?,
        name: string("name")?,
        input: fields
            .as_ref()
            .and_then(|v| v.get("input"))
            .map(|v| v.argument(spans))
            .transpose()?
            .unwrap_or_default(),
        namespace: None,
        extensions: Default::default(),
    })
}

/// Legacy (agent1) message: an object with a string `role`. Emits the main turn, then its
/// `tool_results` as a separate [`Role::Tool`] turn (0.2.0 keeps them on the assistant message;
/// 0.1.0 put them on the next user message). Non-legacy shapes are ignored.
fn legacy(
    fields: &Fields<'_>,
    source: &JsonSource,
    spans: bool,
    names: &mut HashMap<String, String>,
    f: &mut dyn FnMut(Message) -> Flow,
) -> Result<Flow> {
    let Some(role) = fields.string("role")? else {
        return Ok(Flow::Continue);
    };
    let role = match role.as_str() {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    };
    let mut msg = Message::new(role);
    // A legacy `system` role carries the system prompt (rare; Zed's is normally app-level).
    if role == Role::System {
        msg.kind = MessageKind::SystemPrompt;
    }
    msg.id = fields
        .get("id")
        .map(|v| v.as_number())
        .transpose()?
        .flatten()
        .and_then(|v| v.as_i64())
        .map(|id| id.to_string());
    // segments[] (0.1.0+) or a plain `text` field (the oldest, versionless shape).
    if let Some(segments) = fields.get("segments").map(|v| v.array()).transpose()?.flatten() {
        for segment in segments {
            let block = anthropic::block_from_view_with(
                segment?,
                &anthropic::ZED,
                spans,
                &mut |v| v.lazy_text(spans),
                u64::MAX,
            )?;
            msg.content.extend(block);
        }
    } else if let Some(text) = nonempty(text(fields, "text", spans)?) {
        msg.content.push(Block::from_text(text));
    }
    if let Some(uses) = fields.get("tool_uses").map(|v| v.array()).transpose()?.flatten() {
        for value in uses {
            let block = tool_use(value?, spans)?;
            remember(&block, names);
            msg.content.push(block);
        }
    }
    // `context` is the rendered attached-files preamble. When it's the *only* content (an
    // attachment-only user turn) it is the text (mirroring Zed's own 0.2.0→0.3.0 upgrade);
    // otherwise the turn stays a prompt and the preamble is noted in `extra["zed"]`.
    if let Some(context) = nonempty(text(fields, "context", spans)?) {
        if msg.content.is_empty() {
            msg.content.push(Block::from_text(context));
        } else {
            msg.harness_extra_mut(Harness::Zed).insert(
                "context".into(),
                Value::String(context.try_resolve(&Resolver::new(None))?.into_owned()),
            );
        }
    }
    if fields.get("is_hidden").map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
        msg.harness_extra_mut(Harness::Zed)
            .insert("is_hidden".into(), Value::Bool(true));
        // Hidden user turns are Zed's auto-injected "Continue where you left off" continuations.
        if role == Role::User {
            msg.kind = MessageKind::InjectedContext;
            msg.origin = Origin::Harness;
        }
    }
    if let Some(value) = fields.get("creases") {
        if let Some(mut array) = value.array()? {
            if array.next().transpose()?.is_some() {
                msg.harness_extra_mut(Harness::Zed)
                    .insert("creases".into(), value.resolve()?.into_owned());
            }
        }
    }
    if !msg.content.is_empty() && f(msg) == Flow::Stop {
        return Ok(Flow::Stop);
    }
    let mut blocks = Vec::new();
    if let Some(results) = fields.get("tool_results").map(|v| v.array()).transpose()?.flatten() {
        for result in results {
            if let Some(block) = tool_result(result?, source, spans, names)? {
                blocks.push(block);
            }
        }
    }
    Ok(emit_tools(blocks, f))
}

fn emit_tools(blocks: Vec<Block>, f: &mut dyn FnMut(Message) -> Flow) -> Flow {
    if blocks.is_empty() {
        return Flow::Continue;
    }
    let mut msg = Message::new(Role::Tool);
    msg.content = blocks;
    f(msg)
}

/// One legacy/modern tool result. The legacy shape has no `tool_name`; it is recovered from the
/// paired tool use. `output` (the tool's structured return) rides in `details`.
fn tool_result(
    view: JsonView<'_>,
    source: &JsonSource,
    spans: bool,
    names: &HashMap<String, String>,
) -> Result<Option<Block>> {
    let Some(fields) = Fields::parse(view, &["tool_use_id", "tool_name", "content", "is_error", "output"])? else {
        return Ok(None);
    };
    let Some(id) = fields.string("tool_use_id")? else {
        return Ok(None);
    };
    Ok(Some(Block::ToolResult {
        tool_name: fields.string("tool_name")?.or_else(|| names.get(&id).cloned()),
        tool_use_id: id,
        content: result_content(fields.get("content"), source, spans)?,
        is_error: fields
            .get("is_error")
            .map(|v| v.as_bool())
            .transpose()?
            .flatten()
            .unwrap_or(false),
        details: nonnull(&fields, "output")?,
        status: None,
        extensions: Default::default(),
    }))
}

/// `LanguageModelToolResultContent`: a plain string, `{"Text": "…"}` (case-tolerant), or
/// `{"Image": {…}}` (flattened to a marker; the IR doesn't inline image bytes). Anything else is
/// its compact JSON, generated into the cell's arena when spans are demanded.
fn result_content(view: Option<JsonView<'_>>, source: &JsonSource, spans: bool) -> Result<Text> {
    let Some(view) = view else { return Ok(Text::default()) };
    if let Some(text) = view.lazy_text(spans)? {
        return Ok(text);
    }
    if let Some(fields) = Fields::parse(view, &["Text", "text", "Image", "image"])? {
        // Presence wins before type checking, including a nonstring uppercase field.
        if let Some(text) = fields
            .get("Text")
            .or_else(|| fields.get("text"))
            .map(|v| v.lazy_text(spans))
            .transpose()?
            .flatten()
        {
            return Ok(text);
        }
        if fields.get("Image").or_else(|| fields.get("image")).is_some() {
            return Ok("[image]".into());
        }
    }
    if view.is_null()? {
        return Ok(Text::default());
    }
    if !spans {
        return Ok(Text::Inline(view.resolve()?.to_string()));
    }
    crate::harness::store::text::append(source, &source.decoded_backing()?, |writer| writer.push_json(view))
}

/// Modern (agent2 / 0.3.0) message: the externally tagged `Message` enum. `"Resume"` markers
/// and unknown variants carry no transcript content; `Compaction` summaries become a
/// `CompactionSummary` turn. Non-modern shapes are ignored.
fn modern(fields: &Fields<'_>, source: &JsonSource, spans: bool, f: &mut dyn FnMut(Message) -> Flow) -> Result<Flow> {
    if let Some(user) = fields.get("User") {
        let mut msg = Message::new(Role::User);
        if let Some(user) = Fields::parse(user, &["id", "content"])? {
            msg.id = user.string("id")?;
            if let Some(content) = user.get("content").map(|v| v.array()).transpose()?.flatten() {
                for item in content {
                    msg.content.extend(user_block(item?, spans)?);
                }
            }
        }
        return Ok(if msg.content.is_empty() { Flow::Continue } else { f(msg) });
    }
    if let Some(agent) = fields.get("Agent") {
        let mut msg = Message::new(Role::Assistant);
        let mut names = HashMap::new();
        let Some(agent) = Fields::parse(agent, &["content", "reasoning_details", "tool_results"])? else {
            return Ok(Flow::Continue);
        };
        if let Some(content) = agent.get("content").map(|v| v.array()).transpose()?.flatten() {
            for item in content {
                if let Some(block) = agent_block(item?, spans)? {
                    remember(&block, &mut names);
                    msg.content.push(block);
                }
            }
        }
        // Reasoning details alone make an emitted turn.
        if let Some(value) = nonnull(&agent, "reasoning_details")? {
            msg.harness_extra_mut(Harness::Zed)
                .insert("reasoning_details".into(), value);
        }
        if (!msg.content.is_empty() || !msg.extra.is_empty()) && f(msg) == Flow::Stop {
            return Ok(Flow::Stop);
        }
        // tool_results is an id-keyed map ({"<tool_use_id>": {…}}), ordered by insertion.
        let mut blocks = Vec::new();
        if let Some(results) = agent.get("tool_results") {
            results.for_each_entry(|_, result| {
                blocks.extend(tool_result(result, source, spans, &names)?);
                Ok(())
            })?;
        }
        return Ok(emit_tools(blocks, f));
    }
    if let Some(compaction) = fields.get("Compaction") {
        // CompactionInfo::Summary(text) — the context-compaction summary that replaced history.
        if let Some(compaction) = Fields::parse(compaction, &["Summary"])? {
            if let Some(text) = text(&compaction, "Summary", spans)? {
                if !text_is_blank(&text, &Resolver::new(None))? {
                    let mut msg = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
                    msg.content.push(Block::from_text(text));
                    return Ok(f(msg));
                }
            }
        }
    }
    Ok(Flow::Continue)
}

fn user_block(view: JsonView<'_>, spans: bool) -> Result<Option<Block>> {
    let Some(item) = Fields::parse(view, &["Text", "Mention", "Image"])? else {
        return Ok(None);
    };
    if let Some(text) = text(&item, "Text", spans)? {
        return Ok(nonempty(Some(text)).map(Block::from_text));
    }
    if let Some(mention) = item.get("Mention") {
        // The mention's resolved `content` (an attached file's whole body) is dropped — the URI
        // is the durable reference.
        let Some(mention) = Fields::parse(mention, &["uri"])? else {
            return Ok(None);
        };
        let Some(uri) = mention.get("uri") else { return Ok(None) };
        let value = uri.resolve()?;
        let source = Some(value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()));
        return Ok(Some(Block::File {
            mime: None,
            path: None,
            source,
            extensions: Default::default(),
        }));
    }
    if let Some(image) = item.get("Image") {
        // LanguageModelImage: {source: <base64>, size}.
        let fields = Fields::parse(image, &["source"])?;
        let data_ref = match fields.as_ref() {
            // A bounded head: a retained source payload is never fully decoded for a reference.
            Some(fields) => text(fields, "source", spans)?
                .map(|text| crate::projection::Head::of_text(&text, &Resolver::new(None), 120))
                .transpose()?,
            None => None,
        };
        return Ok(Some(Block::Image {
            media_type: None,
            data_ref,
            extensions: Default::default(),
        }));
    }
    Ok(None)
}

fn agent_block(view: JsonView<'_>, spans: bool) -> Result<Option<Block>> {
    let Some(item) = Fields::parse(view, &["Text", "Thinking", "RedactedThinking", "ToolUse"])? else {
        return Ok(None);
    };
    if let Some(text) = text(&item, "Text", spans)? {
        return Ok(nonempty(Some(text)).map(Block::from_text));
    }
    if let Some(thinking) = item.get("Thinking") {
        let fields = Fields::parse(thinking, &["text", "signature"])?;
        return Ok(Some(Block::Thinking {
            text: fields
                .as_ref()
                .map(|v| text(v, "text", spans))
                .transpose()?
                .flatten()
                .unwrap_or_default(),
            signature: nonempty(fields.as_ref().map(|v| v.string("signature")).transpose()?.flatten()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        }));
    }
    if let Some(data) = item.string("RedactedThinking")? {
        return Ok(Some(Block::Thinking {
            text: Text::default(),
            signature: None,
            encrypted: Some(data),
            redacted: true,
            extensions: Default::default(),
        }));
    }
    item.get("ToolUse").map(|v| tool_use(v, spans)).transpose()
}
