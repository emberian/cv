//! A sub-agent transcript split across sessions by an in-place `/resume`.
//!
//! A running sub-agent writes `<project>/<session>/subagents/[workflows/<run>/]agent-<id>.jsonl`,
//! and `agent-<id>.meta.json` once, at its spawn. When the process that owns it resumes another
//! session in place, the agent keeps running, and its next record goes to the same place under the
//! *resumed* session, stamped with that session's id. The new file starts mid-conversation: its
//! first record's `parentUuid` is the last record of the file it continues, and no meta is written
//! beside it. The spawning tool call, the meta and every earlier turn stay under the first
//! session; nothing else on disk joins the two files.
//!
//! The record chain is the join. A segment whose first `uuid` record names a parent continues the
//! segment that holds that parent, at the same place under another session of the same project.
//! A transcript whose first record has no parent starts its own chain (every agent that was never
//! split, and every `cv adopt` copy), and costs one head read to know it. A whole read of the
//! newest segment reads every segment, oldest first, as one agent; a forest names an agent by its
//! newest segment from whichever session it is listed under.

use super::record::Facts;
use crate::harness::store::jsonl;
use crate::stream::Flow;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How far a head read goes before giving up on finding the first `uuid` record.
const HEAD_LINES: usize = 64;

/// `(session dir, path below it)` of an agent transcript: `<session>` and
/// `subagents/[workflows/<run>/]agent-<id>.jsonl`.
fn place(path: &Path) -> Option<(&Path, &Path)> {
    let session = path
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "subagents"))?
        .parent()?;
    Some((session, path.strip_prefix(session).ok()?))
}

/// The session a segment is filed under: its directory's name.
pub(super) fn session_of(path: &Path) -> Option<String> {
    Some(place(path)?.0.file_name()?.to_str()?.to_owned())
}

/// The first `uuid` record's parent, when it names one: what a continuation starts from.
fn first_parent(path: &Path) -> Option<String> {
    let read = jsonl::Read::open(path).ok()?;
    let mut parent = None;
    let mut lines = 0;
    read.lines(0, |line| {
        lines += 1;
        if let Some(view) = line.view {
            let (facts, _) = Facts::parse(view)?;
            if facts.uuid.is_some() {
                parent = facts.parent.flatten();
                return Ok(Flow::Stop);
            }
        }
        Ok(if lines == HEAD_LINES {
            Flow::Stop
        } else {
            Flow::Continue
        })
    })
    .ok()?;
    parent
}

/// The other sessions' copies of `path`'s place, in name order.
fn elsewhere(path: &Path) -> Vec<PathBuf> {
    let Some((session, below)) = place(path) else {
        return Vec::new();
    };
    let Some(project) = session.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(project) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|dir| dir != session)
        .map(|dir| dir.join(below))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// The segments `path` continues, oldest first; empty when its first record has no parent or
/// no other session holds that parent at the same place. Of two holders (a `cv adopt` copy of
/// the segment) the one written first is the original. A loop ends at the first segment seen
/// again.
pub(super) fn before(path: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    if place(path).is_none() {
        return chain;
    }
    let mut at = path.to_path_buf();
    while let Some(parent) = first_parent(&at) {
        let needle = format!("\"{parent}\"");
        let Some(previous) = elsewhere(&at)
            .into_iter()
            .filter(|p| p != path && !chain.contains(p) && super::fork::holds(p, &needle))
            .min_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        else {
            break;
        };
        chain.push(previous.clone());
        at = previous;
    }
    chain.reverse();
    chain
}

/// The meta written at the spawn: `path`'s own, else the oldest segment's.
pub(super) fn meta(path: &Path) -> PathBuf {
    let own = path.with_extension("meta.json");
    if own.is_file() {
        return own;
    }
    before(path)
        .first()
        .map(|root| root.with_extension("meta.json"))
        .unwrap_or(own)
}

/// The project's other sessions, listed once for a forest: where its agents may have continued.
pub(super) struct Beside {
    /// Each other session's dir and the time its `subagents/` dir last changed. A continuation's
    /// file is created there after its segment's last write, so an older dir holds none.
    sessions: Vec<(PathBuf, SystemTime)>,
}

impl Beside {
    /// The sessions beside the one whose transcript is `transcript`.
    pub(super) fn of(transcript: &Path) -> Beside {
        let own = super::sidecar::session_dir(transcript);
        let sessions = transcript
            .parent()
            .and_then(|project| fs::read_dir(project).ok())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|dir| Some(dir) != own.as_ref())
            .filter_map(|dir| {
                let changed = fs::metadata(dir.join("subagents")).ok()?.modified().ok()?;
                Some((dir, changed))
            })
            .collect();
        Beside { sessions }
    }

    /// `path`, or the segment continuing it under another session, followed to the newest.
    pub(super) fn tip(&self, path: &Path) -> PathBuf {
        let mut seen = vec![path.to_path_buf()];
        while let Some(next) = self.next(seen.last().unwrap()).filter(|n| !seen.contains(n)) {
            seen.push(next);
        }
        seen.pop().unwrap()
    }

    fn next(&self, path: &Path) -> Option<PathBuf> {
        let (session, below) = place(path)?;
        let written = fs::metadata(path).ok()?.modified().ok()?;
        // A workflow agent's file lands a tier deeper, where `subagents/`'s time does not move.
        let direct = below.parent() == Some(Path::new("subagents"));
        self.sessions
            .iter()
            .filter(|(dir, changed)| dir != session && (!direct || *changed >= written))
            .map(|(dir, _)| dir.join(below))
            .filter(|p| p.is_file())
            .find(|p| first_parent(p).is_some_and(|parent| super::fork::holds(path, &format!("\"{parent}\""))))
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

    fn rec(uuid: &str, parent: Option<&str>) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "isSidechain": true,
               "message": {"role": "user", "content": uuid}})
    }

    #[test]
    fn a_segment_names_the_one_it_continues_and_a_forest_its_newest() {
        let w = crate::test_world("claude-agent-chain");
        let project = w.base.join("projects").join("-p");
        let agent = |session: &str| project.join(session).join("subagents").join("agent-a1.jsonl");
        // s1 spawned it; a resume moved it to s2, then to s3. s0 holds an unrelated agent of the
        // same id shape and a copy (`cv adopt`) that starts its own chain.
        write(&agent("s1"), &[rec("x1", None), rec("x2", Some("x1"))]);
        fs::write(agent("s1").with_extension("meta.json"), r#"{"agentType":"t"}"#).unwrap();
        write(&agent("s2"), &[rec("y1", Some("x2")), rec("y2", Some("y1"))]);
        write(&agent("s3"), &[rec("z1", Some("y2"))]);
        write(&agent("s0"), &[rec("x1", None), rec("x2", Some("x1"))]);
        for s in ["s0", "s1", "s2", "s3"] {
            fs::write(project.join(format!("{s}.jsonl")), "").unwrap();
        }

        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(agent("s1"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(
            before(&agent("s3")),
            vec![agent("s1"), agent("s2")],
            "the first written of two holders"
        );
        assert_eq!(before(&agent("s1")), Vec::<PathBuf>::new(), "a head continues nothing");
        assert_eq!(
            before(&agent("s0")),
            Vec::<PathBuf>::new(),
            "a copy starts its own chain"
        );
        assert_eq!(meta(&agent("s3")), agent("s1").with_extension("meta.json"));
        assert_eq!(session_of(&agent("s2")).as_deref(), Some("s2"));

        for s in ["s1", "s2", "s3"] {
            let beside = Beside::of(&project.join(format!("{s}.jsonl")));
            assert_eq!(beside.tip(&agent(s)), agent("s3"), "from {s}");
        }
    }
}
