//! Trajectory sensors folded in the same streamed doctor pass: what happened right after each
//! compaction (`post_compaction_churn`), what a compaction summary says that nobody said before
//! it (`summary_provenance_contrast`), repeated identical reads (`spin_episodes`), and loaded
//! instruction files long enough to crowd every turn (`instruction_bloat`).
//!
//! All four are deterministic counts over message order. A turn is one assistant response:
//! consecutive assistant records sharing a response key ([`crate::stats::usage_key`]) are one turn.

use crate::ir::{Block, Message, MessageKind, Origin, Role};
use crate::tools::classify::{visit_tool_ops, ClassifyOpts, ToolOp};
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;

/// Turns inspected after each compaction seam.
pub const CHURN_TURNS_AFTER: usize = 6;
/// The windows each seam is counted over: the first 3 turns and all [`CHURN_TURNS_AFTER`].
pub const CHURN_WINDOWS: [usize; 2] = [3, CHURN_TURNS_AFTER];
/// Pre-compaction turns whose reads count as already known.
pub const CHURN_TURNS_BEFORE: usize = 12;
/// A seam is lossy when at least this many tool errors and human corrections follow it in the
/// full window…
pub const LOSSY_MIN_EVENTS: u64 = 2;
/// …at a rate of at least this many per tool call.
pub const LOSSY_MIN_RATE: f64 = 0.25;
/// A loaded instruction file longer than this many words is reported.
pub const INSTRUCTION_BLOAT_WORDS: usize = 600;
/// Identical consecutive read/search calls that make a spin.
pub const SPIN_MIN_CALLS: usize = 4;
/// Unwitnessed tokens listed per summary (the count is exact).
const UNWITNESSED_SHOWN: usize = 12;
/// Witness vocabulary cap; past it, further tokens are not recorded (biasing toward flagging).
const WITNESS_CAP: usize = 500_000;
/// Bounded normalized-whitespace corpus for contiguous command substring/n-gram matching.
const WITNESS_CORPUS_CAP: usize = 4 * 1024 * 1024;

/// What followed one compaction seam within its first `k` turns. Records re-emitted under an
/// already-counted message id or tool call id count once.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ChurnWindow {
    pub k: usize,
    pub tool_calls: u64,
    /// The expected cost of compaction: reads of files already read in the
    /// [`CHURN_TURNS_BEFORE`] turns before the seam, whose contents it evicted.
    pub eviction_rereads: u64,
    pub tool_errors: u64,
    /// Failed calls whose tool and input already failed in this window (a subset of
    /// `tool_errors`).
    pub repeated_failures: u64,
    /// Human prompts that read as a correction ("no, …", "I already said …").
    pub corrections: u64,
    /// The suspect regressions: `tool_errors + corrections`.
    pub repeated_failures_and_corrections: u64,
}

/// What followed one compaction seam.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChurnSeam {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Message index of the seam (the boundary, or the summary when no boundary preceded it).
    pub index: usize,
    /// Turns inspected (at most [`CHURN_TURNS_AFTER`]; fewer when the session ends).
    pub turns: usize,
    /// One window per [`CHURN_WINDOWS`] entry, shortest first.
    pub windows: [ChurnWindow; 2],
    /// The full window's `repeated_failures_and_corrections / max(tool_calls, 1)`.
    pub churn_rate: f64,
    /// Suspect regressions, not eviction re-reads, cross [`LOSSY_MIN_EVENTS`] and
    /// [`LOSSY_MIN_RATE`] in the full window.
    pub lossy: bool,
    /// The re-read paths, first occurrence order.
    pub reread_paths: Vec<String>,
}

/// One loaded instruction file (`CLAUDE.md`, `AGENTS.md`, a memory index, …) over
/// [`INSTRUCTION_BLOAT_WORDS`]. Every turn after the load carries it, so its size is paid per turn.
/// The word and token counts are estimates of the injected text, an advisory heuristic.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstructionBloat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Message index of the first load.
    pub index: usize,
    /// The file as the harness named it.
    pub path: String,
    /// Whitespace-separated words in the largest load.
    pub words: usize,
    pub tokens_est: u64,
    /// Times the file was injected in this session (re-injected after compactions, for one).
    pub loads: usize,
}

/// Imperative, path and URL tokens in one compaction summary, against what was said before it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SummaryContrast {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub index: usize,
    /// Distinct URL, inline-command and path tokens found in the summary.
    pub candidates: usize,
    /// How many never appeared in earlier human prompts or assistant-authored text and calls.
    pub unwitnessed_count: usize,
    /// The first [`UNWITNESSED_SHOWN`] of them.
    pub unwitnessed: Vec<String>,
}

/// One run of identical read/search calls.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SpinEpisode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Message index of the run's first call.
    pub index: usize,
    pub tool: String,
    /// The canonical input, truncated.
    pub input: String,
    pub calls: usize,
}

#[derive(Debug, Default)]
pub(super) struct Findings {
    pub churn: Vec<ChurnSeam>,
    pub contrast: Vec<SummaryContrast>,
    pub spins: Vec<SpinEpisode>,
    pub bloat: Vec<InstructionBloat>,
}

struct OpenSeam {
    seam: ChurnSeam,
    known: HashSet<String>,
    /// Message ids (`m:`), calls (`c:`) and failed results (`e:`) already counted.
    seen: HashSet<String>,
    /// The current message repeats one already counted.
    dup: bool,
    /// Call id → tool and canonical input, for calls made in the window.
    calls: HashMap<String, String>,
    failed: HashSet<String>,
}

impl OpenSeam {
    /// Apply `f` to every window the seam's current turn still falls in.
    fn bump(&mut self, f: impl Fn(&mut ChurnWindow)) {
        let turns = self.seam.turns;
        for w in self.seam.windows.iter_mut().filter(|w| turns <= w.k) {
            f(w);
        }
    }
}

#[derive(Default)]
pub(super) struct Trajectory {
    idx: usize,
    /// Response key of the current turn, so split assistant records stay one turn.
    turn_key: Option<Option<String>>,
    /// Reads of the last [`CHURN_TURNS_BEFORE`] turns, newest last.
    recent_reads: VecDeque<Vec<String>>,
    open: Vec<OpenSeam>,
    /// A boundary opened a seam and no turn has happened since: its summary joins that seam.
    seam_pending_summary: bool,
    witness_tokens: HashSet<String>,
    witness_spans: HashSet<String>,
    witness_corpus: String,
    spin: Option<(String, String, usize, usize)>,
    /// Instruction files seen, by path, in first-load order.
    instructions: Vec<InstructionBloat>,
    found: Findings,
}

static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]{2,200})`").unwrap());
static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:[~.]{0,2}/?[\w.@+-]+(?:/[\w.@+-]+)+/?",
        r"|[\w.-]*\w\.(?:rs|py|js|mjs|ts|tsx|jsx|go|rb|java|kt|c|h|cc|cpp|hpp|cs|swift|md|toml|json|jsonl|yaml|yml|sh|zsh|txt|html|css|sql|lock|env|cfg|ini|xml))$"
    ))
    .unwrap()
});

/// A human prompt that pushes back on what the agent just did.
static CORRECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^\W*(?:no\b|nope\b|stop\b|wrong\b|undo\b|revert\b|don'?t\b|do not\b|that'?s not\b|not that\b)",
        r"|(?i)\b(?:i (?:already )?(?:said|told you)|as i said|you forgot|we already|already (?:did|done|fixed|told)|not what i)\b"
    ))
    .unwrap()
});
/// Claude Code's instruction-file header inside a system reminder.
static CONTENTS_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^Contents of (\S+) \(").unwrap());
/// Codex's AGENTS.md dump.
static AGENTS_FOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\s*# AGENTS\.md instructions for (\S+)").unwrap());

/// `(path, body)` for each instruction file injected in `text`.
fn instruction_files(text: &str) -> Vec<(&str, &str)> {
    if let Some(caps) = AGENTS_FOR.captures(text) {
        let path = caps.get(1).map_or("AGENTS.md", |m| m.as_str());
        return vec![(path, &text[caps.get(0).map_or(0, |m| m.end())..])];
    }
    let heads: Vec<_> = CONTENTS_OF.captures_iter(text).collect();
    let mut out = Vec::with_capacity(heads.len());
    for (i, caps) in heads.iter().enumerate() {
        let (Some(whole), Some(path)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let start = text[whole.end()..].find('\n').map_or(text.len(), |n| whole.end() + n);
        let end = heads
            .get(i + 1)
            .and_then(|c| c.get(0))
            .map_or(text.len(), |m| m.start());
        let body = &text[start..end];
        let body = body.find("</system-reminder>").map_or(body, |n| &body[..n]);
        out.push((path.as_str(), body));
    }
    out
}

fn trim_token(t: &str) -> &str {
    t.trim_matches(|c: char| "\"'()[]{}<>,;:!?`*".contains(c))
        .trim_end_matches('.')
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace().map(trim_token).filter(|t| t.len() >= 2)
}

fn is_url(t: &str) -> bool {
    t.starts_with("http://") || t.starts_with("https://")
}

fn norm_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract non-comment lines inside CommonMark fenced code blocks (` ``` ` / `~~~`).
fn fenced_code_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut open: Option<(u8, usize)> = None;
    for raw in text.lines() {
        let trimmed = raw.trim_start_matches([' ', '\t', '>']);
        let fence_byte = trimmed.as_bytes().first().copied();
        if let Some(ch @ (b'`' | b'~')) = fence_byte {
            let run = trimmed.bytes().take_while(|&b| b == ch).count();
            if run >= 3 {
                let rest = &trimmed[run..];
                if let Some((open_ch, open_len)) = open {
                    if ch == open_ch && run >= open_len && rest.trim().is_empty() {
                        open = None;
                        continue;
                    }
                } else if ch != b'`' || !rest.contains('`') {
                    open = Some((ch, run));
                    continue;
                }
            }
        }
        if open.is_some() {
            let line = raw.trim().trim_start_matches("$ ").trim();
            if line.len() >= 2 && line.len() <= 200 && !line.starts_with('#') && !line.starts_with("//") {
                out.push(line);
            }
        }
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// True when `needle` (already whitespace-normalized) appears as a contiguous phrase in `haystack`
/// without splitting an identifier/flag token at either end.
fn contains_contiguous_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    let check_left = is_word_byte(nb[0]);
    let check_right = is_word_byte(nb[nb.len() - 1]);
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let pos = from + rel;
        let end = pos + nb.len();
        let left_ok = !check_left || pos == 0 || !is_word_byte(hb[pos - 1]);
        let right_ok = !check_right || end == hb.len() || !is_word_byte(hb[end]);
        if left_ok && right_ok {
            return true;
        }
        from = pos + haystack[pos..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// URL, inline/fenced-command and path candidates in `text`, deduplicated in first-seen order.
/// `(token, is_command)`: a multi-word inline code span or fenced code line is a command.
fn candidates(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut push = |t: &str, cmd: bool| {
        if !out.iter().any(|(s, _)| s == t) {
            out.push((t.to_string(), cmd));
        }
    };
    for caps in INLINE_CODE.captures_iter(text) {
        let span = caps[1].trim();
        if span.split_whitespace().nth(1).is_some() {
            push(span, true);
        }
    }
    for line in fenced_code_lines(text) {
        if line.split_whitespace().nth(1).is_some() {
            push(line, true);
        }
    }
    for t in words(text) {
        if is_url(t) || (PATHLIKE.is_match(t) && t.chars().any(char::is_alphabetic)) {
            push(t, false);
        }
    }
    out
}

impl Trajectory {
    pub(super) fn skip_message(&mut self) {
        self.idx += 1;
    }

    fn insert_path_and_suffixes(&mut self, token: &str) {
        if self.witness_tokens.len() >= WITNESS_CAP {
            return;
        }
        if !self.witness_tokens.contains(token) {
            self.witness_tokens.insert(token.to_string());
        }
        let trimmed = token
            .trim_start_matches("./")
            .trim_start_matches("~/")
            .trim_start_matches('/')
            .trim_end_matches('/');
        if !trimmed.is_empty() && trimmed != token && self.witness_tokens.len() < WITNESS_CAP {
            self.witness_tokens.insert(trimmed.to_string());
        }
        let mut rest = trimmed;
        while let Some((_, suffix)) = rest.split_once('/') {
            if !suffix.is_empty() && self.witness_tokens.len() < WITNESS_CAP {
                self.witness_tokens.insert(suffix.to_string());
            }
            rest = suffix;
        }
    }

    fn append_corpus_line(&mut self, line: &str) {
        let norm = norm_ws(line);
        if norm.is_empty() || self.witness_corpus.len().saturating_add(norm.len() + 1) > WITNESS_CORPUS_CAP {
            return;
        }
        if !self.witness_corpus.is_empty() {
            self.witness_corpus.push('\n');
        }
        self.witness_corpus.push_str(&norm);
    }

    fn witness(&mut self, text: &str) {
        for caps in INLINE_CODE.captures_iter(text) {
            let norm = norm_ws(caps[1].trim());
            if !norm.is_empty() && self.witness_spans.len() < WITNESS_CAP {
                self.witness_spans.insert(norm);
            }
        }
        for line in fenced_code_lines(text) {
            let norm = norm_ws(line);
            if !norm.is_empty() && self.witness_spans.len() < WITNESS_CAP {
                self.witness_spans.insert(norm);
            }
        }
        for raw_line in text.lines() {
            self.append_corpus_line(raw_line);
        }
        for t in words(text) {
            if self.witness_tokens.len() >= WITNESS_CAP {
                break;
            }
            if t.contains('/') {
                self.insert_path_and_suffixes(t);
            } else if !self.witness_tokens.contains(t) {
                self.witness_tokens.insert(t.to_string());
            }
        }
    }

    fn witnessed(&self, token: &str, command: bool) -> bool {
        if command {
            let norm = norm_ws(token);
            !norm.is_empty()
                && (self.witness_spans.contains(&norm) || contains_contiguous_phrase(&self.witness_corpus, &norm))
        } else {
            self.witness_tokens.contains(token)
                || self.witness_tokens.contains(token.trim_end_matches('/'))
                || self.witness_tokens.contains(token.trim_start_matches("./"))
        }
    }

    fn open_seam(&mut self) {
        let known: HashSet<String> = self.recent_reads.iter().flatten().cloned().collect();
        self.open.push(OpenSeam {
            seam: ChurnSeam {
                session: None,
                index: self.idx,
                turns: 0,
                windows: CHURN_WINDOWS.map(|k| ChurnWindow {
                    k,
                    ..Default::default()
                }),
                churn_rate: 0.0,
                lossy: false,
                reread_paths: Vec::new(),
            },
            known,
            seen: HashSet::new(),
            dup: false,
            calls: HashMap::new(),
            failed: HashSet::new(),
        });
    }

    fn close(seam: OpenSeam) -> ChurnSeam {
        let mut s = seam.seam;
        for w in &mut s.windows {
            w.repeated_failures_and_corrections = w.tool_errors + w.corrections;
        }
        let full = &s.windows[1];
        let events = full.repeated_failures_and_corrections;
        s.churn_rate = events as f64 / full.tool_calls.max(1) as f64;
        s.lossy = events >= LOSSY_MIN_EVENTS && s.churn_rate >= LOSSY_MIN_RATE;
        s
    }

    /// A new assistant turn begins: age the read window and every open seam.
    fn start_turn(&mut self) {
        self.seam_pending_summary = false;
        self.recent_reads.push_back(Vec::new());
        if self.recent_reads.len() > CHURN_TURNS_BEFORE {
            self.recent_reads.pop_front();
        }
        let mut still = Vec::with_capacity(self.open.len());
        for mut seam in std::mem::take(&mut self.open) {
            if seam.seam.turns == CHURN_TURNS_AFTER {
                self.found.churn.push(Self::close(seam));
            } else {
                seam.seam.turns += 1;
                still.push(seam);
            }
        }
        self.open = still;
    }

    fn note_instructions(&mut self, text: &str, index: usize) {
        for (path, body) in instruction_files(text) {
            let words = body.split_whitespace().count();
            let tokens_est = crate::ir::est_str_tokens(body, crate::ir::TokenKind::Text);
            match self.instructions.iter_mut().find(|b| b.path == path) {
                Some(seen) => {
                    seen.loads += 1;
                    if words > seen.words {
                        (seen.words, seen.tokens_est) = (words, tokens_est);
                    }
                }
                None => self.instructions.push(InstructionBloat {
                    session: None,
                    index,
                    path: path.to_string(),
                    words,
                    tokens_est,
                    loads: 1,
                }),
            }
        }
    }

    fn end_spin(&mut self) {
        self.spin = None;
    }

    fn note_spin(&mut self, tool: &str, input: String, index: usize) {
        match &mut self.spin {
            Some((t, i, _, n)) if t == tool && *i == input => {
                *n += 1;
                let n = *n;
                if n == SPIN_MIN_CALLS {
                    let (tool, input, first, _) = self.spin.clone().expect("spin is open");
                    self.found.spins.push(SpinEpisode {
                        session: None,
                        index: first,
                        tool,
                        input,
                        calls: n,
                    });
                } else if n > SPIN_MIN_CALLS {
                    if let Some(last) = self.found.spins.last_mut() {
                        last.calls = n;
                    }
                }
            }
            _ => self.spin = Some((tool.to_string(), input, index, 1)),
        }
    }

    pub(super) fn observe(
        &mut self,
        harness: Option<crate::Harness>,
        turn_key: Option<String>,
        m: &Message,
        resolver: &crate::lazy::Resolver,
    ) -> anyhow::Result<()> {
        let index = self.idx;
        let visible = m.is_model_visible();
        for seam in &mut self.open {
            seam.dup = m.id.as_ref().is_some_and(|id| !seam.seen.insert(format!("m:{id}")));
        }
        if visible && matches!(m.kind, MessageKind::InjectedContext | MessageKind::SystemPrompt) {
            for b in &m.content {
                if let Block::Text { text, .. } = b {
                    let text = text.try_resolve(resolver)?;
                    self.note_instructions(&text, index);
                    self.witness(&text);
                }
            }
        }
        match m.kind {
            MessageKind::CompactionBoundary => {
                self.open_seam();
                self.seam_pending_summary = true;
            }
            MessageKind::CompactionSummary => {
                if !self.seam_pending_summary {
                    self.open_seam();
                }
                self.seam_pending_summary = false;
                let mut text = String::new();
                for b in &m.content {
                    if let Block::Text { text: t, .. } = b {
                        text.push_str(&t.try_resolve(resolver)?);
                        text.push('\n');
                    }
                }
                let found = candidates(&text);
                let unwitnessed: Vec<String> = found
                    .iter()
                    .filter(|(t, cmd)| !self.witnessed(t, *cmd))
                    .map(|(t, _)| t.clone())
                    .collect();
                self.found.contrast.push(SummaryContrast {
                    session: None,
                    index,
                    candidates: found.len(),
                    unwitnessed_count: unwitnessed.len(),
                    unwitnessed: unwitnessed.into_iter().take(UNWITNESSED_SHOWN).collect(),
                });
            }
            _ => {}
        }
        if visible && m.role == Role::User && m.kind == MessageKind::Prompt && m.origin == Origin::Human {
            self.end_spin();
            let mut corrects = false;
            for b in &m.content {
                if let Block::Text { text, .. } = b {
                    let text = text.try_resolve(resolver)?;
                    corrects |= CORRECTION.is_match(&text);
                    self.witness(&text);
                }
            }
            if corrects {
                for seam in self.open.iter_mut().filter(|s| !s.dup) {
                    seam.bump(|w| w.corrections += 1);
                }
            }
        }
        if visible && m.role == Role::Assistant && m.kind != MessageKind::CompactionSummary {
            let key = Some(turn_key);
            if self.turn_key.as_ref() != key.as_ref() || key.as_ref().is_some_and(Option::is_none) {
                self.start_turn();
            }
            self.turn_key = key;
            for b in &m.content {
                match b {
                    Block::Text { text, .. } => {
                        let text = text.try_resolve(resolver)?;
                        self.witness(&text);
                    }
                    Block::ToolUse { id, name, input, .. } => {
                        let call_key = format!("c:{id}");
                        let mut counting = Vec::with_capacity(self.open.len());
                        for seam in &mut self.open {
                            let fresh = !seam.dup && seam.seen.insert(call_key.clone());
                            if fresh {
                                seam.bump(|w| w.tool_calls += 1);
                            }
                            counting.push(fresh);
                        }
                        if counting.iter().any(|&c| c) {
                            let (canonical, _) = input.canonical_prefix(512)?;
                            let key = format!("{name}\0{canonical}");
                            for (seam, _) in self.open.iter_mut().zip(&counting).filter(|(_, c)| **c) {
                                seam.calls.insert(id.clone(), key.clone());
                            }
                        }
                        let mut reads = Vec::new();
                        let mut said = Vec::new();
                        let mut kind = None;
                        input.with_view(|view| {
                            visit_tool_ops(
                                harness,
                                name,
                                view,
                                ClassifyOpts {
                                    command_limit: Some(4096),
                                    ..ClassifyOpts::FULL
                                },
                                |op| match op.op {
                                    ToolOp::FileRead { path } => {
                                        said.push(path.to_string());
                                        reads.push(path.into_owned());
                                        kind.get_or_insert("read");
                                    }
                                    ToolOp::Search { .. } => {
                                        kind.get_or_insert("read");
                                    }
                                    ToolOp::FileEdit { path, .. } => {
                                        said.push(path.into_owned());
                                        kind = Some("edit");
                                    }
                                    ToolOp::Command { cmd, .. } => {
                                        if let Some(cmd) = cmd {
                                            said.push(cmd.into_owned());
                                        }
                                        kind = Some("command");
                                    }
                                    _ => {}
                                },
                            )
                        })?;
                        // The assistant's own call arguments are its words, not tool output.
                        for s in &said {
                            self.witness(s);
                            let norm = norm_ws(s.trim());
                            if !norm.is_empty() && self.witness_spans.len() < WITNESS_CAP {
                                self.witness_spans.insert(norm);
                            }
                        }
                        for (seam, _) in self.open.iter_mut().zip(&counting).filter(|(_, c)| **c) {
                            let evicted: Vec<&String> = reads.iter().filter(|p| seam.known.contains(*p)).collect();
                            for p in evicted {
                                seam.bump(|w| w.eviction_rereads += 1);
                                if !seam.seam.reread_paths.contains(p) {
                                    seam.seam.reread_paths.push(p.clone());
                                }
                            }
                        }
                        if let Some(turn) = self.recent_reads.back_mut() {
                            turn.extend(reads);
                        }
                        match kind {
                            Some("edit" | "command") => self.end_spin(),
                            Some(_) => {
                                let (canonical, _) = input.canonical_prefix(512)?;
                                self.note_spin(name, canonical, index);
                            }
                            None => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        if visible {
            for b in &m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                } = b
                {
                    let err_key = format!("e:{tool_use_id}");
                    for seam in self.open.iter_mut().filter(|s| !s.dup) {
                        if !seam.seen.insert(err_key.clone()) {
                            continue;
                        }
                        let repeated = seam
                            .calls
                            .get(tool_use_id)
                            .is_some_and(|key| !seam.failed.insert(key.clone()));
                        seam.bump(|w| {
                            w.tool_errors += 1;
                            w.repeated_failures += u64::from(repeated);
                        });
                    }
                }
            }
        }
        self.idx += 1;
        Ok(())
    }

    pub(super) fn finish(mut self, session: Option<&str>) -> Findings {
        for seam in std::mem::take(&mut self.open) {
            self.found.churn.push(Self::close(seam));
        }
        self.found.churn.sort_by_key(|s| s.index);
        let session = session.map(str::to_string);
        for s in &mut self.found.churn {
            s.session.clone_from(&session);
        }
        for s in &mut self.found.contrast {
            s.session.clone_from(&session);
        }
        for s in &mut self.found.spins {
            s.session.clone_from(&session);
        }
        self.found.bloat = std::mem::take(&mut self.instructions)
            .into_iter()
            .filter(|b| b.words > INSTRUCTION_BLOAT_WORDS)
            .map(|b| InstructionBloat {
                session: session.clone(),
                ..b
            })
            .collect();
        self.found
    }
}
