//! Trajectory sensors folded in the same streamed doctor pass: what happened right after each
//! compaction (`post_compaction_churn`), what a compaction summary says that nobody said before
//! it (`summary_provenance_contrast`), and repeated identical reads (`spin_episodes`).
//!
//! All three are deterministic counts over message order. A turn is one assistant response:
//! consecutive assistant records sharing a response key ([`crate::stats::usage_key`]) are one turn.

use crate::ir::{Block, Message, MessageKind, Origin, Role};
use crate::tools::classify::{visit_tool_ops, ClassifyOpts, ToolOp};
use regex::Regex;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;

/// Turns inspected after each compaction seam.
pub const CHURN_TURNS_AFTER: usize = 6;
/// Pre-compaction turns whose reads count as already known.
pub const CHURN_TURNS_BEFORE: usize = 12;
/// A seam is lossy when at least this many re-reads and errors follow it…
pub const LOSSY_MIN_EVENTS: u64 = 2;
/// …and they make up at least this share of the window's tool calls.
pub const LOSSY_MIN_RATE: f64 = 0.5;
/// Identical consecutive read/search calls that make a spin.
pub const SPIN_MIN_CALLS: usize = 4;
/// Unwitnessed tokens listed per summary (the count is exact).
const UNWITNESSED_SHOWN: usize = 12;
/// Witness vocabulary cap; past it, further tokens are not recorded (biasing toward flagging).
const WITNESS_CAP: usize = 500_000;

/// What followed one compaction seam.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChurnSeam {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Message index of the seam (the boundary, or the summary when no boundary preceded it).
    pub index: usize,
    /// Turns inspected (at most [`CHURN_TURNS_AFTER`]; fewer when the session ends).
    pub turns: usize,
    pub tool_calls: u64,
    /// Reads of files already read in the [`CHURN_TURNS_BEFORE`] turns before the seam.
    pub rereads: u64,
    pub tool_errors: u64,
    /// `(rereads + tool_errors) / max(tool_calls, 1)`.
    pub churn_rate: f64,
    pub lossy: bool,
    /// The re-read paths, first occurrence order.
    pub reread_paths: Vec<String>,
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
}

struct OpenSeam {
    seam: ChurnSeam,
    known: HashSet<String>,
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
    spin: Option<(String, String, usize, usize)>,
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

/// URL, inline-command and path candidates in `text`, deduplicated in first-seen order.
/// `(token, is_command)`: a multi-word inline code span is a command.
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
    for t in words(text) {
        if is_url(t) || (PATHLIKE.is_match(t) && t.chars().any(char::is_alphabetic)) {
            push(t, false);
        }
    }
    out
}

impl Trajectory {
    fn witness(&mut self, text: &str) {
        for caps in INLINE_CODE.captures_iter(text) {
            if self.witness_spans.len() < WITNESS_CAP {
                self.witness_spans.insert(caps[1].trim().to_string());
            }
        }
        for t in words(text) {
            if self.witness_tokens.len() >= WITNESS_CAP {
                break;
            }
            if !self.witness_tokens.contains(t) {
                self.witness_tokens.insert(t.to_string());
            }
        }
    }

    fn witnessed(&self, token: &str, command: bool) -> bool {
        if command {
            self.witness_spans.contains(token) || words(token).all(|w| self.witness_tokens.contains(w))
        } else {
            self.witness_tokens.contains(token)
        }
    }

    fn open_seam(&mut self) {
        let known: HashSet<String> = self.recent_reads.iter().flatten().cloned().collect();
        self.open.push(OpenSeam {
            seam: ChurnSeam {
                session: None,
                index: self.idx,
                turns: 0,
                tool_calls: 0,
                rereads: 0,
                tool_errors: 0,
                churn_rate: 0.0,
                lossy: false,
                reread_paths: Vec::new(),
            },
            known,
        });
    }

    fn close(seam: OpenSeam) -> ChurnSeam {
        let mut s = seam.seam;
        let events = s.rereads + s.tool_errors;
        s.churn_rate = events as f64 / s.tool_calls.max(1) as f64;
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
            for b in &m.content {
                if let Block::Text { text, .. } = b {
                    let text = text.try_resolve(resolver)?;
                    self.witness(&text);
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
                    Block::ToolUse { name, input, .. } => {
                        for seam in &mut self.open {
                            seam.seam.tool_calls += 1;
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
                                    ToolOp::Command { cmd: Some(cmd), .. } => said.push(cmd.into_owned()),
                                    _ => {}
                                },
                            )
                        })?;
                        // The assistant's own call arguments are its words, not tool output.
                        for s in &said {
                            self.witness(s);
                            if self.witness_spans.len() < WITNESS_CAP {
                                self.witness_spans.insert(s.trim().to_string());
                            }
                        }
                        for seam in &mut self.open {
                            for p in reads.iter().filter(|p| seam.known.contains(*p)) {
                                seam.seam.rereads += 1;
                                if !seam.seam.reread_paths.contains(p) {
                                    seam.seam.reread_paths.push(p.clone());
                                }
                            }
                        }
                        if let Some(turn) = self.recent_reads.back_mut() {
                            turn.extend(reads);
                        }
                        match kind {
                            Some("edit") => self.end_spin(),
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
                if let Block::ToolResult { is_error: true, .. } = b {
                    for seam in &mut self.open {
                        seam.seam.tool_errors += 1;
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
        self.found
    }
}
