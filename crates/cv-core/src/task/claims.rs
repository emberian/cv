//! Structured note support, evaluated against a single task snapshot and local checkout.
use super::{Note, TaskProjection, TaskReadModel};
use crate::source::FileId;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceRef {
    Message { revision: String, at: usize },
    Receipt { event: String, digest: String },
    TaskEvent { store: String, id: String },
    File { repo: PathBuf, path: String, blob: String },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    FileAt {
        path: String,
        blob: String,
    },
    ChoiceIs {
        task: String,
        choice: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        final_only: bool,
    },
    TaskActive {
        task: String,
    },
    /// The subject session had `message_count` messages when the note was written.
    SessionAt {
        session: String,
        message_count: usize,
    },
    /// The subject task had `event_count` events when the note was written.
    TaskSeqAt {
        task: String,
        event_count: usize,
    },
}
pub type AllOf = Vec<Condition>;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    Fact,
    Fail,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub kind: ClaimKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default = "unconditional", skip_serializing_if = "is_unconditional")]
    pub any_of: Vec<AllOf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolves: Vec<String>,
}
fn unconditional() -> Vec<AllOf> {
    vec![vec![]]
}
fn is_unconditional(terms: &[AllOf]) -> bool {
    matches!(terms, [one] if one.is_empty())
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}
impl Claim {
    pub(super) fn admissible(&self) -> bool {
        let valid_atom = |condition: &Condition| match condition {
            Condition::FileAt { path, blob } => relative(path) && blake3::Hash::from_hex(blob).is_ok(),
            Condition::TaskActive { task } => !task.trim().is_empty(),
            Condition::ChoiceIs { task, choice, .. } => !task.trim().is_empty() && !choice.trim().is_empty(),
            Condition::SessionAt { session, .. } => !session.trim().is_empty() && session.len() <= 512,
            Condition::TaskSeqAt { task, .. } => !task.trim().is_empty(),
        };
        self.scope.len() <= 64
            && self.scope.iter().all(|path| relative(path))
            && self.resolves.len() <= 64
            && self.evidence.len() <= 64
            && self.any_of.len() <= 32
            && self.any_of.iter().map(Vec::len).sum::<usize>() <= 128
            && self.any_of.iter().flatten().all(valid_atom)
            && serde_json::to_vec(self).is_ok_and(|v| v.len() <= 65536)
    }
    pub fn covers(&self, path: &Path, repo: Option<&Path>) -> bool {
        let path = if path.is_absolute() {
            let Some(rel) = repo.and_then(|r| path.strip_prefix(r).ok()) else {
                return false;
            };
            rel
        } else {
            path
        };
        (self.scope.is_empty() && repo.is_some())
            || self
                .scope
                .iter()
                .any(|s| path.starts_with(s) || Path::new(s).starts_with(path))
    }
}
// Order makes min conjunction and max disjunction in three-valued logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applicability {
    Stale,
    Unknown,
    Current,
}
impl From<bool> for Applicability {
    fn from(b: bool) -> Self {
        if b {
            Self::Current
        } else {
            Self::Stale
        }
    }
}
/// Read-time caches for one evaluation pass: file digests and subject session lengths.
#[derive(Default)]
pub struct FileDigests {
    files: HashMap<(PathBuf, FileId), blake3::Hash>,
    sessions: HashMap<String, Option<usize>>,
}

/// A subject cursor against its current position: unchanged is current, advanced is stale, and a
/// position behind the recorded one (or unreadable) is unknown.
fn cursor(recorded: usize, now: Option<usize>) -> Applicability {
    match now {
        Some(now) if now == recorded => Applicability::Current,
        Some(now) if now > recorded => Applicability::Stale,
        _ => Applicability::Unknown,
    }
}

/// The current message count of the session a hint names.
pub fn session_length(hint: &str) -> Option<usize> {
    resolve_session_hint(hint)?.message_count
}

impl FileDigests {
    fn session(&mut self, hint: &str) -> Option<usize> {
        *self
            .sessions
            .entry(hint.to_owned())
            .or_insert_with(|| session_length(hint))
    }

    fn at(&mut self, repo: Option<&Path>, path: &str, blob: &str) -> Applicability {
        use Applicability::*;
        let Some(root) = repo.and_then(|p| p.canonicalize().ok()).filter(|p| p.is_dir()) else {
            return Unknown;
        };
        if !relative(path) {
            return Unknown;
        }
        let named = root.join(path);
        let path = match named.canonicalize() {
            Ok(p) if p.starts_with(&root) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Stale,
            _ => return Unknown,
        };
        let Some(id) = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| FileId::of(&m))
        else {
            return Unknown;
        };
        let key = (path, id);
        if let Some(hash) = self.files.get(&key) {
            return hash.to_hex().as_str().eq_ignore_ascii_case(blob).into();
        }
        let digest = (|| {
            let f = std::fs::File::open(&key.0).ok()?;
            let meta = f.metadata().ok()?;
            if !meta.is_file() || FileId::of(&meta) != Some(id) {
                return None;
            }
            let hash = blake3::Hasher::new().update_reader(f).ok()?.finalize();
            (FileId::at(&key.0) == Some(id)).then_some(hash)
        })();
        let Some(hash) = digest else { return Unknown };
        self.files.insert(key, hash);
        hash.to_hex().as_str().eq_ignore_ascii_case(blob).into()
    }

    /// Check `path` (absolute under `repo`, or relative to `repo`) against `blob`.
    pub fn check_path(&mut self, repo: Option<&Path>, path: &str, blob: &str) -> Applicability {
        let p = Path::new(path);
        if p.is_absolute() {
            let Some(root) = repo else {
                return Applicability::Unknown;
            };
            if let Some(rel) = p.strip_prefix(root).ok().and_then(Path::to_str) {
                return self.at(repo, rel, blob);
            }
            let Ok(canon_root) = root.canonicalize() else {
                return Applicability::Unknown;
            };
            let resolved = p.canonicalize().ok().or_else(|| {
                p.ancestors().skip(1).find_map(|anc| {
                    let tail = p.strip_prefix(anc).ok()?.to_str()?;
                    if !relative(tail) {
                        return None;
                    }
                    Some(anc.canonicalize().ok()?.join(tail))
                })
            });
            if let Some(rel_s) = resolved
                .as_deref()
                .and_then(|cp| cp.strip_prefix(&canon_root).ok())
                .and_then(Path::to_str)
            {
                return self.at(Some(&canon_root), rel_s, blob);
            }
            return Applicability::Unknown;
        }
        self.at(repo, path, blob)
    }
}

/// Compute the BLAKE3 hex digest of a regular file at `rel_path` inside `repo`, verifying `FileId`
/// stability across the read.
pub fn hash_checkout_file(repo: &Path, rel_path: &str) -> Option<String> {
    if !relative(rel_path) {
        return None;
    }
    let root = repo.canonicalize().ok().filter(|p| p.is_dir())?;
    let path = root.join(rel_path).canonicalize().ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let f = std::fs::File::open(&path).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let id = FileId::of(&meta)?;
    let hash = blake3::Hasher::new().update_reader(f).ok()?.finalize();
    (FileId::at(&path) == Some(id)).then(|| hash.to_hex().to_string())
}

impl Claim {
    /// Parse a claim from a shorthand (`"fact"` / `"fail"`) or JSON object, merging optional
    /// `--scope` and `--resolves` flags. Returns `(claim, explicit_any_of)`.
    pub fn parse_spec(
        raw: Option<&str>,
        extra_scope: &[String],
        extra_resolves: &[String],
    ) -> anyhow::Result<Option<(Self, bool)>> {
        if raw.is_none() && extra_scope.is_empty() && extra_resolves.is_empty() {
            return Ok(None);
        }
        let (mut claim, explicit_any_of) = match raw.map(str::trim) {
            None => (
                Claim {
                    kind: ClaimKind::Fact,
                    scope: Vec::new(),
                    evidence: Vec::new(),
                    any_of: unconditional(),
                    resolves: Vec::new(),
                },
                false,
            ),
            Some(s) if s.eq_ignore_ascii_case("fact") => (
                Claim {
                    kind: ClaimKind::Fact,
                    scope: Vec::new(),
                    evidence: Vec::new(),
                    any_of: unconditional(),
                    resolves: Vec::new(),
                },
                false,
            ),
            Some(s) if s.eq_ignore_ascii_case("fail") || s.eq_ignore_ascii_case("failure") => (
                Claim {
                    kind: ClaimKind::Fail,
                    scope: Vec::new(),
                    evidence: Vec::new(),
                    any_of: unconditional(),
                    resolves: Vec::new(),
                },
                false,
            ),
            Some(json_str) => {
                let val: serde_json::Value = serde_json::from_str(json_str)?;
                let explicit = val.as_object().is_some_and(|o| o.contains_key("any_of"));
                (serde_json::from_value(val)?, explicit)
            }
        };
        for s in extra_scope {
            let s = s.trim().trim_start_matches("./");
            if !s.is_empty() && !claim.scope.iter().any(|have| have == s) {
                claim.scope.push(s.to_string());
            }
        }
        for r in extra_resolves {
            let r = r.trim();
            if !r.is_empty() && !claim.resolves.iter().any(|have| have == r) {
                claim.resolves.push(r.to_string());
            }
        }
        Ok(Some((claim, explicit_any_of)))
    }
}

fn resolve_session_hint(hint: &str) -> Option<crate::ir::SessionRef> {
    let mut s = hint.trim();
    if s.is_empty() || matches!(s, "cv" | "agent") || s.starts_with("web:") {
        return None;
    }
    let receipt_holder: String;
    if let Some(rest) = s.strip_prefix("receipt:") {
        let (harness, rem) = rest.split_once(':')?;
        let (sess, _) = rem.rsplit_once(':')?;
        receipt_holder = format!("{harness}:{sess}");
        s = &receipt_holder;
    } else if let Some(rest) = s.strip_prefix("agent:") {
        s = rest.trim();
    }
    if let Some((prefix, rest)) = s.split_once(':') {
        if let Some(harness) = crate::ir::Harness::parse(prefix) {
            let p = Path::new(rest);
            if p.is_file() {
                return Some(crate::ir::SessionRef::from_path(p.to_path_buf(), harness));
            }
            return crate::find(rest, Some(harness)).ok().flatten().map(|(r, _)| r);
        }
    }
    if let Some((r, _)) = crate::find(s, None).ok().flatten() {
        return Some(r);
    }
    if !s.starts_with("agent-") {
        if let Some((r, _)) = crate::find(&format!("agent-{s}"), None).ok().flatten() {
            return Some(r);
        }
    }
    None
}

fn repo_rel_path(root: &Path, base: &Path, raw: &str) -> Option<String> {
    let p = Path::new(raw);
    let full = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    let resolved = full
        .canonicalize()
        .ok()
        .unwrap_or_else(|| crate::tools::classify::normalize_path(&full, None));
    let rel = resolved
        .strip_prefix(root)
        .ok()
        .or_else(|| full.strip_prefix(root).ok())?;
    let rel_str = rel.to_str()?.trim_start_matches("./");
    relative(rel_str).then(|| rel_str.to_string())
}

fn collect_bounded_dir_files(root: &Path, dir: &Path, limit: usize) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let entries = std::fs::read_dir(&cur).ok()?;
        for entry in entries.flatten() {
            let ft = entry.file_type().ok()?;
            if ft.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if name == ".git" || name == "target" || name == "node_modules" {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let canon = path.canonicalize().ok()?;
                let rel = canon.strip_prefix(root).ok()?.to_str()?;
                if relative(rel) {
                    out.push(rel.to_string());
                    if out.len() > limit {
                        return None;
                    }
                }
            }
        }
    }
    out.sort();
    Some(out)
}

/// Automatically populate `FileAt` (from session-touched/read files or `--scope` checkout files)
/// and `ChoiceIs` (from active provisional decisions) when `any_of` was not manually specified.
///
/// A peer session named by `session_hint` and the task's non-decision blockers are pinned at their
/// current length, so the note goes stale once either subject advances. An explicit `--scope`
/// directory too large to pin fails rather than yield a claim nothing could invalidate.
pub fn populate_auto_conditions(
    claim: &mut Claim,
    explicit_any_of: bool,
    model: &TaskReadModel,
    task_id: &str,
    session_hint: Option<&str>,
    actor_hint: Option<&str>,
) -> anyhow::Result<()> {
    if explicit_any_of || !is_unconditional(&claim.any_of) {
        return Ok(());
    }
    const MAX_AUTO_FILES: usize = 32;
    const MAX_SCOPE_DIR_FILES: usize = 16;
    const MAX_AUTO_DECISIONS: usize = 16;
    const MAX_AUTO_TASKS: usize = 16;

    let task_proj = model.tasks.get(task_id);
    let mut conjunction: Vec<Condition> = Vec::new();

    if let Some(root) = task_proj
        .and_then(|t| t.repo.as_deref())
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir())
    {
        let mut file_blobs: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        let session = session_hint
            .and_then(resolve_session_hint)
            .or_else(|| actor_hint.and_then(resolve_session_hint));

        if let Some(sess) = session {
            let base = sess.cwd.as_deref().unwrap_or(&root);
            let recorded =
                crate::events::workspace::observed_file_blobs(sess.harness.as_str(), &sess.id).unwrap_or_default();
            let mut recorded_by_rel = std::collections::BTreeMap::new();
            for (raw_path, blob) in &recorded {
                if let Some(rel) = repo_rel_path(&root, base, raw_path) {
                    recorded_by_rel.insert(rel, blob.clone());
                }
            }
            let mut candidates: Vec<String> = Vec::new();
            if let Ok(rows) = crate::events::read_ref(&sess, None) {
                // Prioritize edits first, then reads, preserving deterministic order.
                for want_kind in ["file_edit", "file_read"] {
                    for row in rows.iter().filter(|r| r.kind == want_kind) {
                        if let Some(target) = &row.target {
                            if let Some(rel) = repo_rel_path(&root, base, target) {
                                if !candidates.contains(&rel) {
                                    candidates.push(rel);
                                }
                            }
                        }
                    }
                }
            }
            for rel in recorded_by_rel.keys() {
                if !candidates.contains(rel) {
                    candidates.push(rel.clone());
                }
            }
            for rel in candidates {
                if file_blobs.len() >= MAX_AUTO_FILES {
                    break;
                }
                if !claim.scope.is_empty() && !claim.covers(Path::new(&rel), Some(&root)) {
                    continue;
                }
                if let Some(blob) = recorded_by_rel
                    .get(&rel)
                    .cloned()
                    .or_else(|| hash_checkout_file(&root, &rel))
                {
                    file_blobs.insert(rel, blob);
                }
            }
        }

        for s in &claim.scope {
            if file_blobs.len() >= MAX_AUTO_FILES {
                break;
            }
            let candidate = root.join(s);
            if let Ok(canon) = candidate.canonicalize() {
                if !canon.starts_with(&root) {
                    continue;
                }
                if canon.is_file() {
                    if let Ok(rel) = canon.strip_prefix(&root) {
                        if let Some(rel_s) = rel.to_str().filter(|r| relative(r)) {
                            if !file_blobs.contains_key(rel_s) {
                                if let Some(blob) = hash_checkout_file(&root, rel_s) {
                                    file_blobs.insert(rel_s.to_string(), blob);
                                }
                            }
                        }
                    }
                } else if canon.is_dir() && !file_blobs.keys().any(|k| Path::new(k).starts_with(s)) {
                    match collect_bounded_dir_files(&root, &canon, MAX_SCOPE_DIR_FILES) {
                        Some(dir_files) => {
                            for rel_s in dir_files {
                                if file_blobs.len() >= MAX_AUTO_FILES {
                                    break;
                                }
                                if let Some(blob) = hash_checkout_file(&root, &rel_s) {
                                    file_blobs.insert(rel_s, blob);
                                }
                            }
                        }
                        None => anyhow::bail!(
                            "--scope {s} holds more than {MAX_SCOPE_DIR_FILES} files; scope the claim to files or \
                             pass explicit any_of conditions"
                        ),
                    }
                }
            }
        }

        for (path, blob) in file_blobs {
            conjunction.push(Condition::FileAt { path, blob });
        }
    }

    // A peer session the note points at, distinct from the author's own.
    if let Some(hint) = session_hint {
        let own = actor_hint.and_then(resolve_session_hint).map(|r| (r.harness, r.id));
        if let Some(peer) = resolve_session_hint(hint).filter(|r| own.as_ref() != Some(&(r.harness, r.id.clone()))) {
            if let Some(message_count) = peer.message_count {
                conjunction.push(Condition::SessionAt {
                    session: format!("{}:{}", peer.harness, peer.id),
                    message_count,
                });
            }
        }
    }
    if let Some(task) = task_proj {
        let mut blockers: Vec<_> = task
            .blocked_by
            .iter()
            .filter_map(|id| model.tasks.get(id))
            .filter(|b| b.decision.is_none())
            .collect();
        blockers.sort_by_key(|b| &b.task_id);
        for b in blockers.into_iter().take(MAX_AUTO_TASKS) {
            conjunction.push(Condition::TaskSeqAt {
                task: b.task_id.clone(),
                event_count: b.event_count,
            });
        }
    }

    // Attach active provisional decisions blocking this task or in the same checkout.
    let mut prov_decisions: Vec<(&str, &str)> = model
        .tasks
        .values()
        .filter(|d_task| d_task.task_id != task_id && d_task.state != super::TaskState::Superseded)
        .filter(|d_task| {
            let blocks_this = task_proj.is_some_and(|t| t.blocked_by.iter().any(|b| b == &d_task.task_id));
            let same_repo = task_proj.and_then(|t| t.repo.as_ref()).is_some()
                && d_task.repo == task_proj.and_then(|t| t.repo.clone());
            blocks_this || same_repo
        })
        .filter_map(|d_task| {
            let res = d_task.decision.as_ref()?.resolution.as_ref()?;
            res.provisional
                .then_some((d_task.task_id.as_str(), res.choice.as_str()))
        })
        .collect();
    prov_decisions.sort_by_key(|(id, _)| *id);
    prov_decisions.truncate(MAX_AUTO_DECISIONS);
    for (task, choice) in prov_decisions {
        conjunction.push(Condition::ChoiceIs {
            task: task.to_string(),
            choice: choice.to_string(),
            final_only: false,
        });
    }

    if !conjunction.is_empty() {
        claim.any_of = vec![conjunction];
    }
    Ok(())
}
impl Claim {
    /// How far each pinned subject has advanced since the note: `+Δ messages in <session>` and
    /// `+Δ events on <task>`, for the cursors that moved forward.
    pub fn drift(&self, model: &TaskReadModel, files: &mut FileDigests) -> Vec<String> {
        let mut out = Vec::new();
        for atom in self.any_of.iter().flatten() {
            match atom {
                Condition::SessionAt { session, message_count } => {
                    if let Some(now) = files.session(session).filter(|n| n > message_count) {
                        out.push(format!("+{} messages in {session}", now - message_count));
                    }
                }
                Condition::TaskSeqAt { task, event_count } => {
                    if let Some(now) = model.tasks.get(task).map(|t| t.event_count).filter(|n| n > event_count) {
                        out.push(format!("+{} events on {task}", now - event_count));
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub fn evaluate(&self, model: &TaskReadModel, repo: Option<&Path>, files: &mut FileDigests) -> Applicability {
        use Applicability::*;
        let mut disjunction = Stale;
        for term in &self.any_of {
            let mut conjunction = Current;
            for atom in term {
                let value = match atom {
                    Condition::FileAt { path, blob } => files.at(repo, path, blob),
                    Condition::TaskActive { task } => model
                        .tasks
                        .get(task)
                        .map_or(Unknown, |t| (!t.state.is_terminal()).into()),
                    Condition::ChoiceIs {
                        task,
                        choice,
                        final_only,
                    } => model
                        .tasks
                        .get(task)
                        .and_then(|t| t.decision.as_ref())
                        .and_then(|d| d.resolution.as_ref())
                        .map_or(Unknown, |r| {
                            (r.choice == *choice && !(*final_only && r.provisional)).into()
                        }),
                    Condition::SessionAt { session, message_count } => cursor(*message_count, files.session(session)),
                    Condition::TaskSeqAt { task, event_count } => {
                        cursor(*event_count, model.tasks.get(task).map(|t| t.event_count))
                    }
                };
                conjunction = conjunction.min(value);
                if conjunction == Stale {
                    break;
                }
            }
            disjunction = disjunction.max(conjunction);
            if disjunction == Current {
                break;
            }
        }
        disjunction
    }
}

pub struct Finding<'a> {
    pub note: &'a Note,
    pub claim: &'a Claim,
    pub applicability: Applicability,
    pub addressed: bool,
}
pub fn findings(model: &TaskReadModel, include: impl Fn(&TaskProjection) -> bool) -> Vec<Finding<'_>> {
    let mut files = FileDigests::default();
    let mut rows = Vec::new();
    for task in model.tasks.values().filter(|t| include(t)) {
        for note in &task.notes {
            if let Some(claim) = &note.claim {
                rows.push(Finding {
                    note,
                    claim,
                    applicability: claim.evaluate(model, task.repo.as_deref(), &mut files),
                    addressed: false,
                });
            }
        }
    }
    let addressed: HashSet<_> = rows
        .iter()
        .filter(|r| r.applicability == Applicability::Current)
        .flat_map(|r| r.claim.resolves.iter().cloned())
        .collect();
    for row in &mut rows {
        row.addressed = addressed.contains(row.note.event_id.as_str());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_dnf_empty_missing_and_file_content() {
        // #tooth No checkout is unknown. A missing path in an identified checkout is stale.
        let w = crate::test_world("task-dnf");
        let model = TaskReadModel::default();
        let parse = |v| serde_json::from_value::<Claim>(v).unwrap();
        let mut files = FileDigests::default();
        let mut eval = |v, root: Option<&Path>| parse(v).evaluate(&model, root, &mut files);
        let root = Some(w.base.as_path());
        assert_eq!(eval(json!({"kind":"fact"}), None), Applicability::Current);
        assert_eq!(eval(json!({"kind":"fact","any_of":[]}), None), Applicability::Stale);
        let atom = json!({"kind":"file_at","path":"source","blob":blake3::hash(b"old").to_hex().as_str()});
        let claim = json!({"kind":"fail","any_of":[[atom]]});
        assert_eq!(eval(claim.clone(), None), Applicability::Unknown);
        assert_eq!(eval(claim.clone(), root), Applicability::Stale);
        std::fs::write(w.base.join("source"), b"old").unwrap();
        assert_eq!(eval(claim.clone(), root), Applicability::Current);
        std::fs::write(w.base.join("source"), b"new").unwrap();
        assert_eq!(eval(claim, root), Applicability::Stale);
        let unknown = json!({"kind":"task_active","task":"absent"});
        assert_eq!(
            eval(json!({"kind":"fact","any_of":[[unknown,atom]]}), root),
            Applicability::Stale
        );
        assert_eq!(
            eval(json!({"kind":"fact","any_of":[[unknown],[atom]]}), root),
            Applicability::Unknown
        );
        assert!(!parse(json!({"kind":"fact","scope":vec!["src";65]})).admissible());
    }

    #[test]
    fn subject_cursors_flip_stale_when_the_subject_advances() {
        // #tooth A note pinned to a peer task's length goes stale, with its drift, once the peer
        // records another event; an unresolvable session is unknown, never current.
        let open = crate::task::new_event(None, "lead", crate::task::opened_kind("peer", None));
        let mut events = vec![open.clone()];
        let model = crate::task::TaskReducer::reduce(&events).unwrap();
        let claim: Claim = serde_json::from_value(json!({"kind":"fact","any_of":[[
            {"kind":"task_seq_at","task":open.id,"event_count":model.tasks[&open.id].event_count}
        ]]}))
        .unwrap();
        assert!(claim.admissible());
        let mut files = FileDigests::default();
        assert_eq!(claim.evaluate(&model, None, &mut files), Applicability::Current);
        events.push(crate::task::new_event(
            Some(&open.id),
            "peer",
            crate::task::TaskEventKind::Noted {
                text: "moved on".into(),
                session_ref: None,
                claim: None,
            },
        ));
        let advanced = crate::task::TaskReducer::reduce(&events).unwrap();
        assert_eq!(claim.evaluate(&advanced, None, &mut files), Applicability::Stale);
        assert_eq!(
            claim.drift(&advanced, &mut files),
            [format!("+1 events on {}", open.id)]
        );

        let _w = crate::test_world("task-session-cursor");
        let session: Claim = serde_json::from_value(json!({"kind":"fact","any_of":[[
            {"kind":"session_at","session":"claude:no-such-session","message_count":3}
        ]]}))
        .unwrap();
        assert_eq!(session.evaluate(&model, None, &mut files), Applicability::Unknown);
    }

    #[test]
    fn oversized_scope_directory_fails_closed() {
        // #tooth A directory scope too large to pin never yields a claim nothing can invalidate.
        let w = crate::test_world("task-scope-large");
        std::fs::create_dir_all(w.base.join("big")).unwrap();
        for i in 0..17 {
            std::fs::write(w.base.join(format!("big/f{i}.rs")), format!("{i}")).unwrap();
        }
        let open = crate::task::new_event(
            None,
            "author",
            crate::task::TaskEventKind::Opened {
                title: "audit".into(),
                body: String::new(),
                repo: Some(w.base.clone()),
                issue: None,
                channel: None,
                assignee: None,
            },
        );
        let model = crate::task::TaskReducer::reduce(std::slice::from_ref(&open)).unwrap();
        let (mut claim, explicit) = Claim::parse_spec(Some("fact"), &["big".into()], &[]).unwrap().unwrap();
        let err = populate_auto_conditions(&mut claim, explicit, &model, &open.id, None, None).unwrap_err();
        assert!(err.to_string().contains("more than 16 files"), "{err}");

        // Even when an earlier scope entry already populated `file_blobs`, an oversized sibling
        // scope directory still fails closed rather than dropping its files silently.
        std::fs::write(w.base.join("small.rs"), "pub fn ok() {}\n").unwrap();
        let (mut mixed, explicit) = Claim::parse_spec(Some("fact"), &["small.rs".into(), "big".into()], &[])
            .unwrap()
            .unwrap();
        let err = populate_auto_conditions(&mut mixed, explicit, &model, &open.id, None, None).unwrap_err();
        assert!(err.to_string().contains("more than 16 files"), "{err}");

        #[cfg(unix)]
        {
            let real_repo = w.base.join("real-repo");
            std::fs::create_dir_all(real_repo.join("sub")).unwrap();
            let linked_repo = w.base.join("linked-repo");
            std::os::unix::fs::symlink(&real_repo, &linked_repo).unwrap();
            let file_in_real = real_repo.join("sub/gone.rs");
            std::fs::write(&file_in_real, b"v1").unwrap();
            let blob = blake3::hash(b"v1").to_hex().to_string();
            let mut digests = FileDigests::default();
            assert_eq!(
                digests.check_path(Some(&linked_repo), file_in_real.to_str().unwrap(), &blob),
                Applicability::Current
            );
            std::fs::remove_file(&file_in_real).unwrap();
            let mut digests = FileDigests::default();
            assert_eq!(
                digests.check_path(Some(&linked_repo), file_in_real.to_str().unwrap(), &blob),
                Applicability::Stale,
                "deleted file under a symlinked repo root must be Stale, not Unknown"
            );
        }
    }

    #[test]
    fn auto_conditions_pin_scoped_files_and_provisional_decisions_without_manual_dnf() {
        // #tooth Without manual any_of JSON, file edits or provisional vetoes flip support stale.
        let w = crate::test_world("task-auto-claim");
        std::fs::create_dir_all(w.base.join("src")).unwrap();
        std::fs::write(w.base.join("src/parser.rs"), b"fn parse() {}").unwrap();

        let open_work = crate::task::new_event(
            None,
            "author",
            crate::task::TaskEventKind::Opened {
                title: "audit parser".into(),
                body: String::new(),
                repo: Some(w.base.clone()),
                issue: None,
                channel: None,
                assignee: Some("worker".into()),
            },
        );
        let open_dec = crate::task::new_event(
            None,
            "author",
            crate::task::TaskEventKind::Opened {
                title: "parser mode".into(),
                body: String::new(),
                repo: Some(w.base.clone()),
                issue: None,
                channel: None,
                assignee: Some("owner".into()),
            },
        );
        let posed = crate::task::new_event(
            Some(&open_dec.id),
            "author",
            crate::task::TaskEventKind::Posed {
                options: vec!["strict".into(), "lenient".into()],
                default_choice: "strict".into(),
                deadline: None,
                source: None,
            },
        );
        let prov = crate::task::new_event(
            Some(&open_dec.id),
            "author",
            crate::task::TaskEventKind::Resolved {
                choice: "strict".into(),
                note: None,
                provisional: true,
            },
        );
        let events = vec![open_work.clone(), open_dec.clone(), posed, prov];
        let model = crate::task::TaskReducer::reduce(&events).unwrap();

        let (mut claim, explicit) = Claim::parse_spec(Some("fail"), &["src/parser.rs".into()], &[])
            .unwrap()
            .unwrap();
        assert!(!explicit);
        populate_auto_conditions(&mut claim, explicit, &model, &open_work.id, None, None).unwrap();
        assert!(claim.admissible());
        assert_eq!(claim.any_of.len(), 1);
        assert_eq!(claim.any_of[0].len(), 2, "one FileAt + one provisional ChoiceIs");

        let mut digests = FileDigests::default();
        assert_eq!(
            claim.evaluate(&model, Some(&w.base), &mut digests),
            Applicability::Current
        );

        // Editing the scoped file flips the claim Stale with zero task-log writes.
        std::fs::write(w.base.join("src/parser.rs"), b"fn parse() -> Result<()> { Ok(()) }").unwrap();
        let mut digests2 = FileDigests::default();
        assert_eq!(
            claim.evaluate(&model, Some(&w.base), &mut digests2),
            Applicability::Stale
        );

        // Restoring the file makes it Current again until the provisional decision is vetoed.
        std::fs::write(w.base.join("src/parser.rs"), b"fn parse() {}").unwrap();
        let veto = crate::task::new_event(
            Some(&open_dec.id),
            "owner",
            crate::task::TaskEventKind::Resolved {
                choice: "lenient".into(),
                note: None,
                provisional: false,
            },
        );
        let mut after_events = events;
        after_events.push(veto);
        let vetoed_model = crate::task::TaskReducer::reduce(&after_events).unwrap();
        let mut digests3 = FileDigests::default();
        assert_eq!(
            claim.evaluate(&vetoed_model, Some(&w.base), &mut digests3),
            Applicability::Stale
        );
    }
}
