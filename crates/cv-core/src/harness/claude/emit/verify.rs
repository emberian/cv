//! Resumability facts from the record codec's [`record::Facts`] over strict checked JSONL views;
//! no retained transcript payloads.
use super::*;
use crate::harness::claude::record;
use crate::json::{ReadInput, Validation};
use crate::{harness::store::jsonl::ranges, json::JsonSource};
use std::collections::{HashMap, HashSet};
/// What the rules read of one record.
struct Facts {
    /// A `user` or `assistant` record: where the resume walk may start.
    turn: bool,
    uuid: Option<String>,
    parent: Option<String>,
    calls: Vec<String>,
    side: bool,
    agent: Option<String>,
}
/// One record that breaks a resumability rule, keyed by what it breaks so a source's own breaks
/// can be told apart from ones a reshape introduced.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    /// A `parentUuid` naming no record in the file.
    Parent(String),
    /// A parent walk revisits a UUID instead of terminating at a root.
    Cycle(String),
    /// A `tool_use` id with no `tool_result`.
    Call(String),
    /// The record (by `uuid`) carries the wrong identity.
    Identity(Option<String>),
}
struct Break {
    key: Key,
    message: String,
}
/// A file's breaks in file order (per record: parent, calls, identity), where each `uuid` first
/// and last occurs among its records, and the conversation tree Claude Code walks on resume.
struct Scan {
    breaks: Vec<Break>,
    trailing_open_calls: HashSet<String>,
    has_untagged_bookkeeping: bool,
    ids: HashMap<String, (usize, usize)>,
    first: Option<String>,
    /// `uuid` → `parentUuid` (the last record with that `uuid`).
    links: HashMap<String, Option<String>>,
    /// The newest `user`/`assistant` record: where the resume walk starts.
    leaf: Option<String>,
}

impl Scan {
    /// The parent the walk from the leaf stops at because no record holds it, if any.
    fn chain_end(&self) -> Option<&String> {
        let mut seen = HashSet::new();
        let mut at = self.leaf.as_ref()?;
        while seen.insert(at) {
            let parent = self.links.get(at)?.as_ref()?;
            if !self.links.contains_key(parent) {
                return Some(parent);
            }
            at = parent;
        }
        None
    }
}

/// Scan `source`'s captured extent. A line that is not one JSON record is an error, or with
/// `skip_invalid` (a reshape's source, which may hold one past a cut) skipped, as Claude Code's
/// loader skips it.
fn scan(source: &JsonSource, file: &fs::File, label: &Path, agent: Option<&str>, skip_invalid: bool) -> Result<Scan> {
    scan_slices(
        &[(
            source,
            file,
            ranges::Range {
                offset: 0,
                len: source.len(),
            },
        )],
        label,
        agent,
        skip_invalid,
    )
}

fn scan_slices(
    slices: &[(&JsonSource, &fs::File, ranges::Range)],
    label: &Path,
    agent: Option<&str>,
    skip_invalid: bool,
) -> Result<Scan> {
    let (mut ids, mut answered) = (HashMap::new(), HashSet::new());
    let mut facts = Vec::new();
    let mut last_user_idx: Option<usize> = None;
    for &(source, file, slice) in slices {
        let framed = ranges::records(file, slice)?;
        for row in framed {
            let (raw, _) = row?;
            if ranges::trim(file, raw)?.len == 0 {
                continue;
            }
            let n = facts.len();
            let fact = match source.read(ReadInput::Range(raw.offset, raw.len), Validation::Value, |view| {
                Ok(record::Facts::parse(view)?.0)
            }) {
                Ok(fact) => fact,
                Err(error) if skip_invalid && error.is::<crate::json::InvalidJson>() => continue,
                Err(error) => return Err(error.context(format!("{}: not a JSON record", label.display()))),
            };
            if let Some(id) = &fact.uuid {
                ids.entry(id.clone()).or_insert((n, n)).1 = n;
            }
            // A bookkeeping record's `message` is no turn (an `api-request-blob` holds a request body):
            // its blocks call and answer nothing.
            let mut calls = Vec::new();
            if !record::META_TYPES.contains(&fact.ty.as_str()) {
                for block in &fact.blocks {
                    match block.ty.as_deref() {
                        Some("tool_use") => calls.push(block.id.clone().unwrap_or_default()),
                        Some("tool_result") => answered.extend(block.tool_use_id.clone()),
                        _ => {}
                    }
                }
            }
            if fact.ty == "user" {
                last_user_idx = Some(n);
            }
            let fact = Facts {
                turn: matches!(fact.ty.as_str(), "user" | "assistant"),
                uuid: fact.uuid,
                parent: fact.parent.flatten(),
                calls,
                side: fact.sidechain,
                agent: fact.agent_id,
            };
            facts.push(fact);
        }
        source.check()?;
    }
    let first = facts.iter().find_map(|f| f.uuid.clone());
    let leaf = facts.iter().rev().filter(|f| f.turn).find_map(|f| f.uuid.clone());
    let mut links = HashMap::new();
    let mut breaks = Vec::new();
    let mut trailing_open_calls = HashSet::new();
    let mut has_untagged_bookkeeping = false;
    for (i, fact) in facts.into_iter().enumerate() {
        let n = i + 1;
        if !fact.turn && fact.uuid.is_none() && !fact.side && fact.agent.is_none() {
            has_untagged_bookkeeping = true;
        }
        if let Some(uuid) = &fact.uuid {
            links.insert(uuid.clone(), fact.parent.clone());
        }
        if let Some(parent) = fact.parent {
            if !ids.contains_key(&parent) {
                breaks.push(Break {
                    message: format!(
                        "{}:{n}: parentUuid {parent} names no record in the file",
                        label.display()
                    ),
                    key: Key::Parent(parent),
                });
            }
        }
        for id in fact.calls {
            if !answered.contains(&id) {
                if last_user_idx.is_none_or(|u| i > u) {
                    trailing_open_calls.insert(id.clone());
                }
                breaks.push(Break {
                    message: format!("{}:{n}: tool_use {id} has no tool_result", label.display()),
                    key: Key::Call(id),
                });
            }
        }
        let message = match agent {
            None if fact.side || fact.agent.is_some() => Some(format!(
                "{}:{n}: a main-thread session carries a sidechain marker",
                label.display()
            )),
            Some(a) if !fact.side || fact.agent.as_deref() != Some(a) => Some(format!(
                "{}:{n}: a sub-agent record must carry isSidechain:true and agentId {a:?}",
                label.display()
            )),
            _ => None,
        };
        if let Some(message) = message {
            breaks.push(Break {
                message,
                key: Key::Identity(fact.uuid),
            });
        }
    }
    // Parent existence alone admits self-links and multi-record cycles. Check every component,
    // including detached branches, once; a valid resume walk must terminate.
    let mut checked = HashSet::new();
    let mut starts: Vec<_> = links.keys().collect();
    starts.sort();
    for start in starts {
        let mut path = HashSet::new();
        let mut at = start;
        while !checked.contains(at) {
            if !path.insert(at.clone()) {
                // Canonical identity lets derived copies retain the same inherited cycle even
                // when their walk enters it through a different branch.
                let mut key = at;
                let mut cursor = links.get(at).and_then(Option::as_ref).expect("cycle parent");
                while cursor != at {
                    key = key.min(cursor);
                    cursor = links.get(cursor).and_then(Option::as_ref).expect("cycle parent");
                }
                breaks.push(Break {
                    key: Key::Cycle(key.clone()),
                    message: format!("{}: parentUuid cycle at {key}", label.display()),
                });
                break;
            }
            let Some(Some(parent)) = links.get(at) else {
                break;
            };
            at = parent;
        }
        checked.extend(path);
    }
    Ok(Scan {
        breaks,
        trailing_open_calls,
        has_untagged_bookkeeping,
        ids,
        first,
        links,
        leaf,
    })
}

pub(super) fn verify(path: &Path, agent: Option<&str>) -> Result<()> {
    verify_as(path, path, agent)
}

/// [`verify`] of the file at `path`, its errors naming `label` (a staged file's destination).
pub(super) fn verify_as(path: &Path, label: &Path, agent: Option<&str>) -> Result<()> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    match scan(&source, &file, label, agent, false)?.breaks.into_iter().next() {
        Some(first) => Err(anyhow::anyhow!(first.message)),
        None => Ok(()),
    }
}

/// [`verify`] for a reshape's staged `output` (identity `agent`) of `source` (identity
/// `source_agent`): it may keep a break its source already has (a continuation's first parent is
/// in the previous file; a live session's last call is still running). A parent counts only where
/// the resume walk from the leaf stops at it, and not when the source holds it before the copy's
/// first record (a rewind starts at a compaction).
pub(super) fn verify_derived(
    source: &JsonSource,
    file: &fs::File,
    source_agent: Option<&str>,
    output: &Path,
    destination: &Path,
    agent: Option<&str>,
) -> Result<()> {
    let staged = JsonSource::capture(output)?;
    let staged_file = fs::File::open(output)?;
    let output = scan(&staged, &staged_file, destination, agent, false)?;
    let end = output.chain_end();
    let mut candidates = output.breaks.iter().filter(|b| match &b.key {
        Key::Parent(parent) => end == Some(parent),
        _ => true,
    });
    // The source is read again only when the copy has a break the source might explain.
    let Some(first) = candidates.next() else {
        return Ok(());
    };
    let source = scan(source, file, destination, source_agent, true)?;
    let inherited: HashSet<&Key> = source.breaks.iter().map(|b| &b.key).collect();
    let start = output
        .first
        .as_ref()
        .and_then(|u| source.ids.get(u))
        .map(|(_, last)| *last);
    let before_start =
        |parent: &String| start.is_some_and(|start| source.ids.get(parent).is_some_and(|(first, _)| *first < start));
    let new = std::iter::once(first)
        .chain(candidates)
        .find(|b| !inherited.contains(&b.key) && !matches!(&b.key, Key::Parent(parent) if before_start(parent)));
    match new {
        Some(new) => Err(anyhow::anyhow!(
            "{}; the source has no such break, so nothing was written",
            new.message
        )),
        None => Ok(()),
    }
}

/// [`verify_derived`] across one or more chained source slices `sources` plus an optional joined
/// destination tail `joined_tail` (`--carry-agents` / `adopt --join`), excusing breaks inherited
/// from `sources` union `joined_tail`'s trailing open `Call`s.
pub(super) fn verify_carried(
    sources: &[(&JsonSource, &fs::File, ranges::Range)],
    joined_tail: Option<(&JsonSource, &fs::File, ranges::Range)>,
    source_agent: Option<&str>,
    output: &Path,
    destination: &Path,
    agent: Option<&str>,
) -> Result<()> {
    let staged = JsonSource::capture(output)?;
    let staged_file = fs::File::open(output)?;
    let output = scan(&staged, &staged_file, destination, agent, false)?;
    let end = output.chain_end();
    let mut candidates = output.breaks.iter().filter(|b| match &b.key {
        Key::Parent(parent) => end == Some(parent),
        _ => true,
    });
    let Some(first) = candidates.next() else {
        return Ok(());
    };
    let source = scan_slices(sources, destination, source_agent, true)?;
    let mut inherited: HashSet<Key> = source.breaks.into_iter().map(|b| b.key).collect();
    if let Some(tail) = joined_tail {
        let tail_scan = scan_slices(&[tail], destination, agent, true)?;
        for call in tail_scan.trailing_open_calls {
            inherited.insert(Key::Call(call));
        }
        if tail_scan.has_untagged_bookkeeping {
            inherited.insert(Key::Identity(None));
        }
    }
    let start = output
        .first
        .as_ref()
        .and_then(|u| source.ids.get(u))
        .map(|(_, last)| *last);
    let before_start =
        |parent: &String| start.is_some_and(|start| source.ids.get(parent).is_some_and(|(first, _)| *first < start));
    let new = std::iter::once(first)
        .chain(candidates)
        .find(|b| !inherited.contains(&b.key) && !matches!(&b.key, Key::Parent(parent) if before_start(parent)));
    match new {
        Some(new) => Err(anyhow::anyhow!(
            "{}; the source has no such break, so nothing was written",
            new.message
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;

    #[test]
    fn parent_walk_rejects_self_and_detached_cycles_but_accepts_branches() -> Result<()> {
        let world = cv_testkit::World::new("parent-cycles");
        let path = world.base.join("session.jsonl");
        for rows in [
            "{\"type\":\"user\",\"uuid\":\"a\",\"parentUuid\":\"a\"}\n",
            "{\"uuid\":\"a\",\"parentUuid\":\"b\"}\n{\"uuid\":\"b\",\"parentUuid\":\"a\"}\n{\"type\":\"user\",\"uuid\":\"leaf\"}\n",
        ] {
            fs::write(&path, rows)?;
            assert!(verify(&path, None).unwrap_err().to_string().contains("parentUuid cycle"));
        }
        fs::write(&path, "{\"uuid\":\"root\"}\n{\"uuid\":\"left\",\"parentUuid\":\"root\"}\n{\"type\":\"user\",\"uuid\":\"right\",\"parentUuid\":\"root\"}\n")?;
        verify(&path, None)
    }
}
