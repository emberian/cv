//! The Claude record codec: one validated record view and the reader's demand in, session facts
//! and at most one IR message out. Every byte source feeds this one function: mapped lines,
//! positioned records and parsed strings differ only in the [`JsonView`] they hand it. A demand
//! (`crate::stream::Demand` plus the sink's projection) chooses which fields are resolved, never which code
//! runs; a field nobody asked for is skipped by the view, not decoded and dropped.

use super::notification::NotificationSummary;
use super::*;
use crate::harness::store::document::Fields;
use crate::json::JsonView;
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use crate::stream::Tier;

/// Every top-level key any record kind reads. One selection pass per record; a field is
/// resolved only by the branch that needs it.
const TOP: &[&str] = &[
    "type",
    "uuid",
    "parentUuid",
    "timestamp",
    "cwd",
    "gitBranch",
    "permissionMode",
    "message",
    "isMeta",
    "isCompactSummary",
    "isApiErrorMessage",
    "scheduledTaskId",
    "scheduledFireId",
    "turnOrigin",
    "origin",
    "toolUseResult",
    "error",
    "errorDetails",
    "apiErrorStatus",
    "requestId",
    "isSnapshotUpdate",
    "snapshot",
    "trackingPath",
    "aiTitle",
    "customTitle",
    "summary",
    "agentName",
    "tag",
    "relocatedCwd",
    "continuedInSessionId",
    "parentSessionId",
    "parentLastUuid",
    "contextLength",
    "prUrl",
    "attachment",
    "rendered",
    "renderedInHumanTurn",
    "agentId",
    "isSidechain",
    "subtype",
    "content",
    "compactMetadata",
    "hookCount",
    "hookInfos",
    "hookErrors",
    "hasOutput",
    "hookAdditionalContext",
    "level",
    "cause",
    "retryAttempt",
    "maxRetries",
    "retryInMs",
    "source",
    "slug",
    "logicalParentUuid",
    "stopReason",
    "preventedContinuation",
    "toolUseID",
    "originalModel",
    "fallbackModel",
    "apiRefusalCategory",
    "apiRefusalExplanation",
    "retractedMessageUuids",
    "trigger",
    "direction",
    "version",
    "parentToolUseID",
    "sourceToolAssistantUUID",
    "sourceToolUseID",
    "attributionAgent",
    "attributionMcpServer",
    "attributionMcpTool",
    "attributionSkill",
    "userType",
];

/// Keys a `system` record may carry into `extra` under rich demand, in this order.
const SYSTEM_EXTRA: &[&str] = &[
    "level",
    "compactMetadata",
    "cause",
    "error",
    // A retried request (2.1.302+ `api_error` with `source: request_retry`): which attempt, of how
    // many, and the back-off before it.
    "retryAttempt",
    "maxRetries",
    "retryInMs",
    "source",
    "isMeta",
    "isSidechain",
    "agentId",
    "slug",
    "logicalParentUuid",
    // Hook activity (Stop/PreToolUse/… hook firings): the command, its output, and errors.
    "hookCount",
    "hookInfos",
    "hookErrors",
    "stopReason",
    "preventedContinuation",
    "toolUseID",
    // Model-refusal fallback (a provider safety refusal that re-routed to another model):
    // which models, the refusal category/explanation, and what got retracted.
    "originalModel",
    "fallbackModel",
    "apiRefusalCategory",
    "apiRefusalExplanation",
    "retractedMessageUuids",
    "trigger",
    "direction",
];

#[derive(Clone, Copy)]
pub(crate) enum Demand {
    Message,
    Metadata,
    /// The decision facts of [`Facts`].
    Facts,
    /// A compaction boundary's `subtype` and `compactMetadata`.
    Compact,
    /// The `message` whose role, text and tool calls the workflow readers mine.
    Tools,
}

/// A read's tier picks the head: [`Tier::Discovery`] selects the listing keys, every other tier
/// the message keys. Facts, Compact and Tools are the reshape readers' own selections.
impl From<&crate::stream::Demand> for Demand {
    fn from(demand: &crate::stream::Demand) -> Self {
        if demand.tier == Tier::Discovery {
            Demand::Metadata
        } else {
            Demand::Message
        }
    }
}

/// One shared selection/type parse for metadata and message demands. Fields are lazy views;
/// discovery never constructs a message or resolves bodies, rich extras or tool arguments.
pub(crate) struct Head<'a> {
    pub(crate) top: Fields<'a>,
    pub(crate) ty: String,
}

impl<'a> Head<'a> {
    pub(crate) fn parse(view: JsonView<'a>, demand: Demand) -> Result<Option<Self>> {
        let keys = match demand {
            Demand::Message => TOP,
            Demand::Facts => FACT_KEYS,
            Demand::Compact => &["type", "subtype", "compactMetadata"],
            Demand::Tools => &["type", "timestamp", "message"],
            // The listing keys, then the rest of what [`counts_as_turn`] decides on.
            Demand::Metadata => &[
                "type",
                "version",
                "cwd",
                "timestamp",
                "customTitle",
                "aiTitle",
                "message",
                "isApiErrorMessage",
                "isCompactSummary",
                "isMeta",
                "scheduledTaskId",
                "scheduledFireId",
                "turnOrigin",
                "origin",
            ],
        };
        let Some(top) = Fields::parse(view, keys)? else {
            return Ok(None);
        };
        let ty = top.string("type")?.unwrap_or_default();
        Ok(Some(Self { top, ty }))
    }
}

/// Every record type the codec knows is no conversational turn: the titles (`summary`,
/// `ai-title`, `custom-title`), the session facts (`continued-in`, `fork-context-ref`,
/// `agent-name`, `tag`, `relocated`, `pr-link`), model-visible `attachment`s, and pure
/// bookkeeping / live-process records with no conversational payload: the input queue,
/// UI/permission state, the leaf pointer, sub-agent hook/streaming telemetry, running cost, the
/// ATIS and isolation latches, artifact watches, request-side content replacements, and the
/// request ledger (2.1.300+: `api-request` per API call, with its `api-request-shape` system
/// prompt + tool schemas and `api-request-blob` messages, content-addressed; a blob's `message`
/// is a request body, not a turn). Only `user`, `assistant` and `system` records are turns.
pub(crate) const META_TYPES: &[&str] = &[
    "summary",
    "ai-title",
    "custom-title",
    "continued-in",
    "fork-context-ref",
    "agent-name",
    "tag",
    "relocated",
    "pr-link",
    "attachment",
    "mode",
    "permission-mode",
    "last-prompt",
    "progress",
    "started",
    "result",
    "queue-operation",
    "x-quota",
    "file-history-snapshot",
    "cost-state",
    "atis-latch",
    "isolation-latch",
    "frame-link",
    "content-replacement",
    "artifact-comment-monitor",
    "artifact-autoreact-ledger",
    "api-request",
    "api-request-shape",
    "api-request-blob",
    "tools-used",
];

/// The keys [`Demand::Facts`] selects: what prune, rewind, fork recognition and the resumability
/// check decide on, never a body.
const FACT_KEYS: &[&str] = &[
    "type",
    "subtype",
    "isSidechain",
    "agentId",
    "isApiErrorMessage",
    "sessionId",
    "cwd",
    "timestamp",
    "uuid",
    "parentUuid",
    "logicalParentUuid",
    "_cv_orig_ctx",
    "message",
    "compactMetadata",
    "toolUseResult",
    "rendered",
];

/// One content block's identity, at its position in `message.content` (a non-object element
/// keeps its slot with every field `None`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FactBlock {
    pub ty: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub tool_use_id: Option<String>,
}

/// A record's decision facts, typed: what `cv prune` and `cv rewind` select on, and what a fork's
/// head is recognized by ([`super::fork`]). Strings are
/// `Some` only when the record holds a string there; flags are `true` only for a JSON `true`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// `type`, or empty when absent or not a string.
    pub ty: String,
    pub subtype: Option<String>,
    pub sidechain: bool,
    /// `agentId`: the sub-agent a sidechain record belongs to.
    pub agent_id: Option<String>,
    pub api_error: bool,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// `timestamp`, as recorded.
    pub timestamp: Option<String>,
    pub uuid: Option<String>,
    /// `parentUuid`: `None` when the key is absent, `Some(None)` when it is null or not a string.
    pub parent: Option<Option<String>>,
    pub logical_parent: Option<String>,
    /// `message` is an object (a turn with a body).
    pub message: bool,
    /// `message.id`: one API response split over several records shares it.
    pub message_id: Option<String>,
    pub model: Option<String>,
    /// The context size `message.usage` records, read the way Claude Code reads it
    /// ([`usage_context`]); `None` without a usage object.
    pub usage_in: Option<u64>,
    pub blocks: Vec<FactBlock>,
    /// `compactMetadata.preservedSegment.headUuid`.
    pub compact_head: Option<String>,
    /// `_cv_orig_ctx`: the recorded context size a `cv prune --revive` replaced in `usage`.
    pub orig_ctx: Option<u64>,
    /// Byte spans of the `sessionId`/`session_id` strings a restamp rewrites.
    pub ids: Vec<(u64, u64)>,
}

/// The body views a [`Facts`] read leaves in the source, for a consumer that prices them.
#[derive(Default)]
pub(crate) struct Body<'a> {
    pub content: Option<JsonView<'a>>,
    pub tool_use_result: Option<JsonView<'a>>,
    pub rendered: Option<JsonView<'a>>,
    /// The `content` of each `tool_result` block, in order.
    pub results: Vec<JsonView<'a>>,
}

impl Facts {
    /// A main-thread conversational turn: `Some(is_user)`, `None` for bookkeeping, sub-agent
    /// lines and synthetic notices ([`is_turn`]).
    pub(crate) fn turn_kind(&self) -> Option<bool> {
        if self.sidechain || !is_turn(&self.ty, self.api_error, self.model.as_deref()) {
            return None;
        }
        Some(self.ty == "user")
    }

    /// The record's place in the conversation tree: `None` when it records no `parentUuid`,
    /// else the parent (`parentUuid`, or `logicalParentUuid` across a compaction) if it has one.
    pub(crate) fn link(&self) -> Option<Option<&str>> {
        let parent = self.parent.as_ref()?;
        Some(parent.as_deref().or(self.logical_parent.as_deref()))
    }

    pub(crate) fn is_compact_boundary(&self) -> bool {
        self.ty == "system" && self.subtype.as_deref() == Some("compact_boundary")
    }

    /// Facts and body views of one record. A record that is not an object has no facts but its
    /// id spans (a positional `["<id>"]` row is restamped like a keyed one).
    pub(crate) fn parse(view: JsonView<'_>) -> Result<(Self, Body<'_>)> {
        let ids = crate::restamp::source_id_spans(view, super::IDENTITY.session_keys)?;
        let Some(Head { top, ty }) = Head::parse(view, Demand::Facts)? else {
            return Ok((Self { ids, ..Self::default() }, Body::default()));
        };
        let flag =
            |key: &str| -> Result<bool> { Ok(top.get(key).map(|v| v.as_bool()).transpose()?.flatten() == Some(true)) };
        let number = |key: &str| -> Result<Option<u64>> {
            Ok(top
                .get(key)
                .map(|v| v.as_number())
                .transpose()?
                .flatten()
                .and_then(|n| n.as_u64()))
        };
        let mut facts = Self {
            ty,
            subtype: top.string("subtype")?,
            sidechain: flag("isSidechain")?,
            agent_id: top.string("agentId")?,
            api_error: flag("isApiErrorMessage")?,
            session_id: top.string("sessionId")?,
            cwd: top.string("cwd")?,
            timestamp: top.string("timestamp")?,
            uuid: top.string("uuid")?,
            parent: match top.get("parentUuid") {
                Some(_) => Some(top.string("parentUuid")?),
                None => None,
            },
            logical_parent: top.string("logicalParentUuid")?,
            orig_ctx: number("_cv_orig_ctx")?,
            ids,
            ..Self::default()
        };
        let mut body = Body {
            tool_use_result: top.get("toolUseResult"),
            rendered: top.get("rendered"),
            ..Body::default()
        };
        if let Some(message) = top
            .get("message")
            .map(|m| Fields::parse(m, &["id", "model", "usage", "content"]))
            .transpose()?
            .flatten()
        {
            facts.message = true;
            facts.message_id = message.string("id")?;
            facts.model = message.string("model")?;
            facts.usage_in = message.get("usage").map(usage_context).transpose()?.flatten();
            body.content = message.get("content");
            if let Some(blocks) = body.content.map(|c| c.array()).transpose()?.flatten() {
                for block in blocks {
                    let block = Fields::parse(block?, &["type", "id", "name", "tool_use_id", "content"])?;
                    facts.blocks.push(match block {
                        Some(b) => {
                            let ty = b.string("type")?;
                            if ty.as_deref() == Some("tool_result") {
                                body.results.extend(b.get("content"));
                            }
                            FactBlock {
                                ty,
                                id: b.string("id")?,
                                name: b.string("name")?,
                                tool_use_id: b.string("tool_use_id")?,
                            }
                        }
                        None => FactBlock::default(),
                    });
                }
            }
        }
        if let Some(head) = top
            .get("compactMetadata")
            .map(|v| v.get("preservedSegment"))
            .transpose()?
            .flatten()
            .map(|v| Fields::parse(v, &["headUuid"]))
            .transpose()?
            .flatten()
        {
            facts.compact_head = head.string("headUuid")?;
        }
        Ok((facts, body))
    }
}

/// Who wrote a `user` record: the scheduler, the harness (`isMeta`, or an `origin.kind` other
/// than `human`), or the person.
fn user_origin(top: &Fields<'_>) -> Result<Origin> {
    let scheduled = top.get("scheduledTaskId").is_some()
        || top.get("scheduledFireId").is_some()
        || top.string("turnOrigin")?.as_deref() == Some("scheduled");
    let origin_kind = match top.get("origin") {
        Some(origin) => origin
            .get("kind")?
            .filter(|v| v.may_be_string())
            .map(|v| v.resolve().map(|v| v.as_str().unwrap_or_default().to_owned()))
            .transpose()?,
        None => None,
    };
    let is_meta = top.get("isMeta").map(|v| v.as_bool()).transpose()?.flatten() == Some(true);
    Ok(user_origin_from(scheduled, is_meta, origin_kind.as_deref()))
}

/// Whether a record is a conversational turn ([`Message::is_turn`]), decided from its head
/// without building the message: what [`Record::conversation`] classifies a typed prompt or a
/// model reply. A `user` record carrying only tool results is a Tool turn, an injected one
/// (`isMeta`, a harness origin, an interrupt marker), a compaction summary or a slash command
/// is not; neither is a record without a `message` or an assistant client-side notice. `top`
/// selects `message`, `isApiErrorMessage`, `isCompactSummary`, `isMeta`, `scheduledTaskId`,
/// `scheduledFireId`, `turnOrigin` and `origin`. Reads only the first 64 characters of a user record's leading text, and the `type` of each block.
pub(crate) fn counts_as_turn(top: &Fields<'_>, ty: &str) -> Result<bool> {
    let role = match ty {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return Ok(false),
    };
    let Some(message) = top.get("message") else {
        return Ok(false);
    };
    let body = Fields::parse(message, &["model", "content"])?;
    if role == Role::Assistant {
        let api_error = top.get("isApiErrorMessage").map(|v| v.as_bool()).transpose()?.flatten() == Some(true);
        let model = body.as_ref().map_or(Ok(None), |b| b.string("model"))?;
        return Ok(is_turn(ty, api_error, model.as_deref()));
    }
    if top.get("isCompactSummary").map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
        return Ok(false);
    }
    // Every other harness-written user record is injected context or a sub-agent's return.
    let origin = user_origin(top)?;
    if origin == Origin::Harness {
        return Ok(false);
    }
    let mut head = String::new();
    match body.as_ref().and_then(|b| b.get("content")) {
        Some(content) if content.is_string()? => {
            head = string_head(content, 64)?;
            if head.starts_with("<command-name>") || head.starts_with("<local-command-stdout>") {
                return Ok(false);
            }
        }
        Some(content) => {
            if anthropic::only_tool_results(Some(content), |item| anthropic::block_kind(item, &anthropic::ANTHROPIC))? {
                return Ok(false);
            }
            if let Some(first) = content.array()?.and_then(|mut items| items.next()).transpose()? {
                if let Some(text) = first.get("text")?.filter(|v| v.may_be_string()) {
                    head = string_head(text, 64)?;
                }
            }
        }
        None => {}
    }
    Ok(user_kind_from_head(origin, &head).0 == MessageKind::Prompt)
}

/// A conversational turn by type: a user record, or an assistant record that is not one of
/// Claude Code's client-side notices (`isApiErrorMessage`, or the `<synthetic>` model).
pub(crate) fn is_turn(ty: &str, api_error: bool, model: Option<&str>) -> bool {
    match ty {
        "user" => true,
        "assistant" => !api_error && model != Some(SYNTHETIC_MODEL),
        _ => false,
    }
}

/// The three context-bearing counters of a `usage` object (input plus both cache buckets).
pub(crate) const USAGE_CTX_KEYS: [&str; 3] = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"];

/// The context-token count of a `usage` object, read the way Claude Code reads it. Since 2.1.277
/// Claude Code sizes the loaded context from `usage.iterations` when present: the LAST iteration
/// that is not an auxiliary request (`advisor_message`/`compaction`), when it is a well-formed
/// request iteration (`message`/`fallback_message`, all four counts present, non-zero context);
/// the top-level counters otherwise, and zero when they are zero. `None` for a non-object.
pub(crate) fn usage_context(usage: JsonView<'_>) -> Result<Option<u64>> {
    const ITERATION: &[&str] = &[
        "type",
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ];
    let Some(fields) = Fields::parse(
        usage,
        &[
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "iterations",
        ],
    )?
    else {
        return Ok(None);
    };
    let count = |fields: &Fields<'_>, key: &str| -> Result<Option<u64>> {
        Ok(fields
            .get(key)
            .map(|v| v.as_number())
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64()))
    };
    let context = |fields: &Fields<'_>| -> Result<u64> {
        let mut sum = 0u64;
        for key in USAGE_CTX_KEYS {
            sum = sum.saturating_add(count(fields, key)?.unwrap_or(0));
        }
        Ok(sum)
    };
    let top = context(&fields)?;
    if top == 0 {
        return Ok(Some(0));
    }
    // The last non-auxiliary iteration; a non-object one is last too, and never a request.
    let mut last: Option<Option<Fields<'_>>> = None;
    if let Some(iterations) = fields.get("iterations").map(|v| v.array()).transpose()?.flatten() {
        for iteration in iterations {
            let Some(it) = Fields::parse(iteration?, ITERATION)? else {
                last = Some(None);
                continue;
            };
            if matches!(it.string("type")?.as_deref(), Some("advisor_message" | "compaction")) {
                continue;
            }
            last = Some(Some(it));
        }
    }
    if let Some(Some(it)) = last {
        let request = matches!(it.string("type")?.as_deref(), Some("message" | "fallback_message"));
        let mut counted = true;
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            counted &= count(&it, key)?.is_some();
        }
        let sum = context(&it)?;
        if request && counted && sum > 0 {
            return Ok(Some(sum));
        }
    }
    Ok(Some(top))
}

/// One record: update `session` and hand its message (if any) to `sink`. `at` is the record's
/// byte offset when the source has one. The shared driver owns source checks; errors here
/// are source failures, never malformed records.
pub(super) fn record(
    session: &mut Session,
    view: JsonView<'_>,
    at: Option<u64>,
    opts: &crate::stream::Demand,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    // A record that is not an object carries nothing (Value's lookups all miss).
    let Some(Head { top, ty }) = Head::parse(view, opts.into())? else {
        return Ok(Flow::Continue);
    };
    let ty = ty.as_str();
    let r = Record { view, top, at, opts };

    // The approval mode a prompt ran under (`permissionMode` on user rows and on `permission-mode`
    // records), and the file checkpoints `/rewind` restores: session-level affordance facts.
    if let Some(mode) = r.top.string("permissionMode")? {
        crate::affordance::note_mode(session.harness_extra_mut(Harness::Claude), &mode);
    }
    match ty {
        "file-history-snapshot" => {
            let fresh = u64::from(r.bool("isSnapshotUpdate")? != Some(true));
            let mut files = Vec::new();
            if let Some(snapshot) = r.top.get("snapshot") {
                if let Some(backups) = snapshot.get("trackedFileBackups")? {
                    backups.for_each_entry(|key, _| {
                        files.push(key.to_owned());
                        Ok(())
                    })?;
                }
            }
            let bag = session.harness_extra_mut(Harness::Claude);
            crate::affordance::note_checkpoints(bag, fresh, files.iter().map(String::as_str));
        }
        "file-history-delta" => {
            let file = r.top.string("trackingPath")?;
            crate::affordance::note_checkpoints(session.harness_extra_mut(Harness::Claude), 0, file.as_deref());
        }
        _ => {}
    }

    match ty {
        // Titles, by precedence: an explicit `/rename` (`custom-title`) outranks the model-generated
        // `ai-title`, which outranks the legacy `summary` (Claude Code stopped writing `summary`
        // records around 2.1.25x; `ai-title` is rewritten on every prompt).
        "ai-title" => {
            if let Some(t) = r.top.string("aiTitle")? {
                let renamed = session
                    .harness_extra(Harness::Claude)
                    .is_some_and(|b| b.contains_key("custom_title"));
                if !renamed {
                    session.title = Some(t);
                }
            }
            return r.carry_or_skip(sink);
        }
        "custom-title" => {
            if let Some(t) = r.top.string("customTitle")? {
                session.title = Some(t.clone());
                session
                    .harness_extra_mut(Harness::Claude)
                    .insert("custom_title".into(), Value::String(t));
            }
            return r.carry_or_skip(sink);
        }
        "summary" => {
            if session.title.is_none() {
                if let Some(t) = r.top.string("summary")? {
                    session.title = Some(t);
                }
            }
            return r.carry_or_skip(sink);
        }
        // Session-level facts. The continuation pointer is first-class lineage; the derived agent
        // name, a `/tag`, a fork's relocated cwd and the linked PR(s) are Claude facts in
        // `extra["claude"]` (snake_case: these names are cv's).
        "continued-in" => {
            if let Some(id) = r.top.string("continuedInSessionId")? {
                session.lineage.continued_in = Some(id);
            }
            return r.carry_or_skip(sink);
        }
        // A fork sub-agent's first record (2.1.302+, `subagent_type: fork`): the child's context is
        // the parent transcript up to `parentLastUuid` (`contextLength` records), so the parent is
        // where it was forked from; the cut point rides in the bag as `fork_context`.
        "fork-context-ref" => {
            if let Some(id) = r.top.string("parentSessionId")? {
                session.lineage.forked_from = Some(id);
            }
            let mut cut = Map::new();
            if let Some(uuid) = r.top.string("parentLastUuid")? {
                cut.insert("parent_last_uuid".into(), Value::String(uuid));
            }
            if let Some(n) = r.value("contextLength")?.filter(Value::is_number) {
                cut.insert("context_length".into(), n);
            }
            if !cut.is_empty() {
                session
                    .harness_extra_mut(Harness::Claude)
                    .insert("fork_context".into(), Value::Object(cut));
            }
            return r.carry_or_skip(sink);
        }
        "agent-name" | "tag" | "relocated" | "pr-link" => {
            let (from, to) = match ty {
                "agent-name" => ("agentName", "agent_name"),
                "tag" => ("tag", "tag"),
                "relocated" => ("relocatedCwd", "relocated_cwd"),
                _ => ("prUrl", "pr_links"),
            };
            let value = r.top.string(from)?;
            let bag = session.harness_extra_mut(Harness::Claude);
            if let Some(value) = value {
                if ty == "pr-link" {
                    if let Some(list) = bag.entry(to).or_insert_with(|| Value::Array(Vec::new())).as_array_mut() {
                        list.push(Value::String(value));
                    }
                } else {
                    bag.insert(to.into(), Value::String(value));
                }
            }
            return r.carry_or_skip(sink);
        }
        // Model-visible attachments. Since ~2.1.23x the system reminders Claude Code appends to the
        // prompt — hook output, edited-file notices, queued task notifications, CLAUDE.md
        // `instructions`, skill/agent listings, … — are separate `attachment` records whose
        // `rendered[].content` is byte-for-byte what the model saw (they used to be inline
        // `<system-reminder>` text in user content). Surface those as System turns; an attachment
        // with nothing rendered (a hook that printed nothing) is bookkeeping only.
        "attachment" => {
            // `prompt_snapshot` records the whole system prompt Claude Code sent (an array of
            // sections) — the session-level fact, never a message. The latest snapshot wins: a
            // resume re-snapshots, and that is the prompt the rest of the transcript ran under.
            if let Some(attachment) = r.top.get("attachment") {
                if let Some(sections) = attachment
                    .get("systemPrompt")?
                    .map(|v| v.array())
                    .transpose()?
                    .flatten()
                {
                    let mut joined = Vec::new();
                    for section in sections {
                        if let Some(text) = section?.resolve()?.as_str() {
                            joined.push(text.to_owned());
                        }
                    }
                    let joined = joined.join("\n");
                    if !joined.trim().is_empty() {
                        session.system_prompt = Some(joined);
                    }
                }
            }
            return match r.attachment()? {
                Some(mut m) => {
                    r.stamp(&mut m);
                    Ok(sink.message(m))
                }
                None => r.carry_or_skip(sink),
            };
        }
        // The rest of [`META_TYPES`]: bookkeeping. The lean passes drop it; `complete` carries it
        // verbatim as round-trippable carrier messages.
        t if META_TYPES.contains(&t) => return r.carry_or_skip(sink),
        "user" | "assistant" | "system" | "" => {}
        // Any other record is a bookkeeping record we haven't met yet (Claude Code adds a few per
        // release), whether or not it has a `message` field: only the three types above are turns.
        // Carry it in `complete` mode so round-trips stay lossless, skip it otherwise.
        _ => return r.carry_or_skip(sink),
    }

    note_conversation_metadata(
        session,
        r.top.string("cwd")?.as_deref(),
        r.top.string("gitBranch")?.as_deref(),
        r.timestamp()?,
    );
    let message = match ty {
        "system" => r.system()?,
        "user" => r.conversation(Role::User, sink)?,
        "assistant" => r.conversation(Role::Assistant, sink)?,
        _ => None,
    };
    let Some(mut msg) = message else {
        return Ok(Flow::Continue);
    };
    r.stamp(&mut msg);
    // Keep the message model record-local; the session default is the first non-synthetic
    // model encountered in this read, so a window can have a different session default.
    if session.model.is_none() && msg.model.as_deref() != Some(SYNTHETIC_MODEL) {
        session.model = msg.model.clone();
    }
    Ok(sink.message(msg))
}

/// One record under the read's [`stream::Demand`]. At [`Tier::Facts`] Claude builds only the
/// reducer facts: a notice's `compactMetadata` `trigger`/`preTokens`/`durationMs`, `message.id`,
/// `sourceToolUseID`, the attachment type and a tool result's `details.restorable`.
struct Record<'a, 'o> {
    view: JsonView<'a>,
    top: Fields<'a>,
    at: Option<u64>,
    opts: &'o crate::stream::Demand,
}

impl<'a> Record<'a, '_> {
    fn bool(&self, key: &str) -> Result<Option<bool>> {
        Ok(self.top.get(key).map(|v| v.as_bool()).transpose()?.flatten())
    }

    fn timestamp(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self.top.string("timestamp")?.as_deref().and_then(parse_ts))
    }

    /// Offset-stamping pass: stamp the record's byte offset so `cv rewind` can map a message to
    /// its record (see [`crate::offsets::OFFSET_KEY`]). Only `cv rewind` demands it, and only a
    /// source with offsets can supply it. EVERY message-producing record must call this — an
    /// unstamped message is one rewind cannot cut at.
    fn stamp(&self, msg: &mut Message) {
        if let (true, Some(at)) = (self.opts.offsets, self.at) {
            msg.extra.insert(crate::offsets::OFFSET_KEY.into(), at.into());
        }
    }

    /// Carry the record verbatim under `complete`, skip it otherwise — the shared tail of every
    /// non-conversational record arm.
    fn carry_or_skip(&self, sink: &mut dyn MessageSink) -> Result<Flow> {
        Ok(if self.opts.tier == Tier::Complete {
            sink.message(self.carrier()?)
        } else {
            Flow::Continue
        })
    }

    /// A non-conversational meta record carried verbatim under [`crate::stream::Demand::COMPLETE`] so a
    /// `parse → emit` round-trip reproduces it. Modeled as an empty-content `System` message —
    /// render/dataset/count all skip empty turns — with the full original record stashed under
    /// `extra["_record"]` for replay with JSON values and key order preserved; serialization
    /// may normalize whitespace and numeric spelling.
    fn carrier(&self) -> Result<Message> {
        let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
        m.id = self.top.string("uuid")?;
        m.parent_id = self.top.string("parentUuid")?;
        m.timestamp = self.timestamp()?;
        if let Some(t) = self.top.string("type")? {
            let record_type = if t == "system" {
                match self.top.string("subtype")? {
                    Some(sub) if !sub.is_empty() => format!("system.{sub}"),
                    _ => t,
                }
            } else if t == "attachment" {
                match self
                    .top
                    .get("attachment")
                    .and_then(|a| a.get("type").ok().flatten())
                    .filter(|v| v.may_be_string())
                    .map(|v| v.resolve().map(|v| v.as_str().unwrap_or_default().to_owned()))
                    .transpose()?
                {
                    Some(att) if !att.is_empty() => format!("attachment.{att}"),
                    _ => t,
                }
            } else {
                t
            };
            m.harness_extra_mut(Harness::Claude)
                .insert("record_type".into(), Value::String(record_type));
        }
        m.extra.insert(CARRIER_KEY.into(), self.view.resolve()?.into_owned());
        Ok(m)
    }

    fn value(&self, key: &str) -> Result<Option<Value>> {
        self.top.value(key)
    }

    /// A model-visible `attachment` record (one with a non-empty `rendered[]`) as a
    /// [`Role::System`] message: one text block per rendered item, exactly the text appended to
    /// the prompt. `None` when nothing was rendered. Under `complete` the message is also a carrier
    /// of the record, so the emitter preserves its JSON values and key order while lean consumers still
    /// see its text. `origin` is [`Origin::Hook`] for `hook_*` attachments (a user-configured
    /// hook's stdout) and [`Origin::Harness`] otherwise. The attachment kind always rides in
    /// `extra["claude"]["attachment_type"]`; the full `attachment` object only under rich demand.
    fn attachment(&self) -> Result<Option<Message>> {
        let Some(rendered) = self.top.get("rendered").map(|v| v.array()).transpose()?.flatten() else {
            return Ok(None);
        };
        let Some(attachment) = self.top.get("attachment").filter(|v| v.is_object().unwrap_or(false)) else {
            return Ok(None);
        };
        let mut blocks = Vec::new();
        for item in rendered {
            let item = item?;
            let text = match item.get("content")? {
                Some(content) => content.lazy_text(self.opts.spans)?,
                None => None,
            };
            if let Some(text) = text {
                blocks.push(Block::Text { text });
            }
        }
        if blocks.is_empty() {
            return Ok(None);
        }
        let att_type = attachment
            .get("type")?
            .filter(|v| v.may_be_string())
            .map(|v| v.resolve().map(|v| v.as_str().unwrap_or_default().to_owned()))
            .transpose()?
            .unwrap_or_else(|| "attachment".into());
        let mut m = if self.opts.tier == Tier::Complete {
            self.carrier()?
        } else {
            Message::new(Role::System)
        };
        m.kind = MessageKind::InjectedContext;
        m.origin = if att_type.starts_with("hook_") {
            Origin::Hook
        } else {
            Origin::Harness
        };
        m.id = self.top.string("uuid")?;
        m.parent_id = self.top.string("parentUuid")?;
        m.timestamp = self.timestamp()?;
        m.content = blocks;
        let mut rich = Vec::new();
        if self.opts.tier >= Tier::Rich {
            rich.push(("attachment", attachment.resolve()?.into_owned()));
            for key in ["renderedInHumanTurn", "agentId"] {
                if let Some(val) = self.value(key)? {
                    rich.push((key, val));
                }
            }
            if self.bool("isSidechain")? == Some(true) {
                rich.push(("isSidechain", Value::Bool(true)));
            }
        }
        let bag = m.harness_extra_mut(Harness::Claude);
        bag.insert("attachment_type".into(), Value::String(att_type));
        for (key, val) in rich {
            bag.insert(key.into(), val);
        }
        Ok(Some(m))
    }

    /// `system` records cover a grab-bag of subtypes. Only some carry user-visible text worth
    /// surfacing as a [`Role::System`] message; the rest (timing, hook bookkeeping, killed-agent
    /// notices) have no body and are dropped. `subtype`/`level`/metadata go to `extra` either way.
    fn system(&self) -> Result<Option<Message>> {
        // Format-complete: `system` records are carried verbatim. The lean passes *render* them
        // (slash-command distillation, the compaction boundary, the ⛓
        // hook one-liner), which is lossy; carrying the raw record (system bodies are small
        // notices) is the only way `parse → emit` reproduces it.
        if self.opts.tier == Tier::Complete {
            return Ok(Some(self.carrier()?));
        }
        let subtype = self.top.string("subtype")?.unwrap_or_default();
        let content = self.top.get("content").filter(|v| v.may_be_string());
        // `stop_hook_summary` carries no `content`, but it IS the record of a Stop-hook firing:
        // surface it as a readable line when there's something to show (output or an error).
        if subtype == "stop_hook_summary" {
            let mut hooks = Map::new();
            for key in [
                "hookCount",
                "hookErrors",
                "hasOutput",
                "hookAdditionalContext",
                "hookInfos",
            ] {
                if let Some(value) = self.value(key)? {
                    hooks.insert(key.into(), value);
                }
            }
            return match render_hook_summary(&Value::Object(hooks)) {
                Some(body) => self.system_msg(Some(Text::Inline(body)), &subtype),
                None => Ok(None),
            };
        }
        // Since 2.1.302 a retried request is an `api_error` with no `content`: `error` is an object
        // (`formatted`, `message`, `status`) and `retryAttempt`/`maxRetries` say where the retry
        // loop stood. `model_refusal_no_fallback` (2.1.32x+) similarly writes `"content": ""` and
        // puts the human-readable refusal explanation in `apiRefusalExplanation`.
        let retry = if content.is_none() && subtype == "api_error" {
            self.api_error_text()?
        } else {
            None
        };
        let refusal_fallback = if subtype.starts_with("model_refusal") {
            self.top
                .string("apiRefusalExplanation")?
                .filter(|s| !s.trim().is_empty())
        } else {
            None
        };
        // Subtypes with no human-readable body — keep them out of the message stream.
        let bodyless = matches!(subtype.as_str(), "turn_duration" | "agents_killed");
        if bodyless
            || (content.is_none() && retry.is_none() && refusal_fallback.is_none() && subtype != "compact_boundary")
        {
            return Ok(None);
        }
        if let Some(text) = retry {
            return self.system_msg(Some(Text::Inline(text)), &subtype);
        }
        // Slash-command records wrap their payload in pseudo-XML tags; distill the tag soup to a
        // readable one/two-liner (these are always small, so they stay inline).
        if subtype == "local_command" {
            if let Some(c) = content.map(|v| v.resolve()).transpose()? {
                if let Some(rendered) = render_slash_command(c.as_str().unwrap_or_default()) {
                    return self.system_msg(Some(Text::Inline(rendered)), &subtype);
                }
            }
        }
        let mut text = content.map(|c| c.lazy_text(self.opts.spans)).transpose()?.flatten();
        if matches!(&text, Some(Text::Inline(s)) if s.trim().is_empty()) {
            text = None;
        }
        if text.is_none() {
            text = refusal_fallback.map(Text::Inline);
        }
        if text.is_none() && subtype != "compact_boundary" {
            return Ok(None);
        }
        self.system_msg(text, &subtype)
    }

    /// The one-line text of a content-less `api_error`: the error's `formatted` line (else its
    /// `message`, else a bare string `error`), with the retry position when the record has one.
    fn api_error_text(&self) -> Result<Option<String>> {
        let error = self.top.get("error");
        let mut text = match error
            .as_ref()
            .map(|e| Fields::parse(*e, &["formatted", "message"]))
            .transpose()?
            .flatten()
        {
            Some(fields) => fields.string("formatted")?.or(fields.string("message")?),
            None => error
                .map(|e| e.resolve())
                .transpose()?
                .and_then(|e| e.as_str().map(str::to_owned)),
        };
        let number = |key: &str| -> Result<Option<u64>> { Ok(self.value(key)?.and_then(|v| v.as_u64())) };
        if let (Some(line), Some(attempt)) = (text.as_mut(), number("retryAttempt")?) {
            line.push_str(&format!(" (retry {attempt}"));
            if let Some(max) = number("maxRetries")? {
                line.push_str(&format!(" of {max}"));
            }
            line.push(')');
        }
        Ok(text.filter(|t| !t.trim().is_empty()))
    }

    fn system_msg(&self, text: Option<Text>, subtype: &str) -> Result<Option<Message>> {
        // The subtype says what the notice IS: a compaction marker, an error the model never
        // answered, a Stop hook's output, a scheduled wakeup, or a plain harness notice.
        let (kind, origin) = match subtype {
            "compact_boundary" => (MessageKind::CompactionBoundary, Origin::Harness),
            "api_error" | "model_refusal_fallback" | "model_refusal_no_fallback" => {
                (MessageKind::Error, Origin::Harness)
            }
            "stop_hook_summary" => (MessageKind::Notice, Origin::Hook),
            "scheduled_task_fire" => (MessageKind::Notice, Origin::Scheduler),
            _ => (MessageKind::Notice, Origin::Harness),
        };
        let mut bag = Map::new();
        if self.opts.tier == Tier::Facts {
            // Reducers read only the compaction facts: trigger and the two counters.
            if let Some(meta) = self
                .top
                .get("compactMetadata")
                .filter(|v| v.is_object().unwrap_or(false))
            {
                let mut fields = Map::new();
                if let Some(value) = meta.get("trigger")?.filter(|v| v.may_be_string()) {
                    fields.insert("trigger".into(), value.resolve()?.into_owned());
                }
                for key in ["preTokens", "durationMs"] {
                    if let Some(number) = meta.get(key)?.map(|v| v.as_number()).transpose()?.flatten() {
                        if number.as_u64().is_some() {
                            fields.insert(key.into(), Value::Number(number));
                        }
                    }
                }
                bag.insert("compactMetadata".into(), Value::Object(fields));
            }
        } else if self.opts.tier >= Tier::Rich {
            if !subtype.is_empty() {
                bag.insert("subtype".into(), Value::String(subtype.to_string()));
            }
            for key in SYSTEM_EXTRA {
                if let Some(val) = self.value(key)? {
                    bag.insert(key.to_string(), val);
                }
            }
        }
        let mut m = match text {
            Some(text) => {
                let mut m = Message::of_kind(Role::System, kind, origin);
                m.content.push(Block::Text { text });
                m
            }
            None => Message::synthesized(kind, origin),
        };
        m.id = self.top.string("uuid")?;
        m.parent_id = self.top.string("parentUuid")?;
        m.timestamp = self.timestamp()?;
        if !bag.is_empty() {
            m.extra.insert(Harness::Claude.as_str().into(), Value::Object(bag));
        }
        Ok(Some(m))
    }

    /// A `user` or `assistant` record. `None` when it has no `message` at all.
    fn conversation(&self, role: Role, sink: &dyn MessageSink) -> Result<Option<Message>> {
        let Some(msg) = self.top.get("message") else {
            return Ok(None);
        };
        let opts = self.opts;
        // The sink may narrow a rich read to reducer facts for this message.
        let tier = opts.for_sink(sink);
        let body = Fields::parse(
            msg,
            &[
                "model",
                "content",
                "usage",
                "id",
                "stop_reason",
                "stop_sequence",
                "stop_details",
            ],
        )?;
        let body_string = |key: &str| -> Result<Option<String>> { body.as_ref().map_or(Ok(None), |b| b.string(key)) };
        let model = body_string("model")?;
        let usage = match body.as_ref().and_then(|b| b.get("usage")) {
            Some(usage) => Some(parse_usage(usage)?),
            None => None,
        };
        let user_origin = user_origin(&self.top)?;

        let mut blocks = Vec::new();
        let mut head = None;
        let mut bare = false;
        if let Some(content) = body.as_ref().and_then(|b| b.get("content")) {
            if content.is_string()? {
                bare = true;
                if role == Role::User {
                    head = Some(classify_head(content, user_origin)?);
                }
                blocks.push(Block::Text {
                    text: content.lazy_text(opts.spans)?.unwrap_or_default(),
                });
            } else if let Some(items) = content.array()? {
                for (index, item) in items.enumerate() {
                    let item = item?;
                    if index == 0 && role == Role::User {
                        if let Some(text) = item.get("text")?.filter(|v| v.may_be_string()) {
                            head = Some(classify_head(text, user_origin)?);
                        }
                    }
                    if let Some(block) = block(item, opts.spans)? {
                        blocks.push(block);
                    }
                }
            }
        }

        // INTERFACE-V2 §4: `toolUseResult` — the structured sidecar Claude writes *next to* the
        // `tool_result` block — is a SHARED concept, so it rides on the block as
        // `Block::ToolResult::details`, not in the harness bag. Only rich demand resolves it: the
        // sidecar routinely dwarfs the visible transcript. Reducers read only `restorable`.
        let sidecar = self.top.get("toolUseResult");
        if tier == Tier::Facts {
            if let Some(restorable) = sidecar.map(|v| v.get("restorable")).transpose()?.flatten() {
                attach_tool_use_result(&mut blocks, serde_json::json!({"restorable": restorable.resolve()?}));
            }
        } else if tier >= Tier::Rich && blocks.iter().any(|b| matches!(b, Block::ToolResult { .. })) {
            if let Some(sidecar) = sidecar.filter(|v| !v.is_null().unwrap_or(false)) {
                attach_tool_use_result(&mut blocks, sidecar.resolve()?.into_owned());
            }
        }

        // A `user` line carrying only tool results is really a Tool turn.
        let role =
            if role == Role::User && !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }))
            {
                Role::Tool
            } else {
                role
            };
        // A sub-agent's final answer: the `Agent`/`Task` call ran in the foreground, so its
        // sidecar names the agent and is not an `async_launched` receipt.
        let sync_agent_return = match sidecar.filter(|v| v.is_object().unwrap_or(false)) {
            Some(sidecar) => {
                sidecar.get("agentId")?.is_some()
                    && sidecar
                        .get("status")?
                        .filter(|v| v.may_be_string())
                        .map(|v| v.resolve().map(|v| v.as_str() == Some("async_launched")))
                        .transpose()?
                        != Some(true)
            }
            None => false,
        };
        let api_error = self.bool("isApiErrorMessage")?;
        // Claude Code's own client-side notices (an `isApiErrorMessage`, or any `<synthetic>`
        // model row such as "No response requested.") are not turns.
        let synthetic =
            role == Role::Assistant && (api_error == Some(true) || model.as_deref() == Some(SYNTHETIC_MODEL));
        let (head_text, user) = head.unwrap_or_else(|| (String::new(), user_kind_from_head(user_origin, "")));
        let class = ConversationClass {
            synthetic,
            api_error: synthetic && api_error == Some(true),
            compact_summary: self.bool("isCompactSummary")? == Some(true),
            sync_agent_return,
            // A `user` record whose whole content is a slash-command record: the invocation or its
            // captured output. Only a bare string that *starts* with the tag qualifies.
            slash_command: bare
                && (head_text.starts_with("<command-name>") || head_text.starts_with("<local-command-stdout>")),
            user,
        };
        let (role, kind, origin) = classify_conversation(role, class, opts.tier == Tier::Complete);
        let model = if synthetic && opts.tier != Tier::Complete {
            None
        } else {
            model
        };

        // Everything Claude-specific lands in `extra["claude"]`; the top level stays clean.
        let mut bag = Map::new();
        if synthetic && !matches!(tier, Tier::Facts | Tier::Complete) {
            for key in [
                "isApiErrorMessage",
                "error",
                "errorDetails",
                "apiErrorStatus",
                "requestId",
            ] {
                if let Some(val) = self.value(key)?.filter(|val| !val.is_null()) {
                    bag.insert(key.to_string(), val);
                }
            }
        }
        // Skill bodies loaded by a tool have an ID; slash-loaded bodies do not. This small
        // correlation fact prevents counting the tool invocation and its injected body twice.
        if let Some(id) = self.top.string("sourceToolUseID")? {
            bag.insert("sourceToolUseID".into(), Value::String(id));
        }
        if matches!(tier, Tier::Usage | Tier::Facts) {
            if let Some(id) = body_string("id")? {
                bag.insert(format!("{MESSAGE_EXTRA_PREFIX}id"), Value::String(id));
            }
        } else if tier >= Tier::Rich {
            for &key in CLAUDE_EXTRA_TOP {
                if let Some(value) = self.value(key)?.filter(|v| keep_extra_top(key, v)) {
                    bag.insert(key.to_owned(), value);
                }
            }
            for &key in CLAUDE_EXTRA_MSG {
                if let Some(value) = body
                    .as_ref()
                    .map(|b| b.value(key))
                    .transpose()?
                    .flatten()
                    .filter(|v| !v.is_null())
                {
                    bag.insert(format!("{MESSAGE_EXTRA_PREFIX}{key}"), value);
                }
            }
        }
        // Format-complete: capture EVERY remaining top-level + message-level field (beyond the
        // first-classed ones) so `parse → emit` reproduces the record with no dropped keys. The
        // already-captured fields are re-confirmed, not duplicated. This is also the only producer
        // of the bag's verbatim `toolUseResult`, the complete-mode replay fallback.
        if opts.tier == Tier::Complete {
            self.view.for_each_entry(|key, value| {
                if !CLAUDE_FIRSTCLASS_TOP.contains(&key) && !bag.contains_key(key) {
                    bag.insert(key.to_owned(), value.resolve()?.into_owned());
                }
                Ok(())
            })?;
            msg.for_each_entry(|key, value| {
                if CLAUDE_FIRSTCLASS_MSG.contains(&key) {
                    return Ok(());
                }
                let key = format!("{MESSAGE_EXTRA_PREFIX}{key}");
                if !bag.contains_key(&key) {
                    bag.insert(key, value.resolve()?.into_owned());
                }
                Ok(())
            })?;
        }
        let mut extra = Map::new();
        if !bag.is_empty() {
            extra.insert(Harness::Claude.as_str().into(), Value::Object(bag));
        }
        Ok(Some(Message {
            complete_record: None,
            id: self.top.string("uuid")?,
            parent_id: self.top.string("parentUuid")?,
            role,
            kind,
            origin,
            timestamp: self.timestamp()?,
            model,
            content: blocks,
            usage,
            extra,
        }))
    }
}

/// One Anthropic content block through the family's view codec, with Claude's text rule. A
/// persisted-output stub keeps the stub as `content` (that IS what the model saw) and records
/// where the full output lives, so renders can point at it and indexers can read it.
fn block(item: JsonView<'_>, spans: bool) -> Result<Option<Block>> {
    // An array result larger than a positioned read buffer is joined in place, not owned.
    let joined_over = if spans { 64 * 1024 } else { u64::MAX };
    let mut block = anthropic::block_from_view_with(
        item,
        &anthropic::ANTHROPIC,
        spans,
        &mut |v| v.lazy_text(spans),
        joined_over,
    )?;
    if let Some(Block::ToolResult { content, details, .. }) = &mut block {
        let stub = match item.get("content")?.filter(|v| v.may_be_string()) {
            Some(raw) if string_head(raw, PERSISTED.len())? == PERSISTED => {
                persisted_output(raw.resolve()?.as_str().unwrap_or_default())
            }
            Some(_) => None,
            None => content.inline_str().and_then(persisted_output),
        };
        *details = stub.map(|(path, size)| serde_json::json!({ "persistedOutput": { "path": path, "size": size } }));
    }
    Ok(block)
}

const PERSISTED: &str = "<persisted-output>";

/// The usage counters of a `message.usage` object; anything else counts nothing.
fn parse_usage(v: JsonView<'_>) -> Result<Usage> {
    let fields = Fields::parse(
        v,
        &[
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
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
    Ok(Usage {
        input_tokens: get("input_tokens")?,
        output_tokens: get("output_tokens")?,
        cache_read_tokens: get("cache_read_input_tokens")?,
        cache_creation_tokens: get("cache_creation_input_tokens")?,
        ..Usage::default()
    })
}

/// The first `chars` characters of a string after its leading whitespace, decoded in place:
/// classification never owns or windows a giant body.
fn string_head(view: JsonView<'_>, chars: usize) -> Result<String> {
    let mut head = String::new();
    let mut n = 0;
    view.for_each_char(|c| {
        if !(head.is_empty() && c.is_whitespace()) {
            head.push(c);
            n += 1;
        }
        n < chars
    })?;
    Ok(head)
}

/// What a `user` record's head makes it, and the head itself (for slash-command detection). A
/// harness `<task-notification>` is a sub-agent's return when its first `<summary>` names an agent
/// or a workflow; that summary can follow a giant output, so it is found by streaming.
fn classify_head(text: JsonView<'_>, origin: Origin) -> Result<(String, (MessageKind, Origin))> {
    let prefix = string_head(text, 64)?;
    let kind = if origin == Origin::Harness && prefix.starts_with("<task-notification>") {
        let mut summary = NotificationSummary::default();
        text.for_each_str_chunk(|chunk| {
            summary.push(chunk);
            Ok(())
        })?;
        if summary.is_agent_return() {
            (MessageKind::SubagentReturn, Origin::Subagent)
        } else {
            (MessageKind::InjectedContext, Origin::Harness)
        }
    } else {
        user_kind_from_head(origin, &prefix)
    };
    Ok((prefix, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle(body: &str, origin: Origin) -> (MessageKind, Origin) {
        let head = body.trim_start();
        if head.starts_with("[Request interrupted by user") {
            (MessageKind::InjectedContext, Origin::Harness)
        } else if origin != Origin::Harness {
            (MessageKind::Prompt, origin)
        } else if head.starts_with("<task-notification>")
            && tag_inner(head, "summary").is_some_and(|summary| {
                let summary = summary.trim_start();
                summary.starts_with("Agent ") || summary.to_ascii_lowercase().contains("workflow")
            })
        {
            (MessageKind::SubagentReturn, Origin::Subagent)
        } else {
            (MessageKind::InjectedContext, Origin::Harness)
        }
    }

    /// The bounded head and streamed summary classify exactly as the whole decoded string would,
    /// in owned and source views, plain and escaped (including escaped tags).
    #[test]
    fn bounded_classification_matches_whole_string_oracle() {
        let mut bodies = vec![
            "<task-notification><summary>Agent a completed</summary>".to_string(),
            "<task-notification><summary>agent a completed</summary>".into(),
            "<task-notification><summary>  \u{2003}Agent a completed</summary>".into(),
            "<task-notification><summary>WoRkFlOw completed</summary>".into(),
            "<task-notification><summary>work</summary>flow".into(),
            "<task-notification>workflow<summary>command done</summary>".into(),
            "<task-notification><summary>command done</summary><summary>Agent a</summary>".into(),
            "<task-notification><summary>Agent a".into(),
            "<task-notification><summary>workflow</Summary>".into(),
            "<task-notification><summary></summary><summary>workflow</summary>".into(),
            "<task-notification></summary><summ<summary>workflow</summary>".into(),
            "<task-notification><summary><summary>Agent a</summary></summary>".into(),
            "<task-notification><summary>wo🦝rkflow</summary>".into(),
            "<task-notification><summary>wwoWorkflow done</summary>".into(),
            "[Request interrupted by user]".into(),
            "<command-name>/help</command-name>".into(),
        ];
        for summary in ["Agent completed", "worKflow done", "command done", "Agent unclosed"] {
            let close = if summary.ends_with("unclosed") {
                ""
            } else {
                "</summary>"
            };
            bodies.push(format!(
                "\u{2003}<task-notification>{}<summary>{summary}{close}",
                "ignored\n🦝".repeat(20_000)
            ));
        }
        let dir = cv_testkit::temp_dir("claude-classification");
        let path = dir.join("text.json");
        for body in bodies {
            for origin in [Origin::Harness, Origin::Human, Origin::Scheduler] {
                let expected = oracle(&body, origin);
                let value = Value::String(format!("{body}\n"));
                assert_eq!(classify_head((&value).into(), origin).unwrap().1, expected);
                for encoded in [
                    serde_json::to_string(&value).unwrap(),
                    serde_json::to_string(&value).unwrap().replace('<', r"\u003c"),
                ] {
                    fs::write(&path, &encoded).unwrap();
                    let source = JsonSource::capture(&path).unwrap();
                    let actual = source
                        .read(ReadInput::Record(0, &encoded, None), Validation::Value, |view| {
                            classify_head(view, origin)
                        })
                        .unwrap();
                    assert_eq!(actual.1, expected, "{origin:?}");
                }
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notification_summary_crosses_windows() {
        for body in [
            "🦝<summary>\u{2003}Agent completed</summary>outside workflow",
            "<summary>woRkFlOw done</summary>",
            "<summary>work</summary>flow<summary>Agent ignored</summary>",
            "<summary>Agent unclosed",
            "<summary><summary>workflow</summary>",
        ] {
            let expected = tag_inner(body, "summary").is_some_and(|summary| {
                let summary = summary.trim_start();
                summary.starts_with("Agent ") || summary.to_ascii_lowercase().contains("workflow")
            });
            for width in 1..24 {
                let mut summary = NotificationSummary::default();
                let mut start = 0;
                while start < body.len() {
                    let mut end = (start + width).min(body.len());
                    while !body.is_char_boundary(end) {
                        end += 1;
                    }
                    summary.push(&body[start..end]);
                    start = end;
                }
                assert_eq!(summary.is_agent_return(), expected, "width {width}");
            }
        }
    }

    /// The selection lists skip silently: a non-object line is `Ok(None)` under every demand (the
    /// non-turn filter), and an object missing every selected key still parses — fields are
    /// presence-selected, completeness is objecthood.
    #[test]
    fn selections_skip_non_objects_and_select_by_presence() -> Result<()> {
        for demand in [
            Demand::Message,
            Demand::Metadata,
            Demand::Facts,
            Demand::Compact,
            Demand::Tools,
        ] {
            for line in ["[1,2]", "\"scalar\"", "42", "null"] {
                let value: serde_json::Value = serde_json::from_str(line)?;
                let parsed = Head::parse(JsonView::from(&value), demand)?;
                assert!(parsed.is_none(), "{line} parsed");
            }
            let value: serde_json::Value = serde_json::from_str(r#"{"unrelated":true}"#)?;
            let head = Head::parse(JsonView::from(&value), demand)?;
            assert!(head.is_some(), "object with none of the keys");
        }
        Ok(())
    }

    #[test]
    fn usage_context_saturates_u64_overflow() -> Result<()> {
        let v: serde_json::Value = serde_json::json!({
            "input_tokens": u64::MAX,
            "cache_read_input_tokens": 10u64,
            "cache_creation_input_tokens": 5u64
        });
        assert_eq!(usage_context(JsonView::from(&v))?, Some(u64::MAX));
        Ok(())
    }
}
