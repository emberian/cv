//! Canonical git command and commit-evidence miners shared by `rewind`, `blame`, `distill`,
//! `overview`, and `cv-search`.
//!
//! Shell tool calls routinely wrap `git` behind directory hops (`cd /repo && ...`), global flags
//! (`git -C /repo -c commit.gpgsign=false --no-pager ...`), quiet commits (`git commit -q -F msg
//! && git log --oneline -1`), and bracketed commit summaries (`[main 1a2b3c4] subject`,
//! `[feat/x (root-commit) 1a2b3c4] init`, `[detached HEAD 1a2b3c4] wip`). Parsing those forms in
//! one module prevents subtle drift across provenance, blame, distillation, and search indexing.

use crate::ir::{Block, Message, SessionRef};
use crate::lazy::Resolver;
use crate::offsets::OFFSET_KEY;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Shortest sha prefix accepted (git's own default abbreviation).
pub const MIN_SHA_LEN: usize = 7;

/// What a matching tool call did with the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// The command ran a commit-creating git verb (`commit`, `merge`, `cherry-pick`, `revert`)
    /// without `--dry-run`, and its output names the sha: the commit was made here.
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
pub(crate) struct ShaMatches<'a> {
    queries: &'a [String],
    hits: Vec<bool>,
    prefix: [u8; 64],
    len: usize,
}

impl<'a> ShaMatches<'a> {
    pub(crate) fn new(queries: &'a [String]) -> Self {
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
    pub(crate) fn push(&mut self, text: &str) {
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
    pub(crate) fn finish(mut self) -> Vec<bool> {
        self.end_token();
        self.hits
    }
}

fn clause_tail(after_verb: &str) -> &str {
    after_verb
        .split([';', '&', '|', '\n', ')'])
        .next()
        .unwrap_or(after_verb)
}

fn has_dry_run_flag(clause_args: &str) -> bool {
    clause_args.split_whitespace().any(|tok| {
        let t = tok.trim_matches(['\'', '"']);
        t == "--dry-run"
            || (t.starts_with('-')
                && !t.starts_with("--")
                && t[1..].bytes().all(|b| b.is_ascii_alphabetic())
                && t.contains('n'))
    })
}

/// The commit-relevant git verbs a shell command runs: `Created` when any is commit-creating
/// (without `--dry-run` / `-n`), else `Pushed` for a non-dry-run push, else `None`. Global options
/// between `git` and the verb (`-C <dir>`, `-c k=v`, `--no-pager`, …) are skipped.
pub fn git_evidence_kind(cmd: &str) -> Option<EvidenceKind> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[^\w.-])git(?:\s+(?:-[Cc]\s+\S+|--?[\w-]+(?:=\S+)?))*\s+(commit|merge|cherry-pick|revert|push)(?:$|[\s;&|)])",
        )
        .expect("static git verb regex")
    });
    let mut kind = None;
    for c in re.captures_iter(cmd) {
        let m = c.get(1).expect("verb group");
        let args = clause_tail(&cmd[m.end()..]);
        if matches!(&c[1], "commit" | "push") && has_dry_run_flag(args) {
            continue;
        }
        match &c[1] {
            "push" => kind = kind.or(Some(EvidenceKind::Pushed)),
            _ => return Some(EvidenceKind::Created),
        }
    }
    kind
}

/// Parsed `git commit` / `cherry-pick` / `merge` bracketed summary line (`[<head> <sha>] <subject>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitSummary<'a> {
    /// Branch name (`Some("main")`), or `None` for `[detached HEAD <sha>]`.
    pub branch: Option<&'a str>,
    /// Display label inside brackets (`"main"`, `"feat/x"`, or `"detached HEAD"`).
    pub header_label: &'a str,
    /// Abbreviated or full commit SHA (`7..=40` hex digits).
    pub sha: &'a str,
    /// Trimmed commit subject following `]`.
    pub subject: &'a str,
}

/// Parse one line as git's `print_commit_summary` output (`[branch 1a2b3c4] subject`,
/// `[branch (root-commit) 1a2b3c4] subject`, or `[detached HEAD 1a2b3c4] subject`).
pub fn parse_commit_summary_line(line: &str) -> Option<CommitSummary<'_>> {
    let rest = line.trim_start().strip_prefix('[')?;
    let (inner, after) = rest.split_once(']')?;
    let mut words = inner.split_whitespace();
    let first = words.next()?;
    let second = words.next()?;
    let (branch, header_label, sha) = match words.next() {
        None => (Some(first), first, second),
        Some(third) => {
            if words.next().is_some() {
                return None;
            }
            if first == "detached" && second == "HEAD" {
                (None, "detached HEAD", third)
            } else if second.starts_with('(') && second.ends_with(')') {
                (Some(first), first, third)
            } else {
                return None;
            }
        }
    };
    let valid_sha =
        (MIN_SHA_LEN..=40).contains(&sha.len()) && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    valid_sha.then(|| CommitSummary {
        branch,
        header_label,
        sha,
        subject: after.trim(),
    })
}

/// First bracketed `git commit` summary line within the first `max_lines` of `output`.
pub fn first_commit_summary(output: &str, max_lines: usize) -> Option<CommitSummary<'_>> {
    output.lines().take(max_lines).find_map(parse_commit_summary_line)
}

/// Every non-empty-subject bracketed `git commit` summary line in `output` (line-start anchored).
pub fn parse_commit_summaries(output: &str) -> impl Iterator<Item = CommitSummary<'_>> {
    output.lines().filter_map(|line| {
        if !line.starts_with('[') {
            return None;
        }
        parse_commit_summary_line(line).filter(|s| !s.subject.is_empty())
    })
}

/// Extract message file paths passed to `git ... commit ... -F <file>` / `-qF <file>` / `--file=<file>`.
pub fn commit_message_files(cmd: &str) -> Vec<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"\bcommit\b[^|;&\n]*?\s(?:-[a-zA-Z]*F\s*|--file[=\s])['"]?([^\s'";&|]+)"#)
            .expect("static commit -F regex")
    });
    re.captures_iter(cmd)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect()
}

fn cd_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bcd\s+([^\s;&|)']+)").expect("static cd regex"))
}

fn git_log_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[^\w.-])git(?:\s+(?:-c\s+\S+|-C\s+(\S+)|--?[\w-]+(?:=\S+)?))*\s+log\b((?:\s+-[^\s|;&)]*)*)(?:\s+([^\s|;&)'-][^\s|;&)']*))?",
        )
        .expect("static git log regex")
    })
}

fn git_commit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[^\w.-])git(?:\s+(?:-c\s+\S+|-C\s+(\S+)|--?[\w-]+(?:=\S+)?))*\s+commit\b")
            .expect("static git commit regex")
    })
}

/// The `<sha> <subject>` lines of `out` (`git log --oneline`), in order. Any line after the first
/// must carry a hex letter `a..=f` in its sha so a pure decimal count (`wc -l`'s `1234567 total`)
/// is never mistaken for a commit.
pub fn oneline_commits(out: &str) -> impl Iterator<Item = (&str, &str)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^([0-9a-f]{7,40})\s+(\S.*)$").expect("static oneline regex"));
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .enumerate()
        .filter_map(move |(n, line)| {
            let m = re.captures(line)?;
            let (sha, subject) = (m.get(1)?.as_str(), m.get(2)?.as_str().trim());
            (n == 0 || sha.bytes().any(|b| b.is_ascii_alphabetic())).then_some((sha, subject))
        })
}

/// The repository a single `git log` in `cmd` reads, and the first `oneline` `(sha, subject)` it
/// printed: `(repo_key, sha, subject)`. `repo_key` is `[host:]dir`, plus ` [ref]` when the log
/// names a ref instead of HEAD. Commands with multiple `git log` invocations return `None`.
pub fn git_log_head<'a>(cmd: &str, out: &'a str, host: Option<&str>) -> Option<(String, &'a str, &'a str)> {
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
    let mut key = match host {
        Some(h) if !h.is_empty() => format!("{h}:{dir}"),
        _ => dir,
    };
    if let Some(r) = c.get(3) {
        key = format!("{key} [{}]", r.as_str());
    }
    let (sha, subject) = oneline_commits(out).next()?;
    Some((key, sha, subject))
}

/// Commits made quietly: `git commit -q` prints no `[branch sha] subject` line, so when a command
/// runs `k` non-dry-run `git commit`s followed by a `git log`, the first `k` oneline lines it
/// printed are those commits `(repo_dir, sha, subject)`.
pub fn quiet_commits<'a>(cmd: &str, out: &'a str) -> Vec<(String, &'a str, &'a str)> {
    let commits: Vec<_> = git_commit_re()
        .captures_iter(cmd)
        .filter(|c| {
            let end = c.get(0).map_or(0, |m| m.end());
            !has_dry_run_flag(clause_tail(&cmd[end..]))
        })
        .collect();
    let Some(last) = commits.last().and_then(|c| c.get(0)) else {
        return Vec::new();
    };
    if !git_log_re().is_match(&cmd[last.end()..]) {
        return Vec::new();
    }
    let repos: Vec<String> = commits
        .iter()
        .map(|c| {
            let at = c.get(0).map_or(0, |m| m.start());
            c.get(1)
                .map(|d| d.as_str().to_string())
                .or_else(|| cd_re().captures_iter(&cmd[..at]).last().map(|d| d[1].to_string()))
                .unwrap_or_else(|| ".".into())
        })
        .collect();
    oneline_commits(out)
        .zip(repos)
        .map(|((sha, subject), repo)| (repo, sha, subject))
        .collect()
}

/// A [`MessageSink`] that collects [`CommitEvidence`] for a set of shas as the transcript streams
/// past. Remembers only the git-verb tool calls it has seen (`id -> (kind, command)`), never content.
pub struct CommitEvidenceSink {
    shas: Vec<String>,
    pub(crate) git_calls: HashMap<String, (EvidenceKind, String)>,
    msg_idx: usize,
    resolver: Resolver,
    /// Stop the stream at the first `Created` match (a rewind wants the landing moment, not
    /// every later mention).
    pub(crate) stop_on_created: bool,
    pub found: Vec<CommitEvidence>,
    pub(crate) fail: Option<anyhow::Error>,
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
    pub(crate) fn observe(&mut self, m: &Message, idx: usize) -> Result<bool> {
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
    fn tier(&self) -> crate::Tier {
        crate::Tier::Facts
    }
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
/// mention any sha's 7-digit head at all?) skips the parse for sessions that can't match.
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

/// Which of `needles` (ASCII, case-insensitive) occur anywhere in the file, by one chunked read.
pub(crate) fn file_mentions(path: &Path, needles: &[&str]) -> Result<Vec<bool>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_evidence_rejects_dry_run_and_handles_global_options() {
        assert_eq!(git_evidence_kind("git commit -m x"), Some(EvidenceKind::Created));
        assert_eq!(
            git_evidence_kind("cd /r && git -C /r -c commit.gpgsign=false --no-pager commit -qF msg"),
            Some(EvidenceKind::Created)
        );
        assert_eq!(git_evidence_kind("git commit --dry-run -m x"), None);
        assert_eq!(git_evidence_kind("git commit -qn -m x && git log --oneline -1"), None);
        assert_eq!(git_evidence_kind("git push --dry-run origin main"), None);
        assert_eq!(
            git_evidence_kind("git commit --dry-run -m x && git commit -m real"),
            Some(EvidenceKind::Created)
        );
    }

    #[test]
    fn commit_summary_parses_branch_root_and_detached_head() {
        assert_eq!(
            parse_commit_summary_line("[main 1a2b3c4] ship it"),
            Some(CommitSummary {
                branch: Some("main"),
                header_label: "main",
                sha: "1a2b3c4",
                subject: "ship it",
            })
        );
        assert_eq!(
            parse_commit_summary_line("[feat/x (root-commit) 0123456789ab] init"),
            Some(CommitSummary {
                branch: Some("feat/x"),
                header_label: "feat/x",
                sha: "0123456789ab",
                subject: "init",
            })
        );
        assert_eq!(
            parse_commit_summary_line("[detached HEAD 1a2b3c4] wip"),
            Some(CommitSummary {
                branch: None,
                header_label: "detached HEAD",
                sha: "1a2b3c4",
                subject: "wip",
            })
        );
        assert_eq!(parse_commit_summary_line("[TRACE DEADBEEF] upper"), None);
        assert_eq!(parse_commit_summary_line("[a b c 1a2b3c4] noise"), None);
    }

    #[test]
    fn quiet_commits_log_head_and_commit_files_share_flag_parsing() {
        let cmd = "cd /r/demo && git -c commit.gpgsign=false --no-pager commit -qF /tmp/m.txt && git --no-pager log --oneline -1";
        assert_eq!(commit_message_files(cmd), vec!["/tmp/m.txt"]);
        assert_eq!(
            quiet_commits(cmd, "40fdb60 ship: unified git miner\n"),
            vec![("/r/demo".to_string(), "40fdb60", "ship: unified git miner")]
        );
        assert_eq!(
            git_log_head(cmd, "40fdb60 ship: unified git miner\n", Some("hbox")),
            Some(("hbox:/r/demo".to_string(), "40fdb60", "ship: unified git miner"))
        );
    }
}
