//! The Devin cell codec: one validated `message_nodes` cell view in, navigation facts or one IR
//! message out. Every read (discovery, the forest, the stream, every demand) hands these
//! functions a [`JsonView`] of a cell from [`Cells`](crate::harness::store::sqlite::Cells); the
//! demand (`spans`) decides whether long text and arguments stay in the cell's bytes, never which
//! code runs.
use super::*;
use crate::json::JsonView;

pub(super) fn string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    view.map(|view| view.string()).transpose().map(Option::flatten)
}
fn boolean(view: Option<JsonView<'_>>) -> Result<Option<bool>> {
    view.map(JsonView::as_bool).transpose().map(Option::flatten)
}
fn number(view: Option<JsonView<'_>>) -> Result<Option<serde_json::Number>> {
    view.map(JsonView::as_number).transpose().map(Option::flatten)
}
/// `view[key]` of an object, None for a missing key or a nonobject.
fn get<'a>(view: Option<JsonView<'a>>, key: &str) -> Result<Option<JsonView<'a>>> {
    view.map(|v| v.get(key)).transpose().map(Option::flatten)
}
/// A present, non-null value, owned.
fn present(view: Option<JsonView<'_>>) -> Result<Option<Value>> {
    match view {
        Some(v) if !v.is_null()? => Ok(Some(v.resolve()?.into_owned())),
        _ => Ok(None),
    }
}

#[derive(Default)]
pub(super) struct Navigation {
    pub role: Option<String>,
    pub is_turn: bool,
    pub agent_id: Option<String>,
    pub chain_node_id: Option<i64>,
    pub tool_call_id: Option<String>,
    pub model: Option<String>,
}
/// The facts the forest keeps per node: role, turn, sub-agent receipt and model. Body and
/// telemetry are skipped by the view, never decoded.
pub(super) fn navigation(view: JsonView<'_>) -> Result<Navigation> {
    let mut facts = listing(view)?;
    match facts.role.as_deref() {
        Some("tool") => facts.tool_call_id = string(view.get("tool_call_id")?)?,
        Some("assistant") => facts.model = string(get(view.get("metadata")?, "generation_model")?)?,
        _ => {}
    }
    Ok(facts)
}

/// The turn and child-chain markers needed to list the session.
pub(super) fn listing(view: JsonView<'_>) -> Result<Navigation> {
    let mut role = String::new();
    let mut too_long = false;
    if let Some(value) = view.get("role")? {
        value.for_each_str_chunk(|chunk| {
            if role.len() + chunk.len() <= "assistant".len() && !too_long {
                role.push_str(chunk);
            } else {
                too_long = true;
            }
            Ok(())
        })?;
    }
    if too_long || !matches!(role.as_str(), "user" | "assistant" | "system" | "tool") {
        return Ok(Navigation::default());
    }
    let metadata = view.get("metadata")?;
    let extensions = metadata.and_then(|v| v.get("extensions").transpose()).transpose()?;
    Ok(Navigation {
        agent_id: if role == "tool" {
            string(
                extensions
                    .and_then(|v| v.get("subagent/agent_id").transpose())
                    .transpose()?,
            )?
        } else {
            None
        },
        chain_node_id: if role == "tool" {
            extensions
                .and_then(|v| v.get("subagent/chain_node_id").transpose())
                .transpose()?
                .and_then(|v| v.as_number().transpose())
                .transpose()?
                .and_then(|n| n.as_i64())
        } else {
            None
        },
        is_turn: role == "assistant"
            || (role == "user"
                && metadata
                    .and_then(|v| v.get("is_user_input").transpose())
                    .transpose()?
                    .and_then(|v| v.as_bool().transpose())
                    .transpose()?
                    .unwrap_or(false)),
        role: Some(role),
        ..Navigation::default()
    })
}

/// The node `metadata` column's navigation facts: fixed size per node, whatever the cell holds.
/// None for a nonobject.
pub(super) fn facts(view: JsonView<'_>) -> Result<Option<NodeFacts>> {
    if !view.is_object()? {
        return Ok(None);
    }
    let summarized_from = view.get("summarized_from")?;
    Ok(Some(NodeFacts {
        is_summary: summarized_from
            .map(|v| v.is_null().map(|null| !null))
            .transpose()?
            .unwrap_or(false),
        summarized_from: number(summarized_from)?.and_then(|n| n.as_i64()),
        num_tokens_preceding: number(view.get("num_tokens_preceding")?)?.and_then(|n| n.as_u64()),
        is_system_prefix: boolean(view.get("is_system_prefix")?)?.unwrap_or(false),
    }))
}

/// The node `metadata` values a message carries in its bag, in order.
pub(super) fn carried(view: JsonView<'_>) -> Result<Vec<(&'static str, Value)>> {
    let mut out = Vec::new();
    for (key, value) in [
        ("num_tokens_preceding", view.get("num_tokens_preceding")?),
        ("summarized_from", view.get("summarized_from")?),
        (
            "prior_node_ids",
            get(view.get("extensions")?, "compact/prior_node_ids")?,
        ),
    ] {
        if let Some(value) = present(value)? {
            out.push((key, value));
        }
    }
    Ok(out)
}

/// Text by the codec's span rule; an empty string is no text.
fn nonempty(view: Option<JsonView<'_>>, spans: bool) -> Result<Option<crate::Text>> {
    let Some(view) = view else { return Ok(None) };
    Ok(view
        .lazy_text(spans)?
        .filter(|text| !matches!(text, crate::Text::Inline(s) if s.is_empty())))
}

/// One chain node's `chat_message` as one message. `facts` is the node's metadata facts and
/// `carried` the metadata values its bag keeps; `tools` pairs results with their calls.
pub(super) fn node(
    chat: JsonView<'_>,
    nid: i64,
    created: i64,
    facts: Option<&NodeFacts>,
    carried: Vec<(&'static str, Value)>,
    tools: &mut HashMap<String, String>,
    spans: bool,
) -> Result<Message> {
    let role = string(chat.get("role")?)?.unwrap_or_default();
    // The message's own metadata object (inside `chat_message`, distinct from the node column).
    let mmeta = chat.get("metadata")?;
    let ext = get(mmeta, "extensions")?;
    let is_prefix = facts.is_some_and(|n| n.is_system_prefix);
    let is_user_input = boolean(get(mmeta, "is_user_input")?)?.unwrap_or(false);
    // The lead's brief to a sub-agent is a `user` prompt typed by another agent.
    let is_handoff = boolean(get(ext, "subagent/handoff")?)?.unwrap_or(false);

    let mut m = match (role.as_str(), is_prefix, is_user_input) {
        ("system", true, _) => Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness),
        ("system", false, _) => Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness),
        ("user", _, true) if is_handoff => Message::of_kind(Role::User, MessageKind::Prompt, Origin::Subagent),
        ("user", _, true) => Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human),
        // A `user` row the human didn't type is harness-injected context (same modeling Goose
        // uses for `userVisible: false` rows).
        ("user", _, false) => Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness),
        ("assistant", _, _) => Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model),
        ("tool", _, _) => Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness),
        _ => Message::of_kind(Role::System, MessageKind::Notice, Origin::Harness),
    };
    // Compaction marks the node, whatever the role it copied into (observed live: system nodes
    // carrying `subagent/handoff_history`/`compact/*`/`devin-rs/summary` extension sidecars).
    if facts.is_some_and(|n| n.is_summary) {
        m.kind = MessageKind::CompactionSummary;
    }

    m.id = string(chat.get("message_id")?)?;
    m.timestamp = string(get(mmeta, "created_at")?)?
        .as_deref()
        .and_then(super::super::parse_ts)
        .or_else(|| ts_from_epoch(created));

    let content = nonempty(chat.get("content")?, spans)?;
    match role.as_str() {
        "assistant" => {
            if let Some(th) = chat.get("thinking")? {
                let text = nonempty(th.get("thinking")?, spans)?;
                let signature = string(th.get("signature")?)?;
                if text.is_some() || signature.is_some() {
                    m.content.push(Block::Thinking {
                        text: text.unwrap_or_default(),
                        signature,
                        encrypted: None,
                        redacted: false,
                        extensions: Default::default(),
                    });
                }
            }
            if let Some(text) = content {
                m.content.push(Block::from_text(text));
            }
            if let Some(calls) = chat.get("tool_calls")?.map(JsonView::array).transpose()?.flatten() {
                for c in calls {
                    let c = c?;
                    let id = string(c.get("id")?)?.unwrap_or_default();
                    let name = string(c.get("name")?)?.unwrap_or_default();
                    let input = match c.get("arguments")? {
                        Some(input) => input.argument(spans)?,
                        None => Value::Null.into(),
                    };
                    if !id.is_empty() && !name.is_empty() {
                        tools.insert(id.clone(), name.clone());
                    }
                    m.content.push(Block::ToolUse {
                        id,
                        name,
                        input,
                        namespace: None,
                        extensions: Default::default(),
                    });
                }
            }
            m.model = string(get(mmeta, "generation_model")?)?.filter(|s| !s.is_empty());
            if let Some(metrics) = get(mmeta, "metrics")? {
                let get = |k: &str| -> Result<Option<u64>> { Ok(number(metrics.get(k)?)?.and_then(|n| n.as_u64())) };
                let u = Usage {
                    input_tokens: get("input_tokens")?,
                    output_tokens: get("output_tokens")?,
                    cache_read_tokens: get("cache_read_tokens")?,
                    cache_creation_tokens: get("cache_creation_tokens")?,
                    ..Default::default()
                };
                if !u.is_empty() {
                    m.usage = Some(u);
                }
            }
        }
        "tool" => {
            let tool_use_id = string(chat.get("tool_call_id")?)?.unwrap_or_default();
            let meta = get(ext, "chisel/tool_result_meta")?;
            let success = boolean(get(meta, "success")?)?;
            // `chisel/tool_failure` is a failure even without a tool_result_meta verdict.
            let failure_reason = string(get(get(ext, "chisel/tool_failure")?, "reason")?)?;
            let mut details = serde_json::Map::new();
            if let Some(r) = &failure_reason {
                details.insert("failure_reason".into(), json!(r));
            }
            if let Some(term) = get(ext, "chisel/terminal_output")? {
                if let Some(cwd) = term.get("cwd")? {
                    details.insert("cwd".into(), cwd.resolve()?.into_owned());
                }
                let exit = term.get("exit")?;
                for key in ["exit_code", "terminal_id"] {
                    if let Some(v) = get(exit, key)? {
                        details.insert(key.into(), v.resolve()?.into_owned());
                    }
                }
            }
            if let Some(d) = get(get(ext, "chisel/tool_call_timing")?, "duration_ms")? {
                details.insert("duration_ms".into(), d.resolve()?.into_owned());
            }
            if let Some(k) = get(meta, "kind")? {
                details.insert("kind".into(), k.resolve()?.into_owned());
            }
            // `chisel/undo` names what `/revert` can put back: a `restore_file` entry is a file
            // checkpoint (its `original_content` stays in the store); `irreversible` is none.
            let mut restorable = Vec::new();
            if let Some(undo) = get(ext, "chisel/undo")?.map(JsonView::array).transpose()?.flatten() {
                for u in undo {
                    let u = u?;
                    if string(u.get("kind")?)?.as_deref() == Some("restore_file") {
                        if let Some(path) = u.get("path")? {
                            restorable.push(path.resolve()?.into_owned());
                        }
                    }
                }
            }
            if !restorable.is_empty() {
                details.insert("restorable".into(), Value::Array(restorable));
            }
            m.content.push(Block::ToolResult {
                tool_name: tools.get(&tool_use_id).cloned(),
                tool_use_id,
                content: content.unwrap_or_default(),
                is_error: success == Some(false) || failure_reason.is_some(),
                status: if failure_reason.is_some() {
                    Some("error".to_string())
                } else {
                    success.map(|ok| if ok { "completed" } else { "error" }.to_string())
                },
                details: (!details.is_empty()).then_some(Value::Object(details)),
                extensions: Default::default(),
            });
        }
        _ => {
            if let Some(text) = content {
                m.content.push(Block::from_text(text));
            }
        }
    }

    // Harness facts that have no first-class IR home: the node's position in the forest and the
    // metadata both layers record. Only present keys are written.
    let bag = m.harness_extra_mut(Harness::Devin);
    bag.insert("node_id".into(), json!(nid));
    for k in ["finish_reason", "request_id", "telemetry", "num_tokens"] {
        if let Some(v) = present(get(mmeta, k)?)? {
            bag.insert(k.into(), v);
        }
    }
    for (k, v) in carried {
        bag.insert(k.into(), v);
    }
    for (k, dst) in [
        ("affogato/cog-context", "cog_context"),
        // The tool row that answers an in-band question dialog (and `cv show --kind answer` reads).
        ("chisel/user_question_answers", "user_question_answers"),
    ] {
        if let Some(v) = present(get(ext, k)?)? {
            bag.insert(dst.into(), v);
        }
    }
    // Which sub-agent a completion row is about: the parent's `sidekick` tool result and the
    // `<subagent_completion_notification>` system row both carry these.
    let mut sub = serde_json::Map::new();
    for (k, dst) in [
        ("subagent/profile_name", "profile_name"),
        ("subagent/model", "model"),
        ("subagent/agent_id", "agent_id"),
        ("subagent/chain_node_id", "chain_node_id"),
    ] {
        if let Some(v) = present(get(ext, k)?)? {
            sub.insert(dst.into(), v);
        }
    }
    if !sub.is_empty() {
        bag.insert("subagent".into(), Value::Object(sub));
    }
    // Sub-agent wiring: the lead's model uid on a handoff prompt, and the rules/skills the
    // child's system prefix says it loaded (agent-ext/*).
    for (k, dst) in [
        ("chisel/fusion_lead_model_uid", "lead_model"),
        ("agent-ext/rules-loaded", "rules_loaded"),
    ] {
        if let Some(v) = present(get(ext, k)?)? {
            bag.insert(dst.into(), v);
        }
    }
    if let Some(skills) = get(get(ext, "agent-ext/skills-loaded")?, "skills")?
        .map(JsonView::array)
        .transpose()?
        .flatten()
    {
        let mut reduced = Vec::new();
        for s in skills {
            reduced.push(Value::Object(pick(s?, &["name", "path"])?));
        }
        bag.insert("skills_loaded".into(), Value::Array(reduced));
    }
    // The ACP view of the calls, reduced to title/kind/status — `content`/`rawInput`/`_meta`
    // duplicate the ToolUse block and the result row.
    if let Some(tcc) = get(ext, "chisel/tool_call_content")? {
        let mut reduced = serde_json::Map::new();
        if tcc.for_each_entry(|id, v| {
            reduced.insert(id.into(), Value::Object(pick(v, &["title", "kind", "status"])?));
            Ok(())
        })? {
            bag.insert("tool_call_content".into(), Value::Object(reduced));
        }
    }
    Ok(m)
}

/// The present `keys` of an object, in `keys` order (nothing for a nonobject).
fn pick(view: JsonView<'_>, keys: &[&str]) -> Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    for k in keys {
        if let Some(v) = view.get(k)? {
            out.insert((*k).into(), v.resolve()?.into_owned());
        }
    }
    Ok(out)
}

pub(super) fn compaction(view: JsonView<'_>) -> Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    if let Some(ext) = view
        .get("metadata")?
        .and_then(|v| v.get("extensions").transpose())
        .transpose()?
    {
        if let Some(src) = ext
            .get("devin-rs/summary")?
            .and_then(|v| v.get("source").transpose())
            .transpose()?
        {
            if !src.is_null()? {
                out.insert("source".into(), src.resolve()?.into_owned());
            }
        }
        if let Some(paths) = ext
            .get("compact/edited_files")?
            .and_then(|v| v.get("paths").transpose())
            .transpose()?
        {
            out.insert("edited_files".into(), paths.resolve()?.into_owned());
        }
        if let Some(todos) = ext.get("compact/todo_list")? {
            if !todos.is_null()? {
                out.insert("todo_list".into(), todos.resolve()?.into_owned());
            }
        }
    }
    if let Some(content) = view.get("content")? {
        let mut scan = HistoryPath::default();
        let scanned = content.for_each_str_chunk(|chunk| {
            scan.push(chunk);
            if scan.ended {
                return Err(crate::stream::Stop.into());
            }
            Ok(())
        });
        if let Err(error) = scanned {
            if error.downcast_ref::<crate::stream::Stop>().is_none() {
                return Err(error);
            }
        }
        // The enclosing checked cell read revalidates original source/backing guards,
        // including after this local successful cutoff. Whole-cell acceptance ran first.
        if let Some(path) = scan.finish() {
            out.insert("history_path".into(), json!(path));
        }
    }
    Ok(out)
}

#[derive(Default)]
struct HistoryPath {
    matched: usize,
    found: bool,
    ended: bool,
    path: String,
    prefix: String,
    decided: bool,
}
impl HistoryPath {
    fn word(&mut self, ch: char) {
        if ch.is_whitespace() {
            if !self.path.is_empty() {
                self.ended = true;
            }
        } else {
            self.path.push(ch);
        }
    }
    fn push(&mut self, chunk: &str) {
        const NEEDLE: &str = "Full conversation history saved at ";
        for ch in chunk.chars() {
            if self.ended {
                break;
            }
            if !self.found {
                if ch == NEEDLE.as_bytes()[self.matched] as char {
                    self.matched += 1;
                    if self.matched == NEEDLE.len() {
                        self.found = true;
                    }
                } else {
                    self.matched = usize::from(ch == 'F');
                }
            } else if !self.decided {
                if self.prefix.is_empty() && ch.is_whitespace() {
                    continue;
                }
                self.prefix.push(ch);
                // split(NEEDLE).nth(1) ends before a second delimiter. Disambiguate that
                // bounded prefix before accepting "Full" as the first path token.
                if NEEDLE.starts_with(&self.prefix) {
                    if self.prefix == NEEDLE {
                        self.ended = true;
                    }
                } else {
                    self.decided = true;
                    for c in std::mem::take(&mut self.prefix).chars() {
                        if self.ended {
                            break;
                        }
                        self.word(c);
                    }
                }
            } else {
                self.word(ch);
            }
        }
    }
    fn finish(self) -> Option<String> {
        let token = if !self.decided && !self.ended {
            self.prefix.split_whitespace().next().unwrap_or("")
        } else {
            &self.path
        };
        let path = token.trim_end_matches('.');
        (!path.is_empty()).then(|| path.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_only_usage_and_signature_only_thinking_survive() -> Result<()> {
        let value = json!({"role":"assistant","thinking":{"signature":"provider signature"},
            "metadata":{"metrics":{"cache_read_tokens":17,"cache_creation_tokens":0}}});
        let m = node((&value).into(), 1, 1000, None, Vec::new(), &mut HashMap::new(), false)?;
        let usage = m.usage.as_ref().unwrap();
        assert_eq!(usage.cache_read_tokens, Some(17));
        assert_eq!(usage.cache_creation_tokens, Some(0));
        assert_eq!(usage.prompt_tokens(), 17);
        assert!(matches!(&m.content[0], Block::Thinking { text, signature, .. }
            if text.is_empty() && signature.as_deref() == Some("provider signature")));
        Ok(())
    }

    #[test]
    fn tool_rows_reduce_their_extensions() -> Result<()> {
        let body = "λ\nquoted \"body\"".repeat(1000);
        let value = json!({"role":"tool","tool_call_id":"call","content":"out","metadata":{"extensions":{
            "chisel/terminal_output":{"cwd":"/cwd","stdout":body,"exit":{"exit_code":1,"terminal_id":"term","x":body}},
            "chisel/tool_result_meta":{"success":false,"kind":"shell"},
            "chisel/undo":[{"kind":"restore_file","path":"/file","original_content":body},{"kind":"irreversible"}],
            "agent-ext/skills-loaded":{"skills":[{"name":"n","path":"p","description":body}]},
            "chisel/tool_call_content":{"call":{"title":"t","kind":"k","status":"s","rawInput":body}},
            "subagent/agent_id":"sidekick","subagent/handoff_history":body}}});
        let mut tools = HashMap::from([("call".into(), "Read".into())]);
        let m = node((&value).into(), 7, 1000, None, Vec::new(), &mut tools, false)?;
        let Block::ToolResult {
            tool_name,
            is_error,
            status,
            details,
            ..
        } = &m.content[0]
        else {
            panic!("tool result")
        };
        assert_eq!(tool_name.as_deref(), Some("Read"));
        assert!(*is_error);
        assert_eq!(status.as_deref(), Some("error"));
        assert_eq!(
            details.as_ref().unwrap(),
            &json!({"cwd":"/cwd","exit_code":1,"terminal_id":"term","kind":"shell","restorable":["/file"]})
        );
        let bag = m.harness_extra(Harness::Devin).unwrap();
        assert_eq!(bag["skills_loaded"], json!([{"name":"n","path":"p"}]));
        assert_eq!(
            bag["tool_call_content"],
            json!({"call":{"title":"t","kind":"k","status":"s"}})
        );
        assert_eq!(bag["subagent"], json!({"agent_id":"sidekick"}));
        Ok(())
    }

    #[test]
    fn history_path_scan_matches_the_split_rule() -> Result<()> {
        for content in [
            "Full conversation history saved at /a/b.md.",
            "λ Full conversation history saved at \n\t /a/b.md... tail",
            "no history",
            "Full conversation history saved at ",
            "Full Full conversation history saved at /file.",
            "Full conversation history saved at Full conversation history saved at /second",
            "Full conversation history saved at Full convo",
        ] {
            let expected = content
                .split("Full conversation history saved at ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .map(|s| s.trim_end_matches('.'))
                .filter(|s| !s.is_empty());
            let value = json!({"content":content,"metadata":{"extensions":{"devin-rs/summary":{"source":"async"}}}});
            let facts = compaction((&value).into())?;
            assert_eq!(facts.get("history_path").and_then(Value::as_str), expected);
            assert_eq!(facts["source"], json!("async"));
            for size in [1, 7, 31, 64] {
                let mut scan = HistoryPath::default();
                for part in content.as_bytes().chunks(size) {
                    if let Ok(part) = std::str::from_utf8(part) {
                        scan.push(part)
                    }
                }
                assert_eq!(scan.finish().as_deref(), expected);
            }
        }
        Ok(())
    }
}
