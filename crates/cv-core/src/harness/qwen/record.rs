//! The Qwen ChatRecord codec: one record view and the reader's demand in, session facts and up
//! to two IR messages out. Every byte source (mapped lines, positioned records, an owned value)
//! feeds this one function; the demand chooses how bodies are resolved, never which code runs.

use super::*;
use crate::harness::store::document::{text_head, text_summary, Fields};
use crate::harness::wire::genai::{self, Dialect, Part};
use crate::json::{JsonSource, JsonView};
use crate::lazy::{Resolver, Text};

/// How a record's bodies are resolved.
#[derive(Clone, Copy)]
pub(super) enum Demand<'s> {
    /// Every body inline.
    Owned,
    /// Strings stay in the captured source; composed text goes to its decoded backing.
    Spans(&'s JsonSource),
    /// Discovery: message presence, tool identities and the first prompt's title only. No
    /// argument tree, result body or system display body is decoded.
    Discovery,
}

/// Every top-level key any record kind reads.
const TOP: &[&str] = &[
    "type",
    "subtype",
    "provenance",
    "uuid",
    "parentUuid",
    "sessionId",
    "timestamp",
    "cwd",
    "gitBranch",
    "version",
    "isSidechain",
    "model",
    "usageMetadata",
    "systemPayload",
    "executionContext",
    "forkedFrom",
    "toolCallResult",
    "message",
];

#[derive(Clone, Copy)]
enum Kind {
    Prompt,
    UserContext,
    Assistant,
    ToolResult,
    Compression,
    Notice,
    Rewind,
    Model,
    Title,
    Parent,
    Error,
    Ignore,
}

/// Parsed fields and classification shared by metadata and message demands.
struct Record<'v> {
    fields: Fields<'v>,
    ty: String,
    subtype: Option<String>,
    kind: Kind,
}
impl<'v> Record<'v> {
    fn of(fields: Fields<'v>) -> Result<Self> {
        let ty = fields.string("type")?.unwrap_or_default();
        let subtype = fields.string("subtype")?;
        let provenance = fields.string("provenance")?;
        let kind = match (ty.as_str(), subtype.as_deref()) {
            ("user", None | Some("mid_turn_user_message"))
                if provenance.as_deref().is_none_or(|p| p == "real_user") =>
            {
                Kind::Prompt
            }
            ("user", _) => Kind::UserContext,
            ("assistant", _) => Kind::Assistant,
            ("tool_result", _) => Kind::ToolResult,
            ("system", Some("chat_compression")) => Kind::Compression,
            (
                "system",
                Some("goal_state" | "slash_command" | "at_command" | "notification" | "background_task_completed"),
            ) => Kind::Notice,
            ("system", Some("rewind")) => Kind::Rewind,
            ("system", Some("session_model")) => Kind::Model,
            ("system", Some("custom_title")) => Kind::Title,
            ("system", Some("parent_session")) => Kind::Parent,
            ("system", Some("turn_result")) => Kind::Error,
            _ => Kind::Ignore,
        };
        Ok(Self {
            fields,
            ty,
            subtype,
            kind,
        })
    }

    fn control(&self, s: &mut Session, st: &mut ReplayState) -> Result<bool> {
        let payload = self.fields.get("systemPayload");
        match self.kind {
            Kind::Title => {
                if let Some(t) = payload_str(payload, &["title", "customTitle"])? {
                    st.title = Some(t);
                }
            }
            Kind::Parent => {
                if let Some(p) = payload_str(payload, &["parentSessionId", "sessionId"])? {
                    s.lineage.forked_from = Some(p);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// Listing projection: inspect bodies only until presence and first prompt are established.
pub(super) fn scan(view: JsonView<'_>, s: &mut Session, st: &mut ReplayState, need_presence: bool) -> Result<bool> {
    let Some(fields) = fields(view, s)? else {
        return Ok(false);
    };
    let record = Record::of(fields)?;
    if record.control(s, st)? {
        return Ok(false);
    }
    let r = &record.fields;
    Ok(match record.kind {
        Kind::Prompt if need_presence || (st.title.is_none() && st.first_prompt.is_none()) => {
            let (body, hooks) = split_prompt(content_blocks(r.get("message"), Demand::Discovery)?)?;
            remember_prompt(st, &body);
            !body.is_empty() || !hooks.is_empty()
        }
        Kind::UserContext | Kind::Assistant | Kind::ToolResult if need_presence => {
            !content_blocks(r.get("message"), Demand::Discovery)?.is_empty()
        }
        Kind::Compression | Kind::Rewind | Kind::Model => true,
        Kind::Notice | Kind::Error if need_presence => {
            !system_blocks(r.get("systemPayload"), record.subtype.as_deref(), Demand::Discovery)?.is_empty()
        }
        _ => false,
    })
}

fn fields<'v>(view: JsonView<'v>, s: &mut Session) -> Result<Option<Fields<'v>>> {
    let fields = Fields::parse(view, TOP)?;
    if let Some(fields) = &fields {
        session_facts(s, fields)?;
    }
    Ok(fields)
}

/// One record: fold its session facts, then (unless the sink already stopped) hand its messages
/// to `sink`. Returns false once the sink has stopped.
pub(super) fn ingest(
    view: JsonView<'_>,
    s: &mut Session,
    st: &mut ReplayState,
    demand: Demand<'_>,
    stopped: bool,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    let Some(fields) = fields(view, s)? else {
        return Ok(Flow::Continue);
    };
    if stopped {
        return Ok(Flow::Continue);
    }
    emit(&Record::of(fields)?, s, st, demand, sink)
}

fn string(view: Option<JsonView<'_>>, key: &str) -> Result<Option<String>> {
    let Some(view) = view.map(|v| v.get(key)).transpose()?.flatten() else {
        return Ok(None);
    };
    Ok(view
        .is_string()?
        .then(|| view.resolve())
        .transpose()?
        .and_then(|v| v.as_str().map(str::to_owned)))
}

fn timestamp(r: &Fields<'_>) -> Result<Option<DateTime>> {
    Ok(r.string("timestamp")?.as_deref().and_then(parse_ts))
}
type DateTime = chrono::DateTime<chrono::Utc>;

/// Session-level facts every record carries: id, cwd, branch, version, time span, fork origin,
/// approval mode.
fn session_facts(s: &mut Session, r: &Fields<'_>) -> Result<()> {
    if s.lineage.parent.is_none() {
        if let Some(id) = r.string("sessionId")? {
            if s.id != id {
                s.id = id;
            }
        }
    }
    if s.cwd.is_none() {
        s.cwd = r.string("cwd")?.map(PathBuf::from);
    }
    if let Some(branch) = r.string("gitBranch")? {
        if s.git.as_ref().and_then(|g| g.branch.as_deref()) != Some(branch.as_str()) {
            s.git = Some(GitInfo {
                branch: Some(branch),
                ..GitInfo::default()
            });
        }
    }
    if let Some(v) = r.string("version")? {
        s.harness_extra_mut(Harness::Qwen)
            .insert("version".into(), Value::String(v));
    }
    // The approval mode each prompt ran under (`--approval-mode`, `/approval-mode`).
    if let Some(mode) = string(r.get("executionContext"), "approvalMode")? {
        crate::affordance::note_mode(s.harness_extra_mut(Harness::Qwen), &mode);
    }
    if let Some(ts) = timestamp(r)? {
        if s.created_at.is_none_or(|c| ts < c) {
            s.created_at = Some(ts);
        }
        if s.updated_at.is_none_or(|u| ts > u) {
            s.updated_at = Some(ts);
        }
    }
    if s.lineage.forked_from.is_none() {
        if let Some(from) = string(r.get("forkedFrom"), "sessionId")? {
            s.lineage.forked_from = Some(from);
        }
    }
    Ok(())
}

/// Translate one `ChatRecord` into 0..2 IR messages.
fn emit(
    record: &Record<'_>,
    s: &mut Session,
    st: &mut ReplayState,
    demand: Demand<'_>,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    if record.control(s, st)? {
        return Ok(Flow::Continue);
    }
    let r = &record.fields;
    let ty = &record.ty;
    let subtype = &record.subtype;
    let payload = r.get("systemPayload");
    let blocks = || -> Result<Vec<Block>> {
        if ty == "system" {
            system_blocks(payload, subtype.as_deref(), demand)
        } else {
            content_blocks(r.get("message"), demand)
        }
    };
    let m = match record.kind {
        Kind::Prompt => {
            // A sub-agent transcript's prompt was written by the parent agent, not a person.
            let origin = if r.get("isSidechain").map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
                Origin::Subagent
            } else {
                Origin::Human
            };
            let mut m = Message::of_kind(Role::User, MessageKind::Prompt, origin);
            let (body, hooked) = split_prompt(blocks()?)?;
            m.content = body;
            remember_prompt(st, &m.content);
            if !hooked.is_empty() {
                // A UserPromptSubmit hook's `additionalContext`: its own Hook turn, after the prompt.
                let mut hook = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Hook);
                hook.content = hooked;
                hook.timestamp = timestamp(r)?;
                if finish(m, r, subtype.as_deref(), sink)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                return Ok(sink.message(hook));
            }
            m
        }
        Kind::UserContext => {
            let sub = subtype.as_deref();
            // Model-facing input the harness (or a scheduler, or another agent) wrote: notifications,
            // cron fires, goal runtime turns, agent mentions/messages.
            let origin = match sub {
                Some("cron") => Origin::Scheduler,
                Some("agent_mention" | "agent_message") => Origin::Subagent,
                _ => Origin::Harness,
            };
            let mut m = Message::of_kind(Role::System, MessageKind::InjectedContext, origin);
            m.content = blocks()?;
            m
        }
        Kind::Assistant => {
            let mut m = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
            m.content = blocks()?;
            if let Some(model) = r.string("model")? {
                match st.session_model.as_deref() {
                    None => st.session_model = Some(model),
                    Some(m0) if m0 != model => m.model = Some(model),
                    _ => {}
                }
            }
            if !matches!(demand, Demand::Discovery) {
                m.usage = r.get("usageMetadata").map(usage_of).transpose()?.flatten();
            }
            for b in &m.content {
                if let Block::ToolUse { id, name, .. } = b {
                    st.call_names.insert(id.clone(), name.clone());
                    if name == "agent" {
                        st.spawn_calls.insert(id.clone());
                    }
                }
            }
            if m.content
                .iter()
                .any(|b| matches!(b, Block::ToolUse { id, .. } if st.spawn_calls.contains(id)))
            {
                m.kind = MessageKind::SubagentSpawn;
            }
            m
        }
        Kind::ToolResult => {
            let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            m.content = blocks()?;
            let status = string(r.get("toolCallResult"), "status")?;
            let mut spawned = false;
            for b in &mut m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    is_error,
                    status: st_field,
                    tool_name,
                    ..
                } = b
                {
                    *is_error |= status.as_deref() == Some("error");
                    st_field.clone_from(&status);
                    if let Some(n) = st.call_names.get(tool_use_id.as_str()) {
                        *tool_name = Some(n.clone());
                    }
                    spawned |= st.spawn_calls.contains(tool_use_id.as_str());
                }
            }
            if spawned {
                m.kind = MessageKind::SubagentReturn;
                m.origin = Origin::Subagent;
            }
            m
        }
        Kind::Compression => {
            // `{info: {originalTokenCount, newTokenCount, triggerReason, ...}, compressedHistory:
            // Content[]}`: the boundary, then the summary turn that seeds the new window.
            let mut m = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
            m.content.push(Block::Text {
                text: "[conversation compressed]".to_string().into(),
            });
            let info = payload.map(|p| p.get("info")).transpose()?.flatten();
            if let Some(info) = info.filter(|v| v.is_object().unwrap_or(false)) {
                let mut kept = Vec::new();
                for k in ["triggerReason", "originalTokenCount", "newTokenCount"] {
                    if let Some(v) = info.get(k)? {
                        kept.push((k, v.resolve()?.into_owned()));
                    }
                }
                let bag = m.harness_extra_mut(Harness::Qwen);
                for (k, v) in kept {
                    bag.insert(k.into(), v);
                }
            }
            let summary = blocks()?;
            let summary_ts = timestamp(r)?;
            if finish(m, r, subtype.as_deref(), sink)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
            if summary.is_empty() {
                return Ok(Flow::Continue);
            }
            let mut sm = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
            sm.content = summary;
            sm.timestamp = summary_ts;
            return Ok(sink.message(sm));
        }
        Kind::Notice => {
            let mut m = Message::of_kind(Role::System, MessageKind::Notice, Origin::Harness);
            m.content = blocks()?;
            m
        }
        Kind::Rewind => {
            let mut m = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
            m.content.push(Block::Text {
                text: "[rewound]".to_string().into(),
            });
            m
        }
        Kind::Model => {
            let mut m = Message::of_kind(Role::System, MessageKind::ModelChange, Origin::Harness);
            m.model = payload_str(payload, &["model", "modelId"])?;
            m
        }
        Kind::Error => {
            let mut m = Message::of_kind(Role::System, MessageKind::Error, Origin::Harness);
            m.content = blocks()?;
            m
        }
        // Telemetry, attribution and file-history snapshots, session settings echoes, artifacts,
        // goal bookkeeping and the managed-session protocol: not conversation.
        _ => return Ok(Flow::Continue),
    };
    finish(m, r, subtype.as_deref(), sink)
}

fn split_prompt(blocks: Vec<Block>) -> Result<(Vec<Block>, Vec<Block>)> {
    let mut body = Vec::new();
    let mut hooks = Vec::new();
    for block in blocks {
        if matches!(&block, Block::Text { text } if text_head(text, &Resolver::new(None), 80, true)?.0.starts_with(HOOK_CONTEXT_TAG))
        {
            hooks.push(block);
        } else {
            body.push(block);
        }
    }
    Ok((body, hooks))
}
fn remember_prompt(st: &mut ReplayState, blocks: &[Block]) {
    if st.first_prompt.is_none() {
        st.first_prompt = text_summary(
            blocks
                .iter()
                .filter_map(|b| if let Block::Text { text } = b { Some(text) } else { None }),
            &Resolver::new(None),
            false,
        );
    }
}

/// Stamp a message with its record's identity and hand it to the sink (an empty one is dropped).
fn finish(mut m: Message, r: &Fields<'_>, subtype: Option<&str>, sink: &mut dyn MessageSink) -> Result<Flow> {
    if m.content.is_empty() && m.kind != MessageKind::ModelChange {
        return Ok(Flow::Continue);
    }
    m.id = r.string("uuid")?;
    m.parent_id = r.string("parentUuid")?;
    m.timestamp = timestamp(r)?;
    if let Some(sub) = subtype {
        m.harness_extra_mut(Harness::Qwen)
            .insert("subtype".into(), Value::String(sub.to_string()));
    }
    Ok(sink.message(m))
}

/// The first string among a `systemPayload`'s `keys`.
fn payload_str(payload: Option<JsonView<'_>>, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        if let Some(value) = string(payload, key)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// Gemini usage metadata: `promptTokenCount` includes `cachedContentTokenCount`; the IR's input is
/// the disjoint remainder.
fn usage_of(v: JsonView<'_>) -> Result<Option<Usage>> {
    let fields = Fields::parse(
        v,
        &[
            "promptTokenCount",
            "candidatesTokenCount",
            "cachedContentTokenCount",
            "thoughtsTokenCount",
        ],
    )?;
    let get = |k: &str| -> Result<Option<u64>> {
        Ok(fields
            .as_ref()
            .and_then(|f| f.get(k))
            .map(|v| v.as_number())
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64()))
    };
    let cached = get("cachedContentTokenCount")?;
    let u = Usage {
        input_tokens: get("promptTokenCount")?.map(|p| p.saturating_sub(cached.unwrap_or(0))),
        output_tokens: get("candidatesTokenCount")?,
        cache_read_tokens: cached.filter(|c| *c > 0),
        reasoning_tokens: get("thoughtsTokenCount")?.filter(|t| *t > 0),
        ..Usage::default()
    };
    Ok((u.input_tokens.is_some() || u.output_tokens.is_some()).then_some(u))
}

/// A string under the demand's text rule: inline, or a checked source text under spans.
fn text_of(v: JsonView<'_>, demand: Demand<'_>) -> Result<Option<Text>> {
    match demand {
        Demand::Owned => v.text(false),
        Demand::Spans(_) | Demand::Discovery => v.source_text(),
    }
}

/// IR blocks for a Gemini `Content`'s parts (`message.parts`).
fn content_blocks(message: Option<JsonView<'_>>, demand: Demand<'_>) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    let parts = message
        .map(|m| m.get("parts"))
        .transpose()?
        .flatten()
        .map(|v| v.array())
        .transpose()?
        .flatten();
    let discovery = matches!(demand, Demand::Discovery);
    for part in parts.into_iter().flatten() {
        let part = part?;
        match genai::select(part, Dialect::Qwen)?.part {
            Some(Part::Call(call)) => {
                let name = string(Some(call), "name")?.unwrap_or_else(|| "tool".into());
                let id = string(Some(call), "id")?.unwrap_or_else(|| name.clone());
                let args = call.get("args")?;
                // `tool_call` invokes a deferred tool (one `tool_search` loaded) by name:
                // `{name, arguments}`. The call is that tool's, namespaced `tool_call`.
                let inner = if name == TOOL_CALL { string(args, "name")? } else { None };
                let (name, namespace, input) = match inner {
                    Some(inner) => (
                        inner,
                        Some(TOOL_CALL.to_string()),
                        args.map(|v| v.get("arguments")).transpose()?.flatten(),
                    ),
                    None => (name, None, args),
                };
                let input = match (input, demand) {
                    (Some(_), Demand::Discovery) | (None, _) => Value::Null.into(),
                    (Some(v), Demand::Spans(_)) => v.to_json()?,
                    (Some(v), Demand::Owned) => v.resolve()?.into_owned().into(),
                };
                out.push(Block::ToolUse {
                    id,
                    name,
                    input,
                    namespace,
                });
            }
            Some(Part::Response(result)) => {
                let name = string(Some(result), "name")?.unwrap_or_else(|| "tool".into());
                let resp = result.get("response")?;
                out.push(Block::ToolResult {
                    tool_use_id: string(Some(result), "id")?.unwrap_or_else(|| name.clone()),
                    content: response(resp, demand)?,
                    is_error: resp.map(|v| v.get("error")).transpose()?.flatten().is_some(),
                    tool_name: Some(name),
                    status: None,
                    details: None,
                });
            }
            Some(Part::Media(block)) => out.push(block),
            Some(Part::Text {
                value,
                thought,
                signature,
            }) => {
                let Some(mut text) = text_of(value, demand)? else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                if discovery {
                    // Counting needs block presence, tool identity and the first prompt's title;
                    // it never needs a decoded result body or argument tree. Keep the classifier's
                    // trimmed hook prefix separately from the title's untrimmed 81 characters.
                    let head = if !thought
                        && text_head(&text, &Resolver::new(None), 80, true)?
                            .0
                            .starts_with(HOOK_CONTEXT_TAG)
                    {
                        HOOK_CONTEXT_TAG.to_string()
                    } else {
                        text_head(&text, &Resolver::new(None), 81, false)?.0
                    };
                    text = head.into();
                }
                let signature = match signature {
                    Some(value) if thought && !discovery && value.is_string()? => {
                        value.resolve()?.as_str().map(str::to_owned)
                    }
                    _ => None,
                };
                out.push(genai::text_block(text, thought, signature));
            }
            None => {}
        }
    }

    Ok(out)
}

/// A `functionResponse.response` as result text: its `output` (or `error`) when it is an object,
/// a string itself, anything else as compact JSON.
fn response(view: Option<JsonView<'_>>, demand: Demand<'_>) -> Result<Text> {
    let Some(view) = view else { return Ok(Text::default()) };
    if matches!(demand, Demand::Discovery) {
        return Ok(Text::default());
    }
    let payload = if view.is_object()? {
        view.get("output")?.or(view.get("error")?).unwrap_or(view)
    } else {
        view
    };
    compose(&[Piece::Value(payload)], demand)
}

/// One piece of a composed text: a literal, any value (a string as its text, anything else as its
/// compact JSON), or any value as its compact JSON.
#[derive(Clone, Copy)]
enum Piece<'a, 'v> {
    Lit(&'a str),
    Value(JsonView<'v>),
    Json(JsonView<'v>),
}

/// The concatenation of `pieces` under the demand. Under spans one string stays in the source and
/// anything composed goes to the decoded backing, never owned. Discovery keeps one character of
/// each string: only whether the text is empty is ever read.
fn compose(pieces: &[Piece<'_, '_>], demand: Demand<'_>) -> Result<Text> {
    match demand {
        Demand::Spans(source) => {
            if let [Piece::Value(v)] = pieces {
                if let Some(text) = v.source_text()? {
                    return Ok(text);
                }
            }
            crate::harness::store::text::append(source, &source.decoded_backing()?, |out| {
                for piece in pieces {
                    match *piece {
                        Piece::Lit(s) => out.push(s)?,
                        Piece::Value(v) => {
                            if !out.push_string(v)? {
                                out.push_json(v)?;
                            }
                        }
                        Piece::Json(v) => out.push_json(v)?,
                    }
                }
                Ok(())
            })
        }
        Demand::Owned | Demand::Discovery => {
            let discovery = matches!(demand, Demand::Discovery);
            let mut text = String::new();
            for piece in pieces {
                match *piece {
                    Piece::Lit(s) => text.push_str(s),
                    Piece::Value(v) if v.is_string()? => {
                        if discovery {
                            let head = v.source_text()?.unwrap_or_default();
                            text.push_str(&text_head(&head, &Resolver::new(None), 1, false)?.0);
                        } else {
                            text.push_str(v.resolve()?.as_str().unwrap_or_default());
                        }
                    }
                    Piece::Value(_) | Piece::Json(_) if discovery => text.push_str("{}"),
                    Piece::Value(v) | Piece::Json(v) => text.push_str(&v.resolve()?.to_string()),
                }
            }
            Ok(text.into())
        }
    }
}

/// The display text of a `system` record's `systemPayload`, by subtype; none when it is empty.
fn system_blocks(payload: Option<JsonView<'_>>, subtype: Option<&str>, demand: Demand<'_>) -> Result<Vec<Block>> {
    let mut pieces = Vec::new();
    match subtype {
        Some("chat_compression") => {
            let first = payload
                .map(|p| p.get("compressedHistory"))
                .transpose()?
                .flatten()
                .map(JsonView::array)
                .transpose()?
                .flatten()
                .and_then(|mut items| items.next())
                .transpose()?;
            let parts = first
                .map(|v| v.get("parts"))
                .transpose()?
                .flatten()
                .map(JsonView::array)
                .transpose()?
                .flatten();
            for part in parts.into_iter().flatten() {
                if let Some(text) = part?.get("text")?.filter(|v| v.is_string().unwrap_or(false)) {
                    if !pieces.is_empty() {
                        pieces.push(Piece::Lit("\n"));
                    }
                    pieces.push(Piece::Value(text));
                }
            }
            if pieces.is_empty() {
                return Ok(Vec::new());
            }
        }
        Some("goal_state") => {
            let goal = payload
                .map(|p| p.get("snapshot"))
                .transpose()?
                .flatten()
                .map(|v| v.get("goal"))
                .transpose()?
                .flatten();
            let Some(objective) = goal
                .map(|g| g.get("objective"))
                .transpose()?
                .flatten()
                .filter(|v| v.is_string().unwrap_or(false))
            else {
                return Ok(Vec::new());
            };
            let cause = format!(
                "[goal {}] ",
                string(payload, "cause")?.unwrap_or_else(|| "update".into())
            );
            let status = format!(" ({})", string(goal, "status")?.unwrap_or_else(|| "?".into()));
            let text = compose(
                &[Piece::Lit(&cause), Piece::Value(objective), Piece::Lit(&status)],
                demand,
            )?;
            return Ok(non_empty(text));
        }
        Some("slash_command" | "at_command" | "notification" | "background_task_completed") => {
            let mut found = None;
            for key in ["rawCommand", "userText", "text", "message", "summary"] {
                if let Some(v) = payload
                    .map(|p| p.get(key))
                    .transpose()?
                    .flatten()
                    .filter(|v| v.is_string().unwrap_or(false))
                {
                    found = Some(v);
                    break;
                }
            }
            match (found, payload) {
                (Some(v), _) => pieces.push(Piece::Value(v)),
                (None, Some(v)) => pieces.push(Piece::Json(v)),
                (None, None) => return Ok(Vec::new()),
            }
        }
        Some("turn_result") => {
            let Some(error) = payload
                .map(|p| p.get("error"))
                .transpose()?
                .flatten()
                .filter(|e| !e.is_null().unwrap_or(true))
            else {
                return Ok(Vec::new());
            };
            // An empty error string remains an Error message with an empty text block.
            let piece = match error.get("message")?.filter(|v| v.is_string().unwrap_or(false)) {
                Some(message) => Piece::Value(message),
                None => Piece::Json(error),
            };
            return Ok(vec![Block::Text {
                text: compose(&[piece], demand)?,
            }]);
        }
        _ => return Ok(Vec::new()),
    }
    Ok(non_empty(compose(&pieces, demand)?))
}

fn non_empty(text: Text) -> Vec<Block> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Block::Text { text }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_content_keeps_only_bounded_title_and_tool_facts() -> Result<()> {
        let world = crate::test_world("qwen-discovery-content-facts");
        let path = world.base.join("record.json");
        let giant = "雪\n".repeat(128 * 1024);
        let record = serde_json::json!({"message":{"parts":[
            {"text":giant},
            {"text":format!("{}{}hook", " ".repeat(100_000), HOOK_CONTEXT_TAG)},
            {"text":giant,"thought":true,"thoughtSignature":giant},
            {"functionCall":{"id":"call","name":"tool_call","args":{"name":"agent","arguments":{"prompt":giant}}}},
            {"functionResponse":{"id":"call","name":"tool_call","response":{"output":{"nested":giant},"error":null}}}
        ]}});
        std::fs::write(&path, serde_json::to_vec(&record)?)?;
        let source = JsonSource::capture(&path)?;
        let blocks = source.read_document(|view| content_blocks(view.get("message")?, Demand::Discovery))?;
        assert!(serde_json::to_vec(&blocks)?.len() < 2000);
        assert!(
            matches!(&blocks[3], Block::ToolUse { id, name, namespace: Some(namespace), input }
            if id == "call" && name == "agent" && namespace == "tool_call" && input.resolve()?.is_null())
        );
        assert!(
            matches!(&blocks[4], Block::ToolResult { tool_use_id, content, is_error: true, .. }
            if tool_use_id == "call" && content.is_empty())
        );
        let Block::Text { text } = &blocks[1] else {
            panic!("hook text")
        };
        assert_eq!(text.try_resolve(&Resolver::new(None))?.as_ref(), HOOK_CONTEXT_TAG);
        Ok(())
    }
}

#[cfg(test)]
mod genai_contracts {
    use super::*;
    use crate::harness::wire::genai_frozen as frozen;
    #[test]
    fn decoder_matches_independent_frozen_parts() -> Result<()> {
        let world = crate::test_world("qwen-genai-frozen");
        let path = world.base.join("parts.json");
        let resolver = Resolver::new(Some(path.clone()));
        let mut rows = Vec::new();
        for (case, parts) in frozen::cases().into_iter().enumerate() {
            let value = serde_json::json!({"parts":parts});
            let bytes = serde_json::to_vec(&value)?;
            std::fs::write(&path, &bytes)?;
            let source = JsonSource::capture(&path)?;
            for demand in 0..3 {
                let (blocks, flag) = source.read_range_value(0, source.len(), |view| {
                    let d = match demand {
                        0 => Demand::Owned,
                        1 => Demand::Spans(&source),
                        _ => Demand::Discovery,
                    };
                    Ok((content_blocks(Some(view), d)?, false))
                })?;
                rows.push(serde_json::json!({"case":case,"demand":demand,"source_sha256":crate::digest::sha256_hex(&bytes),"result":frozen::row(blocks,flag,&resolver)?}));
            }
            assert_eq!(std::fs::read(&path)?, bytes, "decoder changed source");
        }
        frozen::assert_frozen(rows, include_str!("../../../tests/fixtures/wire/qwen-parts.jsonl"))
    }
}
