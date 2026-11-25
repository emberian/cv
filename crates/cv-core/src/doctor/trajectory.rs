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
const WITNESS_CAP: usize = 32_768;
/// Bounded normalized-whitespace corpus for contiguous command substring/n-gram matching.
const WITNESS_CORPUS_CAP: usize = 512 * 1024;
/// Streaming decode chunk size for `LazyText::try_for_each_chunk`.
const STREAM_CHUNK: usize = 64 * 1024;
/// Maximum UTF-8 bytes retained for one whitespace-delimited token (longer runs are raw blobs).
const MAX_WORD_BYTES: usize = 1024;
/// Maximum UTF-8 bytes buffered for a single line when scanning fences and normalized phrases.
const MAX_LINE_BYTES: usize = 16 * 1024;

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
static CORRECTION_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:i (?:already )?(?:said|told you)|as i said|you forgot|we already|already (?:did|done|fixed|told)|not what i)\b",
    )
    .unwrap()
});
/// Codex's AGENTS.md dump header at the start of an injected block.
static AGENTS_FOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\s*# AGENTS\.md instructions for (\S+)").unwrap());

fn parse_contents_of_header(line_head: &str) -> Option<&str> {
    let rest = line_head.strip_prefix("Contents of ")?;
    let (path, after) = rest.split_once(' ')?;
    (!path.is_empty() && !path.chars().any(char::is_whitespace) && after.starts_with('(')).then_some(path)
}

/// Streaming accumulator for one open instruction-file body (`words` + [`crate::ir::StrTokens`]),
/// stopping cleanly if `</system-reminder>` appears (even split across chunk boundaries).
struct ActiveInstruction {
    path: String,
    words: usize,
    in_word: bool,
    tokens: crate::ir::StrTokens,
    close_on_reminder: bool,
    tail: String,
    closed: bool,
}

impl ActiveInstruction {
    fn new(path: String, close_on_reminder: bool) -> Self {
        Self {
            path,
            words: 0,
            in_word: false,
            tokens: crate::ir::StrTokens::new(crate::ir::TokenKind::Text),
            close_on_reminder,
            tail: String::new(),
            closed: false,
        }
    }

    fn feed_accepted(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.tokens.push(s.as_bytes());
        for c in s.chars() {
            if c.is_whitespace() {
                self.in_word = false;
            } else if !self.in_word {
                self.in_word = true;
                self.words += 1;
            }
        }
    }

    fn push(&mut self, s: &str) {
        const TAG: &str = "</system-reminder>";
        const KEEP: usize = TAG.len() - 1;
        if self.closed || s.is_empty() {
            return;
        }
        if !self.close_on_reminder {
            self.feed_accepted(s);
            return;
        }
        self.tail.push_str(s);
        if let Some(pos) = self.tail.find(TAG) {
            let before = self.tail[..pos].to_string();
            self.tail.clear();
            self.feed_accepted(&before);
            self.closed = true;
            return;
        }
        if self.tail.len() > KEEP {
            let mut safe = self.tail.len() - KEEP;
            while !self.tail.is_char_boundary(safe) {
                safe -= 1;
            }
            let flush = self.tail[..safe].to_string();
            self.tail.drain(..safe);
            self.feed_accepted(&flush);
        }
    }

    fn finish(mut self) -> (String, usize, u64) {
        if !self.closed && !self.tail.is_empty() {
            let rem = std::mem::take(&mut self.tail);
            self.feed_accepted(&rem);
        }
        (self.path, self.words, self.tokens.tokens())
    }
}

/// Bounded streaming scanner for `AGENTS_FOR` and `Contents of <path> (` inside injected context.
struct InstructionScanner {
    agents_probe: Option<String>,
    agents_mode: bool,
    at_line_start: bool,
    line_head: String,
    head_decided: bool,
    skip_header_line: bool,
    active: Option<ActiveInstruction>,
}

impl Default for InstructionScanner {
    fn default() -> Self {
        Self {
            agents_probe: Some(String::new()),
            agents_mode: false,
            at_line_start: true,
            line_head: String::new(),
            head_decided: false,
            skip_header_line: false,
            active: None,
        }
    }
}

impl InstructionScanner {
    fn emit_closed(cur: ActiveInstruction, out: &mut Vec<(String, usize, u64)>) {
        out.push(cur.finish());
    }

    fn push_contents_slice(&mut self, mut s: &str, out: &mut Vec<(String, usize, u64)>) {
        const HEAD_CAP: usize = 512;
        while !s.is_empty() {
            if self.skip_header_line {
                if let Some(nl) = s.find('\n') {
                    self.skip_header_line = false;
                    if let Some(cur) = self.active.as_mut() {
                        cur.push("\n");
                    }
                    self.at_line_start = true;
                    self.head_decided = false;
                    self.line_head.clear();
                    s = &s[nl + 1..];
                } else {
                    return;
                }
                continue;
            }
            if self.at_line_start && !self.head_decided {
                let nl = s.find('\n');
                let limit = nl.unwrap_or(s.len());
                let room = HEAD_CAP.saturating_sub(self.line_head.len());
                let mut take = limit.min(room);
                while !s.is_char_boundary(take) {
                    take -= 1;
                }
                self.line_head.push_str(&s[..take]);
                s = &s[take..];
                if s.starts_with('\n') || self.line_head.len() >= HEAD_CAP {
                    self.head_decided = true;
                    if let Some(path) = parse_contents_of_header(&self.line_head) {
                        let path = path.to_string();
                        if let Some(prev) = self.active.take() {
                            Self::emit_closed(prev, out);
                        }
                        self.active = Some(ActiveInstruction::new(path, true));
                        self.line_head.clear();
                        if s.starts_with('\n') {
                            if let Some(cur) = self.active.as_mut() {
                                cur.push("\n");
                            }
                            s = &s[1..];
                            self.at_line_start = true;
                            self.head_decided = false;
                        } else {
                            self.skip_header_line = true;
                            self.at_line_start = false;
                        }
                    } else {
                        if let Some(cur) = self.active.as_mut() {
                            cur.push(&self.line_head);
                            if cur.closed {
                                if let Some(done) = self.active.take() {
                                    Self::emit_closed(done, out);
                                }
                            }
                        }
                        self.line_head.clear();
                        if s.starts_with('\n') {
                            if let Some(cur) = self.active.as_mut() {
                                cur.push("\n");
                                if cur.closed {
                                    if let Some(done) = self.active.take() {
                                        Self::emit_closed(done, out);
                                    }
                                }
                            }
                            s = &s[1..];
                            self.at_line_start = true;
                            self.head_decided = false;
                        } else {
                            self.at_line_start = false;
                        }
                    }
                }
                continue;
            }
            // Mid-line after `line_head` was decided as non-header.
            let (seg, after_nl) = match s.find('\n') {
                Some(nl) => (&s[..nl + 1], &s[nl + 1..]),
                None => (s, ""),
            };
            if let Some(cur) = self.active.as_mut() {
                cur.push(seg);
                if cur.closed {
                    if let Some(done) = self.active.take() {
                        Self::emit_closed(done, out);
                    }
                }
            }
            if seg.ends_with('\n') {
                self.at_line_start = true;
                self.head_decided = false;
            }
            s = after_nl;
        }
    }

    fn push_chunk(&mut self, chunk: &str, out: &mut Vec<(String, usize, u64)>) {
        const PROBE_CAP: usize = 1024;
        if self.agents_mode {
            if let Some(cur) = self.active.as_mut() {
                cur.push(chunk);
            }
            return;
        }
        if let Some(mut probe) = self.agents_probe.take() {
            if probe.is_empty() && chunk.trim_start().is_empty() {
                // Keep skipping pure leading whitespace without growing `probe` past 1 byte.
                self.agents_probe = Some(String::from(" "));
                self.push_contents_slice(chunk, out);
                return;
            }
            let room = PROBE_CAP.saturating_sub(probe.len());
            let mut take = chunk.len().min(room);
            while !chunk.is_char_boundary(take) {
                take -= 1;
            }
            probe.push_str(&chunk[..take]);
            let rest = &chunk[take..];
            let trimmed = probe.trim_start();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                // Cannot be `# AGENTS.md instructions for ...`; continue in `Contents of` mode.
                self.push_contents_slice(chunk, out);
                return;
            }
            if let Some(caps) = AGENTS_FOR.captures(&probe) {
                // Ensure the `(\S+)` capture isn't truncated right at the chunk boundary.
                let m_end = caps.get(0).map_or(0, |m| m.end());
                if m_end < probe.len() || rest.is_empty() && chunk.len() < room {
                    let path = caps.get(1).map_or("AGENTS.md", |m| m.as_str()).to_string();
                    self.agents_mode = true;
                    out.clear();
                    let mut cur = ActiveInstruction::new(path, false);
                    cur.push(&probe[m_end..]);
                    cur.push(rest);
                    self.active = Some(cur);
                    return;
                }
            }
            if probe.len() < PROBE_CAP && rest.is_empty() {
                // Need next chunk (or `finish`) to decide `AGENTS_FOR`.
                self.agents_probe = Some(probe);
                return;
            }
            // Exceeded `PROBE_CAP` without matching `AGENTS_FOR`: replay `probe` + `rest` as `Contents of`.
            self.push_contents_slice(&probe, out);
            self.push_contents_slice(rest, out);
            return;
        }
        self.push_contents_slice(chunk, out);
    }

    fn finish(mut self, out: &mut Vec<(String, usize, u64)>) {
        if let Some(probe) = self.agents_probe.take() {
            if probe.trim().is_empty() {
                return;
            }
            if let Some(caps) = AGENTS_FOR.captures(&probe) {
                let path = caps.get(1).map_or("AGENTS.md", |m| m.as_str()).to_string();
                let m_end = caps.get(0).map_or(0, |m| m.end());
                let mut cur = ActiveInstruction::new(path, false);
                cur.push(&probe[m_end..]);
                Self::emit_closed(cur, out);
                return;
            }
            self.push_contents_slice(&probe, out);
        }
        if !self.skip_header_line && self.at_line_start && !self.head_decided && !self.line_head.is_empty() {
            if let Some(path) = parse_contents_of_header(&self.line_head) {
                if let Some(prev) = self.active.take() {
                    Self::emit_closed(prev, out);
                }
                // Header line at EOF with no trailing newline has an empty body.
                out.push((path.to_string(), 0, 0));
                return;
            } else if let Some(cur) = self.active.as_mut() {
                cur.push(&self.line_head);
            }
        } else if self.skip_header_line {
            // Header line overflowed `HEAD_CAP` and reached EOF with no `\n`: body is empty.
            if let Some(cur) = self.active.take() {
                out.push((cur.path, 0, 0));
            }
            return;
        }
        if let Some(cur) = self.active.take() {
            Self::emit_closed(cur, out);
        }
    }
}

fn trim_token(t: &str) -> &str {
    t.trim_matches(|c: char| "\"'()[]{}<>,;:!?`*".contains(c))
        .trim_end_matches('.')
}

fn is_url(t: &str) -> bool {
    t.starts_with("http://") || t.starts_with("https://")
}

fn norm_ws(s: &str) -> String {
    let mut out = String::new();
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Advance CommonMark fenced-code state for one raw line, returning `Some(trimmed_code_line)` when
/// inside an open fence and `2 <= line.len() <= 200` (non-comment).
fn step_fence<'a>(open: &mut Option<(u8, usize)>, raw: &'a str) -> Option<&'a str> {
    let trimmed = raw.trim_start_matches([' ', '\t', '>']);
    if let Some(ch @ (b'`' | b'~')) = trimmed.as_bytes().first().copied() {
        let run = trimmed.bytes().take_while(|&b| b == ch).count();
        if run >= 3 {
            let rest = &trimmed[run..];
            if let Some((open_ch, open_len)) = *open {
                if ch == open_ch && run >= open_len && rest.trim().is_empty() {
                    *open = None;
                    return None;
                }
            } else if ch != b'`' || !rest.contains('`') {
                *open = Some((ch, run));
                return None;
            }
        }
    }
    if open.is_some() {
        let line = raw.trim().trim_start_matches("$ ").trim();
        if line.len() >= 2 && line.len() <= 200 && !line.starts_with('#') && !line.starts_with("//") {
            return Some(line);
        }
    }
    None
}

enum ScanEvent<'a> {
    Word(&'a str),
    InlineSpan(&'a str),
    Line { raw: &'a str, fenced: Option<&'a str> },
}

/// Bounded streaming scanner for words (`<= MAX_WORD_BYTES`), inline `` `...` `` code spans
/// (`2..=200` bytes), fenced code lines (`<= 200` bytes), and normalized lines (`<= MAX_LINE_BYTES`).
#[derive(Default)]
struct StreamScanner {
    word: String,
    word_overflow: bool,
    tick: Option<Option<String>>,
    line: String,
    line_overflow: bool,
    fence: Option<(u8, usize)>,
}

impl StreamScanner {
    fn feed_chunk(&mut self, chunk: &str, mut on_event: impl FnMut(ScanEvent<'_>)) {
        for c in chunk.chars() {
            if c.is_whitespace() {
                if !self.word_overflow && !self.word.is_empty() {
                    let t = trim_token(&self.word);
                    if t.len() >= 2 {
                        on_event(ScanEvent::Word(t));
                    }
                }
                self.word.clear();
                self.word_overflow = false;
            } else if !self.word_overflow {
                if self.word.len() + c.len_utf8() <= MAX_WORD_BYTES {
                    self.word.push(c);
                } else {
                    self.word.clear();
                    self.word_overflow = true;
                }
            }

            match (&mut self.tick, c) {
                (_, '\n') => self.tick = None,
                (None, '`') => self.tick = Some(Some(String::new())),
                (Some(Some(buf)), '`') => {
                    if buf.len() >= 2 {
                        on_event(ScanEvent::InlineSpan(buf));
                        self.tick = None;
                    } else {
                        buf.clear();
                    }
                }
                (Some(None), '`') => self.tick = Some(Some(String::new())),
                (Some(Some(buf)), ch) => {
                    if buf.len() + ch.len_utf8() <= 200 {
                        buf.push(ch);
                    } else {
                        self.tick = Some(None);
                    }
                }
                _ => {}
            }

            if c == '\n' {
                if !self.line_overflow {
                    let raw = self.line.strip_suffix('\r').unwrap_or(&self.line);
                    let fenced = step_fence(&mut self.fence, raw);
                    on_event(ScanEvent::Line { raw, fenced });
                }
                self.line.clear();
                self.line_overflow = false;
            } else if !self.line_overflow {
                if self.line.len() + c.len_utf8() <= MAX_LINE_BYTES {
                    self.line.push(c);
                } else {
                    self.line.clear();
                    self.line_overflow = true;
                }
            }
        }
    }

    fn finish(mut self, mut on_event: impl FnMut(ScanEvent<'_>)) {
        if !self.word_overflow && !self.word.is_empty() {
            let t = trim_token(&self.word);
            if t.len() >= 2 {
                on_event(ScanEvent::Word(t));
            }
        }
        if !self.line_overflow && !self.line.is_empty() {
            let fenced = step_fence(&mut self.fence, &self.line);
            on_event(ScanEvent::Line {
                raw: &self.line,
                fenced,
            });
        }
    }
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

/// Streaming URL, inline/fenced-command and path candidate collector over summary blocks.
#[derive(Default)]
struct SummaryCandidateCollector {
    inline: Vec<String>,
    fenced: Vec<String>,
    urls_paths: Vec<String>,
}

impl SummaryCandidateCollector {
    fn handle(&mut self, ev: ScanEvent<'_>) {
        match ev {
            ScanEvent::Word(t) => {
                if is_url(t) || (PATHLIKE.is_match(t) && t.chars().any(char::is_alphabetic)) {
                    self.urls_paths.push(t.to_string());
                }
            }
            ScanEvent::InlineSpan(span) => {
                let s = span.trim();
                if s.split_whitespace().nth(1).is_some() {
                    self.inline.push(s.to_string());
                }
            }
            ScanEvent::Line { fenced, .. } => {
                if let Some(line) = fenced.filter(|l| l.split_whitespace().nth(1).is_some()) {
                    self.fenced.push(line.to_string());
                }
            }
        }
    }

    fn feed_text(&mut self, text: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> anyhow::Result<()> {
        let mut scan = StreamScanner::default();
        text.try_for_each_chunk(resolver, STREAM_CHUNK, |chunk| {
            scan.feed_chunk(chunk, |ev| self.handle(ev));
            Ok(())
        })?;
        scan.finish(|ev| self.handle(ev));
        Ok(())
    }

    fn finish(self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = Vec::new();
        let mut push = |t: String, cmd: bool| {
            if !out.iter().any(|(s, _)| s == &t) {
                out.push((t, cmd));
            }
        };
        for s in self.inline {
            push(s, true);
        }
        for l in self.fenced {
            push(l, true);
        }
        for u in self.urls_paths {
            push(u, false);
        }
        out
    }
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
        if self.witness_corpus.len() >= WITNESS_CORPUS_CAP {
            return;
        }
        let mut norm_len = 0usize;
        for w in line.split_whitespace() {
            norm_len += w.len() + usize::from(norm_len > 0);
        }
        if norm_len == 0 {
            return;
        }
        let sep = usize::from(!self.witness_corpus.is_empty());
        let needed = self.witness_corpus.len().saturating_add(sep + norm_len);
        if needed > WITNESS_CORPUS_CAP {
            return;
        }
        if self.witness_corpus.capacity() < needed {
            let target = needed.next_power_of_two().min(WITNESS_CORPUS_CAP);
            self.witness_corpus.reserve_exact(target - self.witness_corpus.len());
        }
        if sep > 0 {
            self.witness_corpus.push('\n');
        }
        for (i, w) in line.split_whitespace().enumerate() {
            if i > 0 {
                self.witness_corpus.push(' ');
            }
            self.witness_corpus.push_str(w);
        }
    }

    fn witness_word(&mut self, t: &str) {
        if self.witness_tokens.len() >= WITNESS_CAP {
            return;
        }
        if t.contains('/') {
            self.insert_path_and_suffixes(t);
        } else if !self.witness_tokens.contains(t) {
            self.witness_tokens.insert(t.to_string());
        }
    }

    fn witness_span(&mut self, raw: &str) {
        if self.witness_spans.len() >= WITNESS_CAP {
            return;
        }
        let norm = norm_ws(raw.trim());
        if !norm.is_empty() && !self.witness_spans.contains(&norm) {
            self.witness_spans.insert(norm);
        }
    }

    fn handle_scan_event(&mut self, ev: ScanEvent<'_>) {
        match ev {
            ScanEvent::Word(t) => self.witness_word(t),
            ScanEvent::InlineSpan(span) => self.witness_span(span),
            ScanEvent::Line { raw, fenced } => {
                if let Some(line) = fenced {
                    self.witness_span(line);
                }
                self.append_corpus_line(raw);
            }
        }
    }

    fn witness(&mut self, text: &str) {
        let mut scan = StreamScanner::default();
        scan.feed_chunk(text, |ev| self.handle_scan_event(ev));
        scan.finish(|ev| self.handle_scan_event(ev));
    }

    fn witness_lazy(&mut self, text: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> anyhow::Result<()> {
        let mut scan = StreamScanner::default();
        text.try_for_each_chunk(resolver, STREAM_CHUNK, |chunk| {
            scan.feed_chunk(chunk, |ev| self.handle_scan_event(ev));
            Ok(())
        })?;
        scan.finish(|ev| self.handle_scan_event(ev));
        Ok(())
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

    fn record_instruction_load(&mut self, path: String, words: usize, tokens_est: u64, index: usize) {
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
                path,
                words,
                tokens_est,
                loads: 1,
            }),
        }
    }

    fn observe_injected_lazy(
        &mut self,
        text: &crate::lazy::Text,
        resolver: &crate::lazy::Resolver,
        index: usize,
    ) -> anyhow::Result<()> {
        let mut instr = InstructionScanner::default();
        let mut loaded = Vec::new();
        let mut scan = StreamScanner::default();
        text.try_for_each_chunk(resolver, STREAM_CHUNK, |chunk| {
            instr.push_chunk(chunk, &mut loaded);
            scan.feed_chunk(chunk, |ev| self.handle_scan_event(ev));
            Ok(())
        })?;
        instr.finish(&mut loaded);
        scan.finish(|ev| self.handle_scan_event(ev));
        for (path, words, tokens_est) in loaded {
            self.record_instruction_load(path, words, tokens_est, index);
        }
        Ok(())
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
        let mut scan = StreamScanner::default();
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
            scan.feed_chunk(chunk, |ev| self.handle_scan_event(ev));
            Ok(())
        })?;
        scan.finish(|ev| self.handle_scan_event(ev));
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
        if visible && matches!(m.kind, MessageKind::InjectedContext | MessageKind::SystemPrompt) {
            for b in &m.content {
                if let Block::Text { text, .. } = b {
                    self.observe_injected_lazy(text, resolver, index)?;
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
                let mut collector = SummaryCandidateCollector::default();
                for b in &m.content {
                    if let Block::Text { text: t, .. } = b {
                        collector.feed_text(t, resolver)?;
                    }
                }
                let found = collector.finish();
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
                match b {
                    Block::Text { text, .. } => {
                        self.witness_lazy(text, resolver)?;
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
