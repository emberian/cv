//! Trajectory sensors folded in the same streamed doctor pass: what happened right after each
//! compaction (`post_compaction_churn`) and repeated identical reads (`spin_episodes`).
//!
//! Deterministic counts over message order. A turn is one assistant response: consecutive assistant
//! records sharing a response key ([`crate::stats::usage_key`]) are one turn.

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
/// Retained for schema compatibility with `doctor` reports.
pub const INSTRUCTION_BLOAT_WORDS: usize = 600;
/// Identical consecutive read/search calls that make a spin.
pub const SPIN_MIN_CALLS: usize = 4;
/// Streaming decode chunk size for `LazyText::try_for_each_chunk`.
const STREAM_CHUNK: usize = 64 * 1024;

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
    spin: Option<(String, String, usize, usize)>,
    found: Findings,
}

/// A human prompt that pushes back on what the agent just did.
static CORRECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^\W*(?:no\b|nope\b|stop\b|wrong\b|undo\b|revert\b|don'?t\b|do not\b|that'?s not\b|not that\b)",
        r"|(?i)\b(?:i (?:already )?(?:said|told you)|as i said|you forgot|we already|already (?:did|done|fixed|told)|not what i)\b"
    ))
    .unwrap()
});
static CORRECTION_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:i (?:already )?(?:said|told you)|as i said|you forgot|we already|already (?:did|done|fixed|told)|not what i)\b",
    )
    .unwrap()
});

impl Trajectory {
    pub(super) fn skip_message(&mut self) {
        self.idx += 1;
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

    fn observe_prompt_lazy(
        &mut self,
        text: &crate::lazy::Text,
        resolver: &crate::lazy::Resolver,
    ) -> anyhow::Result<bool> {
        const OVERLAP: usize = 64;
        let mut corrects = false;
        let mut all_nonword = true;
        let mut tail = String::new();
        let mut window = String::new();
        text.try_for_each_chunk(resolver, STREAM_CHUNK, |chunk| {
            if !corrects && !chunk.is_empty() {
                window.clear();
                window.push_str(&tail);
                window.push_str(chunk);
                corrects = if all_nonword {
                    CORRECTION.is_match(&window)
                } else {
                    CORRECTION_BODY.is_match(&window)
                };
                if all_nonword && chunk.chars().any(|c| c.is_alphanumeric() || c == '_') {
                    all_nonword = false;
                }
                tail.clear();
                if !corrects {
                    let mut start = chunk.len().saturating_sub(OVERLAP);
                    while !chunk.is_char_boundary(start) {
                        start += 1;
                    }
                    tail.push_str(&chunk[start..]);
                }
            }
            Ok(())
        })?;
        Ok(corrects)
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
            }
            _ => {}
        }
        if visible && m.role == Role::User && m.kind == MessageKind::Prompt && m.origin == Origin::Human {
            self.end_spin();
            let mut corrects = false;
            for b in &m.content {
                if let Block::Text { text, .. } = b {
                    corrects |= self.observe_prompt_lazy(text, resolver)?;
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
                if let Block::ToolUse { id, name, input, .. } = b {
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
                    let mut kind = None;
                    input.with_view(|view| {
                        visit_tool_ops(
                            harness,
                            name,
                            view,
                            ClassifyOpts {
                                command_limit: Some(512),
                                ..ClassifyOpts::FULL
                            },
                            |op| match op.op {
                                ToolOp::FileRead { path } => {
                                    reads.push(path.into_owned());
                                    kind.get_or_insert("read");
                                }
                                ToolOp::Search { .. } => {
                                    kind.get_or_insert("read");
                                }
                                ToolOp::FileEdit { .. } => {
                                    kind = Some("edit");
                                }
                                ToolOp::Command { .. } => {
                                    kind = Some("command");
                                }
                                _ => {}
                            },
                        )
                    })?;
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
        for s in &mut self.found.spins {
            s.session.clone_from(&session);
        }
        self.found
    }
}
