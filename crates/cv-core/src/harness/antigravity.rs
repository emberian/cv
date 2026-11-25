//! Google Antigravity (`agy` CLI, Antigravity 2.0, Antigravity IDE) adapter.
//!
//! ## Storage topology (`third_party/jetski/cortex/trajectory_store/`)
//! Root is `$JETSKI_APP_DATA_DIR` when set, otherwise the existing directories among:
//! 1. `~/.gemini/antigravity-cli` (Antigravity CLI, `agy`)
//! 2. `~/.gemini/antigravity` (Antigravity 2.0 hub/app)
//! 3. `~/.gemini/antigravity-ide` (Antigravity IDE)
//!
//! Because `antigravity` and `antigravity-ide` can mirror or cross-index `antigravity-cli`
//! trajectories by UUID, discovery deduplicates across roots by `<uuid>`, preferring the richest
//! unencrypted step log per conversation:
//! `brain/<uuid>/.system_generated/logs/transcript_full.jsonl`
//!   > `transcript.jsonl`
//!   > `overview.txt` (legacy 1.x NDJSON using the same step schema).
//!
//! Byte-chunked logs under `logs/chunks/{transcript_full,transcript}/` are a fallback
//! when their monolithic export is absent. They share the same record codec.
//!
//! Explicitly skipped sidecars:
//! - `implicit/<uuid>.pb` (AEAD-encrypted implicit context blobs, not sessions)
//! - `conversations/<uuid>.pb` (Keychain AEAD-encrypted `ProtoStore` trajectory blobs; `cv` never
//!   touches OS Keychains and reads `overview.txt` + plaintext SQLite/pbtxt sidecars instead)
//! - `history.jsonl` (prompt input history, not session transcripts)
//!
//! ## Optional metadata sidecars (all read-only)
//! - `annotations/<uuid>.pbtxt` — textproto `title: "..."` (highest-priority session title).
//! - `brain/<uuid>/.system_generated/subagents/<child-uuid>.json` — child subagent descriptors
//!   (`conversationId`, `spawnStepIndex`, `state`, `subagentDescriptor{typeName, role}`,
//!   `workspaceUris`).
//! - `conversation_summaries.db` (`#[cfg(feature = "sqlite")]`, opened via [`super::store::sqlite`])
//!   — `conversation_summaries` catalog (`title`, `preview`, `workspace_uris`,
//!   `parent_conversation_id`, `agent_name`, `source`, `project_id`, `last_modified_time`).
//! - `conversations/<uuid>.db` (`#[cfg(feature = "sqlite")]`): native steps and their
//!   metadata tables, read by the `native` module.

use super::store::document::Fields;
use super::store::jsonl::{self, Codec as _};
use super::Adapter;
use crate::affordance::Denial;
use crate::ir::*;
use crate::json::{Json, JsonView};
use crate::lazy::Text;
use crate::source::{Dependencies, Probe, ReadSet};
use crate::stream::{Demand, Flow, MessageSink, Start, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
#[cfg(feature = "sqlite")]
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "sqlite")]
pub mod cold;
mod lifecycle;
#[cfg(feature = "sqlite")]
mod merge;
#[cfg(feature = "sqlite")]
mod native;

const APP_DATA_SUBDIRS: &[&str] = &["antigravity-cli", "antigravity", "antigravity-ide"];

const DENIALS: &[Denial] = &[
    Denial {
        prefix: "permission check failed",
        also: None,
    },
    Denial {
        prefix: "Encountered error in tool execution:",
        also: Some("permission check failed"),
    },
    Denial {
        prefix: "Created At:",
        also: Some("permission check failed"),
    },
    Denial {
        prefix: "User denied",
        also: None,
    },
    Denial {
        prefix: "Encountered error in tool execution:",
        also: Some("User denied"),
    },
    Denial {
        prefix: "Created At:",
        also: Some("User denied"),
    },
];

pub struct Antigravity {
    roots: Vec<PathBuf>,
}

/// Rootless instance for static capability/affordance queries (`super::rootless`).
pub(crate) static ROOTLESS: Antigravity = Antigravity { roots: Vec::new() };

fn non_empty_env_path(key: &str) -> Option<PathBuf> {
    crate::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn gemini_root_dir() -> Option<PathBuf> {
    non_empty_env_path("ANTIGRAVITY_GEMINI_DIR").or_else(|| crate::env::home_dir().map(|h| h.join(".gemini")))
}

fn default_cli_app_dir() -> Option<PathBuf> {
    gemini_root_dir().map(|g| g.join(APP_DATA_SUBDIRS[0]))
}

fn active_cli_app_dir() -> Option<PathBuf> {
    non_empty_env_path("ANTIGRAVITY_APP_DATA_DIR")
        .or_else(|| non_empty_env_path("JETSKI_APP_DATA_DIR"))
        .or_else(default_cli_app_dir)
}

fn has_native_conversation(app_data_dir: &Path, id: &str) -> bool {
    let conversations = app_data_dir.join("conversations");
    ["db", "pb"]
        .iter()
        .any(|ext| conversations.join(format!("{id}.{ext}")).is_file())
}

fn same_app_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => std::path::absolute(a).ok() == std::path::absolute(b).ok(),
    }
}

impl Antigravity {
    pub fn detect() -> Self {
        if let Some(dir) =
            non_empty_env_path("ANTIGRAVITY_APP_DATA_DIR").or_else(|| non_empty_env_path("JETSKI_APP_DATA_DIR"))
        {
            return Antigravity {
                roots: dir.exists().then_some(dir).into_iter().collect(),
            };
        }
        let roots = gemini_root_dir()
            .map(|gemini| existing_app_dirs(&gemini))
            .unwrap_or_default();
        Antigravity { roots }
    }

    fn select_resume_app_dir(&self, r: &SessionRef) -> Option<PathBuf> {
        let spelled = std::path::absolute(&r.path).unwrap_or_else(|_| r.path.clone());
        if let Some(loc) = BrainLoc::from_transcript(&spelled) {
            if loc.id == r.id && has_native_conversation(&loc.app_data_dir, &r.id) {
                return Some(loc.app_data_dir);
            }
        }
        if let Some(active) = active_cli_app_dir() {
            if has_native_conversation(&active, &r.id) {
                return Some(active);
            }
        }
        if let Some(def) = default_cli_app_dir() {
            if has_native_conversation(&def, &r.id) {
                return Some(def);
            }
        }
        self.roots
            .iter()
            .find(|root| has_native_conversation(root, &r.id))
            .cloned()
    }

    /// Construct an adapter scoped strictly inside `root`:
    /// - `<home>/.gemini/{antigravity-cli,antigravity,antigravity-ide}` when present,
    /// - `<gemini>/{antigravity-cli,antigravity,antigravity-ide}` when present,
    /// - otherwise `root` itself as an `<appDataDir>` (or flat JSONL directory).
    pub fn with_home(root: &Path) -> Self {
        if !root.exists() {
            return Antigravity { roots: Vec::new() };
        }
        let dot_gemini = root.join(".gemini");
        if dot_gemini.is_dir() {
            let sub = existing_app_dirs(&dot_gemini);
            if !sub.is_empty() {
                return Antigravity { roots: sub };
            }
        }
        let direct_sub = existing_app_dirs(root);
        if !direct_sub.is_empty() {
            return Antigravity { roots: direct_sub };
        }
        Antigravity {
            roots: vec![root.to_path_buf()],
        }
    }
}

fn existing_app_dirs(gemini_dir: &Path) -> Vec<PathBuf> {
    APP_DATA_SUBDIRS
        .iter()
        .map(|sub| gemini_dir.join(sub))
        .filter(|p| p.is_dir())
        .collect()
}

impl Adapter for Antigravity {
    fn lifecycle_body_required(&self) -> bool {
        true
    }

    fn tool_result_lifecycle(&self, result: &super::lifecycle::ToolResult<'_>) -> super::lifecycle::Observation {
        lifecycle::tool_result(result)
    }

    fn message_lifecycle(&self, message: &Message, body: &str) -> super::lifecycle::Observation {
        lifecycle::message(message, body)
    }

    fn harness(&self) -> Harness {
        Harness::Antigravity
    }

    fn resolve_path(&self, path: &Path) -> Option<(SessionRef, Box<dyn Adapter>)> {
        source_for_path(path).map(|(r, a)| (r, Box::new(a) as Box<dyn Adapter>))
    }

    fn has_cold_rewind(&self) -> bool {
        cfg!(feature = "sqlite")
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn capabilities(&self) -> super::Capabilities {
        super::Capabilities {
            denials: DENIALS,
            edit_guard: None,
            lane_stops: None,
            deferred_wrapper: None,
            rich_spans: true,
        }
    }

    /// Each conversation is its own SQLite file.
    fn path_selects_session(&self, _r: &SessionRef) -> bool {
        true
    }

    fn resume_env(&self, r: &SessionRef) -> Option<std::result::Result<super::ResumeEnv, String>> {
        let target = self.select_resume_app_dir(r)?;
        let mut env = super::ResumeEnv::default();
        if active_cli_app_dir()
            .as_deref()
            .is_some_and(|act| same_app_dir(act, &target))
        {
            return Some(Ok(env));
        }
        if default_cli_app_dir()
            .as_deref()
            .is_some_and(|def| same_app_dir(def, &target))
        {
            for key in ["ANTIGRAVITY_APP_DATA_DIR", "JETSKI_APP_DATA_DIR"] {
                if non_empty_env_path(key).is_some() {
                    env.unset.push(key);
                }
            }
            return Some(Ok(env));
        }
        let val = target.to_string_lossy().into_owned();
        env.set.insert("ANTIGRAVITY_APP_DATA_DIR".into(), val.clone());
        env.set.insert("JETSKI_APP_DATA_DIR".into(), val);
        Some(Ok(env))
    }

    /// `agy --conversation <id>` reads its app data dir (`$ANTIGRAVITY_APP_DATA_DIR`,
    /// `$JETSKI_APP_DATA_DIR`, or `${ANTIGRAVITY_GEMINI_DIR:-~/.gemini}/antigravity-cli`).
    /// When a native `conversations/<id>.{db,pb}` exists in the active or selected app data
    /// dir, `resume_env` addresses it directly; export-only transcripts have no import.
    fn resume_caveat(&self, r: &SessionRef) -> Option<String> {
        if self.select_resume_app_dir(r).is_some() {
            return None;
        }
        let cli = active_cli_app_dir()?;
        let conversations = cli.join("conversations");
        Some(format!(
            "agy resumes conversations stored in {}; {} is not there (read from {}) and agy has no import",
            conversations.display(),
            r.id,
            r.path.display()
        ))
    }

    fn append_only(&self, _r: &SessionRef) -> bool {
        // Completed parallel tools can arrive out of step order, and a later record can
        // replace a previously exported step. Byte append does not imply message append.
        false
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if let Some(directory) = chunk_directory(&r.path) {
            deps.directories.push(directory.to_path_buf());
        }
        if let Some(loc) = BrainLoc::from_transcript(&r.path) {
            deps.files
                .push(loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id)));
            deps.directories
                .push(loc.brain_dir.join(".system_generated").join("subagents"));
            deps.files
                .push(loc.brain_dir.join(".system_generated/swarm/members.json"));
            deps.files
                .push(loc.brain_dir.join(".system_generated/swarm/board.jsonl"));
            if let Some(reg) = find_swarm_registry(&loc.app_data_dir, &loc.id, None) {
                if reg.coordinator_id != loc.id {
                    let coord_swarm = loc
                        .app_data_dir
                        .join("brain")
                        .join(&reg.coordinator_id)
                        .join(".system_generated/swarm");
                    deps.files.push(coord_swarm.join("members.json"));
                    deps.files.push(coord_swarm.join("board.jsonl"));
                }
            }
            #[cfg(feature = "sqlite")]
            {
                let conv_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
                record_sqlite_deps(&conv_db, &mut deps.files);
                record_sqlite_deps(&loc.app_data_dir.join("conversation_summaries.db"), &mut deps.files);
                for root in &self.roots {
                    if root != &loc.app_data_dir {
                        record_sqlite_deps(
                            &root.join("conversations").join(format!("{}.db", loc.id)),
                            &mut deps.files,
                        );
                    }
                }
            }
        }
        Some(())
    }

    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        #[cfg(feature = "sqlite")]
        {
            BrainLoc::from_transcript(&r.path).map(|loc| loc.app_data_dir.join("conversation_summaries.db"))
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = r;
            None
        }
    }

    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        #[cfg(feature = "sqlite")]
        {
            summary_rows(store)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = store;
            None
        }
    }

    fn delivered_body<'a>(&self, m: &Message, text: &'a str) -> Option<&'a str> {
        if m.origin == Origin::Subagent || m.kind == MessageKind::InjectedContext {
            let body = unwrap_system_message_body(text)?;
            if m.kind == MessageKind::SubagentReturn {
                // What the child said, without the harness's idle notice around it.
                return Some(idle_child(body).map_or(body, |(_, response)| response));
            }
            return Some(body);
        }
        None
    }

    fn spawn_returns(&self, name: &str, input: &Json) -> Result<Option<crate::affordance::Returns>> {
        if name == "swarm" {
            let hit = crate::affordance::classify_tool_json(Harness::Antigravity, name, input)?;
            return Ok(matches!(hit, Some((crate::affordance::Kind::SubagentSpawn, _)))
                .then_some(crate::affordance::Returns::Later));
        }
        Ok(crate::affordance::spawn_returns(Harness::Antigravity, name))
    }

    fn record_affordance(
        &self,
        m: &Message,
        _first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        if m.origin == Origin::Scheduler {
            return Ok(Some(crate::affordance::RecordHit {
                kind: crate::affordance::Kind::Schedule,
                action: "fire",
                tool: "schedule",
            }));
        }
        Ok(None)
    }

    fn message_affordances(&self, m: &Message) -> Vec<crate::affordance::BagHit> {
        let bag = m.extra.get("antigravity");
        // A denial already surfaces through its refusal text; an approval leaves no
        // trace in the result body.
        if let Some(interaction) = bag.and_then(|v| v.get("interaction")).filter(|i| {
            i.get("kind").and_then(Value::as_str) == Some("permission")
                && i.get("decision").and_then(Value::as_str) == Some("approved")
        }) {
            let part = |key: &str| interaction.get(key).and_then(Value::as_str).unwrap_or_default();
            let action = part("action");
            let target = part("target");
            let summary = if target.is_empty() {
                action.to_owned()
            } else {
                format!("{action} {target}")
            };
            return vec![crate::affordance::BagHit {
                kind: crate::affordance::Kind::Approval,
                action: "approve",
                tool: "permission",
                summary,
            }];
        }
        let task = |key: &str| bag.and_then(|v| v.get(key)).and_then(Value::as_str);
        let (action, task) = match (task("completed_task"), task("canceled_task")) {
            (Some(task), _) => ("complete", task),
            (None, Some(task)) => ("stop", task),
            (None, None) => return Vec::new(),
        };
        vec![crate::affordance::BagHit {
            kind: crate::affordance::Kind::BackgroundJob,
            action,
            tool: "manage_task",
            summary: task.to_owned(),
        }]
    }

    fn agent_transcript(&self, parent: &SessionRef, agent_session: &str, _workflow: Option<&str>) -> Option<PathBuf> {
        if agent_session.is_empty() || agent_session.contains(['/', '\\']) {
            return None;
        }
        let loc = BrainLoc::from_transcript(&parent.path)?;
        select_session_source(&loc.app_data_dir, agent_session).or_else(|| {
            let descriptor = loc
                .brain_dir
                .join(".system_generated/subagents")
                .join(format!("{agent_session}.json"));
            descriptor.is_file().then_some(descriptor).or_else(|| {
                read_subagent_descriptors(&loc.brain_dir)
                    .ok()?
                    .into_iter()
                    .find(|desc| desc.conversation_id == agent_session)
                    .map(|desc| desc.path)
            })
        })
    }

    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        if workflow.is_some() {
            return Ok(Vec::new());
        }
        let Some(loc) = BrainLoc::from_transcript(&r.path) else {
            return Ok(Vec::new());
        };
        let descriptors = read_subagent_descriptors(&loc.brain_dir)?;
        if descriptors.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "sqlite")]
        let summaries = read_summaries_map(&loc.app_data_dir.join("conversation_summaries.db")).unwrap_or_default();
        let spawn_calls = {
            let children: Vec<&str> = descriptors.iter().map(|desc| desc.conversation_id.as_str()).collect();
            spawn_calls_in_session(&loc.app_data_dir, &loc.id, Some(&r.path), &children)
        };
        let mut board: Option<SwarmBoard> = None;
        let mut out = Vec::new();
        #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))]
        for mut desc in descriptors {
            if crate::publish::validate_output_id(&desc.conversation_id).is_err() {
                continue;
            }
            if !select(&desc.conversation_id, None) {
                continue;
            }
            let (mut child_ref, available) = match select_session_source(&loc.app_data_dir, &desc.conversation_id) {
                Some(child_log) => match scan_transcript(&desc.conversation_id, &child_log, Some(&loc.app_data_dir)) {
                    Ok(Some(reference)) => (reference, true),
                    Ok(None) => continue,
                    Err(err) if super::vanished_at(&child_log, &err) => {
                        let descriptor = desc.path.clone();
                        if !descriptor.is_file() {
                            continue;
                        }
                        (
                            SessionRef::new(&desc.conversation_id, Harness::Antigravity, descriptor),
                            false,
                        )
                    }
                    Err(err) => return Err(err),
                },
                None => {
                    let descriptor = desc.path.clone();
                    if !descriptor.is_file() {
                        continue;
                    }
                    (
                        SessionRef::new(&desc.conversation_id, Harness::Antigravity, descriptor),
                        false,
                    )
                }
            };
            if child_ref.cwd.is_none() {
                child_ref.cwd = desc.workspace_uris.iter().find_map(|u| file_uri_to_path(u));
            }
            #[cfg(feature = "sqlite")]
            let mut sum_killed = false;
            #[cfg(feature = "sqlite")]
            if let Some(sum) = summaries.get(&desc.conversation_id) {
                let has_pbtxt = read_pbtxt_title(
                    &loc.app_data_dir
                        .join("annotations")
                        .join(format!("{}.pbtxt", desc.conversation_id)),
                )
                .is_some_and(|t| !t.trim().is_empty());
                apply_summary_fallback(&mut child_ref, sum, has_pbtxt);
                sum_killed = sum.killed;
                if desc.type_name.is_none() {
                    desc.type_name = sum.agent_type.clone().or_else(|| sum.agent_name.clone());
                }
                if desc.role.is_none() {
                    desc.role.clone_from(&sum.agent_role);
                }
            }
            #[cfg(not(feature = "sqlite"))]
            let sum_killed = false;
            let mut result_summary = None;
            if !available {
                if let Some(member_name) = desc.member.get("member").and_then(Value::as_str) {
                    let swarm_board = board.get_or_insert_with(|| read_swarm_board(&loc.brain_dir));
                    result_summary = swarm_board.last_summaries.get(member_name).cloned();
                    if child_ref.title.is_none() {
                        child_ref.title = swarm_board.brief_titles.get(member_name).cloned();
                    }
                }
            }
            // Only the parent's own store/transcript names the spawning call; `spawnStepIndex` does not identify it.
            let tool_use_id = spawn_calls.get(&desc.conversation_id).cloned();
            out.push(crate::AgentRef {
                session: child_ref,
                agent_type: desc.type_name,
                description: desc.role,
                tool_use_id,
                workflow: None,
                result_status: match desc.state.as_deref() {
                    Some("SUBAGENT_STATE_KILLED") => Some("killed".into()),
                    _ if sum_killed => Some("killed".into()),
                    _ if !available => Some("unavailable".into()),
                    _ => None,
                },
                result_summary,
            });
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.session.created_at));
        Ok(out)
    }

    fn subagent_end(&self, transcript: &Path) -> Result<Option<crate::lanes::SubagentEnd>> {
        if descriptor_parent(transcript).is_some() {
            return Ok(None);
        }
        let id = BrainLoc::from_transcript(transcript)
            .map(|loc| loc.id)
            .or_else(|| transcript.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
            .unwrap_or_default();
        let reference = SessionRef::new(id, Harness::Antigravity, transcript);
        let mut saw_any = false;
        let mut last_turn_at: Option<DateTime<Utc>> = None;
        let mut has_nonterminal_step = false;
        let mut open_tools: BTreeSet<String> = BTreeSet::new();
        let mut trailing_wake = false;
        let mut saw_assistant_or_abort = false;
        let mut last_assistant_zero_tools = false;
        let mut swarm_wait_ids: BTreeSet<String> = BTreeSet::new();
        let mut completed_swarm_wait = false;
        let mut ended_in_abort = false;

        let session = self.try_stream(
            &reference,
            &Demand::RICH.with_spans(),
            &mut crate::stream::TryFnSink::new(|m: Message| {
                if m.kind == MessageKind::CompactionBoundary {
                    open_tools.clear();
                    return Ok(Flow::Continue);
                }
                saw_any = true;
                if let Some(ts) = m.timestamp {
                    last_turn_at = Some(last_turn_at.map_or(ts, |prev| prev.max(ts)));
                }
                if m.extra
                    .get("antigravity")
                    .and_then(|bag| bag.get("status"))
                    .and_then(Value::as_str)
                    .is_some_and(|st| matches!(st, "RUNNING" | "PENDING" | "WAITING" | "QUEUED" | "GENERATING"))
                {
                    has_nonterminal_step = true;
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
                match m.role {
                    Role::User if matches!(m.kind, MessageKind::Prompt | MessageKind::SubagentReturn) => {
                        trailing_wake = true;
                        ended_in_abort = false;
                    }
                    Role::System if m.kind == MessageKind::InjectedContext => {
                        trailing_wake = true;
                        ended_in_abort = false;
                    }
                    Role::System if m.kind == MessageKind::Error => {
                        saw_assistant_or_abort = true;
                        trailing_wake = false;
                        ended_in_abort = true;
                    }
                    Role::Assistant => {
                        saw_assistant_or_abort = true;
                        trailing_wake = false;
                        ended_in_abort = m
                            .extra
                            .get("antigravity")
                            .and_then(|bag| bag.get("interrupted"))
                            .and_then(Value::as_bool)
                            == Some(true);
                        swarm_wait_ids.clear();
                        completed_swarm_wait = false;
                        let mut tool_count = 0usize;
                        for b in &m.content {
                            if let Block::ToolUse { id, name, .. } = b {
                                tool_count += 1;
                                if name == "swarm_wait" && !id.is_empty() {
                                    swarm_wait_ids.insert(id.clone());
                                }
                            }
                        }
                        last_assistant_zero_tools = tool_count == 0;
                    }
                    Role::Tool => {
                        for b in &m.content {
                            if let Block::ToolResult {
                                tool_use_id,
                                status,
                                is_error,
                                ..
                            } = b
                            {
                                if swarm_wait_ids.contains(tool_use_id)
                                    && status.as_deref() == Some("completed")
                                    && !*is_error
                                {
                                    completed_swarm_wait = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                Ok(Flow::Continue)
            }),
        )?;

        if !saw_any {
            return Ok(None);
        }
        let parked = !has_nonterminal_step
            && open_tools.is_empty()
            && !trailing_wake
            && saw_assistant_or_abort
            && (last_assistant_zero_tools || completed_swarm_wait || ended_in_abort);
        let is_swarm_member = session
            .harness_extra(Harness::Antigravity)
            .and_then(|bag| bag.get("member"))
            .is_some();
        Ok(Some(if parked {
            crate::lanes::SubagentEnd {
                last_turn_at,
                stopped_at: Some(last_turn_at.unwrap_or(DateTime::UNIX_EPOCH)),
                parked_status: is_swarm_member.then(|| "waiting".to_string()),
                active: false,
                api_error: None,
            }
        } else {
            crate::lanes::SubagentEnd {
                last_turn_at,
                stopped_at: None,
                parked_status: None,
                active: true,
                api_error: None,
            }
        }))
    }

    fn task_notices(&self, parent: &SessionRef) -> Result<std::collections::HashMap<String, crate::lanes::TaskNotice>> {
        struct NoticeCollector {
            resolver: crate::lazy::Resolver,
            swarm_reg: Option<SwarmRegistry>,
            parent_id: String,
            send_recipients: BTreeMap<String, String>,
            notices: std::collections::HashMap<String, crate::lanes::TaskNotice>,
            error: Option<anyhow::Error>,
        }
        impl MessageSink for NoticeCollector {
            fn meta(&mut self, session: &Session) {
                if session.source_path.is_some() {
                    self.resolver = session.resolver();
                }
            }
            fn message(&mut self, m: Message) -> Flow {
                let res = (|| -> Result<()> {
                    if m.role == Role::Assistant {
                        for b in &m.content {
                            if let Block::ToolUse { id, name, input, .. } = b {
                                if name == "send_message" && !id.is_empty() {
                                    if let Some(recipient) = input
                                        .with_view(|v| v.get("Recipient")?.map(JsonView::string).transpose())?
                                        .flatten()
                                        .filter(|s| !s.is_empty())
                                    {
                                        self.send_recipients.insert(id.clone(), recipient);
                                    }
                                }
                            }
                        }
                    } else if m.kind == MessageKind::SubagentReturn {
                        if let Some(child) = m
                            .extra
                            .get("antigravity")
                            .and_then(|bag| bag.get("returned_child"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            self.notices.insert(
                                child.to_string(),
                                crate::lanes::TaskNotice {
                                    status: "completed".to_string(),
                                    ts: m.timestamp,
                                },
                            );
                        }
                    } else if m.role == Role::Tool {
                        for b in &m.content {
                            let Block::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                                tool_name,
                                ..
                            } = b
                            else {
                                continue;
                            };
                            if *is_error {
                                continue;
                            }
                            match tool_name.as_deref() {
                                Some("send_message") => {
                                    if let Some(recipient) = self.send_recipients.remove(tool_use_id) {
                                        self.notices.remove(&recipient);
                                    }
                                }
                                Some("manage_subagents") => {
                                    let head = content.try_resolve_prefix(&self.resolver, 64 * 1024)?;
                                    if let Some(line) = head.lines().map(str::trim).find(|l| l.starts_with('[')) {
                                        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(line) {
                                            for item in items {
                                                let cid = item.get("conversationId").and_then(Value::as_str);
                                                let state = item.get("state").and_then(Value::as_str);
                                                if let (Some(cid), Some(state)) = (cid, state) {
                                                    let mapped = match state {
                                                        "idle" => Some("completed"),
                                                        "running" => Some("running"),
                                                        "killed" => Some("killed"),
                                                        "stopped" => Some("stopped"),
                                                        _ => None,
                                                    };
                                                    if let Some(status) = mapped {
                                                        self.notices.insert(
                                                            cid.to_string(),
                                                            crate::lanes::TaskNotice {
                                                                status: status.to_string(),
                                                                ts: m.timestamp,
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Some("swarm") => {
                                    if let Some(reg) = &self.swarm_reg {
                                        let head = content.try_resolve_prefix(&self.resolver, 64 * 1024)?;
                                        if let Some(line) = head.lines().map(str::trim).find(|l| l.starts_with('{')) {
                                            if let Ok(val) = serde_json::from_str::<Value>(line) {
                                                if val.get("status").and_then(Value::as_str) == Some("killed") {
                                                    if let Some(cid) = val
                                                        .get("name")
                                                        .and_then(Value::as_str)
                                                        .and_then(|n| reg.by_name.get(n))
                                                        .map(|m_info| &m_info.conversation_id)
                                                        .filter(|cid| *cid != &self.parent_id)
                                                    {
                                                        self.notices.insert(
                                                            cid.clone(),
                                                            crate::lanes::TaskNotice {
                                                                status: "killed".to_string(),
                                                                ts: m.timestamp,
                                                            },
                                                        );
                                                    }
                                                } else if val.get("status").is_none() {
                                                    if let Some(members) = val.get("members").and_then(Value::as_array)
                                                    {
                                                        for item in members {
                                                            let Some(name) = item.get("name").and_then(Value::as_str)
                                                            else {
                                                                continue;
                                                            };
                                                            let Some(m_info) = reg.by_name.get(name) else {
                                                                continue;
                                                            };
                                                            if m_info.conversation_id == self.parent_id {
                                                                continue;
                                                            }
                                                            let killed = item.get("killed").and_then(Value::as_bool)
                                                                == Some(true);
                                                            let state = if killed {
                                                                Some("killed")
                                                            } else {
                                                                item.get("state").and_then(Value::as_str)
                                                            };
                                                            if let Some(st) = state.filter(|st| {
                                                                matches!(
                                                                    *st,
                                                                    "waiting"
                                                                        | "working"
                                                                        | "stopped"
                                                                        | "killed"
                                                                        | "running"
                                                                )
                                                            }) {
                                                                self.notices.insert(
                                                                    m_info.conversation_id.clone(),
                                                                    crate::lanes::TaskNotice {
                                                                        status: st.to_string(),
                                                                        ts: m.timestamp,
                                                                    },
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(err) = res {
                    self.error = Some(err);
                    return Flow::Stop;
                }
                Flow::Continue
            }
            fn take_error(&mut self) -> Option<anyhow::Error> {
                self.error.take()
            }
        }
        let swarm_reg = BrainLoc::from_transcript(&parent.path).and_then(|loc| read_swarm_registry(&loc.brain_dir));
        let mut collector = NoticeCollector {
            resolver: crate::lazy::Resolver::new(Some(parent.path.clone())),
            swarm_reg,
            parent_id: parent.id.clone(),
            send_recipients: BTreeMap::new(),
            notices: std::collections::HashMap::new(),
            error: None,
        };
        self.try_stream(parent, &Demand::RICH.with_spans(), &mut collector)?;
        Ok(collector.notices)
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let _cache_scope = crate::discover_cache::read::Context::current().enter();
        #[cfg(feature = "sqlite")]
        let _sqlite_scope = super::store::sqlite::read_scope();

        let mut out = crate::Discovered::default();
        // Deduplicate by session UUID across roots, preferring higher-fidelity log ranks
        // (full log = 4 > full chunks = 3 > compact log/chunks = 2 > overview/native = 1 > flat = 0)
        // and breaking equal-rank ties by mtime, then earlier root (`antigravity-cli` > `antigravity` > `antigravity-ide`).
        struct Candidate {
            rank: u8,
            mtime: Option<std::time::SystemTime>,
            transcript: PathBuf,
            app_data_dir: Option<PathBuf>,
        }
        let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
        #[cfg(feature = "sqlite")]
        let mut summaries_by_root: HashMap<PathBuf, HashMap<String, SummaryRow>> = HashMap::new();

        for root in &self.roots {
            if !root.is_dir() {
                continue;
            }
            #[cfg(feature = "sqlite")]
            {
                let summaries_db = root.join("conversation_summaries.db");
                if summaries_db.is_file() {
                    if let Some(map) = out.keep(&summaries_db, read_summaries_map(&summaries_db)) {
                        summaries_by_root.insert(root.clone(), map);
                    }
                }
            }

            let brain_root = root.join("brain");
            if brain_root.is_dir() {
                for entry in super::read_dir_bulk_kept(&brain_root, &mut out.skipped) {
                    let Some(id) = entry.name.to_str() else {
                        continue;
                    };
                    if id.starts_with('.') {
                        continue;
                    };
                    let brain_dir = brain_root.join(&entry.name);
                    let is_dir = entry.file_type == crate::io_admission::BulkFileType::Dir
                        || (entry.file_type == crate::io_admission::BulkFileType::Symlink && brain_dir.is_dir());
                    if !is_dir {
                        continue;
                    }
                    let Some((rank, log_path)) = select_brain_log(&brain_dir) else {
                        continue;
                    };
                    let mtime = file_mtime(&log_path);
                    let replace = match candidates.get(id) {
                        None => true,
                        Some(prev) => (rank, mtime) > (prev.rank, prev.mtime),
                    };
                    if replace {
                        candidates.insert(
                            id.to_string(),
                            Candidate {
                                rank,
                                mtime,
                                transcript: log_path,
                                app_data_dir: Some(root.clone()),
                            },
                        );
                    }
                }
            }

            #[cfg(feature = "sqlite")]
            for entry in super::read_dir_bulk_kept(&root.join("conversations"), &mut out.skipped) {
                let path = root.join("conversations").join(&entry.name);
                if !is_native_store(&path) || !path.is_file() {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                    continue;
                };
                let mtime = file_mtime(&path);
                let prev_rank = candidates.get(&id).map(|c| c.rank);
                let wins_tie = candidates
                    .get(&id)
                    .is_none_or(|prev| (1u8, mtime) > (prev.rank, prev.mtime));
                if !wins_tie {
                    continue;
                }
                match native::has_steps(&path) {
                    Ok(true) => {
                        candidates.insert(
                            id,
                            Candidate {
                                rank: 1,
                                mtime,
                                transcript: path,
                                app_data_dir: Some(root.clone()),
                            },
                        );
                    }
                    Ok(false) => {}
                    // Only report a broken/non-step DB when no exported brain log already owns the ID
                    // (1.5.6 stores metadata-only `.db` sidecars without a `steps` table beside brain logs).
                    Err(err) => {
                        if prev_rank.is_none_or(|r| r == 0) {
                            out.keep(&path, Err::<(), _>(err));
                        }
                    }
                }
            }

            #[cfg(not(feature = "sqlite"))]
            for entry in super::read_dir_bulk_kept(&root.join("conversations"), &mut out.skipped) {
                let path = root.join("conversations").join(&entry.name);
                if is_native_store(&path) && path.is_file() {
                    out.skipped.push(crate::Skipped {
                        path,
                        why: "reading native Antigravity stores requires SQLite support".into(),
                    });
                }
            }

            // Also support flat `*.jsonl` transcripts when `with_home` points directly at a
            // directory of exported Antigravity step logs (excluding `history.jsonl`).
            for entry in super::read_dir_bulk_kept(root, &mut out.skipped) {
                let Some(fname) = entry.name.to_str() else {
                    continue;
                };
                if fname == "history.jsonl" || !fname.ends_with(".jsonl") {
                    continue;
                }
                let path = root.join(&entry.name);
                let is_file = entry.file_type == crate::io_admission::BulkFileType::File
                    || (entry.file_type == crate::io_admission::BulkFileType::Symlink && path.is_file());
                if !is_file {
                    continue;
                }
                let id = fname.trim_end_matches(".jsonl").to_string();
                if id.is_empty() {
                    continue;
                }
                let mtime = file_mtime(&path);
                let wins_tie = candidates
                    .get(&id)
                    .is_none_or(|prev| (0u8, mtime) > (prev.rank, prev.mtime));
                if !wins_tie {
                    continue;
                }
                if looks_like_antigravity_file(&path) {
                    candidates.insert(
                        id,
                        Candidate {
                            rank: 0,
                            mtime,
                            transcript: path,
                            app_data_dir: None,
                        },
                    );
                }
            }
        }

        let items: Vec<(String, Candidate)> = candidates.into_iter().collect();
        let scanned = crate::par_discovery_filter_map(items, |(id, cand)| {
            let mut reads = ReadSet::default();
            reads.record(&cand.transcript);
            if let Some(app_dir) = &cand.app_data_dir {
                reads.record(&app_dir.join("annotations").join(format!("{id}.pbtxt")));
                reads.record(
                    &app_dir
                        .join("brain")
                        .join(&id)
                        .join(".system_generated")
                        .join("subagents"),
                );
                #[cfg(feature = "sqlite")]
                {
                    record_sqlite_readset(&app_dir.join("conversations").join(format!("{id}.db")), &mut reads);
                    for root in &self.roots {
                        if root != app_dir {
                            record_sqlite_readset(&root.join("conversations").join(format!("{id}.db")), &mut reads);
                        }
                    }
                }
            }
            let res = crate::discover_cache::cached_scan_with_reads(&cand.transcript, reads, || {
                scan_transcript(&id, &cand.transcript, cand.app_data_dir.as_deref())
            });
            #[cfg(feature = "sqlite")]
            let res = res.map(|opt| {
                opt.map(|mut sref| {
                    if let Some(sum) = cand
                        .app_data_dir
                        .as_ref()
                        .and_then(|r| summaries_by_root.get(r))
                        .and_then(|m| m.get(&id))
                    {
                        let has_pbtxt = cand
                            .app_data_dir
                            .as_ref()
                            .and_then(|app| read_pbtxt_title(&app.join("annotations").join(format!("{id}.pbtxt"))))
                            .is_some_and(|t| !t.trim().is_empty());
                        apply_summary_fallback(&mut sref, sum, has_pbtxt);
                    }
                    sref
                })
            });
            crate::Skipped::listed(&cand.transcript, res)
        });
        for found in scanned {
            out.push(found);
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        // An exporter can create the full-log file before its first flush, and a log
        // can be removed after discovery chose it. Empty or missing history has no
        // precedence over a surviving chunked, compact or native store of the session.
        let unusable = match fs::metadata(&r.path) {
            Ok(meta) => meta.len() == 0,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        let fallback = if unusable {
            BrainLoc::from_transcript(&r.path)
                .and_then(|loc| select_session_source(&loc.app_data_dir, &loc.id))
                .filter(|path| path != &r.path)
                .map(|path| {
                    let mut reference = r.clone();
                    reference.path = path;
                    reference
                })
        } else {
            None
        };
        let r = fallback.as_ref().unwrap_or(r);
        if let Some(parent) = descriptor_parent(&r.path) {
            if matches!(from, Start::Checkpoint(_)) {
                return Ok(None);
            }
            let source = crate::json::JsonSource::capture(&r.path)?;
            anyhow::ensure!(source.len() <= 1024 * 1024, "oversized Antigravity child descriptor");
            source.read(
                crate::json::ReadInput::Document,
                crate::json::Validation::Value,
                |view| {
                    let descriptor = view.resolve()?.into_owned();
                    let matches = if r.path.file_name().is_some_and(|name| name == "members.json") {
                        descriptor
                            .get("members")
                            .and_then(Value::as_object)
                            .is_some_and(|members| {
                                members.values().any(|member| {
                                    member.get("conversation_id").and_then(Value::as_str) == Some(r.id.as_str())
                                })
                            })
                    } else {
                        descriptor.get("conversationId").and_then(Value::as_str) == Some(r.id.as_str())
                    };
                    anyhow::ensure!(matches, "Antigravity child descriptor identity mismatch");
                    Ok(())
                },
            )?;
            source.check()?;
            let mut session = Session::new(&r.id, Harness::Antigravity);
            session.source_path = Some(r.path.clone());
            session.title = r.title.clone();
            session.cwd = r.cwd.clone();
            session.lineage.parent = Some(parent);
            if let Some(app_data_dir) = descriptor_app_data_dir(&r.path) {
                enrich_child_lineage(&mut session, &app_data_dir, None, opts.tier >= Tier::Rich);
            }
            session
                .harness_extra_mut(Harness::Antigravity)
                .insert("transcript_unavailable".into(), Value::Bool(true));
            crate::coverage::ReadCoverage {
                sources: [crate::coverage::ReadCoverage::MEMBER_REGISTRY.into()].into(),
                limitations: [
                    crate::coverage::ReadCoverage::METADATA_ONLY.into(),
                    crate::coverage::ReadCoverage::MISSING_HISTORY.into(),
                ]
                .into(),
            }
            .attach(&mut session);
            sink.meta(&session);
            return Ok(Some(session));
        }
        crate::stream::checked(sink, |sink| {
            if let Some(loc) = BrainLoc::from_transcript(&r.path) {
                let pbtxt = loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id));
                sink.dependency(&pbtxt);
                sink.dependency(&loc.brain_dir.join(".system_generated/swarm/members.json"));
                sink.dependency(&loc.brain_dir.join(".system_generated/swarm/board.jsonl"));
                #[cfg(feature = "sqlite")]
                {
                    let conv_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
                    record_sink_sqlite_deps(&conv_db, sink);
                    record_sink_sqlite_deps(&loc.app_data_dir.join("conversation_summaries.db"), sink);
                    for root in &self.roots {
                        if root != &loc.app_data_dir {
                            record_sink_sqlite_deps(&root.join("conversations").join(format!("{}.db", loc.id)), sink);
                        }
                    }
                }
            }
            // Header enrichment is owned from its read snapshot. Its dependencies still
            // invalidate cached products, but changing a title or live WAL cannot change
            // the bytes of a transcript span. Guard those bytes by their own source.
            #[cfg(feature = "sqlite")]
            let input = if is_native_store(&r.path) {
                native::open(&r.path, opts.tier == Tier::Complete)?
            } else {
                open_transcript(&r.path)?
            };
            #[cfg(not(feature = "sqlite"))]
            let input = open_transcript(&r.path)?;
            if matches!(from, Start::Checkpoint(_)) {
                return Ok(None);
            }
            let (s, enrich) = build_session_header(r, opts);
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Antigravity, sink);
            let mut codec = StepCodec {
                s,
                opts: *opts,
                spans: opts.spans,
                enrich,
                state: StepState::default(),
                source_path: Some(r.path.clone()),
            };
            codec
                .state
                .child_ids
                .extend(codec.enrich.known_children.iter().cloned());
            let mut order = StepOrder {
                local: true,
                ..StepOrder::default()
            };
            let to = match from {
                Start::Prefix(to) => to,
                _ => input.len(),
            };
            input.snapshot(0, to, |line| {
                order.ingest(&line)?;
                Ok(Flow::Continue)
            })?;
            // Native steps the export lacks, read from the native projection at their index.
            #[cfg(feature = "sqlite")]
            let mut native_only: Option<(jsonl::Read, StepOrder)> = None;
            #[cfg(feature = "sqlite")]
            if !is_native_store(&r.path) {
                if let Some(loc) = BrainLoc::from_transcript(&r.path) {
                    let local_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
                    let local_mtime = local_db.is_file().then(|| file_mtime(&local_db)).flatten();
                    let cross_db = self
                        .roots
                        .iter()
                        .filter(|root| *root != &loc.app_data_dir)
                        .map(|root| root.join("conversations").join(format!("{}.db", loc.id)))
                        .filter(|cand| cand.is_file())
                        .filter_map(|cand| {
                            let cand_mtime = file_mtime(&cand);
                            if !local_db.is_file() || cand_mtime > local_mtime {
                                Some((cand_mtime, cand))
                            } else {
                                None
                            }
                        })
                        .max_by(|(a_mtime, _), (b_mtime, _)| a_mtime.cmp(b_mtime))
                        .map(|(_, path)| path);

                    fn adopt_verified(
                        verified: merge::Verified,
                        order: &mut StepOrder,
                        codec: &mut StepCodec,
                        native_only: &mut Option<(jsonl::Read, StepOrder)>,
                    ) {
                        if !verified.repairs.is_empty()
                            || !verified.matches.is_empty()
                            || !verified.native_only.steps.is_empty()
                            || verified.unreconciled
                        {
                            order
                                .coverage
                                .sources
                                .insert(crate::coverage::ReadCoverage::NATIVE_STORE.into());
                            if verified.unread_fields {
                                order
                                    .coverage
                                    .limitations
                                    .insert(crate::coverage::ReadCoverage::UNSUPPORTED_NATIVE_FIELDS.into());
                            }
                        }
                        codec.enrich.verified = verified.matches;
                        codec.enrich.repairs = verified.repairs;
                        for index in &verified.recovered_media {
                            order.missing_media.remove(index);
                        }
                        if verified.unreconciled {
                            order
                                .coverage
                                .limitations
                                .insert(crate::coverage::ReadCoverage::UNRECONCILED_HISTORY.into());
                        }
                        if !verified.native_only.steps.is_empty() {
                            let found = &verified.native_only.coverage;
                            order.coverage.sources.extend(found.sources.iter().cloned());
                            order.coverage.limitations.extend(found.limitations.iter().cloned());
                            *native_only = Some((verified.native, verified.native_only));
                        }
                    }

                    // A cross-root native DB (e.g. CLI resuming an IDE-mirrored export) is adopted
                    // only when overlapping steps verify with zero conflicts; unrelated divergent
                    // copies in another root stay separate and never flag unreconciled_history.
                    let mut adopted_cross = false;
                    if let Some(cross_db) = cross_db {
                        if let Ok(verified) = merge::verified(
                            &input,
                            &order,
                            &cross_db,
                            opts.tier == Tier::Complete,
                            matches!(from, Start::Whole),
                            false,
                        ) {
                            if verified.overlaps_verified && !verified.unreconciled {
                                apply_native_db_metadata(
                                    &cross_db,
                                    opts.tier >= Tier::Rich,
                                    &mut codec.s,
                                    &mut codec.enrich,
                                );
                                adopt_verified(verified, &mut order, &mut codec, &mut native_only);
                                adopted_cross = true;
                            }
                        }
                    }

                    if !adopted_cross && local_db.is_file() {
                        // A changing or unreadable sidecar must not erase surviving export
                        // history; whatever it holds beyond the export is then unaccounted for.
                        match merge::verified(
                            &input,
                            &order,
                            &local_db,
                            opts.tier == Tier::Complete,
                            matches!(from, Start::Whole),
                            true,
                        ) {
                            Ok(verified) => adopt_verified(verified, &mut order, &mut codec, &mut native_only),
                            Err(_) => {
                                order
                                    .coverage
                                    .limitations
                                    .insert(crate::coverage::ReadCoverage::UNRECONCILED_HISTORY.into());
                            }
                        }
                    }
                }
            }
            if !order.missing_media.is_empty() {
                order
                    .coverage
                    .limitations
                    .insert(crate::coverage::ReadCoverage::MISSING_ATTACHMENTS.into());
            }
            if order.coverage.sources.is_empty() {
                order.coverage.sources.insert(
                    if r.path.extension().is_some_and(|ext| ext == "db") {
                        crate::coverage::ReadCoverage::NATIVE_STORE
                    } else {
                        crate::coverage::ReadCoverage::EXPORTED_HISTORY
                    }
                    .into(),
                );
            }
            order
                .coverage
                .limitations
                .remove(crate::coverage::ReadCoverage::INFERRED_TOOL_IDENTITY);
            let mut coverage_records = input.records();
            for (index, (at, end)) in &order.steps {
                #[cfg(feature = "sqlite")]
                if native_only
                    .as_ref()
                    .is_some_and(|(_, found)| found.steps.contains_key(index))
                {
                    continue;
                }
                #[cfg(not(feature = "sqlite"))]
                let _ = index;
                coverage_records.read(*at, end - at, |view| {
                    if codec.inferred_identity(view)? {
                        order
                            .coverage
                            .limitations
                            .insert(crate::coverage::ReadCoverage::INFERRED_TOOL_IDENTITY.into());
                    }
                    Ok(Flow::Continue)
                })?;
            }
            #[cfg(feature = "sqlite")]
            if let Some((native, found)) = &native_only {
                let mut records = native.records();
                for (at, end) in found.steps.values() {
                    records.read(*at, end - at, |view| {
                        if codec.inferred_identity(view)? {
                            order
                                .coverage
                                .limitations
                                .insert(crate::coverage::ReadCoverage::INFERRED_TOOL_IDENTITY.into());
                        }
                        Ok(Flow::Continue)
                    })?;
                }
            }
            order.coverage.attach(&mut codec.s);
            spawns.meta(&codec.s);
            let skipped = order.skipped;
            #[cfg(feature = "sqlite")]
            if let Some((native, found)) = native_only {
                // One pass in step order over both sources, each under its own guard.
                let mut merged: BTreeMap<u64, (bool, u64, u64)> = order
                    .steps
                    .into_iter()
                    .map(|(index, (at, end))| (index, (false, at, end)))
                    .collect();
                merged.extend(
                    found
                        .steps
                        .into_iter()
                        .map(|(index, (at, end))| (index, (true, at, end))),
                );
                let mut exported = input.records();
                let mut natives = native.records();
                return crate::stream::guarded(&mut spawns, &input, |sink| {
                    let stopped = crate::stream::guarded(sink, &native, |sink| {
                        for (from_native, at, end) in merged.into_values() {
                            let records = if from_native { &mut natives } else { &mut exported };
                            if records.read(at, end - at, |view| codec.record(view, at, sink))? == Flow::Stop {
                                return Ok(true);
                            }
                        }
                        Ok(false)
                    })?;
                    super::note_skipped_lines(codec.session(), skipped);
                    codec.finish(stopped, sink).map(Some)
                });
            }
            let mut records = input.records();
            crate::stream::guarded(&mut spawns, &input, |sink| {
                let mut stopped = false;
                for (at, end) in order.steps.into_values() {
                    if records.read(at, end - at, |view| codec.record(view, at, sink))? == Flow::Stop {
                        stopped = true;
                        break;
                    }
                }
                super::note_skipped_lines(codec.session(), skipped);
                codec.finish(stopped, sink).map(Some)
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Pure in-memory JSONL entrypoints (`ingest`, tests, wasm32)
// ---------------------------------------------------------------------------

pub(super) fn is_tool_step_type(ty: &str) -> bool {
    matches!(
        ty,
        "GENERIC"
            | "RUN_COMMAND"
            | "VIEW_FILE"
            | "LIST_DIRECTORY"
            | "CODE_ACTION"
            | "WRITE_TO_FILE"
            | "FILE_CHANGE"
            | "GREP_SEARCH"
            | "FIND"
            | "COMMAND_STATUS"
            | "READ_URL_CONTENT"
            | "SEARCH_WEB"
            | "MCP_TOOL"
            | "LIST_RESOURCES"
            | "READ_RESOURCE"
            | "FINISH"
            | "INVOKE_SUBAGENT"
            | "ASK_QUESTION"
    )
}

/// Sniff whether a single JSONL line is an Antigravity Cortex step record.
pub(crate) fn is_step_record_view(view: JsonView<'_>) -> bool {
    let Ok(Some(fields)) = Fields::parse(view, &["step_index", "source", "type"]) else {
        return false;
    };
    let has_idx = fields
        .get("step_index")
        .and_then(|v| v.as_number().ok().flatten())
        .and_then(|n| n.as_u64())
        .is_some();
    if !has_idx {
        return false;
    }
    let Ok(Some(source)) = fields.string("source") else {
        return false;
    };
    let Ok(Some(ty)) = fields.string("type") else {
        return false;
    };
    is_known_source(&source)
        && (is_tool_step_type(&ty)
            || matches!(
                ty.as_str(),
                "USER_INPUT"
                    | "PLANNER_RESPONSE"
                    | "SYSTEM_MESSAGE"
                    | "CHECKPOINT"
                    | "ERROR_MESSAGE"
                    | "EPHEMERAL_MESSAGE"
                    | "CONVERSATION_HISTORY"
            ))
}

fn is_known_source(source: &str) -> bool {
    matches!(
        source,
        "USER_EXPLICIT" | "USER_IMPLICIT" | "MODEL" | "SYSTEM" | "SYSTEM_SDK"
    )
}

/// Parse an Antigravity JSONL transcript from memory without filesystem access.
pub fn parse_str(id: &str, text: &str, source_path: Option<PathBuf>) -> Result<Session> {
    struct Collect(Vec<Message>);
    impl MessageSink for Collect {
        fn message(&mut self, m: Message) -> Flow {
            self.0.push(m);
            Flow::Continue
        }
    }
    let mut collect = Collect(Vec::new());
    let mut s = stream_str(id, text, source_path, &Demand::RICH, &mut collect)?;
    s.messages = collect.0;
    Ok(s)
}

/// Stream an in-memory Antigravity JSONL transcript into `sink`.
pub fn stream_str(
    id: &str,
    text: &str,
    source_path: Option<PathBuf>,
    opts: &Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    crate::stream::checked(sink, |sink| {
        let s = Session {
            source_path: source_path.clone(),
            ..Session::new(id, Harness::Antigravity)
        };
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Antigravity, sink);
        let mut codec = StepCodec {
            s,
            opts: *opts,
            spans: false,
            enrich: SidecarEnrich::default(),
            state: StepState::default(),
            source_path,
        };
        let mut order = StepOrder::default();
        jsonl::slice(text.as_bytes(), |line| {
            order.ingest(&line)?;
            Ok(Flow::Continue)
        })?;
        order
            .coverage
            .sources
            .insert(crate::coverage::ReadCoverage::EXPORTED_HISTORY.into());
        order
            .coverage
            .limitations
            .remove(crate::coverage::ReadCoverage::INFERRED_TOOL_IDENTITY);
        for (at, end) in order.steps.values() {
            jsonl::slice(&text.as_bytes()[*at as usize..*end as usize], |line| {
                if let Some(view) = line.view {
                    if codec.inferred_identity(view)? {
                        order
                            .coverage
                            .limitations
                            .insert(crate::coverage::ReadCoverage::INFERRED_TOOL_IDENTITY.into());
                    }
                }
                Ok(Flow::Continue)
            })?;
        }
        for (at, end) in order.steps.values().take(HEAD_STEPS as usize) {
            if codec.s.title.is_some() {
                break;
            }
            jsonl::slice(&text.as_bytes()[*at as usize..*end as usize], |line| {
                if let Some(view) = line.view {
                    peek_step_head_view(view, &mut codec.s.title, &mut None)?;
                }
                Ok(Flow::Continue)
            })?;
        }
        order.coverage.attach(&mut codec.s);
        spawns.meta(&codec.s);
        let mut stopped = false;
        for (at, end) in order.steps.into_values() {
            jsonl::slice(&text.as_bytes()[at as usize..end as usize], |line| {
                if let Some(view) = line.view {
                    stopped = codec.record(view, at, &mut spawns)? == Flow::Stop;
                }
                Ok(Flow::Continue)
            })?;
            if stopped {
                break;
            }
        }
        super::note_skipped_lines(codec.session(), order.skipped);
        codec.finish(stopped, &mut spawns)
    })
}

/// A surviving child descriptor is evidence of a lane even after its native
/// transcript has been removed. It carries state, never fabricated conversation text.
fn descriptor_parent(path: &Path) -> Option<String> {
    let descriptors = path.parent()?;
    let is_descriptor = descriptors.file_name()? == "subagents";
    let is_members = descriptors.file_name()? == "swarm" && path.file_name()? == "members.json";
    if !(is_descriptor || is_members) || descriptors.parent()?.file_name()? != ".system_generated" {
        return None;
    }
    descriptors.parent()?.parent()?.file_name()?.to_str().map(str::to_owned)
}

fn descriptor_app_data_dir(path: &Path) -> Option<PathBuf> {
    let brain_dir = path.parent()?.parent()?.parent()?;
    let brain_root = brain_dir.parent()?;
    (brain_root.file_name()?.to_str()? == "brain").then(|| brain_root.parent().map(Path::to_path_buf))?
}

/// Export completion order differs from step order. Keep the last revision of each
/// index, retaining only byte ranges; decode selected bodies through the same codec.
#[derive(Default)]
struct StepOrder {
    steps: BTreeMap<u64, (u64, u64)>,
    skipped: u64,
    coverage: crate::coverage::ReadCoverage,
    /// Reading a store on this machine: an exported media path can be checked.
    local: bool,
    /// Exported steps that link a media file which is no longer there.
    missing_media: BTreeSet<u64>,
}
impl StepOrder {
    fn ingest(&mut self, line: &jsonl::Line<'_>) -> Result<()> {
        use crate::coverage::ReadCoverage;
        let mut missing_media = false;
        if let Some(view) = line.view {
            if view.get("native_store")?.is_some() {
                self.coverage.sources.insert(ReadCoverage::NATIVE_STORE.into());
                if view.get("native_unsupported")?.is_some() {
                    self.coverage
                        .limitations
                        .insert(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS.into());
                }
                if view.get("native_missing_attachments")?.is_some() {
                    self.coverage
                        .limitations
                        .insert(ReadCoverage::MISSING_ATTACHMENTS.into());
                }
                if view.get("native_cleared")?.is_some() || view.get("native_missing_payloads")?.is_some() {
                    self.coverage.limitations.insert(ReadCoverage::MISSING_PAYLOADS.into());
                }
            } else {
                self.coverage.sources.insert(ReadCoverage::EXPORTED_HISTORY.into());
                if self.local {
                    if let Some(media) = view.get("media")?.map(JsonView::array).transpose()?.flatten() {
                        for item in media {
                            // An inline data URI can be megabytes; a path is short.
                            let Some(uri) = item?.get("uri")? else {
                                continue;
                            };
                            let uri = peek_view_prefix(uri, 4097)?;
                            missing_media |= uri.len() <= 4096 && attachment_missing(&uri);
                        }
                    }
                }
                if let Some(calls) = view.get("tool_calls")?.map(JsonView::array).transpose()?.flatten() {
                    for call in calls {
                        if call?
                            .get("id")?
                            .map(JsonView::string)
                            .transpose()?
                            .flatten()
                            .is_none_or(|id| id.is_empty())
                        {
                            self.coverage
                                .limitations
                                .insert(ReadCoverage::INFERRED_TOOL_IDENTITY.into());
                        }
                    }
                }
            }
        }
        let index = line
            .view
            .map(|view| {
                Ok::<_, anyhow::Error>(
                    view.get("step_index")?
                        .map(JsonView::as_number)
                        .transpose()?
                        .flatten()
                        .and_then(|n| n.as_u64()),
                )
            })
            .transpose()?
            .flatten();
        if let Some(index) = index {
            self.steps.insert(index, (line.at, line.end));
            // The last revision of a step decides.
            if missing_media {
                self.missing_media.insert(index);
            } else {
                self.missing_media.remove(&index);
            }
        } else {
            self.skipped += u64::from(!line.blank);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Layout & sidecar enrichment
// ---------------------------------------------------------------------------

struct BrainLoc {
    app_data_dir: PathBuf,
    brain_dir: PathBuf,
    id: String,
}

impl BrainLoc {
    /// Match `<appDataDir>/brain/<id>/.system_generated/logs/<filename>` or `<appDataDir>/conversations/<id>.db`.
    fn from_transcript(path: &Path) -> Option<Self> {
        if is_native_store(path) {
            let app_data_dir = path.parent()?.parent()?.to_path_buf();
            let id = path.file_stem()?.to_str()?.to_owned();
            return Some(Self {
                brain_dir: app_data_dir.join("brain").join(&id),
                app_data_dir,
                id,
            });
        }
        let logs_dir = match chunk_directory(path) {
            Some(directory) => directory.parent()?.parent()?,
            None => path.parent()?,
        };
        if logs_dir.file_name()?.to_str()? != "logs" {
            return None;
        }
        let sys_dir = logs_dir.parent()?;
        if sys_dir.file_name()?.to_str()? != ".system_generated" {
            return None;
        }
        let brain_dir = sys_dir.parent()?;
        let id = brain_dir.file_name()?.to_str()?.to_string();
        let brain_root = brain_dir.parent()?;
        if brain_root.file_name()?.to_str()? != "brain" {
            return None;
        }
        let app_data_dir = brain_root.parent()?.to_path_buf();
        Some(BrainLoc {
            app_data_dir,
            brain_dir: brain_dir.to_path_buf(),
            id,
        })
    }
}

/// Resolve the native conversation identity of an explicitly named store or brain export.
pub fn source_for_path(path: &Path) -> Option<(SessionRef, Antigravity)> {
    let loc = BrainLoc::from_transcript(path)?;
    Some((
        SessionRef::new(loc.id, Harness::Antigravity, path),
        Antigravity {
            roots: vec![loc.app_data_dir],
        },
    ))
}

fn select_session_source(root: &Path, id: &str) -> Option<PathBuf> {
    let log = select_brain_log(&root.join("brain").join(id));
    if let Some((rank, path)) = &log {
        if *rank > 0 {
            return Some(path.clone());
        }
    }
    #[cfg(feature = "sqlite")]
    {
        let path = root.join("conversations").join(format!("{id}.db"));
        if path.is_file() && native::has_steps(&path).unwrap_or(false) {
            return Some(path);
        }
    }
    log.map(|(_, path)| path)
}

fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let base = fs::metadata(path).ok().and_then(|m| m.modified().ok());
    #[cfg(feature = "sqlite")]
    if is_native_store(path) {
        let mut wal_os = path.as_os_str().to_os_string();
        wal_os.push("-wal");
        let wal = fs::metadata(Path::new(&wal_os))
            .ok()
            .filter(|m| m.len() > 0)
            .and_then(|m| m.modified().ok());
        return base.max(wal);
    }
    base
}

/// Prefer full history, including its byte-chunked copy, over the compact projection.
fn select_brain_log(brain_dir: &Path) -> Option<(u8, PathBuf)> {
    let logs = brain_dir.join(".system_generated").join("logs");
    let mut empty = None;
    for (rank, name) in [
        (4, "transcript_full.jsonl"),
        (3, "chunks/transcript_full/00000000.jsonl"),
        (2, "transcript.jsonl"),
        (2, "chunks/transcript/00000000.jsonl"),
        (1, "overview.txt"),
    ] {
        let path = logs.join(name);
        if let Ok(meta) = fs::metadata(&path) {
            if meta.is_file() {
                if meta.len() > 0 {
                    return Some((rank, path));
                }
                empty.get_or_insert((0, path));
            }
        }
    }
    empty
}

/// Chunks are fixed-size byte slices, not independent JSONL files: a split can
/// fall in a JSON escape or a UTF-8 character. Reassemble once in bounded backing.
fn chunk_directory(path: &Path) -> Option<&Path> {
    let directory = path.parent()?;
    (matches!(directory.file_name()?.to_str()?, "transcript" | "transcript_full")
        && directory.parent()?.file_name()? == "chunks"
        && directory.parent()?.parent()?.file_name()? == "logs")
        .then_some(directory)
}

fn is_native_store(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "db")
        && path
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "conversations"))
}

fn open_transcript(path: &Path) -> Result<jsonl::Read> {
    #[cfg(not(feature = "sqlite"))]
    anyhow::ensure!(
        !is_native_store(path),
        "reading native Antigravity stores requires SQLite support"
    );
    #[cfg(feature = "sqlite")]
    if is_native_store(path) {
        return native::open(path, false);
    }
    let Some(directory) = chunk_directory(path) else {
        return jsonl::Read::open(path);
    };
    let mut reads = ReadSet::default();
    reads.record(directory);
    let mut chunks = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let index = p
            .file_stem()
            .and_then(|x| x.to_str())
            .and_then(|x| x.parse::<u64>().ok())
            .context("invalid Antigravity chunk filename")?;
        anyhow::ensure!(
            chunks.insert(index, p.clone()).is_none(),
            "duplicate Antigravity chunk number"
        );
        reads.record(&p);
    }
    if chunks.is_empty() {
        fs::File::open(path)?;
        anyhow::bail!("empty Antigravity chunk history");
    }
    let input = jsonl::Read::with(crate::json::JsonSource::capture_with_reads(path, &reads)?)?;
    let arena = std::sync::Arc::new(super::store::bytes::ByteSource::arena()?);
    let (_, len) = arena.append(|output| {
        for (expected, (index, path)) in chunks.into_iter().enumerate() {
            anyhow::ensure!(index == expected as u64, "missing Antigravity chunk {expected}");
            std::io::copy(&mut fs::File::open(&path)?, output)?;
        }
        Ok(())
    })?;
    input.check()?;
    Ok(input.decoded(arena, len))
}

fn looks_like_antigravity_file(path: &Path) -> bool {
    let Ok(input) = jsonl::Read::open(path) else {
        return false;
    };
    let mut matched = false;
    let _ = input.prefix(64 * 1024, |line| {
        if let Some(view) = line.view {
            if is_step_record_view(view) {
                matched = true;
            }
            return Ok(Flow::Stop);
        }
        Ok(Flow::Continue)
    });
    matched
}

#[derive(Default)]
struct SidecarEnrich {
    #[cfg(feature = "sqlite")]
    verified: BTreeMap<u64, merge::Match>,
    #[cfg(feature = "sqlite")]
    repairs: BTreeMap<u64, Map<String, Value>>,
    has_native_db: bool,
    step_models: BTreeMap<u64, String>,
    known_children: BTreeSet<String>,
    subagent_descriptors: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
struct SubagentDescriptor {
    path: PathBuf,
    conversation_id: String,
    spawn_step_index: Option<u64>,
    state: Option<String>,
    type_name: Option<String>,
    role: Option<String>,
    workspace_uris: Vec<String>,
    /// The swarm registry's own record of this member: name, who added it, and the
    /// distinct phase, desired-state, wake-mode and killed fields.
    member: Map<String, Value>,
}

fn read_subagent_descriptors(brain_dir: &Path) -> Result<Vec<SubagentDescriptor>> {
    use std::io::Read as _;
    let sub_dir = brain_dir.join(".system_generated").join("subagents");
    let entries = super::side_entries(&sub_dir)?;
    let mut paths: Vec<PathBuf> = entries
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json") && p.is_file())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() > 1024 * 1024 {
            continue;
        }
        let mut text = String::new();
        if std::io::Read::take(file, 1024 * 1024 + 1)
            .read_to_string(&mut text)
            .is_err()
            || text.len() > 1024 * 1024
        {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(cid) = val
            .get("conversationId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        else {
            continue;
        };
        let spawn_step_index = val.get("spawnStepIndex").and_then(Value::as_u64);
        let state = val.get("state").and_then(Value::as_str).map(str::to_owned);
        let desc_obj = val.get("subagentDescriptor");
        let type_name = desc_obj
            .and_then(|d| d.get("typeName"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let role = desc_obj
            .and_then(|d| d.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let workspace_uris = val
            .get("workspaceUris")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        out.push(SubagentDescriptor {
            path,
            conversation_id: cid,
            spawn_step_index,
            state,
            type_name,
            role,
            workspace_uris,
            member: Map::new(),
        });
    }
    let own = brain_dir.file_name().and_then(|name| name.to_str());
    let members_path = brain_dir.join(".system_generated/swarm/members.json");
    if let Ok(source) = crate::json::JsonSource::capture(&members_path) {
        if source.len() <= 1024 * 1024 {
            let members = source.read(
                crate::json::ReadInput::Document,
                crate::json::Validation::Value,
                |view| Ok(view.resolve()?.into_owned()),
            );
            if let Ok(value) = members {
                if let Some(members) = value.get("members").and_then(Value::as_object) {
                    for (name, member) in members {
                        let Some(id) = member.get("conversation_id").and_then(Value::as_str) else {
                            continue;
                        };
                        // The registry lists its owner (the coordinator) among the members.
                        if Some(id) == own {
                            continue;
                        }
                        let mut record = Map::new();
                        record.insert("member".into(), Value::String(name.clone()));
                        for key in ["added_by", "phase", "desired_state", "wake_mode", "killed"] {
                            if let Some(value) = member.get(key) {
                                record.insert(key.into(), value.clone());
                            }
                        }
                        let killed = member.get("killed").and_then(Value::as_bool) == Some(true)
                            || member.get("phase").and_then(Value::as_str) == Some("KILLED");
                        if let Some(existing) = out.iter_mut().find(|d| d.conversation_id == id) {
                            if killed {
                                existing.state = Some("SUBAGENT_STATE_KILLED".into());
                            }
                            if existing.type_name.is_none() {
                                existing.type_name = Some(name.clone());
                            }
                            existing.member = record;
                            continue;
                        }
                        out.push(SubagentDescriptor {
                            path: members_path.clone(),
                            conversation_id: id.to_owned(),
                            spawn_step_index: None,
                            state: killed.then(|| "SUBAGENT_STATE_KILLED".into()),
                            type_name: Some(name.clone()),
                            role: member.get("role").and_then(Value::as_str).map(str::to_owned),
                            workspace_uris: Vec::new(),
                            member: record,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The catalog uses the adapter's topology, including not-yet-created child logs,
/// rather than a second unbounded JSON parser with a divergent list of sidecars.
#[cfg(feature = "sqlite")]
pub(crate) fn forest_dependencies(r: &SessionRef, guard: &mut ReadSet) {
    let Some(loc) = BrainLoc::from_transcript(&r.path) else {
        return;
    };
    let root = loc.app_data_dir.join("brain");
    let mut pending = vec![loc.brain_dir];
    let mut visited = BTreeSet::new();
    while let Some(brain) = pending.pop() {
        if !visited.insert(brain.clone()) {
            continue;
        }
        guard.record(&brain.join(".system_generated/subagents"));
        guard.record(&brain.join(".system_generated/swarm/members.json"));
        guard.record(&brain.join(".system_generated/swarm/board.jsonl"));
        let Ok(descriptors) = read_subagent_descriptors(&brain) else {
            continue;
        };
        for desc in descriptors {
            guard.record(&desc.path);
            if crate::publish::validate_output_id(&desc.conversation_id).is_err() {
                continue;
            }
            let child = root.join(&desc.conversation_id);
            let logs = child.join(".system_generated/logs");
            guard.record(&logs);
            for name in [
                "transcript_full.jsonl",
                "transcript.jsonl",
                "overview.txt",
                "chunks/transcript_full",
                "chunks/transcript",
            ] {
                guard.record(&logs.join(name));
            }
            pending.push(child);
        }
    }
}

/// Steps inspected for a title and a creation time.
const HEAD_STEPS: u32 = 8;

fn peek_step_head_view(
    view: JsonView<'_>,
    title: &mut Option<String>,
    created_at: &mut Option<DateTime<Utc>>,
) -> Result<Option<u64>> {
    let Some(fields) = Fields::parse(view, &["step_index", "type", "created_at", "content", "native_content"])? else {
        return Ok(None);
    };
    let idx = fields
        .get("step_index")
        .and_then(|v| v.as_number().ok().flatten())
        .and_then(|n| n.as_u64());
    if created_at.is_none() {
        if let Some(ts_str) = fields.string("created_at")? {
            *created_at = super::parse_ts(&ts_str);
        }
    }
    if title.is_none() {
        let ty = fields.string("type")?.unwrap_or_default();
        if ty == "USER_INPUT" || (ty == "SYSTEM_MESSAGE" && idx.is_some_and(|i| i <= 1)) {
            // The store keeps a typed request as items; the first is its text.
            let raw = match fields.string("content")? {
                Some(raw) => Some(raw),
                None => match fields.get("native_content").map(JsonView::array).transpose()?.flatten() {
                    Some(mut parts) => parts.next().transpose()?.map(JsonView::string).transpose()?.flatten(),
                    None => None,
                },
            };
            if let Some(raw) = raw {
                let body = if ty == "SYSTEM_MESSAGE" {
                    unwrap_system_message_body(&raw).unwrap_or(&raw)
                } else {
                    unwrap_user_request(&raw)
                };
                let cleaned = body.trim();
                // A swarm worker's log opens with a board wake; its brief follows.
                if !cleaned.is_empty() && board_wake(cleaned).is_none() {
                    let mut head = crate::projection::Head::bounded(crate::projection::Head::TITLE);
                    head.push(cleaned);
                    *title = Some(head.finish());
                }
            }
        }
    }
    Ok(idx)
}

/// A title from the first prompt (or a child's brief) and the first timestamp, each only
/// if still unset, from at most [`HEAD_STEPS`] leading steps. Returns the first step index.
fn peek_head(input: &jsonl::Read, title: &mut Option<String>, created_at: &mut Option<DateTime<Utc>>) -> Option<u64> {
    let mut inspected = 0;
    let mut first = None;
    let _ = input.prefix(64 * 1024, |line| {
        let Some(view) = line.view else {
            return Ok(Flow::Continue);
        };
        inspected += 1;
        let idx = peek_step_head_view(view, title, created_at)?;
        first = first.or(idx);
        if (created_at.is_some() && title.is_some()) || inspected >= HEAD_STEPS {
            return Ok(Flow::Stop);
        }
        Ok(Flow::Continue)
    });
    first
}

fn scan_transcript(id: &str, transcript: &Path, app_data_dir: Option<&Path>) -> Result<Option<SessionRef>> {
    #[cfg(feature = "sqlite")]
    if is_native_store(transcript) {
        if !native::has_steps(transcript).or_else(|err| {
            fs::File::open(transcript)?;
            Err(err)
        })? {
            return Ok(None);
        }
        let metadata = native::metadata(transcript, false).or_else(|err| {
            fs::File::open(transcript)?;
            Err(err)
        })?;
        let mut reference = SessionRef::new(id, Harness::Antigravity, transcript);
        reference.cwd = metadata.cwd;
        reference.created_at = metadata.created_at;
        reference.updated_at = fs::metadata(transcript)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(DateTime::from);
        if let Some(root) = app_data_dir {
            reference.title = read_pbtxt_title(&root.join("annotations").join(format!("{id}.pbtxt")));
        }
        if reference.title.as_deref().is_none_or(|title| title.trim().is_empty()) {
            reference.title = None;
            if let Ok(head) = native::head(transcript, HEAD_STEPS) {
                peek_head(&head, &mut reference.title, &mut reference.created_at);
            }
        }
        return Ok(Some(reference));
    }
    let meta = match fs::metadata(transcript) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mtime: Option<DateTime<Utc>> = meta.modified().ok().map(Into::into);

    // Only the store-backed build retitles from the first native prompt.
    #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))]
    let mut title = app_data_dir
        .and_then(|app| read_pbtxt_title(&app.join("annotations").join(format!("{id}.pbtxt"))))
        .filter(|t| !t.trim().is_empty());
    let mut cwd: Option<PathBuf> = None;
    let mut created_at: Option<DateTime<Utc>> = None;

    if let Some(app) = app_data_dir {
        let brain_dir = app.join("brain").join(id);
        if let Ok(descs) = read_subagent_descriptors(&brain_dir) {
            cwd = descs
                .iter()
                .flat_map(|d| d.workspace_uris.iter())
                .find_map(|u| file_uri_to_path(u));
        }
        #[cfg(feature = "sqlite")]
        {
            let conv_db = app.join("conversations").join(format!("{id}.db"));
            if conv_db.is_file() {
                if let Ok(db_meta) = native::metadata(&conv_db, false) {
                    if cwd.is_none() {
                        cwd = db_meta.cwd;
                    }
                    if created_at.is_none() {
                        created_at = db_meta.created_at;
                    }
                }
            }
        }
    }

    // Peek head of transcript for `created_at` and fallback prompt title without scanning the body.
    let mut prompt = None;
    let first = open_transcript(transcript)
        .ok()
        .and_then(|input| peek_head(&input, &mut prompt, &mut created_at));
    // A log re-created at a resume starts mid-conversation: the store has the first prompt.
    #[cfg(feature = "sqlite")]
    if title.is_none() && first != Some(0) {
        if let Some(store) = app_data_dir
            .map(|app| app.join("conversations").join(format!("{id}.db")))
            .filter(|store| store.is_file())
        {
            if let Ok(head) = native::head(&store, HEAD_STEPS) {
                peek_head(&head, &mut title, &mut None);
            }
        }
    }
    #[cfg(not(feature = "sqlite"))]
    let _ = first;
    let title = title.or(prompt);

    Ok(Some(SessionRef {
        id: id.to_string(),
        harness: Harness::Antigravity,
        path: transcript.to_path_buf(),
        cwd,
        title,
        created_at: created_at.or(mtime),
        updated_at: mtime.or(created_at),
        message_count: None,
    }))
}

fn build_session_header(r: &SessionRef, opts: &Demand) -> (Session, SidecarEnrich) {
    let mut s = Session {
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Antigravity)
    };
    let mut enrich = SidecarEnrich::default();
    #[cfg(feature = "sqlite")]
    if is_native_store(&r.path) {
        enrich.has_native_db = true;
    }

    let Some(loc) = BrainLoc::from_transcript(&r.path) else {
        return (s, enrich);
    };

    #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))]
    let mut annotations =
        read_pbtxt_annotations(&loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id)))
            .unwrap_or_default();
    let pbtxt_title = annotations.title.clone().filter(|t| !t.trim().is_empty());
    #[cfg(feature = "sqlite")]
    let has_pbtxt = pbtxt_title.is_some();
    if let Some(pbtxt_title) = pbtxt_title {
        s.title = Some(pbtxt_title);
    }

    let want_extra = opts.tier >= Tier::Rich;

    if let Ok(descs) = read_subagent_descriptors(&loc.brain_dir) {
        for d in descs {
            if s.cwd.is_none() {
                s.cwd = d.workspace_uris.iter().find_map(|u| file_uri_to_path(u));
            }
            enrich.known_children.insert(d.conversation_id.clone());
            if want_extra {
                let mut entry = Map::new();
                if let Some(step) = d.spawn_step_index {
                    entry.insert("spawnStepIndex".into(), Value::Number(step.into()));
                }
                if let Some(st) = d.state {
                    entry.insert("state".into(), Value::String(st));
                }
                if let Some(tn) = d.type_name {
                    entry.insert("typeName".into(), Value::String(tn));
                }
                if let Some(role) = d.role {
                    entry.insert("role".into(), Value::String(role));
                }
                entry.extend(d.member);
                enrich
                    .subagent_descriptors
                    .insert(d.conversation_id, Value::Object(entry));
            }
        }
    }

    #[cfg(feature = "sqlite")]
    {
        let conv_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
        if conv_db.is_file() {
            apply_native_db_metadata(&conv_db, want_extra, &mut s, &mut enrich);
        }

        let sum_db = loc.app_data_dir.join("conversation_summaries.db");
        if sum_db.is_file() {
            if let Ok(Some(sum)) = read_summary_one(&sum_db, &loc.id) {
                apply_summary_to_session(&mut s, &sum, has_pbtxt, want_extra);
                annotations.merge_fallback(&sum.annotations);
            }
        }
    }

    if want_extra {
        if let Some(ann_val) = annotations.to_extra_value() {
            s.harness_extra_mut(Harness::Antigravity)
                .insert("annotations".into(), ann_val);
        }
    }

    if s.lineage.parent.is_none() {
        s.lineage.parent = find_sidecar_parent(&loc.app_data_dir, &loc.id);
    }

    (s, enrich)
}

// ---------------------------------------------------------------------------
// Step JSONL Codec
// ---------------------------------------------------------------------------

const STEP_KEYS: &[&str] = &[
    "step_index",
    "source",
    "type",
    "status",
    "created_at",
    "content",
    "thinking",
    "tool_calls",
    "input_tokens",
    "cache_read_tokens",
    "output_tokens",
    "error",
    "media",
    "truncated_fields",
    "native_store",
    "native_cleared",
    "native_missing_payloads",
    "native_unsupported",
    "native_missing_attachments",
    "native_content",
    "native_agent_refs",
    "native_scope_items",
    "native_task_details",
    "native_planner_details",
    "native_checkpoint_details",
    "native_structured_result",
    "native_tool_id",
    "native_tool_name",
    "native_interaction",
    "cache_creation_tokens",
    "reasoning_tokens",
    "thinking_signature",
    "thinking_redacted",
    "native_output_uri",
    "native_payload",
];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PendingCall {
    id: String,
    name: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ScheduleSpec {
    max_fires: Option<u64>,
    fires_seen: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct StepState {
    pending_calls: VecDeque<PendingCall>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    schedule_calls: BTreeMap<String, Option<u64>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    scheduled_tasks: BTreeMap<String, ScheduleSpec>,
    child_ids: BTreeSet<String>,
    spawned_children: BTreeMap<String, String>,
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
    inferred_parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings_model: Option<String>,
    /// A prompt or a model turn has been read: later deliveries are not the opening brief.
    #[serde(default)]
    opened: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState<P = PathBuf, S = StepState> {
    source: P,
    state: S,
}

struct StepCodec {
    s: Session,
    opts: Demand,
    spans: bool,
    enrich: SidecarEnrich,
    state: StepState,
    source_path: Option<PathBuf>,
}

impl jsonl::Codec for StepCodec {
    fn demand(&self) -> Demand {
        self.opts
    }

    fn session(&mut self) -> &mut Session {
        &mut self.s
    }

    fn state(&self) -> impl serde::Serialize + '_ {
        ResumeState {
            source: self.source_path.as_deref().unwrap_or_else(|| Path::new("")),
            state: &self.state,
        }
    }

    fn record(&mut self, view: JsonView<'_>, at: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        struct Native<'a> {
            inner: &'a mut dyn MessageSink,
            record: Option<Json>,
        }
        impl MessageSink for Native<'_> {
            fn tier(&self) -> Tier {
                self.inner.tier()
            }
            fn message(&mut self, mut message: Message) -> Flow {
                // The synthesized compaction boundary is not another native record.
                if message.id.is_some() {
                    if let Some(record) = self.record.take() {
                        CompleteRecord::attach(
                            std::slice::from_mut(&mut message),
                            Harness::Antigravity,
                            record,
                            Map::new(),
                        );
                    }
                }
                self.inner.message(message)
            }
            fn take_error(&mut self) -> Option<anyhow::Error> {
                self.inner.take_error()
            }
        }
        let record = (self.opts.tier == Tier::Complete).then(|| view.to_json()).transpose()?;
        #[cfg(feature = "sqlite")]
        let repaired;
        #[cfg(feature = "sqlite")]
        let view = if let Some(fields) = view
            .get("step_index")?
            .and_then(|v| v.as_number().ok().flatten())
            .and_then(|n| n.as_u64())
            .and_then(|i| self.enrich.repairs.remove(&i))
        {
            let mut step = view.resolve()?.into_owned();
            step.as_object_mut()
                .context("invalid Antigravity export")?
                .extend(fields);
            repaired = step;
            JsonView::from(&repaired)
        } else {
            view
        };
        self.decode(view, at, &mut Native { inner: sink, record })
    }

    fn finish(self, stopped: bool, sink: &mut dyn MessageSink) -> Result<Session> {
        self.finish_session(stopped, sink)
    }
}

impl StepCodec {
    /// Assess the selected revisions using the same explicit/matched identity
    /// choices as decoding, before metadata is delivered to a streaming consumer.
    fn inferred_identity(&self, view: JsonView<'_>) -> Result<bool> {
        let index = view
            .get("step_index")?
            .map(JsonView::as_number)
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        #[cfg(feature = "sqlite")]
        let verified = self.enrich.verified.get(&index);
        #[cfg(not(feature = "sqlite"))]
        let _ = index;
        if view
            .get("type")?
            .map(JsonView::string)
            .transpose()?
            .flatten()
            .as_deref()
            .is_some_and(is_tool_step_type)
        {
            let explicit = view
                .get("native_tool_id")?
                .map(JsonView::string)
                .transpose()?
                .flatten()
                .is_some_and(|id| !id.is_empty());
            #[cfg(feature = "sqlite")]
            let explicit = explicit || verified.is_some_and(|m| m.result.is_some());
            return Ok(!explicit);
        }
        if let Some(calls) = view.get("tool_calls")?.map(JsonView::array).transpose()?.flatten() {
            for (at, call) in calls.enumerate() {
                let explicit = call?
                    .get("id")?
                    .map(JsonView::string)
                    .transpose()?
                    .flatten()
                    .is_some_and(|id| !id.is_empty());
                #[cfg(feature = "sqlite")]
                let explicit = explicit || verified.is_some_and(|m| m.calls.get(at).is_some());
                #[cfg(not(feature = "sqlite"))]
                let _ = at;
                if !explicit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn decode(&mut self, view: JsonView<'_>, _at: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        let Some(fields) = Fields::parse(view, STEP_KEYS)? else {
            return Ok(Flow::Continue);
        };
        let Some(ty) = fields.string("type")? else {
            return Ok(Flow::Continue);
        };
        let step_index = fields
            .get("step_index")
            .and_then(|v| v.as_number().ok().flatten())
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let source = fields.string("source")?.unwrap_or_default();
        let status = fields.string("status")?;
        let ts = fields.string("created_at")?.as_deref().and_then(super::parse_ts);

        if let Some(t) = ts {
            if self.state.first_ts.is_none() {
                self.state.first_ts = Some(t);
            }
            self.state.last_ts = Some(t);
        }

        // A new turn supersedes unfinished calls. Background deliveries can interleave
        // with parallel results and must not discard their pending identities.
        if matches!(
            ty.as_str(),
            "PLANNER_RESPONSE" | "USER_INPUT" | "CHECKPOINT" | "ERROR_MESSAGE"
        ) {
            self.state.pending_calls.clear();
            self.state.schedule_calls.clear();
        }
        if matches!(ty.as_str(), "PLANNER_RESPONSE" | "USER_INPUT") {
            self.state.opened = true;
        }

        let want_extra = self.opts.tier >= Tier::Rich;
        // Spans alone are not permission to discard media: public windows and
        // render/export sinks still request Rich. Reducers need presence and MIME.
        let media_marker = self.opts.spans
            && self.opts.tier != Tier::Complete
            && (matches!(self.opts.tier, Tier::Discovery | Tier::Usage | Tier::Facts) || sink.tier() <= Tier::Facts);
        match ty.as_str() {
            "USER_INPUT" => {
                if let Some(content_view) = fields.get("content") {
                    if let Some(model) = extract_view_settings_model(content_view)? {
                        self.state.settings_model = Some(model);
                    }
                }
                let origin = if self.s.lineage.parent.is_some() || self.state.inferred_parent.is_some() {
                    Origin::Subagent
                } else {
                    Origin::Human
                };
                let mut m = Message::of_kind(Role::User, MessageKind::Prompt, origin);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                if let Some(parts) = fields.get("native_content").map(JsonView::array).transpose()?.flatten() {
                    for part in parts {
                        push_nonempty_text(&mut m.content, Some(part?), self.spans)?;
                    }
                }
                push_media_blocks(&mut m.content, fields.get("media"), media_marker)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "PLANNER_RESPONSE" => {
                let mut m = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                let native_identity = fields.get("native_store").is_some();
                #[cfg(feature = "sqlite")]
                let native_identity = native_identity || self.enrich.verified.contains_key(&step_index);
                m.model = if native_identity {
                    self.enrich.step_models.get(&step_index).cloned()
                } else {
                    None
                };
                if m.model.is_none() && !self.enrich.has_native_db {
                    m.model = self.state.settings_model.clone();
                }

                if let Some(thinking_view) = fields.get("thinking") {
                    if let Some(text) = nonempty_view_text(thinking_view, self.spans)? {
                        m.content.push(Block::Thinking {
                            text,
                            signature: {
                                let signature = fields.string("thinking_signature")?;
                                #[cfg(feature = "sqlite")]
                                let signature = signature.or_else(|| {
                                    self.enrich.verified.get(&step_index).and_then(|m| m.signature.clone())
                                });
                                signature
                            },
                            encrypted: None,
                            redacted: fields
                                .value("thinking_redacted")?
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            extensions: Default::default(),
                        });
                    }
                }

                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                push_media_blocks(&mut m.content, fields.get("media"), media_marker)?;

                if let Some(calls) = fields.get("tool_calls").map(JsonView::array).transpose()?.flatten() {
                    for (k, item) in calls.enumerate() {
                        let item = item?;
                        let name = item
                            .get("name")?
                            .map(JsonView::string)
                            .transpose()?
                            .flatten()
                            .unwrap_or_default();
                        let input = item
                            .get("args")?
                            .map(|v| v.argument(self.spans))
                            .transpose()?
                            .unwrap_or_else(|| Json::Owned(json!({})));
                        let input = match item.get("arguments")? {
                            Some(arguments) => arguments.encoded_argument()?,
                            None => input,
                        };
                        let call_id = item
                            .get("id")?
                            .map(JsonView::string)
                            .transpose()?
                            .flatten()
                            .filter(|id| !id.is_empty())
                            .unwrap_or_else(|| derived_call_id("antigravity", format!("{step_index}.{k}")));
                        #[cfg(feature = "sqlite")]
                        let call_id = self
                            .enrich
                            .verified
                            .get(&step_index)
                            .and_then(|m| m.calls.get(k))
                            .cloned()
                            .unwrap_or(call_id);
                        if name == "schedule" {
                            self.state
                                .schedule_calls
                                .insert(call_id.clone(), schedule_max_fires(&input)?);
                        }
                        self.state.pending_calls.push_back(PendingCall {
                            id: call_id.clone(),
                            name: name.clone(),
                        });
                        m.content.push(Block::ToolUse {
                            id: call_id,
                            name,
                            input,
                            namespace: None,
                            extensions: Default::default(),
                        });
                    }
                }

                #[cfg(feature = "sqlite")]
                if want_extra {
                    if let Some(details) = self
                        .enrich
                        .verified
                        .get(&step_index)
                        .and_then(|m| m.planner_details.as_ref())
                    {
                        m.harness_extra_mut(Harness::Antigravity)
                            .insert("planner_details".into(), details.clone());
                    }
                }
                m.usage = parse_step_usage(&fields)?;
                #[cfg(feature = "sqlite")]
                if let Some(native) = self.enrich.verified.get(&step_index).and_then(|m| m.usage.as_ref()) {
                    let usage = m.usage.get_or_insert_with(Usage::default);
                    usage.cache_creation_tokens = native.cache_creation_tokens.or(usage.cache_creation_tokens);
                    usage.reasoning_tokens = native.reasoning_tokens.or(usage.reasoning_tokens);
                }
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                // The step is DONE either way; only the stop reason says the reply was cut off.
                if want_extra {
                    let bag = m.harness_extra_mut(Harness::Antigravity);
                    let stop = bag
                        .get("planner_details")
                        .and_then(|details| details.get("stop"))
                        .and_then(Value::as_str);
                    if matches!(stop, Some("error" | "client_canceled" | "client_stream_error")) {
                        bag.insert("interrupted".into(), Value::Bool(true));
                    }
                }
                Ok(sink.message(m))
            }
            t if is_tool_step_type(t) => {
                let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;

                let explicit_id = fields.string("native_tool_id")?.filter(|id| !id.is_empty());
                #[cfg(feature = "sqlite")]
                let explicit_id =
                    explicit_id.or_else(|| self.enrich.verified.get(&step_index).and_then(|m| m.result.clone()));
                // A native id cannot name a call whose own id was not recoverable (its planner
                // row was cleared, so the exported call has a derived id): such calls are
                // answered in order, which the planner step's inferred identity discloses.
                let derived = derived_call_id("antigravity", "");
                let selected = match &explicit_id {
                    Some(id) => match self.state.pending_calls.iter().position(|call| &call.id == id) {
                        Some(at) => self.state.pending_calls.remove(at),
                        None if self
                            .state
                            .pending_calls
                            .front()
                            .is_some_and(|call| call.id.starts_with(&derived)) =>
                        {
                            self.state.pending_calls.pop_front()
                        }
                        None => None,
                    },
                    None => self.state.pending_calls.pop_front(),
                };
                let (tool_use_id, tool_name) = match selected {
                    Some(call) => (call.id, Some(call.name)),
                    None => (
                        explicit_id.unwrap_or_else(|| derived_call_id("antigravity", step_index)),
                        fields.string("native_tool_name")?,
                    ),
                };

                // The export stops revising a step once it is written; the store's later
                // terminal status for the same call replaces a stale running one.
                #[cfg(feature = "sqlite")]
                let revised = self
                    .enrich
                    .verified
                    .get(&step_index)
                    .and_then(|m| m.revised.as_ref())
                    .filter(|native| {
                        matches!(
                            status.as_deref(),
                            Some("RUNNING" | "PENDING" | "WAITING" | "QUEUED" | "GENERATING")
                        ) && matches!(native.status.as_str(), "DONE" | "CANCELED" | "ERROR" | "INVALID")
                    });
                #[cfg(feature = "sqlite")]
                let status = revised.map(|native| native.status.clone()).or(status);
                let is_error = matches!(status.as_deref(), Some("ERROR" | "INVALID"));
                let norm_status = status.as_deref().map(normalize_step_status);

                // Inspect a bounded 64 KiB prefix of `content` for persisted-output paths and
                // `invoke_subagent` child conversationIds so multi-megabyte tool outputs stay in
                // their zero-copy source spans without truncating long preamble headers.
                const STEP_PEEK_BYTES: usize = 64 * 1024;
                let head = fields
                    .get("content")
                    .map(|v| peek_view_prefix(v, STEP_PEEK_BYTES))
                    .transpose()?
                    .unwrap_or_default();

                let persisted = fields
                    .string("native_output_uri")?
                    .and_then(|uri| file_uri_to_path(&uri))
                    .map(|path| path.to_string_lossy().into_owned())
                    .or_else(|| extract_persisted_output_path(&head));
                let mut details = persisted.map(|claimed| {
                    let resolved = self
                        .source_path
                        .as_deref()
                        .and_then(|transcript| spilled_output(transcript, &claimed));
                    json!({ "persistedOutput": Block::persisted_output(resolved, &claimed, None) })
                });
                let payload = fields.value("native_payload")?;
                #[cfg(feature = "sqlite")]
                let payload = payload.or_else(|| self.enrich.verified.get(&step_index).and_then(|m| m.payload.clone()));

                // The typed result names every child; the text names at most the first.
                let is_invoke_subagent = tool_name.as_deref() == Some("invoke_subagent") || ty == "INVOKE_SUBAGENT";
                let typed = payload
                    .as_ref()
                    .filter(|_| is_invoke_subagent)
                    .and_then(|payload| payload.get("conversation_ids")?.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|id| id.as_str().map(str::to_owned));
                let named: Vec<String> = if is_invoke_subagent {
                    extract_spawned_conversation_id(&head).into_iter().collect()
                } else if tool_name.as_deref() == Some("swarm") {
                    extract_swarm_added_ids(&head)
                } else {
                    Vec::new()
                };
                let spawned: Vec<_> = typed.chain(named).collect();
                if !spawned.is_empty() {
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("spawned_children".into(), json!(spawned));
                }
                for child_id in spawned {
                    self.state.child_ids.insert(child_id.clone());
                    self.state.spawned_children.insert(child_id, tool_use_id.clone());
                }
                let scheduled_call = self.state.schedule_calls.remove(&tool_use_id);
                let is_schedule = scheduled_call.is_some() || tool_name.as_deref() == Some("schedule");
                if is_schedule && !is_error && !matches!(status.as_deref(), Some("CANCELED")) {
                    let native_task_id = fields
                        .value("native_task_details")?
                        .and_then(|t| t.get("id")?.as_str().map(str::to_owned));
                    #[cfg(feature = "sqlite")]
                    let native_task_id = native_task_id.or_else(|| {
                        revised
                            .and_then(|native| native.task_details.as_ref())
                            .and_then(|t| t.get("id")?.as_str().map(str::to_owned))
                    });
                    if let Some(task_id) = lifecycle::background_job(&head)
                        .map(str::to_owned)
                        .or(native_task_id)
                        .filter(|id| !id.is_empty())
                    {
                        let max_fires = scheduled_call.flatten().or_else(|| {
                            payload
                                .as_ref()
                                .and_then(|p| p.get("mode")?.as_str())
                                .filter(|mode| matches!(*mode, "timer" | "one_shot" | "duration"))
                                .map(|_| 1)
                        });
                        self.state.scheduled_tasks.entry(task_id).or_insert(ScheduleSpec {
                            max_fires,
                            fires_seen: 0,
                        });
                    }
                }
                if let Some(payload) = payload {
                    details.get_or_insert_with(|| json!({}))["payload"] = payload;
                }
                let structured_result = fields.value("native_structured_result")?;
                #[cfg(feature = "sqlite")]
                let structured_result = structured_result.or_else(|| {
                    self.enrich
                        .verified
                        .get(&step_index)
                        .and_then(|m| m.structured_result.clone())
                });
                if let Some(structured) = structured_result {
                    details.get_or_insert_with(|| json!({}))["result"] = structured;
                }

                let error_text = fields.string("error")?.filter(|e| !e.is_empty());
                let content = match error_text {
                    Some(err) => {
                        let body = fields.string("content")?.unwrap_or_default();
                        if body.is_empty() || err.contains(&body) {
                            Text::Inline(err)
                        } else if body.contains(&err) {
                            fields
                                .get("content")
                                .map(|v| v.text(self.spans))
                                .transpose()?
                                .flatten()
                                .unwrap_or_else(|| Text::Inline(body))
                        } else {
                            Text::Inline(format!("{body}\n{err}"))
                        }
                    }
                    None => fields
                        .get("content")
                        .map(|v| v.text(self.spans))
                        .transpose()?
                        .flatten()
                        .unwrap_or_else(|| Text::Inline(String::new())),
                };

                m.content.push(Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    tool_name,
                    status: norm_status,
                    details,
                    extensions: Default::default(),
                });
                push_media_blocks(&mut m.content, fields.get("media"), media_marker)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                if want_extra && ty != "GENERIC" {
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("step_type".into(), Value::String(ty));
                }
                // Job identity and a permission answer drive lifecycle and affordance
                // decoding at every tier; non-permission interactions attach on Rich+.
                let interaction = fields.value("native_interaction")?;
                #[cfg(feature = "sqlite")]
                let interaction = interaction.or_else(|| {
                    self.enrich
                        .verified
                        .get(&step_index)
                        .and_then(|m| m.interaction.clone())
                });
                if let Some(interaction) = interaction {
                    if want_extra || interaction.get("kind").and_then(Value::as_str) == Some("permission") {
                        m.harness_extra_mut(Harness::Antigravity)
                            .insert("interaction".into(), interaction);
                    }
                }
                let task = fields.value("native_task_details")?;
                #[cfg(feature = "sqlite")]
                let task = task
                    .or_else(|| revised.and_then(|native| native.task_details.clone()))
                    .or_else(|| {
                        self.enrich
                            .verified
                            .get(&step_index)
                            .and_then(|m| m.task_details.clone())
                    });
                if let Some(task) = task {
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("task_details".into(), task);
                }
                Ok(sink.message(m))
            }
            "EPHEMERAL_MESSAGE" => {
                let mut m = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                push_media_blocks(&mut m.content, fields.get("media"), media_marker)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "CONVERSATION_HISTORY" => {
                let mut content = Vec::new();
                push_nonempty_text(&mut content, fields.get("content"), self.spans)?;
                push_media_blocks(&mut content, fields.get("media"), media_marker)?;
                if !content.is_empty() {
                    let mut m = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness);
                    m.id = Some(step_index.to_string());
                    m.timestamp = ts;
                    m.content = content;
                    attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                    return Ok(sink.message(m));
                }
                if self.opts.tier == Tier::Complete {
                    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
                    m.id = Some(step_index.to_string());
                    m.timestamp = ts;
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("record_type".into(), Value::String(ty));
                    return Ok(sink.message(m));
                }
                Ok(Flow::Continue)
            }
            "SYSTEM_MESSAGE" => {
                const SYS_PEEK_BYTES: usize = 64 * 1024;
                let head = fields
                    .get("content")
                    .map(|v| peek_view_prefix(v, SYS_PEEK_BYTES))
                    .transpose()?
                    .unwrap_or_default();
                let sender = extract_system_message_sender(&head);
                let delivered = unwrap_system_message_body(&head);
                let board = delivered.and_then(board_wake);
                let returned = delivered
                    .and_then(idle_child)
                    .map(|(child, _)| child)
                    .filter(|child| self.state.child_ids.contains(*child))
                    .or_else(|| sender.as_deref().filter(|id| self.state.child_ids.contains(*id)))
                    .map(str::to_owned);

                let task_notice = sender.as_deref().filter(|id| id.contains("/task-")).map(|task| {
                    let finished = delivered
                        .is_some_and(|body| body.starts_with(&format!("Task id \"{task}\" finished with result:")));
                    let canceled =
                        delivered.is_some_and(|body| body.starts_with(&format!("Task id \"{task}\" was canceled")));
                    if finished {
                        ("completed_task", task.to_owned(), false, false)
                    } else if canceled {
                        ("canceled_task", task.to_owned(), false, false)
                    } else if let Some(spec) = self.state.scheduled_tasks.get_mut(task) {
                        spec.fires_seen += 1;
                        let settled = spec.max_fires == Some(spec.fires_seen);
                        ("task_sender", task.to_owned(), true, settled)
                    } else {
                        ("task_sender", task.to_owned(), false, false)
                    }
                });
                let scheduled_fire = task_notice.as_ref().is_some_and(|(_, _, fire, _)| *fire);

                let (role, kind, origin) = if board.is_some() {
                    // A board post wakes every member it concerns; it is neither the
                    // worker's brief nor a child's return.
                    (Role::System, MessageKind::InjectedContext, Origin::Harness)
                } else if !self.state.opened
                    && sender
                        .as_deref()
                        .is_some_and(|s_id| looks_like_uuid(s_id) && s_id != self.s.id)
                {
                    // The first delivery from another conversation, before any turn, is
                    // the brief this one was started with.
                    self.state.opened = true;
                    let parent = sender.clone().unwrap();
                    if self.state.inferred_parent.is_none() {
                        self.state.inferred_parent = Some(parent);
                    }
                    (Role::User, MessageKind::Prompt, Origin::Subagent)
                } else if returned.is_some() {
                    (Role::User, MessageKind::SubagentReturn, Origin::Subagent)
                } else if scheduled_fire || sender.as_deref().is_some_and(|s_id| s_id.contains("timer")) {
                    (Role::System, MessageKind::InjectedContext, Origin::Scheduler)
                } else {
                    (Role::System, MessageKind::InjectedContext, Origin::Harness)
                };

                let mut m = Message::of_kind(role, kind, origin);
                if let Some((key, task, _, settled_schedule)) = task_notice {
                    let bag = m.harness_extra_mut(Harness::Antigravity);
                    bag.insert(key.into(), Value::String(task.clone()));
                    if settled_schedule {
                        bag.insert("completed_schedule".into(), Value::String(task));
                    }
                }
                if let Some(seq) = board {
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("board_message".into(), Value::Number(seq.into()));
                }
                if let Some(child) = returned.filter(|_| kind == MessageKind::SubagentReturn) {
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("returned_child".into(), Value::String(child));
                }
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                push_media_blocks(&mut m.content, fields.get("media"), media_marker)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                if want_extra {
                    if let Some(sender) = sender {
                        m.harness_extra_mut(Harness::Antigravity)
                            .insert("sender".into(), Value::String(sender));
                    }
                }
                Ok(sink.message(m))
            }
            "CHECKPOINT" => {
                let mut boundary = Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness);
                boundary.timestamp = ts;
                if sink.message(boundary) == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                let mut m = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                #[cfg(feature = "sqlite")]
                if want_extra {
                    if let Some(matched) = self.enrich.verified.get(&step_index) {
                        if let Some(details) = matched.planner_details.as_ref() {
                            m.harness_extra_mut(Harness::Antigravity)
                                .insert("planner_details".into(), details.clone());
                        }
                        if let Some(details) = matched.checkpoint_details.as_ref() {
                            m.harness_extra_mut(Harness::Antigravity)
                                .insert("checkpoint_details".into(), details.clone());
                        }
                    }
                }
                m.usage = parse_step_usage(&fields)?;
                #[cfg(feature = "sqlite")]
                if let Some(native) = self.enrich.verified.get(&step_index).and_then(|m| m.usage.as_ref()) {
                    let usage = m.usage.get_or_insert_with(Usage::default);
                    usage.cache_creation_tokens = native.cache_creation_tokens.or(usage.cache_creation_tokens);
                    usage.reasoning_tokens = native.reasoning_tokens.or(usage.reasoning_tokens);
                }
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "ERROR_MESSAGE" => {
                let mut m = Message::of_kind(Role::System, MessageKind::Error, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                let err = fields.string("error")?.unwrap_or_default();
                let body = fields.string("content")?.unwrap_or_default();
                let combined = match (err.is_empty(), body.is_empty()) {
                    (false, false) if err.contains(&body) => Some(err),
                    (false, false) if body.contains(&err) => Some(body),
                    (false, false) => Some(format!("{err}\n{body}")),
                    (false, true) => Some(err),
                    (true, false) => Some(body),
                    (true, true) => None,
                };
                if let Some(text) = combined {
                    m.content.push(Block::from_text(Text::Inline(text)));
                }
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            _ => {
                if self.opts.tier == Tier::Complete {
                    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
                    m.id = Some(step_index.to_string());
                    m.timestamp = ts;
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("record_type".into(), Value::String(ty));
                    return Ok(sink.message(m));
                }
                Ok(Flow::Continue)
            }
        }
    }

    fn finish_session(mut self, _stopped: bool, _sink: &mut dyn MessageSink) -> Result<Session> {
        if self.s.created_at.is_none() {
            self.s.created_at = self.state.first_ts;
        }
        if let Some(last) = self.state.last_ts {
            if self.s.updated_at.is_none_or(|prev| last > prev) {
                self.s.updated_at = Some(last);
            }
        }
        if self.s.model.is_none() && !self.enrich.has_native_db {
            self.s.model = self.state.settings_model;
        }
        let sender_parent = self.state.inferred_parent.clone();
        if self.s.lineage.parent.is_none() {
            self.s.lineage.parent = self.state.inferred_parent;
        }
        if let Some(loc) = self.source_path.as_deref().and_then(BrainLoc::from_transcript) {
            enrich_child_lineage(
                &mut self.s,
                &loc.app_data_dir,
                sender_parent.as_deref(),
                self.opts.tier >= Tier::Rich,
            );
        }
        if self.opts.tier >= Tier::Rich
            && (!self.enrich.subagent_descriptors.is_empty() || !self.state.spawned_children.is_empty())
        {
            let mut merged = self.enrich.subagent_descriptors;
            for (cid, call_id) in self.state.spawned_children {
                let entry = merged.entry(cid).or_insert_with(|| Value::Object(Map::new()));
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("call_id".into(), Value::String(call_id));
                }
            }
            self.s
                .harness_extra_mut(Harness::Antigravity)
                .insert("subagents".into(), Value::Object(merged.into_iter().collect()));
        }
        Ok(self.s)
    }
}

fn schedule_max_fires(input: &Json) -> Result<Option<u64>> {
    input.with_view(|view| {
        if view
            .get("DurationSeconds")?
            .map(JsonView::as_number)
            .transpose()?
            .flatten()
            .is_some()
        {
            return Ok(Some(1));
        }
        Ok(view
            .get("MaxIterations")?
            .map(JsonView::as_number)
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64())
            .filter(|n| *n > 0))
    })
}

fn normalize_step_status(status: &str) -> String {
    match status {
        "DONE" => "completed".to_string(),
        "RUNNING" => "running".to_string(),
        "ERROR" => "error".to_string(),
        "INVALID" => "invalid".to_string(),
        "CANCELED" => "canceled".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

fn parse_step_usage(fields: &Fields<'_>) -> Result<Option<Usage>> {
    let u64_field = |k: &str| -> Option<u64> {
        fields
            .get(k)
            .and_then(|v| v.as_number().ok().flatten())
            .and_then(|n| n.as_u64())
    };
    let input_tokens = u64_field("input_tokens");
    let output_tokens = u64_field("output_tokens");
    let cache_read_tokens = u64_field("cache_read_tokens");
    let usage = Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens: u64_field("cache_creation_tokens"),
        reasoning_tokens: u64_field("reasoning_tokens"),
        ..Default::default()
    };
    Ok((!usage.is_empty()).then_some(usage))
}

fn nonempty_view_text(view: JsonView<'_>, spans: bool) -> Result<Option<Text>> {
    let mut non_empty = false;
    let is_str = view.for_each_str_chunk(|chunk| {
        if !chunk.is_empty() {
            non_empty = true;
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    });
    match is_str {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(e) if e.is::<crate::stream::Stop>() => {}
        Err(e) => return Err(e),
    }
    if !non_empty {
        return Ok(None);
    }
    view.text(spans)
}

fn push_nonempty_text(blocks: &mut Vec<Block>, view: Option<JsonView<'_>>, spans: bool) -> Result<()> {
    if let Some(view) = view {
        if let Some(text) = nonempty_view_text(view, spans)? {
            blocks.push(Block::from_text(text));
        }
    }
    Ok(())
}

fn is_inline_base64(uri: JsonView<'_>) -> Result<bool> {
    if peek_view_prefix(uri, 5)? != "data:" {
        return Ok(false);
    }
    let mut suffix = [0u8; 7];
    let mut encoded = false;
    crate::stream::stopped(
        uri.for_each_str_chunk(|chunk| {
            for byte in chunk.bytes() {
                if byte == b',' {
                    encoded = suffix == *b";base64";
                    return Err(crate::stream::Stop.into());
                }
                suffix.copy_within(1.., 0);
                suffix[6] = byte;
            }
            Ok(())
        })
        .map(|_| ()),
    )?;
    Ok(encoded)
}

fn push_media_blocks(blocks: &mut Vec<Block>, media_view: Option<JsonView<'_>>, marker: bool) -> Result<()> {
    let Some(items) = media_view.map(JsonView::array).transpose()?.flatten() else {
        return Ok(());
    };
    for item in items {
        let item = item?;
        let mime = item.get("mime_type")?.map(JsonView::string).transpose()?.flatten();
        let uri_view = item.get("uri")?;
        if marker {
            if let Some(uri) = uri_view {
                if is_inline_base64(uri)? {
                    blocks.push(if mime.as_deref().is_some_and(|m| m.starts_with("image/")) {
                        Block::Image {
                            media_type: mime,
                            data_ref: Some("base64:inline".into()),
                            extensions: Default::default(),
                        }
                    } else {
                        Block::File {
                            mime,
                            path: None,
                            source: Some("base64:inline".into()),
                            extensions: Default::default(),
                        }
                    });
                    continue;
                }
            }
        }
        let uri = uri_view.map(JsonView::string).transpose()?.flatten();
        let Some(raw_uri) = uri.filter(|u| !u.is_empty()) else {
            continue;
        };
        let resolved = file_uri_to_path(&raw_uri)
            .map(|p| p.display().to_string())
            .unwrap_or(raw_uri);
        if mime.as_deref().is_some_and(|m| m.starts_with("image/")) {
            blocks.push(Block::Image {
                media_type: mime,
                data_ref: Some(resolved),
                extensions: Default::default(),
            });
        } else {
            blocks.push(Block::File {
                mime,
                path: Some(resolved),
                source: None,
                extensions: Default::default(),
            });
        }
    }
    Ok(())
}

fn attach_step_extras(
    m: &mut Message,
    fields: &Fields<'_>,
    step_index: u64,
    source: &str,
    status: Option<&str>,
    want_extra: bool,
) -> Result<()> {
    if !want_extra {
        return Ok(());
    }
    let bag = m.harness_extra_mut(Harness::Antigravity);
    bag.insert("step_index".into(), Value::Number(step_index.into()));
    if !source.is_empty() && is_known_source(source) {
        bag.insert("source".into(), Value::String(source.to_string()));
    }
    if let Some(st) = status {
        bag.insert("status".into(), Value::String(st.to_string()));
    }
    for (field, key) in [
        ("native_planner_details", "planner_details"),
        ("native_checkpoint_details", "checkpoint_details"),
        ("native_scope_items", "scope_items"),
        ("native_task_details", "task_details"),
    ] {
        if let Some(value) = fields.value(field)? {
            bag.insert(key.into(), value);
        }
    }
    if let Some(references) = fields.value("native_agent_refs")? {
        bag.insert("agent_references".into(), references);
    }
    if let Some(trunc) = fields.value("truncated_fields")? {
        if trunc.as_array().is_some_and(|a| !a.is_empty()) {
            bag.insert("truncated_fields".into(), trunc);
        }
    }
    Ok(())
}

fn peek_view_prefix(view: JsonView<'_>, max_bytes: usize) -> Result<String> {
    let mut out = String::new();
    let res = view.for_each_str_chunk(|chunk| {
        let rem = max_bytes.saturating_sub(out.len());
        if rem == 0 {
            return Err(crate::stream::Stop.into());
        }
        if chunk.len() <= rem {
            out.push_str(chunk);
        } else {
            let end = chunk.floor_char_boundary(rem);
            out.push_str(&chunk[..end]);
            return Err(crate::stream::Stop.into());
        }
        if out.len() >= max_bytes {
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    });
    match res {
        Ok(_) => Ok(out),
        Err(e) if e.is::<crate::stream::Stop>() => Ok(out),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Text & sidecar parsing helpers
// ---------------------------------------------------------------------------

/// The file a truncation notice names, inside the conversation it belongs to. Antigravity spills
/// a step's full output under `brain/<id>/.system_generated/`; a claim is resolved against this
/// conversation's directory, or a sibling conversation's by the same shape, and nowhere else.
fn spilled_output(transcript: &Path, claimed: &str) -> Option<PathBuf> {
    let brain_dir = BrainLoc::from_transcript(transcript)?.brain_dir;
    let generated = |scope: &Path, at: usize| {
        let path = crate::source::contained_sidecar(scope, claimed)?;
        let mut inside = path.strip_prefix(scope).ok()?.components();
        (inside.nth(at)?.as_os_str() == ".system_generated" && inside.next().is_some()).then_some(path)
    };
    generated(&brain_dir, 0).or_else(|| generated(brain_dir.parent()?, 1))
}

fn extract_persisted_output_path(head: &str) -> Option<String> {
    // A tool can read another transcript containing this notice as escaped JSON.
    // Only a standalone harness notice points to this result's persisted output;
    // searching anywhere in the body mistakes quoted previews for filenames.
    const MARKER: &str =
        "The tool's output was truncated due to being too long. You can find the full output in a file at ";
    let rest = head.lines().find_map(|line| line.strip_prefix(MARKER))?;
    let token = rest.trim().trim_end_matches('.');
    if token.is_empty() {
        return None;
    }
    Some(
        file_uri_to_path(token)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| token.to_string()),
    )
}

fn find_conversation_id_in_value(val: &Value) -> Option<String> {
    match val {
        Value::Object(map) => {
            if let Some(id) = map
                .get("conversationId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return Some(id.to_owned());
            }
            for child in map.values() {
                if let Some(id) = find_conversation_id_in_value(child) {
                    return Some(id);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_conversation_id_in_value),
        _ => None,
    }
}

fn extract_spawned_conversation_id(head: &str) -> Option<String> {
    let trimmed = head.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        if let Some(id) = find_conversation_id_in_value(&val) {
            return Some(id);
        }
    }
    // Tool outputs often prefix or suffix JSON objects with human-readable status lines:
    // stream-parse any top-level JSON object embedded in `head` before falling back to a
    // JSON-decoded string scan if the object was truncated at the 64 KiB peek boundary.
    for (idx, ch) in head.char_indices() {
        if ch == '{' {
            let mut stream = serde_json::Deserializer::from_str(&head[idx..]).into_iter::<Value>();
            if let Some(Ok(val)) = stream.next() {
                if let Some(id) = find_conversation_id_in_value(&val) {
                    return Some(id);
                }
            }
        }
    }
    // Truncated JSON fallback: decode the JSON string value properly with serde_json so escapes
    // (`\"`, `\\`, `\uXXXX`) are handled and prose mentions without `"conversationId": "..."` fail.
    const KEY: &str = "\"conversationId\"";
    let mut search = head;
    while let Some(idx) = search.find(KEY) {
        let after_key = &search[idx + KEY.len()..];
        if let Some(after_colon) = after_key.trim_start().strip_prefix(':') {
            let val_start = after_colon.trim_start();
            if val_start.starts_with('"') {
                let mut stream = serde_json::Deserializer::from_str(val_start).into_iter::<String>();
                if let Some(Ok(id)) = stream.next() {
                    if !id.is_empty() {
                        return Some(id);
                    }
                }
            }
        }
        search = after_key;
    }
    None
}

fn extract_system_message_sender(head: &str) -> Option<String> {
    let msg_idx = head.find("[Message]")?;
    let line = head[msg_idx..].lines().next()?;
    for part in line.split_whitespace() {
        if let Some(sender) = part.strip_prefix("sender=") {
            if !sender.is_empty() {
                return Some(sender.to_string());
            }
        }
    }
    None
}

/// The exported form is `<preamble>\n\n<SYSTEM_MESSAGE>\n[Message] … content=<body>\n
/// </SYSTEM_MESSAGE>`; the native store keeps `[Message] … content=<body>` alone. The
/// preamble itself names the tag ("The following is a <SYSTEM_MESSAGE> not actually sent
/// by the user"), so the envelope is the tag after it.
fn unwrap_system_message_body(text: &str) -> Option<&str> {
    const PREAMBLE: &str = "The following is a <SYSTEM_MESSAGE> not actually sent by the user.";
    let text = text.trim_start();
    let enveloped = text.strip_prefix(PREAMBLE).unwrap_or(text);
    let inner = match enveloped.find("<SYSTEM_MESSAGE>") {
        Some(open) => {
            let inner = &enveloped[open + "<SYSTEM_MESSAGE>".len()..];
            inner.strip_suffix("</SYSTEM_MESSAGE>").unwrap_or_else(|| {
                inner
                    .rfind("</SYSTEM_MESSAGE>")
                    .map(|end| &inner[..end])
                    .unwrap_or(inner)
            })
        }
        None if text.starts_with("[Message]") => text,
        None => return None,
    };
    let inner = inner.trim();
    if let Some(rest) = inner.strip_prefix("[Message]") {
        if let Some(c_idx) = rest.find(" content=") {
            return Some(rest[c_idx + " content=".len()..].trim());
        }
    }
    Some(inner)
}

/// The typed request inside the export's `<USER_REQUEST>` envelope. The writer appends
/// `<ADDITIONAL_METADATA>` and settings notices after the closing tag.
fn exported_user_request(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix("<USER_REQUEST>")?;
    Some(
        inner
            .split_once("</USER_REQUEST>")
            .map_or(inner, |(request, _)| request)
            .trim(),
    )
}

/// CLI 1.5.9 reports a child's reply through the system sender:
/// `Subagent <conversation> has gone idle. This was the subagent's most recent response: <reply>`.
fn idle_child(body: &str) -> Option<(&str, &str)> {
    let (id, rest) = body.strip_prefix("Subagent ")?.split_once(" has gone idle.")?;
    if !looks_like_uuid(id) {
        return None;
    }
    let response = rest
        .strip_prefix(" This was the subagent's most recent response: ")
        .unwrap_or(rest);
    Some((id, response.trim()))
}

/// `[swarm] new board message #<seq>`: a wake for a post on the shared board. Its sender
/// field names a member, but not reliably the one who posted.
fn board_wake(body: &str) -> Option<u64> {
    let seq = body.strip_prefix("[swarm] new board message #")?;
    let digits = seq.len() - seq.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    seq[..digits].parse().ok()
}

/// Conversations a `swarm` call added: `{"status":"added",…,"conversation_id":"<id>"}`
/// (plus sharded `conversation_ids` / `members` forms).
pub(super) fn extract_swarm_added_ids(head: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push_id = |id: &str| {
        if looks_like_uuid(id) && !out.iter().any(|existing| existing == id) {
            out.push(id.to_owned());
        }
    };
    let collect_from = |value: &Value, push_id: &mut dyn FnMut(&str)| {
        if value.get("status").and_then(Value::as_str) != Some("added") {
            return;
        }
        if let Some(id) = value.get("conversation_id").and_then(Value::as_str) {
            push_id(id);
        }
        if let Some(ids) = value.get("conversation_ids").and_then(Value::as_array) {
            for id in ids.iter().filter_map(Value::as_str) {
                push_id(id);
            }
        }
        match value.get("members") {
            Some(Value::Array(items)) => {
                for item in items {
                    if let Some(id) = item.get("conversation_id").and_then(Value::as_str) {
                        push_id(id);
                    }
                }
            }
            Some(Value::Object(map)) => {
                for item in map.values() {
                    if let Some(id) = item.get("conversation_id").and_then(Value::as_str) {
                        push_id(id);
                    }
                }
            }
            _ => {}
        }
    };
    for line in head.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('{') {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                collect_from(&value, &mut push_id);
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
struct SwarmMember {
    name: String,
    conversation_id: String,
    added_by: Option<String>,
    record: Map<String, Value>,
}

#[derive(Debug, Clone)]
struct SwarmRegistry {
    coordinator_id: String,
    title: Option<String>,
    by_name: BTreeMap<String, SwarmMember>,
    name_by_id: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct SwarmBoard {
    brief_titles: BTreeMap<String, String>,
    last_summaries: BTreeMap<String, String>,
}

const MAX_BOARD_BYTES: u64 = 4 * 1024 * 1024;

fn read_swarm_board(brain_dir: &Path) -> SwarmBoard {
    use std::io::Read as _;
    let mut out = SwarmBoard::default();
    let board_path = brain_dir.join(".system_generated/swarm/board.jsonl");
    let Ok(file) = fs::File::open(&board_path) else {
        return out;
    };
    let Ok(meta) = file.metadata() else {
        return out;
    };
    if !meta.is_file() || meta.len() > MAX_BOARD_BYTES {
        return out;
    }
    let mut text = String::new();
    if file.take(MAX_BOARD_BYTES + 1).read_to_string(&mut text).is_err() || text.len() as u64 > MAX_BOARD_BYTES {
        return out;
    }
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = val.get("kind").and_then(Value::as_str);
        let Some(msg_text) = val.get("text").and_then(Value::as_str) else {
            continue;
        };
        if kind == Some("brief") {
            let trimmed_text = msg_text.trim_start();
            if let Some(after_at) = trimmed_text.strip_prefix('@') {
                let end = after_at.find(char::is_whitespace).unwrap_or(after_at.len());
                let member = &after_at[..end];
                let body = after_at[end..].trim();
                if !member.is_empty() && !body.is_empty() {
                    let mut head = crate::projection::Head::bounded(crate::projection::Head::TITLE);
                    head.push(body);
                    out.brief_titles
                        .entry(member.to_owned())
                        .or_insert_with(|| head.finish());
                }
            }
        } else if let Some(agent) = val.get("agent").and_then(Value::as_str) {
            let body = msg_text.trim();
            if !agent.is_empty() && !body.is_empty() {
                out.last_summaries.insert(agent.to_owned(), body.to_owned());
            }
        }
    }
    out
}

fn read_swarm_title(brain_dir: &Path) -> Option<String> {
    let members_path = brain_dir.join(".system_generated/swarm/members.json");
    let source = crate::json::JsonSource::capture(&members_path).ok()?;
    if source.len() > 1024 * 1024 {
        return None;
    }
    source
        .read(
            crate::json::ReadInput::Document,
            crate::json::Validation::Value,
            |view| {
                Ok(view
                    .get("title")?
                    .map(JsonView::string)
                    .transpose()?
                    .flatten()
                    .map(|t| t.trim().to_owned())
                    .filter(|t| !t.is_empty()))
            },
        )
        .ok()
        .flatten()
}

fn read_swarm_registry(brain_dir: &Path) -> Option<SwarmRegistry> {
    let coordinator_id = brain_dir.file_name()?.to_str()?.to_owned();
    let members_path = brain_dir.join(".system_generated/swarm/members.json");
    let source = crate::json::JsonSource::capture(&members_path).ok()?;
    if source.len() > 1024 * 1024 {
        return None;
    }
    let value = source
        .read(
            crate::json::ReadInput::Document,
            crate::json::Validation::Value,
            |view| Ok(view.resolve()?.into_owned()),
        )
        .ok()?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let members = value.get("members")?.as_object()?;
    let mut by_name = BTreeMap::new();
    let mut name_by_id = BTreeMap::new();
    for (name, member) in members {
        let Some(id) = member
            .get("conversation_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mut record = Map::new();
        record.insert("member".into(), Value::String(name.clone()));
        for key in ["added_by", "phase", "desired_state", "wake_mode", "killed"] {
            if let Some(val) = member.get(key) {
                record.insert(key.into(), val.clone());
            }
        }
        let added_by = member
            .get("added_by")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        name_by_id.insert(id.to_owned(), name.clone());
        by_name.insert(
            name.clone(),
            SwarmMember {
                name: name.clone(),
                conversation_id: id.to_owned(),
                added_by,
                record,
            },
        );
    }
    (!by_name.is_empty()).then_some(SwarmRegistry {
        coordinator_id,
        title,
        by_name,
        name_by_id,
    })
}

fn find_swarm_registry(app_data_dir: &Path, session_id: &str, parent_hint: Option<&str>) -> Option<SwarmRegistry> {
    let brain_root = app_data_dir.join("brain");
    let check_brain = |id: &str| -> Option<SwarmRegistry> {
        let reg = read_swarm_registry(&brain_root.join(id))?;
        reg.name_by_id.contains_key(session_id).then_some(reg)
    };
    if let Some(reg) = check_brain(session_id) {
        return Some(reg);
    }
    if let Some(parent) = parent_hint.filter(|p| !p.is_empty() && *p != session_id) {
        if let Some(reg) = check_brain(parent) {
            return Some(reg);
        }
    }
    let entries = fs::read_dir(&brain_root).ok()?;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() && !(ft.is_symlink() && entry.path().is_dir()) {
            continue;
        }
        let Some(dir_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if dir_id == session_id || parent_hint == Some(dir_id.as_str()) {
            continue;
        }
        if let Some(reg) = check_brain(&dir_id) {
            return Some(reg);
        }
    }
    None
}

fn spawn_calls_in_session(
    app_data_dir: &Path,
    spawner_id: &str,
    transcript_hint: Option<&Path>,
    children: &[&str],
) -> BTreeMap<String, String> {
    if children.is_empty() {
        return BTreeMap::new();
    }
    #[cfg(feature = "sqlite")]
    {
        let store = app_data_dir.join("conversations").join(format!("{spawner_id}.db"));
        if store.is_file() {
            return native::spawn_calls(&store, children).unwrap_or_default();
        }
    }
    let log = transcript_hint
        .filter(|p| !is_native_store(p) && descriptor_parent(p).is_none() && p.is_file())
        .map(Path::to_path_buf)
        .or_else(|| {
            select_session_source(app_data_dir, spawner_id)
                .filter(|p| !is_native_store(p) && descriptor_parent(p).is_none())
        });
    let Some(log) = log else {
        return BTreeMap::new();
    };
    jsonl_spawn_calls(&log, spawner_id, children).unwrap_or_default()
}

fn jsonl_spawn_calls(path: &Path, spawner_id: &str, children: &[&str]) -> Result<BTreeMap<String, String>> {
    let input = open_transcript(path)?;
    let mut order = StepOrder::default();
    input.snapshot(0, input.len(), |line| {
        order.ingest(&line)?;
        Ok(Flow::Continue)
    })?;
    let mut codec = StepCodec {
        s: Session::new(spawner_id, Harness::Antigravity),
        opts: Demand::LEAN.with_spans(),
        spans: true,
        enrich: SidecarEnrich::default(),
        state: StepState::default(),
        source_path: None,
    };
    let mut discard = crate::stream::Discard;
    let mut records = input.records();
    for (at, end) in order.steps.into_values() {
        records.read(at, end - at, |view| codec.decode(view, at, &mut discard))?;
    }
    Ok(codec
        .state
        .spawned_children
        .into_iter()
        .filter(|(cid, _)| children.contains(&cid.as_str()))
        .collect())
}

fn find_sidecar_parent(app_data_dir: &Path, child_id: &str) -> Option<String> {
    if let Some(reg) = find_swarm_registry(app_data_dir, child_id, None) {
        if reg.coordinator_id != child_id {
            return Some(reg.coordinator_id);
        }
    }
    let brain_root = app_data_dir.join("brain");
    let entries = fs::read_dir(&brain_root).ok()?;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() && !(ft.is_symlink() && entry.path().is_dir()) {
            continue;
        }
        let Some(dir_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if dir_id == child_id {
            continue;
        }
        if brain_root
            .join(&dir_id)
            .join(".system_generated/subagents")
            .join(format!("{child_id}.json"))
            .is_file()
        {
            return Some(dir_id);
        }
    }
    None
}

fn enrich_child_lineage(s: &mut Session, app_data_dir: &Path, sender_parent: Option<&str>, want_extra: bool) {
    if want_extra {
        if let Some(title) = read_swarm_title(&app_data_dir.join("brain").join(&s.id)) {
            s.harness_extra_mut(Harness::Antigravity)
                .insert("swarm_title".into(), Value::String(title));
        }
    }
    if let Some(reg) = find_swarm_registry(app_data_dir, &s.id, s.lineage.parent.as_deref().or(sender_parent)) {
        if want_extra {
            if let Some(title) = &reg.title {
                s.harness_extra_mut(Harness::Antigravity)
                    .entry("swarm_title")
                    .or_insert_with(|| Value::String(title.clone()));
            }
        }
        if s.id != reg.coordinator_id {
            s.lineage.parent = Some(reg.coordinator_id.clone());
            if let Some(member_name) = reg.name_by_id.get(&s.id) {
                let member = &reg.by_name[member_name];
                // Walk `added_by` chain (cycle-guarded) to build `/coordinator/.../<member>`.
                let mut chain = vec![member.name.as_str()];
                let mut visited = BTreeSet::from([member.name.as_str()]);
                let mut cur = member.added_by.as_deref();
                while let Some(adder) = cur {
                    if !visited.insert(adder) {
                        break;
                    }
                    chain.push(adder);
                    cur = reg.by_name.get(adder).and_then(|m| m.added_by.as_deref());
                }
                if chain.last().copied() != Some("coordinator") && reg.by_name.contains_key("coordinator") {
                    chain.push("coordinator");
                }
                chain.reverse();
                s.lineage.agent_path = Some(format!("/{}", chain.join("/")));

                let adder_member = member.added_by.as_deref().and_then(|adder| reg.by_name.get(adder));
                let spawner_id = adder_member
                    .map(|m| m.conversation_id.as_str())
                    .unwrap_or(&reg.coordinator_id);
                let mut calls = spawn_calls_in_session(app_data_dir, spawner_id, None, &[s.id.as_str()]);
                if let Some(call_id) = calls.remove(&s.id) {
                    s.lineage.spawned_by_tool_use = Some(call_id);
                }

                if want_extra {
                    let bag = s.harness_extra_mut(Harness::Antigravity);
                    for (k, v) in &member.record {
                        bag.insert(k.clone(), v.clone());
                    }
                    if let Some(adder_m) = adder_member {
                        bag.insert("added_by_id".into(), Value::String(adder_m.conversation_id.clone()));
                    }
                }
            }
        }
        return;
    }
    if s.lineage.spawned_by_tool_use.is_none() {
        if let Some(parent_id) = s.lineage.parent.clone() {
            let mut calls = spawn_calls_in_session(app_data_dir, &parent_id, None, &[s.id.as_str()]);
            if let Some(call_id) = calls.remove(&s.id) {
                s.lineage.spawned_by_tool_use = Some(call_id);
            } else if let Some(spawner) = sender_parent.filter(|sp| *sp != parent_id && *sp != s.id) {
                let mut calls = spawn_calls_in_session(app_data_dir, spawner, None, &[s.id.as_str()]);
                if let Some(call_id) = calls.remove(&s.id) {
                    s.lineage.spawned_by_tool_use = Some(call_id);
                }
            }
        }
    }
}

fn unwrap_user_request(text: &str) -> &str {
    exported_user_request(text).unwrap_or_else(|| text.trim())
}

pub(crate) fn looks_like_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
}

const MAX_SETTINGS_TAG_BYTES: usize = 16 * 1024;

/// Stream-scan a `USER_INPUT` `content` string for `<USER_SETTINGS_CHANGE>...</USER_SETTINGS_CHANGE>`
/// blocks without allocating the surrounding prompt/metadata when no tag is present.
fn extract_view_settings_model(view: JsonView<'_>) -> Result<Option<String>> {
    const OPEN: &str = "<USER_SETTINGS_CHANGE>";
    const CLOSE: &str = "</USER_SETTINGS_CHANGE>";

    let mut carry = String::new();
    let mut inside: Option<String> = None;
    let mut found: Option<String> = None;

    let res = view.for_each_str_chunk(|chunk| {
        carry.push_str(chunk);
        loop {
            if let Some(buf) = &mut inside {
                if let Some(end) = carry.find(CLOSE) {
                    let take = MAX_SETTINGS_TAG_BYTES.saturating_sub(buf.len()).min(end);
                    buf.push_str(&carry[..carry.floor_char_boundary(take)]);
                    if let Some(model) = parse_user_settings_model(buf) {
                        found = Some(model);
                    }
                    inside = None;
                    carry.drain(..end + CLOSE.len());
                } else {
                    let safe = carry.len().saturating_sub(CLOSE.len() - 1);
                    let safe = carry.floor_char_boundary(safe);
                    if safe > 0 {
                        let take = MAX_SETTINGS_TAG_BYTES.saturating_sub(buf.len()).min(safe);
                        buf.push_str(&carry[..carry.floor_char_boundary(take)]);
                        carry.drain(..safe);
                    }
                    break;
                }
            } else if let Some(start) = carry.find(OPEN) {
                inside = Some(String::new());
                carry.drain(..start + OPEN.len());
            } else {
                let safe = carry.len().saturating_sub(OPEN.len() - 1);
                let safe = carry.floor_char_boundary(safe);
                if safe > 0 {
                    carry.drain(..safe);
                }
                break;
            }
        }
        Ok(())
    });
    res?;
    Ok(found)
}

fn clean_settings_model_token(raw: &str) -> Option<String> {
    let token = raw
        .trim()
        .trim_end_matches(',')
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim();
    if token.is_empty() || token.len() > 128 || token.contains('\n') {
        return None;
    }
    if matches!(
        token.to_ascii_lowercase().as_str(),
        "none" | "null" | "unspecified" | "default" | "auto"
    ) {
        return None;
    }
    Some(token.to_owned())
}

fn parse_user_settings_model(block: &str) -> Option<String> {
    let trimmed = block.trim();
    if trimmed.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            for key in ["model", "Model", "model_name", "modelSelection", "Model Selection"] {
                if let Some(m) = val
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(clean_settings_model_token)
                {
                    return Some(m);
                }
            }
        }
    }
    let mut result = None;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Key-value forms: `Model: ...`, `"model": "..."`, `model = ...`, `Model Selection: ...`
        if let Some((k, v)) = line.split_once(':').or_else(|| line.split_once('=')) {
            let key_norm = k
                .trim()
                .trim_start_matches(['-', '*'])
                .trim()
                .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
                .trim()
                .to_ascii_lowercase();
            if matches!(
                key_norm.as_str(),
                "model" | "model selection" | "model_selection" | "model_name" | "model_id"
            ) {
                let mut rhs = v.trim();
                if let Some((before_sentence, _)) = rhs.split_once(". ") {
                    rhs = before_sentence;
                }
                rhs = rhs.strip_suffix('.').unwrap_or(rhs).trim();
                if let Some((_, after_arrow)) = rhs.rsplit_once("->") {
                    rhs = after_arrow;
                } else if rhs.to_ascii_lowercase().starts_with("from ") {
                    if let Some((_, after_to)) = rhs.rsplit_once(" to ") {
                        rhs = after_to;
                    }
                }
                if let Some(m) = clean_settings_model_token(rhs) {
                    result = Some(m);
                    continue;
                }
            }
        }
        // Prose form (`The user changed setting `Model Selection` from None to Gemini 3.1 Pro. No need...`):
        // Split at `. ` (never inside `3.1` since version dots are followed by digits, not spaces).
        let first = line.split_once(". ").map_or(line, |(first, _)| first);
        let sentence = first.strip_suffix('.').unwrap_or(first).trim();
        let lower = sentence.to_ascii_lowercase();
        if let Some(model_pos) = lower.find("model") {
            let tail = &sentence[model_pos..];
            let tail_lower = &lower[model_pos..];
            let candidate = if let Some((_, after_arrow)) = tail.rsplit_once("->") {
                Some(after_arrow)
            } else if let Some(from_pos) = tail_lower.find(" from ") {
                let after_from = &tail[from_pos + " from ".len()..];
                after_from.split_once(" to ").map(|(_, after_to)| after_to)
            } else {
                tail.split_once(" to ").map(|(_, after_to)| after_to)
            };
            if let Some(m) = candidate.and_then(clean_settings_model_token) {
                result = Some(m);
            }
        }
    }
    result
}

pub(crate) fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    // Windows `file:///C:/...` -> `C:/...`
    let path_part = if rest.len() >= 3
        && rest.as_bytes()[0] == b'/'
        && rest.as_bytes()[1].is_ascii_alphabetic()
        && rest.as_bytes()[2] == b':'
    {
        &rest[1..]
    } else {
        rest
    };
    Some(PathBuf::from(percent_decode(path_part)))
}

/// A media reference to a local file that no longer exists. A reference that is not a
/// path or `file:` URI cannot be checked and is not reported.
pub(crate) fn attachment_missing(uri: &str) -> bool {
    file_uri_to_path(uri)
        .or_else(|| uri.starts_with('/').then(|| PathBuf::from(uri)))
        .is_some_and(|path| !path.exists())
}

fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) const MAX_PBTXT_BYTES: usize = 256 * 1024;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Annotations {
    pub title: Option<String>,
    pub archived: Option<bool>,
    pub starred: Option<bool>,
    pub pinned: Option<bool>,
    pub tags: Vec<String>,
    pub group_id: Option<String>,
    pub active_profile: Option<String>,
}

impl Annotations {
    #[cfg(feature = "sqlite")]
    fn merge_fallback(&mut self, fallback: &Annotations) {
        if self.title.is_none() {
            self.title.clone_from(&fallback.title);
        }
        if self.archived.is_none() {
            self.archived = fallback.archived;
        }
        if self.starred.is_none() {
            self.starred = fallback.starred;
        }
        if self.pinned.is_none() {
            self.pinned = fallback.pinned;
        }
        if self.tags.is_empty() && !fallback.tags.is_empty() {
            self.tags.clone_from(&fallback.tags);
        }
        if self.group_id.is_none() {
            self.group_id.clone_from(&fallback.group_id);
        }
        if self.active_profile.is_none() {
            self.active_profile.clone_from(&fallback.active_profile);
        }
    }

    fn to_extra_value(&self) -> Option<Value> {
        let mut map = Map::new();
        if let Some(archived) = self.archived {
            map.insert("archived".into(), Value::Bool(archived));
        }
        if let Some(starred) = self.starred {
            map.insert("starred".into(), Value::Bool(starred));
        }
        if let Some(pinned) = self.pinned {
            map.insert("pinned".into(), Value::Bool(pinned));
        }
        if !self.tags.is_empty() {
            map.insert(
                "tags".into(),
                Value::Array(self.tags.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(group_id) = self.group_id.as_ref().filter(|s| !s.is_empty()) {
            map.insert("group_id".into(), Value::String(group_id.clone()));
        }
        if let Some(profile) = self.active_profile.as_ref().filter(|s| !s.is_empty()) {
            map.insert("active_profile".into(), Value::String(profile.clone()));
        }
        (!map.is_empty()).then_some(Value::Object(map))
    }
}

/// Parse `annotations/<uuid>.pbtxt` in one bounded pass (`MAX_PBTXT_BYTES`).
fn read_pbtxt_annotations(path: &Path) -> Option<Annotations> {
    use std::io::Read as _;
    let file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_PBTXT_BYTES as u64 {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_PBTXT_BYTES as u64 + 1).read_to_string(&mut text).ok()?;
    if text.len() > MAX_PBTXT_BYTES {
        return None;
    }
    Some(parse_pbtxt_annotations(&text))
}

fn read_pbtxt_title(path: &Path) -> Option<String> {
    read_pbtxt_annotations(path).and_then(|a| a.title)
}

#[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
pub(crate) fn enrich_ingested_session_from_app_dir(s: &mut Session, app_data_dir: &Path, has_pbtxt: bool) {
    let mut enrich = SidecarEnrich::default();
    let mut annotations =
        read_pbtxt_annotations(&app_data_dir.join("annotations").join(format!("{}.pbtxt", s.id))).unwrap_or_default();
    let has_pbtxt = has_pbtxt || annotations.title.as_deref().is_some_and(|t| !t.trim().is_empty());
    if let Some(pbtxt_title) = annotations.title.clone().filter(|t| !t.trim().is_empty()) {
        s.title = Some(pbtxt_title);
    }
    let conv_db = app_data_dir.join("conversations").join(format!("{}.db", s.id));
    if conv_db.is_file() {
        apply_native_db_metadata(&conv_db, true, s, &mut enrich);
    }
    let sum_db = app_data_dir.join("conversation_summaries.db");
    if sum_db.is_file() {
        if let Ok(Some(sum)) = read_summary_one(&sum_db, &s.id) {
            apply_summary_to_session(s, &sum, has_pbtxt, true);
            annotations.merge_fallback(&sum.annotations);
        }
    }
    if let Some(ann_val) = annotations.to_extra_value() {
        s.harness_extra_mut(Harness::Antigravity)
            .insert("annotations".into(), ann_val);
    }
}

fn unescape_pbtxt_quoted(rest: &str) -> Option<(String, &str)> {
    let rest = rest.trim_start();
    let quote = match rest.as_bytes().first().copied()? {
        q @ (b'"' | b'\'') => q,
        _ => return None,
    };
    let raw = &rest.as_bytes()[1..];
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b if b == quote => {
                let s = String::from_utf8(bytes).ok()?;
                return Some((s, &rest[1 + i + 1..]));
            }
            b'\\' if i + 1 < raw.len() => {
                i += 1;
                match raw[i] {
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'\\' => bytes.push(b'\\'),
                    b'"' => bytes.push(b'"'),
                    b'\'' => bytes.push(b'\''),
                    b'x' if i + 2 < raw.len() => {
                        if let (Some(hi), Some(lo)) = (hex_val(raw[i + 1]), hex_val(raw[i + 2])) {
                            bytes.push((hi << 4) | lo);
                            i += 2;
                        }
                    }
                    b'0'..=b'7' => {
                        let mut val = raw[i] - b'0';
                        for _ in 0..2 {
                            if i + 1 < raw.len() && (b'0'..=b'7').contains(&raw[i + 1]) {
                                i += 1;
                                val = (val << 3) | (raw[i] - b'0');
                            } else {
                                break;
                            }
                        }
                        bytes.push(val);
                    }
                    other => bytes.push(other),
                }
            }
            ch => bytes.push(ch),
        }
        i += 1;
    }
    String::from_utf8(bytes).ok().map(|s| (s, ""))
}

fn parse_pbtxt_bool(rest: &str) -> Option<bool> {
    let token = rest.split('#').next()?.trim().trim_end_matches([',', ';']).trim();
    match token {
        "true" | "True" | "TRUE" | "t" | "1" => Some(true),
        "false" | "False" | "FALSE" | "f" | "0" => Some(false),
        _ => None,
    }
}

pub(crate) fn parse_pbtxt_annotations(text: &str) -> Annotations {
    let mut out = Annotations::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim() {
            "title" if out.title.is_none() => {
                out.title = unescape_pbtxt_quoted(rest).map(|(s, _)| s);
            }
            "archived" => {
                if let Some(b) = parse_pbtxt_bool(rest) {
                    out.archived = Some(b);
                }
            }
            "starred" => {
                if let Some(b) = parse_pbtxt_bool(rest) {
                    out.starred = Some(b);
                }
            }
            "pinned" => {
                if let Some(b) = parse_pbtxt_bool(rest) {
                    out.pinned = Some(b);
                }
            }
            "tags" => {
                let rest = rest.trim_start();
                if let Some(mut list) = rest.strip_prefix('[') {
                    while let Some((tag, rem)) = unescape_pbtxt_quoted(list) {
                        let t = tag.trim();
                        if !t.is_empty() {
                            out.tags.push(t.to_owned());
                        }
                        list = rem.trim_start().strip_prefix(',').unwrap_or(rem);
                    }
                } else if let Some((tag, _)) = unescape_pbtxt_quoted(rest) {
                    let t = tag.trim();
                    if !t.is_empty() {
                        out.tags.push(t.to_owned());
                    }
                }
            }
            "group_id" => {
                if let Some((val, _)) = unescape_pbtxt_quoted(rest) {
                    let v = val.trim();
                    if !v.is_empty() {
                        out.group_id = Some(v.to_owned());
                    }
                }
            }
            "active_profile" => {
                if let Some((val, _)) = unescape_pbtxt_quoted(rest) {
                    let v = val.trim();
                    if !v.is_empty() {
                        out.active_profile = Some(v.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn parse_pbtxt_title(text: &str) -> Option<String> {
    parse_pbtxt_annotations(text).title
}

// ---------------------------------------------------------------------------
// SQLite & Protobuf sidecar readers (`#[cfg(feature = "sqlite")]`)
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
fn record_sqlite_deps(db: &Path, out: &mut Vec<PathBuf>) {
    out.push(db.to_path_buf());
    for suffix in ["-wal", "-journal"] {
        let mut os = db.as_os_str().to_os_string();
        os.push(suffix);
        out.push(PathBuf::from(os));
    }
}

#[cfg(feature = "sqlite")]
fn record_sink_sqlite_deps(db: &Path, sink: &mut dyn MessageSink) {
    sink.dependency(db);
    for suffix in ["-wal", "-journal"] {
        let mut os = db.as_os_str().to_os_string();
        os.push(suffix);
        sink.dependency(Path::new(&os));
    }
}

#[cfg(feature = "sqlite")]
fn apply_native_db_metadata(conv_db: &Path, want_extra: bool, s: &mut Session, enrich: &mut SidecarEnrich) {
    enrich.has_native_db = true;
    if let Ok(db_meta) = native::metadata(conv_db, true) {
        if s.cwd.is_none() {
            s.cwd = db_meta.cwd;
        }
        if s.created_at.is_none() {
            s.created_at = db_meta.created_at;
        }
        if s.git.is_none() {
            if let Some(branch) = db_meta.git_branch {
                s.git = Some(GitInfo {
                    branch: Some(branch),
                    commit: None,
                    remote: None,
                    ..Default::default()
                });
            }
        }
        if let Some(git_root) = db_meta.git_root {
            if want_extra {
                s.harness_extra_mut(Harness::Antigravity)
                    .entry("git_root")
                    .or_insert_with(|| Value::String(git_root.display().to_string()));
            }
        }
        if s.lineage.parent.is_none() {
            s.lineage.parent = db_meta.parent_id;
        }
        if want_extra {
            let bag = s.harness_extra_mut(Harness::Antigravity);
            for (key, value) in [
                ("root_conversation_id", db_meta.root_id),
                ("agent_type", db_meta.agent_type),
                ("agent_role", db_meta.agent_role),
            ] {
                if let Some(value) = value {
                    bag.entry(key).or_insert(Value::String(value));
                }
            }
            if let Some(tt) = db_meta.trajectory_type {
                bag.entry("trajectory_type").or_insert_with(|| Value::Number(tt.into()));
            }
            if let Some(ts) = db_meta.trajectory_source {
                bag.entry("trajectory_source")
                    .or_insert_with(|| Value::Number(ts.into()));
            }
        }
        if s.model.is_none() {
            s.model = db_meta.dominant_model;
        }
        for (step, model) in db_meta.step_models {
            enrich.step_models.entry(step).or_insert(model);
        }
    }
}

#[cfg(feature = "sqlite")]
fn summary_rows(store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
    use crate::source::Rows;
    let app_data_dir = store.parent()?;
    let mut certificates = super::store::sqlite::Certificates::default();
    let seed = Rows {
        count: 0,
        high: 0,
        digest: Rows::SEED,
    };
    if let Ok(entries) = fs::read_dir(app_data_dir.join("brain")) {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if !name.starts_with('.') {
                    certificates.0.entry(name).or_insert(seed);
                }
            }
        }
    }
    if let Ok(entries) = fs::read_dir(app_data_dir.join("conversations")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if is_native_store(&path) {
                if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                    certificates.0.entry(id.to_owned()).or_insert(seed);
                }
            }
        }
    }
    if store.try_exists().ok()? {
        super::store::sqlite::open_ro(store)
            .ok()?
            .read_snapshot(|conn, _| {
                if !super::store::sqlite::table_exists(conn, "conversation_summaries") {
                    return Ok(());
                }
                if let Some(select_sql) = summary_columns_sql(conn)? {
                    certificates.digest(conn, &format!("{select_sql} ORDER BY conversation_id"))?;
                }
                Ok(())
            })
            .ok()?;
    }
    Some(certificates.0)
}

#[cfg(feature = "sqlite")]
fn record_sqlite_readset(db: &Path, reads: &mut ReadSet) {
    reads.record(db);
    for suffix in ["-wal", "-journal"] {
        let mut os = db.as_os_str().to_os_string();
        os.push(suffix);
        reads.record(&PathBuf::from(os));
    }
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Default, Clone)]
struct SummaryRow {
    title: Option<String>,
    preview: Option<String>,
    cwd: Option<PathBuf>,
    git_root: Option<PathBuf>,
    git_branch: Option<String>,
    parent_id: Option<String>,
    root_id: Option<String>,
    agent_name: Option<String>,
    agent_type: Option<String>,
    agent_role: Option<String>,
    source: Option<String>,
    project_id: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    killed: bool,
    status: Option<String>,
    fork_parent_id: Option<String>,
    annotations: Annotations,
}

#[cfg(feature = "sqlite")]
fn apply_summary_fallback(sref: &mut SessionRef, sum: &SummaryRow, has_pbtxt_title: bool) {
    if !has_pbtxt_title {
        if let Some(sum_title) = sum.title.as_ref().filter(|s| !s.trim().is_empty()) {
            sref.title = Some(sum_title.clone());
        } else if sref.title.is_none() {
            sref.title = sum.preview.clone().filter(|s| !s.trim().is_empty());
        }
    } else if sref.title.is_none() {
        sref.title = sum
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| sum.preview.clone().filter(|s| !s.trim().is_empty()));
    }
    if sref.cwd.is_none() {
        sref.cwd.clone_from(&sum.cwd);
    }
    if let Some(created) = sum.created_at {
        let from_mtime = sref.created_at.is_some()
            && sref.created_at == sref.updated_at
            && fs::metadata(&sref.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from)
                == sref.created_at;
        if sref.created_at.is_none() || from_mtime {
            sref.created_at = Some(created);
        }
    }
    if sref.updated_at.is_none() {
        sref.updated_at = sum.updated_at.or(sum.created_at);
    }
}

#[cfg(feature = "sqlite")]
fn apply_summary_to_session(s: &mut Session, sum: &SummaryRow, has_pbtxt_title: bool, want_extra: bool) {
    if !has_pbtxt_title {
        if let Some(sum_title) = sum.title.as_ref().filter(|t| !t.trim().is_empty()) {
            s.title = Some(sum_title.clone());
        } else if s.title.is_none() {
            s.title = sum.preview.clone().filter(|t| !t.trim().is_empty());
        }
    } else if s.title.is_none() {
        s.title = sum
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| sum.preview.clone().filter(|t| !t.trim().is_empty()));
    }
    if s.cwd.is_none() {
        s.cwd.clone_from(&sum.cwd);
    }
    if s.created_at.is_none() {
        s.created_at = sum.created_at;
    }
    if s.updated_at.is_none() {
        s.updated_at = sum.updated_at.or(sum.created_at);
    }
    if s.git.is_none() {
        if let Some(branch) = &sum.git_branch {
            s.git = Some(GitInfo {
                branch: Some(branch.clone()),
                commit: None,
                remote: None,
                ..Default::default()
            });
        }
    }
    if let Some(parent) = sum.parent_id.as_ref().filter(|p| !p.is_empty()) {
        if s.lineage.parent.is_none() {
            s.lineage.parent = Some(parent.clone());
        }
    }
    if let Some(agent_name) = sum.agent_name.as_ref().filter(|a| !a.is_empty()) {
        if s.lineage.parent.is_some() && s.lineage.agent_path.is_none() {
            s.lineage.agent_path = Some(agent_name.clone());
        }
        if want_extra {
            s.harness_extra_mut(Harness::Antigravity)
                .insert("agent_name".into(), Value::String(agent_name.clone()));
        }
    }
    if want_extra {
        let bag = s.harness_extra_mut(Harness::Antigravity);
        if let Some(git_root) = &sum.git_root {
            bag.entry("git_root")
                .or_insert_with(|| Value::String(git_root.display().to_string()));
        }
        for (key, value) in [
            ("root_conversation_id", sum.root_id.as_ref()),
            ("agent_type", sum.agent_type.as_ref()),
            ("agent_role", sum.agent_role.as_ref()),
            ("source", sum.source.as_ref()),
            ("project_id", sum.project_id.as_ref()),
            ("status", sum.status.as_ref()),
            ("forked_from", sum.fork_parent_id.as_ref()),
        ] {
            if let Some(val) = value.filter(|x| !x.is_empty()) {
                bag.entry(key).or_insert_with(|| Value::String(val.clone()));
            }
        }
        if sum.killed {
            bag.entry("killed").or_insert(Value::Bool(true));
        }
    }
}

#[cfg(feature = "sqlite")]
fn summary_columns_sql(conn: &rusqlite::Connection) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('conversation_summaries')")?;
    let cols: BTreeSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    if !cols.contains("conversation_id") {
        return Ok(None);
    }
    let pick = |name: &str| {
        if cols.contains(name) {
            name.to_string()
        } else {
            format!("NULL AS {name}")
        }
    };
    let raw_col = if cols.contains("raw_summary") {
        format!(
            "CASE WHEN typeof(raw_summary) = 'blob' AND length(raw_summary) <= {} THEN raw_summary ELSE NULL END AS raw_summary",
            SummaryRow::MAX_RAW_SUMMARY_BYTES
        )
    } else {
        "NULL AS raw_summary".to_string()
    };
    Ok(Some(format!(
        "SELECT conversation_id, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM conversation_summaries",
        pick("title"),
        pick("preview"),
        pick("workspace_uris"),
        pick("parent_conversation_id"),
        pick("agent_name"),
        pick("source"),
        pick("project_id"),
        pick("last_modified_time"),
        pick("killed"),
        pick("status"),
        pick("group_id"),
        raw_col,
    )))
}

#[cfg(feature = "sqlite")]
fn decode_summary_row(
    row: &rusqlite::Row<'_>,
    stage: &mut Option<native::Stage>,
) -> rusqlite::Result<(String, SummaryRow)> {
    use rusqlite::types::ValueRef;
    let opt_text = |idx: usize| -> rusqlite::Result<Option<String>> {
        Ok(match row.get_ref(idx)? {
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .map(str::to_owned)
                .filter(|s| !s.trim().is_empty()),
            ValueRef::Integer(i) => Some(i.to_string()),
            _ => None,
        })
    };
    let id: String = row.get(0)?;
    let title = opt_text(1)?;
    let preview = opt_text(2)?;
    let workspace_uris = opt_text(3)?;
    let parent_id = opt_text(4)?.map(|s| s.trim().to_owned());
    let agent_name = opt_text(5)?.map(|s| s.trim().to_owned());
    let source = opt_text(6)?.map(|s| s.trim().to_owned());
    let project_id = opt_text(7)?.map(|s| s.trim().to_owned());
    let last_modified = opt_text(8)?;
    let col_killed = match row.get_ref(9)? {
        ValueRef::Integer(i) => Some(i != 0),
        ValueRef::Text(t) => match std::str::from_utf8(t).ok().map(str::trim) {
            Some("1") => Some(true),
            Some("0") => Some(false),
            Some(s) if s.eq_ignore_ascii_case("true") => Some(true),
            Some(s) if s.eq_ignore_ascii_case("false") => Some(false),
            _ => None,
        },
        _ => None,
    };
    let status = opt_text(10)?.map(|s| s.trim().to_owned());
    let group_id = opt_text(11)?.map(|s| s.trim().to_owned());
    let mut annotations = Annotations::default();
    if let Some(gid) = group_id {
        annotations.group_id = Some(gid);
    }
    let cwd = workspace_uris.as_deref().and_then(first_workspace_uri_path);
    let updated_at = last_modified.as_deref().and_then(parse_sqlite_ts);
    let mut summary_row = SummaryRow {
        title,
        preview,
        cwd,
        parent_id,
        agent_name,
        source,
        project_id,
        updated_at,
        killed: col_killed.unwrap_or(false),
        status,
        annotations,
        ..SummaryRow::default()
    };
    if let ValueRef::Blob(b) | ValueRef::Text(b) = row.get_ref(12)? {
        summary_row.merge_raw_summary(stage, b);
    }
    Ok((id, summary_row))
}

#[cfg(feature = "sqlite")]
fn read_summaries_map(db_path: &Path) -> Result<HashMap<String, SummaryRow>> {
    super::store::sqlite::open_ro(db_path)?.read_snapshot(|conn, _reads| {
        let Some(select_sql) = summary_columns_sql(conn)? else {
            return Ok(HashMap::new());
        };
        let mut stmt = conn.prepare(&select_sql)?;
        let mut stage = None;
        let mut rows = stmt.query([])?;
        let mut map = HashMap::new();
        while let Some(row) = rows.next()? {
            let (id, sum) = decode_summary_row(row, &mut stage)?;
            map.insert(id, sum);
        }
        Ok(map)
    })
}

#[cfg(feature = "sqlite")]
fn read_summary_one(db_path: &Path, id: &str) -> Result<Option<SummaryRow>> {
    use rusqlite::OptionalExtension as _;
    super::store::sqlite::open_ro(db_path)?.read_snapshot(|conn, _reads| {
        let Some(select_sql) = summary_columns_sql(conn)? else {
            return Ok(None);
        };
        let sql = format!("{select_sql} WHERE conversation_id = ?1");
        let mut stage = None;
        let row = conn
            .query_row(&sql, [id], |r| decode_summary_row(r, &mut stage))
            .optional()?
            .map(|(_, sum)| sum);
        Ok(row)
    })
}

#[cfg(feature = "sqlite")]
fn first_workspace_uri_path(raw: &str) -> Option<PathBuf> {
    if let Ok(uris) = serde_json::from_str::<Vec<String>>(raw) {
        return uris.iter().find_map(|u| file_uri_to_path(u));
    }
    file_uri_to_path(raw.trim())
}

#[cfg(feature = "sqlite")]
fn parse_sqlite_ts(s: &str) -> Option<DateTime<Utc>> {
    super::parse_ts(s).or_else(|| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
            .ok()
            .map(|ndt| ndt.and_utc())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse as _;

    const PARENT_ID: &str = "11111111-2222-4333-8444-555555555555";
    const CHILD_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    #[test]
    fn latest_steps_replay_in_index_order_with_interleaved_deliveries() {
        let rows = [
            json!({"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[
                {"name":"first","args":{}},{"name":"second","args":{}}]}),
            json!({"step_index":3,"type":"GENERIC","content":"second result"}),
            json!({"step_index":1,"type":"SYSTEM_MESSAGE","content":"background notification"}),
            json!({"step_index":2,"type":"GENERIC","content":"obsolete result","status":"RUNNING"}),
            json!({"step_index":2,"type":"GENERIC","content":"first result","status":"DONE"}),
        ];
        let text = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
        let world = crate::test_world("agy-step-order");
        let path = world.base.join("steps.jsonl");
        fs::write(&path, &text).unwrap();
        let adapter = Antigravity::with_home(&world.base);
        let reference = SessionRef::new(PARENT_ID, Harness::Antigravity, &path);
        assert!(!adapter.append_only(&reference));
        for session in [
            parse_str(PARENT_ID, &text, None).unwrap(),
            adapter.parse(&reference).unwrap(),
        ] {
            assert_eq!(
                session
                    .messages
                    .iter()
                    .map(|m| m.id.as_deref().unwrap())
                    .collect::<Vec<_>>(),
                ["0", "1", "2", "3"]
            );
            for (index, name, body) in [(2, "first", "first result"), (3, "second", "second result")] {
                let Block::ToolResult {
                    tool_use_id,
                    tool_name,
                    content,
                    ..
                } = &session.messages[index].content[0]
                else {
                    panic!("result")
                };
                assert_eq!(tool_name.as_deref(), Some(name));
                let Block::ToolUse { id, .. } = &session.messages[0].content[index - 2] else {
                    panic!("call")
                };
                assert_eq!(tool_use_id, id);
                assert_eq!(
                    content
                        .try_resolve(&crate::lazy::Resolver::new(Some(path.clone())))
                        .unwrap(),
                    body
                );
            }
        }
    }

    #[test]
    fn chunked_history_reassembles_bytes_and_rejects_missing_chunks() {
        let world = crate::test_world("agy-chunks");
        let app = world.base.join("antigravity-cli");
        let logs = app.join("brain").join(PARENT_ID).join(".system_generated/logs");
        let chunks = logs.join("chunks/transcript_full");
        fs::create_dir_all(&chunks).unwrap();
        let body = "quoted \\\" and λ🦊".repeat(20);
        let text = format!(
            "{}\n{}\n",
            json!({"step_index":2,"type":"PLANNER_RESPONSE","content":body}),
            json!({"step_index":0,"type":"USER_INPUT","content":"full history"})
        );
        // Deliberately splits JSON strings, escapes, and multibyte characters.
        for (index, bytes) in text.as_bytes().chunks(37).enumerate() {
            fs::write(chunks.join(format!("{index:08}.jsonl")), bytes).unwrap();
        }
        fs::write(
            logs.join("transcript.jsonl"),
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"compact only\"}\n",
        )
        .unwrap();
        let adapter = Antigravity::with_home(&app);
        let discovered = adapter.discover().unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].path, chunks.join("00000000.jsonl"));
        let session = adapter.parse(&discovered[0]).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].id.as_deref(), Some("0"));
        let Block::Text { text, .. } = &session.messages[1].content[0] else {
            panic!("text")
        };
        assert_eq!(
            text.try_resolve(&crate::lazy::Resolver::new(Some(discovered[0].path.clone())))
                .unwrap(),
            body
        );
        fs::remove_file(chunks.join("00000001.jsonl")).unwrap();
        assert!(adapter
            .parse(&discovered[0])
            .unwrap_err()
            .to_string()
            .contains("missing Antigravity chunk"));
    }

    #[test]
    fn killed_children_remain_in_the_forest_without_a_transcript() {
        let world = crate::test_world("agy-missing-child");
        let app = world.base.join("antigravity-cli");
        let parent = app.join("brain").join(PARENT_ID);
        let logs = parent.join(".system_generated/logs");
        let children = parent.join(".system_generated/subagents");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&children).unwrap();
        let path = logs.join("transcript_full.jsonl");
        fs::write(&path, "{}").unwrap();
        fs::write(
            children.join(format!("{CHILD_ID}.json")),
            json!({
                "conversationId":CHILD_ID,"state":"SUBAGENT_STATE_KILLED","spawnStepIndex":4,
                "subagentDescriptor":{"typeName":"worker","role":"check parser"}
            })
            .to_string(),
        )
        .unwrap();
        let adapter = Antigravity::with_home(&app);
        let parent = SessionRef::new(PARENT_ID, Harness::Antigravity, path);
        let children = adapter
            .forest(&parent, crate::SubagentDemand::Rich, None, &|_, _| true)
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].result_status.as_deref(), Some("killed"));
        let child = adapter.parse(&children[0].session).unwrap();
        assert!(child.messages.is_empty());
        assert_eq!(child.lineage.parent.as_deref(), Some(PARENT_ID));
        assert_eq!(child.extra["antigravity"]["transcript_unavailable"], true);
        assert_eq!(
            adapter.agent_transcript(&parent, CHILD_ID, None),
            Some(children[0].session.path.clone())
        );
        // Modern swarms may retain only the members registry after killing a worker.
        #[cfg(feature = "sqlite")]
        let mut guard = ReadSet::default();
        #[cfg(feature = "sqlite")]
        forest_dependencies(&parent, &mut guard);
        fs::remove_file(&children[0].session.path).unwrap();
        let swarm = app.join("brain").join(PARENT_ID).join(".system_generated/swarm");
        fs::create_dir_all(&swarm).unwrap();
        fs::write(
            swarm.join("members.json"),
            json!({"members": {
                "reviewer": {"conversation_id":CHILD_ID,"role":"review parser","killed":true}
            }})
            .to_string(),
        )
        .unwrap();
        let members = adapter
            .forest(&parent, crate::SubagentDemand::Rich, None, &|_, _| true)
            .unwrap();
        assert_eq!(members.len(), 1);
        #[cfg(feature = "sqlite")]
        assert!(!guard.fresh(&mut Probe::default()));
        assert_eq!(members[0].agent_type.as_deref(), Some("reviewer"));
        assert_eq!(members[0].result_status.as_deref(), Some("killed"));
        let child = adapter.parse(&members[0].session).unwrap();
        assert!(child.messages.is_empty());
        assert_eq!(child.lineage.parent.as_deref(), Some(PARENT_ID));
    }

    #[test]
    fn complete_mode_preserves_known_and_future_records() {
        let records = [
            json!({"step_index":0,"type":"USER_INPUT","content":"hello","future":{"value":7}}),
            json!({"step_index":1,"type":"CHECKPOINT","content":"summary","future":"kept"}),
            json!({"step_index":2,"type":"FUTURE_TYPE","payload":{"opaque":true}}),
        ];
        struct Collect(Vec<Message>);
        impl MessageSink for Collect {
            fn message(&mut self, m: Message) -> Flow {
                self.0.push(m);
                Flow::Continue
            }
        }
        let mut sink = Collect(Vec::new());
        let text = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        stream_str(PARENT_ID, &text, None, &Demand::COMPLETE, &mut sink).unwrap();
        let actual = sink
            .0
            .iter()
            .filter(|m| m.id.is_some())
            .map(|m| serde_json::to_value(m).unwrap()["extra"]["_record"].clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, records);
    }

    #[test]
    fn swarm_spawns_approvals_and_task_deliveries_have_distinct_affordances() {
        let records = [
            json!({"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[
                {"name":"swarm","args":{"action":"status"}},
                {"name":"swarm","args":{"action":"add","name":"worker","brief":"inspect"}},
                {"name":"ask_permission","args":{"Action":"unsandboxed","Reason":"build"}}
            ]}),
            json!({"step_index":1,"type":"SYSTEM_MESSAGE","content":format!("<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-14T00:00:00Z sender={PARENT_ID}/task-7 content=Task id \"{PARENT_ID}/task-7\" finished with result:\nCommand exited with code 0.\n</SYSTEM_MESSAGE>")}),
        ];
        let text = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        let session = parse_str(PARENT_ID, &text, None).unwrap();
        assert_eq!(session.messages[0].kind, MessageKind::SubagentSpawn);
        let message = &session.messages[1];
        assert_ne!(message.origin, Origin::Scheduler);
        let hits = ROOTLESS.message_affordances(message);
        assert_eq!(
            (hits[0].kind, hits[0].action, hits[0].summary.as_str()),
            (
                crate::affordance::Kind::BackgroundJob,
                "complete",
                format!("{PARENT_ID}/task-7").as_str()
            )
        );
        for (name, args, expected) in [
            ("swarm", json!({"action":"status"}), None),
            (
                "swarm",
                json!({"action":"add"}),
                Some((crate::affordance::Kind::SubagentSpawn, "spawn")),
            ),
            (
                "swarm",
                json!({"action":"model"}),
                Some((crate::affordance::Kind::ModelChange, "change")),
            ),
            (
                "ask_permission",
                json!({}),
                Some((crate::affordance::Kind::Approval, "ask")),
            ),
        ] {
            assert_eq!(
                crate::affordance::classify_tool_json(Harness::Antigravity, name, &Json::Owned(args)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn metadata_changes_do_not_invalidate_transcript_bytes() {
        let world = crate::test_world("agy-live-metadata");
        let app = world.base.join("antigravity-cli");
        let logs = app.join("brain").join(PARENT_ID).join(".system_generated/logs");
        let annotations = app.join("annotations");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&annotations).unwrap();
        let title = annotations.join(format!("{PARENT_ID}.pbtxt"));
        fs::write(&title, "title: \"before\"\n").unwrap();
        let path = logs.join("transcript_full.jsonl");
        fs::write(
            &path,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hello\"}\n",
        )
        .unwrap();
        struct Update {
            title: PathBuf,
            messages: Vec<Message>,
        }
        impl MessageSink for Update {
            fn message(&mut self, message: Message) -> Flow {
                fs::write(&self.title, "title: \"after\"\n").unwrap();
                self.messages.push(message);
                Flow::Continue
            }
        }
        let mut sink = Update {
            title,
            messages: Vec::new(),
        };
        let session = Antigravity::with_home(&app)
            .stream(
                &SessionRef::new(PARENT_ID, Harness::Antigravity, &path),
                &Demand::RICH.with_spans(),
                &mut sink,
            )
            .unwrap();
        assert_eq!(session.title.as_deref(), Some("before"));
        let Block::Text { text, .. } = &sink.messages[0].content[0] else {
            panic!("text")
        };
        assert_eq!(
            text.try_resolve(&crate::lazy::Resolver::new(Some(path))).unwrap(),
            "hello"
        );
    }

    #[test]
    fn persisted_output_notices_ignore_quoted_transcripts() {
        let transcript = PathBuf::from(format!(
            "/store/brain/{PARENT_ID}/.system_generated/logs/transcript_full.jsonl"
        ));
        let claimed =
            format!("/Users/u/.gemini/antigravity-cli/brain/{PARENT_ID}/.system_generated/steps/229/output.txt");
        let resolved = format!("/store/brain/{PARENT_ID}/.system_generated/steps/229/output.txt");
        let (path, resolved) = (claimed.as_str(), resolved.as_str());
        let notice = format!(
            "The tool's output was truncated due to being too long. You can find the full output in a file at file://{path}.\n\nHere is a preview with the middle of the output truncated:\npreview"
        );
        let quoted = serde_json::to_string(&json!({"content": notice})).unwrap();
        for (body, expected) in [
            (notice.clone(), Some(resolved)),
            (
                format!("Created At: today\nCompleted At: today\n{notice}"),
                Some(resolved),
            ),
            (
                format!("<SYSTEM_MESSAGE>\nTask finished with result:\n{notice}\n</SYSTEM_MESSAGE>"),
                Some(resolved),
            ),
            // A notice naming a file outside the conversation's directory is not a pointer.
            (notice.replace(path, "/Users/u/.ssh/id_ed25519"), None),
            (
                notice.replace(
                    path,
                    &format!("/Users/u/.gemini/antigravity-cli/brain/{PARENT_ID}/notes.md"),
                ),
                None,
            ),
            (format!("File Path: transcript.jsonl\n{quoted}"), None),
            (
                format!(
                    "File Path: transcript.jsonl\n{}",
                    serde_json::to_string(&quoted).unwrap()
                ),
                None,
            ),
        ] {
            let jsonl = serde_json::to_string(&json!({
                "step_index": 244,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "DONE",
                "content": body
            }))
            .unwrap();
            let session = parse_str(PARENT_ID, &jsonl, Some(transcript.clone())).unwrap();
            assert_eq!(session.messages[0].content[0].persisted_output_path(), expected);
        }
    }

    #[test]
    fn parses_steps_tools_denials_compaction_and_subagents() {
        let jsonl = [
            serde_json::to_string(&json!({
                "step_index": 0,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:00Z",
                "content": "<USER_REQUEST>\nInspect src/lib.rs and delegate research.\n</USER_REQUEST>",
                "media": [{"mime_type": "image/png", "uri": "file:///workspace/demo/screen.png"}]
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:02Z",
                "thinking": "Let me read the file and spawn a helper.",
                "content": "Checking files and spawning a researcher.",
                "tool_calls": [
                    {"name": "read_file", "args": {"Path": "/workspace/demo/src/lib.rs"}},
                    {"name": "write_file", "args": {"Path": "/etc/blocked", "Content": "nope"}},
                    {"name": "invoke_subagent", "args": {"TypeName": "research", "Role": "Repo Researcher", "Prompt": "Audit exports"}}
                ],
                "input_tokens": 1200,
                "cache_read_tokens": 400,
                "output_tokens": 95
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:03Z",
                "content": "The tool's output was truncated due to being too long. You can find the full output in a file at file:///workspace/demo/.system_generated/steps/3/output.txt\nCreated At: 2026-11-11T10:00:03Z\npub fn demo() {}"
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 4,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "ERROR",
                "created_at": "2026-11-11T10:00:04Z",
                "content": "Created At: 2026-11-11T10:00:04Z",
                "error": "permission check failed for write_file on /etc/blocked"
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 5,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:05Z",
                "content": format!("Created At: 2026-11-11T10:00:05Z\nCreated the following subagent:\n{{\n  \"conversationId\": \"{CHILD_ID}\"\n}}")
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 8,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:10Z",
                "content": format!("Created At: 2026-11-11T10:00:10Z\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-11T10:00:10Z sender={CHILD_ID} priority=HIGH content=Audit complete: 3 exports.\n</SYSTEM_MESSAGE>")
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 9,
                "source": "SYSTEM",
                "type": "CHECKPOINT",
                "status": "DONE",
                "created_at": "2026-11-11T10:01:00Z",
                "content": "# Resuming from a compaction\nSummary of prior steps.",
                "input_tokens": 800,
                "cache_read_tokens": 100,
                "output_tokens": 40
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 10,
                "source": "SYSTEM",
                "type": "ERROR_MESSAGE",
                "status": "ERROR",
                "created_at": "2026-11-11T10:01:05Z",
                "error": "transient upstream timeout"
            }))
            .unwrap(),
        ]
        .join("\n");

        let s = parse_str(PARENT_ID, &jsonl, None).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.harness, Harness::Antigravity);
        assert_eq!(s.messages.len(), 9); // 8 records + 1 synthetic CompactionBoundary

        // Step 0: User prompt + image attachment
        assert_eq!(
            (s.messages[0].role, s.messages[0].kind),
            (Role::User, MessageKind::Prompt)
        );
        assert!(
            matches!(&s.messages[0].content[1], Block::Image { data_ref: Some(p), .. } if p == "/workspace/demo/screen.png")
        );

        // Step 2: Planner response refined to SubagentSpawn by AffordanceSink
        assert_eq!(
            (s.messages[1].role, s.messages[1].kind),
            (Role::Assistant, MessageKind::SubagentSpawn)
        );
        let usage = s.messages[1].usage.as_ref().unwrap();
        assert_eq!(
            (usage.input_tokens, usage.cache_read_tokens, usage.output_tokens),
            (Some(1200), Some(400), Some(95))
        );

        // Step 3: the truncation notice's path is extracted; with no store behind this read it
        // stays a claim no reader opens.
        assert_eq!(s.messages[2].content[0].persisted_output_path(), None);
        let Block::ToolResult { details: Some(d), .. } = &s.messages[2].content[0] else {
            panic!("expected a tool result with details");
        };
        assert_eq!(
            d["persistedOutput"]["unresolved"],
            "/workspace/demo/.system_generated/steps/3/output.txt"
        );

        // Step 4: Permission denial surfaces via Affordance
        let affs = crate::affordance::of_session(&s).unwrap();
        assert!(
            affs.iter()
                .any(|a| a.kind == crate::affordance::Kind::Approval && a.action == "deny"),
            "expected approval/deny in {affs:?}"
        );
        assert!(
            affs.iter().any(|a| a.kind == crate::affordance::Kind::SubagentSpawn),
            "expected subagent_spawn in {affs:?}"
        );
        assert!(
            affs.iter().any(|a| a.kind == crate::affordance::Kind::SubagentReturn),
            "expected subagent_return in {affs:?}"
        );

        // Step 8: Child SYSTEM_MESSAGE classified as SubagentReturn + delivered_body unwrapped
        assert_eq!(
            (s.messages[5].role, s.messages[5].kind, s.messages[5].origin),
            (Role::User, MessageKind::SubagentReturn, Origin::Subagent)
        );
        let raw_ret = s.messages[5].text().unwrap();
        assert_eq!(
            ROOTLESS.delivered_body(&s.messages[5], &raw_ret),
            Some("Audit complete: 3 exports.")
        );

        // Step 9: CompactionBoundary + CompactionSummary
        assert_eq!(s.messages[6].kind, MessageKind::CompactionBoundary);
        assert_eq!(s.messages[7].kind, MessageKind::CompactionSummary);

        // Step 10: Error message
        assert_eq!(s.messages[8].kind, MessageKind::Error);
    }

    #[test]
    fn subagent_initial_system_message_sets_prompt_and_parent_lineage() {
        let child_jsonl = serde_json::to_string(&json!({
            "step_index": 0,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-11T10:00:05Z",
            "content": format!("Created At: 2026-11-11T10:00:05Z\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-11T10:00:05Z sender={PARENT_ID} priority=HIGH content=Audit all exported functions.\n</SYSTEM_MESSAGE>")
        }))
        .unwrap();
        let s = parse_str(CHILD_ID, &child_jsonl, None).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some(PARENT_ID));
        assert_eq!(s.messages.len(), 1);
        assert_eq!(
            (s.messages[0].role, s.messages[0].kind, s.messages[0].origin),
            (Role::User, MessageKind::Prompt, Origin::Subagent)
        );
        assert!(s.messages[0].is_turn());
    }

    #[test]
    fn deduplicates_across_roots_and_ignores_chunks_and_encrypted_pb() {
        let world = crate::test_world("antigravity-dedup");
        let gemini = world.base.join(".gemini");
        let cli_logs = gemini
            .join("antigravity-cli/brain")
            .join(PARENT_ID)
            .join(".system_generated/logs");
        let ide_logs = gemini
            .join("antigravity-ide/brain")
            .join(PARENT_ID)
            .join(".system_generated/logs");
        fs::create_dir_all(cli_logs.join("chunks/transcript_full")).unwrap();
        fs::create_dir_all(&ide_logs).unwrap();
        fs::create_dir_all(gemini.join("antigravity-cli/implicit")).unwrap();
        fs::create_dir_all(gemini.join("antigravity-cli/annotations")).unwrap();

        let step = serde_json::to_string(&json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "created_at": "2026-11-11T10:00:00Z",
            "content": "hello from full log"
        }))
        .unwrap();
        fs::write(cli_logs.join("transcript_full.jsonl"), format!("{step}\n")).unwrap();
        fs::write(cli_logs.join("transcript.jsonl"), format!("{step}\n")).unwrap();
        fs::write(cli_logs.join("chunks/transcript_full/0.jsonl"), format!("{step}\n")).unwrap();
        fs::write(ide_logs.join("overview.txt"), format!("{step}\n")).unwrap();
        fs::write(gemini.join("antigravity-cli/implicit/9999.pb"), [0xffu8; 32]).unwrap();
        fs::write(
            gemini.join("antigravity-cli/history.jsonl"),
            "{\"display\":\"ignored\"}\n",
        )
        .unwrap();
        fs::write(
            gemini
                .join("antigravity-cli/annotations")
                .join(format!("{PARENT_ID}.pbtxt")),
            "title: \"Escaped \\\"Title\\\" Here\"\n",
        )
        .unwrap();

        let adapter = Antigravity::with_home(&world.base);
        let found = adapter.discover().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, PARENT_ID);
        assert_eq!(found[0].path, cli_logs.join("transcript_full.jsonl"));
        assert_eq!(found[0].title.as_deref(), Some("Escaped \"Title\" Here"));

        let parsed = adapter.parse(&found[0]).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("Escaped \"Title\" Here"));
        assert_eq!(parsed.messages.len(), 1);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn enriches_model_cwd_git_and_lineage_from_readonly_sqlite_sidecars() {
        fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                out.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        fn pb_bytes(field: u64, payload: &[u8], out: &mut Vec<u8>) {
            pb_varint((field << 3) | 2, out);
            pb_varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }
        fn pb_u64(field: u64, val: u64, out: &mut Vec<u8>) {
            pb_varint(field << 3, out);
            pb_varint(val, out);
        }

        let world = crate::test_world("antigravity-sqlite");
        let app_dir = world.base.join(".gemini/antigravity-cli");
        let logs_dir = app_dir.join("brain").join(CHILD_ID).join(".system_generated/logs");
        fs::create_dir_all(&logs_dir).unwrap();
        fs::create_dir_all(app_dir.join("conversations")).unwrap();

        let lines = [
            serde_json::to_string(&json!({
                "step_index": 0,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-11T12:00:00Z",
                "content": "run check"
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-11T12:00:01Z",
                "content": "ok",
                "input_tokens": 50,
                "cache_read_tokens": 10,
                "output_tokens": 5
            }))
            .unwrap(),
        ]
        .join("\n");
        fs::write(logs_dir.join("transcript_full.jsonl"), format!("{lines}\n")).unwrap();

        // Build `conversation_summaries.db`
        let sum_db = app_dir.join("conversation_summaries.db");
        {
            let conn = rusqlite::Connection::open(&sum_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT PRIMARY KEY,
                    title TEXT,
                    preview TEXT,
                    workspace_uris TEXT,
                    parent_conversation_id TEXT,
                    agent_name TEXT,
                    source TEXT,
                    project_id TEXT,
                    last_modified_time TEXT
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    CHILD_ID,
                    "SQLite Summary Title",
                    "preview text",
                    "[\"file:///workspace/demo\"]",
                    PARENT_ID,
                    "research",
                    "CORTEX_TRAJECTORY_SOURCE_CLI",
                    "proj-demo",
                    "2026-11-11T12:00:05Z"
                ],
            )
            .unwrap();
        }

        // Build `conversations/<uuid>.db` with `gen_metadata` and `trajectory_metadata_blob`
        let conv_db = app_dir.join("conversations").join(format!("{CHILD_ID}.db"));
        {
            let conn = rusqlite::Connection::open(&conv_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);",
            )
            .unwrap();
            let mut timestamp = Vec::new();
            pb_u64(1, 1_794_398_401, &mut timestamp);
            let mut usage = Vec::new();
            pb_u64(4, 12, &mut usage);
            pb_u64(9, 3, &mut usage);
            let mut metadata = Vec::new();
            pb_bytes(1, &timestamp, &mut metadata);
            pb_bytes(9, &usage, &mut metadata);
            conn.execute("INSERT INTO steps VALUES (1, ?1)", [&metadata]).unwrap();
            let mut sub_model = Vec::new();
            pb_bytes(19, b"gemini-4-argon", &mut sub_model);
            let mut packed_steps = Vec::new();
            pb_varint(1, &mut packed_steps);
            let mut gen_blob = Vec::new();
            pb_bytes(1, &sub_model, &mut gen_blob);
            pb_bytes(2, &packed_steps, &mut gen_blob);
            conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (0, ?1)", [&gen_blob])
                .unwrap();

            let mut ws_sub = Vec::new();
            pb_bytes(1, b"file:///workspace/demo", &mut ws_sub);
            pb_bytes(2, b"file:///workspace/demo", &mut ws_sub);
            pb_bytes(4, b"feat/antigravity", &mut ws_sub);
            let mut ts_sub = Vec::new();
            pb_u64(1, 1_794_398_400, &mut ts_sub);
            let mut traj_blob = Vec::new();
            pb_bytes(1, &ws_sub, &mut traj_blob);
            pb_bytes(2, &ts_sub, &mut traj_blob);
            pb_bytes(5, PARENT_ID.as_bytes(), &mut traj_blob);
            pb_bytes(6, PARENT_ID.as_bytes(), &mut traj_blob);
            let mut spec = Vec::new();
            pb_bytes(1, b"researcher", &mut spec);
            pb_bytes(2, b"Inspect the storage contract", &mut spec);
            pb_bytes(8, &spec, &mut traj_blob);
            conn.execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                [&traj_blob],
            )
            .unwrap();
        }

        let adapter = Antigravity::with_home(&world.base);
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/workspace/demo")));
        assert_eq!(refs[0].title.as_deref(), Some("SQLite Summary Title"));

        let s = adapter.parse(&refs[0]).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.model.as_deref(), Some("gemini-4-argon"));
        // Metadata alone cannot identify the historical revision of this reply.
        assert_eq!(s.messages[1].model, None);
        assert_eq!(s.messages[1].usage.as_ref().unwrap().reasoning_tokens, None);
        assert_eq!(s.messages[1].usage.as_ref().unwrap().cache_creation_tokens, None);
        assert_eq!(
            s.git.as_ref().and_then(|g| g.branch.as_deref()),
            Some("feat/antigravity")
        );
        assert_eq!(s.lineage.parent.as_deref(), Some(PARENT_ID));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("research"));

        // Native lineage survives a missing summary catalog.
        fs::remove_file(&sum_db).unwrap();
        let native = adapter.parse(&refs[0]).unwrap();
        assert_eq!(native.lineage.parent.as_deref(), Some(PARENT_ID));
        assert_eq!(native.extra["antigravity"]["agent_type"], "researcher");
        assert_eq!(native.extra["antigravity"]["root_conversation_id"], PARENT_ID);

        // Ensure zero `-wal` / `-shm` coordination files were created next to either SQLite DB!
        for db in [&sum_db, &conv_db] {
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut p = db.as_os_str().to_os_string();
                p.push(suffix);
                assert!(!Path::new(&p).exists(), "created sidecar {}", Path::new(&p).display());
            }
        }
    }

    #[test]
    fn step_peek_handles_large_preambles_and_structured_conversation_id_extraction() {
        // Quoted prose containing `"conversationId": "spoof"` inside another field must not hijack
        // structured JSON extraction, and payloads past 1 KiB (up to 64 KiB) still resolve!
        let padding = "x".repeat(4096);
        let payload = serde_json::json!({
            "note": format!("prose mentioning {{\"conversationId\":\"spoofed-id\"}} {padding}"),
            "result": {
                "conversationId": "real-child-uuid-9999"
            }
        })
        .to_string();
        assert_eq!(
            extract_spawned_conversation_id(&payload).as_deref(),
            Some("real-child-uuid-9999")
        );

        let parent_id = "11111111-1111-4111-8111-111111111111";
        let child_id = "22222222-2222-4222-8222-222222222222";
        let long_header = " ".repeat(2048);
        let transcript = [
            serde_json::json!({
                "step_index": 0,
                "type": "SYSTEM_MESSAGE",
                "content": format!("{long_header}<SYSTEM_MESSAGE>\n[Message] sender={parent_id}\nAssigned work\n</SYSTEM_MESSAGE>")
            })
            .to_string(),
            serde_json::json!({
                "step_index": 1,
                "type": "TOOL_RESULT",
                "tool_call_id": "tc_sub",
                "tool_name": "invoke_subagent",
                "content": serde_json::json!({
                    "log": padding,
                    "echo": "{\"conversationId\":\"wrong\"}",
                    "spawn": { "conversationId": child_id }
                }).to_string()
            })
            .to_string(),
        ]
        .join("\n");

        let parsed = parse_str("33333333-3333-4333-8333-333333333333", &transcript, None).unwrap();
        assert_eq!(parsed.lineage.parent.as_deref(), Some(parent_id));
        assert_eq!(parsed.messages[0].origin, Origin::Subagent);
        assert_eq!(
            parsed.title.as_deref(),
            Some("[Message] sender=11111111-1111-4111-8111-111111111111 Assigned work")
        );
    }

    #[test]
    fn portable_brain_loc_user_implicit_source_and_stream_str_unwrapped_title() {
        let loc = BrainLoc::from_transcript(Path::new("/app/data/conversations/abcd-1234.db"))
            .expect("native .db path resolves without sqlite feature");
        assert_eq!(loc.app_data_dir, Path::new("/app/data"));
        assert_eq!(loc.brain_dir, Path::new("/app/data/brain/abcd-1234"));
        assert_eq!(loc.id, "abcd-1234");

        let line = serde_json::json!({
            "step_index": 0,
            "source": "USER_IMPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "content": "<USER_REQUEST>\nFix chunked zip ingest\n</USER_REQUEST>\n<ADDITIONAL_METADATA>ignored</ADDITIONAL_METADATA>"
        })
        .to_string();
        let mut matched = false;
        jsonl::slice(line.as_bytes(), |l| {
            matched = l.view.is_some_and(is_step_record_view);
            Ok(Flow::Stop)
        })
        .unwrap();
        assert!(matched, "USER_IMPLICIT must be recognized as a known step source");

        let parsed = parse_str("44444444-4444-4444-8444-444444444444", &line, None).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("Fix chunked zip ingest"));
        assert_eq!(parsed.messages[0].extra["antigravity"]["source"], "USER_IMPLICIT");
    }

    #[test]
    fn swarm_turn_boundary_park_mid_turn_wake_and_jsonl_sharded_spawns() {
        let world = crate::test_world("agy-swarm-park-and-shards");
        let app = world.base.join("antigravity-cli");
        let coord_id = PARENT_ID;
        let shard1 = "22222222-2222-4222-8222-222222222221";
        let shard2 = "22222222-2222-4222-8222-222222222222";
        let helper = "33333333-3333-4333-8333-333333333333";

        let write_log = |id: &str, rows: &[Value]| -> PathBuf {
            let dir = app.join("brain").join(id).join(".system_generated/logs");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("transcript_full.jsonl");
            let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
            fs::write(&path, text).unwrap();
            path
        };

        let swarm_dir = app.join("brain").join(coord_id).join(".system_generated/swarm");
        fs::create_dir_all(&swarm_dir).unwrap();
        fs::write(
            swarm_dir.join("members.json"),
            json!({
                "members": {
                    "coordinator": {"conversation_id": coord_id, "role": "coordinator"},
                    "worker-1": {"conversation_id": shard1, "role": "worker", "wake_mode": "mentions", "phase": "RUNNING", "killed": false},
                    "worker-2": {"conversation_id": shard2, "role": "worker", "wake_mode": "mentions", "phase": "RUNNING", "killed": false},
                    "helper": {"conversation_id": helper, "role": "worker", "added_by": "worker-1", "wake_mode": "mentions", "phase": "RUNNING", "killed": false}
                }
            })
            .to_string(),
        )
        .unwrap();

        let coord_path = write_log(
            coord_id,
            &[
                json!({"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-15T03:00:00Z","content":"run sharded swarm"}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T03:00:01Z","tool_calls":[
                    {"name":"swarm","args":{"action":"add","name":"worker","n":2,"brief":"do shard"}}
                ]}),
                json!({"step_index":2,"type":"GENERIC","status":"DONE","created_at":"2026-11-15T03:00:01Z","content":json!({
                    "status":"added",
                    "conversation_ids":[shard1, shard2]
                }).to_string()}),
            ],
        );

        // `shard1`: added `helper` and completed `GENERIC`, with NO subsequent `PLANNER_RESPONSE` yet => mid-turn (`running`)!
        let shard1_path = write_log(
            shard1,
            &[
                json!({"step_index":0,"type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-15T03:00:01Z","content":format!("<SYSTEM_MESSAGE>\n[Message] sender={coord_id} content=do shard 1\n</SYSTEM_MESSAGE>")}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T03:00:02Z","tool_calls":[
                    {"name":"swarm","args":{"action":"add","name":"helper","brief":"help shard 1"}}
                ]}),
                json!({"step_index":2,"type":"GENERIC","status":"DONE","created_at":"2026-11-15T03:00:02Z","content":json!({
                    "status":"added","name":"helper","conversation_id":helper
                }).to_string()}),
            ],
        );

        // `shard2`: parked on `swarm_wait`, then woken by a trailing board message => mid-turn (`running`), then after removing wake => `"waiting"`.
        let shard2_woken = [
            json!({"step_index":0,"type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-15T03:00:01Z","content":format!("<SYSTEM_MESSAGE>\n[Message] sender={coord_id} content=do shard 2\n</SYSTEM_MESSAGE>")}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T03:00:02Z","tool_calls":[
                {"name":"post_message","args":{"text":"shard 2 interim"}},
                {"name":"swarm_wait","args":{"since":1}}
            ]}),
            json!({"step_index":2,"type":"GENERIC","status":"DONE","created_at":"2026-11-15T03:00:02Z","content":"{\"status\":\"posted\",\"seq\":2}"}),
            json!({"step_index":3,"type":"GENERIC","status":"DONE","created_at":"2026-11-15T03:00:02Z","content":"Waiting for a message..."}),
            json!({"step_index":4,"type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-15T03:00:03Z","content":format!("<SYSTEM_MESSAGE>\n[Message] sender={coord_id} content=[swarm] new board message #3\n</SYSTEM_MESSAGE>")}),
        ];
        let shard2_path = write_log(shard2, &shard2_woken);

        // `helper`: ended at 0-tool `PLANNER_RESPONSE` => parked (`"waiting"`), unless coordinator `swarm status` says `"stopped"`.
        let _helper_path = write_log(
            helper,
            &[
                json!({"step_index":0,"type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-15T03:00:02Z","content":format!("<SYSTEM_MESSAGE>\n[Message] sender={shard1} content=help shard 1\n</SYSTEM_MESSAGE>")}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T03:00:03Z","content":"helper finished"}),
            ],
        );

        let adapter = Antigravity::with_home(&world.base);
        let coord_ref = SessionRef::new(coord_id, Harness::Antigravity, &coord_path);
        let forest = adapter
            .forest(&coord_ref, crate::SubagentDemand::Rich, None, &|_, _| true)
            .unwrap();
        assert_eq!(forest.len(), 3);
        let find_ref = |id: &str| forest.iter().find(|a| a.session.id == id).unwrap();
        assert_eq!(find_ref(shard1).tool_use_id.as_deref(), Some("cv:antigravity:1.0"));
        assert_eq!(find_ref(shard2).tool_use_id.as_deref(), Some("cv:antigravity:1.0"));
        assert_eq!(find_ref(helper).tool_use_id, None);

        let helper_sess = adapter.parse(&find_ref(helper).session).unwrap();
        assert_eq!(helper_sess.lineage.parent.as_deref(), Some(coord_id));
        assert_eq!(
            helper_sess.lineage.agent_path.as_deref(),
            Some("/coordinator/worker-1/helper")
        );
        assert_eq!(
            helper_sess.lineage.spawned_by_tool_use.as_deref(),
            Some("cv:antigravity:1.0")
        );
        assert_eq!(helper_sess.extra["antigravity"]["added_by_id"], shard1);

        assert_eq!(
            adapter.subagent_end(&shard1_path).unwrap().unwrap().stopped(),
            Some(false)
        );
        assert_eq!(
            adapter.subagent_end(&shard2_path).unwrap().unwrap().stopped(),
            Some(false)
        );

        let lanes = crate::lanes::lanes_of(&coord_ref).unwrap();
        let status = |id: &str| {
            lanes
                .iter()
                .find(|l| l.session_id == id)
                .unwrap()
                .status
                .as_str()
                .to_owned()
        };
        assert_eq!(status(shard1), "running");
        assert_eq!(status(shard2), "running");
        assert_eq!(status(helper), "waiting");

        // Park shard2 by dropping the trailing wake, and add a newer `swarm status` reporting `helper` as `"stopped"`.
        write_log(shard2, &shard2_woken[..4]);
        write_log(
            coord_id,
            &[
                json!({"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-15T03:00:00Z","content":"run sharded swarm"}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T03:00:01Z","tool_calls":[
                    {"name":"swarm","args":{"action":"add","name":"worker","n":2,"brief":"do shard"}}
                ]}),
                json!({"step_index":2,"type":"GENERIC","status":"DONE","created_at":"2026-11-15T03:00:01Z","content":json!({
                    "status":"added","conversation_ids":[shard1, shard2]
                }).to_string()}),
                json!({"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T03:00:05Z","tool_calls":[
                    {"name":"swarm","args":{"action":"status"}}
                ]}),
                json!({"step_index":4,"type":"GENERIC","status":"DONE","created_at":"2026-11-15T03:00:05Z","content":json!({
                    "members":[
                        {"name":"worker-1","state":"working","killed":false},
                        {"name":"worker-2","state":"waiting","killed":false},
                        {"name":"helper","state":"stopped","killed":false}
                    ]
                }).to_string()}),
            ],
        );
        let lanes2 = crate::lanes::lanes_of(&coord_ref).unwrap();
        let lane2 = |id: &str| lanes2.iter().find(|l| l.session_id == id).unwrap();
        assert_eq!(lane2(shard1).status.as_str(), "running");
        assert_eq!(lane2(shard2).status.as_str(), "waiting");
        assert_eq!(lane2(shard2).last_text.as_deref(), Some("shard 2 interim"));
        assert_eq!(lane2(helper).status, crate::lanes::LaneStatus::Stopped);
    }

    #[test]
    fn forest_and_lanes_skip_vanished_child_and_chunk_while_failing_on_corrupt_json() {
        let world = crate::test_world("agy-vanished-child-chunk");
        let app = world.base.join("antigravity-cli");
        let coord_id = PARENT_ID;
        let live_id = "44444444-4444-4444-8444-444444444441";
        let chunk_id = "44444444-4444-4444-8444-444444444442";
        let unavail_id = "44444444-4444-4444-8444-444444444443";

        let coord_dir = app.join("brain").join(coord_id).join(".system_generated");
        let sub_dir = coord_dir.join("subagents");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::create_dir_all(coord_dir.join("logs")).unwrap();
        let coord_path = coord_dir.join("logs/transcript_full.jsonl");
        fs::write(
            &coord_path,
            format!(
                "{}\n",
                json!({"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-15T04:00:00Z","content":"run workers"})
            ),
        )
        .unwrap();

        let write_desc = |id: &str, name: &str| -> PathBuf {
            let p = sub_dir.join(format!("{id}.json"));
            fs::write(
                &p,
                json!({
                    "conversationId": id,
                    "subagentDescriptor": {"typeName": name, "role": "worker"}
                })
                .to_string(),
            )
            .unwrap();
            p
        };
        write_desc(live_id, "live");
        let chunk_desc = write_desc(chunk_id, "chunked");

        let live_logs = app.join("brain").join(live_id).join(".system_generated/logs");
        fs::create_dir_all(&live_logs).unwrap();
        fs::write(
            live_logs.join("transcript_full.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"step_index":0,"type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-15T04:00:01Z","content":"go"}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T04:00:02Z","content":"live ok"})
            ),
        )
        .unwrap();

        let chunk_dir = app
            .join("brain")
            .join(chunk_id)
            .join(".system_generated/logs/chunks/transcript_full");
        fs::create_dir_all(&chunk_dir).unwrap();
        let chunk_file = chunk_dir.join("00000000.jsonl");
        fs::write(
            &chunk_file,
            format!(
                "{}\n{}\n",
                json!({"step_index":0,"type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-15T04:00:01Z","content":"go chunk"}),
                json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T04:00:02Z","content":"chunk ok"})
            ),
        )
        .unwrap();

        let adapter = Antigravity::with_home(&world.base);
        let coord_ref = SessionRef::new(coord_id, Harness::Antigravity, &coord_path);
        let initial_lanes = crate::lanes::lanes_of(&coord_ref).unwrap();
        assert_eq!(initial_lanes.len(), 2);

        // Unlinking `00000000.jsonl` while leaving `chunks/transcript_full/` empty surfaces NotFound
        // (`vanished_at == true`), so mid-pass `lanes` skips it instead of aborting on "empty Antigravity chunk history".
        fs::remove_file(&chunk_file).unwrap();
        let err = open_transcript(&chunk_file).err().expect("unlinked chunk must error");
        assert!(
            crate::harness::vanished_at(&chunk_file, &err),
            "unlinked chunk must satisfy vanished_at: {err:#}"
        );

        #[cfg(unix)]
        {
            // A broken symlink at `00000000.jsonl` fails `File::open` with NotFound, but `symlink_metadata`
            // succeeds, so `vanished_at` is false and broken symlinks still fail.
            std::os::unix::fs::symlink(chunk_dir.join("missing-target.jsonl"), &chunk_file).unwrap();
            let sym_err = open_transcript(&chunk_file).err().expect("broken symlink must error");
            assert!(
                !crate::harness::vanished_at(&chunk_file, &sym_err),
                "broken symlink must not be treated as vanished"
            );
            fs::remove_file(&chunk_file).unwrap();
        }

        // If the descriptor remains while the chunk vanished, forest falls back to unavailable;
        // once the descriptor is also unlinked, lanes_of skips the vanished worker cleanly.
        let with_desc_only = crate::lanes::lanes_of(&coord_ref).unwrap();
        assert_eq!(with_desc_only.len(), 2);
        assert_eq!(
            with_desc_only
                .iter()
                .find(|l| l.session_id == chunk_id)
                .unwrap()
                .status
                .as_str(),
            "unavailable"
        );
        fs::remove_file(&chunk_desc).unwrap();
        let surviving = crate::lanes::lanes_of(&coord_ref).unwrap();
        assert_eq!(surviving.len(), 1);
        assert_eq!(surviving[0].session_id, live_id);

        // Corrupt JSON in a descriptor read by `parse` (or corrupt SQLite child store in `forest`) still fails.
        let unavail_desc = write_desc(unavail_id, "unavail");
        let forest = adapter
            .forest(&coord_ref, crate::SubagentDemand::Rich, None, &|_, _| true)
            .unwrap();
        let unavail_ref = forest
            .iter()
            .find(|a| a.session.id == unavail_id)
            .unwrap()
            .session
            .clone();
        fs::write(&unavail_desc, "{corrupt json\n").unwrap();
        let corrupt_err = adapter
            .parse(&unavail_ref)
            .expect_err("corrupt descriptor JSON must fail");
        assert!(!crate::harness::vanished_at(&unavail_desc, &corrupt_err));
        fs::remove_file(&unavail_desc).unwrap();

        #[cfg(feature = "sqlite")]
        {
            let vanished_db = app.join("conversations/44444444-4444-4444-8444-444444444444.db");
            let db_err = scan_transcript("44444444-4444-4444-8444-444444444444", &vanished_db, Some(&app))
                .expect_err("missing native child store must surface NotFound");
            assert!(
                crate::harness::vanished_at(&vanished_db, &db_err),
                "vanished native child store must satisfy vanished_at: {db_err:#}"
            );
        }

        // Corrupt chunk filename in an active child log fails `lanes_of` rather than silently skipping.
        let bad_chunk_id = "44444444-4444-4444-8444-444444444445";
        let _bad_chunk_desc = write_desc(bad_chunk_id, "badchunk");
        let bad_chunk_dir = app
            .join("brain")
            .join(bad_chunk_id)
            .join(".system_generated/logs/chunks/transcript_full");
        fs::create_dir_all(&bad_chunk_dir).unwrap();
        fs::write(bad_chunk_dir.join("00000000.jsonl"), "{}\n").unwrap();
        fs::write(bad_chunk_dir.join("corrupt.jsonl"), "{not json\n").unwrap();
        assert!(
            crate::lanes::lanes_of(&coord_ref).is_err(),
            "corrupt chunk entry must fail lanes_of"
        );
    }
}
