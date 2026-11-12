//! The unified intermediate representation (IR).
//!
//! Every harness parses *into* these types, and cross-harness conversion emits *out of* them.
//! The IR is deliberately a superset: fields that a given harness doesn't have are `None`/empty,
//! and harness-specific extras ride along in [`Message::extra`] so conversions can be as lossless
//! as the target allows.

use crate::projection::Head;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) mod record;
pub use record::CompleteRecord;

/// Declares the [`Harness`] enum together with [`Harness::ALL`] and [`Harness::as_str`] from one
/// variant list, so they can never drift apart (the old hand-maintained 17-element `ALL` array
/// silently went stale whenever a variant was added). Adding a harness is now a single line here
/// (plus its aliases in [`Harness::parse`], which a test ties back to `ALL`).
macro_rules! harnesses {
    ($( $(#[$meta:meta])* $name:ident => $str:literal ),+ $(,)?) => {
        /// Which agent harness a session came from.
        ///
        /// Serde goes through [`Harness::as_str`] and [`Harness::parse`], NOT a derived
        /// `rename_all`: the derive spells a variant name in lowercase, so `KimiCode` serialized as
        /// `"kimicode"` while `as_str` (and `cv --harness`, and `cv ls --json`) said `"kimi-code"`.
        /// The same field therefore had two spellings depending on which door you asked — exactly
        /// the ambiguity `docs/INTERFACE-V2.md` §3 exists to remove — and the five hyphenated
        /// harnesses were the ones that suffered.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Harness {
            $( $(#[$meta])* $name, )+
        }

        impl serde::Serialize for Harness {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for Harness {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Harness::parse(&s)
                    .ok_or_else(|| serde::de::Error::custom(format!("unknown harness {s:?}")))
            }
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

            /// `formats/<name>.toml`, embedded: one manifest per harness by construction, so a
            /// harness without one does not compile.
            pub(crate) fn manifest_toml(self) -> &'static str {
                match self {
                    $( Harness::$name => include_str!(concat!("../formats/", $str, ".toml")), )+
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
    /// Devin CLI (Cognition), `~/.local/share/devin/cli/sessions.db` (SQLite message forest).
    Devin => "devin",
    /// ChatGPT account **data export** (`conversations.json`, a `mapping` DAG per conversation).
    ChatGptExport => "chatgpt-export",
    /// Claude.ai account **data export** (`conversations.json`, linear `chat_messages[]`).
    ClaudeExport => "claude-export",
    /// cv's own **OpenSession** interchange documents (`docs/OPENSESSION.md`) — what
    /// `cv export --format json` writes, read back in. A document names the harness it came
    /// from, so a parsed session keeps that harness; this variant is what *discovery* tags a
    /// `.opensession.json` file with, and what a document with no (or an unknown) `harness` gets.
    OpenSession => "opensession",
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
            "kimi" | "kimi-cli" => Harness::Kimi,
            "kimi-code" | "kimicode" | "kimi2" => Harness::KimiCode,
            "qwen" | "qwen-code" => Harness::Qwen,
            "lmstudio" | "lm-studio" => Harness::LmStudio,
            "cline" => Harness::Cline,
            "roo" | "roo-code" | "roocode" => Harness::Roo,
            "continue" | "continuedev" => Harness::Continue,
            "goose" => Harness::Goose,
            "zed" | "zed-editor" => Harness::Zed,
            "devin" | "devin-cli" => Harness::Devin,
            "chatgpt-export" | "openai-export" | "chatgpt-data" | "openai-data" => Harness::ChatGptExport,
            "claude-export" | "claude-ai" | "claudeai" | "claude-data" => Harness::ClaudeExport,
            "opensession" | "open-session" | "open_session" => Harness::OpenSession,
            _ => return None,
        })
    }

    /// A name cv once accepted as a harness and no longer does, with where to look instead.
    /// `None` for a name that never was one. The CLI answers a hit with a usage error (exit 2),
    /// so a script written against the old name fails loudly with the replacement instead of
    /// matching nothing.
    pub fn retired(s: &str) -> Option<&'static str> {
        match s.to_ascii_lowercase().as_str() {
            "claude-app" | "claude-desktop" | "claudeapp" => Some(
                "the Claude desktop app keeps no transcripts on disk (claude.ai holds them server-side); \
                 the Claude Code sessions it launches are `--harness claude` — see docs/FORMATS.md, \"Desktop apps\"",
            ),
            "chatgpt-app" | "chatgpt" | "chatgpt-desktop" | "openai-app" => Some(
                "the ChatGPT desktop app encrypts its history at rest and cv cannot read it; \
                 ChatGPT conversations come from the account export (`cv config --add-export <dir>`, \
                 `--harness chatgpt-export`) — see docs/FORMATS.md, \"Desktop apps\"",
            ),
            _ => None,
        }
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
    /// The opaque identity this session acts as. Consumers bind it to their own domain;
    /// cv never infers it from cwd, harness, session id, or a local account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<Principal>,
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
    /// `source_path`'s identity when this session's spans were parsed; [`Session::resolver`]
    /// holds the file to it. Set by the stream layer, never serialized.
    #[serde(skip)]
    pub source_stamp: Option<crate::lazy::SourceStamp>,
    /// Harness-specific session facts, nested under the harness name — `extra["claude"]["custom_title"]`,
    /// `extra["codex"]["history_mode"]` — never flat. Shared concepts have first-class fields
    /// (`system_prompt`, `lineage`, …). See `docs/INTERFACE-V2.md` §4.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// An opaque OpenSession principal, either a stable string or an uninterpreted object.
/// Object keys and values have no cv-defined identity or authorization semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Principal {
    String(String),
    Object(serde_json::Map<String, serde_json::Value>),
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

/// A tool-call id for a call whose wire carries none, derived from where the call sits in its
/// session (`cv:harmony:3.0.1`, `cv:cursor:12`). The `cv:` prefix declares it cv's: unique within
/// the session and stable across reads, so a call and its result still pair, but never a value the
/// harness wrote.
pub fn derived_call_id(format: &str, position: impl std::fmt::Display) -> String {
    format!("cv:{format}:{position}")
}

/// The namespace inside `extra` for facts cv itself produces rather than reads from a harness:
/// parse diagnostics like `skipped_lines`, and the provenance cv stamps on a session it synthesized
/// (`loom`, `splice`). It is spelled `"cv"`, which `Harness::parse` never yields, so it can never
/// collide with a harness bag. See `docs/INTERFACE-V2.md` §4.
pub const CV_NAMESPACE: &str = "cv";

/// The one other top-level `extra` key: the verbatim native record a carrier (or a complete-mode
/// turn) keeps under `Demand::COMPLETE`. Not a harness bag, and not Claude's: every adapter
/// with complete-mode carriers writes it.
pub const CARRIER_KEY: &str = "_record";

impl Harness {
    /// Whether this harness's complete-mode *turns* keep their verbatim record under
    /// [`CARRIER_KEY`] once serialized (OpenCode: `CompleteRecord::attach_replay` on a turn naming
    /// its `record_type`), so a record there does not make the message a carrier.
    pub fn turns_keep_records(self) -> bool {
        match self {
            Harness::OpenCode => true,
            Harness::Claude
            | Harness::Codex
            | Harness::Grok
            | Harness::Gemini
            | Harness::Hermes
            | Harness::OpenClaw
            | Harness::Cursor
            | Harness::Kimi
            | Harness::KimiCode
            | Harness::Qwen
            | Harness::LmStudio
            | Harness::Cline
            | Harness::Roo
            | Harness::Continue
            | Harness::Goose
            | Harness::Zed
            | Harness::Devin
            | Harness::ChatGptExport
            | Harness::ClaudeExport
            | Harness::OpenSession => false,
        }
    }
}

impl Session {
    /// An empty session of `harness`: every optional field `None`, no messages, no extras. The one
    /// way an adapter spells a fresh session — fill in what the store says with struct-update
    /// syntax (`Session { cwd, title, ..Session::new(id, h) }`) rather than restating thirteen
    /// `None`s.
    pub fn new(id: impl Into<String>, harness: Harness) -> Session {
        Session {
            id: id.into(),
            harness,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            principal: None,
            system_prompt: None,
            lineage: Lineage::default(),
            messages: Vec::new(),
            source_path: None,
            source_stamp: None,
            extra: serde_json::Map::new(),
        }
    }

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

    /// A short human label for listings: [`Session::synth_title`] at most 72 characters long.
    pub fn label(&self) -> String {
        label_from(self.synth_title().as_deref(), None)
    }

    /// The session's title: the explicit [`title`](Session::title) when present, otherwise its
    /// first [`Message::prompt_title`], passing over a prompt whose lazy text cannot be read
    /// ([`Session::title_with`] makes that an error). The result is transcript-derived text
    /// (untrusted): JSON consumers get it raw; a terminal seam must sanitize it.
    pub fn synth_title(&self) -> Option<String> {
        let resolver = self.resolver();
        self.title.clone().or_else(|| {
            self.messages
                .iter()
                .find_map(|m| m.prompt_title(&resolver).ok().flatten())
        })
    }

    /// [`Session::synth_title`] reading lazy blocks through `resolver`, an unreadable one the error.
    pub fn title_with(&self, resolver: &crate::lazy::Resolver) -> anyhow::Result<Option<String>> {
        if let Some(t) = &self.title {
            return Ok(Some(t.clone()));
        }
        for m in &self.messages {
            if let Some(t) = m.prompt_title(resolver)? {
                return Ok(Some(t));
            }
        }
        Ok(None)
    }

    /// A [`Resolver`](crate::lazy::Resolver) for this session's lazy content spans, rooted at its
    /// `source_path` and held to its parse-time [`source_stamp`](Self::source_stamp), so a file
    /// replaced after the parse fails the first demand. Without a stamp it stamps the file now.
    pub fn resolver(&self) -> crate::lazy::Resolver {
        match &self.source_stamp {
            Some(stamp) => crate::lazy::Resolver::stamped(self.source_path.clone(), Some(stamp.clone())),
            None => crate::lazy::Resolver::new(self.source_path.clone()),
        }
    }

    /// Capture the session's primary and retained auxiliary sources before a multi-message
    /// demand. Check `ReadSet::fresh` with a new `source::Probe` before publishing its output,
    /// so a later message cannot conceal a change to an earlier message's source.
    pub fn source_reads(&self) -> crate::source::ReadSet {
        crate::source::session_reads(self)
    }

    /// Resolve every lazy content [`Span`](crate::lazy::Span) in place, so the session owns all its
    /// content inline. Whole-session consumers (cross-harness emit building output in memory, JSON
    /// serialization) call this; streaming consumers resolve per-block instead. After it, every
    /// content `Text` is `Inline`, so `Deref`/`Display` are safe.
    pub fn materialize(&mut self) -> anyhow::Result<()> {
        let resolver = self.resolver();
        for m in &mut self.messages {
            m.materialize(&resolver)?;
        }
        Ok(())
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

impl Role {
    /// Every role, in declaration order: a new variant must be listed here (the array length
    /// forces it) before the schema can publish it.
    pub const ALL: [Role; 4] = [Role::System, Role::User, Role::Assistant, Role::Tool];

    /// The canonical lowercase name (what `--json` prints); the same spelling serde derives.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// Every role as serde spells it, beside the enum: the vocabulary `cv schema --json` publishes.
pub const ROLE_NAMES: [&str; Role::ALL.len()] = {
    let mut names = [""; Role::ALL.len()];
    let mut i = 0;
    while i < Role::ALL.len() {
        names[i] = Role::ALL[i].as_str();
        i += 1;
    }
    names
};

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
    /// A verbatim non-conversational record carried only under `Demand::COMPLETE`.
    Carrier,
}

impl MessageKind {
    /// Every kind, in declaration order: a new variant must be listed here (the array length
    /// forces it) before the schema can publish it.
    pub const ALL: [MessageKind; 14] = [
        MessageKind::Prompt,
        MessageKind::Reply,
        MessageKind::ToolResult,
        MessageKind::InjectedContext,
        MessageKind::SystemPrompt,
        MessageKind::Notice,
        MessageKind::CompactionBoundary,
        MessageKind::CompactionSummary,
        MessageKind::ModelChange,
        MessageKind::Error,
        MessageKind::SubagentSpawn,
        MessageKind::SubagentReturn,
        MessageKind::Branch,
        MessageKind::Carrier,
    ];

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
    pub const fn as_str(self) -> &'static str {
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

    /// Is this turn part of the exchange itself: what the person asked, what the model replied
    /// or ran, what came back, what a sub-agent was handed or returned, an unanswered error?
    /// Everything else is context around it (injected instructions, the system prompt, notices,
    /// compaction summaries restating earlier turns) or bookkeeping.
    pub fn is_exchange(self) -> bool {
        matches!(
            self,
            MessageKind::Prompt
                | MessageKind::Reply
                | MessageKind::ToolResult
                | MessageKind::SubagentSpawn
                | MessageKind::SubagentReturn
                | MessageKind::Error
        )
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

/// Every kind as serde spells it, beside the enum: the vocabulary `cv schema --json` publishes.
pub const MESSAGE_KIND_NAMES: [&str; MessageKind::ALL.len()] = {
    let mut names = [""; MessageKind::ALL.len()];
    let mut i = 0;
    while i < MessageKind::ALL.len() {
        names[i] = MessageKind::ALL[i].as_str();
        i += 1;
    }
    names
};

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
    /// Every origin, in declaration order: a new variant must be listed here (the array length
    /// forces it) before the schema can publish it.
    pub const ALL: [Origin; 8] = [
        Origin::Human,
        Origin::Model,
        Origin::Harness,
        Origin::Hook,
        Origin::Scheduler,
        Origin::Subagent,
        Origin::Import,
        Origin::Unknown,
    ];

    /// The canonical snake_case name (what `--json` prints); the same spelling serde derives.
    pub const fn as_str(self) -> &'static str {
        match self {
            Origin::Human => "human",
            Origin::Model => "model",
            Origin::Harness => "harness",
            Origin::Hook => "hook",
            Origin::Scheduler => "scheduler",
            Origin::Subagent => "subagent",
            Origin::Import => "import",
            Origin::Unknown => "unknown",
        }
    }

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

/// Every origin as serde spells it, beside the enum: the vocabulary `cv schema --json` publishes.
pub const ORIGIN_NAMES: [&str; Origin::ALL.len()] = {
    let mut names = [""; Origin::ALL.len()];
    let mut i = 0;
    while i < Origin::ALL.len() {
        names[i] = Origin::ALL[i].as_str();
        i += 1;
    }
    names
};

/// One message/turn in a conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    /// WHO speaks.
    pub role: Role,
    /// WHAT the turn is. See [`MessageKind`].
    pub kind: MessageKind,
    /// WHERE it came from. See [`Origin`].
    pub origin: Origin,
    pub timestamp: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    /// Harness-specific message facts, nested under the harness name
    /// (`extra["claude"]["attachment_type"]`); cv's own facts go under [`CV_NAMESPACE`]
    /// ([`Message::synthesized`]). The only other top-level key allowed is
    /// [`CARRIER_KEY`](crate::ir::CARRIER_KEY) (`"_record"`, the verbatim record under
    /// `Demand::COMPLETE`). Shared concepts are first-class (`kind`, `origin`, `usage`,
    /// `Block::ToolResult::details`). See `docs/INTERFACE-V2.md` §4.
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Checked complete native backing; serializes through `extra._record`, never as a new field.
    pub(crate) complete_record: Option<std::sync::Arc<CompleteRecord>>,
}

/// serde by derive over the fields themselves. The destructuring is exhaustive, so a new field is
/// a compile error here until it is serialized or deliberately skipped; `complete_record` reaches
/// the output only as `extra._record`, through [`record::Extra`].
impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Message {
            id,
            parent_id,
            role,
            kind,
            origin,
            timestamp,
            model,
            content,
            usage,
            extra: _,
            complete_record: _,
        } = self;
        #[derive(Serialize)]
        #[serde(rename = "Message")]
        struct Wire<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            id: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            parent_id: Option<&'a String>,
            role: &'a Role,
            kind: &'a MessageKind,
            origin: &'a Origin,
            #[serde(skip_serializing_if = "Option::is_none")]
            timestamp: Option<&'a DateTime<Utc>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            model: Option<&'a String>,
            content: &'a Vec<Block>,
            #[serde(skip_serializing_if = "Option::is_none")]
            usage: Option<&'a Usage>,
            #[serde(skip_serializing_if = "record::Extra::is_empty")]
            extra: record::Extra<'a>,
        }
        Wire {
            id: id.as_ref(),
            parent_id: parent_id.as_ref(),
            role,
            kind,
            origin,
            timestamp: timestamp.as_ref(),
            model: model.as_ref(),
            content,
            usage: usage.as_ref(),
            extra: record::Extra(self),
        }
        .serialize(serializer)
    }
}

/// The wire form [`Message`]'s `Serialize` writes, read back; `kind` and `origin` are required
/// (every cv since IR v2 writes them, and OpenSession 0.3 requires them).
impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            parent_id: Option<String>,
            role: Role,
            kind: MessageKind,
            origin: Origin,
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
            kind: w.kind,
            origin: w.origin,
            timestamp: w.timestamp,
            model: w.model,
            content: w.content,
            usage: w.usage,
            extra: w.extra,
            complete_record: None,
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
    debug_assert!(
        slot.is_object(),
        "extra[{name}] held {slot}: a non-object bag can only be a bug"
    );
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
            complete_record: None,
        }
    }

    /// A message of an explicit kind, with the role that kind conventionally carries.
    pub fn of_kind(role: Role, kind: MessageKind, origin: Origin) -> Self {
        let mut m = Message::new(role);
        m.kind = kind;
        m.origin = origin;
        m
    }

    /// A contentless turn cv supplies for a harness boundary: inferred before a summary row,
    /// or representing a structured boundary with no native text. It starts without an id;
    /// adapters may retain the source record's identity and metadata. The representation is
    /// declared in `extra["cv"]["synthesized"] = {kind, origin}`. Renderers label it by kind
    /// ([`crate::render::turn_label`]), so the text a reader sees comes from cv, not the transcript.
    pub fn synthesized(kind: MessageKind, origin: Origin) -> Self {
        let mut m = Message::of_kind(Role::System, kind, origin);
        let tag = serde_json::json!({ "kind": kind, "origin": origin });
        bag_mut(&mut m.extra, CV_NAMESPACE).insert("synthesized".into(), tag);
        m
    }

    /// Did cv synthesize this turn ([`Message::synthesized`])?
    pub fn is_synthesized(&self) -> bool {
        self.extra
            .get(CV_NAMESPACE)
            .and_then(|bag| bag.get("synthesized"))
            .is_some()
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

    /// A verbatim meta record carried under `Demand::COMPLETE`, never a conversational turn:
    /// `kind == Carrier`, or a [`CARRIER_KEY`] record on a message that is not a complete-mode turn
    /// of a harness whose turns keep their record ([`Harness::turns_keep_records`]; such a turn
    /// names its `record_type` in its bag and stays a turn).
    pub fn is_carrier(&self) -> bool {
        self.kind == MessageKind::Carrier
            || (self.extra.contains_key(CARRIER_KEY)
                && !Harness::ALL.into_iter().filter(|h| h.turns_keep_records()).any(|h| {
                    self.harness_extra(h)
                        .is_some_and(|extra| extra.contains_key("record_type"))
                }))
    }

    /// Resolve this message's lazy content spans in place against `resolver`, so its content is owned
    /// inline. Streaming consumers call this per message (peak = one message) instead of holding a
    /// whole materialized session.
    pub fn materialize(&mut self, resolver: &crate::lazy::Resolver) -> anyhow::Result<()> {
        self.try_materialize_text(resolver)?;
        for block in &mut self.content {
            if let Block::ToolUse { input, .. } = block {
                input.make_owned()?;
            }
        }
        if let Some(record) = &mut self.complete_record {
            let record = std::sync::Arc::make_mut(record);
            record.materialize()?;
        }
        Ok(())
    }

    /// Own demanded text while preserving lazy structured arguments, propagating source errors.
    /// Fallible renderers use this instead of the legacy best-effort `materialize_text` helper.
    pub fn try_materialize_text(&mut self, resolver: &crate::lazy::Resolver) -> anyhow::Result<()> {
        for block in &mut self.content {
            let text = match block {
                Block::Text { text } | Block::Thinking { text, .. } => text,
                Block::ToolResult { content, .. } => content,
                _ => continue,
            };
            if text.is_lazy() {
                let owned = text.try_resolve(resolver)?.into_owned();
                *text = crate::lazy::Text::Inline(owned);
            }
        }
        Ok(())
    }

    /// Own text fields while leaving complete structured arguments available for explicit
    /// projection. Renderers with argument previews need no whole argument tree.
    pub fn materialize_text(&mut self, resolver: &crate::lazy::Resolver) {
        for b in &mut self.content {
            let slot = match b {
                Block::Text { text } | Block::Thinking { text, .. } => text,
                Block::ToolResult { content, .. } => content,
                _ => continue,
            };
            if !matches!(slot, crate::lazy::Text::Inline(_)) {
                if let Ok(owned) = slot.try_resolve(resolver) {
                    *slot = crate::lazy::Text::Inline(owned.into_owned());
                }
            }
        }
    }

    /// Append this message's searchable text to `out`: [`crate::projection::searchable_text_checked`]
    /// with no cancellation, resolving any span content through the default resolver.
    pub fn append_searchable(&self, out: &mut String) -> anyhow::Result<()> {
        out.push_str(&crate::projection::searchable_text_checked(
            self,
            &crate::lazy::Resolver::new(None),
            || Ok(()),
        )?);
        Ok(())
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        /// The call id the wire gave it. Where the wire has none (Harmony text, a Cursor tool
        /// record naming no call id), the reader derives one from the call's position,
        /// [`derived_call_id`]: `cv:<format>:<position>`, a shape no harness writes.
        id: String,
        name: String,
        /// Complete arguments, owned or backed by a validated source range. Serialization keeps
        /// the existing JSON value shape. Projections and materialization can fail if the source
        /// changes; consumers must propagate that failure rather than substitute missing fields.
        input: crate::json::Json,
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
    /// The text carried by this block, including thinking and tool output.
    pub fn text(&self) -> Option<&crate::lazy::Text> {
        match self {
            Self::Text { text } | Self::Thinking { text, .. } | Self::ToolResult { content: text, .. } => Some(text),
            _ => None,
        }
    }
    pub fn text_mut(&mut self) -> Option<&mut crate::lazy::Text> {
        match self {
            Self::Text { text } | Self::Thinking { text, .. } | Self::ToolResult { content: text, .. } => Some(text),
            _ => None,
        }
    }

    /// The block's serde `type` tag (`tool_use`, …): the one spelling of the wire word.
    pub fn type_tag(&self) -> &'static str {
        match self {
            Block::Text { .. } => "text",
            Block::Thinking { .. } => "thinking",
            Block::ToolUse { .. } => "tool_use",
            Block::ToolResult { .. } => "tool_result",
            Block::Image { .. } => "image",
            Block::File { .. } => "file",
        }
    }

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

/// One model call's token accounting.
///
/// The four token counts are **disjoint**: `input_tokens` is the *uncached* prompt only, and the
/// prompt the model actually read is `input_tokens + cache_read_tokens + cache_creation_tokens`
/// ([`Usage::prompt_tokens`]). That is Anthropic's convention; providers that report cached tokens
/// as a *subset* of input (OpenAI/Codex, Google/Gemini) are normalized by their adapter on parse
/// and re-inclusived by their emitter, so a sum over any mix of harnesses means one thing.
/// `reasoning_tokens`, where reported, is a subset of `output_tokens`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached prompt tokens (disjoint from the two cache counts).
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

/// Equal iff serialized equal: a nonfinite cost serializes as `null`, so any two nonfinite costs
/// are equal; an absent cost is omitted, so it stays distinct from one.
impl PartialEq for Usage {
    fn eq(&self, other: &Self) -> bool {
        self.input_tokens == other.input_tokens
            && self.output_tokens == other.output_tokens
            && self.cache_read_tokens == other.cache_read_tokens
            && self.cache_creation_tokens == other.cache_creation_tokens
            && self.reasoning_tokens == other.reasoning_tokens
            && match (self.cost_usd, other.cost_usd) {
                (Some(a), Some(b)) => a == b || (!a.is_finite() && !b.is_finite()),
                (a, b) => a.is_none() && b.is_none(),
            }
    }
}

impl Usage {
    /// Whether the source recorded no token counts or cost (recorded zeroes are present).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Every prompt token the call read: uncached input + cache reads + cache writes.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
    }

    /// Prompt plus output: the call's whole token throughput.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens().saturating_add(self.output_tokens.unwrap_or(0))
    }
}

/// A lightweight handle to a session discovered on disk, cheap to produce for listings/search
/// without parsing the whole transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Number of conversational messages: **user + assistant turns only** (a reply that spawned a
    /// sub-agent is one).
    ///
    /// This is the contract every adapter must meet — do not count system/meta records, tool
    /// results, sidecar files, or directory entries. The number a person would call "how long is
    /// this conversation", comparable across harnesses. Adapters that can't know it cheaply
    /// should compute it from the same records they'd parse into [`Role::User`]/[`Role::Assistant`]
    /// messages, not from a proxy like file count.
    ///
    /// `None` means **not counted yet**: a listing that read only the session's metadata (Zed,
    /// Hermes, Goose, Kimi, Kimi Code, Codex, Grok, OpenCode and a Cursor composer whose bubbles
    /// live in their own cells defer counting, because for them counting means decoding every
    /// message, or an OpenCode message's parts) until a full read
    /// or `cv index` certifies the number; a [`crate::stream::Demand::COMPLETE`] read, which also
    /// carries rewound and superseded rows, never does. An adapter whose discovery reads every
    /// turn record anyway (Claude, Cursor, Cline, Continue, Devin, Gemini, Qwen, LM Studio,
    /// OpenClaw, OpenSession, the exports) always counts, by the rule its parser kinds turns
    /// with, so `Some(0)` is a session with no turns, never a placeholder. Listings print a
    /// dash and `--json` emits `null` for an uncounted session; sorting by messages puts
    /// uncounted sessions last.
    #[serde(default)]
    pub message_count: Option<usize>,
}

impl Message {
    /// The title rule: what every listing, index, hit and label calls a session that carries no
    /// explicit title is its first turn that titles one. That is the exchange's prompt (what the
    /// person typed, or the task another agent handed this thread: `Role::User`,
    /// [`MessageKind::Prompt`]) once leading `<system-reminder>` blocks are peeled, unless what
    /// remains is empty, the `Caveat:` local-command preamble or a bare command wrapper. Injected
    /// context (instruction files, environment blocks) never titles a session.
    ///
    /// The title is the message's Text blocks joined by newlines, at most 80 characters. Reads at
    /// most [`TITLE_HEAD`] raw bytes of each lazy block; an unreadable one is the error.
    pub fn prompt_title(&self, resolver: &crate::lazy::Resolver) -> anyhow::Result<Option<String>> {
        if self.role != Role::User || self.kind != MessageKind::Prompt {
            return Ok(None);
        }
        let mut s = String::new();
        for b in &self.content {
            if let Block::Text { text } = b {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&text.try_resolve_prefix(resolver, TITLE_HEAD)?);
            }
        }
        Ok(meaningful_prompt(&s).map(|t| truncate(&t, Head::TITLE)))
    }

    /// Whether this message is a conversational turn in [`SessionRef::message_count`]'s sense: a
    /// typed prompt (one that spawns a sub-agent, an `@agent` mention, included) or a model reply
    /// (one that spawns, included). Tool results, injected context, notices, errors and carriers
    /// are not. The count a full read certifies in the catalog's counts record.
    pub fn is_turn(&self) -> bool {
        matches!(
            (self.role, self.kind),
            (Role::User | Role::Assistant, MessageKind::SubagentSpawn)
                | (Role::User, MessageKind::Prompt)
                | (Role::Assistant, MessageKind::Reply)
        )
    }

    /// Whether the model saw or said this message: [`MessageKind::is_model_visible`], plus a
    /// sub-agent spawn or return carried by a conversational role (the `Reply` that called the
    /// spawn tool, the tool result or delivery that brought the answer back). A spawn/return that
    /// is a harness's own System marker (Codex `SubAgentActivity`, Kimi Code `subagent.*`) is not.
    pub fn is_model_visible(&self) -> bool {
        self.kind.is_model_visible()
            || (matches!(self.kind, MessageKind::SubagentSpawn | MessageKind::SubagentReturn)
                && self.role != Role::System)
    }
}

impl SessionRef {
    /// The serialized field names in declaration order: the one spelling every row that re-spells
    /// a session (`cv ls --json`, the search row, the catalog) derives from. A field added to the
    /// struct without a name here fails `fields_are_the_serde_key_set`.
    pub const FIELDS: &'static [&'static str] = &[
        "id",
        "harness",
        "path",
        "cwd",
        "title",
        "created_at",
        "updated_at",
        "message_count",
    ];

    /// The [`Self::FIELDS`] on which `self` and `other` differ, by name: what a stored copy of a ref
    /// (an index's meta doc, an embedding row) compares, minus the fields it does not hold.
    pub fn changed_fields<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = &'static str> + 'a {
        let SessionRef {
            id,
            harness,
            path,
            cwd,
            title,
            created_at,
            updated_at,
            message_count,
        } = self;
        let same: [bool; Self::FIELDS.len()] = [
            *id == other.id,
            *harness == other.harness,
            *path == other.path,
            *cwd == other.cwd,
            *title == other.title,
            *created_at == other.created_at,
            *updated_at == other.updated_at,
            *message_count == other.message_count,
        ];
        Self::FIELDS
            .iter()
            .zip(same)
            .filter_map(|(field, same)| (!same).then_some(*field))
    }

    /// Compare copies of one logical session, with the preferred copy sorting first.
    /// Newest update (or creation) wins; harness and path break ties deterministically.
    pub fn preferred_copy_cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .updated_at
            .or(other.created_at)
            .cmp(&self.updated_at.or(self.created_at))
            .then_with(|| self.harness.as_str().cmp(other.harness.as_str()))
            .then_with(|| self.path.cmp(&other.path))
    }

    /// A bare ref — the three facts discovery always has (id, harness, where on disk) and nothing
    /// else known yet. Adapters fill the rest with struct-update syntax
    /// (`SessionRef { cwd, title, ..SessionRef::new(id, h, path) }`).
    pub fn new(id: impl Into<String>, harness: Harness, path: impl Into<PathBuf>) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness,
            path: path.into(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        }
    }

    /// A ref for a transcript named by path alone: the id is the file stem, the harness is the
    /// caller's, and nothing else is known (no cwd/title/timestamps, `message_count: None`). The one
    /// spelling for "read this file as a session" — a CLI given a path, a test over a fixture, a
    /// reshape verb opening the file it just wrote.
    pub fn from_path(path: impl Into<PathBuf>, harness: Harness) -> SessionRef {
        let path: PathBuf = path.into();
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        SessionRef::new(id, harness, path)
    }
}

/// One member of a session's sub-agent forest, in any harness: the agent's transcript as a
/// [`SessionRef`] plus what the harness recorded *around* it (Claude's `*.meta.json` sidecar and
/// workflow journal, Codex's spawn records, Devin's node tree). The row `cv events --subagents`,
/// `cv tools`, lanes and `/api/session/{harness}/{id}/subagents` read.
///
/// A deep session fans out into hundreds of these, and the sidecars are where their *purpose* and
/// *outcome* live.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentRef {
    /// The transcript itself (Claude: id = `agent-<agentId>`), parseable like any other session.
    pub session: SessionRef,
    /// The agent kind (Claude's sidecar `agentType`: `general-purpose`, `Explore`,
    /// `workflow-subagent`, a named custom agent, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// The human one-line task the parent gave it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The tool call in the *parent* transcript that spawned this agent: links the child back to
    /// the exact turn that launched it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The workflow run id (`wf_…`) this agent belongs to, or `None` for a directly-spawned
    /// sub-agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// For workflow agents: the journaled outcome status (`done` / `partial` / `failed` / …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_status: Option<String>,
    /// For workflow agents: the journaled result summary — the agent's structured return value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
}

impl AgentRef {
    /// The agent's own id: the agent part of a `<parent>/<agent>` transcript id (Kimi, Kimi Code),
    /// else the transcript id with Claude's `agent-` prefix stripped, the key the workflow journal
    /// and the transcript records use.
    pub fn agent_id(&self) -> &str {
        match self.session.id.rsplit_once('/') {
            Some((_, agent)) => agent,
            None => self.session.id.strip_prefix("agent-").unwrap_or(&self.session.id),
        }
    }
}

/// A short human label from a title and/or the first [`Message::prompt_title`] — the projection
/// [`Session::label`] makes, exposed so streaming consumers can build it without a whole `Session`.
pub fn label_from(title: Option<&str>, prompt: Option<&str>) -> String {
    title
        .or(prompt)
        .map(|s| truncate(s, 72))
        .unwrap_or_else(|| "(untitled)".into())
}

/// How many raw bytes of each lazy text block [`Message::prompt_title`] reads: enough to peel the
/// reminders a harness prepends to a prompt.
pub const TITLE_HEAD: u64 = 8 * 1024;

/// A user turn's text as a real prompt, or `None` when it's only harness noise. Peels any leading
/// `<system-reminder>…</system-reminder>` and Cursor `<timestamp>…</timestamp>` blocks
/// and surrounding whitespace, then rejects the
/// residual if it's empty, the `Caveat:` local-command preamble, or a bare command wrapper
/// (`<command-name>`/`<command-message>`/`<local-command-stdout>`). Used by [`Message::prompt_title`].
fn meaningful_prompt(text: &str) -> Option<String> {
    let mut s = text.trim_start();
    loop {
        s = s.trim_start();
        let peeled = [
            ("<system-reminder>", "</system-reminder>"),
            ("<timestamp>", "</timestamp>"),
        ]
        .into_iter()
        .find_map(|(open, close)| {
            let rest = s.strip_prefix(open)?;
            Some(&rest[rest.find(close)? + close.len()..])
        });
        let Some(rest) = peeled else { break };
        s = rest;
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

/// What a run of bytes is, for [`est_tokens_bytes`]: content kinds tokenize at different rates.
///
/// Calibrated 2026-10-14 against the API's `count_tokens` on the exact requests Claude Code
/// 2.1.299 built when resuming 14 sessions (claude-opus-4-6 through claude-opus-5-5, tool-heavy and
/// prose-heavy, each unpruned and pruned): the per-kind token cost was isolated by blanking one
/// kind at a time. Measured bytes/token on the post-4.6 tokenizer: prose 2.4–2.7, tool-call input
/// 1.8–2.5, tool-result text 1.9–2.8, thinking signatures 3.6–5.8, prune markers 1.6. Each ratio
/// here sits at or below the measured middle, so estimates err high; the claude-opus-4-6 tokenizer
/// is about 1.35× sparser ([`tokenizer_scale`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Prose: user and assistant text, thinking plaintext.
    Text,
    /// A tool call's input JSON (compact serialization).
    ToolInput,
    /// A tool result's text.
    ToolResult,
    /// Opaque reasoning: thinking signatures, redacted/encrypted payloads.
    Signature,
    /// `cv prune` markers (`[PRUNED id=…]`): ids and hex tokenize densely.
    Marker,
}

impl TokenKind {
    pub const fn bytes_per_token(self) -> f64 {
        match self {
            TokenKind::Text => 2.5,
            TokenKind::ToolInput => 2.1,
            TokenKind::ToolResult => 2.2,
            TokenKind::Signature => 3.6,
            TokenKind::Marker => 1.5,
        }
    }
}

/// One image ≈ this many context tokens (image bytes are never inlined into the IR). Measured
/// images cost 1.4k–4.2k tokens; this sits high.
pub const IMAGE_TOKENS: u64 = 3000;

/// Structure per content block (type tags, tool ids and names): 8–25 tokens measured.
pub const BLOCK_TOKENS: u64 = 12;

/// The marker `cv prune` leaves where a payload was; [`est_str_tokens`] prices it as
/// [`TokenKind::Marker`].
const MARKER_PREFIX: &str = crate::prune::MARKER_PREFIX;

/// Byte-estimated tokens for `len` bytes of `kind` (ceiling). The one arithmetic behind every
/// token estimate cv prints.
pub fn est_tokens_bytes(len: usize, kind: TokenKind) -> u64 {
    (len as f64 / kind.bytes_per_token()).ceil() as u64
}

/// [`est_tokens_bytes`] for a string, pricing any embedded prune markers as [`TokenKind::Marker`].
pub fn est_str_tokens(text: &str, kind: TokenKind) -> u64 {
    let mut tokens = StrTokens::new(kind);
    tokens.push(text.as_bytes());
    tokens.tokens()
}

/// Streaming [`est_str_tokens`] over bytes in any chunking; also a [`std::io::Write`] sink, so a
/// serializer can be priced without materializing its output.
pub struct StrTokens {
    kind: TokenKind,
    plain: usize,
    marker: usize,
    in_marker: bool,
    carry: Vec<u8>,
}

impl StrTokens {
    pub fn new(kind: TokenKind) -> Self {
        StrTokens {
            kind,
            plain: 0,
            marker: 0,
            in_marker: false,
            carry: Vec::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let prefix = MARKER_PREFIX.as_bytes();
        let joined;
        let mut rest = if self.carry.is_empty() {
            bytes
        } else {
            let mut carry = std::mem::take(&mut self.carry);
            carry.extend_from_slice(bytes);
            joined = carry;
            &joined[..]
        };
        loop {
            if self.in_marker {
                match memchr::memchr(b']', rest) {
                    Some(end) => {
                        self.marker += end + 1;
                        rest = &rest[end + 1..];
                        self.in_marker = false;
                    }
                    None => {
                        self.marker += rest.len();
                        return;
                    }
                }
            } else if let Some(at) = memchr::memmem::find(rest, prefix) {
                self.plain += at;
                rest = &rest[at..];
                self.in_marker = true;
            } else {
                // A prefix may straddle the next chunk: hold back its longest possible start.
                let keep = (prefix.len() - 1).min(rest.len());
                self.plain += rest.len() - keep;
                self.carry = rest[rest.len() - keep..].to_vec();
                return;
            }
        }
    }

    pub fn tokens(&self) -> u64 {
        est_tokens_bytes(self.plain + self.carry.len(), self.kind) + est_tokens_bytes(self.marker, TokenKind::Marker)
    }
}

impl std::io::Write for StrTokens {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.push(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How much sparser a model's tokenizer is than the one [`TokenKind`] is calibrated for:
/// 1.35 for the Claude 4.6-and-earlier tokenizer (claude-opus-4-6, -sonnet-4-6, -opus-4-5,
/// -haiku-4-5, -sonnet-4-5 measured 3.77 bytes/token on prose where 4.7 and later measured 2.74),
/// 1 otherwise, including unknown models.
pub fn tokenizer_scale(model: &str) -> f64 {
    let Some(rest) = model.strip_prefix("claude-") else {
        return 1.0;
    };
    let mut parts = rest.split('-').skip_while(|p| p.parse::<u32>().is_err() || p.len() > 2);
    let (Some(major), minor) = (parts.next().and_then(|p| p.parse::<u32>().ok()), parts.next()) else {
        return 1.0;
    };
    // A dated id (`claude-opus-4-20250514`) or a missing minor is the .0 release.
    let minor = minor
        .filter(|m| m.len() <= 2)
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    if major < 4 || (major == 4 && minor <= 6) {
        1.35
    } else {
        1.0
    }
}

/// Estimated context tokens for one block: [`BLOCK_TOKENS`] plus each field at its kind's rate —
/// text as prose; thinking plaintext as prose plus signature/encrypted bytes as
/// [`TokenKind::Signature`]; tool-call input JSON; tool-result content; [`IMAGE_TOKENS`] per image;
/// nothing for a file reference. Shared by doctor and distill; provider-reported usage statistics
/// use [`Usage`] instead.
pub fn est_tokens(block: &Block) -> anyhow::Result<u64> {
    est_tokens_with(block, |text| Ok(text.len()))
}

/// The same estimate as [`est_tokens`] after materializing the block, without retaining its
/// decoded text. Spans are read in bounded chunks; malformed fields retain the resolver's raw
/// fallback. Unavailable sources and out-of-bounds spans return errors instead of zero tokens.
/// This is useful for byte-counting consumers such as the compaction doctor.
pub fn est_tokens_resolved(block: &Block, resolver: &crate::lazy::Resolver) -> anyhow::Result<u64> {
    est_tokens_with(block, |text| text.try_decoded_len(resolver))
}

/// Token estimate when a lazy text/result/thinking block's decoded byte count was already observed.
pub(crate) fn est_tokens_with_len(block: &Block, decoded_len: Option<usize>) -> anyhow::Result<u64> {
    est_tokens_with(block, |text| Ok(decoded_len.unwrap_or_else(|| text.len())))
}

fn est_tokens_with(
    block: &Block,
    text_len: impl Fn(&crate::lazy::Text) -> anyhow::Result<usize>,
) -> anyhow::Result<u64> {
    use TokenKind::*;
    let b = est_tokens_bytes;
    Ok(BLOCK_TOKENS
        + match block {
            Block::Text { text } => b(text_len(text)?, Text),
            Block::Thinking {
                text,
                signature,
                encrypted,
                ..
            } => {
                b(text_len(text)?, Text)
                    + b(signature.as_deref().map_or(0, str::len), Signature)
                    + b(encrypted.as_deref().map_or(0, str::len), Signature)
            }
            Block::ToolUse { input, .. } => b(usize::try_from(input.canonical_len()?)?, ToolInput),
            Block::ToolResult { content, .. } => b(text_len(content)?, ToolResult),
            Block::Image { .. } => IMAGE_TOKENS,
            Block::File { .. } => 0,
        })
}

/// Whether a tool is an MCP server's: Claude Code names those `mcp__<server>__<tool>`. Decided by
/// name, not by `Block::ToolUse::namespace`: the Claude adapter sets no namespace on MCP calls,
/// and Codex's one namespace (`collaboration`) is its built-in agent tools, not MCP.
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

/// One tool call as [`tool_index`] sees it: where it was made, what was called, with what.
#[derive(Debug, Clone, Copy)]
pub struct ToolCall<'a> {
    /// Index into the messages of the assistant turn that made the call.
    pub msg: usize,
    pub name: &'a str,
    pub input: &'a crate::json::Json,
}

/// Every tool call in `messages` by its `tool_use_id`. Tool results carry only the id (and
/// sometimes a name), so attributing a result to its call goes through this one index.
pub fn tool_index(messages: &[Message]) -> std::collections::HashMap<&str, ToolCall<'_>> {
    let mut map = std::collections::HashMap::new();
    for (msg, m) in messages.iter().enumerate() {
        for b in &m.content {
            if let Block::ToolUse { id, name, input, .. } = b {
                map.insert(
                    id.as_str(),
                    ToolCall {
                        msg,
                        name: name.as_str(),
                        input,
                    },
                );
            }
        }
    }
    map
}

/// A byte count with a unit (`5242880` → `5.0 MB`): the one spelling every report and renderer uses.
pub fn human_bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.1} {}", U[i])
    }
}

/// `~/dev/x` for a path under `$HOME`, the full path otherwise. `$HOME` (not `dirs::home_dir`) so a
/// test or a daemon running under a redirected home agrees with the CLI.
pub fn home_rel(p: &std::path::Path) -> String {
    if let Some(home) = crate::env::var_os("HOME").map(std::path::PathBuf::from) {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A head is `truncate` (of the trimmed text, for a trimmed head) however the text is chunked,
    /// and it stops reading once the answer cannot change.
    #[test]
    fn head_is_truncate_with_one_lookahead() {
        let texts = [
            String::new(),
            " \n ".into(),
            "a".repeat(79),
            "a".repeat(80),
            "a".repeat(81),
            format!("  {}  ", "é".repeat(80)),
            format!("{}\n{}", "x".repeat(40), " ".repeat(500)),
            format!("{}{}", " ".repeat(200), "late"),
            format!("{} {}", "λ".repeat(80), " ".repeat(300)),
        ];
        for text in &texts {
            let chars: Vec<char> = text.chars().collect();
            for width in [1, 3, 7, chars.len().max(1)] {
                for trim in [false, true] {
                    let mut head = if trim { Head::trimmed(80) } else { Head::bounded(80) };
                    for chunk in chars.chunks(width) {
                        head.push(&chunk.iter().collect::<String>());
                    }
                    let expected = if trim { text.trim() } else { text.as_str() };
                    assert_eq!(head.finish(), truncate(expected, 80), "{text:?} trim={trim}");
                    assert_eq!(head.is_blank(), text.trim().is_empty());
                    assert!(head.as_str().chars().count() <= Head::kept(80));
                }
            }
        }
    }

    /// Every `Option` set and a record retained: serde emits one key per field (the destructuring
    /// in `Serialize for Message` makes a new field a compile error), the record appears only as
    /// `extra._record`, the streaming writer agrees byte for byte, and the output reads back.
    #[test]
    fn message_key_set_is_its_fields_and_the_record_rides_extra() {
        let mut messages = vec![Message::new(Role::Assistant)];
        let m = &mut messages[0];
        m.id = Some("a".into());
        m.parent_id = Some("p".into());
        m.timestamp = Some(chrono::DateTime::from_timestamp(1, 0).unwrap());
        m.model = Some("m".into());
        m.content.push(Block::Text { text: "t".into() });
        m.usage = Some(Usage::default());
        m.extra.insert("claude".into(), serde_json::json!({"k": 1}));
        CompleteRecord::attach(
            &mut messages,
            Harness::Claude,
            crate::json::Json::Owned(serde_json::json!({"type": "assistant", "uuid": "a"})),
            serde_json::Map::new(),
        );
        let m = &messages[0];
        let value = serde_json::to_value(m).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "parent_id",
                "role",
                "kind",
                "origin",
                "timestamp",
                "model",
                "content",
                "usage",
                "extra"
            ]
        );
        let extra: Vec<&str> = value["extra"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(extra, ["claude", CARRIER_KEY]);
        let mut written = Vec::new();
        m.write_json(&mut written, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(written, serde_json::to_vec(m).unwrap());
        let back: Message = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), value);
        let bare = serde_json::to_value(Message::new(Role::User)).unwrap();
        let keys: Vec<&str> = bare.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["role", "kind", "origin", "content"]);
        // `kind` and `origin` are part of the wire form: a message without them is not IR.
        for missing in ["kind", "origin"] {
            let mut partial = bare.clone();
            partial.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<Message>(partial).is_err(), "{missing}");
        }
    }

    /// Every `Option` set, so serde emits every field: the key set is exactly `FIELDS`, in order.
    #[test]
    fn fields_are_the_serde_key_set() {
        let full = SessionRef {
            cwd: Some("/w".into()),
            title: Some("t".into()),
            created_at: Some(chrono::DateTime::from_timestamp(1, 0).unwrap()),
            updated_at: Some(chrono::DateTime::from_timestamp(2, 0).unwrap()),
            message_count: Some(3),
            ..SessionRef::new("id", Harness::Claude, "/x.jsonl")
        };
        let value = serde_json::to_value(&full).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            SessionRef::FIELDS,
            "SessionRef::FIELDS must name every serialized field, in order"
        );
    }

    /// `changed_fields` names each field by its `FIELDS` spelling: changing one field changes
    /// exactly that name.
    #[test]
    fn changed_fields_names_each_field_once() {
        let base = SessionRef::new("id", Harness::Claude, "/x.jsonl");
        let at = chrono::DateTime::from_timestamp(1, 0);
        let edits: [(&str, SessionRef); 8] = [
            (
                "id",
                SessionRef {
                    id: "other".into(),
                    ..base.clone()
                },
            ),
            (
                "harness",
                SessionRef {
                    harness: Harness::Codex,
                    ..base.clone()
                },
            ),
            (
                "path",
                SessionRef {
                    path: "/y.jsonl".into(),
                    ..base.clone()
                },
            ),
            (
                "cwd",
                SessionRef {
                    cwd: Some("/w".into()),
                    ..base.clone()
                },
            ),
            (
                "title",
                SessionRef {
                    title: Some("t".into()),
                    ..base.clone()
                },
            ),
            (
                "created_at",
                SessionRef {
                    created_at: at,
                    ..base.clone()
                },
            ),
            (
                "updated_at",
                SessionRef {
                    updated_at: at,
                    ..base.clone()
                },
            ),
            (
                "message_count",
                SessionRef {
                    message_count: Some(0),
                    ..base.clone()
                },
            ),
        ];
        assert_eq!(edits.each_ref().map(|(name, _)| *name), SessionRef::FIELDS);
        for (name, edited) in &edits {
            assert_eq!(base.changed_fields(edited).collect::<Vec<_>>(), [*name]);
        }
        assert_eq!(base.changed_fields(&base.clone()).count(), 0);
    }

    #[test]
    fn labels_skip_unused_bodies_and_empty_user_turns() {
        let mut session = Session::new("label", Harness::Claude);
        session.title = Some("explicit title".into());
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text {
            text: crate::Text::Span(crate::Span {
                source: None,
                offset: 0,
                len: u64::MAX,
                escaped: false,
            }),
        });
        session.messages.push(message);
        assert_eq!(session.label(), "explicit title");
        session.title = None;
        session.messages[0].content = vec![Block::Text { text: " \n".into() }];
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "actual prompt".into(),
        });
        session.messages.push(user);
        assert_eq!(session.label(), "actual prompt");
    }

    #[test]
    fn full_materialization_rejects_missing_span_sources_without_erasing_text() {
        let world = crate::test_world("materialize-span-failure");
        let path = world.base.join("gone.txt");
        let span = crate::Text::Span(crate::Span {
            source: None,
            offset: 1,
            len: 4,
            escaped: false,
        });
        let mut session = Session::new("session", Harness::Claude);
        session.source_path = Some(path.clone());
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::Text { text: span.clone() });
        session.messages.push(message);
        assert!(session.materialize().is_err());
        assert!(matches!(&session.messages[0].content[0], Block::Text { text } if text == &span));

        std::fs::write(&path, "\"text\"").unwrap();
        let reads = session.source_reads();
        session.materialize().unwrap();
        assert!(reads.fresh(&mut crate::source::Probe::default()));
        assert!(matches!(&session.messages[0].content[0], Block::Text { text } if text.inline_str() == Some("text")));
        std::fs::remove_file(&path).unwrap();
        assert!(!reads.fresh(&mut crate::source::Probe::default()));
    }

    #[test]
    fn checked_text_materialization_preserves_lazy_arguments_and_reports_failed_text() {
        let world = crate::test_world("checked-message-text");
        let path = world.base.join("text.json");
        let body = "exact🌙\n\"".repeat(1024);
        let wire = serde_json::to_vec(&body).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let source = crate::Json::source(&path, 0, wire.len() as u64).unwrap();
        let text = source.with_view(|view| view.source_text()).unwrap().unwrap();
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::Text { text });
        message.content.push(Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            namespace: None,
            input: source,
        });
        message.content.push(Block::Thinking {
            text: crate::Text::Span(crate::Span {
                source: Some(path.clone()),
                offset: 1,
                len: wire.len() as u64 - 2,
                escaped: true,
            }),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        let original = message.clone();
        message.try_materialize_text(&crate::Resolver::new(None)).unwrap();
        assert!(matches!(&message.content[0], Block::Text { text: crate::Text::Inline(value) } if value == &body));
        assert!(matches!(
            &message.content[1],
            Block::ToolUse {
                input: crate::Json::Source(_),
                ..
            }
        ));
        assert!(
            matches!(&message.content[2], Block::Thinking { text: crate::Text::Inline(value), .. } if value == &body)
        );
        std::fs::remove_file(path).unwrap();
        let mut failed = original;
        assert!(failed.try_materialize_text(&crate::Resolver::new(None)).is_err());
        assert!(matches!(
            &failed.content[0],
            Block::Text {
                text: crate::Text::Source(_)
            }
        ));
    }

    #[test]
    fn resolved_token_estimate_matches_materialized_fields() {
        use crate::lazy::{Resolver, Span, Text};

        let dir = cv_testkit::temp_dir("resolved-token-estimate");
        let source = dir.join("body");
        // Spans cover valid JSON string bodies; malformed ones are refused, never estimated.
        for raw in [
            "hello π".as_bytes().to_vec(),
            br"\n\t\u03c0\ud83d\ude42".repeat(20_000),
            br"\u03c0".repeat(20_000),
            b"abc".repeat(30_000),
        ] {
            std::fs::write(&source, [&b"\""[..], &raw, b"\""].concat()).unwrap();
            for escaped in [false, true] {
                let resolver = Resolver::new(None);
                let text = Text::Span(Span {
                    source: Some(source.clone()),
                    offset: 1,
                    len: raw.len() as u64,
                    escaped,
                });
                let blocks = vec![
                    Block::Text { text: text.clone() },
                    Block::Thinking {
                        text: text.clone(),
                        signature: Some("abc".into()),
                        encrypted: Some("π".into()),
                        redacted: false,
                    },
                    Block::ToolResult {
                        tool_use_id: "t".into(),
                        content: text,
                        is_error: false,
                        tool_name: None,
                        status: None,
                        details: None,
                    },
                ];
                for block in blocks {
                    let mut message = Message::new(Role::Assistant);
                    message.content.push(block.clone());
                    message.materialize(&resolver).unwrap();
                    assert_eq!(
                        est_tokens_resolved(&block, &resolver).unwrap(),
                        est_tokens(&message.content[0]).unwrap(),
                        "escaped={escaped}"
                    );
                }
            }
        }
        let resolver = Resolver::new(Some(source));
        for (offset, len) in [(u64::MAX, 1), (1, u64::MAX - 2)] {
            let block = Block::Text {
                text: Text::Span(Span {
                    source: None,
                    offset,
                    len,
                    escaped: true,
                }),
            };
            let error = est_tokens_resolved(&block, &resolver).unwrap_err();
            assert!(error.to_string().contains("Text span is outside source"));
        }
        drop(resolver);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn markers_price_at_their_own_rate_across_any_chunking() {
        let marker = format!("{MARKER_PREFIX}t1 tool=Read | text 9000B/12L | retrieve: cv cat s t1]");
        let text = format!("{}{marker}{}", "a".repeat(250), "b".repeat(250));
        let whole = est_str_tokens(&text, TokenKind::ToolResult);
        assert_eq!(
            whole,
            est_tokens_bytes(500, TokenKind::ToolResult) + est_tokens_bytes(marker.len(), TokenKind::Marker)
        );
        for split in 0..text.len() {
            let mut tokens = StrTokens::new(TokenKind::ToolResult);
            tokens.push(&text.as_bytes()[..split]);
            tokens.push(&text.as_bytes()[split..]);
            assert_eq!(tokens.tokens(), whole, "split at {split}");
        }
    }

    #[test]
    fn tokenizer_scale_separates_the_pre_4_7_tokenizer() {
        for old in [
            "claude-opus-4-6",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
            "claude-opus-4-20250514",
            "claude-3-5-sonnet-20241022",
        ] {
            assert_eq!(tokenizer_scale(old), 1.35, "{old}");
        }
        for new in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-fable-5-1",
            "claude-opus-5-5",
            "<synthetic>",
            "gpt-6",
        ] {
            assert_eq!(tokenizer_scale(new), 1.0, "{new}");
        }
    }

    #[test]
    fn token_json_byte_count_matches_compact_serialization() {
        for input in [
            serde_json::json!(null),
            serde_json::json!([1, -100, 1.5, true, false, null]),
            serde_json::json!({"escape\n": "\u{0000}\t\n\\\"π🙂".repeat(20_000), "nested": [{"x": 9}]}),
        ] {
            assert_eq!(
                crate::json::Json::from(input.clone()).canonical_len().unwrap(),
                input.to_string().len() as u64
            );
        }
    }

    #[test]
    fn source_tool_input_preserves_wire_tokens_and_propagates_read_failure() {
        let dir = cv_testkit::temp_dir("ir-source-tool-input");
        let path = dir.join("arguments.json");
        let raw = br#"{"z":"hello\n\ud83c\udf19","a":[true,1e2],"z":"final\u03c0"}"#;
        std::fs::write(&path, raw).unwrap();
        let value: serde_json::Value = serde_json::from_slice(raw).unwrap();
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "t".into(),
            name: "Edit".into(),
            input: crate::json::Json::source(&path, 0, raw.len() as u64).unwrap(),
            namespace: None,
        });
        assert_eq!(
            est_tokens(&message.content[0]).unwrap(),
            BLOCK_TOKENS + est_tokens_bytes(serde_json::to_vec(&value).unwrap().len(), TokenKind::ToolInput)
        );
        let mut text = "earlier\n".to_string();
        message.append_searchable(&mut text).unwrap();
        assert_eq!(text, format!("earlier\nEdit {value}\n"));
        let wire = serde_json::to_vec(&message).unwrap();
        let restored: Message = serde_json::from_slice(&wire).unwrap();
        assert!(matches!(
            restored.content[0],
            Block::ToolUse {
                input: crate::json::Json::Owned(_),
                ..
            }
        ));
        let mut owned = message.clone();
        owned.materialize(&crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(serde_json::to_vec(&owned).unwrap(), wire);
        std::fs::remove_file(&path).unwrap();
        assert!(est_tokens(&message.content[0]).is_err());
        assert!(message.append_searchable(&mut String::new()).is_err());
        assert!(message.materialize(&crate::lazy::Resolver::new(None)).is_err());
        assert!(serde_json::to_vec(&message).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn human_bytes_picks_the_unit() {
        assert_eq!(human_bytes(10), "10 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(human_bytes(5 << 30), "5.0 GB");
    }

    /// One spelling of a harness name on every door. Serde used to derive `rename_all =
    /// "lowercase"`, which spells the VARIANT — so `KimiCode` serialized as `"kimicode"` while
    /// `as_str()`, `cv --harness` and `cv ls --json` all said `"kimi-code"`, and a consumer
    /// filtering on the name got nothing back from `cv show --json`. Five hyphenated harnesses
    /// were affected.
    #[test]
    fn harness_serializes_as_its_canonical_name() {
        for h in Harness::ALL {
            let json = serde_json::to_string(&h).expect("serialize");
            assert_eq!(
                json,
                format!("{:?}", h.as_str()),
                "{h:?} must serialize as its canonical name, not its variant"
            );
            let back: Harness = serde_json::from_str(&json).expect("round trip");
            assert_eq!(back, h);
        }
        // The spelling the rest of the CLI uses is accepted on the way back in.
        let h: Harness = serde_json::from_str("\"kimi-code\"").unwrap();
        assert_eq!(h, Harness::KimiCode);
        assert!(
            serde_json::from_str::<Harness>("\"kimicode\"").is_ok(),
            "aliases still parse"
        );
        assert!(
            serde_json::from_str::<Harness>("\"marsrover\"").is_err(),
            "unknown is an error"
        );
    }

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

    /// A retired harness name is neither a harness nor an unknown word: `parse` refuses it and
    /// `retired` says where its sessions went, so the CLI can fail with a pointer (exit 2). No
    /// live harness is retired.
    #[test]
    fn retired_names_do_not_parse_and_point_somewhere() {
        for name in [
            "claude-app",
            "claudeapp",
            "claude-desktop",
            "chatgpt-app",
            "chatgpt",
            "openai-app",
        ] {
            assert_eq!(Harness::parse(name), None, "{name} must not parse");
            assert!(Harness::retired(name).is_some(), "{name} must carry a pointer");
        }
        for h in Harness::ALL {
            assert_eq!(Harness::retired(h.as_str()), None, "{h} is live");
        }
        assert_eq!(Harness::retired("marsrover"), None);
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
        assert_eq!(
            meaningful_prompt("<timestamp>Monday, Oct 12, 2026, 7:50 PM (UTC-4)</timestamp>\nreal question").as_deref(),
            Some("real question")
        );
        assert_eq!(
            meaningful_prompt("<timestamp>metadata</timestamp><system-reminder>noise</system-reminder>"),
            None
        );
        assert_eq!(
            meaningful_prompt("<timestamp>unfinished").as_deref(),
            Some("<timestamp>unfinished")
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
            title: Some("explicit".into()),
            messages: vec![user("<system-reminder>noise</system-reminder>\n\nask about otters")],
            ..Session::new("s", Harness::Claude)
        };
        // Explicit title wins.
        assert_eq!(s.synth_title().as_deref(), Some("explicit"));
        // Without one, the first meaningful user turn is synthesized (reminder peeled).
        s.title = None;
        assert_eq!(s.synth_title().as_deref(), Some("ask about otters"));
        // A harness-injected turn (Codex's AGENTS.md / environment context) is never the title.
        let mut injected = user("# AGENTS.md instructions for /work");
        injected.kind = MessageKind::InjectedContext;
        s.messages.insert(0, injected);
        // Nor is any other user-role turn around the exchange: only a prompt titles a session.
        let mut summary = user("Summary of the conversation so far");
        summary.kind = MessageKind::CompactionSummary;
        s.messages.insert(0, summary);
        assert_eq!(s.synth_title().as_deref(), Some("ask about otters"));
        assert_eq!(s.label(), "ask about otters");
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

#[cfg(test)]
mod usage_arithmetic_tests {
    use super::Usage;

    #[test]
    fn token_aggregates_saturate_at_the_representable_limit() {
        let usage = Usage {
            input_tokens: Some(u64::MAX),
            cache_read_tokens: Some(1),
            cache_creation_tokens: Some(2),
            output_tokens: Some(3),
            ..Default::default()
        };
        assert_eq!(usage.prompt_tokens(), u64::MAX);
        assert_eq!(usage.total_tokens(), u64::MAX);
        let usage = Usage {
            input_tokens: Some(u64::MAX - 3),
            output_tokens: Some(4),
            ..Default::default()
        };
        assert_eq!(usage.prompt_tokens(), u64::MAX - 3);
        assert_eq!(usage.total_tokens(), u64::MAX);
        assert_eq!(Usage::default().total_tokens(), 0);
    }
}
