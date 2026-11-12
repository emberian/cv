//! One staged transcript, with only identity/tool facts retained for global resumability.
use super::*;
use crate::{
    harness::{
        store::JsonlStage,
        wire::{
            anthropic::write as blocks,
            write::{Object, ObjectWriter},
        },
    },
    lazy::Resolver,
    Message,
};
use anyhow::Context as _;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Write,
};

struct Context<'a> {
    session: &'a Session,
    session_id: String,
    agent: Option<String>,
    cwd: String,
    branch: Option<String>,
    ts: String,
    mode: ThinkingMode,
}
struct Record<'a> {
    generated: bool,
    base: Cow<'a, Map<String, Value>>,
    extras: Option<&'a Map<String, Value>>,
    blocks: Vec<Cow<'a, Block>>,
    role: Role,
    mode: ThinkingMode,
    special: Option<MessageKind>,
    sidecar: Option<&'a Value>,
}
impl Record<'_> {
    fn with_fields<T>(&self, f: impl FnOnce(Object<'_>, Option<Object<'_>>) -> Result<T>) -> Result<T> {
        let mut top = Object::new(&self.base);
        let mut message = top.get("message").and_then(Value::as_object).map(Object::new);
        if let Some(extras) = self.extras {
            for (key, value) in extras {
                if is_internal_extra_key(key) || key == "sessionId" || key == "cwd" {
                    continue;
                }
                if key == crate::harness::claude::TOOL_USE_RESULT_KEY && top.get(key).is_some() {
                    continue;
                }
                if let Some(sub) = key.strip_prefix(crate::harness::claude::MESSAGE_EXTRA_PREFIX) {
                    if let Some(message) = message.as_mut() {
                        message.set(sub, value);
                    }
                } else {
                    top.set(key, value);
                    if key == "message" {
                        message = value.as_object().map(Object::new);
                    }
                }
            }
        }
        f(top, message)
    }
    fn generated_content(&self, message: &Object<'_>) -> bool {
        if !self.generated {
            return false;
        }
        // Object borrows values from its base map. Pointer identity means extras did not
        // replace content, so it is still the generated placeholder for the IR blocks.
        self.base
            .get("message")
            .and_then(|m| m.get("content"))
            .zip(message.get("content"))
            .is_some_and(|(original, current)| std::ptr::eq(original, current))
    }
    fn bare(&self) -> bool {
        self.role == Role::User && self.blocks.iter().all(|b| matches!(b.as_ref(), Block::Text { .. }))
    }
    fn eligible(&self, b: &Block) -> bool {
        match self.role {
            Role::Tool => matches!(b, Block::ToolResult { .. }),
            Role::Assistant => !matches!(b, Block::ToolResult { .. }),
            _ => true,
        }
    }
    fn default_empty(&self) -> bool {
        self.role == Role::Assistant && !self.blocks.iter().any(|b| self.eligible(b))
    }
    /// (before, after): only an assistant nonempty array reduced to empty drops its record.
    fn array_facts(
        &self,
        message: Option<&Object<'_>>,
        answered: Option<&HashSet<String>>,
        mut note: impl FnMut(&str, bool),
    ) -> (usize, usize) {
        let Some(message) = message else { return (0, 0) };
        if self.special.is_some() {
            return (0, 0);
        }
        if self.generated_content(message) {
            if self.bare() {
                return (0, 0);
            }
            if self.default_empty() {
                return (1, 1);
            }
            let (mut before, mut after) = (0, 0);
            for b in &self.blocks {
                if !self.eligible(b) {
                    continue;
                }
                before += 1;
                match b.as_ref() {
                    Block::ToolUse { id, .. } => {
                        note(id, false);
                        if answered.is_some_and(|a| !a.contains(id)) {
                            continue;
                        }
                    }
                    Block::ToolResult { tool_use_id, .. } => note(tool_use_id, true),
                    _ => {}
                }
                after += 1;
            }
            (before, after)
        } else {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                return (0, 0);
            };
            let mut after = 0;
            for b in content {
                if self.mode == ThinkingMode::Drop
                    && matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    )
                {
                    continue;
                }
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                        note(id, false);
                        if answered
                            .is_some_and(|a| b.get("id").and_then(Value::as_str).is_none_or(|id| !a.contains(id)))
                        {
                            continue;
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = b.get("tool_use_id").and_then(Value::as_str) {
                            note(id, true);
                        }
                    }
                    _ => {}
                }
                after += 1;
            }
            (content.len(), after)
        }
    }
    fn survives(
        &self,
        top: &Object<'_>,
        message: Option<&Object<'_>>,
        answered: &HashSet<String>,
        agent: Option<&str>,
    ) -> bool {
        if agent.is_some() && top.get("type").and_then(Value::as_str) == Some("ai-title") {
            return false;
        }
        if top.get("type").and_then(Value::as_str) == Some("assistant") {
            let (before, after) = self.array_facts(message, Some(answered), |_, _| {});
            if before > 0 && after == 0 {
                return false;
            }
        }
        true
    }
    fn write_content(
        &self,
        writer: &mut dyn Write,
        message: &Object<'_>,
        answered: Option<&HashSet<String>>,
        resolver: &Resolver,
    ) -> Result<()> {
        if self.generated_content(message) {
            if self.special == Some(MessageKind::CompactionSummary) {
                return self.write_joined(writer, resolver);
            }
            if self.bare() {
                return crate::harness::wire::write::joined_text(
                    writer,
                    self.blocks.iter().filter_map(|b| {
                        if let Block::Text { text } = b.as_ref() {
                            Some(text)
                        } else {
                            None
                        }
                    }),
                    resolver,
                );
            }
            writer.write_all(b"[")?;
            let mut first = true;
            for b in &self.blocks {
                if !self.eligible(b) {
                    continue;
                }
                if let Block::ToolUse { id, .. } = b.as_ref() {
                    if answered.is_some_and(|a| !a.contains(id)) {
                        continue;
                    }
                }
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                blocks::block(writer, b, &anthropic::ANTHROPIC, self.role != Role::Assistant, resolver)?;
            }
            // Generated content uses the ordinary empty-turn fallback. Replayed content below
            // instead marks a nonempty native array emptied by Drop as reasoning omitted.
            if first && self.default_empty() {
                serde_json::to_writer(&mut *writer, &json!({"type":"text","text":""}))?;
            }
            writer.write_all(b"]")?;
        } else if let Some(content) = message.get("content") {
            if let Some(array) = content.as_array() {
                writer.write_all(b"[")?;
                let mut first = true;
                for b in array {
                    if self.mode == ThinkingMode::Drop
                        && matches!(
                            b.get("type").and_then(Value::as_str),
                            Some("thinking" | "redacted_thinking")
                        )
                    {
                        continue;
                    }
                    if b.get("type").and_then(Value::as_str) == Some("tool_use")
                        && answered.is_some_and(|answered| {
                            b.get("id")
                                .and_then(Value::as_str)
                                .is_none_or(|id| !answered.contains(id))
                        })
                    {
                        continue;
                    }
                    if !first {
                        writer.write_all(b",")?;
                    }
                    first = false;
                    serde_json::to_writer(&mut *writer, b)?;
                }
                if first && self.mode == ThinkingMode::Drop && !array.is_empty() {
                    serde_json::to_writer(&mut *writer, &json!({"type":"text","text":"[reasoning omitted]"}))?;
                }
                writer.write_all(b"]")?;
            } else {
                serde_json::to_writer(writer, content)?;
            }
        }
        Ok(())
    }
    fn write(
        &self,
        writer: &mut dyn Write,
        c: &Context<'_>,
        answered: &HashSet<String>,
        ids: &HashSet<String>,
        previous: Option<&str>,
        resolver: &Resolver,
    ) -> Result<Option<String>> {
        self.with_fields(|top, message| {
            if !self.survives(&top, message.as_ref(), answered, c.agent.as_deref()) {
                return Ok(None);
            }
            let uuid = top.get("uuid").and_then(Value::as_str).map(str::to_owned);
            let parent = top
                .get("parentUuid")
                .and_then(Value::as_str)
                .filter(|p| !ids.contains(*p))
                .map(|_| json!(previous));
            let identity = json!(c.session_id);
            let cwd = json!(c.cwd);
            let agent = json!(c.agent);
            let side = Value::Bool(c.agent.is_some());
            let mut top = top.shortened();
            if let Some(parent) = &parent {
                top.set("parentUuid", parent);
            }
            // Carriers only rewrite identity fields they originally held.
            if top.get("sessionId").is_some() {
                top.set("sessionId", &identity);
            }
            if top.get("cwd").is_some() {
                top.set("cwd", &cwd);
            }
            if c.agent.is_some() {
                top.set("isSidechain", &side);
                top.set("agentId", &agent);
            } else {
                if top.get("isSidechain").is_some() {
                    top.set("isSidechain", &side);
                }
                top.remove("agentId");
            }
            let prune = top.get("type").and_then(Value::as_str) == Some("assistant");
            top.write(writer, |key, value, writer| {
                if key == "message" {
                    if let Some(message) = &message {
                        message.write(writer, |key, _, writer| {
                            if key == "content" {
                                self.write_content(writer, message, prune.then_some(answered), resolver)?;
                                Ok(true)
                            } else {
                                Ok(false)
                            }
                        })?;
                        return Ok(true);
                    }
                }
                if self.special.is_some() && self.base.get(key).is_some_and(|original| std::ptr::eq(original, value)) {
                    if key == "content" {
                        self.write_joined(writer, resolver)?;
                        return Ok(true);
                    }
                    if key == "rendered" {
                        writer.write_all(b"[{\"content\":")?;
                        self.write_joined(writer, resolver)?;
                        writer.write_all(b"}]")?;
                        return Ok(true);
                    }
                }
                if key == crate::harness::claude::TOOL_USE_RESULT_KEY
                    && self.base.get(key).is_some_and(|original| std::ptr::eq(original, value))
                {
                    let Some(details) = self.sidecar else { return Ok(false) };
                    if let Some(parked) = details.get(crate::harness::claude::TOOL_USE_RESULT_KEY) {
                        serde_json::to_writer(writer, parked)?;
                    } else if let Some(map) = details.as_object() {
                        let mut object = Object::new(map);
                        object.remove(crate::harness::claude::PERSISTED_OUTPUT_KEY);
                        object.write(writer, |_, _, _| Ok(false))?;
                    } else {
                        serde_json::to_writer(writer, details)?;
                    }
                    return Ok(true);
                }
                Ok(false)
            })?;
            Ok(uuid)
        })
    }
    fn write_joined(&self, writer: &mut dyn Write, resolver: &Resolver) -> Result<()> {
        crate::harness::wire::write::joined_text(
            writer,
            self.blocks.iter().filter_map(|b| {
                if let Block::Text { text } = b.as_ref() {
                    Some(text)
                } else {
                    None
                }
            }),
            resolver,
        )
    }
}

fn projected<'a>(message: &'a Message, mode: ThinkingMode, resolver: &Resolver) -> Result<Vec<Cow<'a, Block>>> {
    // Portable policy is applied to the final wire records, including raw content overrides.
    if mode == ThinkingMode::Portable {
        return Ok(message.content.iter().map(Cow::Borrowed).collect());
    }
    let mut blocks = Vec::with_capacity(message.content.len());
    for block in &message.content {
        match crate::emit::thinking::fate(block, Harness::Claude, mode, resolver)? {
            crate::emit::ThinkingFate::Drop => {}
            crate::emit::ThinkingFate::Placeholder => {
                let Block::Thinking {
                    signature,
                    encrypted,
                    redacted,
                    ..
                } = block
                else {
                    unreachable!()
                };
                blocks.push(Cow::Owned(Block::Text {
                    text: crate::emit::thinking_placeholder(signature.as_deref(), encrypted.as_deref(), *redacted)
                        .into(),
                }));
            }
            _ => blocks.push(Cow::Borrowed(block)),
        }
    }
    Ok(blocks)
}

/// Cache only completed acyclic resolutions while the dropped-record map is unchanged.
/// Forward links can become resolvable when a later record is dropped, so every insertion
/// invalidates the cache. Cycles retain the original bounded-walk result without compression.
#[derive(Default)]
struct ParentLinks {
    links: HashMap<String, Option<String>>,
    resolved: HashMap<String, std::sync::Arc<Option<String>>>,
    #[cfg(test)]
    probes: usize,
}
impl ParentLinks {
    fn insert(&mut self, id: String, parent: Option<String>) {
        self.links.insert(id, parent);
        self.resolved.clear();
    }
    fn clear(&mut self) {
        self.links.clear();
        self.resolved.clear();
    }
    fn resolve(&mut self, pid: String) -> Option<String> {
        let mut parent = Some(pid);
        let mut path = Vec::new();
        let mut completed = false;
        let mut shared = None;
        for _ in 0..=self.links.len() {
            let Some(id) = parent.as_ref() else {
                completed = true;
                break;
            };
            if let Some(cached) = self.resolved.get(id) {
                parent = cached.as_ref().clone();
                shared = Some(cached.clone());
                completed = true;
                break;
            }
            #[cfg(test)]
            {
                self.probes += 1;
            }
            match self.links.get(id) {
                Some(next) => {
                    path.push(id.clone());
                    parent = next.clone();
                }
                None => {
                    completed = true;
                    break;
                }
            }
        }
        if !path.is_empty() && (completed || parent.is_none()) {
            let shared = shared.unwrap_or_else(|| std::sync::Arc::new(parent.clone()));
            for id in path {
                self.resolved.insert(id, shared.clone());
            }
        }
        parent
    }
}
/// A foreign-session message written as one of Claude Code's special record kinds (a compaction
/// boundary or summary, an injected-context attachment) rather than an ordinary turn.
fn special_kind(session: &Session, message: &Message) -> Option<MessageKind> {
    (session.harness != Harness::Claude
        && matches!(
            message.kind,
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary | MessageKind::InjectedContext
        ))
    .then_some(message.kind)
}

fn record<'a>(
    message: &'a Message,
    c: &Context<'a>,
    uuid: Option<&str>,
    previous: &mut Option<String>,
    dropped: &mut ParentLinks,
    resolver: &Resolver,
) -> Result<Option<Record<'a>>> {
    if let Some(carrier) = carrier_record_of(message) {
        return Ok(Some(Record {
            generated: false,
            base: Cow::Borrowed(carrier.as_object().unwrap()),
            extras: None,
            blocks: Vec::new(),
            role: message.role,
            mode: c.mode,
            special: None,
            sidecar: None,
        }));
    }
    let special = special_kind(c.session, message);
    let parent = message
        .parent_id
        .clone()
        .map(|p| dropped.resolve(p))
        .unwrap_or_else(|| previous.clone());
    if special.is_none() && message.role == Role::System {
        if let Some(id) = &message.id {
            dropped.insert(id.clone(), parent);
        }
        return Ok(None);
    }
    let uuid = uuid.context("missing reconstructed UUID")?;
    let ts = message
        .timestamp
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| c.ts.clone());
    let mut base = Map::new();
    // Special-record key order is the existing codec's literal order.
    if let Some(kind) = special {
        match kind {
            MessageKind::CompactionBoundary => {
                base.insert("type".into(), json!("system"));
                base.insert("subtype".into(), json!("compact_boundary"));
                base.insert("level".into(), json!("info"));
            }
            MessageKind::CompactionSummary => {
                base.insert("type".into(), json!("user"));
                base.insert("isCompactSummary".into(), json!(true));
            }
            _ => {
                base.insert("type".into(), json!("attachment"));
            }
        }
    }
    base.insert("uuid".into(), json!(uuid));
    base.insert("parentUuid".into(), json!(parent));
    base.insert("sessionId".into(), json!(c.session_id));
    base.insert("cwd".into(), json!(c.cwd));
    base.insert("timestamp".into(), json!(ts));
    if special.is_none() {
        if let Some(branch) = &c.branch {
            base.insert("gitBranch".into(), json!(branch));
        }
    }
    if let Some(kind) = special {
        match kind {
            MessageKind::CompactionBoundary => {
                base.insert("content".into(), Value::Null);
                base.insert("compactMetadata".into(), json!({"trigger":"manual","preTokens":0}));
            }
            MessageKind::CompactionSummary => {
                base.insert("message".into(), json!({"role":"user","content":Value::Null}));
            }
            _ => {
                base.insert("attachment".into(), json!({"type":"injected_context"}));
                base.insert("rendered".into(), Value::Null);
            }
        }
    } else {
        base.insert(
            "type".into(),
            json!(if message.role == Role::Assistant {
                "assistant"
            } else {
                "user"
            }),
        );
        let mut inner = Map::new();
        inner.insert(
            "role".into(),
            json!(if message.role == Role::Assistant {
                "assistant"
            } else {
                "user"
            }),
        );
        if message.role == Role::Assistant {
            if let Some(model) = message.model.as_ref().or(c.session.model.as_ref()) {
                inner.insert("model".into(), json!(model));
            }
            if let Some(usage) = claude_usage(message.usage.as_ref()) {
                inner.insert("usage".into(), usage);
            }
        }
        inner.insert("content".into(), Value::Null);
        base.insert("message".into(), Value::Object(inner));
    }
    let sidecar = if special.is_none() {
        message
            .content
            .iter()
            .find_map(|b| {
                if let Block::ToolResult {
                    details: Some(details), ..
                } = b
                {
                    Some(details)
                } else {
                    None
                }
            })
            .filter(|details| {
                details.as_object().is_none_or(|map| {
                    map.get(crate::harness::claude::TOOL_USE_RESULT_KEY).is_some()
                        || map.keys().any(|k| k != crate::harness::claude::PERSISTED_OUTPUT_KEY)
                })
            })
    } else {
        None
    };
    if sidecar.is_some() {
        base.insert(crate::harness::claude::TOOL_USE_RESULT_KEY.into(), Value::Null);
    }
    *previous = Some(uuid.to_owned());
    Ok(Some(Record {
        generated: true,
        base: Cow::Owned(base),
        extras: (c.session.harness == Harness::Claude && special.is_none())
            .then(|| message.harness_extra(Harness::Claude))
            .flatten(),
        blocks: projected(message, c.mode, resolver)?,
        role: if special == Some(MessageKind::CompactionSummary) {
            Role::User
        } else {
            message.role
        },
        mode: c.mode,
        special,
        sidecar,
    }))
}

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    crate::emit::validate_native_source(session, Harness::Claude, opts.thinking)?;
    let (session_id, agent) = match &opts.sidechain_of {
        Some(of) => (of.root.clone(), Some(of.agent.clone())),
        None => (opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string()), None),
    };
    let new_id = agent.clone().unwrap_or_else(|| session_id.clone());
    let cwd = effective_cwd(session, opts).map(|p| realpath_for_claude(&p));
    let c = Context {
        session,
        session_id,
        agent,
        cwd: cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        branch: branch_of(session),
        ts: session
            .created_at
            .or(session.updated_at)
            .unwrap_or_else(Utc::now)
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        mode: opts.thinking,
    };
    let resolver = session.resolver();
    let uuid: Vec<_> = session
        .messages
        .iter()
        .map(|message| {
            let special = special_kind(session, message).is_some();
            (carrier_record_of(message).is_none() && (special || message.role != Role::System))
                .then(|| message.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string()))
        })
        .collect();
    let mut answered = HashSet::new();
    let mut ids = HashSet::new();
    let (mut previous, mut dropped) = (None, ParentLinks::default());
    for (message, uuid) in session.messages.iter().zip(&uuid) {
        if let Some(record) = record(message, &c, uuid.as_deref(), &mut previous, &mut dropped, &resolver)? {
            record.with_fields(|_, message| {
                record.array_facts(message.as_ref(), None, |id, result| {
                    if result {
                        answered.insert(id.to_owned());
                    }
                });
                Ok(())
            })?;
        }
    }
    previous = None;
    dropped.clear();
    // This pass decides every record's survival once; the writing pass goes by its verdicts.
    let mut verdicts = Vec::with_capacity(session.messages.len());
    for (message, uuid) in session.messages.iter().zip(&uuid) {
        let mut survives = false;
        if let Some(record) = record(message, &c, uuid.as_deref(), &mut previous, &mut dropped, &resolver)? {
            survives = record.with_fields(|top, message| {
                let survives = record.survives(&top, message.as_ref(), &answered, c.agent.as_deref());
                if survives {
                    if let Some(id) = top.get("uuid").and_then(Value::as_str) {
                        ids.insert(id.to_owned());
                    }
                }
                Ok(survives)
            })?;
        }
        verdicts.push(survives);
    }
    let mut lines = JsonlStage::new();
    let has_title_carrier = session
        .messages
        .iter()
        .any(|m| carrier_record_of(m).is_some_and(|r| r.get("type").and_then(Value::as_str) == Some("ai-title")));
    if let Some(title) = &session.title {
        if !has_title_carrier && c.agent.is_none() {
            lines.push_with(|writer| {
                let mut o = ObjectWriter::new(writer)?;
                o.field("type", "ai-title")?;
                o.field("aiTitle", title)?;
                o.field("sessionId", &c.session_id)?;
                o.field("cwd", &c.cwd)?;
                o.field("timestamp", &c.ts)?;
                o.finish()
            })?;
        }
    }
    if session.harness != Harness::Claude {
        if let Some(prompt) = session.system_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            lines.push_with(|writer| {
                let mut o = ObjectWriter::new(writer)?;
                o.field("type", "attachment")?;
                o.field("sessionId", &c.session_id)?;
                o.field("cwd", &c.cwd)?;
                o.field("timestamp", &c.ts)?;
                o.field_with("attachment", |writer| {
                    let mut a = ObjectWriter::new(writer)?;
                    a.field("type", "prompt_snapshot")?;
                    a.field_with("systemPrompt", |writer| {
                        writer.write_all(b"[")?;
                        serde_json::to_writer(&mut *writer, prompt)?;
                        writer.write_all(b"]")?;
                        Ok(())
                    })?;
                    a.finish()
                })?;
                if let Some(agent) = &c.agent {
                    o.field("isSidechain", true)?;
                    o.field("agentId", agent)?;
                }
                o.finish()
            })?;
        }
    }
    previous = None;
    dropped.clear();
    let mut surviving_previous = None;
    for ((message, uuid), survives) in session.messages.iter().zip(&uuid).zip(&verdicts) {
        if let Some(record) = record(message, &c, uuid.as_deref(), &mut previous, &mut dropped, &resolver)? {
            if !survives {
                continue;
            }
            let mut emitted_uuid = None;
            lines.push_with(|writer| {
                emitted_uuid = record.write(writer, &c, &answered, &ids, surviving_previous.as_deref(), &resolver)?;
                Ok(())
            })?;
            if emitted_uuid.is_some() {
                surviving_previous = emitted_uuid;
            }
        }
    }
    let file_path = match &c.agent {
        Some(agent) => {
            fs::create_dir_all(out_dir)?;
            out_dir.join(format!("agent-{agent}.jsonl"))
        }
        None => {
            let in_place_project = (opts.new_cwd.is_none() && session.harness == Harness::Claude)
                .then_some(session.source_path.as_deref())
                .flatten()
                .and_then(|src| {
                    src.ancestors()
                        .skip(1)
                        .find(|dir| dir.parent() == Some(out_dir))
                        .map(Path::to_path_buf)
                });
            let project = in_place_project
                .unwrap_or_else(|| out_dir.join(cwd.as_deref().map(claude_encode_cwd).unwrap_or_else(|| "-".into())));
            fs::create_dir_all(&project)?;
            project.join(format!("{new_id}.jsonl"))
        }
    };
    // The three rules hold before the file is published: a violation is a bug in cv, and it
    // leaves nothing behind.
    let check = |staged: &Path| super::verify::verify_as(staged, &file_path, c.agent.as_deref());
    if opts.thinking != ThinkingMode::Native {
        let source = lines.into_source()?;
        let (stage, _) = crate::harness::claude::replay::normalize(source, opts.thinking)?;
        crate::harness::store::publish_stage_checked(stage, &file_path, opts.claim(), check)?;
    } else {
        lines.publish_checked(&file_path, opts.claim(), check)?;
    }
    let resume = match (&c.agent, &cwd) {
        (Some(agent), _) => format!(
            "from session {}: SendMessage to=\"{agent}\" with the next instruction",
            c.session_id
        ),
        (None, cwd) => crate::harness::resume_in(
            cwd.as_deref(),
            format!("claude --resume {}", crate::harness::shell_quote(&new_id)),
        ),
    };
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some(resume),
    })
}

#[cfg(test)]
mod parent_tests {
    use super::*;
    fn old_resolve(dropped: &HashMap<String, Option<String>>, pid: String) -> (Option<String>, usize) {
        let mut p = Some(pid);
        let mut probes = 0;
        for _ in 0..=dropped.len() {
            probes += 1;
            match p.as_ref().and_then(|id| dropped.get(id)) {
                Some(next) => p = next.clone(),
                None => break,
            }
        }
        (p, probes)
    }
    #[test]
    fn forward_dropped_ancestors_resolve_with_linear_probes_and_exact_results() {
        for count in [64, 1024, 4096] {
            let mut parents = ParentLinks::default();
            for n in 0..count {
                parents.insert(
                    format!("d{n}"),
                    Some(if n + 1 == count {
                        "root".into()
                    } else {
                        format!("d{}", n + 1)
                    }),
                );
            }
            let mut old_probes = 0;
            for n in 0..count {
                let id = format!("d{n}");
                let (expected, probes) = old_resolve(&parents.links, id.clone());
                old_probes += probes;
                assert_eq!(parents.resolve(id), expected);
            }
            assert!(old_probes >= count * count / 2);
            assert!(parents.probes <= count + 1);
            // Resolving a previously absent parent then inserting it must not reuse stale data.
            parents.insert("root".into(), None);
            assert_eq!(parents.resolve("d0".into()), None);
        }
    }
    #[test]
    fn parent_cache_shares_wide_resolved_endpoints_across_the_path() {
        let mut parents = ParentLinks::default();
        let wide = "root".repeat(100000);
        for n in 0..1024 {
            parents.insert(
                format!("d{n}"),
                Some(if n == 1023 { wide.clone() } else { format!("d{}", n + 1) }),
            );
        }
        assert_eq!(parents.resolve("d0".into()), Some(wide));
        let endpoint = parents.resolved.get("d0").unwrap();
        assert!(parents
            .resolved
            .values()
            .all(|value| std::sync::Arc::ptr_eq(value, endpoint)));
    }

    #[test]
    fn emitted_forward_parent_fanout_matches_owned_native_oracle() -> Result<()> {
        let world = crate::test_world("claude-parent-fanout");
        let mut session = crate::Session::new("original", Harness::Claude);
        session.created_at = chrono::DateTime::from_timestamp(1, 0);
        let mut root = Message::new(Role::User);
        root.id = Some("root".into());
        root.content.push(Block::Text {
            text: "root prompt".into(),
        });
        session.messages.push(root);
        for n in 0..128 {
            let mut dropped = Message::new(Role::System);
            dropped.id = Some(format!("d{n}"));
            dropped.parent_id = Some(if n == 127 { "root".into() } else { format!("d{}", n + 1) });
            session.messages.push(dropped);
        }
        for n in 0..128 {
            let mut child = Message::new(Role::User);
            child.id = Some(format!("child{n}"));
            child.parent_id = Some(format!("d{n}"));
            child.content.push(Block::Text {
                text: format!("child {n}").into(),
            });
            session.messages.push(child);
        }
        let opts = EmitOptions {
            new_id: Some("destination".into()),
            ..Default::default()
        };
        let new = emit(&session, &world.base.join("new"), &opts)?;
        let bytes = fs::read(&new.path)?;
        let mut frozen = crate::emit::frozen::Frozen::new("claude-parent-fanout");
        frozen.insert("destination", json!([bytes.len(), crate::digest::sha256_hex(&bytes)]));
        frozen.check()?;
        super::super::verify_claude_resumable(&new.path, None)?;
        Ok(())
    }

    #[test]
    fn dropped_parent_cache_preserves_cycles_and_duplicate_updates() {
        let mut parents = ParentLinks::default();
        for (id, parent) in [("a", "b"), ("b", "a"), ("c", "a")] {
            parents.insert(id.into(), Some(parent.into()));
        }
        for _ in 0..3 {
            for id in ["a", "b", "c", "unknown"] {
                assert_eq!(parents.resolve(id.into()), old_resolve(&parents.links, id.into()).0);
            }
        }
        parents.insert("b".into(), Some("root".into()));
        assert_eq!(parents.resolve("c".into()), Some("root".into()));
        parents.insert("b".into(), None);
        assert_eq!(parents.resolve("c".into()), None);
        parents.clear();
        assert_eq!(parents.resolve("a".into()), Some("a".into()));
    }
}
