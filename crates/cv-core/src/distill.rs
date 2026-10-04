//! `cv distill` — reshape a long agent transcript into a compact context that a fresh model (or the
//! same lane, resumed) can work from, without re-reading every tool output it ever saw.
//!
//! The extraction is deterministic and rule-based (no model calls). It reads the IR, so any
//! harness cv parses can be distilled, and it produces four things from one analysis pass:
//!
//!  * a **brief**: the first human prompt, verbatim;
//!  * a **timeline** of everything said *to* the agent (coordinator prompts, peer messages, sub-agent
//!    returns, compaction summaries, harness errors) and everything it said or wrote *itself* (its
//!    text blocks, its outbound `SendMessage`s, notes it wrote to prose files, sub-agents it
//!    spawned), verbatim and in order;
//!  * a **facts index** mined from tool inputs and outputs: commits it made (`[branch sha] subject`),
//!    branches, hosts it reached over ssh/scp/rsync, the paths it touched most, build/test verdicts
//!    and every tool error;
//!  * a **ledger**: one line per tool call (label → salient outcome), with repeated polls collapsed;
//!
//! and then a **tail**: the last `keep_last` tool calls (and whatever the agent said around them)
//! kept as real turns, so the resumed model is still mid-motion rather than reading about itself.
//!
//! What is dropped: tool outputs before the tail (into a sidecar retrievable with `cv cat`, the
//! same `<stem>.flat.jsonl` format `cv prune` writes), harness reminders, and the model's thinking.
//! Thinking matters here: on current Claude models it is stored signature-only (no plaintext), so
//! a lane's *reasons* survive a distill only where the lane wrote them down — in its text, its
//! messages, its commit messages and notes. That is a property of the store, not of this module.

use crate::ir::{Block, Message, MessageKind, Origin, Role, Session};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Characters per token for distilled content. Prune's 3.5 bytes/token suits prose; a distilled
/// pack is dense with shas, paths, hosts and code, and measured on three real lanes (resumed with
/// Claude Code, harness overhead subtracted) it ran 2.2–2.4 characters per token.
const CHARS_PER_TOKEN: f64 = 2.4;

/// Knobs for [`distill`].
#[derive(Debug, Clone)]
pub struct DistillOptions {
    /// Keep the last N tool calls (and the turns around them) verbatim as the tail.
    pub keep_last: usize,
    /// Cap, in bytes, for one tool result inside the verbatim tail (head and tail kept, middle
    /// elided with a `cv cat` pointer). The full output always goes to the sidecar.
    pub tail_result_max: usize,
    /// Cap, in bytes, for a note the agent wrote to a prose file, a sub-agent's return, or a
    /// spawned sub-agent's prompt. The agent's own text and all messages to it are never capped.
    pub note_max: usize,
    /// Keep the agent's thinking blocks inside the tail (they cost context and, being
    /// signature-only, cannot be shown in the pack; the resumed model can still read them).
    pub keep_tail_thinking: bool,
    /// Include the one-line-per-tool-call ledger.
    pub ledger: bool,
    /// How the pack names the session that holds elided outputs, in its `cv cat <ref> <id>` hints.
    /// Defaults to the source session id.
    pub retrieve_ref: Option<String>,
}

impl Default for DistillOptions {
    fn default() -> Self {
        DistillOptions {
            keep_last: 12,
            tail_result_max: 6000,
            note_max: 6000,
            keep_tail_thinking: false,
            ledger: true,
            retrieve_ref: None,
        }
    }
}

/// A tool output that did not survive verbatim — written to the sidecar so `cv cat` can return it.
#[derive(Debug, Clone)]
pub struct Elided {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    pub content: String,
}

/// Size accounting for a distill. Token figures are byte estimates except `recorded_context_tokens`,
/// which is the source's own last recorded prompt size (what the next turn would re-read).
#[derive(Debug, Clone, Default, Serialize)]
pub struct DistillStats {
    pub source_id: String,
    pub messages: usize,
    pub tool_calls: usize,
    /// The last recorded prompt size of the source (input + cache tokens), when it has usage.
    pub recorded_context_tokens: Option<u64>,
    /// Byte estimate of everything model-visible in the source (text, tool calls, tool results).
    pub source_est_tokens: u64,
    pub pack_est_tokens: u64,
    pub tail_est_tokens: u64,
    /// `pack_est_tokens + tail_est_tokens`: what a resumed distilled session loads, before the
    /// harness's own system prompt and tool definitions.
    pub distilled_est_tokens: u64,
    pub tail_start: usize,
    pub tail_tool_calls: usize,
    pub timeline_entries: usize,
    pub commits: usize,
    pub errors: usize,
    pub elided_outputs: usize,
}

/// The result of [`distill`].
#[derive(Debug, Clone)]
pub struct Distilled {
    /// Brief + timeline + facts + ledger (the part that replaces the head of the transcript).
    pub head_md: String,
    /// The verbatim tail rendered as markdown (used by the pack; the resumable session carries the
    /// real turns instead).
    pub tail_md: String,
    /// Index into `session.messages` where the verbatim tail starts (`len` when there is none).
    pub tail_start: usize,
    pub elided: Vec<Elided>,
    pub stats: DistillStats,
    label: String,
    retrieve_ref: String,
}

impl Distilled {
    /// The standalone markdown context pack: a fresh agent's first message (candidate c).
    pub fn pack(&self) -> String {
        let mut s = self.head_md.clone();
        if !self.tail_md.is_empty() {
            s.push_str(&self.tail_md);
        }
        s
    }
}

// ── analysis ─────────────────────────────────────────────────────────────────────────────────

/// One entry in the chronological record of what was said to and by the agent.
struct Entry {
    idx: usize,
    when: String,
    heading: String,
    body: String,
}

#[derive(Default)]
struct Facts {
    commits: Vec<(usize, String, String, String)>, // idx, branch, sha, subject
    branches: Vec<(String, usize)>,                // name, mentions (first-seen order)
    hosts: Vec<(String, usize)>,
    paths: Vec<(String, usize)>,
    gates: Vec<(usize, String, String)>,  // idx, label, verdict
    tips: Vec<(usize, String, String)>,   // idx, branch, sha — latest observation wins
    heads: Vec<(usize, String, String)>,  // idx, "host:dir [ref]", "sha subject" — from `git log` output
    errors: Vec<(usize, String, String)>, // idx, label, first error line
}

fn bump(v: &mut Vec<(String, usize)>, key: &str) {
    if let Some(e) = v.iter_mut().find(|(k, _)| k == key) {
        e.1 += 1;
    } else {
        v.push((key.to_string(), 1));
    }
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

fn commit_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?m)^\[([^\]\s]+)(?: \([^)]*\))? ([0-9a-f]{7,40})\] (.+)$")
}
fn on_branch_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?m)^On branch (\S+)")
}
fn new_branch_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"git\s+(?:-C\s+\S+\s+)?(?:checkout\s+-[bB]|switch\s+-[cC]|worktree\s+add\s+(?:\S+\s+)*?-b)\s+([\w./-]+)",
    )
}
fn push_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"git\s+(?:-C\s+\S+\s+)?push\s+(?:-\S+\s+)*[\w./:@-]+\s+\+?([\w./-]+(?::[\w./-]+)?)",
    )
}
fn path_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?:^|[\s'`=(:,\x22])(~?/[\w.@+-]+(?:/[\w.@+-]+)+/?)")
}
fn remote_target_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?:^|\s)((?:[\w.-]+@)?[\w.-]+):[/~]")
}
fn gate_cmd_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"\blake\s+(?:build|env\s+lean|test)\b|\bcargo\s+(?:build|test|nextest|check|clippy)\b|swarm-build|\bmake\b|\bpytest\b|\bnpm\s+(?:run\s+)?test\b|\bgo\s+test\b|clean-build|local-gates|\bctest\b|\bninja\b|\brc=",
    )
}
fn verdict_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?i)\berror\b|\bfail|\bpassed\b|\bsuccess|\bcompleted\b|\brc=\d|exit code|✔|✖|\bbuilt\b|test result|declaration uses 'sorry'",
    )
}
fn error_line_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?i)\berror\b|\bfatal\b|denied|no such file|not found|cannot|can't|failed|refused|timed out|unknown|invalid|conflict|panicked|traceback",
    )
}
fn tip_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?m)^([0-9a-f]{40})\s+refs/heads/(\S+)$")
}
fn git_log_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"git\s+(?:-C\s+(\S+)\s+)?log\b((?:\s+-[^\s|;&)]*)*)(?:\s+([^\s|;&)'-][^\s|;&)']*))?",
    )
}
fn cd_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\bcd\s+([^\s;&|)']+)")
}
fn oneline_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^([0-9a-f]{7,40})\s+(\S.*)$")
}

/// The repository a single `git log` in `cmd` reads, and the HEAD line it printed: `(key, line)`.
/// The key is `host:dir`, plus `[ref]` when the log names a ref instead of HEAD. Commands with
/// several `git log`s (loops over many repos) are skipped — their output can't be attributed.
fn git_log_head(cmd: &str, out: &str) -> Option<(String, String)> {
    let mut logs = git_log_re().captures_iter(cmd);
    let c = logs.next()?;
    if logs.next().is_some() {
        return None;
    }
    let at = c.get(0)?.start();
    let dir = c
        .get(1)
        .map(|m| m.as_str().to_string())
        .or_else(|| cd_re().captures_iter(&cmd[..at]).last().map(|d| d[1].to_string()))
        .unwrap_or_else(|| ".".into());
    let host = hosts_in(cmd).into_iter().next();
    let mut key = match host {
        Some(h) => format!("{h}:{dir}"),
        None => dir,
    };
    if let Some(r) = c.get(3) {
        key = format!("{key} [{}]", r.as_str());
    }
    let line = out.lines().map(str::trim).find(|l| !l.is_empty())?;
    let m = oneline_re().captures(line)?;
    Some((key, format!("{} {}", &m[1], clip(&m[2], 90))))
}

fn heredoc_target_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(?:cat|tee(?:\s+-a)?)\s*>{0,2}\s*['"]?([^\s'"<>|;&]+)['"]?\s*<<-?\s*['"]?\w+"#,
    )
}

/// Prose files an agent writes to record state: notes, statuses, briefs, commit messages.
fn is_note_path(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
    let code_ext = [
        ".lean", ".rs", ".py", ".js", ".ts", ".tsx", ".json", ".toml", ".yaml", ".yml", ".sh", ".c", ".h", ".go",
        ".html", ".css", ".jsonl", ".lock", ".nix", ".sql",
    ];
    if code_ext.iter().any(|e| name.ends_with(e)) {
        return false;
    }
    name.ends_with(".md")
        || name.ends_with(".txt")
        || name.contains("msg")
        || name.contains("status")
        || name.contains("notes")
        || name.contains("report")
        || name.contains("handoff")
        || !name.contains('.')
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Keep `max` bytes of `s` as head + tail around an elision marker naming where the rest lives.
fn head_tail(s: &str, max: usize, pointer: &str) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = max * 3 / 5;
    let tail = max - head;
    let mut h = head;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!("{}\n[… {} bytes elided — {pointer} …]\n{}", &s[..h], t - h, &s[t..])
}

fn when(m: &Message) -> String {
    m.timestamp
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn attachment_type(m: &Message) -> Option<&str> {
    m.extra
        .get("claude")
        .and_then(|b| b.get("attachment_type"))
        .and_then(Value::as_str)
}

fn est(s: &str) -> u64 {
    (s.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as u64
}

fn block_est(b: &Block) -> u64 {
    match b {
        Block::Text { text } => est(text),
        Block::Thinking { text, signature, .. } => est(text) + signature.as_deref().map(est).unwrap_or(0),
        Block::ToolUse { input, name, .. } => est(name) + est(&input.to_string()),
        Block::ToolResult { content, .. } => est(content),
        Block::Image { .. } => 1500,
        Block::File { .. } => 0,
    }
}

fn message_est(m: &Message) -> u64 {
    if !m.kind.is_model_visible() {
        return 0;
    }
    m.content.iter().map(block_est).sum()
}

/// A short label for one tool call, for the ledger and the facts lists.
fn tool_label(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let l = match name {
        "Bash" => {
            let d = s("description");
            if d.is_empty() {
                clip(first_line(s("command")), 120)
            } else {
                clip(d, 120)
            }
        }
        "Read" => format!("Read {}", s("file_path")),
        "Write" => format!("Write {} ({} bytes)", s("file_path"), s("content").len()),
        "Edit" | "MultiEdit" | "NotebookEdit" => format!("{name} {}", s("file_path")),
        "Grep" | "Glob" => format!("{name} {:?} {}", s("pattern"), s("path")),
        "SendMessage" => format!("SendMessage → {}", s("to")),
        "Agent" | "Task" => format!("{name}: {}", s("description")),
        _ => format!("{name} {}", clip(&input.to_string(), 100)),
    };
    clip(&l, 140)
}

/// The command text of a tool call, when it has one (Bash and Bash-likes).
fn command_of(input: &Value) -> Option<&str> {
    input.get("command").and_then(Value::as_str)
}

/// ssh/scp/rsync targets in a shell command.
fn hosts_in(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    // ssh: the first non-option word after `ssh`, skipping options that take an argument.
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let with_arg = [
        "-o", "-p", "-i", "-J", "-l", "-F", "-E", "-c", "-D", "-L", "-R", "-W", "-b", "-m", "-O", "-Q", "-S", "-w",
    ];
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].trim_start_matches(['(', ';', '&', '|', '$', '`']);
        if t == "ssh" {
            let mut j = i + 1;
            while j < toks.len() && toks[j].starts_with('-') {
                j += if with_arg.contains(&toks[j]) { 2 } else { 1 };
            }
            if let Some(h) = toks.get(j) {
                let h = h.trim_matches(['\'', '"']);
                if !h.is_empty() && !h.starts_with('$') && h.chars().all(|c| c.is_alphanumeric() || "@.-_".contains(c))
                {
                    out.push(h.to_string());
                }
            }
            i = j;
        }
        i += 1;
    }
    if cmd.contains("scp ") || cmd.contains("rsync ") {
        for c in remote_target_re().captures_iter(cmd) {
            let h = &c[1];
            if !h.contains('/') && !h.starts_with("http") && h.len() > 2 {
                out.push(h.to_string());
            }
        }
    }
    out
}

fn note_paths(p: &str) -> bool {
    p.len() >= 8
        && !p.starts_with("/dev/")
        && !p.starts_with("/proc/")
        && !p.starts_with("/usr/")
        && !p.starts_with("/bin/")
        && !p.starts_with("/opt/homebrew")
        && !p.starts_with("/etc/")
}

/// Distill `session` (any harness; materialized) into a context pack, a tail cut point and the
/// elided-output list. Pure: no filesystem access.
pub fn distill(session: &Session, opts: &DistillOptions) -> Distilled {
    let msgs = &session.messages;
    let retrieve_ref = opts.retrieve_ref.clone().unwrap_or_else(|| session.id.clone());

    // Tool-use index: id → (message index, name, input).
    let mut uses: HashMap<&str, (usize, &str, &Value)> = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        for b in &m.content {
            if let Block::ToolUse { id, name, input, .. } = b {
                uses.insert(id.as_str(), (i, name.as_str(), input));
            }
        }
    }
    let tool_calls = uses.len();

    let tail_start = tail_cut(msgs, opts.keep_last);

    // Documents named in the brief or in any message to the agent are instructions by reference:
    // when the agent reads one, its latest read is kept (capped) in the timeline.
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in msgs {
        let incoming = (m.role == Role::User && m.kind == MessageKind::Prompt)
            || (m.kind == MessageKind::InjectedContext && matches!(attachment_type(m), Some("queued_command")));
        if !incoming {
            continue;
        }
        let t = m.text().unwrap_or_default();
        for c in path_re().captures_iter(&t) {
            referenced.insert(c[1].trim_end_matches(['.', ',', ';', ':', ')', '`', '\'']).to_string());
        }
    }
    // tool_use_id → path, for reads of a referenced document (Read, or a plain `cat`/`sed -n` of it).
    let mut doc_reads: HashMap<&str, String> = HashMap::new();
    for m in &msgs[..tail_start] {
        for b in &m.content {
            if let Block::ToolUse { id, name, input, .. } = b {
                let path = match name.as_str() {
                    "Read" => input.get("file_path").and_then(Value::as_str).map(String::from),
                    _ => command_of(input).and_then(|c| {
                        let c = c.trim();
                        let simple = c.starts_with("cat ") || c.starts_with("sed -n");
                        let p = c.split_whitespace().last()?.trim_matches(['\'', '"']);
                        (simple && !c.contains('|') && !c.contains(';') && !c.contains("&&")).then(|| p.to_string())
                    }),
                };
                if let Some(p) = path.filter(|p| referenced.contains(p)) {
                    doc_reads.insert(id.as_str(), p);
                }
            }
        }
    }
    let mut last_read_of: HashMap<String, &str> = HashMap::new();
    for m in &msgs[..tail_start] {
        for b in &m.content {
            if let Block::ToolResult {
                tool_use_id,
                is_error: false,
                ..
            } = b
            {
                if let Some(p) = doc_reads.get(tool_use_id.as_str()) {
                    last_read_of.insert(p.clone(), tool_use_id.as_str());
                }
            }
        }
    }
    let tail_tool_calls = msgs[tail_start..]
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, Block::ToolUse { .. }))
        .count();

    // ── pass: brief, timeline, facts, ledger ──
    let mut brief: Option<(usize, String)> = None;
    let mut timeline: Vec<Entry> = Vec::new();
    let mut facts = Facts::default();
    let mut ledger: Vec<(usize, usize, String, String)> = Vec::new(); // first idx, last idx, label, outcome
    let mut ledger_runs: Vec<usize> = Vec::new(); // repeat count per ledger row
    let mut elided: Vec<Elided> = Vec::new();

    for (i, m) in msgs.iter().enumerate() {
        let in_tail = i >= tail_start;
        match (m.role, m.kind) {
            (Role::User, MessageKind::Prompt) => {
                let text = m.text().unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                if brief.is_none() {
                    brief = Some((i, text));
                } else if !in_tail {
                    let who = if m.origin == Origin::Human {
                        "user"
                    } else {
                        "message to this agent"
                    };
                    timeline.push(Entry {
                        idx: i,
                        when: when(m),
                        heading: who.into(),
                        body: text,
                    });
                }
            }
            (_, MessageKind::CompactionSummary) if !in_tail => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "compaction summary (the harness's own digest of what came before)".into(),
                    body: m.text().unwrap_or_default(),
                });
            }
            (_, MessageKind::SubagentReturn) if !in_tail => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "sub-agent returned".into(),
                    body: head_tail(
                        &m.text().unwrap_or_default(),
                        opts.note_max,
                        "the full return is in the source transcript",
                    ),
                });
            }
            (Role::System, MessageKind::Error) if !in_tail => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "harness error".into(),
                    body: clip(&m.text().unwrap_or_default(), 300),
                });
            }
            (_, MessageKind::InjectedContext) if !in_tail => {
                // A message delivered mid-run (peer agent, coordinator, queued user input).
                if matches!(attachment_type(m), Some("queued_command")) {
                    timeline.push(Entry {
                        idx: i,
                        when: when(m),
                        heading: "message delivered while working".into(),
                        body: strip_reminder(&m.text().unwrap_or_default()),
                    });
                }
            }
            (Role::Assistant, _) => {
                for b in &m.content {
                    match b {
                        Block::Text { text } if !in_tail && !text.trim().is_empty() => {
                            timeline.push(Entry {
                                idx: i,
                                when: when(m),
                                heading: "agent said".into(),
                                body: text.to_string(),
                            });
                        }
                        Block::ToolUse { name, input, .. } => {
                            scan_input_facts(name, input, &mut facts);
                            if !in_tail {
                                if let Some(e) = own_words_from_tool(i, m, name, input, opts) {
                                    timeline.push(e);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            (Role::Tool, _) | (Role::User, MessageKind::ToolResult) => {
                for b in &m.content {
                    let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        tool_name,
                        ..
                    } = b
                    else {
                        continue;
                    };
                    let (name, input) = uses
                        .get(tool_use_id.as_str())
                        .map(|(_, n, inp)| (n.to_string(), (*inp).clone()))
                        .unwrap_or_else(|| (tool_name.clone().unwrap_or_default(), Value::Null));
                    let label = tool_label(&name, &input);
                    let out: &str = content;
                    let outcome = scan_result_facts(i, &name, &input, &label, out, *is_error, &mut facts);
                    if let Some(p) = doc_reads.get(tool_use_id.as_str()) {
                        if !in_tail && last_read_of.get(p) == Some(&tool_use_id.as_str()) {
                            timeline.push(Entry {
                                idx: i,
                                when: when(m),
                                heading: format!("agent read {p} (a document named in its instructions)"),
                                body: format!(
                                    "```\n{}\n```",
                                    head_tail(out, opts.note_max, &format!("cv cat {retrieve_ref} {tool_use_id}"))
                                ),
                            });
                        }
                    }
                    if (name == "Agent" || name == "Task") && !in_tail {
                        timeline.push(Entry {
                            idx: i,
                            when: when(m),
                            heading: format!(
                                "sub-agent returned ({})",
                                clip(input.get("description").and_then(Value::as_str).unwrap_or(""), 80)
                            ),
                            body: head_tail(out, opts.note_max, &format!("cv cat {retrieve_ref} {tool_use_id}")),
                        });
                    }
                    let keep_whole = in_tail && out.len() <= opts.tail_result_max;
                    if !keep_whole {
                        elided.push(Elided {
                            tool_use_id: tool_use_id.clone(),
                            tool_name: name.clone(),
                            input: input.clone(),
                            content: out.to_string(),
                        });
                    }
                    if !in_tail && opts.ledger {
                        let use_idx = uses.get(tool_use_id.as_str()).map(|u| u.0).unwrap_or(i);
                        match ledger.last_mut() {
                            Some(last) if last.2 == label => {
                                last.1 = use_idx;
                                last.3 = outcome;
                                *ledger_runs.last_mut().unwrap() += 1;
                            }
                            _ => {
                                ledger.push((use_idx, use_idx, label, outcome));
                                ledger_runs.push(1);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // Paths the agent itself names in prose count toward the path index too.
        if m.role == Role::Assistant {
            for b in &m.content {
                if let Block::Text { text } = b {
                    for c in path_re().captures_iter(text) {
                        let p = c[1].trim_end_matches(['.', ',', ';', ':', ')', '`']);
                        if note_paths(p) {
                            bump(&mut facts.paths, p);
                        }
                    }
                }
            }
        }
    }

    // ── render the head ──
    let label = session_label(session);
    let mut h = String::new();
    let _ = writeln!(h, "# Distilled context — {label}\n");
    let _ = writeln!(
        h,
        "Source: `{}:{}`{} · {} messages · {} tool calls. Distilled by `cv distill` (deterministic \
         extraction, no model). Anything elided is retrievable: `cv cat {retrieve_ref} <tool_use_id>`; \
         the full source stays readable with `cv show {} --range A..B` (bracketed numbers below are \
         its message indices).\n",
        session.harness.as_str(),
        session.id,
        session
            .lineage
            .parent
            .as_deref()
            .map(|p| format!(" (sub-agent of `{p}`)"))
            .unwrap_or_default(),
        msgs.len(),
        tool_calls,
        session.id,
    );
    if let Some((i, text)) = &brief {
        let _ = writeln!(h, "## 1. Brief (verbatim) [{i}]\n\n{}\n", text.trim());
    }
    let _ = writeln!(
        h,
        "## 2. Timeline — everything said to this agent, and everything it said or wrote itself (verbatim)\n"
    );
    if timeline.is_empty() {
        let _ = writeln!(h, "_(nothing before the verbatim tail)_\n");
    }
    for e in &timeline {
        let _ = writeln!(h, "### [{}] {} · {}\n\n{}\n", e.idx, e.when, e.heading, e.body.trim());
    }
    let _ = writeln!(h, "## 3. Facts (mined from tool inputs and outputs)\n");
    render_facts(&mut h, &facts);
    if opts.ledger && !ledger.is_empty() {
        let _ = writeln!(
            h,
            "## 4. Ledger — every tool call before the tail, one line each (`[index] label → outcome`; ×N = repeated)\n"
        );
        for ((a, b, label, outcome), n) in ledger.iter().zip(&ledger_runs) {
            let idx = if a == b {
                format!("[{a}]")
            } else {
                format!("[{a}–{b}]")
            };
            let rep = if *n > 1 { format!(" ×{n}") } else { String::new() };
            let _ = writeln!(h, "- {idx} {label}{rep} → {outcome}");
        }
        h.push('\n');
    }

    let tail_md = render_tail(msgs, tail_start, opts, &retrieve_ref);

    let source_est_tokens: u64 = msgs.iter().map(message_est).sum();
    let pack_est_tokens = est(&h);
    let tail_est_tokens: u64 = resumable_tail(msgs, tail_start, opts, &retrieve_ref)
        .iter()
        .map(|m| m.content.iter().map(block_est).sum::<u64>())
        .sum();
    let recorded_context_tokens = msgs
        .iter()
        .rev()
        .find_map(|m| m.usage.as_ref().map(|u| u.prompt_tokens()))
        .filter(|t| *t > 0);

    let stats = DistillStats {
        source_id: session.id.clone(),
        messages: msgs.len(),
        tool_calls,
        recorded_context_tokens,
        source_est_tokens,
        pack_est_tokens,
        tail_est_tokens,
        distilled_est_tokens: pack_est_tokens + tail_est_tokens,
        tail_start,
        tail_tool_calls,
        timeline_entries: timeline.len(),
        commits: facts.commits.len(),
        errors: facts.errors.len(),
        elided_outputs: elided.len(),
    };
    Distilled {
        head_md: h,
        tail_md,
        tail_start,
        elided,
        stats,
        label,
        retrieve_ref,
    }
}

/// A peer message arrives wrapped in a `<system-reminder>`; keep what is inside.
fn strip_reminder(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("<system-reminder")
        .map(|r| r.split_once('>').map(|x| x.1).unwrap_or(r))
        .unwrap_or(s);
    let s = s.strip_suffix("</system-reminder>").unwrap_or(s);
    s.trim().to_string()
}

/// The agent's own words carried by a tool call: messages it sent, notes it wrote, agents it spawned.
fn own_words_from_tool(i: usize, m: &Message, name: &str, input: &Value, opts: &DistillOptions) -> Option<Entry> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "SendMessage" => Some(Entry {
            idx: i,
            when: when(m),
            heading: format!("agent sent a message → {}", s("to")),
            body: s("message").to_string(),
        }),
        "Write" if is_note_path(s("file_path")) => Some(Entry {
            idx: i,
            when: when(m),
            heading: format!("agent wrote {}", s("file_path")),
            body: head_tail(s("content"), opts.note_max, "the full file is on disk"),
        }),
        "Agent" | "Task" => Some(Entry {
            idx: i,
            when: when(m),
            heading: format!("agent spawned a sub-agent: {}", s("description")),
            body: head_tail(
                s("prompt"),
                opts.note_max.min(2000),
                "the full prompt is in the source transcript",
            ),
        }),
        _ => {
            let cmd = command_of(input)?;
            let target = heredoc_target_re().captures(cmd)?.get(1)?.as_str().to_string();
            if !is_note_path(&target) || cmd.starts_with("python") {
                return None;
            }
            Some(Entry {
                idx: i,
                when: when(m),
                heading: format!("agent wrote {target} (shell)"),
                body: format!(
                    "```\n{}\n```",
                    head_tail(cmd, opts.note_max, "the full command is in the source transcript")
                ),
            })
        }
    }
}

fn scan_input_facts(_name: &str, input: &Value, f: &mut Facts) {
    let mut texts: Vec<&str> = Vec::new();
    if let Some(c) = command_of(input) {
        texts.push(c);
        for h in hosts_in(c) {
            bump(&mut f.hosts, &h);
        }
        for c2 in new_branch_re().captures_iter(c) {
            bump(&mut f.branches, &c2[1]);
        }
        for c2 in push_re().captures_iter(c) {
            let r = &c2[1];
            let b = r.rsplit(':').next().unwrap_or(r);
            bump(&mut f.branches, b);
        }
    }
    for k in ["file_path", "path", "notebook_path"] {
        if let Some(p) = input.get(k).and_then(Value::as_str) {
            if note_paths(p) {
                bump(&mut f.paths, p);
            }
        }
    }
    for t in texts {
        for c in path_re().captures_iter(t) {
            let p = c[1].trim_end_matches(['.', ',', ';', ':', ')', '`', '\'']);
            if note_paths(p) {
                bump(&mut f.paths, p);
            }
        }
    }
}

/// Mine one tool result; returns its ledger outcome.
fn scan_result_facts(
    i: usize,
    name: &str,
    input: &Value,
    label: &str,
    out: &str,
    is_error: bool,
    f: &mut Facts,
) -> String {
    let mut commit_outcome: Option<String> = None;
    for c in commit_re().captures_iter(out) {
        let (b, sha, subj) = (c[1].to_string(), c[2].to_string(), c[3].trim().to_string());
        // `git log --oneline`-style listings never use the bracketed form; a bracketed line is a
        // commit (or cherry-pick/amend) the agent made.
        if !f.commits.iter().any(|x| x.2 == sha) {
            bump(&mut f.branches, &b);
            commit_outcome.get_or_insert_with(|| format!("⊕ [{b} {sha}] {}", clip(&subj, 90)));
            f.tips.push((i, b.clone(), sha.clone()));
            f.commits.push((i, b, sha, subj));
        }
    }
    for c in on_branch_re().captures_iter(out) {
        bump(&mut f.branches, &c[1]);
    }
    if !is_error {
        if let Some((key, line)) = command_of(input).and_then(|c| git_log_head(c, out)) {
            f.heads.push((i, key, line));
        }
    }
    for c in tip_re().captures_iter(out) {
        bump(&mut f.branches, &c[2]);
        f.tips.push((i, c[2].to_string(), c[1][..12].to_string()));
    }
    if is_error {
        // A failed command's output often starts with whatever it printed before failing; the
        // diagnostic is the first error-shaped line, else the last line.
        let lines: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("Exit code"))
            .collect();
        let line = lines
            .iter()
            .find(|l| error_line_re().is_match(l))
            .or(lines.last())
            .copied()
            .unwrap_or("");
        let line = clip(line, 200);
        f.errors.push((i, label.to_string(), line.clone()));
        return format!("✗ {line}");
    }
    if let Some(c) = commit_outcome {
        return c;
    }
    let is_gate = name == "Bash" && command_of(input).is_some_and(|c| gate_cmd_re().is_match(c));
    if is_gate {
        let verdict = out
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| verdict_re().is_match(l))
            .or_else(|| out.lines().rev().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("(no output)");
        let verdict = clip(verdict, 160);
        f.gates.push((i, label.to_string(), verdict.clone()));
        return format!("⚑ {verdict}");
    }
    let n = out.lines().filter(|l| !l.trim().is_empty()).count();
    let fl = clip(first_line(out), 100);
    if n > 1 {
        format!("{fl} (+{} lines)", n - 1)
    } else if fl.is_empty() {
        "(no output)".into()
    } else {
        fl
    }
}

fn render_facts(h: &mut String, f: &Facts) {
    let list = |v: &Vec<(String, usize)>, max: usize| -> String {
        let mut v: Vec<&(String, usize)> = v.iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        v.iter()
            .take(max)
            .map(|(k, n)| format!("`{k}` ×{n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    if f.commits.is_empty() {
        let _ = writeln!(h, "- **Commits made**: none seen");
    } else {
        let _ = writeln!(h, "- **Commits made** (in order; the last is the newest):");
        for (i, b, sha, subj) in &f.commits {
            let _ = writeln!(h, "  - [{i}] `{b}` `{sha}` {}", clip(subj, 160));
        }
    }
    if !f.tips.is_empty() {
        // The newest observation per branch (a commit it made, or a `refs/heads` listing).
        let mut latest: Vec<&(usize, String, String)> = Vec::new();
        for t in f.tips.iter().rev() {
            if !latest.iter().any(|x| x.1 == t.1) {
                latest.push(t);
            }
        }
        latest.reverse();
        let _ = writeln!(
            h,
            "- **Branch tips last seen**: {}",
            latest
                .iter()
                .map(|(i, b, sha)| format!("`{b}` @ `{sha}` [{i}]"))
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    if !f.heads.is_empty() {
        // The newest `git log` head per repository (host:dir, or host:dir [ref]).
        let mut latest: Vec<&(usize, String, String)> = Vec::new();
        for t in f.heads.iter().rev() {
            if !latest.iter().any(|x| x.1 == t.1) {
                latest.push(t);
            }
        }
        latest.reverse();
        let _ = writeln!(
            h,
            "- **Repository heads last seen** (first line of the newest `git log` per repo):"
        );
        for (i, key, line) in latest.iter().rev().take(15).rev() {
            let _ = writeln!(h, "  - [{i}] `{key}` → `{line}`");
        }
    }
    if !f.branches.is_empty() {
        let _ = writeln!(h, "- **Branches**: {}", list(&f.branches, 15));
    }
    if !f.hosts.is_empty() {
        let _ = writeln!(h, "- **Hosts reached**: {}", list(&f.hosts, 12));
    }
    if !f.paths.is_empty() {
        let _ = writeln!(h, "- **Paths most referenced**: {}", list(&f.paths, 30));
    }
    if !f.gates.is_empty() {
        let _ = writeln!(h, "- **Build/test verdicts** (most recent last):");
        let skip = f.gates.len().saturating_sub(25);
        for (i, label, v) in f.gates.iter().skip(skip) {
            let _ = writeln!(h, "  - [{i}] {label} → {v}");
        }
    }
    if !f.errors.is_empty() {
        let _ = writeln!(h, "- **Tool errors hit** ({}):", f.errors.len());
        let skip = f.errors.len().saturating_sub(40);
        for (i, label, line) in f.errors.iter().skip(skip) {
            let _ = writeln!(h, "  - [{i}] {label} → {line}");
        }
    }
    h.push('\n');
}

/// Where the verbatim tail starts: the assistant message that issued the `keep_last`-th last tool
/// call, pulled back over adjacent assistant records (one API reply is several records) so it
/// opens on a whole reply. Every tool result after the cut then has its call after the cut too.
fn tail_cut(msgs: &[Message], keep_last: usize) -> usize {
    if keep_last == 0 {
        return msgs.len();
    }
    let mut seen = 0usize;
    let mut start = msgs.len();
    for (i, m) in msgs.iter().enumerate().rev() {
        if m.role != Role::Assistant {
            continue;
        }
        let calls = m.content.iter().filter(|b| matches!(b, Block::ToolUse { .. })).count();
        if calls == 0 {
            continue;
        }
        seen += calls;
        start = i;
        if seen >= keep_last {
            break;
        }
    }
    while start > 0 && msgs[start - 1].role == Role::Assistant {
        start -= 1;
    }
    // Never cut before the brief: the tail must follow it.
    let first_prompt = msgs
        .iter()
        .position(|m| m.role == Role::User && m.kind == MessageKind::Prompt);
    if let Some(p) = first_prompt {
        if start <= p {
            start = (p + 1).min(msgs.len());
            while start < msgs.len() && msgs[start].role != Role::Assistant {
                start += 1;
            }
        }
    }
    start
}

/// The tail as real IR turns for a resumable session: thinking dropped (unless asked), recorded
/// usage stripped (so a resume gate never reads the source's stale near-limit size), oversized
/// results clipped with a `cv cat` pointer, queued messages turned into user turns, harness-only
/// records dropped, and a trailing call with no result removed.
fn resumable_tail(msgs: &[Message], tail_start: usize, opts: &DistillOptions, retrieve_ref: &str) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for m in &msgs[tail_start..] {
        match (m.role, m.kind) {
            (Role::Assistant, _) => {
                let mut m2 = m.clone();
                m2.usage = None;
                if !opts.keep_tail_thinking {
                    m2.content.retain(|b| !matches!(b, Block::Thinking { .. }));
                }
                if !m2.content.is_empty() {
                    out.push(m2);
                }
            }
            (Role::Tool, _) | (Role::User, MessageKind::ToolResult) => {
                let mut m2 = m.clone();
                for b in &mut m2.content {
                    if let Block::ToolResult {
                        tool_use_id, content, ..
                    } = b
                    {
                        if content.len() > opts.tail_result_max {
                            let clipped = head_tail(
                                content,
                                opts.tail_result_max,
                                &format!("cv cat {retrieve_ref} {tool_use_id}"),
                            );
                            *content = clipped.into();
                        }
                    }
                }
                out.push(m2);
            }
            (Role::User, MessageKind::Prompt) => out.push(m.clone()),
            (_, MessageKind::InjectedContext) if matches!(attachment_type(m), Some("queued_command")) => {
                let mut u = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Subagent);
                u.id = m.id.clone();
                u.parent_id = m.parent_id.clone();
                u.timestamp = m.timestamp;
                u.content.push(Block::Text {
                    text: strip_reminder(&m.text().unwrap_or_default()).into(),
                });
                out.push(u);
            }
            _ => {}
        }
    }
    // Drop tool calls whose results never arrived (the run stopped mid-call).
    let answered: std::collections::HashSet<String> = out
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();
    for m in out.iter_mut().filter(|m| m.role == Role::Assistant) {
        m.content.retain(|b| match b {
            Block::ToolUse { id, .. } => answered.contains(id),
            _ => true,
        });
    }
    out.retain(|m| !m.content.is_empty());
    out
}

fn render_tail(msgs: &[Message], tail_start: usize, opts: &DistillOptions, retrieve_ref: &str) -> String {
    let tail = resumable_tail(msgs, tail_start, opts, retrieve_ref);
    if tail.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "## 5. The last {} tool calls, verbatim (from message [{tail_start}] on)\n",
        opts.keep_last
    );
    for m in &tail {
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    let who = if m.role == Role::Assistant {
                        "agent"
                    } else {
                        "message to this agent"
                    };
                    let _ = writeln!(s, "**{who}** ({}):\n\n{}\n", when(m), text.trim());
                }
                Block::ToolUse { id, name, input, .. } => {
                    let body = match command_of(input) {
                        Some(c) => c.to_string(),
                        None => serde_json::to_string_pretty(input).unwrap_or_default(),
                    };
                    let _ = writeln!(
                        s,
                        "▶ **{name}** `{id}` — {}\n```\n{}\n```\n",
                        tool_label(name, input),
                        body.trim()
                    );
                }
                Block::ToolResult { content, is_error, .. } => {
                    let tag = if *is_error { "◀ error" } else { "◀ result" };
                    let _ = writeln!(s, "{tag}\n```\n{}\n```\n", content.trim());
                }
                _ => {}
            }
        }
    }
    s
}

fn session_label(s: &Session) -> String {
    let desc = s
        .extra
        .get("claude")
        .and_then(|b| b.get("agent_description"))
        .and_then(Value::as_str);
    match desc {
        Some(d) => d.to_string(),
        None => s.label(),
    }
}

/// The preamble a resumed distilled session opens with, ahead of the head of the pack.
pub fn resume_preamble(d: &Distilled, keep_last: usize) -> String {
    format!(
        "[cv distill] You are resuming the agent whose history is distilled below; it was you. The \
         earlier transcript ({} messages, ~{} tokens) was compressed: your brief and every message \
         sent to you are verbatim; everything you said, sent or wrote down is verbatim; commits, \
         branches, hosts, paths, build verdicts and errors are indexed; every tool call is listed \
         in the ledger. Your last {keep_last} tool calls follow this message as real turns. Your \
         earlier private reasoning did not survive, so re-derive rather than assume, and check \
         live state (git status, running jobs) before acting. Elided tool output is retrievable \
         with `cv cat {} <tool_use_id>`.\n\n",
        d.stats.messages,
        d.stats.recorded_context_tokens.unwrap_or(d.stats.source_est_tokens),
        d.retrieve_ref,
    )
}

/// A resumable IR session: one opening prompt carrying the pack head, then the tail's real turns.
/// The caller emits it (as a session, or as a sub-agent of a root) with the Claude emitter.
pub fn resumable_session(session: &Session, d: &Distilled, opts: &DistillOptions) -> Session {
    let mut first = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
    first.timestamp = session
        .messages
        .get(d.tail_start)
        .and_then(|m| m.timestamp)
        .or(session.updated_at);
    first.content.push(Block::Text {
        text: format!("{}{}", resume_preamble(d, opts.keep_last), d.head_md).into(),
    });
    let mut tail = resumable_tail(&session.messages, d.tail_start, opts, &d.retrieve_ref);
    let mut messages = vec![first];
    messages.append(&mut tail);
    linearize(&mut messages);
    Session {
        id: session.id.clone(),
        harness: session.harness,
        cwd: session.cwd.clone(),
        title: Some(format!("distilled: {}", d.label)),
        created_at: session.created_at,
        updated_at: session.updated_at,
        model: session.model.clone(),
        git: session.git.clone(),
        system_prompt: None,
        lineage: crate::ir::Lineage {
            forked_from: Some(session.id.clone()),
            ..Default::default()
        },
        messages,
        source_path: None,
        extra: serde_json::Map::new(),
    }
}

/// Thread `messages` as one linear chain. Source parent links point at records a reshaped session
/// does not carry (dropped turns, and harness records the IR never holds, such as hook attachments);
/// Claude Code resumes by walking `parentUuid` back from the newest record and stops at the first
/// missing link, so a single dangling parent silently truncates the resumed context to the turns
/// after it. With every parent cleared, the emitter chains each record to the one before it.
pub fn linearize(messages: &mut [Message]) {
    for m in messages.iter_mut() {
        m.parent_id = None;
    }
}

/// Another lane's findings, for injection into this one's pack (`--with`): its own words (text,
/// messages sent, notes) and its facts — not its brief, ledger or tail.
pub fn findings(session: &Session, opts: &DistillOptions) -> String {
    let o = DistillOptions {
        keep_last: 0,
        ledger: false,
        ..opts.clone()
    };
    let d = distill(session, &o);
    // Keep only the agent's own entries from the timeline and the facts section.
    let mut out = String::new();
    let _ = writeln!(out, "## Findings from another lane — {} (`{}`)\n", d.label, session.id);
    let mut keep = false;
    let mut in_facts = false;
    for block in d.head_md.split("\n### ") {
        if block.starts_with("## 3. Facts") || block.contains("\n## 3. Facts") {
            in_facts = true;
        }
        let head = block.lines().next().unwrap_or("");
        let own = head.contains("· agent said") || head.contains("· agent sent") || head.contains("· agent wrote");
        if own && !in_facts {
            let section = block.split("\n## ").next().unwrap_or(block);
            let _ = writeln!(out, "### {}\n", section.trim());
            keep = true;
        }
    }
    if let Some(f) = d
        .head_md
        .split("## 3. Facts (mined from tool inputs and outputs)\n")
        .nth(1)
    {
        let facts = f.split("\n## ").next().unwrap_or(f);
        let _ = writeln!(out, "### Facts\n\n{}", facts.trim());
        keep = true;
    }
    if !keep {
        let _ = writeln!(out, "_(no findings)_");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;
    use serde_json::json;

    fn msg(role: Role, kind: MessageKind, blocks: Vec<Block>) -> Message {
        let mut m = Message::of_kind(role, kind, Origin::for_role(role));
        m.content = blocks;
        m
    }
    fn text(s: &str) -> Block {
        Block::Text { text: s.into() }
    }
    fn call(id: &str, cmd: &str, desc: &str) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: "Bash".into(),
            input: json!({"command": cmd, "description": desc}),
            namespace: None,
        }
    }
    fn result(id: &str, out: &str, err: bool) -> Block {
        Block::ToolResult {
            tool_use_id: id.into(),
            content: out.into(),
            is_error: err,
            tool_name: None,
            status: None,
            details: None,
        }
    }

    fn lane() -> Session {
        let mut msgs = vec![msg(
            Role::User,
            MessageKind::Prompt,
            vec![text("You are lane X. Build the thing on box1.")],
        )];
        for k in 0..20 {
            let id = format!("t{k}");
            msgs.push(msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![call(&id, "ssh ember@box1 'cat log'", "Poll the build")],
            ));
            msgs.push(msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result(&id, &"x\n".repeat(500), false)],
            ));
        }
        msgs.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![text("Chose rebase over merge: the queue wants linear ranges.")],
        ));
        msgs.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![call("c1", "cd /srv/lanes/x/src && git commit -F msg", "Commit wave 1")],
        ));
        msgs.push(msg(
            Role::Tool,
            MessageKind::ToolResult,
            vec![result(
                "c1",
                "[lane/x 4795b657] kernel: wave one\n 3 files changed",
                false,
            )],
        ));
        msgs.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![call("e1", "lake build Foo", "Build Foo")],
        ));
        msgs.push(msg(
            Role::Tool,
            MessageKind::ToolResult,
            vec![result("e1", "Exit code 1\nerror: unknown identifier `bar`", true)],
        ));
        msgs.push(msg(Role::Assistant, MessageKind::Reply, vec![call("z1", "ls", "List")]));
        Session {
            id: "agent-test".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            system_prompt: None,
            lineage: Default::default(),
            messages: msgs,
            source_path: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn keeps_brief_words_commits_errors_and_collapses_polls() {
        let s = lane();
        let d = distill(
            &s,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        );
        let p = d.pack();
        assert!(p.contains("You are lane X. Build the thing on box1."));
        assert!(p.contains("Chose rebase over merge"));
        assert!(p.contains("`lane/x` `4795b657` kernel: wave one"));
        assert!(p.contains("unknown identifier `bar`"));
        assert!(p.contains("Poll the build ×20"), "{p}");
        assert!(p.contains("`ember@box1` ×20"));
        // The 20 polls' outputs are elided, not inlined.
        assert!(!p.contains(&"x\n".repeat(50)));
        assert_eq!(d.stats.commits, 1);
        assert!(d.elided.len() >= 20);
    }

    #[test]
    fn tail_opens_on_an_assistant_turn_and_drops_unanswered_calls() {
        let s = lane();
        let d = distill(
            &s,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        );
        assert_eq!(s.messages[d.tail_start].role, Role::Assistant);
        let r = resumable_session(
            &s,
            &d,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        );
        assert_eq!(r.messages[0].role, Role::User);
        assert_eq!(r.messages[1].role, Role::Assistant);
        // `z1` never got a result: it must not survive into the resumable session.
        let has_z1 = r
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { id, .. } if id == "z1"));
        assert!(!has_z1);
        assert!(r.messages.iter().all(|m| m.usage.is_none()));
        assert!(
            r.messages.iter().all(|m| m.parent_id.is_none()),
            "reshaped sessions thread linearly"
        );
    }

    #[test]
    fn git_log_heads_are_keyed_by_host_and_dir() {
        let (k, l) = git_log_head(
            "ssh ember@box 'cd /srv/x/src && git log --oneline -3'",
            "5740efc6 resource(book)!: x\nabc1234 y",
        )
        .unwrap();
        assert_eq!(
            (k.as_str(), l.as_str()),
            ("ember@box:/srv/x/src", "5740efc6 resource(book)!: x")
        );
        let (k, _) = git_log_head("git -C /r log --oneline -1 origin/main", "1234567 m").unwrap();
        assert_eq!(k, "/r [origin/main]");
        assert!(git_log_head("for d in a b; do git -C $d log -1; git -C x log -1; done", "1234567 m").is_none());
    }

    #[test]
    fn hosts_skip_ssh_options() {
        assert_eq!(hosts_in("ssh -o ConnectTimeout=10 persvati 'ls'"), vec!["persvati"]);
        assert_eq!(hosts_in("scp -q ember@1.2.3.4:/srv/x /tmp/y"), vec!["ember@1.2.3.4"]);
    }
}
