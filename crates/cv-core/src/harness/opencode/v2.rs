//! The OpenCode 2.x message model: `session_v2` + `session_message` in `opencode.db`
//! (`packages/core/src/session/sql.ts`, payloads in `packages/schema/src/session-message.ts`, at
//! opencode `v2.0.25`).
//!
//! 2.x replaced the 1.x `message` + `part` pair with one typed, sequenced row per event:
//! `session_message(id, session_id, type, seq, time_created, time_updated, data)`, where `data` is
//! the `Session.Message.Info` JSON minus `id`/`type`. A 1.x store is migrated in place on first run
//! (`database/v1-migration.ts`): the legacy `session`/`message`/`part` tables are left untouched and
//! every session is copied into the new tables under the same id, so a migrated database carries
//! both; the new tables win (2.x writes only those).
//!
//! | `type` | IR |
//! |---|---|
//! | `user` | `Prompt`/Human: `text`, `files[]` (Image or File), `agents[]` (`@name`, a `SubagentSpawn`), `skills[]` (bag) |
//! | `synthetic` | `InjectedContext`/Harness, User role (`description` in the bag); a background sub-agent's completion (`metadata.source = "subagent"`) is a `SubagentReturn`/Subagent, a background shell's (`"shell"`) stays `InjectedContext` |
//! | `system` | `InjectedContext`/Harness, System role: a mid-session model-facing update (not the system prompt) |
//! | `skill` | `InjectedContext`/Harness, User role: a loaded skill's text (`skill`, `name` in the bag) |
//! | `shell` | `InjectedContext`/Human: a user `!command`, its output as text (`shellID`, `status`, `exit` in the bag) |
//! | `assistant` | `Reply`/Model: `content[]` of `text`, `reasoning`, `tool` (results split into a trailing `ToolResult`); `error` makes it an `Error` |
//! | `compaction` | `CompactionBoundary`/Harness, then (status `completed`) a `CompactionSummary` with `summary` |
//! | `model-switched` | `ModelChange`/Harness, `Message::model` = the new model |
//! | `agent-switched`, `location-switched` | `Notice`/Harness |
//! | `idle` | a turn-end marker: a `Carrier` under `Demand::COMPLETE`, otherwise skipped |
//!
//! Anything else is kept verbatim as a `Carrier` under `complete`, and is counted in
//! `extra["opencode"]["unknown_message_types"]` on the session otherwise — never silently dropped.

use crate::json::{ReadInput, Validation};
/// cv preserves tool-result details in OpenCode's free-form metadata under this key.
/// The reader restores the original IR value; it is not an OpenCode-native status field.
pub(super) const DETAILS_KEY: &str = "clustervision.details";

use super::record::{find_signature, get, nonempty, owned, push_extra_array, string, usage, Ctx};
use crate::ir::*;
use crate::json::JsonView;
use crate::lazy::Text;
use crate::stream::{Demand, Tier};
use anyhow::Result;
use serde_json::{json, Map, Value};

#[cfg(feature = "sqlite")]
pub(super) fn tag_complete(messages: &mut [Message], id: &str, ty: &str) {
    if let Some(primary) = messages.first_mut() {
        primary
            .harness_extra_mut(Harness::OpenCode)
            .insert("record_type".into(), json!(ty));
    }
    for derived in messages.iter_mut().skip(1) {
        derived
            .harness_extra_mut(Harness::OpenCode)
            .insert("record_owner".into(), json!(id));
    }
}
#[cfg(feature = "sqlite")]
pub(super) fn attach_complete(messages: &mut [Message], id: &str, ty: &str, data: crate::Json) {
    tag_complete(messages, id, ty);
    let mut identity = Map::new();
    identity.insert("id".into(), json!(id));
    identity.insert("type".into(), json!(ty));
    crate::ir::CompleteRecord::attach_replay(messages, Harness::OpenCode, data, identity);
}

#[cfg(feature = "sqlite")]
pub(super) fn known_type(ty: &str) -> bool {
    matches!(
        ty,
        "user"
            | "synthetic"
            | "system"
            | "skill"
            | "shell"
            | "assistant"
            | "compaction"
            | "model-switched"
            | "agent-switched"
            | "location-switched"
            | "idle"
    )
}

/// Map one `session_message` row's `data` view to IR messages (0–2: a tool-bearing reply gets a
/// trailing `ToolResult`, a completed compaction gets its summary). `None` means an unknown
/// `type`, which the caller tallies; it is still carried under `complete`.
pub(super) fn build(
    id: &str,
    ty: &str,
    data: JsonView<'_>,
    opts: &Demand,
    ctx: Ctx<'_>,
) -> Result<Option<Vec<Message>>> {
    let ts = owned(get(data.get("time")?, "created")?)?
        .as_ref()
        .and_then(super::super::ts_from_value);
    let new = |role: Role, kind: MessageKind, origin: Origin| {
        let mut m = Message::of_kind(role, kind, origin);
        m.id = Some(id.to_string());
        m.timestamp = ts;
        m
    };
    let text_of = |key: &str| -> Result<Option<Text>> {
        match data.get(key)? {
            Some(text) if nonempty(text)? => ctx.text(text),
            _ => Ok(None),
        }
    };
    let mut out = Vec::new();
    match ty {
        "user" => {
            let mut m = new(Role::User, MessageKind::Prompt, Origin::Human);
            let mut bag = Map::new();
            if let Some(text) = text_of("text")? {
                m.content.push(Block::from_text(text));
            }
            if let Some(files) = data.get("files")?.map(JsonView::array).transpose()?.flatten() {
                for file in files {
                    m.content.push(file_block(file?)?);
                }
            }
            if let Some(agents) = nonempty_array(data.get("agents")?)? {
                for agent in agents.array()?.into_iter().flatten() {
                    if let Some(name) = string(agent?.get("name")?)? {
                        m.content.push(Block::from_text(format!("@{name}").into()));
                    }
                }
                bag.insert("agent_refs".into(), agents.resolve()?.into_owned());
                m.kind = MessageKind::SubagentSpawn;
            }
            if let Some(skills) = nonempty_array(data.get("skills")?)? {
                bag.insert("skills".into(), skills.resolve()?.into_owned());
            }
            keep(data, &mut bag, &["metadata"])?;
            finish(&mut m, bag);
            out.push(m);
        }
        "synthetic" | "system" | "skill" => {
            let role = if ty == "system" { Role::System } else { Role::User };
            // A background sub-agent's completion arrives as a `synthetic` row with
            // `metadata.source = "subagent"` (and `childID`): that is the sub-agent's return.
            let mut m =
                if ty == "synthetic" && string(get(data.get("metadata")?, "source")?)?.as_deref() == Some("subagent") {
                    new(role, MessageKind::SubagentReturn, Origin::Subagent)
                } else {
                    new(role, MessageKind::InjectedContext, Origin::Harness)
                };
            if let Some(text) = text_of("text")? {
                m.content.push(Block::from_text(text));
            }
            let mut bag = Map::new();
            bag.insert("message_type".into(), json!(ty));
            keep(data, &mut bag, &["description", "skill", "name", "metadata"])?;
            finish(&mut m, bag);
            out.push(m);
        }
        "shell" => {
            let mut m = new(Role::User, MessageKind::InjectedContext, Origin::Human);
            if let Some(command) = data.get("command")?.filter(|c| nonempty(*c).unwrap_or(false)) {
                m.content.push(Block::from_text(ctx.generated(|writer| {
                    writer.push("$ ")?;
                    anyhow::ensure!(writer.push_string(command)?, "expected shell command string");
                    Ok(())
                })?));
            }
            if let Some(output) = data.get("output")?.filter(|o| !o.is_null().unwrap_or(true)) {
                // `Shell.Output`: `{output, cursor, size, truncated?}`. Presence wins before type:
                // a non-string inner output renders the whole object.
                let inner = if output.is_string()? {
                    Some(output)
                } else {
                    match output.get("output")? {
                        Some(inner) => Some(inner),
                        None => output.get("text")?,
                    }
                    .filter(|inner| inner.is_string().unwrap_or(false))
                };
                let text = match inner {
                    Some(inner) if nonempty(inner)? => ctx.text(inner)?,
                    Some(_) => None,
                    None => Some(ctx.generated(|writer| writer.push_json(output))?),
                };
                if let Some(text) = text {
                    m.content.push(Block::from_text(text));
                }
            }
            let mut bag = Map::new();
            bag.insert("message_type".into(), json!("shell"));
            keep(data, &mut bag, &["shellID", "status", "exit", "metadata"])?;
            finish(&mut m, bag);
            out.push(m);
        }
        "assistant" => out.extend(assistant(
            new(Role::Assistant, MessageKind::Reply, Origin::Model),
            data,
            ctx,
        )?),
        "compaction" => {
            let mut b = new(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
            let mut bag = Map::new();
            keep(data, &mut bag, &["status", "reason", "error", "metadata"])?;
            let completed = string(data.get("status")?)?.as_deref() == Some("completed");
            let error = data.get("error")?;
            if error.map(JsonView::is_null).transpose()? == Some(false) {
                if let Some(t) = string(get(error, "type")?)? {
                    bag.insert("error_name".into(), json!(t));
                }
            }
            finish(&mut b, bag);
            out.push(b);
            if completed {
                let mut s = new(Role::Assistant, MessageKind::CompactionSummary, Origin::Harness);
                // One native compaction produces two IR turns; suffix the summary id so it
                // differs from its boundary. Native prefixed IDs do not contain `#`.
                s.id = Some(format!("{id}#summary"));
                if let Some(text) = text_of("summary")? {
                    s.content.push(Block::from_text(text));
                }
                let model = data.get("model")?;
                s.model = string(get(model, "id")?)?;
                s.usage = usage(data.get("tokens")?, data.get("cost")?)?;
                let mut bag = Map::new();
                // `recent` is the verbatim tail kept after the summary; `providerContext` the
                // provider-side state the next request resumes from.
                keep(data, &mut bag, &["recent", "providerState", "providerContext"])?;
                if let Some(p) = owned(get(model, "providerID")?)? {
                    bag.insert("providerID".into(), p);
                }
                finish(&mut s, bag);
                out.push(s);
            }
        }
        "model-switched" => {
            let mut m = new(Role::System, MessageKind::ModelChange, Origin::Harness);
            m.model = string(get(data.get("model")?, "id")?)?;
            let mut bag = Map::new();
            keep(data, &mut bag, &["model", "previous"])?;
            finish(&mut m, bag);
            out.push(m);
        }
        "agent-switched" | "location-switched" => {
            let mut m = new(Role::System, MessageKind::Notice, Origin::Harness);
            let note = if ty == "agent-switched" {
                string(data.get("agent")?)?.map(|a| format!("agent: {a}"))
            } else {
                match get(data.get("location")?, "directory")? {
                    Some(directory) => string(Some(directory))?,
                    None => string(data.get("subpath")?)?,
                }
                .map(|l| format!("location: {l}"))
            };
            if let Some(t) = note {
                m.content.push(Block::from_text(t.into()));
            }
            let mut bag = Map::new();
            bag.insert("message_type".into(), json!(ty));
            keep(
                data,
                &mut bag,
                &["agent", "location", "projectID", "subpath", "previous"],
            )?;
            finish(&mut m, bag);
            out.push(m);
        }
        "idle" => {
            if opts.tier == Tier::Complete {
                out.push(carrier(id, ty, data, ts)?);
            }
        }
        _ => {
            if opts.tier != Tier::Complete {
                return Ok(None);
            }
            out.push(carrier(id, ty, data, ts)?);
        }
    }
    crate::affordance::refine_complete_group(Harness::OpenCode, &mut out)?;
    Ok(Some(out))
}

fn nonempty_array(view: Option<JsonView<'_>>) -> Result<Option<JsonView<'_>>> {
    match view {
        Some(view) if view.array()?.is_some_and(|mut items| items.next().is_some()) => Ok(Some(view)),
        _ => Ok(None),
    }
}

fn assistant(mut m: Message, data: JsonView<'_>, ctx: Ctx<'_>) -> Result<Vec<Message>> {
    let model = data.get("model")?;
    m.model = string(get(model, "id")?)?;
    m.usage = usage(data.get("tokens")?, data.get("cost")?)?;
    let mut results = Vec::new();
    for c in data
        .get("content")?
        .map(JsonView::array)
        .transpose()?
        .flatten()
        .into_iter()
        .flatten()
    {
        let c = c?;
        match string(c.get("type")?)?.as_deref().unwrap_or("") {
            "text" => {
                if let Some(text) = c.get("text")?.filter(|t| nonempty(*t).unwrap_or(false)) {
                    if let Some(text) = ctx.text(text)? {
                        m.content.push(Block::from_text(text));
                    }
                }
            }
            "reasoning" => {
                let text = c.get("text")?.filter(|t| t.is_string().unwrap_or(false));
                let state = owned(c.get("state")?)?;
                let signature = find_signature(state.as_ref())
                    .or_else(|| state.as_ref()?.get("signature")?.as_str())
                    .map(str::to_string);
                if text.map(nonempty).transpose()?.unwrap_or(false) || signature.is_some() {
                    m.content.push(Block::Thinking {
                        text: text.map(|t| ctx.text(t)).transpose()?.flatten().unwrap_or_default(),
                        signature,
                        encrypted: None,
                        redacted: false,
                        extensions: Default::default(),
                    });
                }
            }
            "tool" => {
                let (u, r) = tool_blocks(c, ctx)?;
                m.content.push(u);
                results.extend(r);
            }
            _ => {
                let bag = m.harness_extra_mut(Harness::OpenCode);
                push_extra_array(bag, "unknown_content", c.resolve()?.into_owned());
            }
        }
    }
    let mut bag = Map::new();
    keep(
        data,
        &mut bag,
        &[
            "agent",
            "finish",
            "rawFinish",
            "snapshot",
            "error",
            "retry",
            "providerState",
            "metadata",
        ],
    )?;
    if let Some(p) = owned(get(model, "providerID")?)? {
        bag.insert("providerID".into(), p);
    }
    if let Some(v) = owned(get(model, "variant")?)? {
        bag.insert("variant".into(), v);
    }
    let error = data.get("error")?;
    if error.map(JsonView::is_null).transpose()? == Some(false) {
        m.kind = MessageKind::Error;
        m.origin = Origin::Harness;
        if let Some(t) = string(get(error, "type")?)? {
            bag.insert("error_name".into(), json!(t));
        }
        // An error turn often carries no content; its message is what the user saw.
        if m.content.is_empty() {
            if let Some(t) = string(get(error, "message")?)? {
                m.content.push(Block::from_text(t.into()));
            }
        }
    }
    finish(&mut m, bag);
    let mut out = vec![m];
    if !results.is_empty() {
        let mut tm = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        tm.timestamp = out[0].timestamp;
        tm.content = results;
        out.push(tm);
    }
    Ok(out)
}

/// A 2.x `tool` content item: `{id, name, state{status, input, content[]?, error?, metadata?}, time}`.
/// `content[]` is `text` and `file` items; a `file` result becomes an Image/File block after the
/// result, as a 1.x `state.attachments[]` does.
fn tool_blocks(c: JsonView<'_>, ctx: Ctx<'_>) -> Result<(Block, Vec<Block>)> {
    let id = string(c.get("id")?)?.unwrap_or_default();
    let name = string(c.get("name")?)?.unwrap_or_default();
    let state = c.get("state")?;
    let status = string(get(state, "status")?)?.unwrap_or_default();
    let input = match get(state, "input")? {
        // `streaming` holds the raw, still-partial argument text.
        Some(input) if input.is_string()? => decoded_input(input, ctx)?,
        Some(input) => input.argument(ctx.spans())?,
        None => Value::Null.into(),
    };
    let use_block = Block::ToolUse {
        id: id.clone(),
        name: name.clone(),
        input,
        namespace: None,
        extensions: Default::default(),
    };
    // The result is its `text` items (and any other non-`file` item as JSON), joined by newlines.
    let mut parts = Vec::new();
    let mut files = Vec::new();
    for item in get(state, "content")?
        .map(JsonView::array)
        .transpose()?
        .flatten()
        .into_iter()
        .flatten()
    {
        let item = item?;
        match string(item.get("type")?)?.as_deref() {
            Some("text") => parts.push((item, true)),
            Some("file") => files.push(result_file(item)?),
            _ => parts.push((item, false)),
        }
    }
    let is_error = status == "error";
    let content = match parts.as_slice() {
        [] => match is_error {
            true => string(get(get(state, "error")?, "message")?)?.map(Text::from),
            false => None,
        },
        [(item, true)] => Some(match item.get("text")? {
            Some(text) => ctx.text(text)?.unwrap_or_default(),
            None => Text::default(),
        }),
        parts => Some(ctx.generated(|writer| {
            for (index, (item, text)) in parts.iter().enumerate() {
                if index > 0 {
                    writer.push("\n")?;
                }
                if !text {
                    writer.push_json(*item)?;
                } else if let Some(text) = item.get("text")? {
                    writer.push_string(text)?;
                }
            }
            Ok(())
        })?),
    };
    let mut details = Map::new();
    for (k, v) in [
        ("metadata", get(state, "metadata")?),
        ("error", get(state, "error")?),
        ("time", c.get("time")?),
    ] {
        if let Some(v) = owned(v)?.filter(|v| !v.is_null() && !v.as_object().is_some_and(|o| o.is_empty())) {
            details.insert(k.to_string(), v);
        }
    }
    for k in ["executed", "providerState", "providerResultState"] {
        if let Some(v) = owned(c.get(k)?)? {
            details.insert(k.to_string(), v);
        }
    }
    let mut out = Vec::new();
    if content.is_some() || is_error || status == "completed" {
        out.push(Block::ToolResult {
            tool_use_id: id,
            content: content.unwrap_or_default(),
            is_error,
            tool_name: (!name.is_empty()).then_some(name),
            status: (!status.is_empty()).then_some(status),
            details: match owned(get(get(state, "metadata")?, DETAILS_KEY)?)? {
                Some(details) => Some(details),
                None => (!details.is_empty()).then_some(Value::Object(details)),
            },
            extensions: Default::default(),
        });
    }
    out.extend(files);
    Ok((use_block, out))
}

/// A 2.x tool input string: an encoded argument document when it parses (decoded into the arena
/// by the span rule), else the string itself — empty and partial streaming text included.
fn decoded_input(view: JsonView<'_>, ctx: Ctx<'_>) -> Result<crate::json::Json> {
    let Ctx::Spans(source, arena) = ctx else {
        return Ok(encoded(view)?.into());
    };
    if !view
        .source_range()?
        .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
    {
        return Ok(encoded(view)?.into());
    }
    let (offset, len) = arena.append(|writer| {
        view.for_each_str_chunk(|chunk| {
            writer.write_all(chunk.as_bytes())?;
            Ok(())
        })?;
        Ok(())
    })?;
    if len != 0 {
        match source.read(
            ReadInput::Backing(arena.clone(), offset, len),
            Validation::Value,
            |decoded| decoded.argument(true),
        ) {
            Ok(decoded) => return Ok(decoded),
            Err(error) if error.is::<crate::json::InvalidJson>() => {}
            Err(error) => return Err(error),
        }
    }
    view.argument(true)
}

fn encoded(view: JsonView<'_>) -> Result<Value> {
    let value = view.resolve()?.into_owned();
    Ok(serde_json::from_str(value.as_str().unwrap_or_default()).unwrap_or(value))
}

/// A user `files[]` attachment: `{data (base64), mime, source{type: inline|uri, uri?}, name?}`. The
/// bytes stay out of the IR; `data_ref`/`source` name the attachment instead.
fn file_block(f: JsonView<'_>) -> Result<Block> {
    let mime = string(f.get("mime")?)?.filter(|m| !m.is_empty());
    let uri = string(get(f.get("source")?, "uri")?)?;
    let name = string(f.get("name")?)?;
    Ok(if mime.as_ref().is_some_and(|m| m.starts_with("image/")) {
        Block::Image {
            media_type: mime,
            data_ref: uri.or(name),
            extensions: Default::default(),
        }
    } else {
        Block::File {
            mime,
            path: name,
            source: uri,
            extensions: Default::default(),
        }
    })
}

/// A tool result's `file` content item: `{uri, mime, name?}`.
fn result_file(f: JsonView<'_>) -> Result<Block> {
    let mime = string(f.get("mime")?)?.filter(|m| !m.is_empty());
    let uri = string(f.get("uri")?)?;
    Ok(if mime.as_ref().is_some_and(|m| m.starts_with("image/")) {
        Block::Image {
            media_type: mime,
            data_ref: uri,
            extensions: Default::default(),
        }
    } else {
        Block::File {
            mime,
            path: string(f.get("name")?)?,
            source: uri,
            extensions: Default::default(),
        }
    })
}

fn carrier(id: &str, ty: &str, data: JsonView<'_>, ts: Option<chrono::DateTime<chrono::Utc>>) -> Result<Message> {
    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
    m.id = Some(id.to_string());
    m.timestamp = ts;
    m.harness_extra_mut(Harness::OpenCode)
        .insert("record_type".into(), json!(ty));
    let mut rec = data.resolve()?.as_object().cloned().unwrap_or_default();
    rec.insert("id".into(), json!(id));
    rec.insert("type".into(), json!(ty));
    m.extra.insert(crate::ir::CARRIER_KEY.into(), Value::Object(rec));
    Ok(m)
}

fn keep(data: JsonView<'_>, bag: &mut Map<String, Value>, keys: &[&str]) -> Result<()> {
    for k in keys {
        if let Some(v) = owned(data.get(k)?)?.filter(|v| !v.is_null()) {
            bag.insert((*k).to_string(), v);
        }
    }
    Ok(())
}

fn finish(m: &mut Message, bag: Map<String, Value>) {
    if !bag.is_empty() {
        m.harness_extra_mut(Harness::OpenCode).extend(bag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(id: &str, ty: &str, data: &Value, opts: &Demand) -> Option<Vec<Message>> {
        super::build(id, ty, JsonView::owned(data), opts, Ctx::Owned).unwrap()
    }
    fn one(ty: &str, data: &str) -> Vec<Message> {
        build("msg_1", ty, &serde_json::from_str(data).unwrap(), &Demand::RICH).unwrap()
    }

    #[test]
    fn each_message_type_has_its_kind() {
        let kinds = |ms: &[Message]| ms.iter().map(|m| (m.role, m.kind, m.origin)).collect::<Vec<_>>();
        let u = one(
            "user",
            r#"{"time":{"created":1},"text":"hi","files":[{"data":"AA==","mime":"image/png","source":{"type":"inline"},"name":"dot.png"}]}"#,
        );
        assert_eq!(kinds(&u), [(Role::User, MessageKind::Prompt, Origin::Human)]);
        assert!(matches!(&u[0].content[1], Block::Image { data_ref: Some(n), .. } if n == "dot.png"));

        let a = one(
            "assistant",
            r#"{"time":{"created":1},"agent":"build","model":{"id":"glm","providerID":"corpus"},
                "content":[{"type":"reasoning","text":"t"},{"type":"text","text":"x"},
                  {"type":"tool","id":"c1","name":"shell","state":{"status":"error","input":{"command":"cat nope"},
                   "error":{"type":"tool.execution","message":"No such file"}},"time":{"created":1}}],
                "cost":0.5,"tokens":{"input":3,"output":2,"reasoning":1,"cache":{"read":0,"write":0}}}"#,
        );
        assert_eq!(
            kinds(&a),
            [
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness)
            ]
        );
        assert_eq!(a[0].model.as_deref(), Some("glm"));
        assert_eq!(a[0].usage.as_ref().unwrap().cost_usd, Some(0.5));
        assert!(
            matches!(&a[1].content[0], Block::ToolResult { is_error: true, content, .. } if content.matches_str("No such file") == Some(true))
        );

        let e = one(
            "assistant",
            r#"{"time":{"created":1},"agent":"build","model":{"id":"glm","providerID":"corpus"},"content":[],
                "error":{"type":"provider.error","message":"boom"}}"#,
        );
        assert_eq!(kinds(&e), [(Role::Assistant, MessageKind::Error, Origin::Harness)]);
        assert_eq!(e[0].extra["opencode"]["error_name"], "provider.error");

        let c = one(
            "compaction",
            r#"{"time":{"created":1},"status":"completed","reason":"manual","summary":"S","recent":"R"}"#,
        );
        assert_eq!(
            kinds(&c),
            [
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::Assistant, MessageKind::CompactionSummary, Origin::Harness)
            ]
        );
        assert_eq!(c[1].text().as_deref(), Some("S"));

        let m = one(
            "model-switched",
            r#"{"time":{"created":1},"model":{"id":"alt","providerID":"corpus"}}"#,
        );
        assert_eq!(
            (m[0].kind, m[0].model.as_deref()),
            (MessageKind::ModelChange, Some("alt"))
        );
        assert_eq!(
            one("agent-switched", r#"{"time":{"created":1},"agent":"plan"}"#)[0].kind,
            MessageKind::Notice
        );
        assert_eq!(
            one("synthetic", r#"{"time":{"created":1},"text":"done"}"#)[0].kind,
            MessageKind::InjectedContext
        );
        let r = one(
            "synthetic",
            r#"{"time":{"created":1},"text":"ok","metadata":{"source":"subagent","childID":"ses_c"}}"#,
        );
        assert_eq!(
            (r[0].kind, r[0].origin),
            (MessageKind::SubagentReturn, Origin::Subagent)
        );
        assert_eq!(
            one(
                "shell",
                r#"{"time":{"created":1},"shellID":"s","command":"ls","status":"exited"}"#
            )[0]
            .origin,
            Origin::Human
        );
        let sh = one(
            "shell",
            r#"{"time":{"created":1},"shellID":"s","command":"echo hi","status":"exited","exit":0,"output":{"output":"hi\n","cursor":3,"size":3}}"#,
        );
        assert_eq!(sh[0].text().as_deref(), Some("$ echo hi\nhi\n"));
    }

    #[test]
    fn idle_and_unknown_types_are_carried_only_when_complete() {
        let d = serde_json::json!({"time": {"created": 1}, "outcome": "succeeded"});
        assert!(build("i", "idle", &d, &Demand::RICH).unwrap().is_empty());
        let c = build("i", "idle", &d, &Demand::COMPLETE).unwrap();
        assert_eq!(c[0].kind, MessageKind::Carrier);
        assert!(build("x", "from-the-future", &d, &Demand::RICH).is_none());
        let c = build("x", "from-the-future", &d, &Demand::COMPLETE).unwrap();
        assert_eq!(c[0].kind, MessageKind::Carrier);
        assert_eq!(c[0].extra[crate::ir::CARRIER_KEY]["outcome"], "succeeded");
    }
}
