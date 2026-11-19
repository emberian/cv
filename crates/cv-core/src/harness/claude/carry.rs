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
//! Every file is read and written a line at a time and published by rename; memory is one
//! line plus, for a join, the two files' record ids.

use super::sidecar;
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
    /// Files the destination already held whole (a meta of its own, a transcript holding every
    /// source record): left as they are.
    pub already: usize,
    /// Files an earlier source had already supplied (a splice of several sessions holding one
    /// agent): the first source's copy stands.
    pub duplicates: usize,
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

/// One line of a transcript, without its newline.
fn lines(path: &Path, mut each: impl FnMut(&[u8]) -> Result<()>) -> Result<u64> {
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, std::fs::File::open(path)?);
    let mut line = Vec::new();
    let mut read = 0u64;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(read);
        }
        read += n as u64;
        each(line.strip_suffix(b"\n").unwrap_or(&line))?;
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

/// `line` with its session id restamped; a line that is not a record, or carries no session
/// id, as it is.
fn write_restamped(line: &[u8], new_id: &str, out: &mut impl Write) -> Result<()> {
    match std::str::from_utf8(line)
        .ok()
        .and_then(|text| crate::restamp_line(text, new_id))
    {
        Some(stamped) => out.write_all(stamped.as_bytes())?,
        None => out.write_all(line)?,
    }
    out.write_all(b"\n")?;
    Ok(())
}

enum Outcome {
    Copied,
    Joined,
    Already,
}

/// The source transcript restamped at `to`, joined with what `to` already holds.
fn transcript(from: &Path, to: &Path, new_id: &str, dry_run: bool) -> Result<Outcome> {
    use crate::publish::{Claim, Staged};
    let existing = std::fs::symlink_metadata(to).is_ok();
    let mut held = HashSet::new();
    let mut held_bytes = 0;
    if existing {
        held_bytes = lines(to, |line| {
            held.extend(uuid_of(line));
            Ok(())
        })?;
    }
    // The source's record ids, and whether the destination lacks any of them.
    let mut source = HashSet::new();
    let mut missing = !existing;
    lines(from, |line| {
        if let Some(uuid) = uuid_of(line) {
            missing |= !held.contains(&uuid);
            source.insert(uuid);
        }
        Ok(())
    })?;
    if !missing {
        return Ok(Outcome::Already);
    }
    let outcome = if existing { Outcome::Joined } else { Outcome::Copied };
    if dry_run {
        return Ok(outcome);
    }
    drop(held);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mode = match std::fs::File::open(if existing { to } else { from }) {
        Ok(file) => crate::publish::mode_of(&file),
        Err(_) => 0o600,
    };
    let mut staged = std::io::BufWriter::with_capacity(64 * 1024, Staged::beside(to, "carry", mode)?);
    lines(from, |line| write_restamped(line, new_id, &mut staged))?;
    if !existing {
        let staged = staged.into_inner().map_err(|e| e.into_error())?;
        staged.publish(to, Claim::New)?;
        return Ok(outcome);
    }
    // Then what only the destination holds, in its order. An agent may still be appending:
    // whatever arrived since the last pass is copied too, until a pass finds nothing new.
    let mut at = 0u64;
    let mut end = held_bytes;
    for _ in 0..8 {
        let mut file = std::fs::File::open(to)?;
        file.seek(std::io::SeekFrom::Start(at))?;
        let mut reader = std::io::BufReader::with_capacity(64 * 1024, file.take(end - at));
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let body = line.strip_suffix(b"\n").unwrap_or(&line);
            if !uuid_of(body).is_some_and(|uuid| source.contains(&uuid)) {
                staged.write_all(body)?;
                staged.write_all(b"\n")?;
            }
        }
        at = end;
        end = std::fs::metadata(to)?.len();
        if end <= at {
            break;
        }
    }
    let staged = staged.into_inner().map_err(|e| e.into_error())?;
    staged.publish(to, Claim::Replace)?;
    Ok(outcome)
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
        let Some(dir) = sidecar::subagents_dir(source).filter(|dir| *dir != target) else {
            continue;
        };
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
    for entry in &entries {
        let to = target.join(&entry.to);
        let context = || format!("carrying {}", entry.from.display());
        if entry.transcript {
            match transcript(&entry.from, &to, new_id, dry_run).with_context(context)? {
                Outcome::Copied => carried.transcripts += 1,
                Outcome::Joined => carried.joined += 1,
                Outcome::Already => carried.already += 1,
            }
        } else if std::fs::symlink_metadata(&to).is_ok() {
            // The destination's own spawn record stands (a retargeted model, say).
            carried.already += 1;
        } else {
            carried.metas += 1;
            if !dry_run {
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let file = std::fs::File::open(&entry.from).with_context(context)?;
                crate::publish::publish_reader(&to, "carry", file, crate::publish::Claim::New).with_context(context)?;
            }
        }
    }
    Ok(Some(carried))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(uuid: &str, parent: Option<&str>, session: &str, role: &str, text: &str) -> String {
        serde_json::json!({"type": role, "uuid": uuid, "parentUuid": parent, "sessionId": session,
            "session_id": session, "agentId": "a1", "isSidechain": true,
            "message": {"role": role, "content": text}})
        .to_string()
            + "\n"
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
            )
            + "not a record\n";
        std::fs::write(sub.join("agent-a1.jsonl"), &transcript)?;
        std::fs::write(
            sub.join("workflows/run-1/agent-w1.meta.json"),
            r#"{"agentType":"builder"}"#,
        )?;
        std::fs::write(sub.join("workflows/run-1/journal.jsonl"), "{}\n")?;
        let new = world.proj.join("new.jsonl");
        let sources = [old.clone(), old.clone()];

        let dry = carry_agents(&sources[..1], &new, "new", CarryAgents::Full, &[], true)?.unwrap();
        assert_eq!((dry.metas, dry.transcripts), (2, 1));
        assert!(!world.proj.join("new").exists(), "a dry run writes nothing");

        let carried = carry_agents(&sources, &new, "new", CarryAgents::Full, &[], false)?.unwrap();
        let dst = world.proj.join("new/subagents");
        assert_eq!(
            carried,
            Carried {
                dir: dst.clone(),
                metas: 2,
                transcripts: 1,
                joined: 0,
                already: 0,
                duplicates: 3
            }
        );
        assert_eq!(std::fs::read_to_string(dst.join("agent-a1.meta.json"))?, meta);
        assert_eq!(
            std::fs::read_to_string(dst.join("workflows/run-1/agent-w1.meta.json"))?,
            r#"{"agentType":"builder"}"#
        );
        assert!(!dst.join("workflows/run-1/journal.jsonl").exists());
        let restamped = transcript
            .replace(r#""sessionId":"old""#, r#""sessionId":"new""#)
            .replace(r#""session_id":"old""#, r#""session_id":"new""#);
        assert_eq!(std::fs::read_to_string(dst.join("agent-a1.jsonl"))?, restamped);
        // The source is untouched, and carrying again changes nothing.
        assert_eq!(std::fs::read_to_string(sub.join("agent-a1.jsonl"))?, transcript);
        let again = carry_agents(&sources[..1], &new, "new", CarryAgents::Full, &[], false)?.unwrap();
        assert_eq!(
            (again.metas, again.transcripts, again.joined, again.already),
            (0, 0, 0, 3)
        );
        assert_eq!(std::fs::read_to_string(dst.join("agent-a1.jsonl"))?, restamped);

        let lean = world.proj.join("lean.jsonl");
        let carried = carry_agents(
            &sources[..1],
            &lean,
            "lean",
            CarryAgents::Meta,
            &["agent-a1".into()],
            false,
        )?;
        assert_eq!(carried.map(|c| (c.metas, c.transcripts)), Some((1, 0)));
        assert!(!world.proj.join("lean/subagents/agent-a1.jsonl").exists());
        assert!(
            !world.proj.join("lean/subagents/workflows").exists(),
            "only a1 was asked for"
        );
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
        let tail = record("b3", Some("b2"), "new", "assistant", "forty trees")
            + &record("b4", Some("b3"), "new", "user", "and the hedges");
        std::fs::write(from.join("agent-a1.jsonl"), format!("{brief}{early}"))?;
        std::fs::write(into.join("agent-a1.jsonl"), &tail)?;
        let new = world.proj.join("new.jsonl");
        let stamp = |text: &str| {
            text.replace(r#""sessionId":"old""#, r#""sessionId":"new""#)
                .replace(r#""session_id":"old""#, r#""session_id":"new""#)
        };

        let dry = carry_agents(std::slice::from_ref(&old), &new, "new", CarryAgents::Full, &[], true)?.unwrap();
        assert_eq!((dry.metas, dry.joined), (1, 1));
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, tail, "dry");

        let carried = carry_agents(std::slice::from_ref(&old), &new, "new", CarryAgents::Full, &[], false)?.unwrap();
        assert_eq!((carried.metas, carried.transcripts, carried.joined), (1, 0, 1));
        let joined = stamp(&format!("{brief}{early}")) + &tail;
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, joined);
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.meta.json"))?, meta);
        // Every record once, each parent before its child.
        let ids: Vec<String> = joined.lines().filter_map(|l| uuid_of(l.as_bytes())).collect();
        assert_eq!(ids, ["b1", "b2", "b3", "b4"]);

        // The agent wrote one more record to the OLD folder after an earlier carry: a repeat
        // puts it where the source has it, before the records only the new folder holds.
        let late = record("b2x", Some("b2"), "old", "assistant", "the second row");
        std::fs::write(from.join("agent-a1.jsonl"), format!("{brief}{early}{late}"))?;
        let again = carry_agents(std::slice::from_ref(&old), &new, "new", CarryAgents::Full, &[], false)?.unwrap();
        assert_eq!((again.metas, again.joined, again.already), (0, 1, 1));
        let rejoined = stamp(&format!("{brief}{early}{late}")) + &tail;
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, rejoined);
        let settled = carry_agents(std::slice::from_ref(&old), &new, "new", CarryAgents::Full, &[], false)?.unwrap();
        assert_eq!((settled.joined, settled.already), (0, 2));
        assert_eq!(std::fs::read_to_string(into.join("agent-a1.jsonl"))?, rejoined);
        // The old folder is never written.
        assert_eq!(
            std::fs::read_to_string(from.join("agent-a1.jsonl"))?,
            format!("{brief}{early}{late}")
        );
        Ok(())
    }
}
