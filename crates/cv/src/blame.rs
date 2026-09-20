//! `cv blame` — code provenance: which agent session wrote this code, and what was it thinking?
//!
//! ## The correlation model
//!
//! git knows *which commit* changed a line; the event catalog knows *which session* edited a file
//! and roughly when. Neither alone answers "what was the agent thinking" — the bridge is time:
//! agents edit files and the human (or the agent itself) commits shortly after. So for each commit
//! that touched the file we look for `file_edit` events on it inside a window of
//! `[commit − 6h, commit + 15min]`, preferring the closest edit *before* the commit time. Sessions
//! whose recorded cwd is inside the same repo outrank sessions matched only by repo-relative path
//! suffix (a checkout living elsewhere), and timed matches outrank the fallback for harnesses that
//! record no message timestamps — for those all we can say is "the session was active when the
//! commit landed" (its created..updated span contains the commit time), and we label that weak.
//!
//! File identity is matched two ways: by the file's absolute path in the *current* checkout, and
//! by its repo-relative suffix (`target` ends with `/<rel>`), because a session may have edited
//! the same file in a worktree or an older clone at a different path.
//!
//! ## Honest limits
//!
//! * Time correlation breaks under rebases, squash-merges, and cherry-picks: the rewritten
//!   commit's committer time can land hours or days after the edit, outside the window. The
//!   session still shows in `cv touched`; it just won't be pinned to the rewritten commit.
//! * Clock skew between the machine that recorded the session and the one that committed shifts
//!   deltas; the 6-hour pre-window absorbs most of it, but this is a heuristic, not proof.
//! * `git log --follow` tracks renames only as well as git's similarity detection does, and
//!   events recorded against a file's *old* name don't suffix-match its new one.
//! * Several sessions often overlap one commit (a swarm in one working tree). We print the best
//!   few, closest-first — provenance candidates, not a verdict.
//!
//! `msg_idx` in the catalog counts messages in adapter stream order — the same counting `cv show
//! --range` does (both index every message the adapter streams, regardless of parse options), so
//! the `--range` hints land on the right window.
//!
//! Streaming discipline: nothing here parses a whole session. Candidates come from indexed
//! catalog queries ([`cv_core::events::edits_touching_file`]); `--show` reuses the windowed
//! `cv show --range` rendering path, which resolves only the messages inside the window — and,
//! when `cv index` has recorded per-message byte offsets ([`cv_core::offsets`]), *seeks* straight
//! to the window instead of streaming from byte 0.

use anyhow::{anyhow, bail, Context, Result};
use cv_core::events::{self, EditEvent};
use cv_core::ir::{truncate, Harness};
use std::path::{Path, PathBuf};
use std::process::Command;

/// An edit this long before the commit can still have produced it (absorbs long sessions
/// and moderate clock skew).
pub(crate) const WINDOW_BEFORE_SECS: i64 = 6 * 60 * 60;
/// An edit slightly *after* the commit time still counts (committer-vs-author time, skew,
/// or an agent that edits and amends).
pub(crate) const WINDOW_AFTER_SECS: i64 = 15 * 60;
/// Most-recent commits shown without `-L` (we note how many more exist).
const MAX_COMMITS: usize = 15;
/// Best session matches printed per commit.
const MATCHES_PER_COMMIT: usize = 3;
/// Messages of context on each side of the matched edit in the `cv show --range` hint.
const RANGE_CONTEXT: i64 = 3;

// ---------- git plumbing ----------

/// One commit that touched the file, as far as blame cares.
#[derive(Debug, Clone)]
struct CommitInfo {
    short: String,
    /// Committer time, unix seconds (what the correlation window keys on).
    time: i64,
    summary: String,
}

/// Spawn `git -C <dir> <args>`. A missing `git` binary degrades to a clear, actionable error
/// instead of a raw NotFound.
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

/// Like [`run_git`] but a non-zero exit is an error carrying git's stderr.
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

/// The repo toplevel containing `abs`, or `None` when it isn't inside a work tree.
/// Only a *spawn* failure (git missing) is an error.
fn repo_toplevel(abs: &Path) -> Result<Option<PathBuf>> {
    let dir = match abs.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let out = run_git(dir, &["rev-parse", "--show-toplevel"])?;
    if !out.status.success() {
        return Ok(None);
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!top.is_empty()).then(|| PathBuf::from(top)))
}

/// The file's commit history, newest first: `git log --follow` with a `%x1f`-separated format.
fn file_commits(root: &Path, rel: &str) -> Result<Vec<CommitInfo>> {
    let out = git_stdout(root, &["log", "--follow", "--format=%H%x1f%ct%x1f%h%x1f%s", "--", rel])?;
    let mut commits = Vec::new();
    for line in out.lines() {
        let mut f = line.split('\u{1f}');
        let (Some(_sha), Some(ct), Some(short), summary) = (f.next(), f.next(), f.next(), f.next()) else {
            continue;
        };
        let Ok(time) = ct.parse::<i64>() else { continue };
        commits.push(CommitInfo {
            short: short.to_string(),
            time,
            summary: summary.unwrap_or("").to_string(),
        });
    }
    Ok(commits)
}

/// The distinct commits owning lines `lo..=hi`, via `git blame --porcelain`, newest first.
fn line_commits(root: &Path, rel: &str, lo: u32, hi: u32) -> Result<Vec<CommitInfo>> {
    let range = format!("{lo},{hi}");
    let out = git_stdout(root, &["blame", "-L", &range, "--porcelain", "--", rel])?;
    // Porcelain emits the full header block (author-time/committer-time/summary/…) the first time
    // each commit appears; later occurrences are just the `<sha> <orig> <final> [<n>]` line.
    let mut order: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    use std::collections::HashMap;
    #[derive(Default)]
    struct Meta {
        committer_time: Option<i64>,
        author_time: Option<i64>,
        summary: Option<String>,
    }
    let mut metas: HashMap<String, Meta> = HashMap::new();
    for line in out.lines() {
        if line.starts_with('\t') {
            continue; // content line
        }
        if let Some(sha) = blame_header_sha(line) {
            if !order.iter().any(|s| s == sha) {
                order.push(sha.to_string());
            }
            cur = Some(sha.to_string());
            metas.entry(sha.to_string()).or_default();
        } else if let Some(c) = &cur {
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
    let mut commits: Vec<CommitInfo> = order
        .into_iter()
        .map(|sha| {
            let m = metas.remove(&sha).unwrap_or_default();
            CommitInfo {
                short: sha.chars().take(7).collect(),
                time: m.committer_time.or(m.author_time).unwrap_or(0),
                summary: m.summary.unwrap_or_default(),
            }
        })
        .collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.time));
    Ok(commits)
}

/// The sha of a porcelain blame *header* line (`<40-hex> <orig> <final> [<n>]`), else `None`.
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

// ---------- correlation (pure) ----------

/// How a session got tied to a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strength {
    /// A timestamped edit landed inside the correlation window. The real signal.
    Timed,
    /// No event timestamps; the session's created..updated span merely contains the commit time.
    SessionSpan,
}

/// One session matched to one commit: the index (into the candidate slice) of its best edit
/// event, plus everything ranking needs.
#[derive(Debug, Clone)]
pub(crate) struct SessionMatch {
    /// Index into the `edits` slice handed to [`correlate`].
    pub event: usize,
    pub strength: Strength,
    /// `commit_time − event_ts` (positive = edit before commit). 0 for [`Strength::SessionSpan`].
    pub delta: i64,
    /// Whether the session's recorded cwd sits inside the repo being blamed.
    pub cwd_in_repo: bool,
}

/// Lower sorts first: timed beats span, in-repo cwd beats elsewhere, an edit *before* the
/// commit beats one after, then closest wins.
fn match_rank(m: &SessionMatch) -> (u8, u8, u8, i64) {
    (
        match m.strength {
            Strength::Timed => 0,
            Strength::SessionSpan => 1,
        },
        u8::from(!m.cwd_in_repo),
        u8::from(m.delta < 0),
        m.delta.abs(),
    )
}

/// THE pure correlation: which sessions plausibly produced the commit at `commit_time`, given the
/// file's `file_edit` events. One match per session (its best event), sorted best-first.
///
/// A timestamped edit must fall in `[commit − WINDOW_BEFORE, commit + WINDOW_AFTER]`; sessions
/// whose events carry no timestamps fall back to "the session's created..updated span contains
/// the commit time" (with the same small after-grace, since commits land right as sessions end).
pub(crate) fn correlate(commit_time: i64, edits: &[EditEvent], repo_root: &Path) -> Vec<SessionMatch> {
    use std::collections::HashMap;
    let mut best: HashMap<(&str, &str), SessionMatch> = HashMap::new();
    for (i, e) in edits.iter().enumerate() {
        let cand = match e.ts {
            Some(ts) => {
                let delta = commit_time - ts;
                if !(-WINDOW_AFTER_SECS..=WINDOW_BEFORE_SECS).contains(&delta) {
                    continue;
                }
                SessionMatch {
                    event: i,
                    strength: Strength::Timed,
                    delta,
                    cwd_in_repo: cwd_in_repo(e.cwd.as_deref(), repo_root),
                }
            }
            None => {
                let (Some(c), Some(u)) = (e.created_at, e.updated_at) else {
                    continue;
                };
                if commit_time < c || commit_time > u + WINDOW_AFTER_SECS {
                    continue;
                }
                SessionMatch {
                    event: i,
                    strength: Strength::SessionSpan,
                    delta: 0,
                    cwd_in_repo: cwd_in_repo(e.cwd.as_deref(), repo_root),
                }
            }
        };
        let key = (e.harness.as_str(), e.session_id.as_str());
        match best.get(&key) {
            Some(prev) if match_rank(prev) <= match_rank(&cand) => {}
            _ => {
                best.insert(key, cand);
            }
        }
    }
    let mut out: Vec<SessionMatch> = best.into_values().collect();
    out.sort_by_key(match_rank);
    out
}

/// Whether a session cwd (string form, possibly stale) lies inside `root`.
fn cwd_in_repo(cwd: Option<&str>, root: &Path) -> bool {
    cwd.map(Path::new).is_some_and(|c| c.starts_with(root))
}

/// The repo-relative suffix rule [`cv_core::events::edits_touching_file`]'s SQL implements, as a
/// pure function so the rule itself is unit-testable: a recorded target names this file when it
/// equals the current absolute path, equals the bare relative path, or ends with `/<rel>` (the
/// same file in a checkout living somewhere else).
#[cfg(test)]
pub(crate) fn target_matches(target: &str, abs: &str, rel: &str) -> bool {
    target == abs || target == rel || target.strip_suffix(rel).is_some_and(|head| head.ends_with('/'))
}

/// The `cv show --range` window around an edit at `msg_idx`: `RANGE_CONTEXT` messages of leading
/// context, end-exclusive (so `142` → `139-145`).
pub(crate) fn range_hint(msg_idx: i64) -> (usize, usize) {
    let start = (msg_idx - RANGE_CONTEXT).max(0) as usize;
    let end = (msg_idx + RANGE_CONTEXT).max(1) as usize;
    (start, end)
}

// ---------- CLI entry ----------

pub fn cmd_blame(file: &str, lines: Option<&str>, show: bool) -> Result<()> {
    let abs = absolutize_input(file)?;
    let Some(root) = repo_toplevel(&abs)? else {
        return blame_event_only(file, &abs);
    };
    let rel = abs
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().into_owned())
        .with_context(|| format!("{} is not inside the repo at {}", abs.display(), root.display()))?;

    // The commits owning this file (or these lines), newest first.
    let (mut commits, total) = match lines {
        None => {
            let all = file_commits(&root, &rel)?;
            let total = all.len();
            (all, total)
        }
        Some(spec) => {
            let (lo, hi) = parse_line_spec(spec)?;
            let cs = line_commits(&root, &rel, lo, hi)?;
            let total = cs.len();
            (cs, total)
        }
    };
    commits.truncate(MAX_COMMITS);

    if commits.is_empty() {
        println!("no commits touch {rel} — is it committed? (event-catalog view: `cv touched {rel}`)");
        return Ok(());
    }

    // Candidate edits from the event catalog, matched absolutely and by repo-relative suffix.
    let edits = events::edits_touching_file(&abs.to_string_lossy(), &rel);
    if edits.is_empty() && !events::catalog_has_events() {
        eprintln!("(event catalog is empty — run `cv index` first to ingest tool events)");
    }

    match lines {
        Some(spec) => println!(
            "✦ blame {rel} -L {spec} — {} commit(s) own those lines\n",
            commits.len()
        ),
        None => {
            let more = if total > commits.len() {
                format!(" (of {total}; showing the most recent)")
            } else {
                String::new()
            };
            println!("✦ blame {rel} — {} commit(s){more}\n", commits.len());
        }
    }

    // Newest first: each commit, with its best-matched sessions.
    let mut matched_commits = 0usize;
    let mut best_overall: Option<(String, String, i64)> = None; // (harness, session_id, msg_idx)
    for c in &commits {
        println!("◆ {} {}  {}", c.short, date(c.time), c.summary);
        let matches = correlate(c.time, &edits, &root);
        if matches.is_empty() {
            println!("  (no agent session found)");
            continue;
        }
        matched_commits += 1;
        for m in matches.iter().take(MATCHES_PER_COMMIT) {
            let e = &edits[m.event];
            print_match(e, m);
            // Best overall = the top match of the newest commit that matched anything.
            if best_overall.is_none() {
                best_overall = Some((e.harness.clone(), e.session_id.clone(), e.msg_idx));
            }
        }
    }

    println!(
        "\nprovenance: {matched_commits} of {} commit(s) matched an agent session",
        commits.len()
    );

    if show {
        match &best_overall {
            None => println!("(--show: no agent session matched any commit)"),
            Some((harness, sid, idx)) => show_best(harness, sid, *idx)?,
        }
    }
    Ok(())
}

/// One matched-session line under a commit, plus its copy-pasteable `cv show --range` hint.
fn print_match(e: &EditEvent, m: &SessionMatch) {
    let when = match m.strength {
        Strength::Timed => format!("({})", fmt_delta(m.delta)),
        Strength::SessionSpan => "(weak: session active at commit time; no event timestamps)".into(),
    };
    let elsewhere = if m.cwd_in_repo { "" } else { " · other checkout" };
    let date = e.ts.or(e.updated_at).map(date).unwrap_or_else(|| "----------".into());
    let title = e
        .title
        .as_deref()
        .map(|t| format!("\"{}\"  ", truncate(t, 48)))
        .unwrap_or_default();
    println!(
        "  {:8} {}  {}  {}edit at msg {} {}{}",
        e.harness,
        crate::short_id(&e.session_id),
        date,
        title,
        e.msg_idx,
        when,
        elsewhere,
    );
    let (s, end) = range_hint(e.msg_idx);
    println!("    ↳ cv show {} --range {s}..{end}", crate::short_id(&e.session_id));
}

/// `--show`: render the conversation window around the best match's edit, through the exact
/// same streaming range path as `cv show --range` (only the window's messages are resolved).
fn show_best(harness: &str, session_id: &str, msg_idx: i64) -> Result<()> {
    let want = Harness::parse(harness);
    let (r, adapter) = cv_core::find(session_id, want)?
        .with_context(|| format!("session {harness}:{session_id} not found (deleted since indexing?)"))?;
    let (start, end) = range_hint(msg_idx);
    println!(
        "\n── conversation around the edit · {harness} {} · msg {start}..{end} ──\n",
        crate::short_id(session_id)
    );
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    crate::stream_session_render(
        adapter.as_ref(),
        &r,
        &mut out,
        crate::show_header,
        crate::show_message,
        crate::RenderOpts {
            range: Some((start, Some(end))),
            max_bytes: None,
        },
    )?;
    use std::io::Write;
    out.flush()?;
    Ok(())
}

/// No git repo around the file: degrade to pure event-catalog mode — the sessions that edited
/// it (an enriched `cv touched`), with range hints but no commit correlation.
fn blame_event_only(file: &str, abs: &Path) -> Result<()> {
    println!("(not inside a git repository — event-catalog matches only, no commit correlation)\n");
    let rel = file.trim_start_matches("./");
    let edits = events::edits_touching_file(&abs.to_string_lossy(), rel);
    if edits.is_empty() {
        if !events::catalog_has_events() {
            println!("no edit events found — run `cv index` first to ingest tool events");
        } else {
            println!("no sessions edited {file:?}");
        }
        return Ok(());
    }
    // Latest edit per session, newest session first.
    let mut latest: Vec<&EditEvent> = Vec::new();
    for e in &edits {
        match latest
            .iter_mut()
            .find(|l| l.harness == e.harness && l.session_id == e.session_id)
        {
            Some(slot) => {
                if e.ts.unwrap_or(i64::MIN) >= slot.ts.unwrap_or(i64::MIN) {
                    *slot = e;
                }
            }
            None => latest.push(e),
        }
    }
    latest.sort_by_key(|e| std::cmp::Reverse(e.ts.or(e.updated_at)));
    println!("{} session(s) edited {file:?}:\n", latest.len());
    for e in latest {
        let date = e.ts.or(e.updated_at).map(date).unwrap_or_else(|| "----------".into());
        println!(
            "  {:8} {}  {}  {}  last edit at msg {}",
            e.harness,
            crate::short_id(&e.session_id),
            date,
            e.title.as_deref().map(|t| truncate(t, 48)).unwrap_or_default(),
            e.msg_idx,
        );
        let (s, end) = range_hint(e.msg_idx);
        println!("    ↳ cv show {} --range {s}..{end}", crate::short_id(&e.session_id));
    }
    Ok(())
}

// ---------- small helpers ----------

/// Lexically absolutize the user's path against the cwd, then canonicalize when possible
/// (resolves `/var` → `/private/var`-style symlinks so strip_prefix against the git toplevel
/// agrees). A path that no longer exists keeps its lexical form.
fn absolutize_input(file: &str) -> Result<PathBuf> {
    let p = Path::new(file);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .context("cannot resolve current directory")?
            .join(p)
    };
    Ok(abs.canonicalize().unwrap_or(abs))
}

/// Parse `-L <line>[,<endline>]` into an inclusive 1-based `(lo, hi)`.
pub(crate) fn parse_line_spec(spec: &str) -> Result<(u32, u32)> {
    let (a, b) = match spec.split_once(',') {
        None => (spec, spec),
        Some((a, b)) => (a, b),
    };
    let lo: u32 = a
        .trim()
        .parse()
        .with_context(|| format!("bad -L {spec:?}: expected <line> or <line>,<endline>"))?;
    let hi: u32 = b
        .trim()
        .parse()
        .with_context(|| format!("bad -L {spec:?}: end line must be a number"))?;
    if lo == 0 {
        bail!("bad -L {spec:?}: lines are 1-based");
    }
    if hi < lo {
        bail!("bad -L {spec:?}: end line ({hi}) is before start ({lo})");
    }
    Ok((lo, hi))
}

fn date(ts: i64) -> String {
    crate::util::fmt_local_ts(ts, "%Y-%m-%d").unwrap_or_else(|| "----------".into())
}

/// Humanize a commit↔edit delta: `8m before commit`, `2h10m before commit`, `30s after commit`.
pub(crate) fn fmt_delta(delta: i64) -> String {
    let (mag, dir) = if delta >= 0 {
        (delta, "before")
    } else {
        (-delta, "after")
    };
    let (h, m, s) = (mag / 3600, (mag % 3600) / 60, mag % 60);
    let t = if h > 0 {
        if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    };
    format!("{t} {dir} commit")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(sid: &str, msg_idx: i64, ts: Option<i64>, cwd: Option<&str>, span: Option<(i64, i64)>) -> EditEvent {
        EditEvent {
            harness: "claude".into(),
            session_id: sid.into(),
            msg_idx,
            ts,
            target: Some("/repo/src/lib.rs".into()),
            title: None,
            cwd: cwd.map(Into::into),
            created_at: span.map(|(c, _)| c),
            updated_at: span.map(|(_, u)| u),
        }
    }

    const T: i64 = 1_780_000_000; // an arbitrary commit time
    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn correlate_prefers_closest_edit_before_commit() {
        let edits = vec![
            ev("far", 3, Some(T - 5 * 3600), Some("/repo"), None),
            ev("near", 9, Some(T - 8 * 60), Some("/repo"), None),
            ev("after", 4, Some(T + 5 * 60), Some("/repo"), None),
        ];
        let m = correlate(T, &edits, &root());
        assert_eq!(m.len(), 3);
        assert_eq!(edits[m[0].event].session_id, "near");
        assert_eq!(m[0].delta, 8 * 60);
        // Both "before" matches outrank the (closer!) "after" one.
        assert_eq!(edits[m[1].event].session_id, "far");
        assert_eq!(edits[m[2].event].session_id, "after");
        assert!(m[2].delta < 0);
    }

    #[test]
    fn correlate_window_excludes_stale_and_future_edits() {
        let edits = vec![
            ev("ancient", 1, Some(T - WINDOW_BEFORE_SECS - 1), Some("/repo"), None),
            ev("future", 2, Some(T + WINDOW_AFTER_SECS + 1), Some("/repo"), None),
            ev("edge-lo", 3, Some(T - WINDOW_BEFORE_SECS), Some("/repo"), None),
            ev("edge-hi", 4, Some(T + WINDOW_AFTER_SECS), Some("/repo"), None),
        ];
        let m = correlate(T, &edits, &root());
        let ids: Vec<&str> = m.iter().map(|x| edits[x.event].session_id.as_str()).collect();
        assert!(ids.contains(&"edge-lo") && ids.contains(&"edge-hi"));
        assert!(!ids.contains(&"ancient") && !ids.contains(&"future"));
    }

    #[test]
    fn correlate_ranks_in_repo_cwd_above_other_checkouts() {
        // The other-checkout session is *closer* in time, but the in-repo one still wins.
        let edits = vec![
            ev("elsewhere", 1, Some(T - 60), Some("/somewhere/else"), None),
            ev("in-repo", 2, Some(T - 30 * 60), Some("/repo/crates"), None),
        ];
        let m = correlate(T, &edits, &root());
        assert_eq!(edits[m[0].event].session_id, "in-repo");
        assert!(m[0].cwd_in_repo && !m[1].cwd_in_repo);
    }

    #[test]
    fn correlate_null_ts_falls_back_to_session_span_weakly() {
        let edits = vec![
            // No event ts, but the session was open across the commit → weak match.
            ev("spanning", 7, None, Some("/repo"), Some((T - 3600, T + 600))),
            // No ts and the session closed long before → no match.
            ev("closed", 1, None, Some("/repo"), Some((T - 9000, T - 8000))),
            // No ts and no session dates at all → no match.
            ev("dateless", 2, None, Some("/repo"), None),
            // Any timed in-window edit outranks the span match.
            ev("timed", 3, Some(T - 3000), None, None),
        ];
        let m = correlate(T, &edits, &root());
        assert_eq!(m.len(), 2);
        assert_eq!(edits[m[0].event].session_id, "timed");
        assert_eq!(m[0].strength, Strength::Timed);
        assert_eq!(edits[m[1].event].session_id, "spanning");
        assert_eq!(m[1].strength, Strength::SessionSpan);
    }

    #[test]
    fn correlate_dedupes_to_best_event_per_session() {
        let edits = vec![
            ev("s", 10, Some(T - 2 * 3600), Some("/repo"), None),
            ev("s", 42, Some(T - 5 * 60), Some("/repo"), None),
            ev("s", 50, Some(T + 60), Some("/repo"), None),
        ];
        let m = correlate(T, &edits, &root());
        assert_eq!(m.len(), 1);
        assert_eq!(edits[m[0].event].msg_idx, 42);
    }

    #[test]
    fn suffix_matching_rules() {
        let abs = "/Users/em/pug/repo/src/ir.rs";
        let rel = "src/ir.rs";
        assert!(target_matches(abs, abs, rel));
        assert!(target_matches("src/ir.rs", abs, rel)); // bare relative (cwd-less session)
        assert!(target_matches("/old/clone/src/ir.rs", abs, rel)); // other checkout
        assert!(target_matches("/w/repo-2/src/ir.rs", abs, rel));
        assert!(!target_matches("/repo/src/xir.rs", abs, rel)); // not a path-component boundary
        assert!(!target_matches("/repo/docs/ir.rs", abs, rel));
        assert!(!target_matches("/repo/src/ir.rs.bak", abs, rel));
    }

    #[test]
    fn range_hint_centers_and_clamps() {
        assert_eq!(range_hint(142), (139, 145)); // the documented example
        assert_eq!(range_hint(1), (0, 4));
        assert_eq!(range_hint(0), (0, 3));
    }

    #[test]
    fn line_spec_forms() {
        assert_eq!(parse_line_spec("12").unwrap(), (12, 12));
        assert_eq!(parse_line_spec("10,20").unwrap(), (10, 20));
        assert_eq!(parse_line_spec(" 3 , 4 ").unwrap(), (3, 4));
        assert!(parse_line_spec("0").is_err());
        assert!(parse_line_spec("9,3").is_err());
        assert!(parse_line_spec("x").is_err());
    }

    #[test]
    fn delta_humanizes() {
        assert_eq!(fmt_delta(8 * 60), "8m before commit");
        assert_eq!(fmt_delta(2 * 3600 + 600), "2h10m before commit");
        assert_eq!(fmt_delta(3 * 3600), "3h before commit");
        assert_eq!(fmt_delta(-30), "30s after commit");
    }

    #[test]
    fn blame_porcelain_header_detection() {
        assert_eq!(
            blame_header_sha("49790775624d4ac7a52a01b3db762a486d8b236f 5 5 1"),
            Some("49790775624d4ac7a52a01b3db762a486d8b236f")
        );
        assert_eq!(
            blame_header_sha("49790775624d4ac7a52a01b3db762a486d8b236f 6 6"),
            Some("49790775624d4ac7a52a01b3db762a486d8b236f")
        );
        assert_eq!(blame_header_sha("summary fix: a thing"), None);
        assert_eq!(blame_header_sha("author-time 1748000000"), None);
        assert_eq!(blame_header_sha("\t    let x = 1;"), None);
    }
}
