//! A sub-agent transcript split across sessions by an in-place `/resume`.
//!
//! A running sub-agent writes `<project>/<session>/subagents/[workflows/<run>/]agent-<id>.jsonl`,
//! and `agent-<id>.meta.json` once, at its spawn. When the process that owns it resumes another
//! session in place, the agent keeps running, and its next record goes to the same place under the
//! *resumed* session, stamped with that session's id. The new file starts mid-conversation: its
//! first `uuid` record's `parentUuid` is the last record of the file it continues, and no meta is
//! written beside it. A second `/resume` back to the first session appends to the first file again,
//! starting a second contiguous run inside it whose first `uuid` record points back to the middle
//! session's leaf.
//!
//! Per-project agent placement is indexed once per directory state ([`ProjectAgents`]), so an agent
//! that exists under only one session (99.99% of agents) short-circuits in one `HashMap` lookup
//! with zero `stat`s or file opens. Only multi-session places scan candidate sibling files into
//! contiguous byte-range [`Run`]s and join them by exact parsed `uuid` membership.

use crate::harness::store::document::Fields;
use crate::harness::store::jsonl;
use crate::ir::{Harness, SessionRef};
use crate::source::{FileId, Probe};
use crate::stream::Flow;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_PROJECTS: usize = 64;
const MAX_SEGMENTS: usize = 1024;

/// One contiguous byte slice `[start, end)` of an agent segment file in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunSlice {
    pub path: PathBuf,
    pub start: u64,
    pub end: u64,
}

fn is_agent_jsonl(name: &str) -> bool {
    name.starts_with("agent-")
        && name.ends_with(".jsonl")
        && !name.ends_with(".flat.jsonl")
        && name.len() > "agent-.jsonl".len()
}

fn dir_name(p: &Path) -> Option<&str> {
    p.file_name()?.to_str()
}

/// `(session_dir, below)` for `<project>/<session>/subagents/[workflows/<run>/]agent-<id>.jsonl`.
pub(super) fn place(path: &Path) -> Option<(&Path, &Path)> {
    let name = path.file_name()?.to_str()?;
    if !is_agent_jsonl(name) {
        return None;
    }
    let dir = path.parent()?;
    let subagents = match dir.parent().and_then(dir_name) {
        Some("workflows") => dir.parent()?.parent()?,
        _ => dir,
    };
    if dir_name(subagents) != Some("subagents") {
        return None;
    }
    let session = subagents.parent()?;
    session.file_name()?.to_str()?;
    session.parent()?;
    Some((session, path.strip_prefix(session).ok()?))
}

/// The session id a segment is filed under: its `<session>` directory's name.
pub(super) fn session_of(path: &Path) -> Option<String> {
    Some(place(path)?.0.file_name()?.to_str()?.to_owned())
}

/// The owning parent session's `SessionRef` (`<project>/<session>.jsonl`) for a sub-agent path.
pub(super) fn owner(path: &Path) -> Option<SessionRef> {
    let (session_dir, _) = place(path)?;
    let sid = session_dir.file_name()?.to_str()?;
    let project = session_dir.parent()?;
    Some(SessionRef::new(
        sid,
        Harness::Claude,
        project.join(format!("{sid}.jsonl")),
    ))
}

fn real_dir_id(path: &Path) -> Option<FileId> {
    let md = fs::symlink_metadata(path).ok()?;
    md.file_type().is_dir().then(|| FileId::of(&md)).flatten()
}

/// Per-project agent placement index: maps relative `subagents/[workflows/<run>/]agent-<id>.jsonl`
/// to the sorted list of `<project>/<session>` directories holding a regular file there.
pub(super) struct ProjectAgents {
    dirs: Vec<(PathBuf, Option<FileId>)>,
    places: HashMap<PathBuf, Vec<PathBuf>>,
}

impl ProjectAgents {
    fn fresh(&self, probe: &mut Option<&mut Probe>) -> bool {
        match probe.as_deref_mut() {
            Some(p) => self
                .dirs
                .iter()
                .all(|(dir, expected)| p.file_id(dir) == Some(*expected)),
            None => self.dirs.iter().all(|(dir, expected)| real_dir_id(dir) == *expected),
        }
    }

    fn scan(project: &Path) -> Option<Self> {
        let project_id = real_dir_id(project)?;
        let mut dirs = vec![(project.to_path_buf(), Some(project_id))];
        let mut places: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

        let mut session_dirs: Vec<PathBuf> = fs::read_dir(project)
            .ok()?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
            .map(|e| e.path())
            .collect();
        session_dirs.sort();

        for session_dir in session_dirs {
            let subagents = session_dir.join("subagents");
            let sub_id = real_dir_id(&subagents);
            dirs.push((subagents.clone(), sub_id));
            if sub_id.is_none() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(&subagents) {
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
                        continue;
                    }
                    let fname = entry.file_name();
                    if let Some(name) = fname.to_str().filter(|n| is_agent_jsonl(n)) {
                        places
                            .entry(Path::new("subagents").join(name))
                            .or_default()
                            .push(session_dir.clone());
                    }
                }
            }
            let workflows = subagents.join("workflows");
            let wf_id = real_dir_id(&workflows);
            dirs.push((workflows.clone(), wf_id));
            if wf_id.is_none() {
                continue;
            }
            let mut run_dirs: Vec<(std::ffi::OsString, PathBuf)> = fs::read_dir(&workflows)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
                .map(|e| (e.file_name(), e.path()))
                .collect();
            run_dirs.sort_by(|a, b| a.0.cmp(&b.0));
            for (run_name, run_dir) in run_dirs {
                let run_id = real_dir_id(&run_dir);
                dirs.push((run_dir.clone(), run_id));
                if run_id.is_none() {
                    continue;
                }
                if let Ok(entries) = fs::read_dir(&run_dir) {
                    for entry in entries.flatten() {
                        if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
                            continue;
                        }
                        let fname = entry.file_name();
                        if let Some(name) = fname.to_str().filter(|n| is_agent_jsonl(n)) {
                            let below = Path::new("subagents").join("workflows").join(&run_name).join(name);
                            places.entry(below).or_default().push(session_dir.clone());
                        }
                    }
                }
            }
        }
        for holders in places.values_mut() {
            holders.sort();
            holders.dedup();
        }
        Some(Self { dirs, places })
    }

    fn holders(&self, below: &Path) -> &[PathBuf] {
        self.places.get(below).map_or(&[], Vec::as_slice)
    }
}

fn project_agents(project: &Path, mut probe: Option<&mut Probe>) -> Option<Arc<ProjectAgents>> {
    type ProjectMemo = HashMap<PathBuf, Arc<ProjectAgents>>;
    static MEMO: OnceLock<Mutex<ProjectMemo>> = OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);

    let cached = memo
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(project)
        .cloned();
    if let Some(hit) = cached.filter(|c| c.fresh(&mut probe)) {
        return Some(hit);
    }

    let scanned = Arc::new(ProjectAgents::scan(project)?);
    let mut check_probe = None;
    if scanned.fresh(&mut check_probe) {
        let mut guard = memo.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.len() >= MAX_PROJECTS && !guard.contains_key(project) {
            guard.clear();
        }
        guard.insert(project.to_path_buf(), Arc::clone(&scanned));
    }
    Some(scanned)
}

/// Contiguous record run `[start, end)` within a single segment file.
#[derive(Debug, Clone)]
struct Run {
    start: u64,
    end: u64,
    first_uuid: Option<String>,
    external_parent: Option<String>,
    last_uuid: Option<String>,
    last_turn_uuid: Option<String>,
    uuids: HashSet<String>,
}

impl Run {
    fn starting_at(start: u64) -> Self {
        Self {
            start,
            end: start,
            first_uuid: None,
            external_parent: None,
            last_uuid: None,
            last_turn_uuid: None,
            uuids: HashSet::new(),
        }
    }

    fn absorb(&mut self, next: Run) {
        self.end = next.end;
        if self.first_uuid.is_none() {
            self.first_uuid = next.first_uuid;
            self.external_parent = next.external_parent;
        }
        if next.last_uuid.is_some() {
            self.last_uuid = next.last_uuid;
        }
        if next.last_turn_uuid.is_some() {
            self.last_turn_uuid = next.last_turn_uuid;
        }
        self.uuids.extend(next.uuids);
    }
}

#[derive(Debug)]
struct Segment {
    modified: Option<SystemTime>,
    raw_runs: Vec<Run>,
    uuids: HashSet<String>,
}

impl Segment {
    /// Merge internal splits whose `external_parent` is not held by any sibling session, leaving
    /// only genuine cross-session runs (`A -> B -> A`).
    fn effective_runs(&self, sibling_holds: impl Fn(&str) -> bool) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::with_capacity(self.raw_runs.len());
        for run in &self.raw_runs {
            let split = !out.is_empty() && run.external_parent.as_deref().is_some_and(&sibling_holds);
            if split {
                out.push(run.clone());
            } else if let Some(prev) = out.last_mut() {
                prev.absorb(run.clone());
            } else {
                out.push(run.clone());
            }
        }
        out
    }
}

#[cfg(test)]
thread_local! {
    static SEGMENT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn scan_segment(path: &Path) -> Option<Arc<Segment>> {
    type SegmentMemo = HashMap<PathBuf, (FileId, Arc<Segment>)>;
    static MEMO: OnceLock<Mutex<SegmentMemo>> = OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);

    let md = fs::symlink_metadata(path).ok()?;
    if !md.file_type().is_file() {
        return None;
    }
    let before = FileId::of(&md)?;
    if let Some((_, hit)) = memo
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .filter(|(seen, _)| *seen == before)
    {
        return Some(Arc::clone(hit));
    }

    #[cfg(test)]
    SEGMENT_SCANS.with(|n| n.set(n.get() + 1));

    let read = jsonl::Read::open(path).ok()?;
    let len = read.len();
    let modified = md
        .modified()
        .ok()
        .or_else(|| UNIX_EPOCH.checked_add(Duration::from_nanos(before.mtime_ns)));
    let mut prev_end = 0u64;
    let mut file_uuids: HashSet<String> = HashSet::new();
    let mut raw_runs: Vec<Run> = Vec::new();
    let mut cur = Run::starting_at(0);
    let mut last_turn_at: Option<DateTime<Utc>> = None;
    let mut stopped_at: Option<DateTime<Utc>> = None;

    read.lines(0, |line| {
        if let Some(view) = line.view {
            if let Ok(Some(fields)) = Fields::parse(view, &["type", "uuid", "parentUuid", "timestamp", "attachment"]) {
                let ty = fields.string("type")?.unwrap_or_default();
                let ts = fields.string("timestamp")?.as_deref().and_then(super::parse_ts);
                if matches!(ty.as_str(), "user" | "assistant") {
                    if let Some(ts) = ts {
                        last_turn_at = Some(last_turn_at.map_or(ts, |old| old.max(ts)));
                    }
                } else if ty == "attachment" {
                    let hook = fields
                        .get("attachment")
                        .map(|v| v.get("hookEvent"))
                        .transpose()?
                        .flatten();
                    if hook
                        .map(|v| crate::projection::string_prefix(v, 13))
                        .transpose()?
                        .flatten()
                        .as_deref()
                        == Some("SubagentStop")
                    {
                        if let Some(ts) = ts {
                            stopped_at = Some(stopped_at.map_or(ts, |old| old.max(ts)));
                        }
                    }
                }
                if let Some(u) = fields.string("uuid")? {
                    let parent = fields.string("parentUuid")?;
                    if cur.first_uuid.is_some() && parent.as_ref().is_some_and(|p| !file_uuids.contains(p)) {
                        cur.end = prev_end;
                        raw_runs.push(std::mem::replace(&mut cur, Run::starting_at(prev_end)));
                    }
                    if cur.first_uuid.is_none() {
                        cur.first_uuid = Some(u.clone());
                        cur.external_parent = parent.clone().filter(|p| !file_uuids.contains(p));
                    }
                    if matches!(ty.as_str(), "user" | "assistant") {
                        cur.last_turn_uuid = Some(u.clone());
                    }
                    cur.last_uuid = Some(u.clone());
                    cur.uuids.insert(u.clone());
                    file_uuids.insert(u);
                }
            }
        }
        prev_end = line.end;
        Ok(Flow::Continue)
    })
    .ok()?;

    let _ = (last_turn_at, stopped_at);
    cur.end = len;
    raw_runs.push(cur);
    let seg = Arc::new(Segment {
        modified,
        raw_runs,
        uuids: file_uuids,
    });
    if fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_file())
        .and_then(|m| FileId::of(&m))
        == Some(before)
    {
        let mut guard = memo.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.len() >= MAX_SEGMENTS && !guard.contains_key(path) {
            guard.clear();
        }
        guard.insert(path.to_path_buf(), (before, Arc::clone(&seg)));
    }
    Some(seg)
}

struct CandidateSet {
    files: Vec<(PathBuf, Arc<Segment>, Vec<Run>)>,
}

impl CandidateSet {
    fn load(project: &ProjectAgents, below: &Path) -> Self {
        let holders = project.holders(below);
        let loaded: Vec<(PathBuf, Arc<Segment>)> = holders
            .iter()
            .filter_map(|dir| {
                let p = dir.join(below);
                Some((p.clone(), scan_segment(&p)?))
            })
            .collect();
        let files = loaded
            .iter()
            .map(|(path, seg)| {
                let runs = seg.effective_runs(|parent| {
                    loaded
                        .iter()
                        .any(|(other, other_seg)| other != path && other_seg.uuids.contains(parent))
                });
                (path.clone(), Arc::clone(seg), runs)
            })
            .collect();
        Self { files }
    }

    fn get(&self, path: &Path) -> Option<(&Arc<Segment>, &[Run])> {
        self.files
            .iter()
            .find(|(p, _, _)| p == path)
            .map(|(_, s, r)| (s, r.as_slice()))
    }
}

fn runs_in_project(path: &Path, project: Option<&ProjectAgents>) -> Vec<RunSlice> {
    let single = || {
        vec![RunSlice {
            path: path.to_path_buf(),
            start: 0,
            end: u64::MAX,
        }]
    };
    let Some((session, below)) = place(path) else {
        return single();
    };
    let Some(project) = project else {
        return single();
    };
    let holders = project.holders(below);
    if holders.iter().all(|dir| dir == session) {
        return single();
    }

    let set = CandidateSet::load(project, below);
    let temp_seg;
    let temp_runs;
    let (_, start_runs) = match set.get(path) {
        Some(found) => found,
        None => {
            let Some(seg) = scan_segment(path) else {
                return single();
            };
            temp_runs = seg.effective_runs(|parent| set.files.iter().any(|(_, s, _)| s.uuids.contains(parent)));
            temp_seg = seg;
            let _ = &temp_seg;
            (&temp_seg, temp_runs.as_slice())
        }
    };
    let Some(last_run) = start_runs.last() else {
        return single();
    };

    let mut visited: HashSet<(PathBuf, u64)> = HashSet::new();
    let mut cur_path = path.to_path_buf();
    let mut cur_run = last_run.clone();
    visited.insert((cur_path.clone(), cur_run.start));

    let mut slices = vec![RunSlice {
        path: cur_path.clone(),
        start: cur_run.start,
        end: cur_run.end,
    }];

    while let Some(parent) = cur_run.external_parent.clone() {
        let best = set
            .files
            .iter()
            .filter(|(cand_path, _, _)| *cand_path != cur_path)
            .flat_map(|(cand_path, cand_seg, cand_runs)| {
                cand_runs.iter().map(move |r| (cand_path, cand_seg.modified, r))
            })
            .filter(|(cand_path, _, r)| {
                !visited.contains(&((*cand_path).clone(), r.start)) && r.uuids.contains(&parent)
            })
            .min_by_key(|(cand_path, mtime, r)| {
                let not_run_leaf = r.last_uuid.as_deref() != Some(parent.as_str())
                    && r.last_turn_uuid.as_deref() != Some(parent.as_str());
                (not_run_leaf, mtime.is_none(), *mtime, *cand_path, r.start)
            });

        let Some((prev_path, _, prev_run)) = best else {
            break;
        };
        cur_path = prev_path.clone();
        cur_run = prev_run.clone();
        visited.insert((cur_path.clone(), cur_run.start));
        slices.push(RunSlice {
            path: cur_path.clone(),
            start: cur_run.start,
            end: cur_run.end,
        });
    }

    slices.reverse();
    let mut coalesced: Vec<RunSlice> = Vec::with_capacity(slices.len());
    for slice in slices {
        if let Some(prev) = coalesced
            .last_mut()
            .filter(|prev| prev.path == slice.path && prev.end == slice.start)
        {
            prev.end = slice.end;
        } else {
            coalesced.push(slice);
        }
    }
    coalesced
}

/// Ordered byte-range slices `(path, start..end)` for reading `path` root-to-tip.
/// Unsplit agents return one full-file slice `(path, 0..u64::MAX)` in 1 `HashMap` lookup.
pub(super) fn runs(path: &Path) -> Vec<RunSlice> {
    let Some((session, _)) = place(path) else {
        return vec![RunSlice {
            path: path.to_path_buf(),
            start: 0,
            end: u64::MAX,
        }];
    };
    let project = session.parent().and_then(|p| project_agents(p, None));
    runs_in_project(path, project.as_deref())
}

/// `(prefix_runs, tail_start)` for `path`: `prefix_runs` are the slices strictly before `path`'s
/// final run (oldest first), and `tail_start` is the byte offset where `path`'s final run begins
/// (`0` unless `path` re-entered after another session).
pub(super) fn prefix_and_tail(path: &Path) -> (Vec<RunSlice>, u64) {
    let mut all = runs(path);
    let tail = all.pop().expect("runs always includes at least the target slice");
    debug_assert_eq!(tail.path, path);
    (all, tail.start)
}

/// Distinct predecessor segment files consulted before `path` (`p != path`), oldest first.
pub(super) fn predecessors_with_probe(path: &Path, probe: Option<&mut Probe>) -> Vec<PathBuf> {
    let Some((session, _)) = place(path) else {
        return Vec::new();
    };
    let Some(project_dir) = session.parent() else {
        return Vec::new();
    };
    let project = project_agents(project_dir, probe);
    let all = runs_in_project(path, project.as_deref());
    if all.len() <= 1 {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for slice in &all[..all.len() - 1] {
        if slice.path != path && seen.insert(slice.path.clone()) {
            out.push(slice.path.clone());
        }
    }
    out
}

/// The meta written at the spawn: `path`'s own when present, else the oldest segment's.
pub(super) fn meta(path: &Path) -> PathBuf {
    let own = path.with_extension("meta.json");
    if own.is_file() {
        return own;
    }
    let all = runs(path);
    if all.len() > 1 {
        if let Some(root) = all.first() {
            return root.path.with_extension("meta.json");
        }
    }
    own
}

/// Per-project lookup handle for resolving each forest member to its newest continuation.
pub(crate) struct Beside {
    project: Option<Arc<ProjectAgents>>,
}

impl Beside {
    /// Load the project's agent placement index for `transcript` (`<project>/<sid>.jsonl`).
    pub(crate) fn of(transcript: &Path) -> Beside {
        let project = transcript.parent().and_then(|p| project_agents(p, None));
        Beside { project }
    }

    /// `path`, or the segment continuing it under another session, followed to the newest tip.
    pub(crate) fn tip(&self, path: &Path) -> PathBuf {
        let Some((session, below)) = place(path) else {
            return path.to_path_buf();
        };
        let Some(project) = self.project.as_deref() else {
            return path.to_path_buf();
        };
        let holders = project.holders(below);
        if holders.iter().all(|dir| dir == session) {
            return path.to_path_buf();
        }

        let set = CandidateSet::load(project, below);
        let Some((_, start_runs)) = set.get(path) else {
            return path.to_path_buf();
        };
        let Some(last_run) = start_runs.last() else {
            return path.to_path_buf();
        };

        let mut cur_path = path.to_path_buf();
        let mut cur_run = last_run.clone();
        let mut visited: HashSet<(PathBuf, u64)> = HashSet::new();
        visited.insert((cur_path.clone(), cur_run.start));

        while let Some((next_path, next_run)) = self.next_run(&set, &cur_path, &cur_run, &visited) {
            visited.insert((next_path.clone(), next_run.start));
            cur_path = next_path;
            cur_run = next_run;
        }
        cur_path
    }

    fn next_run(
        &self,
        set: &CandidateSet,
        cur_path: &Path,
        cur_run: &Run,
        visited: &HashSet<(PathBuf, u64)>,
    ) -> Option<(PathBuf, Run)> {
        set.files
            .iter()
            .filter(|(cand_path, _, _)| cand_path != cur_path)
            .flat_map(|(cand_path, cand_seg, cand_runs)| {
                cand_runs.iter().map(move |r| (cand_path, cand_seg.modified, r))
            })
            .filter(|(cand_path, _, r)| {
                !visited.contains(&((*cand_path).clone(), r.start))
                    && r.external_parent
                        .as_deref()
                        .is_some_and(|parent| cur_run.uuids.contains(parent))
            })
            .max_by_key(|(cand_path, mtime, r)| (mtime.is_some(), *mtime, *cand_path, r.start))
            .map(|(cand_path, _, r)| (cand_path.clone(), r.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn write(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, text.join("\n") + "\n").unwrap();
    }

    fn rec(uuid: &str, parent: Option<&str>, text: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "isSidechain": true,
               "message": {"role": "user", "content": text}})
    }

    fn before_paths(path: &Path) -> Vec<PathBuf> {
        prefix_and_tail(path).0.into_iter().map(|s| s.path).collect()
    }

    #[test]
    fn a_segment_names_the_one_it_continues_and_a_forest_its_newest() {
        let w = crate::test_world("claude-agent-chain");
        let project = w.base.join("projects").join("-p");
        let agent = |session: &str| project.join(session).join("subagents").join("agent-a1.jsonl");
        // s1 spawned it; a resume moved it to s2, then to s3. s0 holds a `cv adopt` copy of s1
        // that starts its own chain.
        write(&agent("s1"), &[rec("x1", None, "x1"), rec("x2", Some("x1"), "x2")]);
        fs::write(agent("s1").with_extension("meta.json"), r#"{"agentType":"t"}"#).unwrap();
        write(
            &agent("s2"),
            &[rec("y1", Some("x2"), "y1"), rec("y2", Some("y1"), "y2")],
        );
        write(&agent("s3"), &[rec("z1", Some("y2"), "z1")]);
        write(&agent("s0"), &[rec("x1", None, "x1"), rec("x2", Some("x1"), "x2")]);
        for s in ["s0", "s1", "s2", "s3"] {
            fs::write(project.join(format!("{s}.jsonl")), "").unwrap();
        }

        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(agent("s1"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(
            before_paths(&agent("s3")),
            vec![agent("s1"), agent("s2")],
            "the first written of two holders"
        );
        assert!(prefix_and_tail(&agent("s1")).0.is_empty(), "a head continues nothing");
        assert!(
            prefix_and_tail(&agent("s0")).0.is_empty(),
            "a copy starts its own chain"
        );
        assert_eq!(meta(&agent("s3")), agent("s1").with_extension("meta.json"));
        assert_eq!(session_of(&agent("s2")).as_deref(), Some("s2"));

        for s in ["s1", "s2", "s3"] {
            let beside = Beside::of(&project.join(format!("{s}.jsonl")));
            assert_eq!(beside.tip(&agent(s)), agent("s3"), "from {s}");
        }
    }

    #[test]
    fn reentry_across_three_runs_orders_byte_slices_and_resolves_both_tips_to_latest() {
        let w = crate::test_world("claude-agent-reentry");
        let project = w.base.join("projects").join("-p");
        for below in ["subagents/agent-a1.jsonl", "subagents/workflows/wf_1/agent-w1.jsonl"] {
            let agent = |s: &str| project.join(s).join(below);
            let run0 = jsonl_bytes(&[rec("x1", None, "one"), rec("x2", Some("x1"), "two")]);
            let run1 = jsonl_bytes(&[rec("y1", Some("x2"), "three"), rec("y2", Some("y1"), "four")]);
            let run2 = jsonl_bytes(&[rec("z1", Some("y2"), "five"), rec("z2", Some("z1"), "six")]);

            fs::create_dir_all(agent("sA").parent().unwrap()).unwrap();
            fs::create_dir_all(agent("sB").parent().unwrap()).unwrap();
            let mut a_bytes = run0.clone();
            a_bytes.extend_from_slice(&run2);
            fs::write(agent("sA"), &a_bytes).unwrap();
            fs::write(agent("sB"), &run1).unwrap();
            fs::write(project.join("sA.jsonl"), "").unwrap();
            fs::write(project.join("sB.jsonl"), "").unwrap();

            assert_eq!(
                runs(&agent("sA")),
                vec![
                    RunSlice {
                        path: agent("sA"),
                        start: 0,
                        end: run0.len() as u64,
                    },
                    RunSlice {
                        path: agent("sB"),
                        start: 0,
                        end: run1.len() as u64,
                    },
                    RunSlice {
                        path: agent("sA"),
                        start: run0.len() as u64,
                        end: a_bytes.len() as u64,
                    },
                ],
                "reading A interleaves B between A's two runs ({below})"
            );
            assert_eq!(
                runs(&agent("sB")),
                vec![
                    RunSlice {
                        path: agent("sA"),
                        start: 0,
                        end: run0.len() as u64,
                    },
                    RunSlice {
                        path: agent("sB"),
                        start: 0,
                        end: run1.len() as u64,
                    },
                ],
                "reading B includes only A's earlier run, never A's future z1..z2 ({below})"
            );
            for s in ["sA", "sB"] {
                let beside = Beside::of(&project.join(format!("{s}.jsonl")));
                assert_eq!(beside.tip(&agent(s)), agent("sA"), "tip from {s} ({below})");
            }
        }
    }

    #[test]
    fn holds_uuid_ignores_quoted_text_and_duplicate_parent_uuid_tails() {
        let w = crate::test_world("claude-agent-holds-uuid-exact");
        let project = w.base.join("projects").join("-p");
        let agent = |s: &str| project.join(s).join("subagents").join("agent-a1.jsonl");
        // s1 (original root) was deleted; s_quote quotes `"x2"` in body text and api-request-blob;
        // s2 and s3 both start with `parentUuid: "x2"`. Neither s_quote nor s2 holds record uuid `x2`.
        write(
            &agent("s_quote"),
            &[
                rec("q1", None, "quoted uuid \"x2\" inside content"),
                json!({"type": "api-request-blob", "body": "\"uuid\":\"x2\""}),
            ],
        );
        write(
            &agent("s2"),
            &[rec("y1", Some("x2"), "tail one"), rec("y2", Some("y1"), "tail two")],
        );
        write(
            &agent("s3"),
            &[rec("y1", Some("x2"), "tail one"), rec("y2", Some("y1"), "tail two")],
        );
        for s in ["s_quote", "s2", "s3"] {
            fs::write(project.join(format!("{s}.jsonl")), "").unwrap();
        }
        assert!(prefix_and_tail(&agent("s2")).0.is_empty());
        assert!(prefix_and_tail(&agent("s3")).0.is_empty());
    }

    #[test]
    fn unsplit_agents_short_circuit_with_zero_segment_file_opens() {
        let w = crate::test_world("claude-agent-unsplit-zero-opens");
        let project = w.base.join("projects").join("-p");
        fs::create_dir_all(&project).unwrap();
        for i in 0..8 {
            let sid = format!("s{i}");
            fs::write(project.join(format!("{sid}.jsonl")), "").unwrap();
            let a = project
                .join(&sid)
                .join("subagents")
                .join(format!("agent-unique{i}.jsonl"));
            write(&a, &[rec(&format!("u{i}"), None, "solo")]);
        }
        jsonl::OPENED.with(|o| o.borrow_mut().clear());
        SEGMENT_SCANS.with(|n| n.set(0));
        let mut probe = Probe::default();
        for i in 0..8 {
            let sid = format!("s{i}");
            let a = project
                .join(&sid)
                .join("subagents")
                .join(format!("agent-unique{i}.jsonl"));
            let beside = Beside::of(&project.join(format!("{sid}.jsonl")));
            assert_eq!(beside.tip(&a), a);
            assert!(prefix_and_tail(&a).0.is_empty());
            assert!(predecessors_with_probe(&a, Some(&mut probe)).is_empty());
        }
        assert_eq!(SEGMENT_SCANS.with(std::cell::Cell::get), 0);
        assert!(jsonl::OPENED.with(|o| o.borrow().is_empty()));
    }

    fn jsonl_bytes(lines: &[Value]) -> Vec<u8> {
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        (text.join("\n") + "\n").into_bytes()
    }
}
