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
    pub messages: Vec<Message>,
    /// Where this session was read from on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    /// Session-level metadata that doesn't have a first-class home yet.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Session {
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

/// One message/turn in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub content: Vec<Block>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Harness-specific fields preserved verbatim for lossless-ish round-tripping.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Message {
    pub fn new(role: Role) -> Self {
        Message {
            id: None,
            parent_id: None,
            role,
            timestamp: None,
            model: None,
            content: Vec::new(),
            usage: None,
            extra: serde_json::Map::new(),
        }
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

/// A unit of message content. Tagged so it (de)serializes to clean JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
