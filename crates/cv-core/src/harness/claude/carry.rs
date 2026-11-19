//! `--carry-agents`: take a session's sub-agents along to a session derived from it.
//!
//! Claude Code re-enters a background agent from the CURRENT session's `<session>/subagents/`:
//! `agent-<id>.meta.json` gives it its model and type, `agent-<id>.jsonl` its brief and what it
//! has done. A derived session (prune, rewind, splice) has a new id and so no such folder.
//! Carrying copies each meta verbatim and (in [`CarryAgents::Full`]) each transcript with the
//! session id restamped ([`crate::restamp_line`], as `cv adopt` does), to the same place under
//! the new id; a workflow's agents keep their run folder.
//!
//! The new session may already hold an agent. One that was mid-turn when its parent moved to
//! the new session keeps running and writes its next records into the NEW session's folder: a
//! new file that begins mid-conversation, with no meta, while the brief and everything before
//! stay in the old folder. So a transcript the destination already holds is joined, never
//! skipped or replaced: the source's records first, then the records the destination holds
//! that the source lacks (by `uuid`), in the order it holds them. A meta is copied wherever the
//! destination has none. Joining again changes nothing, so a carry can be repeated after the
//! move to pick up what an agent wrote to the old folder in between.
//!
//! A transcript goes through the path an adoption takes: the source is captured and checked
//! ([`JsonSource`]), its records restamped by [`crate::derive`], the staged copy checked against
//! the resumability rules ([`crate::derive::verify`]) before anything is published, and the
//! transcript published with its meta as one set. For a join, that checked copy of the source
//! is the head, and the destination's own records are appended after the check: an agent that
//! is still running has an unanswered call the source never had. A source whose last record is
//! unfinished (its writer is mid-append) is carried up to that record, and the report says so.
//! A transcript whose content is refused (a whole line that is not a record, a copy the check
//! will not pass) is skipped and named; its meta still goes, and every other agent is carried.
//!
//! Every file is read and written a record at a time; memory is one record plus the files'
//! record ranges and, for a join, their record ids.

use super::sidecar;
use crate::harness::store::jsonl::ranges::{self, Range};
use crate::ir::Harness;
use crate::json::{InvalidJson, JsonSource};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::{BufRead, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// What of each agent is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryAgents {
    /// The spawn record alone: the agent keeps its model and type.
    Meta,
    /// The spawn record and the transcript: it keeps its brief and its cache too.
    Full,
}

impl std::str::FromStr for CarryAgents {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "meta" => Ok(CarryAgents::Meta),
            "full" => Ok(CarryAgents::Full),
            _ => anyhow::bail!("unknown --carry-agents {s:?} (expected meta or full)"),
        }
    }
}

/// What was carried (or, in a dry run, would be).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Carried {
    /// `<project>/<new id>/subagents`.
    pub dir: PathBuf,
    /// Spawn records (`agent-<id>.meta.json`) copied.
    pub metas: usize,
    /// Transcripts (`agent-<id>.jsonl`) copied, restamped.
    pub transcripts: usize,
    /// Transcripts the destination already held part of: the source's records were put before
    /// the records only the destination holds.
    pub joined: usize,
    /// Copied or joined transcripts whose last record was unfinished when they were read (a
    /// writer was mid-append): carried up to it, never with a broken record.
    pub unfinished: usize,
    /// Files the destination already held whole (a meta of its own, a transcript holding every
    /// source record): left as they are.
    pub already: usize,
    /// Files an earlier source had already supplied (a splice of several sessions holding one
    /// agent): the first source's copy stands.
    pub duplicates: usize,
    /// Bytes of the copied and joined transcripts as written.
    pub transcript_bytes: u64,
    /// Transcripts not carried because of what they hold: skipped, the rest carried. Each one's
    /// meta still goes, so the agent keeps its model and type and loses its brief.
    pub refused: Vec<Refusal>,
}

/// One transcript the carry refused.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Refusal {
    /// The agent id.
    pub agent: String,
    pub why: String,
}

/// A transcript refused for its content: not an error of the carry, which goes on.
#[derive(Debug)]
struct Refused(String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

/// `error` as a [`Refused`] when it is about the transcript's content: a line that is not one
/// JSON record, or a copy the resume check will not pass. An I/O failure stays what it is.
fn refusal(error: anyhow::Error) -> anyhow::Error {
    if error.is::<InvalidJson>() || error.root_cause().downcast_ref::<std::io::Error>().is_none() {
        Refused(format!("{error:#}")).into()
    } else {
        error
    }
}

struct Entry {
    from: PathBuf,
    /// Relative to `subagents/`.
    to: PathBuf,
    transcript: bool,
}

/// Every agent file under `dir`, in name order, sub-folders (workflow runs) included. `only`
/// (agent ids, with or without `agent-`) narrows it; empty takes all.
fn collect(dir: &Path, relative: &Path, mode: CarryAgents, only: &[String], out: &mut Vec<Entry>) -> Result<()> {
    let mut entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let to = relative.join(&name);
        if entry.file_type()?.is_dir() {
            collect(&entry.path(), &to, mode, only, out)?;
            continue;
        }
        let Some(name) = name.to_str() else { continue };
        // `<stem>.flat.jsonl` is a prune sidecar, never a transcript.
        if name.ends_with(".flat.jsonl") {
            continue;
        }
        let (id, transcript) = match (name.strip_suffix(".meta.json"), name.strip_suffix(".jsonl")) {
            (Some(stem), _) => (stem.strip_prefix("agent-"), false),
            (None, Some(stem)) if mode == CarryAgents::Full => (stem.strip_prefix("agent-"), true),
            _ => continue,
        };
        let Some(id) = id else { continue };
        if only.is_empty() || only.iter().any(|o| o.strip_prefix("agent-").unwrap_or(o) == id) {
            out.push(Entry {
                from: entry.path(),
                to,
                transcript,
            });
        }
    }
    Ok(())
}

/// Each line of the first `limit` bytes of `reader`, without its newline; `false` for one the
/// bytes end inside.
fn lines(reader: impl Read, limit: u64, mut each: impl FnMut(&[u8], bool) -> Result<()>) -> Result<()> {
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, reader.take(limit));
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        match line.strip_suffix(b"\n") {
            Some(whole) => each(whole, true)?,
            None => each(&line, false)?,
        }
    }
}

/// A record's `uuid`, when the line is a record that has one.
fn uuid_of(line: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Id {
        uuid: Option<String>,
    }
    serde_json::from_slice::<Id>(line).ok()?.uuid
}

enum Outcome {
    Copied,
    Joined,
    Already,
}

/// What [`transcript`] did with one agent.
struct Done {
    outcome: Outcome,
    bytes: u64,
    unfinished: bool,
    meta: bool,
}

/// A meta staged for `to`, with the guard on its source.
fn stage_meta(from: &Path, to: &Path) -> Result<(crate::publish::Staged, JsonSource, std::fs::File)> {
    let source = JsonSource::capture(from).with_context(|| format!("reading {}", from.display()))?;
    let mut file = std::fs::File::open(from)?;
    source.check_metadata(file.metadata()?)?;
    // No more exposed than the spawn record it copies.
    let mut staged = crate::publish::Staged::beside(to, "carry", crate::publish::mode_of(&file))?;
    std::io::copy(&mut file, &mut staged)?;
    Ok((staged, source, file))
}

/// The source transcript restamped at `to`, joined with what `to` already holds, published
/// with its `meta` (source, destination) when the destination has none.
fn transcript(from: &Path, to: &Path, new_id: &str, meta: Option<(&Path, &Path)>, dry_run: bool) -> Result<Done> {
    use crate::publish::{Claim, Set, Staged};
    let identity = super::IDENTITY;
    let source = JsonSource::capture(from).with_context(|| format!("reading {}", from.display()))?;
    let file = std::fs::File::open(from)?;
    source.check_metadata(file.metadata()?)?;
    let extent = Range {
        offset: 0,
        len: source.len(),
    };
    let mut rows = ranges::records(&file, extent)?.collect::<Result<Vec<(Range, bool)>>>()?;
    let read = |&(range, complete): &(Range, bool)| {
        crate::derive::Select::read(&source, &file, range, complete, true, &identity)
    };
    // A last row with no newline that is not one JSON value: its writer is mid-append.
    let mut unfinished = false;
    if let Some(last) = rows.last().filter(|(_, complete)| !complete) {
        if read(last).is_err_and(|error| error.is::<InvalidJson>()) {
            rows.pop();
            unfinished = true;
        }
    }
    let carried_len = rows.last().map_or(0, |(range, _)| range.offset + range.len);

    let existing = std::fs::symlink_metadata(to).is_ok();
    let mut held = HashSet::new();
    let mut held_len = 0u64;
    if existing {
        held_len = std::fs::metadata(to)?.len();
        lines(std::fs::File::open(to)?, held_len, |line, _| {
            held.extend(uuid_of(line));
            Ok(())
        })?;
    }
    // The carried records' ids, and whether the destination lacks any of them.
    let mut ids = HashSet::new();
    let mut missing = !existing;
    lines(std::fs::File::open(from)?, carried_len, |line, _| {
        if let Some(uuid) = uuid_of(line) {
            missing |= !held.contains(&uuid);
            ids.insert(uuid);
        }
        Ok(())
    })?;
    drop(held);
    let meta = meta.filter(|(_, to)| std::fs::symlink_metadata(to).is_err());
    if !missing {
        // Its meta alone may still be owed.
        if let (Some((meta_from, meta_to)), false) = (meta, dry_run) {
            let (staged, guard, guarded) = stage_meta(meta_from, meta_to)?;
            guard.check_metadata(guarded.metadata()?)?;
            guard.check()?;
            staged.publish(meta_to, Claim::New)?;
        }
        return Ok(Done {
            outcome: Outcome::Already,
            bytes: 0,
            unfinished: false,
            meta: meta.is_some(),
        });
    }
    let rewrite = crate::derive::Rewrite {
        session_id: new_id,
        standalone: false,
        identity,
    };
    let outcome = if existing { Outcome::Joined } else { Outcome::Copied };
    let ends_whole = rows.last().is_none_or(|(_, complete)| *complete);
    if dry_run {
        let head = crate::derive::derive(&source, &file, rows.iter().map(read), &rewrite, &mut std::io::sink())
            .map_err(refusal)?;
        let mut bytes = head.bytes as u64;
        if existing {
            lines(std::fs::File::open(to)?, held_len, |line, whole| {
                if whole && !uuid_of(line).is_some_and(|uuid| ids.contains(&uuid)) {
                    bytes += line.len() as u64 + 1;
                }
                Ok(())
            })?;
        }
        return Ok(Done {
            outcome,
            bytes,
            unfinished,
            meta: meta.is_some(),
        });
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // The head: the source's records, restamped and checked as an adoption's are.
    let mode = crate::publish::mode_of(&file);
    let mut staged = std::io::BufWriter::with_capacity(64 * 1024, Staged::beside(to, "carry", mode)?);
    let head = crate::derive::derive(&source, &file, rows.iter().map(read), &rewrite, &mut staged).map_err(refusal)?;
    let mut staged = staged.into_inner().map_err(|e| e.into_error())?;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let agent = crate::derive::agent_of(from);
    crate::derive::verify(Harness::Claude, &source, &file, agent, staged.path(), to, agent).map_err(refusal)?;
    let mut bytes = head.bytes as u64;
    if existing {
        // Then what only the destination holds, in its order, whole records only. An agent may
        // still be appending: what arrived since the last pass is copied too, until a pass finds
        // nothing new. Its own unfinished last record is left out like the source's.
        let mut out = std::io::BufWriter::with_capacity(64 * 1024, staged.file());
        if !ends_whole {
            out.write_all(b"\n")?;
            bytes += 1;
        }
        let (mut at, mut end) = (0u64, held_len);
        let mut cut = false;
        for _ in 0..8 {
            let mut reader = std::fs::File::open(to)?;
            reader.seek(std::io::SeekFrom::Start(at))?;
            let mut consumed = 0u64;
            cut = false;
            lines(reader, end - at, |line, whole| {
                if !whole && serde_json::from_slice::<serde::de::IgnoredAny>(line).is_err() {
                    cut = true;
                    return Ok(());
                }
                consumed += line.len() as u64 + u64::from(whole);
                if !uuid_of(line).is_some_and(|uuid| ids.contains(&uuid)) {
                    out.write_all(line)?;
                    out.write_all(b"\n")?;
                    bytes += line.len() as u64 + 1;
                }
                Ok(())
            })?;
            at += consumed;
            end = std::fs::metadata(to)?.len();
            if end <= at || (cut && consumed == 0) {
                break;
            }
        }
        unfinished |= cut;
        out.flush()?;
    }
    // One set, the meta first: a published transcript always has its meta.
    let mut set = Set::new();
    let mut guards = Vec::new();
    if let Some((meta_from, meta_to)) = meta {
        let (meta_staged, guard, guarded) = stage_meta(meta_from, meta_to)?;
        guards.push((guard, guarded));
        set.add(meta_staged, meta_to, Claim::New);
    }
    for (guard, guarded) in &guards {
        guard.check_metadata(guarded.metadata()?)?;
        guard.check()?;
    }
    set.add(staged, to, if existing { Claim::Replace } else { Claim::New });
    set.publish()?;
    Ok(Done {
        outcome,
        bytes,
        unfinished,
        meta: meta.is_some(),
    })
}

/// Carry the agents of each of `sources` (main transcripts) to the session at `destination`
/// (`<project>/<new_id>.jsonl`). `only` narrows it to those agent ids (empty: all). `None`
/// when the sources have no such agents. `dry_run` counts without writing.
pub fn carry_agents(
    sources: &[PathBuf],
    destination: &Path,
    new_id: &str,
    mode: CarryAgents,
    only: &[String],
    dry_run: bool,
) -> Result<Option<Carried>> {
    let target = sidecar::subagents_dir(destination)
        .with_context(|| format!("{} is not a session transcript path", destination.display()))?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut carried = Carried {
        dir: target.clone(),
        ..Carried::default()
    };
    for source in sources {
        let Some(dir) = sidecar::subagents_dir(source) else {
            continue;
        };
        anyhow::ensure!(dir != target, "session {new_id} cannot carry its agents into itself");
        let mut found = Vec::new();
        collect(&dir, Path::new(""), mode, only, &mut found)?;
        for entry in found {
            if seen.insert(entry.to.clone()) {
                entries.push(entry);
            } else {
                carried.duplicates += 1;
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    // Transcripts first, each with its meta as one set; then the metas that have none.
    let metas: std::collections::HashMap<&Path, &Path> = entries
        .iter()
        .filter(|e| !e.transcript)
        .map(|e| (e.to.as_path(), e.from.as_path()))
        .collect();
    let mut paired = HashSet::new();
    for entry in entries.iter().filter(|e| e.transcript) {
        let to = target.join(&entry.to);
        let meta_rel = entry.to.with_extension("meta.json");
        let meta_to = target.join(&meta_rel);
        let meta = metas.get(meta_rel.as_path()).map(|from| (*from, meta_to.as_path()));
        let done = match transcript(&entry.from, &to, new_id, meta, dry_run) {
            Ok(done) => done,
            // What this transcript holds will not change on a repeat: skip it and carry the rest.
            // Its meta is left for the pass below.
            Err(error) => match error.downcast::<Refused>() {
                Ok(Refused(why)) => {
                    carried.refused.push(Refusal {
                        agent: crate::derive::agent_of(&entry.from).unwrap_or_default().to_string(),
                        why,
                    });
                    continue;
                }
                Err(error) => return Err(error.context(format!("carrying {}", entry.from.display()))),
            },
        };
        if meta.is_some() {
            paired.insert(meta_rel);
            if done.meta {
                carried.metas += 1;
            } else {
                // The destination's own spawn record stands (a retargeted model, say).
                carried.already += 1;
            }
        }
        match done.outcome {
            Outcome::Copied => carried.transcripts += 1,
            Outcome::Joined => carried.joined += 1,
            Outcome::Already => carried.already += 1,
        }
        carried.unfinished += usize::from(done.unfinished);
        carried.transcript_bytes += done.bytes;
    }
    for entry in entries.iter().filter(|e| !e.transcript && !paired.contains(&e.to)) {
        let to = target.join(&entry.to);
        if std::fs::symlink_metadata(&to).is_ok() {
            carried.already += 1;
            continue;
        }
        carried.metas += 1;
        if !dry_run {
            let context = || format!("carrying {}", entry.from.display());
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let (staged, guard, guarded) = stage_meta(&entry.from, &to).with_context(context)?;
            guard.check_metadata(guarded.metadata()?)?;
            guard.check()?;
            staged.publish(&to, crate::publish::Claim::New).with_context(context)?;
        }
    }
    Ok(Some(carried))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(uuid: &str, parent: Option<&str>, session: &str, role: &str, text: &str) -> String {
        let line = serde_json::json!({"type": role, "uuid": uuid, "parentUuid": parent, "sessionId": session,
            "session_id": session, "agentId": "a1", "isSidechain": true,
            "message": {"role": role, "content": text}});
        format!("{line}\n")
    }
    fn counts(c: &Carried) -> (usize, usize, usize, usize, usize) {
        (c.metas, c.transcripts, c.joined, c.already, c.unfinished)
    }
    fn carry(old: &Path, new: &Path, id: &str, mode: CarryAgents, dry: bool) -> Result<Carried> {
        Ok(carry_agents(std::slice::from_ref(&old.to_path_buf()), new, id, mode, &[], dry)?.expect("agents"))
    }

    #[test]
    fn metas_go_verbatim_and_transcripts_restamped_to_the_same_place_under_the_new_id() -> Result<()> {
        let world = crate::test_world("carry-agents");
        let old = world.proj.join("old.jsonl");
        std::fs::write(&old, "{}\n")?;
        let sub = world.proj.join("old/subagents");
        std::fs::create_dir_all(sub.join("workflows/run-1"))?;
        let meta = r#"{"agentType":"Explore","model":"sonnet","description":"survey the beds","toolUseId":"toolu_1"}"#;
        std::fs::write(sub.join("agent-a1.meta.json"), meta)?;
        let transcript = record("b1", None, "old", "user", "survey the beds")
            + &record(
                "b2",
                Some("b1"),
                "old",
                "assistant",
                "old beds, sessionId stays in text",
            );
        std::fs::write(sub.join("agent-a1.jsonl"), &transcript)?;
        std::fs::write(
            sub.join("workflows/run-1/agent-w1.meta.json"),
            r#"{"agentType":"builder"}"#,
        )?;
        std::fs::write(sub.join("workflows/run-1/journal.jsonl"), "{}\n")?;
        // A spawn record whose transcript was never written still comes along; a prune sidecar
        // is no transcript.
        std::fs::write(sub.join("agent-c2.meta.json"), r#"{"agentType":"general-purpose"}"#)?;
        std::fs::write(sub.join("agent-a1.flat.jsonl"), "{}\n")?;
        let new = world.proj.join("new.jsonl");
        let sources = [old.clone(), old.clone()];

        let dry = carry(&old, &new, "new", CarryAgents::Full, true)?;
        assert_eq!(counts(&dry), (3, 1, 0, 0, 0));
        assert!(!world.proj.join("new").exists(), "a dry run writes nothing");

        let carried = carry_agents(&sources, &new, "new", CarryAgents::Full, &[], false)?.unwrap();
        let dst = world.proj.join("new/subagents");
        assert_eq!((counts(&carried), carried.duplicates), ((3, 1, 0, 0, 0), 4));
        assert_eq!(carried.dir, dst);
        assert_eq!(std::fs::read_to_string(dst.join("agent-a1.meta.json"))?, meta);
        assert_eq!(
            std::fs::read_to_string(dst.join("agent-c2.meta.json"))?,
            r#"{"agentType":"general-purpose"}"#
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("workflows/run-1/agent-w1.meta.json"))?,
            r#"{"agentType":"builder"}"#
        );
        assert!(!dst.join("workflows/run-1/journal.jsonl").exists());
        // The three agent files, the run folder, and no staging file left beside them.
        assert_eq!(std::fs::read_dir(&dst)?.count(), 4);
        let out = std::fs::read_to_string(dst.join("agent-a1.jsonl"))?;
        assert_eq!(
            (carried.transcript_bytes, dry.transcript_bytes),
            (out.len() as u64, out.len() as u64)
        );
        // Only the top-level session ids changed: swapping them back gives the source bytes.
        assert_eq!(out.matches(r#":"new""#).count(), 4);
        assert_eq!(crate::restamp::restamp(&out, "old").body, transcript);
        // The source is untouched, and carrying again changes nothing.
        assert_eq!(std::fs::read_to_string(sub.join("agent-a1.jsonl"))?, transcript);
        let again = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&again), (0, 0, 0, 4, 0));
        assert_eq!(std::fs::read_to_string(dst.join("agent-a1.jsonl"))?, out);

        let lean = world.proj.join("lean.jsonl");
        let only = ["agent-a1".to_string()];
        let carried = carry_agents(&sources[..1], &lean, "lean", CarryAgents::Meta, &only, false)?;
        assert_eq!(carried.map(|c| (c.metas, c.transcripts)), Some((1, 0)));
        assert!(!world.proj.join("lean/subagents/agent-a1.jsonl").exists());
        assert!(
            !world.proj.join("lean/subagents/workflows").exists(),
            "only a1 was asked for"
        );
        // A session is never its own successor.
        assert!(carry(&old, &old, "old", CarryAgents::Meta, false).is_err());
        Ok(())
    }

    /// An agent mid-turn when its parent moved: the new session's folder holds a file that
    /// begins with its next assistant record, and no meta. The old folder holds the brief, the
    /// earlier records and the meta. The carry puts the old records before what the new file
    /// holds and copies the meta. An agent that was carried at the move and then wrote to both
    /// folders is joined the same way, and a repeat is a no-op.
    #[test]
    fn an_agent_that_kept_writing_into_the_new_session_gets_its_earlier_records_put_first() -> Result<()> {
        let world = crate::test_world("carry-agents-mid-turn");
        let old = world.proj.join("old.jsonl");
        std::fs::write(&old, "{}\n")?;
        let from = world.proj.join("old/subagents");
        let into = world.proj.join("new/subagents");
        std::fs::create_dir_all(&from)?;
        std::fs::create_dir_all(&into)?;
        let meta = r#"{"agentType":"Explore","model":"sonnet","description":"count the trees"}"#;
        std::fs::write(from.join("agent-a1.meta.json"), meta)?;
        let brief = record("b1", None, "old", "user", "count the trees, then wait");
        let early = record("b2", Some("b1"), "old", "assistant", "counting the first row");
        // What the agent wrote after the move ends with a call still running: a break the
        // source never had, which the check of the head must not see.
        let running = serde_json::json!({"type": "assistant", "uuid": "b5", "parentUuid": "b4", "sessionId": "new",
            "agentId": "a1", "isSidechain": true, "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_running", "name": "Bash", "input": {"command": "count"}}]}});
        let tail = record("b3", Some("b2"), "new", "assistant", "forty trees")
            + &record("b4", Some("b3"), "new", "user", "and the hedges")
            + &format!("{running}\n");
        std::fs::write(from.join("agent-a1.jsonl"), format!("{brief}{early}"))?;
        std::fs::write(into.join("agent-a1.jsonl"), &tail)?;
        let new = world.proj.join("new.jsonl");
        let stamp = |text: &str| {
            text.replace(r#""sessionId":"old""#, r#""sessionId":"new""#)
                .replace(r#""session_id":"old""#, r#""session_id":"new""#)
        };

        let dry = carry(&old, &new, "new", CarryAgents::Full, true)?;
        assert_eq!(counts(&dry), (1, 0, 1, 0, 0));
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, tail, "dry");

        let carried = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (1, 0, 1, 0, 0));
        let joined = stamp(&format!("{brief}{early}")) + &tail;
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, joined);
        assert_eq!(
            (carried.transcript_bytes, dry.transcript_bytes),
            (joined.len() as u64, joined.len() as u64)
        );
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.meta.json"))?, meta);
        // Every record once, each parent before its child; nothing staged is left.
        let ids: Vec<String> = joined.lines().filter_map(|l| uuid_of(l.as_bytes())).collect();
        assert_eq!(ids, ["b1", "b2", "b3", "b4", "b5"]);
        assert_eq!(std::fs::read_dir(&into)?.count(), 2);

        // The agent wrote one more record to the OLD folder after an earlier carry: a repeat
        // puts it where the source has it, before the records only the new folder holds.
        let late = record("b2x", Some("b2"), "old", "assistant", "the second row");
        std::fs::write(from.join("agent-a1.jsonl"), format!("{brief}{early}{late}"))?;
        let again = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&again), (0, 0, 1, 1, 0));
        let rejoined = stamp(&format!("{brief}{early}{late}")) + &tail;
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, rejoined);
        let settled = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&settled), (0, 0, 0, 2, 0));
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, rejoined);
        // The old folder is never written.
        assert_eq!(
            std::fs::read_to_string(from.join("agent-a1.jsonl"))?,
            format!("{brief}{early}{late}")
        );
        Ok(())
    }

    /// A transcript whose writer is mid-append ends in a record that is not whole. The carry
    /// takes what is whole and says so; it neither writes the broken record nor refuses. The
    /// same holds for the destination's own last record in a join.
    #[test]
    fn an_unfinished_last_record_is_left_out_and_counted() -> Result<()> {
        let world = crate::test_world("carry-agents-unfinished");
        let old = world.proj.join("old.jsonl");
        std::fs::write(&old, "{}\n")?;
        let from = world.proj.join("old/subagents");
        std::fs::create_dir_all(&from)?;
        let whole =
            record("b1", None, "old", "user", "count the trees") + &record("b2", Some("b1"), "old", "assistant", "one");
        let half = r#"{"type":"assistant","uuid":"b3","parentUuid":"b2","sessionId":"old","agentId":"a1","isSidech"#;
        std::fs::write(from.join("agent-a1.jsonl"), format!("{whole}{half}"))?;
        let stamped = whole
            .replace(r#""sessionId":"old""#, r#""sessionId":"new""#)
            .replace(r#""session_id":"old""#, r#""session_id":"new""#);

        let new = world.proj.join("new.jsonl");
        let carried = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (0, 1, 0, 0, 1));
        let to = world.proj.join("new/subagents/agent-a1.jsonl");
        assert_eq!(std::fs::read_to_string(&to)?, stamped);

        // A join whose destination also ends mid-record: its whole records follow the head.
        let joined = world.proj.join("join.jsonl");
        let into = world.proj.join("join/subagents");
        std::fs::create_dir_all(&into)?;
        let tail = record("b4", Some("b2"), "join", "assistant", "two");
        std::fs::write(into.join("agent-a1.jsonl"), format!("{tail}{{\"type\":\"user\",\"uu"))?;
        std::fs::write(from.join("agent-a1.jsonl"), &whole)?;
        let carried = carry(&old, &joined, "join", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (0, 0, 1, 0, 1));
        assert_eq!(
            std::fs::read_to_string(into.join("agent-a1.jsonl"))?,
            stamped.replace(r#":"new""#, r#":"join""#) + &tail
        );
        Ok(())
    }

    /// One transcript the carry refuses must not cost the others: of three agents in name order
    /// the middle one holds a whole line that is not a record. The first and third are carried,
    /// the middle one's meta goes and its transcript does not, it is named, and a repeat changes
    /// nothing.
    #[test]
    fn a_refused_transcript_is_skipped_and_named_and_the_rest_are_carried() -> Result<()> {
        let world = crate::test_world("carry-agents-refused");
        let old = world.proj.join("old.jsonl");
        std::fs::write(&old, "{}\n")?;
        let from = world.proj.join("old/subagents");
        std::fs::create_dir_all(&from)?;
        let agent = |id: &str, extra: &str| -> Result<()> {
            let lines = record("b1", None, "old", "user", "count")
                .replace(r#""agentId":"a1""#, &format!(r#""agentId":"{id}""#));
            std::fs::write(from.join(format!("agent-{id}.jsonl")), format!("{lines}{extra}"))?;
            std::fs::write(
                from.join(format!("agent-{id}.meta.json")),
                format!(r#"{{"description":"{id}"}}"#),
            )?;
            Ok(())
        };
        agent("a1", "")?;
        agent("a2", "not a record\n")?;
        agent("a3", "")?;
        let new = world.proj.join("new.jsonl");
        let into = world.proj.join("new/subagents");
        for dry in [true, false] {
            let carried = carry(&old, &new, "new", CarryAgents::Full, dry)?;
            assert_eq!(counts(&carried), (3, 2, 0, 0, 0), "dry {dry}");
            assert_eq!(carried.refused.len(), 1);
            assert_eq!(carried.refused[0].agent, "a2");
            assert!(
                carried.refused[0].why.contains("is not one JSON value"),
                "{:?}",
                carried.refused
            );
        }
        for id in ["a1", "a3"] {
            assert!(into.join(format!("agent-{id}.jsonl")).is_file(), "{id}");
        }
        assert!(!into.join("agent-a2.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(into.join("agent-a2.meta.json"))?,
            r#"{"description":"a2"}"#
        );
        assert_eq!(std::fs::read_dir(&into)?.count(), 5, "no staging file left");
        // A repeat refuses the same one and changes nothing.
        let again = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!((counts(&again), again.refused.len()), ((0, 0, 0, 5, 0), 1));
        assert_eq!(std::fs::read_dir(&into)?.count(), 5);
        Ok(())
    }
}
