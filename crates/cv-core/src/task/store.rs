//! Durable task event log: a single append-only JSONL file with locked compare-and-swap appends.
//!
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` + sibling `events.lock`, using the same crash-safe
//! flock + `O_APPEND` single-`write_all` recipe as the board (see [`crate::lockfile`]).
//!
//! # Why not a board channel
//!
//! The board's reader deliberately *skips* malformed lines — right for chat, fatal for a replayed
//! event log (a silently dropped `Refuted` would resurrect a dead revision). Here interior
//! garbage is **loud** (surfaced as warnings on every read) and only a *trailing* partial line —
//! the one torn shape the locked writer can produce by crashing mid-append — is tolerated.
//!
//! # The CAS append
//!
//! [`TaskStore::append`] holds the lock across replay-validate-append: acquire → replay the log
//! through the reducer → apply the candidate event (any [`ReduceError`] rejects it) → append →
//! release. This is what makes `claim` a race-free first-writer-wins with zero extra lock kinds,
//! and it guarantees the log never contains an event the reducer would refuse on replay.
//!
//! # Law 1 at the seam
//!
//! [`TaskStore::append_agent_event`] — the only path the CLI verbs and MCP tools use — rejects
//! verifier-only kinds (`MergedLocal`, `Landed`, ...). Only the git verifier calls
//! [`TaskStore::append_verifier_event`]. The reducer itself cannot enforce this (it must replay
//! verifier events); the store seam is where "observed, not attested" becomes mechanical.
//!
//! # Growth & retention (the compaction plan)
//!
//! The log is append-only and currently **unbounded**: every read replays every event ever
//! appended, so read cost grows linearly with history. Fine at fleet-task volumes for a long
//! time; not forever. The planned compaction is **snapshot + tail**: periodically fold the
//! log's stable prefix into a snapshot of the read model, keep the live tail as raw events,
//! and retire the prefix to an archive file — **preserving the audit prefix** (retired events
//! stay on disk and re-verifiable; they just stop being replayed on every read). The sibling
//! plan for the board's chatter (presence beats, routine posts) is segment retirement. Neither
//! is implemented yet; whoever picks this up should keep the CAS append's invariant intact —
//! a snapshot the appender validates against must be exactly the fold of the retired prefix.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::lockfile::FileLock;

use super::model::{TaskEvent, TaskEventKind, VERIFIER_BY};
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
///
/// The optional `token` is the TOFU per-endpoint credential the handle presents on writes: the
/// store's append seam authenticates identity-bearing events against the endpoint bindings
/// ([`super::identity`]) using it. `None` is the solo/human path (unbound endpoints stay trusted).
#[derive(Clone, Debug)]
pub struct TaskStore {
    dir: PathBuf,
    token: Option<String>,
}

impl TaskStore {
    /// The default store at [`super::tasks_dir`], presenting no token.
    pub fn default_store() -> TaskStore {
        TaskStore {
            dir: super::tasks_dir(),
            token: None,
        }
    }

    /// A store rooted at `dir` (testing and embedding), presenting no token.
    pub fn at(dir: impl Into<PathBuf>) -> TaskStore {
        TaskStore {
            dir: dir.into(),
            token: None,
        }
    }

    /// This handle, but presenting `token` on identity-bearing appends (the TOFU credential). The
    /// front-ends build the store with the resolved [`super::token`] before appending.
    pub fn with_token(mut self, token: Option<String>) -> TaskStore {
        self.token = token;
        self
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

    /// Replay the log without holding the lock (readers never block writers; a concurrent append
    /// is either wholly visible or wholly absent thanks to single-`write_all` O_APPEND lines).
    ///
    /// Events the reducer refuses are **quarantined**: skipped with a loud warning (line number +
    /// reason) and counted, so one poisoned interior line degrades the model instead of bricking
    /// the CLI, cvd, and every append at once. Verifier-only events whose `by` is not the
    /// verifier identity get a provenance warning (never a rejection — old logs and tests exist).
    pub fn replay(&self) -> Result<ReplayOutcome> {
        let (raw, mut warnings) = self.read_events()?;
        let path = self.events_path();
        let mut reducer = TaskReducer::new();
        let mut events = Vec::with_capacity(raw.len());
        let mut quarantined = 0usize;
        for (line_no, ev) in raw {
            match reducer.apply(&ev) {
                Ok(()) => events.push(ev),
                Err(e) => {
                    quarantined += 1;
                    warnings.push(format!(
                        "task log {}: line {line_no} quarantined — the reducer refused it ({e}); \
                         event skipped, model is best-effort",
                        path.display()
                    ));
                }
            }
        }
        for ev in &events {
            if ev.kind.is_verifier_only() && ev.by != VERIFIER_BY {
                warnings.push(format!(
                    "task {}: verifier-only event '{}' appended by '{}' — landing state may be \
                     attested, not observed",
                    &ev.task_id[..ev.task_id.len().min(8)],
                    ev.kind.tag(),
                    ev.by
                ));
            }
        }
        Ok(ReplayOutcome {
            model: reducer.into_model(),
            events,
            warnings,
            quarantined,
        })
    }

    /// Agent-facing append: rejects verifier-only kinds (law 1), then CAS-appends.
    pub fn append_agent_event(&self, event: TaskEvent) -> Result<TaskEvent> {
        if event.kind.is_verifier_only() {
            bail!(
                "event kind '{}' is verifier-only: landing state is observed by `cv task verify`, never asserted",
                event.kind.tag()
            );
        }
        self.append(event)
    }

    /// Agent-facing append of several events as ONE unit: every event is validated in order
    /// against the replay under one lock and the lines are written with one `write_all`, so
    /// either all of them land or none does. `cv task done --note-file F` uses it so a lane
    /// cannot lose its evidence by ordering (the note, then the terminal event).
    pub fn append_agent_events(&self, events: Vec<TaskEvent>) -> Result<Vec<TaskEvent>> {
        if let Some(e) = events.iter().find(|e| e.kind.is_verifier_only()) {
            bail!(
                "event kind '{}' is verifier-only: landing state is observed by `cv task verify`, never asserted",
                e.kind.tag()
            );
        }
        self.append_all(events)
    }

    /// Verifier-only append: same CAS, no kind restriction. Callers other than the git verifier
    /// (and its tests) must not use this.
    pub fn append_verifier_event(&self, event: TaskEvent) -> Result<TaskEvent> {
        self.append(event)
    }

    /// The locked CAS: replay → apply candidate → append. Returns the appended event.
    ///
    /// Unlike the read path, the append path is **strict and fails closed on degraded reads**:
    /// any read warning (interior garbage) or reducer-refused line means this binary's model of
    /// the log is incomplete — a stale binary must not write against an incomplete model, so the
    /// append is refused with the problem named instead of being validated against a lie.
    fn append(&self, event: TaskEvent) -> Result<TaskEvent> {
        let mut out = self.append_all(vec![event])?;
        Ok(out.pop().expect("one event in, one out"))
    }

    /// [`Self::append`] for a batch: validate every candidate in order, authenticate every one,
    /// then bind and write them all in one `write_all`.
    fn append_all(&self, events: Vec<TaskEvent>) -> Result<Vec<TaskEvent>> {
        if events.is_empty() {
            return Ok(events);
        }
        fs::create_dir_all(&self.dir).with_context(|| format!("creating tasks dir {}", self.dir.display()))?;
        let _lock = FileLock::acquire(self.lock_path())?;

        // Replay under the lock: the candidate is validated against the exact state it will
        // land after.
        let (raw, warnings) = self.read_events()?;
        if let Some(first) = warnings.first() {
            bail!(
                "refusing to append: task log {} is degraded ({} warning(s); first: {first}) — \
                 repair the log or upgrade cv before writing",
                self.events_path().display(),
                warnings.len()
            );
        }
        let mut reducer = TaskReducer::new();
        for (line_no, ev) in &raw {
            reducer.apply(ev).map_err(|e| {
                anyhow::anyhow!(
                    "refusing to append: task log {} line {line_no} fails replay ({e}) — an \
                     incomplete model must not be written against",
                    self.events_path().display()
                )
            })?;
        }
        for event in &events {
            reducer
                .apply(event)
                .map_err(|e| anyhow::anyhow!("event rejected: {e}"))?;
        }

        // Identity gate (TOFU): after the events are validated but before any is written,
        // authenticate each `by` claim against the endpoint bindings. Runs under the same lock as
        // the append and binding write, so load-decide-bind-append is atomic. Non-identity events
        // proceed untouched; a bound endpoint without the matching token is rejected here, before
        // anything is bound or written.
        let mut binds = Vec::new();
        for event in &events {
            let decision = super::identity::authorize(
                &self.dir,
                &event.by,
                event.kind.is_identity_bearing(),
                self.token.as_deref(),
            )?;
            if let super::identity::Decision::Bind { endpoint, hash } = decision {
                binds.push((endpoint, hash));
            }
        }
        for (endpoint, hash) in &binds {
            super::identity::commit(&self.dir, endpoint, hash)?;
        }

        // First append to a fresh log stamps the format header (older, header-less logs stay
        // valid: readers treat them as implicit v1).
        let path = self.events_path();
        let mut line = String::new();
        if !path.exists() {
            line.push_str(HEADER_LINE);
            line.push('\n');
        }
        for event in &events {
            line.push_str(&serde_json::to_string(event).context("serializing task event")?);
            line.push('\n');
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening task log {}", self.events_path().display()))?;
        f.write_all(line.as_bytes())
            .with_context(|| format!("appending to task log {}", self.events_path().display()))?;
        f.flush()
            .with_context(|| format!("flushing task log {}", self.events_path().display()))?;
        Ok(events)
    }

    /// Read raw events as `(1-based line number, event)` pairs. Interior unparseable lines are
    /// collected as warnings; a trailing partial line (no terminating newline, fails to parse) is
    /// tolerated silently. A format header is skipped when current, and refused loudly when it
    /// declares a version this cv does not read (a log written by a newer cv).
    fn read_events(&self) -> Result<(NumberedEvents, Vec<String>)> {
        let path = self.events_path();
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(e).with_context(|| format!("opening task log {}", path.display())),
        };

        let mut events: Vec<(usize, TaskEvent)> = Vec::new();
        let mut bad: Vec<(usize, String)> = Vec::new(); // (1-based line no, snippet)
        let mut reader = BufReader::new(file);
        let mut raw = String::new();
        let mut line_no = 0usize;
        loop {
            raw.clear();
            let n = reader
                .read_line(&mut raw)
                .with_context(|| format!("reading task log {}", path.display()))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let terminated = raw.ends_with('\n');
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
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
                        continue; // current (or ancient) header: skip
                    }
                    if !terminated {
                        // Trailing torn line: the only shape a crashed locked writer leaves behind.
                        break;
                    }
                    let snippet: String = line.chars().take(80).collect();
                    bad.push((line_no, snippet));
                }
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
        Ok((events, warnings))
    }
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

/// Convenience: agent-facing append to the default store.
pub fn append_agent_event(task_id: Option<&str>, by: &str, kind: TaskEventKind) -> Result<TaskEvent> {
    TaskStore::default_store().append_agent_event(new_event(task_id, by, kind))
}

/// Convenience: verifier append to the default store.
pub fn append_verifier_event(task_id: &str, by: &str, kind: TaskEventKind) -> Result<TaskEvent> {
    TaskStore::default_store().append_verifier_event(new_event(Some(task_id), by, kind))
}

/// Convenience: replay the default store.
pub fn replay() -> Result<ReplayOutcome> {
    TaskStore::default_store().replay()
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
        TaskEventKind::Opened {
            title: "demo".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            assignee: None,
        }
    }

    fn open_task(store: &TaskStore) -> String {
        store
            .append_agent_event(new_event(None, "agent:author", open_kind()))
            .unwrap()
            .task_id
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

    /// A batch lands whole or not at all: a note then a `done` both land; a note then a refused
    /// event writes neither (the note is not left behind as a half-applied unit).
    #[test]
    fn batch_append_is_all_or_nothing() {
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

    #[test]
    fn agent_append_rejects_verifier_only_kinds() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:sneaky",
                TaskEventKind::Landed {
                    upstream_head: "a".repeat(40),
                    observed_patch_id: "b".repeat(40),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("verifier-only"), "{err}");
        // Nothing was appended.
        assert_eq!(store.replay().unwrap().events.len(), 1);
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
}
