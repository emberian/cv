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
//! Claude-specific fields survive, and lines that need no rewrite stay byte-identical. Portable
//! replay additionally repairs illegal tool IDs, demotes plaintext reasoning to text and removes
//! reasoning that has none; native preserves archive fidelity. The default is native when every
//! model recorded in the window is `claude-*`, portable otherwise. The derived
//! file is the contiguous record range `[start, cut]` of the source:
//!
//! * **cut** — a message index (the `cv show --range` counting), or a commit sha resolved to the
//!   tool result that *shows the commit being made* (see [Commit evidence](#commit-evidence)). If
//!   the cut leaves tool calls open (parallel calls whose results land on later records), the copy
//!   runs on until they close, stopping at the next assistant turn or prompt — the API rejects a
//!   `tool_use` with no result, and those results happened at the same moment. A later record of
//!   the cut's own API response (each parallel `tool_use` is its own record) is no new turn.
//! * **start** — by default the last compaction boundary at or before the cut. Claude Code resumes
//!   from the last boundary anyway, so this is exactly the context the agent had when it made the
//!   cut, and it keeps a month-long, 900 MB session down to one window. A boundary written by
//!   partial compaction names a `preservedSegment` whose head sits *before* the boundary (recent
//!   messages kept verbatim across the compaction); the start moves back to that head so the
//!   preserved messages come along (their relinking is Claude Code's, inferred from the record
//!   shape — the head's `parentUuid` is left as recorded). `--full` starts at the first record.
//!
//! Every record's `sessionId`/`session_id` becomes the new id; `parentUuid` chains are untouched —
//! the window head keeps whatever parent it was recorded with (a boundary's is usually, not always,
//! `null`; Claude Code's back-walk stops at the boundary regardless). The source is only read.
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
//! once up front and every pass reads only that prefix through one held source file. Non-growth
//! mutations are rejected by metadata/identity checks; growth uses the shared resume head/tail
//! witness under the immutable-prefix append contract. Samples do not prove arbitrary unsampled
//! prefix rewrites absent when combined with growth.
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

use crate::harness::claude::record::Facts;
use crate::harness::claude::skills::{self, SkillsPolicy, SkillsReport};
use crate::ir::{Block, Message, MessageKind, SessionRef};
use crate::json::{ReadInput, Validation};
use crate::lazy::Resolver;
use crate::offsets::OFFSET_KEY;
use crate::prune::new_uuid;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
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

/// Only the first 64 hex digits can matter to a normalized query. Carry that prefix across
/// decoder chunks, including arbitrarily long runs, rather than owning a tool's entire output.
struct ShaMatches<'a> {
    queries: &'a [String],
    hits: Vec<bool>,
    prefix: [u8; 64],
    len: usize,
}
impl<'a> ShaMatches<'a> {
    fn new(queries: &'a [String]) -> Self {
        Self {
            queries,
            hits: vec![false; queries.len()],
            prefix: [0; 64],
            len: 0,
        }
    }
    fn end_token(&mut self) {
        if self.len >= MIN_SHA_LEN {
            for (query, hit) in self.queries.iter().zip(&mut self.hits) {
                let n = self.len.min(query.len());
                *hit |= self.prefix[..n].eq_ignore_ascii_case(&query.as_bytes()[..n]);
            }
        }
        self.len = 0;
    }
    fn push(&mut self, text: &str) {
        for byte in text.bytes() {
            if byte.is_ascii_hexdigit() {
                if self.len < self.prefix.len() {
                    self.prefix[self.len] = byte;
                    self.len += 1;
                }
            } else {
                self.end_token();
            }
        }
    }
    fn finish(mut self) -> Vec<bool> {
        self.end_token();
        self.hits
    }
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
    resolver: Resolver,
    /// Stop the stream at the first `Created` match (a rewind wants the landing moment, not
    /// every later mention).
    stop_on_created: bool,
    pub found: Vec<CommitEvidence>,
    fail: Option<anyhow::Error>,
}

impl CommitEvidenceSink {
    /// `shas` must be normalized ([`normalize_sha`]). `source` resolves lazy result bodies.
    pub fn new(shas: Vec<String>, source: Option<PathBuf>) -> Self {
        CommitEvidenceSink {
            shas,
            git_calls: HashMap::new(),
            msg_idx: 0,
            resolver: Resolver::new(source),
            stop_on_created: false,
            found: Vec::new(),
            fail: None,
        }
    }

    /// Observe one message at stream index `idx`; returns whether a `Created` match landed.
    fn observe(&mut self, m: &Message, idx: usize) -> Result<bool> {
        let mut created = false;
        for b in &m.content {
            match b {
                Block::ToolUse { id, input, .. } => {
                    if let Some(cmd) = crate::events::command_of_json(input)? {
                        if let Some(kind) = git_evidence_kind(&cmd) {
                            self.git_calls.insert(id.clone(), (kind, cmd));
                        }
                    }
                }
                // Failed git operations can quote the requested SHA in diagnostics without
                // creating or pushing it. They cannot support successful commit provenance.
                Block::ToolResult { is_error: true, .. } => {}
                Block::ToolResult {
                    tool_use_id, content, ..
                } => {
                    let Some((kind, cmd)) = self.git_calls.get(tool_use_id) else {
                        continue;
                    };
                    let mut matches = ShaMatches::new(&self.shas);
                    content.try_for_each_chunk(&self.resolver, 64 * 1024, |chunk| {
                        matches.push(chunk);
                        Ok(())
                    })?;
                    for (sha, matched) in self.shas.iter().zip(matches.finish()) {
                        if matched {
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
        Ok(created)
    }

    /// The best evidence for `sha`: its first `Created` match, else its first `Pushed` one.
    pub fn best(found: &[CommitEvidence], sha: &str) -> Option<CommitEvidence> {
        let of = |k| found.iter().find(|e| e.sha == sha && e.kind == k);
        of(EvidenceKind::Created).or_else(|| of(EvidenceKind::Pushed)).cloned()
    }
}

impl MessageSink for CommitEvidenceSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.fail.take()
    }

    fn message(&mut self, m: Message) -> Flow {
        let idx = self.msg_idx;
        self.msg_idx += 1;
        match self.observe(&m, idx) {
            Ok(true) if self.stop_on_created => return Flow::Stop,
            Err(error) => {
                self.fail = Some(error);
                return Flow::Stop;
            }
            _ => {}
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
        want = want
            .into_iter()
            .zip(present)
            .filter(|(_, p)| *p)
            .map(|(s, _)| s)
            .collect();
    }
    if want.is_empty() {
        return Ok(Vec::new());
    }
    let adapter = crate::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    let mut sink = CommitEvidenceSink::new(want, Some(r.path.clone()));
    adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut sink)?;
    Ok(sink.found)
}

/// [`commit_evidence`] over many sessions at once (in parallel where the build allows), keeping
/// only sessions with evidence. A failed source read returns an error instead of an absent match.
pub fn commit_evidence_many(refs: Vec<SessionRef>, shas: &[String]) -> Result<Vec<(SessionRef, Vec<CommitEvidence>)>> {
    let rows = crate::par_filter_map(refs, |r| Some(commit_evidence(&r, shas).map(|found| (r, found))));
    Ok(rows
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, found)| !found.is_empty())
        .collect())
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
    /// Through the last message AND the records after it, to the end of the source: the tail
    /// (`cost-state`, `history-suppression`, `bridge-session`, titles, attachments) is part of the
    /// session, so nothing after the last message is dropped.
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
    /// Portable replay removes signed thinking and repairs tool IDs; native keeps archival bytes.
    /// `None`: [`crate::emit::ThinkingMode::replay_default`] over the models the window records.
    pub thinking: Option<crate::emit::ThinkingMode>,
    /// Which invoked skills the derived session restores.
    pub skills_policy: SkillsPolicy,
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
    /// Where to launch `claude --resume` from: the recorded cwd whose Claude project-dir encoding
    /// is the dir the session belongs in (Claude looks a session up by the launch dir), else
    /// `cut_cwd`.
    pub cwd: Option<String>,
    /// The cwd the agent was working in at the cut (the last record carrying one) — often a
    /// worktree it had `cd`'d into, which is not where it resumes from.
    pub cut_cwd: Option<String>,
    pub warnings: Vec<String>,
    /// The thinking policy applied (`native` or `portable`), chosen or defaulted.
    pub thinking: &'static str,
    /// Restored skills policy, omitted at its byte-preserving default.
    #[serde(skip_serializing_if = "SkillsReport::is_keep")]
    pub skills: SkillsReport,
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
    /// Indices of messages whose recorded model is not `claude-*`.
    foreign: Vec<usize>,
}

impl MessageSink for ScanSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.evidence.as_mut().and_then(MessageSink::take_error)
    }

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
        if crate::emit::ThinkingMode::replay_default(m.model.as_deref()) == crate::emit::ThinkingMode::Portable {
            self.foreign.push(idx);
        }
        if let Some(id) = &m.id {
            self.uuid_off.insert(id.clone(), self.offsets[idx]);
        }
        if let Some(ev) = &mut self.evidence {
            match ev.observe(&m, idx) {
                Ok(true) => return Flow::Stop,
                Err(error) => {
                    ev.fail = Some(error);
                    return Flow::Stop;
                }
                _ => {}
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
    r.id.starts_with("agent-")
        || r.path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("agent-"))
}

/// The source rewind copies a prefix of, captured before the copy and checked after it: the
/// same file, unchanged or (an append-only transcript) grown with its sampled prefix intact
/// ([`crate::source::FileId::admits`], whose sampling limit applies). The provenance's sha256 is
/// of the bytes actually copied.
struct PrefixReadGuard {
    source: crate::json::JsonSource,
}
impl PrefixReadGuard {
    fn capture(path: &Path) -> Result<(Self, std::fs::File)> {
        let source = crate::json::JsonSource::capture(path)?;
        let file = std::fs::File::open(path)?;
        source.check_metadata(file.metadata()?)?;
        Ok((Self { source }, file))
    }
    fn verify(&self, file: &std::fs::File) -> Result<()> {
        self.source.check_metadata(file.metadata()?)?;
        self.source.check()
    }
}

/// Derive the rewound session (see the module docs). Claude Code sessions only: the derivation
/// copies raw records, and only Claude's records are resumable by id.
pub fn rewind_session(r: &SessionRef, opts: &RewindOptions) -> Result<RewindResult> {
    rewind_session_after_scan(r, opts, || Ok(()))
}

fn rewind_session_after_scan(
    r: &SessionRef,
    opts: &RewindOptions,
    after_scan: impl FnOnce() -> Result<()>,
) -> Result<RewindResult> {
    anyhow::ensure!(
        matches!(
            opts.thinking,
            None | Some(crate::emit::ThinkingMode::Native | crate::emit::ThinkingMode::Portable)
        ),
        "rewind --thinking supports native or portable"
    );
    if let Some(id) = &opts.new_id {
        crate::publish::validate_output_id(id)?;
    }
    // Only an adapter that declares derived copies (Claude's) says what a rewound copy rewrites.
    let identity = crate::derive::identity(r.harness).map_err(|_| {
        anyhow::anyhow!(
            "cv rewind currently supports Claude Code sessions only (got {})",
            r.harness
        )
    })?;
    let (source_guard, file) =
        PrefixReadGuard::capture(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
    let size = source_guard.source.len();
    let subagent = is_subagent(r);

    let window = window(r, opts, &source_guard, &file, size, after_scan)?;
    // The destination, refused up front when taken, and the staged file pass 2 writes.
    let new_id = opts.new_id.clone().unwrap_or_else(new_uuid);
    let out_dir = opts.out_dir.clone().unwrap_or_else(|| project_dir(&r.path));
    let new_path = out_dir.join(format!("{new_id}.jsonl"));
    let provenance_path = out_dir.join(format!("{new_id}{PROVENANCE_SUFFIX}"));
    if new_id == r.id {
        bail!("new id must differ from the source id");
    }
    if !opts.dry_run {
        crate::publish::refuse_existing(&new_path, " (pick another --to)")?;
        crate::publish::refuse_existing(&provenance_path, " (pick another --to)")?;
    }
    // The rewound session holds the source's records: it is no more exposed than the source.
    let mode = crate::publish::mode_of(&file);
    let mut writer = if opts.dry_run {
        None
    } else {
        std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
        Some(std::io::BufWriter::new(crate::publish::Staged::create(
            &out_dir, "rewind", mode,
        )?))
    };

    let rewrite = crate::derive::Rewrite {
        session_id: &new_id,
        standalone: subagent,
        identity,
    };
    let staged = opts.skills_policy != SkillsPolicy::Keep || window.thinking == crate::emit::ThinkingMode::Portable;
    let Copied {
        start_line,
        cut_line,
        end_line,
        mut lines_written,
        mut bytes_written,
        omitted_lines,
        omitted_bytes,
        open,
        closed,
        source_session_id,
        cwds,
        source_sha256,
        replay_stage,
    } = copy(r, &source_guard, &file, size, &window, &rewrite, staged, &mut writer)?;
    let Window {
        cut_idx,
        cut_off,
        boundary_idx,
        start_off,
        start_msg_idx,
        preserved_head,
        thinking,
        evidence,
        ..
    } = window;

    // The launch dir: among the cwds the window recorded (plus the owning session's first one — a
    // sub-agent's records may all sit in a worktree), the one Claude would file this session under.
    let home = project_dir(&r.path);
    let home_name = home
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let owner = source_session_id
        .as_deref()
        .and_then(|sid| first_cwd(&home.join(format!("{sid}.jsonl"))));
    let cut_cwd = cwds.last().cloned();
    let launch = cwds
        .iter()
        .rev()
        .cloned()
        .chain(owner)
        .find(|c| crate::harness::claude::emit::claude_encode_cwd(Path::new(c)) == home_name);
    let mut warnings = Vec::new();
    if launch.is_none() && cut_cwd.is_some() {
        warnings.push(format!(
            "no recorded cwd maps to {home_name}; `claude --resume` from the cut's cwd may not find the session"
        ));
    }
    if !open.is_empty() {
        warnings.push(format!(
            "{} tool call(s) issued before the cut have no result in the rewind (the next turn began first)",
            open.len()
        ));
    }

    let mut skills = SkillsReport {
        policy: opts.skills_policy.as_str(),
        ..Default::default()
    };
    if staged {
        let mut source = replay_stage.finish()?;
        if thinking == crate::emit::ThinkingMode::Portable {
            let (stage, changes) = crate::harness::claude::replay::normalize(source, thinking)?;
            lines_written -= changes.records;
            if changes.ids + changes.thinking + changes.dropped != 0 {
                warnings.push(format!(
                    "portable replay: remapped {} tool ID(s), demoted {} thinking block(s) to text, removed {} without plaintext ({} record(s) left empty)",
                    changes.ids, changes.thinking, changes.dropped, changes.records
                ));
            }
            source = stage.finish()?;
        }
        (source, skills) = skills::apply(source, opts.skills_policy, skills::config_dir_of(&r.path).as_deref())?;
        bytes_written = source.byte_len()?;
        if let Some(w) = writer.as_mut() {
            source.copy_to(w)?;
        }
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
        cwd: launch.or_else(|| cut_cwd.clone()),
        cut_cwd,
        warnings,
        thinking: thinking.as_str(),
        skills,
        dry_run: opts.dry_run,
    };

    if let Some(w) = writer.take() {
        use crate::publish::{Claim, Set, Staged};
        let transcript = w.into_inner().map_err(|e| e.into_error())?;
        // A sub-agent's copy is a standalone session.
        crate::derive::verify(
            r.harness,
            &source_guard.source,
            &file,
            crate::derive::agent_of(&r.path),
            transcript.path(),
            &res.new_path,
            None,
        )?;
        let body = serde_json::to_string_pretty(&provenance(&res, opts))? + "\n";
        let mut provenance_file = Staged::create(&out_dir, "rewind", mode)?;
        provenance_file.write_all(body.as_bytes())?;
        // One set, provenance first: a published transcript always has the receipt that explains it.
        let mut set = Set::new();
        set.add(provenance_file, &res.provenance_path, Claim::New);
        set.add(transcript, &res.new_path, Claim::New);
        set.publish()?;
    }
    Ok(res)
}

/// Pass 1's verdict: the records the copy spans and how it is written.
struct Window {
    cut_idx: usize,
    cut_off: u64,
    /// `End` cuts at the end of the file; an explicit cut stops at its message's record.
    to_eof: bool,
    boundary_idx: Option<usize>,
    start_off: u64,
    start_msg_idx: usize,
    preserved_head: bool,
    thinking: crate::emit::ThinkingMode,
    evidence: Option<CommitEvidence>,
}

/// Pass 1 — stream the captured prefix through the adapter for message offsets, compaction
/// boundaries and commit evidence, then resolve the cut, the start and the thinking mode.
fn window(
    r: &SessionRef,
    opts: &RewindOptions,
    source_guard: &PrefixReadGuard,
    file: &std::fs::File,
    size: u64,
    after_scan: impl FnOnce() -> Result<()>,
) -> Result<Window> {
    let want_sha = match &opts.at {
        CutAt::Commit(s) => {
            Some(normalize_sha(s).with_context(|| format!("{s:?} is not a commit sha (7–64 hex digits)"))?)
        }
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
        foreign: Vec::new(),
    };
    crate::harness::Adapter::transcript(
        &crate::harness::claude::ROOTLESS,
        r,
        &Demand::LEAN.with_spans().with_offsets(),
        crate::stream::Start::Prefix(size),
        &mut scan,
    )?;
    if let Some(error) = scan.take_error() {
        return Err(error);
    }
    after_scan()?;
    source_guard.verify(file)?;
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
                bail!(
                    "--at {n}: {} has {total} message(s) (0-based; the last is {})",
                    r.id,
                    total - 1
                );
            }
            *n
        }
        (CutAt::Commit(_), Some(e)) => e.msg_idx,
        _ => total - 1,
    };
    let cut_off = scan.offsets[cut_idx];
    // `End` cuts at the end of the FILE, not at the last message's record: the records after the
    // last message are part of the session and ride along. An explicit `--at`/`--commit` is a
    // deliberate truncation and still stops at that message's record.
    let to_eof = matches!(opts.at, CutAt::End);

    // Start: the last compaction boundary at or before the cut, moved back to its preserved
    // segment's head when that sits earlier; `--full` (or no boundary) starts at the top.
    let boundary_idx = (!opts.full)
        .then(|| scan.boundaries.iter().rev().find(|b| **b <= cut_idx).copied())
        .flatten();
    let (start_off, preserved_head) = match boundary_idx {
        None => (0, false),
        Some(b) => {
            let b_off = scan.offsets[b];
            let head = read_line_at(&source_guard.source, file, b_off)?
                .and_then(|facts| facts.compact_head)
                .and_then(|h| scan.uuid_off.get(&h).copied())
                .filter(|h| *h < b_off);
            match head {
                Some(h) => (h, true),
                None => (b_off, false),
            }
        }
    };
    let start_msg_idx = scan.offsets.partition_point(|o| *o < start_off);
    let thinking = opts
        .thinking
        .unwrap_or(if scan.foreign.iter().any(|i| (start_msg_idx..=cut_idx).contains(i)) {
            crate::emit::ThinkingMode::Portable
        } else {
            crate::emit::ThinkingMode::Native
        });
    Ok(Window {
        cut_idx,
        cut_off,
        to_eof,
        boundary_idx,
        start_off,
        start_msg_idx,
        preserved_head,
        thinking,
        evidence,
    })
}

/// What pass 2 copied.
struct Copied {
    start_line: usize,
    cut_line: usize,
    end_line: usize,
    lines_written: usize,
    bytes_written: u64,
    omitted_lines: usize,
    omitted_bytes: u64,
    /// Each call still open and the API response (`message.id`) that issued it.
    open: HashMap<String, Option<String>>,
    closed: usize,
    source_session_id: Option<String>,
    cwds: Vec<String>,
    source_sha256: String,
    /// The window's records for a portable replay to re-emit.
    replay_stage: crate::harness::store::bytes::ByteStage,
}

/// Pass 2 — one sequential read of the prefix frames, hashes and counts every line, and copies
/// the window's records (rewritten to `new_id`) into `writer`, or into the replay stage when the
/// copy re-emits. Past an explicit cut it copies only what closes the cut's open calls.
#[allow(clippy::too_many_arguments)]
fn copy(
    r: &SessionRef,
    source_guard: &PrefixReadGuard,
    file: &std::fs::File,
    size: u64,
    w: &Window,
    rewrite: &crate::derive::Rewrite<'_>,
    staged: bool,
    writer: &mut Option<std::io::BufWriter<crate::publish::Staged>>,
) -> Result<Copied> {
    let Window {
        cut_idx,
        cut_off,
        to_eof,
        start_off,
        thinking,
        ..
    } = *w;
    // Only a portable replay walks every record back through the reader; a native copy is bytes.
    let reemits = thinking == crate::emit::ThinkingMode::Portable;
    let mut line_no = 0usize;
    let (mut start_line, mut cut_line, mut end_line) = (0usize, 0usize, 0usize);
    let (mut lines_written, mut bytes_written) = (0usize, 0u64);
    let (mut omitted_lines, mut omitted_bytes) = (0usize, 0u64);
    // Each open call and the API response (`message.id`) that issued it.
    let mut open: HashMap<String, Option<String>> = HashMap::new();
    let mut closed = 0usize;
    let mut replay_stage = crate::harness::store::bytes::ByteStage::new();
    let mut source_session_id: Option<String> = None;
    let mut cwds: Vec<String> = Vec::new();
    #[derive(PartialEq)]
    enum Phase {
        Before,
        Copy,
        Closing,
        Done,
    }
    let mut phase = Phase::Before;
    use crate::harness::store::jsonl::ranges::{self, Range};
    let mut rows = ranges::records(file, Range { offset: 0, len: size })?.hashing();
    for row in rows.by_ref() {
        let (range, newline) = row?;
        let at = range.offset;
        let n = range.len + u64::from(newline);
        line_no += 1;
        if phase == Phase::Before && at >= start_off {
            phase = Phase::Copy;
            start_line = line_no;
        }
        if phase == Phase::Before {
            continue;
        }
        if phase == Phase::Done {
            omitted_lines += 1;
            omitted_bytes += n;
            continue;
        }
        // Only JSON whitespace is an empty physical record, copied verbatim; the selector refuses
        // any other range that is not one JSON value, a line the new session could not read back,
        // before anything is published (akapug, #27).
        let mut selected =
            match crate::derive::Select::read(&source_guard.source, file, range, newline, false, &rewrite.identity) {
                Ok(selected) => selected,
                Err(error) if error.is::<crate::json::InvalidJson>() => {
                    // Past an explicit cut only a record that closes an open call is copied, and a
                    // line that does not parse closes nothing: it is omitted like any other.
                    if phase == Phase::Closing {
                        omitted_lines += 1;
                        omitted_bytes += n;
                        continue;
                    }
                    // The captured prefix can end inside a record the session is still appending.
                    let why = if range.offset + range.len == size && !newline {
                        format!(
                            "is unterminated at the end of the {size} bytes captured; if the session is still \
                         being written, rerun the rewind; if it was cut short, repair it"
                        )
                    } else {
                        "is not a complete JSON record; repair it".into()
                    };
                    drop(writer.take());
                    bail!(
                        "{}: line {line_no} {why} or cut before it with `--at <MSG_IDX>`",
                        r.path.display()
                    );
                }
                Err(error) => return Err(error),
            };
        if let Some(record) = &selected.record {
            let facts = record.with_view(|view| Ok(Facts::parse(view)?.0))?;
            if source_session_id.is_none() {
                source_session_id = facts.session_id.clone();
            }
            // A `user`/`assistant` record with no `message` body: the Claude codec carries it
            // verbatim on a byte copy, but a re-emitted copy walks it through the reader, which
            // takes nothing out of it and would drop it silently. Refuse only where the copy
            // would lose it; nothing is published either way.
            if reemits {
                if let Some(ty) = unreadable_record(&facts) {
                    drop(writer.take());
                    bail!(
                        "{}: line {line_no} is a `{ty}` record with no `message` body (uuid {}); a \
                         re-emitted copy would drop it: repair or remove that record, rewind without \
                         re-emitting, or cut before it with `--at <MSG_IDX>`",
                        r.path.display(),
                        facts.uuid.as_deref().unwrap_or("(none)"),
                    );
                }
            }
            if phase == Phase::Closing && !closes_open_call(&facts, &open) {
                phase = Phase::Done;
                omitted_lines += 1;
                omitted_bytes += n;
                continue;
            }
            track_tool_calls(
                &facts,
                &mut open,
                if phase == Phase::Closing {
                    Some(&mut closed)
                } else {
                    None
                },
            );
            if let Some(c) = facts.cwd.as_deref() {
                if cwds.last().map(String::as_str) != Some(c) {
                    cwds.retain(|x| x != c);
                    cwds.push(c.to_string());
                }
            }
            // Native valid records have always ended in a newline in rewind output.
            selected.newline = true;
        }
        let selected = std::iter::once(Ok(selected));
        let counts = if staged {
            crate::derive::derive(&source_guard.source, file, selected, rewrite, &mut replay_stage)?
        } else if let Some(w) = writer.as_mut() {
            crate::derive::derive(&source_guard.source, file, selected, rewrite, w)?
        } else {
            crate::derive::derive(&source_guard.source, file, selected, rewrite, &mut std::io::sink())?
        };
        lines_written += 1;
        bytes_written += counts.bytes as u64;
        end_line = line_no;
        if at == cut_off {
            cut_line = line_no;
            // Past an explicit cut the copy keeps only what closes the cut's open calls; an `End`
            // rewind copies to EOF, so its cut never closes the phase.
            if !to_eof {
                phase = Phase::Closing;
            }
        }
        if phase == Phase::Closing && open.is_empty() {
            phase = Phase::Done;
        }
    }
    if cut_line == 0 {
        drop(writer.take());
        bail!(
            "message {cut_idx}'s record (byte {cut_off}) is not a line start in {}",
            r.path.display()
        );
    }
    source_guard.verify(file)?;
    let source_sha256 = rows.finish_digest().expect("the framer was hashing");
    Ok(Copied {
        start_line,
        cut_line,
        end_line,
        lines_written,
        bytes_written,
        omitted_lines,
        omitted_bytes,
        open,
        closed,
        source_session_id,
        cwds,
        source_sha256,
        replay_stage,
    })
}

/// The provenance sidecar body (`<new-id>.rewind.json`).
fn provenance(res: &RewindResult, opts: &RewindOptions) -> Value {
    let cut_by = match &opts.at {
        CutAt::Message(_) => "msg_idx",
        CutAt::Commit(_) => "commit",
        CutAt::End => "end",
    };
    let mut value = serde_json::json!({
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
        "thinking": res.thinking,
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
    });
    if !res.skills.is_keep() {
        value["skills"] = serde_json::json!(res.skills);
    }
    value
}

/// The first `cwd` a transcript records (within its first few hundred lines), if any.
fn first_cwd(path: &Path) -> Option<String> {
    let source = crate::json::JsonSource::capture(path).ok()?;
    let file = std::fs::File::open(path).ok()?;
    let rows = crate::harness::store::jsonl::ranges::records(
        &file,
        crate::harness::store::jsonl::ranges::Range {
            offset: 0,
            len: source.len(),
        },
    )
    .ok()?;
    for row in rows.take(500) {
        let (range, _) = row.ok()?;
        let value = source.read(ReadInput::Range(range.offset, range.len), Validation::Value, |view| {
            view.get("cwd")?
                .map(|v| Ok(v.resolve()?.as_str().map(str::to_owned)))
                .transpose()
                .map(Option::flatten)
        });
        if let Ok(Some(cwd)) = value {
            source.check().ok()?;
            return Some(cwd);
        }
    }
    None
}

/// Read only the decision fields of the record starting at `off`.
fn read_line_at(source: &crate::json::JsonSource, file: &std::fs::File, off: u64) -> Result<Option<Facts>> {
    use crate::harness::store::jsonl::ranges::{self, Range};
    let Some(row) = ranges::records(
        file,
        Range {
            offset: off,
            len: source.len() - off,
        },
    )?
    .next() else {
        return Ok(None);
    };
    let (range, _) = row?;
    match source.read(ReadInput::Range(range.offset, range.len), Validation::Value, |view| {
        Ok(Facts::parse(view)?.0)
    }) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is::<crate::json::InvalidJson>() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Update the open tool-call set from one record: a top-level assistant record's `tool_use` blocks
/// open calls, a top-level user record's `tool_result` blocks close them (counted into `closed`
/// past the cut). The record's own `type` decides, as the conversation decoder does: a request
/// snapshot (`api-request-blob`) embeds the same message shape verbatim and is evidence to keep,
/// never an event. Sidechain records carry the same types and still count, so [`Facts::turn_kind`],
/// which excludes them, is not the gate.
fn track_tool_calls(facts: &Facts, open: &mut HashMap<String, Option<String>>, mut closed: Option<&mut usize>) {
    let opens = match facts.ty.as_str() {
        "assistant" => true,
        "user" => false,
        _ => return,
    };
    for b in &facts.blocks {
        match b.ty.as_deref() {
            Some("tool_use") if opens => {
                if let Some(id) = &b.id {
                    open.insert(id.clone(), facts.message_id.clone());
                }
            }
            Some("tool_result") if !opens && open.remove(b.tool_use_id.as_deref().unwrap_or("")).is_some() => {
                if let Some(c) = closed.as_deref_mut() {
                    *c += 1;
                }
            }
            _ => {}
        }
    }
}

/// Past the cut, a record still belongs to the cut's moment when it isn't a new turn: bookkeeping
/// (attachments, hooks, system notes), a user record answering an `open` call, or another record of
/// a response with an open call (Claude Code writes each parallel `tool_use` of one response as
/// its own record). The next assistant turn or a real prompt ends the copy.
fn closes_open_call(facts: &Facts, open: &HashMap<String, Option<String>>) -> bool {
    match facts.turn_kind() {
        None => true,
        Some(false) => facts
            .message_id
            .as_ref()
            .is_some_and(|id| open.values().any(|r| r.as_ref() == Some(id))),
        Some(true) => facts.blocks.iter().any(|b| {
            b.ty.as_deref() == Some("tool_result") && b.tool_use_id.as_ref().is_some_and(|id| open.contains_key(id))
        }),
    }
}

/// The `type` of a record the Claude codec cannot read: a conversation record (`user`/`assistant`)
/// with no `message` body, which [`crate::harness::claude::record`] drops without a word. Every
/// other shape is one cv knows how to place — a known bookkeeping type is carried, an unknown type
/// without a body is a meta record, and a `system` row without content is a bodyless notice — so
/// only a record that *claims* to be a turn and has no turn in it is unreadable.
fn unreadable_record(facts: &Facts) -> Option<&str> {
    (matches!(facts.ty.as_str(), "user" | "assistant") && !facts.message).then_some(facts.ty.as_str())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod json_read_tests {
    use super::*;

    #[test]
    fn commit_evidence_excludes_failed_tool_results_even_when_they_name_the_sha() {
        let mut sink = CommitEvidenceSink::new(vec!["abcdef1".into()], None);
        let mut call = Message::new(crate::Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Bash".into(),
            namespace: None,
            input: serde_json::json!({"command":"git revert abcdef1"}).into(),
        });
        assert!(!sink.observe(&call, 0).unwrap());
        let mut failed = Message::new(crate::Role::Tool);
        failed.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            tool_name: None,
            status: None,
            details: None,
            is_error: true,
            content: "error: commit abcdef1 cannot be reverted because of conflicts".into(),
        });
        assert!(!sink.observe(&failed, 1).unwrap());
        assert!(sink.found.is_empty());
        let mut succeeded = Message::new(crate::Role::Tool);
        succeeded.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            tool_name: None,
            status: None,
            details: None,
            is_error: false,
            content: "[main abcdef1] created".into(),
        });
        assert!(sink.observe(&succeeded, 2).unwrap());
        assert_eq!(sink.found.len(), 1);
        assert_eq!(sink.found[0].msg_idx, 2);
    }

    #[test]
    fn chunked_sha_matches_the_whole_output_oracle() {
        let queries = ["abcdef1", "abcdef1234567", "1234567", &"a".repeat(64)]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let cases = [
            "".into(),
            "abcdef abcdef1zABCDEF1234567".into(),
            "[main ABCDEF123] hi\n1234567..abcdef1".into(),
            format!("{}!{}", "a".repeat(200_000), "🌙"),
            "aabcdef1234567 abcdef0 123456".into(),
        ];
        for body in cases {
            let expected: Vec<_> = queries.iter().map(|q| output_names_sha(&body, q)).collect();
            for size in [1, 2, 7, 64, 65_536] {
                let mut scan = ShaMatches::new(&queries);
                // Character boundaries model decoded chunks, including UTF-8 delimiters.
                let mut start = 0;
                while start < body.len() {
                    let mut end = (start + size).min(body.len());
                    while !body.is_char_boundary(end) {
                        end += 1;
                    }
                    scan.push(&body[start..end]);
                    start = end;
                }
                assert_eq!(scan.finish(), expected);
            }
        }
    }

    #[test]
    fn commit_evidence_scans_guarded_source_text_without_an_implicit_path() {
        let world = crate::test_world("rewind-guarded-text");
        let path = world.home.join("source.json");
        let sidecar = world.home.join("meta.json");
        let body = format!("{}[main ABCDEF123] done", "λ\n".repeat(30_000));
        let encoded = serde_json::to_vec(&body).unwrap();
        std::fs::write(&path, &encoded).unwrap();
        let mut guard = crate::source::ReadSet::default();
        guard.record(&sidecar);
        let json = crate::Json::source_with_reads(&path, 0, encoded.len() as u64, &guard).unwrap();
        let content = json.with_view(|view| view.source_text()).unwrap().unwrap();
        let mut message = Message::new(crate::Role::Tool);
        message.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            content,
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        let sink = || {
            let mut sink = CommitEvidenceSink::new(vec!["abcdef1234567".into()], None);
            sink.git_calls
                .insert("call".into(), (EvidenceKind::Created, "git commit".into()));
            sink
        };
        let mut first = sink();
        assert_eq!(first.message(message.clone()), Flow::Continue);
        assert!(first.take_error().is_none());
        assert_eq!(first.found.len(), 1);
        std::fs::write(sidecar, "changed").unwrap();
        let mut stale = sink();
        assert_eq!(stale.message(message), Flow::Stop);
        assert!(stale.take_error().is_some());
        assert!(stale.found.is_empty());
    }

    #[test]
    fn commit_evidence_reports_unavailable_lazy_results() {
        let world = crate::test_world("rewind_text_failure");
        let path = world.home.join("absent-output");
        let mut sink = CommitEvidenceSink::new(vec!["abcdef1".into()], None);
        sink.git_calls
            .insert("call".into(), (EvidenceKind::Created, "git commit".into()));
        let mut message = Message::new(crate::Role::Tool);
        message.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            content: crate::Text::Span(crate::Span {
                source: Some(path),
                offset: 0,
                len: 8,
                escaped: false,
            }),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        assert_eq!(sink.message(message), Flow::Stop);
        assert!(sink.take_error().is_some());
        assert!(sink.found.is_empty());
    }

    #[test]
    fn commit_evidence_does_not_ignore_unreadable_command_arguments() {
        let world = crate::test_world("rewind_json_failure");
        let path = world.home.join("input.json");
        let body = r#"{"command":"git commit -m test"}"#;
        std::fs::write(&path, body).unwrap();
        let input = crate::json::Json::source(&path, 0, body.len() as u64).unwrap();
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Bash".into(),
            namespace: None,
            input,
        });
        std::fs::remove_file(path).unwrap();
        let mut evidence = CommitEvidenceSink::new(vec!["abcdef123".into()], None);
        assert_eq!(evidence.message(message.clone()), Flow::Stop);
        assert!(evidence.take_error().is_some());
        assert!(evidence.found.is_empty());
        let mut scan = ScanSink {
            offsets: Vec::new(),
            unstamped: false,
            boundaries: Vec::new(),
            uuid_off: HashMap::new(),
            stop_after: None,
            evidence: Some(CommitEvidenceSink::new(vec!["abcdef123".into()], None)),
            foreign: Vec::new(),
        };
        assert_eq!(scan.message(message), Flow::Stop);
        assert!(scan.take_error().is_some());
    }
}
