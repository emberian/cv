//! Claude Code `Workflow`-tool runs, parsed as **first-class structured objects** — not merely a
//! tier of sub-agents.
//!
//! A `Workflow` tool invocation runs a JS *script* that fans out a phased tree of sub-agents and
//! aggregates their structured returns. Claude Code records each run three ways under the session's
//! sidecar dir (`<projects>/<encoded>/<sessionId>/`):
//!
//! 1. **`workflows/wf_<runId>.json`** — the run's *state*: `workflowName`, `status`
//!    (`completed`/`killed`), the inline `script` + its `scriptPath`, `phases[]` (`{title, detail}`),
//!    a `workflowProgress[]` interleaving `workflow_phase` + `workflow_agent` entries (the phase tree
//!    with every agent's `state`/`tokens`/`toolCalls`/`resultPreview`/`promptPreview` under its
//!    phase), `agentCount`/`totalTokens`/`totalToolCalls`/`durationMs`, and the aggregated `result`.
//! 2. **`workflows/scripts/<name>-wf_<runId>.js`** — the driving script on disk (same bytes as the
//!    inline `script`, but the file is the durable copy and the resume entry point).
//! 3. **`subagents/workflows/wf_<runId>/`** — each agent's full transcript (`agent-*.jsonl`) +
//!    `journal.jsonl` (the orchestrator's structured per-agent results). This tier is what
//!    [`super::claude::subagent_tree`] already walks; here we add the *state/phase/script* layer.
//!
//! This module reads (1) and (2) into a [`Workflow`] so `cv workflow <runId>` can show
//! phases → agents → outcomes + the script — making workflows queryable on their own terms.

use super::claude::sidecar;
use crate::json::{ReadInput, Validation};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One run of the `Workflow` tool, read from `workflows/wf_<runId>.json` (+ its on-disk script).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Workflow {
    /// The run id (`wf_<…>`), matching the `subagents/workflows/<runId>/` dir.
    pub run_id: String,
    /// `workflowName` — the script's `meta.name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `status`: `completed`, `killed`, … (the orchestrator's terminal state for the run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The run's freeform `summary` (the orchestrator's own roll-up line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `error` text when the run did not complete (`status: killed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `defaultModel` the workflow launched its agents with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Number of agents the run spawned (`agentCount`).
    pub agent_count: u64,
    /// Aggregate `totalTokens` across the run's agents.
    pub total_tokens: u64,
    /// Aggregate `totalToolCalls` across the run's agents.
    pub total_tool_calls: u64,
    /// Wall-clock `durationMs` of the whole run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Where the driving script lives on disk (`scriptPath`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<PathBuf>,
    /// The driving script source (the inline `script` — same bytes as `script_path`'s file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The run this one resumed from (`resumeFromRunId`), for resumed/cached workflows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<String>,
    /// The workflow script's aggregated **return value** (`result`) — what the script's final
    /// `return` produced. The harvest payload; a run without it either returned nothing or died.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The script's `log()` lines, in order — the run's own progress narration (includes per-agent
    /// failure notes like rate-limit hits).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// The `args` value the workflow was invoked with (verbatim; may itself be a JSON-encoded
    /// string if the caller stringified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// The background task id (`taskId`) — matches the "Task ID: …" in the launch tool_result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// When the run started (`startTime` epoch-ms, falling back to the ISO `timestamp`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The declared phases, in order, each with the agents that ran under it (from
    /// `workflowProgress`). The phase tree the brief asks to surface richly.
    pub phases: Vec<WorkflowPhase>,
    /// Agents whose `workflowProgress` entry named a `phaseIndex` with no matching phase header
    /// (defensive — empty in practice). Surfaced so no agent is silently dropped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphan_agents: Vec<WorkflowAgent>,
}

/// One declared phase of a workflow (`phases[]` entry) with the agents that ran under it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowPhase {
    /// 1-based phase index (matches `workflow_agent.phaseIndex`).
    pub index: u64,
    /// The phase title (`phases[].title` / `workflow_phase.title`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The phase's `detail` blurb from the script's `meta.phases`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The agents that ran under this phase, in spawn order.
    pub agents: Vec<WorkflowAgent>,
}

/// One agent within a workflow run — a `workflow_agent` entry of `workflowProgress`. Carries the
/// agent's live telemetry (`state`/`tokens`/`toolCalls`) and the previews the orchestrator kept of
/// its prompt and structured result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowAgent {
    /// 1-based agent index across the run.
    pub index: u64,
    /// The agent's short label (`label`), e.g. `migrate:stark→p3`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The `agent-<id>` id (bare, no prefix) — the key into `subagents/workflows/<run>/` and the
    /// journal. `None` for a queued-but-never-started slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// 1-based phase index this agent ran under.
    pub phase_index: u64,
    /// `state`: `done` / `error` / `progress` / `start` (live or terminal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The model this agent ran on (may differ from the workflow default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tokens this agent consumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Tool calls this agent made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    /// This agent's wall-clock `durationMs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// `error` text when the agent's `state` is `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether this agent's result was served from cache (a resumed run).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// Head of the prompt the orchestrator handed this agent (`promptPreview`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
    /// Head of the agent's structured return value (`resultPreview`) — the real outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    /// Retry attempt number (`attempt`), 1-based; >1 means the agent was re-issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// When the agent started running (`startedAt`, epoch-ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The agent's last recorded heartbeat (`lastProgressAt`, epoch-ms). For a non-terminal agent
    /// (crash/kill mid-run), this is when it was last alive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last tool the agent was seen using (`lastToolName`) — for a dead/interrupted agent,
    /// where it was when it stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    /// One-line summary of that last tool call (`lastToolSummary`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_summary: Option<String>,
    /// The agent's **full** journaled return value, from the run's `journal.jsonl` — the complete
    /// structured result, where `result_preview` is a ~400-char head. Populated by
    /// [`Workflow::attach_journal`], not by the state-file parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_result: Option<Value>,
}

impl Workflow {
    /// Total agents across all phases (+ any orphans) — the phase-tree count, which matches
    /// `agent_count` for a well-formed run.
    #[cfg(test)]
    pub fn agents_in_phases(&self) -> usize {
        self.phases.iter().map(|p| p.agents.len()).sum::<usize>() + self.orphan_agents.len()
    }

    /// A histogram of agent terminal states (`done`/`error`/…) across the run, for a one-glance
    /// "how did this workflow go" summary.
    pub fn state_counts(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for a in self.phases.iter().flat_map(|p| &p.agents).chain(&self.orphan_agents) {
            if let Some(s) = &a.state {
                *m.entry(s.clone()).or_insert(0) += 1;
            }
        }
        m
    }

    /// Enrich every agent with its **full** journaled return value from the run's
    /// `subagents/workflows/<runId>/journal.jsonl` (the state file only keeps a ~400-char
    /// `resultPreview`). When the journal is absent or empty (e.g. a crashed run), falls back to
    /// each missing agent's own transcript: the final assistant text of
    /// `subagents/workflows/<runId>/agent-<id>.jsonl` is the return the orchestrator never got to
    /// journal — the harvestable work.
    pub fn attach_journal(&mut self, parent_path: &Path) {
        let results = run_journal(parent_path, &self.run_id);
        for a in self
            .phases
            .iter_mut()
            .flat_map(|p| &mut p.agents)
            .chain(&mut self.orphan_agents)
        {
            if let Some(id) = &a.agent_id {
                a.journal_result = results
                    .get(id)
                    .or_else(|| results.get(id.strip_prefix("agent-").unwrap_or(id)))
                    .cloned();
            }
        }
        // A crash can leave a partial journal: a sibling's recorded result must not hide
        // completed work from agents whose returns never reached the orchestrator.
        self.attach_transcript_returns(parent_path);
    }

    /// The missing-journal-entry fallback for [`Self::attach_journal`]: read each agent's return straight
    /// from its transcript in the run's debris dir. Only fills agents whose transcript contains
    /// substantive assistant text; journaled results remain authoritative.
    fn attach_transcript_returns(&mut self, parent_path: &Path) {
        let Some(dir) = sidecar::workflow_run_dir(parent_path, &self.run_id) else {
            return;
        };
        if !dir.is_dir() {
            return;
        }
        for a in self
            .phases
            .iter_mut()
            .flat_map(|p| &mut p.agents)
            .chain(&mut self.orphan_agents)
        {
            if a.journal_result.is_some() {
                continue;
            }
            if let Some(id) = &a.agent_id {
                let bare = id.strip_prefix("agent-").unwrap_or(id);
                if let Some(ret) = super::claude::subagent_return(&dir.join(format!("agent-{bare}.jsonl")))
                    .unwrap_or_else(|error| {
                        crate::env::warn_once("workflow agent return", format!("{bare}: {error:#}"));
                        None
                    })
                {
                    a.journal_result = Some(Value::String(ret));
                }
            }
        }
    }
}

/// A run's journaled results ([`sidecar::journal_results`]) by parent transcript + run id.
fn run_journal(parent_path: &Path, run_id: &str) -> BTreeMap<String, Value> {
    sidecar::workflow_run_dir(parent_path, run_id)
        .map(|d| sidecar::journal_results(&d))
        .unwrap_or_default()
}

/// The directory holding a session's workflow *state* files: `<sessionId>/workflows/`. (Distinct
/// from `<sessionId>/subagents/workflows/`, which holds the per-run agent transcripts.)
fn workflows_dir(parent_path: &Path) -> Option<PathBuf> {
    sidecar::workflows_dir(parent_path)
}

/// Enumerate state files in the same order for rich reads and checked selector projection.
fn state_paths(parent_path: &Path) -> Vec<PathBuf> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_state = p.extension().and_then(|x| x.to_str()) == Some("json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("wf_"));
            if is_state {
                paths.push(p);
            }
        }
    }
    paths
}

/// Every recorded workflow run, newest first; empty when the state directory is absent.
/// Claude Code's run store, as the [`crate::harness::Workflows`] capability its adapter offers.
pub(crate) struct Runs;
pub(crate) static RUNS: Runs = Runs;

impl crate::harness::Workflows for Runs {
    fn runs(&self, r: &crate::SessionRef) -> Vec<Workflow> {
        workflows(&r.path)
    }
    fn named(&self, r: &crate::SessionRef, name: &str) -> Vec<Workflow> {
        workflows_named(&r.path, name)
    }
    fn run(&self, r: &crate::SessionRef, key: &str) -> Option<Workflow> {
        workflow(&r.path, key)
    }
    fn run_id(&self, r: &crate::SessionRef, key: &str) -> Option<String> {
        workflow_id(&r.path, key)
    }
    fn ghosts(&self, r: &crate::SessionRef) -> Vec<WorkflowLaunch> {
        ghost_launches(&r.path)
    }
    fn has_records(&self, r: &crate::SessionRef) -> bool {
        has_workflow_records(&r.path)
    }
    /// A filename scan of both tiers (`subagents/` and `subagents/workflows/<run>/`); nothing is
    /// parsed.
    fn has_agent(&self, r: &crate::SessionRef, stem: &str) -> bool {
        use crate::harness::claude::sidecar;
        let Some(base) = sidecar::subagents_dir(&r.path) else {
            return false;
        };
        let has_match = |dir: &Path| {
            std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".jsonl") && n.starts_with(stem))
            })
        };
        has_match(&base)
            || sidecar::workflow_runs_dir(&r.path)
                .and_then(|d| std::fs::read_dir(d).ok())
                .into_iter()
                .flatten()
                .flatten()
                .any(|run| has_match(&run.path()))
    }
}

/// Sort key placing a run newest first: the state file's mtime (the JSON carries an ISO
/// `timestamp` too, but mtime needs no parse), keyed through the on-disk `<runId>.json` name.
fn state_recency(parent_path: &Path, run_id: &str) -> std::cmp::Reverse<Option<std::time::SystemTime>> {
    std::cmp::Reverse(
        workflows_dir(parent_path)
            .map(|dir| dir.join(format!("{run_id}.json")))
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok()),
    )
}

pub fn workflows(parent_path: &Path) -> Vec<Workflow> {
    let paths = state_paths(parent_path);
    let mut out = crate::par_filter_map(paths, |p| parse_state_file(&p));
    out.sort_by_cached_key(|w| state_recency(parent_path, &w.run_id));
    out
}

/// One workflow run by its `runId` **or its workflow name**, or `None` if nothing matches
/// unambiguously. Run ids accept the bare/`wf_`-prefixed form and unique prefixes; names accept an
/// exact match (newest run wins when a name was re-run) and a unique name-prefix.
pub fn workflow(parent_path: &Path, key: &str) -> Option<Workflow> {
    let mut index = workflow_index(parent_path);
    let selected = resolve_workflow_index(&index, key, |entry| (entry.1.as_str(), entry.2.as_deref()))?;
    let (source, _, _) = index.swap_remove(selected);
    #[cfg(test)]
    record_state_materialization(source.path());
    source
        .read(ReadInput::Document, Validation::Value, |view| {
            Ok(parse_state(view.resolve()?.as_ref()))
        })
        .ok()
}

/// Resolve a workflow selector without owning script, progress, argument or return bodies.
pub fn workflow_id(parent_path: &Path, key: &str) -> Option<String> {
    let mut index = workflow_index(parent_path);
    let selected = resolve_workflow_index(&index, key, |entry| (entry.1.as_str(), entry.2.as_deref()))?;
    Some(index.swap_remove(selected).1)
}

fn workflow_index(parent_path: &Path) -> Vec<(crate::json::JsonSource, String, Option<String>)> {
    let mut index = crate::par_filter_map(state_paths(parent_path), |path| {
        let source = crate::json::JsonSource::capture(&path).ok()?;
        let (id, name) = source
            .read(ReadInput::Document, Validation::Value, |view| {
                Ok((
                    crate::affordance::input::field_string(view, "runId")?.unwrap_or_default(),
                    crate::affordance::input::field_string(view, "workflowName")?,
                ))
            })
            .ok()?;
        Some((source, id, name))
    });
    // Keep the existing wire-ID-based mtime ordering, including repeated workflow names.
    index.sort_by_cached_key(|entry| state_recency(parent_path, &entry.1));
    index
}

fn resolve_workflow_index<T>(
    all: &[T],
    key: &str,
    identity: impl for<'a> Fn(&'a T) -> (&'a str, Option<&'a str>),
) -> Option<usize> {
    let want = key.strip_prefix("wf_").unwrap_or(key);
    let mut prefix = None;
    let mut matches = 0;
    for (i, entry) in all.iter().enumerate() {
        let (run, _) = identity(entry);
        let bare = run.strip_prefix("wf_").unwrap_or(run);
        if bare == want || run == key {
            return Some(i);
        }
        if bare.starts_with(want) {
            prefix.get_or_insert(i);
            matches += 1;
        }
    }
    if matches == 1 {
        return prefix;
    }
    if let Some(i) = all.iter().position(|entry| identity(entry).1 == Some(key)) {
        return Some(i);
    }
    let mut first = None;
    let mut name = None;
    for (i, entry) in all.iter().enumerate() {
        if let Some(candidate) = identity(entry).1.filter(|name| name.starts_with(key)) {
            if name.is_some_and(|name| name != candidate) {
                return None;
            }
            first.get_or_insert(i);
            name = Some(candidate);
        }
    }
    first
}

/// The `(name, run_id)` pairs readable from `workflows/scripts/*.js` **filenames** alone — the
/// zero-parse half of the run-name index (the persisted basename is `<name>-wf_<runId>.js`).
fn scripts_index(parent_path: &Path) -> Vec<(String, String)> {
    let Some(dir) = workflows_dir(parent_path).map(|d| d.join("scripts")) else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) != Some("js") || !e.path().is_file() {
                continue;
            }
            if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                if let (Some(name), Some(rid)) = script_path_identity(stem) {
                    if rid.len() > "wf_".len() {
                        out.push((name, rid));
                    }
                }
            }
        }
    }
    out
}

/// Cheap, complete `(name, run_id)` index over a session's recorded runs without materializing
/// the state JSONs. Names come from script filenames where a script was persisted; otherwise
/// a checked document view selects the top-level workflowName. Sessions with no
/// `workflows/` dir return empty at the cost of one failed `read_dir`.
pub fn run_names(parent_path: &Path) -> Vec<(String, String)> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    let by_script: BTreeMap<String, String> = scripts_index(parent_path).into_iter().map(|(n, r)| (r, n)).collect();
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let Some(rid) = p
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| n.starts_with("wf_") && n.ends_with(".json"))
                .map(|n| n.trim_end_matches(".json").to_string())
            else {
                continue;
            };
            let name = by_script.get(&rid).cloned().or_else(|| {
                crate::json::JsonSource::capture(&p)
                    .ok()?
                    .read(ReadInput::Document, Validation::Value, |view| {
                        crate::affordance::input::field_string(view, "workflowName")
                    })
                    .ok()
                    .flatten()
            });
            if let Some(name) = name {
                out.push((name, rid));
            }
        }
    }
    out
}

/// Script files whose run has **no state file** — a launch that persisted its script (written at
/// launch) but whose state JSON never made it to disk. The on-disk half of ghost evidence, and
/// the only place a ghost's run id (→ its `subagents/workflows/<runId>/` debris dir) survives.
pub fn orphan_scripts(parent_path: &Path) -> Vec<(String, String)> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    scripts_index(parent_path)
        .into_iter()
        .filter(|(_, rid)| !dir.join(format!("{rid}.json")).exists())
        .collect()
}

/// Whether a session recorded anything workflow-shaped at all (state files OR persisted scripts)
/// — the cheap bound for fleet-wide scans, replacing a full parse of every state file.
pub fn has_workflow_records(parent_path: &Path) -> bool {
    let Some(dir) = workflows_dir(parent_path) else {
        return false;
    };
    let any_state = fs::read_dir(&dir).into_iter().flatten().flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("wf_") && n.ends_with(".json"))
    });
    any_state
        || fs::read_dir(dir.join("scripts"))
            .map(|mut rd| rd.next().is_some())
            .unwrap_or(false)
}

/// The runs whose name starts with `prefix`, fully parsed — but ONLY those: the cheap
/// [`run_names`] index picks the candidates, so non-matching (multi-MB) state files are never
/// parsed. The fleet-wide by-name search rides on this.
pub fn workflows_named(parent_path: &Path, prefix: &str) -> Vec<Workflow> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    run_names(parent_path)
        .into_iter()
        .filter(|(n, _)| n.starts_with(prefix))
        .filter_map(|(_, rid)| parse_state_file(&dir.join(format!("{rid}.json"))))
        .collect()
}

/// Parse one `wf_<runId>.json` state file into a [`Workflow`]. Tolerant: a missing/odd field is an
/// absent option, never a failure (the catalog/forest views degrade gracefully).
fn parse_state_file(path: &Path) -> Option<Workflow> {
    #[cfg(test)]
    record_state_materialization(path);
    let txt = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    Some(parse_state(&v))
}

#[cfg(test)]
static STATE_MATERIALIZATIONS: std::sync::Mutex<(Option<PathBuf>, Vec<PathBuf>)> =
    std::sync::Mutex::new((None, Vec::new()));
#[cfg(test)]
fn record_state_materialization(path: &Path) {
    let mut trace = STATE_MATERIALIZATIONS.lock().unwrap();
    if trace.0.as_ref().is_some_and(|root| path.starts_with(root)) {
        trace.1.push(path.to_path_buf());
    }
}

/// Parse a workflow state `Value` into a [`Workflow`] (pure — the testable core of
/// [`parse_state_file`]).
pub(crate) fn parse_state(v: &Value) -> Workflow {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let u = |k: &str| v.get(k).and_then(Value::as_u64);
    let run_id = s("runId").unwrap_or_default();

    // Build the phase tree from `workflowProgress`: `workflow_phase` entries declare the ordered
    // headers; `workflow_agent` entries slot under their `phaseIndex`. We also fold in the
    // `phases[]` array for the per-phase `detail` (which `workflowProgress` headers omit).
    let progress = v.get("workflowProgress").and_then(Value::as_array);
    let phase_details = phase_detail_map(v);

    // Ordered phase headers (index → title), preserving first-seen order.
    let mut phase_order: Vec<u64> = Vec::new();
    let mut phase_title: BTreeMap<u64, Option<String>> = BTreeMap::new();
    if let Some(arr) = progress {
        for e in arr {
            if e.get("type").and_then(Value::as_str) == Some("workflow_phase") {
                if let Some(idx) = e.get("index").and_then(Value::as_u64) {
                    if !phase_title.contains_key(&idx) {
                        phase_order.push(idx);
                    }
                    phase_title.insert(idx, e.get("title").and_then(Value::as_str).map(str::to_string));
                }
            }
        }
    }
    // Fall back to the declared `phases[]` (1-based) when `workflowProgress` had no phase headers.
    if phase_order.is_empty() {
        if let Some(arr) = v.get("phases").and_then(Value::as_array) {
            for (i, p) in arr.iter().enumerate() {
                let idx = (i + 1) as u64;
                phase_order.push(idx);
                phase_title.insert(idx, p.get("title").and_then(Value::as_str).map(str::to_string));
            }
        }
    }

    // The zero phase is inserted at most once; testing its ordered Vec for every unnumbered
    // agent would repeatedly scan all declared phases.
    let mut zero_phase = phase_order.contains(&0);
    // Agents grouped by phaseIndex (in spawn order).
    let mut by_phase: BTreeMap<u64, Vec<WorkflowAgent>> = BTreeMap::new();
    let mut orphan_agents: Vec<WorkflowAgent> = Vec::new();
    if let Some(arr) = progress {
        for e in arr {
            if e.get("type").and_then(Value::as_str) == Some("workflow_agent") {
                let agent = parse_agent(e);
                let pidx = agent.phase_index;
                if phase_title.contains_key(&pidx) {
                    by_phase.entry(pidx).or_default().push(agent);
                } else if pidx == 0 {
                    // No phaseIndex recorded → treat as a phase-less agent only if there are no
                    // phases at all; otherwise it's an orphan worth surfacing.
                    if phase_order.is_empty() {
                        orphan_agents.push(agent);
                    } else {
                        by_phase.entry(0).or_default().push(agent);
                        if !zero_phase {
                            phase_order.insert(0, 0);
                            zero_phase = true;
                        }
                    }
                } else {
                    orphan_agents.push(agent);
                }
            }
        }
    }

    let phases = phase_order
        .into_iter()
        .map(|idx| WorkflowPhase {
            index: idx,
            title: phase_title.get(&idx).cloned().flatten(),
            detail: phase_details.get(&idx).cloned().flatten(),
            agents: by_phase.remove(&idx).unwrap_or_default(),
        })
        .collect();

    Workflow {
        run_id,
        name: s("workflowName"),
        status: s("status"),
        summary: s("summary"),
        error: s("error"),
        default_model: s("defaultModel"),
        agent_count: u("agentCount").unwrap_or(0),
        total_tokens: u("totalTokens").unwrap_or(0),
        total_tool_calls: u("totalToolCalls").unwrap_or(0),
        duration_ms: u("durationMs"),
        script_path: s("scriptPath").map(PathBuf::from),
        script: s("script"),
        resume_from: s("resumeFromRunId"),
        result: v.get("result").filter(|r| !r.is_null()).cloned(),
        logs: v
            .get("logs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        args: v.get("args").filter(|a| !a.is_null()).cloned(),
        task_id: s("taskId"),
        started_at: u("startTime").and_then(ms_to_utc).or_else(|| {
            s("timestamp")
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
        }),
        phases,
        orphan_agents,
    }
}

/// Epoch-milliseconds → UTC instant (the state files' `startTime`/`startedAt`/… shape).
fn ms_to_utc(ms: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(ms as i64)
}

/// Map phase index (1-based) → `detail` from the state's declared `phases[]` array.
fn phase_detail_map(v: &Value) -> BTreeMap<u64, Option<String>> {
    let mut m = BTreeMap::new();
    if let Some(arr) = v.get("phases").and_then(Value::as_array) {
        for (i, p) in arr.iter().enumerate() {
            m.insert(
                (i + 1) as u64,
                p.get("detail").and_then(Value::as_str).map(str::to_string),
            );
        }
    }
    m
}

// ===================== ghost launches =====================

/// One `Workflow` tool invocation as recorded in the session **transcript** — the launch-side
/// record, independent of whether a `workflows/wf_*.json` state file ever got persisted.
///
/// The distinction matters for crash forensics: a run's state file is written *by the live
/// harness*, so a power loss / hard kill can leave a launch visible in the transcript with **no
/// run record at all** (its sub-agent work, if any, sits unindexed under
/// `subagents/workflows/`). [`ghost_launches`] surfaces exactly those.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WorkflowLaunch {
    /// Transcript timestamp of the tool_use (UTC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<chrono::DateTime<chrono::Utc>>,
    /// The workflow's name: the inline script's `meta.name`, the `name` input (a saved workflow),
    /// or the `<name>` half of a `scriptPath` basename (`<name>-wf_<runId>.js`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The `wf_<runId>` embedded in a `scriptPath` relaunch's basename — identifies the run family
    /// the relaunch belongs to (its state file, when the family completed any run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_run_id: Option<String>,
    /// The `resumeFromRunId` input, when this launch resumed a prior run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<String>,
    /// The launch's paired tool_result was an error (`<tool_use_error>…`) — the run never started.
    pub errored: bool,
    /// For a GHOST: the run id recovered from its orphaned script file (`scripts/<name>-wf_….js`
    /// is written at launch and survives the crash that ate the state file). The key into the
    /// debris dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// For a GHOST with a recovered run id: how many `agent-*.jsonl` transcripts sit in its
    /// `subagents/workflows/<runId>/` debris dir — the harvestable work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debris_agents: Option<usize>,
    /// For a GHOST with a recovered run id: how many full agent results its `journal.jsonl`
    /// recorded before the crash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_result_count: Option<usize>,
}

/// Every `Workflow` tool invocation in a session's transcript, in order, with whether its
/// paired tool result was an error. Read through the one line reader and the record codec's
/// tool demand; an aborted scan is an error so it cannot certify missing workflow runs.
pub fn launches(session_path: &Path) -> anyhow::Result<Vec<WorkflowLaunch>> {
    use super::claude::record::{Demand, Head};
    use super::store::document::Fields;
    // tool_use_id -> index into `out`, so the (later) tool_result line can set `errored`.
    let mut by_use_id: BTreeMap<String, usize> = BTreeMap::new();
    let mut out: Vec<WorkflowLaunch> = Vec::new();
    let read = super::store::jsonl::for_each_record(session_path, |view| {
        let Some(head) = Head::parse(view, Demand::Tools)? else {
            return Ok(());
        };
        let Some(blocks) = head
            .top
            .get("message")
            .map(|m| Fields::parse(m, &["content"]))
            .transpose()?
            .flatten()
            .and_then(|m| m.get("content"))
            .map(|c| c.array())
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        for block in blocks {
            let Some(b) = Fields::parse(block?, &["type", "name", "id", "input", "tool_use_id", "is_error"])? else {
                continue;
            };
            match b.string("type")?.as_deref() {
                Some("tool_use") if b.string("name")?.as_deref() == Some("Workflow") => {
                    let input = b
                        .get("input")
                        .map(|i| Fields::parse(i, &["script", "scriptPath", "name", "resumeFromRunId"]))
                        .transpose()?
                        .flatten();
                    let g = |k: &str| -> anyhow::Result<Option<String>> {
                        Ok(input.as_ref().map(|i| i.string(k)).transpose()?.flatten())
                    };
                    let (path_name, path_run) = g("scriptPath")?
                        .as_deref()
                        .map(script_path_identity)
                        .unwrap_or((None, None));
                    let ts = head
                        .top
                        .string("timestamp")?
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|d| d.with_timezone(&chrono::Utc));
                    let launch = WorkflowLaunch {
                        ts,
                        name: g("script")?.as_deref().and_then(meta_name).or(g("name")?).or(path_name),
                        script_run_id: path_run,
                        resume_from: g("resumeFromRunId")?,
                        errored: false,
                        run_id: None,
                        debris_agents: None,
                        journal_result_count: None,
                    };
                    if let Some(id) = b.string("id")? {
                        by_use_id.insert(id, out.len());
                    }
                    out.push(launch);
                }
                Some("tool_result") => {
                    // Resolve and retire the matching launch with one indexed lookup.
                    if let Some(idx) = b.string("tool_use_id")?.and_then(|id| by_use_id.remove(&id)) {
                        if b.get("is_error").map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
                            out[idx].errored = true;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    });
    read?;
    Ok(out)
}

/// What a workflow lane's transcript shows it did, for `cv workflow --revive`: the FULL first
/// user prompt the orchestrator handed it, the files it wrote or edited, its distinct Bash
/// commands (first line, 70 characters) and its last substantive (over 80 bytes) assistant note.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaneWork {
    pub prompt: Option<String>,
    pub files: Vec<String>,
    pub commands: Vec<String>,
    pub last_note: Option<String>,
    /// Why the scan ended before the transcript did (a read or decode failure); what it found up
    /// to there stands. `None` for a whole read, and for no transcript at all.
    pub unread: Option<String>,
}

impl LaneWork {
    /// Mine a lane transcript. Rejected records are omitted; a read failure ends this
    /// best-effort recovery scan with what it found, and says so in [`LaneWork::unread`].
    /// When the lane transcript lacks an opening user prompt (e.g. crashed before writing its
    /// first record or launched from computed/parallel parent tool inputs), falls back to
    /// recovering the recorded `Agent`/`Task` prompt from the parent session transcript.
    pub fn mine(path: &Path) -> Self {
        let mut miner = Miner::default();
        if let Err(error) = super::store::jsonl::for_each_record(path, |view| miner.record(view)) {
            if !super::is_not_found(&error) {
                miner.work.unread = Some(format!("{error:#}"));
            }
        }
        if miner.work.prompt.is_none() {
            miner.work.prompt = recover_prompt_from_ancestors(path);
        }
        miner.work
    }

    /// Mine a lane transcript and, if its opening prompt is absent, recover the non-literal or
    /// parallel prompt recorded in `parent_transcript` for `agent_id`.
    pub fn mine_with_parent(path: &Path, parent_transcript: &Path, agent_id: Option<&str>) -> Self {
        let mut work = Self::mine(path);
        if work.prompt.is_none() {
            let inferred = agent_id.map(str::to_owned).or_else(|| agent_id_from_lane_path(path));
            work.prompt = recover_prompt_from_parent_transcript(parent_transcript, inferred.as_deref());
        }
        work
    }

    /// [`Self::mine`] over JSONL bytes.
    pub fn mine_bytes(bytes: &[u8]) -> Self {
        let mut miner = Miner::default();
        let scanned = super::store::jsonl::slice(bytes, |line| {
            if let Some(view) = line.view {
                miner.record(view)?;
            }
            Ok(crate::stream::Flow::Continue)
        });
        if let Err(error) = scanned {
            miner.work.unread = Some(format!("{error:#}"));
        }
        miner.work
    }
}

#[derive(Default)]
struct Miner {
    work: LaneWork,
    files: std::collections::BTreeSet<String>,
    commands: std::collections::BTreeSet<String>,
}

impl Miner {
    fn record(&mut self, view: crate::json::JsonView<'_>) -> anyhow::Result<()> {
        use super::claude::record::{Demand, Head};
        use super::store::document::Fields;
        let Some(head) = Head::parse(view, Demand::Tools)? else {
            return Ok(());
        };
        let Some(message) = head
            .top
            .get("message")
            .map(|m| Fields::parse(m, &["role", "content"]))
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        let role = message.string("role")?.unwrap_or_default();
        let Some(content) = message.get("content") else {
            return Ok(());
        };
        // The lane's task is the first user message: its text, or its first block's text.
        if role == "user" && self.work.prompt.is_none() {
            let full = if content.is_string()? {
                content.resolve()?.as_str().map(str::to_owned)
            } else {
                let mut first = None;
                if let Some(blocks) = content.array()? {
                    for block in blocks {
                        if let Some(Some(text)) = Fields::parse(block?, &["text"])?
                            .map(|b| b.string("text"))
                            .transpose()?
                        {
                            first = Some(text);
                            break;
                        }
                    }
                }
                first
            };
            if let Some(full) = full.filter(|f| !f.trim().is_empty()) {
                self.work.prompt = Some(full);
            }
        }
        let Some(blocks) = content.array()? else {
            return Ok(());
        };
        for block in blocks {
            let Some(b) = Fields::parse(block?, &["type", "name", "input", "text"])? else {
                continue;
            };
            match b.string("type")?.as_deref() {
                Some("tool_use") => {
                    let input = b
                        .get("input")
                        .map(|i| Fields::parse(i, &["file_path", "command"]))
                        .transpose()?
                        .flatten();
                    let arg = |k: &str| -> anyhow::Result<Option<String>> {
                        Ok(input.as_ref().map(|i| i.string(k)).transpose()?.flatten())
                    };
                    match b.string("name")?.as_deref().unwrap_or("") {
                        "Write" | "Edit" | "NotebookEdit" => {
                            if let Some(file) = arg("file_path")? {
                                if self.files.insert(file.clone()) {
                                    self.work.files.push(file);
                                }
                            }
                        }
                        "Bash" => {
                            if let Some(command) = arg("command")? {
                                let head: String = command
                                    .split('\n')
                                    .next()
                                    .unwrap_or(&command)
                                    .chars()
                                    .take(70)
                                    .collect();
                                if self.commands.insert(head.clone()) {
                                    self.work.commands.push(head);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some("text") if role == "assistant" => {
                    if let Some(text) = b.string("text")? {
                        if text.trim().len() > 80 {
                            self.work.last_note = Some(text);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Transcript launches with **no recorded run**: launched successfully per the transcript, but no
/// `workflows/wf_*.json` state file accounts for them — the signature of a crash / power loss /
/// hard kill before the harness persisted (or synced) the run's state.
///
/// Each ghost is enriched from what DID survive: its orphaned script file (written at launch)
/// recovers the run id, which keys the `subagents/workflows/<runId>/` debris dir — agent
/// transcripts and journaled results produced before the crash, i.e. the harvestable work.
///
/// A state file that exists but cannot be read is not evidence that its run was never recorded:
/// it is named once on stderr and the session reports no ghosts, never a false one.
pub fn ghost_launches(session_path: &Path) -> Vec<WorkflowLaunch> {
    // Ghost matching needs only identity, name and resume ancestry, never scripts or results.
    let read = crate::par_filter_map(state_paths(session_path), |path| {
        let state = crate::json::JsonSource::capture(&path).and_then(|source| {
            source.read(ReadInput::Document, Validation::Value, |view| {
                let mut fields = serde_json::Map::new();
                for key in ["runId", "workflowName", "resumeFromRunId"] {
                    if let Some(value) = crate::affordance::input::field_string(view, key)? {
                        fields.insert(key.into(), Value::String(value));
                    }
                }
                Ok(parse_state(&Value::Object(fields)))
            })
        });
        Some(state.map_err(|error| (path, error)))
    });
    let mut states = Vec::with_capacity(read.len());
    for state in read {
        match state {
            Ok(state) => states.push(state),
            Err((_, error)) if crate::harness::is_not_found(&error) => {}
            Err((path, error)) => {
                crate::env::warn_once("workflow run state", format!("{}: {error:#}", path.display()));
                return Vec::new();
            }
        }
    }
    let mut ghosts = compute_ghosts(
        launches(session_path).unwrap_or_else(|error| {
            crate::env::warn_once("workflow launches", format!("{}: {error:#}", session_path.display()));
            Vec::new()
        }),
        &states,
    );
    let mut orphans = OrphanScripts::new(orphan_scripts(session_path));
    for g in &mut ghosts {
        let Some(name) = g.name.as_deref() else { continue };
        // Consume one same-named orphan script per ghost (each launch persists its own script).
        let Some(rid) = orphans.take(name) else {
            continue;
        };
        if let Some(debris) = sidecar::workflow_run_dir(session_path, &rid) {
            g.debris_agents = Some(
                fs::read_dir(&debris)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_str()
                            .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".jsonl"))
                    })
                    .count(),
            );
            g.journal_result_count = Some(sidecar::journal_results(&debris).len());
        }
        g.run_id = Some(rid);
    }
    ghosts
}

/// Match the first same-named current Vec slot, preserving the prior swap_remove ordering.
/// Each removal updates only its name bucket and the entry moved from the final slot.
struct OrphanScripts {
    entries: Vec<(String, String)>,
    positions: std::collections::HashMap<String, std::collections::BTreeSet<usize>>,
}
impl OrphanScripts {
    fn new(entries: Vec<(String, String)>) -> Self {
        let mut positions: std::collections::HashMap<String, std::collections::BTreeSet<usize>> = Default::default();
        for (index, (name, _)) in entries.iter().enumerate() {
            positions.entry(name.clone()).or_default().insert(index);
        }
        Self { entries, positions }
    }
    fn take(&mut self, name: &str) -> Option<String> {
        let position = self.positions.get_mut(name)?.pop_first()?;
        let last = self.entries.len() - 1;
        if position != last {
            let moved = self
                .positions
                .get_mut(&self.entries[last].0)
                .expect("indexed orphan name");
            moved.remove(&last);
            moved.insert(position);
        }
        Some(self.entries.swap_remove(position).1)
    }
}

/// The pure matcher behind [`ghost_launches`]: each launch is accounted for by (a) having errored
/// at launch (no run to record), (b) a state file for its `scriptPath`-embedded run family, or
/// (c) consuming one same-named state file (multiset — N inline launches of one name need N
/// recorded runs). Unidentifiable launches (no name, no run id) are skipped rather than guessed at.
fn compute_ghosts(launches: Vec<WorkflowLaunch>, states: &[Workflow]) -> Vec<WorkflowLaunch> {
    use std::collections::HashMap;
    let ids: std::collections::HashSet<&str> = states.iter().map(|w| w.run_id.as_str()).collect();
    let resumed: std::collections::HashSet<&str> = states.iter().filter_map(|w| w.resume_from.as_deref()).collect();
    let mut name_budget: HashMap<&str, usize> = HashMap::new();
    for w in states {
        if let Some(n) = &w.name {
            *name_budget.entry(n.as_str()).or_default() += 1;
        }
    }
    launches
        .into_iter()
        .filter(|l| {
            if l.errored {
                return false;
            }
            if let Some(rid) = &l.script_run_id {
                if ids.contains(rid.as_str()) {
                    return false;
                }
            }
            if let Some(rf) = &l.resume_from {
                // A resume's new run may keep the resumed family's id in `resume_from`.
                if resumed.contains(rf.as_str()) {
                    return false;
                }
            }
            match &l.name {
                Some(n) => match name_budget.get_mut(n.as_str()) {
                    Some(b) if *b > 0 => {
                        *b -= 1;
                        false
                    }
                    _ => true,
                },
                None => false, // unidentifiable — don't report a guess
            }
        })
        .collect()
}

/// Extract `meta.name` from an inline workflow script by locating the `export const meta = { … }`
/// (or `const meta = { … }`) object literal and scanning only its top-level (`depth == 1`)
/// properties. Identifiers containing `name` (`rename`), sibling objects before or after `meta`,
/// and nested `{ name: … }` objects inside `meta.phases` cannot shadow or spoof `meta.name`.
fn meta_name(script: &str) -> Option<String> {
    let block = find_meta_object(script)?;
    extract_top_level_string_prop(block, "name")
}

fn is_js_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Advance past a JS comment (`//…` or `/*…*/`) or string/template literal starting at `i`.
/// Returns `Some(next_index)` if `bytes[i..]` starts one, else `None`.
fn skip_js_trivia_or_literal(script: &str, i: usize) -> Option<usize> {
    let bytes = script.as_bytes();
    let b = *bytes.get(i)?;
    if b == b'/' {
        match bytes.get(i + 1) {
            Some(b'/') => {
                let end = bytes[i + 2..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(bytes.len(), |p| i + 2 + p + 1);
                return Some(end);
            }
            Some(b'*') => {
                let mut j = i + 2;
                while j + 1 < bytes.len() {
                    if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                        return Some(j + 2);
                    }
                    j += 1;
                }
                return Some(bytes.len());
            }
            _ => return None,
        }
    }
    if matches!(b, b'\'' | b'"' | b'`') {
        let quote = b;
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == b'\\' {
                j = (j + 2).min(bytes.len());
                continue;
            }
            if bytes[j] == quote {
                return Some(j + 1);
            }
            j += 1;
        }
        return Some(bytes.len());
    }
    None
}

fn skip_js_ws_and_comments(script: &str, mut i: usize) -> usize {
    let bytes = script.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if bytes[i] == b'/' && matches!(bytes.get(i + 1), Some(b'/' | b'*')) {
            i = skip_js_trivia_or_literal(script, i).unwrap_or(bytes.len());
        } else {
            break;
        }
    }
    i
}

/// Locate `[export] (const|let|var) meta = { … }` and return the balanced `{ … }` slice.
fn find_meta_object(script: &str) -> Option<&str> {
    let bytes = script.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_js_trivia_or_literal(script, i) {
            i = next;
            continue;
        }
        if is_js_ident_byte(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_js_ident_byte(bytes[i]) {
                i += 1;
            }
            let word = &script[start..i];
            if matches!(word, "const" | "let" | "var") {
                let j = skip_js_ws_and_comments(script, i);
                if script[j..].starts_with("meta") && !bytes.get(j + 4).is_some_and(|&c| is_js_ident_byte(c)) {
                    let k = skip_js_ws_and_comments(script, j + 4);
                    if bytes.get(k) == Some(&b'=') && bytes.get(k + 1) != Some(&b'=') {
                        let brace = skip_js_ws_and_comments(script, k + 1);
                        if bytes.get(brace) == Some(&b'{') {
                            return balanced_braces(script, brace);
                        }
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    None
}

fn balanced_braces(script: &str, open: usize) -> Option<&str> {
    let bytes = script.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        if let Some(next) = skip_js_trivia_or_literal(script, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&script[open..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_js_string_literal(s: &str) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let mut out = String::new();
    let mut chars = s[1..].char_indices();
    while let Some((off, ch)) = chars.next() {
        if ch == '\\' {
            let (_, esc) = chars.next()?;
            match esc {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => out.push(other),
            }
        } else if quote == b'`' && ch == '$' && s[1 + off + 1..].starts_with('{') {
            // Computed template interpolation is not a static literal name.
            return None;
        } else if ch as u8 == quote && ch.is_ascii() {
            return Some((out, 1 + off + 1));
        } else {
            out.push(ch);
        }
    }
    None
}

fn extract_top_level_string_prop(obj: &str, target_key: &str) -> Option<String> {
    let bytes = obj.as_bytes();
    let mut depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut at_prop_start = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && matches!(bytes.get(i + 1), Some(b'/' | b'*')) {
            i = skip_js_trivia_or_literal(obj, i).unwrap_or(bytes.len());
            continue;
        }
        if depth == 1 && bracket_depth == 0 && paren_depth == 0 && at_prop_start {
            let key_match = if matches!(bytes[i], b'\'' | b'"') {
                parse_js_string_literal(&obj[i..]).map(|(k, consumed)| (k == target_key, i + consumed))
            } else if is_js_ident_byte(bytes[i]) {
                let start = i;
                let mut end = i;
                while end < bytes.len() && is_js_ident_byte(bytes[end]) {
                    end += 1;
                }
                Some((&obj[start..end] == target_key, end))
            } else {
                None
            };
            if let Some((matched, after_key)) = key_match {
                let colon = skip_js_ws_and_comments(obj, after_key);
                if bytes.get(colon) == Some(&b':') {
                    let val_pos = skip_js_ws_and_comments(obj, colon + 1);
                    if matched {
                        return parse_js_string_literal(&obj[val_pos..]).map(|(val, _)| val);
                    }
                    at_prop_start = false;
                    i = val_pos;
                    continue;
                }
            }
            at_prop_start = false;
        }
        if let Some(next) = skip_js_trivia_or_literal(obj, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'{' => {
                depth += 1;
                if depth == 1 {
                    at_prop_start = true;
                }
            }
            b'}' => {
                depth = depth.saturating_sub(1);
            }
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b',' if depth == 1 && bracket_depth == 0 && paren_depth == 0 => {
                at_prop_start = true;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn agent_id_from_lane_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(stem.strip_prefix("agent-").unwrap_or(stem).to_owned())
}

fn recover_prompt_from_ancestors(path: &Path) -> Option<String> {
    let agent_id = agent_id_from_lane_path(path);
    for ancestor in path.ancestors() {
        if ancestor.file_name().and_then(|n| n.to_str()) == Some("subagents") {
            let session_dir = ancestor.parent()?;
            let parent_transcript = session_dir.with_extension("jsonl");
            if let Some(prompt) = recover_prompt_from_parent_transcript(&parent_transcript, agent_id.as_deref()) {
                return Some(prompt);
            }
            // If this lane is under `subagents/workflows/<runId>/agent-<id>.jsonl`, also check
            // `<sessionId>/workflows/<runId>.json` for a recorded promptPreview fallback.
            if let Some(aid) = agent_id.as_deref() {
                if let Some(run_dir) = path.parent() {
                    if run_dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some("workflows") {
                        if let Some(run_id) = run_dir.file_name().and_then(|n| n.to_str()) {
                            let state_path = session_dir.join("workflows").join(format!("{run_id}.json"));
                            if let Some(wf) = parse_state_file(&state_path) {
                                for agent in wf.phases.iter().flat_map(|p| &p.agents).chain(&wf.orphan_agents) {
                                    let bare = agent.agent_id.as_deref().map(|s| s.strip_prefix("agent-").unwrap_or(s));
                                    if bare == Some(aid) {
                                        if let Some(preview) =
                                            agent.prompt_preview.as_ref().filter(|s| !s.trim().is_empty())
                                        {
                                            return Some(preview.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            break;
        }
    }
    None
}

/// Scan a parent Claude transcript for `Agent`/`Task` `tool_use` blocks and pair them with their
/// `tool_result` / `toolUseResult` `agentId` outcomes to recover computed or parallel sub-agent prompts.
fn recover_prompt_from_parent_transcript(parent_path: &Path, target_agent_id: Option<&str>) -> Option<String> {
    use super::claude::record::{Demand, Head};
    use super::store::document::Fields;

    let mut calls: Vec<(String, String)> = Vec::new();
    let mut by_agent_id: BTreeMap<String, String> = BTreeMap::new();
    let target_norm = target_agent_id.map(|id| id.strip_prefix("agent-").unwrap_or(id));

    let _ = super::store::jsonl::for_each_record(parent_path, |view| {
        let Some(head) = Head::parse(view, Demand::Tools)? else {
            return Ok(());
        };
        let sidecar_agent_id = view
            .get("toolUseResult")?
            .map(|tur| -> anyhow::Result<Option<String>> {
                if tur.is_string()? {
                    Ok(tur.resolve()?.as_str().and_then(extract_agent_id_from_text))
                } else if let Some(f) = Fields::parse(tur, &["agentId"])? {
                    Ok(f.string("agentId")?)
                } else {
                    Ok(None)
                }
            })
            .transpose()?
            .flatten();

        let Some(blocks) = head
            .top
            .get("message")
            .map(|m| Fields::parse(m, &["content"]))
            .transpose()?
            .flatten()
            .and_then(|m| m.get("content"))
            .map(|c| c.array())
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        for block in blocks {
            let Some(b) = Fields::parse(block?, &["type", "id", "name", "input", "tool_use_id", "content"])? else {
                continue;
            };
            match b.string("type")?.as_deref() {
                Some("tool_use") => {
                    let name = b.string("name")?.unwrap_or_default();
                    if matches!(name.as_str(), "Agent" | "Task" | "dispatch_agent") {
                        let Some(id) = b.string("id")? else { continue };
                        let prompt = b
                            .get("input")
                            .map(|i| Fields::parse(i, &["prompt"]))
                            .transpose()?
                            .flatten()
                            .map(|i| i.string("prompt"))
                            .transpose()?
                            .flatten()
                            .filter(|p| !p.trim().is_empty());
                        if let Some(prompt) = prompt {
                            calls.push((id, prompt));
                        }
                    }
                }
                Some("tool_result") => {
                    let Some(tuid) = b.string("tool_use_id")? else {
                        continue;
                    };
                    let Some((_, prompt)) = calls.iter().find(|(id, _)| id == &tuid) else {
                        continue;
                    };
                    let mut aid = sidecar_agent_id.clone();
                    if aid.is_none() {
                        if let Some(content) = b.get("content") {
                            let text = if content.is_string()? {
                                content.resolve()?.as_str().map(str::to_owned)
                            } else if let Some(parts) = content.array()? {
                                let mut joined = String::new();
                                for part in parts {
                                    if let Some(Some(t)) =
                                        Fields::parse(part?, &["text"])?.map(|p| p.string("text")).transpose()?
                                    {
                                        joined.push_str(&t);
                                        joined.push('\n');
                                    }
                                }
                                Some(joined)
                            } else {
                                None
                            };
                            aid = text.as_deref().and_then(extract_agent_id_from_text);
                        }
                    }
                    if let Some(aid) = aid {
                        let bare = aid.strip_prefix("agent-").unwrap_or(&aid).to_owned();
                        by_agent_id.insert(bare, prompt.clone());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    });

    if let Some(target) = target_norm {
        if let Some(prompt) = by_agent_id.remove(target) {
            return Some(prompt);
        }
    }
    // Single-spawn fallback when the parent crashed before recording the tool_result's agentId.
    if calls.len() == 1 && by_agent_id.is_empty() {
        return calls.pop().map(|(_, prompt)| prompt);
    }
    None
}

fn extract_agent_id_from_text(text: &str) -> Option<String> {
    for marker in ["\"agentId\":\"", "\"agentId\": \"", "agentId: ", "agent_id: "] {
        if let Some(idx) = text.find(marker) {
            let rest = &text[idx + marker.len()..];
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Split a `scriptPath` basename `<name>-wf_<runId>.js` into `(name, wf_<runId>)`. Without the
/// `-wf_` separator, the whole stem is the name and the run id is `None`.
fn script_path_identity(p: &str) -> (Option<String>, Option<String>) {
    let stem = Path::new(p).file_stem().and_then(|s| s.to_str()).unwrap_or(p);
    match stem.rsplit_once("-wf_") {
        Some((name, rid)) => (Some(name.to_string()), Some(format!("wf_{rid}"))),
        None => (Some(stem.to_string()), None),
    }
}

/// Parse one `workflow_agent` progress entry into a [`WorkflowAgent`].
fn parse_agent(e: &Value) -> WorkflowAgent {
    let s = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
    let u = |k: &str| e.get(k).and_then(Value::as_u64);
    WorkflowAgent {
        index: u("index").unwrap_or(0),
        label: s("label"),
        agent_id: s("agentId"),
        phase_index: u("phaseIndex").unwrap_or(0),
        state: s("state"),
        model: s("model"),
        tokens: u("tokens"),
        tool_calls: u("toolCalls"),
        duration_ms: u("durationMs"),
        error: s("error"),
        cached: e.get("cached").and_then(Value::as_bool).unwrap_or(false),
        prompt_preview: s("promptPreview"),
        result_preview: s("resultPreview"),
        attempt: u("attempt"),
        started_at: u("startedAt").and_then(ms_to_utc),
        last_progress_at: u("lastProgressAt").and_then(ms_to_utc),
        last_tool_name: s("lastToolName"),
        last_tool_summary: s("lastToolSummary"),
        journal_result: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_state() -> Value {
        json!({
            "runId": "wf_abc-123",
            "workflowName": "titanium-critical-path",
            "status": "completed",
            "summary": "did the thing",
            "defaultModel": "claude-opus-4-8[1m]",
            "agentCount": 3,
            "totalTokens": 589698,
            "totalToolCalls": 198,
            "durationMs": 3144884,
            "scriptPath": "/x/workflows/scripts/titanium-wf_abc-123.js",
            "script": "export const meta = { name: 'titanium-critical-path' }",
            "phases": [
                {"title": "Coherence-0.1", "detail": "migrate off stark.rs"},
                {"title": "Verify-0.1", "detail": "adversarial check"}
            ],
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "Coherence-0.1"},
                {"type": "workflow_phase", "index": 2, "title": "Verify-0.1"},
                {"type": "workflow_agent", "index": 1, "label": "migrate", "phaseIndex": 1,
                 "agentId": "ac7eeb16dca41d194", "model": "claude-opus-4-8[1m]", "state": "done",
                 "tokens": 277133, "toolCalls": 95, "durationMs": 2142913,
                 "promptPreview": "You are doing the single most security-critical fix",
                 "resultPreview": "{\"done\":true,\"commit\":\"26bb171e\"}"},
                {"type": "workflow_agent", "index": 2, "label": "verify", "phaseIndex": 2,
                 "agentId": "a29d1de560fb4a1b7", "model": "claude-opus-4-8[1m]", "state": "error",
                 "tokens": 90642, "toolCalls": 39, "error": "broke anti-ghost",
                 "resultPreview": "{\"verdict\":\"PARTIAL\"}"}
            ]
        })
    }

    #[test]
    fn parses_phase_tree_with_agents_under_phases() {
        let w = parse_state(&sample_state());
        assert_eq!(w.run_id, "wf_abc-123");
        assert_eq!(w.name.as_deref(), Some("titanium-critical-path"));
        assert_eq!(w.status.as_deref(), Some("completed"));
        assert_eq!(w.agent_count, 3);
        assert_eq!(w.total_tokens, 589698);
        assert_eq!(w.total_tool_calls, 198);
        assert!(w.script.as_deref().unwrap().contains("titanium-critical-path"));
        assert!(w.script_path.is_some());

        // Two phases, each carrying its detail and its agent.
        assert_eq!(w.phases.len(), 2);
        assert_eq!(w.phases[0].index, 1);
        assert_eq!(w.phases[0].title.as_deref(), Some("Coherence-0.1"));
        assert_eq!(w.phases[0].detail.as_deref(), Some("migrate off stark.rs"));
        assert_eq!(w.phases[0].agents.len(), 1);
        let a = &w.phases[0].agents[0];
        assert_eq!(a.agent_id.as_deref(), Some("ac7eeb16dca41d194"));
        assert_eq!(a.state.as_deref(), Some("done"));
        assert_eq!(a.tokens, Some(277133));
        assert!(a.result_preview.as_deref().unwrap().contains("26bb171e"));

        // The error agent under phase 2.
        let e = &w.phases[1].agents[0];
        assert_eq!(e.state.as_deref(), Some("error"));
        assert_eq!(e.error.as_deref(), Some("broke anti-ghost"));

        // State histogram.
        let sc = w.state_counts();
        assert_eq!(sc.get("done"), Some(&1));
        assert_eq!(sc.get("error"), Some(&1));
        assert_eq!(w.agents_in_phases(), 2);
    }

    #[test]
    fn falls_back_to_phases_array_without_progress_headers() {
        // A state whose workflowProgress has agents but no workflow_phase headers: phase titles
        // come from `phases[]`, agents slot by phaseIndex.
        let v = json!({
            "runId": "wf_x",
            "phases": [{"title": "P1", "detail": "d1"}, {"title": "P2", "detail": "d2"}],
            "workflowProgress": [
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1", "state": "done"},
                {"type": "workflow_agent", "index": 2, "phaseIndex": 2, "agentId": "a2", "state": "done"}
            ]
        });
        let w = parse_state(&v);
        assert_eq!(w.phases.len(), 2);
        assert_eq!(w.phases[0].title.as_deref(), Some("P1"));
        assert_eq!(w.phases[0].agents[0].agent_id.as_deref(), Some("a1"));
        assert_eq!(w.phases[1].agents[0].agent_id.as_deref(), Some("a2"));
    }

    #[test]
    fn parses_result_logs_args_task_and_timing() {
        let v = json!({
            "runId": "wf_rich-1",
            "workflowName": "harvest-me",
            "status": "completed",
            "taskId": "w9mkonv48",
            "startTime": 1783423014394u64,
            "args": {"seeds_file": "/tmp/seeds.json"},
            "logs": ["20 families queued", "[refine:temporal] failed: session limit"],
            "result": {"confirmed": [1, 2], "notes": "ok"},
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "P"},
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1",
                 "state": "error", "attempt": 2, "startedAt": 1783389189922u64,
                 "lastProgressAt": 1783389200000u64, "lastToolName": "Bash",
                 "lastToolSummary": "cd /x && grep -n def"}
            ]
        });
        let w = parse_state(&v);
        assert_eq!(w.task_id.as_deref(), Some("w9mkonv48"));
        assert_eq!(w.started_at.unwrap().timestamp_millis(), 1_783_423_014_394);
        assert_eq!(w.args.as_ref().unwrap()["seeds_file"], "/tmp/seeds.json");
        assert_eq!(w.logs.len(), 2);
        assert!(w.logs[1].contains("session limit"));
        assert_eq!(w.result.as_ref().unwrap()["confirmed"][1], 2);
        let a = &w.phases[0].agents[0];
        assert_eq!(a.attempt, Some(2));
        assert_eq!(a.started_at.unwrap().timestamp_millis(), 1_783_389_189_922);
        assert_eq!(a.last_progress_at.unwrap().timestamp_millis(), 1_783_389_200_000);
        assert_eq!(a.last_tool_name.as_deref(), Some("Bash"));
        assert!(a.last_tool_summary.as_deref().unwrap().starts_with("cd /x"));
        // Null result stays absent, not Some(Null).
        let w2 = parse_state(&json!({"runId": "wf_x", "result": null}));
        assert!(w2.result.is_none());
    }

    #[test]
    fn run_names_orphans_and_ghost_debris() {
        let dir = std::env::temp_dir().join(format!("cv-wf-index-test-{}", std::process::id()));
        let wfdir = dir.join("sess").join("workflows");
        std::fs::create_dir_all(wfdir.join("scripts")).unwrap();
        let sess = dir.join("sess.jsonl");
        // A transcript with two successful launches: one recorded, one ghosted.
        let lines = [
            json!({"type":"assistant","timestamp":"2026-07-08T09:00:00Z","message":{"content":[
                {"type":"tool_use","id":"t1","name":"Workflow","input":{"script":"export const meta = { name: 'alive-swarm' }"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Workflow launched in background. Task ID: w1"}]}}).to_string(),
            json!({"type":"assistant","timestamp":"2026-07-08T09:02:00Z","message":{"content":[
                {"type":"tool_use","id":"t2","name":"Workflow","input":{"script":"export const meta = { name: 'doomed-swarm' }"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"Workflow launched in background. Task ID: w2"}]}}).to_string(),
        ];
        std::fs::write(&sess, lines.join("\n")).unwrap();
        // Recorded run: state file WITHOUT a script file (the byte-scan fallback path).
        std::fs::write(
            wfdir.join("wf_alive-1.json"),
            json!({"runId": "wf_alive-1", "workflowName": "alive-swarm"}).to_string(),
        )
        .unwrap();
        // Ghost: script file only (written at launch; the crash ate the state file) + debris.
        std::fs::write(
            wfdir.join("scripts").join("doomed-swarm-wf_dead-2.js"),
            "export const meta = {}",
        )
        .unwrap();
        let debris = dir.join("sess").join("subagents").join("workflows").join("wf_dead-2");
        std::fs::create_dir_all(&debris).unwrap();
        std::fs::write(debris.join("agent-a1.jsonl"), "").unwrap();
        std::fs::write(debris.join("agent-a2.jsonl"), "").unwrap();
        std::fs::write(
            debris.join("journal.jsonl"),
            json!({"type":"result","key":"v2:k","agentId":"a1","result":{"ok":true}}).to_string(),
        )
        .unwrap();

        // The cheap index sees both halves without parsing the recorded state fully.
        let mut names = run_names(&sess);
        names.sort();
        assert_eq!(names, vec![("alive-swarm".into(), "wf_alive-1".into())]);
        assert_eq!(orphan_scripts(&sess), vec![("doomed-swarm".into(), "wf_dead-2".into())]);
        assert!(has_workflow_records(&sess));
        assert_eq!(workflows_named(&sess, "alive").len(), 1);
        assert!(workflows_named(&sess, "nope").is_empty());

        // The ghost carries its recovered run id + debris counts.
        let ghosts = ghost_launches(&sess);
        assert_eq!(ghosts.len(), 1);
        let g = &ghosts[0];
        assert_eq!(g.name.as_deref(), Some("doomed-swarm"));
        assert_eq!(g.run_id.as_deref(), Some("wf_dead-2"));
        assert_eq!(g.debris_agents, Some(2));
        assert_eq!(g.journal_result_count, Some(1));

        // A state file that cannot be read might be the doomed run's: no ghost is claimed.
        std::fs::write(wfdir.join("wf_unreadable-3.json"), "{\"runId\": \"wf_unread").unwrap();
        assert!(ghost_launches(&sess).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_names_selects_decoded_top_level_fields_and_only_script_files() {
        let world = crate::test_world("workflow-name-projection");
        let parent = world.base.join("sess.jsonl");
        let dir = world.base.join("sess/workflows");
        fs::create_dir_all(dir.join("scripts")).unwrap();
        let source = serde_json::json!({
            "args": {"workflowName":"nested-decoy", "padding":"x".repeat(1100000)},
            "workflowName":"a\"b λ", "runId":"wf_formatted",
        });
        fs::write(
            dir.join("wf_formatted.json"),
            serde_json::to_string_pretty(&source).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("wf_duplicate.json"),
            r#"{"workflowName":"old","workflowName":"last"}"#,
        )
        .unwrap();
        fs::write(
            dir.join("scripts/wrong-wf_formatted.txt"),
            "not a persisted workflow script",
        )
        .unwrap();
        let mut names = run_names(&parent);
        names.sort();
        assert_eq!(
            names,
            vec![
                ("a\"b λ".into(), "wf_formatted".into()),
                ("last".into(), "wf_duplicate".into())
            ]
        );
        assert!(orphan_scripts(&parent).is_empty());
        // Persisted JS filenames remain the name authority for a matching recorded run.
        fs::write(dir.join("scripts/filename-wf_formatted.js"), "export const meta = {}").unwrap();
        assert!(run_names(&parent).contains(&("filename".into(), "wf_formatted".into())));
    }

    #[test]
    fn partial_journal_recovers_only_missing_agent_returns() {
        let world = crate::test_world("workflow-partial-journal");
        let parent = world.base.join("sess.jsonl");
        let run = world.base.join("sess/subagents/workflows/wf_partial");
        fs::create_dir_all(&run).unwrap();
        let transcript = |text: &str| {
            serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text","text":text}]}}).to_string()
        };
        fs::write(run.join("agent-a1.jsonl"), transcript("stale transcript result")).unwrap();
        fs::write(run.join("agent-a2.jsonl"), transcript("completed sibling work")).unwrap();
        fs::write(
            run.join("journal.jsonl"),
            serde_json::json!({"type":"result","agentId":"a1","result":{"status":"done","summary":"authoritative"}})
                .to_string(),
        )
        .unwrap();
        let mut workflow = parse_state(&serde_json::json!({"runId":"wf_partial", "workflowProgress":[
            {"type":"workflow_agent","index":1,"agentId":"a1"},
            {"type":"workflow_agent","index":2,"agentId":"agent-a2"},
            {"type":"workflow_agent","index":3,"agentId":"missing"}
        ]}));
        workflow.attach_journal(&parent);
        assert_eq!(
            workflow.orphan_agents[0].journal_result.as_ref().unwrap()["summary"],
            "authoritative"
        );
        assert_eq!(
            workflow.orphan_agents[1].journal_result,
            Some(Value::String("completed sibling work".into()))
        );
        assert!(workflow.orphan_agents[2].journal_result.is_none());
    }

    #[test]
    fn attach_journal_fills_full_results_last_write_wins() {
        let dir = std::env::temp_dir().join(format!("cv-wf-journal-test-{}", std::process::id()));
        let run_dir = dir.join("sess").join("subagents").join("workflows").join("wf_j-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let sess = dir.join("sess.jsonl");
        std::fs::write(&sess, "").unwrap();
        let journal = [
            json!({"type":"started","key":"v2:k1","agentId":"a1"}).to_string(),
            json!({"type":"result","key":"v2:k1","agentId":"a1","result":{"status":"RED","try":1}}).to_string(),
            // A re-issued agent journals again — the LAST result must win.
            json!({"type":"result","key":"v2:k1","agentId":"a1","result":{"status":"GREEN","try":2,"detail":"x".repeat(600)}})
                .to_string(),
        ];
        std::fs::write(run_dir.join("journal.jsonl"), journal.join("\n")).unwrap();

        let mut w = parse_state(&json!({
            "runId": "wf_j-1",
            "phases": [{"title": "P"}],
            "workflowProgress": [
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1",
                 "state": "done", "resultPreview": "{\"status\":\"GREEN\"…"},
                {"type": "workflow_agent", "index": 2, "phaseIndex": 1, "agentId": "a2", "state": "done"}
            ]
        }));
        w.attach_journal(&sess);
        std::fs::remove_dir_all(&dir).ok();

        let a1 = &w.phases[0].agents[0];
        let jr = a1.journal_result.as_ref().unwrap();
        assert_eq!(jr["status"], "GREEN");
        assert_eq!(jr["try"], 2);
        assert_eq!(jr["detail"].as_str().unwrap().len(), 600); // FULL, beyond any preview cap
        assert!(w.phases[0].agents[1].journal_result.is_none()); // no journal entry → stays absent
    }

    #[test]
    fn attach_journal_falls_back_to_agent_transcripts_when_journal_absent() {
        // A crashed run: the debris dir has agent transcripts but journal.jsonl was never written.
        let dir = std::env::temp_dir().join(format!("cv-wf-nojournal-test-{}", std::process::id()));
        let run_dir = dir.join("sess").join("subagents").join("workflows").join("wf_c-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let sess = dir.join("sess.jsonl");
        std::fs::write(&sess, "").unwrap();
        let transcript = [
            json!({"type":"user","message":{"role":"user","content":"do the thing"}}).to_string(),
            json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"text","text":"intermediate progress note"}]}})
            .to_string(),
            json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"text","text":"FINAL: proved 3 lemmas, all green"}]}})
            .to_string(),
        ];
        std::fs::write(run_dir.join("agent-a1.jsonl"), transcript.join("\n")).unwrap();
        // a2 has no transcript at all — it must stay None, never a fabricated return.

        let mut w = parse_state(&json!({
            "runId": "wf_c-1",
            "phases": [{"title": "P"}],
            "workflowProgress": [
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1", "state": "done"},
                {"type": "workflow_agent", "index": 2, "phaseIndex": 1, "agentId": "a2", "state": "progress"}
            ]
        }));
        w.attach_journal(&sess);
        std::fs::remove_dir_all(&dir).ok();

        let a1 = &w.phases[0].agents[0];
        assert_eq!(
            a1.journal_result.as_ref().unwrap().as_str().unwrap(),
            "FINAL: proved 3 lemmas, all green",
            "the LAST assistant text is the agent's return"
        );
        assert!(w.phases[0].agents[1].journal_result.is_none());
    }

    #[test]
    fn workflow_resolves_by_run_id_and_by_name() {
        let dir = std::env::temp_dir().join(format!("cv-wf-byname-test-{}", std::process::id()));
        let wfdir = dir.join("sess").join("workflows");
        std::fs::create_dir_all(&wfdir).unwrap();
        let sess = dir.join("sess.jsonl");
        std::fs::write(&sess, "").unwrap();
        let mk = |rid: &str, name: &str| {
            std::fs::write(
                wfdir.join(format!("{rid}.json")),
                json!({"runId": rid, "workflowName": name}).to_string(),
            )
            .unwrap()
        };
        mk("wf_aaa-111", "stark-kill-emit-swarm");
        mk("wf_bbb-222", "stark-kill-assurance-audit");
        mk("wf_ccc-333", "stark-kill-assurance-audit"); // a re-run of the same name

        // By run id (prefix, with/without wf_).
        assert_eq!(workflow(&sess, "wf_aaa-111").unwrap().run_id, "wf_aaa-111");
        assert_eq!(workflow(&sess, "aaa").unwrap().run_id, "wf_aaa-111");
        // By exact name; a re-run name resolves (to one of its runs) instead of failing.
        assert_eq!(workflow(&sess, "stark-kill-emit-swarm").unwrap().run_id, "wf_aaa-111");
        let audit = workflow(&sess, "stark-kill-assurance-audit").unwrap();
        assert_eq!(audit.name.as_deref(), Some("stark-kill-assurance-audit"));
        // Unique name-prefix resolves; ambiguous prefix (two distinct names) does not.
        assert_eq!(workflow(&sess, "stark-kill-emit").unwrap().run_id, "wf_aaa-111");
        assert!(workflow(&sess, "stark-kill").is_none());
        // Identity-only consumers preserve the rich reader's precedence and ambiguity, while
        // neither projecting selectors nor resolving a missing selector materializes any state.
        STATE_MATERIALIZATIONS.lock().unwrap().0 = Some(dir.clone());
        let before = STATE_MATERIALIZATIONS.lock().unwrap().1.len();
        for key in [
            "wf_aaa-111",
            "aaa",
            "stark-kill-emit-swarm",
            "stark-kill-assurance-audit",
            "stark-kill-emit",
            "stark-kill",
            "absent",
        ] {
            let expected = match key {
                "wf_aaa-111" | "aaa" | "stark-kill-emit-swarm" | "stark-kill-emit" => Some("wf_aaa-111".to_owned()),
                "stark-kill-assurance-audit" => Some(audit.run_id.clone()),
                _ => None,
            };
            assert_eq!(workflow_id(&sess, key), expected);
        }
        assert!(STATE_MATERIALIZATIONS.lock().unwrap().1[before..]
            .iter()
            .all(|path| !path.starts_with(&dir)));
        let before = STATE_MATERIALIZATIONS.lock().unwrap().1.len();
        let selected = workflow(&sess, "aaa").unwrap();
        assert_eq!(selected.run_id, "wf_aaa-111");
        let reads: Vec<_> = STATE_MATERIALIZATIONS.lock().unwrap().1[before..]
            .iter()
            .filter(|path| path.starts_with(&dir))
            .cloned()
            .collect();
        assert_eq!(reads, [wfdir.join("wf_aaa-111.json")]);
        *STATE_MATERIALIZATIONS.lock().unwrap() = (None, Vec::new());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workflow_receipts_resolve_large_outstanding_fanout_in_original_launch_order() {
        let world = crate::test_world("workflow-pending-fanout");
        let path = world.base.join("session.jsonl");
        let count = 1024;
        let mut rows = String::new();
        for n in 0..count {
            rows.push_str(&format!("{}\n", json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":format!("pending-{n}"),"name":"Workflow","input":{"name":format!("workflow-{n}")}}]}})));
        }
        rows.push_str("malformed-json\n");
        rows.push_str(&format!("{}\n", json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"unrelated","content":"pending-0 ".repeat(100000)}]}})));
        for n in (0..count).rev() {
            rows.push_str(&format!("{}\n", json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":format!("pending-{n}"),"is_error":n % 3 == 0,"content":"receipt"}]}})));
        }
        fs::write(&path, rows).unwrap();
        let launches = launches(&path).unwrap();
        assert_eq!(launches.len(), count);
        for (n, launch) in launches.iter().enumerate() {
            assert_eq!(launch.name, Some(format!("workflow-{n}")));
            assert_eq!(launch.errored, n % 3 == 0);
        }
    }

    #[test]
    fn orphan_index_preserves_swap_removal_order() {
        let entries: Vec<_> = ["a", "b", "a", "c", "a", "b"]
            .iter()
            .enumerate()
            .map(|(n, name)| (name.to_string(), n.to_string()))
            .collect();
        let mut expected = entries.clone();
        let mut actual = OrphanScripts::new(entries);
        for name in ["a", "b", "a", "c", "a", "b", "missing"] {
            let old = expected
                .iter()
                .position(|(n, _)| n == name)
                .map(|position| expected.swap_remove(position).1);
            assert_eq!(actual.take(name), old);
        }
        for count in [64, 1024, 4096] {
            let entries: Vec<_> = (0..count).map(|n| (format!("name-{n}"), format!("run-{n}"))).collect();
            let mut actual = OrphanScripts::new(entries.clone());
            let mut old = entries;
            for n in (0..count).rev() {
                let name = format!("name-{n}");
                let position = old.iter().position(|(candidate, _)| candidate == &name).unwrap();
                assert_eq!(actual.take(&name), Some(old.swap_remove(position).1));
            }
        }
    }

    #[test]
    fn ghost_resume_index_and_phase_zero_grouping_preserve_large_fanout() {
        let count = 4096;
        let mut states: Vec<_> = (0..count).map(|n| state(&format!("wf_{n}"), "same-name")).collect();
        for (n, state) in states.iter_mut().enumerate() {
            state.resume_from = Some(format!("resume-{n}"));
        }
        let launches: Vec<_> = (0..count)
            .map(|n| {
                let mut item = launch(Some("same-name"), None, false);
                item.resume_from = Some(format!("resume-{n}"));
                item
            })
            .collect();
        assert!(compute_ghosts(launches, &states).is_empty());
        let mut progress: Vec<_> = (1..=count)
            .map(|n| json!({"type":"workflow_phase","index":n,"title":format!("phase-{n}")}))
            .collect();
        progress.extend((1..=count).map(|n| json!({"type":"workflow_agent","index":n,"phaseIndex":0})));
        let workflow = parse_state(&json!({"workflowProgress":progress}));
        assert_eq!(workflow.phases.len(), count + 1);
        assert_eq!(workflow.phases[0].index, 0);
        assert_eq!(workflow.phases[0].agents.len(), count);
        assert!(workflow.orphan_agents.is_empty());
    }

    fn launch(name: Option<&str>, script_run_id: Option<&str>, errored: bool) -> WorkflowLaunch {
        WorkflowLaunch {
            ts: None,
            name: name.map(str::to_string),
            script_run_id: script_run_id.map(str::to_string),
            resume_from: None,
            errored,
            run_id: None,
            debris_agents: None,
            journal_result_count: None,
        }
    }

    fn state(run_id: &str, name: &str) -> Workflow {
        parse_state(&json!({"runId": run_id, "workflowName": name}))
    }

    #[test]
    fn ghosts_are_launches_without_a_recorded_run() {
        // The real 2026-07-08 power-loss shape: census/audit runs recorded, the two final
        // launches' state files never persisted.
        let states = vec![state("wf_aaa-111", "census"), state("wf_bbb-222", "audit")];
        let launches = vec![
            launch(Some("census"), None, false),
            launch(Some("audit"), None, false),
            launch(Some("final-flips"), None, false),  // ghost
            launch(Some("bugfix-swarm"), None, false), // ghost
        ];
        let ghosts = compute_ghosts(launches, &states);
        assert_eq!(
            ghosts.iter().filter_map(|g| g.name.as_deref()).collect::<Vec<_>>(),
            vec!["final-flips", "bugfix-swarm"]
        );
    }

    #[test]
    fn errored_and_resume_launches_are_not_ghosts() {
        let states = vec![state("wf_aaa-111", "swarm")];
        let launches = vec![
            launch(Some("swarm"), None, false),               // consumes the state
            launch(Some("swarm"), Some("wf_aaa-111"), false), // scriptPath resume of a recorded family
            launch(Some("swarm"), Some("wf_aaa-111"), true),  // errored at launch — never ran
        ];
        assert!(compute_ghosts(launches, &states).is_empty());
        // But a second *inline* launch of the same name with only one recorded run IS a ghost.
        let launches = vec![launch(Some("swarm"), None, false), launch(Some("swarm"), None, false)];
        assert_eq!(compute_ghosts(launches, &states).len(), 1);
    }

    #[test]
    fn meta_name_and_script_path_identity() {
        assert_eq!(
            meta_name("export const meta = {\n  name: 'stark-kill-bugfix-swarm',\n  description: 'x'}"),
            Some("stark-kill-bugfix-swarm".into())
        );
        assert_eq!(
            meta_name(r#"export const meta = { name: "dq", phases: [] }"#),
            Some("dq".into())
        );
        assert_eq!(meta_name("no meta here"), None);
        assert_eq!(
            script_path_identity("/x/workflows/scripts/rung1-swarm-wf_1ee0382f-94a.js"),
            (Some("rung1-swarm".into()), Some("wf_1ee0382f-94a".into()))
        );
        assert_eq!(script_path_identity("/x/custom.js"), (Some("custom".into()), None));
    }

    #[test]
    fn launches_parses_a_transcript() {
        let dir = std::env::temp_dir().join(format!("cv-wf-launch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sess.jsonl");
        let lines = [
            json!({"type":"assistant","timestamp":"2026-07-08T09:02:20.652Z","message":{"content":[
                {"type":"tool_use","id":"tu_1","name":"Workflow","input":{"script":"export const meta = { name: 'doomed-swarm' }"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"tu_1","content":"Workflow launched in background. Task ID: w123"}]}})
            .to_string(),
            json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"tu_2","name":"Workflow","input":{"scriptPath":"/x/scripts/doomed-swarm-wf_abc-123.js","resumeFromRunId":"wf_abc-123"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"tu_2","is_error":true,"content":"<tool_use_error>still running</tool_use_error>"}]}})
            .to_string(),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        let ls = launches(&p).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0].name.as_deref(), Some("doomed-swarm"));
        assert!(!ls[0].errored);
        assert_eq!(ls[0].ts.unwrap().to_rfc3339(), "2026-07-08T09:02:20.652+00:00");
        assert_eq!(ls[1].script_run_id.as_deref(), Some("wf_abc-123"));
        assert_eq!(ls[1].resume_from.as_deref(), Some("wf_abc-123"));
        assert!(ls[1].errored);
    }

    #[test]
    fn killed_run_keeps_error_and_partial_agents() {
        let v = json!({
            "runId": "wf_k",
            "workflowName": "doomed",
            "status": "killed",
            "error": "session limit",
            "agentCount": 1,
            "phases": [{"title": "only"}],
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "only"},
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1",
                 "state": "progress", "tokens": 1000}
            ]
        });
        let w = parse_state(&v);
        assert_eq!(w.status.as_deref(), Some("killed"));
        assert_eq!(w.error.as_deref(), Some("session limit"));
        assert_eq!(w.phases[0].agents[0].state.as_deref(), Some("progress"));
    }

    #[test]
    fn meta_name_is_bounded_to_balanced_meta_object() {
        // Earlier identifier containing `name`, earlier object with `name:`, nested `{ name: ... }`
        // inside `phases` before `meta.name`, and comments/strings with unbalanced braces.
        let script = r#"
            const rename = { name: 'wrong-helper' };
            // export const meta = { name: 'commented-out' }
            const tpl = `fake { export const meta = { name: 'in-template' }`;
            export const meta = {
                description: "has } brace in string",
                phases: [{ name: "nested-phase-name", title: "P1" }],
                "name": "real-workflow-name",
            };
            const after = { name: "too-late" };
        "#;
        assert_eq!(meta_name(script).as_deref(), Some("real-workflow-name"));

        // If `export const meta` has no top-level `name`, later objects do not leak in.
        let nameless = r#"
            export const meta = { description: "only desc", phases: [{ name: "phase-only" }] };
            const other = { name: "leak" };
        "#;
        assert_eq!(meta_name(nameless), None);
    }

    #[test]
    fn lane_work_mine_recovers_parallel_computed_prompts_from_parent_transcript() {
        let world = crate::test_world("claude-workflow-parent-prompt-recovery");
        let proj = world.base.join("proj");
        let sub_dir = proj.join("sess-parent").join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let parent = proj.join("sess-parent.jsonl");

        let parent_lines = [
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "tu_alpha",
                            "name": "Agent",
                            "input": { "description": "alpha", "prompt": "Computed prompt for ALPHA shard #1" }
                        },
                        {
                            "type": "tool_use",
                            "id": "tu_beta",
                            "name": "Task",
                            "input": { "description": "beta", "prompt": "Computed prompt for BETA shard #2" }
                        }
                    ]
                }
            })
            .to_string(),
            json!({
                "type": "user",
                "toolUseResult": { "agentId": "a111" },
                "message": {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu_alpha", "content": "ok" }]
                }
            })
            .to_string(),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "tu_beta",
                        "content": "agentId: b222\ncompleted"
                    }]
                }
            })
            .to_string(),
        ];
        std::fs::write(&parent, parent_lines.join("\n")).unwrap();

        // Subagent transcripts crashed before recording user message (or empty/missing file).
        let lane_alpha = sub_dir.join("agent-a111.jsonl");
        let lane_beta = sub_dir.join("agent-b222.jsonl");
        std::fs::write(
            &lane_beta,
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"w1","name":"Write","input":{"file_path":"/tmp/out.rs"}}]}}).to_string(),
        )
        .unwrap();

        let mined_alpha = LaneWork::mine(&lane_alpha);
        assert_eq!(
            mined_alpha.prompt.as_deref(),
            Some("Computed prompt for ALPHA shard #1")
        );

        let mined_beta = LaneWork::mine(&lane_beta);
        assert_eq!(mined_beta.prompt.as_deref(), Some("Computed prompt for BETA shard #2"));
        assert_eq!(mined_beta.files, vec!["/tmp/out.rs".to_string()]);
    }
}
