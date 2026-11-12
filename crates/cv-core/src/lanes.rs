//! The sub-agent forest as **lanes**: one row per sub-agent with what an orchestrator asks of it —
//! who it is, what it ran on, how long, how much, where it is now, and whether it is parked.
//!
//! [`crate::subagent_tree_of`] lists the forest; this module reads each transcript once (a
//! streamed pass, content inline, usage deduplicated by API `message.id` exactly as
//! `cv stats --tokens` does) and pairs it with two harness-side signals: the parent transcript's
//! `<task-notification>` records (the status Claude Code itself reported for the child) and the
//! child's own `SubagentStop` hook attachment (it stopped; a `SendMessage` resume appends turns
//! after it). The strand class — an agent that stopped with a final text saying it is *waiting*
//! for something that will never wake it — is flagged from those two facts and the text.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::ir::{Block, Message, MessageKind, Role, SessionRef, Usage};
use crate::stream::{Demand, Flow};
use crate::AgentRef;

/// Token totals for one lane, following [`Usage`]'s disjoint convention.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct LaneTokens {
    pub calls: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// `input + cache_read + cache_write + output`.
    pub total: u64,
}

impl LaneTokens {
    fn add(&mut self, u: &Usage) {
        self.calls += 1;
        self.input += u.input_tokens.unwrap_or(0);
        self.cache_read += u.cache_read_tokens.unwrap_or(0);
        self.cache_write += u.cache_creation_tokens.unwrap_or(0);
        self.output += u.output_tokens.unwrap_or(0);
        self.total = self.input + self.cache_read + self.cache_write + self.output;
    }
}

/// Where a lane's `status` string was read from, so a reader knows how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    /// A `Workflow` run's `journal.jsonl` result record (`done` / `partial` / `failed` / …).
    Journal,
    /// The parent transcript's last `<task-notification>` for this agent (`completed` /
    /// `failed` / `killed` / `stopped`).
    TaskNotification,
    /// The child's own `SubagentStop` hook attachment after its last turn (`stopped`).
    SubagentStop,
    /// A `stop` row for this agent in the hook ledger ([`LaneEvent`]) at or after its last turn
    /// (`stopped`) — the same fact as [`StatusSource::SubagentStop`], written by `cv hook lane`
    /// when the transcript has not recorded the attachment (it lags the harness).
    HookLedger,
    /// Nothing says it stopped: the transcript ends mid-work (`running`) — or, with no stop
    /// recorded anywhere, it ends in a final report that has sat untouched for
    /// [`RETURNED_QUIET_SECS`] (`returned`: the lane finished and the harness lost the
    /// notification, typically across a restart).
    Transcript,
}

/// How long a transcript must be quiet after a text-only final turn before `returned` is
/// inferred — long enough that a tool call still being streamed out never reads as a return.
pub const RETURNED_QUIET_SECS: i64 = 600;

/// One row of the hook ledger: what Claude Code's `SubagentStart` / `SubagentStop` hooks told
/// `cv hook lane`, appended as one JSON line to [`ledger_path`] with no model action. The ledger
/// is a side channel for the same facts the transcripts carry; [`lanes_of`] reads it so a lane
/// whose transcript has not yet recorded its stop still reads `stopped`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneEvent {
    pub ts: DateTime<Utc>,
    pub event: LaneEventKind,
    /// The parent session's id (the hook's `session_id`).
    pub session_id: String,
    /// The bare agent id (`agent-` stripped, as `SendMessage` uses it).
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// The parent transcript (`transcript_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    /// The agent's own transcript (`agent_transcript_path`, or derived from the parent's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_transcript_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The head of `last_assistant_message` on a stop (capped by the writer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_text: Option<String>,
}

/// What one lane-ledger row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneEventKind {
    /// A sub-agent started (`SubagentStart`).
    Start,
    /// A sub-agent stopped (`SubagentStop`).
    Stop,
}

/// A lane's status: cv's own three verdicts, or the word a harness notice or a workflow journal
/// recorded, kept as spelled (`completed`, `failed`, `killed`, `stopped`, `done`, `partial`, …).
/// It serializes as that one word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneStatus {
    /// No stop recorded anywhere, and the transcript does not read as finished.
    Running,
    /// Parked: a stop hook (in the transcript or the ledger) fired at or after the last turn.
    Stopped,
    /// Finished with no stop recorded anywhere: the harness lost the notification.
    Returned,
    /// As the harness's notice or the workflow journal recorded it.
    Recorded(String),
}

impl LaneStatus {
    pub fn as_str(&self) -> &str {
        match self {
            LaneStatus::Running => "running",
            LaneStatus::Stopped => "stopped",
            LaneStatus::Returned => "returned",
            LaneStatus::Recorded(word) => word,
        }
    }
}

impl std::fmt::Display for LaneStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for LaneStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// `$CLUSTERVISION_HOME/lanes.jsonl` (default `~/.clustervision/lanes.jsonl`);
/// falls back to the working directory when no cv home can be resolved.
pub fn ledger_path() -> PathBuf {
    crate::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lanes.jsonl")
}

/// Append one stored lane event with `O_APPEND`, without a read-modify-write transaction.
/// `write_all` may retry a short write: readers tolerate torn lines, not transactional append
/// isolation. See [`crate::lockfile`] for stores that require serialized transactions.
pub fn append_lane_event(ev: &LaneEvent) -> std::io::Result<()> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing lanes"));
    }
    use std::io::Write;
    let path = ledger_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(ev).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(line.as_bytes())
}

/// Every row of the ledger, file order. Unparseable lines (a torn append) are skipped. Empty
/// when there is no ledger.
///
/// Kept while the ledger stats as it did when it was parsed: a `--subagents` pass asks once per
/// Codex parent (`hook_forest`), and parsing the whole ledger each time made the fold
/// O(parents × ledger rows).
pub fn read_ledger() -> std::sync::Arc<[LaneEvent]> {
    use crate::source::FileId;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    type Memo = HashMap<PathBuf, (FileId, Arc<[LaneEvent]>)>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    let path = ledger_path();
    let Some(before) = FileId::at(&path) else {
        return Arc::from([]);
    };
    let memo = MEMO.get_or_init(Default::default);
    if let Some((_, rows)) = memo
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&path)
        .filter(|(seen, _)| *seen == before)
    {
        return Arc::clone(rows);
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Arc::from([]);
    };
    let rows: Arc<[LaneEvent]> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<LaneEvent>(l).ok())
        .collect();
    // An append during the read leaves no entry: the next ask parses the ledger as it then stands.
    if FileId::at(&path) == Some(before) {
        memo.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, (before, Arc::clone(&rows)));
    }
    rows
}

/// The ledger's rows indexed for lineage walks: each parent session's rows, and the latest
/// `start` per agent. Built once per ledger state ([`ledger_index`]).
#[derive(Default)]
pub struct LedgerIndex {
    pub rows: std::sync::Arc<[LaneEvent]>,
    by_session: HashMap<String, Vec<usize>>,
    started: HashMap<String, usize>,
    stopped: HashMap<String, DateTime<Utc>>,
}

impl LedgerIndex {
    fn of(rows: std::sync::Arc<[LaneEvent]>) -> Self {
        let mut index = LedgerIndex {
            rows,
            ..Default::default()
        };
        for (at, row) in index.rows.iter().enumerate() {
            index.by_session.entry(row.session_id.clone()).or_default().push(at);
            match row.event {
                LaneEventKind::Start => {
                    index.started.insert(row.agent_id.clone(), at);
                }
                LaneEventKind::Stop => {
                    let ts = index.stopped.entry(row.agent_id.clone()).or_insert(row.ts);
                    *ts = (*ts).max(row.ts);
                }
            }
        }
        index
    }
    /// The positions of the rows naming `session` as their parent, in file order.
    pub fn rows_of(&self, session: &str) -> &[usize] {
        self.by_session.get(session).map_or(&[], Vec::as_slice)
    }
    /// The latest `stop` of `agent`, any parent's.
    pub fn latest_stop(&self, agent: &str) -> Option<DateTime<Utc>> {
        self.stopped.get(agent).copied()
    }
    /// The last `start` row registering `agent`.
    pub fn latest_start(&self, agent: &str) -> Option<&LaneEvent> {
        self.started.get(agent).map(|&at| &self.rows[at])
    }
}

/// [`read_ledger`] indexed, kept while the ledger's parse is: a lineage walk per parent (Codex
/// `hook_forest`) or per touching session (the edit guard) is then a lookup, not a scan of
/// every row.
pub fn ledger_index() -> std::sync::Arc<LedgerIndex> {
    use std::sync::{Arc, Mutex, PoisonError};
    static MEMO: Mutex<Option<Arc<LedgerIndex>>> = Mutex::new(None);
    let rows = read_ledger();
    let mut memo = MEMO.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(index) = memo.as_ref().filter(|index| Arc::ptr_eq(&index.rows, &rows)) {
        return Arc::clone(index);
    }
    let index = Arc::new(LedgerIndex::of(rows));
    *memo = Some(Arc::clone(&index));
    index
}

/// The latest `stop` per bare agent id among the rows `keep` admits — what [`status_of`]
/// consults. The parent scope keeps one parent's rows; the forest scope keeps the lanes' own.
fn ledger_stops<'a>(
    rows: impl IntoIterator<Item = &'a LaneEvent>,
    mut keep: impl FnMut(&LaneEvent) -> bool,
) -> HashMap<String, DateTime<Utc>> {
    let mut m: HashMap<String, DateTime<Utc>> = HashMap::new();
    for r in rows.into_iter().filter(|r| r.event == LaneEventKind::Stop && keep(r)) {
        let e = m.entry(r.agent_id.clone()).or_insert(r.ts);
        *e = (*e).max(r.ts);
    }
    m
}

/// How a harness's hook rows in the lane ledger name the lanes whose `stop` counts for a parent
/// ([`crate::harness::Capabilities::lane_stops`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopScope {
    /// Rows whose `session_id` is the parent: its direct sub-agents.
    Parent,
    /// Rows whose `agent_id` is any member of the parent's forest, at any depth (a descendant's
    /// row names its own parent).
    Forest,
}

/// Where a sub-agent transcript ends: the timestamp of its last conversational record (`user` or
/// `assistant`) and of its last `SubagentStop` hook attachment. Claude Code appends a
/// `SubagentStop` attachment each time the agent stops; a `SendMessage` resume appends new turns
/// after it. So `stopped_at > last_turn_at` means the agent is parked — finished or waiting —
/// and `last_turn_at > stopped_at` (or no stop at all) means it is running, or died without the
/// hook firing (a harness restart). Keeps timing and error facts, discarding ordinary content after each record.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SubagentEnd {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
    /// The last API-error notice (`isApiErrorMessage`) with no real turn after it — what killed
    /// the agent, if anything did. A resume clears it (the next real record is a new turn).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_error: Option<ApiErrorNotice>,
}

/// The harness-side status of one sub-agent, as the parent's own records report it
/// (`completed` / `failed` / `killed` / `stopped`, Claude's `<task-notification>`), with the
/// notice's timestamp. A notice fires each time the agent stops, so this is the LAST one per
/// agent id ([`crate::harness::Adapter::task_notices`]).
#[derive(Debug, Clone, Serialize)]
pub struct TaskNotice {
    pub status: String,
    pub ts: Option<DateTime<Utc>>,
}

/// One of Claude Code's synthetic API-error notices, as the transcript records it: the notice
/// text (`You've hit your session limit · resets 9pm (America/New_York)`, `Prompt is too long`),
/// the `error` class (`rate_limit`, `invalid_request`), the HTTP status when present, and the
/// quota's reset instant (`quotaLimits.resetsAt`, epoch seconds) for a rate limit.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ApiErrorNotice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

impl SubagentEnd {
    /// Parked (a `SubagentStop` fired after the last turn)? `None` when the transcript has no
    /// stop record at all — which is *also* what a killed harness leaves behind, so the caller
    /// pairs this with the parent's `<task-notification>` status before saying "running".
    pub fn stopped(&self) -> Option<bool> {
        let stop = self.stopped_at?;
        Some(self.last_turn_at.is_none_or(|t| stop >= t))
    }
}

/// One sub-agent, summarized.
#[derive(Debug, Clone, Serialize)]
pub struct Lane {
    /// The bare `agentId` (the `agent-` prefix stripped) — what `SendMessage` and the journal use.
    pub agent_id: String,
    /// The transcript's session id (`agent-<agent_id>`), as `cv show` resolves it.
    pub session_id: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// The last conversational record (`user`/`assistant`) in the transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub messages: usize,
    pub tool_calls: u64,
    pub tokens: LaneTokens,
    /// `running` · `completed` · `stopped` · `failed` · `killed` · `returned` (finished, no stop
    /// recorded — a lost notification), or a journaled workflow status.
    pub status: LaneStatus,
    pub status_source: StatusSource,
    /// The last assistant text turn — the return value for a finished agent, the last narration
    /// for a running one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_text: Option<String>,
    /// The last tool call (`Bash · cargo build …`): where a running or dead agent was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// Parked on a promise nothing will keep: stopped, and its last text says it is waiting.
    pub stranded: bool,
    /// Why a `failed` / `killed` lane died, read from its transcript's last API-error notice:
    /// `rate-limited` (an API 429 / session or usage limit — resume after the reset),
    /// `context` (prompt too long — relaunch from its clone, it cannot be resumed), or `stopped`
    /// (no error; the harness's own stop hook fired). `None` = undetermined: plain `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_cause: Option<String>,
    /// For `rate-limited`: when the quota resets (`quotaLimits.resetsAt`), when the record says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// For `rate-limited` / `context`: the notice's own text (`You've hit your session limit ·
    /// resets 9pm (America/New_York)`, `Prompt is too long`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    /// The task-store endpoint this lane acts as (`lane:<name>`): the first `CV_ENDPOINT=…` its
    /// own shell commands (a tool input's `command`) exported, else (after [`attach_tasks`]) a
    /// `lane:<slug>` assignee matching the description's leading token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Where `endpoint` came from: `transcript` (exact) or `description` (a guess by name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_source: Option<EndpointSource>,
    /// The tasks the endpoint holds (assignee), newest activity first — present only when joined
    /// ([`attach_tasks`]; `cv lanes --tasks`, the serve page's Lanes pane).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<LaneTask>>,
}

/// How a lane's task-store endpoint was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSource {
    /// The lane's own shell command exported `CV_ENDPOINT=…` (first occurrence) — exact.
    Transcript,
    /// No export seen; the description's leading token matched a `lane:<slug>` assignee.
    Description,
}

/// One task a lane's endpoint holds, as the lane table shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaneTask {
    pub id: String,
    pub title: String,
    pub state: String,
    /// The first line of the task's last note, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_note: Option<String>,
    pub last_ts: DateTime<Utc>,
}

/// The description's leading token as a lane slug: `FIX-KICK: a kick ends authority…` →
/// `fix-kick`. `None` for a description with no plausible token.
pub fn description_slug(description: &str) -> Option<String> {
    let token: String = description
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    (token.len() >= 2).then(|| token.to_lowercase())
}

/// Join each lane to the task store: resolve its endpoint (the transcript's export, else a
/// `lane:<slug>` assignee matching the description's leading token, case-insensitively) and list
/// the tasks that endpoint holds, newest activity first.
pub fn attach_tasks(lanes: &mut [Lane], model: &crate::task::TaskReadModel) {
    if lanes.is_empty() {
        return;
    }
    // Index the task roster once instead of scanning and case-folding it for every lane.
    // A description match historically chose the lexically first spelling when several
    // assignees differed only in case. Keep that choice separate from exact actor identity.
    let mut assignees: HashMap<String, &str> = HashMap::new();
    let mut tasks: HashMap<&str, Vec<&crate::task::TaskProjection>> = HashMap::new();
    for task in model.tasks.values() {
        if let Some(assignee) = task.assignee.as_deref() {
            assignees
                .entry(assignee.to_lowercase())
                .and_modify(|existing| *existing = (*existing).min(assignee))
                .or_insert(assignee);
            tasks
                .entry(crate::task::project::canonical_actor(assignee))
                .or_default()
                .push(task);
        }
    }
    for group in tasks.values_mut() {
        // Stable sorting retains task-ID order for equal timestamps, as the original
        // per-lane scan over the model's BTreeMap did.
        group.sort_by_key(|task| std::cmp::Reverse(task.last_ts));
    }
    for lane in lanes.iter_mut() {
        if lane.endpoint.is_none() {
            if let Some(slug) = lane.description.as_deref().and_then(description_slug) {
                let want = format!("lane:{slug}");
                if let Some(hit) = assignees.get(&want) {
                    lane.endpoint = Some(hit.to_string());
                    lane.endpoint_source = Some(EndpointSource::Description);
                }
            }
        }
        let Some(endpoint) = lane.endpoint.as_deref() else {
            lane.tasks = Some(Vec::new());
            continue;
        };
        let rows: Vec<LaneTask> = tasks
            .get(crate::task::project::canonical_actor(endpoint))
            .into_iter()
            .flatten()
            .map(|t| LaneTask {
                id: t.task_id.clone(),
                title: t.title.clone(),
                state: crate::task::effective_display(t),
                last_note: t.notes.last().map(|n| {
                    n.text
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("")
                        .to_string()
                }),
                last_ts: t.last_ts,
            })
            .collect();
        lane.tasks = Some(rows);
    }
}

impl Lane {
    /// Finished for real: a terminal status AND not stranded. A stranded lane is `completed` as
    /// far as the harness knows — that is exactly the trap — so it never counts as done here.
    pub fn is_done(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "done" | "returned") && !self.stranded
    }

    /// Finished with no stop recorded anywhere: the harness lost this lane's notification.
    pub fn lost_notification(&self) -> bool {
        self.status == LaneStatus::Returned
    }

    /// Any activity (start or last turn) at or after `since`.
    pub fn active_since(&self, since: DateTime<Utc>) -> bool {
        self.started_at.is_some_and(|t| t >= since) || self.last_turn_at.is_some_and(|t| t >= since)
    }

    pub fn is_running(&self) -> bool {
        self.status == LaneStatus::Running
    }
}

/// The phrases that mark a final text as a parked promise. Each is matched case-insensitively
/// against the tail of the last assistant text. They are the shapes that stranded four lanes on
/// one day (`Waiting on notifications`, `I'll continue when the monitor fires`, `waiting for the
/// … verdict`), generalized just enough to catch their siblings and no further.
const STRAND_PATTERNS: &[&str] = &[
    r"waiting (on|for) ([\w'’-]+ ){0,5}(notification|notifications|monitor|verdict|verdicts|build|builds|result|results|lane|lanes|run|job|report|reply|answer)\b",
    r"continue (when|once|after) (the |that |this |it )?\w*( \w+){0,2} ?(fires|lands|arrives|finishes|completes|returns|comes back|is ready|reports)\b",
    r"\b(i.?ll|i will|will) (continue|resume|pick (this|it|that) (back )?up|check back|follow up|report back|proceed) (when|once|after|as soon as)\b",
    r"\b(parked|paused|standing by|on hold) (until|for|pending)\b",
    r"\bawaiting (the |a )?(notification|notifications|verdict|result|results|monitor|build|reply|answer)\b",
    r"\b(when|once|after) the (monitor|notification|watcher|timer) fires\b",
];

fn strand_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        let joined = STRAND_PATTERNS
            .iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|");
        Regex::new(&format!("(?i){joined}")).expect("strand patterns compile")
    })
}

/// Does a final text read as a parked promise? A promise is how a parked text *ends*, so only
/// its last two sentences are consulted — a report that mentions waiting in its middle and then
/// concludes is not stranded — and past-tense narration (`was waiting on … earlier`) is removed
/// before matching, since the regex engine has no lookbehind to exclude it in place.
pub fn text_is_waiting(text: &str) -> bool {
    let sentences: Vec<&str> = text
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let start = sentences.len().saturating_sub(2);
    let tail = sentences[start..].join(". ");
    static PAST: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let past = PAST.get_or_init(|| {
        Regex::new(r"(?i)\b(was|were|had been|have been|has been) (waiting|standing by|parked)\b").unwrap()
    });
    let present = past.replace_all(&tail, "");
    strand_regex().is_match(&present)
}

/// Classify a dead lane's cause from its transcript's last uncleared API-error notice (see
/// [`Lane::failure_cause`]). Only `failed` / `killed` lanes get a cause; everything else is `None`.
pub fn failure_cause(status: &str, end: &SubagentEnd) -> Option<&'static str> {
    if !matches!(status, "failed" | "killed") {
        return None;
    }
    match &end.api_error {
        Some(e) => classify_api_error(e),
        None if end.stopped() == Some(true) => Some("stopped"),
        None => None,
    }
}

/// `rate-limited` / `context` from one API-error notice, else `None`.
pub fn classify_api_error(e: &ApiErrorNotice) -> Option<&'static str> {
    let text = e.text.to_lowercase();
    let error = e.error.as_deref().unwrap_or("");
    if error == "rate_limit"
        || e.status == Some(429)
        || ["session limit", "usage limit", "rate limit", "rate_limit"]
            .iter()
            .any(|p| text.contains(p))
    {
        return Some("rate-limited");
    }
    if [
        "prompt is too long",
        "context length",
        "context window",
        "maximum context",
        "too many tokens",
    ]
    .iter()
    .any(|p| text.contains(p))
    {
        return Some("context");
    }
    None
}

/// One line for a tool call: the tool name plus the argument a reader would look at first.
pub fn tool_summary(name: &str, input: &crate::json::Json) -> Result<String> {
    tool_summary_for(None, name, input)
}

fn tool_summary_for(harness: Option<crate::ir::Harness>, name: &str, input: &crate::json::Json) -> Result<String> {
    input.with_view(|view| crate::tools::classify::compact_label(harness, name, view, 80))
}

fn tool_endpoint(harness: Option<crate::ir::Harness>, name: &str, input: &crate::json::Json) -> Result<Option<String>> {
    input.with_view(|view| {
        if !view.is_string()? {
            for key in ["command", "cmd", "script", "CommandLine"] {
                if let Some(val) = view.get(key)? {
                    if let Some(ep) = endpoint_from(val)? {
                        return Ok(Some(ep));
                    }
                }
            }
        }
        let mut cmds = Vec::new();
        crate::tools::classify::visit_tool_ops(
            harness,
            name,
            view,
            crate::tools::classify::ClassifyOpts::EVENTS,
            |sem| {
                if let crate::tools::classify::ToolOp::Command { cmd: Some(cmd), .. } = sem.op {
                    cmds.push(cmd.into_owned());
                }
            },
        )?;
        for cmd in cmds {
            let val = serde_json::Value::String(cmd);
            if let Some(ep) = endpoint_from(crate::json::JsonView::from(&val))? {
                return Ok(Some(ep));
            }
        }
        Ok(None)
    })
}

/// Locate the existing endpoint grammar without retaining a whole command or an unsuccessful
/// candidate. Replay only the accepted range; a valid endpoint's full value is demanded output.
fn endpoint_from(view: crate::json::JsonView<'_>) -> Result<Option<String>> {
    const KEY: &[u8] = b"CV_ENDPOINT=";
    let kind_char = |b: u8| b.is_ascii_alphanumeric() || b"_.@-".contains(&b);
    let mut key = 0;
    let mut phase = 0;
    let mut start = 0;
    let mut end = 0;
    let mut pos = 0;
    let mut found = None;
    let string = crate::projection::string_until(view, |chunk| {
        for &byte in chunk.as_bytes() {
            if found.is_some() {
                break;
            }
            match phase {
                1 if matches!(byte, b'\"' | b'\'') => phase = 2,
                1 | 2 if kind_char(byte) => {
                    start = pos;
                    phase = 3;
                }
                3 if kind_char(byte) => {}
                3 if byte == b':' => phase = 4,
                4 if kind_char(byte) => {
                    end = pos + 1;
                    phase = 5;
                }
                5 if kind_char(byte) || matches!(byte, b'/' | b':') => end = pos + 1,
                5 => {
                    found = Some((start, end));
                    phase = 0;
                }
                _ => phase = 0,
            }
            key = if byte == KEY[key] {
                key + 1
            } else {
                usize::from(byte == KEY[0])
            };
            if key == KEY.len() {
                if found.is_none() {
                    phase = 1;
                }
                key = 0;
            }
            pos += 1;
        }
        found.is_some()
    })?;
    if !string {
        return Ok(None);
    }
    if found.is_none() && phase == 5 {
        found = Some((start, end));
    }
    let Some((start, end)) = found else { return Ok(None) };
    let mut output = String::with_capacity(end - start);
    let mut offset = 0;
    crate::projection::string_until(view, |chunk| {
        let a = start.saturating_sub(offset).min(chunk.len());
        let b = end.saturating_sub(offset).min(chunk.len());
        if a < b {
            output.push_str(&chunk[a..b]);
        }
        offset += chunk.len();
        offset >= end
    })?;
    Ok(Some(output))
}

/// What one streamed pass over a transcript yields.
#[derive(Default)]
struct Pass {
    model: Option<String>,
    messages: usize,
    tool_calls: u64,
    tokens: LaneTokens,
    last_text: Option<String>,
    last_tool: Option<String>,
    /// The transcript's last conversational turn is an assistant text with no tool call: the
    /// shape of a final report (a running lane ends in a tool call or a tool result).
    ends_in_report: bool,
    /// The first `CV_ENDPOINT=…` a shell command of this lane exported.
    endpoint: Option<String>,
}

fn pass(r: &SessionRef) -> Result<Pass> {
    let adapter = crate::harness::for_harness(r.harness).context("unsupported lane harness")?;
    let mut p = Pass::default();
    let mut seen = std::collections::HashSet::new();
    // Only response identity is needed beyond the ordinary IR. Keep source-backed arguments
    // without unrelated raw result metadata; demanded assistant reports retain their full text.
    let opts = Demand::USAGE.with_spans();
    let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
    let session = adapter.try_stream(
        r,
        &opts,
        &mut crate::stream::TryFnSink::new(|m: Message| {
            if m.is_model_visible() && matches!(m.role, Role::User | Role::Assistant) {
                p.messages += 1;
            }
            if m.role == Role::User && matches!(m.kind, MessageKind::Prompt | MessageKind::ToolResult) {
                p.ends_in_report = false;
            }
            if m.role == Role::Assistant {
                let calls_a_tool = m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
                let report = (m.kind == MessageKind::Reply)
                    .then(|| -> Result<String> {
                        let mut output = String::new();
                        for block in &m.content {
                            if let Block::Text { text } = block {
                                if !output.is_empty() {
                                    output.push('\n');
                                }
                                output.push_str(&text.try_resolve(&resolver)?);
                            }
                        }
                        Ok(output)
                    })
                    .transpose()?
                    .filter(|text| !text.trim().is_empty());
                let has_text = report.is_some();
                if calls_a_tool {
                    p.ends_in_report = false;
                } else if has_text {
                    p.ends_in_report = true;
                }
                if p.model.is_none() {
                    p.model = m.model.clone().filter(|s| !s.is_empty());
                }
                if let Some(u) = m.usage.as_ref().filter(|u| u.total_tokens() > 0) {
                    let dup = crate::stats::usage_key(r.harness, &m).is_some_and(|k| !seen.insert(k));
                    if !dup {
                        p.tokens.add(u);
                    }
                }
                if let Some(report) = report {
                    p.last_text = Some(report);
                }
                for b in &m.content {
                    if let Block::ToolUse { name, input, .. } = b {
                        p.tool_calls += 1;
                        p.last_tool = Some(tool_summary_for(Some(r.harness), name, input)?);
                        // Only the lane's OWN shell commands name its endpoint. An `Agent` prompt
                        // or a `Write`n brief that carries `export CV_ENDPOINT=lane:child` is the
                        // CHILD's export; scanning the whole input joined the parent to it.
                        if p.endpoint.is_none() {
                            p.endpoint = tool_endpoint(Some(r.harness), name, input)?;
                        }
                    }
                }
            }
            Ok(Flow::Continue)
        }),
    )?;
    if p.model.is_none() {
        p.model = session.model.clone();
    }
    Ok(p)
}

/// Resolve the status of one lane from its three possible sources, most authoritative first;
/// with none of them, the transcript's own shape decides between `running` and `returned`.
fn status_of(
    sub: &AgentRef,
    end: &SubagentEnd,
    notice: Option<&TaskNotice>,
    ledger_stop: Option<DateTime<Utc>>,
    ends_in_report: bool,
    now: DateTime<Utc>,
) -> (LaneStatus, StatusSource) {
    if let Some(s) = sub.result_status.as_deref().filter(|s| !s.is_empty()) {
        return (LaneStatus::Recorded(s.to_string()), StatusSource::Journal);
    }
    // A notification that post-dates the last turn describes the current stop; an older one
    // describes a stop the agent has since been resumed from.
    if let Some(n) = notice {
        let current = match (n.ts, end.last_turn_at) {
            (Some(nt), Some(lt)) => nt >= lt,
            (Some(_), None) => true,
            (None, _) => end.stopped() == Some(true),
        };
        if current {
            return (LaneStatus::Recorded(n.status.clone()), StatusSource::TaskNotification);
        }
    }
    if end.stopped() == Some(true) {
        return (LaneStatus::Stopped, StatusSource::SubagentStop);
    }
    // The hook ledger saw the stop before the transcript recorded it (or the harness never
    // wrote the attachment): a stop at or after the last turn parks the lane the same way.
    if ledger_stop.is_some_and(|ts| end.last_turn_at.is_none_or(|lt| ts >= lt)) {
        return (LaneStatus::Stopped, StatusSource::HookLedger);
    }
    // No journal, no notification, no stop hook — but the transcript ends in a final report and
    // has been quiet for a while: the lane returned and nobody recorded it (a harness restart
    // drops the pending notification). One "running" row was a lane that had finished 15 h
    // earlier, found this way.
    let quiet = end
        .last_turn_at
        .is_some_and(|t| now.signed_duration_since(t).num_seconds() >= RETURNED_QUIET_SECS);
    if ends_in_report && quiet {
        return (LaneStatus::Returned, StatusSource::Transcript);
    }
    (LaneStatus::Running, StatusSource::Transcript)
}

fn lane_of(
    sub: AgentRef,
    notices: &HashMap<String, TaskNotice>,
    ledger: &HashMap<String, DateTime<Utc>>,
) -> Result<Lane> {
    let p = pass(&sub.session)?;
    // Only a store that records the end beside the transcript answers; other harnesses' child
    // sessions live inside their own stores (a `.db` path here), where there is nothing to scan.
    let end = crate::harness::rootless(sub.session.harness)
        .map(|adapter| adapter.subagent_end(&sub.session.path))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let agent_id = sub.agent_id().to_string();
    let (status, status_source) = status_of(
        &sub,
        &end,
        notices.get(&agent_id),
        ledger.get(&agent_id).copied(),
        p.ends_in_report,
        Utc::now(),
    );
    let started_at = sub.session.created_at;
    let last_turn_at = end.last_turn_at.or(sub.session.updated_at);
    let duration_ms = match (started_at, last_turn_at) {
        (Some(s), Some(e)) if e >= s => Some((e - s).num_milliseconds() as u64),
        _ => None,
    };
    // A workflow agent's journaled summary is its real return; a direct agent's is its last text.
    let last_text = sub.result_summary.clone().or(p.last_text);
    let parked = matches!(status.as_str(), "completed" | "stopped" | "returned");
    let stranded = parked && last_text.as_deref().is_some_and(text_is_waiting);
    let cause = failure_cause(status.as_str(), &end);
    let notice = end
        .api_error
        .as_ref()
        .filter(|_| matches!(cause, Some("rate-limited" | "context")));
    Ok(Lane {
        agent_id,
        session_id: sub.session.id.clone(),
        path: sub.session.path.clone(),
        agent_type: sub.agent_type,
        description: sub.description,
        tool_use_id: sub.tool_use_id,
        workflow: sub.workflow,
        model: p.model,
        started_at,
        last_turn_at,
        duration_ms,
        messages: p.messages,
        tool_calls: p.tool_calls,
        tokens: p.tokens,
        status,
        status_source,
        last_text,
        last_tool: p.last_tool,
        stranded,
        failure_cause: cause.map(str::to_string),
        resets_at: notice.and_then(|n| n.resets_at),
        failure_detail: notice.map(|n| n.text.clone()),
        endpoint_source: p.endpoint.as_ref().map(|_| EndpointSource::Transcript),
        endpoint: p.endpoint,
        tasks: None,
    })
}

/// Every sub-agent of `parent` as a [`Lane`], **launch order** (oldest first — the order the
/// orchestrator issued them, which is how a status table reads). Read failures are explicit so a
/// changing or unreadable source cannot silently remove a lane from the forest.
pub fn lanes_of(parent: &SessionRef) -> Result<Vec<Lane>> {
    let subs = crate::subagent_tree_of(parent)?;
    let notices = crate::harness::rootless(parent.harness)
        .map(|adapter| adapter.task_notices(parent))
        .transpose()?
        .unwrap_or_default();
    // From the ledger's index: a parent's rows, or each lane's latest stop, never a scan of the
    // whole ledger per parent (`cv lanes` asks once per session).
    let index = ledger_index();
    let ledger = match crate::harness::rootless(parent.harness).and_then(|adapter| adapter.capabilities().lane_stops) {
        Some(StopScope::Parent) => ledger_stops(index.rows_of(&parent.id).iter().map(|&at| &index.rows[at]), |_| true),
        Some(StopScope::Forest) => {
            let agents: std::collections::HashSet<_> = subs.iter().map(|sub| sub.agent_id()).collect();
            agents
                .into_iter()
                .filter_map(|agent| index.latest_stop(agent).map(|ts| (agent.to_owned(), ts)))
                .collect()
        }
        None => HashMap::new(),
    };
    let mut lanes = crate::par_filter_map(subs, |s| Some(lane_of(s, &notices, &ledger)))
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    lanes.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.agent_id.cmp(&b.agent_id)));
    Ok(lanes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first `CV_ENDPOINT=<kind>:<name>` a shell command sets (`export CV_ENDPOINT=lane:x; …`,
    /// `CV_ENDPOINT="lane:x" cv task …`). `<name>` never starts with `/`, so a URL
    /// (`CV_ENDPOINT=https://…`) is not an endpoint.
    fn endpoint_in(text: &str) -> Option<String> {
        endpoint_from(crate::json::JsonView::from(&serde_json::json!(text))).unwrap()
    }

    #[test]
    fn streamed_endpoint_is_the_same_across_decoded_chunks_and_sources() {
        let dir = cv_testkit::temp_dir("streamed-lane-endpoint");
        let path = dir.join("command.json");
        let mut cases = vec![
            "CV_ENDPOINT=lane:one".to_string(),
            "CV_ENDPOINT=lane:/bad; CV_ENDPOINT=lane:good".into(),
            "CV_ENDPOINT='lane:one/two:three' tail".into(),
            "CV_ENDPOINT=badCV_ENDPOINT=lane:inner".into(),
            "CV_ENDPOINT=lane:CV_ENDPOINT=other:later".into(),
            "CV_ENDPOINT=\"\"lane:no; CV_ENDPOINT=lane:yes".into(),
            "CV_ENDPOINT={owner}; CV_ENDPOINT=$X; CV_ENDPOINT=lane:real".into(),
            "CV_ENDPOINT=世界:no".into(),
        ];
        for split in 0..20 {
            cases.push(format!(
                "{}CV_ENDPOINT=\"lane:chunk/tail\" done",
                "\t".repeat(65_536 - split)
            ));
        }
        cases.push(format!(
            "CV_ENDPOINT={} CV_ENDPOINT=lane:after-invalid",
            "x".repeat(150_000)
        ));
        let mut frozen = crate::emit::frozen::Frozen::new("lanes-endpoint");
        for (n, text) in cases.into_iter().enumerate() {
            let value = serde_json::json!(text);
            let expected = endpoint_from(crate::json::JsonView::from(&value)).unwrap();
            let encoded = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &encoded).unwrap();
            let source = crate::json::Json::source(&path, 0, encoded.len() as u64).unwrap();
            assert_eq!(source.with_view(endpoint_from).unwrap(), expected);
            frozen.insert(format!("{n:02}"), serde_json::json!(expected));
        }
        frozen.check().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lane_pass_preserves_full_report_while_skipping_unrelated_large_content() {
        let world = crate::test_world("lane-pass-projections");
        let path = world.proj.join("lane.jsonl");
        let giant = "unneeded λ\n".repeat(10_000);
        let records = [
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":giant}}),
            serde_json::json!({"type":"assistant","uuid":"a","message":{"id":"same-response","role":"assistant","usage":{"input_tokens":5,"output_tokens":7},"content":[
                {"type":"thinking","thinking":giant},
                {"type":"tool_use","id":"t","name":"Bash","input":{"command":"CV_ENDPOINT=lane:test; echo ready"}}
            ]}}),
            serde_json::json!({"type":"user","uuid":"r","toolUseResult":{"stdout":giant,"unused":giant},"message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t","content":giant}
            ]}}),
            serde_json::json!({"type":"assistant","uuid":"done","message":{"id":"same-response","role":"assistant","usage":{"input_tokens":50,"output_tokens":70},"content":[
                {"type":"text","text":""},{"type":"text","text":"ready λ"},
                {"type":"text","text":""},{"type":"text","text":"last line"}
            ]}}),
        ];
        let text = records.iter().map(|record| format!("{record}\n")).collect::<String>();
        std::fs::write(&path, text).unwrap();
        let reference = SessionRef {
            id: "lane".into(),
            harness: crate::Harness::Claude,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let adapter = crate::harness::for_harness(reference.harness).unwrap();
        let whole = crate::stream::collect(adapter.as_ref(), &reference).unwrap();
        let summary = pass(&reference).unwrap();
        assert_eq!(summary.last_text, whole.messages.last().unwrap().text());
        assert!(summary.ends_in_report);
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.tokens.calls, 1);
        assert_eq!((summary.tokens.input, summary.tokens.output), (5, 7));
        assert_eq!(summary.endpoint.as_deref(), Some("lane:test"));
        assert_eq!(
            summary.last_tool.as_deref(),
            Some("Bash · CV_ENDPOINT=lane:test; echo ready")
        );
    }

    #[test]
    fn the_three_real_strand_phrasings_match_and_a_report_does_not() {
        for t in [
            "Build is mirrored. Waiting on notifications.",
            "the build is still running; I'll continue when the monitor fires",
            "Both lanes are green. I'm now waiting for the integrator's verdict before touching main.",
            "Nothing else to do here until the hbox build finishes — standing by for the notification.",
        ] {
            assert!(text_is_waiting(t), "should read as waiting: {t:?}");
        }
        for t in [
            "sdk-ts is fixed: npm test goes from 97/106 to 110/110. The work is three commits on sdk-ts-repair.",
            "I was waiting on notifications earlier, but the build landed; the lane is complete and the branch is pushed.",
            "K-RAN works: the kernel re-executes a runner's claimed Nock run and admits the writes only when they match.",
        ] {
            assert!(!text_is_waiting(t), "should NOT read as waiting: {t:?}");
        }
    }

    #[test]
    fn only_the_tail_is_consulted() {
        let mut long = String::from("Waiting on notifications. ");
        long.push_str(&"The lane then did a great deal of work. ".repeat(40));
        long.push_str("Done; branch pushed.");
        assert!(!text_is_waiting(&long));
    }

    #[test]
    fn endpoint_and_slug_are_read_from_the_shapes_lanes_write() {
        assert_eq!(
            endpoint_in("export CV_ENDPOINT=lane:cv-edits; cv task list --repo /x").as_deref(),
            Some("lane:cv-edits")
        );
        assert_eq!(
            endpoint_in(r#"CV_ENDPOINT="lane:fix-kick" cv task claim 01a0"#).as_deref(),
            Some("lane:fix-kick")
        );
        assert_eq!(endpoint_in("cv task inbox ember"), None);
        // A URL is not an endpoint: a `<kind>:` is never followed by `//`.
        assert_eq!(endpoint_in("CV_ENDPOINT=https://example.com/x cv task list"), None);
        assert_eq!(
            endpoint_in("CV_ENDPOINT=lane:a; curl https://example.com").as_deref(),
            Some("lane:a")
        );
        // A format string or a shell variable is not an endpoint: keep looking.
        assert_eq!(
            endpoint_in("export CV_ENDPOINT={owner}; CV_ENDPOINT=$X; CV_ENDPOINT=lane:real").as_deref(),
            Some("lane:real")
        );
        assert_eq!(
            description_slug("FIX-KICK: a kick ends authority").as_deref(),
            Some("fix-kick")
        );
        assert_eq!(description_slug("CV-EDITS").as_deref(), Some("cv-edits"));
        assert_eq!(description_slug(": nothing"), None);
    }

    #[test]
    fn task_join_preserves_actor_aliases_description_choice_and_stable_order() {
        use crate::task::{new_event, TaskEventKind, TaskReducer};
        let actors = [
            Some("lane:alpha"),
            Some("web:lane:ALPHA"),
            Some("lane:ALPHA"),
            Some("lane:ALPHA"),
            Some("orchestrator:ALPHA"),
            None,
        ];
        let events: Vec<_> = actors
            .iter()
            .enumerate()
            .map(|(i, actor)| {
                let mut event = new_event(
                    None,
                    "human",
                    TaskEventKind::Opened {
                        title: format!("task {i}"),
                        body: String::new(),
                        repo: None,
                        issue: None,
                        channel: None,
                        assignee: actor.map(str::to_owned),
                    },
                );
                event.ts = DateTime::from_timestamp(100 + (i % 2) as i64, 0).unwrap();
                event
            })
            .collect();
        let model = TaskReducer::reduce(&events).unwrap();
        let lane = |endpoint: Option<&str>, description: Option<&str>| Lane {
            agent_id: "agent".into(),
            session_id: "session".into(),
            path: PathBuf::new(),
            agent_type: None,
            description: description.map(str::to_owned),
            tool_use_id: None,
            workflow: None,
            model: None,
            started_at: None,
            last_turn_at: None,
            duration_ms: None,
            messages: 0,
            tool_calls: 0,
            tokens: LaneTokens::default(),
            status: LaneStatus::Running,
            status_source: StatusSource::Transcript,
            last_text: None,
            last_tool: None,
            stranded: false,
            failure_cause: None,
            resets_at: None,
            failure_detail: None,
            endpoint: endpoint.map(str::to_owned),
            endpoint_source: endpoint.map(|_| EndpointSource::Transcript),
            tasks: None,
        };
        let mut lanes = vec![
            lane(None, Some("Alpha: work")),
            lane(Some("lane:alpha"), Some("ALPHA")),
            lane(Some("web:lane:ALPHA"), None),
            lane(Some("lane:missing"), None),
            lane(None, None),
        ];
        attach_tasks(&mut lanes, &model);
        assert_eq!(lanes[0].endpoint.as_deref(), Some("lane:ALPHA"));
        assert_eq!(lanes[0].endpoint_source, Some(EndpointSource::Description));
        assert_eq!(lanes[1].endpoint_source, Some(EndpointSource::Transcript));
        for lane in &lanes {
            // Independent original scan oracle: matching actors remains case-sensitive,
            // with the single web: alias and stable newest-first ordering.
            let mut expected: Vec<_> = model
                .tasks
                .values()
                .filter(|task| {
                    lane.endpoint
                        .as_deref()
                        .zip(task.assignee.as_deref())
                        .is_some_and(|(endpoint, actor)| crate::task::same_actor(endpoint, actor))
                })
                .collect();
            expected.sort_by_key(|task| std::cmp::Reverse(task.last_ts));
            assert_eq!(
                lane.tasks
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|task| task.id.as_str())
                    .collect::<Vec<_>>(),
                expected.iter().map(|task| task.task_id.as_str()).collect::<Vec<_>>(),
            );
        }
        assert_eq!(lanes[0].tasks, lanes[2].tasks);
        assert_eq!(lanes[0].tasks.as_ref().unwrap().len(), 3);
        assert_eq!(lanes[1].tasks.as_ref().unwrap().len(), 1);
    }

    /// The record shapes of session 0c315aee's 10-01/02 kills (content redacted to the notice):
    /// the session-limit 429s are `rate-limited` with the quota's reset, the integrator's
    /// `Prompt is too long` is `context`, a resumed lane's old error is history, and a lane that
    /// is not dead gets no cause.
    #[test]
    fn failure_cause_reads_the_last_uncleared_api_error() {
        let rate = ApiErrorNotice {
            ts: None,
            text: "You've hit your session limit · resets 9pm (America/New_York)".into(),
            error: Some("rate_limit".into()),
            status: Some(429),
            resets_at: DateTime::from_timestamp(1_790_902_800, 0),
        };
        let ctx = ApiErrorNotice {
            text: "Prompt is too long".into(),
            error: Some("invalid_request".into()),
            ..ApiErrorNotice::default()
        };
        let end = |e: Option<ApiErrorNotice>| SubagentEnd {
            api_error: e,
            ..SubagentEnd::default()
        };
        assert_eq!(failure_cause("failed", &end(Some(rate.clone()))), Some("rate-limited"));
        assert_eq!(failure_cause("killed", &end(Some(ctx.clone()))), Some("context"));
        assert_eq!(failure_cause("completed", &end(Some(rate))), None, "not dead: no cause");
        assert_eq!(
            failure_cause("failed", &end(None)),
            None,
            "nothing says why: plain failed"
        );
        let other = ApiErrorNotice {
            text: "API Error: 500 internal".into(),
            ..ApiErrorNotice::default()
        };
        assert_eq!(failure_cause("failed", &end(Some(other))), None);
        let stopped = SubagentEnd {
            stopped_at: DateTime::from_timestamp(10, 0),
            last_turn_at: DateTime::from_timestamp(5, 0),
            api_error: None,
        };
        assert_eq!(failure_cause("failed", &stopped), Some("stopped"));
        // The usage-limit phrasing without a status is still a rate limit.
        let usage = ApiErrorNotice {
            text: "Claude AI usage limit reached|1759370400".into(),
            ..ApiErrorNotice::default()
        };
        assert_eq!(classify_api_error(&usage), Some("rate-limited"));
    }

    /// The ledger is consulted after the transcript's own stop and before the running/returned
    /// guess: a `stop` at or after the last turn parks the lane; an older one is history (the
    /// lane was resumed since); and a transcript that recorded its own stop keeps that source.
    #[test]
    fn a_ledger_stop_parks_a_lane_the_transcript_has_not_stopped() {
        let sub = AgentRef {
            session: SessionRef {
                id: "agent-a1".into(),
                harness: crate::ir::Harness::Claude,
                path: PathBuf::from("/x/agent-a1.jsonl"),
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: None,
            },
            agent_type: None,
            description: None,
            tool_use_id: None,
            workflow: None,
            result_status: None,
            result_summary: None,
        };
        let now = DateTime::from_timestamp(1_000, 0).unwrap();
        let running = SubagentEnd {
            last_turn_at: DateTime::from_timestamp(100, 0),
            stopped_at: None,
            api_error: None,
        };
        assert_eq!(
            status_of(&sub, &running, None, None, false, now),
            (LaneStatus::Running, StatusSource::Transcript)
        );
        assert_eq!(
            status_of(&sub, &running, None, DateTime::from_timestamp(100, 0), false, now),
            (LaneStatus::Stopped, StatusSource::HookLedger)
        );
        assert_eq!(
            status_of(&sub, &running, None, DateTime::from_timestamp(50, 0), false, now),
            (LaneStatus::Running, StatusSource::Transcript),
            "a stop before the last turn is a stop the lane was resumed from"
        );
        let stopped = SubagentEnd {
            stopped_at: DateTime::from_timestamp(120, 0),
            ..running.clone()
        };
        assert_eq!(
            status_of(&sub, &stopped, None, DateTime::from_timestamp(130, 0), false, now).1,
            StatusSource::SubagentStop
        );
        // The reducer keeps the newest stop per agent, bare id.
        let row = |ts: i64, event: LaneEventKind| LaneEvent {
            ts: DateTime::from_timestamp(ts, 0).unwrap(),
            event,
            session_id: "s".into(),
            agent_id: "a1".into(),
            agent_type: None,
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            last_text: None,
        };
        let mut unrelated = row(200, LaneEventKind::Stop);
        unrelated.session_id = "another-parent".into();
        let stops = ledger_stops(
            &[
                row(10, LaneEventKind::Start),
                row(20, LaneEventKind::Stop),
                row(30, LaneEventKind::Stop),
                row(5, LaneEventKind::Stop),
                unrelated,
            ],
            |r| r.session_id == "s",
        );
        assert_eq!(stops.get("a1").copied(), DateTime::from_timestamp(30, 0));
        assert!(
            ledger_stops(&[row(300, LaneEventKind::Stop)], |r| r.session_id == "another-parent").is_empty(),
            "another parent's rows are not this parent's stops"
        );
    }

    #[test]
    fn tool_summary_picks_the_telling_argument() {
        assert_eq!(
            tool_summary(
                "Bash",
                &serde_json::json!({"command": "cargo build", "timeout": 5}).into()
            )
            .unwrap(),
            "Bash · cargo build"
        );
        assert_eq!(
            tool_summary("Read", &serde_json::json!({"file_path": "/x/y.rs"}).into()).unwrap(),
            "Read · /x/y.rs"
        );
        assert_eq!(
            tool_summary("TaskStop", &serde_json::json!({}).into()).unwrap(),
            "TaskStop"
        );
        assert_eq!(
            tool_summary_for(
                Some(crate::ir::Harness::Codex),
                "exec",
                &serde_json::json!(
                    "await tools.exec_command({ cmd: 'export CV_ENDPOINT=lane:worker && cargo test' });"
                )
                .into(),
            )
            .unwrap(),
            "exec · export CV_ENDPOINT=lane:worker && cargo test"
        );
        assert_eq!(
            tool_endpoint(
                Some(crate::ir::Harness::Codex),
                "exec",
                &serde_json::json!(
                    "await tools.exec_command({ cmd: 'export CV_ENDPOINT=lane:worker && cv task inbox' });"
                )
                .into(),
            )
            .unwrap()
            .as_deref(),
            Some("lane:worker")
        );
        assert_eq!(
            tool_endpoint(
                Some(crate::ir::Harness::Claude),
                "Agent",
                &serde_json::json!({"prompt": "export CV_ENDPOINT=lane:child"}).into(),
            )
            .unwrap(),
            None
        );
    }
}
