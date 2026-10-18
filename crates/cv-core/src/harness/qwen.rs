//! Qwen Code CLI adapter. Qwen Code began as a gemini-cli fork and has two on-disk shapes; both are
//! read:
//!
//! 1. **Session JSONL** (current; verified on 0.25.1) —
//!    `~/.qwen/projects/<sanitized cwd>/chats/<sessionId>.jsonl`, one `ChatRecord` per line
//!    (`packages/core/src/services/chatRecordingService.ts`): `{uuid, parentUuid, sessionId,
//!    timestamp, type, subtype?, cwd, version, gitBranch, provenance, message?: Content, model?,
//!    usageMetadata?, toolCallResult?, systemPayload?, forkedFrom?}` with `type` one of `user`,
//!    `assistant`, `tool_result`, `system`. `message` is a Gemini `Content` (`{role, parts}`: `text`,
//!    `thought`, `functionCall`, `functionResponse`, `inlineData`). Sub-agent transcripts are
//!    `projects/<cwd>/subagents/<parentSessionId>/agent-<agentId>.jsonl` (+ `.meta.json`), the same
//!    record shape. `<sanitized cwd>` is the cwd with every non-alphanumeric byte a dash.
//! 2. **gemini-cli recordings** (the fork's original layout) — `~/.qwen/tmp/<projectHash>/{logs.json,
//!    chats/session-*.json|jsonl, checkpoint-*.json}`, byte-identical to Gemini's: delegated to
//!    [`crate::harness::gemini`] and re-tagged [`Harness::Qwen`].
//!
//! Missing dirs degrade gracefully to empty results.

use super::store::jsonl;
use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::source::{Dependencies, Probe};
use crate::stream::{Checkpoint, Flow, MessageSink, ParseOptions, ADAPTER_LAYER};
use anyhow::Result;
use record::Demand;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub(crate) mod emit;
mod record;
pub use emit::emit;

/// The deferred-tool invoker (`ToolNames.TOOL_CALL`).
const TOOL_CALL: &str = "tool_call";

/// The text part a `UserPromptSubmit` hook's `additionalContext` arrives in, appended to the prompt.
const HOOK_CONTEXT_TAG: &str = "<qwen:user-prompt-submit-context>";

pub struct Qwen {
    /// `~/.qwen/tmp`: the gemini-cli-shaped recordings.
    root: Option<PathBuf>,
    /// `~/.qwen/projects`: the session JSONL.
    projects: Option<PathBuf>,
}

impl Qwen {
    pub fn detect() -> Self {
        match crate::env::home_dir() {
            Some(h) => Qwen::with_home(&h.join(".qwen")),
            None => Qwen {
                root: None,
                projects: None,
            },
        }
    }

    /// Rooted at one Qwen home (`~/.qwen` or a `QWEN_HOME`): reads `<dir>/projects` and `<dir>/tmp`.
    /// For tests and the corpus; never consults the process environment.
    pub fn with_home(qwen_dir: &Path) -> Self {
        let existing = |p: PathBuf| p.exists().then_some(p);
        Qwen {
            root: existing(qwen_dir.join("tmp")),
            projects: existing(qwen_dir.join("projects")),
        }
    }
}

impl Adapter for Qwen {
    fn harness(&self) -> Harness {
        Harness::Qwen
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        deps.files.push(r.path.with_extension("meta.json"));
        if let Some(chats) = r.path.parent().filter(|p| p.file_name().is_some_and(|n| n == "chats")) {
            if let (Some(root), Some(cwd)) = (
                chats.parent().and_then(Path::parent).and_then(Path::parent),
                r.cwd.as_ref().and_then(|p| p.to_str()),
            ) {
                deps.files.push(
                    root.join("tmp")
                        .join(crate::digest::sha256_hex(cwd.as_bytes()))
                        .join("scheduled_tasks.json"),
                );
            }
        }
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.projects.iter().chain(self.root.iter()).cloned().collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Some(projects) = &self.projects {
            paths.extend(
                WalkDir::new(projects)
                    .max_depth(4)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.into_path())
                    .filter(|p| is_session_jsonl(p)),
            );
        }
        if let Some(root) = &self.root {
            // Same per-file scan Gemini uses (shared machinery, re-tagged Qwen), including the
            // filename-derived `checkpoint-<tag>` ids, so discover ids match what `parse`/`stream`
            // produce.
            paths.extend(
                WalkDir::new(root)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.into_path())
                    .filter(|p| is_session_file(p)),
            );
        }
        Ok(crate::par_discovery_flat_map(paths, |path| {
            crate::discover_cache::cached_scan_many(&path, || {
                if is_session_jsonl(&path) {
                    scan_session_jsonl(&path).into_iter().collect()
                } else {
                    crate::harness::gemini::scan_session_file(&path, Harness::Qwen)
                }
            })
        }))
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            if is_session_jsonl(&r.path) {
                let input = jsonl::Read::open(&r.path)?;
                let demand = if opts.spans {
                    Demand::Spans(input.source())
                } else {
                    Demand::Owned
                };
                let s = session_shell(&r.path, sink);
                return replay(&input, s, ReplayState::default(), 0, None, demand, sink, &mut || Ok(()))
                    .map(Option::unwrap);
            }
            // The gemini-cli shape: the streaming machinery (mmap + RawValue for legacy whole-file
            // JSON, line streaming for JSONL recordings) is shared and re-tags the session as Qwen.
            crate::harness::gemini::stream_for(Harness::Qwen, r, opts, sink)
        })
    }

    /// Project ChatRecords are chronological: each complete physical record finalizes all its
    /// emitted messages, including rewind/compression markers. Legacy Gemini replay is excluded.
    fn resume_format(&self) -> Option<u32> {
        Some(1)
    }

    fn resume(
        &self,
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        from: &Checkpoint,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            if !is_session_jsonl(&r.path) {
                return Ok(None);
            }
            let Some(state) = from.get::<ReplayCheckpoint>(ADAPTER_LAYER) else {
                return Ok(None);
            };
            let input = jsonl::Read::open(&r.path)?;
            let demand = if opts.spans {
                Demand::Spans(input.source())
            } else {
                Demand::Owned
            };
            let (s, replay_state) = (state.session, state.replay);
            replay(&input, s, replay_state, from.offset, None, demand, sink, &mut || Ok(()))
        })
    }

    /// The cron jobs `cron_create` persists per project in `tmp/<sha256(cwd)>/scheduled_tasks.json`
    /// ([`scheduled_tasks`]), for a session JSONL.
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        scheduled_tasks(r)
    }
}

/// The project's scheduled tasks that belong to `r`: one `schedule`/`state` row per job. A job
/// names its session only when one owns it (`sessionId`, `qwen serve`); a job the `cron_create`
/// tool made names none, so it belongs to the session that was live when it was created
/// (`createdAt` inside the session's first..last record). The file is per project
/// (`Storage.getGlobalTempDir()/getProjectHash(cwd)`, the sha256 of the cwd), so sub-agent
/// transcripts, which share their parent's window, take none.
fn scheduled_tasks(r: &SessionRef) -> Vec<crate::affordance::Affordance> {
    let chats = r.path.parent();
    if chats.and_then(Path::file_name).is_none_or(|n| n != "chats") {
        return Vec::new();
    }
    let Some(qwen_dir) = chats
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
    else {
        return Vec::new();
    };
    let Some(cwd) = r.cwd.as_ref().and_then(|c| c.to_str()) else {
        return Vec::new();
    };
    let file = qwen_dir
        .join("tmp")
        .join(crate::digest::sha256_hex(cwd.as_bytes()))
        .join("scheduled_tasks.json");
    let read = || {
        std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null)
    };
    if !crate::source::inventory_scoped() {
        // A single session needs one linear filter, not a time index for unrelated owners.
        return task_rows_for_session(read(), r);
    }
    let mut baseline = crate::source::ReadSet::default();
    baseline.record(&file);
    // The inventory's complete read set is this same file, already captured before the
    // affordance pass by source::Probe::dependencies(Qwen). Cache hits require that exact
    // baseline; the enclosing publication receipt revalidates it, so no dynamic guard is lost.
    let Ok((inventory, _reads)) = crate::source::inventory("qwen-scheduled-tasks", &file, &baseline, || {
        let inventory = TaskInventory::from_value(read());
        let retained = inventory.retained_bytes();
        Ok((inventory, baseline.clone(), retained))
    }) else {
        return Vec::new();
    };
    inventory.for_session(r)
}

fn task_affordance(task: &Value) -> crate::affordance::Affordance {
    let text = |key: &str| task.get(key).and_then(Value::as_str).unwrap_or("");
    task_label(
        [text("id"), text("cron"), text("prompt")],
        task.get("recurring").and_then(Value::as_bool) == Some(true),
    )
}
fn task_label(parts: [&str; 3], recurring: bool) -> crate::affordance::Affordance {
    let mut summary = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    if recurring {
        summary.push_str(" · recurring");
    }
    crate::affordance::Affordance::store(
        crate::affordance::Kind::Schedule,
        "scheduled_tasks.json",
        truncate(&summary, 120),
        Vec::new(),
    )
}

fn task_rows_for_session(value: Value, session: &SessionRef) -> Vec<crate::affordance::Affordance> {
    let Value::Array(tasks) = value else { return Vec::new() };
    tasks.into_iter().filter(|task| match task.get("sessionId").and_then(Value::as_str) {
        Some(owner) => owner == session.id,
        None => matches!((task.get("createdAt").and_then(Value::as_i64).and_then(chrono::DateTime::from_timestamp_millis), session.created_at.zip(session.updated_at)),
            (Some(created), Some((from,to))) if from <= created && created <= to),
    }).map(|task| task_affordance(&task)).collect()
}

struct TaskRow {
    parts: [String; 3],
    recurring: bool,
    label: std::sync::OnceLock<crate::affordance::Affordance>,
}
impl TaskRow {
    fn new(task: &Value) -> Self {
        // One extra scalar beyond the label limit preserves ellipsis/empty/delimiter semantics
        // while unrelated giant prompts do not become retained inventory payloads.
        Self {
            parts: ["id", "cron", "prompt"].map(|key| {
                task.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(121)
                    .collect()
            }),
            recurring: task.get("recurring").and_then(Value::as_bool) == Some(true),
            label: std::sync::OnceLock::new(),
        }
    }
    fn label(&self) -> crate::affordance::Affordance {
        self.label
            .get_or_init(|| task_label(self.parts.each_ref().map(|part| part.as_str()), self.recurring))
            .clone()
    }
}
type TaskTime = (chrono::DateTime<chrono::Utc>, usize);
#[derive(Default)]
struct TaskInventory {
    rows: Vec<TaskRow>,
    owners: std::collections::HashMap<String, Vec<usize>>,
    unowned: Vec<TaskTime>,
    time_queried: std::sync::atomic::AtomicBool,
    time_index: std::sync::OnceLock<super::store::selection::Positions>,
}
impl TaskInventory {
    fn from_value(value: Value) -> Self {
        let Value::Array(tasks) = value else {
            return Self::default();
        };
        let mut inventory = Self::default();
        for task in tasks {
            let index = inventory.rows.len();
            if let Some(owner) = task.get("sessionId").and_then(Value::as_str) {
                inventory.owners.entry(owner.to_string()).or_default().push(index);
            } else {
                let Some(created) = task
                    .get("createdAt")
                    .and_then(Value::as_i64)
                    .and_then(chrono::DateTime::from_timestamp_millis)
                else {
                    continue;
                };
                inventory.unowned.push((created, index));
            }
            inventory.rows.push(TaskRow::new(&task));
        }
        inventory
    }
    fn retained_bytes(&self) -> usize {
        let rows = self.rows.iter().fold(
            self.rows.capacity().saturating_mul(std::mem::size_of::<TaskRow>()),
            |bytes, row| {
                // Charge every eventual lazy summary/tool allocation before the cache admits us.
                row.parts.iter().fold(
                    bytes.saturating_add(512 + "scheduled_tasks.json".len()),
                    |bytes, part| bytes.saturating_add(part.capacity()),
                )
            },
        );
        let owners = self.owners.iter().fold(
            self.owners
                .capacity()
                .saturating_mul(std::mem::size_of::<(String, Vec<usize>)>() + 16),
            |bytes, (owner, rows)| {
                bytes
                    .saturating_add(owner.capacity())
                    .saturating_add(rows.capacity().saturating_mul(std::mem::size_of::<usize>()))
            },
        );
        // The second query's position index is charged even while still uninitialized.
        let times = self
            .unowned
            .capacity()
            .saturating_mul(std::mem::size_of::<TaskTime>())
            .saturating_add(self.unowned.len().saturating_mul(std::mem::size_of::<usize>()));
        rows.saturating_add(owners)
            .saturating_add(times)
            .saturating_add(std::mem::size_of::<Self>())
    }
    fn for_session(&self, session: &SessionRef) -> Vec<crate::affordance::Affordance> {
        let owned = self.owners.get(&session.id).map(Vec::as_slice).unwrap_or(&[]);
        let mut selected = owned.to_vec();
        if let Some((from, to)) = session
            .created_at
            .zip(session.updated_at)
            .filter(|(from, to)| from <= to)
        {
            if !self.time_queried.swap(true, std::sync::atomic::Ordering::Relaxed) {
                // First query keeps both subsets in physical file order: a linear filter and
                // ordered merge, including when all tasks match a single scoped CLI session.
                let unnamed = self
                    .unowned
                    .iter()
                    .filter(|(at, _)| from <= *at && *at <= to)
                    .map(|(_, index)| *index);
                let mut left = owned.iter().copied().peekable();
                let mut right = unnamed.peekable();
                selected.clear();
                while let (Some(&a), Some(&b)) = (left.peek(), right.peek()) {
                    selected.push(if a < b {
                        left.next().unwrap()
                    } else {
                        right.next().unwrap()
                    });
                }
                selected.extend(left);
                selected.extend(right);
            } else {
                let times = self.time_index.get_or_init(|| {
                    super::store::selection::Positions::new(self.unowned.len(), |a, b| {
                        self.unowned[a].cmp(&self.unowned[b])
                    })
                });
                let positions = times.range(|i| self.unowned[i].0 < from, |i| self.unowned[i].0 <= to);
                selected.extend(positions.iter().map(|&i| self.unowned[i].1));
                selected.sort_unstable();
            }
        }
        selected.into_iter().map(|index| self.rows[index].label()).collect()
    }
}

/// Whether a file under `~/.qwen/tmp` is one of the gemini-cli shapes: `logs.json`, a chat recording
/// (`chats/…json|jsonl`), or a `checkpoint-*.json`.
fn is_session_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if name == "logs.json" {
        return true;
    }
    if name.starts_with("checkpoint-") && name.ends_with(".json") {
        return true;
    }
    let in_chats = path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("chats"));
    in_chats && matches!(path.extension().and_then(|e| e.to_str()), Some("json") | Some("jsonl"))
}

/// Whether `path` is a session JSONL under `projects/`: `<cwd>/chats/<id>.jsonl` or
/// `<cwd>/subagents/<parent>/agent-<id>.jsonl`.
pub(crate) fn is_session_jsonl(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    let dir_name = |p: Option<&Path>| {
        p.and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
    };
    let parent = path.parent();
    let grand = parent.and_then(Path::parent);
    let in_projects = |p: Option<&Path>| dir_name(p.and_then(Path::parent)).as_deref() == Some("projects");
    match dir_name(parent).as_deref() {
        Some("chats") => in_projects(grand),
        _ => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("agent-")
                && dir_name(grand).as_deref() == Some("subagents")
                && in_projects(grand.and_then(Path::parent))
        }
    }
}

/// The parent session of a sub-agent transcript, from its `subagents/<parent>/` dir.
fn subagent_parent(path: &Path) -> Option<String> {
    let dir = path.parent()?;
    (dir.parent()?.file_name()? == "subagents")
        .then(|| dir.file_name()?.to_str().map(str::to_string))
        .flatten()
}

fn scan_session_jsonl(path: &Path) -> Option<SessionRef> {
    let input = jsonl::Read::open(path).ok()?;
    let mut sink = |_: Message| Flow::Continue;
    let mut s = session_shell(path, &mut sink);
    let mut state = ReplayState::default();
    let mut present = false;
    input
        .lines(0, |line| {
            if let Some(view) = line.view {
                present |= record::scan(view, &mut s, &mut state, !present)?;
            }
            Ok(Flow::Continue)
        })
        .ok()?;
    if !present && s.created_at.is_none() {
        return None;
    }
    let mut r = SessionRef::new(s.id, Harness::Qwen, path.to_path_buf());
    r.cwd = s.cwd;
    r.title = state.title.or(state.first_prompt);
    r.created_at = s.created_at;
    r.updated_at = s.updated_at;
    r.message_count = 0;
    Some(r)
}

/// Cross-record state of one session file's emission.
#[derive(Default, Serialize, Deserialize)]
struct ReplayState {
    session_model: Option<String>,
    first_prompt: Option<String>,
    title: Option<String>,
    /// `functionCall` ids of `agent` calls: their `tool_result` is a sub-agent's return.
    spawn_calls: BTreeSet<String>,
    /// `functionCall` id → the tool's name (unwrapped from `tool_call`), for the results.
    call_names: std::collections::BTreeMap<String, String>,
    skipped: u64,
}

#[derive(Serialize, Deserialize)]
struct ReplayCheckpoint<S = Session, R = ReplayState> {
    session: S,
    replay: R,
}

/// Stream one session JSONL into `sink`, returning the session's metadata.
fn session_shell(path: &Path, sink: &mut dyn MessageSink) -> Session {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let parent = subagent_parent(path);
    let mut s = Session::new(stem, Harness::Qwen);
    s.source_path = Some(path.to_path_buf());
    s.lineage.parent = parent.clone();
    if parent.is_some() {
        // The spawning call/type may change independently of the transcript, including absence.
        let path = path.with_extension("meta.json");
        sink.dependency(&path);
        let meta = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if let Some(meta) = meta {
            let get = |k: &str| meta.get(k).and_then(Value::as_str).map(str::to_string);
            s.lineage.spawned_by_tool_use = get("toolUseId");
            s.lineage.agent_path = get("subagentName").or_else(|| get("agentType"));
        }
    }
    s
}

/// The message count an append-only project transcript had at a byte baseline, including an
/// incomplete final physical record exactly as the whole parser would have accepted it then.
/// Legacy Gemini shapes cannot pin old logical output with a physical byte prefix.
pub(crate) fn count_prefix(r: &SessionRef, len: u64) -> Result<usize> {
    count_prefix_checked(r, len, &mut || Ok(()))
}

pub(crate) fn count_prefix_checked(r: &SessionRef, len: u64, check: &mut dyn FnMut() -> Result<()>) -> Result<usize> {
    anyhow::ensure!(
        is_session_jsonl(&r.path),
        "Qwen recording is not chronological project JSONL"
    );
    let mut count = 0;
    let mut sink = |_: Message| {
        count += 1;
        Flow::Continue
    };
    let input = jsonl::Read::open(&r.path)?;
    let shell = session_shell(&r.path, &mut sink);
    replay(
        &input,
        shell,
        ReplayState::default(),
        0,
        Some(len),
        Demand::Owned,
        &mut sink,
        check,
    )?;
    Ok(count)
}

/// The one Qwen read: records `at..to` (EOF by default) of the captured transcript, each through
/// the record codec, accumulating into `s` and `st` (a checkpoint's state, or a fresh shell at 0).
/// A complete ChatRecord never changes an earlier message: checkpoints are offered only after a
/// finished line, so a valid but unfinished EOF record is replayed from the previous boundary on
/// append. After the sink stops, records still fold their session facts through EOF. `Ok(None)`:
/// `at` lies beyond the file.
#[allow(clippy::too_many_arguments)] // The replay state, its window and the demand.
fn replay(
    input: &jsonl::Read,
    mut s: Session,
    mut st: ReplayState,
    at: u64,
    to: Option<u64>,
    demand: Demand<'_>,
    sink: &mut dyn MessageSink,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Option<Session>> {
    let source = input.source();
    if at > source.len() {
        return Ok(None);
    }
    let to = to.map_or(source.len(), |to| to.min(source.len()));
    let mut stopped = false;
    input.snapshot(at, to, |line| {
        check()?;
        match line.view {
            Some(view) => {
                if record::ingest(view, &mut s, &mut st, demand, stopped, sink)? == Flow::Stop {
                    stopped = true;
                }
            }
            None => st.skipped += u64::from(!line.blank),
        }
        input.check_backing()?;
        let checkpoint = if !stopped && line.complete {
            let wanted = sink.wants_checkpoint(line.end);
            input.check_backing()?;
            wanted
        } else {
            false
        };
        if checkpoint {
            let mut cp = Checkpoint::new(line.end);
            cp.put(
                ADAPTER_LAYER,
                &ReplayCheckpoint {
                    session: &s,
                    replay: &st,
                },
            );
            sink.checkpoint(&mut cp);
            input.check_backing()?;
        }
        Ok(Flow::Continue)
    })?;
    s.model = st.session_model.take();
    s.title = st.title.take().or(st.first_prompt.take());
    super::note_skipped_lines(&mut s, st.skipped);
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;
    use std::fs;

    #[derive(Default)]
    struct Kept {
        messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
    }
    impl MessageSink for Kept {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.push(message);
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), checkpoint.clone()));
        }
    }

    #[test]
    fn growing_call_maps_amortize_production_checkpoints_and_preserve_delayed_results() -> Result<()> {
        use crate::stream::stream_counted;
        let world = crate::test_world("qwen-growing-call-checkpoints");
        let home = world.home.join(".qwen");
        let path = home.join("projects/hash/chats/session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let cadence = 4 * 1024 * 1024; // FTS production cadence, independent of wall-clock timing.
        for calls in [2048, 4096] {
            let mut bytes = String::new();
            for i in 0..calls {
                let name = if i == 0 {
                    "agent".to_string()
                } else {
                    format!("tool-{i}-{}", "x".repeat(4096))
                };
                let row = serde_json::json!({"type":"assistant","uuid":format!("message-{i}"),"model":"model","message":{"parts":[{"functionCall":{"id":format!("call-{i}"),"name":name,"args":{"index":i}}}]}});
                bytes.push_str(&row.to_string());
                bytes.push('\n');
            }
            // Results can arrive after thousands of other calls, and repeat. Discarding old
            // mapping entries would change names and subagent-return classification.
            for id in [0, 1, 0, 1] {
                let row = serde_json::json!({"type":"tool_result","message":{"parts":[{"functionResponse":{"id":format!("call-{id}"),"response":{"output":"delayed"}}}]}});
                bytes.push_str(&row.to_string());
                bytes.push('\n');
            }
            std::fs::write(&path, &bytes)?;
            let adapter = Qwen::with_home(&home);
            let r = SessionRef::new("session", Harness::Qwen, &path);
            let mut full = Kept::default();
            let done = stream_counted(
                &adapter,
                &r,
                &ParseOptions::lazy(),
                &mut full,
                crate::stream::Pass {
                    every: Some(cadence),
                    amortized: true,
                    ..Default::default()
                },
            )?;
            assert!(!full.checkpoints.is_empty());
            let copied = full
                .checkpoints
                .iter()
                .map(|(_, cp)| serde_json::to_vec(cp).map(|v| v.len()))
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .sum::<usize>();
            assert!(
                copied <= 2 * bytes.len(),
                "{calls} calls: cumulative checkpoint bytes={copied}, source bytes={}",
                bytes.len()
            );
            assert_eq!(full.messages.len(), calls + 4);
            assert_eq!(full.messages[calls].kind, MessageKind::SubagentReturn);
            assert_eq!(full.messages[calls + 2].kind, MessageKind::SubagentReturn);
            for index in [0, full.checkpoints.len() - 1] {
                let (count, cp) = &full.checkpoints[index];
                let mut rest = Kept::default();
                let resumed = stream_counted(
                    &adapter,
                    &r,
                    &ParseOptions::lazy(),
                    &mut rest,
                    crate::stream::Pass {
                        from: Some(cp),
                        every: Some(cadence),
                        amortized: true,
                        ..Default::default()
                    },
                )?;
                assert_eq!(
                    serde_json::to_value(&resumed.session)?,
                    serde_json::to_value(&done.session)?
                );
                assert_eq!(
                    resumed.counted.as_ref().unwrap().turns,
                    done.counted.as_ref().unwrap().turns
                );
                assert_eq!(
                    resumed.counted.as_ref().unwrap().counts,
                    done.counted.as_ref().unwrap().counts
                );
                assert_eq!(
                    serde_json::to_value(rest.messages)?,
                    serde_json::to_value(&full.messages[*count..])?
                );
                assert_eq!(
                    rest.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
                    full.checkpoints[index + 1..]
                        .iter()
                        .map(|(_, cp)| cp)
                        .collect::<Vec<_>>()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn checked_resume_keeps_selected_payloads_and_exact_skipped_checkpoint_state() {
        let world = crate::test_world("qwen_selected_resume");
        let home = world.home.join(".qwen");
        let dir = home.join("projects/hash/chats");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let payload = "雪\n\"\\".repeat(20_000);
        let body = format!(
            "bad json\n{}\nbad suffix\n{}\n{}\n{}",
            serde_json::json!({"type":"user","uuid":"p","sessionId":"session","message":{"parts":[{"text":"start"}]}}),
            serde_json::json!({"type":"assistant","uuid":"a","model":"model","message":{"parts":[{"text":payload},{"functionCall":{"id":"call","name":"Read","args":{"payload":payload}}}]}}),
            serde_json::json!({"type":"tool_result","uuid":"r","message":{"parts":[{"functionResponse":{"id":"call","name":"Read","response":{"output":payload}}}]}}),
            serde_json::json!({"type":"user","uuid":"tail","message":{"parts":[{"text":"partial physical tail"}]}})
        );
        std::fs::write(&path, &body).unwrap();
        let adapter = Qwen::with_home(&home);
        let r = SessionRef::new("session", Harness::Qwen, &path);
        let mut full = Kept::default();
        let shell = adapter.stream(&r, &ParseOptions::full(), &mut full).unwrap();
        let (count, checkpoint) = full.checkpoints.iter().find(|(count, _)| *count == 1).unwrap();
        let mut resumed = Kept::default();
        let suffix = adapter
            .resume(&r, &ParseOptions::lazy(), &mut resumed, checkpoint)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(suffix).unwrap(),
            serde_json::to_value(shell).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&resumed.messages).unwrap(),
            serde_json::to_value(&full.messages[*count..]).unwrap()
        );
        assert!(resumed.messages.iter().flat_map(|m| &m.content).any(|b| matches!(
            b,
            Block::Text {
                text: crate::Text::Source(_)
            }
        )));
        assert_eq!(
            resumed.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
            full.checkpoints
                .iter()
                .skip_while(|(_, cp)| cp.offset <= checkpoint.offset)
                .map(|(_, cp)| cp)
                .collect::<Vec<_>>()
        );
        // Truncation, not growth: an append-only log may grow under a retained source.
        std::fs::write(&path, "").unwrap();
        assert!(serde_json::to_value(&resumed.messages).is_err());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn deferred_counts_refill_and_late_metadata_invalidate_the_same_source() -> Result<()> {
        let world = crate::test_world("qwen-count-publication");
        let home = world.home.join(".qwen");
        let path = home.join("projects/p/chats/count.jsonl");
        fs::create_dir_all(path.parent().unwrap())?;
        let raw = "{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"parts\":[{\"text\":\"question\"}]}}\n{\"type\":\"assistant\",\"message\":{\"parts\":[{\"text\":\"answer\"}]}}\n";
        fs::write(&path, raw)?;
        let adapter = Qwen::with_home(&home);
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let reference = crate::sessions_for(Some(Harness::Qwen)).remove(0);
        assert_eq!(reference.message_count, 0);
        let mut sink = crate::stream::CollectSink::default();
        crate::stream::stream_counted(
            &adapter,
            &reference,
            &ParseOptions::facts(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(crate::sessions_for(Some(Harness::Qwen))[0].message_count, 2);
        let late = serde_json::json!({"type":"system","subtype":"custom_title","timestamp":"2026-01-03T00:00:00Z","systemPayload":{"title":"late native title","unused":"x".repeat(100_000)}});
        fs::write(
            &path,
            format!("{raw}{late}\n{{\"timestamp\":\"2025-12-31T00:00:00Z\"}}\n"),
        )?;
        let changed = crate::sessions_for(Some(Harness::Qwen)).remove(0);
        assert_eq!(changed.message_count, 0);
        assert_eq!(changed.title.as_deref(), Some("late native title"));
        assert_eq!(changed.created_at, parse_ts("2025-12-31T00:00:00Z"));
        assert_eq!(changed.updated_at, parse_ts("2026-01-03T00:00:00Z"));
        crate::stream::stream_counted(
            &adapter,
            &changed,
            &ParseOptions::facts(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(crate::sessions_for(Some(Harness::Qwen))[0].message_count, 2);
        Ok(())
    }

    #[test]
    fn visible_presence_preserves_empty_and_timestamp_only_inclusion() -> Result<()> {
        let world = crate::test_world("qwen-presence");
        let path = world.base.join("projects/p/chats/presence.jsonl");
        fs::create_dir_all(path.parent().unwrap())?;
        let cases = [
            (serde_json::json!({"type":"system","subtype":"session_model"}), true),
            (serde_json::json!({"type":"system","subtype":"chat_compression"}), true),
            (serde_json::json!({"type":"system","subtype":"rewind"}), true),
            (
                serde_json::json!({"type":"system","subtype":"custom_title","systemPayload":{"title":"named only"}}),
                false,
            ),
            (
                serde_json::json!({"type":"user","message":{"parts":[{"text":""}]}}),
                false,
            ),
            (
                serde_json::json!({"type":"user","message":{"parts":[{"text":"   "}]}}),
                true,
            ),
            (
                serde_json::json!({"type":"user","message":{"parts":[{"text":"<qwen:user-prompt-submit-context>context"}]}}),
                true,
            ),
            (
                serde_json::json!({"type":"assistant","message":{"parts":[{"functionCall":{"name":"noop","args":{"x":1}}}]}}),
                true,
            ),
            (
                serde_json::json!({"type":"tool_result","message":{"parts":[{"functionResponse":{"response":{"output":""}}}]}}),
                true,
            ),
            (serde_json::json!({"type":"unknown"}), false),
            (
                serde_json::json!({"type":"unknown","timestamp":"2026-01-01T00:00:00Z"}),
                true,
            ),
        ];
        for (row, included) in cases {
            fs::write(&path, format!("{row}\n"))?;
            let found = scan_session_jsonl(&path);
            assert_eq!(found.is_some(), included, "{row}");
            if let Some(reference) = found {
                assert_eq!(reference.message_count, 0);
            }
        }
        Ok(())
    }

    #[test]
    fn continued_metadata_after_stop_checks_source_before_mapped_reuse() -> Result<()> {
        let world = crate::test_world("qwen-stop-source");
        let path = world.base.join("projects/p/chats/source.jsonl");
        fs::create_dir_all(path.parent().unwrap())?;
        let raw = "{\"type\":\"user\",\"sessionId\":\"first\",\"timestamp\":\"2026-01-02T00:00:00Z\",\"message\":{\"parts\":[{\"text\":\"first prompt\"}]}}\n{\"type\":\"ignored\",\"sessionId\":\"last\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n{\"type\":\"ignored\",\"timestamp\":\"2026-01-03T00:00:00Z\"}\n".to_owned() + &"{}\n".repeat(2000);
        let reference = SessionRef::new("source", Harness::Qwen, &path);
        let adapter = Qwen::with_home(&world.base);
        fs::write(&path, &raw)?;
        let mut calls = 0;
        let stopped = adapter.stream(&reference, &ParseOptions::full(), &mut |_: Message| {
            calls += 1;
            Flow::Stop
        })?;
        assert_eq!(calls, 1);
        assert_eq!(stopped.id, "last", "unchanged Stop still folds later metadata");
        assert_eq!(stopped.created_at, parse_ts("2026-01-01T00:00:00Z"));
        assert_eq!(stopped.updated_at, parse_ts("2026-01-03T00:00:00Z"));
        for flow in [Flow::Stop, Flow::Continue] {
            fs::write(&path, &raw)?;
            let mut calls = 0;
            let result = adapter.stream(&reference, &ParseOptions::full(), &mut |_: Message| {
                calls += 1;
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(0)
                    .unwrap();
                flow
            });
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
        Ok(())
    }

    #[test]
    fn compression_callback_cannot_invalidate_fields_needed_for_summary() -> Result<()> {
        let world = crate::test_world("qwen-compression-source");
        let path = world.base.join("projects/p/chats/source.jsonl");
        fs::create_dir_all(path.parent().unwrap())?;
        let row = serde_json::json!({"type":"system","subtype":"chat_compression","timestamp":"2026-01-01T00:00:00Z","systemPayload":{"compressedHistory":[{"parts":[{"text":"summary"}]}]},"ignored":"x".repeat(16384)});
        fs::write(&path, format!("{row}\n"))?;
        let reference = SessionRef::new("source", Harness::Qwen, &path);
        let adapter = Qwen::with_home(&world.base);
        let mut stable = 0;
        adapter.stream(&reference, &ParseOptions::full(), &mut |_: Message| {
            stable += 1;
            Flow::Continue
        })?;
        assert_eq!(stable, 2, "boundary and summary come from the same record");
        let mut calls = 0;
        let result = adapter.stream(&reference, &ParseOptions::full(), &mut |_: Message| {
            calls += 1;
            if calls == 1 {
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(0)
                    .unwrap();
            }
            Flow::Continue
        });
        assert!(result.is_err());
        assert!(calls > 0);
        Ok(())
    }

    #[test]
    fn discovery_demand_matches_the_whole_replay_with_giant_system_payloads() -> Result<()> {
        let world = crate::test_world("qwen-discovery-selected");
        let body = "body λ\n".repeat(512 * 1024);
        let oracle = |path: &Path| -> Result<Option<SessionRef>> {
            let mut messages = 0;
            let mut sink = |_: Message| {
                messages += 1;
                Flow::Continue
            };
            let input = jsonl::Read::open(path)?;
            let shell = session_shell(path, &mut sink);
            let session = replay(
                &input,
                shell,
                ReplayState::default(),
                0,
                None,
                Demand::Owned,
                &mut sink,
                &mut || Ok(()),
            )?
            .unwrap();
            if messages == 0 && session.created_at.is_none() {
                return Ok(None);
            }
            let mut reference = SessionRef::new(session.id, Harness::Qwen, path);
            reference.cwd = session.cwd;
            reference.title = session.title;
            reference.created_at = session.created_at;
            reference.updated_at = session.updated_at;
            reference.message_count = 0;
            Ok(Some(reference))
        };
        let rows = [
            serde_json::json!({"type":"user","sessionId":"s","cwd":"/p","timestamp":"2026-10-13T00:00:00Z","forkedFrom":{"sessionId":"parent"},"message":{"parts":[{"text":body},{"text":"<qwen:user-prompt-submit-context>hook"}]}}),
            serde_json::json!({"type":"assistant","model":"m","message":{"parts":[{"text":body,"thought":true},{"functionCall":{"id":"a","name":"agent","args":{"prompt":body}}}]}}),
            serde_json::json!({"type":"tool_result","message":{"parts":[{"functionResponse":{"id":"a","name":"agent","response":{"output":body}}}]}}),
            serde_json::json!({"type":"system","subtype":"chat_compression","systemPayload":{"info":{"unknown":body},"compressedHistory":[{"parts":[{"text":body}]}]}}),
            serde_json::json!({"type":"system","subtype":"notification","systemPayload":{"text":body}}),
            serde_json::json!({"type":"system","subtype":"goal_state","systemPayload":{"snapshot":{"goal":{"objective":body,"status":"active"}}}}),
            serde_json::json!({"type":"system","subtype":"turn_result","systemPayload":{"error":{"message":body}}}),
            serde_json::json!({"type":"system","subtype":"custom_title","systemPayload":{"title":"selected title λ","ignored":body}}),
        ];
        for suffix in ["chats/s.jsonl", "subagents/parent/agent-child.jsonl"] {
            let path = world.base.join("projects/p").join(suffix);
            fs::create_dir_all(path.parent().unwrap())?;
            let mut bytes = rows
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>()
                .into_bytes();
            bytes.extend_from_slice(b"invalid-json\n\xff\n{\"ignored\":1e999}\n");
            bytes.extend_from_slice(
                "\u{2003}{\"type\":\"user\",\"message\":{\"parts\":[{\"text\":\"partial\"}]}}\u{2003}".as_bytes(),
            );
            bytes.extend_from_slice(
                b"\n{\"type\":\"system\",\"type\":\"user\",\"message\":{\"parts\":[{\"text\":\"duplicate type\"}]}}\n",
            );
            fs::write(&path, bytes)?;
            assert_eq!(
                serde_json::to_value(scan_session_jsonl(&path))?,
                serde_json::to_value(oracle(&path)?)?
            );
            for payload in [
                serde_json::json!({"text":""}),
                serde_json::json!({"text":null,"unused":"fallback"}),
                serde_json::json!(null),
                serde_json::json!({"compressedHistory":[{"parts":[{"text":""},{"text":""}]}]}),
            ] {
                for subtype in [
                    "notification",
                    "chat_compression",
                    "turn_result",
                    "goal_state",
                    "session_model",
                    "custom_title",
                ] {
                    fs::write(
                        &path,
                        serde_json::to_vec(
                            &serde_json::json!({"type":"system","subtype":subtype,"systemPayload":payload}),
                        )?,
                    )?;
                    assert_eq!(
                        serde_json::to_value(scan_session_jsonl(&path))?,
                        serde_json::to_value(oracle(&path)?)?,
                        "{subtype}/{payload}"
                    );
                }
            }
        }
        // No custom title may hide first-prompt differences: title joins, hook partitioning,
        // whitespace-only text, reasoning-only turns and deferred tool identities all matter.
        let path = world.base.join("projects/p/chats/title-cases.jsonl");
        for parts in [
            serde_json::json!([{"text":""},{"text":"  title λ"},{"text":body}]),
            serde_json::json!([{"text":" ".repeat(100_000)},{"text":"tail"}]),
            serde_json::json!([{"text":format!("{}<qwen:user-prompt-submit-context>hook", " ".repeat(100_000))},{"text":body}]),
            serde_json::json!([{"text":body,"thought":true},{"functionCall":{"id":"a","name":"tool_call","args":{"name":"agent","arguments":{"payload":body}}}}]),
        ] {
            let rows = [
                serde_json::json!({"type":"user","message":{"parts":parts}}),
                serde_json::json!({"type":"assistant","message":{"parts":[{"functionCall":{"id":"a","name":"tool_call","args":{"name":"agent","arguments":{"payload":body}}}}]}}),
                serde_json::json!({"type":"tool_result","message":{"parts":[{"functionResponse":{"id":"a","response":{"output":{"nested":body}}}}]}}),
            ];
            fs::write(&path, rows.iter().map(|row| format!("{row}\n")).collect::<String>())?;
            assert_eq!(
                serde_json::to_value(scan_session_jsonl(&path))?,
                serde_json::to_value(oracle(&path)?)?
            );
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/qwen/0.25.1/.qwen");
        for reference in Qwen::with_home(&root).discover()? {
            if !is_session_jsonl(&reference.path) {
                continue;
            }
            assert_eq!(
                serde_json::to_value(scan_session_jsonl(&reference.path))?,
                serde_json::to_value(oracle(&reference.path)?)?
            );
        }
        Ok(())
    }

    #[test]
    fn scoped_task_inventory_revalidates_the_enclosing_dependency_file() -> Result<()> {
        let world = crate::test_world("qwen-task-inventory-source-guard");
        let home = world.home.join(".qwen");
        let path = home.join("projects/hash/chats/session.jsonl");
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, "{}\n")?;
        let mut reference = SessionRef::new("session", Harness::Qwen, &path);
        reference.cwd = Some(PathBuf::from("/project"));
        let file = home
            .join("tmp")
            .join(crate::digest::sha256_hex(b"/project"))
            .join("scheduled_tasks.json");
        fs::create_dir_all(file.parent().unwrap())?;
        fs::write(
            &file,
            serde_json::to_vec(&serde_json::json!([{"id":"first","sessionId":"session"}]))?,
        )?;
        let _scope = crate::source::read_scope();
        let before = crate::source::Probe::default().snapshot(&reference);
        assert_eq!(scheduled_tasks(&reference)[0].summary, "first");
        assert_eq!(scheduled_tasks(&reference)[0].summary, "first");
        #[cfg(feature = "sqlite")]
        {
            crate::catalog::sync(std::slice::from_ref(&reference));
            let source = before.with_reads(&crate::source::ReadSet::default());
            crate::catalog::note_affordances_with_source(&reference, &source, || {
                [(crate::affordance::Kind::Schedule, 1)].into_iter().collect()
            });
            assert_eq!(
                crate::catalog::AffordanceTable::load().counts(&reference).unwrap()[&crate::affordance::Kind::Schedule],
                1
            );
        }
        fs::write(
            &file,
            serde_json::to_vec(&serde_json::json!([{"id":"changed schedule","sessionId":"session"}]))?,
        )?;
        assert!(!crate::source::unchanged(&reference, &before));
        assert_eq!(scheduled_tasks(&reference)[0].summary, "changed schedule");
        #[cfg(feature = "sqlite")]
        assert!(
            crate::catalog::AffordanceTable::load().counts(&reference).is_none(),
            "has:schedule must not reuse counts after a per-project edit"
        );
        fs::remove_file(&file)?;
        assert!(scheduled_tasks(&reference).is_empty());
        fs::write(
            &file,
            serde_json::to_vec(&serde_json::json!([{"id":"recreated","sessionId":"session"}]))?,
        )?;
        assert_eq!(scheduled_tasks(&reference)[0].summary, "recreated");
        Ok(())
    }

    #[test]
    fn scoped_first_task_query_is_linear_and_repeat_initializes_time_index() -> Result<()> {
        let world = crate::test_world("qwen-lazy-task-time-index");
        let home = world.home.join(".qwen");
        let mut reference = SessionRef::new("owner", Harness::Qwen, home.join("projects/hash/chats/session.jsonl"));
        reference.cwd = Some(PathBuf::from("/project"));
        reference.created_at = chrono::DateTime::from_timestamp_millis(10);
        reference.updated_at = chrono::DateTime::from_timestamp_millis(40);
        let file = home
            .join("tmp")
            .join(crate::digest::sha256_hex(b"/project"))
            .join("scheduled_tasks.json");
        fs::create_dir_all(file.parent().unwrap())?;
        let tasks=(0..4096).map(|i|serde_json::json!({
            "id":format!("task-{i}"),"cron":"* * * * *","prompt":"λ".repeat(300),
            "sessionId":if i%3==0 {serde_json::json!("owner")} else if i%3==1 {serde_json::json!("other")} else {Value::Null},
            "createdAt":(4096-i)%97,"recurring":i%7==0,
        })).collect::<Vec<_>>();
        let value = Value::Array(tasks);
        fs::write(&file, serde_json::to_vec(&value)?)?;
        let expected = task_rows_for_session(value, &reference);
        let _scope = crate::source::read_scope();
        let first = scheduled_tasks(&reference);
        assert_eq!(serde_json::to_value(&first)?, serde_json::to_value(&expected)?);
        let mut baseline = crate::source::ReadSet::default();
        baseline.record(&file);
        let (inventory, _) =
            crate::source::inventory::<TaskInventory>("qwen-scheduled-tasks", &file, &baseline, || {
                panic!("warm scoped inventory expected")
            })?;
        assert!(inventory.time_index.get().is_none());
        assert_eq!(
            inventory.rows.iter().filter(|row| row.label.get().is_some()).count(),
            first.len()
        );
        let charged = inventory.retained_bytes();
        let second = scheduled_tasks(&reference);
        assert_eq!(serde_json::to_value(second)?, serde_json::to_value(expected)?);
        assert!(inventory.time_index.get().is_some());
        assert_eq!(
            charged,
            inventory.retained_bytes(),
            "lazy allocations were charged before admission"
        );
        Ok(())
    }

    #[test]
    fn indexed_task_inventory_matches_file_order_owner_and_precise_windows() -> Result<()> {
        use crate::affordance::{Affordance, Kind};
        let tasks = (0..4096)
            .map(|i| {
                serde_json::json!({
                    "id":format!("task-{i}"), "cron":"* * * * *", "prompt":"prompt λ",
                    "sessionId":if i % 3 == 0 { Value::Null } else { Value::String(format!("owner-{}", i % 32)) },
                    "createdAt":(4096-i)%97, "recurring": i%7 == 0,
                })
            })
            .collect::<Vec<_>>();
        let inventory = TaskInventory::from_value(Value::Array(tasks.clone()));
        for owner in ["owner-0", "owner-17", "missing"] {
            for window in [None, Some((0, 96)), Some((13, 49)), Some((49, 13))] {
                let mut reference = SessionRef::new(owner, Harness::Qwen, "irrelevant");
                if let Some((from, to)) = window {
                    // Fractional milliseconds must not accidentally include the lower endpoint.
                    reference.created_at =
                        chrono::DateTime::from_timestamp_millis(from).map(|t| t + chrono::Duration::microseconds(500));
                    reference.updated_at = chrono::DateTime::from_timestamp_millis(to);
                }
                // Frozen old filtering/label recipe, deliberately retaining the physical task
                // sequence instead of consulting either new owner/time index.
                let expected = tasks.iter().filter(|task| match task.get("sessionId").and_then(Value::as_str) {
                    Some(owner) => owner == reference.id,
                    None => matches!((task.get("createdAt").and_then(Value::as_i64).and_then(chrono::DateTime::from_timestamp_millis), reference.created_at.zip(reference.updated_at)),
                        (Some(created),Some((from,to))) if from <= created && created <= to),
                }).map(|task| {
                    let text = |key: &str| task.get(key).and_then(Value::as_str).unwrap_or("");
                    let mut summary = [text("id"),text("cron"),text("prompt")].into_iter()
                        .filter(|text| !text.is_empty()).collect::<Vec<_>>().join(" · ");
                    if task.get("recurring").and_then(Value::as_bool)==Some(true) { summary.push_str(" · recurring"); }
                    Affordance::store(Kind::Schedule, "scheduled_tasks.json", truncate(&summary,120),Vec::new())
                }).collect::<Vec<_>>();
                assert_eq!(
                    serde_json::to_value(task_rows_for_session(Value::Array(tasks.clone()), &reference))?,
                    serde_json::to_value(&expected)?,
                    "standalone {owner}/{window:?}"
                );
                assert_eq!(
                    serde_json::to_value(inventory.for_session(&reference))?,
                    serde_json::to_value(expected)?,
                    "{owner}/{window:?}"
                );
            }
        }
        Ok(())
    }

    /// A job names its owner (`qwen serve`) or, made by `cron_create`, belongs to the session live
    /// when it was created; sub-agent transcripts take none.
    #[test]
    fn scheduled_tasks_go_to_their_owner_or_the_live_session() {
        let home = std::env::temp_dir().join(format!("cv-qwen-cron-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let tmp = home.join("tmp").join(crate::digest::sha256_hex(b"/p"));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            tmp.join("scheduled_tasks.json"),
            serde_json::json!([
                {"id": "own", "cron": "0 9 * * *", "prompt": "a", "sessionId": "s1", "createdAt": 0},
                {"id": "theirs", "cron": "0 9 * * *", "prompt": "b", "sessionId": "s2", "createdAt": 1_500},
                {"id": "made", "cron": "*/5 * * * *", "prompt": "c", "recurring": true, "createdAt": 1_500},
                {"id": "later", "cron": "0 0 * * *", "prompt": "d", "createdAt": 9_000},
            ])
            .to_string(),
        )
        .unwrap();
        let at = |ms| chrono::DateTime::from_timestamp_millis(ms);
        let mut r = SessionRef::new("s1", Harness::Qwen, home.join("projects/-p/chats/s1.jsonl"));
        r.cwd = Some(PathBuf::from("/p"));
        (r.created_at, r.updated_at) = (at(1_000), at(2_000));
        let got: Vec<String> = scheduled_tasks(&r).into_iter().map(|a| a.summary).collect();
        assert_eq!(got, ["own · 0 9 * * * · a", "made · */5 * * * * · c · recurring"]);
        r.path = home.join("projects/-p/subagents/s1/agent-x.jsonl");
        assert!(scheduled_tasks(&r).is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    /// Parse Qwen session text by delegating to the (identical) Gemini parser, then re-tag every
    /// resulting session as [`Harness::Qwen`] — the pure-text mirror of what the adapter's on-disk
    /// paths do, used here to cross-check them.
    fn parse_qwen_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
        let mut sessions = crate::harness::gemini::parse_all_str_for(Harness::Qwen, text, source_path);
        for s in &mut sessions {
            s.harness = Harness::Qwen;
        }
        sessions
    }

    fn fixture(name: &str) -> String {
        // Reuse the Gemini fixtures — Qwen shares the on-disk format.
        let path = format!("{}/tests/fixtures/gemini/{}", env!("CARGO_MANIFEST_DIR"), name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn parses_logs_json_as_qwen() {
        let text = fixture("logs.json");
        let mut sessions = parse_qwen_str(&text, None);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.harness == Harness::Qwen));
        assert_eq!(sessions[0].id, "sess-a");
    }

    #[test]
    fn parses_legacy_chat_recording_as_qwen() {
        let text = fixture("session_legacy.json");
        let sessions = parse_qwen_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.harness, Harness::Qwen);
        assert_eq!(s.id, "9aeb2942-7c46-47b7-aded-13772d4d4e63");
        // The shared machinery types every turn; Qwen-specific facts say "qwen", never "gemini".
        let kinds: Vec<(Role, MessageKind, Origin)> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::Notice, Origin::Harness),
            ]
        );
        let info = &s.messages[4];
        assert_eq!(info.harness_extra(Harness::Qwen).unwrap()["record_type"], "info");
        assert!(info.harness_extra(Harness::Gemini).is_none());
        for m in &s.messages {
            for k in m.extra.keys() {
                assert_eq!(k, "qwen", "flat or misfiled extra key {k:?}");
            }
        }
        assert!(s.messages.iter().all(|m| m.model.is_none()), "one model throughout");
    }

    #[test]
    fn injected_context_and_rewinds_as_qwen() {
        let text = concat!(
            r#"{"sessionId":"q1","projectHash":"h","startTime":"2026-03-01T00:00:00.000Z","lastUpdated":"2026-03-01T00:00:04.000Z","kind":"main"}"#,
            "\n",
            r#"{"id":"u1","timestamp":"2026-03-01T00:00:01.000Z","type":"user","content":"<session_context>\nhi\n</session_context>"}"#,
            "\n",
            r#"{"id":"u2","timestamp":"2026-03-01T00:00:02.000Z","type":"user","content":"<hook_context>\nok\n</hook_context>"}"#,
            "\n",
            r#"{"id":"u3","timestamp":"2026-03-01T00:00:03.000Z","type":"user","content":"do it"}"#,
            "\n",
            r#"{"id":"g1","timestamp":"2026-03-01T00:00:04.000Z","type":"gemini","model":"qwen3-coder","content":"done"}"#,
            "\n",
            r#"{"$rewindTo":"g1"}"#,
            "\n",
            r#"{"id":"g1","timestamp":"2026-03-01T00:00:05.000Z","type":"gemini","model":"qwen3-coder","content":"done, properly"}"#,
            "\n",
        );
        let s = &parse_qwen_str(text, None)[0];
        assert_eq!(s.harness, Harness::Qwen);
        let kinds: Vec<(MessageKind, Origin)> = s.messages.iter().map(|m| (m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (MessageKind::InjectedContext, Origin::Harness),
                (MessageKind::InjectedContext, Origin::Hook),
                (MessageKind::Prompt, Origin::Human),
                (MessageKind::Branch, Origin::Harness),
                (MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[3].harness_extra(Harness::Qwen).unwrap()["rewind_to"], "g1");
        assert_eq!(s.harness_extra(Harness::Qwen).unwrap()["kind"], "main");
        assert_eq!(s.title.as_deref(), Some("do it"));
    }

    #[test]
    fn parses_modern_jsonl_as_qwen() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_qwen_str(&text, None);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].harness, Harness::Qwen);
        assert_eq!(sessions[0].id, "11112222-3333-4444-5555-666677778888");
    }

    #[test]
    fn streams_modern_jsonl_as_qwen() {
        // The native streaming path (shared Gemini machinery) must re-tag the session as Qwen and
        // emit the same messages the pure parser materializes.
        let text = fixture("session_modern.jsonl");
        let dir = std::env::temp_dir().join(format!("cv-qwen-stream-{}", std::process::id()));
        // `chats/` in the path is how the shared machinery recognizes a chat recording.
        let chats = dir.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-test.jsonl");
        fs::write(&path, &text).unwrap();

        let r = SessionRef::new("11112222-3333-4444-5555-666677778888", Harness::Qwen, path);
        let mut sink = crate::stream::CollectSink::default();
        let s = Qwen::detect()
            .stream(&r, &crate::stream::ParseOptions::full(), &mut sink)
            .expect("stream");
        assert_eq!(s.harness, Harness::Qwen);

        let parsed = &parse_qwen_str(&text, None)[0];
        assert_eq!(sink.messages.len(), parsed.messages.len());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_qwen_str("not json", None).is_empty());
        assert!(parse_qwen_str("{}", None).is_empty());
    }

    #[test]
    fn checkpoint_ids_are_filename_derived_and_distinct() {
        // Two checkpoint files must discover under DISTINCT filename-derived ids, and parse must
        // return that same id. (The old discover/parse went through the filename-blind pure parser,
        // which gave every checkpoint the generic id "checkpoint" — colliding across files and
        // disagreeing with the streaming path's "checkpoint-<tag>".)
        let text = fixture("checkpoint.json");
        let dir = std::env::temp_dir().join(format!("cv-qwen-ckpt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("checkpoint-alpha.json");
        let p2 = dir.join("checkpoint-beta%20tag.json");
        fs::write(&p1, &text).unwrap();
        fs::write(&p2, &text).unwrap();

        let refs1 = crate::harness::gemini::scan_session_file(&p1, Harness::Qwen);
        let refs2 = crate::harness::gemini::scan_session_file(&p2, Harness::Qwen);
        assert_eq!(refs1.len(), 1);
        assert_eq!(refs2.len(), 1);
        assert_eq!(refs1[0].id, "checkpoint-alpha");
        assert_eq!(refs2[0].id, "checkpoint-beta tag", "percent-decoded tag");
        assert!(refs1.iter().all(|r| r.harness == Harness::Qwen));

        // parse() (= collect(stream)) agrees with the discovered id and re-tags as Qwen.
        let s = Qwen {
            root: None,
            projects: None,
        }
        .parse(&refs1[0])
        .expect("parse checkpoint");
        assert_eq!(s.id, "checkpoint-alpha");
        assert_eq!(s.harness, Harness::Qwen);
        assert!(!s.messages.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_session_file_detection() {
        assert!(is_session_file(Path::new("/x/tmp/h/logs.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/checkpoint-foo.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/session-1-2.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/session-1-2.jsonl")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/parent/child.jsonl")));
        assert!(!is_session_file(Path::new("/x/tmp/h/other.json")));
        assert!(!is_session_file(Path::new("/x/tmp/h/notes.txt")));
    }

    /// The 0.25.1 session JSONL, as the corpus shows it: a hooked prompt, a deferred `agent` call
    /// through `tool_call`, its sub-agent transcript, a compression, a goal, telemetry, a fork.
    #[test]
    fn reads_session_jsonl_and_subagent_transcripts() {
        let home = std::env::temp_dir().join(format!("cv-qwen-modern-{}", uuid::Uuid::new_v4()));
        let proj = home.join("projects").join("-home-corpus-project");
        fs::create_dir_all(proj.join("chats")).unwrap();
        fs::create_dir_all(proj.join("subagents").join("s1")).unwrap();
        let base = r#""sessionId":"s1","cwd":"/home/corpus/project","version":"0.25.1","gitBranch":"main""#;
        let rec = |uuid: &str, ts: u32, body: &str| {
            format!(
                r#"{{"uuid":"{uuid}","parentUuid":null,{base},"timestamp":"2026-10-11T00:00:{ts:02}.000Z",{body}}}"#
            )
        };
        let lines = [
            rec(
                "u1",
                1,
                r#""type":"user","provenance":"real_user","message":{"role":"user","parts":[{"text":"spawn one"},{"text":"<qwen:user-prompt-submit-context>\nteal\n</qwen:user-prompt-submit-context>"}]}"#,
            ),
            rec(
                "t0",
                2,
                r#""type":"system","subtype":"ui_telemetry","systemPayload":{"uiEvent":{}}}"#,
            ),
            rec(
                "a1",
                3,
                r#""type":"assistant","model":"glm-5.3-flash","message":{"role":"model","parts":[{"text":"hm","thought":true},{"functionCall":{"id":"c1","name":"tool_call","args":{"name":"agent","arguments":{"prompt":"say ok"}}}}]},"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5,"cachedContentTokenCount":40,"thoughtsTokenCount":2}"#,
            ),
            rec(
                "r1",
                4,
                r#""type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"id":"c1","name":"tool_call","response":{"output":"ok"}}}]},"toolCallResult":{"callId":"c1","status":"success"}"#,
            ),
            rec(
                "c1",
                5,
                r#""type":"system","subtype":"chat_compression","systemPayload":{"info":{"originalTokenCount":9,"newTokenCount":7,"triggerReason":"manual"},"compressedHistory":[{"role":"user","parts":[{"text":"<state_snapshot>sum</state_snapshot>"}]}]}"#,
            ),
            rec(
                "g1",
                6,
                r#""type":"system","subtype":"goal_state","systemPayload":{"cause":"create","snapshot":{"goal":{"objective":"say goal","status":"active"}}}"#,
            ),
            rec(
                "a2",
                7,
                r#""type":"assistant","model":"glm-4.7-flash","message":{"role":"model","parts":[{"text":"ok"}]}"#,
            ),
        ];
        fs::write(proj.join("chats").join("s1.jsonl"), lines.join("\n") + "\n").unwrap();
        let fork = rec("u9", 8, r#""type":"user","provenance":"real_user","forkedFrom":{"sessionId":"s0","messageUuid":"u9"},"message":{"role":"user","parts":[{"text":"forked"}]}"#)
            .replace(r#""sessionId":"s1""#, r#""sessionId":"s2""#);
        fs::write(proj.join("chats").join("s2.jsonl"), fork + "\n").unwrap();
        let sub = proj.join("subagents").join("s1");
        fs::write(
            sub.join("agent-general-purpose-c1.jsonl"),
            rec(
                "x1",
                3,
                r#""type":"user","isSidechain":true,"message":{"role":"user","parts":[{"text":"say ok"}]}"#,
            ) + "\n",
        )
        .unwrap();
        fs::write(
            sub.join("agent-general-purpose-c1.meta.json"),
            r#"{"agentId":"general-purpose-c1","toolUseId":"c1","subagentName":"general-purpose"}"#,
        )
        .unwrap();

        let q = Qwen::with_home(&home);
        let mut refs = q.discover().unwrap();
        refs.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["agent-general-purpose-c1", "s1", "s2"]);

        let s = q.parse(&refs[1]).unwrap();
        let kinds: Vec<(Role, MessageKind, Origin)> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::System, MessageKind::InjectedContext, Origin::Hook),
                (Role::Assistant, MessageKind::SubagentSpawn, Origin::Model),
                (Role::Tool, MessageKind::SubagentReturn, Origin::Subagent),
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::System, MessageKind::CompactionSummary, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.title.as_deref(), Some("spawn one"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/home/corpus/project")));
        assert_eq!(s.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(
            s.messages[7].model.as_deref(),
            Some("glm-4.7-flash"),
            "the switch is per message"
        );
        match &s.messages[2].content[1] {
            Block::ToolUse {
                name, namespace, input, ..
            } => {
                assert_eq!(name, "agent");
                assert_eq!(namespace.as_deref(), Some("tool_call"));
                assert_eq!(input.resolve().unwrap()["prompt"], "say ok");
            }
            b => panic!("expected the unwrapped call, got {b:?}"),
        }
        let u = s.messages[2].usage.as_ref().unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.reasoning_tokens),
            (Some(60), Some(40), Some(2))
        );

        let child = q.parse(&refs[0]).unwrap();
        assert_eq!(child.lineage.parent.as_deref(), Some("s1"));
        assert_eq!(child.lineage.spawned_by_tool_use.as_deref(), Some("c1"));
        assert_eq!(child.messages[0].origin, Origin::Subagent);
        assert_eq!(q.parse(&refs[2]).unwrap().lineage.forked_from.as_deref(), Some("s0"));
        fs::remove_dir_all(&home).ok();
    }
}
