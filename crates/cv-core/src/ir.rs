//! The unified intermediate representation (IR).
//!
//! Every harness parses *into* these types, and cross-harness conversion emits *out of* them.
//! The IR is deliberately a superset: fields that a given harness doesn't have are `None`/empty,
//! and harness-specific extras ride along in [`Message::extra`] so conversions can be as lossless
//! as the target allows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Declares the [`Harness`] enum together with [`Harness::ALL`] and [`Harness::as_str`] from one
/// variant list, so they can never drift apart (the old hand-maintained 17-element `ALL` array
/// silently went stale whenever a variant was added). Adding a harness is now a single line here
/// (plus its aliases in [`Harness::parse`], which a test ties back to `ALL`).
macro_rules! harnesses {
    ($( $(#[$meta:meta])* $name:ident => $str:literal ),+ $(,)?) => {
        /// Which agent harness a session came from.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum Harness {
            $( $(#[$meta])* $name, )+
        }

        impl Harness {
            /// How many harnesses exist (generated from the variant list).
            pub const COUNT: usize = [$($str),+].len();

            /// Every harness, in declaration order — generated from the same list as the enum
            /// itself, so it is exhaustive by construction.
            pub const ALL: [Harness; Harness::COUNT] = [$(Harness::$name),+];

            /// The canonical lowercase name (what `--harness` accepts and listings print).
            pub fn as_str(self) -> &'static str {
                match self {
                    $( Harness::$name => $str, )+
                }
            }
        }
    };
}

harnesses! {
    Claude => "claude",
    Codex => "codex",
    Grok => "grok",
    OpenCode => "opencode",
    Gemini => "gemini",
    Hermes => "hermes",
    OpenClaw => "openclaw",
    /// Cursor IDE (chat/composer history in `state.vscdb`).
    Cursor => "cursor",
    /// The Claude desktop app (macOS/Windows).
    ClaudeApp => "claude-app",
    /// The ChatGPT desktop app (macOS/Windows).
    ChatGptApp => "chatgpt-app",
    /// Kimi CLI (MoonshotAI), `~/.kimi` — the legacy store, frozen since the 2026-06 migration.
    Kimi => "kimi",
    /// Kimi Code (MoonshotAI), kimi-cli's successor: `~/.kimi-code/sessions/**/wire.jsonl`.
    KimiCode => "kimi-code",
    /// Qwen Code CLI (a gemini-cli fork), `~/.qwen`.
    Qwen => "qwen",
    /// LM Studio desktop app, `~/.lmstudio`.
    LmStudio => "lmstudio",
    /// Cline (VS Code extension), per-task Anthropic-format JSON.
    Cline => "cline",
    /// Roo Code (a Cline fork).
    Roo => "roo",
    /// Continue (VS Code/JetBrains), `~/.continue/sessions`.
    Continue => "continue",
    /// Goose (Block), `~/.local/share/goose/sessions`.
    Goose => "goose",
    /// Zed editor's agent panel (`threads.db` SQLite, zstd-compressed JSON thread blobs).
    Zed => "zed",
    /// ChatGPT account **data export** (`conversations.json`, a `mapping` DAG per conversation).
    ChatGptExport => "chatgpt-export",
    /// Claude.ai account **data export** (`conversations.json`, linear `chat_messages[]`).
    ClaudeExport => "claude-export",
}

impl Harness {
    pub fn parse(s: &str) -> Option<Harness> {
        Some(match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Harness::Claude,
            "codex" => Harness::Codex,
            "grok" => Harness::Grok,
            "opencode" | "oc" => Harness::OpenCode,
            "gemini" | "antigravity" => Harness::Gemini,
            "hermes" | "hermes-agent" => Harness::Hermes,
            "openclaw" | "claw" => Harness::OpenClaw,
            "cursor" => Harness::Cursor,
            "claude-app" | "claude-desktop" | "claudeapp" => Harness::ClaudeApp,
            "chatgpt-app" | "chatgpt" | "chatgpt-desktop" | "openai-app" => Harness::ChatGptApp,
            "kimi" | "kimi-cli" => Harness::Kimi,
            "kimi-code" | "kimicode" | "kimi2" => Harness::KimiCode,
            "qwen" | "qwen-code" => Harness::Qwen,
            "lmstudio" | "lm-studio" => Harness::LmStudio,
            "cline" => Harness::Cline,
            "roo" | "roo-code" | "roocode" => Harness::Roo,
            "continue" | "continuedev" => Harness::Continue,
            "goose" => Harness::Goose,
            "zed" | "zed-editor" => Harness::Zed,
            "chatgpt-export" | "openai-export" | "chatgpt-data" | "openai-data" => Harness::ChatGptExport,
            "claude-export" | "claude-ai" | "claudeai" | "claude-data" => Harness::ClaudeExport,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fully-parsed session in the unified representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub harness: Harness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    /// The system prompt the harness sent, when the store keeps it (Hermes, Kimi Code, OpenClaw,
    /// Claude `prompt_snapshot`, Codex `base_instructions`). Session-level; NOT a message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Where this session came from and where it went (fork, parent, continuation).
    #[serde(default, skip_serializing_if = "Lineage::is_empty")]
    pub lineage: Lineage,
    pub messages: Vec<Message>,
    /// Where this session was read from on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    /// Harness-specific session facts, nested under the harness name — `extra["claude"]["custom_title"]`,
    /// `extra["codex"]["history_mode"]` — never flat. Shared concepts have first-class fields
    /// (`system_prompt`, `lineage`, …). See `docs/INTERFACE-V2.md` §4.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Where a session came from and where it went. Every field is a session id in the SAME harness
/// unless the harness records otherwise; `None` means "the store does not say".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    /// The session this one was forked or branched from (Codex `forked_from_id`, OpenClaw fork,
    /// Hermes `_branched_from`, Claude `--fork-session`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// The session that owns this one as a sub-agent (Codex `parent_thread_id`, the parent of a
    /// Claude `agent-*` transcript, Kimi `parentAgentId`, Hermes `_delegate_from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The tool call in the parent that spawned this session (Claude `toolUseId`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by_tool_use: Option<String>,
    /// The session this one continued in (Claude `continued-in`, Hermes compression rotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_in: Option<String>,
    /// The session this one continues (the inverse pointer, when the store records it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// Sub-agent path or nickname when the harness has one (Codex `agent_path`, Kimi `agent-N`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_path: Option<String>,
}

impl Lineage {
    pub fn is_empty(&self) -> bool {
        self == &Lineage::default()
    }
}

/// The namespace inside `extra` for facts cv itself produces rather than reads from a harness:
/// parse diagnostics like `skipped_lines`, and the provenance cv stamps on a session it synthesized
/// (`loom`, `splice`). It is spelled `"cv"`, which `Harness::parse` never yields, so it can never
/// collide with a harness bag. See `docs/INTERFACE-V2.md` §4.
pub const CV_NAMESPACE: &str = "cv";

impl Session {
    /// cv's own fact bag — `extra["cv"]` as an object, created on demand. For things cv produced
    /// itself; anything read out of a harness store belongs in that harness's bag instead.
    pub fn cv_extra_mut(&mut self) -> &mut serde_json::Map<String, serde_json::Value> {
        bag_mut(&mut self.extra, CV_NAMESPACE)
    }

    /// cv's own fact bag, if anything was recorded in it.
    pub fn cv_extra(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.extra.get(CV_NAMESPACE).and_then(serde_json::Value::as_object)
    }

    /// The harness-specific fact bag for `h` — `extra[h.as_str()]` as an object, created on demand.
    /// Adapters write here, never at the top level of `extra`.
    pub fn harness_extra_mut(&mut self, h: Harness) -> &mut serde_json::Map<String, serde_json::Value> {
        harness_bag_mut(&mut self.extra, h)
    }

    /// The harness-specific fact bag for `h`, if any facts were recorded.
    pub fn harness_extra(&self, h: Harness) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.extra.get(h.as_str()).and_then(serde_json::Value::as_object)
    }

    /// First non-empty user text, used as a fallback title / preview.
    pub fn first_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.text().filter(|t| !t.trim().is_empty()))
    }

    /// A short human label for listings.
    pub fn label(&self) -> String {
        label_from(self.title.as_deref(), self.first_user_text().as_deref())
    }

    /// The best title for a machine listing: the explicit [`title`](Session::title) when present,
    /// otherwise one synthesized from the first user message that reads as a real prompt — skipping
    /// the `Caveat:` command preamble, bare `<system-reminder>`/command-wrapper turns, and peeling a
    /// leading `<system-reminder>` block off an otherwise-real turn (tool results are [`Role::Tool`],
    /// already excluded). `None` only when a session carries no explicit title and no user prose.
    ///
    /// Resolver-aware and bounded (reads at most a few KB of any one message via
    /// [`Text::resolve_prefix`](crate::lazy::Text::resolve_prefix)), so it never panics on a lazy
    /// [`Span`](crate::lazy::Span) and never materializes a giant paste — safe to call on a
    /// [`ParseOptions::lazy`](crate::ParseOptions::lazy) parse. The result is transcript-derived text
    /// (untrusted): JSON consumers get it raw; a terminal seam must sanitize it.
    pub fn synth_title(&self) -> Option<String> {
        if let Some(t) = &self.title {
            return Some(t.clone());
        }
        // A title is short; reading the head of each candidate is plenty and keeps a multi-megabyte
        // first message from being materialized in full.
        const HEAD: u64 = 8 * 1024;
        let resolver = self.resolver();
        self.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .find_map(|m| {
                let mut s = String::new();
                for b in &m.content {
                    if let Block::Text { text } = b {
                        if !s.is_empty() {
                            s.push('\n');
                        }
                        s.push_str(&text.resolve_prefix(&resolver, HEAD));
                    }
                }
                meaningful_prompt(&s)
            })
            .map(|t| truncate(&t, 80))
    }

    /// All textual content concatenated — used to build a search index.
    ///
    /// The per-message projection lives in [`Message::append_searchable`]; this just prepends the
    /// title and folds every message into one `String`.
    pub fn searchable_text(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            out.push_str(t);
            out.push('\n');
        }
        for m in &self.messages {
            m.append_searchable(&mut out);
        }
        out
    }

    /// A [`Resolver`](crate::lazy::Resolver) for this session's lazy content spans, rooted at its
    /// `source_path`. Streaming consumers create one and resolve each block as it passes.
    pub fn resolver(&self) -> crate::lazy::Resolver {
        crate::lazy::Resolver::new(self.source_path.clone())
    }

    /// Resolve every lazy content [`Span`](crate::lazy::Span) in place, so the session owns all its
    /// content inline. Whole-session consumers (cross-harness emit building output in memory, JSON
    /// serialization) call this; streaming consumers resolve per-block instead. After it, every
    /// content `Text` is `Inline`, so `Deref`/`Display`/`searchable_text` are safe.
    pub fn materialize(&mut self) {
        let resolver = self.resolver();
        for m in &mut self.messages {
            m.materialize(&resolver);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool/function result fed back to the model. (Some harnesses model this as a `user` turn;
    /// we keep it distinct so conversions can re-encode it correctly.)
    Tool,
}

/// What a message IS — every adapter sets it, every emitter reads it (`docs/INTERFACE-V2.md` §4).
/// `Role` says who speaks; `MessageKind` says what the turn is; `Origin` says where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A human's typed prompt (`Role::User`, `Origin::Human`).
    Prompt,
    /// The model's reply (`Role::Assistant`): text, thinking and tool calls.
    Reply,
    /// Tool output fed back to the model (`Role::Tool`).
    ToolResult,
    /// Context the HARNESS injected into the model's input: Claude attachments / system reminders,
    /// Kimi injections, Goose `<turn-context>`, Codex `<environment_context>`, hook stdout.
    InjectedContext,
    /// The system prompt, when the store keeps it as a message (Kimi `profile.bind`, Hermes system
    /// row, OpenClaw). The session-level copy is `Session::system_prompt`.
    SystemPrompt,
    /// A harness notice shown to the user, not sent to the model: slash-command output, task
    /// terminated, turn aborted, "usage limit reset", Codex `item_completed` notes.
    Notice,
    /// The point where the harness compacted the context.
    CompactionBoundary,
    /// The summary that seeds the next window after a compaction.
    CompactionSummary,
    /// The model (or effort) changed from here on; `Message::model` holds the new one.
    ModelChange,
    /// An error the model never answered: API error, refusal fallback, retry exhaustion.
    Error,
    /// A sub-agent was spawned here (the `Agent` / `spawn` call).
    SubagentSpawn,
    /// A sub-agent's final return, delivered to the parent.
    SubagentReturn,
    /// A branch / rewind / reset marker: what follows does not continue what precedes.
    Branch,
    /// A verbatim non-conversational record carried only under `ParseOptions::complete`.
    Carrier,
}

impl MessageKind {
    /// The kind a bare role implies when an adapter has not said otherwise: User→Prompt,
    /// Assistant→Reply, Tool→ToolResult, System→Notice. Adapters MUST set the precise kind; this
    /// exists for `Message::new` and for legacy IR JSON that predates `kind`.
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::User => MessageKind::Prompt,
            Role::Assistant => MessageKind::Reply,
            Role::Tool => MessageKind::ToolResult,
            Role::System => MessageKind::Notice,
        }
    }

    /// The canonical snake_case name (what `--json` prints).
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Prompt => "prompt",
            MessageKind::Reply => "reply",
            MessageKind::ToolResult => "tool_result",
            MessageKind::InjectedContext => "injected_context",
            MessageKind::SystemPrompt => "system_prompt",
            MessageKind::Notice => "notice",
            MessageKind::CompactionBoundary => "compaction_boundary",
            MessageKind::CompactionSummary => "compaction_summary",
            MessageKind::ModelChange => "model_change",
            MessageKind::Error => "error",
            MessageKind::SubagentSpawn => "subagent_spawn",
            MessageKind::SubagentReturn => "subagent_return",
            MessageKind::Branch => "branch",
            MessageKind::Carrier => "carrier",
        }
    }

    /// Was this turn part of what the model actually saw or said (as opposed to a harness-side
    /// notice, marker or carrier)? Prompts, replies, tool results, injected context and the
    /// system prompt are; everything else is bookkeeping around the conversation.
    pub fn is_model_visible(self) -> bool {
        matches!(
            self,
            MessageKind::Prompt
                | MessageKind::Reply
                | MessageKind::ToolResult
                | MessageKind::InjectedContext
                | MessageKind::SystemPrompt
                | MessageKind::CompactionSummary
        )
    }
}

/// Where a message came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Typed by a person.
    Human,
    /// Produced by the model.
    Model,
    /// Produced by the harness itself (reminders, notices, compaction, environment context).
    Harness,
    /// A user-configured hook's output.
    Hook,
    /// Cron / loop wakeups and other automation.
    Scheduler,
    /// Another agent: inter-agent messages, sub-agent returns.
    Subagent,
    /// Imported from another harness by the harness (Goose / Hermes importers).
    Import,
    /// The store does not say.
    #[default]
    Unknown,
}

impl Origin {
    /// The origin a bare role implies: User→Human, Assistant→Model, Tool→Harness (the harness ran
    /// the tool), System→Harness. Adapters override when the store says otherwise.
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::User => Origin::Human,
            Role::Assistant => Origin::Model,
            Role::Tool | Role::System => Origin::Harness,
        }
    }
}

/// One message/turn in a conversation.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// WHO speaks.
    pub role: Role,
    /// WHAT the turn is. See [`MessageKind`].
    pub kind: MessageKind,
    /// WHERE it came from. See [`Origin`].
    pub origin: Origin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub content: Vec<Block>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Harness-specific message facts, nested under the harness name
    /// (`extra["claude"]["attachment_type"]`). The only other top-level key allowed is
    /// [`CARRIER_KEY`](crate::harness::claude::CARRIER_KEY) (`"_record"`, the verbatim record under
    /// `ParseOptions::complete`). Shared concepts are first-class (`kind`, `origin`, `usage`,
    /// `Block::ToolResult::details`). See `docs/INTERFACE-V2.md` §4.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Deserialize with `kind` / `origin` optional: IR JSON written before they existed (and
/// hand-written test fixtures) get the role-implied defaults instead of failing.
impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            parent_id: Option<String>,
            role: Role,
            #[serde(default)]
            kind: Option<MessageKind>,
            #[serde(default)]
            origin: Option<Origin>,
            #[serde(default)]
            timestamp: Option<DateTime<Utc>>,
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            content: Vec<Block>,
            #[serde(default)]
            usage: Option<Usage>,
            #[serde(default)]
            extra: serde_json::Map<String, serde_json::Value>,
        }
        let w = Wire::deserialize(d)?;
        Ok(Message {
            id: w.id,
            parent_id: w.parent_id,
            role: w.role,
            kind: w.kind.unwrap_or_else(|| MessageKind::for_role(w.role)),
            origin: w.origin.unwrap_or_else(|| Origin::for_role(w.role)),
            timestamp: w.timestamp,
            model: w.model,
            content: w.content,
            usage: w.usage,
            extra: w.extra,
        })
    }
}

/// `extra[h.as_str()]` as a mutable object, created on demand. Shared by [`Session`] and
/// [`Message`]; a non-object value already under that key is replaced (it can only be a bug).
fn harness_bag_mut(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    h: Harness,
) -> &mut serde_json::Map<String, serde_json::Value> {
    bag_mut(extra, h.as_str())
}

/// `extra[name]` as a mutable object, created on demand. A non-object value already under that key
/// can only be a bug, so it is replaced.
fn bag_mut<'a>(
    extra: &'a mut serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    let slot = extra
        .entry(name)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !slot.is_object() {
        *slot = serde_json::Value::Object(serde_json::Map::new());
    }
    slot.as_object_mut().expect("just ensured an object")
}

impl Message {
    /// A message with the role-implied `kind` and `origin` (`MessageKind::for_role`,
    /// `Origin::for_role`). Adapters refine both once they know better.
    pub fn new(role: Role) -> Self {
        Message {
            id: None,
            parent_id: None,
            role,
            kind: MessageKind::for_role(role),
            origin: Origin::for_role(role),
            timestamp: None,
            model: None,
            content: Vec::new(),
            usage: None,
            extra: serde_json::Map::new(),
        }
    }

    /// A message of an explicit kind, with the role that kind conventionally carries.
    pub fn of_kind(role: Role, kind: MessageKind, origin: Origin) -> Self {
        let mut m = Message::new(role);
        m.kind = kind;
        m.origin = origin;
        m
    }

    /// The harness-specific fact bag for `h` — `extra[h.as_str()]` as an object, created on demand.
    /// Adapters write here, never at the top level of `extra`.
    pub fn harness_extra_mut(&mut self, h: Harness) -> &mut serde_json::Map<String, serde_json::Value> {
        harness_bag_mut(&mut self.extra, h)
    }

    /// The harness-specific fact bag for `h`, if any facts were recorded.
    pub fn harness_extra(&self, h: Harness) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.extra.get(h.as_str()).and_then(serde_json::Value::as_object)
    }

    /// Resolve this message's lazy content spans in place against `resolver`, so its content is owned
    /// inline. Streaming consumers call this per message (peak = one message) instead of holding a
    /// whole materialized session.
    pub fn materialize(&mut self, resolver: &crate::lazy::Resolver) {
        for b in &mut self.content {
            let slot = match b {
                Block::Text { text } | Block::Thinking { text, .. } => text,
                Block::ToolResult { content, .. } => content,
                _ => continue,
            };
            if slot.is_span() {
                let owned = slot.resolve(resolver).into_owned();
                *slot = crate::lazy::Text::Inline(owned);
            }
        }
    }

    /// Append this message's searchable text to `out` — the single canonical projection behind
    /// both [`Session::searchable_text`] (whole session) and
    /// [`append_searchable`](crate::stream::append_searchable) (streaming, per message).
    /// Writes into the caller's buffer so neither path allocates intermediates.
    pub fn append_searchable(&self, out: &mut String) {
        use std::fmt::Write as _;
        for b in &self.content {
            match b {
                Block::Text { text } | Block::Thinking { text, .. } => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::ToolUse { name, input, .. } => {
                    out.push_str(name);
                    out.push(' ');
                    // `Display` for `serde_json::Value` produces exactly `to_string()`, without
                    // the intermediate allocation.
                    let _ = write!(out, "{input}");
                    out.push('\n');
                }
                Block::ToolResult { content, .. } => {
                    out.push_str(content);
                    out.push('\n');
                }
                Block::File { path, source, .. } => {
                    if let Some(p) = path.as_deref().or(source.as_deref()) {
                        out.push_str(p);
                        out.push('\n');
                    }
                }
                Block::Image { .. } => {}
            }
        }
    }

    /// Concatenated plain text of this message's text blocks (ignores thinking/tools).
    pub fn text(&self) -> Option<String> {
        let mut s = String::new();
        for b in &self.content {
            if let Block::Text { text } = b {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(text);
            }
        }
        (!s.is_empty()).then_some(s)
    }
}

/// A unit of message content. Tagged by `type` — the word every harness uses on the wire — so a
/// block's `type` is never confused with its message's `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: crate::lazy::Text,
    },
    /// Extended reasoning / chain-of-thought. `encrypted` holds an opaque blob some providers emit.
    Thinking {
        text: crate::lazy::Text,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        /// Whether the provider redacted this reasoning content.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    /// A tool/function invocation by the assistant.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// The tool's namespace when the harness has one (Codex `collaboration`, MCP server names).
        /// `name` stays the bare tool name so tool statistics keep grouping.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
    /// The result of a tool/function call.
    ToolResult {
        tool_use_id: String,
        content: crate::lazy::Text,
        #[serde(default)]
        is_error: bool,
        /// Name of the tool this result is for, when the adapter knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// Adapter-computed status string (e.g. "completed", "error", "running").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Structured extra details about the result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    Image {
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// Path or opaque reference; we don't inline image bytes into the IR.
        #[serde(skip_serializing_if = "Option::is_none")]
        data_ref: Option<String>,
    },
    /// A first-class file/dir/resource attachment.
    File {
        #[serde(skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl Block {
    /// For a tool result whose real output was too large for the transcript and went to a file
    /// (Claude Code's `<persisted-output>` stub → `<session>/tool-results/<id>.txt`): where that
    /// file lives. The block's `content` is the stub the model saw; this is the rest.
    pub fn persisted_output_path(&self) -> Option<&str> {
        match self {
            Block::ToolResult { details: Some(d), .. } => d.pointer("/persistedOutput/path")?.as_str(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Reasoning / thinking tokens when the provider reports them separately from `output_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported cost in USD, when the harness stores it (Goose, OpenCode, Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// A lightweight handle to a session discovered on disk, cheap to produce for listings/search
/// without parsing the whole transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
    pub harness: Harness,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// Number of conversational messages: **user + assistant turns only**.
    ///
    /// This is the contract every adapter must meet — do not count system/meta records, tool
    /// results, sidecar files, or directory entries. The number a person would call "how long is
    /// this conversation", comparable across harnesses. Adapters that can't know it cheaply
    /// should compute it from the same records they'd parse into [`Role::User`]/[`Role::Assistant`]
    /// messages, not from a proxy like file count.
    pub message_count: usize,
}

/// A short human label from a title and/or first-user-text fallback — the projection
/// [`Session::label`] makes, exposed so streaming consumers can build it without a whole `Session`.
pub fn label_from(title: Option<&str>, first_user_text: Option<&str>) -> String {
    title
        .or(first_user_text)
        .map(|s| truncate(s, 72))
        .unwrap_or_else(|| "(untitled)".into())
}

/// A user turn's text as a real prompt, or `None` when it's only harness noise. Peels any leading
/// `<system-reminder>…</system-reminder>` blocks and surrounding whitespace, then rejects the
/// residual if it's empty, the `Caveat:` local-command preamble, or a bare command wrapper
/// (`<command-name>`/`<command-message>`/`<local-command-stdout>`). Used by [`Session::synth_title`].
fn meaningful_prompt(text: &str) -> Option<String> {
    let mut s = text.trim_start();
    loop {
        s = s.trim_start();
        let Some(rest) = s.strip_prefix("<system-reminder>") else {
            break;
        };
        let Some(end) = rest.find("</system-reminder>") else {
            break;
        };
        s = &rest[end + "</system-reminder>".len()..];
    }
    let s = s.trim();
    if s.is_empty()
        || s.starts_with("Caveat:")
        || s.starts_with("<command-name>")
        || s.starts_with("<command-message>")
        || s.starts_with("<local-command-stdout>")
    {
        return None;
    }
    Some(s.to_string())
}

/// Flatten newlines to spaces and truncate to at most `max` chars, ending with `…` when cut.
/// The one canonical truncation used by labels, renderers, and listings.
///
/// Reads at most `max + 1` chars of `s` — it never allocates a flattened copy of the whole input
/// (labels and renderers routinely truncate multi-megabyte message bodies down to ~72 chars).
pub fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    for _ in 0..max {
        match chars.next() {
            Some(c) => out.push(if c == '\n' { ' ' } else { c }),
            None => return out, // shorter than max: the whole (flattened) string
        }
    }
    if chars.next().is_none() {
        return out; // exactly max chars: fits without an ellipsis
    }
    // Longer than max: keep the first max-1 chars and mark the cut.
    out.pop();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Harness::ALL` and `as_str` are macro-generated from one list; `parse` is hand-written
    /// (it carries aliases). This ties them together: every harness's canonical name must parse
    /// back to itself, so adding a variant without a `parse` arm fails here.
    #[test]
    fn every_harness_canonical_name_parses_back() {
        for h in Harness::ALL {
            assert_eq!(Harness::parse(h.as_str()), Some(h), "parse arm missing for {h:?}");
        }
        assert_eq!(Harness::ALL.len(), Harness::COUNT);
    }

    /// `meaningful_prompt` peels system-reminder blocks and rejects pure harness noise, so
    /// `synth_title` never surfaces a caveat/command wrapper as a title.
    #[test]
    fn meaningful_prompt_skips_noise_and_peels_reminders() {
        // A leading reminder block is peeled off an otherwise-real prompt.
        assert_eq!(
            meaningful_prompt("<system-reminder>x</system-reminder>\n\nreal question").as_deref(),
            Some("real question")
        );
        // Multiple stacked reminders are all peeled.
        assert_eq!(
            meaningful_prompt("<system-reminder>a</system-reminder><system-reminder>b</system-reminder> hi").as_deref(),
            Some("hi")
        );
        // Pure noise (no residual prose) yields None.
        assert_eq!(
            meaningful_prompt("<system-reminder>only a reminder</system-reminder>"),
            None
        );
        assert_eq!(meaningful_prompt("Caveat: The messages below were generated…"), None);
        assert_eq!(meaningful_prompt("<command-name>/foo</command-name>"), None);
        assert_eq!(meaningful_prompt("   \n  "), None);
        // A plain prompt passes through unchanged.
        assert_eq!(meaningful_prompt("just a prompt").as_deref(), Some("just a prompt"));
    }

    /// `synth_title` prefers the explicit title, falls back to the first meaningful user turn, and
    /// stays `None` when a session carries neither (the documented residual).
    #[test]
    fn synth_title_prefers_title_then_falls_back_then_none() {
        let user = |t: &str| {
            let mut m = Message::new(Role::User);
            m.content.push(Block::Text { text: t.into() });
            m
        };
        let mut s = Session {
            id: "s".into(),
            harness: Harness::Claude,
            cwd: None,
            title: Some("explicit".into()),
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            messages: vec![user("<system-reminder>noise</system-reminder>\n\nask about otters")],
            source_path: None,
            extra: Default::default(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };
        // Explicit title wins.
        assert_eq!(s.synth_title().as_deref(), Some("explicit"));
        // Without one, the first meaningful user turn is synthesized (reminder peeled).
        s.title = None;
        assert_eq!(s.synth_title().as_deref(), Some("ask about otters"));
        // A session with no user prose stays None.
        s.messages = vec![user("<system-reminder>only noise</system-reminder>")];
        assert_eq!(s.synth_title(), None);
        // And truly no user turns at all is None too.
        s.messages = vec![];
        assert_eq!(s.synth_title(), None);
    }

    /// The streaming `truncate` must behave exactly like the old flatten-whole-string version.
    #[test]
    fn truncate_matches_flatten_then_cut_semantics() {
        let reference = |s: &str, max: usize| -> String {
            let s = s.replace('\n', " ");
            if s.chars().count() <= max {
                s
            } else {
                let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
                out.push('…');
                out
            }
        };
        let cases = [
            ("", 0),
            ("", 5),
            ("ab", 0),
            ("ab", 1),
            ("ab", 2),
            ("ab", 3),
            ("a\nb\nc", 3),
            ("a\nb\nc", 10),
            ("héllo wörld", 4),
            ("héllo wörld", 11),
            ("日本語のテキスト", 5),
            ("line one\nline two\r\nline three", 72),
            ("exactly", 7),
        ];
        for (s, max) in cases {
            assert_eq!(truncate(s, max), reference(s, max), "s={s:?} max={max}");
        }
    }
}
