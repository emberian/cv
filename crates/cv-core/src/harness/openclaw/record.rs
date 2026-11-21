//! The OpenClaw record codec. Both byte sources hold the same records: a JSONL transcript line
//! and a `transcript_events` cell (`event_json`, or the zstd frame in `event_zstd`) are one entry.
//! Every read (the stream, branch navigation, every demand) hands [`record`] or [`navigation`] a
//! validated [`JsonView`] of one; the demand (`spans`, `complete`) decides which fields are
//! resolved and whether long text stays in the source, never which code runs.
use super::*;
use crate::harness::store::{document::Fields, text};
use crate::json::JsonView;
use crate::lazy::Text;
use crate::stream::Tier;

/// Array text joined from several strings stays in the source above this many bytes when spans
/// are demanded, as Claude's tool-result arrays do.
const JOINED_OVER: u64 = 64 * 1024;

/// A `session` header's facts.
pub(super) struct Header {
    pub version: Option<i64>,
    pub cwd: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub forked_from: Option<String>,
    /// The whole header, under `complete`.
    pub record: Option<Value>,
}

/// What one entry contributes to its session.
#[derive(Default)]
pub(super) struct Record {
    pub header: Option<Header>,
    /// A `session_info` entry's non-blank name.
    pub session_name: Option<String>,
    /// A `label` entry's target and label.
    pub label: Option<(String, String)>,
    pub messages: Vec<Message>,
    /// The entry itself, under `complete`, for an object record.
    pub native: Option<crate::Json>,
}

const TOP: &[&str] = &[
    "type",
    "id",
    "parentId",
    "timestamp",
    "message",
    "version",
    "cwd",
    "parentSession",
    "name",
    "targetId",
    "label",
    "summary",
    "content",
    "reason",
    "modelId",
    "provider",
    "thinkingLevel",
];

const BODY: &[&str] = &[
    "role",
    "model",
    "provenance",
    "content",
    "toolCallId",
    "toolName",
    "isError",
    "details",
    "usage",
    "api",
    "provider",
    "responseModel",
    "responseId",
    "stopReason",
    "diagnostics",
    "errorMessage",
    "errorCode",
    "errorType",
    "command",
    "output",
    "exitCode",
    "cancelled",
    "truncated",
    "fullOutputPath",
    "excludeFromContext",
    "summary",
    "fromId",
    "tokensBefore",
    "tokensAfter",
    "firstKeptEntryId",
    "display",
    "customType",
];

fn string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    match view {
        Some(v) if v.is_string()? => Ok(v.resolve()?.as_str().map(str::to_string)),
        _ => Ok(None),
    }
}
/// A present, non-null value, owned.
fn present(view: Option<JsonView<'_>>) -> Result<Option<Value>> {
    match view {
        Some(v) if !v.is_null()? => Ok(Some(v.resolve()?.into_owned())),
        _ => Ok(None),
    }
}
/// A string with a non-whitespace character (`!str::trim().is_empty()`), decoded in place.
fn non_blank(view: Option<JsonView<'_>>) -> Result<bool> {
    let Some(view) = view else { return Ok(false) };
    let mut found = false;
    Ok(view.for_each_char(|c| {
        found = !c.is_whitespace();
        !found
    })? && found)
}
/// Transcript text by the codec's span rule, held as checked source text: an OpenClaw turn also
/// depends on `sessions.json` and sibling transcripts (prompt origin, lineage), and only a source
/// descriptor carries those dependencies to the reader that resolves it. None for a nonstring.
fn text_of(view: JsonView<'_>, spans: bool) -> Result<Option<Text>> {
    view.lazy_text(spans)
}
/// [`text_of`] a string field; "" for a missing or nonstring one.
fn lazy(view: Option<JsonView<'_>>, spans: bool) -> Result<Text> {
    Ok(view
        .map(|v| text_of(v, spans))
        .transpose()?
        .flatten()
        .unwrap_or_default())
}

enum Piece<'a> {
    Literal(&'a str),
    String(JsonView<'a>),
}
/// Text joined from literals and string views. Inline unless spans are demanded and the
/// containing value's source is over [`JOINED_OVER`]: then checked joined text in its backing.
fn joined(container: JsonView<'_>, spans: bool, pieces: &[Piece<'_>]) -> Result<Text> {
    if spans && container.source_range()?.is_some_and(|(_, len)| len > JOINED_OVER) {
        if let crate::Json::Source(range) = container.to_json()? {
            let source = range.captured_source();
            return text::append(source, &source.decoded_backing()?, |builder| {
                for piece in pieces {
                    match piece {
                        Piece::Literal(text) => builder.push(text)?,
                        Piece::String(view) => anyhow::ensure!(builder.push_string(*view)?, "joined string"),
                    }
                }
                Ok(())
            });
        }
    }
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::String(view) => out.push_str(view.resolve()?.as_str().unwrap_or_default()),
        }
    }
    Ok(out.into())
}

/// `content` as text: a string whole, an array's string `text` fields joined by newlines,
/// anything else "".
fn text_parts(content: Option<JsonView<'_>>, spans: bool) -> Result<Text> {
    use crate::harness::wire;
    let backing = wire::Backing::Over(if spans { JOINED_OVER } else { u64::MAX });
    wire::joined(content, wire::TEXT, backing, &mut |v| text_of(v, spans))
}

/// One `(TextContent|ThinkingContent|ToolCall|ImageContent)` item: the Anthropic shape under
/// OpenClaw's keys; `redacted` flags a server-redacted reasoning block.
fn block(item: JsonView<'_>, spans: bool) -> Result<Option<Block>> {
    block_with(item, spans, &mut |v| text_of(v, spans))
}
/// [`block`] with the caller's text rule (the emitter keeps native text as checked source text).
pub(super) fn block_with(
    item: JsonView<'_>,
    spans: bool,
    text: &mut dyn FnMut(JsonView<'_>) -> Result<Option<Text>>,
) -> Result<Option<Block>> {
    let joined_over = if spans { JOINED_OVER } else { u64::MAX };
    let mut block = anthropic::block_from_view_with(item, &anthropic::OPENCLAW, spans, text, joined_over)?;
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
fn blocks(m: &mut Message, content: Option<JsonView<'_>>, spans: bool) -> Result<()> {
    if let Some(items) = content.map(JsonView::array).transpose()?.flatten() {
        for item in items {
            if let Some(b) = block(item?, spans)? {
                m.content.push(b);
            }
        }
    }
    Ok(())
}

/// The navigation fields of an entry, for branch selection. Only string/null values affect
/// navigation; another value keeps its key with a tiny sentinel instead of being resolved.
pub(super) fn navigation(view: JsonView<'_>) -> Result<Value> {
    const KEYS: &[&str] = &[
        "type",
        "id",
        "parentId",
        "appendMode",
        "targetId",
        "appendParentId",
        "firstKeptEntryId",
    ];
    let Some(fields) = Fields::parse(view, KEYS)? else {
        return Ok(Value::Null);
    };
    let mut descriptor = serde_json::Map::new();
    for key in KEYS {
        if let Some(field) = fields.get(key) {
            let value = if field.is_string()? {
                field.resolve()?.into_owned()
            } else if field.is_null()? {
                Value::Null
            } else {
                Value::Bool(false)
            };
            descriptor.insert((*key).into(), value);
        }
    }
    Ok(Value::Object(descriptor))
}

/// An entry's identity: `id`, `parentId`, and its timestamp (the entry-level ISO `timestamp`, else
/// the inner epoch-ms `message.timestamp`).
struct Common {
    id: Option<String>,
    parent: Option<String>,
    timestamp: Option<DateTime<Utc>>,
}
impl Common {
    fn read(top: Option<&Fields<'_>>) -> Result<Self> {
        let Some(top) = top else {
            return Ok(Common {
                id: None,
                parent: None,
                timestamp: None,
            });
        };
        let mut timestamp = top.string("timestamp")?.as_deref().and_then(parse_ts);
        if timestamp.is_none() {
            if let Some(inner) = top.get("message").map(|m| m.get("timestamp")).transpose()?.flatten() {
                timestamp = ts_from_value(inner.resolve()?.as_ref());
            }
        }
        Ok(Common {
            id: top.string("id")?,
            parent: top.string("parentId")?,
            timestamp,
        })
    }
    fn message(&self, role: Role, kind: MessageKind, origin: Origin) -> Message {
        let mut m = Message::of_kind(role, kind, origin);
        m.id = self.id.clone();
        m.parent_id = self.parent.clone();
        m.timestamp = self.timestamp;
        m
    }
}

/// One entry → its session facts and messages.
pub(super) fn record(view: JsonView<'_>, opts: &Demand) -> Result<Record> {
    // A complete read keeps text in the source too: its reader materializes each turn whole,
    // together with the record it carries, never a second inline copy beside it.
    let spans = opts.spans || opts.tier == Tier::Complete;
    let top = Fields::parse(view, TOP)?;
    let ty = top
        .as_ref()
        .map(|f| f.string("type"))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let mut out = Record::default();
    if let (Some(top), "session") = (&top, ty.as_str()) {
        out.header = Some(Header {
            version: top
                .get("version")
                .map(JsonView::as_number)
                .transpose()?
                .flatten()
                .and_then(|n| n.as_i64()),
            cwd: top.string("cwd")?,
            created_at: top.string("timestamp")?.as_deref().and_then(parse_ts),
            forked_from: top.string("parentSession")?,
            record: (opts.tier == Tier::Complete)
                .then(|| view.resolve())
                .transpose()?
                .map(|v| v.into_owned()),
        });
        return Ok(out);
    }
    let common = Common::read(top.as_ref())?;
    out.messages = match (&top, ty.as_str()) {
        (Some(top), "message") => message(top, &common, spans)?,
        (
            Some(top),
            "compaction" | "reset" | "branch_summary" | "custom_message" | "model_change" | "thinking_level_change",
        ) => control(view, top, &ty, &common, spans)?,
        // Session-level facts with a first-class home; the record itself only rides under
        // `complete`, as the notice the user saw.
        (Some(top), "session_info") => {
            let name = top.get("name");
            out.session_name = if non_blank(name)? { string(name)? } else { None };
            let text = format!("[session named: {}]", out.session_name.as_deref().unwrap_or(""));
            notice(view, &ty, &common, opts, text)?
        }
        (Some(top), "label") => {
            let target = top.string("targetId")?;
            let label = top.string("label")?;
            let text = format!(
                "[label {}: {}]",
                target.as_deref().unwrap_or(""),
                label.as_deref().unwrap_or("")
            );
            out.label = target.zip(label);
            notice(view, &ty, &common, opts, text)?
        }
        // `custom` (extension state, not in context), `leaf` navigation controls and anything
        // newer: bookkeeping, carried only under `complete`.
        _ => carrier(view, &ty, &common, opts)?,
    };
    if opts.tier == Tier::Complete && out.messages.is_empty() {
        out.messages = carrier(view, &ty, &common, opts)?;
    }
    if opts.tier == Tier::Complete && view.is_object()? {
        out.native = Some(view.to_json()?);
    }
    Ok(out)
}

/// Under [`Demand::COMPLETE`], a bookkeeping entry (`custom`, `leaf`, …) rides along verbatim
/// as a `Carrier` turn: the raw record at the top-level `_record`, its type in
/// `extra["openclaw"]["record_type"]`. The lean passes drop it.
fn carrier(view: JsonView<'_>, ty: &str, common: &Common, opts: &Demand) -> Result<Vec<Message>> {
    if opts.tier != Tier::Complete {
        return Ok(Vec::new());
    }
    let mut m = common.message(Role::System, MessageKind::Carrier, Origin::Harness);
    m.harness_extra_mut(Harness::OpenClaw)
        .insert("record_type".into(), Value::from(ty));
    m.extra
        .insert(crate::ir::CARRIER_KEY.into(), view.resolve()?.into_owned());
    Ok(vec![m])
}

/// Under [`Demand::COMPLETE`], a session-fact entry the user saw as a notice (`session_info`,
/// `label`) rides along as a `Notice` turn with `text`, the raw record at `_record` and its type in
/// the bag. Lean passes keep only the fact's first-class home.
fn notice(view: JsonView<'_>, ty: &str, common: &Common, opts: &Demand, text: String) -> Result<Vec<Message>> {
    let mut out = carrier(view, ty, common, opts)?;
    if let Some(m) = out.first_mut() {
        m.kind = MessageKind::Notice;
        m.content.push(Block::Text { text: text.into() });
    }
    Ok(out)
}

/// A non-`message` canonical entry that carries context or a boundary the reader should see, as
/// the kinds vocabulary has it: `compaction` → a `CompactionBoundary` (the entry's own fields:
/// `firstKeptEntryId`, `tokensBefore`, `fromHook`, `details`) followed by the
/// `CompactionSummary` that replaced the compacted span; `reset` → `Branch`; `model_change` /
/// `thinking_level_change` → `ModelChange`; `branch_summary` / `custom_message` (extension text
/// that IS in model context) → `Notice`. All origin Harness. In the harness bag: `record_type`
/// plus every field the entry carries besides the structural ones and the `summary`/`content`
/// that become the turn's text, under its native camelCase key.
fn control(view: JsonView<'_>, top: &Fields<'_>, ty: &str, common: &Common, spans: bool) -> Result<Vec<Message>> {
    let kind = match ty {
        "compaction" => MessageKind::CompactionBoundary,
        "reset" => MessageKind::Branch,
        "model_change" | "thinking_level_change" => MessageKind::ModelChange,
        _ => MessageKind::Notice,
    };
    let mut m = if ty == "compaction" {
        let mut m = Message::synthesized(kind, Origin::Harness);
        m.id = common.id.clone();
        m.parent_id = common.parent.clone();
        m.timestamp = common.timestamp;
        m
    } else {
        common.message(Role::System, kind, Origin::Harness)
    };
    let bag = m.harness_extra_mut(Harness::OpenClaw);
    bag.insert("record_type".into(), Value::from(ty));
    view.for_each_entry(|key, value| {
        if !matches!(key, "type" | "id" | "parentId" | "timestamp" | "summary" | "content") && !value.is_null()? {
            bag.insert(key.into(), value.resolve()?.into_owned());
        }
        Ok(())
    })?;
    let text: Text = match ty {
        "compaction" => Text::default(),
        "branch_summary" => lazy(top.get("summary"), spans)?,
        "reset" => format!(
            "[session reset: {}]",
            top.string("reason")?.as_deref().unwrap_or("reset")
        )
        .into(),
        "model_change" => {
            let model = top.string("modelId")?.unwrap_or_default();
            m.model = (!model.is_empty()).then(|| model.clone());
            match top.string("provider")? {
                Some(p) if !p.is_empty() => format!("[model: {p}/{model}]"),
                _ => format!("[model: {model}]"),
            }
            .into()
        }
        "thinking_level_change" => match top.string("thinkingLevel")? {
            Some(level) => format!("[thinking level: {level}]"),
            None => "[thinking level changed]".to_string(),
        }
        .into(),
        _ => text_parts(top.get("content"), spans)?,
    };
    if !text.is_empty() {
        m.content.push(Block::Text { text });
    }
    let mut out = vec![m];
    let summary = top.get("summary");
    if ty == "compaction" && non_blank(summary)? {
        let mut sm = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
        sm.parent_id = common.id.clone();
        sm.timestamp = common.timestamp;
        sm.content.push(Block::Text {
            text: lazy(summary, spans)?,
        });
        sm.harness_extra_mut(Harness::OpenClaw)
            .insert("record_type".into(), Value::from(ty));
        out.push(sm);
    }
    Ok(out)
}

/// A `message` entry → its turn(s). The `compactionSummary` custom role (the pre-`compaction`-entry
/// form) yields two: the boundary (the entry's id and token counts) and the summary.
fn message(top: &Fields<'_>, common: &Common, spans: bool) -> Result<Vec<Message>> {
    let Some(container) = top.get("message") else {
        return Ok(Vec::new());
    };
    let Some(body) = Fields::parse(container, BODY)? else {
        return Ok(Vec::new());
    };
    let Some(role_str) = body.string("role")? else {
        return Ok(Vec::new());
    };
    let (role, kind, origin) = match role_str.as_str() {
        "user" => (Role::User, MessageKind::Prompt, Origin::Human),
        "assistant" => (Role::Assistant, MessageKind::Reply, Origin::Model),
        "toolResult" => (Role::Tool, MessageKind::ToolResult, Origin::Harness),
        "system" => (Role::System, MessageKind::SystemPrompt, Origin::Harness),
        // Custom (declaration-merged) message types, as System turns: a `!cmd` run and its output
        // are flattened into the model's context; the summaries and extension markers are notices
        // (the compaction summary gets its boundary below).
        "bashExecution" => (Role::System, MessageKind::InjectedContext, Origin::Harness),
        "branchSummary" | "custom" => (Role::System, MessageKind::Notice, Origin::Harness),
        "compactionSummary" => (Role::System, MessageKind::CompactionSummary, Origin::Harness),
        _ => return Ok(Vec::new()),
    };
    // A system prompt is also the owned `Session::system_prompt`: its text is always inline. So
    // this message's span rule is the entry's, narrowed: named apart from the parameter.
    let keep_source = spans && role_str != "system";
    let provenance = match body.get("provenance") {
        Some(p) if p.is_object()? => Some(p.resolve()?.into_owned()),
        _ => None,
    };
    let (kind, origin) = match (role_str.as_str(), &provenance) {
        ("user", Some(p)) => user_provenance(p),
        _ => (kind, origin),
    };
    let mut m = common.message(role, kind, origin);
    m.model = body.string("model")?;
    if let Some(p) = provenance {
        m.harness_extra_mut(Harness::OpenClaw).insert("provenance".into(), p);
    }
    let content = body.get("content");
    match role_str.as_str() {
        "toolResult" => {
            // `toolName` and the arbitrary structured `details` payload (diffs, file lists) live
            // on the block, where every harness puts them.
            m.content.push(Block::ToolResult {
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
            });
            // Image parts in a tool result aren't captured by the text coercion; surface them.
            if let Some(items) = content.map(JsonView::array).transpose()?.flatten() {
                for item in items {
                    let item = item?;
                    if string(item.get("type")?)?.as_deref() == Some("image") {
                        if let Some(b) = block(item, keep_source)? {
                            m.content.push(b);
                        }
                    }
                }
            }
        }
        "assistant" => {
            blocks(&mut m, content, keep_source)?;
            crate::harness::wire::pi_assistant_metadata(&mut m, &body, Harness::OpenClaw)?;
        }
        "user" | "system" => match content {
            Some(c) if c.is_string()? => m.content.push(Block::Text {
                text: lazy(Some(c), keep_source)?,
            }),
            _ => blocks(&mut m, content, keep_source)?,
        },
        "bashExecution" => {
            let command = body.get("command").filter(|v| v.is_string().unwrap_or(false));
            let output = body.get("output").filter(|v| v.is_string().unwrap_or(false));
            let mut pieces = vec![Piece::Literal("$ ")];
            pieces.extend(command.map(Piece::String));
            if let Some(output) = output {
                let mut empty = true;
                output.for_each_char(|_| {
                    empty = false;
                    false
                })?;
                if !empty {
                    pieces.extend([Piece::Literal("\n"), Piece::String(output)]);
                }
            }
            m.content.push(Block::Text {
                text: joined(container, keep_source, &pieces)?,
            });
            custom_role_bag(
                &mut m,
                "bashExecution",
                &body,
                &[
                    "exitCode",
                    "cancelled",
                    "truncated",
                    "fullOutputPath",
                    "excludeFromContext",
                ],
            )?;
        }
        "branchSummary" => {
            m.content.push(Block::Text {
                text: lazy(body.get("summary"), keep_source)?,
            });
            let from_id = body.string("fromId")?;
            let bag = custom_role_bag(&mut m, "branchSummary", &body, &[])?;
            if let Some(from_id) = from_id {
                bag.insert("fromId".into(), Value::from(from_id));
            }
        }
        "compactionSummary" => {
            m.content.push(Block::Text {
                text: lazy(body.get("summary"), keep_source)?,
            });
            custom_role_bag(
                &mut m,
                "compactionSummary",
                &body,
                &["tokensBefore", "tokensAfter", "firstKeptEntryId"],
            )?;
        }
        _ => {
            let text = text_parts(content, keep_source)?;
            if !text.is_empty() {
                m.content.push(Block::Text { text });
            }
            let custom_type = body.string("customType")?;
            let details = present(body.get("details"))?;
            let bag = custom_role_bag(&mut m, "custom", &body, &["display"])?;
            if let Some(ct) = custom_type {
                bag.insert("customType".into(), Value::from(ct));
            }
            if let Some(details) = details {
                bag.insert("details".into(), details);
            }
        }
    }

    // An empty old-form compactionSummary also drops its boundary: it carries neither
    // summary content nor metadata from which to construct a meaningful boundary.
    if m.content.is_empty() && m.extra.is_empty() {
        return Ok(Vec::new());
    }
    if role_str != "compactionSummary" {
        return Ok(vec![m]);
    }
    // The old single-record form: the record IS the compaction. The boundary keeps the entry's id
    // (what a later `firstKeptEntryId` / label points at) and counts; the summary hangs off it.
    // The whole bag goes to the boundary — `message_role` and the token counts it already carries —
    // and the summary gets its own role marker re-inserted below, so the split is deliberate.
    let mut boundary = Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness);
    boundary.id = m.id.take();
    boundary.parent_id = m.parent_id.take();
    boundary.timestamp = m.timestamp;
    boundary.extra.extend(std::mem::take(&mut m.extra));
    m.parent_id = boundary.id.clone();
    m.harness_extra_mut(Harness::OpenClaw)
        .insert("message_role".into(), Value::from(role_str));
    Ok(vec![boundary, m])
}

/// Whether a `message` entry's body is a turn ([`Message::is_turn`]) as [`message`] kinds it: a
/// `user` message that [`user_provenance`] leaves a prompt, or an `assistant` reply that is not a
/// failure. Reads `role`, `provenance`, `errorMessage` and `stopReason`, never content.
pub(super) fn is_turn(body: &Fields<'_>) -> Result<bool> {
    Ok(match body.string("role")?.as_deref() {
        Some("user") => match body.get("provenance") {
            Some(p) if p.is_object()? => user_provenance(&*p.resolve()?).0 == MessageKind::Prompt,
            _ => true,
        },
        Some("assistant") => !crate::harness::wire::assistant_failed(body)?,
        _ => false,
    })
}

/// A user message OpenClaw wrote on someone else's behalf carries `provenance` (`InputProvenance`,
/// `src/sessions/input-provenance.ts`; not yet in 2026.1.29, present by `0e9181234a`): `inter_session` is a
/// message from another session (`sourceSessionKey`), and a sub-agent's announce to its requester is
/// its return; `internal_system` with `sourceTool: "cron"` is a cron run's prompt.
fn user_provenance(p: &Value) -> (MessageKind, Origin) {
    let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
    match field("kind") {
        "inter_session"
            if field("sourceRole") == "subagent" || field("sourceSessionKey").split(':').nth(2) == Some("subagent") =>
        {
            (MessageKind::SubagentReturn, Origin::Subagent)
        }
        "inter_session" => (MessageKind::Prompt, Origin::Subagent),
        "internal_system" if field("sourceTool") == "cron" => (MessageKind::Prompt, Origin::Scheduler),
        "internal_system" => (MessageKind::InjectedContext, Origin::Harness),
        _ => (MessageKind::Prompt, Origin::Human),
    }
}

/// The custom message roles keep their role name and fields in the harness bag.
fn custom_role_bag<'m>(
    m: &'m mut Message,
    role: &str,
    body: &Fields<'_>,
    keys: &[&str],
) -> Result<&'m mut serde_json::Map<String, Value>> {
    let bag = m.harness_extra_mut(Harness::OpenClaw);
    bag.insert("message_role".into(), Value::from(role));
    for key in keys {
        if let Some(val) = present(body.get(key))? {
            bag.insert((*key).into(), val);
        }
    }
    Ok(bag)
}
