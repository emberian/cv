//! Task event log: one JSONL file with locked compare-and-swap appends.
//!
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` + sibling `events.lock`: advisory locking across
//! replay, validation, tail repair and `O_APPEND` writes (see [`crate::lockfile`]). Accepted
//! history is append-only; an incomplete final record may be removed before a successful append.
//! `write_all` can make multiple writes: a failed/crashed batch may leave a completed prefix and
//! an incomplete last record. Flush is not an fsync or a crash-atomic batch guarantee.
//!
//! # Loud, not lenient
//!
//! A chat reader may skip malformed lines; a replayed event log may not (a silently dropped
//! event changes history). Here interior garbage is **loud** (surfaced as warnings on every read)
//! and complete unrecognized final rows are loud too. Reads tolerate only an incomplete JSON
//! tail or an incomplete UTF-8 scalar at EOF. A complete but unrecognized final record refuses
//! append. Repair follows admission of the entire candidate
//! batch. A valid final event without a newline is preserved and separated from new records.
//!
//! # The CAS append
//!
//! [`TaskStore::append`] holds the lock across replay-validate-append: acquire → replay the log
//! through the reducer → apply the candidate event (any [`ReduceError`] rejects it) → append →
//! release. This is what makes `claim` a race-free first-writer-wins with zero extra lock kinds,
//! and it guarantees the log never contains an event the reducer would refuse on replay.
//!
//! # Retired kinds at the seam
//!
//! [`TaskStore::append_agent_event`] — the only path the CLI verbs and MCP tools use — refuses
//! the kinds retired in 0.15.0 ([`super::model::RETIRED_TAGS`]). The reducer itself cannot
//! (it must replay them from old logs); the store seam is where "never written again" is
//! mechanical.
//!
//! # Growth & retention
//!
//! The log is append-only and unbounded. A command that needs only the read model (the prompt
//! hook, a mutation's target lookup and its CAS append, `cv task list`, `GET /api/tasks`)
//! resumes from a [`Fold`]: the reducer's state at the last newline-terminated line, kept in the
//! catalog and admitted only while the log's prefix still samples as it did. It reads the events
//! after that mark alone, so E appends cost O(E) parses in all rather than O(E²); loading the
//! fold costs the state's size (the model and one digest per event id). A reader that lists
//! events (`events`, `show`, the inbox's authors) still replays the whole log: that is its answer.
//! The append validates against the fold only when it carries no warning; otherwise it replays
//! whole, so its refusal names the line as before.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::lockfile::FileLock;

use super::model::{TaskEvent, TaskEventKind};
use super::reduce::{TaskReadModel, TaskReducer};

/// The log-format header record: the first line of every log this cv creates. Readers skip a
/// valid header; a header-less file is an implicit v1 (pre-header logs exist); a header with a
/// version above [`LOG_VERSION`] refuses replay loudly (written by a newer cv).
const LOG_FORMAT: &str = "cv-task-log";
const LOG_VERSION: u64 = 1;
const HEADER_LINE: &str = r#"{"format":"cv-task-log","v":1}"#;

/// Events paired with their 1-based line number in the log file — the raw material of a replay,
/// where the line number keys quarantine warnings back to the physical file.
type NumberedEvents = Vec<(usize, TaskEvent)>;

/// What one pass over the log file yields: the parseable events with their line numbers, loud
/// warnings about interior garbage, and the physical EOF framing.
struct RawRead {
    events: NumberedEvents,
    warnings: Vec<String>,
    tail: LogTail,
    /// Where the last newline-terminated line ends, how many lines precede it, and how many of
    /// `events` and `warnings` it covers: the boundary a [`Fold`] may be taken at.
    committed: Committed0,
}

#[cfg(test)]
thread_local! {
    /// Non-blank log lines parsed: the fold test's counter.
    static PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// See [`RawRead::committed`].
#[derive(Clone, Copy, Default)]
struct Committed0 {
    offset: u64,
    lines: usize,
    events: usize,
    warnings: usize,
}

/// Physical EOF framing, separate from events admitted by replay. Readers do not modify it.
#[derive(Clone, Copy)]
enum LogTail {
    Terminated,
    NeedsNewline,
    Torn { offset: u64 },
    Unrecognized { line_no: usize },
}

/// A parsed replay of the whole log: the read model plus any loud warnings about interior
/// garbage. Every read surface must show `warnings` to its caller — a corrupted coordination log
/// is a finding, not a detail.
///
/// Read paths are **best effort + loud**: an interior event the reducer refuses is *quarantined*
/// (skipped, counted, warned about) rather than bricking every consumer at once. The append path
/// stays strict — see [`TaskStore::append`].
#[derive(Debug)]
pub struct ReplayOutcome {
    pub model: TaskReadModel,
    /// The events that actually folded into `model` (quarantined events are excluded).
    pub events: Vec<TaskEvent>,
    pub warnings: Vec<String>,
    /// Parseable events the reducer refused on replay; each also produced a warning.
    pub quarantined: usize,
}

/// Handle on a task log directory. Cheap to construct; every operation opens files fresh.
#[derive(Clone, Debug)]
pub struct TaskStore {
    dir: PathBuf,
}

/// The state validated and committed while holding the append lock. Carry it to callers rather
/// than replaying again after unlocking, when another writer may already have changed the log.
pub(super) struct Committed {
    pub events: Vec<TaskEvent>,
    pub model: TaskReadModel,
}

impl TaskStore {
    /// The default store at [`super::tasks_dir`].
    pub fn default_store() -> TaskStore {
        TaskStore {
            dir: super::tasks_dir(),
        }
    }

    /// A store rooted at `dir` (testing and embedding).
    pub fn at(dir: impl Into<PathBuf>) -> TaskStore {
        TaskStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("events.lock")
    }

    /// Authoritative `state.db` backing this task store (`$CLUSTERVISION_HOME/state.db` when rooted
    /// at `<home>/tasks`, or `<dir>/state.db` for standalone directories).
    #[cfg(feature = "sqlite")]
    pub fn state_store(&self) -> crate::state::StateStore {
        if self.dir.file_name().and_then(|n| n.to_str()) == Some("tasks") {
            if let Some(parent) = self.dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                return crate::state::StateStore::at(parent.join("state.db"));
            }
        }
        crate::state::StateStore::at(self.dir.join("state.db"))
    }

    /// Replay without holding the lock. Readers may see a completed prefix of a concurrent
    /// batch and ignore an incomplete final record; this is not an atomic snapshot of a batch.
    ///
    /// Events the reducer refuses are **quarantined**: skipped with a loud warning (line number +
    /// reason) and counted, so one poisoned interior line degrades the model instead of bricking
    /// the CLI, `cv serve`, and every append at once.
    pub fn replay(&self) -> Result<ReplayOutcome> {
        let mut read = self.read_events()?;
        let path = self.events_path();
        let mut reducer = TaskReducer::new();
        let mut events = Vec::with_capacity(read.events.len());
        let mut quarantined = 0usize;
        for (line_no, ev) in read.events {
            match reducer.apply(&ev) {
                Ok(()) => events.push(ev),
                Err(e) => {
                    quarantined += 1;
                    read.warnings.push(quarantine_warning(&path, line_no, &e));
                }
            }
        }
        Ok(ReplayOutcome {
            model: reducer.into_model(),
            events,
            warnings: read.warnings,
            quarantined,
        })
    }

    /// The read model without the event list: [`Self::replay`]'s `model`, `warnings` and
    /// `quarantined`, resumed from the log's stored [`Fold`] so only the events appended since
    /// are read and reduced. What the prompt hook and every mutation's target lookup ask.
    pub fn fold(&self) -> Result<Folded> {
        let path = self.events_path();
        let state = self.state_store_for_fold();
        let start = Fold::stored(&path, state.as_ref());
        let from = start.as_ref().map(Fold::boundary).unwrap_or_default();
        let (mut reducer, mut garbage, mut refused) = match start {
            Some(fold) => (
                TaskReducer::resume(fold.model, fold.applied),
                fold.garbage,
                fold.refused,
            ),
            None => (TaskReducer::new(), Vec::new(), Vec::new()),
        };
        let read = self.read_events_from(from)?;
        let committed = read.committed;
        let mut snapshot = None;
        let take = |reducer: &TaskReducer, refused: &[String], garbage: &[String]| {
            (committed.offset > from.offset).then(|| (reducer.checkpoint(), refused.to_vec(), garbage.to_vec()))
        };
        let mut new_garbage = read.warnings.into_iter();
        garbage.extend(new_garbage.by_ref().take(committed.warnings));
        for (i, (line_no, ev)) in read.events.into_iter().enumerate() {
            if i == committed.events {
                snapshot = take(&reducer, &refused, &garbage);
            }
            if let Err(e) = reducer.apply(&ev) {
                refused.push(quarantine_warning(&path, line_no, &e));
            }
        }
        if snapshot.is_none() {
            snapshot = take(&reducer, &refused, &garbage);
        }
        if let Some(((model, applied), refused, garbage)) = snapshot {
            Fold {
                version: STATE_FOLD_VERSION,
                offset: committed.offset,
                lines: committed.lines,
                model,
                applied,
                garbage,
                refused,
            }
            .store(&path, state.as_ref());
        }
        garbage.extend(new_garbage);
        let quarantined = refused.len();
        garbage.extend(refused);
        Ok(Folded {
            model: reducer.into_model(),
            warnings: garbage,
            quarantined,
        })
    }

    /// Agent-facing append: the write-time rules ([`TaskReducer::admit`] — retired kinds, bare
    /// `decision` tags) and the CAS, in one locked step.
    pub fn append_agent_event(&self, event: TaskEvent) -> Result<TaskEvent> {
        self.append(event)
    }

    /// Validate several events in order under one lock before writing any. Candidate refusal
    /// leaves the log unchanged; I/O failure or a crash may leave a completed prefix of the batch.
    /// `cv task done --note-file F` orders the evidence note before the terminal event.
    pub fn append_agent_events(&self, events: Vec<TaskEvent>) -> Result<Vec<TaskEvent>> {
        if events.is_empty() {
            return Ok(events);
        }
        Ok(self.append_committed(events)?.events)
    }

    /// The locked CAS: replay → apply candidate → append. Returns the appended event.
    ///
    /// Unlike the read path, the append path is **strict and fails closed on degraded reads**:
    /// any read warning (interior garbage) or reducer-refused line means this binary's model of
    /// the log is incomplete — a stale binary must not write against an incomplete model, so the
    /// append is refused with the problem named instead of being validated against a lie.
    fn append(&self, event: TaskEvent) -> Result<TaskEvent> {
        let mut out = self.append_committed(vec![event])?.events;
        Ok(out.pop().expect("one event in, one out"))
    }

    /// [`Self::append`] for a batch: admit and serialize every candidate before repairing EOF
    /// framing or writing. The lock covers the entire replay, repair and append.
    pub(super) fn append_committed(&self, events: Vec<TaskEvent>) -> Result<Committed> {
        anyhow::ensure!(!crate::env::frozen(), "--frozen forbids writing tasks");
        debug_assert!(!events.is_empty());
        fs::create_dir_all(&self.dir).with_context(|| format!("creating tasks dir {}", self.dir.display()))?;
        let _lock = FileLock::acquire(&self.lock_path())?;

        // Replay under the lock: the candidate is validated against the exact state it will
        // land after. A clean stored fold stands for the log's prefix; only what follows is read.
        let state_ref = self.state_store_for_fold();
        let start = Fold::stored(&self.events_path(), state_ref.as_ref())
            .filter(|fold| fold.garbage.is_empty() && fold.refused.is_empty());
        let read = self.read_events_from(start.as_ref().map(Fold::boundary).unwrap_or_default())?;
        let tail = read.tail;
        if let LogTail::Unrecognized { line_no } = tail {
            bail!(
                "refusing to append: task log {} line {line_no} is an unrecognized final record \
                — only an incomplete tail can be recovered; repair the log or upgrade cv before writing",
                self.events_path().display()
            );
        }
        if let Some(first) = read.warnings.first() {
            bail!(
                "refusing to append: task log {} is degraded ({} warning(s); first: {first}) — \
                 repair the log or upgrade cv before writing",
                self.events_path().display(),
                read.warnings.len()
            );
        }
        let mut reducer = match start {
            Some(fold) => TaskReducer::resume(fold.model, fold.applied),
            None => TaskReducer::new(),
        };
        for (line_no, ev) in &read.events {
            reducer.apply(ev).map_err(|e| {
                anyhow::anyhow!(
                    "refusing to append: task log {} line {line_no} fails replay ({e}) — an \
                     incomplete model must not be written against",
                    self.events_path().display()
                )
            })?;
        }
        // Each candidate is admitted (what may be written today) and applied (what it will mean)
        // against the exact state it lands after — the previous candidates of the batch included.
        for event in &events {
            reducer
                .admit(event)
                .and_then(|()| reducer.apply(event))
                .map_err(|e| anyhow::anyhow!("event rejected: {e}"))?;
        }

        // Persist authoritative rows inside `state.db` (`task_events`) under `BEGIN IMMEDIATE` +
        // `synchronous=FULL` before updating the companion JSONL stream.
        #[cfg(feature = "sqlite")]
        {
            let state = self.state_store();
            let mut conn = state
                .open_writable()
                .with_context(|| format!("opening authoritative state.db at {}", state.path().display()))?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .context("starting state.db task_events transaction")?;
            for (_, prior) in &read.events {
                let raw_json = serde_json::to_string(prior).context("serializing prior task event")?;
                tx.execute(
                    "INSERT INTO task_events(event_id, task_id, ts, by_actor, raw_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(event_id) DO NOTHING",
                    rusqlite::params![prior.id, prior.task_id, prior.ts.to_rfc3339(), prior.by, raw_json],
                )?;
            }
            for event in &events {
                let raw_json = serde_json::to_string(event).context("serializing task event")?;
                tx.execute(
                    "INSERT INTO task_events(event_id, task_id, ts, by_actor, raw_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(event_id) DO NOTHING",
                    rusqlite::params![event.id, event.task_id, event.ts.to_rfc3339(), event.by, raw_json],
                )?;
            }
            tx.commit().context("committing task_events to state.db")?;
        }

        // First append to a fresh log stamps the format header (older, header-less logs stay
        // valid: readers treat them as implicit v1).
        let path = self.events_path();
        let mut line = Vec::new();
        if matches!(tail, LogTail::NeedsNewline) {
            line.push(b'\n');
        }
        if !path.exists() {
            line.extend_from_slice(HEADER_LINE.as_bytes());
            line.push(b'\n');
        }
        for event in &events {
            serde_json::to_writer(&mut line, event).context("serializing task event")?;
            line.push(b'\n');
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening task log {}", self.events_path().display()))?;
        // Candidate rejection and serialization failure cannot mutate an existing log. Once
        // admitted, discard only the unparseable EOF suffix excluded from the validated history.
        if let LogTail::Torn { offset } = tail {
            f.set_len(offset)
                .with_context(|| format!("repairing incomplete task log tail {}", path.display()))?;
        }
        f.write_all(&line)
            .with_context(|| format!("appending to task log {}", self.events_path().display()))?;
        f.sync_data()
            .with_context(|| format!("syncing task log {}", self.events_path().display()))?;
        Ok(Committed {
            events,
            model: reducer.into_model(),
        })
    }

    #[cfg(feature = "sqlite")]
    fn state_store_for_fold(&self) -> Option<crate::state::StateStore> {
        Some(self.state_store())
    }

    #[cfg(not(feature = "sqlite"))]
    fn state_store_for_fold(&self) -> Option<()> {
        None
    }

    /// Read raw events as [`RawRead`]. Interior unparseable lines are collected as warnings,
    /// including complete malformed rows without a final newline.
    /// A syntactically incomplete EOF prefix is tolerated silently. Syntax-incomplete tails carry a byte boundary for locked recovery;
    /// other unrecognized final rows refuse append. Valid unterminated records need a newline.
    /// A format header is skipped when current, and refused loudly for a newer format version.
    fn read_events(&self) -> Result<RawRead> {
        self.read_events_from(Committed0::default())
    }

    /// [`Self::read_events`] of the lines after `from` (a [`Fold`]'s boundary), numbered on.
    fn read_events_from(&self, from: Committed0) -> Result<RawRead> {
        use std::io::{Seek, SeekFrom};
        let path = self.events_path();
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(feature = "sqlite")]
                if from.offset == 0 {
                    if let Some(db_read) = self.read_events_from_state_db()? {
                        return Ok(db_read);
                    }
                }
                return Ok(RawRead {
                    events: Vec::new(),
                    warnings: Vec::new(),
                    tail: LogTail::Terminated,
                    committed: Committed0::default(),
                });
            }
            Err(e) => return Err(e).with_context(|| format!("opening task log {}", path.display())),
        };
        file.seek(SeekFrom::Start(from.offset))
            .with_context(|| format!("reading task log {}", path.display()))?;

        let mut events: Vec<(usize, TaskEvent)> = Vec::new();
        let mut bad: Vec<(usize, String)> = Vec::new(); // (1-based line no, snippet)
        let mut reader = BufReader::new(file);
        let mut raw = Vec::new();
        let mut line_no = from.lines;
        let mut offset = from.offset;
        let mut tail = LogTail::Terminated;
        let mut committed = Committed0 {
            offset: from.offset,
            lines: from.lines,
            events: 0,
            warnings: 0,
        };
        loop {
            raw.clear();
            let n = reader
                .read_until(b'\n', &mut raw)
                .with_context(|| format!("reading task log {}", path.display()))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let start = offset;
            offset += n as u64;
            let terminated = raw.ends_with(b"\n");
            tail = if terminated {
                LogTail::Terminated
            } else {
                LogTail::NeedsNewline
            };
            let text = match std::str::from_utf8(&raw) {
                Ok(text) => text,
                Err(error) if !terminated && error.error_len().is_none() => {
                    // A valid UTF-8 prefix ending inside its final scalar is a possible torn write.
                    tail = LogTail::Torn { offset: start };
                    break;
                }
                Err(error) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                        .with_context(|| format!("reading task log {}", path.display()));
                }
            };
            let line = text.trim();
            if line.is_empty() {
                if terminated {
                    committed = Committed0 {
                        offset,
                        lines: line_no,
                        events: events.len(),
                        warnings: bad.len(),
                    };
                }
                continue;
            }
            #[cfg(test)]
            PARSED.with(|n| n.set(n.get() + 1));
            match serde_json::from_str::<TaskEvent>(line) {
                Ok(ev) => events.push((line_no, ev)),
                Err(_) => {
                    if let Some(v) = header_version(line) {
                        if v > LOG_VERSION {
                            bail!(
                                "task log {} declares format v{v} (this cv reads v{LOG_VERSION}) \
                                 — the log was written by a newer cv; upgrade before touching it",
                                path.display()
                            );
                        }
                        // current (or ancient) header: skip
                        if terminated {
                            committed = Committed0 {
                                offset,
                                lines: line_no,
                                events: events.len(),
                                warnings: bad.len(),
                            };
                        }
                        continue;
                    }
                    if !terminated {
                        // A complete unknown event could come from a newer binary. Recover only
                        // an unfinished JSON value, never erase a row just because its event schema
                        // or syntax is unsupported. RawValue checks grammar without a Value tree.
                        tail = match serde_json::from_str::<&serde_json::value::RawValue>(line) {
                            Err(error) if error.is_eof() => LogTail::Torn { offset: start },
                            _ => {
                                bad.push((line_no, line.chars().take(80).collect()));
                                LogTail::Unrecognized { line_no }
                            }
                        };
                        break;
                    }
                    let snippet: String = line.chars().take(80).collect();
                    bad.push((line_no, snippet));
                }
            }
            if terminated {
                committed = Committed0 {
                    offset,
                    lines: line_no,
                    events: events.len(),
                    warnings: bad.len(),
                };
            }
        }

        let warnings = bad
            .into_iter()
            .map(|(n, snippet)| {
                format!(
                    "task log {}: line {} is not a valid event (log may be corrupted): {}",
                    path.display(),
                    n,
                    snippet
                )
            })
            .collect();
        Ok(RawRead {
            events,
            warnings,
            tail,
            committed,
        })
    }

    #[cfg(feature = "sqlite")]
    fn read_events_from_state_db(&self) -> Result<Option<RawRead>> {
        let state = self.state_store();
        let Some(conn) = state
            .open_readonly()
            .with_context(|| format!("opening authoritative state.db at {}", state.path().display()))?
        else {
            return Ok(None);
        };
        let mut stmt = conn.prepare("SELECT seq, raw_json FROM task_events ORDER BY seq ASC")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut events = Vec::new();
        let mut warnings = Vec::new();
        for row in rows {
            let (seq, raw_json) = row?;
            #[cfg(test)]
            PARSED.with(|n| n.set(n.get() + 1));
            match serde_json::from_str::<TaskEvent>(&raw_json) {
                Ok(ev) => events.push((seq as usize, ev)),
                Err(_) => {
                    let snippet: String = raw_json.chars().take(80).collect();
                    warnings.push(format!(
                        "task store {}: row {} is not a valid event: {}",
                        state.path().display(),
                        seq,
                        snippet
                    ));
                }
            }
        }
        if events.is_empty() && warnings.is_empty() {
            return Ok(None);
        }
        Ok(Some(RawRead {
            events,
            warnings,
            tail: LogTail::Terminated,
            committed: Committed0::default(),
        }))
    }
}

/// The warning a replay gives for an event the reducer refused.
fn quarantine_warning(path: &Path, line_no: usize, e: &super::reduce::ReduceError) -> String {
    format!(
        "task log {}: line {line_no} quarantined — the reducer refused it ({e}); \
         event skipped, model is best-effort",
        path.display()
    )
}

/// [`TaskStore::fold`]'s answer: a replay's model and warnings, without its events.
#[derive(Debug)]
pub struct Folded {
    pub model: TaskReadModel,
    pub warnings: Vec<String>,
    pub quarantined: usize,
}

/// The log's first `offset` bytes, folded: the reducer's state (the model and each applied
/// event's digest) and the warnings so far, unparseable lines apart from refused events because
/// a replay lists every unparseable line first. Kept in the catalog under [`crate::source::VERSION`]
/// as a `task-fold` record keyed by the log's path, and admitted only while the log is the same
/// file, at least `offset` long, its prefix sampling as it did ([`crate::source::Prefix`]: an
/// edit outside the samples is not seen, the limit every resume mark has).
const STATE_FOLD_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct Fold {
    version: u32,
    offset: u64,
    lines: usize,
    model: TaskReadModel,
    applied: std::collections::BTreeMap<String, String>,
    garbage: Vec<String>,
    refused: Vec<String>,
}

#[cfg(feature = "sqlite")]
#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
    path: String,
    prefix: crate::source::Prefix,
    fold: Fold,
}

impl Fold {
    fn boundary(&self) -> Committed0 {
        Committed0 {
            offset: self.offset,
            lines: self.lines,
            events: 0,
            warnings: 0,
        }
    }

    #[cfg(feature = "sqlite")]
    fn stored(path: &Path, state: Option<&crate::state::StateStore>) -> Option<Fold> {
        let conn = state?.open_readonly().ok()??;
        let raw: String = conn
            .query_row("SELECT model_json FROM task_folds WHERE slot = 1", [], |row| row.get(0))
            .ok()?;
        let Stored {
            path: stored_path,
            prefix,
            fold,
        } = serde_json::from_str(&raw).ok()?;
        (fold.version == STATE_FOLD_VERSION
            && stored_path == path.to_string_lossy()
            && prefix.len == fold.offset
            && prefix.holds(path))
        .then_some(fold)
    }

    #[cfg(not(feature = "sqlite"))]
    fn stored(_path: &Path, _state: Option<&()>) -> Option<Fold> {
        None
    }

    /// Best effort: persisted inside `state.db` (`task_folds`), independent of `catalog.db` and
    /// `source::VERSION`, and skipped completely under `--frozen`.
    #[cfg(feature = "sqlite")]
    fn store(self, path: &Path, state: Option<&crate::state::StateStore>) {
        if crate::env::frozen() {
            return;
        }
        let Some(state) = state else { return };
        let Some(prefix) = File::open(path)
            .ok()
            .and_then(|file| crate::source::Prefix::of(&file, self.offset))
        else {
            return;
        };
        let upto_seq = self.applied.len() as i64;
        let stored = Stored {
            path: path.to_string_lossy().into_owned(),
            prefix,
            fold: self,
        };
        let Ok(raw) = serde_json::to_string(&stored) else {
            return;
        };
        let same = serde_json::from_str::<Stored>(&raw)
            .is_ok_and(|back| back.fold.model == stored.fold.model && back.fold.applied == stored.fold.applied);
        if !same {
            return;
        }
        let Ok(conn) = state.open_writable() else { return };
        if let Err(error) = conn.execute(
            "INSERT OR REPLACE INTO task_folds(slot, upto_seq, model_json, applied_json, garbage_json, refused_json)
             VALUES (1, ?1, ?2, '{}', '[]', '[]')",
            rusqlite::params![upto_seq, raw],
        ) {
            crate::env::warn_once("the task store's fold", error);
        }
    }

    #[cfg(not(feature = "sqlite"))]
    fn store(self, _path: &Path, _state: Option<&()>) {}
}

/// If `line` is a log-format header record, its declared version (`v` missing reads as 0, which
/// is ≤ current and therefore skipped — keep it dumb).
fn header_version(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    (v.get("format")?.as_str()? == LOG_FORMAT).then(|| v.get("v").and_then(serde_json::Value::as_u64).unwrap_or(0))
}

/// Build a new event with a fresh uuid v7 id and `ts = now`. For `Opened`, pass
/// `task_id = None` — the event id becomes the task id (open identity invariant).
pub fn new_event(task_id: Option<&str>, by: &str, kind: TaskEventKind) -> TaskEvent {
    let id = uuid::Uuid::now_v7().to_string();
    TaskEvent {
        task_id: task_id.map(String::from).unwrap_or_else(|| id.clone()),
        id,
        ts: Utc::now(),
        by: by.to_string(),
        kind,
    }
}

/// Convenience: replay the default store.
pub fn replay() -> Result<ReplayOutcome> {
    TaskStore::default_store().replay()
}

/// Convenience: [`TaskStore::fold`] of the default store.
pub fn fold() -> Result<Folded> {
    TaskStore::default_store().fold()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::TaskState;

    /// A throwaway task dir under the system temp dir, unique per test (board.rs idiom).
    fn tmp_tasks() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-task-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_kind() -> TaskEventKind {
        crate::task::opened_kind("demo", None)
    }

    fn open_task(store: &TaskStore) -> String {
        store
            .append_agent_event(new_event(None, "agent:author", open_kind()))
            .unwrap()
            .task_id
    }

    #[test]
    fn deprecated_fields_and_self_supersession_refuse_without_writing() {
        let world = cv_testkit::World::new("task-admission-fields");
        let store = TaskStore::at(world.base.join("tasks"));
        let id = open_task(&store);
        let path = store.events_path();
        let before = fs::read(&path).unwrap();
        let channel = TaskEventKind::Opened {
            title: "old channel".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: Some("old board".into()),
            assignee: None,
        };
        for event in [
            new_event(None, "author", channel),
            new_event(
                Some(&id),
                "author",
                TaskEventKind::Done {
                    observed: None,
                    check: Some(serde_json::json!({"cmd":"old check"})),
                },
            ),
            new_event(Some(&id), "author", TaskEventKind::Superseded { by_task: id.clone() }),
        ] {
            assert!(store.append_agent_event(event).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        assert_eq!(store.replay().unwrap().model.tasks[&id].state, TaskState::Open);
    }

    #[test]
    fn append_then_replay_round_trips() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap();

        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty());
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn cas_rejects_events_the_reducer_refuses() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);

        // Done on an Open task with no live revision is fine…
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Done {
                    observed: None,
                    check: None,
                },
            ))
            .unwrap();
        // …after which nothing else applies, and crucially the log did NOT grow.
        let before = store.replay().unwrap().events.len();
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("event rejected"), "{err}");
        assert_eq!(store.replay().unwrap().events.len(), before);
    }

    /// Validation is all-or-nothing: a note then `done` are ordered; a note then a refused
    /// candidate writes neither. This does not test or promise crash-atomic publication.
    #[test]
    fn batch_validation_writes_all_or_none() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let note = |t: &str| {
            new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: t.into(),
                    session_ref: None,
                },
            )
        };
        let released = new_event(Some(&task), "agent:a", TaskEventKind::Released {});
        let err = store.append_agent_events(vec![note("lost?"), released]).unwrap_err();
        assert!(err.to_string().contains("event rejected"), "{err}");
        assert_eq!(
            store.replay().unwrap().events.len(),
            1,
            "nothing of the refused batch landed"
        );

        let done = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        let out = store.append_agent_events(vec![note("the evidence"), done]).unwrap();
        assert_eq!(out.len(), 2);
        let outcome = store.replay().unwrap();
        let t = &outcome.model.tasks[&task];
        assert_eq!(t.state, TaskState::Done);
        assert_eq!(t.notes.len(), 1);
        assert!(!t.notes[0].post_close, "the note preceded the close");
    }

    /// A retired kind is never written again, however it was constructed — and a log that already
    /// holds one still replays, the retired line folding to nothing.
    #[test]
    fn agent_append_rejects_retired_kinds_but_replay_still_reads_them() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let landed: TaskEventKind = serde_json::from_value(serde_json::json!({
            "event": "landed", "upstream_head": "a".repeat(40), "observed_patch_id": "b".repeat(40)
        }))
        .unwrap();
        let err = store
            .append_agent_event(new_event(Some(&task), "agent:sneaky", landed.clone()))
            .unwrap_err();
        assert!(err.to_string().contains("retired in 0.15.0"), "{err}");
        assert_eq!(store.replay().unwrap().events.len(), 1, "nothing was appended");

        // An old log with the line already in it: read, replayed, state untouched.
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        let old = new_event(Some(&task), "cv-verify", landed);
        writeln!(f, "{}", serde_json::to_string(&old).unwrap()).unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[1].kind.tag(), "landed");
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Open);
    }

    #[test]
    fn concurrent_claims_have_exactly_one_winner() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);

        let n = 8;
        let wins: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let store = store.clone();
                    let task = task.clone();
                    s.spawn(move || {
                        store
                            .append_agent_event(new_event(
                                Some(&task),
                                &format!("agent:{i}"),
                                TaskEventKind::Claimed {
                                    assignee: format!("agent:{i}"),
                                },
                            ))
                            .is_ok()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one claim must win");
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2); // open + the single winning claim
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn trailing_torn_line_is_tolerated_but_interior_garbage_is_loud() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: "hi".into(),
                    session_ref: None,
                },
            ))
            .unwrap();

        // Torn trailing line (no newline): tolerated silently.
        let path = store.events_path();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"trunc").unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2);
        assert!(outcome.warnings.is_empty());

        // Interior garbage (newline-terminated junk followed by a valid event): loud.
        let mut f = OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
        let good = serde_json::to_string(&new_event(None, "agent:a", open_kind())).unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(f, "{good}").unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("line 1"), "{}", outcome.warnings[0]);
    }

    #[test]
    fn append_recovers_torn_tail_and_preserves_completed_batch_prefix() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let note = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "completed evidence 🌙".into(),
                session_ref: None,
            },
        );
        store.append_agent_event(note.clone()).unwrap();
        let path = store.events_path();
        let prefix = fs::read(&path).unwrap();
        let mut torn = prefix.clone();
        torn.extend_from_slice(b"{\"event\":\"done\",\"id\":\"unfinished");
        fs::write(&path, &torn).unwrap();
        let replay = store.replay().unwrap();
        assert!(replay.warnings.is_empty());
        assert_eq!(replay.events.len(), 2);
        assert_eq!(fs::read(&path).unwrap(), torn, "reads never repair the log");

        let done = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        assert_eq!(store.append_agent_event(done.clone()).unwrap(), done);
        let mut expected = prefix;
        expected.extend_from_slice(serde_json::to_string(&done).unwrap().as_bytes());
        expected.push(b'\n');
        assert_eq!(
            fs::read(&path).unwrap(),
            expected,
            "only the excluded EOF suffix was removed"
        );
        let replay = TaskStore::at(store.dir()).replay().unwrap();
        assert!(replay.warnings.is_empty());
        assert_eq!(replay.quarantined, 0);
        assert_eq!(&replay.events[1..], &[note, done]);
        assert_eq!(replay.model.tasks[&task].state, TaskState::Done);
        assert_eq!(replay.model.tasks[&task].notes.len(), 1);
    }

    #[test]
    fn append_separates_valid_unterminated_event_and_header() {
        for header in [false, true] {
            let store = TaskStore::at(tmp_tasks());
            let opened = new_event(None, "agent:a", open_kind());
            let prefix = format!(
                "{} \t{}\r ",
                if header {
                    format!("{HEADER_LINE}\n")
                } else {
                    String::new()
                },
                serde_json::to_string(&opened).unwrap()
            );
            fs::write(store.events_path(), &prefix).unwrap();
            assert_eq!(store.replay().unwrap().events, vec![opened.clone()]);
            let note = new_event(
                Some(&opened.task_id),
                "agent:a",
                TaskEventKind::Noted {
                    text: "still separate".into(),
                    session_ref: None,
                },
            );
            store.append_agent_event(note.clone()).unwrap();
            assert_eq!(
                fs::read_to_string(store.events_path()).unwrap(),
                format!("{prefix}\n{}\n", serde_json::to_string(&note).unwrap())
            );
            let replay = store.replay().unwrap();
            assert!(replay.warnings.is_empty());
            assert_eq!(replay.quarantined, 0);
            assert_eq!(replay.events, vec![opened, note]);
        }
        let store = TaskStore::at(tmp_tasks());
        fs::write(store.events_path(), HEADER_LINE).unwrap();
        let opened = new_event(None, "agent:a", open_kind());
        store.append_agent_event(opened.clone()).unwrap();
        assert_eq!(
            fs::read_to_string(store.events_path()).unwrap(),
            format!("{HEADER_LINE}\n{}\n", serde_json::to_string(&opened).unwrap())
        );
        assert_eq!(store.replay().unwrap().events, vec![opened]);
    }

    #[test]
    fn refused_batch_does_not_repair_or_change_existing_log() {
        // Valid missing newline, torn JSON, incomplete UTF-8, and interior garbage plus a torn
        // suffix all remain byte-identical when a later candidate in the batch is refused.
        for suffix in [
            &b""[..],
            &b"{\"id\":\"torn"[..],
            &b"{\"text\":\"\xf0\x9f\x8c"[..],
            &b"garbage\n{torn"[..],
        ] {
            let store = TaskStore::at(tmp_tasks());
            let task = open_task(&store);
            let path = store.events_path();
            let mut before = fs::read(&path).unwrap();
            if suffix.is_empty() {
                assert_eq!(before.pop(), Some(b'\n'));
            } else {
                before.extend_from_slice(suffix);
            }
            fs::write(&path, &before).unwrap();
            let note = new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: "must not land".into(),
                    session_ref: None,
                },
            );
            let refused = new_event(Some(&task), "agent:a", TaskEventKind::Released {});
            assert!(store.append_agent_events(vec![note, refused]).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(store.replay().unwrap().events.len(), 1);
        }
    }

    #[test]
    fn incomplete_utf8_tail_is_recovered_but_invalid_utf8_is_fail_closed() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let path = store.events_path();
        let prefix = fs::read(&path).unwrap();
        let mut torn = prefix.clone();
        torn.extend_from_slice(b"{\"text\":\"\xf0\x9f\x8c");
        fs::write(&path, torn).unwrap();
        assert!(store.replay().unwrap().warnings.is_empty());
        let note = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "recovered".into(),
                session_ref: None,
            },
        );
        store.append_agent_event(note.clone()).unwrap();
        let mut expected = prefix.clone();
        expected.extend_from_slice(serde_json::to_string(&note).unwrap().as_bytes());
        expected.push(b'\n');
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(store.replay().unwrap().events.len(), 2);

        for invalid in [&b"\xff"[..], &b"{\"text\":\"\xf0\x9f\x8c\n"[..], &b"\xe2\x28\xa1"[..]] {
            let mut before = prefix.clone();
            before.extend_from_slice(invalid);
            fs::write(&path, &before).unwrap();
            assert!(store.replay().is_err());
            assert!(store.append_agent_event(note.clone()).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn unrecognized_complete_or_malformed_final_rows_refuse_append_without_erasure() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let path = store.events_path();
        let prefix = fs::read(&path).unwrap();
        for tail in [
            r#"{"event":"a-future-event","id":"future","payload":{}}"#,
            r#"{"id":42}"#,
            "not json",
            r#"{"id":"invalid",!}"#,
        ] {
            let mut before = prefix.clone();
            before.extend_from_slice(tail.as_bytes());
            fs::write(&path, &before).unwrap();
            let replay = store.replay().unwrap();
            assert_eq!(replay.events.len(), 1);
            assert_eq!(replay.warnings.len(), 1);
            assert!(replay.warnings[0].contains("line 3"));
            let error = store
                .append_agent_event(new_event(
                    Some(&task),
                    "agent:a",
                    TaskEventKind::Noted {
                        text: "must not erase unknown history".into(),
                        session_ref: None,
                    },
                ))
                .unwrap_err();
            assert!(error.to_string().contains("unrecognized final record"), "{error}");
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn concurrent_claims_recover_torn_tail_with_one_winner() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let path = store.events_path();
        let mut before = fs::read(&path).unwrap();
        before.extend_from_slice(b"{\"id\":\"torn");
        fs::write(&path, before).unwrap();
        let wins = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let store = store.clone();
                    let task = task.clone();
                    scope.spawn(move || {
                        store
                            .append_agent_event(new_event(
                                Some(&task),
                                &format!("agent:{i}"),
                                TaskEventKind::Claimed {
                                    assignee: format!("agent:{i}"),
                                },
                            ))
                            .is_ok()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count()
        });
        assert_eq!(wins, 1);
        let replay = store.replay().unwrap();
        assert!(replay.warnings.is_empty());
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.model.tasks[&task].state, TaskState::Claimed);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn a_fold_reads_only_the_events_after_its_mark_and_equals_a_replay() {
        let _world = crate::test_world("task-fold");
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let parsed = || PARSED.with(|n| n.replace(0));
        let same = |store: &TaskStore| {
            let (folded, replayed) = (store.fold().unwrap(), store.replay().unwrap());
            assert_eq!(folded.model, replayed.model);
            assert_eq!(folded.warnings, replayed.warnings);
            assert_eq!(folded.quarantined, replayed.quarantined);
        };
        let task = open_task(&store);
        let note = |text: &str| {
            new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: text.into(),
                    session_ref: None,
                },
            )
        };
        for i in 0..40 {
            store.append_agent_event(note(&format!("note {i}"))).unwrap();
        }
        store.fold().unwrap();
        parsed();
        // One more event: the append reads nothing (the fold stands for the whole log), and the
        // next fold reads that one event.
        let late = store.append_agent_event(note("late")).unwrap();
        assert_eq!(parsed(), 0, "the append read only what followed the fold");
        store.fold().unwrap();
        assert_eq!(parsed(), 1, "the fold read only the appended event");
        assert_eq!(store.fold().unwrap().model.tasks[&task].notes.len(), 41);
        assert_eq!(parsed(), 0, "an unchanged log reads nothing");
        same(&store);
        // An idempotent re-append of an event before the mark is still recognized, and a
        // different event under its id still refuses.
        store.append_agent_event(late.clone()).unwrap();
        let mut forged = late.clone();
        forged.by = "agent:forger".into();
        assert!(store.append_agent_event(forged).is_err());
        // Garbage, a refused event and an unterminated valid event ride the fold as a replay
        // reports them, in a replay's order.
        let refused = new_event(
            Some("00000000-0000-7000-8000-00000000dead"),
            "agent:evil",
            TaskEventKind::Claimed {
                assignee: "agent:evil".into(),
            },
        );
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        writeln!(f, "not an event").unwrap();
        writeln!(f, "{}", serde_json::to_string(&refused).unwrap()).unwrap();
        same(&store);
        writeln!(f, "also not an event").unwrap();
        write!(f, "{}", serde_json::to_string(&note("unterminated")).unwrap()).unwrap();
        drop(f);
        same(&store);
        same(&store);
        // Another file at the path (a repaired log) is not the folded one.
        let body = std::fs::read_to_string(store.events_path()).unwrap();
        let first_lines: String = body.lines().take(3).map(|l| format!("{l}\n")).collect();
        std::fs::remove_file(store.events_path()).unwrap();
        std::fs::write(store.events_path(), first_lines).unwrap();
        same(&store);
        assert_eq!(store.fold().unwrap().model.tasks[&task].notes.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interior_reducer_refused_event_is_quarantined_and_replay_continues() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap();

        // Hand-write a parseable event the reducer refuses (claim on an unknown task), then a
        // valid trailing event — the poisoned line must not take the rest of the log with it.
        let refused = new_event(
            Some("00000000-0000-7000-8000-00000000dead"),
            "agent:evil",
            TaskEventKind::Claimed {
                assignee: "agent:evil".into(),
            },
        );
        let good = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "still here".into(),
                session_ref: None,
            },
        );
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&refused).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&good).unwrap()).unwrap();
        drop(f);

        let outcome = store.replay().unwrap();
        assert_eq!(outcome.quarantined, 1);
        assert_eq!(outcome.events.len(), 3, "open + claim + trailing note all survive");
        assert_eq!(outcome.warnings.len(), 1);
        // Line 1 is the header, so the refused event landed on line 4.
        assert!(
            outcome.warnings[0].contains("line 4") && outcome.warnings[0].contains("quarantined"),
            "{}",
            outcome.warnings[0]
        );
        let t = &outcome.model.tasks[&task];
        assert_eq!(t.state, TaskState::Claimed);
        assert_eq!(t.notes.len(), 1, "events after the quarantined line still reduce");
    }

    #[test]
    fn append_fails_closed_on_degraded_log() {
        // Degraded by a reducer-refused interior event.
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let refused = new_event(
            Some("00000000-0000-7000-8000-00000000dead"),
            "agent:evil",
            TaskEventKind::Claimed {
                assignee: "agent:evil".into(),
            },
        );
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&refused).unwrap()).unwrap();
        drop(f);
        let before = fs::read(store.events_path()).unwrap();
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("refusing to append"), "{err}");
        assert_eq!(fs::read(store.events_path()).unwrap(), before);
        assert_eq!(
            store.replay().unwrap().events.len(),
            1,
            "the refused append wrote nothing"
        );

        // Degraded by interior garbage (a read warning): same refusal.
        let dir2 = tmp_tasks();
        let store2 = TaskStore::at(&dir2);
        let task2 = open_task(&store2);
        let mut f = OpenOptions::new().append(true).open(store2.events_path()).unwrap();
        writeln!(f, "not json at all").unwrap();
        drop(f);
        let before2 = fs::read(store2.events_path()).unwrap();
        let err = store2
            .append_agent_event(new_event(
                Some(&task2),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("refusing to append"), "{err}");
        assert_eq!(fs::read(store2.events_path()).unwrap(), before2);
    }

    #[test]
    fn header_is_written_once_and_skipped_on_read() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap();

        let content = fs::read_to_string(store.events_path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], HEADER_LINE, "first line of a fresh log is the format header");
        assert_eq!(
            content.matches(LOG_FORMAT).count(),
            1,
            "the header is written exactly once, not per append"
        );

        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.events.len(), 2, "the header is not an event");
    }

    #[test]
    fn newer_format_header_refuses_replay_and_append() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let good = serde_json::to_string(&new_event(None, "agent:a", open_kind())).unwrap();
        let mut f = File::create(store.events_path()).unwrap();
        writeln!(f, "{{\"format\":\"cv-task-log\",\"v\":2}}").unwrap();
        writeln!(f, "{good}").unwrap();
        drop(f);

        let err = store.replay().unwrap_err();
        assert!(err.to_string().contains("newer cv"), "{err}");
        let err = store
            .append_agent_event(new_event(None, "agent:a", open_kind()))
            .unwrap_err();
        assert!(err.to_string().contains("newer cv"), "{err}");
    }

    #[test]
    fn replay_equals_live_across_process_restarts() {
        // Simulated restart: two distinct TaskStore values over the same dir.
        let dir = tmp_tasks();
        let task = {
            let store = TaskStore::at(&dir);
            open_task(&store)
        };
        let store2 = TaskStore::at(&dir);
        store2
            .append_agent_event(new_event(
                Some(&task),
                "agent:b",
                TaskEventKind::Claimed {
                    assignee: "agent:b".into(),
                },
            ))
            .unwrap();
        let outcome = store2.replay().unwrap();
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
        assert_eq!(
            outcome.model,
            TaskReducer::reduce(&outcome.events).unwrap(),
            "replay parity"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn state_db_backs_tasks_survives_jsonl_removal_and_honors_frozen() {
        let base = std::env::temp_dir().join(format!("cv-task-statedb-{}", uuid::Uuid::now_v7()));
        let tasks_dir = base.join("tasks");
        let store = TaskStore::at(&tasks_dir);
        assert_eq!(store.state_store().path(), base.join("state.db"));

        // Under `--frozen` on a fresh missing directory, `replay()` and `fold()` succeed with zero
        // filesystem writes, while `append_agent_event` fails closed.
        {
            let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
            assert!(store.replay().unwrap().events.is_empty());
            assert!(store.fold().unwrap().model.tasks.is_empty());
            assert!(!base.exists(), "read-only frozen calls must not create directories");
            assert!(store
                .append_agent_event(new_event(None, "agent:a", open_kind()))
                .is_err());
        }

        let task_id = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task_id),
                "agent:worker",
                TaskEventKind::Claimed {
                    assignee: "agent:worker".into(),
                },
            ))
            .unwrap();
        assert!(base.join("state.db").exists());

        // Even if `events.jsonl` is absent, authoritative `state.db` (`task_events`) replays cleanly!
        fs::remove_file(store.events_path()).unwrap();
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.model.tasks[&task_id].state, TaskState::Claimed);

        // Frozen reads over an existing `state.db` also succeed without mutating disk.
        {
            let _frozen = crate::env::Env::from_vars([("CV_FROZEN", "1")]).enter();
            assert_eq!(store.fold().unwrap().model.tasks[&task_id].state, TaskState::Claimed);
            assert!(store
                .append_agent_event(new_event(Some(&task_id), "agent:worker", TaskEventKind::Released {}))
                .is_err());
        }

        fs::remove_dir_all(&base).ok();
    }
}
