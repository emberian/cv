//! The OpenCode 1.x record codec: one validated message record view (a `storage/message` file or a
//! `message.data` cell) and its part views in, 0–2 IR messages out. Every read (the JSON tree, the
//! SQLite store, every demand) hands these functions [`JsonView`]s; the demand (`spans`) decides
//! whether long text, tool arguments and rendered results stay in the source, never which code
//! runs. Values the IR keeps owned (bags, attachments, details) are resolved from the view.
use super::*;
use crate::harness::store::bytes::ByteSource;
use crate::json::{JsonSource, JsonView};
use crate::lazy::Text;
use std::sync::Arc;

/// The demand: everything inline, or (`spans`) long text and arguments held in the source and
/// text generated from it (joined results, rendered JSON) appended to the arena it guards.
#[derive(Clone, Copy)]
pub(super) enum Ctx<'a> {
    Owned,
    Spans(&'a JsonSource, &'a Arc<ByteSource>),
}

impl<'a> Ctx<'a> {
    pub(super) fn new(spans: bool, source: &'a JsonSource, arena: &'a Arc<ByteSource>) -> Self {
        if spans {
            Ctx::Spans(source, arena)
        } else {
            Ctx::Owned
        }
    }
    pub(super) fn spans(self) -> bool {
        matches!(self, Ctx::Spans(..))
    }
    /// A string by the span rule; None for a nonstring.
    pub(super) fn text(&self, view: JsonView<'_>) -> Result<Option<Text>> {
        view.lazy_source_text(self.spans())
    }
    /// Text built from the source: held in the arena by the span rule, else inline.
    pub(super) fn generated(
        &self,
        produce: impl FnOnce(&mut crate::harness::store::text::Builder<'_>) -> Result<()>,
    ) -> Result<Text> {
        if let Ctx::Spans(source, arena) = self {
            return crate::harness::store::text::append(source, arena, produce);
        }
        let mut quoted = Vec::new();
        crate::harness::store::text::generate_string(&mut quoted, produce)?;
        Ok(Text::Inline(serde_json::from_slice(&quoted)?))
    }
    /// A string as itself, anything else as its compact JSON.
    fn coerced(&self, view: JsonView<'_>) -> Result<Text> {
        match self.text(view)? {
            Some(text) => Ok(text),
            None => self.generated(|writer| writer.push_json(view)),
        }
    }
}

pub(super) fn string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    match view {
        Some(view) if view.is_string()? => Ok(view.resolve()?.as_str().map(str::to_string)),
        _ => Ok(None),
    }
}
/// `view[key]` of an object, None for a missing key or a nonobject.
pub(super) fn get<'a>(view: Option<JsonView<'a>>, key: &str) -> Result<Option<JsonView<'a>>> {
    view.map(|v| v.get(key)).transpose().map(Option::flatten)
}
pub(super) fn owned(view: Option<JsonView<'_>>) -> Result<Option<Value>> {
    view.map(|v| v.resolve().map(|v| v.into_owned())).transpose()
}
/// A non-empty string, judged without decoding a long one.
pub(super) fn nonempty(view: JsonView<'_>) -> Result<bool> {
    if !view.is_string()? {
        return Ok(false);
    }
    match view.source_range()? {
        Some((_, len)) => Ok(len > 2),
        None => Ok(view.resolve()?.as_str().is_some_and(|text| !text.is_empty())),
    }
}

/// One 1.x message record being assembled from its header and parts.
pub(super) struct Record {
    role: Role,
    m: Message,
    bag: Map<String, Value>,
    summary: Option<Text>,
    compaction_summary: bool,
    error: bool,
    compaction: bool,
    spawn: bool,
    results: Vec<Block>,
}

/// The ids a store row hydrates into a part (`{...data, id, messageID, sessionID}`, as opencode's
/// `message-v2.ts` does); a JSON-tree part carries its own.
pub(super) type PartIds<'a> = Option<[&'a str; 3]>;

impl Record {
    /// The record's header. `id` is the row's id for a store record (it wins over the payload's).
    /// None for a nonobject.
    pub(super) fn header(view: JsonView<'_>, id: Option<&str>, ctx: Ctx<'_>) -> Result<Option<Self>> {
        let keys = [
            "id",
            "role",
            "parentID",
            "time",
            "modelID",
            "model",
            "tokens",
            "cost",
            "agent",
            "mode",
            "providerID",
            "finish",
            "error",
            "variant",
            "structured",
            "format",
            "system",
            "tools",
            "path",
            "summary",
        ];
        let Some(fields) = view.select_fields(&keys)? else {
            return Ok(None);
        };
        let [own_id, role, parent, time, model_id, model, tokens, cost, rest @ .., path, summary] = fields.as_slice()
        else {
            unreachable!("one field per key")
        };
        let role = match string(*role)?.as_deref() {
            Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ => Role::User,
        };
        let mut m = match role {
            Role::System => Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness),
            role => Message::new(role),
        };
        let id = match id {
            Some(id) => Some(id.to_string()),
            None => string(*own_id)?,
        };
        m.id = id.filter(|id| !id.is_empty());
        m.parent_id = string(*parent)?;
        m.timestamp = owned(get(*time, "created")?)?
            .as_ref()
            .and_then(super::super::ts_from_value);
        // Newer records carry `modelID`; user records nest it under `model.modelID`.
        m.model = match string(*model_id)? {
            Some(model) => Some(model),
            None => string(get(*model, "modelID")?)?,
        };
        m.usage = usage(*tokens, *cost)?;
        // Harness facts, nested under `extra["opencode"]` at the end (module doc).
        let mut bag = Map::new();
        for (key, value) in keys[8..18].iter().zip(rest) {
            if let Some(value) = value.filter(|v| !v.is_null().unwrap_or(true)) {
                bag.insert(key.to_string(), value.resolve()?.into_owned());
            }
        }
        // `path: {cwd, root}` — per-turn working dir.
        if let Some(cwd) = string(get(*path, "cwd")?)? {
            bag.insert("cwd".into(), json!(cwd));
        }
        // The assistant `error` union is tagged by `name` (ProviderAuthError, UnknownError,
        // MessageOutputLengthError, MessageAbortedError, StructuredOutputError,
        // ContextOverflowError, ContentFilterError, APIError) with the detail under `data`. Lift
        // the tag so consumers can filter on it without unpicking the object.
        let error = rest[4];
        if let Some(name) = string(get(error, "name")?)? {
            bag.insert("error_name".into(), json!(name));
        }
        // The older inline summary object: `body` (else `title`) is the text of a record with no
        // parts, its `diffs[]` are kept. A boolean `summary: true` is the newer compaction marker.
        let mut text = None;
        if let Some(summary) = summary.filter(|s| s.is_object().unwrap_or(false)) {
            if let Some(diffs) = summary.get("diffs")? {
                if diffs.array()?.is_some_and(|mut items| items.next().is_some()) {
                    bag.insert("summary_diffs".into(), diffs.resolve()?.into_owned());
                }
            }
            let body = match summary.get("body")? {
                Some(body) => Some(body),
                None => summary.get("title")?,
            };
            if let Some(body) = body.filter(|body| nonempty(*body).unwrap_or(false)) {
                text = ctx.text(body)?;
            }
        }
        Ok(Some(Record {
            role,
            m,
            bag,
            summary: text,
            compaction_summary: summary.map(JsonView::as_bool).transpose()?.flatten() == Some(true),
            error: error.map(JsonView::is_null).transpose()? == Some(false),
            compaction: false,
            spawn: false,
            results: Vec::new(),
        }))
    }

    pub(super) fn id(&self) -> Option<&str> {
        self.m.id.as_deref()
    }

    /// Fold one part into the record. A nonobject part is skipped.
    pub(super) fn part(&mut self, view: JsonView<'_>, ids: PartIds<'_>, ctx: Ctx<'_>) -> Result<()> {
        if !view.is_object()? {
            return Ok(());
        }
        // The part as the bag keeps it, hydrated like opencode's own reader.
        let hydrated = || -> Result<Value> {
            let mut part = view.resolve()?.into_owned();
            if let (Some([id, message, session]), Some(object)) = (ids, part.as_object_mut()) {
                object.insert("id".into(), json!(id));
                object.insert("messageID".into(), json!(message));
                object.insert("sessionID".into(), json!(session));
            }
            Ok(part)
        };
        let m = &mut self.m;
        let bag = &mut self.bag;
        match string(view.get("type")?)?.as_deref().unwrap_or("") {
            "text" => {
                if let Some(text) = view.get("text")?.filter(|t| nonempty(*t).unwrap_or(false)) {
                    if let Some(text) = ctx.text(text)? {
                        m.content.push(Block::from_text(text));
                    }
                }
            }
            "reasoning" => {
                let Some(text) = view.get("text")?.filter(|t| t.is_string().unwrap_or(false)) else {
                    return Ok(());
                };
                // Providers stash an opaque signature under `metadata.<provider>.signature`
                // (e.g. anthropic). Preserve it so a thinking block can round-trip.
                let metadata = owned(view.get("metadata")?)?;
                let signature = metadata
                    .as_ref()
                    .and_then(|m| m.pointer("/anthropic/signature"))
                    .and_then(Value::as_str)
                    .or_else(|| find_signature(metadata.as_ref()))
                    .map(str::to_string);
                // A signature-only part (empty text, opaque blob) is still the whole of a redacted
                // thinking turn: dropping it deleted the turn's only content, and with it the
                // message — which cost a ported session 134 assistant turns and their usage,
                // timestamps and ids.
                if nonempty(text)? || signature.is_some() {
                    m.content.push(Block::Thinking {
                        text: ctx.text(text)?.unwrap_or_default(),
                        signature,
                        encrypted: None,
                        redacted: false,
                        extensions: Default::default(),
                    });
                }
            }
            "tool" => {
                let (use_block, results) = tool_blocks(view, ids, ctx)?;
                m.content.push(use_block);
                self.results.extend(results);
            }
            "file" => m.content.push(file_block(view.resolve()?.as_ref())),
            "agent" => {
                // An `@agent` mention / sub-agent reference embedded in a user prompt — opencode
                // routes the prompt to that agent in a child session. No IR block fits; surface
                // its name as text and keep the structured form in the bag.
                if let Some(name) = string(view.get("name")?)? {
                    m.content.push(Block::from_text(format!("@{name}").into()));
                }
                push_extra_array(bag, "agent_refs", hydrated()?);
                self.spawn = true;
            }
            "subtask" => {
                // A spawned sub-session (Task tool style). Render its prompt + which agent ran
                // it; keep the full record (agent/model/command/description) in the bag.
                let agent = string(view.get("agent")?)?;
                let prompt = string(view.get("prompt")?)?;
                m.content.push(Block::from_text(
                    format!(
                        "[subtask → {}] {}",
                        agent.as_deref().unwrap_or("agent"),
                        prompt.as_deref().unwrap_or("")
                    )
                    .into(),
                ));
                push_extra_array(bag, "subtasks", hydrated()?);
                self.spawn = true;
            }
            "patch" => push_extra_array(bag, "patches", hydrated()?),
            "snapshot" => push_extra_array(bag, "snapshots", hydrated()?),
            "step-start" | "step-finish" => push_extra_array(bag, "steps", hydrated()?),
            "retry" => push_extra_array(bag, "retries", hydrated()?),
            "compaction" => {
                push_extra_array(bag, "compactions", hydrated()?);
                self.compaction = true;
            }
            // Missing or empty types are ignored; named unknown types are preserved.
            "" => {}
            _ => push_extra_array(bag, "unknown_parts", hydrated()?),
        }
        Ok(())
    }

    /// The record's messages: the record (unless it carries nothing) and its trailing tool
    /// results.
    pub(super) fn finish(self) -> Vec<Message> {
        let Record {
            role,
            mut m,
            bag,
            summary,
            compaction_summary,
            error,
            compaction,
            spawn,
            results,
        } = self;
        // Older inline-summary generation stored no parts — fall back to the AI summary so the
        // turn isn't blank.
        if m.content.is_empty() {
            if let Some(text) = summary {
                m.content.push(Block::from_text(text));
            }
        }
        // What the record IS, most specific first (module doc "Kinds").
        if compaction {
            m.kind = MessageKind::CompactionBoundary;
            m.origin = Origin::Harness;
        } else if compaction_summary {
            m.kind = MessageKind::CompactionSummary;
            m.origin = Origin::Harness;
        } else if error {
            m.kind = MessageKind::Error;
            m.origin = Origin::Harness;
        } else if spawn {
            m.kind = MessageKind::SubagentSpawn;
        }
        if !bag.is_empty() {
            m.harness_extra_mut(Harness::OpenCode).extend(bag);
        }
        let mut out = Vec::new();
        if !m.content.is_empty() || !m.extra.is_empty() || m.kind != MessageKind::for_role(role) {
            out.push(m);
        }
        if !results.is_empty() {
            let mut tm = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            tm.content = results;
            out.push(tm);
        }
        out
    }
}

/// The `ToolUse` block for a `tool` part, plus the blocks that belong in the Tool message: the
/// `ToolResult` (if the state carries one) followed by any `state.attachments[]` (files/images a
/// completed tool handed back — a `read` of an image, a screenshot — same shape as a `file` part).
/// The state's `title`/`metadata`/`time` ride in the result's `details`. Tolerant of the
/// `completed`/`error`/`running`/`pending` state shapes.
fn tool_blocks(part: JsonView<'_>, ids: PartIds<'_>, ctx: Ctx<'_>) -> Result<(Block, Vec<Block>)> {
    let call_id = match part.get("callID")? {
        Some(call) => string(Some(call))?,
        None => match ids {
            Some([id, ..]) => Some(id.to_string()),
            None => string(part.get("id")?)?,
        },
    }
    .unwrap_or_default();
    let name = string(part.get("tool")?)?.unwrap_or_default();
    let state = part.get("state")?;
    let status = string(get(state, "status")?)?.unwrap_or_default();
    let tool_name = (!name.is_empty()).then(|| name.clone());
    let status_field = (!status.is_empty()).then(|| status.clone());
    let mut details = Map::new();
    for key in ["title", "metadata", "time"] {
        if let Some(v) = owned(get(state, key)?)? {
            let blank = v.is_null() || v.as_object().is_some_and(|o| o.is_empty()) || v.as_str() == Some("");
            if !blank {
                details.insert(key.to_string(), v);
            }
        }
    }
    let details = (!details.is_empty()).then_some(Value::Object(details));
    let use_block = Block::ToolUse {
        id: call_id.clone(),
        name,
        input: match get(state, "input")? {
            Some(input) => input.argument(ctx.spans())?,
            None => Value::Null.into(),
        },
        namespace: None,
        extensions: Default::default(),
    };
    // A completed tool carries `output`; an error tool carries `error`.
    let output = get(state, "output")?;
    let (content, is_error) = match status.as_str() {
        "error" => (
            Some(match get(state, "error")?.or(output) {
                Some(content) => ctx.coerced(content)?,
                None => Text::default(),
            }),
            true,
        ),
        // Unknown status but an output is present — still surface it.
        _ => (output.map(|out| ctx.coerced(out)).transpose()?, false),
    };
    let mut out: Vec<Block> = content
        .map(|content| Block::ToolResult {
            tool_use_id: call_id,
            content,
            is_error,
            tool_name,
            status: status_field,
            details,
            extensions: Default::default(),
        })
        .into_iter()
        .collect();
    if let Some(attachments) = get(state, "attachments")?.map(JsonView::array).transpose()?.flatten() {
        for attachment in attachments {
            out.push(file_block(attachment?.resolve()?.as_ref()));
        }
    }
    Ok((use_block, out))
}

/// Map a `file` part to a block. Genuine images become `Block::Image`; everything else
/// (text files, directories via `@path`, symbol/resource refs) becomes a first-class
/// `Block::File` reference.
fn file_block(part: &Value) -> Block {
    let mime = part.get("mime").and_then(Value::as_str).unwrap_or("");
    if mime.starts_with("image/") {
        Block::Image {
            media_type: (!mime.is_empty()).then(|| mime.to_string()),
            data_ref: part.get("url").and_then(Value::as_str).map(str::to_string),
            extensions: Default::default(),
        }
    } else {
        // `source.path` is the workspace-relative path when present; `filename`/`url` are fallbacks.
        let path = part
            .pointer("/source/path")
            .or_else(|| part.get("filename"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Block::File {
            mime: (!mime.is_empty()).then(|| mime.to_string()),
            path,
            source: part.get("url").and_then(Value::as_str).map(str::to_string),
            extensions: Default::default(),
        }
    }
}

pub(super) fn push_extra_array(extra: &mut Map<String, Value>, key: &str, v: Value) {
    match extra.get_mut(key) {
        Some(Value::Array(a)) => a.push(v),
        _ => {
            extra.insert(key.to_string(), Value::Array(vec![v]));
        }
    }
}

/// Look for a `signature` string anywhere one provider-level deep inside `metadata`
/// (`metadata.<provider>.signature`), without hard-coding the provider name. This is a heuristic:
/// an unrelated nested signature can be mistaken for a thinking signature.
pub(super) fn find_signature(metadata: Option<&Value>) -> Option<&str> {
    let obj = metadata?.as_object()?;
    for v in obj.values() {
        if let Some(sig) = v.pointer("/signature").and_then(Value::as_str) {
            return Some(sig);
        }
    }
    None
}

/// `tokens{input, output, reasoning, cache{read, write}}` + `cost` (USD) of an assistant record.
/// `reasoning` is always written by opencode; it is reported only when non-zero.
pub(super) fn usage(tokens: Option<JsonView<'_>>, cost: Option<JsonView<'_>>) -> Result<Option<Usage>> {
    let tokens = owned(tokens)?.filter(Value::is_object);
    let cost = owned(cost)?.as_ref().and_then(Value::as_f64);
    if tokens.is_none() && cost.is_none() {
        return Ok(None);
    }
    let get = |k: &str| tokens.as_ref().and_then(|t| t.pointer(k)).and_then(Value::as_u64);
    Ok(Some(Usage {
        input_tokens: get("/input"),
        output_tokens: get("/output"),
        cache_read_tokens: get("/cache/read"),
        cache_creation_tokens: get("/cache/write"),
        reasoning_tokens: get("/reasoning").filter(|&r| r > 0),
        cost_usd: cost,
        ..Default::default()
    }))
}
