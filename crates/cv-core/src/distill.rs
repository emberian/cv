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

use crate::ir::{
    est_tokens, est_tokens_bytes, tool_index, truncate, Block, Message, MessageKind, Origin, Role, Session,
};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::OnceLock;

/// A distilled pack is dense with shas, paths, hosts and code: measured on three real lanes
/// (resumed with Claude Code, harness overhead subtracted) it ran 2.2–2.4 bytes per token, so it is
/// priced like tool-call JSON. Blocks use the shared [`crate::ir::est_tokens`].
const PACK_KIND: crate::ir::TokenKind = crate::ir::TokenKind::ToolInput;

/// Knobs for [`prepare_distill`].
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

/// The result of [`prepare_distill`].
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
    /// The accepted source label shared by the pack and resume description.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Add another lane's extracted findings, keeping head/tail estimates on the same byte
    /// heuristic as the original distillation. Callers need not know its conversion constant.
    pub fn append_findings(&mut self, findings: &str) {
        self.head_md.push_str(findings);
        self.head_md.push('\n');
        self.stats.pack_est_tokens = est(&self.head_md);
        self.stats.distilled_est_tokens = self.stats.pack_est_tokens + self.stats.tail_est_tokens;
    }

    /// The standalone markdown context pack: a fresh agent's first message (candidate c).
    pub fn pack(&self) -> String {
        let mut s = self.head_md.clone();
        if !self.tail_md.is_empty() {
            s.push_str(&self.tail_md);
        }
        s
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Default, Debug)]
struct Work {
    tail_preparations: usize,
    elided_copy_bytes: usize,
    source_estimates: usize,
}
#[cfg(test)]
thread_local! { static WORK: std::cell::Cell<Work> = std::cell::Cell::new(Work::default()); }
#[cfg(test)]
fn note_work(f: impl FnOnce(&mut Work)) {
    WORK.with(|cell| {
        let mut work = cell.get();
        f(&mut work);
        cell.set(work);
    });
}

/// Full distillation products with one already transformed tail. Consume this value after
/// adding optional findings to `distilled.head_md`; the tail moves into the resumable session.
/// Unlike an implicit cache, this preparation owns its options and is consumed once.
pub struct PreparedDistillation {
    pub distilled: Distilled,
    tail: Vec<Message>,
    keep_last: usize,
}
impl PreparedDistillation {
    pub fn into_resumable_session(self, source: &Session) -> (Distilled, Session) {
        let output = session_with_tail(source, &self.distilled, self.keep_last, self.tail);
        (self.distilled, output)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Demand {
    Full,
    Pack,
    Findings,
}

/// Prepare the pack, statistics, sidecar roster and resumable tail together. CLI resume routes
/// consume the tail instead of constructing it again after rendering the pack.
pub fn prepare_distill(session: &Session, opts: &DistillOptions) -> anyhow::Result<PreparedDistillation> {
    analyze(session, opts, Demand::Full)
}

/// Serialize elided outputs through the shared bounded JSONL stage, then publish the sidecar
/// whole under `claim`. Inputs and content are borrowed from the existing elision roster; no
/// artifact-sized copy is made.
pub fn write_sidecar(path: &std::path::Path, elided: &[Elided], claim: crate::publish::Claim) -> anyhow::Result<()> {
    let mut stage = crate::harness::store::JsonlStage::new();
    for entry in elided {
        stage.push(crate::prune::BorrowedSidecarEntry::text(
            &entry.tool_use_id,
            &entry.tool_name,
            &entry.input,
            &entry.content,
        ))?;
    }
    stage.publish(path, claim)
}

// ── analysis ─────────────────────────────────────────────────────────────────────────────────

/// One entry in the chronological record of what was said to and by the agent.
struct Entry {
    idx: usize,
    when: String,
    heading: String,
    body: String,
}

struct Mentions {
    first: usize,
    count: usize,
}
type Counts = HashMap<String, Mentions>;

#[derive(Default)]
struct Facts {
    commits: Vec<(usize, String, String, String)>, // idx, branch (a quiet commit: its repo), sha, subject
    /// The shas in `commits`: a capture is checked against a set, not a scan of every commit so far.
    commit_shas: HashSet<String>,
    branches: Counts, // name, mentions and first-seen order
    hosts: Counts,
    paths: Counts,
    gates: Vec<(usize, String, String)>,  // idx, label, verdict
    tips: Vec<(usize, String, String)>,   // idx, branch, sha — latest observation wins
    heads: Vec<(usize, String, String)>,  // idx, "host:dir [ref]", "sha subject" — from `git log` output
    errors: Vec<(usize, String, String)>, // idx, label, first error line
    /// PID files the lane named (in a command or its own prose): path → last mention.
    pid_files: Vec<(usize, String)>,
    /// Directories it `cd`'d into, newest last.
    dirs: Vec<(usize, String)>,
}

impl Facts {
    /// Remember the newest mention of `value` in `list` (one row per distinct value).
    fn note_latest(list: &mut Vec<(usize, String)>, i: usize, value: &str) {
        list.retain(|(_, v)| v != value);
        list.push((i, value.to_string()));
    }
    fn note_pid_files(&mut self, i: usize, text: &str) {
        for c in re(Re::PidFile).captures_iter(text) {
            Self::note_latest(&mut self.pid_files, i, &c[1]);
        }
    }
}

fn bump(counts: &mut Counts, key: &str) {
    if let Some(mentions) = counts.get_mut(key) {
        mentions.count += 1;
    } else {
        let first = counts.len();
        counts.insert(key.to_owned(), Mentions { first, count: 1 });
    }
}

/// The distiller's patterns, each compiled on first use.
#[derive(Clone, Copy)]
enum Re {
    Commit,
    OnBranch,
    NewBranch,
    Push,
    Path,
    RemoteTarget,
    GateCmd,
    Verdict,
    ErrorLine,
    Tip,
    GitLog,
    Cd,
    Oneline,
    HeredocTarget,
    GitCommit,
    CommitFile,
    PidFile,
    OpenItem,
    Decision,
}

static RES: [OnceLock<Regex>; 19] = [const { OnceLock::new() }; 19];

fn re(which: Re) -> &'static Regex {
    let (cell, pat) = match which {
        Re::Commit => (&RES[0], r"(?m)^\[([^\]\s]+)(?: \([^)]*\))? ([0-9a-f]{7,40})\] (.+)$"),
        Re::OnBranch => (&RES[1], r"(?m)^On branch (\S+)"),
        Re::NewBranch => (
            &RES[2],
            r"git\s+(?:-C\s+\S+\s+)?(?:checkout\s+-[bB]|switch\s+-[cC]|worktree\s+add\s+(?:\S+\s+)*?-b)\s+([\w./-]+)",
        ),
        Re::Push => (
            &RES[3],
            r"git\s+(?:-C\s+\S+\s+)?push\s+(?:-\S+\s+)*[\w./:@-]+\s+\+?([\w./-]+(?::[\w./-]+)?)",
        ),
        Re::Path => (&RES[4], r"(?:^|[\s'`=(:,\x22])(~?/[\w.@+-]+(?:/[\w.@+-]+)+/?)"),
        Re::RemoteTarget => (&RES[5], r"(?:^|\s)((?:[\w.-]+@)?[\w.-]+):[/~]"),
        Re::GateCmd => (
            &RES[6],
            r"\blake\s+(?:build|env\s+lean|test)\b|\bcargo\s+(?:build|test|nextest|check|clippy)\b|swarm-build|\bmake\b|\bpytest\b|\bnpm\s+(?:run\s+)?test\b|\bgo\s+test\b|clean-build|local-gates|\bctest\b|\bninja\b|\brc=",
        ),
        Re::Verdict => (
            &RES[7],
            r"(?i)\berror\b|\bfail|\bpassed\b|\bsuccess|\bcompleted\b|\brc=\d|exit code|✔|✖|\bbuilt\b|test result|declaration uses 'sorry'",
        ),
        Re::ErrorLine => (
            &RES[8],
            r"(?i)\berror\b|\bfatal\b|denied|no such file|not found|cannot|can't|failed|refused|timed out|unknown|invalid|conflict|panicked|traceback",
        ),
        Re::Tip => (&RES[9], r"(?m)^([0-9a-f]{40})\s+refs/heads/(\S+)$"),
        Re::GitLog => (
            &RES[10],
            r"git\s+(?:-C\s+(\S+)\s+)?log\b((?:\s+-[^\s|;&)]*)*)(?:\s+([^\s|;&)'-][^\s|;&)']*))?",
        ),
        Re::Cd => (&RES[11], r"\bcd\s+([^\s;&|)']+)"),
        Re::Oneline => (&RES[12], r"^([0-9a-f]{7,40})\s+(\S.*)$"),
        Re::HeredocTarget => (
            &RES[13],
            r#"(?:cat|tee(?:\s+-a)?)\s*>{0,2}\s*['"]?([^\s'"<>|;&]+)['"]?\s*<<-?\s*['"]?\w+"#,
        ),
        // `git commit`, through any `-c key=value` / `-C dir` options; group 1 is a `-C` dir.
        Re::GitCommit => (&RES[14], r"\bgit(?:\s+-c\s+\S+|\s+-C\s+(\S+))*\s+commit\b"),
        // A PID file path: `/abs/…/name.pid`, `~/…`, or `$VAR/…` (how lanes name their own).
        Re::PidFile => (&RES[16], r"((?:~|\$\{?\w+\}?)?/[\w.@/${}-]*\.pid)\b"),
        // A line naming something still open, in the words lanes use for it.
        Re::OpenItem => (
            &RES[17],
            r"(?i)\bowed\b|\bbatch for\b|\bstill red\b|\bnot done\b|\bwaiting on\b",
        ),
        // A line recording a decision.
        Re::Decision => (&RES[18], r"(?i)\bdecided\b|\bruling\b|\bdefault\b"),
        // The message file of `git commit … -F <file>` (also `-qF`, `--file=`).
        Re::CommitFile => (
            &RES[15],
            r#"\bcommit\b[^|;&\n]*?\s(?:-[a-zA-Z]*F\s*|--file[=\s])['"]?([^\s'";&|]+)"#,
        ),
    };
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

/// The repository a single `git log` in `cmd` reads, and the HEAD line it printed: `(key, line)`.
/// The key is `host:dir`, plus `[ref]` when the log names a ref instead of HEAD. Commands with
/// several `git log`s (loops over many repos) are skipped — their output can't be attributed.
fn git_log_head(cmd: &str, out: &str) -> Option<(String, String)> {
    let mut logs = re(Re::GitLog).captures_iter(cmd);
    let c = logs.next()?;
    if logs.next().is_some() {
        return None;
    }
    let at = c.get(0)?.start();
    let dir = c
        .get(1)
        .map(|m| m.as_str().to_string())
        .or_else(|| re(Re::Cd).captures_iter(&cmd[..at]).last().map(|d| d[1].to_string()))
        .unwrap_or_else(|| ".".into());
    let host = hosts_in(cmd).into_iter().next();
    let mut key = match host {
        Some(h) => format!("{h}:{dir}"),
        None => dir,
    };
    if let Some(r) = c.get(3) {
        key = format!("{key} [{}]", r.as_str());
    }
    // A compound command prints other output first (a build's last line, a test verdict): the
    // head is the first `git log --oneline`-shaped line, wherever it sits.
    let (sha, subject) = oneline_lines(out).next()?;
    Some((key, format!("{sha} {}", truncate(subject, 90))))
}

/// The `<sha> <subject>` lines of `out`, in order. A line that is not the output's first must
/// carry a hex letter in its sha, so a count (`wc -l`'s `1234567 total`) is not read as a commit.
fn oneline_lines(out: &str) -> impl Iterator<Item = (&str, &str)> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .enumerate()
        .filter_map(|(n, line)| {
            let m = re(Re::Oneline).captures(line)?;
            let (sha, subject) = (m.get(1)?.as_str(), m.get(2)?.as_str().trim());
            (n == 0 || sha.bytes().any(|b| b.is_ascii_alphabetic())).then_some((sha, subject))
        })
}

/// Commits made quietly: `git commit -q` prints no `[branch sha] subject` line, so a lane that
/// commits that way (as minidregg's lanes must) and then prints `git log --oneline -1` shows its
/// commits only as oneline output. When a command runs k `git commit`s followed by a `git log`,
/// the first k oneline lines it printed are those commits: `(repo, sha, subject)`, the repo being
/// the commit's `-C` dir or the last `cd` before it.
fn quiet_commits<'a>(cmd: &str, out: &'a str) -> Vec<(String, &'a str, &'a str)> {
    let commits: Vec<_> = re(Re::GitCommit)
        .captures_iter(cmd)
        .filter(|c| {
            !cmd[c.get(0).map_or(0, |m| m.end())..]
                .trim_start()
                .starts_with("--dry-run")
        })
        .collect();
    let Some(last) = commits.last().and_then(|c| c.get(0)) else {
        return Vec::new();
    };
    if !re(Re::GitLog).is_match(&cmd[last.end()..]) {
        return Vec::new();
    }
    let repos: Vec<String> = commits
        .iter()
        .map(|c| {
            let at = c.get(0).map_or(0, |m| m.start());
            c.get(1)
                .map(|d| d.as_str().to_string())
                .or_else(|| re(Re::Cd).captures_iter(&cmd[..at]).last().map(|d| d[1].to_string()))
                .unwrap_or_else(|| ".".into())
        })
        .collect();
    oneline_lines(out)
        .zip(repos)
        .map(|((sha, subject), repo)| (repo, sha, subject))
        .collect()
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

fn est(s: &str) -> u64 {
    est_tokens_bytes(s.len(), PACK_KIND)
}

fn block_est(b: &Block) -> anyhow::Result<u64> {
    est_tokens(b)
}

// Count decoded model-visible bodies without retaining them for the analysis lifetime.
fn message_est_resolved(m: &Message, resolver: &crate::Resolver) -> anyhow::Result<u64> {
    #[cfg(test)]
    note_work(|work| work.source_estimates += 1);
    if !m.is_model_visible() {
        return Ok(0);
    }
    m.content.iter().try_fold(0u64, |sum, block| {
        Ok(sum + crate::ir::est_tokens_resolved(block, resolver)?)
    })
}

/// Facts and labels demand just these Write fields; code content contributes its decoded
/// length. Notes, tail turns and sidecars still demand their complete original values.
struct ToolInput<'a> {
    fields: std::borrow::Cow<'a, Value>,
    write_bytes: Option<usize>,
}
impl<'a> ToolInput<'a> {
    fn new(name: &str, input: &'a crate::Json, notes: bool, label: bool) -> anyhow::Result<Self> {
        if name != "Write" || matches!(input, crate::Json::Owned(_)) {
            return Ok(Self {
                fields: input.resolve()?,
                write_bytes: None,
            });
        }
        input.with_view(|view| {
            let mut fields = serde_json::Map::new();
            for key in ["file_path", "path", "notebook_path", "command"] {
                if let Some(value) = view.get(key)? {
                    if value.is_string()? {
                        fields.insert(key.to_owned(), value.resolve()?.into_owned());
                    }
                }
            }
            let note = notes
                && fields
                    .get("file_path")
                    .and_then(Value::as_str)
                    .is_some_and(is_note_path);
            let mut bytes = 0;
            if let Some(content) = view.get("content")? {
                if note && content.is_string()? {
                    let content = content.resolve()?.into_owned();
                    bytes = content.as_str().map_or(0, str::len);
                    fields.insert("content".into(), content);
                } else if label {
                    content.for_each_str_chunk(|chunk| {
                        bytes += chunk.len();
                        Ok(())
                    })?;
                }
            }
            Ok(Self {
                fields: std::borrow::Cow::Owned(Value::Object(fields)),
                write_bytes: Some(bytes),
            })
        })
    }
    fn label(&self, name: &str) -> String {
        match self.write_bytes {
            Some(bytes) => truncate(
                format!(
                    "Write {} ({bytes} bytes)",
                    self.fields.get("file_path").and_then(Value::as_str).unwrap_or("")
                )
                .trim(),
                140,
            ),
            None => tool_label(name, &self.fields),
        }
    }
}

/// A short label for one tool call, for the ledger and the facts lists.
fn tool_label(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let l = match name {
        "Bash" => {
            let d = s("description");
            if d.is_empty() {
                truncate(first_line(s("command")), 120)
            } else {
                truncate(d.trim(), 120)
            }
        }
        "Read" => format!("Read {}", s("file_path")),
        "Write" => format!("Write {} ({} bytes)", s("file_path"), s("content").len()),
        "Edit" | "MultiEdit" | "NotebookEdit" => format!("{name} {}", s("file_path")),
        "Grep" | "Glob" => format!("{name} {:?} {}", s("pattern"), s("path")),
        "SendMessage" => format!("SendMessage → {}", s("to")),
        "Agent" | "Task" => format!("{name}: {}", s("description")),
        _ => format!("{name} {}", truncate(&input.to_string(), 100)),
    };
    truncate(l.trim(), 140)
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
        for c in re(Re::RemoteTarget).captures_iter(cmd) {
            let h = &c[1];
            if !h.contains('/') && !h.starts_with("http") && h.len() > 2 {
                out.push(h.to_string());
            }
        }
    }
    out
}

fn plausible_path(p: &str) -> bool {
    p.len() >= 8
        && !p.starts_with("/dev/")
        && !p.starts_with("/proc/")
        && !p.starts_with("/usr/")
        && !p.starts_with("/bin/")
        && !p.starts_with("/opt/homebrew")
        && !p.starts_with("/etc/")
}

/// Prepare only the pack and identical statistics, without retaining the sidecar roster.
/// `elided` is empty even when `stats.elided_outputs` is nonzero; use [`prepare_distill`]
/// when complete sidecar entries or resumable products are needed.
pub fn distill_pack(session: &Session, opts: &DistillOptions) -> anyhow::Result<Distilled> {
    Ok(analyze(session, opts, Demand::Pack)?.distilled)
}

/// Scoped demanded text. Owned blocks borrow directly; lazy fields resolve at most once while
/// this message is analyzed. Thinking and other unobserved fields never become owned strings.
struct MessageText<'a> {
    message: &'a Message,
    resolver: &'a crate::Resolver,
    resolved: Option<Vec<Option<String>>>,
}
impl<'a> MessageText<'a> {
    fn new(message: &'a Message, resolver: &'a crate::Resolver) -> Self {
        Self {
            message,
            resolver,
            resolved: None,
        }
    }
    fn block(&mut self, index: usize) -> anyhow::Result<&str> {
        let text = match &self.message.content[index] {
            Block::Text { text } | Block::Thinking { text, .. } => text,
            Block::ToolResult { content, .. } => content,
            _ => anyhow::bail!("distill text requested for a non-text block"),
        };
        if let Some(text) = text.inline_str() {
            return Ok(text);
        }
        let resolved = self
            .resolved
            .get_or_insert_with(|| (0..self.message.content.len()).map(|_| None).collect());
        if resolved[index].is_none() {
            // Malformed legacy escapes retain their raw/lossy fallback, but failed source
            // reads must propagate for both Span and checked Source carriers.
            resolved[index] = Some(text.try_resolve(self.resolver)?.into_owned());
        }
        Ok(resolved[index].as_deref().expect("resolved demanded text"))
    }
    fn release(&mut self, index: usize) {
        if let Some(resolved) = self.resolved.as_mut() {
            resolved[index] = None;
        }
    }
    fn text(&mut self) -> anyhow::Result<Option<String>> {
        let mut output = String::new();
        for index in 0..self.message.content.len() {
            if matches!(&self.message.content[index], Block::Text { .. }) {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(self.block(index)?);
            }
        }
        Ok((!output.is_empty()).then_some(output))
    }
}

fn validate_analysis_sources(session: &Session) -> anyhow::Result<()> {
    let resolver = session.resolver();
    for message in &session.messages {
        // Preserve materialize's validation order: text first, then structured arguments.
        for block in &message.content {
            let text = match block {
                Block::Text { text } | Block::Thinking { text, .. } => Some(text),
                Block::ToolResult { content, .. } => Some(content),
                _ => None,
            };
            if let Some(text) = text.filter(|text| text.is_lazy()) {
                text.try_for_each_chunk(&resolver, 64 * 1024, |_| Ok(()))?;
            }
        }
        for block in &message.content {
            if let Block::ToolUse {
                input: input @ crate::Json::Source(_),
                ..
            } = block
            {
                input.with_view(|_| Ok(()))?;
            }
        }
    }
    Ok(())
}

fn analysis_label(session: &Session, resolver: &crate::Resolver) -> anyhow::Result<String> {
    // Every adapter is asked: a bag is keyed by the harness that wrote it, not by `session.harness`.
    let label = match crate::Harness::ALL
        .iter()
        .filter_map(|h| crate::harness::rootless(*h))
        .find_map(|adapter| adapter.agent_description(session))
    {
        Some(description) => description.to_owned(),
        None => crate::ir::label_from(session.title_with(resolver)?.as_deref(), None),
    };
    // A distilled lane is labelled `distilled: <label>`; distilling it again keeps one prefix,
    // not `distilled: distilled: …`.
    let mut rest = label.as_str();
    while let Some(inner) = rest.strip_prefix("distilled: ") {
        rest = inner;
    }
    Ok(rest.to_string())
}

fn analyze(session: &Session, opts: &DistillOptions, demand: Demand) -> anyhow::Result<PreparedDistillation> {
    validate_analysis_sources(session)?;
    let resolver = session.resolver();
    let msgs = &session.messages;
    let retrieve_ref = opts.retrieve_ref.clone().unwrap_or_else(|| session.id.clone());

    let uses = tool_index(msgs);
    let tool_calls = uses.len();

    let tail_start = tail_cut(msgs, opts.keep_last, &uses);

    // Documents named in the brief or in any message to the agent are instructions by reference:
    // when the agent reads one, its latest read is kept (capped) in the timeline.
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in msgs.iter().filter(|_| demand != Demand::Findings) {
        let incoming = (m.role == Role::User && m.kind == MessageKind::Prompt)
            || (m.kind == MessageKind::InjectedContext
                && matches!(crate::harness::attachment_kind(m), Some("queued_command")));
        if !incoming {
            continue;
        }
        let t = MessageText::new(m, &resolver).text()?.unwrap_or_default();
        // A resumed distilled lane's opening prompt is its predecessor's pack: only the original
        // brief inside it names documents as instructions.
        let t = unwrap_pack(&t).map_or(t, |(brief, _)| brief);
        for c in re(Re::Path).captures_iter(&t) {
            referenced.insert(c[1].trim_end_matches(['.', ',', ';', ':', ')', '`', '\'']).to_string());
        }
    }
    // tool_use_id → path, for reads of a referenced document (Read, or a plain `cat`/`sed -n` of it).
    let mut doc_reads: HashMap<&str, String> = HashMap::new();
    for m in msgs[..tail_start].iter().filter(|_| !referenced.is_empty()) {
        for b in &m.content {
            if let Block::ToolUse { id, name, input, .. } = b {
                let input = ToolInput::new(name, input, false, false)?;
                let input = &input.fields;
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

    // Files the lane passed to `git commit -F`: a note it wrote there is a commit message.
    let mut commit_files: HashSet<String> = HashSet::new();
    for m in msgs[..tail_start].iter().filter(|_| demand != Demand::Findings) {
        for b in &m.content {
            if let Block::ToolUse { name, input, .. } = b {
                if name != "Write" {
                    let input = ToolInput::new(name, input, false, false)?;
                    for c in command_of(&input.fields)
                        .into_iter()
                        .flat_map(|c| re(Re::CommitFile).captures_iter(c))
                    {
                        commit_files.insert(basename(&c[1]).to_string());
                    }
                }
            }
        }
    }

    // ── pass: brief, timeline, facts, ledger ──
    let mut brief: Option<(usize, String)> = None;
    // Earlier generations carried by a brief that is itself a distilled pack (see `unwrap_pack`).
    let mut earlier = String::new();
    let mut timeline: Vec<Entry> = Vec::new();
    let mut facts = Facts::default();
    // first idx, last idx, label, outcome, the last call's tool_use id and output bytes
    let mut ledger: Vec<(usize, usize, String, String, String, usize)> = Vec::new();
    let mut ledger_runs: Vec<usize> = Vec::new(); // repeat count per ledger row
    let mut elided: Vec<Elided> = Vec::new();
    let mut elided_outputs = 0usize;
    // The lane's last text block anywhere (tail included): its own hand-off.
    let mut last_words: Option<(usize, String)> = None;

    for (i, m) in msgs.iter().enumerate() {
        let in_tail = i >= tail_start;
        let mut reader = MessageText::new(m, &resolver);
        match (m.role, m.kind) {
            (Role::User, MessageKind::Prompt) if demand != Demand::Findings => {
                let text = reader.text()?.unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                if brief.is_none() {
                    brief = Some(match unwrap_pack(&text) {
                        Some((original, generations)) => {
                            earlier = generations;
                            (i, original)
                        }
                        None => (i, text),
                    });
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
            (_, MessageKind::CompactionSummary) if !in_tail && demand != Demand::Findings => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "compaction summary (the harness's own digest of what came before)".into(),
                    body: reader.text()?.unwrap_or_default(),
                });
            }
            (_, MessageKind::SubagentReturn) if !in_tail && demand != Demand::Findings => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "sub-agent returned".into(),
                    body: head_tail(
                        &reader.text()?.unwrap_or_default(),
                        opts.note_max,
                        "the full return is in the source transcript",
                    ),
                });
            }
            (Role::System, MessageKind::Error) if !in_tail && demand != Demand::Findings => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "harness error".into(),
                    body: truncate(reader.text()?.unwrap_or_default().trim(), 300),
                });
            }
            (_, MessageKind::InjectedContext) if !in_tail && demand != Demand::Findings => {
                // A message delivered mid-run (peer agent, coordinator, queued user input).
                if matches!(crate::harness::attachment_kind(m), Some("queued_command")) {
                    timeline.push(Entry {
                        idx: i,
                        when: when(m),
                        heading: "message delivered while working".into(),
                        body: strip_reminder(&reader.text()?.unwrap_or_default()),
                    });
                }
            }
            (Role::Assistant, _) => {
                for (index, b) in m.content.iter().enumerate() {
                    match b {
                        Block::Text { .. } if !in_tail => {
                            let text = reader.block(index)?;
                            if text.trim().is_empty() {
                                continue;
                            }
                            timeline.push(Entry {
                                idx: i,
                                when: when(m),
                                heading: "agent said".into(),
                                body: text.to_string(),
                            });
                        }
                        Block::ToolUse { name, input, .. } => {
                            let input = ToolInput::new(name, input, !in_tail, false)?;
                            let input = &input.fields;
                            scan_input_facts(name, input, &mut facts);
                            if let Some(c) = command_of(input) {
                                facts.note_pid_files(i, c);
                                if let Some(d) = re(Re::Cd).captures_iter(c).last() {
                                    Facts::note_latest(&mut facts.dirs, i, &d[1]);
                                }
                            }
                            if !in_tail {
                                if let Some(mut e) = own_words_from_tool(i, m, name, input, opts) {
                                    shorten_commit_note(&mut e, input, &commit_files);
                                    timeline.push(e);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            (Role::Tool, _) | (Role::User, MessageKind::ToolResult) => {
                for (index, b) in m.content.iter().enumerate() {
                    let Block::ToolResult {
                        tool_use_id,
                        is_error,
                        tool_name,
                        ..
                    } = b
                    else {
                        continue;
                    };
                    let orphan = crate::Json::from(Value::Null);
                    let (name, original_input) = uses
                        .get(tool_use_id.as_str())
                        .map(|call| (call.name.to_string(), call.input))
                        .unwrap_or_else(|| (tool_name.clone().unwrap_or_default(), &orphan));
                    let summary = ToolInput::new(&name, original_input, false, true)?;
                    let label = summary.label(&name);
                    let input = &summary.fields;
                    let out = reader.block(index)?;
                    let outcome = scan_result_facts(i, &name, input, &label, out, *is_error, &mut facts);
                    if let Some(p) = doc_reads.get(tool_use_id.as_str()) {
                        if !in_tail && demand != Demand::Findings && last_read_of.get(p) == Some(&tool_use_id.as_str())
                        {
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
                    if (name == "Agent" || name == "Task") && !in_tail && demand != Demand::Findings {
                        timeline.push(Entry {
                            idx: i,
                            when: when(m),
                            heading: format!(
                                "sub-agent returned ({})",
                                truncate(
                                    input.get("description").and_then(Value::as_str).unwrap_or("").trim(),
                                    80
                                )
                            ),
                            body: head_tail(out, opts.note_max, &format!("cv cat {retrieve_ref} {tool_use_id}")),
                        });
                    }
                    let keep_whole = in_tail && out.len() <= opts.tail_result_max;
                    if !keep_whole && demand != Demand::Findings {
                        elided_outputs += 1;
                    }
                    if !keep_whole && demand == Demand::Full {
                        #[cfg(test)]
                        note_work(|work| work.elided_copy_bytes += out.len());
                        elided.push(Elided {
                            tool_use_id: tool_use_id.clone(),
                            tool_name: name.clone(),
                            input: original_input.resolve()?.into_owned(),
                            content: out.to_string(),
                        });
                    }
                    if !in_tail && opts.ledger {
                        let use_idx = uses.get(tool_use_id.as_str()).map(|c| c.msg).unwrap_or(i);
                        match ledger.last_mut() {
                            Some(last) if last.2 == label => {
                                last.1 = use_idx;
                                last.3 = outcome;
                                last.4 = tool_use_id.clone();
                                last.5 = out.len();
                                *ledger_runs.last_mut().unwrap() += 1;
                            }
                            _ => {
                                ledger.push((use_idx, use_idx, label, outcome, tool_use_id.clone(), out.len()));
                                ledger_runs.push(1);
                            }
                        }
                    }
                    // Results are consumed once; do not retain other results in this message.
                    reader.release(index);
                }
            }
            _ => {}
        }
        // Paths the agent itself names in prose count toward the path index too.
        if m.role == Role::Assistant {
            for (index, b) in m.content.iter().enumerate() {
                if let Block::Text { .. } = b {
                    let prose = reader.block(index)?;
                    for c in re(Re::Path).captures_iter(prose) {
                        let p = c[1].trim_end_matches(['.', ',', ';', ':', ')', '`']);
                        if plausible_path(p) {
                            bump(&mut facts.paths, p);
                        }
                    }
                    facts.note_pid_files(i, prose);
                    if !prose.trim().is_empty() {
                        last_words = Some((i, prose.trim().to_string()));
                    }
                }
            }
        }
    }

    // ── render the head ──
    let label = analysis_label(session, &resolver)?;
    if demand == Demand::Findings {
        let mut h = String::new();
        let _ = writeln!(h, "## Findings from another lane — {label} (`{}`)\n", session.id);
        // These headings are produced by the extractor. Source Markdown is body content and
        // must never be reparsed as our own section boundaries or used to classify ownership.
        for entry in timeline.iter().filter(|entry| {
            entry.heading.starts_with("agent said")
                || entry.heading.starts_with("agent sent")
                || entry.heading.starts_with("agent wrote")
        }) {
            let _ = writeln!(
                h,
                "### [{}] {} · {}\n\n{}\n",
                entry.idx,
                entry.when,
                entry.heading,
                entry.body.trim()
            );
        }
        let mut rendered = String::new();
        render_facts(&mut rendered, &facts);
        let _ = writeln!(h, "### Facts\n\n{}", rendered.trim());
        return Ok(PreparedDistillation {
            distilled: Distilled {
                head_md: h,
                tail_md: String::new(),
                tail_start,
                elided,
                stats: DistillStats::default(),
                label,
                retrieve_ref,
            },
            tail: Vec::new(),
            keep_last: opts.keep_last,
        });
    }
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
    let elided_hint = (opts.ledger && elided_outputs > 0).then_some((retrieve_ref.as_str(), elided_outputs));
    render_handoff(&mut h, last_words.as_ref(), &facts, msgs, elided_hint);
    render_grounding(&mut h, &ground(msgs, &uses, &resolver)?);
    if let Some((i, text)) = &brief {
        let _ = writeln!(h, "{BRIEF_HEAD}{i}]\n\n{}\n", text.trim());
    }
    if !earlier.is_empty() {
        let _ = writeln!(
            h,
            "{EARLIER_HEAD} — this lane was distilled before. Per generation, oldest first: its \
             brief's first paragraph, its commits and the state it pointed at. Its timeline, ledger \
             and other facts stay in its source (`cv show <source> --range A..B`).\n\n{}\n",
            earlier.trim()
        );
    }
    let _ = writeln!(
        h,
        "{TIMELINE_HEAD}, and everything it said or wrote itself (verbatim)\n"
    );
    if timeline.is_empty() {
        let _ = writeln!(h, "_(nothing before the verbatim tail)_\n");
    }
    for e in &timeline {
        let _ = writeln!(h, "### [{}] {} · {}\n\n{}\n", e.idx, e.when, e.heading, e.body.trim());
    }
    let _ = writeln!(h, "{FACTS_HEAD}\n");
    render_facts(&mut h, &facts);
    if opts.ledger && !ledger.is_empty() {
        let _ = writeln!(
            h,
            "{LEDGER_HEAD}, one line each (`[index] label → outcome · tool_use_id size`; ×N = repeated, \
             the id is the last one's; `cv cat {retrieve_ref} <tool_use_id>` prints an output whole)\n"
        );
        for ((a, b, label, outcome, id, bytes), n) in ledger.iter().zip(&ledger_runs) {
            let idx = if a == b {
                format!("[{a}]")
            } else {
                format!("[{a}–{b}]")
            };
            let rep = if *n > 1 { format!(" ×{n}") } else { String::new() };
            let _ = writeln!(h, "- {idx} {label}{rep} → {outcome} · `{id}` {}", size(*bytes));
        }
        h.push('\n');
    }

    let tail = resumable_tail(msgs, tail_start, opts, &retrieve_ref, &session.resolver())?;
    let tail_md = render_prepared_tail(&tail, tail_start, opts)?;
    let tail_tool_calls = tail
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, Block::ToolUse { .. }))
        .count();

    let source_est_tokens: u64 = msgs.iter().try_fold(0u64, |sum, message| {
        Ok::<_, anyhow::Error>(sum + message_est_resolved(message, &resolver)?)
    })?;
    let pack_est_tokens = est(&h);
    let tail_est_tokens: u64 = tail
        .iter()
        .flat_map(|m| &m.content)
        .try_fold(0u64, |sum, block| Ok::<_, anyhow::Error>(sum + block_est(block)?))?;
    let recorded_context_tokens = msgs
        .iter()
        .rev()
        .find_map(|m| m.usage.as_ref().map(|u| u.prompt_tokens()).filter(|tokens| *tokens > 0));

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
        elided_outputs,
    };
    Ok(PreparedDistillation {
        distilled: Distilled {
            head_md: h,
            tail_md,
            tail_start,
            elided,
            stats,
            label,
            retrieve_ref,
        },
        tail,
        keep_last: opts.keep_last,
    })
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

// The pack's section headings. The renderer writes them numbered; `unwrap_pack` finds them by
// title, so packs from every numbering (0.18.0's `## 1. Brief`, this one's) read back.
const PREAMBLE_HEAD: &str = "[cv distill] You are resuming";
const HANDOFF_HEAD: &str = "## 0. Hand-off — ";
const GROUNDING_HEAD: &str = "## 1. Grounding — ";
const BRIEF_HEAD: &str = "## 2. Brief (verbatim) [";
const EARLIER_HEAD: &str = "## 2b. Earlier generations";
const TIMELINE_HEAD: &str = "## 3. Timeline — everything said to this agent";
const FACTS_HEAD: &str = "## 4. Facts (mined from tool inputs and outputs)";
const LEDGER_HEAD: &str = "## 5. Ledger — every tool call before the tail";

/// The title of a `## N. <title>` heading constant: what `heading` searches for.
fn title(head: &str) -> &str {
    head.split_once(". ").map_or(head, |(_, title)| title)
}

/// The start of the line holding the `## N. <title>` (or `## Nb. <title>`) heading at or after
/// `from`: the first such line, or with `last` the last one.
fn heading(t: &str, head: &str, from: usize, last: bool) -> Option<usize> {
    let title = title(head);
    let mut found = None;
    let mut pos = from;
    while let Some(k) = t[pos..].find(title) {
        let at = pos + k;
        let line = t[..at].rfind('\n').map_or(0, |n| n + 1);
        let prefix = &t[line..at];
        let numbered = prefix.len() <= 8 && prefix.starts_with("## ") && prefix.ends_with(". ");
        if numbered && line >= from {
            if !last {
                return Some(line);
            }
            found = Some(line);
        }
        pos = at + title.len();
    }
    found
}

/// The text up to its first blank line, capped: a brief's first paragraph.
fn first_paragraph(s: &str) -> String {
    let s = s.trim();
    let end = s.find("\n\n").unwrap_or(s.len());
    truncate(s[..end].trim(), 1200)
}

/// The body of the first entry in a rendered timeline that was said *to* the lane.
fn first_incoming(timeline: &str) -> Option<&str> {
    timeline.split("\n### [").skip(1).find_map(|entry| {
        let (head, body) = entry.split_once('\n')?;
        let incoming = head.contains("· message delivered while working")
            || head.contains("· message to this agent")
            || head.ends_with("· user");
        incoming.then(|| body.trim())
    })
}

/// A brief that is a resumed distilled lane's opening prompt (the preamble, then the
/// predecessor's pack), split into the original brief and the earlier generations to carry:
/// `(brief, generations)`. Re-wrapping the whole pack as "the brief, verbatim" nested every
/// generation inside the next: on session 698fc87d the fourth infra pack was 85% nested
/// predecessors (442 of 521 KB) and compressed 1.6×.
///
/// An old generation is owed little (ember's ruling, 2026-10-27): its entry is its brief's first
/// paragraph (for a resumed generation, the first message sent to it), its commits, and the state
/// it pointed at: branch tips, repository heads, branches, PID files and state-file paths. Its
/// timeline, ledger and other facts stay in its source, which the entry names. Packs written
/// before the `Earlier generations` section existed nest in their briefs and unwrap recursively.
fn unwrap_pack(text: &str) -> Option<(String, String)> {
    let t = text.trim_start();
    if !t.starts_with(PREAMBLE_HEAD) {
        return None;
    }
    let b = heading(t, BRIEF_HEAD, 0, false)?;
    let source = t[..b]
        .find("Source: `")
        .and_then(|k| t[k + 9..b].split('`').next())
        .unwrap_or("an earlier distill");
    let body = b + t[b..].find('\n')?;
    let tl = heading(t, TIMELINE_HEAD, body, true)?;
    let facts = heading(t, FACTS_HEAD, tl + 1, true)?;
    let end = heading(t, LEDGER_HEAD, facts + 1, true).unwrap_or(t.len());
    let (mut brief, mut earlier) = match heading(&t[..tl], EARLIER_HEAD, body, false) {
        Some(k) => {
            let carried = t[k..tl].split_once('\n').map_or("", |(_, rest)| rest);
            (t[body..k].trim().to_string(), carried.trim().to_string())
        }
        None => (t[body..tl].trim().to_string(), String::new()),
    };
    let mut resumed = !earlier.is_empty();
    if let Some((inner, generations)) = unwrap_pack(&brief) {
        brief = inner;
        earlier = format!("{generations}\n\n{earlier}").trim().to_string();
        resumed = true;
    }
    let _ = write!(earlier, "\n\n### ═══ generation distilled from `{source}` ═══\n\n");
    if !resumed {
        let _ = writeln!(earlier, "**Its brief**: the brief above.\n");
    } else if let Some(own) = first_incoming(&t[tl..facts]).map(first_paragraph) {
        let _ = writeln!(
            earlier,
            "**Its brief** (first paragraph of the first message to it): {own}\n"
        );
    }
    // The commit list and the repository-head list (each a header line and its indented items),
    // the tip and branch lines, and the state-file paths among the referenced paths.
    let mut in_list = false;
    for line in t[facts..end].lines() {
        let list_head = line.starts_with("- **Commits made**") || line.starts_with("- **Repository heads last seen**");
        if list_head || line.starts_with("- **Branch tips last seen**") || line.starts_with("- **Branches**") {
            in_list = list_head;
            let _ = writeln!(earlier, "{line}");
        } else if in_list && line.starts_with("  - ") {
            let _ = writeln!(earlier, "{line}");
        } else {
            in_list = false;
            if let Some(paths) = line.strip_prefix("- **Paths most referenced**: ") {
                let state: Vec<&str> = paths
                    .split(" · ")
                    .filter(|p| p.to_ascii_lowercase().contains("state"))
                    .collect();
                if !state.is_empty() {
                    let _ = writeln!(earlier, "- **State files named**: {}", state.join(" · "));
                }
            }
        }
    }
    if let Some(grounding) = heading(&t[..b], GROUNDING_HEAD, 0, false) {
        for line in t[grounding..b].lines().filter(|l| l.starts_with("**State file** `")) {
            let path = line.split('`').nth(1).unwrap_or("");
            let _ = writeln!(earlier, "- **State file it last wrote**: `{path}`");
        }
    }
    if let Some(handoff) = heading(&t[..b], HANDOFF_HEAD, 0, false) {
        for line in t[handoff..b]
            .lines()
            .filter(|l| l.starts_with("- **PID files it named**"))
        {
            let _ = writeln!(earlier, "{line}");
        }
    }
    Some((brief, earlier.trim().to_string()))
}

/// A commit message is kept to this many bytes in the timeline: the commit holds the whole of it.
const COMMIT_NOTE_MAX: usize = 700;
const COMMIT_NOTE_POINTER: &str = "a commit message: the commit holds all of it (`git log`)";

fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

/// A note that is a commit message: written to a file the lane passed to `git commit -F`, or
/// carrying a commit trailer. On session 698fc87d these were 101 KB of the first infra pack's
/// 227 KB timeline (36 messages) and 365 KB across its 14 packs; each restates a commit the facts
/// list already names, and the repository keeps it whole.
fn is_commit_note(path: &str, content: &str, commit_files: &HashSet<String>) -> bool {
    commit_files.contains(basename(path)) || content.contains("Co-Authored-By:") || content.contains("Signed-off-by:")
}

fn shorten_commit_note(e: &mut Entry, input: &Value, commit_files: &HashSet<String>) {
    let Some(rest) = e.heading.strip_prefix("agent wrote ") else {
        return;
    };
    let path = rest.trim_end_matches(" (shell)").to_string();
    match (input.get("content").and_then(Value::as_str), command_of(input)) {
        (Some(content), _) if is_commit_note(&path, content, commit_files) => {
            e.heading = format!("agent wrote {path} (commit message)");
            e.body = head_tail(content, COMMIT_NOTE_MAX, COMMIT_NOTE_POINTER);
        }
        (None, Some(cmd)) if is_commit_note(&path, cmd, commit_files) => {
            e.heading = format!("agent wrote {path} (commit message, shell)");
            e.body = format!("```\n{}\n```", head_tail(cmd, COMMIT_NOTE_MAX, COMMIT_NOTE_POINTER));
        }
        _ => {}
    }
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
            let target = re(Re::HeredocTarget).captures(cmd)?.get(1)?.as_str().to_string();
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
        for c2 in re(Re::NewBranch).captures_iter(c) {
            bump(&mut f.branches, &c2[1]);
        }
        for c2 in re(Re::Push).captures_iter(c) {
            let r = &c2[1];
            let b = r.rsplit(':').next().unwrap_or(r);
            bump(&mut f.branches, b);
        }
    }
    for k in ["file_path", "path", "notebook_path"] {
        if let Some(p) = input.get(k).and_then(Value::as_str) {
            if plausible_path(p) {
                bump(&mut f.paths, p);
            }
        }
    }
    for t in texts {
        for c in re(Re::Path).captures_iter(t) {
            let p = c[1].trim_end_matches(['.', ',', ';', ':', ')', '`', '\'']);
            if plausible_path(p) {
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
    for c in re(Re::Commit).captures_iter(out) {
        let (b, sha, subj) = (c[1].to_string(), c[2].to_string(), c[3].trim().to_string());
        // `git log --oneline`-style listings never use the bracketed form; a bracketed line is a
        // commit (or cherry-pick/amend) the agent made.
        if f.commit_shas.insert(sha.clone()) {
            bump(&mut f.branches, &b);
            commit_outcome.get_or_insert_with(|| format!("⊕ [{b} {sha}] {}", truncate(subj.trim(), 90)));
            f.tips.push((i, b.clone(), sha.clone()));
            f.commits.push((i, b, sha, subj));
        }
    }
    if commit_outcome.is_none() && !is_error {
        for (repo, sha, subject) in command_of(input).map(|c| quiet_commits(c, out)).unwrap_or_default() {
            if f.commit_shas.insert(sha.to_string()) {
                commit_outcome.get_or_insert_with(|| format!("⊕ [{repo} {sha}] {}", truncate(subject, 90)));
                f.commits.push((i, repo, sha.to_string(), subject.to_string()));
            }
        }
    }
    for c in re(Re::OnBranch).captures_iter(out) {
        bump(&mut f.branches, &c[1]);
    }
    if !is_error {
        if let Some((key, line)) = command_of(input).and_then(|c| git_log_head(c, out)) {
            f.heads.push((i, key, line));
        }
    }
    for c in re(Re::Tip).captures_iter(out) {
        bump(&mut f.branches, &c[2]);
        f.tips.push((i, c[2].to_string(), c[1][..12].to_string()));
    }
    if is_error {
        // A failed command's output often starts with whatever it printed before failing; the
        // diagnostic is the first error-shaped line, else the last line.
        let meaningful = |line: &&str| !line.is_empty() && !line.starts_with("Exit code");
        let line = out
            .lines()
            .map(str::trim)
            .filter(meaningful)
            .find(|line| re(Re::ErrorLine).is_match(line))
            .or_else(|| out.lines().rev().map(str::trim).find(meaningful))
            .unwrap_or("");
        let line = truncate(line.trim(), 200);
        f.errors.push((i, label.to_string(), line.clone()));
        return format!("✗ {line}");
    }
    if let Some(c) = commit_outcome {
        return c;
    }
    let is_gate = name == "Bash" && command_of(input).is_some_and(|c| re(Re::GateCmd).is_match(c));
    if is_gate {
        let verdict = out
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| re(Re::Verdict).is_match(l))
            .or_else(|| out.lines().rev().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("(no output)");
        let verdict = truncate(verdict.trim(), 160);
        f.gates.push((i, label.to_string(), verdict.clone()));
        return format!("⚑ {verdict}");
    }
    let n = out.lines().filter(|l| !l.trim().is_empty()).count();
    let fl = truncate(first_line(out), 100);
    if n > 1 {
        format!("{fl} (+{} lines)", n - 1)
    } else if fl.is_empty() {
        "(no output)".into()
    } else {
        fl
    }
}

/// `## 0. Hand-off`: the lane's last message and the live state it left, first, where a resumed
/// lane and its coordinator look. On session 698fc87d both sat at the end of a 100–500 KB pack:
/// the root re-typed tips, paths and next steps into every resume message (~2 KB each, all of
/// them already in the pack), and successors spent 180 of their first 40-call windows (48k tokens)
/// re-probing git, PIDs and job logs. The newest heads, tips, PID files and directories are the
/// places to verify; they are what the lane last saw, not what is true now.
/// A byte count for the ledger: `812 B`, `14.2 KB`, `3.1 MB`.
fn size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn render_handoff(
    h: &mut String,
    last_words: Option<&(usize, String)>,
    f: &Facts,
    msgs: &[Message],
    elided: Option<(&str, usize)>,
) {
    let newest = |list: &[(usize, String, String)], n: usize| -> Vec<String> {
        let mut seen = HashSet::new();
        let mut rows: Vec<String> = Vec::new();
        for (i, key, value) in list.iter().rev() {
            if seen.insert(key) && rows.len() < n {
                rows.push(format!("`{key}` → `{value}` [{i}]"));
            }
        }
        rows
    };
    let heads = newest(&f.heads, 5);
    let tips = newest(&f.tips, 5);
    let recent = |list: &[(usize, String)], n: usize| -> String {
        let skip = list.len().saturating_sub(n);
        list.iter()
            .skip(skip)
            .map(|(i, v)| format!("`{v}` [{i}]"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    if last_words.is_none() && heads.is_empty() && tips.is_empty() && f.pid_files.is_empty() && elided.is_none() {
        return;
    }
    let _ = writeln!(
        h,
        "{HANDOFF_HEAD}the lane's last message and the state it last saw (verify before acting)\n"
    );
    if let Some((i, words)) = last_words {
        let at = msgs.get(*i).map(when).unwrap_or_default();
        let _ = writeln!(h, "**Its last message** [{i}] {at}:\n\n{words}\n");
    }
    if !heads.is_empty() {
        let _ = writeln!(h, "- **Newest repository heads**: {}", heads.join(" · "));
    }
    if !tips.is_empty() {
        let _ = writeln!(h, "- **Newest branch tips**: {}", tips.join(" · "));
    }
    if !f.pid_files.is_empty() {
        let _ = writeln!(
            h,
            "- **PID files it named** (newest last): {}",
            recent(&f.pid_files, 12)
        );
    }
    if !f.dirs.is_empty() {
        let _ = writeln!(
            h,
            "- **Directories it worked in** (newest last): {}",
            recent(&f.dirs, 4)
        );
    }
    // On session 698fc87d no successor ran `cv cat` once: nothing in the pack named an id.
    if let Some((reference, n)) = elided {
        let _ = writeln!(
            h,
            "- **Elided outputs** ({n}): `cv cat {reference} <tool_use_id>`; the ledger names them, with \
             sizes. Fetch a build log or a row table there instead of re-running the command."
        );
    }
    h.push('\n');
}

/// `## 1. Grounding`: what the lane itself wrote down about where it stands, so a successor grounds
/// on its own record instead of re-deriving it (ember's ruling, 2026-10-27). Deterministic, from the
/// lane's own words (its text and the messages it sent), each line verbatim with its message
/// index: the last state file it wrote, the open items it listed in its last
/// [`GROUNDING_WINDOW`] calls, and the decisions it recorded anywhere.
#[derive(Default)]
struct Grounding {
    /// idx, path, how it was written, its content (`None` for an in-place edit).
    state: Option<(usize, String, &'static str, Option<String>)>,
    open: Vec<(usize, String)>,
    decisions: Vec<(usize, String)>,
}

const GROUNDING_WINDOW: usize = 60;
const GROUNDING_LINES: usize = 25;
const GROUNDING_LINE_MAX: usize = 600;
/// A state file is carried whole up to this many bytes (head and tail beyond, the rest on disk).
const STATE_FILE_MAX: usize = 24 * 1024;

/// A file under `docs/` or a scratch directory whose name says it is a state file.
fn is_state_path(p: &str) -> bool {
    let name = basename(p);
    let placed = ["docs/", "scratch"]
        .iter()
        .any(|d| p.starts_with(d) || p.contains(&format!("/{d}")));
    placed && (name.contains("STATE") || name.contains("state"))
}

/// The body of the heredoc that writes `target` in `cmd`, up to its delimiter line.
fn heredoc_body(cmd: &str, target: &str) -> Option<String> {
    let rest = &cmd[cmd.find(target)?..];
    let after = rest[rest.find("<<")? + 2..].trim_start_matches('-').trim_start();
    let delim = after.split_whitespace().next()?.trim_matches(['\'', '"']);
    let body = &after[after.find('\n')? + 1..];
    let lines: Vec<&str> = body.lines().take_while(|l| l.trim() != delim).collect();
    Some(lines.join("\n"))
}

fn ground(
    msgs: &[Message],
    uses: &HashMap<&str, crate::ir::ToolCall<'_>>,
    resolver: &crate::Resolver,
) -> anyhow::Result<Grounding> {
    let open_re = re(Re::OpenItem);
    let decided_re = re(Re::Decision);
    let window = tail_cut(msgs, GROUNDING_WINDOW, uses);
    let mut g = Grounding::default();
    let mut open: Vec<(usize, String)> = Vec::new();
    let mut decisions: Vec<(usize, String)> = Vec::new();
    let note = |list: &mut Vec<(usize, String)>, i: usize, line: &str| {
        let line = truncate(line, GROUNDING_LINE_MAX);
        list.retain(|(_, l)| *l != line);
        list.push((i, line));
    };
    for (i, m) in msgs.iter().enumerate().filter(|(_, m)| m.role == Role::Assistant) {
        let mut reader = MessageText::new(m, resolver);
        for (index, block) in m.content.iter().enumerate() {
            let words: String = match block {
                Block::Text { .. } => reader.block(index)?.to_string(),
                Block::ToolUse { name, input, .. } => {
                    let input = input.resolve()?;
                    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
                    if i >= window {
                        let path = s("file_path");
                        match name.as_str() {
                            "Write" if is_state_path(path) => {
                                g.state = Some((i, path.to_string(), "Write", Some(s("content").to_string())))
                            }
                            "Edit" | "MultiEdit" if is_state_path(path) => {
                                g.state = Some((i, path.to_string(), "Edit", None))
                            }
                            _ => {
                                if let Some(cmd) = command_of(&input) {
                                    for c in re(Re::HeredocTarget).captures_iter(cmd) {
                                        if is_state_path(&c[1]) {
                                            let body = heredoc_body(cmd, &c[1]);
                                            g.state = Some((i, c[1].to_string(), "shell", body));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if name != "SendMessage" {
                        continue;
                    }
                    s("message").to_string()
                }
                _ => continue,
            };
            for line in words.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if i >= window && open_re.is_match(line) {
                    note(&mut open, i, line);
                }
                if decided_re.is_match(line) {
                    note(&mut decisions, i, line);
                }
            }
        }
    }
    for list in [&mut open, &mut decisions] {
        let skip = list.len().saturating_sub(GROUNDING_LINES);
        list.drain(..skip);
    }
    g.open = open;
    g.decisions = decisions;
    Ok(g)
}

fn render_grounding(h: &mut String, g: &Grounding) {
    if g.state.is_none() && g.open.is_empty() && g.decisions.is_empty() {
        return;
    }
    let _ = writeln!(
        h,
        "{GROUNDING_HEAD}the lane's own record of where it stands (its verbatim lines, [message index])\n"
    );
    if let Some((i, path, how, content)) = &g.state {
        match content {
            Some(content) => {
                let content = head_tail(content, STATE_FILE_MAX, &format!("the whole file is {path}"));
                let _ = writeln!(
                    h,
                    "**State file** `{path}` [{i}] (written by {how}):\n\n~~~~\n{}\n~~~~\n",
                    content.trim_end()
                );
            }
            None => {
                let _ = writeln!(h, "**State file** `{path}` [{i}] (edited in place; read it on disk)\n");
            }
        }
    }
    let list = |h: &mut String, title: &str, rows: &[(usize, String)]| {
        if !rows.is_empty() {
            let _ = writeln!(h, "**{title}**:\n");
            for (i, line) in rows {
                let _ = writeln!(h, "- [{i}] {line}");
            }
            h.push('\n');
        }
    };
    list(
        h,
        &format!("Open items it listed (its last {GROUNDING_WINDOW} calls, newest {GROUNDING_LINES})"),
        &g.open,
    );
    list(
        h,
        &format!("Decisions it recorded (newest {GROUNDING_LINES})"),
        &g.decisions,
    );
}

fn render_facts(h: &mut String, f: &Facts) {
    let list = |counts: &Counts, max: usize| -> String {
        let mut values: Vec<_> = counts.iter().collect();
        values.sort_by_key(|(_, mentions)| (std::cmp::Reverse(mentions.count), mentions.first));
        values
            .iter()
            .take(max)
            .map(|(key, mentions)| format!("`{key}` ×{}", mentions.count))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    if f.commits.is_empty() {
        let _ = writeln!(h, "- **Commits made**: none seen");
    } else {
        let _ = writeln!(h, "- **Commits made** (in order; the last is the newest):");
        for (i, b, sha, subj) in &f.commits {
            let _ = writeln!(h, "  - [{i}] `{b}` `{sha}` {}", truncate(subj.trim(), 160));
        }
    }
    if !f.tips.is_empty() {
        // The newest observation per branch (a commit it made, or a `refs/heads` listing).
        let mut latest: Vec<&(usize, String, String)> = Vec::new();
        let mut seen = HashSet::new();
        for t in f.tips.iter().rev() {
            if seen.insert(&t.1) {
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
        let mut seen = HashSet::new();
        for t in f.heads.iter().rev() {
            if seen.insert(&t.1) {
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
/// opens on a whole reply. Delayed results pull in earlier matching calls transitively; source
/// orphan results and calls before the brief remain outside this repair's contract.
fn tail_cut(msgs: &[Message], keep_last: usize, uses: &HashMap<&str, crate::ir::ToolCall<'_>>) -> usize {
    if keep_last == 0 {
        return msgs.len();
    }
    let mut seen = 0usize;
    let mut start = msgs.len();
    for (i, m) in msgs.iter().enumerate().rev() {
        if m.role != Role::Assistant || !m.is_model_visible() {
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
    let minimum = first_prompt.map_or(0, |index| index + 1);
    let mut cursor = msgs.len();
    while cursor > start {
        cursor -= 1;
        if !msgs[cursor].is_model_visible() {
            continue;
        }
        for block in &msgs[cursor].content {
            if let Block::ToolResult { tool_use_id, .. } = block {
                if let Some(call) = uses.get(tool_use_id.as_str()) {
                    if call.msg >= minimum && call.msg < start && msgs[call.msg].is_model_visible() {
                        start = call.msg;
                        while start > minimum && msgs[start - 1].role == Role::Assistant {
                            start -= 1;
                        }
                    }
                }
            }
        }
    }
    start
}

fn clear_resume_usage(message: &mut Message) {
    message.usage = None;
    // The old complete native record would reintroduce stale usage or unclipped payloads
    // under complete emission. Distillation deliberately replaces that representation.
    message.complete_record = None;
    message.extra.remove(crate::ir::CARRIER_KEY);
}

/// The tail as real IR turns for a resumable session: thinking dropped (unless asked), recorded
/// usage stripped (so a resume gate never reads the source's stale near-limit size), oversized
/// results clipped with a `cv cat` pointer, queued messages turned into user turns, harness-only
/// records dropped. A trailing call with no result stays here (the pack shows the lane stopped
/// mid-call); the Claude emitter drops it from a resumable session (`harness::claude::emit::emit`).
fn resumable_tail(
    msgs: &[Message],
    tail_start: usize,
    opts: &DistillOptions,
    retrieve_ref: &str,
    resolver: &crate::Resolver,
) -> anyhow::Result<Vec<Message>> {
    #[cfg(test)]
    note_work(|work| work.tail_preparations += 1);
    let mut out: Vec<Message> = Vec::new();
    for m in &msgs[tail_start..] {
        if !m.is_model_visible() {
            continue;
        }
        match (m.role, m.kind) {
            (Role::Assistant, _) => {
                let mut m2 = m.clone();
                clear_resume_usage(&mut m2);
                if !opts.keep_tail_thinking {
                    m2.content.retain(|b| !matches!(b, Block::Thinking { .. }));
                }
                m2.materialize(resolver)?;
                if !m2.content.is_empty() {
                    out.push(m2);
                }
            }
            (Role::Tool, _) | (Role::User, MessageKind::ToolResult) => {
                let mut m2 = m.clone();
                clear_resume_usage(&mut m2);
                m2.materialize(resolver)?;
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
            (Role::User, MessageKind::Prompt) => {
                let mut owned = m.clone();
                clear_resume_usage(&mut owned);
                owned.materialize(resolver)?;
                out.push(owned);
            }
            (_, MessageKind::InjectedContext)
                if matches!(crate::harness::attachment_kind(m), Some("queued_command")) =>
            {
                let mut u = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Subagent);
                u.id = m.id.clone();
                u.parent_id = m.parent_id.clone();
                u.timestamp = m.timestamp;
                u.content.push(Block::Text {
                    text: {
                        let mut owned = m.clone();
                        owned.materialize(resolver)?;
                        strip_reminder(&owned.text().unwrap_or_default()).into()
                    },
                });
                out.push(u);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn render_prepared_tail(tail: &[Message], tail_start: usize, opts: &DistillOptions) -> anyhow::Result<String> {
    if tail.is_empty() {
        return Ok(String::new());
    }
    let mut s = String::new();
    let calls = tail
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, Block::ToolUse { .. }))
        .count();
    if calls > opts.keep_last {
        let _ = writeln!(
            s,
            "_Requested the last {} calls; retained {calls} to keep whole replies and matching call context._\n",
            opts.keep_last
        );
    }
    let _ = writeln!(
        s,
        "## 6. The last {} tool calls, verbatim (from message [{tail_start}] on)\n",
        opts.keep_last
    );
    for m in tail {
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
                    let input = input.resolve()?;
                    let body = match command_of(&input) {
                        Some(c) => c.to_string(),
                        None => serde_json::to_string_pretty(&*input)?,
                    };
                    let _ = writeln!(
                        s,
                        "▶ **{name}** `{id}` — {}\n```\n{}\n```\n",
                        tool_label(name, &input),
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
    Ok(s)
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
/// The caller emits it (as a session, or as a sub-agent of a root) with the Claude emitter, which
/// owns the resumability rules: the first tail turn's parent names a record the tail does not
/// carry and is re-linked to the opening prompt, a hook attachment's child is re-linked over the
/// gap, an unanswered trailing call is dropped (`harness::claude::emit::emit`). This is a selector.
pub fn resumable_session(session: &Session, d: &Distilled, opts: &DistillOptions) -> anyhow::Result<Session> {
    let tail = resumable_tail(
        &session.messages,
        d.tail_start,
        opts,
        &d.retrieve_ref,
        &session.resolver(),
    )?;
    Ok(session_with_tail(session, d, opts.keep_last, tail))
}

fn session_with_tail(session: &Session, d: &Distilled, keep_last: usize, mut tail: Vec<Message>) -> Session {
    let mut first = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
    first.timestamp = session
        .messages
        .get(d.tail_start)
        .and_then(|m| m.timestamp)
        .or(session.updated_at);
    first.content.push(Block::Text {
        text: format!("{}{}", resume_preamble(d, keep_last), d.head_md).into(),
    });
    let mut messages = vec![first];
    messages.append(&mut tail);
    let output = Session {
        cwd: session.cwd.clone(),
        title: Some(format!("distilled: {}", d.label)),
        created_at: session.created_at,
        updated_at: session.updated_at,
        model: session.model.clone(),
        git: session.git.clone(),
        principal: session.principal.clone(),
        lineage: crate::ir::Lineage {
            forked_from: Some(session.id.clone()),
            ..Default::default()
        },
        messages,
        ..Session::new(session.id.clone(), session.harness)
    };
    // The selected tail was already materialized; earlier inputs were never demanded.
    output
}

/// Another lane's findings, for injection into this one's pack (`--with`): its own words (text,
/// messages sent, notes) and its facts — not its brief, ledger or tail.
pub fn findings(session: &Session, opts: &DistillOptions) -> anyhow::Result<String> {
    let o = DistillOptions {
        keep_last: 0,
        ledger: false,
        ..opts.clone()
    };
    Ok(analyze(session, &o, Demand::Findings)?.distilled.head_md)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::emit::frozen::Frozen;
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
            input: (json!({"command": cmd, "description": desc})).into(),
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

    fn digest(bytes: impl AsRef<[u8]>) -> Value {
        let bytes = bytes.as_ref();
        json!([bytes.len(), crate::digest::sha256_hex(bytes)])
    }
    fn json_digest(value: &impl serde::Serialize) -> Value {
        digest(serde_json::to_vec(value).unwrap())
    }
    /// One distillation, frozen: each product's length and SHA-256.
    fn products(d: &Distilled) -> Value {
        let elided: Vec<_> = (d.elided.iter())
            .map(|e| json!({"id":e.tool_use_id,"name":e.tool_name,"input":e.input,"content":e.content}))
            .collect();
        json!({
            "head": digest(&d.head_md),
            "tail": digest(&d.tail_md),
            "tail_start": d.tail_start,
            "pack": digest(d.pack()),
            "stats": json_digest(&d.stats),
            "elided": json_digest(&elided),
        })
    }
    fn same_products(actual: &Distilled, expected: &Distilled) {
        assert_eq!(actual.head_md, expected.head_md);
        assert_eq!(actual.tail_md, expected.tail_md);
        assert_eq!(actual.tail_start, expected.tail_start);
        assert_eq!(actual.pack(), expected.pack());
        assert_eq!(
            serde_json::to_value(&actual.stats).unwrap(),
            serde_json::to_value(&expected.stats).unwrap()
        );
        let elided = |d: &Distilled| {
            d.elided
                .iter()
                .map(|e| serde_json::json!({"id":e.tool_use_id,"name":e.tool_name,"input":e.input,"content":e.content}))
                .collect::<Vec<_>>()
        };
        assert_eq!(elided(actual), elided(expected));
    }
    #[test]
    fn shared_products_match_frozen_oracle_and_move_one_prepared_tail() -> anyhow::Result<()> {
        let mut source = lane();
        // Embedded markdown exercises findings' existing section-extraction edge behavior.
        source.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![text(
                "### agent said nested
kept?
## 3. Facts
body λ",
            )],
        ));
        source.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![Block::Thinking {
                text: "thought λ".into(),
                signature: Some("signature".into()),
                encrypted: None,
                redacted: false,
            }],
        ));
        let fixed_time = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")?.with_timezone(&chrono::Utc);
        source.created_at = Some(fixed_time);
        source.updated_at = Some(fixed_time);
        let mut parent = None;
        for (index, message) in source.messages.iter_mut().enumerate() {
            let id = format!("00000000-0000-4000-8000-{:012}", index + 100);
            message.id = Some(id.clone());
            message.parent_id = parent.replace(id);
            message.timestamp = Some(fixed_time);
        }
        let world = cv_testkit::World::new("distill-products-native");
        let mut frozen = Frozen::new("distill-shared-products");
        for keep_last in [0, 2, 12, 999] {
            for tail_result_max in [0, 17, 6000] {
                let opts = DistillOptions {
                    keep_last,
                    tail_result_max,
                    keep_tail_thinking: keep_last % 2 == 0,
                    retrieve_ref: Some("retrieval".into()),
                    ..Default::default()
                };
                WORK.with(|cell| cell.set(Work::default()));
                let prepared = prepare_distill(&source, &opts)?;
                let expected = prepared.distilled.clone();
                let pointers: Vec<_> = prepared
                    .tail
                    .iter()
                    .flat_map(|m| &m.content)
                    .filter_map(|b| match b {
                        Block::Text {
                            text: crate::Text::Inline(text),
                        } => Some(text.as_ptr()),
                        _ => None,
                    })
                    .collect();
                let (actual, actual_session) = prepared.into_resumable_session(&source);
                let new_work = WORK.with(|cell| cell.get());
                same_products(&actual, &expected);
                let moved: Vec<_> = actual_session
                    .messages
                    .iter()
                    .skip(1)
                    .flat_map(|m| &m.content)
                    .filter_map(|b| match b {
                        Block::Text {
                            text: crate::Text::Inline(text),
                        } => Some(text.as_ptr()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(pointers, moved, "prepared tail bodies must move");
                assert_eq!(new_work.tail_preparations, 1);
                let mut entry = json!({
                    "products": products(&actual),
                    "session": json_digest(&actual_session),
                    "work": [new_work.elided_copy_bytes, new_work.source_estimates],
                });
                if keep_last == 2 && tail_result_max == 17 {
                    let mut actual_native = actual_session.clone();
                    actual_native.messages[0].id = Some("00000000-0000-4000-8000-000000000001".into());
                    for target in [Harness::Claude, Harness::Codex] {
                        let emit_opts = crate::emit::EmitOptions {
                            new_id: Some("00000000-0000-4000-8000-000000000002".into()),
                            // Without a cwd the Codex writer falls back to $HOME: pin it.
                            new_cwd: Some("/synthetic/distill".into()),
                            ..Default::default()
                        };
                        let new = crate::emit::emit(
                            &actual_native,
                            target,
                            &world.base.join(format!("new-{target}")),
                            &emit_opts,
                        )?;
                        entry[target.as_str()] = digest(std::fs::read(&new.path)?);
                    }
                }
                frozen.insert(format!("keep{keep_last}-result{tail_result_max}"), entry);
                same_products(&prepare_distill(&source, &opts)?.distilled, &actual);
                assert_eq!(
                    serde_json::to_value(resumable_session(&source, &actual, &opts)?)?,
                    serde_json::to_value(&actual_session)?
                );
            }
        }
        frozen.check()
    }
    #[test]
    fn prepared_products_allow_findings_append_and_preserve_source_errors() -> anyhow::Result<()> {
        let source = lane();
        let opts = DistillOptions {
            keep_last: 2,
            ..Default::default()
        };
        let mut actual = prepare_distill(&source, &opts)?;
        actual.distilled.append_findings(&findings(&source, &opts)?);
        let (actual, actual_session) = actual.into_resumable_session(&source);
        let changed_opts = DistillOptions {
            tail_result_max: 17,
            keep_tail_thinking: true,
            ..opts.clone()
        };
        // The compatibility wrapper must honor its caller's options.
        assert_ne!(
            serde_json::to_value(resumable_session(&source, &actual, &changed_opts)?)?,
            serde_json::to_value(&actual_session)?,
        );
        let mut frozen = Frozen::new("distill-findings-append");
        frozen.insert(
            "keep2",
            json!({
                "products": products(&actual),
                "session": json_digest(&actual_session),
                "changed_options_session": json_digest(&resumable_session(&source, &actual, &changed_opts)?),
            }),
        );
        frozen.check()?;
        let world = cv_testkit::World::new("distill-products-source-error");
        let path = world.base.join("body.json");
        let bytes = serde_json::to_vec(&"guarded body".repeat(1024))?;
        std::fs::write(&path, &bytes)?;
        let guarded = crate::Json::source(&path, 0, bytes.len() as u64)?
            .with_view(|view| view.source_text())?
            .unwrap();
        let mut source = Session::new("guarded", Harness::Claude);
        source.messages.push(msg(
            Role::User,
            MessageKind::Prompt,
            vec![Block::Text { text: guarded }],
        ));
        std::fs::write(&path, b"changed")?;
        assert!(prepare_distill(&source, &opts).is_err());
        assert!(prepare_distill(&source, &opts).is_err());
        assert!(findings(&source, &opts).is_err());
        Ok(())
    }

    #[test]
    fn findings_only_preserves_source_markdown_without_unused_products() -> anyhow::Result<()> {
        let mut source = lane();
        source.messages.push(msg(
            Role::Tool,
            MessageKind::ToolResult,
            vec![result("e1", &"irrelevant tool output λ\n".repeat(65536), false)],
        ));
        source.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![text(
                "### agent said nested
body
## 3. Facts
facts text",
            )],
        ));
        let opts = DistillOptions::default();
        WORK.with(|cell| cell.set(Work::default()));
        let actual = findings(&source, &opts)?;
        let new_work = WORK.with(|cell| cell.get());
        // Embedded source Markdown survives (the retired section parser dropped it).
        assert!(actual.contains("### agent said nested\nbody\n## 3. Facts\nfacts text"));
        assert!(actual.contains("Chose rebase over merge"));
        assert!(actual.contains("`lane/x` `4795b657` kernel: wave one"));
        assert_eq!(new_work.tail_preparations, 0);
        assert_eq!(new_work.elided_copy_bytes, 0);
        assert_eq!(new_work.source_estimates, 0);
        Ok(())
    }

    #[test]
    fn full_and_pack_demands_match_owned_write_oracle() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("distill-complete-demand");
        let mut source = lane();
        let payload = "code λ\n\"\\".repeat(32768);
        let input = json!({"file_path":"/tmp/generated.rs", "content":payload,
            "path":"/tmp/referenced.rs", "unknown":{"retained":"sidecar fidelity"}});
        let bytes = serde_json::to_vec(&input)?;
        let input_path = world.base.join("input.json");
        std::fs::write(&input_path, &bytes)?;
        source.messages.insert(
            1,
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![Block::ToolUse {
                    id: "write".into(),
                    name: "Write".into(),
                    input: crate::Json::source(&input_path, 0, bytes.len() as u64)?,
                    namespace: None,
                }],
            ),
        );
        source.messages.insert(
            2,
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("write", "File written", false)],
            ),
        );
        source.messages.insert(
            3,
            msg(
                Role::User,
                MessageKind::Prompt,
                vec![text("Review /tmp/generated.rs after the write.")],
            ),
        );
        // Result association survives an intervening message, not only adjacent call/result turns.
        source.messages.swap(2, 3);
        let thinking_path = world.base.join("thinking.json");
        let bytes = serde_json::to_vec(&"ignored λ\n".repeat(32768))?;
        std::fs::write(&thinking_path, &bytes)?;
        let thinking = crate::Json::source(&thinking_path, 0, bytes.len() as u64)?
            .with_view(|view| view.source_text())?
            .unwrap();
        source.messages[1].content.insert(
            0,
            Block::Thinking {
                text: thinking,
                signature: None,
                encrypted: None,
                redacted: false,
            },
        );
        let mut frozen = Frozen::new("distill-complete-demand");
        for keep_last in [0, 2, 100] {
            let opts = DistillOptions {
                keep_last,
                ..Default::default()
            };
            let prepared = prepare_distill(&source, &opts)?;
            let (actual, actual_session) = prepared.into_resumable_session(&source);
            same_products(&prepare_distill(&source, &opts)?.distilled, &actual);
            WORK.with(|cell| cell.set(Work::default()));
            let pack = distill_pack(&source, &opts)?;
            let work = WORK.with(|cell| cell.get());
            assert_eq!(work.elided_copy_bytes, 0);
            assert_eq!(work.tail_preparations, 1);
            assert_eq!(work.source_estimates, source.messages.len());
            assert_eq!(pack.pack(), actual.pack());
            assert_eq!(serde_json::to_value(pack.stats)?, serde_json::to_value(&actual.stats)?);
            assert!(pack.elided.is_empty());
            let sidecar = world.base.join(format!("sidecar-{keep_last}.jsonl"));
            write_sidecar(&sidecar, &actual.elided, crate::publish::Claim::New)?;
            let expected_bytes: String = actual
                .elided
                .iter()
                .map(|row| {
                    format!(
                        "{}\n",
                        crate::prune::sidecar_line(&row.tool_use_id, &row.tool_name, &row.input, &row.content)
                    )
                })
                .collect();
            assert!(
                std::fs::read(&sidecar)? == expected_bytes.as_bytes(),
                "sidecar bytes differ"
            );
            frozen.insert(
                format!("keep{keep_last}"),
                json!({
                    "products": products(&actual),
                    "session": json_digest(&actual_session),
                    "sidecar": digest(std::fs::read(&sidecar)?),
                }),
            );
        }
        frozen.check()?;
        // Omitted thinking and code input retain the same checked failure boundary.
        std::fs::write(&thinking_path, b"changed")?;
        assert!(distill_pack(&source, &Default::default()).is_err());
        assert!(prepare_distill(&source, &Default::default()).is_err());
        source.messages[1]
            .content
            .retain(|block| !matches!(block, Block::Thinking { .. }));
        std::fs::write(&input_path, b"changed arguments")?;
        assert!(distill_pack(&source, &Default::default()).is_err());
        assert!(prepare_distill(&source, &Default::default()).is_err());
        Ok(())
    }

    #[test]
    fn full_analysis_reads_span_text_but_rejects_malformed_and_failed_sources() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("distill-legacy-spans");
        let path = world.base.join("span.txt");
        let opts = DistillOptions {
            keep_last: 0,
            ..Default::default()
        };
        let mut frozen = Frozen::new("distill-span-text");
        for body in [Some(r#"escaped\n\u03bb"#), Some(r#"malformed\q"#), None] {
            // A span covers a JSON string's body.
            if let Some(body) = body {
                std::fs::write(&path, format!("\"{body}\""))?;
            } else {
                std::fs::remove_file(&path)?;
            }
            let mut source = lane();
            source.messages.push(msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![Block::Text {
                    text: crate::Text::Span(crate::Span {
                        source: Some(path.clone()),
                        offset: 1,
                        len: body.map_or(20, str::len) as u64,
                        escaped: true,
                    }),
                }],
            ));
            if body.is_none_or(|body| body.starts_with("malformed")) {
                assert!(prepare_distill(&source, &opts).is_err());
                assert!(prepare_distill(&source, &opts).is_err());
                assert!(distill_pack(&source, &opts).is_err());
                assert!(findings(&source, &opts).is_err());
                continue;
            }
            let expected = prepare_distill(&source, &opts)?.distilled;
            frozen.insert(body.unwrap(), products(&expected));
            let pack = distill_pack(&source, &opts)?;
            assert_eq!(pack.pack(), expected.pack());
            assert_eq!(serde_json::to_value(pack.stats)?, serde_json::to_value(expected.stats)?);
        }
        frozen.check()
    }

    #[test]
    fn write_projection_preserves_decoded_lengths_and_wrong_type_fallbacks() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("distill-write-projection");
        let mut inputs = [
            Value::Null,
            json!({"file_path":44,"content":[]}),
            json!({"file_path":"/tmp/code.rs","content":"é\n\"\\", "command":"git log", "path":"/tmp/other"}),
            json!({"file_path":"/tmp/notes.md","content":"my note λ\n"}),
        ]
        .iter()
        .map(serde_json::to_vec)
        .collect::<Result<Vec<_>, _>>()?;
        inputs.extend([
            br#"{
                "file_path": "/tmp/code.rs", "content": "discarded",
                "file_path": 42, "content": ["late wrong type"]
            }"#
            .to_vec(),
            br#"{
                "file_path": null, "content": 17,
                "file_path": "/tmp/n\u006ftes.md", "content": "escaped \u03bb\n\"\\",
                "metadata": {"n": 1e0, "n": -0.0}
            }"#
            .to_vec(),
        ]);
        let mut frozen = Frozen::new("distill-write-projection");
        for (i, bytes) in inputs.into_iter().enumerate() {
            let input: Value = serde_json::from_slice(&bytes)?;
            let path = world.base.join(format!("input-{i}.json"));
            std::fs::write(&path, &bytes)?;
            let source = crate::Json::source(&path, 0, bytes.len() as u64)?;
            let summary = ToolInput::new("Write", &source, true, true)?;
            assert_eq!(summary.label("Write"), tool_label("Write", &input));
            let message = msg(Role::Assistant, MessageKind::Reply, Vec::new());
            let opts = DistillOptions::default();
            assert_eq!(
                own_words_from_tool(0, &message, "Write", &summary.fields, &opts).map(|e| e.body),
                own_words_from_tool(0, &message, "Write", &input, &opts).map(|e| e.body)
            );
            let brief = ToolInput::new("Write", &source, false, false)?;
            assert!(brief.fields.get("content").is_none());
            let mut session = Session::new("projected-write", Harness::Claude);
            session.messages = vec![
                msg(Role::User, MessageKind::Prompt, vec![text("brief")]),
                msg(
                    Role::Assistant,
                    MessageKind::Reply,
                    vec![Block::ToolUse {
                        id: "write".into(),
                        name: "Write".into(),
                        input: source,
                        namespace: None,
                    }],
                ),
                msg(
                    Role::Tool,
                    MessageKind::ToolResult,
                    vec![result("write", "written", false)],
                ),
            ];
            let options = DistillOptions {
                keep_last: 0,
                ..Default::default()
            };
            let expected = prepare_distill(&session, &options)?.distilled;
            frozen.insert(format!("input{i}"), products(&expected));
            let pack = distill_pack(&session, &options)?;
            assert_eq!(pack.pack(), expected.pack());
            assert_eq!(serde_json::to_value(pack.stats)?, serde_json::to_value(expected.stats)?);
        }
        frozen.check()
    }

    #[test]
    fn findings_checked_sources_match_owned_and_validate_ignored_thinking() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("findings-demanded-sources");
        let mut source = lane();
        source.messages[0].content = vec![text(&"Build the thing on /tmp/input.md. λ\n".repeat(256))];
        source.messages.push(msg(
            Role::Tool,
            MessageKind::ToolResult,
            vec![result("e1", &"error: demanded result λ\n".repeat(1024), true)],
        ));
        source.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![text(&"Chose simple /tmp/notes.md. λ\n".repeat(256))],
        ));
        for (mi, message) in source.messages.iter_mut().enumerate() {
            for (bi, block) in message.content.iter_mut().enumerate() {
                let path = world.base.join(format!("{mi}-{bi}.json"));
                match block {
                    Block::Text { text } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => {
                        let bytes = serde_json::to_vec(text.inline_str().unwrap())?;
                        std::fs::write(&path, &bytes)?;
                        *text = crate::Json::source(&path, 0, bytes.len() as u64)?
                            .with_view(|view| view.source_text())?
                            .unwrap();
                    }
                    Block::ToolUse { input, .. } => {
                        let bytes = serde_json::to_vec(input.resolve()?.as_ref())?;
                        std::fs::write(&path, &bytes)?;
                        *input = crate::Json::source(&path, 0, bytes.len() as u64)?;
                    }
                    _ => {}
                }
            }
        }
        let ignored_path = world.base.join("ignored.json");
        let bytes = serde_json::to_vec(&"ignored thinking λ\n".repeat(65536))?;
        std::fs::write(&ignored_path, &bytes)?;
        let ignored = crate::Json::source(&ignored_path, 0, bytes.len() as u64)?
            .with_view(|view| view.source_text())?
            .unwrap();
        assert!(matches!(ignored, crate::Text::Source(_)));
        for _ in 0..8 {
            source.messages.push(msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![Block::Thinking {
                    text: ignored.clone(),
                    signature: None,
                    encrypted: None,
                    redacted: false,
                }],
            ));
        }
        let options = DistillOptions::default();
        let actual = findings(&source, &options)?;
        // The final own-words entry survives the following Facts section.
        assert!(actual.contains("Chose simple /tmp/notes.md."));
        let owned = crate::emit::materialized_session(&source)?;
        assert_eq!(findings(&source, &options)?, findings(&owned, &options)?);
        let argument_path = world.base.join("1-0.json");
        let original_arguments = std::fs::read(&argument_path)?;
        std::fs::write(&argument_path, b"changed arguments")?;
        assert!(findings(&source, &options).is_err());
        // Use a fresh descriptor after restoring bytes; guards intentionally reject old identities.
        std::fs::write(&argument_path, &original_arguments)?;
        let Block::ToolUse { input, .. } = &mut source.messages[1].content[0] else {
            unreachable!()
        };
        *input = crate::Json::source(&argument_path, 0, original_arguments.len() as u64)?;
        let span_path = world.base.join("legacy-span.txt");
        std::fs::write(&span_path, b"\"legacy span reply\"")?;
        source.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![Block::Text {
                text: crate::Text::Span(crate::Span {
                    source: Some(span_path),
                    offset: 1,
                    len: 17,
                    escaped: false,
                }),
            }],
        ));
        let actual = findings(&source, &options)?;
        assert!(actual.contains("legacy span reply"));
        let owned = crate::emit::materialized_session(&source)?;
        assert_eq!(actual, findings(&owned, &options)?);
        let wrong_path = world.base.join("wrong-type.json");
        std::fs::write(&wrong_path, b"42")?;
        let crate::Json::Source(range) = crate::Json::source(&wrong_path, 0, 2)? else {
            unreachable!()
        };
        let mut wrong = Session::new("wrong", Harness::Claude);
        wrong.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![Block::Thinking {
                text: crate::Text::Source(crate::lazy::SourceText::new(range)),
                signature: None,
                encrypted: None,
                redacted: false,
            }],
        ));
        assert!(findings(&wrong, &options).is_err());
        // Even completely ignored source payloads preserve the old failure boundary.
        std::fs::write(&ignored_path, b"changed")?;
        assert!(findings(&source, &options).is_err());
        Ok(())
    }

    /// The distill fixture: a lane with a brief, polls, a commit, an error, and a trailing call
    /// (`z1`) that never got its result. Shared with the emitter's resumability test.
    pub(crate) fn lane() -> Session {
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
            messages: msgs,
            ..Session::new("agent-test", Harness::Claude)
        }
    }

    #[test]
    fn keeps_brief_words_commits_errors_and_collapses_polls() {
        let s = lane();
        let d = prepare_distill(
            &s,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        )
        .unwrap()
        .distilled;
        let p = d.pack();
        assert!(p.contains("You are lane X. Build the thing on box1."));
        assert!(p.contains("Chose rebase over merge"));
        assert!(p.contains("`lane/x` `4795b657` kernel: wave one"));
        assert!(p.contains("unknown identifier `bar`"));
        assert!(p.contains("Poll the build ×20"), "{p}");
        // Each ledger row names the (last) call's id and output size, and the hand-off says how
        // to fetch it.
        assert!(p.contains("Poll the build ×20 → x (+499 lines) · `t19` 1000 B"), "{p}");
        assert!(p.contains("- **Elided outputs** (2"), "{p}");
        assert!(p.contains("`cv cat agent-test <tool_use_id>`; the ledger names them"));
        assert!(p.contains("`ember@box1` ×20"));
        // The 20 polls' outputs are elided, not inlined.
        assert!(!p.contains(&"x\n".repeat(50)));
        assert_eq!(d.stats.commits, 1);
        assert!(d.elided.len() >= 20);
    }

    #[test]
    fn tail_opens_on_an_assistant_turn_and_keeps_the_source_threading() {
        let s = lane();
        let d = prepare_distill(
            &s,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        )
        .unwrap()
        .distilled;
        assert_eq!(s.messages[d.tail_start].role, Role::Assistant);
        let r = resumable_session(
            &s,
            &d,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.messages[0].role, Role::User);
        assert_eq!(r.messages[1].role, Role::Assistant);
        assert!(r.messages.iter().all(|m| m.usage.is_none()));
        // A selector: `z1` (never answered) and the tail's source parents are still here; the
        // emitter drops the one and re-links the other (see `emit::reshape_verbs_emit_resumable`).
        let has_z1 = r
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { id, .. } if id == "z1"));
        assert!(has_z1, "the pack shows the lane stopped mid-call");
        assert_eq!(r.messages[0].parent_id, None, "the opening prompt is the root");
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
    fn quiet_commits_and_compound_git_log_heads_are_facts() {
        // `git commit -q` then `git log --oneline -1`: the oneline is the commit, keyed by repo.
        let one =
            "cd /r/demo && git add x && git -c commit.gpgsign=false commit -q -F $M/a.txt && git log --oneline -1";
        assert_eq!(
            quiet_commits(one, "40fdb60 mini ship: a genesis may name the observer\n"),
            vec![(
                "/r/demo".to_string(),
                "40fdb60",
                "mini ship: a genesis may name the observer"
            )]
        );
        let two = "git -C /r/a commit -q -F m1 && git -C /r/a log --oneline -1 && git -C /r/b commit -qF m2 && git -C /r/b log --oneline -1";
        let got = quiet_commits(two, "Auto-merging x\naaa1111 one\n 1 file changed\nbbb2222 two");
        assert_eq!(
            got,
            vec![("/r/a".into(), "aaa1111", "one"), ("/r/b".into(), "bbb2222", "two")]
        );
        // No `git log` after the commit, a dry run, or no commit at all: nothing is claimed.
        assert!(quiet_commits("git commit -q -F m", "aaa1111 x").is_empty());
        assert!(quiet_commits("git commit --dry-run && git log --oneline -1", "aaa1111 x").is_empty());
        assert!(quiet_commits("git log --oneline -1", "aaa1111 x").is_empty());

        // A compound command's head is its oneline line wherever it sits; a count is not one.
        let (_, line) = git_log_head(
            "cd /w && swarm-build cargo build 2>&1 | tail -1 && wc -l f && git log --oneline -1",
            "Finished release\n1234567 total\n3c90b001 parity dry run",
        )
        .unwrap();
        assert_eq!(line, "3c90b001 parity dry run");

        // Through the whole distill: the commit is listed and counted.
        let mut lane = lane();
        lane.messages.insert(
            1,
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![call("q1", one, "Commit quietly")],
            ),
        );
        lane.messages.insert(
            2,
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result(
                    "q1",
                    "40fdb60 mini ship: a genesis may name the observer",
                    false,
                )],
            ),
        );
        let d = distill_pack(
            &lane,
            &DistillOptions {
                keep_last: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(d.stats.commits, 2, "the quiet commit and the bracketed one");
        assert!(d.head_md.contains("[2] `/r/demo` `40fdb60` mini ship"), "{}", d.head_md);
    }

    /// Distill `source`, resume it, and let the resumed lane say and run one more thing.
    fn next_generation(source: &Session, words: &str, k: usize) -> (Distilled, Session) {
        let opts = DistillOptions {
            keep_last: 2,
            ..Default::default()
        };
        let (d, mut resumed) = prepare_distill(source, &opts).unwrap().into_resumable_session(source);
        let id = format!("g{k}");
        resumed
            .messages
            .push(msg(Role::Assistant, MessageKind::Reply, vec![text(words)]));
        resumed
            .messages
            .push(msg(Role::Assistant, MessageKind::Reply, vec![call(&id, "ls", "List")]));
        resumed.messages.push(msg(
            Role::Tool,
            MessageKind::ToolResult,
            vec![result(&id, "a\nb", false)],
        ));
        (d, resumed)
    }

    #[test]
    fn distilling_a_distilled_lane_carries_generations_instead_of_nesting_packs() {
        let brief = "You are lane X. Build the thing on box1.";
        let (first, gen2) = next_generation(&lane(), "Second generation decision.", 2);
        assert!(first.head_md.contains("Poll the build ×20"));
        let (second, gen3) = next_generation(&gen2, "Third generation decision.", 3);
        let (third, _) = next_generation(&gen3, "Fourth generation decision.", 4);
        for d in [&second, &third] {
            let h = &d.head_md;
            let body = &h[h.find(BRIEF_HEAD).unwrap()..];
            assert_eq!(body.matches(brief).count(), 1, "the original brief, once: {h}");
            assert!(!h.contains(PREAMBLE_HEAD), "no pack nested in the brief: {h}");
            assert!(
                !h.contains("Chose rebase over merge"),
                "an old generation's words stay in its source"
            );
            assert!(h.contains("`lane/x` `4795b657` kernel: wave one"), "its commits carry");
            assert!(h.contains("**Its brief**: the brief above."));
            assert!(!h.contains("Poll the build ×20"), "its ledger stays in its source");
            assert!(d.pack().contains("Second generation decision."));
        }
        let generations = |d: &Distilled| {
            d.head_md
                .matches("generation distilled from `claude:agent-test`")
                .count()
        };
        assert_eq!((generations(&second), generations(&third)), (1, 2));
        assert!(third.pack().contains("Third generation decision."));
        // One more generation costs a few lines, not a copy of every pack before it.
        assert!(third.head_md.len() < second.head_md.len() + first.head_md.len() / 4);

        // A pack written before the `Earlier generations` section existed nested its predecessor
        // in its brief; its own brief is the first message sent to it.
        let old = format!(
            "{PREAMBLE_HEAD} the agent…\n\n# Distilled context — X\n\nSource: `claude:old-gen`\n\n\
             {BRIEF_HEAD}0]\n\n{}\n\n{TIMELINE_HEAD}, and …\n\n### [2] 10-27 08:59 · message delivered \
             while working\n\nNext: ship it.\n\nThe rules again.\n\n### [3] 10-27 09:00 · agent said\n\n\
             Old words.\n\n{FACTS_HEAD}\n\n- **Commits made**: none seen\n- **Paths most referenced**: \
             `/x/docs/LANE-STATE.md` ×2 · `/x/y/z.rs` ×3\n\n{LEDGER_HEAD}, one line each\n\n- [1] List → a\n",
            gen2.messages[0].text().unwrap()
        );
        let (inner, carried) = unwrap_pack(&old).unwrap();
        assert_eq!(inner, brief);
        let first_gen = carried.find("generation distilled from `claude:agent-test`").unwrap();
        let old_gen = carried.find("generation distilled from `claude:old-gen`").unwrap();
        assert!(first_gen < old_gen, "oldest first: {carried}");
        assert!(carried.contains("(first paragraph of the first message to it): Next: ship it.\n"));
        assert!(
            carried.ends_with("- **State files named**: `/x/docs/LANE-STATE.md` ×2"),
            "{carried}"
        );
        for gone in [
            "Old words.",
            "The rules again.",
            "/x/y/z.rs",
            "[1] List → a",
            "Chose rebase",
        ] {
            assert!(!carried.contains(gone), "{gone}: {carried}");
        }
        assert!(unwrap_pack(brief).is_none());
    }

    #[test]
    fn commit_message_notes_are_cut_to_their_head_and_other_notes_stay_whole() {
        let reasons = "Because the queue wants linear ranges. ".repeat(60);
        let write = |id: &str, path: &str, content: &str| Block::ToolUse {
            id: id.into(),
            name: "Write".into(),
            input: json!({"file_path": path, "content": content}).into(),
            namespace: None,
        };
        let shell_msg = format!(
            "cat > /s/msg/e1.txt <<'EOF'\nedge: the shell subject\n\n{reasons}\n\nCo-Authored-By: A <a@b>\nEOF\n\
             git commit -q -F /s/msg/e1.txt && git log --oneline -1"
        );
        let notes = format!("Hand-off: {}", "the box is idle. ".repeat(100));
        let mut s = Session::new("agent-notes", Harness::Claude);
        s.messages = vec![
            msg(Role::User, MessageKind::Prompt, vec![text("Ship it.")]),
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![call("h1", &shell_msg, "Commit")],
            ),
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("h1", "aaa1111 edge: the shell subject", false)],
            ),
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![write(
                    "w1",
                    "/s/msg/e2.txt",
                    &format!("edge: the written subject\n\n{reasons}"),
                )],
            ),
            msg(Role::Tool, MessageKind::ToolResult, vec![result("w1", "ok", false)]),
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![call("c2", "git commit -qF $M/e2.txt && git log --oneline -1", "Commit")],
            ),
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("c2", "bbb2222 edge: the written subject", false)],
            ),
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![write("w2", "/s/NOTES.md", &notes)],
            ),
            msg(Role::Tool, MessageKind::ToolResult, vec![result("w2", "ok", false)]),
            msg(Role::Assistant, MessageKind::Reply, vec![call("z", "ls", "List")]),
            msg(Role::Tool, MessageKind::ToolResult, vec![result("z", "a", false)]),
        ];
        let d = distill_pack(
            &s,
            &DistillOptions {
                keep_last: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let h = &d.head_md;
        assert!(h.contains("agent wrote /s/msg/e1.txt (commit message, shell)"), "{h}");
        assert!(h.contains("agent wrote /s/msg/e2.txt (commit message)"), "{h}");
        assert!(h.contains("edge: the shell subject") && h.contains("edge: the written subject"));
        assert_eq!(h.matches(COMMIT_NOTE_POINTER).count(), 2);
        assert!(!h.contains(&reasons), "the commit holds the whole message");
        assert!(
            h.contains(notes.trim()),
            "a note that is not a commit message stays whole"
        );
        assert_eq!(d.stats.commits, 2);
    }

    #[test]
    fn handoff_opens_the_pack_with_the_last_message_and_live_state() {
        let mut s = lane();
        let n = s.messages.len();
        s.messages.insert(
            n - 1,
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![call(
                    "j1",
                    "cd /w/lane && nohup ./run.sh & echo $! > $S/run.pid; git log --oneline -1",
                    "Start",
                )],
            ),
        );
        s.messages.insert(
            n,
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("j1", "started\n9f3257a0 lane: tip", false)],
            ),
        );
        s.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![text(
                "Handing off: the box is idle; the watcher's PID is in /build/lanes/w.pid.",
            )],
        ));
        let d = distill_pack(
            &s,
            &DistillOptions {
                keep_last: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let h = &d.head_md;
        let handoff = h.find("## 0. Hand-off").expect("a hand-off section");
        assert!(handoff < h.find(BRIEF_HEAD).unwrap(), "the hand-off comes first");
        assert!(h.contains("**Its last message**") && h.contains("Handing off: the box is idle"));
        assert!(h.contains("`/w/lane` → `9f3257a0 lane: tip`"), "{h}");
        assert!(h.contains("`$S/run.pid`") && h.contains("`/build/lanes/w.pid`"), "{h}");
        assert!(
            h.contains("**Directories it worked in** (newest last): `/srv/lanes/x/src`") || h.contains("`/w/lane`")
        );
        // A lane with nothing to hand off (no words, no state) gets no empty section.
        let mut bare = Session::new("agent-bare", Harness::Claude);
        bare.messages = vec![msg(Role::User, MessageKind::Prompt, vec![text("Go.")])];
        assert!(!distill_pack(&bare, &DistillOptions::default())
            .unwrap()
            .head_md
            .contains("Hand-off"));
    }

    #[test]
    fn grounding_carries_the_last_state_file_open_items_and_decisions_verbatim() {
        let write = |id: &str, path: &str, content: &str| Block::ToolUse {
            id: id.into(),
            name: "Write".into(),
            input: json!({"file_path": path, "content": content}).into(),
            namespace: None,
        };
        let send = |id: &str, message: &str| Block::ToolUse {
            id: id.into(),
            name: "SendMessage".into(),
            input: json!({"to": "main", "message": message}).into(),
            namespace: None,
        };
        let turn = |blocks: Vec<Block>| msg(Role::Assistant, MessageKind::Reply, blocks);
        let ok = |id: &str| msg(Role::Tool, MessageKind::ToolResult, vec![result(id, "ok", false)]);
        let mut s = Session::new("agent-ground", Harness::Claude);
        s.messages = vec![
            msg(Role::User, MessageKind::Prompt, vec![text("Ship it.")]),
            turn(vec![text(
                "Owed: an item from long ago, since closed.\nDecided: rebase over merge.",
            )]),
        ];
        // 61 polls push the old open item out of the last-60-calls window; decisions are kept.
        for k in 0..61 {
            let id = format!("p{k}");
            s.messages.push(turn(vec![call(&id, "ls", "Poll")]));
            s.messages.push(ok(&id));
        }
        s.messages.extend([
            turn(vec![write("w1", "/w/docs/LANE-STATE.md", "# old state")]),
            ok("w1"),
            turn(vec![
                text("Still red: jtour step 4.\nNothing else to say."),
                write(
                    "w2",
                    "/w/docs/LANE-STATE.md",
                    "# Lane state\n\nstep 4 red; next: fix the row",
                ),
            ]),
            ok("w2"),
            turn(vec![send(
                "m1",
                "Batch for the parent: the sudoers line.\nThe ruling stands: default caps 4030.",
            )]),
            ok("m1"),
            turn(vec![call("z", "ls", "List")]),
            ok("z"),
        ]);
        let d = distill_pack(
            &s,
            &DistillOptions {
                keep_last: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let h = &d.head_md;
        let (handoff, grounding, brief) = (
            h.find(HANDOFF_HEAD).unwrap(),
            h.find(GROUNDING_HEAD).unwrap(),
            h.find(BRIEF_HEAD).unwrap(),
        );
        assert!(handoff < grounding && grounding < brief, "{h}");
        let g = &h[grounding..brief];
        assert!(g.contains("**State file** `/w/docs/LANE-STATE.md` [126] (written by Write):\n\n~~~~\n# Lane state\n\nstep 4 red; next: fix the row\n~~~~"), "{g}");
        assert!(!g.contains("# old state"), "the last write wins");
        assert!(
            g.contains("- [126] Still red: jtour step 4.\n")
                && g.contains("- [128] Batch for the parent: the sudoers line.\n"),
            "{g}"
        );
        assert!(
            !g.contains("an item from long ago"),
            "open items come from the last 60 calls: {g}"
        );
        assert!(
            g.contains("- [1] Decided: rebase over merge.\n")
                && g.contains("- [128] The ruling stands: default caps 4030.\n"),
            "{g}"
        );
        assert!(!g.contains("Nothing else to say"));

        // A heredoc into a scratch state file carries its body; the next generation keeps its path.
        let shell = "cat > /s/scratchpad/lane/state.md <<'EOF'\nwaiting on cut 9\nEOF\necho done";
        s.messages
            .insert(s.messages.len() - 2, turn(vec![call("h1", shell, "State")]));
        s.messages.insert(s.messages.len() - 2, ok("h1"));
        let (d, mut resumed) = prepare_distill(
            &s,
            &DistillOptions {
                keep_last: 1,
                ..Default::default()
            },
        )
        .unwrap()
        .into_resumable_session(&s);
        assert!(d.head_md.contains("**State file** `/s/scratchpad/lane/state.md` [130] (written by shell):\n\n~~~~\nwaiting on cut 9\n~~~~"), "{}", d.head_md);
        resumed.messages.push(turn(vec![text("Resumed.")]));
        let next = distill_pack(&resumed, &DistillOptions::default()).unwrap();
        assert!(
            next.head_md
                .contains("- **State file it last wrote**: `/s/scratchpad/lane/state.md`"),
            "{}",
            next.head_md
        );
    }

    #[test]
    fn hosts_skip_ssh_options() {
        assert_eq!(hosts_in("ssh -o ConnectTimeout=10 persvati 'ls'"), vec!["persvati"]);
        assert_eq!(hosts_in("scp -q ember@1.2.3.4:/srv/x /tmp/y"), vec!["ember@1.2.3.4"]);
    }
    #[test]
    fn findings_preserve_embedded_markdown_without_importing_incoming_sections() -> anyhow::Result<()> {
        let mut source = Session::new("markdown-findings", Harness::Claude);
        source.title = Some("Markdown findings".into());
        let own = "A decision.\n### source subsection\nStill my own words.\n## 3. Facts (mined from tool inputs and outputs)\nNot extracted facts.\n### [pretend] · agent said\nStill source text.";
        source.messages = vec![
            msg(
                Role::User,
                MessageKind::Prompt,
                vec![text("incoming\n### [forged] · agent said\nnot mine")],
            ),
            msg(Role::Assistant, MessageKind::Reply, vec![text(own)]),
            msg(
                Role::Assistant,
                MessageKind::Reply,
                vec![text("A later independent finding.")],
            ),
        ];
        let actual = findings(&source, &Default::default())?;
        assert!(actual.contains(own), "{actual}");
        assert!(actual.contains("A later independent finding."), "{actual}");
        assert!(!actual.contains("not mine"), "{actual}");
        assert!(
            actual.contains("### Facts\n\n- **Commits made**: none seen"),
            "{actual}"
        );
        Ok(())
    }
    #[test]
    fn appending_findings_keeps_the_distill_token_heuristic() -> anyhow::Result<()> {
        let opts = DistillOptions {
            keep_last: 2,
            ..Default::default()
        };
        let mut distilled = distill_pack(&lane(), &opts)?;
        let original = distilled.stats.pack_est_tokens;
        let tail = distilled.stats.tail_est_tokens;
        distilled.append_findings("Another lane's findings λ.");
        assert!(distilled.stats.pack_est_tokens > original);
        assert_eq!(
            distilled.stats.pack_est_tokens,
            est_tokens_bytes(distilled.head_md.len(), PACK_KIND)
        );
        assert_eq!(distilled.stats.tail_est_tokens, tail);
        assert_eq!(
            distilled.stats.distilled_est_tokens,
            distilled.stats.pack_est_tokens + tail
        );
        Ok(())
    }
    #[test]
    fn tail_keeps_transitive_delayed_call_results_and_clears_all_resume_usage() -> anyhow::Result<()> {
        let mut source = Session::new("parallel-tail", Harness::Claude);
        source.messages = vec![
            msg(Role::User, MessageKind::Prompt, vec![text("brief")]),
            msg(Role::Assistant, MessageKind::Reply, vec![call("a", "first", "first")]),
            msg(Role::User, MessageKind::Prompt, vec![text("follow-up")]),
            msg(Role::Assistant, MessageKind::Reply, vec![call("b", "second", "second")]),
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("a", "delayed first", false)],
            ),
            msg(Role::User, MessageKind::Prompt, vec![text("another follow-up")]),
            msg(Role::Assistant, MessageKind::Reply, vec![call("c", "third", "third")]),
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("b", "delayed second", false)],
            ),
            msg(
                Role::Tool,
                MessageKind::ToolResult,
                vec![result("c", "third result", false)],
            ),
            msg(
                Role::Assistant,
                MessageKind::Carrier,
                vec![call("hidden", "hidden", "harness only")],
            ),
        ];
        for message in &mut source.messages {
            message.usage = Some(crate::ir::Usage {
                input_tokens: Some(100000),
                ..Default::default()
            });
            message
                .extra
                .insert(crate::ir::CARRIER_KEY.into(), json!({"usage":{"input_tokens":100000}}));
            crate::ir::CompleteRecord::attach(
                std::slice::from_mut(message),
                Harness::Claude,
                json!({"usage":{"input_tokens":100000}}).into(),
                Default::default(),
            );
        }
        let options = DistillOptions {
            keep_last: 1,
            ..Default::default()
        };
        let prepared = prepare_distill(&source, &options)?;
        assert_eq!(prepared.distilled.tail_start, 1);
        assert_eq!(prepared.distilled.stats.tail_tool_calls, 3);
        let (_, resumed) = prepared.into_resumable_session(&source);
        assert!(resumed.messages.iter().all(|message| message.usage.is_none()));
        assert!(resumed
            .messages
            .iter()
            .all(|message| message.complete_record.is_none() && !message.extra.contains_key(crate::ir::CARRIER_KEY)));
        assert!(resumed.messages.iter().all(Message::is_model_visible));
        let retained = tool_index(&resumed.messages);
        for message in &resumed.messages {
            for block in &message.content {
                if let Block::ToolResult { tool_use_id, .. } = block {
                    assert!(retained.contains_key(tool_use_id.as_str()), "{tool_use_id}");
                }
            }
        }
        assert_eq!(retained.len(), 3);
        assert!(retained.keys().all(|id| *id != "hidden"));
        assert_eq!(
            tail_cut(&source.messages, 0, &tool_index(&source.messages)),
            source.messages.len()
        );
        Ok(())
    }
    #[test]
    fn facts_keep_first_seen_ties_and_latest_observations_without_linear_lookup() {
        let mut facts = Facts::default();
        for path in ["z", "a", "z", "b", "b", "a", "b"] {
            bump(&mut facts.paths, path);
        }
        facts.tips = vec![
            (1, "main".into(), "old".into()),
            (2, "feature".into(), "feature-sha".into()),
            (3, "main".into(), "new".into()),
        ];
        let mut rendered = String::new();
        render_facts(&mut rendered, &facts);
        assert!(
            rendered.contains("**Paths most referenced**: `b` ×3 · `z` ×2 · `a` ×2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("`feature` @ `feature-sha` [2] · `main` @ `new` [3]"),
            "{rendered}"
        );
        assert!(!rendered.contains("`old`"));
        let output = format!(
            "{}error: first diagnostic\nerror: second diagnostic\nExit code 1\n",
            "progress\n".repeat(10000)
        );
        assert_eq!(
            scan_result_facts(0, "Bash", &Value::Null, "command", &output, true, &mut facts),
            "✗ error: first diagnostic"
        );
        assert_eq!(
            scan_result_facts(
                1,
                "Bash",
                &Value::Null,
                "command",
                "progress\nlast detail\nExit code 1",
                true,
                &mut facts
            ),
            "✗ last detail"
        );
    }
    #[test]
    fn context_accounting_uses_the_last_nonzero_recorded_prompt() -> anyhow::Result<()> {
        let mut source = lane();
        source.messages[0].usage = Some(crate::ir::Usage {
            input_tokens: Some(1234),
            ..Default::default()
        });
        source.messages.last_mut().unwrap().usage = Some(crate::ir::Usage {
            output_tokens: Some(9),
            ..Default::default()
        });
        let actual = distill_pack(&source, &Default::default())?;
        assert_eq!(actual.stats.recorded_context_tokens, Some(1234));
        Ok(())
    }
    #[test]
    fn findings_reject_missing_legacy_thinking_sources_even_when_omitted() {
        let world = cv_testkit::World::new("findings-missing-span");
        let mut source = Session::new("missing-span", Harness::Claude);
        source.messages.push(msg(
            Role::Assistant,
            MessageKind::Reply,
            vec![
                text("visible finding"),
                Block::Thinking {
                    text: crate::Text::Span(crate::Span {
                        source: Some(world.base.join("missing.txt")),
                        offset: 0,
                        len: 10,
                        escaped: false,
                    }),
                    signature: None,
                    encrypted: None,
                    redacted: false,
                },
            ],
        ));
        assert!(findings(&source, &Default::default()).is_err());
        assert!(distill_pack(&source, &Default::default()).is_err());
    }
}
