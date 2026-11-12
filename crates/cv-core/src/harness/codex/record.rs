//! The Codex record codec: one validated record view and the reader's demand in, session facts
//! and IR messages out (into the reader's scratch, where a following usage record can still
//! reach the trailing assistant message). Every byte source feeds this one function: mapped and
//! positioned lines of a plain rollout, the decoded arena of a `.jsonl.zst`, the items of a
//! 2025 single-document rollout, and parsed strings. The demand chooses which fields are resolved
//! and how a body becomes [`Text`], never which code runs.

use super::*;
use crate::json::{Json, JsonView};

/// Shared wire classification for metadata, grammar detection and message projection.
pub(super) struct Head<'v> {
    pub view: JsonView<'v>,
    pub ty: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub ordinal: Option<u64>,
    pub payload: Option<JsonView<'v>>,
}

impl<'v> Head<'v> {
    pub fn of(view: JsonView<'v>) -> Result<Self> {
        Ok(Self {
            view,
            ty: str_at(Some(view), "type")?,
            timestamp: str_at(Some(view), "timestamp")?.as_deref().and_then(parse_ts),
            ordinal: number(view.get("ordinal")?)?.and_then(|n| n.as_u64()),
            payload: view.get("payload")?,
        })
    }

    pub fn inherited(&self, cutoff: Option<u64>) -> bool {
        matches!((self.ordinal, cutoff), (Some(o), Some(cut)) if o < cut)
    }
}

pub(super) struct Payload<'v> {
    pub view: JsonView<'v>,
    pub ty: Option<String>,
    pub role: Option<String>,
}

impl<'v> Payload<'v> {
    pub fn of(view: JsonView<'v>) -> Result<Self> {
        let ty = str_at(Some(view), "type")?;
        let role = if ty.as_deref() == Some("message") {
            str_at(Some(view), "role")?
        } else {
            None
        };
        Ok(Self { view, ty, role })
    }
}

/// What the reader wants from each record.
#[derive(Clone, Copy)]
pub(super) struct Demand {
    /// Large bodies stay in the source ([`JsonView::lazy_text`]) and large tool arguments become
    /// checked source JSON.
    pub spans: bool,
    /// [`Tier::Discovery`]: only the listing facts (id, cwd, title, timestamps, the writer's
    /// version) are read; no message, session bag or system prompt is built.
    pub metadata: bool,
}

impl From<&stream::Demand> for Demand {
    fn from(demand: &stream::Demand) -> Self {
        Demand {
            spans: demand.spans,
            metadata: demand.tier == Tier::Discovery,
        }
    }
}

impl Demand {
    /// A body string by the demand's text rule; None for a nonstring.
    fn text(self, v: JsonView<'_>) -> Result<Option<Text>> {
        if !v.is_string()? {
            return Ok(None);
        }
        v.lazy_text(self.spans)
    }

    /// `text_parts` of a body field (a string, or the string `text` of each part joined by
    /// newlines) by the demand's text rule. Several parts under spans are joined into the
    /// source's private decoded backing, never owned.
    fn joined(self, v: Option<JsonView<'_>>, source: Option<&crate::json::JsonSource>) -> Result<Text> {
        use crate::harness::wire;
        let backing = source
            .filter(|_| self.spans)
            .map_or(wire::Backing::Inline, wire::Backing::Source);
        wire::joined(v, wire::TEXT, backing, &mut |v| self.text(v))
    }
}

/// Visit the characters of `text_parts(v)` (a string, or the string `text` of each part joined by
/// newlines), decoded in place, until `visit` returns false.
fn for_each_joined_char(v: Option<JsonView<'_>>, visit: impl FnMut(char) -> bool) -> Result<()> {
    for_each_joined_char_of(v, None, visit)
}

/// [`for_each_joined_char`] over only the parts whose `type` is `only`, when given.
pub(super) fn for_each_joined_char_of(
    v: Option<JsonView<'_>>,
    only: Option<&str>,
    mut visit: impl FnMut(char) -> bool,
) -> Result<()> {
    let Some(v) = v else { return Ok(()) };
    let mut going = true;
    if v.is_string()? {
        v.for_each_char(|c| {
            going = visit(c);
            going
        })?;
    } else if let Some(parts) = v.array()? {
        let mut prior = false;
        for part in parts {
            let part = part?;
            if let Some(only) = only {
                if str_at(Some(part), "type")?.as_deref() != Some(only) {
                    continue;
                }
            }
            let Some(text) = part.get("text")?.filter(|t| t.is_string().unwrap_or(false)) else {
                continue;
            };
            if prior && !visit('\n') {
                return Ok(());
            }
            prior = true;
            text.for_each_char(|c| {
                going = visit(c);
                going
            })?;
            if !going {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Is `text_parts(v)` one of Codex's injected preambles ([`is_injected_text`])? Reads only the head.
fn injected(v: Option<JsonView<'_>>) -> Result<bool> {
    let mut head = String::new();
    for_each_joined_char(v, |c| {
        if !(head.is_empty() && c.is_whitespace()) {
            head.push(c);
        }
        head.len() < 64
    })?;
    Ok(is_injected_text(&head))
}

/// `text_parts(v) == expected`, decoded in place.
fn joined_eq(v: Option<JsonView<'_>>, expected: &str) -> Result<bool> {
    let mut rest = expected.chars();
    let mut equal = true;
    for_each_joined_char(v, |c| {
        equal = rest.next() == Some(c);
        equal
    })?;
    Ok(equal && rest.next().is_none())
}

fn get<'a>(v: Option<JsonView<'a>>, key: &str) -> Result<Option<JsonView<'a>>> {
    Ok(v.map(|v| v.get(key)).transpose()?.flatten())
}

fn string(v: Option<JsonView<'_>>) -> Result<Option<String>> {
    match v {
        Some(v) if v.is_string()? => Ok(v.resolve()?.as_str().map(str::to_owned)),
        _ => Ok(None),
    }
}

fn str_at(v: Option<JsonView<'_>>, key: &str) -> Result<Option<String>> {
    string(get(v, key)?)
}

/// The value, unless absent or null.
fn nonnull(v: Option<JsonView<'_>>) -> Result<Option<Value>> {
    match v {
        Some(v) if !v.is_null()? => Ok(Some(v.resolve()?.into_owned())),
        _ => Ok(None),
    }
}

fn number(v: Option<JsonView<'_>>) -> Result<Option<serde_json::Number>> {
    Ok(v.map(JsonView::as_number).transpose()?.flatten())
}

fn u64_at(v: Option<JsonView<'_>>, key: &str) -> Result<Option<u64>> {
    Ok(number(get(v, key)?)?.and_then(|n| n.as_u64()))
}

fn i64_at(v: Option<JsonView<'_>>, key: &str) -> Result<Option<i64>> {
    Ok(number(get(v, key)?)?.and_then(|n| n.as_i64()))
}

/// Keys of an object in source order, a repeated key in its first place (a `Value` map's keys).
fn keys(v: Option<JsonView<'_>>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    if let Some(v) = v {
        v.for_each_entry(|key, _| {
            if !out.iter().any(|k| k == key) {
                out.push(key.to_owned());
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// The per-record state a reader carries between records.
pub(super) struct Reader<'s> {
    pub session: &'s mut Session,
    pub ctx: &'s mut CodexCtx,
    pub out: &'s mut Vec<Message>,
    pub has_events: bool,
    pub demand: Demand,
    /// The captured source, for joining multi-part bodies under spans.
    pub source: Option<&'s crate::json::JsonSource>,
}

impl Reader<'_> {
    /// One rollout record: fold its facts into the session and push its messages.
    pub(super) fn record(&mut self, v: JsonView<'_>) -> Result<()> {
        let head = Head::of(v)?;
        let ty = &head.ty;
        let ts = head.timestamp;
        if let Some(ts) = ts {
            self.session.created_at.get_or_insert(ts);
            self.session.updated_at = Some(ts);
        }
        if self.demand.metadata {
            return self.listing(&head);
        }
        // Inherited prefix (fork/subagent rollouts embed the parent's history before
        // `subagent_history_start_ordinal`): those records still shape the thread's metadata — the
        // child inherits the parent's model/cwd — but they are the parent's turns, not this
        // thread's, so the lean passes emit no messages for them; `complete` keeps them, tagged.
        let inherited = head.inherited(self.ctx.inherit_before);
        let before = self.out.len();
        let payload = head.payload;
        match ty.as_deref() {
            None => {
                // bare {id,timestamp} header
                if self.session.id.is_empty() {
                    if let Some(id) = str_at(Some(head.view), "id")? {
                        self.session.id = id;
                    }
                }
            }
            Some("session_meta") => self.meta(payload)?,
            Some("turn_context") => self.turn_context(payload, ts)?,
            Some("event_msg") => {
                if !self.event(payload, ts)? && self.ctx.complete {
                    self.carry(v, ts)?;
                }
            }
            Some("response_item") => {
                if !self.item(payload, ts)? && self.ctx.complete {
                    self.carry(v, ts)?;
                }
            }
            Some("compacted") => self.compacted(payload, ts)?,
            Some("token_usage_record") => self.token_usage_record(payload, ts)?,
            // Precedes the `agent_message` it describes (verified on 0.154 rollouts: M at n, A at n+1).
            Some("inter_agent_communication_metadata") => {
                self.ctx.pending_trigger_turn = get(payload, "trigger_turn")?
                    .map(JsonView::as_bool)
                    .transpose()?
                    .flatten();
            }
            // `world_state`, `security_risk_score`, `retained_context`, `realtime_item`, and whatever
            // a newer Codex adds: bookkeeping with no conversational payload. Lean passes drop them;
            // `complete` carries them so nothing is lost and the census can count them.
            Some(_) => {
                if self.ctx.complete {
                    self.carry(v, ts)?;
                }
            }
        }
        if inherited {
            if self.ctx.complete {
                for m in &mut self.out[before..] {
                    bag(m).insert("inherited".into(), Value::Bool(true));
                }
            } else {
                self.out.truncate(before);
            }
        }
        Ok(())
    }

    /// Under `stream::Demand::COMPLETE`, a record with no conversational shape — one no arm knows, or
    /// a lifecycle marker the lean passes drop — becomes a [`MessageKind::Carrier`]: the verbatim
    /// record under the top-level `_record` key and its type in `extra["codex"]["record_type"]`, so
    /// a same-harness round-trip loses nothing and `cv formats` can count what the adapter does not
    /// model. The type is spelled as `formats/codex.toml` names it: the top-level `type`, qualified
    /// by `payload.type` for the two envelopes that have one (`event_msg.task_started`,
    /// `response_item.additional_tools`) — a bare `event_msg` would match no manifest entry.
    fn carry(&mut self, v: JsonView<'_>, ts: Option<DateTime<Utc>>) -> Result<()> {
        let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
        m.timestamp = ts;
        let ty = string(v.get("type")?)?.unwrap_or_default();
        let ty = match str_at(v.get("payload")?, "type")? {
            Some(sub) if matches!(ty.as_str(), "event_msg" | "response_item") => format!("{ty}.{sub}"),
            _ => ty,
        };
        bag(&mut m).insert("record_type".into(), Value::String(ty));
        m.extra.insert(crate::ir::CARRIER_KEY.into(), v.resolve()?.into_owned());
        self.out.push(m);
        Ok(())
    }

    /// The metadata demand: what a listing shows. The id and cwd come from the header or the
    /// first `session_meta` like the full read's; the title is this thread's first user words
    /// (an `event_msg` `user_message`, a user-role `message`, or a task another agent handed it),
    /// never an injected preamble or the parent history a subagent rollout embeds.
    fn listing(&mut self, head: &Head<'_>) -> Result<()> {
        let ty = head.ty.as_deref();
        if head.inherited(self.ctx.inherit_before) && ty != Some("session_meta") {
            return Ok(());
        }
        let p = head.payload;
        let s = &mut *self.session;
        match ty {
            None => {
                if s.id.is_empty() {
                    if let Some(id) = str_at(Some(head.view), "id")? {
                        s.id = id;
                    }
                }
            }
            Some("session_meta") => {
                if s.id.is_empty() {
                    if let Some(id) = str_at(p, "id")? {
                        s.id = id;
                    }
                }
                if s.cwd.is_none() {
                    s.cwd = str_at(p, "cwd")?.map(PathBuf::from);
                }
                if !self.ctx.meta_seen {
                    self.ctx.meta_seen = true;
                    self.ctx.writer_version = str_at(p, "cli_version")?;
                    self.ctx.inherit_before = u64_at(p, "subagent_history_start_ordinal")?;
                    self.ctx.agent_path = str_at(p, "agent_path")?;
                }
            }
            _ if s.title.is_some() => {}
            Some("event_msg") => {
                let payload = p.map(Payload::of).transpose()?;
                if payload.and_then(|p| p.ty).as_deref() == Some("user_message") {
                    if let Some(message) = get(p, "message")?.filter(|m| m.is_string().unwrap_or(false)) {
                        consider_title(&mut s.title, title_of(Some(message), None)?);
                    }
                }
            }
            Some("response_item") => {
                let payload = p.map(Payload::of).transpose()?;
                match payload.as_ref().and_then(|p| p.ty.as_deref()) {
                    // An absent role is `user`, as in the full read.
                    Some("message") if payload.as_ref().and_then(|p| p.role.as_deref()).unwrap_or("user") == "user" => {
                        consider_title(&mut s.title, title_of(get(p, "content")?, None)?);
                    }
                    // A task handed to this thread by another agent is its prompt — the only one a
                    // subagent thread usually gets (its `user`-role messages are the developer preamble).
                    Some("agent_message") if str_at(p, "author")? != self.ctx.agent_path => {
                        consider_title(&mut s.title, title_of(get(p, "content")?, Some("input_text"))?);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn meta(&mut self, payload: Option<JsonView<'_>>) -> Result<()> {
        let Some(p) = payload else { return Ok(()) };
        let s = &mut *self.session;
        if let Some(id) = str_at(Some(p), "id")? {
            if s.id.is_empty() {
                s.id = id;
            }
        }
        // Thread-level facts come from the FIRST meta only: a fork/subagent rollout embeds the
        // parent's `session_meta` as its second line (recorder.rs:1108-1112 treats the first as
        // canonical).
        if !self.ctx.meta_seen {
            self.ctx.meta_seen = true;
            for key in META_EXTRA_KEYS {
                if let Some(val) = nonnull(p.get(key)?)? {
                    sbag(s).insert((*key).to_string(), val);
                }
            }
            s.lineage.parent = str_at(Some(p), "parent_thread_id")?;
            s.lineage.forked_from = str_at(Some(p), "forked_from_id")?;
            s.lineage.agent_path = str_at(Some(p), "agent_path")?;
            // `base_instructions {text, provenance?}` (0.147+): the system prompt this thread runs on.
            s.system_prompt = str_at(p.get("base_instructions")?, "text")?.filter(|t| !t.trim().is_empty());
            self.ctx.inherit_before = u64_at(Some(p), "subagent_history_start_ordinal")?;
            self.ctx.agent_path = s.lineage.agent_path.clone();
            self.ctx.cv_wrote = str_at(Some(p), "originator")?.as_deref() == Some(CV_ORIGINATOR);
        }
        if s.cwd.is_none() {
            s.cwd = str_at(Some(p), "cwd")?.map(PathBuf::from);
        }
        if let Some(ts) = str_at(Some(p), "timestamp")?.as_deref().and_then(parse_ts) {
            s.created_at.get_or_insert(ts);
        }
        if s.git.is_none() {
            if let Some(g) = p.get("git")? {
                s.git = Some(GitInfo {
                    branch: str_at(Some(g), "branch")?,
                    commit: str_at(Some(g), "commit_hash")?,
                    remote: str_at(Some(g), "repository_url")?,
                });
            }
        }
        Ok(())
    }

    /// `turn_context` records persist per-turn config (model, effort, cwd, sandbox/approval). We
    /// seed the session-level `model`/`cwd`/`reasoning_effort` from the first one, and note every
    /// later model or effort change (see [`note_settings_change`]).
    fn turn_context(&mut self, payload: Option<JsonView<'_>>, ts: Option<DateTime<Utc>>) -> Result<()> {
        let Some(p) = payload else { return Ok(()) };
        if self.session.cwd.is_none() {
            self.session.cwd = str_at(Some(p), "cwd")?.map(PathBuf::from);
        }
        if let Some(policy) = str_at(Some(p), "approval_policy")? {
            crate::affordance::note_mode(sbag(self.session), &policy);
        }
        // 0.147+ also carries the collaboration-mode settings; `model` there mirrors the top-level one.
        let settings = get(p.get("collaboration_mode")?, "settings")?;
        let model = match str_at(Some(p), "model")? {
            Some(model) => Some(model),
            None => str_at(settings, "model")?,
        };
        let effort = match str_at(Some(p), "effort")? {
            Some(effort) => Some(effort),
            None => str_at(settings, "reasoning_effort")?,
        };
        let mut extra = Map::new();
        for key in ["turn_id", "effort", "cwd"] {
            if let Some(val) = nonnull(p.get(key)?)? {
                extra.insert(key.to_string(), val);
            }
        }
        note_settings_change(
            self.session,
            self.out,
            model.as_deref(),
            effort.as_deref(),
            ts,
            "turn_context",
            extra,
        );
        Ok(())
    }

    /// Handle an `event_msg` record. These are the UI-side mirror of the model exchange. We emit
    /// natural-language `user_message`/`agent_message` text (only when `has_events`, since otherwise
    /// the `response_item message`s carry it), surface `view_image_tool_call` as an image attachment
    /// (legacy files; paginated ones record it as an `item_completed` `ImageView`), attach
    /// `token_count` usage/rate-limit info to the assistant message it trails, turn paginated-mode
    /// `item_completed` items into structured system notes, follow `thread_settings_applied` model /
    /// effort changes, and note `turn_aborted` (a notice), `thread_rolled_back` (a branch) and
    /// persisted `error`s. Streaming deltas, approvals and `exec_command_*`/`web_search_*` events
    /// are never persisted (rollout/src/policy.rs), and `task_started`/`task_complete` carry nothing
    /// the items don't — except the last agent message, which is stashed session-level as a preview.
    ///
    /// Returns whether the record is represented in the IR (as a message, merged usage or session
    /// metadata); `false` lets the record carry verbatim under `stream::Demand::COMPLETE`.
    fn event(&mut self, payload: Option<JsonView<'_>>, ts: Option<DateTime<Utc>>) -> Result<bool> {
        let Some(p) = payload else { return Ok(false) };
        let has_events = self.has_events;
        let head = Payload::of(p)?;
        let p = head.view;
        match head.ty.as_deref() {
            Some("user_message") if has_events => {
                let body = p.get("message")?;
                if let Some(text) = body.map(|b| self.demand.text(b)).transpose()?.flatten() {
                    let mut m = if injected(body)? {
                        Message::of_kind(Role::User, MessageKind::InjectedContext, Origin::Harness)
                    } else {
                        Message::new(Role::User)
                    };
                    m.content.push(Block::Text { text });
                    m.timestamp = ts;
                    if let Some(kind) = str_at(Some(p), "kind")? {
                        bag(&mut m).insert("kind".into(), Value::String(kind));
                    }
                    self.out.push(m);
                }
            }
            Some("agent_message") if has_events => {
                if let Some(text) = p.get("message")?.map(|b| self.demand.text(b)).transpose()?.flatten() {
                    let mut m = Message::new(Role::Assistant);
                    m.timestamp = ts;
                    m.content.push(Block::Text { text });
                    self.out.push(m);
                }
            }
            Some("view_image_tool_call") => {
                // The agent attached a local image via the `view_image` tool.
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                m.content.push(Block::Image {
                    media_type: None,
                    data_ref: str_at(Some(p), "path")?,
                });
                let call_id = str_at(Some(p), "call_id")?;
                let b = bag(&mut m);
                if let Some(id) = call_id {
                    b.insert("call_id".into(), Value::String(id));
                }
                b.insert("codex_event".into(), Value::String("view_image_tool_call".into()));
                self.out.push(m);
            }
            // Usage totals + rate limits. Attach to the assistant message this event trails so the
            // numbers ride along with the turn they belong to; otherwise drop a bare carrier message
            // so the rate-limit snapshot isn't lost.
            Some("token_count") => self.token_count(p, ts)?,
            Some("item_completed") => self.item_completed(p, ts)?,
            Some("thread_settings_applied") => {
                // The reliable "settings changed" signal (protocol.rs:2194-2233): model, provider,
                // effort, personality, cwd, approval/permission profile. The next `turn_context` will
                // agree, so the model-change note fires here at most once.
                let Some(t) = p.get("thread_settings")?.filter(|t| t.is_object().unwrap_or(false)) else {
                    return Ok(true);
                };
                let model = str_at(Some(t), "model")?;
                let mut kept = Map::new();
                for key in [
                    "model",
                    "model_provider_id",
                    "reasoning_effort",
                    "reasoning_summary",
                    "personality",
                    "cwd",
                    "approval_policy",
                    "service_tier",
                ] {
                    if let Some(val) = nonnull(t.get(key)?)? {
                        kept.insert(key.to_string(), val);
                    }
                }
                let effort = str_at(Some(t), "reasoning_effort")?;
                if let Some(policy) = str_at(Some(t), "approval_policy")? {
                    crate::affordance::note_mode(sbag(self.session), &policy);
                }
                sbag(self.session).insert("thread_settings".into(), Value::Object(kept));
                note_settings_change(
                    self.session,
                    self.out,
                    model.as_deref(),
                    effort.as_deref(),
                    ts,
                    "thread_settings_applied",
                    Map::new(),
                );
            }
            Some("task_complete") => {
                // The turn's closing marker: its `last_agent_message` is the session preview; the
                // record itself has no conversational shape (carried under `complete`, see below).
                if let Some(last) = str_at(Some(p), "last_agent_message")? {
                    if !last.trim().is_empty() {
                        sbag(self.session).insert("last_agent_message".into(), Value::String(last));
                    }
                }
                return Ok(false);
            }
            Some("error") => {
                // A persisted API/usage error the model never answered — `ErrorEvent {message,
                // codex_error_info}` (protocol.rs:1930). CLI ≤ 0.1xx wrote them (88 in the local
                // 2026-04 corpus, all `usage_limit_exceeded`); the current policy no longer persists
                // `EventMsg::Error`, but old rollouts keep theirs.
                let message = str_at(Some(p), "message")?.unwrap_or_default();
                let message = message.trim();
                let text = if message.is_empty() {
                    "[error]".to_string()
                } else {
                    message.to_string()
                };
                let mut m = note(MessageKind::Error, ts, text);
                let mut error = Map::new();
                for (k, val) in p.resolve()?.as_object().into_iter().flatten() {
                    if k != "type" && !val.is_null() {
                        error.insert(k.clone(), val.clone());
                    }
                }
                let b = bag(&mut m);
                b.insert("codex_event".into(), Value::String("error".into()));
                b.insert("error".into(), Value::Object(error));
                self.out.push(m);
            }
            Some("turn_aborted") => {
                let reason = str_at(Some(p), "reason")?.unwrap_or_else(|| "unknown".into());
                let mut m = note(MessageKind::Notice, ts, format!("[turn aborted: {reason}]"));
                let b = bag(&mut m);
                b.insert("codex_event".into(), Value::String("turn_aborted".into()));
                // `error` (an `ErrorEvent`, 0.161+) says why; `root_turn_id` is 0.162+.
                for key in ["turn_id", "reason", "duration_ms", "error", "root_turn_id"] {
                    if let Some(val) = nonnull(p.get(key)?)? {
                        b.insert(key.to_string(), val);
                    }
                }
                self.out.push(m);
            }
            Some("thread_rolled_back") => {
                // `/undo`-style rollback: the last N turns left the model's context
                // (protocol.rs:3685) — what follows does not continue what precedes: a
                // [`MessageKind::Branch`].
                let n = u64_at(Some(p), "num_turns")?.unwrap_or(0);
                let mut m = note(
                    MessageKind::Branch,
                    ts,
                    format!("[rolled back {n} turn{}]", if n == 1 { "" } else { "s" }),
                );
                let b = bag(&mut m);
                b.insert("codex_event".into(), Value::String("thread_rolled_back".into()));
                b.insert("num_turns".into(), Value::from(n));
                self.out.push(m);
            }
            // Legacy-mode twins of records already represented — the `response_item`s, the top-level
            // `compacted`, `item_completed` activity (rollout/src/policy.rs:109-120 persists these
            // only under `history_mode: legacy`): silent in every pass, not unknown and not lost.
            Some(
                "user_message"
                | "agent_message"
                | "agent_reasoning"
                | "agent_reasoning_raw_content"
                | "patch_apply_end"
                | "mcp_tool_call_end"
                | "web_search_end"
                | "image_generation_end"
                | "context_compacted"
                | "sub_agent_activity",
            ) => {}
            // Lifecycle markers with no conversational shape and no twin (`task_started`,
            // `thread_goal_updated`, `entered_review_mode`/`exited_review_mode`) and whatever a
            // newer Codex adds: nothing to show in a lean pass; `complete` carries them verbatim.
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Paginated history mode records every finished `TurnItem` as an `item_completed` event
    /// (codex-rs/protocol/src/items.rs). The model-visible twins (`UserMessage`/`AgentMessage`/
    /// `Reasoning`/`FunctionCallOutput`/`ContextCompaction`) already arrive as `response_item`s, so
    /// those are skipped; the rest are UI-side facts with no `response_item` counterpart — command
    /// executions with exit codes, file changes as diffs, MCP calls with errors, viewed/generated
    /// images, sub-agent lifecycle, collaboration calls, plans, web searches — and become
    /// [`Role::System`] notes: a one-line summary as text (so `cv show` reads), the structured item
    /// in `extra.item`.
    ///
    /// Why notes and not `ToolResult`s: on real 0.154 rollouts the `CommandExecution` items PRECEDE
    /// the `custom_tool_call_output` they belong to, several map to one output (a unified-exec
    /// session runs many commands), and their `exec-…` ids share nothing with the `call_…` id — so
    /// there is no safe join, and a `ToolResult` with an unmatched `tool_use_id` would round-trip
    /// into an orphan tool result (which Claude Code rejects on resume). System notes are dropped by
    /// every emitter. The model saw none of this (its context is the `response_item`s), so the
    /// notes also add no tokens to doctor's attribution — and `ImageView`/image-gen items carry the
    /// path as text only, the image itself is already a `Block::Image` on the tool output the model
    /// received. Discovery's count reads none of it: notes are never turns.
    fn item_completed(&mut self, p: JsonView<'_>, ts: Option<DateTime<Utc>>) -> Result<()> {
        let Some(item) = p.get("item")? else { return Ok(()) };
        let item = Some(item);
        let ity = str_at(item, "type")?.unwrap_or_default();
        let str_of = |k: &str| -> Result<String> { Ok(str_at(item, k)?.unwrap_or_default()) };
        let strings = |v: Option<JsonView<'_>>| -> Result<Option<Vec<String>>> {
            let Some(array) = v.map(JsonView::array).transpose()?.flatten() else {
                return Ok(None);
            };
            let mut out = Vec::new();
            for v in array {
                if let Some(s) = string(Some(v?))? {
                    out.push(s);
                }
            }
            Ok(Some(out))
        };
        let text = match ity.as_str() {
            "CommandExecution" => {
                let cmd = strings(get(item, "command")?)?.unwrap_or_default().join(" ");
                let status = str_of("status")?;
                let exit = i64_at(item, "exit_code")?;
                let mut t = format!("$ {}", truncate(&cmd, 160));
                if !status.is_empty() {
                    t.push_str(&format!(" → {status}"));
                }
                if let Some(code) = exit {
                    t.push_str(&format!(" (exit {code})"));
                }
                t
            }
            "FileChange" => {
                let changes = get(item, "changes")?.filter(|c| c.is_object().unwrap_or(false));
                let paths = keys(changes)?;
                let status = str_of("status")?;
                format!(
                    "[file change{}] {}",
                    if status.is_empty() {
                        String::new()
                    } else {
                        format!(" {status}")
                    },
                    truncate(&paths.join(", "), 200)
                )
            }
            "McpToolCall" => {
                let err = nonnull(get(item, "error")?)?.map(|e| e.to_string());
                format!(
                    "[mcp {}.{}] {}{}",
                    str_of("server")?,
                    str_of("tool")?,
                    str_of("status")?,
                    err.map(|e| format!(": {}", truncate(&e, 120))).unwrap_or_default()
                )
            }
            "ImageView" => format!("[viewed image: {}]", str_of("path")?),
            "ImageGeneration" => format!("[generated image: {}]", str_of("saved_path")?),
            "Extension" => match str_of("kind")?.as_str() {
                "image_gen.generation" => format!("[generated image: {}]", str_of("saved_path")?),
                "web.search" => {
                    let queries = strings(get(get(item, "action")?, "queries")?)?
                        .map(|q| q.join(" · "))
                        .filter(|q| !q.is_empty());
                    let queries = match queries {
                        Some(q) => q,
                        None => str_of("query")?,
                    };
                    format!("[web search] {}", truncate(&queries, 200))
                }
                _ => return Ok(()), // clock.sleep and friends: nothing to say
            },
            "SubAgentActivity" => format!("[sub-agent {} {}]", str_of("agent_path")?, str_of("kind")?),
            "CollabAgentToolCall" => {
                let receivers = match get(item, "receiver_thread_ids")?
                    .map(JsonView::array)
                    .transpose()?
                    .flatten()
                {
                    Some(array) => array.count(),
                    None => 0,
                };
                format!(
                    "[collab {}] {}{}",
                    str_of("tool")?,
                    str_of("status")?,
                    if receivers > 0 {
                        format!(" → {receivers} agent(s)")
                    } else {
                        String::new()
                    }
                )
            }
            "Plan" => str_of("text")?,
            // Twins of `response_item`s already emitted (or, for `FunctionCallOutput`, the output's
            // own record) — nothing new to say.
            _ => return Ok(()),
        };
        if text.trim().is_empty() {
            return Ok(());
        }
        // Sub-agent lifecycle is first-class: a `started` activity is the spawn, `completed` the
        // return; everything else here is a harness notice.
        let kind = match (ity.as_str(), str_of("kind")?.as_str()) {
            ("SubAgentActivity", "started") => MessageKind::SubagentSpawn,
            ("SubAgentActivity", "completed") => MessageKind::SubagentReturn,
            _ => MessageKind::Notice,
        };
        let when = i64_at(Some(p), "completed_at_ms")?
            .and_then(DateTime::from_timestamp_millis)
            .or(ts);
        let mut m = note(kind, when, text);
        let mut extras = Vec::new();
        for key in ["turn_id", "started_at_ms", "completed_at_ms"] {
            if let Some(val) = nonnull(p.get(key)?)? {
                extras.push((key, val));
            }
        }
        if ity == "SubAgentActivity" {
            for key in ["agent_thread_id", "agent_path"] {
                if let Some(val) = nonnull(get(item, key)?)? {
                    extras.push((key, val));
                }
            }
        }
        let item = match item {
            Some(item) => item.resolve()?.into_owned(),
            None => Value::Null,
        };
        let b = bag(&mut m);
        b.insert(
            "codex_event".into(),
            Value::String(if ity == "Plan" {
                "plan".into()
            } else {
                "item_completed".into()
            }),
        );
        b.insert("item_type".into(), Value::String(ity));
        for (key, val) in extras {
            b.insert(key.to_string(), val);
        }
        b.insert("item".into(), item);
        self.out.push(m);
        Ok(())
    }

    /// Paginated rollouts record usage per model response as a top-level `token_usage_record`
    /// (protocol.rs:2258): `usage` for this response, plus running `turn_token_usage` /
    /// `thread_token_usage`, keyed by `response_id`/`turn_id`. It precedes the `token_count` event
    /// for the same response and is the authoritative figure — attach it to the trailing assistant
    /// message (same trailing-only rule as [`Self::token_count`]), or carry it if nothing trails.
    fn token_usage_record(&mut self, payload: Option<JsonView<'_>>, ts: Option<DateTime<Utc>>) -> Result<()> {
        let Some(p) = payload else { return Ok(()) };
        let Some(usage) = parse_usage(p.get("usage")?)? else {
            return Ok(());
        };
        self.ctx.pending_record_usage = Some(usage.clone());
        let attach = self
            .out
            .last()
            .is_some_and(|m| m.role == Role::Assistant && m.usage.is_none());
        if !attach {
            // A native usage event belongs to the assistant's accounting, but is not a reply.
            let mut nm = Message::of_kind(Role::Assistant, MessageKind::Notice, Origin::Harness);
            nm.timestamp = ts;
            bag(&mut nm).insert("codex_event".into(), Value::String(USAGE_FROM_RECORD.into()));
            self.out.push(nm);
        }
        let mut extras = Vec::new();
        for key in ["response_id", "turn_id", "thread_token_usage"] {
            if let Some(val) = nonnull(p.get(key)?)? {
                extras.push((key, val));
            }
        }
        let m = self.out.last_mut().unwrap();
        m.usage = Some(usage);
        let b = bag(m);
        b.insert("usage_source".into(), Value::String(USAGE_FROM_RECORD.into()));
        for (key, val) in extras {
            b.insert(key.to_string(), val);
        }
        Ok(())
    }

    /// Pull `last_token_usage` into IR [`Usage`] and stash rate-limit / context-window info in
    /// `extra`.
    fn token_count(&mut self, p: JsonView<'_>, ts: Option<DateTime<Utc>>) -> Result<()> {
        let info = p.get("info")?;
        let mut usage = parse_usage(get(info, "last_token_usage")?)?;
        // The twin of a `token_usage_record` already applied (same numbers; on real 0.154 files the
        // call's `item_completed` notes and its output sit between the two, so the record's message
        // no longer trails): its usage is a duplicate — keep only the rate-limit/context-window
        // snapshot. Both sides come from `parse_usage`, which records no cost (Codex stores
        // none), so `Usage` equality is token equality here.
        let twin_of_record = match (&usage, &self.ctx.pending_record_usage) {
            (Some(u), Some(r)) => u == r,
            _ => false,
        };
        if twin_of_record {
            self.ctx.pending_record_usage = None;
            usage = None;
        }
        // A re-emitted snapshot: the running total has not moved since the last `token_count`.
        if let Some(t) = nonnull(get(info, "total_token_usage")?)? {
            if self.ctx.last_total_usage.as_ref() == Some(&t) {
                usage = None;
            }
            self.ctx.last_total_usage = Some(t);
        }
        let rate_limits = nonnull(p.get("rate_limits")?)?;
        let ctx_window = nonnull(get(info, "model_context_window")?)?;
        if usage.is_none() && rate_limits.is_none() && ctx_window.is_none() {
            return Ok(());
        }
        // Attach to the *trailing* assistant message — the just-finished turn this event reports
        // on. Trailing-only (no reach-back past intervening records): a `token_count` that doesn't
        // directly follow its assistant message reports stale `last_token_usage` (e.g. the snapshot
        // re-emitted after a user message), so attaching it further back would mislabel an older
        // turn. It also keeps the streaming reader exact: it holds back only the trailing assistant
        // message (see [`flush_all_but_held`]), and with this rule that's the only message ever
        // targeted. A `token_usage_record` already gave the trailing assistant message its
        // (authoritative) usage: this is its `token_count` twin — merge the rate-limit/context-window
        // snapshot, keep the usage.
        let from_record = self.out.last().is_some_and(|m| {
            m.role == Role::Assistant && bag_get(m, "usage_source").and_then(Value::as_str) == Some(USAGE_FROM_RECORD)
        });
        if twin_of_record && !from_record && rate_limits.is_none() && ctx_window.is_none() {
            return Ok(()); // the twin carried nothing beyond the usage already attached elsewhere
        }
        let attach = from_record
            || self
                .out
                .last()
                .is_some_and(|m| m.role == Role::Assistant && m.usage.is_none());
        if !attach {
            // A native usage event belongs to the assistant's accounting, but is not a reply.
            let mut nm = Message::of_kind(Role::Assistant, MessageKind::Notice, Origin::Harness);
            nm.timestamp = ts;
            bag(&mut nm).insert("codex_event".into(), Value::String("token_count".into()));
            self.out.push(nm);
        }
        let m = self.out.last_mut().unwrap();
        if let Some(u) = usage {
            if !from_record {
                m.usage = Some(u);
            }
        }
        let b = bag(m);
        if let Some(rl) = rate_limits {
            b.insert("rate_limits".into(), rl);
        }
        if let Some(cw) = ctx_window {
            b.insert("model_context_window".into(), cw);
        }
        Ok(())
    }

    /// A top-level `compacted` record marks an auto/manual history-compaction boundary
    /// ([`MessageKind::CompactionBoundary`]); when it carries a human `message`, that summary
    /// follows as a [`MessageKind::CompactionSummary`] (usually `""` on 0.147+ files, where the
    /// model-visible replacement lives in `replacement_history`, whose length rides in the bag).
    /// The history itself (often the largest record of a rollout) is never resolved.
    fn compacted(&mut self, payload: Option<JsonView<'_>>, ts: Option<DateTime<Utc>>) -> Result<()> {
        let Some(p) = payload else { return Ok(()) };
        let mut m = note(MessageKind::CompactionBoundary, ts, "[history compacted]".into());
        let history = p
            .get("replacement_history")?
            .map(JsonView::array)
            .transpose()?
            .flatten()
            .map(Iterator::count);
        let mut extras = Vec::new();
        for key in ["window_number", "window_id", "compaction_response_id"] {
            if let Some(val) = nonnull(p.get(key)?)? {
                extras.push((key, val));
            }
        }
        let b = bag(&mut m);
        b.insert("codex_event".into(), Value::String("compacted".into()));
        if let Some(n) = history {
            b.insert("replacement_history_len".into(), Value::Number(n.into()));
        }
        for (key, val) in extras {
            b.insert(key.to_string(), val);
        }
        self.out.push(m);
        let summary = str_at(Some(p), "message")?.unwrap_or_default();
        if !summary.trim().is_empty() {
            let mut sm = note(MessageKind::CompactionSummary, ts, summary);
            bag(&mut sm).insert("codex_event".into(), Value::String("compacted".into()));
            self.out.push(sm);
        }
        Ok(())
    }

    /// A `response_item` payload or a legacy `items[]` entry. Returns whether it is represented.
    pub(super) fn item(&mut self, payload: Option<JsonView<'_>>, ts: Option<DateTime<Utc>>) -> Result<bool> {
        let Some(p) = payload else { return Ok(false) };
        let d = self.demand;
        let head = Payload::of(p)?;
        let ty = head.ty.as_deref().unwrap_or_default();
        let field = |k: &str| -> Result<String> { Ok(str_at(Some(p), k)?.unwrap_or_default()) };
        let passthrough = p.get("internal_chat_message_metadata_passthrough")?;
        let ts = passthrough_ts(passthrough)?.or(ts);
        let turn_id = nonnull(get(passthrough, "turn_id")?)?;
        let before = self.out.len();
        let out = &mut *self.out;

        match ty {
            "message" => {
                let role = head.role.as_deref().unwrap_or("user");
                // Natural-language user/assistant text is taken from event_msg when present.
                if self.has_events && (role == "user" || role == "assistant") {
                    return Ok(true);
                }
                // A message can mix text and images; emit one message carrying all blocks in order.
                let content = p.get("content")?;
                let blocks = openai::blocks_from_view(content, &openai::CODEX, &mut |v| d.text(v))?;
                if !blocks.is_empty() {
                    let mut m = match role {
                        "assistant" => Message::new(Role::Assistant),
                        // `developer`/`system` messages are instructions the harness injected
                        // (`<user_instructions>`, AGENTS.md); never the user's own words.
                        "developer" | "system" => {
                            Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness)
                        }
                        _ if injected(content)? => {
                            Message::of_kind(Role::User, MessageKind::InjectedContext, Origin::Harness)
                        }
                        _ => Message::new(Role::User),
                    };
                    m.timestamp = ts;
                    if let Some(phase) = str_at(Some(p), "phase")? {
                        bag(&mut m).insert("phase".into(), Value::String(phase));
                    }
                    m.content = blocks;
                    out.push(m);
                }
            }
            "reasoning" => {
                // `summary` holds the user-visible reasoning summary; `content` (when present) holds
                // the raw chain-of-thought. Prefer raw content, fall back to summary.
                // `encrypted_content` carries the opaque replay blob.
                let summary_view = p.get("summary")?;
                let raw = d.joined(p.get("content")?, self.source)?;
                let encrypted = string(p.get("encrypted_content")?)?;
                // When we surfaced raw content but a distinct summary also exists, keep the summary.
                let (text, summary) = if raw.is_empty() {
                    (d.joined(summary_view, self.source)?, None)
                } else {
                    let inline = Demand {
                        spans: false,
                        metadata: false,
                    };
                    let summary = match inline.joined(summary_view, None)? {
                        Text::Inline(summary) => summary,
                        _ => unreachable!("inline demand"),
                    };
                    let distinct = !summary.is_empty() && !joined_eq(p.get("content")?, &summary)?;
                    (raw, distinct.then_some(summary))
                };
                if !text.is_empty() || encrypted.is_some() {
                    let mut m = Message::new(Role::Assistant);
                    m.timestamp = ts;
                    if let Some(summary) = summary {
                        bag(&mut m).insert("reasoning_summary".into(), Value::String(summary));
                    }
                    m.content.push(Block::Thinking {
                        text,
                        signature: None,
                        encrypted,
                        redacted: false,
                    });
                    out.push(m);
                }
            }
            // Inter-agent (swarm) message: `{id, author, recipient, content:[input_text |
            // encrypted_content]}` (models.rs:1036), produced by `InterAgentCommunication`
            // (protocol.rs:880-915) and preceded by a top-level `inter_agent_communication_metadata`
            // line carrying `trigger_turn`. From this thread's point of view a message it authored
            // is assistant output; one addressed to it is (user-role) input. The payload body is
            // usually an `encrypted_content` item — only the header text is readable.
            "agent_message" => {
                let author = field("author")?;
                let me = self.ctx.agent_path.as_deref().unwrap_or("/root");
                // Authored here: this thread's reply; addressed to it: a prompt — either way it came
                // from another agent, not a person.
                let mut m = if author == me {
                    Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Subagent)
                } else {
                    Message::of_kind(Role::User, MessageKind::Prompt, Origin::Subagent)
                };
                m.timestamp = ts;
                let mut encrypted = false;
                if let Some(items) = p.get("content")?.map(JsonView::array).transpose()?.flatten() {
                    for it in items {
                        let it = it?;
                        match str_at(Some(it), "type")?.as_deref() {
                            Some("encrypted_content") => encrypted = true,
                            Some("input_text") => {
                                if let Some(text) = it.get("text")?.map(|t| d.text(t)).transpose()?.flatten() {
                                    if !text.is_empty() {
                                        m.content.push(Block::Text { text });
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let trigger = self.ctx.pending_trigger_turn.take();
                let recipient = field("recipient")?;
                let b = bag(&mut m);
                b.insert("codex_event".into(), Value::String("agent_message".into()));
                b.insert("author".into(), Value::String(author));
                b.insert("recipient".into(), Value::String(recipient));
                if encrypted {
                    b.insert("encrypted".into(), Value::Bool(true));
                }
                if let Some(trigger) = trigger {
                    b.insert("trigger_turn".into(), Value::Bool(trigger));
                }
                out.push(m);
            }
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                // local_shell_call may carry only a legacy `id`.
                let call_id = match str_at(Some(p), "call_id")? {
                    Some(id) => id,
                    None => field("id")?,
                };
                let (name, input) = if ty == "local_shell_call" {
                    // {status, action:{type:"exec", command:[...], ...}}
                    let mut obj = Map::new();
                    for key in ["action", "status"] {
                        if let Some(v) = p.get(key)? {
                            obj.insert(key.into(), v.resolve()?.into_owned());
                        }
                    }
                    ("local_shell".to_string(), Json::Owned(Value::Object(obj)))
                } else {
                    // Tool `arguments`/`input` arrive as JSON-encoded *strings* (`custom_tool_call`'s
                    // `input` is freeform text).
                    let args = match p.get("arguments")? {
                        Some(args) if args.is_string()? => Some(args),
                        _ => p.get("input")?,
                    };
                    let input = match args {
                        None => Json::Owned(Value::Null),
                        Some(args) if args.is_string()? && d.spans => args.encoded_argument()?,
                        Some(args) if args.is_string()? => Json::Owned(openai::structured_arguments(
                            args.resolve()?.as_str().unwrap_or_default(),
                        )),
                        Some(args) => args.argument(d.spans)?,
                    };
                    (field("name")?, input)
                };
                let mut m = Message::new(Role::Assistant);
                if name == "spawn_agent"
                    && sbag(self.session).get("history_mode").and_then(Value::as_str) != Some("paginated")
                {
                    m.kind = MessageKind::SubagentSpawn;
                }
                m.timestamp = ts;
                if let Some(status) = str_at(Some(p), "status")? {
                    bag(&mut m).insert("status".into(), Value::String(status));
                }
                // `namespace` (0.147+, e.g. `collaboration` for `send_message`/`spawn`/`wait`) is
                // first-class on the block; `name` stays the bare tool name so per-tool stats keep
                // grouping.
                m.content.push(Block::ToolUse {
                    id: call_id,
                    name,
                    input,
                    namespace: str_at(Some(p), "namespace")?,
                });
                out.push(m);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = field("call_id")?;
                // Error-ness: cv's own `[error] ` string convention (see [`ERROR_PREFIX`]) in a
                // rollout cv wrote, or the legacy object form `{success:false}` /
                // `{metadata:{exit_code≠0}}` some older recorders wrote. Real Codex 0.147+ files
                // carry neither — a failed command's exit code lives only in the `item_completed
                // CommandExecution` note.
                let output = p.get("output")?;
                let (content, images, is_error) = match output {
                    Some(o) if o.is_string()? => {
                        const PATCH_ERR_PREFIX: &str = "apply_patch verification failed";
                        let want = if self.ctx.cv_wrote {
                            ERROR_PREFIX.len().max(PATCH_ERR_PREFIX.len())
                        } else {
                            PATCH_ERR_PREFIX.len()
                        };
                        let mut prefix = String::new();
                        o.for_each_char(|c| {
                            prefix.push(c);
                            prefix.len() < want
                        })?;
                        let mut text = d.text(o)?.unwrap_or_default();
                        let cv_err = self.ctx.cv_wrote && prefix.starts_with(ERROR_PREFIX);
                        if cv_err {
                            text = text.with_skipped_prefix(ERROR_PREFIX.len())?;
                        }
                        let is_error = cv_err || prefix.starts_with(PATCH_ERR_PREFIX);
                        (text, Vec::new(), is_error)
                    }
                    _ => {
                        let (content, images) = coerce_output(output)?;
                        (Text::Inline(content), images, output_is_error(output)?)
                    }
                };
                let mut m = Message::new(Role::Tool);
                m.timestamp = ts;
                // 0.147+ outputs name their tool (`name`, `namespace`; models.rs:1113-1131).
                let tool_name = str_at(Some(p), "name")?;
                if let Some(ns) = str_at(Some(p), "namespace")? {
                    bag(&mut m).insert("namespace".into(), Value::String(ns));
                }
                m.content.push(Block::ToolResult {
                    tool_use_id: call_id,
                    content,
                    is_error,
                    tool_name,
                    // The record's own status when it has one (none through 0.163: `call_id`,
                    // `output`, `id`); `is_error` already carries what cv can tell.
                    status: str_at(Some(p), "status")?,
                    details: None,
                });
                // Structured outputs can return image content items alongside text.
                m.content.extend(images);
                out.push(m);
            }
            "tool_search_call" => {
                let mut input = Map::new();
                for key in ["arguments", "execution"] {
                    if let Some(v) = p.get(key)? {
                        input.insert(key.into(), v.resolve()?.into_owned());
                    }
                }
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                m.content.push(Block::ToolUse {
                    id: field("call_id")?,
                    name: "tool_search".into(),
                    input: Value::Object(input).into(),
                    namespace: None,
                });
                out.push(m);
            }
            "tool_search_output" => {
                let content = match p.get("tools")? {
                    Some(tools) => tools.resolve()?.to_string(),
                    None => "[]".into(),
                };
                let mut m = Message::new(Role::Tool);
                m.timestamp = ts;
                m.content.push(Block::ToolResult {
                    tool_use_id: field("call_id")?,
                    content: content.into(),
                    is_error: false,
                    tool_name: Some("tool_search".into()),
                    status: None,
                    details: None,
                });
                out.push(m);
            }
            "web_search_call" => {
                // Model-side web search; surface the query/queries as a tool call.
                let call_id = match str_at(Some(p), "call_id")? {
                    Some(id) => id,
                    None => field("id")?,
                };
                let input = match p.get("action")? {
                    Some(action) => action.resolve()?.into_owned(),
                    None => Value::Null,
                };
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                if let Some(status) = str_at(Some(p), "status")? {
                    bag(&mut m).insert("status".into(), Value::String(status));
                }
                m.content.push(Block::ToolUse {
                    id: call_id,
                    name: "web_search".into(),
                    input: input.into(),
                    namespace: None,
                });
                out.push(m);
            }
            "image_generation_call" => {
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                if let Some(rp) = str_at(Some(p), "revised_prompt")? {
                    bag(&mut m).insert("revised_prompt".into(), Value::String(rp));
                }
                // `result` is base64 image data; record a reference, not the bytes.
                let result = p.get("result")?.map(JsonView::is_string).transpose()?.unwrap_or(false);
                m.content.push(Block::Image {
                    media_type: None,
                    data_ref: result.then(|| "base64:inline".to_string()),
                });
                out.push(m);
            }
            // Mid-history compaction recorded inline as a response_item (vs. the top-level
            // `compacted` record). Only an opaque encrypted blob survives; note the boundary.
            "compaction" | "compaction_summary" | "context_compaction" => {
                let encrypted = p
                    .get("encrypted_content")?
                    .map(JsonView::is_string)
                    .transpose()?
                    .unwrap_or(false);
                let mut m = note(MessageKind::CompactionBoundary, ts, "[history compacted]".into());
                let b = bag(&mut m);
                b.insert("codex_event".into(), Value::String(ty.to_owned()));
                if encrypted {
                    b.insert("encrypted".into(), Value::Bool(true));
                }
                out.push(m);
            }
            _ => return Ok(false),
        }
        // The turn this item belongs to (from the passthrough), on every message it produced.
        if let Some(tid) = turn_id {
            for m in &mut self.out[before..] {
                bag(m).insert("turn_id".into(), tid.clone());
            }
        }
        Ok(true)
    }
}

/// The real creation time of a `response_item`, when Codex recorded one:
/// `internal_chat_message_metadata_passthrough.create_time` (epoch seconds, float; models.rs:959).
/// A fork/subagent rollout stamps its inherited prefix with the FORK time as the envelope
/// `timestamp`, so this is the only honest per-item clock there.
fn passthrough_ts(passthrough: Option<JsonView<'_>>) -> Result<Option<DateTime<Utc>>> {
    let Some(secs) = number(get(passthrough, "create_time")?)?.and_then(|n| n.as_f64()) else {
        return Ok(None);
    };
    if !secs.is_finite() || secs <= 0.0 {
        return Ok(None);
    }
    Ok(DateTime::from_timestamp(
        secs.trunc() as i64,
        ((secs.fract()) * 1e9) as u32,
    ))
}

/// Codex token-usage block → IR [`Usage`]. `cached_input_tokens` maps to cache-read;
/// `cache_write_input_tokens` (added in 0.147, `#[serde(default)]`, protocol.rs:2235) to
/// cache-creation — absent on older files; `reasoning_output_tokens` to `reasoning_tokens`. Codex
/// stores no cost.
///
/// Codex's `input_tokens` *includes* both cache counts (its `total_tokens` is input + output), so
/// the IR's disjoint `input_tokens` is the remainder; `emit::codex_token_usage` adds them back.
pub(super) fn parse_usage(v: Option<JsonView<'_>>) -> Result<Option<Usage>> {
    let cache_read = u64_at(v, "cached_input_tokens")?;
    let cache_write = u64_at(v, "cache_write_input_tokens")?;
    let u = Usage {
        input_tokens: u64_at(v, "input_tokens")?
            .map(|i| i.saturating_sub(cache_read.unwrap_or(0).saturating_add(cache_write.unwrap_or(0)))),
        output_tokens: u64_at(v, "output_tokens")?,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_write,
        reasoning_tokens: u64_at(v, "reasoning_output_tokens")?,
        cost_usd: None,
    };
    if u.input_tokens.is_none() && u.output_tokens.is_none() && u.cache_read_tokens.is_none() {
        return Ok(None);
    }
    Ok(Some(u))
}

/// Coerce a non-string tool-call `output` into text plus any image blocks. On the wire `output` is
/// one of: a plain string; an object like `{output, metadata}` (legacy) or `{success, content}`;
/// or an array of structured content items (`input_text`/`input_image`/`encrypted_content`).
fn coerce_output(v: Option<JsonView<'_>>) -> Result<(String, Vec<Block>)> {
    let Some(v) = v else {
        return Ok((String::new(), Vec::new()));
    };
    if let Some(s) = string(Some(v))? {
        return Ok((s, Vec::new()));
    }
    if let Some(items) = v.array()? {
        let mut text = Vec::new();
        let mut images = Vec::new();
        for it in items {
            let it = it?;
            match str_at(Some(it), "type")?.as_deref() {
                Some("input_image") | Some("output_image") => {
                    images.extend(openai::part_from_view(it, &openai::CODEX, &mut |v| v.text(false))?)
                }
                Some("encrypted_content") => {}
                _ => {
                    if let Some(t) = str_at(Some(it), "text")? {
                        text.push(t);
                    }
                }
            }
        }
        return Ok((text.join("\n"), images));
    }
    if v.is_object()? {
        // `{output: ...}` may itself nest a string or an array of content items.
        let inner = v.get("output")?;
        if let Some(inner) = inner.filter(|i| i.is_string().unwrap_or(false) || i.is_array().unwrap_or(false)) {
            return coerce_output(Some(inner));
        }
        // `{content: "...", success: bool}` or unknown — best-effort string.
        let s = match str_at(Some(v), "content")? {
            Some(s) => s,
            None => v.resolve()?.to_string(),
        };
        return Ok((s, Vec::new()));
    }
    Ok((v.resolve()?.to_string(), Vec::new()))
}

/// Detect a failed tool result from the `success`/`exit_code`/`metadata` markers or `exec`
/// `"Script failed\n"` / `"Script error:\n"` content items Codex emits.
fn output_is_error(v: Option<JsonView<'_>>) -> Result<bool> {
    let Some(v) = v else {
        return Ok(false);
    };
    if let Some(items) = v.array()? {
        for it in items {
            let it = it?;
            if let Some(text) = it.get("text")?.filter(|t| t.is_string().unwrap_or(false)) {
                let mut head = String::new();
                text.for_each_char(|c| {
                    head.push(c);
                    head.len() < 16
                })?;
                if head.starts_with("Script failed\n")
                    || head == "Script failed"
                    || head.starts_with("Script error:\n")
                    || head.starts_with("Script error:")
                {
                    return Ok(true);
                }
            }
        }
        return Ok(false);
    }
    if !v.is_object()? {
        return Ok(false);
    }
    if v.get("success")?.map(JsonView::as_bool).transpose()?.flatten() == Some(false) {
        return Ok(true);
    }
    if matches!(i64_at(v.get("metadata")?, "exit_code")?, Some(c) if c != 0) {
        return Ok(true);
    }
    if let Some(inner) = v.get("output")? {
        if inner.is_array()? || inner.is_object()? {
            return output_is_error(Some(inner));
        }
    }
    Ok(false)
}
