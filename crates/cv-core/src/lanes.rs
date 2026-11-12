//! The sub-agent forest as **lanes**: one row per sub-agent with what an orchestrator asks of it —
//! who it is, what it ran on, how long, how much, where it is now, and whether it is structurally
//! stranded on open obligations.
//!
//! [`crate::subagent_tree_of`] lists the forest; this module reads each transcript once (a
//! streamed pass, content inline, usage deduplicated by API `message.id` exactly as
//! `cv stats --tokens` does) and pairs it with authoritative signals: the parent transcript's
//! `<task-notification>` records, the child's own `SubagentStop` hook attachment, and `$CLUSTERVISION_HOME/state.db`
//! (`lane_runs`, OS `(pid, proc_start_time, boot_id)` process liveness, `lane_endpoints`, and `lineage_edges`).
//! Stranding is strictly structural (`OpenObligations` or mid-turn process death) — zero prose regexes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ir::{Block, Message, MessageKind, Role, SessionRef, Usage};
use crate::stream::{Demand, Flow};
use crate::AgentRef;

/// Token totals for one lane, following [`Usage`]'s disjoint convention.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    /// A `Workflow` run's `journal.jsonl` result record (`done` / `partial` / `failed` / …).
    Journal,
    /// The parent transcript's last `<task-notification>` for this agent (`completed` /
    /// `failed` / `killed` / `stopped`).
    TaskNotification,
    /// The child's own `SubagentStop` hook attachment after its last turn (`stopped`).
    SubagentStop,
    /// A `stop` row for this agent in the hook ledger (`state.db` `lane_runs`) at or after its last
    /// turn (`stopped`) — written by `cv hook lane` when the transcript has not recorded the attachment.
    HookLedger,
    /// The recorded local OS process `(pid, proc_start_time, boot_id)` in `state.db` `lane_runs`
    /// has exited (or `record_lane_exit` recorded process termination), transitioning out of
    /// `running` immediately without waiting [`RETURNED_QUIET_SECS`].
    ProcessExit,
    /// Nothing says it stopped: the transcript ends mid-work (`running`) — or, with no stop
    /// recorded anywhere, it ends in a final report that has sat untouched for
    /// [`RETURNED_QUIET_SECS`] (`returned`: the lane finished and the harness lost the
    /// notification, typically across a restart).
    Transcript,
}

/// How long a transcript must be quiet after a text-only final turn before `returned` is
/// inferred when no OS process identity or stop hook is recorded.
pub const RETURNED_QUIET_SECS: i64 = 600;

/// Structural unfinished obligations held by a lane. A parked or exited lane with any non-zero
/// obligation (or that died mid-turn on OS process exit) is `stranded == true`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenObligations {
    /// Spawned child sub-agents in this lane's own forest that have not reached `is_done()`.
    pub unreturned_subagents: usize,
    /// `Block::ToolUse` calls in the active window with no matching `Block::ToolResult`.
    pub open_tool_calls: usize,
    /// Non-terminal `TaskState::Claimed` action tasks in `TaskStore` held by this lane's endpoint.
    pub open_claimed_tasks: usize,
    /// Pending `NeedsDecision` tasks owed by, posed by, or blocking this lane's active tasks.
    pub unanswered_decisions: usize,
}

impl OpenObligations {
    pub const fn is_empty(&self) -> bool {
        self.unreturned_subagents == 0
            && self.open_tool_calls == 0
            && self.open_claimed_tasks == 0
            && self.unanswered_decisions == 0
    }

    pub const fn total(&self) -> usize {
        self.unreturned_subagents + self.open_tool_calls + self.open_claimed_tasks + self.unanswered_decisions
    }
}

/// Portable representation of a directed cross-session lineage edge (`src_session_id -> dst_session_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageLink {
    pub src_session_id: String,
    pub dst_session_id: String,
    pub edge_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: i64,
}

#[cfg(feature = "sqlite")]
impl From<crate::state::LineageEdge> for LineageLink {
    fn from(edge: crate::state::LineageEdge) -> Self {
        Self {
            src_session_id: edge.src_session_id,
            dst_session_id: edge.dst_session_id,
            edge_kind: edge.edge_kind,
            harness: edge.harness,
            tool_use_id: edge.tool_use_id,
            metadata: edge.metadata,
            created_at: edge.created_at,
        }
    }
}

/// Multi-generation cross-session lineage graph (`ancestors` leading into a session, `descendants`
/// derived from it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLineage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<LineageLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendants: Vec<LineageLink>,
}

impl SessionLineage {
    pub fn is_empty(&self) -> bool {
        self.ancestors.is_empty() && self.descendants.is_empty()
    }
}

/// Query multi-generation `ancestors` and `descendants` for `session_id` from `$CLUSTERVISION_HOME/state.db`.
/// Read-only and best-effort: returns an empty [`SessionLineage`] when `state.db` is absent or `sqlite` is disabled.
pub fn session_lineage(session_id: &str) -> SessionLineage {
    #[cfg(feature = "sqlite")]
    {
        if session_id.trim().is_empty() {
            return SessionLineage::default();
        }
        let Ok(store) = crate::state::StateStore::default_store() else {
            return SessionLineage::default();
        };
        let db_path = store.path();
        if !std::fs::metadata(db_path).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return SessionLineage::default();
        }
        let Ok(reader) = crate::harness::store::sqlite::open_ro(db_path) else {
            return SessionLineage::default();
        };
        reader
            .read_snapshot(|conn, _reads| {
                let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
                if ver == 0 {
                    return Ok(SessionLineage::default());
                }
                let walk = |sql: &str, step_col: usize| -> rusqlite::Result<Vec<LineageLink>> {
                    let mut stmt = conn.prepare(sql)?;
                    let mut visited = HashSet::from([session_id.to_string()]);
                    let mut queue = std::collections::VecDeque::from([session_id.to_string()]);
                    let mut out = Vec::new();
                    while let Some(cur) = queue.pop_front() {
                        let rows = stmt.query_map([&cur], |r| {
                            let src: String = r.get(0)?;
                            let dst: String = r.get(1)?;
                            let edge_kind: String = r.get(2)?;
                            let harness: Option<String> = r.get(3)?;
                            let tool_use_id: Option<String> = r.get(4)?;
                            let meta_raw: String = r.get(5)?;
                            let created_at: i64 = r.get(6)?;
                            let metadata = serde_json::from_str(&meta_raw).unwrap_or(serde_json::Value::Null);
                            let next = if step_col == 0 { src.clone() } else { dst.clone() };
                            Ok((
                                next,
                                LineageLink {
                                    src_session_id: src,
                                    dst_session_id: dst,
                                    edge_kind,
                                    harness,
                                    tool_use_id,
                                    metadata,
                                    created_at,
                                },
                            ))
                        })?;
                        for item in rows {
                            let (next, link) = item?;
                            if visited.insert(next.clone()) {
                                queue.push_back(next);
                            }
                            out.push(link);
                        }
                    }
                    Ok(out)
                };
                let ancestors = walk(
                    "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
                     FROM lineage_edges WHERE dst_session_id = ?1
                     ORDER BY created_at ASC, src_session_id ASC, edge_kind ASC",
                    0,
                )
                .unwrap_or_default();
                let descendants = walk(
                    "SELECT src_session_id, dst_session_id, edge_kind, harness, tool_use_id, metadata_json, created_at
                     FROM lineage_edges WHERE src_session_id = ?1
                     ORDER BY created_at ASC, dst_session_id ASC, edge_kind ASC",
                    1,
                )
                .unwrap_or_default();
                Ok(SessionLineage { ancestors, descendants })
            })
            .unwrap_or_default()
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = session_id;
        SessionLineage::default()
    }
}

/// Best-effort non-fatal recording of a directed cross-session `LineageEdge` (`src -> dst`) into
/// `$CLUSTERVISION_HOME/state.db`. Silently no-ops under `--frozen`, when `CLUSTERVISION_HOME`
/// cannot be resolved, or when `src == dst`. Callers MUST NOT invoke this on `--dry-run`.
pub fn record_lineage_edge_best_effort(
    src_session_id: &str,
    dst_session_id: &str,
    edge_kind: &str,
    harness: Option<&str>,
    tool_use_id: Option<&str>,
    metadata: serde_json::Value,
) {
    #[cfg(feature = "sqlite")]
    {
        if crate::env::frozen() {
            return;
        }
        let src = src_session_id.trim();
        let dst = dst_session_id.trim();
        let kind = edge_kind.trim();
        if src.is_empty() || dst.is_empty() || kind.is_empty() || src == dst {
            return;
        }
        let Ok(store) = crate::state::StateStore::default_store() else {
            return;
        };
        let _ = store.record_lineage_edge(&crate::state::LineageEdge {
            src_session_id: src.to_string(),
            dst_session_id: dst.to_string(),
            edge_kind: kind.to_string(),
            harness: harness.map(str::to_string),
            tool_use_id: tool_use_id.map(str::to_string),
            metadata,
            created_at: Utc::now().timestamp_millis(),
        });
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (
            src_session_id,
            dst_session_id,
            edge_kind,
            harness,
            tool_use_id,
            metadata,
        );
    }
}

/// One row of the hook ledger: what `SubagentStart` / `SubagentStop` hooks told `cv hook lane`,
/// persisted transactionally in `state.db` (`lane_runs`). [`lanes_of`] and Codex `hook_forest`
/// read it so a lane whose transcript has not yet recorded its stop still reads `stopped`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Parked: a stop hook (in the transcript or the ledger) or mid-turn OS process exit fired.
    Stopped,
    /// Finished with no stop recorded anywhere: the harness lost the notification, or the OS
    /// process exited cleanly after writing its final report.
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

/// Authoritative lane store path: `$CLUSTERVISION_HOME/state.db` when `sqlite` is enabled
/// (clean break from flat `lanes.jsonl`).
pub fn ledger_path() -> PathBuf {
    let base = crate::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(feature = "sqlite")]
    {
        base.join("state.db")
    }
    #[cfg(not(feature = "sqlite"))]
    {
        base.join("lanes.jsonl")
    }
}

#[cfg(feature = "sqlite")]
static IN_PROC_LANE_WRITE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "sqlite")]
fn bump_lane_write_gen() {
    IN_PROC_LANE_WRITE_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[cfg(feature = "sqlite")]
fn state_err_to_io(err: crate::state::StateError) -> std::io::Error {
    match err {
        crate::state::StateError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

/// Derive the canonical child session ID from a `LaneEvent` (`agent-<bare>` when `agent_transcript_path`
/// names a Claude subagent transcript, else `agent_id`).
#[cfg(feature = "sqlite")]
fn child_session_id_of_event(ev: &LaneEvent) -> String {
    if let Some(stem) = ev
        .agent_transcript_path
        .as_deref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .filter(|s| s.starts_with("agent-"))
    {
        return stem.to_string();
    }
    ev.agent_id.clone()
}

/// Query current host OS boot session identifier (`kern.bootsessionuuid` on macOS,
/// `/proc/sys/kernel/random/boot_id` on Linux) for process-reuse-across-reboot safety.
#[cfg(feature = "sqlite")]
pub fn current_boot_id() -> Option<String> {
    static BOOT_ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BOOT_ID
        .get_or_init(|| {
            #[cfg(target_os = "macos")]
            {
                let Ok(name) = std::ffi::CString::new("kern.bootsessionuuid") else {
                    return None;
                };
                let mut buf = [0u8; 64];
                let mut len = buf.len();
                // SAFETY: `name` is NUL-terminated and `buf`/`len` point to valid stack storage.
                let rc = unsafe {
                    libc::sysctlbyname(
                        name.as_ptr(),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        &mut len,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if rc == 0 && len > 0 {
                    let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
                    let s = std::str::from_utf8(&buf[..end]).ok()?.trim();
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
                None
            }
            #[cfg(target_os = "linux")]
            {
                std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                None
            }
        })
        .clone()
}

/// Check whether a recorded [`crate::state::LaneRun`] corresponds to a live local OS process,
/// verifying `boot_id` (when present) and `(pid, proc_start_time)` so recycled PIDs are never
/// misidentified as alive.
#[cfg(feature = "sqlite")]
pub fn is_run_process_alive(run: &crate::state::LaneRun) -> bool {
    if run.exited_at.is_some() {
        return false;
    }
    let Some(pid) = run.pid.filter(|&p| p > 0) else {
        return false;
    };
    if let Some(ref recorded_boot) = run.boot_id {
        if let Some(ref cur_boot) = current_boot_id() {
            if recorded_boot != cur_boot {
                return false;
            }
        }
    }
    if !crate::state::tables::is_lane_run_alive(run) {
        return false;
    }
    // Guard against foreign-UID PID reuse where `kill(pid, 0)` returned EPERM and `proc_start_time_of`
    // returned `None`: if a start timestamp was recorded and can no longer be read back, treat as exited.
    if run.proc_start_time.is_some() && crate::state::tables::proc_start_time_of(pid).is_none() {
        return false;
    }
    true
}

/// Record a process-backed [`crate::state::LaneRun`] directly in `state.db`, invalidating the
/// in-process ledger memo and recording a `parent_subagent` `LineageEdge` when `agent_id` is set.
#[cfg(feature = "sqlite")]
pub fn record_lane_run_start(run: &crate::state::LaneRun) -> std::io::Result<()> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing lanes"));
    }
    let store = crate::state::StateStore::default_store().map_err(state_err_to_io)?;
    store.record_lane_start(run).map_err(state_err_to_io)?;
    bump_lane_write_gen();
    if let Some(agent_id) = run.agent_id.as_deref() {
        let dst = run
            .agent_transcript_path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_stem())
            .and_then(|s| s.to_str())
            .filter(|s| s.starts_with("agent-"))
            .unwrap_or(agent_id);
        record_lineage_edge_best_effort(
            &run.session_id,
            dst,
            "parent_subagent",
            run.harness.as_deref(),
            None,
            serde_json::json!({
                "run_id": run.run_id,
                "agent_id": agent_id,
                "agent_type": run.agent_type,
            }),
        );
    }
    Ok(())
}

/// Record a lane run's exit in `state.db` and invalidate the in-process ledger memo.
#[cfg(feature = "sqlite")]
pub fn record_lane_run_exit(
    run_id: &str,
    exited_at_ms: i64,
    exit_status: Option<&str>,
    last_text: Option<&str>,
) -> std::io::Result<bool> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing lanes"));
    }
    let store = crate::state::StateStore::default_store().map_err(state_err_to_io)?;
    let updated = store
        .record_lane_exit(run_id, exited_at_ms, exit_status, last_text)
        .map_err(state_err_to_io)?;
    bump_lane_write_gen();
    Ok(updated)
}

/// Bind `(run_id, endpoint)` in `state.db` (`lane_endpoints`).
#[cfg(feature = "sqlite")]
pub fn bind_endpoint(run_id: &str, endpoint: &str) -> std::io::Result<crate::state::LaneEndpointBinding> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing lanes"));
    }
    let store = crate::state::StateStore::default_store().map_err(state_err_to_io)?;
    let binding = store.bind_lane_endpoint(run_id, endpoint).map_err(state_err_to_io)?;
    bump_lane_write_gen();
    Ok(binding)
}

/// Look up the latest `LaneRun` bound to `endpoint` in `state.db`.
#[cfg(feature = "sqlite")]
pub fn latest_run_for_endpoint(endpoint: &str) -> Option<crate::state::LaneRun> {
    let store = crate::state::StateStore::default_store().ok()?;
    store.latest_run_for_endpoint(endpoint).ok().flatten()
}

#[cfg(feature = "sqlite")]
fn is_clean_stop_exit_status(status: Option<&str>) -> bool {
    matches!(
        status.map(str::trim),
        Some("hook_stop" | "stop_only" | "stopped" | "subagent_stop" | "completed" | "done" | "ok" | "0")
    )
}

/// Record one lane lifecycle event (`SubagentStart` / `SubagentStop`) transactionally in
/// `$CLUSTERVISION_HOME/state.db` (`lane_runs` + `parent_subagent` `lineage_edges`), with zero
/// flat `lanes.jsonl` writes when `sqlite` is enabled.
pub fn append_lane_event(ev: &LaneEvent) -> std::io::Result<()> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing lanes"));
    }
    #[cfg(feature = "sqlite")]
    {
        let store = crate::state::StateStore::default_store().map_err(state_err_to_io)?;
        match ev.event {
            LaneEventKind::Start => {
                let started_ms = ev.ts.timestamp_millis();
                let run_id = format!("{}:{}:{}", ev.session_id, ev.agent_id, uuid::Uuid::now_v7());
                let run = crate::state::LaneRun {
                    run_id: run_id.clone(),
                    session_id: ev.session_id.clone(),
                    agent_id: Some(ev.agent_id.clone()),
                    agent_type: ev.agent_type.clone(),
                    harness: None,
                    pid: None,
                    proc_start_time: None,
                    boot_id: None,
                    transcript_path: ev.transcript_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    agent_transcript_path: ev
                        .agent_transcript_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    cwd: ev.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    started_at: started_ms,
                    exited_at: None,
                    exit_status: None,
                    last_text: ev.last_text.clone(),
                };
                store.record_lane_start(&run).map_err(state_err_to_io)?;
                bump_lane_write_gen();
                let child_sid = child_session_id_of_event(ev);
                record_lineage_edge_best_effort(
                    &ev.session_id,
                    &child_sid,
                    "parent_subagent",
                    None,
                    None,
                    serde_json::json!({
                        "run_id": run_id,
                        "agent_id": ev.agent_id,
                        "agent_type": ev.agent_type,
                    }),
                );
            }
            LaneEventKind::Stop => {
                // Round sub-millisecond timestamps up by 1ms so `exited_at >= last_turn_at` is
                // preserved across millisecond storage in `lane_runs`.
                let exited_ms =
                    ev.ts.timestamp_millis() + i64::from(!ev.ts.timestamp_subsec_nanos().is_multiple_of(1_000_000));
                let session_runs = store
                    .list_lane_runs_for_session(&ev.session_id)
                    .map_err(state_err_to_io)?;
                let matches_agent = |r: &crate::state::LaneRun| {
                    r.agent_id.as_deref() == Some(ev.agent_id.as_str())
                        || (r.agent_id.is_none() && r.session_id == ev.agent_id)
                };
                // Prefer an active run; otherwise coalesce onto a matching non-`stop_only` run
                // already closed by `hook_lane` (`StateStore::record_lane_exit`) for the same stop.
                let target = session_runs
                    .iter()
                    .rev()
                    .find(|r| matches_agent(r) && r.exited_at.is_none())
                    .cloned()
                    .or_else(|| {
                        session_runs.into_iter().rev().find(|r| {
                            matches_agent(r)
                                && r.exit_status.as_deref() != Some("stop_only")
                                && r.exited_at.is_some_and(|prev| {
                                    (exited_ms - prev).abs() <= 60_000
                                        || r.exit_status.is_none()
                                        || is_clean_stop_exit_status(r.exit_status.as_deref())
                                })
                        })
                    });
                if let Some(existing) = target {
                    let final_exited = existing.exited_at.map_or(exited_ms, |prev| prev.max(exited_ms));
                    let updated = crate::state::LaneRun {
                        agent_type: ev.agent_type.clone().or(existing.agent_type),
                        transcript_path: ev
                            .transcript_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .or(existing.transcript_path),
                        agent_transcript_path: ev
                            .agent_transcript_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .or(existing.agent_transcript_path),
                        cwd: ev
                            .cwd
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .or(existing.cwd),
                        exited_at: Some(final_exited),
                        exit_status: Some("hook_stop".into()),
                        last_text: ev.last_text.clone().or(existing.last_text.clone()),
                        ..existing
                    };
                    store.record_lane_start(&updated).map_err(state_err_to_io)?;
                    store
                        .record_lane_exit(
                            &updated.run_id,
                            final_exited,
                            Some("hook_stop"),
                            updated.last_text.as_deref(),
                        )
                        .map_err(state_err_to_io)?;
                } else {
                    let run_id = format!("stop:{}:{}:{}", ev.session_id, ev.agent_id, uuid::Uuid::now_v7());
                    let stop_run = crate::state::LaneRun {
                        run_id,
                        session_id: ev.session_id.clone(),
                        agent_id: Some(ev.agent_id.clone()),
                        agent_type: ev.agent_type.clone(),
                        harness: None,
                        pid: None,
                        proc_start_time: None,
                        boot_id: None,
                        transcript_path: ev.transcript_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        agent_transcript_path: ev
                            .agent_transcript_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                        cwd: ev.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        started_at: exited_ms,
                        exited_at: Some(exited_ms),
                        exit_status: Some("stop_only".into()),
                        last_text: ev.last_text.clone(),
                    };
                    store.record_lane_start(&stop_run).map_err(state_err_to_io)?;
                }
                bump_lane_write_gen();
            }
        }
        Ok(())
    }
    #[cfg(not(feature = "sqlite"))]
    {
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
}

#[cfg(feature = "sqlite")]
fn wal_path_of(db_path: &std::path::Path) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}

#[cfg(feature = "sqlite")]
fn state_memo_key(
    db_path: &std::path::Path,
    wal_path: &std::path::Path,
) -> Option<(crate::source::FileId, Option<crate::source::FileId>, u64)> {
    let db_id = crate::source::FileId::at(db_path)?;
    let wal_id = crate::source::FileId::at(wal_path);
    let write_gen = IN_PROC_LANE_WRITE_GEN.load(std::sync::atomic::Ordering::Acquire);
    Some((db_id, wal_id, write_gen))
}

#[cfg(feature = "sqlite")]
fn load_events_from_state_store(
    db_path: &std::path::Path,
    wal_path: &std::path::Path,
) -> Option<(std::sync::Arc<[LaneEvent]>, bool)> {
    let meta = std::fs::metadata(db_path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    let before = state_memo_key(db_path, wal_path);
    let reader = crate::harness::store::sqlite::open_ro(db_path).ok()?;
    let raw = reader
        .read_snapshot(|conn, _reads| {
            let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
            if ver == 0 {
                return Ok(Vec::new());
            }
            let mut stmt = conn.prepare(
                "SELECT rowid, session_id, agent_id, agent_type, transcript_path,
                        agent_transcript_path, cwd, started_at, exited_at, exit_status, last_text
                 FROM lane_runs
                 WHERE agent_id IS NOT NULL
                 ORDER BY rowid ASC",
            )?;
            let mut raw: Vec<(i64, i64, u8, LaneEvent)> = Vec::new();
            let rows = stmt.query_map([], |r| {
                let rowid: i64 = r.get(0)?;
                let session_id: String = r.get(1)?;
                let agent_id: String = r.get(2)?;
                let agent_type: Option<String> = r.get(3)?;
                let transcript_path: Option<String> = r.get(4)?;
                let agent_transcript_path: Option<String> = r.get(5)?;
                let cwd: Option<String> = r.get(6)?;
                let started_at: i64 = r.get(7)?;
                let exited_at: Option<i64> = r.get(8)?;
                let exit_status: Option<String> = r.get(9)?;
                let last_text: Option<String> = r.get(10)?;
                Ok((
                    rowid,
                    session_id,
                    agent_id,
                    agent_type,
                    transcript_path.map(PathBuf::from),
                    agent_transcript_path.map(PathBuf::from),
                    cwd.map(PathBuf::from),
                    started_at,
                    exited_at,
                    exit_status,
                    last_text,
                ))
            })?;

            for row in rows {
                let Ok((
                    rowid,
                    session_id,
                    agent_id,
                    agent_type,
                    transcript_path,
                    agent_transcript_path,
                    cwd,
                    started_at,
                    exited_at,
                    exit_status,
                    last_text,
                )) = row
                else {
                    continue;
                };
                let is_stop_only = exit_status.as_deref() == Some("stop_only");
                if !is_stop_only {
                    let ts = DateTime::from_timestamp_millis(started_at)
                        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
                    raw.push((
                        started_at,
                        rowid,
                        0,
                        LaneEvent {
                            ts,
                            event: LaneEventKind::Start,
                            session_id: session_id.clone(),
                            agent_id: agent_id.clone(),
                            agent_type: agent_type.clone(),
                            transcript_path: transcript_path.clone(),
                            agent_transcript_path: agent_transcript_path.clone(),
                            cwd: cwd.clone(),
                            last_text: if exited_at.is_none() { last_text.clone() } else { None },
                        },
                    ));
                }
                if let Some(exited_ms) = exited_at {
                    let ts = DateTime::from_timestamp_millis(exited_ms)
                        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
                    raw.push((
                        exited_ms,
                        rowid,
                        1,
                        LaneEvent {
                            ts,
                            event: LaneEventKind::Stop,
                            session_id,
                            agent_id,
                            agent_type,
                            transcript_path,
                            agent_transcript_path,
                            cwd,
                            last_text,
                        },
                    ));
                }
            }
            Ok(raw)
        })
        .ok()?;
    drop(reader);
    let after = state_memo_key(db_path, wal_path);
    let stable = before.is_some() && before == after;
    let mut raw = raw;
    raw.sort_by_key(|a| (a.0, a.1, a.2));
    let events: Vec<LaneEvent> = raw.into_iter().map(|(_, _, _, ev)| ev).collect();
    Some((events.into(), stable))
}

/// Every row of the lane ledger in chronological/commit order, backed by `$CLUSTERVISION_HOME/state.db`
/// (`lane_runs`) when `sqlite` is enabled.
///
/// Memoized against `(state.db FileId, state.db-wal FileId, in_proc_write_gen)` so Codex
/// `hook_forest` and `--subagents` reuse one `Arc<[LaneEvent]>` across unchanged reads and
/// invalidate immediately on in-process or cross-process WAL commits.
pub fn read_ledger() -> std::sync::Arc<[LaneEvent]> {
    use crate::source::FileId;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};

    static EMPTY: OnceLock<Arc<[LaneEvent]>> = OnceLock::new();
    let empty = || Arc::clone(EMPTY.get_or_init(|| Arc::from([])));

    #[cfg(feature = "sqlite")]
    {
        type StateKey = (FileId, Option<FileId>, u64);
        type Memo = HashMap<PathBuf, (StateKey, Arc<[LaneEvent]>)>;
        static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();

        let db_path = ledger_path();
        let wal_path = wal_path_of(&db_path);
        let Some(before) = state_memo_key(&db_path, &wal_path) else {
            return empty();
        };
        let memo = MEMO.get_or_init(Default::default);
        if let Some((_, rows)) = memo
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&db_path)
            .filter(|(seen, _)| *seen == before)
        {
            return Arc::clone(rows);
        }
        let (rows, stable) = match load_events_from_state_store(&db_path, &wal_path) {
            Some(pair) => pair,
            None => (empty(), true),
        };
        if stable {
            if let Some(post_close) = state_memo_key(&db_path, &wal_path) {
                if post_close.2 == before.2 {
                    memo.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(db_path, (post_close, Arc::clone(&rows)));
                }
            }
        }
        rows
    }
    #[cfg(not(feature = "sqlite"))]
    {
        type Memo = HashMap<PathBuf, (FileId, Arc<[LaneEvent]>)>;
        static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
        let path = ledger_path();
        let Some(before) = FileId::at(&path) else {
            return empty();
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
            return empty();
        };
        let rows: Arc<[LaneEvent]> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<LaneEvent>(l).ok())
            .collect();
        if FileId::at(&path) == Some(before) {
            memo.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path, (before, Arc::clone(&rows)));
        }
        rows
    }
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
/// `assistant`) and of its last `SubagentStop` hook attachment.
#[derive(Debug, Clone, Default, Serialize)]
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
/// notice's timestamp.
#[derive(Debug, Clone, Serialize)]
pub struct TaskNotice {
    pub status: String,
    pub ts: Option<DateTime<Utc>>,
}

/// One of Claude Code's synthetic API-error notices, as the transcript records it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
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
    /// stop record at all.
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
    /// Structurally stranded: parked/exited (`!running && !failed/killed`) with non-empty
    /// [`Self::open_obligations`] or mid-turn OS process exit.
    pub stranded: bool,
    /// Exact structural obligations held by this lane (`unreturned_subagents`, `open_tool_calls`,
    /// `open_claimed_tasks`, `unanswered_decisions`).
    pub open_obligations: OpenObligations,
    /// Why a `failed` / `killed` lane died, read from its transcript's last API-error notice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_cause: Option<String>,
    /// For `rate-limited`: when the quota resets (`quotaLimits.resetsAt`), when the record says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// For `rate-limited` / `context`: the notice's own text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    /// The task-store endpoint this lane acts as (`lane:<name>`): bound via `StateStore`
    /// `lane_endpoints` / task provenance, or exported via `CV_ENDPOINT=…` in its own shell commands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Where `endpoint` came from: `state` (`state.db` `lane_endpoints` or task provenance) or
    /// `transcript` (`CV_ENDPOINT=…` export).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_source: Option<EndpointSource>,
    /// The tasks the endpoint holds (assignee), newest activity first — present only when joined
    /// ([`attach_tasks`]; `cv lanes --tasks`, the serve page's Lanes pane).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<LaneTask>>,
    /// Multi-generation cross-session lineage (`ancestors` / `descendants`) from `state.db` when non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
}

/// How a lane's task-store endpoint was bound (exact provenance only; never guessed from prose/slug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSource {
    /// Bound in `StateStore` (`lane_endpoints` or exact task `session_ref` provenance).
    State,
    /// The lane's own shell command exported `CV_ENDPOINT=…` (first occurrence) — exact.
    Transcript,
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

fn compute_stranded(status: &LaneStatus, obligations: &OpenObligations, crashed_mid_turn: bool) -> bool {
    if *status == LaneStatus::Running || matches!(status.as_str(), "failed" | "killed") {
        return false;
    }
    !obligations.is_empty() || crashed_mid_turn
}

/// Populate `open_claimed_tasks`, `unanswered_decisions`, exact `session_ref` endpoint provenance,
/// and recompute `lane.stranded` from `model` without attaching full `tasks` lists.
fn populate_task_obligations(lanes: &mut [Lane], model: &crate::task::TaskReadModel) {
    if lanes.is_empty() {
        return;
    }
    // Map `session_ref` (session_id or bare agent_id) -> actor from task notes for exact provenance.
    let mut provenance_by_session: HashMap<&str, &str> = HashMap::new();
    for task in model.tasks.values() {
        for note in &task.notes {
            if let Some(sref) = note.session_ref.as_deref().filter(|s| !s.is_empty()) {
                if !note.by.is_empty() {
                    provenance_by_session.insert(sref, note.by.as_str());
                }
            }
        }
    }

    for lane in lanes.iter_mut() {
        if lane.endpoint.is_none() {
            if let Some(&actor) = provenance_by_session
                .get(lane.session_id.as_str())
                .or_else(|| provenance_by_session.get(lane.agent_id.as_str()))
            {
                lane.endpoint = Some(actor.to_string());
                lane.endpoint_source = Some(EndpointSource::State);
            }
        }

        let mut open_claimed_tasks = 0usize;
        let mut unanswered_decisions = 0usize;
        if let Some(endpoint) = lane.endpoint.as_deref() {
            let mut blocked_by_ids: HashSet<&str> = HashSet::new();
            for task in model.tasks.values() {
                if !task.state.is_terminal()
                    && task
                        .assignee
                        .as_deref()
                        .is_some_and(|a| crate::task::same_actor(endpoint, a))
                {
                    for blocker in &task.blocked_by {
                        blocked_by_ids.insert(blocker.as_str());
                    }
                    if !task.is_decision() && task.state == crate::task::TaskState::Claimed {
                        open_claimed_tasks += 1;
                    }
                }
            }
            for task in model.tasks.values() {
                if !task.is_decision() || task.state.is_terminal() {
                    continue;
                }
                let Some(dec) = task.decision.as_ref().filter(|d| d.resolution.is_none()) else {
                    continue;
                };
                let owed_by_lane = task
                    .assignee
                    .as_deref()
                    .is_some_and(|a| crate::task::same_actor(endpoint, a));
                let posed_by_lane = crate::task::same_actor(endpoint, &dec.posed_by);
                let blocks_lane = blocked_by_ids.contains(task.task_id.as_str());
                if owed_by_lane || posed_by_lane || blocks_lane {
                    unanswered_decisions += 1;
                }
            }
        }
        lane.open_obligations.open_claimed_tasks = open_claimed_tasks;
        lane.open_obligations.unanswered_decisions = unanswered_decisions;
        let crashed_mid_turn = lane.status_source == StatusSource::ProcessExit && lane.status == LaneStatus::Stopped;
        lane.stranded = compute_stranded(&lane.status, &lane.open_obligations, crashed_mid_turn);
    }
}

/// Join each lane to the task store: resolve exact endpoint bindings (`StateStore` `lane_endpoints`,
/// task `session_ref` provenance, or transcript `CV_ENDPOINT=…` export — never description guessing),
/// update structural task/decision obligations, and list the tasks that endpoint holds, newest first.
pub fn attach_tasks(lanes: &mut [Lane], model: &crate::task::TaskReadModel) {
    if lanes.is_empty() {
        return;
    }
    populate_task_obligations(lanes, model);

    let mut tasks: HashMap<&str, Vec<&crate::task::TaskProjection>> = HashMap::new();
    for task in model.tasks.values() {
        if let Some(assignee) = task.assignee.as_deref() {
            tasks
                .entry(crate::task::project::canonical_actor(assignee))
                .or_default()
                .push(task);
        }
    }
    for group in tasks.values_mut() {
        // Stable sorting retains task-ID order for equal timestamps, as the BTreeMap scan did.
        group.sort_by_key(|task| std::cmp::Reverse(task.last_ts));
    }
    for lane in lanes.iter_mut() {
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
    /// Finished for real: a terminal status AND not structurally stranded.
    pub fn is_done(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "done" | "returned") && !self.stranded
    }

    /// Finished with no stop recorded anywhere: the harness lost this lane's notification.
    pub fn lost_notification(&self) -> bool {
        self.status == LaneStatus::Returned && self.status_source == StatusSource::Transcript
    }

    /// Any activity (start or last turn) at or after `since`.
    pub fn active_since(&self, since: DateTime<Utc>) -> bool {
        self.started_at.is_some_and(|t| t >= since) || self.last_turn_at.is_some_and(|t| t >= since)
    }

    pub fn is_running(&self) -> bool {
        self.status == LaneStatus::Running
    }
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
    open_tool_calls: usize,
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
    let mut seen = HashSet::new();
    let mut open_tools: HashSet<String> = HashSet::new();
    // Only response identity is needed beyond the ordinary IR. Keep source-backed arguments
    // without unrelated raw result metadata; demanded assistant reports retain their full text.
    let opts = Demand::USAGE.with_spans();
    let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
    let session = adapter.try_stream(
        r,
        &opts,
        &mut crate::stream::TryFnSink::new(|m: Message| {
            if m.kind == MessageKind::CompactionBoundary {
                open_tools.clear();
                p.ends_in_report = false;
            }
            if m.is_model_visible() {
                for b in &m.content {
                    match b {
                        Block::ToolUse { id, .. } if !id.is_empty() => {
                            open_tools.insert(id.clone());
                        }
                        Block::ToolResult { tool_use_id, .. } if !tool_use_id.is_empty() => {
                            open_tools.remove(tool_use_id);
                        }
                        _ => {}
                    }
                }
            }
            if m.is_model_visible() && matches!(m.role, Role::User | Role::Assistant) {
                p.messages += 1;
            }
            if (m.role == Role::User && matches!(m.kind, MessageKind::Prompt | MessageKind::ToolResult))
                || m.role == Role::Tool
            {
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
    p.open_tool_calls = open_tools.len();
    Ok(p)
}

/// Per-agent `state.db` facts resolved in one batch per `lanes_of` call.
#[derive(Default, Clone)]
struct AgentStateFacts {
    /// Explicit endpoint bound via `StateStore::bind_lane_endpoint` (`lane_endpoints`).
    bound_endpoint: Option<String>,
    /// Clean stop timestamp recorded in `lane_runs` (`hook_stop`, `stopped`, `subagent_stop`, …).
    clean_stop_at: Option<DateTime<Utc>>,
    /// True when the latest `LaneRun` recorded a local OS PID that is no longer alive, or recorded
    /// an abnormal non-hook process exit at or after `last_turn_at`.
    process_exited: bool,
    /// When `record_lane_exit` recorded a non-hook exit timestamp, used to verify the lane wasn't
    /// resumed after that exit.
    process_exit_at: Option<DateTime<Utc>>,
}

#[cfg(feature = "sqlite")]
fn load_state_facts_for_forest(
    parent_id: &str,
    scope: Option<StopScope>,
    subs: &[AgentRef],
) -> HashMap<String, AgentStateFacts> {
    let mut out: HashMap<String, AgentStateFacts> = HashMap::new();
    if subs.is_empty() {
        return out;
    }
    let Ok(store) = crate::state::StateStore::default_store() else {
        return out;
    };
    let db_path = store.path();
    if !std::fs::metadata(db_path).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return out;
    }
    let Ok(reader) = crate::harness::store::sqlite::open_ro(db_path) else {
        return out;
    };

    // Build lookup sets covering root `parent_id`, every `sub.session.id` (including nested
    // workflow children), and every `sub.agent_id()`.
    let mut session_keys: Vec<String> = Vec::with_capacity(1 + subs.len() * 2);
    let mut seen_session_keys: HashSet<String> = HashSet::new();
    if seen_session_keys.insert(parent_id.to_string()) {
        session_keys.push(parent_id.to_string());
    }
    let mut agent_keys: Vec<String> = Vec::with_capacity(subs.len() * 2);
    let mut seen_agent_keys: HashSet<String> = HashSet::new();
    let mut key_to_aid: HashMap<String, Vec<String>> = HashMap::new();
    let mut path_to_aid: HashMap<String, String> = HashMap::new();
    let push_key = |map: &mut HashMap<String, Vec<String>>, key: String, aid: &str| {
        let entry = map.entry(key).or_default();
        if !entry.iter().any(|existing| existing == aid) {
            entry.push(aid.to_string());
        }
    };
    for sub in subs {
        let aid = sub.agent_id().to_string();
        let sid = sub.session.id.clone();
        if seen_session_keys.insert(sid.clone()) {
            session_keys.push(sid.clone());
        }
        if seen_session_keys.insert(aid.clone()) {
            session_keys.push(aid.clone());
        }
        if seen_agent_keys.insert(aid.clone()) {
            agent_keys.push(aid.clone());
        }
        let prefixed = format!("agent-{aid}");
        if seen_agent_keys.insert(prefixed.clone()) {
            agent_keys.push(prefixed.clone());
        }
        if seen_agent_keys.insert(sid.clone()) {
            agent_keys.push(sid.clone());
        }
        push_key(&mut key_to_aid, aid.clone(), &aid);
        push_key(&mut key_to_aid, prefixed.clone(), &aid);
        if sid != parent_id {
            push_key(&mut key_to_aid, sid.clone(), &aid);
        }
        for qualified in [
            format!("{parent_id}/{sid}"),
            format!("{parent_id}/{aid}"),
            format!("{parent_id}/{prefixed}"),
        ] {
            if seen_session_keys.insert(qualified.clone()) {
                session_keys.push(qualified.clone());
            }
            push_key(&mut key_to_aid, qualified, &aid);
        }
        path_to_aid.insert(sub.session.path.to_string_lossy().into_owned(), aid);
    }

    let Ok((runs, endpoints_by_run)) = reader.read_snapshot(|conn, _reads| {
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if ver == 0 {
            return Ok((Vec::new(), HashMap::new()));
        }
        let mut by_run_id: HashMap<String, (i64, crate::state::LaneRun)> = HashMap::new();
        let map_run = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, crate::state::LaneRun)> {
            Ok((
                r.get(0)?,
                crate::state::LaneRun {
                    run_id: r.get(1)?,
                    session_id: r.get(2)?,
                    agent_id: r.get(3)?,
                    agent_type: r.get(4)?,
                    harness: r.get(5)?,
                    pid: r.get(6)?,
                    proc_start_time: r.get(7)?,
                    boot_id: r.get(8)?,
                    transcript_path: r.get(9)?,
                    agent_transcript_path: r.get(10)?,
                    cwd: r.get(11)?,
                    started_at: r.get(12)?,
                    exited_at: r.get(13)?,
                    exit_status: r.get(14)?,
                    last_text: r.get(15)?,
                },
            ))
        };
        if let Ok(mut by_sid_stmt) = conn.prepare(
            "SELECT rowid, run_id, session_id, agent_id, agent_type, harness, pid, proc_start_time,
                    boot_id, transcript_path, agent_transcript_path, cwd, started_at,
                    exited_at, exit_status, last_text
             FROM lane_runs WHERE session_id = ?1
             ORDER BY rowid ASC",
        ) {
            for sk in &session_keys {
                if let Ok(rows) = by_sid_stmt.query_map([sk], map_run) {
                    for (rowid, run) in rows.flatten() {
                        by_run_id.entry(run.run_id.clone()).or_insert((rowid, run));
                    }
                }
            }
        }
        if let Ok(mut by_aid_stmt) = conn.prepare(
            "SELECT rowid, run_id, session_id, agent_id, agent_type, harness, pid, proc_start_time,
                    boot_id, transcript_path, agent_transcript_path, cwd, started_at,
                    exited_at, exit_status, last_text
             FROM lane_runs WHERE agent_id = ?1
             ORDER BY rowid ASC",
        ) {
            for ak in &agent_keys {
                if let Ok(rows) = by_aid_stmt.query_map([ak], map_run) {
                    for (rowid, run) in rows.flatten() {
                        by_run_id.entry(run.run_id.clone()).or_insert((rowid, run));
                    }
                }
            }
        }
        let mut ordered: Vec<(i64, crate::state::LaneRun)> = by_run_id.into_values().collect();
        ordered.sort_by(|a, b| {
            a.1.started_at
                .cmp(&b.1.started_at)
                .then(a.0.cmp(&b.0))
                .then(a.1.run_id.cmp(&b.1.run_id))
        });
        let all_runs: Vec<crate::state::LaneRun> = ordered.into_iter().map(|(_, r)| r).collect();

        let mut ep_map: HashMap<String, String> = HashMap::new();
        if !all_runs.is_empty() {
            if let Ok(mut ep_stmt) = conn.prepare(
                "SELECT endpoint FROM lane_endpoints
                 WHERE run_id = ?1
                 ORDER BY bound_at ASC, endpoint ASC",
            ) {
                for run in &all_runs {
                    if ep_map.contains_key(&run.run_id) {
                        continue;
                    }
                    if let Ok(rows) = ep_stmt.query_map([&run.run_id], |r| r.get::<_, String>(0)) {
                        if let Some(last_ep) = rows.flatten().last() {
                            ep_map.insert(run.run_id.clone(), last_ep);
                        }
                    }
                }
            }
        }
        Ok((all_runs, ep_map))
    }) else {
        return out;
    };

    let mut by_agent: HashMap<String, Vec<crate::state::LaneRun>> = HashMap::new();
    for run in runs {
        // Under `StopScope::Parent`, short `agent_id`s can recur in unrelated parent sessions;
        // require the run's `session_id` or `agent_transcript_path` to belong to this forest.
        if scope == Some(StopScope::Parent)
            && !seen_session_keys.contains(&run.session_id)
            && !run
                .agent_transcript_path
                .as_deref()
                .is_some_and(|p| path_to_aid.contains_key(p))
        {
            continue;
        }
        let matched_aids: Option<&[String]> = if let Some(ref raid) = run.agent_id {
            key_to_aid.get(raid).map(Vec::as_slice)
        } else if run.session_id != parent_id {
            key_to_aid.get(&run.session_id).map(Vec::as_slice)
        } else {
            None
        };
        if let Some(aids) = matched_aids {
            for aid in aids {
                by_agent.entry(aid.clone()).or_default().push(run.clone());
            }
        } else if let Some(aid) = run.agent_transcript_path.as_deref().and_then(|p| path_to_aid.get(p)) {
            by_agent.entry(aid.clone()).or_default().push(run);
        }
    }

    for (agent_id, agent_runs) in by_agent {
        let mut facts = AgentStateFacts::default();
        for run in &agent_runs {
            if let Some(ep) = endpoints_by_run.get(&run.run_id) {
                facts.bound_endpoint = Some(ep.clone());
            }
            if let Some(exited_ms) = run.exited_at {
                if is_clean_stop_exit_status(run.exit_status.as_deref()) {
                    if let Some(ts) = DateTime::from_timestamp_millis(exited_ms) {
                        facts.clean_stop_at = Some(facts.clean_stop_at.map_or(ts, |prev| prev.max(ts)));
                    }
                }
            }
        }
        if let Some(latest) = agent_runs
            .iter()
            .rev()
            .find(|r| r.exit_status.as_deref() != Some("stop_only"))
        {
            if latest.exited_at.is_none() && latest.pid.is_some() {
                if !is_run_process_alive(latest) {
                    facts.process_exited = true;
                }
            } else if let Some(exited_ms) = latest.exited_at {
                if !is_clean_stop_exit_status(latest.exit_status.as_deref()) {
                    facts.process_exited = true;
                    facts.process_exit_at = DateTime::from_timestamp_millis(exited_ms);
                }
            }
        }
        out.insert(agent_id, facts);
    }
    out
}

/// Resolve the status of one lane from its authoritative sources, most authoritative first;
/// OS process liveness transitions dead local PIDs out of `running` immediately without waiting
/// [`RETURNED_QUIET_SECS`].
fn status_of(
    sub: &AgentRef,
    end: &SubagentEnd,
    notice: Option<&TaskNotice>,
    ledger_stop: Option<DateTime<Utc>>,
    state_facts: Option<&AgentStateFacts>,
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
    if let Some(sf) = state_facts.filter(|sf| sf.process_exited) {
        let current = match (sf.process_exit_at, end.last_turn_at) {
            (Some(et), Some(lt)) => et >= lt,
            _ => true,
        };
        if current {
            let status = if ends_in_report {
                LaneStatus::Returned
            } else {
                LaneStatus::Stopped
            };
            return (status, StatusSource::ProcessExit);
        }
    }
    // The hook ledger saw the stop before the transcript recorded it (or the harness never
    // wrote the attachment): a stop at or after the last turn parks the lane the same way.
    if ledger_stop.is_some_and(|ts| end.last_turn_at.is_none_or(|lt| ts >= lt)) {
        return (LaneStatus::Stopped, StatusSource::HookLedger);
    }
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
    state_facts_map: &HashMap<String, AgentStateFacts>,
    visited_ancestors: &HashSet<PathBuf>,
) -> Result<Lane> {
    let p = pass(&sub.session)?;
    let end = crate::harness::rootless(sub.session.harness)
        .map(|adapter| adapter.subagent_end(&sub.session.path))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let agent_id = sub.agent_id().to_string();
    let state_facts = state_facts_map.get(&agent_id);
    let effective_ledger_stop = match (
        ledger.get(&agent_id).copied(),
        state_facts.and_then(|sf| sf.clean_stop_at),
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let (status, status_source) = status_of(
        &sub,
        &end,
        notices.get(&agent_id),
        effective_ledger_stop,
        state_facts,
        p.ends_in_report,
        Utc::now(),
    );
    let started_at = sub.session.created_at;
    let last_turn_at = end.last_turn_at.or(sub.session.updated_at);
    let duration_ms = match (started_at, last_turn_at) {
        (Some(s), Some(e)) if e >= s => Some((e - s).num_milliseconds() as u64),
        _ => None,
    };
    let last_text = sub.result_summary.clone().or(p.last_text);

    // Count unreturned child subagents in this lane's own subagent tree (cycle-safe).
    let unreturned_subagents =
        if !visited_ancestors.contains(&sub.session.path) && crate::harness::rootless(sub.session.harness).is_some() {
            let mut child_visited = visited_ancestors.clone();
            child_visited.insert(sub.session.path.clone());
            match lanes_of_inner(&sub.session, &child_visited) {
                Ok(children) => children.iter().filter(|c| !c.is_done()).count(),
                Err(_) => 0,
            }
        } else {
            0
        };

    let open_obligations = OpenObligations {
        unreturned_subagents,
        open_tool_calls: p.open_tool_calls,
        open_claimed_tasks: 0,
        unanswered_decisions: 0,
    };
    let crashed_mid_turn = status_source == StatusSource::ProcessExit && status == LaneStatus::Stopped;
    let stranded = compute_stranded(&status, &open_obligations, crashed_mid_turn);

    let cause = failure_cause(status.as_str(), &end);
    let notice = end
        .api_error
        .as_ref()
        .filter(|_| matches!(cause, Some("rate-limited" | "context")));

    let (endpoint, endpoint_source) = if let Some(bound) = state_facts.and_then(|sf| sf.bound_endpoint.clone()) {
        (Some(bound), Some(EndpointSource::State))
    } else if let Some(ep) = p.endpoint {
        (Some(ep), Some(EndpointSource::Transcript))
    } else {
        (None, None)
    };

    let mut lineage = session_lineage(&sub.session.id);
    if lineage.is_empty() && sub.session.id != agent_id {
        lineage = session_lineage(&agent_id);
    }
    let lineage = (!lineage.is_empty()).then_some(lineage);

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
        open_obligations,
        failure_cause: cause.map(str::to_string),
        resets_at: notice.and_then(|n| n.resets_at),
        failure_detail: notice.map(|n| n.text.clone()),
        endpoint,
        endpoint_source,
        tasks: None,
        lineage,
    })
}

fn task_store_likely_populated() -> bool {
    let Some(home) = crate::env::Env::current().cv_home() else {
        return false;
    };
    home.join("tasks/events.jsonl").is_file() || home.join("state.db").is_file()
}

fn is_task_log_header(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("format")?.as_str().map(|f| f == "cv-task-log"))
        .unwrap_or(false)
}

/// Replay the default task store strictly read-only without creating SQLite `-wal`/`-shm`
/// sidecars (safe under `--frozen` and on read-only mounts).
pub fn replay_tasks_readonly() -> Result<crate::task::ReplayOutcome> {
    use std::io::{BufRead as _, BufReader};

    let store = crate::task::TaskStore::default_store();
    let jsonl_path = store.dir().join("events.jsonl");
    let mut raw_events: Vec<(usize, crate::task::TaskEvent)> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();

    match std::fs::File::open(&jsonl_path) {
        Ok(file) => {
            let mut reader = BufReader::new(file);
            let mut buf = Vec::new();
            let mut line_no = 0usize;
            loop {
                buf.clear();
                let n = reader.read_until(b'\n', &mut buf)?;
                if n == 0 {
                    break;
                }
                line_no += 1;
                let Ok(text) = std::str::from_utf8(&buf) else {
                    warnings.push(format!(
                        "task log {}: line {line_no} is not valid UTF-8",
                        jsonl_path.display()
                    ));
                    continue;
                };
                let line = text.trim();
                if line.is_empty() || is_task_log_header(line) {
                    continue;
                }
                match serde_json::from_str::<crate::task::TaskEvent>(line) {
                    Ok(ev) => {
                        seen_ids.insert(ev.id.clone());
                        raw_events.push((line_no, ev));
                    }
                    Err(_) => {
                        if buf.ends_with(b"\n") {
                            let snippet: String = line.chars().take(80).collect();
                            warnings.push(format!(
                                "task log {}: line {line_no} is not a valid event: {snippet}",
                                jsonl_path.display()
                            ));
                        }
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    #[cfg(feature = "sqlite")]
    {
        let db_path = store.state_store().path().to_path_buf();
        if std::fs::metadata(&db_path).is_ok_and(|m| m.is_file() && m.len() > 0) {
            if let Ok(reader) = crate::harness::store::sqlite::open_ro(&db_path) {
                let _ = reader.read_snapshot(|conn, _reads| {
                    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
                    if ver == 0 {
                        return Ok(());
                    }
                    let Ok(mut stmt) = conn.prepare("SELECT seq, raw_json FROM task_events ORDER BY seq ASC") else {
                        return Ok(());
                    };
                    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) else {
                        return Ok(());
                    };
                    for row in rows {
                        let (seq, raw_json) = row?;
                        match serde_json::from_str::<crate::task::TaskEvent>(&raw_json) {
                            Ok(ev) => {
                                if seen_ids.insert(ev.id.clone()) {
                                    raw_events.push((seq.max(0) as usize, ev));
                                }
                            }
                            Err(_) => {
                                let snippet: String = raw_json.chars().take(80).collect();
                                warnings.push(format!(
                                    "task store {}: row {} is not a valid event: {}",
                                    db_path.display(),
                                    seq,
                                    snippet
                                ));
                            }
                        }
                    }
                    Ok(())
                });
            }
        }
    }

    let mut reducer = crate::task::TaskReducer::new();
    let mut events = Vec::with_capacity(raw_events.len());
    let mut quarantined = 0usize;
    for (line_no, ev) in raw_events {
        match reducer.apply(&ev) {
            Ok(()) => events.push(ev),
            Err(err) => {
                quarantined += 1;
                warnings.push(format!(
                    "task log {}: line {line_no} quarantined: {err}",
                    jsonl_path.display()
                ));
            }
        }
    }

    Ok(crate::task::ReplayOutcome {
        model: reducer.into_model(),
        events,
        warnings,
        quarantined,
    })
}

fn lanes_of_inner(parent: &SessionRef, visited_ancestors: &HashSet<PathBuf>) -> Result<Vec<Lane>> {
    let subs = crate::subagent_tree_of(parent)?;
    if subs.is_empty() {
        return Ok(Vec::new());
    }
    let notices = crate::harness::rootless(parent.harness)
        .map(|adapter| adapter.task_notices(parent))
        .transpose()?
        .unwrap_or_default();
    let scope = crate::harness::rootless(parent.harness).and_then(|adapter| adapter.capabilities().lane_stops);
    let index = ledger_index();
    let ledger = match scope {
        Some(StopScope::Parent) => ledger_stops(index.rows_of(&parent.id).iter().map(|&at| &index.rows[at]), |_| true),
        Some(StopScope::Forest) => {
            let agents: HashSet<_> = subs.iter().map(|sub| sub.agent_id()).collect();
            agents
                .into_iter()
                .filter_map(|agent| index.latest_stop(agent).map(|ts| (agent.to_owned(), ts)))
                .collect()
        }
        None => HashMap::new(),
    };
    #[cfg(feature = "sqlite")]
    let state_facts = load_state_facts_for_forest(&parent.id, scope, &subs);
    #[cfg(not(feature = "sqlite"))]
    let state_facts: HashMap<String, AgentStateFacts> = HashMap::new();

    let mut next_visited = visited_ancestors.clone();
    next_visited.insert(parent.path.clone());

    let mut lanes = crate::par_filter_map(subs, |s| {
        Some(lane_of(s, &notices, &ledger, &state_facts, &next_visited))
    })
    .into_iter()
    .collect::<Result<Vec<_>>>()?;

    // Best-effort read-only check of task/decision obligations when a task store is present,
    // without creating `-wal`/`-shm` sidecars or failing on broken/directory task logs.
    if !lanes.is_empty() && task_store_likely_populated() {
        if let Ok(outcome) = replay_tasks_readonly() {
            populate_task_obligations(&mut lanes, &outcome.model);
        }
    }

    lanes.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.agent_id.cmp(&b.agent_id)));
    Ok(lanes)
}

/// Every sub-agent of `parent` as a [`Lane`], **launch order** (oldest first — the order the
/// orchestrator issued them, which is how a status table reads). Read failures are explicit so a
/// changing or unreadable source cannot silently remove a lane from the forest.
pub fn lanes_of(parent: &SessionRef) -> Result<Vec<Lane>> {
    let visited = HashSet::new();
    lanes_of_inner(parent, &visited)
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
        assert_eq!(summary.open_tool_calls, 0);
        assert_eq!(summary.tokens.calls, 1);
        assert_eq!((summary.tokens.input, summary.tokens.output), (5, 7));
        assert_eq!(summary.endpoint.as_deref(), Some("lane:test"));
        assert_eq!(
            summary.last_tool.as_deref(),
            Some("Bash · CV_ENDPOINT=lane:test; echo ready")
        );
    }

    #[test]
    fn structural_open_obligations_drive_stranded_without_prose_regexes() {
        use crate::task::{new_event, TaskEventKind, TaskReducer};

        let make_lane = |status: LaneStatus, source: StatusSource, last_text: &str, open_tool_calls: usize| Lane {
            agent_id: "a1".into(),
            session_id: "agent-a1".into(),
            path: PathBuf::new(),
            agent_type: None,
            description: Some("LANE-A: test".into()),
            tool_use_id: None,
            workflow: None,
            model: None,
            started_at: None,
            last_turn_at: None,
            duration_ms: None,
            messages: 2,
            tool_calls: 1,
            tokens: LaneTokens::default(),
            status: status.clone(),
            status_source: source,
            last_text: Some(last_text.into()),
            last_tool: None,
            stranded: compute_stranded(
                &status,
                &OpenObligations {
                    open_tool_calls,
                    ..Default::default()
                },
                source == StatusSource::ProcessExit && status == LaneStatus::Stopped,
            ),
            open_obligations: OpenObligations {
                open_tool_calls,
                ..Default::default()
            },
            failure_cause: None,
            resets_at: None,
            failure_detail: None,
            endpoint: Some("lane:alpha".into()),
            endpoint_source: Some(EndpointSource::State),
            tasks: None,
            lineage: None,
        };

        // Waiting-sounding English prose with zero structural obligations is NOT stranded.
        let clean = make_lane(
            LaneStatus::Recorded("completed".into()),
            StatusSource::TaskNotification,
            "Build is mirrored. Waiting on notifications.",
            0,
        );
        assert!(!clean.stranded);
        assert!(clean.is_done());

        // Finished-sounding English prose WITH an open tool call IS stranded and NOT done.
        let open_tool = make_lane(
            LaneStatus::Recorded("completed".into()),
            StatusSource::TaskNotification,
            "All done; branch pushed.",
            1,
        );
        assert!(open_tool.stranded);
        assert!(!open_tool.is_done());

        // Claimed non-terminal task or unanswered decision marks a parked lane stranded.
        let mut ev_open = new_event(
            None,
            "human",
            TaskEventKind::Opened {
                title: "finish migration".into(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("lane:alpha".into()),
            },
        );
        ev_open.id = "t1".into();
        ev_open.task_id = "t1".into();
        let ev_claim = new_event(
            Some("t1"),
            "lane:alpha",
            TaskEventKind::Claimed {
                assignee: "lane:alpha".into(),
            },
        );
        let mut dec_open = new_event(
            None,
            "lane:alpha",
            TaskEventKind::Opened {
                title: "choose schema".into(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("human".into()),
            },
        );
        dec_open.id = "d1".into();
        dec_open.task_id = "d1".into();
        let dec_pose = new_event(
            Some("d1"),
            "lane:alpha",
            TaskEventKind::Posed {
                options: vec!["v1".into(), "v2".into()],
                default_choice: "v2".into(),
                deadline: None,
                source: None,
            },
        );
        let model = TaskReducer::reduce(&[ev_open, ev_claim, dec_open, dec_pose]).unwrap();
        let mut lanes = vec![clean];
        attach_tasks(&mut lanes, &model);
        assert_eq!(lanes[0].open_obligations.open_claimed_tasks, 1);
        assert_eq!(lanes[0].open_obligations.unanswered_decisions, 1);
        assert!(lanes[0].stranded);
        assert!(!lanes[0].is_done());
    }

    #[test]
    fn endpoint_is_read_from_the_shapes_lanes_write() {
        assert_eq!(
            endpoint_in("export CV_ENDPOINT=lane:cv-edits; cv task list --repo /x").as_deref(),
            Some("lane:cv-edits")
        );
        assert_eq!(
            endpoint_in(r#"CV_ENDPOINT="lane:fix-kick" cv task claim 01a0"#).as_deref(),
            Some("lane:fix-kick")
        );
        assert_eq!(endpoint_in("cv task inbox ember"), None);
        assert_eq!(endpoint_in("CV_ENDPOINT=https://example.com/x cv task list"), None);
        assert_eq!(
            endpoint_in("CV_ENDPOINT=lane:a; curl https://example.com").as_deref(),
            Some("lane:a")
        );
        assert_eq!(
            endpoint_in("export CV_ENDPOINT={owner}; CV_ENDPOINT=$X; CV_ENDPOINT=lane:real").as_deref(),
            Some("lane:real")
        );
    }

    #[test]
    fn task_join_preserves_actor_aliases_never_guesses_descriptions_and_keeps_stable_order() {
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
        let lane = |endpoint: Option<&str>, source: Option<EndpointSource>, description: Option<&str>| Lane {
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
            open_obligations: OpenObligations::default(),
            failure_cause: None,
            resets_at: None,
            failure_detail: None,
            endpoint: endpoint.map(str::to_owned),
            endpoint_source: source,
            tasks: None,
            lineage: None,
        };
        let mut lanes = vec![
            lane(None, None, Some("Alpha: work")),
            lane(Some("lane:alpha"), Some(EndpointSource::Transcript), Some("ALPHA")),
            lane(Some("web:lane:ALPHA"), Some(EndpointSource::State), None),
            lane(Some("lane:missing"), Some(EndpointSource::Transcript), None),
            lane(None, None, None),
        ];
        attach_tasks(&mut lanes, &model);
        // Description slug guessing is gone: lane[0] stays unbound without an explicit State or Transcript binding.
        assert_eq!(lanes[0].endpoint, None);
        assert_eq!(lanes[0].endpoint_source, None);
        assert!(lanes[0].tasks.as_ref().unwrap().is_empty());
        assert_eq!(lanes[1].endpoint_source, Some(EndpointSource::Transcript));
        assert_eq!(lanes[2].endpoint_source, Some(EndpointSource::State));
        assert_eq!(lanes[2].tasks.as_ref().unwrap().len(), 3);
        assert_eq!(lanes[1].tasks.as_ref().unwrap().len(), 1);
    }

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
        let usage = ApiErrorNotice {
            text: "Claude AI usage limit reached|1759370400".into(),
            ..ApiErrorNotice::default()
        };
        assert_eq!(classify_api_error(&usage), Some("rate-limited"));
    }

    #[test]
    fn a_ledger_stop_and_process_exit_park_a_lane_without_waiting_quiet_timeout() {
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
        // Only 5 seconds after last turn (far below RETURNED_QUIET_SECS = 600).
        let now = DateTime::from_timestamp(105, 0).unwrap();
        let running = SubagentEnd {
            last_turn_at: DateTime::from_timestamp(100, 0),
            stopped_at: None,
            api_error: None,
        };
        assert_eq!(
            status_of(&sub, &running, None, None, None, false, now),
            (LaneStatus::Running, StatusSource::Transcript)
        );
        // Dead OS process transitions out of Running immediately with StatusSource::ProcessExit!
        let dead_proc = AgentStateFacts {
            process_exited: true,
            ..Default::default()
        };
        assert_eq!(
            status_of(&sub, &running, None, None, Some(&dead_proc), false, now),
            (LaneStatus::Stopped, StatusSource::ProcessExit)
        );
        assert_eq!(
            status_of(&sub, &running, None, None, Some(&dead_proc), true, now),
            (LaneStatus::Returned, StatusSource::ProcessExit)
        );
        assert_eq!(
            status_of(&sub, &running, None, DateTime::from_timestamp(100, 0), None, false, now),
            (LaneStatus::Stopped, StatusSource::HookLedger)
        );
        assert_eq!(
            status_of(&sub, &running, None, DateTime::from_timestamp(50, 0), None, false, now),
            (LaneStatus::Running, StatusSource::Transcript),
            "a stop before the last turn is a stop the lane was resumed from"
        );
        let stopped = SubagentEnd {
            stopped_at: DateTime::from_timestamp(120, 0),
            ..running.clone()
        };
        assert_eq!(
            status_of(&sub, &stopped, None, DateTime::from_timestamp(130, 0), None, false, now).1,
            StatusSource::SubagentStop
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn state_store_ledger_roundtrips_without_lanes_jsonl_and_guards_recycled_pid() {
        let root = std::env::temp_dir().join(format!("cv-lanes-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = crate::env::Env::from_vars([("CLUSTERVISION_HOME", &root)]).enter();

        let t1 = DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        let t2 = DateTime::from_timestamp(1_700_000_010, 0).unwrap();
        append_lane_event(&LaneEvent {
            ts: t1,
            event: LaneEventKind::Start,
            session_id: "sess-1".into(),
            agent_id: "ag1".into(),
            agent_type: Some("worker".into()),
            transcript_path: Some(PathBuf::from("/tmp/sess-1.jsonl")),
            agent_transcript_path: Some(PathBuf::from("/tmp/subagents/agent-ag1.jsonl")),
            cwd: None,
            last_text: None,
        })
        .unwrap();
        append_lane_event(&LaneEvent {
            ts: t2,
            event: LaneEventKind::Stop,
            session_id: "sess-1".into(),
            agent_id: "ag1".into(),
            agent_type: None,
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            last_text: Some("finished".into()),
        })
        .unwrap();

        assert!(
            !root.join("lanes.jsonl").exists(),
            "clean break: no flat lanes.jsonl written"
        );
        assert!(root.join("state.db").exists());
        let rows = read_ledger();
        assert_eq!(rows.len(), 2);
        assert!(std::sync::Arc::ptr_eq(&rows, &read_ledger()));
        assert_eq!(rows[0].event, LaneEventKind::Start);
        assert_eq!(rows[1].event, LaneEventKind::Stop);
        assert_eq!(rows[1].last_text.as_deref(), Some("finished"));

        // `parent_subagent` lineage edge was recorded automatically on Start.
        let lin = session_lineage("agent-ag1");
        assert_eq!(lin.ancestors.len(), 1);
        assert_eq!(lin.ancestors[0].src_session_id, "sess-1");
        assert_eq!(lin.ancestors[0].edge_kind, "parent_subagent");

        // Recycled PID check: current PID is alive with real `proc_start_time`, but NOT alive
        // when `proc_start_time` or `boot_id` differs.
        let (pid, real_start) = crate::state::tables::current_process_identity();
        let live_run = crate::state::LaneRun {
            run_id: "r-live".into(),
            session_id: "sess-1".into(),
            agent_id: Some("ag2".into()),
            agent_type: None,
            harness: Some("claude".into()),
            pid,
            proc_start_time: real_start,
            boot_id: current_boot_id(),
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: 1_700_000_020_000,
            exited_at: None,
            exit_status: None,
            last_text: None,
        };
        assert!(is_run_process_alive(&live_run));
        if let Some(st) = real_start {
            let mut recycled = live_run.clone();
            recycled.proc_start_time = Some(st.saturating_sub(999_999));
            assert!(
                !is_run_process_alive(&recycled),
                "mismatched proc_start_time must detect recycled PID"
            );
        }
        if current_boot_id().is_some() {
            let mut stale_boot = live_run.clone();
            stale_boot.boot_id = Some("00000000-0000-0000-0000-000000000000".into());
            assert!(
                !is_run_process_alive(&stale_boot),
                "mismatched boot_id must detect pre-reboot PID"
            );
        }

        std::fs::remove_dir_all(&root).unwrap();
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
