//! A session's file-touch timeline: every read, write and edit a tool call made, in message order,
//! with whether it succeeded and how far its result can be verified.
//!
//! Verification, strongest first:
//! - `verified_git`: a commit at or after the touch, in the session's repository, holds exactly the
//!   reconstructed post-touch content, or (for an edit whose base is unknown) contains its
//!   replacement text. The blob is read at that commit (`<commit>:<path>`), never at `HEAD`, so a
//!   later commit that changes the file again leaves the claim standing.
//! - `verified_disk`: the checkout's current file equals the reconstructed content. Skipped under
//!   `--frozen`, which promises no reads outside the stores.
//! - `reconstructed`: the content follows from a `Write` and the exact `Edit` replacements after it
//!   in this transcript.
//! - `unverified`: a partial edit with no known base, a patch, or a read.
//!
//! The transcript is streamed once; only write/edit arguments are retained until the end, when
//! results (which arrive after their calls) say which ones changed the file.

use crate::ir::{Block, Message, SessionRef};
use crate::stream::Flow;
use crate::tools::classify::{absolutize_path, visit_tool_ops, ClassifyOpts, ToolOp};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Reconstructed content beyond this size is dropped (the touch degrades to `unverified`).
const MAX_STATE_BYTES: usize = 8 << 20;
/// Later commits inspected per path.
const MAX_COMMITS_PER_PATH: usize = 32;
/// A replacement shorter than this (non-whitespace bytes) is too common to verify by containment.
const MIN_HUNK_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Read,
    Write,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileVerification {
    VerifiedGit { commit: String, blob: String },
    VerifiedDisk { blake3: String },
    Reconstructed,
    Unverified,
}

/// One file touch. `lines_changed` is `(added, removed)`: for a write, the new line count and the
/// replaced content's when it was reconstructed (else 0); for an edit whose base is unknown, the
/// replacement's and the replaced text's line counts.
#[derive(Debug, Clone, Serialize)]
pub struct FileTouch {
    pub msg_idx: usize,
    pub timestamp: Option<DateTime<Utc>>,
    pub op: FileOp,
    /// Repository-relative inside the session's git root (else cwd-relative), else as given.
    pub path: String,
    pub tool: String,
    pub tool_use_id: String,
    pub succeeded: Option<bool>,
    pub lines_changed: Option<(usize, usize)>,
    pub verification: FileVerification,
}

/// What a write/edit call asked for, kept until results are known.
enum Payload {
    None,
    Write(String),
    Edits(Vec<(String, String, bool)>),
}

struct Pending {
    touch: FileTouch,
    abs: String,
    payload: Payload,
}

fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

fn edit_of(v: &Value) -> Option<(String, String, bool)> {
    let old = str_field(v, &["old_string", "old_str"])?;
    let new = str_field(v, &["new_string", "new_str"])?;
    Some((old, new, v.get("replace_all").and_then(Value::as_bool).unwrap_or(false)))
}

/// Bounded string extraction from a `JsonView`, stopping immediately if the decoded length
/// exceeds `MAX_STATE_BYTES` so oversized `Write`/`Edit` inputs are never materialized into `pending`.
fn bounded_view_string(view: crate::json::JsonView<'_>) -> Result<Option<String>> {
    let mut out = String::new();
    let mut over = false;
    let is_str = crate::projection::string_until(view, |chunk| {
        if out.len().saturating_add(chunk.len()) > MAX_STATE_BYTES {
            over = true;
            return true;
        }
        out.push_str(chunk);
        false
    })?;
    if !is_str || over {
        return Ok(None);
    }
    Ok(Some(out))
}

fn bounded_str_field(view: crate::json::JsonView<'_>, keys: &[&str]) -> Result<Option<String>> {
    if view.is_string()? {
        return Ok(None);
    }
    for key in keys {
        if let Some(v) = view.get(key)? {
            if v.is_string()? {
                return bounded_view_string(v);
            }
        }
    }
    Ok(None)
}

/// Extract a bounded write/edit `Payload` through `JsonView` before resolving heap `Value`s.
fn payload_from_json(input: &crate::json::Json, full_write: bool) -> Result<Payload> {
    input.with_view(|view| {
        if full_write {
            return Ok(bounded_str_field(view, &["content", "file_text"])?.map_or(Payload::None, Payload::Write));
        }
        if !view.is_string()? {
            if let Some(old_v) = view.get("old_string")?.or(view.get("old_str")?) {
                if old_v.is_string()? {
                    let Some(old) = bounded_view_string(old_v)? else {
                        return Ok(Payload::None);
                    };
                    let Some(new_v) = view.get("new_string")?.or(view.get("new_str")?) else {
                        return Ok(Payload::None);
                    };
                    let Some(new) = bounded_view_string(new_v)? else {
                        return Ok(Payload::None);
                    };
                    let all = match view.get("replace_all")? {
                        Some(b) => b.resolve()?.as_bool().unwrap_or(false),
                        None => false,
                    };
                    return Ok(Payload::Edits(vec![(old, new, all)]));
                }
            }
            if let Some(edits_v) = view.get("edits")? {
                let resolved = edits_v.resolve()?;
                if let Some(arr) = resolved.as_array() {
                    let mut total = 0usize;
                    let mut out = Vec::with_capacity(arr.len());
                    for item in arr {
                        let Some(e) = edit_of(item) else {
                            return Ok(Payload::None);
                        };
                        total = total.saturating_add(e.0.len()).saturating_add(e.1.len());
                        if total > MAX_STATE_BYTES {
                            return Ok(Payload::None);
                        }
                        out.push(e);
                    }
                    return Ok(Payload::Edits(out));
                }
            }
        }
        Ok(Payload::None)
    })
}

/// Read `abs` strictly when it is a regular non-symlink file inside `repo_root` or `cwd` and
/// at most `MAX_STATE_BYTES` bytes (never reading special device nodes or paths outside workspace).
fn read_contained_regular_file(abs: &str, repo_root: Option<&Path>, cwd: Option<&Path>) -> Option<Vec<u8>> {
    let path = Path::new(abs);
    if !path.is_absolute() {
        return None;
    }
    let sym = std::fs::symlink_metadata(path).ok()?;
    if sym.file_type().is_symlink() || !sym.file_type().is_file() || sym.len() > MAX_STATE_BYTES as u64 {
        return None;
    }
    let canon = std::fs::canonicalize(path).ok()?;
    let contained = [repo_root, cwd]
        .into_iter()
        .flatten()
        .filter_map(|base| std::fs::canonicalize(base).ok())
        .any(|canon_base| canon.starts_with(&canon_base));
    if !contained {
        return None;
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_STATE_BYTES as u64 {
        return None;
    }
    let mut buf = Vec::with_capacity((meta.len() as usize).min(MAX_STATE_BYTES));
    file.take(MAX_STATE_BYTES as u64 + 1).read_to_end(&mut buf).ok()?;
    (buf.len() <= MAX_STATE_BYTES).then_some(buf)
}

fn lines(s: &str) -> usize {
    s.lines().count()
}

/// Apply exact replacements; `None` when any old text is absent (or ambiguous without `replace_all`).
fn apply(base: &str, edits: &[(String, String, bool)]) -> Option<(String, usize, usize)> {
    let mut text = base.to_string();
    let (mut added, mut removed) = (0, 0);
    for (old, new, all) in edits {
        let n = if old.is_empty() {
            0
        } else {
            text.matches(old.as_str()).count()
        };
        if n == 0 || (n > 1 && !all) {
            return None;
        }
        text = text.replace(old.as_str(), new);
        added += n * lines(new);
        removed += n * lines(old);
    }
    Some((text, added, removed))
}

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = String::from_utf8(out).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// One `git cat-file --batch` for the whole timeline: every blob is a request on the same
/// process, not two processes per commit.
struct Blobs {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Blobs {
    fn spawn(root: &Path) -> Option<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Blobs { child, stdin, stdout })
    }

    /// `(blob id, content)` of `spec` (`<commit>:<path>`); `None` when it names no blob. `Err`
    /// when the process stopped answering in protocol, after which it is not asked again.
    fn get(&mut self, spec: &str) -> std::io::Result<Option<(String, Vec<u8>)>> {
        if spec.contains('\n') {
            return Ok(None);
        }
        let stdin = self.stdin.as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?;
        writeln!(stdin, "{spec}")?;
        stdin.flush()?;
        let mut header = String::new();
        if self.stdout.read_line(&mut header)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let header = header.trim_end_matches('\n');
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            return Ok(None);
        }
        let mut fields = header.rsplitn(3, ' ');
        let (Some(size), Some(kind), Some(id)) = (fields.next(), fields.next(), fields.next()) else {
            return Err(std::io::ErrorKind::InvalidData.into());
        };
        let size: u64 = size.parse().map_err(|_| std::io::ErrorKind::InvalidData)?;
        // Drain oversized or non-blob payloads in bounded chunks so `cat-file --batch` stays synchronized.
        if size > MAX_STATE_BYTES as u64 || kind != "blob" {
            let to_drain = size.checked_add(1).ok_or(std::io::ErrorKind::InvalidData)?;
            let drained = std::io::copy(&mut (&mut self.stdout).take(to_drain), &mut std::io::sink())?;
            if drained != to_drain {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            return Ok(None);
        }
        let size = size as usize;
        // The body and its terminating newline follow whatever the type.
        let mut body = vec![0; size + 1];
        self.stdout.read_exact(&mut body)?;
        body.pop();
        Ok(Some((id.to_string(), body)))
    }
}

impl Drop for Blobs {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// Later commits of one path, oldest first, each read at most once.
struct PathHistory {
    commits: Vec<(String, i64)>,
    blobs: HashMap<String, Option<(String, Vec<u8>)>>,
}

struct Repo {
    root: PathBuf,
    paths: HashMap<String, PathHistory>,
    /// Spawned on the first blob read; `None` again once it fails.
    blobs: Option<Option<Blobs>>,
}

impl Repo {
    fn history(&mut self, rel: &str) -> &mut PathHistory {
        let root = self.root.clone();
        self.paths.entry(rel.to_string()).or_insert_with(|| {
            let log = git(
                &root,
                &[
                    "--literal-pathspecs",
                    "log",
                    "--format=%H %ct",
                    "--no-renames",
                    "--diff-filter=AM",
                    "--",
                    rel,
                ],
            )
            .unwrap_or_default();
            let mut commits: Vec<(String, i64)> = String::from_utf8_lossy(&log)
                .lines()
                .filter_map(|l| {
                    let (sha, ct) = l.split_once(' ')?;
                    Some((sha.to_string(), ct.parse().ok()?))
                })
                .collect();
            commits.reverse();
            PathHistory {
                commits,
                blobs: HashMap::new(),
            }
        })
    }

    /// The first commit at or after `since` whose blob for `rel` satisfies `matches`.
    fn first_matching(
        &mut self,
        rel: &str,
        since: DateTime<Utc>,
        matches: impl Fn(&[u8]) -> bool,
    ) -> Option<FileVerification> {
        self.history(rel);
        let root = self.root.clone();
        let blobs = &mut self.blobs;
        let h = self.paths.get_mut(rel).expect("history() was called");
        let later: Vec<String> = h
            .commits
            .iter()
            .filter(|(_, ct)| *ct >= since.timestamp())
            .take(MAX_COMMITS_PER_PATH)
            .map(|(sha, _)| sha.clone())
            .collect();
        for sha in later {
            let blob = h.blobs.entry(sha.clone()).or_insert_with(|| {
                let batch = blobs.get_or_insert_with(|| Blobs::spawn(&root)).as_mut()?;
                match batch.get(&format!("{sha}:{rel}")) {
                    Ok(found) => found,
                    Err(_) => {
                        *blobs = Some(None);
                        None
                    }
                }
            });
            if let Some((id, body)) = blob {
                if matches(body) {
                    return Some(FileVerification::VerifiedGit {
                        commit: sha,
                        blob: id.clone(),
                    });
                }
            }
        }
        None
    }
}

fn display_path(abs: &str, root: Option<&Path>, cwd: Option<&Path>) -> String {
    let p = Path::new(abs);
    for base in [root, cwd].into_iter().flatten() {
        if let Ok(rel) = p.strip_prefix(base) {
            if !rel.as_os_str().is_empty() {
                return rel.to_string_lossy().into_owned();
            }
        }
    }
    abs.to_string()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The file-touch timeline of one session.
pub fn read(adapter: &dyn crate::Adapter, r: &SessionRef) -> Result<Vec<FileTouch>> {
    read_with_checkout(adapter, r, true)
}

/// A replicated transcript can reconstruct edits but cannot certify the producer's checkout.
pub fn read_with_checkout(adapter: &dyn crate::Adapter, r: &SessionRef, checkout: bool) -> Result<Vec<FileTouch>> {
    let mut pending: Vec<Pending> = Vec::new();
    let mut by_call: HashMap<String, Vec<usize>> = HashMap::new();
    let mut results: HashMap<String, bool> = HashMap::new();
    let mut index = 0usize;
    let mut error = None;
    let cwd_hint = r.cwd.clone();
    let mut observe = |m: &Message, index: usize| -> Result<()> {
        if !m.is_model_visible() {
            return Ok(());
        }
        for b in &m.content {
            match b {
                Block::ToolUse { id, name, input, .. } => {
                    let mut ops = Vec::new();
                    input.with_view(|view| {
                        visit_tool_ops(Some(r.harness), name, view, ClassifyOpts::COUNTS, |op| match op.op {
                            ToolOp::FileRead { path } => ops.push((FileOp::Read, path.into_owned())),
                            ToolOp::FileEdit { path, is_full_write } => ops.push((
                                if is_full_write { FileOp::Write } else { FileOp::Edit },
                                path.into_owned(),
                            )),
                            _ => {}
                        })
                    })?;
                    let single = ops.len() == 1;
                    for (op, path) in ops {
                        let payload = if single && op != FileOp::Read {
                            payload_from_json(input, op == FileOp::Write)?
                        } else {
                            Payload::None
                        };
                        let abs = absolutize_path(&path, cwd_hint.as_deref());
                        by_call.entry(id.clone()).or_default().push(pending.len());
                        pending.push(Pending {
                            touch: FileTouch {
                                msg_idx: index,
                                timestamp: m.timestamp,
                                op,
                                path,
                                tool: name.clone(),
                                tool_use_id: id.clone(),
                                succeeded: None,
                                lines_changed: None,
                                verification: FileVerification::Unverified,
                            },
                            abs,
                            payload,
                        });
                    }
                }
                Block::ToolResult {
                    tool_use_id, is_error, ..
                } => {
                    results.insert(tool_use_id.clone(), !*is_error);
                }
                _ => {}
            }
        }
        Ok(())
    };
    let session = crate::stream::stream_counted(
        adapter,
        r,
        &crate::stream::Demand::FACTS.with_spans(),
        &mut |m: Message| {
            if let Err(e) = observe(&m, index) {
                error = Some(e);
                return Flow::Stop;
            }
            index += 1;
            Flow::Continue
        },
        crate::stream::Pass {
            record: checkout,
            ..crate::stream::Pass::RECORD
        },
    )?
    .session;
    if let Some(e) = error {
        return Err(e);
    }
    for (call, rows) in &by_call {
        if let Some(ok) = results.get(call) {
            for &i in rows {
                pending[i].touch.succeeded = Some(*ok);
            }
        }
    }
    let cwd = session.cwd.clone().or_else(|| r.cwd.clone());
    let mut repo = cwd.as_deref().filter(|_| checkout).and_then(git_root).map(|root| Repo {
        root,
        paths: HashMap::new(),
        blobs: None,
    });
    let frozen = !checkout || crate::env::frozen();
    let mut state: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        let Pending {
            mut touch,
            abs,
            payload,
        } = p;
        let abs = if Path::new(&abs).is_absolute() {
            abs
        } else {
            absolutize_path(&touch.path, cwd.as_deref())
        };
        touch.path = display_path(&abs, repo.as_ref().map(|r| r.root.as_path()), cwd.as_deref());
        if touch.op == FileOp::Read || touch.succeeded == Some(false) {
            out.push(touch);
            continue;
        }
        let before = state.get(&abs).cloned().flatten();
        let (after, hunk) = match &payload {
            Payload::Write(content) => {
                touch.lines_changed = Some((lines(content), before.as_deref().map_or(0, lines)));
                (Some(content.clone()), None)
            }
            Payload::Edits(edits) => {
                let applied = before.as_deref().and_then(|b| apply(b, edits));
                match applied {
                    Some((text, added, removed)) => {
                        touch.lines_changed = Some((added, removed));
                        (Some(text), None)
                    }
                    None => {
                        touch.lines_changed = Some((
                            edits.iter().map(|e| lines(&e.1)).sum(),
                            edits.iter().map(|e| lines(&e.0)).sum(),
                        ));
                        let hunk = edits
                            .last()
                            .map(|e| e.1.clone())
                            .filter(|h| h.bytes().filter(|b| !b.is_ascii_whitespace()).count() >= MIN_HUNK_BYTES);
                        (None, hunk)
                    }
                }
            }
            Payload::None => (None, None),
        };
        let after = after.filter(|s| s.len() <= MAX_STATE_BYTES);
        state.insert(abs.clone(), after.clone());
        let rel = repo.as_ref().and_then(|r| {
            Path::new(&abs)
                .strip_prefix(&r.root)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
        let in_git = match (&mut repo, rel, touch.timestamp) {
            (Some(repo), Some(rel), Some(ts)) => match (&after, &hunk) {
                (Some(content), _) => repo.first_matching(&rel, ts, |blob| blob == content.as_bytes()),
                (None, Some(hunk)) => repo.first_matching(&rel, ts, |blob| contains(blob, hunk.as_bytes())),
                (None, None) => None,
            },
            _ => None,
        };
        touch.verification = if let Some(v) = in_git {
            v
        } else if let Some(content) = &after {
            let on_disk = (!frozen)
                .then(|| read_contained_regular_file(&abs, repo.as_ref().map(|r| r.root.as_path()), cwd.as_deref()))
                .flatten()
                .filter(|disk| disk == content.as_bytes());
            match on_disk {
                Some(disk) => FileVerification::VerifiedDisk {
                    blake3: crate::blake3_hex(&disk),
                },
                None => FileVerification::Reconstructed,
            }
        } else {
            FileVerification::Unverified
        };
        out.push(touch);
    }
    // Disk verification describes the checkout now; only the last non-failed write/edit touch of a path can claim it.
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (i, t) in out.iter().enumerate() {
        if t.op != FileOp::Read && t.succeeded != Some(false) {
            last.insert(t.path.as_str(), i);
        }
    }
    let last: std::collections::HashSet<usize> = last.into_values().collect();
    for (i, t) in out.iter_mut().enumerate() {
        if matches!(t.verification, FileVerification::VerifiedDisk { .. }) && !last.contains(&i) {
            t.verification = FileVerification::Reconstructed;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_requires_exact_unique_matches_unless_replace_all() {
        let e = |o: &str, n: &str, all| (o.to_string(), n.to_string(), all);
        assert_eq!(
            apply("a\nb\na\n", &[e("b", "c\nd", false)]),
            Some(("a\nc\nd\na\n".into(), 2, 1))
        );
        assert_eq!(apply("a\nb\na\n", &[e("a", "x", false)]), None);
        assert_eq!(
            apply("a\nb\na\n", &[e("a", "x", true)]),
            Some(("x\nb\nx\n".into(), 2, 2))
        );
        assert_eq!(apply("a", &[e("zz", "x", false)]), None);
    }

    #[test]
    fn one_batch_process_answers_blobs_misses_and_trees_in_order() {
        // #tooth A tree or a miss in between must not desynchronize the next blob's read.
        let world = crate::test_world("files-batch");
        let root = world.base.as_path();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/x.txt"), "line one\nline two\n").unwrap();
        run(&["add", "."]);
        run(&["-c", "commit.gpgsign=false", "commit", "-qm", "x"]);
        let mut blobs = Blobs::spawn(root).unwrap();
        let body = |b: Option<(String, Vec<u8>)>| b.map(|(id, body)| (id.len(), body));
        let want = Some((40, b"line one\nline two\n".to_vec()));
        assert_eq!(body(blobs.get("HEAD:d/x.txt").unwrap()), want);
        assert_eq!(blobs.get("HEAD:nope.txt").unwrap(), None);
        assert_eq!(blobs.get("HEAD:d").unwrap(), None);
        assert_eq!(blobs.get("HEAD:a\nb").unwrap(), None);
        assert_eq!(body(blobs.get("HEAD:d/x.txt").unwrap()), want);
    }

    #[test]
    fn disk_containment_oversized_blob_drain_and_failed_trailing_edit_preserve_verified_disk() {
        // #tooth Oversized git blobs drain without desynchronizing `cat-file --batch`, paths outside
        // `cwd`/`repo.root` or symlinks or `/dev/zero` are never read, oversized `Write` inputs stop
        // in `JsonView`, and a trailing failed edit does not strip an earlier `VerifiedDisk`.
        let world = crate::test_world("files-containment-and-failed-last");
        let work = world.base.join("repo");
        let outside = world.base.join("outside.txt");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(&outside, "secret").unwrap();
        std::fs::write(work.join("good.rs"), "fn ok() {}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, work.join("link.txt")).unwrap();

        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(args)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap()
                .status
                .success());
        };
        run(&["init", "-q"]);
        std::fs::write(work.join("huge.bin"), vec![b'x'; MAX_STATE_BYTES + 64]).unwrap();
        std::fs::write(work.join("small.txt"), "hello\n").unwrap();
        run(&["add", "huge.bin", "small.txt"]);
        run(&["-c", "commit.gpgsign=false", "commit", "-qm", "blobs"]);
        std::fs::remove_file(work.join("huge.bin")).unwrap();

        let mut blobs = Blobs::spawn(&work).unwrap();
        assert_eq!(blobs.get("HEAD:huge.bin").unwrap(), None);
        assert_eq!(
            blobs.get("HEAD:small.txt").unwrap().map(|(_, b)| b),
            Some(b"hello\n".to_vec())
        );

        assert_eq!(read_contained_regular_file("/dev/zero", Some(&work), Some(&work)), None);
        assert_eq!(
            read_contained_regular_file(outside.to_str().unwrap(), Some(&work), Some(&work)),
            None
        );
        #[cfg(unix)]
        assert_eq!(
            read_contained_regular_file(work.join("link.txt").to_str().unwrap(), Some(&work), Some(&work)),
            None
        );

        let huge_json: crate::json::Json = serde_json::json!({
            "file_path": work.join("huge.txt"),
            "content": "a".repeat(MAX_STATE_BYTES + 16),
        })
        .into();
        assert!(matches!(payload_from_json(&huge_json, true).unwrap(), Payload::None));

        let proj = world.home.join(".claude/projects/-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let sess_path = proj.join("s1.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user", "sessionId": "s1", "uuid": "u1", "cwd": work,
                "timestamp": "2026-06-01T10:00:00Z",
                "message": {"role": "user", "content": "write good.rs"}
            }),
            serde_json::json!({
                "type": "assistant", "sessionId": "s1", "uuid": "a1", "cwd": work,
                "timestamp": "2026-06-01T10:00:01Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t_ok", "name": "Write",
                     "input": {"file_path": work.join("good.rs"), "content": "fn ok() {}\n"}},
                    {"type": "tool_use", "id": "t_out", "name": "Write",
                     "input": {"file_path": outside, "content": "secret"}},
                    {"type": "tool_use", "id": "t_fail", "name": "Edit",
                     "input": {"file_path": work.join("good.rs"), "old_string": "missing", "new_string": "bad"}}
                ]}
            }),
            serde_json::json!({
                "type": "user", "sessionId": "s1", "uuid": "r1", "cwd": work,
                "timestamp": "2026-06-01T10:00:02Z",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t_ok", "content": "ok", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "t_out", "content": "ok", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "t_fail", "content": "not found", "is_error": true}
                ]}
            }),
        ];
        let raw: String = lines.iter().map(|v| format!("{v}\n")).collect();
        std::fs::write(&sess_path, raw).unwrap();
        let adapter = crate::harness::claude::Claude::detect();
        let mut sref = SessionRef::from_path(&sess_path, crate::ir::Harness::Claude);
        sref.cwd = Some(work.clone());
        let touches = read(&adapter, &sref).unwrap();
        assert_eq!(touches.len(), 3);
        assert!(
            matches!(touches[0].verification, FileVerification::VerifiedDisk { .. }),
            "earlier succeeded write must stay VerifiedDisk when later edit failed: {:?}",
            touches
        );
        assert_eq!(touches[1].verification, FileVerification::Reconstructed);
        assert_eq!(touches[2].succeeded, Some(false));
    }
}
