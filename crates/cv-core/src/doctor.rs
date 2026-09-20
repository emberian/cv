//! Compaction doctor: attribute a session's context-window pressure to its sources.
//!
//! Powers both `cv doctor` (CLI rendering) and the `doctor` MCP tool. Walks a session's typed
//! blocks and tallies context tokens by source — tool results (ranked per tool, MCP vs builtin),
//! thinking (plaintext PLUS the signature/encrypted blob, both re-sent to the API), assistant/user
//! text, images, the system reminders Claude Code appends per turn (hook output, edited-file
//! notices, queued task notifications, CLAUDE.md, … — itemized by attachment kind) — plus
//! compaction frequency/triggers and a usage-derived size of the fixed system+tools overhead. That
//! fixed block (base prompt, skills, MCP tool schemas) can't be itemized: the transcript records
//! the conversation, not the system block, so it's only *sized* from token usage, not broken down.

use crate::compaction;
use crate::ir::{Block, Message, MessageKind, Role, Session};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

/// Claude tokenizer ≈ 3.3–3.7 bytes/token.
const BYTES_PER_TOKEN: f64 = 3.5;

/// The bucket name for an injected-context turn: the harness's own attachment kind when it recorded
/// one (`extra[h]["attachment_type"]`, e.g. Claude's `hook_success`), else the IR kind name.
fn injected_kind(m: &Message) -> String {
    m.extra
        .values()
        .filter_map(Value::as_object)
        .find_map(|bag| bag.get("attachment_type"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| m.kind.as_str().to_string())
}
/// One image tile ≈ this many tokens (image bytes are never inlined into the IR).
const IMAGE_TOKENS: u64 = 1500;

fn toks(byte_len: usize) -> u64 {
    (byte_len as f64 / BYTES_PER_TOKEN).ceil() as u64
}

#[derive(Default, Debug, Clone, Serialize)]
pub struct ToolStat {
    pub calls: u64,
    pub result_tokens: u64,
    pub is_mcp: bool,
}

/// A context-attribution report. Accumulate sessions with [`Report::observe`], then render
/// (CLI) or serialize ([`Report::to_json`], for `--json` and the MCP tool).
#[derive(Default, Debug, Clone, Serialize)]
pub struct Report {
    pub sessions: u64,
    pub messages: u64,
    // measured conversational content, by source
    pub user_text: u64,
    pub assistant_text: u64,
    pub thinking: u64,
    pub tool_call_args: u64,
    pub tool_results: u64,
    pub images: u64,
    pub system_text: u64,
    /// System reminders Claude Code appended to the prompt (model-visible `attachment` records).
    pub attachments: u64,
    /// The same, by attachment kind (`hook_success`, `edited_text_file`, `queued_command`, …).
    pub by_attachment: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, ToolStat>,
    // compaction
    pub compactions: u64,
    pub auto_compactions: u64,
    pub pre_tokens: Vec<u64>,
    // usage-derived context sizing
    pub startup_ctx: u64, // worst per-session first-turn total context (≈ fixed overhead)
    pub peak_ctx: u64,    // largest total context observed across turns
}

impl Report {
    /// Fold one parsed session into the report: content attribution + compaction tally.
    pub fn observe(&mut self, session: &Session) {
        self.sessions += 1;
        self.messages += session.messages.len() as u64;

        // ToolResult blocks usually omit the tool name but carry `tool_use_id`; the matching
        // ToolUse has {id, name}. Map id→name first so results attribute to real tools (and MCP
        // names) instead of all collapsing into "(unknown tool)".
        let mut id_to_name: HashMap<&str, &str> = HashMap::new();
        for m in &session.messages {
            for b in &m.content {
                if let Block::ToolUse { id, name, .. } = b {
                    id_to_name.insert(id.as_str(), name.as_str());
                }
            }
        }

        let mut session_startup: Option<u64> = None;
        for m in &session.messages {
            if let Some(u) = &m.usage {
                let total = u.input_tokens.unwrap_or(0)
                    + u.cache_read_tokens.unwrap_or(0)
                    + u.cache_creation_tokens.unwrap_or(0);
                if total > 0 {
                    self.peak_ctx = self.peak_ctx.max(total);
                    if session_startup.is_none() {
                        session_startup = Some(total); // first turn ≈ system+tools+rules+first msg
                    }
                }
            }
            for b in &m.content {
                match b {
                    Block::Text { text } => {
                        let t = toks(text.len());
                        match (m.role, m.kind) {
                            // Harness-side notices, errors, markers and carriers are never sent to
                            // the model: they cost no context.
                            (_, MessageKind::Notice)
                            | (_, MessageKind::Error)
                            | (_, MessageKind::Carrier)
                            | (_, MessageKind::CompactionBoundary)
                            | (_, MessageKind::Branch)
                            | (_, MessageKind::ModelChange) => {}
                            // Context the harness injected into the prompt (system reminders, hook
                            // output, turn context): real per-turn context, itemized by kind — it
                            // used to hide inside "fixed overhead".
                            (_, MessageKind::InjectedContext) => {
                                self.attachments += t;
                                *self.by_attachment.entry(injected_kind(m)).or_default() += t;
                            }
                            (Role::Assistant, _) => self.assistant_text += t,
                            (Role::System, _) => self.system_text += t,
                            _ => self.user_text += t,
                        }
                    }
                    Block::Thinking {
                        text,
                        signature,
                        encrypted,
                        ..
                    } => {
                        // Thinking costs context as plaintext PLUS the signature/encrypted blob that
                        // rides with it — both re-sent to the API. Claude Code frequently records
                        // signature-only thinking (plaintext stripped), so counting just `text`
                        // reported a misleading zero.
                        self.thinking += toks(text.len())
                            + toks(signature.as_deref().map_or(0, str::len))
                            + toks(encrypted.as_deref().map_or(0, str::len));
                    }
                    Block::ToolUse { name, input, .. } => {
                        self.tool_call_args += toks(input.to_string().len());
                        let e = self.by_tool.entry(name.clone()).or_default();
                        e.calls += 1;
                        if name.starts_with("mcp__") {
                            e.is_mcp = true;
                        }
                    }
                    Block::ToolResult {
                        content,
                        tool_name,
                        tool_use_id,
                        ..
                    } => {
                        let t = toks(content.len());
                        self.tool_results += t;
                        let name = tool_name
                            .clone()
                            .or_else(|| id_to_name.get(tool_use_id.as_str()).map(|s| s.to_string()))
                            .unwrap_or_else(|| "(unknown tool)".to_string());
                        let mcp = name.starts_with("mcp__");
                        let e = self.by_tool.entry(name).or_default();
                        e.result_tokens += t;
                        if mcp {
                            e.is_mcp = true;
                        }
                    }
                    Block::Image { .. } => self.images += IMAGE_TOKENS,
                    _ => {}
                }
            }
        }

        for c in compaction::detect_in_session(session, false) {
            self.compactions += 1;
            if c.trigger.as_deref() == Some("auto") {
                self.auto_compactions += 1;
            }
            if let Some(p) = c.pre_tokens {
                self.pre_tokens.push(p);
            }
        }

        self.startup_ctx = self.startup_ctx.max(session_startup.unwrap_or(0));
    }

    /// Total measured conversational tokens across all sources.
    pub fn conv_total(&self) -> u64 {
        self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
            + self.attachments
    }

    /// Tool-result tokens attributable to MCP tools (name `mcp__*`).
    pub fn mcp_tool_results(&self) -> u64 {
        self.by_tool
            .values()
            .filter(|s| s.is_mcp)
            .map(|s| s.result_tokens)
            .sum()
    }

    /// The machine-readable diagnosis — `cv doctor --json` and the MCP `doctor` tool share this.
    pub fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .by_tool
            .iter()
            .map(|(name, s)| json!({"tool": name, "mcp": s.is_mcp, "calls": s.calls, "result_tokens": s.result_tokens}))
            .collect();
        let mcp = self.mcp_tool_results();
        json!({
            "sessions": self.sessions,
            "messages": self.messages,
            "compactions": self.compactions,
            "auto_compactions": self.auto_compactions,
            "pre_tokens": self.pre_tokens,
            "fixed_overhead_est": self.startup_ctx,
            "peak_context": self.peak_ctx,
            "conversational": {
                "total": self.conv_total(),
                "tool_results": self.tool_results,
                "thinking": self.thinking,
                "assistant_text": self.assistant_text,
                "user_text": self.user_text,
                "tool_call_args": self.tool_call_args,
                "images": self.images,
                "system_msgs": self.system_text,
                "system_reminders": self.attachments,
            },
            "by_attachment": self.by_attachment,
            "tool_results_mcp": mcp,
            "tool_results_builtin": self.tool_results.saturating_sub(mcp),
            "by_tool": tools,
        })
    }
}
