//! A session's file-touch timeline: every read, write and edit a tool call made, in message order,
//! with whether it succeeded and how far its result can be verified.
//!
//! Verification, strongest first:
//! - `verified_git`: a commit at or after the touch, in the session's repository, holds exactly the
//!   reconstructed post-touch content, or (for an edit whose base is unknown) contains its
//!   replacement text.
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
use std::path::{Path, PathBuf};

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

/// The arguments of a single-file write or edit call; anything else is `Payload::None`.
fn payload(input: &Value, full_write: bool) -> Payload {
    if full_write {
        return str_field(input, &["content", "file_text"]).map_or(Payload::None, Payload::Write);
    }
    if let Some(edits) = input.get("edits").and_then(Value::as_array) {
        let edits: Option<Vec<_>> = edits.iter().map(edit_of).collect();
        return edits.map_or(Payload::None, Payload::Edits);
    }
    edit_of(input).map_or(Payload::None, |e| Payload::Edits(vec![e]))
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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

/// Later commits of one path, oldest first, each read at most once.
struct PathHistory {
    commits: Vec<(String, i64)>,
    blobs: HashMap<String, Option<(String, Vec<u8>)>>,
}

struct Repo {
    root: PathBuf,
    paths: HashMap<String, PathHistory>,
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
        let root = self.root.clone();
        let h = self.history(rel);
        let later: Vec<String> = h
            .commits
            .iter()
            .filter(|(_, ct)| *ct >= since.timestamp())
            .take(MAX_COMMITS_PER_PATH)
            .map(|(sha, _)| sha.clone())
            .collect();
        for sha in later {
            let blob = h.blobs.entry(sha.clone()).or_insert_with(|| {
                let spec = format!("{sha}:{rel}");
                let id = String::from_utf8(git(&root, &["rev-parse", &spec])?).ok()?;
                let id = id.trim().to_string();
                let body = git(&root, &["cat-file", "blob", &id])?;
                Some((id, body))
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
                            payload(input.resolve()?.as_ref(), op == FileOp::Write)
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
        crate::stream::Pass::RECORD,
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
    let mut repo = cwd.as_deref().and_then(git_root).map(|root| Repo {
        root,
        paths: HashMap::new(),
    });
    let frozen = crate::env::frozen();
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
                .then(|| std::fs::read(&abs).ok())
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
    // Disk verification describes the checkout now; only the last touch of a path can claim it.
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (i, t) in out.iter().enumerate() {
        if t.op != FileOp::Read {
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
}
