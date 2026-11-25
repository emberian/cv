//! `--carry-agents`: take a session's sub-agents along to a session derived from it.
//!
//! Claude Code re-enters a background agent from the CURRENT session's `<session>/subagents/`:
//! `agent-<id>.meta.json` gives it its model and type, `agent-<id>.jsonl` its brief and what it
//! has done. A derived session (prune, rewind, splice) has a new id and so no such folder.
//! Carrying copies each meta (validated as JSON) and (in [`CarryAgents::Full`]) each transcript
//! with the session id restamped ([`crate::derive`], as `cv adopt` does), to the same place under
//! the new id; a workflow's agents keep their run folder. A `/resume`-split tail in the source is
//! materialized root-to-tip via [`super::chain::runs`] alongside its spawn metadata
//! ([`super::chain::meta_spec`]).
//!
//! When the destination already holds part of an agent transcript (an agent mid-turn when its
//! parent moved to the new session by an in-place `/resume`), the transcript is joined rather than
//! skipped or replaced: the source's records go first, followed by records only the destination
//! holds (deduplicated by `uuid`, and for UUID-less records by restamped trimmed SHA-256 digest),
//! all restamped to `new_id`, terminated with `\n`, and checked by [`super::verify_carried`] before
//! atomic publication. Symlinks, FIFOs, and path-traversal components on either side are refused.

use super::chain::{self, MetaSpec};
use super::sidecar;
use crate::digest::Sha256;
use crate::harness::store::document::Fields;
use crate::harness::store::jsonl::ranges::{self, Range};
use crate::json::{InvalidJson, JsonSource, ReadInput, Validation};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

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
            _ => bail!("unknown --carry-agents {s:?} (expected meta or full)"),
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

impl Carried {
    /// Whether the sources held nothing to carry or join.
    pub fn is_empty(&self) -> bool {
        self.metas == 0 && self.transcripts == 0 && self.joined == 0 && self.already == 0 && self.refused.is_empty()
    }
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

fn content_refusal(error: anyhow::Error) -> anyhow::Error {
    Refused(format!("{error:#}")).into()
}

fn derive_or_refuse(error: anyhow::Error) -> anyhow::Error {
    if error.is::<InvalidJson>() {
        content_refusal(error)
    } else {
        error
    }
}

/// Verify `path` is an existing regular file (`!is_symlink()`, not a FIFO/socket/directory).
fn ensure_regular_source(path: &Path) -> Result<()> {
    let md = fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    anyhow::ensure!(
        md.file_type().is_file(),
        "{} is not a regular file (refusing symlink or special file)",
        path.display()
    );
    Ok(())
}

/// Verify destination `path` is either absent (`Ok(false)`) or a regular file (`Ok(true)`),
/// refusing symlinks, directories, FIFOs, and other special files.
pub(super) fn check_regular_dest_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(md) => {
            anyhow::ensure!(
                md.file_type().is_file(),
                "{} is not a regular file (refusing symlink or special file)",
                path.display()
            );
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// Walk from `project_dir` through `<new_id>/subagents/<rel_parent>`, verifying every existing
/// directory component with `symlink_metadata` (`!is_symlink()`, `is_dir()`) and creating missing
/// components only when `create` is true.
fn ensure_contained_dest_dir(session_dir: &Path, rel_parent: &Path, create: bool) -> Result<PathBuf> {
    let mut cur = session_dir
        .parent()
        .context("session dir has no project parent")?
        .to_path_buf();
    let session_name = session_dir
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid session dir name")?;
    crate::publish::validate_output_id(session_name)?;

    let mut parts: Vec<&str> = vec![session_name, "subagents"];
    for comp in rel_parent.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(os) => {
                let s = os.to_str().context("non-UTF-8 subagent relative path")?;
                crate::publish::validate_output_id(s)?;
                parts.push(s);
            }
            _ => bail!("invalid relative subagent path {}", rel_parent.display()),
        }
    }

    for part in parts {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(md) => {
                anyhow::ensure!(
                    md.file_type().is_dir(),
                    "{} is not a directory (refusing symlink or non-directory)",
                    cur.display()
                );
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if create {
                    match fs::create_dir(&cur) {
                        Ok(()) => {}
                        Err(race) if race.kind() == io::ErrorKind::AlreadyExists => {
                            let md =
                                fs::symlink_metadata(&cur).with_context(|| format!("checking {}", cur.display()))?;
                            anyhow::ensure!(
                                md.file_type().is_dir(),
                                "{} is not a directory (refusing symlink or non-directory)",
                                cur.display()
                            );
                        }
                        Err(err) => return Err(err).with_context(|| format!("creating {}", cur.display())),
                    }
                } else {
                    break;
                }
            }
            Err(e) => return Err(e).with_context(|| format!("checking {}", cur.display())),
        }
    }
    Ok(session_dir.join("subagents").join(rel_parent))
}

struct Entry {
    from: PathBuf,
    /// Relative to `subagents/`.
    to: PathBuf,
    transcript: bool,
}

fn valid_agent_id(id: &str) -> bool {
    !id.is_empty() && crate::publish::validate_output_id(id).is_ok()
}

/// Collect every agent id present under `dir` (and `workflows/<run>/`), for prefix resolution.
fn collect_agent_ids(dir: &Path, depth: usize, out: &mut BTreeSet<String>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else { continue };
        if ft.is_dir() {
            if depth < 2 && crate::publish::validate_output_id(name).is_ok() {
                collect_agent_ids(&entry.path(), depth + 1, out)?;
            }
            continue;
        }
        if !ft.is_file() || name.ends_with(".flat.jsonl") {
            continue;
        }
        let id = name
            .strip_suffix(".meta.json")
            .or_else(|| name.strip_suffix(".jsonl"))
            .and_then(|stem| stem.strip_prefix("agent-"));
        if let Some(id) = id.filter(|id| valid_agent_id(id)) {
            out.insert(id.to_string());
        }
    }
    Ok(())
}

/// Resolve explicit agent specs (`a5d8…` or `agent-a5d8…`, unique prefix ok; exact match beats
/// prefix) against the sub-agents of `sources`. Returns exact bare agent ids, or an error naming
/// an unmatched, ambiguous, or duplicate spec.
pub fn resolve_agent_specs(sources: &[PathBuf], specs: &[String]) -> Result<Vec<String>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let mut all_ids = BTreeSet::new();
    for source in sources {
        if let Some(dir) = sidecar::subagents_dir(source) {
            if let Ok(md) = fs::symlink_metadata(&dir) {
                anyhow::ensure!(
                    md.file_type().is_dir(),
                    "{} is not a directory (refusing symlink)",
                    dir.display()
                );
                collect_agent_ids(&dir, 0, &mut all_ids)?;
            }
        }
    }
    let mut resolved = Vec::with_capacity(specs.len());
    let mut seen = HashSet::new();
    for spec in specs {
        let want = spec.strip_prefix("agent-").unwrap_or(spec);
        if want.is_empty() {
            bail!("empty sub-agent selector {spec:?}");
        }
        let chosen = if all_ids.contains(want) {
            want.to_string()
        } else {
            let matches: Vec<&str> = all_ids
                .iter()
                .map(String::as_str)
                .filter(|id| id.starts_with(want))
                .collect();
            match matches.as_slice() {
                [] => {
                    let who = sources
                        .first()
                        .and_then(|p| p.file_stem())
                        .and_then(|s| s.to_str())
                        .unwrap_or("source");
                    bail!("no sub-agent {spec:?} in {who}/subagents/");
                }
                [one] => (*one).to_string(),
                many => bail!(
                    "agent prefix {spec:?} matches {} agents: {} — pass a longer id",
                    many.len(),
                    many.join(", ")
                ),
            }
        };
        if !seen.insert(chosen.clone()) {
            bail!("agent {chosen} was selected more than once — nothing was written");
        }
        resolved.push(chosen);
    }
    Ok(resolved)
}

/// Every agent file under `dir`, in name order, sub-folders (workflow runs) included. `only`
/// (agent ids, with or without `agent-`) narrows it; empty takes all. Symlinks and special files
/// on the source side are refused.
fn collect(dir: &Path, relative: &Path, mode: CarryAgents, only: &[String], out: &mut Vec<Entry>) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(md) => anyhow::ensure!(
            md.file_type().is_dir(),
            "{} is not a directory (refusing symlink or special file)",
            dir.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    }
    let mut entries = match fs::read_dir(dir) {
        Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    entries.sort_by_key(|e| e.file_name());

    let mut local_metas = HashSet::new();
    let mut split_jsonls = Vec::new();

    for entry in entries {
        let name = entry.file_name();
        let path = entry.path();
        let ft = entry
            .file_type()
            .with_context(|| format!("inspecting {}", path.display()))?;
        anyhow::ensure!(
            !ft.is_symlink(),
            "refusing symlink in subagents directory: {}",
            path.display()
        );
        let Some(name_str) = name.to_str() else { continue };
        let to = relative.join(&name);
        if ft.is_dir() {
            crate::publish::validate_output_id(name_str)
                .with_context(|| format!("invalid subagents directory {}", path.display()))?;
            collect(&path, &to, mode, only, out)?;
            continue;
        }
        if name_str.ends_with(".flat.jsonl") {
            continue;
        }
        let parsed = match (name_str.strip_suffix(".meta.json"), name_str.strip_suffix(".jsonl")) {
            (Some(stem), _) => stem.strip_prefix("agent-").map(|id| (id, false)),
            (None, Some(stem)) => stem.strip_prefix("agent-").map(|id| (id, true)),
            _ => None,
        };
        let Some((id, is_transcript)) = parsed else {
            continue;
        };
        anyhow::ensure!(valid_agent_id(id), "invalid sub-agent filename {}", path.display());
        anyhow::ensure!(
            ft.is_file(),
            "{} is not a regular file (refusing FIFO or special file)",
            path.display()
        );
        if !only.is_empty() && !only.iter().any(|o| o.strip_prefix("agent-").unwrap_or(o) == id) {
            continue;
        }
        if !is_transcript {
            local_metas.insert(id.to_string());
            out.push(Entry {
                from: path,
                to,
                transcript: false,
            });
        } else if mode == CarryAgents::Full {
            out.push(Entry {
                from: path,
                to,
                transcript: true,
            });
        } else {
            // In `CarryAgents::Meta`, remember `.jsonl` files in case they are `/resume`-split
            // tails whose `.meta.json` stayed in an earlier session.
            split_jsonls.push((id.to_string(), path, to));
        }
    }

    if mode == CarryAgents::Meta {
        for (id, jsonl_path, rel_jsonl) in split_jsonls {
            if local_metas.contains(&id) {
                continue;
            }
            let resolved = chain::meta(&jsonl_path);
            if resolved != jsonl_path.with_extension("meta.json")
                && fs::symlink_metadata(&resolved).is_ok_and(|m| m.file_type().is_file())
            {
                out.push(Entry {
                    from: resolved,
                    to: rel_jsonl.with_extension("meta.json"),
                    transcript: false,
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RecordKey {
    Uuid(String),
    Digest(String),
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Compute the deduplication key of a validated non-blank `Select`: `RecordKey::Uuid` when it
/// carries a string `"uuid"`, otherwise the SHA-256 of its restamped trimmed bytes so UUID-less
/// records (`summary`, `mode`, `queue-operation`, …) deduplicate idempotently across repeats even
/// when top-level `sessionId` / `session_id` was restamped.
fn record_key(
    source: &JsonSource,
    file: &File,
    select: &crate::derive::Select,
    new_id: &str,
) -> Result<Option<RecordKey>> {
    if select.blank {
        return Ok(None);
    }
    let Some(rec) = &select.record else {
        return Ok(None);
    };
    if let Some(uuid) = rec.with_view(|view| {
        Ok(Fields::parse(view, &["uuid"])?
            .map(|f| f.string("uuid"))
            .transpose()?
            .flatten())
    })? {
        return Ok(Some(RecordKey::Uuid(uuid)));
    }
    let trimmed = ranges::trim(file, select.range)?;
    if trimmed.len == 0 {
        return Ok(None);
    }
    let trim_end = trimmed.offset + trimmed.len;
    let mut ids: Vec<(u64, u64)> = select
        .ids
        .iter()
        .copied()
        .filter(|&(s, e)| s >= trimmed.offset && e <= trim_end)
        .collect();
    let mut hasher = Sha256::new();
    crate::restamp::write_restamped_spans(
        source,
        file,
        trimmed.offset,
        trimmed.len,
        new_id,
        &mut ids,
        &mut HashWriter(&mut hasher),
    )?;
    Ok(Some(RecordKey::Digest(hasher.finish_hex())))
}

struct ScannedSlice {
    source: JsonSource,
    file: File,
    carried_range: Range,
    rows: Vec<(crate::derive::Select, Option<RecordKey>)>,
    unfinished: bool,
}

fn scan_slice(
    path: &Path,
    start: u64,
    end_bound: Option<u64>,
    new_id: &str,
    identity: &crate::harness::Identity,
) -> Result<ScannedSlice> {
    ensure_regular_source(path)?;
    let source = JsonSource::capture(path).with_context(|| format!("reading {}", path.display()))?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    source.check_metadata(file.metadata()?)?;
    let file_len = source.len();
    let end = end_bound.map_or(file_len, |b| b.min(file_len));
    anyhow::ensure!(start <= end, "slice start {start} past end {end}");
    let raw_rows = ranges::records(
        &file,
        Range {
            offset: start,
            len: end - start,
        },
    )?
    .collect::<Result<Vec<(Range, bool)>>>()?;

    let mut rows = Vec::with_capacity(raw_rows.len());
    let mut unfinished = false;
    let mut carried_end = start;
    let total = raw_rows.len();

    for (idx, (range, complete)) in raw_rows.into_iter().enumerate() {
        let is_eof_partial = idx + 1 == total && !complete && range.offset + range.len == file_len;
        // Always emit trailing `\n` (`newline = true`) and validate at `Validation::Value` (`syntax = false`).
        match crate::derive::Select::read(&source, &file, range, true, false, identity) {
            Ok(select) => {
                carried_end = range.offset + range.len + u64::from(complete);
                let key = record_key(&source, &file, &select, new_id)?;
                rows.push((select, key));
            }
            Err(error) if is_eof_partial && error.is::<InvalidJson>() => {
                unfinished = true;
                break;
            }
            Err(error) => return Err(derive_or_refuse(error)),
        }
    }

    Ok(ScannedSlice {
        source,
        file,
        carried_range: Range {
            offset: start,
            len: carried_end.saturating_sub(start),
        },
        rows,
        unfinished,
    })
}

enum StagedMeta {
    Copied {
        staged: crate::publish::Staged,
        source: JsonSource,
        file: File,
    },
    Merged {
        staged: crate::publish::Staged,
    },
}

impl StagedMeta {
    fn into_staged(self) -> Result<crate::publish::Staged> {
        match self {
            StagedMeta::Copied { staged, source, file } => {
                source.check_metadata(file.metadata()?)?;
                source.check()?;
                Ok(staged)
            }
            StagedMeta::Merged { staged } => Ok(staged),
        }
    }
}

/// Validate a `.meta.json` with `JsonSource` (`Validation::Value`), refusing symlinks and FIFOs.
fn validate_meta_file(from: &Path) -> Result<(JsonSource, File)> {
    ensure_regular_source(from)?;
    let source = JsonSource::capture(from).with_context(|| format!("reading {}", from.display()))?;
    let file = File::open(from).with_context(|| format!("opening {}", from.display()))?;
    source.check_metadata(file.metadata()?)?;
    source
        .read(ReadInput::Document, Validation::Value, |_| Ok(()))
        .with_context(|| format!("validating {}", from.display()))?;
    Ok((source, file))
}

fn stage_meta_spec(spec: &MetaSpec, to: &Path) -> Result<StagedMeta> {
    match spec {
        MetaSpec::Verbatim(from) => {
            let (source, mut file) = validate_meta_file(from)?;
            let mut staged = crate::publish::Staged::beside(to, "carry", crate::publish::mode_of(&file))?;
            io::copy(&mut file, &mut staged)?;
            Ok(StagedMeta::Copied { staged, source, file })
        }
        MetaSpec::Merged { map, mode_from } => {
            ensure_regular_source(mode_from)?;
            let mode_file = File::open(mode_from)?;
            let mut staged = crate::publish::Staged::beside(to, "carry", crate::publish::mode_of(&mode_file))?;
            serde_json::to_writer(&mut staged, map)?;
            Ok(StagedMeta::Merged { staged })
        }
    }
}

fn validate_meta_spec(spec: &MetaSpec) -> Result<()> {
    match spec {
        MetaSpec::Verbatim(from) => {
            let (source, file) = validate_meta_file(from)?;
            source.check_metadata(file.metadata()?)?;
            source.check()
        }
        MetaSpec::Merged { mode_from, .. } => ensure_regular_source(mode_from),
    }
}

enum Outcome {
    Copied,
    Joined,
    Already,
}

struct Done {
    outcome: Outcome,
    bytes: u64,
    unfinished: bool,
    meta: bool,
}

/// Materialize `from` (including any `/resume`-split predecessor slices via [`chain::runs`]),
/// restamp to `new_id`, join with any existing records in `to`, verify resumability, and publish
/// atomically alongside `meta`.
fn transcript(
    from: &Path,
    to: &Path,
    session_dir: &Path,
    rel_parent: &Path,
    new_id: &str,
    meta: Option<(&MetaSpec, &Path)>,
    dry_run: bool,
) -> Result<Done> {
    use crate::publish::{Claim, Set, Staged};
    let identity = super::IDENTITY;

    ensure_contained_dest_dir(session_dir, rel_parent, false)?;
    let existing = check_regular_dest_file(to)?;
    if let Some((_, meta_to)) = meta {
        let _ = check_regular_dest_file(meta_to)?;
    }

    let chain_slices = chain::runs(from);
    let mut from_scanned: Vec<ScannedSlice> = Vec::with_capacity(chain_slices.len());
    let mut head_keys: Vec<RecordKey> = Vec::new();
    let mut seen_head: HashSet<RecordKey> = HashSet::new();
    let mut unfinished = false;

    for slice in &chain_slices {
        let end_bound = (slice.end != u64::MAX).then_some(slice.end);
        let mut scanned = scan_slice(&slice.path, slice.start, end_bound, new_id, &identity)?;
        unfinished |= scanned.unfinished;
        scanned.rows.retain(|(sel, key)| match key {
            Some(k) => {
                if seen_head.insert(k.clone()) {
                    head_keys.push(k.clone());
                    true
                } else {
                    false
                }
            }
            None => chain_slices.len() == 1 && !existing && sel.blank,
        });
        from_scanned.push(scanned);
    }

    let mut initial_to = if existing {
        Some(scan_slice(to, 0, None, new_id, &identity)?)
    } else {
        None
    };

    let missing = match &initial_to {
        None => true,
        Some(to_scan) => {
            let held: HashSet<&RecordKey> = to_scan.rows.iter().filter_map(|(_, k)| k.as_ref()).collect();
            head_keys.iter().any(|k| !held.contains(k))
        }
    };

    let meta_needed = match meta {
        Some((spec, meta_to)) if !check_regular_dest_file(meta_to)? => {
            validate_meta_spec(spec)?;
            Some((spec, meta_to))
        }
        _ => None,
    };

    let mut artifacts = super::artifacts::Artifacts::default();
    for slice in &chain_slices {
        artifacts.collect(&slice.path)?;
    }
    if !missing {
        if !dry_run {
            let mut set = Set::new();
            artifacts.stage(to, &mut set, Claim::New)?;
            set.publish()?;
        }
        if let (Some((spec, meta_to)), false) = (meta_needed, dry_run) {
            ensure_contained_dest_dir(session_dir, rel_parent, true)?;
            let staged_meta = stage_meta_spec(spec, meta_to)?.into_staged()?;
            staged_meta.publish(meta_to, Claim::New)?;
        }
        return Ok(Done {
            outcome: Outcome::Already,
            bytes: 0,
            unfinished: false,
            meta: meta_needed.is_some(),
        });
    }

    let rewrite = crate::derive::Rewrite {
        session_id: new_id,
        standalone: false,
        identity,
    };
    let outcome = if existing { Outcome::Joined } else { Outcome::Copied };

    if dry_run {
        let mut bytes = 0u64;
        for seg in from_scanned {
            let c = crate::derive::derive(
                &seg.source,
                &seg.file,
                seg.rows.into_iter().map(|(s, _)| Ok(s)),
                &rewrite,
                &mut io::sink(),
            )
            .map_err(derive_or_refuse)?;
            bytes += c.bytes as u64;
        }
        if let Some(mut to_scan) = initial_to.take() {
            unfinished |= to_scan.unfinished;
            let mut emitted = seen_head;
            to_scan
                .rows
                .retain(|(_, key)| key.as_ref().is_some_and(|k| emitted.insert(k.clone())));
            let c = crate::derive::derive(
                &to_scan.source,
                &to_scan.file,
                to_scan.rows.into_iter().map(|(s, _)| Ok(s)),
                &rewrite,
                &mut io::sink(),
            )
            .map_err(derive_or_refuse)?;
            bytes += c.bytes as u64;
        }
        return Ok(Done {
            outcome,
            bytes,
            unfinished,
            meta: meta_needed.is_some(),
        });
    }

    ensure_contained_dest_dir(session_dir, rel_parent, true)?;
    let mode = crate::publish::mode_of(&from_scanned.last().expect("at least one source slice").file);
    let mut writer = io::BufWriter::with_capacity(64 * 1024, Staged::beside(to, "carry", mode)?);
    let mut bytes = 0u64;

    for seg in &mut from_scanned {
        let rows = std::mem::take(&mut seg.rows);
        let c = crate::derive::derive(
            &seg.source,
            &seg.file,
            rows.into_iter().map(|(s, _)| Ok(s)),
            &rewrite,
            &mut writer,
        )
        .map_err(derive_or_refuse)?;
        bytes += c.bytes as u64;
    }

    let mut emitted = seen_head;
    let mut latest_to: Option<(JsonSource, File, Range)> = None;
    if existing {
        let mut at = 0u64;
        for pass in 0..8 {
            let mut to_scan = if pass == 0 {
                initial_to.take().expect("existing to was scanned")
            } else {
                anyhow::ensure!(
                    check_regular_dest_file(to)?,
                    "{} disappeared or became non-regular during join",
                    to.display()
                );
                scan_slice(to, at, None, new_id, &identity)?
            };
            let cut = to_scan.unfinished;
            let consumed = to_scan.carried_range.len;
            let carried_end = to_scan.carried_range.offset + consumed;
            to_scan
                .rows
                .retain(|(_, key)| key.as_ref().is_some_and(|k| emitted.insert(k.clone())));
            if !to_scan.rows.is_empty() {
                let c = crate::derive::derive(
                    &to_scan.source,
                    &to_scan.file,
                    to_scan.rows.into_iter().map(|(s, _)| Ok(s)),
                    &rewrite,
                    &mut writer,
                )
                .map_err(derive_or_refuse)?;
                bytes += c.bytes as u64;
            }
            at = carried_end;
            latest_to = Some((
                to_scan.source,
                to_scan.file,
                Range {
                    offset: 0,
                    len: carried_end,
                },
            ));
            let now_len = fs::symlink_metadata(to)
                .with_context(|| format!("checking {}", to.display()))
                .and_then(|md| {
                    anyhow::ensure!(md.file_type().is_file(), "{} is not a regular file", to.display());
                    Ok(md.len())
                })?;
            if now_len <= at || (cut && consumed == 0) {
                unfinished |= cut && now_len > at;
                break;
            }
        }
    }

    let staged = writer.into_inner().map_err(|e| e.into_error())?;
    for seg in &from_scanned {
        seg.source.check_metadata(seg.file.metadata()?)?;
        seg.source.check()?;
    }

    let agent = crate::derive::agent_of(from);
    let source_refs: Vec<(&JsonSource, &File, Range)> = from_scanned
        .iter()
        .map(|seg| (&seg.source, &seg.file, seg.carried_range))
        .collect();
    let tail_ref = latest_to.as_ref().map(|(s, f, r)| (s, f, *r));
    super::verify_carried(&source_refs, tail_ref, agent, staged.path(), to, agent).map_err(content_refusal)?;

    let mut set = Set::new();
    if let Some((spec, meta_to)) = meta_needed {
        let staged_meta = stage_meta_spec(spec, meta_to)?.into_staged()?;
        set.add(staged_meta, meta_to, Claim::New);
    }
    artifacts.stage(to, &mut set, Claim::New)?;
    set.add(staged, to, if existing { Claim::Replace } else { Claim::New });
    set.publish()?;

    Ok(Done {
        outcome,
        bytes,
        unfinished,
        meta: meta_needed.is_some(),
    })
}

/// Carry the agents of each of `sources` (main transcripts) to the session at `destination`
/// (`<project>/<new_id>.jsonl`). `only` narrows it to those agent ids (empty: all). Always
/// returns a uniform [`Carried`] report (`Carried::is_empty()` when the sources hold no agents).
/// `dry_run` counts and validates without writing.
pub fn carry_agents(
    sources: &[PathBuf],
    destination: &Path,
    new_id: &str,
    mode: CarryAgents,
    only: &[String],
    dry_run: bool,
) -> Result<Carried> {
    crate::publish::validate_output_id(new_id)?;
    let session_dir = sidecar::session_dir(destination)
        .with_context(|| format!("{} is not a session transcript path", destination.display()))?;
    let target = session_dir.join("subagents");
    ensure_contained_dest_dir(&session_dir, Path::new(""), false)?;

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
        return Ok(carried);
    }

    let local_metas: HashMap<&Path, &Path> = entries
        .iter()
        .filter(|e| !e.transcript)
        .map(|e| (e.to.as_path(), e.from.as_path()))
        .collect();
    let mut paired = HashSet::new();

    for entry in entries.iter().filter(|e| e.transcript) {
        let to = target.join(&entry.to);
        let rel_parent = entry.to.parent().unwrap_or(Path::new(""));
        let meta_rel = entry.to.with_extension("meta.json");
        let meta_to = target.join(&meta_rel);
        let has_local_meta = local_metas.contains_key(meta_rel.as_path());
        let resolved_spec = chain::meta_spec(&entry.from)?;
        let meta = resolved_spec.as_ref().map(|spec| (spec, meta_to.as_path()));

        let done = match transcript(&entry.from, &to, &session_dir, rel_parent, new_id, meta, dry_run) {
            Ok(done) => done,
            Err(error) => match error.downcast::<Refused>() {
                Ok(Refused(why)) => {
                    carried.refused.push(Refusal {
                        agent: crate::derive::agent_of(&entry.from).unwrap_or_default().to_string(),
                        why,
                    });
                    // If the transcript was a split tail with no local meta entry, carry its root
                    // meta so the agent still keeps its model and type.
                    if !has_local_meta {
                        if let Some(spec) = &resolved_spec {
                            ensure_contained_dest_dir(&session_dir, rel_parent, false)?;
                            if check_regular_dest_file(&meta_to)? {
                                carried.already += 1;
                            } else {
                                validate_meta_spec(spec)?;
                                if !dry_run {
                                    ensure_contained_dest_dir(&session_dir, rel_parent, true)?;
                                    let staged = stage_meta_spec(spec, &meta_to)?.into_staged()?;
                                    staged.publish(&meta_to, crate::publish::Claim::New)?;
                                }
                                carried.metas += 1;
                            }
                        }
                    }
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
        let rel_parent = entry.to.parent().unwrap_or(Path::new(""));
        ensure_contained_dest_dir(&session_dir, rel_parent, false)?;
        if check_regular_dest_file(&to)? {
            carried.already += 1;
            continue;
        }
        let spec = MetaSpec::Verbatim(entry.from.clone());
        let context = || format!("carrying {}", entry.from.display());
        validate_meta_spec(&spec).with_context(context)?;
        if !dry_run {
            ensure_contained_dest_dir(&session_dir, rel_parent, true)?;
            let staged = stage_meta_spec(&spec, &to).with_context(context)?.into_staged()?;
            staged.publish(&to, crate::publish::Claim::New).with_context(context)?;
        }
        carried.metas += 1;
    }

    Ok(carried)
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
    fn uuid_of(line: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()?
            .get("uuid")?
            .as_str()
            .map(str::to_owned)
    }
    fn counts(c: &Carried) -> (usize, usize, usize, usize, usize) {
        (c.metas, c.transcripts, c.joined, c.already, c.unfinished)
    }
    fn carry(old: &Path, new: &Path, id: &str, mode: CarryAgents, dry: bool) -> Result<Carried> {
        carry_agents(std::slice::from_ref(&old.to_path_buf()), new, id, mode, &[], dry)
    }

    #[test]
    fn metas_go_verbatim_and_transcripts_restamped_to_the_same_place_under_the_new_id() -> Result<()> {
        let world = crate::test_world("carry-agents");
        let old = world.proj.join("old.jsonl");
        fs::write(&old, "{}\n")?;
        let sub = world.proj.join("old/subagents");
        fs::create_dir_all(sub.join("workflows/run-1"))?;
        let meta = r#"{"agentType":"Explore","model":"sonnet","description":"survey the beds","toolUseId":"toolu_1"}"#;
        fs::write(sub.join("agent-a1.meta.json"), meta)?;
        let transcript = record("b1", None, "old", "user", "survey the beds")
            + &record(
                "b2",
                Some("b1"),
                "old",
                "assistant",
                "old beds, sessionId stays in text",
            );
        fs::write(sub.join("agent-a1.jsonl"), &transcript)?;
        fs::write(
            sub.join("workflows/run-1/agent-w1.meta.json"),
            r#"{"agentType":"builder"}"#,
        )?;
        fs::write(sub.join("workflows/run-1/journal.jsonl"), "{}\n")?;
        fs::write(sub.join("agent-c2.meta.json"), r#"{"agentType":"general-purpose"}"#)?;
        fs::write(sub.join("agent-a1.flat.jsonl"), "{}\n")?;
        let new = world.proj.join("new.jsonl");
        let sources = [old.clone(), old.clone()];

        let dry = carry(&old, &new, "new", CarryAgents::Full, true)?;
        assert_eq!(counts(&dry), (3, 1, 0, 0, 0));
        assert!(!world.proj.join("new").exists(), "a dry run writes nothing");

        let carried = carry_agents(&sources, &new, "new", CarryAgents::Full, &[], false)?;
        let dst = world.proj.join("new/subagents");
        assert_eq!((counts(&carried), carried.duplicates), ((3, 1, 0, 0, 0), 4));
        assert_eq!(carried.dir, dst);
        assert_eq!(fs::read_to_string(dst.join("agent-a1.meta.json"))?, meta);
        assert_eq!(
            fs::read_to_string(dst.join("agent-c2.meta.json"))?,
            r#"{"agentType":"general-purpose"}"#
        );
        assert_eq!(
            fs::read_to_string(dst.join("workflows/run-1/agent-w1.meta.json"))?,
            r#"{"agentType":"builder"}"#
        );
        assert!(!dst.join("workflows/run-1/journal.jsonl").exists());
        assert_eq!(fs::read_to_string(dst.join("agent-a1.flat.jsonl"))?, "{}\n");
        assert_eq!(fs::read_dir(&dst)?.count(), 5);
        let out = fs::read_to_string(dst.join("agent-a1.jsonl"))?;
        assert_eq!(
            (carried.transcript_bytes, dry.transcript_bytes),
            (out.len() as u64, out.len() as u64)
        );
        assert_eq!(out.matches(r#":"new""#).count(), 4);
        assert_eq!(crate::restamp::restamp(&out, "old").body, transcript);
        assert_eq!(fs::read_to_string(sub.join("agent-a1.jsonl"))?, transcript);
        let again = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&again), (0, 0, 0, 4, 0));
        assert_eq!(fs::read_to_string(dst.join("agent-a1.jsonl"))?, out);

        let lean = world.proj.join("lean.jsonl");
        let only = ["agent-a1".to_string()];
        let carried = carry_agents(&sources[..1], &lean, "lean", CarryAgents::Meta, &only, false)?;
        assert_eq!((carried.metas, carried.transcripts), (1, 0));
        assert!(!world.proj.join("lean/subagents/agent-a1.jsonl").exists());
        assert!(!world.proj.join("lean/subagents/workflows").exists());
        assert!(carry(&old, &old, "old", CarryAgents::Meta, false).is_err());
        Ok(())
    }

    #[test]
    fn mid_turn_join_deduplicates_uuid_and_uuidless_records_restamps_tail_and_emits_trailing_newline() -> Result<()> {
        let world = crate::test_world("carry-agents-mid-turn");
        let old = world.proj.join("old.jsonl");
        fs::write(&old, "{}\n")?;
        let from = world.proj.join("old/subagents");
        let into = world.proj.join("new/subagents");
        fs::create_dir_all(&from)?;
        fs::create_dir_all(&into)?;
        let meta = r#"{"agentType":"Explore","model":"sonnet","description":"count the trees"}"#;
        fs::write(from.join("agent-a1.meta.json"), meta)?;
        let summary_old = r#"{"type":"summary","sessionId":"old","summary":"headed into orchard"}"#;
        let brief = record("b1", None, "old", "user", "count the trees, then wait");
        let early = record("b2", Some("b1"), "old", "assistant", "counting the first row");
        let running = serde_json::json!({"type": "assistant", "uuid": "b5", "parentUuid": "b4", "sessionId": "stale_sid",
            "agentId": "a1", "isSidechain": true, "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_running", "name": "Bash", "input": {"command": "count"}}]}});
        let queue_new =
            "{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"sessionId\":\"new\",\"content\":\"ping\"}\n";
        // Notice `from` ends without `\n` and `to` also ends without `\n`: both must emit `\n` and
        // match `dry_run.transcript_bytes` byte for byte.
        let from_body = format!("{summary_old}\n{brief}{}", early.trim_end_matches('\n'));
        let tail = format!(
            "{queue_new}{}{}{}",
            record("b3", Some("b2"), "new", "assistant", "forty trees"),
            record("b4", Some("b3"), "new", "user", "and the hedges"),
            running
        );
        fs::write(from.join("agent-a1.jsonl"), &from_body)?;
        fs::write(into.join("agent-a1.jsonl"), &tail)?;
        let new = world.proj.join("new.jsonl");

        let dry = carry(&old, &new, "new", CarryAgents::Full, true)?;
        assert_eq!(counts(&dry), (1, 0, 1, 0, 0));
        assert_eq!(fs::read_to_string(into.join("agent-a1.jsonl"))?, tail, "dry");

        let carried = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (1, 0, 1, 0, 0));
        let out = fs::read_to_string(into.join("agent-a1.jsonl"))?;
        assert!(out.ends_with("}\n"), "always ends with newline");
        assert_eq!(
            (carried.transcript_bytes, dry.transcript_bytes),
            (out.len() as u64, out.len() as u64),
            "dry_run and written byte counts match without skew"
        );
        assert!(
            !out.contains("stale_sid"),
            "destination records are restamped to new_id too:\n{out}"
        );
        let ids: Vec<String> = out.lines().filter_map(uuid_of).collect();
        assert_eq!(ids, ["b1", "b2", "b3", "b4", "b5"]);
        assert_eq!(out.lines().count(), 7, "5 uuid records + 2 uuid-less records");

        // Strictly idempotent on repeat: neither UUID records nor UUID-less (`summary`, `queue-operation`) duplicate!
        let settled = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&settled), (0, 0, 0, 2, 0));
        assert_eq!(fs::read_to_string(into.join("agent-a1.jsonl"))?, out);

        // Also check `Outcome::Copied` when `from` lacks a trailing `\n`: output ends in `\n`.
        let copy_dst = world.proj.join("copied.jsonl");
        let copy_src = world.proj.join("copy_src.jsonl");
        fs::write(&copy_src, "{}\n")?;
        fs::create_dir_all(world.proj.join("copy_src/subagents"))?;
        fs::write(
            world.proj.join("copy_src/subagents/agent-a1.jsonl"),
            brief.trim_end_matches('\n'),
        )?;
        let dry_copy = carry(&copy_src, &copy_dst, "copied", CarryAgents::Full, true)?;
        let real_copy = carry(&copy_src, &copy_dst, "copied", CarryAgents::Full, false)?;
        let copied_out = fs::read_to_string(world.proj.join("copied/subagents/agent-a1.jsonl"))?;
        assert!(copied_out.ends_with("}\n"));
        assert_eq!(dry_copy.transcript_bytes, copied_out.len() as u64);
        assert_eq!(real_copy.transcript_bytes, copied_out.len() as u64);
        Ok(())
    }

    #[test]
    fn an_unfinished_last_record_is_left_out_and_counted() -> Result<()> {
        let world = crate::test_world("carry-agents-unfinished");
        let old = world.proj.join("old.jsonl");
        fs::write(&old, "{}\n")?;
        let from = world.proj.join("old/subagents");
        fs::create_dir_all(&from)?;
        let whole =
            record("b1", None, "old", "user", "count the trees") + &record("b2", Some("b1"), "old", "assistant", "one");
        let half = r#"{"type":"assistant","uuid":"b3","parentUuid":"b2","sessionId":"old","agentId":"a1","isSidech"#;
        fs::write(from.join("agent-a1.jsonl"), format!("{whole}{half}"))?;
        let stamped = whole
            .replace(r#""sessionId":"old""#, r#""sessionId":"new""#)
            .replace(r#""session_id":"old""#, r#""session_id":"new""#);

        let new = world.proj.join("new.jsonl");
        let carried = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (0, 1, 0, 0, 1));
        let to = world.proj.join("new/subagents/agent-a1.jsonl");
        assert_eq!(fs::read_to_string(&to)?, stamped);

        let joined = world.proj.join("join.jsonl");
        let into = world.proj.join("join/subagents");
        fs::create_dir_all(&into)?;
        let tail = record("b4", Some("b2"), "join", "assistant", "two");
        fs::write(into.join("agent-a1.jsonl"), format!("{tail}{{\"type\":\"user\",\"uu"))?;
        fs::write(from.join("agent-a1.jsonl"), &whole)?;
        let carried = carry(&old, &joined, "join", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (0, 0, 1, 0, 1));
        assert_eq!(
            fs::read_to_string(into.join("agent-a1.jsonl"))?,
            stamped.replace(r#":"new""#, r#":"join""#) + &tail
        );
        Ok(())
    }

    #[test]
    fn a_refused_transcript_in_source_or_dest_is_skipped_and_named_and_the_rest_are_carried() -> Result<()> {
        let world = crate::test_world("carry-agents-refused");
        let old = world.proj.join("old.jsonl");
        fs::write(&old, "{}\n")?;
        let from = world.proj.join("old/subagents");
        fs::create_dir_all(&from)?;
        let agent = |id: &str, extra: &str| -> Result<()> {
            let lines = record("b1", None, "old", "user", "count")
                .replace(r#""agentId":"a1""#, &format!(r#""agentId":"{id}""#));
            fs::write(from.join(format!("agent-{id}.jsonl")), format!("{lines}{extra}"))?;
            fs::write(
                from.join(format!("agent-{id}.meta.json")),
                format!(r#"{{"description":"{id}"}}"#),
            )?;
            Ok(())
        };
        agent("a1", "")?;
        agent("a2", "not a record\n")?;
        agent("a3", "")?;
        // `a4` has valid source JSON, but destination `to` has wrong `agentId` (`verify_carried` refusal).
        agent("a4", "")?;
        let new = world.proj.join("new.jsonl");
        let into = world.proj.join("new/subagents");
        fs::create_dir_all(&into)?;
        fs::write(
            into.join("agent-a4.jsonl"),
            r#"{"type":"assistant","uuid":"b2","parentUuid":"b1","sessionId":"new","agentId":"wrong","isSidechain":true,"message":{"role":"assistant","content":"bad"}}"#,
        )?;

        let carried = carry(&old, &new, "new", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried), (4, 2, 0, 0, 0));
        let refused_ids: Vec<&str> = carried.refused.iter().map(|r| r.agent.as_str()).collect();
        assert_eq!(refused_ids, ["a2", "a4"]);
        assert!(carried.refused[0].why.contains("is not one JSON value"));
        assert!(carried.refused[1].why.contains("agentId"));
        for id in ["a1", "a3"] {
            assert!(into.join(format!("agent-{id}.jsonl")).is_file(), "{id}");
        }
        assert!(!into.join("agent-a2.jsonl").exists());
        assert_eq!(
            fs::read_to_string(into.join("agent-a2.meta.json"))?,
            r#"{"description":"a2"}"#
        );
        assert_eq!(
            fs::read_to_string(into.join("agent-a4.meta.json"))?,
            r#"{"description":"a4"}"#
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_on_source_or_destination_are_refused_without_exfiltration_or_escape() -> Result<()> {
        use std::os::unix::fs::symlink;
        let world = crate::test_world("carry-agents-symlinks");
        let secret = world.base.join("secret.json");
        fs::write(&secret, r#"{"token":"super-secret"}"#)?;
        let outside_dir = world.base.join("outside");
        fs::create_dir_all(&outside_dir)?;

        // 1. Source symlink on `.meta.json` is refused.
        let src1 = world.proj.join("src1.jsonl");
        fs::write(&src1, "{}\n")?;
        fs::create_dir_all(world.proj.join("src1/subagents"))?;
        symlink(&secret, world.proj.join("src1/subagents/agent-a1.meta.json"))?;
        let dst1 = world.proj.join("dst1.jsonl");
        assert!(carry(&src1, &dst1, "dst1", CarryAgents::Full, false).is_err());
        assert!(!world.proj.join("dst1").exists());

        // 2. Destination symlinked `subagents` or `workflows` subdir or target file is refused.
        let src2 = world.proj.join("src2.jsonl");
        fs::write(&src2, "{}\n")?;
        fs::create_dir_all(world.proj.join("src2/subagents/workflows/r1"))?;
        fs::write(
            world.proj.join("src2/subagents/agent-a1.meta.json"),
            r#"{"agentType":"Explore"}"#,
        )?;
        fs::write(
            world.proj.join("src2/subagents/agent-a1.jsonl"),
            record("b1", None, "src2", "user", "brief"),
        )?;
        fs::write(
            world.proj.join("src2/subagents/workflows/r1/agent-w1.meta.json"),
            r#"{"agentType":"builder"}"#,
        )?;

        let dst2 = world.proj.join("dst2.jsonl");
        fs::create_dir_all(world.proj.join("dst2/subagents"))?;
        symlink(&outside_dir, world.proj.join("dst2/subagents/workflows"))?;
        assert!(carry(&src2, &dst2, "dst2", CarryAgents::Full, false).is_err());
        assert_eq!(fs::read_dir(&outside_dir)?.count(), 0, "nothing written outside");

        let dst3 = world.proj.join("dst3.jsonl");
        fs::create_dir_all(world.proj.join("dst3/subagents"))?;
        symlink(&secret, world.proj.join("dst3/subagents/agent-a1.jsonl"))?;
        assert!(carry(&src2, &dst3, "dst3", CarryAgents::Full, false).is_err());
        assert_eq!(fs::read_to_string(&secret)?, r#"{"token":"super-secret"}"#);
        Ok(())
    }

    #[test]
    fn split_tail_materializes_root_prefix_and_meta_and_joined_split_advances_beside_tip() -> Result<()> {
        let world = crate::test_world("carry-chain-composition");
        let s1 = world.proj.join("s1.jsonl");
        let s2 = world.proj.join("s2.jsonl");
        let s3 = world.proj.join("s3.jsonl");
        let s3_meta = world.proj.join("s3m.jsonl");
        for p in [&s1, &s2, &s3, &s3_meta] {
            fs::write(p, "{}\n")?;
        }
        fs::create_dir_all(world.proj.join("s1/subagents"))?;
        fs::create_dir_all(world.proj.join("s2/subagents"))?;

        let meta = r#"{"agentType":"Explore","model":"sonnet","description":"survey rivers","toolUseId":"toolu_s1"}"#;
        fs::write(world.proj.join("s1/subagents/agent-a1.meta.json"), meta)?;
        fs::write(
            world.proj.join("s1/subagents/agent-a1.jsonl"),
            record("x1", None, "s1", "user", "survey rivers") + &record("x2", Some("x1"), "s1", "assistant", "first"),
        )?;
        // `s2` is an unjoined `/resume` split tail: starts at `parentUuid: "x2"` and has NO `.meta.json`.
        fs::write(
            world.proj.join("s2/subagents/agent-a1.jsonl"),
            record("y1", Some("x2"), "s2", "user", "more") + &record("y2", Some("y1"), "s2", "assistant", "done"),
        )?;

        // Rule 1a: `--carry-agents=meta` from split tail `s2` discovers and copies `s1`'s `.meta.json`!
        let carried_m = carry(&s2, &s3_meta, "s3m", CarryAgents::Meta, false)?;
        assert_eq!(counts(&carried_m), (1, 0, 0, 0, 0));
        assert_eq!(
            fs::read_to_string(world.proj.join("s3m/subagents/agent-a1.meta.json"))?,
            meta
        );

        // Rule 1b: `--carry-agents=full` from split tail `s2` into `s3` materializes `[x1, x2, y1, y2]` + meta!
        let carried_f = carry(&s2, &s3, "s3", CarryAgents::Full, false)?;
        assert_eq!(counts(&carried_f), (1, 1, 0, 0, 0));
        let s3_jsonl = world.proj.join("s3/subagents/agent-a1.jsonl");
        let s3_ids: Vec<String> = fs::read_to_string(&s3_jsonl)?.lines().filter_map(uuid_of).collect();
        assert_eq!(s3_ids, ["x1", "x2", "y1", "y2"]);
        assert_eq!(
            fs::read_to_string(world.proj.join("s3/subagents/agent-a1.meta.json"))?,
            meta
        );
        assert!(
            chain::prefix_and_tail(&s3_jsonl).0.is_empty(),
            "s3 starts at root x1 (parentUuid: null) and chains nothing externally"
        );

        // Rule 2: joining `s1` INTO `s2` prepends `[x1, x2]` onto `s2`, yet `Beside::of(s1).tip`
        // STILL advances `s1 -> s2` (whereas an uncontinued `cv adopt` copy of `s1` stays distinct).
        let joined = carry(&s1, &s2, "s2", CarryAgents::Full, false)?;
        assert_eq!(counts(&joined), (1, 0, 1, 0, 0));
        let s1_jsonl = world.proj.join("s1/subagents/agent-a1.jsonl");
        let s2_jsonl = world.proj.join("s2/subagents/agent-a1.jsonl");
        assert_eq!(chain::Beside::of(&s1).tip(&s1_jsonl), s2_jsonl);
        Ok(())
    }
}
