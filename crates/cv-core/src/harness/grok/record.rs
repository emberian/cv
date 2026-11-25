//! The Grok record codec: one `chat_history.jsonl` record view in, at most one IR message out.
//! Every byte source (mapped lines, positioned records, an owned value) feeds this one function;
//! the demand only chooses whether large strings stay in the source (`spans`), never which code
//! runs.

use super::*;
use crate::harness::store::document::Fields;
use crate::json::{JsonSource, JsonView};

/// Every top-level key any record kind reads.
const TOP: &[&str] = &[
    "type",
    "tool_call_id",
    "content",
    "summary",
    "encrypted_content",
    "reasoning",
    "model_id",
    "tool_calls",
    "model_fingerprint",
    "synthetic_reason",
];

#[derive(Clone, Copy)]
enum Kind {
    Turn(Role),
    ToolResult,
    Reasoning,
}

/// The shared record projection and classification. Demands choose what to resolve from it;
/// neither discovery nor message decoding has its own wire grammar.
struct Record<'a> {
    top: Fields<'a>,
    kind: Kind,
}

impl<'a> Record<'a> {
    fn parse(view: JsonView<'a>) -> Result<Option<Self>> {
        let Some(top) = Fields::parse(view, TOP)? else {
            return Ok(None);
        };
        let kind = match top.string("type")?.as_deref() {
            Some("system") => Kind::Turn(Role::System),
            Some("user") => Kind::Turn(Role::User),
            Some("assistant") => Kind::Turn(Role::Assistant),
            Some("tool_result") => Kind::ToolResult,
            Some("reasoning") => Kind::Reasoning,
            _ => return Ok(None),
        };
        Ok(Some(Self { top, kind }))
    }

    fn title(&self) -> Result<Option<String>> {
        if !matches!(self.kind, Kind::Turn(Role::User)) {
            return Ok(None);
        }
        super::super::wire::text_title(self.top.get("content"))
    }
}

/// Discovery demand: title text only, without reasoning, arguments or tool outputs.
pub(super) fn discovery_title(view: JsonView<'_>) -> Result<Option<String>> {
    Record::parse(view)?
        .map(|record| record.title())
        .transpose()
        .map(Option::flatten)
}

/// One record: whether it is a standalone `reasoning` record (which joins the next turn), and
/// its message. `spans` is the captured source when the demand keeps large strings in it.
pub(super) fn message(
    view: JsonView<'_>,
    spans: Option<&JsonSource>,
    enrich: &HashMap<String, ToolEnrich>,
    resolver: &crate::Resolver,
) -> Result<(bool, Option<Message>)> {
    let Some(Record { top, kind }) = Record::parse(view)? else {
        return Ok((false, None));
    };
    let text = |v: JsonView<'_>| leaf_text(v, spans);

    // tool_result lines become a distinct Tool-role message so conversions re-encode them correctly.
    if matches!(kind, Kind::ToolResult) {
        let tool_use_id = top.string("tool_call_id")?.unwrap_or_default();
        let content = join(top.get("content"), spans)?;
        let e = enrich.get(&tool_use_id);
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id,
            content,
            is_error: e.is_some_and(ToolEnrich::is_error),
            tool_name: e.and_then(|e| e.title.clone()),
            status: e.and_then(|e| e.status.clone()),
            details: None,
            extensions: Default::default(),
        });
        return Ok((false, Some(m)));
    }

    // 1.0.5x writes reasoning as its own record before the assistant turn it belongs to:
    // `{type: reasoning, id, summary: [{type: summary_text, text}], encrypted_content?}`. Older
    // versions nest it in the assistant line (`reasoning{text,encrypted}`, below).
    if matches!(kind, Kind::Reasoning) {
        let summary = top.get("summary").filter(|v| v.is_array().unwrap_or(false));
        let text = join(summary, spans)?;
        let encrypted = top.string("encrypted_content")?;
        if text.is_empty() && encrypted.is_none() {
            return Ok((true, None));
        }
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::Thinking {
            text,
            signature: None,
            encrypted,
            redacted: false,
            extensions: Default::default(),
        });
        return Ok((true, Some(m)));
    }

    let Kind::Turn(role) = kind else {
        anyhow::bail!("Grok record is not a turn");
    };
    let mut m = Message::new(role);
    let content = join(top.get("content"), spans)?;
    let synthetic = top.string("synthetic_reason")?;
    // kind/origin from Grok's own turn semantics. `assistant` keeps the role-implied Reply/Model.
    match role {
        // The system prompt (also `system_prompt.txt`, mirrored into `Session::system_prompt`).
        Role::System => m.kind = MessageKind::SystemPrompt,
        Role::User => (m.kind, m.origin) = user_kind_origin(synthetic.as_deref(), &content, resolver)?,
        _ => {}
    }

    // assistant lines may carry reasoning (summarized text + opaque encrypted blob).
    if let Some(reasoning) = top.get("reasoning") {
        let fields = Fields::parse(reasoning, &["text", "encrypted"])?;
        let body = match fields.as_ref().and_then(|f| f.get("text")) {
            Some(v) => text(v)?,
            None => None,
        }
        .unwrap_or_default();
        let encrypted = fields.as_ref().map(|f| f.string("encrypted")).transpose()?.flatten();
        if !body.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text: body,
                signature: None,
                encrypted,
                redacted: false,
                extensions: Default::default(),
            });
        }
    }
    m.model = top.string("model_id")?;
    if !content.is_empty() {
        m.content.push(Block::from_text(content));
    }

    // assistant tool_calls → ToolUse blocks (arguments is a JSON *string*).
    if let Some(calls) = top.get("tool_calls").map(|v| v.array()).transpose()?.flatten() {
        for call in calls {
            let block = openai::tool_call_from_view(call?, spans.is_some())?;
            let Block::ToolUse { id, .. } = &block else {
                anyhow::bail!("Grok tool call did not decode as a tool use");
            };
            // Grok-specific sidecar title/kind/locations belong in the harness bag;
            // preserve them there without inventing shared IR fields.
            if let Some(e) = enrich.get(id) {
                let mut info = serde_json::Map::new();
                if let Some(t) = &e.title {
                    info.insert("title".into(), Value::String(t.clone()));
                }
                if let Some(k) = &e.kind {
                    info.insert("kind".into(), Value::String(k.clone()));
                }
                if !e.locations.is_empty() {
                    info.insert(
                        "locations".into(),
                        Value::Array(e.locations.iter().cloned().map(Value::String).collect()),
                    );
                }
                if !info.is_empty() {
                    let bag = m.harness_extra_mut(Harness::Grok);
                    if let Value::Object(obj) = bag
                        .entry("tool_calls")
                        .or_insert_with(|| Value::Object(serde_json::Map::new()))
                    {
                        obj.insert(id.clone(), Value::Object(info));
                    }
                }
            }
            m.content.push(block);
        }
    }

    // Preserve a couple of high-signal scalars for round-tripping (the injected-context flavor —
    // `system_reminder` / `project_instructions` / `task_completed` — is already on `kind`/`origin`,
    // but the exact `synthetic_reason` is kept so the distinction survives).
    let fingerprint = top.string("model_fingerprint")?;
    if fingerprint.is_some() || synthetic.is_some() {
        let bag = m.harness_extra_mut(Harness::Grok);
        if let Some(fp) = fingerprint {
            bag.insert("model_fingerprint".into(), Value::String(fp));
        }
        if let Some(sr) = synthetic {
            bag.insert("synthetic_reason".into(), Value::String(sr));
        }
    }

    if m.content.is_empty() {
        return Ok((false, None));
    }
    Ok((false, Some(m)))
}

/// The demand's leaf rule: under spans a string stays checked source text, else it is owned text
/// (None for a nonstring). Every string leaf of a record goes through this one match.
fn leaf_text(v: JsonView<'_>, spans: Option<&JsonSource>) -> Result<Option<Text>> {
    if spans.is_some() {
        v.source_text()
    } else {
        v.text(false)
    }
}

/// A `content` (or reasoning `summary`) field as transcript text: a string itself, or the string
/// `text` of each part joined by newlines. Under spans, one large string stays in the source and
/// several parts are joined into the source's private decoded backing, never owned.
fn join(view: Option<JsonView<'_>>, spans: Option<&JsonSource>) -> Result<Text> {
    use super::super::wire;
    let backing = spans.map_or(wire::Backing::Inline, wire::Backing::Source);
    wire::joined(view, wire::TEXT, backing, &mut |view| leaf_text(view, spans))
}

/// kind/origin for a Grok `user` record from its own semantics. A real typed prompt is
/// Prompt/Human; the harness injects several kinds of context as their own `user` turns —
/// `<system-reminder>` skill/rule blocks and background-task notices (flagged by
/// `synthetic_reason`) and the unflagged `<user_info>` environment block — all InjectedContext /
/// Harness. A `compaction_meta` turn is the summary that seeds the post-compaction window.
fn user_kind_origin(
    synthetic: Option<&str>,
    content: &Text,
    resolver: &crate::Resolver,
) -> Result<(MessageKind, Origin)> {
    let user_info = || -> Result<bool> {
        const USER_INFO: &str = "<user_info>";
        let (head, _) = document::text_head(content, resolver, USER_INFO.chars().count(), true)?;
        Ok(head == USER_INFO)
    };
    Ok(match synthetic {
        // After a compaction 1.0.5x re-tags the leading `<user_info>` block `compaction_meta` too;
        // only the summary itself is the compaction summary.
        Some("compaction_meta") if !user_info()? => (MessageKind::CompactionSummary, Origin::Harness),
        Some(_) => (MessageKind::InjectedContext, Origin::Harness),
        None if user_info()? => (MessageKind::InjectedContext, Origin::Harness),
        None => (MessageKind::Prompt, Origin::Human),
    })
}
