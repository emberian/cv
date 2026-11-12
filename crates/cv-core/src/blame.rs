//! Git-to-session code provenance (`cv blame`, `GET /api/blame`).
//!
//! Correlates commits (or `-L lo,hi` line ranges) to coding-agent sessions across five strictly
//! ranked evidence tiers:
//! 1. [`Strength::CommitShaAndContentHunk`] — transcript proves the commit SHA (`git commit`/`push`
//!    output) **and** cataloged 64-bit normalized hunk/line hashes match the commit's diff.
//! 2. [`Strength::CommitSha`] (`"exact: commit sha"`) — transcript proves the commit SHA.
//! 3. [`Strength::ContentHunk`] (`"exact: content hunk"`) — cataloged 64-bit normalized hunk/line
//!    hashes ([`crate::tools::classify`]) match the commit's added hunks/lines. Survives
//!    `git rebase`, `commit --amend`, `cherry-pick`, and squash-merges outside any time window,
//!    and under `-L a,b` scopes matches to the session that wrote lines `a..=b`.
//! 4. [`Strength::Timed`] (`"heuristic: time window"`) — timestamped `file_edit` inside
//!    `[commit - 6h, commit + 15m]`.
//! 5. [`Strength::SessionSpan`] (`"weak: session span"`) — session span covers commit time when
//!    individual event timestamps are absent.

use crate::events::{self, EditEvent, EditHunkEvent};
use crate::ir::Harness;
use crate::rewind::{CommitEvidence, CommitEvidenceSink, EvidenceKind};
use crate::tools::classify::{extract_diff_hunk_hashes, hash_normalized_hunk, hash_normalized_line, normalize_path};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pre-commit correlation window for [`Strength::Timed`] (6 hours).
pub const WINDOW_BEFORE_SECS: i64 = 6 * 60 * 60;
/// Post-commit grace window for [`Strength::Timed`] (15 minutes).
pub const WINDOW_AFTER_SECS: i64 = 15 * 60;
/// Maximum commits inspected on whole-file blame without `-L`.
pub const MAX_COMMITS: usize = 15;
/// Maximum session attributions emitted per commit.
pub const MATCHES_PER_COMMIT: usize = 3;
/// Messages of context on each side of a matched message in `cv show --range`.
pub const RANGE_CONTEXT: i64 = 3;

/// Caller input validation error (mapped to HTTP 400 on `/api/blame` and usage error on CLI).
#[derive(Debug, Clone)]
pub struct BlameInputError(pub String);

impl std::fmt::Display for BlameInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BlameInputError {}

/// Provenance evidence tier, ordered strongest-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    /// Both exact SHA output in transcript and normalized content-hunk fingerprint match.
    CommitShaAndContentHunk,
    /// Exact commit SHA recorded in tool output (`git commit` / `git push`).
    CommitSha,
    /// Normalized 64-bit diff hunk / line fingerprint match (rebase-, cherry-pick-, amend-proof).
    ContentHunk,
    /// Timestamped edit inside `[commit - 6h, commit + 15m]`.
    Timed,
    /// Session span `[created_at, updated_at + 15m]` covers commit time (no event timestamps).
    SessionSpan,
}

impl Strength {
    pub fn label(self) -> &'static str {
        match self {
            Self::CommitShaAndContentHunk => "exact: commit sha + content hunk",
            Self::CommitSha => "exact: commit sha",
            Self::ContentHunk => "exact: content hunk",
            Self::Timed => "heuristic: time window",
            Self::SessionSpan => "weak: session span",
        }
    }

    pub fn is_exact_sha(self) -> bool {
        matches!(self, Self::CommitShaAndContentHunk | Self::CommitSha)
    }

    pub fn is_exact(self) -> bool {
        matches!(
            self,
            Self::CommitShaAndContentHunk | Self::CommitSha | Self::ContentHunk
        )
    }
}

/// One session attributed to a commit, ranked best-first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAttribution {
    pub harness: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub strength: Strength,
    pub label: String,
    pub msg_idx: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
    /// `commit_time - edit_ts` when an edit timestamp was correlated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_secs: Option<i64>,
    pub matched_hunks: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_evidence: Option<String>,
    pub cwd_in_repo: bool,
    pub range: (usize, usize),
}

/// One commit touching the blamed file or `-L` line range, with its top session attributions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitBlame {
    pub sha: String,
    pub short_sha: String,
    pub time: i64,
    pub summary: String,
    pub matches: Vec<SessionAttribution>,
}

/// Event-only session match when `file` lies outside any git worktree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventOnlySession {
    pub harness: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub msg_idx: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
    pub range: (usize, usize),
}

/// Structured blame result shared identically by `cv blame --json`, `cv blame`, and `GET /api/blame`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameReport {
    pub file: String,
    pub abs_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u32, u32)>,
    pub in_git_repo: bool,
    pub cold_catalog: bool,
    pub total_commits: usize,
    pub matched_commits: usize,
    pub commits: Vec<CommitBlame>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_only_sessions: Vec<EventOnlySession>,
}

/// Parse `-L <line>[,<endline>]` into an inclusive 1-based `(lo, hi)`.
pub fn parse_line_spec(spec: &str) -> Result<(u32, u32)> {
    let (a, b) = match spec.split_once(',') {
        None => (spec, spec),
        Some((a, b)) => (a, b),
    };
    let lo: u32 = a
        .trim()
        .parse()
        .map_err(|_| BlameInputError(format!("bad -L {spec:?}: expected <line> or <line>,<endline>")))?;
    let hi: u32 = b
        .trim()
        .parse()
        .map_err(|_| BlameInputError(format!("bad -L {spec:?}: end line must be a number")))?;
    if lo == 0 {
        return Err(BlameInputError(format!("bad -L {spec:?}: lines are 1-based")).into());
    }
    if hi < lo {
        return Err(BlameInputError(format!("bad -L {spec:?}: end line ({hi}) is before start ({lo})")).into());
    }
    Ok((lo, hi))
}

/// The `cv show --range` window around an event at `msg_idx`.
pub fn range_hint(msg_idx: i64) -> (usize, usize) {
    let start = (msg_idx - RANGE_CONTEXT).max(0) as usize;
    let end = (msg_idx + RANGE_CONTEXT).max(1) as usize;
    (start, end)
}

#[derive(Debug, Clone)]
pub struct CommitTarget {
    pub sha: String,
    pub short: String,
    pub time: i64,
    pub summary: String,
    pub hunk_hashes: HashSet<u64>,
}

fn run_git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "`git` not found on PATH — cv blame needs git to correlate commits \
                 (try `cv touched <path>` for catalog-only matches)"
            )
        } else {
            anyhow!("failed to run git: {e}")
        }
    })
}

fn git_stdout(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_git(dir, args)?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&"?"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn repo_toplevel(abs: &Path) -> Result<Option<PathBuf>> {
    let mut probe = abs.parent();
    while let Some(p) = probe {
        if p.exists() {
            break;
        }
        probe = p.parent();
    }
    let dir = match probe {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let out = run_git(dir, &["rev-parse", "--show-toplevel"])?;
    if !out.status.success() {
        return Ok(None);
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!top.is_empty()).then(|| normalize_path(Path::new(&top), None)))
}

/// Extract all 64-bit normalized hunk and line fingerprints from a unified diff body without
/// capping at `MAX_HUNK_HASHES_PER_EDIT`, so multi-hundred-line commits still retain fingerprints
/// for every hunk and line added in the commit.
pub fn collect_commit_diff_hashes(diff: &str) -> HashSet<u64> {
    let mut set: HashSet<u64> = extract_diff_hunk_hashes(diff).into_iter().collect();
    let mut in_hunk = false;
    let mut run: Vec<&str> = Vec::new();
    let flush_run = |run: &mut Vec<&str>, set: &mut HashSet<u64>| {
        if !run.is_empty() {
            if let Some(bh) = hash_normalized_hunk(run.iter().copied()) {
                set.insert(bh);
            }
            run.clear();
        }
    };
    for raw in diff.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("@@") {
            flush_run(&mut run, &mut set);
            in_hunk = true;
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") || line.starts_with("+++ ") {
            flush_run(&mut run, &mut set);
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if let Some(lh) = hash_normalized_line(added) {
                set.insert(lh);
            }
            run.push(added);
        } else if line.starts_with('\\') {
            continue;
        } else {
            flush_run(&mut run, &mut set);
        }
    }
    flush_run(&mut run, &mut set);
    set
}

/// Whole-file commit history: total commit count via `rev-list`/`log`, plus diffs for the top
/// [`MAX_COMMITS`] commits in a single `git log -p -U0 --follow` invocation.
fn file_commits_with_hunks(root: &Path, rel: &str) -> Result<(Vec<CommitTarget>, usize)> {
    let count_out = git_stdout(root, &["log", "--follow", "--format=%H", "--", rel])?;
    let total = count_out.lines().filter(|l| !l.trim().is_empty()).count();
    if total == 0 {
        return Ok((Vec::new(), 0));
    }
    let max_arg = format!("-n{MAX_COMMITS}");
    let out = git_stdout(
        root,
        &[
            "log",
            "--follow",
            &max_arg,
            "-p",
            "--unified=0",
            "--no-ext-diff",
            "--no-textconv",
            "--format=%x00COMMIT%x00%H%x1f%ct%x1f%h%x1f%s",
            "--",
            rel,
        ],
    )?;
    let mut commits = Vec::new();
    for chunk in out.split("\0COMMIT\0").skip(1) {
        let (header, diff) = chunk.split_once('\n').unwrap_or((chunk, ""));
        let mut f = header.trim_end_matches('\r').split('\u{1f}');
        let (Some(sha), Some(ct), Some(short), summary) = (f.next(), f.next(), f.next(), f.next()) else {
            continue;
        };
        let Ok(time) = ct.parse::<i64>() else { continue };
        commits.push(CommitTarget {
            sha: sha.to_string(),
            short: short.to_string(),
            time,
            summary: summary.unwrap_or("").to_string(),
            hunk_hashes: collect_commit_diff_hashes(diff),
        });
    }
    Ok((commits, total))
}

fn blame_header_sha(line: &str) -> Option<&str> {
    let mut it = line.split(' ');
    let sha = it.next()?;
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let (a, b) = (it.next()?, it.next()?);
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    (numeric(a) && numeric(b)).then_some(sha)
}

/// Commits owning lines `lo..=hi` via `git blame -L lo,hi --porcelain`, enriched with normalized
/// hunk and line hashes strictly for the blamed lines in `lo..=hi` (plus `git log -L` hunks for
/// those commits when available).
fn line_commits_with_hunks(root: &Path, rel: &str, lo: u32, hi: u32) -> Result<(Vec<CommitTarget>, usize)> {
    let range = format!("{lo},{hi}");
    let out = git_stdout(root, &["blame", "-L", &range, "--porcelain", "--", rel])?;

    #[derive(Default)]
    struct Meta {
        committer_time: Option<i64>,
        author_time: Option<i64>,
        summary: Option<String>,
        hashes: HashSet<u64>,
    }

    let mut order: Vec<String> = Vec::new();
    let mut metas: HashMap<String, Meta> = HashMap::new();
    let mut cur_sha: Option<String> = None;
    let mut run_sha: Option<String> = None;
    let mut run_lines: Vec<String> = Vec::new();

    let flush_run = |r_sha: &mut Option<String>, r_lines: &mut Vec<String>, metas: &mut HashMap<String, Meta>| {
        if let Some(sha) = r_sha.take() {
            if !r_lines.is_empty() {
                if let Some(bh) = hash_normalized_hunk(r_lines.iter().map(String::as_str)) {
                    metas.entry(sha).or_default().hashes.insert(bh);
                }
                r_lines.clear();
            }
        }
    };

    for line in out.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            if let Some(sha) = &cur_sha {
                // Uncommitted working-tree lines use the all-zero pseudo-SHA; skip uncommitted rows.
                if !sha.bytes().all(|b| b == b'0') {
                    if let Some(lh) = hash_normalized_line(content) {
                        metas.entry(sha.clone()).or_default().hashes.insert(lh);
                    }
                    if run_sha.as_deref() != Some(sha.as_str()) {
                        flush_run(&mut run_sha, &mut run_lines, &mut metas);
                        run_sha = Some(sha.clone());
                    }
                    run_lines.push(content.to_string());
                }
            }
            continue;
        }
        if let Some(sha) = blame_header_sha(line) {
            if !sha.bytes().all(|b| b == b'0') {
                if let std::collections::hash_map::Entry::Vacant(entry) = metas.entry(sha.to_string()) {
                    order.push(sha.to_string());
                    entry.insert(Meta::default());
                }
            }
            cur_sha = Some(sha.to_string());
        } else if let Some(c) = &cur_sha {
            if !c.bytes().all(|b| b == b'0') {
                let m = metas.entry(c.clone()).or_default();
                if let Some(t) = line.strip_prefix("committer-time ") {
                    m.committer_time = t.trim().parse().ok();
                } else if let Some(t) = line.strip_prefix("author-time ") {
                    m.author_time = t.trim().parse().ok();
                } else if let Some(s) = line.strip_prefix("summary ") {
                    m.summary = Some(s.to_string());
                }
            }
        }
    }
    flush_run(&mut run_sha, &mut run_lines, &mut metas);

    // Enrich owning commits with `git log -L lo,hi:<rel>` diff hunks (scoped strictly to `lo..=hi`).
    let log_l_spec = format!("{lo},{hi}:{rel}");
    if let Ok(log_out) = run_git(
        root,
        &[
            "log",
            "-L",
            &log_l_spec,
            "-n15",
            "--no-ext-diff",
            "--format=%x00COMMIT%x00%H",
        ],
    ) {
        if log_out.status.success() {
            let text = String::from_utf8_lossy(&log_out.stdout);
            for chunk in text.split("\0COMMIT\0").skip(1) {
                let (sha_line, diff) = chunk.split_once('\n').unwrap_or((chunk, ""));
                let sha = sha_line.trim();
                if let Some(meta) = metas.get_mut(sha) {
                    meta.hashes.extend(collect_commit_diff_hashes(diff));
                }
            }
        }
    }

    let mut commits: Vec<CommitTarget> = order
        .into_iter()
        .map(|sha| {
            let m = metas.remove(&sha).unwrap_or_default();
            CommitTarget {
                short: sha.chars().take(7).collect(),
                time: m.committer_time.or(m.author_time).unwrap_or(0),
                summary: m.summary.unwrap_or_default(),
                hunk_hashes: m.hashes,
                sha,
            }
        })
        .collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.time));
    let total = commits.len();
    commits.truncate(MAX_COMMITS);
    Ok((commits, total))
}

fn cwd_in_repo(cwd: Option<&str>, root: &Path) -> bool {
    let Some(c) = cwd.filter(|s| !s.is_empty()) else {
        return false;
    };
    let norm = normalize_path(Path::new(c), None);
    norm.starts_with(root) || Path::new(c).starts_with(root)
}

/// Candidate edit match for one session on one commit before merging with `CommitEvidence`.
#[derive(Debug, Clone)]
pub struct EditCandidate {
    pub event_idx: usize,
    pub strength: Strength, // ContentHunk | Timed | SessionSpan
    pub matched_hunks: usize,
    pub delta_secs: Option<i64>,
    pub cwd_in_repo: bool,
}

fn edit_candidate_rank(c: &EditCandidate, msg_idx: i64) -> (u8, usize, u8, u8, i64, i64) {
    let tier = match c.strength {
        Strength::ContentHunk => 0,
        Strength::Timed => 1,
        Strength::SessionSpan => 2,
        Strength::CommitShaAndContentHunk | Strength::CommitSha => 0,
    };
    let delta = c.delta_secs.unwrap_or(i64::MAX / 2);
    (
        tier,
        usize::MAX - c.matched_hunks,
        u8::from(!c.cwd_in_repo),
        u8::from(delta < 0),
        delta.saturating_abs(),
        -msg_idx,
    )
}

/// Correlate `EditHunkEvent`s against one commit (`commit_time` + `target_hashes`).
///
/// - Any edit with `matched_hunks > 0` matches as [`Strength::ContentHunk`] regardless of time
///   window (surviving rebases, cherry-picks, amends, and squashes hours or days later).
/// - When `line_scoped` is `true` and at least one session matched [`Strength::ContentHunk`] on the
///   requested `-L` lines, heuristic `Timed` / `SessionSpan` candidates from unrelated lines in the
///   same commit are suppressed so `-L` scopes strictly to the line author(s).
pub fn correlate_hunks(
    commit_time: i64,
    target_hashes: &HashSet<u64>,
    edits: &[EditHunkEvent],
    repo_root: &Path,
    line_scoped: bool,
) -> Vec<EditCandidate> {
    let mut best: HashMap<(&str, &str), EditCandidate> = HashMap::new();
    let mut any_hunk_match = false;

    for (i, eh) in edits.iter().enumerate() {
        let e = &eh.edit;
        let matched_hunks = if target_hashes.is_empty() || eh.hunk_hashes.is_empty() {
            0
        } else {
            eh.hunk_hashes.iter().filter(|h| target_hashes.contains(h)).count()
        };
        let in_repo = cwd_in_repo(e.cwd.as_deref(), repo_root);
        let delta_secs = e.ts.and_then(|ts| commit_time.checked_sub(ts));

        let cand = if matched_hunks > 0 {
            any_hunk_match = true;
            EditCandidate {
                event_idx: i,
                strength: Strength::ContentHunk,
                matched_hunks,
                delta_secs,
                cwd_in_repo: in_repo,
            }
        } else if let Some(delta) = delta_secs {
            if !(-WINDOW_AFTER_SECS..=WINDOW_BEFORE_SECS).contains(&delta) {
                continue;
            }
            EditCandidate {
                event_idx: i,
                strength: Strength::Timed,
                matched_hunks: 0,
                delta_secs: Some(delta),
                cwd_in_repo: in_repo,
            }
        } else {
            let (Some(c), Some(u)) = (e.created_at, e.updated_at) else {
                continue;
            };
            if commit_time < c || commit_time > u.saturating_add(WINDOW_AFTER_SECS) {
                continue;
            }
            EditCandidate {
                event_idx: i,
                strength: Strength::SessionSpan,
                matched_hunks: 0,
                delta_secs: None,
                cwd_in_repo: in_repo,
            }
        };

        let key = (e.harness.as_str(), e.session_id.as_str());
        match best.get(&key) {
            Some(prev)
                if edit_candidate_rank(prev, edits[prev.event_idx].edit.msg_idx)
                    <= edit_candidate_rank(&cand, e.msg_idx) => {}
            _ => {
                best.insert(key, cand);
            }
        }
    }

    let mut out: Vec<EditCandidate> = best
        .into_values()
        .filter(|c| !(line_scoped && any_hunk_match && c.matched_hunks == 0))
        .collect();
    out.sort_by_key(|c| edit_candidate_rank(c, edits[c.event_idx].edit.msg_idx));
    out
}

/// Exact SHA match from scanning candidate transcripts (`CommitEvidence`).
#[derive(Debug, Clone)]
pub struct ExactShaMatch {
    pub harness: String,
    pub session_id: String,
    pub title: Option<String>,
    pub cwd_in_repo: bool,
    pub evidence: CommitEvidence,
}

pub fn candidate_ids(edits: &[EditEvent]) -> Vec<(String, String)> {
    let mut ids: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for e in edits {
        for (h, id) in std::iter::once((e.harness.as_str(), e.session_id.as_str()))
            .chain(e.parent_id.as_deref().map(|p| (e.harness.as_str(), p)))
        {
            if seen.insert((h, id)) {
                ids.push((h.to_string(), id.to_string()));
            }
        }
    }
    ids
}

fn exact_sha_matches(
    commits: &[CommitTarget],
    edits: &[EditHunkEvent],
    repo_root: &Path,
) -> Result<Vec<ExactShaMatch>> {
    let Some(earliest) = commits.iter().map(|c| c.time).min() else {
        return Ok(Vec::new());
    };
    let plain_edits: Vec<EditEvent> = edits.iter().map(|eh| eh.edit.clone()).collect();
    let ids = candidate_ids(&plain_edits);
    let mut titles: HashMap<(&str, &str), &str> = HashMap::new();
    let mut cwds: HashMap<(&str, &str), &str> = HashMap::new();
    for eh in edits {
        let e = &eh.edit;
        if let Some(t) = e.title.as_deref() {
            titles.entry((e.harness.as_str(), e.session_id.as_str())).or_insert(t);
        }
        if let Some(c) = e.cwd.as_deref() {
            cwds.entry((e.harness.as_str(), e.session_id.as_str())).or_insert(c);
        }
    }
    let written_since = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_secs() as i64 + WINDOW_AFTER_SECS >= earliest)
    };
    let refs: Vec<crate::ir::SessionRef> = ids
        .iter()
        .filter_map(|(h, id)| crate::find_cheap(id, Harness::parse(h)).ok().flatten())
        .map(|(r, _)| r)
        .filter(|r| written_since(&r.path))
        .collect();
    let shas: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();
    let mut out = Vec::new();
    for (r, found) in crate::rewind::commit_evidence_many(refs, &shas)? {
        for sha in &shas {
            if let Some(ev) = CommitEvidenceSink::best(&found, &sha.to_ascii_lowercase()) {
                let cwd = cwds
                    .get(&(r.harness.as_str(), r.id.as_str()))
                    .copied()
                    .or_else(|| r.cwd.as_deref().and_then(|p| p.to_str()));
                out.push(ExactShaMatch {
                    harness: r.harness.as_str().to_string(),
                    title: titles
                        .get(&(r.harness.as_str(), r.id.as_str()))
                        .map(|t| (*t).to_string())
                        .or_else(|| r.title.clone()),
                    session_id: r.id.clone(),
                    cwd_in_repo: cwd_in_repo(cwd, repo_root),
                    evidence: ev,
                });
            }
        }
    }
    Ok(out)
}

/// Combine exact commit-SHA evidence with content-hunk / timed candidates for one commit.
///
/// Strict priority:
/// 1. `CommitShaAndContentHunk`
/// 2. `CommitSha`
/// 3. `ContentHunk`
/// 4. `Timed`
/// 5. `SessionSpan`
pub fn rank_attributions(
    sha: &str,
    exact: &[ExactShaMatch],
    edit_cands: Vec<EditCandidate>,
    edits: &[EditHunkEvent],
    line_scoped: bool,
) -> Vec<SessionAttribution> {
    let sha_lc = sha.to_ascii_lowercase();
    let mut proven: Vec<&ExactShaMatch> = exact
        .iter()
        .filter(|x| crate::rewind::output_names_sha(&x.evidence.sha, &sha_lc))
        .collect();
    proven.sort_by_key(|x| {
        (
            x.evidence.kind != EvidenceKind::Created,
            x.evidence.ts,
            x.evidence.msg_idx,
        )
    });

    let mut cand_by_session: HashMap<(&str, &str), EditCandidate> = HashMap::new();
    for c in &edit_cands {
        let e = &edits[c.event_idx].edit;
        cand_by_session.insert((e.harness.as_str(), e.session_id.as_str()), c.clone());
    }
    let any_line_hunk = line_scoped && edit_cands.iter().any(|c| c.matched_hunks > 0);

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut combined: Vec<SessionAttribution> = Vec::new();

    // First collect sessions with `CommitShaAndContentHunk` (both SHA proof + content hunk!).
    for x in &proven {
        let key_ref = (x.harness.as_str(), x.session_id.as_str());
        if let Some(cand) = cand_by_session.get(&key_ref).filter(|c| c.matched_hunks > 0) {
            if seen.insert((x.harness.clone(), x.session_id.clone())) {
                let ev_kind = match x.evidence.kind {
                    EvidenceKind::Created => "created",
                    EvidenceKind::Pushed => "pushed",
                };
                let strength = Strength::CommitShaAndContentHunk;
                let msg_idx = x.evidence.msg_idx as i64;
                combined.push(SessionAttribution {
                    harness: x.harness.clone(),
                    session_id: x.session_id.clone(),
                    title: x.title.clone(),
                    strength,
                    label: strength.label().to_string(),
                    msg_idx,
                    ts: x.evidence.ts.or(edits[cand.event_idx].edit.ts),
                    delta_secs: cand.delta_secs,
                    matched_hunks: cand.matched_hunks,
                    commit_evidence: Some(ev_kind.to_string()),
                    cwd_in_repo: x.cwd_in_repo || cand.cwd_in_repo,
                    range: range_hint(msg_idx),
                });
            }
        }
    }

    // Second: sessions with `CommitSha` only (unless `-L` already matched a specific line-hunk
    // author in another session and this committer did not write those lines — wait: if no line-hunk
    // matched, or if we keep `CommitSha` right after `CommitShaAndContentHunk` when `!any_line_hunk`,
    // wait: under `-L a,b`, if Session B wrote lines `a..=b` and Session A committed the whole repo,
    // scoping `-L a,b` to line authors puts `ContentHunk` of lines `a..=b` ahead or suppresses
    // non-author committer? Brief: "`CommitSha + ContentHunk` highest, then `CommitSha`, then
    // `ContentHunk`, all outranking heuristic `Timed`", AND when `-L a,b` is passed, scopes session
    // matches to the session that actually wrote those lines rather than every edit in the whole
    // commit! Keeping `CommitSha` when `!any_line_hunk` (or after `ContentHunk`? No: keeping
    // `CommitSha` at tier 2 when `!any_line_hunk`, and if `any_line_hunk` is true, only sessions
    // that actually wrote those lines (`matched_hunks > 0`) match `-L a,b`!).
    if !any_line_hunk {
        for x in &proven {
            if seen.insert((x.harness.clone(), x.session_id.clone())) {
                let key_ref = (x.harness.as_str(), x.session_id.as_str());
                let cand = cand_by_session.get(&key_ref);
                let ev_kind = match x.evidence.kind {
                    EvidenceKind::Created => "created",
                    EvidenceKind::Pushed => "pushed",
                };
                let strength = Strength::CommitSha;
                let msg_idx = x.evidence.msg_idx as i64;
                combined.push(SessionAttribution {
                    harness: x.harness.clone(),
                    session_id: x.session_id.clone(),
                    title: x.title.clone(),
                    strength,
                    label: strength.label().to_string(),
                    msg_idx,
                    ts: x.evidence.ts,
                    delta_secs: cand.and_then(|c| c.delta_secs),
                    matched_hunks: 0,
                    commit_evidence: Some(ev_kind.to_string()),
                    cwd_in_repo: x.cwd_in_repo || cand.is_some_and(|c| c.cwd_in_repo),
                    range: range_hint(msg_idx),
                });
            }
        }
    }

    // Third: `ContentHunk`, `Timed`, `SessionSpan` in `edit_cands` order.
    for cand in edit_cands {
        let e = &edits[cand.event_idx].edit;
        if seen.insert((e.harness.clone(), e.session_id.clone())) {
            combined.push(SessionAttribution {
                harness: e.harness.clone(),
                session_id: e.session_id.clone(),
                title: e.title.clone(),
                strength: cand.strength,
                label: cand.strength.label().to_string(),
                msg_idx: e.msg_idx,
                ts: e.ts.or(e.updated_at),
                delta_secs: cand.delta_secs,
                matched_hunks: cand.matched_hunks,
                commit_evidence: None,
                cwd_in_repo: cand.cwd_in_repo,
                range: range_hint(e.msg_idx),
            });
        }
    }

    combined
}

fn absolutize_input(file: &str) -> Result<PathBuf> {
    if file.trim().is_empty() {
        return Err(BlameInputError("missing file parameter".into()).into());
    }
    let cwd = crate::env::current_dir().context("cannot resolve current directory")?;
    Ok(normalize_path(Path::new(file), Some(&cwd)))
}

/// Compute structured blame provenance for `file` and optional `lines` (`"a"` or `"a,b"`).
pub fn blame(file: &str, lines: Option<&str>) -> Result<BlameReport> {
    let parsed_lines = match lines {
        Some(spec) => Some(parse_line_spec(spec)?),
        None => None,
    };
    let abs = absolutize_input(file)?;
    let abs_str = abs.to_string_lossy().into_owned();

    let Some(root) = repo_toplevel(&abs)? else {
        let rel = file.trim_start_matches("./");
        let edits = events::edits_touching_file(&abs_str, rel)?;
        let cold_catalog = edits.is_empty() && !events::catalog_has_events();
        let mut latest: Vec<&EditEvent> = Vec::new();
        let mut positions: HashMap<(&str, &str), usize> = HashMap::new();
        for e in &edits {
            let idx = *positions
                .entry((e.harness.as_str(), e.session_id.as_str()))
                .or_insert_with(|| {
                    latest.push(e);
                    latest.len() - 1
                });
            if e.ts.unwrap_or(i64::MIN) >= latest[idx].ts.unwrap_or(i64::MIN) {
                latest[idx] = e;
            }
        }
        latest.sort_by_key(|e| std::cmp::Reverse(e.ts.or(e.updated_at)));
        let event_only_sessions = latest
            .into_iter()
            .map(|e| EventOnlySession {
                harness: e.harness.clone(),
                session_id: e.session_id.clone(),
                title: e.title.clone(),
                msg_idx: e.msg_idx,
                ts: e.ts.or(e.updated_at),
                range: range_hint(e.msg_idx),
            })
            .collect();
        return Ok(BlameReport {
            file: file.to_string(),
            abs_path: abs_str,
            repo_root: None,
            lines: parsed_lines,
            in_git_repo: false,
            cold_catalog,
            total_commits: 0,
            matched_commits: 0,
            commits: Vec::new(),
            event_only_sessions,
        });
    };

    let rel = abs
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().into_owned())
        .with_context(|| format!("{} is not inside the repo at {}", abs.display(), root.display()))?;

    let (commits_raw, total_commits) = match parsed_lines {
        None => file_commits_with_hunks(&root, &rel)?,
        Some((lo, hi)) => line_commits_with_hunks(&root, &rel, lo, hi)?,
    };

    if commits_raw.is_empty() {
        return Ok(BlameReport {
            file: rel,
            abs_path: abs_str,
            repo_root: Some(root.to_string_lossy().into_owned()),
            lines: parsed_lines,
            in_git_repo: true,
            cold_catalog: false,
            total_commits: 0,
            matched_commits: 0,
            commits: Vec::new(),
            event_only_sessions: Vec::new(),
        });
    }

    let edits = events::edits_with_hunks_touching_file(&abs_str, &rel)?;
    let cold_catalog = edits.is_empty() && !events::catalog_has_events();
    let exact = exact_sha_matches(&commits_raw, &edits, &root)?;
    let line_scoped = parsed_lines.is_some();

    let mut matched_commits = 0usize;
    let mut commits_out = Vec::with_capacity(commits_raw.len());
    for c in &commits_raw {
        let edit_cands = correlate_hunks(c.time, &c.hunk_hashes, &edits, &root, line_scoped);
        let mut ranked = rank_attributions(&c.sha, &exact, edit_cands, &edits, line_scoped);
        if !ranked.is_empty() {
            matched_commits += 1;
        }
        ranked.truncate(MATCHES_PER_COMMIT);
        commits_out.push(CommitBlame {
            sha: c.sha.clone(),
            short_sha: c.short.clone(),
            time: c.time,
            summary: c.summary.clone(),
            matches: ranked,
        });
    }

    Ok(BlameReport {
        file: rel,
        abs_path: abs_str,
        repo_root: Some(root.to_string_lossy().into_owned()),
        lines: parsed_lines,
        in_git_repo: true,
        cold_catalog,
        total_commits,
        matched_commits,
        commits: commits_out,
        event_only_sessions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk_ev(
        sid: &str,
        msg_idx: i64,
        ts: Option<i64>,
        cwd: Option<&str>,
        span: Option<(i64, i64)>,
        hunks: Vec<u64>,
    ) -> EditHunkEvent {
        EditHunkEvent {
            edit: EditEvent {
                harness: "claude".into(),
                session_id: sid.into(),
                msg_idx,
                ts,
                target: Some("/repo/src/lib.rs".into()),
                title: None,
                cwd: cwd.map(Into::into),
                created_at: span.map(|(c, _)| c),
                updated_at: span.map(|(_, u)| u),
                parent_id: None,
            },
            hunk_hashes: hunks,
        }
    }

    #[test]
    fn content_hunk_survives_rebase_days_outside_time_window_and_outranks_timed() {
        let root = Path::new("/repo");
        let diff = "@@ -1 +1,2 @@\n+fn rebased_feature() -> u64 {\n+    42\n+}\n";
        let target_hashes = collect_commit_diff_hashes(diff);
        assert!(!target_hashes.is_empty());

        let author_hunks =
            crate::tools::classify::extract_content_hunk_hashes("fn rebased_feature() -> u64 {\n    42\n}");
        let edit_ts = 1_700_000_000;
        // Commit was rebased 5 days later — far outside the 6h window!
        let rebased_commit_ts = edit_ts + 5 * 86_400;
        let edits = vec![
            // Unrelated session that edited another part of the file 2 minutes before the rebase commit:
            hunk_ev(
                "recent-noise",
                3,
                Some(rebased_commit_ts - 120),
                Some("/repo"),
                None,
                crate::tools::classify::extract_content_hunk_hashes("fn unrelated_tweak() {}"),
            ),
            // Original author session from 5 days earlier:
            hunk_ev(
                "true-author",
                10,
                Some(edit_ts),
                Some("/other/worktree"),
                None,
                author_hunks,
            ),
        ];

        let cands = correlate_hunks(rebased_commit_ts, &target_hashes, &edits, root, false);
        assert_eq!(cands.len(), 2);
        assert_eq!(edits[cands[0].event_idx].edit.session_id, "true-author");
        assert_eq!(cands[0].strength, Strength::ContentHunk);
        assert_eq!(cands[0].strength.label(), "exact: content hunk");
        assert_eq!(edits[cands[1].event_idx].edit.session_id, "recent-noise");
        assert_eq!(cands[1].strength, Strength::Timed);
        assert_eq!(cands[1].strength.label(), "heuristic: time window");
    }

    #[test]
    fn line_scoped_blame_isolates_line_author_from_other_edits_in_same_commit() {
        let root = Path::new("/repo");
        let commit_ts = 1_700_000_500;
        let lines_50_60_hashes: HashSet<u64> = [
            hash_normalized_hunk(["let specific_line = compute_beta(x);"]).unwrap(),
            hash_normalized_line("let specific_line = compute_beta(x);").unwrap(),
        ]
        .into_iter()
        .collect();

        let edits = vec![
            hunk_ev(
                "session-lines-1-10",
                2,
                Some(commit_ts - 60),
                Some("/repo"),
                None,
                crate::tools::classify::extract_content_hunk_hashes("let header_line = compute_alpha();"),
            ),
            hunk_ev(
                "session-lines-50-60",
                5,
                Some(commit_ts - 3600),
                Some("/repo"),
                None,
                crate::tools::classify::extract_content_hunk_hashes("let specific_line = compute_beta(x);"),
            ),
        ];

        let scoped = correlate_hunks(commit_ts, &lines_50_60_hashes, &edits, root, true);
        assert_eq!(scoped.len(), 1, "unrelated edit in same commit must be scoped out");
        assert_eq!(edits[scoped[0].event_idx].edit.session_id, "session-lines-50-60");
        assert_eq!(scoped[0].strength, Strength::ContentHunk);
    }

    #[test]
    fn strength_priority_orders_sha_plus_hunk_then_sha_then_hunk_then_timed() {
        let sha = "deadbeefcafebabe00112233445566778899aabb";
        let commit_ts = 1_700_001_000;
        let h_hash = hash_normalized_line("fn exact_match() {}").unwrap();
        let target_hashes: HashSet<u64> = [h_hash].into_iter().collect();

        let edits = vec![
            hunk_ev("timed-only", 1, Some(commit_ts - 30), Some("/repo"), None, vec![]),
            hunk_ev(
                "hunk-only",
                2,
                Some(commit_ts - 99_999),
                Some("/repo"),
                None,
                vec![h_hash],
            ),
            hunk_ev("sha-only", 3, Some(commit_ts - 600), Some("/repo"), None, vec![]),
            hunk_ev(
                "both-exact",
                4,
                Some(commit_ts - 1200),
                Some("/repo"),
                None,
                vec![h_hash],
            ),
        ];
        let exact = vec![
            ExactShaMatch {
                harness: "claude".into(),
                session_id: "sha-only".into(),
                title: None,
                cwd_in_repo: true,
                evidence: CommitEvidence {
                    sha: sha[..7].into(),
                    kind: EvidenceKind::Created,
                    msg_idx: 8,
                    ts: Some(commit_ts),
                    tool_use_id: "toolu_1".into(),
                    command: "git commit -m x".into(),
                    byte_offset: None,
                },
            },
            ExactShaMatch {
                harness: "claude".into(),
                session_id: "both-exact".into(),
                title: None,
                cwd_in_repo: true,
                evidence: CommitEvidence {
                    sha: sha[..7].into(),
                    kind: EvidenceKind::Created,
                    msg_idx: 9,
                    ts: Some(commit_ts),
                    tool_use_id: "toolu_2".into(),
                    command: "git commit -m x".into(),
                    byte_offset: None,
                },
            },
        ];

        let cands = correlate_hunks(commit_ts, &target_hashes, &edits, Path::new("/repo"), false);
        let ranked = rank_attributions(sha, &exact, cands, &edits, false);
        let order: Vec<(&str, Strength)> = ranked.iter().map(|r| (r.session_id.as_str(), r.strength)).collect();
        assert_eq!(
            order,
            vec![
                ("both-exact", Strength::CommitShaAndContentHunk),
                ("sha-only", Strength::CommitSha),
                ("hunk-only", Strength::ContentHunk),
                ("timed-only", Strength::Timed),
            ]
        );
    }
}
