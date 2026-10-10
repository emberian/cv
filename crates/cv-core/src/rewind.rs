//! `cv rewind` — reconstruct an agent session **as of a past moment** as a new, resumable session.
//!
//! The use case is the keeper reviewer: agent Y changes code that an earlier session X last wrote,
//! and the best reviewer of Y's change is X itself — not X as it is now (it has moved on, compacted
//! many times, maybe finished), but X *at the moment it landed that code*. A rewind derives that
//! agent: the source's records up to and including a cut, rewritten into a new session id, so
//! `claude --resume <new-id> --fork-session` wakes up as "the agent that just did this".
//!
//! ## What the derived session holds
//!
//! Like [`crate::prune`], a rewind works on the **raw JSONL records**, never the IR: every
//! Claude-specific field survives, and lines that need no rewrite stay byte-identical. The derived
//! file is the contiguous record range `[start, cut]` of the source:
//!
//! * **cut** — a message index (the `cv show --range` counting), or a commit sha resolved to the
//!   tool result that *shows the commit being made* (see [Commit evidence](#commit-evidence)). If
//!   the cut leaves tool calls open (parallel calls whose results land on later records), the copy
//!   runs on until they close, stopping at the next assistant turn or prompt — the API rejects a
//!   `tool_use` with no result, and those results happened at the same moment.
//! * **start** — by default the last compaction boundary at or before the cut. Claude Code resumes
//!   from the last boundary anyway, so this is exactly the context the agent had when it made the
//!   cut, and it keeps a month-long, 900 MB session down to one window. A boundary written by
//!   partial compaction names a `preservedSegment` whose head sits *before* the boundary (recent
//!   messages kept verbatim across the compaction); the start moves back to that head so the
//!   preserved messages come along (their relinking is Claude Code's, inferred from the record
//!   shape — the head's `parentUuid` is left as recorded). `--full` starts at the first record.
//!
//! Every record's `sessionId`/`session_id` becomes the new id; `parentUuid` chains are untouched
//! (a boundary record already roots its window with a null parent). The source is only read.
//!
//! ## Sub-agent transcripts
//!
//! Most keepers are sub-agents: `<parent>/subagents/agent-<id>.jsonl`, whose records carry the
//! *parent's* `sessionId` and `isSidechain: true`, so Claude Code won't resume them directly. A
//! rewind of one emits a standalone top-level session: own session id, `isSidechain: false`, the
//! `agentId` tag dropped (top-level records don't carry it), written to the parent's project dir.
//!
//! ## Provenance
//!
//! Next to the derived `<new-id>.jsonl` goes `<new-id>.rewind.json`: the source (id, path, bytes,
//! sha256 of those bytes), the cut and start (message index, byte offset, 1-based line), the
//! commit evidence when the cut came from a sha, how much of the source was left out after the
//! cut, when, and by which cv. A transcript keeps growing while it's read; the size is stat'ed
//! once up front and every pass reads only that prefix, so the hash and the cut agree.
//!
//! ## Commit evidence
//!
//! "Which session made commit `abc1234`?" has an exact answer when the session ran the commit
//! through a tool: the command's output names the sha (`[main abc1234] …`, a pushed range
//! `def5678..abc1234`, or a `git rev-parse HEAD`). [`CommitEvidenceSink`] watches the stream for a
//! shell tool call whose command runs a commit-creating git verb (or `git push`) and whose result
//! contains the sha as a whole hex token (prefix-compatible either way, ≥ 7 digits). It's
//! harness-agnostic (any adapter's tool calls) and is what `cv blame` uses to rank an exact match
//! above its time-window heuristic. Limits: a commit made outside any tool call (a human, a hook)
//! or whose output was silenced (`-q` with no rev-parse) leaves no evidence; a rebase/cherry-pick
//! mints a new sha the session never printed.

use crate::ir::{Block, Harness, Message, MessageKind, SessionRef};
use crate::lazy::Resolver;
use crate::offsets::OFFSET_KEY;
use crate::prune::{is_sidechain, new_uuid, stamp_session_id, turn_kind};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Shortest sha prefix accepted (git's own default abbreviation).
pub const MIN_SHA_LEN: usize = 7;
/// Sidecar suffix for the provenance record next to a derived `<new-id>.jsonl`.
pub const PROVENANCE_SUFFIX: &str = ".rewind.json";

// ---------- commit evidence ----------

/// What a matching tool call did with the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// The command ran a commit-creating git verb (`commit`, `merge`, `cherry-pick`, `revert`)
    /// and its output names the sha: the commit was made here.
    Created,
    /// Only `git push` named it: this session published the commit (it may have been made elsewhere).
    Pushed,
}

impl EvidenceKind {
    /// The label `cv blame` prints.
    pub fn label(self) -> &'static str {
        match self {
            EvidenceKind::Created => "exact: commit created here",
            EvidenceKind::Pushed => "exact: commit pushed here",
        }
    }
}

/// One tool result that shows a commit sha being made (or pushed).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitEvidence {
    /// The queried sha this evidence matched (as the caller gave it, lowercased).
    pub sha: String,
    pub kind: EvidenceKind,
    /// Message index (stream order — the `cv show --range` counting) of the tool RESULT.
    pub msg_idx: usize,
    /// Unix seconds of the result message, when recorded.
    pub ts: Option<i64>,
    pub tool_use_id: String,
    /// The command that ran (capped for display).
    pub command: String,
    /// Source byte offset of the result's record (claude/codex JSONL only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<u64>,
}

/// Normalize a caller sha: trimmed, lowercased, 7–64 hex digits.
pub fn normalize_sha(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    (s.len() >= MIN_SHA_LEN && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then_some(s)
}

/// Whether `output` names `sha` as a whole hex token: a maximal run of hex digits, ≥ 7 long, that
/// is a prefix of the sha or has the sha as its prefix (`[main 63cef47]` names `63cef473b7…`, and
/// a full 40-digit `rev-parse` names a 7-digit query). `sha` must be normalized.
pub fn output_names_sha(output: &str, sha: &str) -> bool {
    output
        .split(|c: char| !c.is_ascii_hexdigit())
        .filter(|t| t.len() >= MIN_SHA_LEN)
        .any(|t| {
            let n = t.len().min(sha.len());
            t[..n].eq_ignore_ascii_case(&sha[..n])
        })
}

/// The commit-relevant git verbs a shell command runs: `Created` when any is commit-creating,
/// else `Pushed` for a push, else `None`. Global options between `git` and the verb (`-C <dir>`,
/// `-c k=v`, `--no-pager`, …) are skipped.
pub fn git_evidence_kind(cmd: &str) -> Option<EvidenceKind> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:^|[^\w.-])git(?:\s+(?:-[Cc]\s+\S+|--?[\w-]+(?:=\S+)?))*\s+(commit|merge|cherry-pick|revert|push)(?:$|[\s;&|)])",
        )
        .expect("static regex")
    });
    let mut kind = None;
    for c in re.captures_iter(cmd) {
        match &c[1] {
            "push" => kind = kind.or(Some(EvidenceKind::Pushed)),
            _ => return Some(EvidenceKind::Created),
        }
    }
    kind
}

/// A [`MessageSink`] that collects [`CommitEvidence`] for a set of shas as the transcript streams
/// past. Remembers only the git-verb tool calls it has seen (id → command), never content.
pub struct CommitEvidenceSink {
    shas: Vec<String>,
    git_calls: HashMap<String, (EvidenceKind, String)>,
    msg_idx: usize,
    resolver: Option<Resolver>,
    /// Stop the stream at the first `Created` match (a rewind wants the landing moment, not
    /// every later mention).
    stop_on_created: bool,
    pub found: Vec<CommitEvidence>,
}

impl CommitEvidenceSink {
    /// `shas` must be normalized ([`normalize_sha`]). `source` resolves lazy result bodies.
    pub fn new(shas: Vec<String>, source: Option<PathBuf>) -> Self {
        CommitEvidenceSink {
            shas,
            git_calls: HashMap::new(),
            msg_idx: 0,
            resolver: source.map(|p| Resolver::new(Some(p))),
            stop_on_created: false,
            found: Vec::new(),
        }
    }

    /// Observe one message at stream index `idx`; returns whether a `Created` match landed.
    fn observe(&mut self, m: &Message, idx: usize) -> bool {
        let mut created = false;
        for b in &m.content {
            match b {
                Block::ToolUse { id, input, .. } => {
                    if let Some(cmd) = crate::events::command_of(input) {
                        if let Some(kind) = git_evidence_kind(&cmd) {
                            self.git_calls.insert(id.clone(), (kind, cmd));
                        }
                    }
                }
                Block::ToolResult {
                    tool_use_id, content, ..
                } => {
                    let Some((kind, cmd)) = self.git_calls.get(tool_use_id) else {
                        continue;
                    };
                    let text = match &self.resolver {
                        Some(r) => content.resolve(r),
                        None => std::borrow::Cow::Borrowed(content.inline_str().unwrap_or("")),
                    };
                    for sha in &self.shas {
                        if output_names_sha(&text, sha) {
                            created |= *kind == EvidenceKind::Created;
                            self.found.push(CommitEvidence {
                                sha: sha.clone(),
                                kind: *kind,
                                msg_idx: idx,
                                ts: m.timestamp.map(|t| t.timestamp()),
                                tool_use_id: tool_use_id.clone(),
                                command: crate::ir::truncate(cmd, 400),
                                byte_offset: m.extra.get(OFFSET_KEY).and_then(Value::as_u64),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        created
    }

    /// The best evidence for `sha`: its first `Created` match, else its first `Pushed` one.
    pub fn best(found: &[CommitEvidence], sha: &str) -> Option<CommitEvidence> {
        let of = |k| found.iter().find(|e| e.sha == sha && e.kind == k);
        of(EvidenceKind::Created).or_else(|| of(EvidenceKind::Pushed)).cloned()
    }
}

impl MessageSink for CommitEvidenceSink {
    fn message(&mut self, m: Message) -> Flow {
        let idx = self.msg_idx;
        self.msg_idx += 1;
        if self.observe(&m, idx) && self.stop_on_created {
            return Flow::Stop;
        }
        Flow::Continue
    }
}

/// Every [`CommitEvidence`] for `shas` in one session. A cheap byte prefilter first (does the file
/// mention any sha's 7-digit head at all?) skips the parse for the sessions that can't match — the
/// common case when `cv blame` checks its candidates. Unnormalizable shas are ignored.
pub fn commit_evidence(r: &SessionRef, shas: &[String]) -> Result<Vec<CommitEvidence>> {
    let mut want: Vec<String> = shas.iter().filter_map(|s| normalize_sha(s)).collect();
    want.dedup();
    let textual = matches!(r.path.extension().and_then(|e| e.to_str()), Some("jsonl" | "json"));
    if textual && r.path.is_file() {
        let heads: Vec<&str> = want.iter().map(|s| &s[..MIN_SHA_LEN]).collect();
        let present = file_mentions(&r.path, &heads)?;
        want = want.into_iter().zip(present).filter(|(_, p)| *p).map(|(s, _)| s).collect();
    }
    if want.is_empty() {
        return Ok(Vec::new());
    }
    let adapter = crate::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    let mut sink = CommitEvidenceSink::new(want, Some(r.path.clone()));
    adapter.stream(r, &ParseOptions::lazy(), &mut sink)?;
    Ok(sink.found)
}

/// Which of `needles` (ASCII, case-insensitive) occur anywhere in the file, by one chunked read —
/// the file is never held whole. Chunks overlap by the longest needle so a match can't straddle.
fn file_mentions(path: &Path, needles: &[&str]) -> Result<Vec<bool>> {
    let mut hit = vec![false; needles.len()];
    if needles.is_empty() {
        return Ok(hit);
    }
    let alts: Vec<String> = needles.iter().map(|n| regex::escape(n)).collect();
    let re = regex::bytes::Regex::new(&format!("(?i){}", alts.join("|"))).context("sha prefilter")?;
    let keep = needles.iter().map(|n| n.len()).max().unwrap_or(0).saturating_sub(1);
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf: Vec<u8> = Vec::with_capacity(4 << 20);
    let mut chunk = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        for m in re.find_iter(&buf) {
            let s = m.as_bytes();
            for (i, nd) in needles.iter().enumerate() {
                hit[i] |= s.eq_ignore_ascii_case(nd.as_bytes());
            }
        }
        if hit.iter().all(|h| *h) {
            break;
        }
        let tail = buf.len().saturating_sub(keep);
        buf.drain(..tail);
    }
    Ok(hit)
}

// ---------- rewind ----------

/// Where the derived session ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutAt {
    /// Through this message (stream index, the `cv show --range` counting), inclusive.
    Message(usize),
    /// Through the tool result that shows this commit being made ([`CommitEvidence`]).
    Commit(String),
    /// Through the last message.
    End,
}

/// Knobs for [`rewind_session`].
#[derive(Debug, Clone)]
pub struct RewindOptions {
    pub at: CutAt,
    /// Start at the first record instead of the last compaction boundary before the cut.
    pub full: bool,
    /// Write here instead of the source's project dir (a sub-agent's: its parent's project dir).
    pub out_dir: Option<PathBuf>,
    /// The derived session's id (default: a fresh UUIDv4 — what Claude Code expects).
    pub new_id: Option<String>,
    pub dry_run: bool,
    /// Recorded in the provenance sidecar as `cv_version`.
    pub generator: String,
}

/// What a rewind did (or, under `dry_run`, would do). Lines are 1-based source line numbers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RewindResult {
    pub source_id: String,
    /// The `sessionId` the source's records carry (a sub-agent's is its parent's).
    pub source_session_id: Option<String>,
    pub source_path: PathBuf,
    /// The source prefix that was read (its size when the rewind began).
    pub source_bytes: u64,
    pub source_sha256: String,
    /// The source is a sub-agent transcript, emitted as a standalone top-level session.
    pub subagent: bool,
    pub new_id: String,
    pub new_path: PathBuf,
    pub provenance_path: PathBuf,
    pub cut_msg_idx: usize,
    pub cut_line: usize,
    pub cut_byte_offset: u64,
    pub start_msg_idx: usize,
    pub start_line: usize,
    pub start_byte_offset: u64,
    /// `from_compaction` or `full`.
    pub start_mode: &'static str,
    /// Message index of the compaction boundary the window starts from (`None`: no boundary before
    /// the cut, or `--full`).
    pub boundary_msg_idx: Option<usize>,
    /// The start moved back to the boundary's preserved-segment head.
    pub preserved_head: bool,
    /// Last source line copied (past `cut_line` when open tool calls were closed).
    pub end_line: usize,
    /// Tool results copied past the cut to close calls the cut left open.
    pub closed_tool_calls: usize,
    /// Tool calls still open at the end of the copy (no result before the next turn).
    pub open_tool_calls: usize,
    pub lines_written: usize,
    pub bytes_written: u64,
    /// Source lines/bytes after the copied range — the future the rewound agent doesn't know.
    pub omitted_lines: usize,
    pub omitted_bytes: u64,
    pub evidence: Option<CommitEvidence>,
    /// The cwd the agent was in at the cut (from the last record carrying one).
    pub cwd: Option<String>,
    pub warnings: Vec<String>,
    pub dry_run: bool,
}

/// The stream pass: per-message byte offsets, compaction boundaries, uuid → offset (to find a
/// preserved-segment head), and commit evidence. Small rows only; content is never kept.
struct ScanSink {
    offsets: Vec<u64>,
    unstamped: bool,
    boundaries: Vec<usize>,
    uuid_off: HashMap<String, u64>,
    stop_after: Option<usize>,
    evidence: Option<CommitEvidenceSink>,
}

impl MessageSink for ScanSink {
    fn message(&mut self, m: Message) -> Flow {
        let idx = self.offsets.len();
        match m.extra.get(OFFSET_KEY).and_then(Value::as_u64) {
            Some(off) => self.offsets.push(off),
            None => {
                self.unstamped = true;
                self.offsets.push(self.offsets.last().copied().unwrap_or(0));
            }
        }
        if m.kind == MessageKind::CompactionBoundary {
            self.boundaries.push(idx);
        }
        if let Some(id) = &m.id {
            self.uuid_off.insert(id.clone(), self.offsets[idx]);
        }
        if let Some(ev) = &mut self.evidence {
            if ev.observe(&m, idx) {
                return Flow::Stop;
            }
        }
        if self.stop_after == Some(idx) {
            return Flow::Stop;
        }
        Flow::Continue
    }
}

/// The project dir a derived session belongs in: the source's own dir, or — for a sub-agent at
/// `<proj>/<sid>/subagents/[workflows/<run>/]agent-<id>.jsonl` — `<proj>`.
pub fn project_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new("."));
    parent
        .ancestors()
        .find(|a| a.file_name().and_then(|n| n.to_str()) == Some("subagents"))
        .and_then(|s| s.parent()?.parent())
        .unwrap_or(parent)
        .to_path_buf()
}

/// Whether `r` is a sub-agent transcript (`agent-<id>.jsonl`, by id or file name).
pub fn is_subagent(r: &SessionRef) -> bool {
    r.id.starts_with("agent-") || r.path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agent-"))
}

/// Derive the rewound session (see the module docs). Claude Code sessions only: the derivation
/// copies raw records, and only Claude's records are resumable by id.
pub fn rewind_session(r: &SessionRef, opts: &RewindOptions) -> Result<RewindResult> {
    if r.harness != Harness::Claude {
        bail!("cv rewind currently supports Claude Code sessions only (got {})", r.harness);
    }
    let size = std::fs::metadata(&r.path)
        .with_context(|| format!("reading {}", r.path.display()))?
        .len();
    let subagent = is_subagent(r);

    // Pass 1 — stream the prefix through the adapter: offsets, boundaries, evidence.
    let want_sha = match &opts.at {
        CutAt::Commit(s) => Some(
            normalize_sha(s).with_context(|| format!("{s:?} is not a commit sha (7–64 hex digits)"))?,
        ),
        _ => None,
    };
    let mut scan = ScanSink {
        offsets: Vec::new(),
        unstamped: false,
        boundaries: Vec::new(),
        uuid_off: HashMap::new(),
        stop_after: match opts.at {
            CutAt::Message(n) => Some(n),
            _ => None,
        },
        evidence: want_sha.clone().map(|s| {
            let mut e = CommitEvidenceSink::new(vec![s], Some(r.path.clone()));
            e.stop_on_created = true;
            e
        }),
    };
    let file = std::fs::File::open(&r.path).with_context(|| format!("opening {}", r.path.display()))?;
    crate::harness::claude::stream_reader(
        &r.id,
        BufReader::new(file.take(size)),
        Some(r.path.clone()),
        &ParseOptions::lazy_offsets(),
        &mut scan,
    );
    if scan.unstamped {
        bail!("cannot map {}'s messages to source records (no byte offsets)", r.id);
    }
    let total = scan.offsets.len();
    if total == 0 {
        bail!("{} has no messages to rewind", r.id);
    }

    let evidence = match &want_sha {
        None => None,
        Some(sha) => {
            let found = scan.evidence.as_ref().map(|e| e.found.as_slice()).unwrap_or_default();
            Some(CommitEvidenceSink::best(found, sha).with_context(|| {
                format!(
                    "no tool result in {} shows commit {sha} being made: no `git commit`/`git push` \
                     output names it (rebased/cherry-picked shas and silenced commits leave none) — \
                     cut by message index instead (`--at <MSG_IDX>`)",
                    r.id
                )
            })?)
        }
    };
    let cut_idx = match (&opts.at, &evidence) {
        (CutAt::Message(n), _) => {
            if *n >= total {
                bail!("--at {n}: {} has {total} message(s) (0-based; the last is {})", r.id, total - 1);
            }
            *n
        }
        (CutAt::Commit(_), Some(e)) => e.msg_idx,
        _ => total - 1,
    };
    let cut_off = scan.offsets[cut_idx];

    // Start: the last compaction boundary at or before the cut, moved back to its preserved
    // segment's head when that sits earlier; `--full` (or no boundary) starts at the top.
    let boundary_idx = (!opts.full)
        .then(|| scan.boundaries.iter().rev().find(|b| **b <= cut_idx).copied())
        .flatten();
    let (start_off, preserved_head) = match boundary_idx {
        None => (0, false),
        Some(b) => {
            let b_off = scan.offsets[b];
            let head = read_line_at(&r.path, b_off)?
                .and_then(|v| {
                    v.pointer("/compactMetadata/preservedSegment/headUuid")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .and_then(|h| scan.uuid_off.get(&h).copied())
                .filter(|h| *h < b_off);
            match head {
                Some(h) => (h, true),
                None => (b_off, false),
            }
        }
    };
    let start_msg_idx = scan.offsets.partition_point(|o| *o < start_off);

    // Pass 2 — one sequential read of the prefix: hash every byte, count lines, copy the window.
    let new_id = opts.new_id.clone().unwrap_or_else(new_uuid);
    let out_dir = opts.out_dir.clone().unwrap_or_else(|| project_dir(&r.path));
    let new_path = out_dir.join(format!("{new_id}.jsonl"));
    let provenance_path = out_dir.join(format!("{new_id}{PROVENANCE_SUFFIX}"));
    let partial = out_dir.join(format!(".{new_id}.jsonl.partial"));
    if new_id == r.id {
        bail!("new id must differ from the source id");
    }
    if !opts.dry_run && (new_path.exists() || provenance_path.exists()) {
        bail!("{} already exists — pick another --to", new_path.display());
    }
    let mut writer = if opts.dry_run {
        None
    } else {
        std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
        Some(std::io::BufWriter::new(
            std::fs::File::create(&partial).with_context(|| format!("writing {}", partial.display()))?,
        ))
    };

    let mut reader = BufReader::with_capacity(1 << 20, std::fs::File::open(&r.path)?.take(size));
    let mut hasher = crate::digest::Sha256::new();
    let mut buf: Vec<u8> = Vec::new();
    let (mut off, mut line_no) = (0u64, 0usize);
    let (mut start_line, mut cut_line, mut end_line) = (0usize, 0usize, 0usize);
    let (mut lines_written, mut bytes_written) = (0usize, 0u64);
    let (mut omitted_lines, mut omitted_bytes) = (0usize, 0u64);
    let mut open: HashSet<String> = HashSet::new();
    let mut closed = 0usize;
    let mut source_session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    #[derive(PartialEq)]
    enum Phase {
        Before,
        Copy,
        Closing,
        Done,
    }
    let mut phase = Phase::Before;
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        line_no += 1;
        let at = off;
        off += n as u64;
        if phase == Phase::Before && at >= start_off {
            phase = Phase::Copy;
            start_line = line_no;
        }
        if phase == Phase::Before {
            continue;
        }
        if phase == Phase::Done {
            omitted_lines += 1;
            omitted_bytes += n as u64;
            continue;
        }
        let parsed: Option<Value> = serde_json::from_slice(&buf[..n]).ok();
        let out_line: Vec<u8> = match parsed {
            None => buf[..n].to_vec(), // non-JSON line: verbatim
            Some(mut v) => {
                if source_session_id.is_none() {
                    source_session_id = v.get("sessionId").and_then(Value::as_str).map(String::from);
                }
                let mut modified = stamp_session_id(&mut v, &new_id);
                if subagent && is_sidechain(&v) {
                    v["isSidechain"] = Value::Bool(false);
                    modified = true;
                }
                if subagent {
                    if let Some(o) = v.as_object_mut() {
                        modified |= o.shift_remove("agentId").is_some();
                    }
                }
                if phase == Phase::Closing && !closes_open_call(&v) {
                    phase = Phase::Done;
                    omitted_lines += 1;
                    omitted_bytes += n as u64;
                    continue;
                }
                track_tool_calls(&v, &mut open, if phase == Phase::Closing { Some(&mut closed) } else { None });
                if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                    cwd = Some(c.to_string());
                }
                if modified {
                    let mut s = v.to_string().into_bytes();
                    s.push(b'\n');
                    s
                } else {
                    let mut s = buf[..n].to_vec();
                    if s.last() != Some(&b'\n') {
                        s.push(b'\n');
                    }
                    s
                }
            }
        };
        if let Some(w) = writer.as_mut() {
            w.write_all(&out_line)?;
        }
        lines_written += 1;
        bytes_written += out_line.len() as u64;
        end_line = line_no;
        if at == cut_off {
            cut_line = line_no;
            phase = Phase::Closing;
        }
        if phase == Phase::Closing && open.is_empty() {
            phase = Phase::Done;
        }
    }
    if cut_line == 0 {
        if let Some(w) = writer.take() {
            drop(w);
            let _ = std::fs::remove_file(&partial);
        }
        bail!("message {cut_idx}'s record (byte {cut_off}) is not a line start in {}", r.path.display());
    }
    let source_sha256 = hasher.finish_hex();

    let mut warnings = Vec::new();
    if !open.is_empty() {
        warnings.push(format!(
            "{} tool call(s) issued before the cut have no result in the rewind (the next turn began first)",
            open.len()
        ));
    }

    let res = RewindResult {
        source_id: r.id.clone(),
        source_session_id,
        source_path: r.path.clone(),
        source_bytes: size,
        source_sha256,
        subagent,
        new_id,
        new_path,
        provenance_path,
        cut_msg_idx: cut_idx,
        cut_line,
        cut_byte_offset: cut_off,
        start_msg_idx,
        start_line,
        start_byte_offset: start_off,
        start_mode: if opts.full { "full" } else { "from_compaction" },
        boundary_msg_idx: boundary_idx,
        preserved_head,
        end_line,
        closed_tool_calls: closed,
        open_tool_calls: open.len(),
        lines_written,
        bytes_written,
        omitted_lines,
        omitted_bytes,
        evidence,
        cwd,
        warnings,
        dry_run: opts.dry_run,
    };

    if let Some(mut w) = writer.take() {
        w.flush()?;
        drop(w);
        std::fs::rename(&partial, &res.new_path)
            .with_context(|| format!("writing {}", res.new_path.display()))?;
        let body = serde_json::to_string_pretty(&provenance(&res, opts))? + "\n";
        std::fs::write(&res.provenance_path, body)
            .with_context(|| format!("writing {}", res.provenance_path.display()))?;
    }
    Ok(res)
}

/// The provenance sidecar body (`<new-id>.rewind.json`).
fn provenance(res: &RewindResult, opts: &RewindOptions) -> Value {
    let cut_by = match &opts.at {
        CutAt::Message(_) => "msg_idx",
        CutAt::Commit(_) => "commit",
        CutAt::End => "end",
    };
    serde_json::json!({
        "format": "cv-rewind",
        "v": 1,
        "source": {
            "id": res.source_id,
            "session_id": res.source_session_id,
            "path": res.source_path.display().to_string(),
            "bytes": res.source_bytes,
            "sha256": res.source_sha256,
            "subagent": res.subagent,
        },
        "new_id": res.new_id,
        "cut": {
            "by": cut_by,
            "msg_idx": res.cut_msg_idx,
            "line": res.cut_line,
            "byte_offset": res.cut_byte_offset,
            "evidence": res.evidence,
        },
        "start": {
            "mode": res.start_mode,
            "msg_idx": res.start_msg_idx,
            "line": res.start_line,
            "byte_offset": res.start_byte_offset,
            "boundary_msg_idx": res.boundary_msg_idx,
            "preserved_head": res.preserved_head,
        },
        "end_line": res.end_line,
        "closed_tool_calls": res.closed_tool_calls,
        "lines_written": res.lines_written,
        "omitted_tail": {
            "lines": res.omitted_lines,
            "bytes": res.omitted_bytes,
            "note": if res.omitted_lines == 0 {
                "nothing omitted: the rewind runs to the end of the source as read".to_string()
            } else {
                format!(
                    "the source continues for {} more line(s) after line {} — work this rewound agent never saw",
                    res.omitted_lines, res.end_line,
                )
            },
        },
        "derived_at": chrono::Utc::now().to_rfc3339(),
        "cv_version": opts.generator,
    })
}

/// Read and parse the single record starting at byte `off`.
fn read_line_at(path: &Path, off: u64) -> Result<Option<Value>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(std::io::SeekFrom::Start(off))?;
    let mut line = Vec::new();
    BufReader::new(f).read_until(b'\n', &mut line)?;
    Ok(serde_json::from_slice(&line).ok())
}

/// Update the open tool-call set from one record: an assistant's `tool_use` blocks open calls, a
/// user record's `tool_result` blocks close them (counted into `closed` past the cut).
fn track_tool_calls(v: &Value, open: &mut HashSet<String>, mut closed: Option<&mut usize>) {
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                if let Some(id) = b.get("id").and_then(Value::as_str) {
                    open.insert(id.to_string());
                }
            }
            Some("tool_result") => {
                let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                if open.remove(id) {
                    if let Some(c) = closed.as_deref_mut() {
                        *c += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Past the cut, a record still belongs to the cut's moment when it isn't a new turn: bookkeeping
/// (attachments, hooks, system notes) or a user record carrying tool results. The next assistant
/// turn or a real prompt ends the copy.
fn closes_open_call(v: &Value) -> bool {
    match turn_kind(v) {
        None => true,
        Some(false) => false,
        Some(true) => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|bs| bs.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))),
    }
}

#[cfg(test)]
mod tests;
