//! Affordances: one cross-harness vocabulary for what an agent does besides talk — plans, goals,
//! schedules, background jobs, sub-agent spawns and returns, skills, hooks, questions, approvals,
//! checkpoints, branches, forks and model switches (`docs/design/AFFORDANCES.md`).
//!
//! This is a **derived view**, not IR: [`of_session`] reads it off what the IR already says (tool
//! names through the one [`classify_tool_view`] table, message kinds and origins, lineage, a short
//! record table), and [`Adapter::store_affordances`](crate::harness::Adapter::store_affordances)
//! adds what lives only in a side store (Claude `tasks/`, Grok `plan.json`, …). The one IR-side
//! piece is [`AffordanceSink`], which adapters wrap around their sink: it unwraps deferred-tool
//! calls (`tool_call{name, arguments}`) to the tool they call, and makes a turn that calls a spawn
//! tool a [`MessageKind::SubagentSpawn`] and a synchronous spawn's result a
//! [`MessageKind::SubagentReturn`].

#[cfg(test)]
use crate::json::{ReadInput, Validation};
pub(crate) mod input;
mod text;

use crate::ir::{Block, Harness, Message, MessageKind, Origin, Role, Session};
use crate::json::{Json, JsonView};
use crate::stream::{Flow, MessageSink};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

macro_rules! kinds {
    ($( $(#[$m:meta])* $name:ident => $s:literal ),+ $(,)?) => {
        /// One word of the vocabulary. Serialized as its snake_case name.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Kind { $( $(#[$m])* $name, )+ }

        impl Kind {
            /// Every kind, in vocabulary order.
            pub const ALL: &'static [Kind] = &[$(Kind::$name),+];

            pub fn as_str(self) -> &'static str {
                match self { $( Kind::$name => $s, )+ }
            }
        }
    };
}

kinds! {
    /// A todo / task list, or a plan-mode switch.
    Plan => "plan",
    /// A standing objective the harness tracks.
    Goal => "goal",
    /// Cron jobs, wakeups, loops, heartbeats — and their fires.
    Schedule => "schedule",
    /// A shell or task left running, polled, stopped, completed.
    BackgroundJob => "background_job",
    /// A sub-agent launched.
    SubagentSpawn => "subagent_spawn",
    /// A sub-agent's result delivered.
    SubagentReturn => "subagent_return",
    /// A skill loaded or managed.
    Skill => "skill",
    /// A user hook's output reached the model.
    Hook => "hook",
    /// The agent asked the person.
    Question => "question",
    /// A permission request and its outcome, or the approval mode the session ran under.
    Approval => "approval",
    /// A restorable point (a file snapshot, a shadow-git commit, a saved chat) and its restore.
    Checkpoint => "checkpoint",
    /// Rewind / undo / reset / clear.
    Branch => "branch",
    /// This session was forked from another.
    Fork => "fork",
    /// The model or effort changed.
    ModelChange => "model_change",
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// Where a row was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A tool call in the transcript.
    ToolCall,
    /// A message the harness recorded (a notification, a marker, a hook's output).
    Message,
    /// A side store beside the transcript (a todo file, a table, a state row).
    Store,
}

/// One entry of a plan or goal, normalized from whatever list shape the harness writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// One affordance row.
#[derive(Debug, Clone, Serialize)]
pub struct Affordance {
    pub kind: Kind,
    /// What happened: `write`, `create`, `update`, `read`, `delete`, `list`, `enter`, `exit`,
    /// `start`, `stop`, `complete`, `fire`, `spawn`, `return`, `load`, `manage`, `output`, `ask`,
    /// `deny`, `restore`, `branch`, `fork`, `change`, `state` (a side store's current contents, or a
    /// fact the harness keeps once per session).
    pub action: &'static str,
    /// The message index, for a row read from the transcript; `None` for a store row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    /// The harness's own name for it: the tool, or the store / record it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// One line: the prompt, command, schedule, skill name, or a count of items. Transcript text
    /// (untrusted): a terminal seam sanitizes it.
    pub summary: String,
    /// A plan's or goal's entries, when the call or store carries them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
    pub source: Source,
}

impl Affordance {
    /// A row from a side store (no message index).
    pub fn store(kind: Kind, tool: impl Into<String>, summary: impl Into<String>, items: Vec<Item>) -> Affordance {
        Affordance {
            kind,
            action: "state",
            index: None,
            timestamp: None,
            tool: Some(tool.into()),
            tool_use_id: None,
            summary: summary.into(),
            items,
            source: Source::Store,
        }
    }
}

impl Affordance {
    /// An event a side store logged (a checkpoint commit, a permission ask): a store row with its
    /// own action and time, still without a message index.
    pub fn store_event(
        kind: Kind,
        action: &'static str,
        timestamp: Option<DateTime<Utc>>,
        tool: impl Into<String>,
        summary: &str,
    ) -> Affordance {
        Affordance {
            action,
            timestamp,
            summary: crate::ir::truncate(summary, 120),
            ..Affordance::store(kind, tool, "", Vec::new())
        }
    }
}

/// A side store's plan (or goal) as one row: its entries and their count by status. `None` when it
/// holds none: an empty todo file is a plan that is not there.
pub fn plan_state(kind: Kind, tool: impl Into<String>, items: Vec<Item>) -> Option<Affordance> {
    (!items.is_empty()).then(|| Affordance::store(kind, tool, items_summary(&items), items))
}

/// Read an optional JSON side file. Absence is empty; I/O and decode failures retain its path.
pub(crate) fn side_json(path: &std::path::Path) -> anyhow::Result<Option<Value>> {
    use anyhow::Context;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("decoding {}", path.display()))
}

/// The JSON files of `dir`, sorted by name (numeric stems sort numerically).
/// Only a missing directory is empty; a failed entry, read or decode is an error.
pub fn json_files(dir: &std::path::Path) -> anyhow::Result<Vec<(String, Value)>> {
    let mut files = Vec::new();
    for entry in crate::harness::side_entries(dir)? {
        let path = entry.path();
        if !path.extension().is_some_and(|x| x == "json") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 JSON filename: {}", path.display()))?;
        if let Some(value) = side_json(&path)? {
            files.push((stem.to_owned(), value));
        }
    }
    files.sort_by(|a, b| match (a.0.parse::<u64>(), b.0.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.0.cmp(&b.0),
    });
    Ok(files)
}

/// The session-bag key under which an adapter lists the approval modes a session ran under, in
/// order of first appearance (Claude `permissionMode`, Codex `approval_policy`, Kimi Code
/// `permission.set_mode`, Qwen `approvalMode`, Kimi `state.json` `approval`).
pub const MODES_KEY: &str = "permission_modes";

/// The session-bag key under which an adapter tallies the restorable points it read past:
/// `{count, files?, tag?}` (Claude `file-history-snapshot`, Kimi Code `file_history.checkpoint`,
/// Kimi `_checkpoint`, a Gemini `/chat save` file).
pub const CHECKPOINTS_KEY: &str = "checkpoints";

/// Note one approval mode in a session bag ([`MODES_KEY`]); a repeat of a listed mode is a no-op.
pub fn note_mode(bag: &mut serde_json::Map<String, Value>, mode: &str) {
    if mode.is_empty() {
        return;
    }
    let list = bag.entry(MODES_KEY).or_insert_with(|| Value::Array(Vec::new()));
    if let Some(a) = list.as_array_mut() {
        if !a.iter().any(|v| v.as_str() == Some(mode)) {
            a.push(Value::String(mode.to_string()));
        }
    }
}

/// Note restorable points in a session bag ([`CHECKPOINTS_KEY`]): `points` more of them, and the
/// files they hold (each listed once).
pub fn note_checkpoints<'f>(
    bag: &mut serde_json::Map<String, Value>,
    points: u64,
    files: impl IntoIterator<Item = &'f str>,
) {
    let entry = bag
        .entry(CHECKPOINTS_KEY)
        .or_insert_with(|| serde_json::json!({"count": 0}));
    let Some(o) = entry.as_object_mut() else {
        return;
    };
    let n = o.get("count").and_then(Value::as_u64).unwrap_or(0) + points;
    o.insert("count".into(), n.into());
    for f in files {
        let list = o.entry("files").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(a) = list.as_array_mut() {
            if !a.iter().any(|v| v.as_str() == Some(f)) {
                a.push(Value::String(f.to_string()));
            }
        }
    }
}

/// How a spawn tool's result relates to the sub-agent's return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Returns {
    /// The tool result IS the sub-agent's final answer (it ran synchronously).
    Result,
    /// The result is the answer unless its text contains this marker of a background launch.
    ResultUnless(&'static str),
    /// The result is only an acknowledgement; the return arrives later as its own record.
    Later,
    /// The result is the answer, and may ride inside the next prompt beside the user's own text
    /// (Roo's `new_task`): that message stays a prompt, the result is named after this spawn tool
    /// (the reader leaves it unnamed) and counted as a return detail.
    ResultInPrompt(&'static str),
}

fn flag(input: JsonView<'_>, key: &str) -> Result<bool> {
    input
        .get(key)?
        .map(|v| v.as_bool())
        .transpose()
        .map(|v| v.flatten() == Some(true))
}

/// The cron-style tools that take an action argument. Compare the complete string without
/// materializing a potentially giant ignored action value.
fn action_verb(input: JsonView<'_>) -> Result<&'static str> {
    const ACTIONS: [(&str, &str); 19] = [
        ("create", "create"),
        ("add", "create"),
        ("new", "create"),
        ("schedule", "create"),
        ("remove", "delete"),
        ("delete", "delete"),
        ("cancel", "delete"),
        ("rm", "delete"),
        ("list", "list"),
        ("ls", "list"),
        ("status", "list"),
        ("get", "list"),
        ("show", "list"),
        ("run", "fire"),
        ("run_now", "fire"),
        ("trigger", "fire"),
        ("kill", "stop"),
        ("stop", "stop"),
        ("terminate", "stop"),
    ];
    let action = input
        .get("action")?
        .map(|v| input::word(v, &ACTIONS.map(|(word, _)| word)))
        .transpose()?
        .flatten();
    Ok(action.map_or("update", |index| ACTIONS[index].1))
}

/// The one table: `(harness, tool name, input)` → `(kind, action)`, or `None` for a tool that is not
/// an affordance. Scoped by harness only where a name means different things in different
/// harnesses (`task` is a sub-agent in OpenCode; `Bash` is a background job only when told to be).
/// Test shim over the view table with owned JSON: owned views cannot encounter source failures.
#[cfg(test)]
fn classify_tool(h: Harness, name: &str, value: &Value) -> Option<(Kind, &'static str)> {
    classify_tool_view(h, name, JsonView::from(value)).expect("owned JSON classification")
}

pub fn classify_tool_json(h: Harness, name: &str, value: &Json) -> Result<Option<(Kind, &'static str)>> {
    value.with_view(|view| classify_tool_view(h, name, view))
}

pub fn classify_tool_view(h: Harness, name: &str, input: JsonView<'_>) -> Result<Option<(Kind, &'static str)>> {
    use Kind::*;
    // OpenClaw names a deferred core tool `openclaw:core:<tool>`.
    let name = name.rsplit(':').next().unwrap_or(name);
    let process = |input| -> Result<(Kind, &'static str)> {
        Ok(match action_verb(input)? {
            "stop" => (BackgroundJob, "stop"),
            _ => (BackgroundJob, "read"),
        })
    };
    // The names a harness gives a meaning of its own; every other name reads the same in all.
    let own = match h {
        Harness::Hermes => match name {
            "todo" => Some((Plan, "write")),
            "todo_list" => Some((Plan, "read")),
            "cronjob" | "cronjob_manage" => Some((Schedule, action_verb(input)?)),
            "terminal" if flag(input, "background")? => Some((BackgroundJob, "start")),
            "process" | "process_manage" => Some(process(input)?),
            "clarify" => Some((Question, "ask")),
            _ => None,
        },
        Harness::OpenClaw => match name {
            "cron" | "automations" => Some((Schedule, action_verb(input)?)),
            "exec" if flag(input, "background")? => Some((BackgroundJob, "start")),
            "process" => Some(process(input)?),
            _ => None,
        },
        Harness::Qwen => match name {
            "task_create" => Some((Plan, "create")),
            "task_update" => Some((Plan, "update")),
            "task_list" | "task_get" => Some((Plan, "read")),
            "skill" => Some((Skill, "load")),
            _ => None,
        },
        // Kimi's `TaskList` lists background tasks, and its `TaskGet` reads no plan.
        Harness::Kimi | Harness::KimiCode => match name {
            "TaskList" => Some((BackgroundJob, "read")),
            "TaskGet" => return Ok(None),
            "Agent" | "AgentSwarm" if h == Harness::KimiCode => Some((SubagentSpawn, "spawn")),
            _ => None,
        },
        Harness::OpenCode => match name {
            "skill" => Some((Skill, "load")),
            "question" => Some((Question, "ask")),
            _ => None,
        },
        Harness::Roo => match name {
            "skill" => Some((Skill, "load")),
            _ => None,
        },
        Harness::Codex => match name {
            "wait" if input.get("cell_id")?.is_some() => Some((BackgroundJob, "read")),
            "write_stdin" => Some((BackgroundJob, "read")),
            _ => None,
        },
        Harness::Claude
        | Harness::Grok
        | Harness::Gemini
        | Harness::Cursor
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => None,
    };
    if own.is_some() {
        return Ok(own);
    }
    Ok(Some(match name {
        // plan: whole-list writes
        "TodoWrite" | "todo_write" | "todowrite" | "write_todos" | "todo__todo_write" | "SetTodoList" | "TodoList"
        | "update_plan" | "Checklist" | "update_todo_list" => (Plan, "write"),
        "todoread" => (Plan, "read"),
        // plan: item-wise task lists
        "TaskCreate" | "tracker_create_task" => (Plan, "create"),
        "TaskUpdate" | "tracker_update_task" => (Plan, "update"),
        "TaskList" | "TaskGet" => (Plan, "read"),
        "tracker_list_tasks" | "tracker_get_task" | "tracker_visualize" => (Plan, "read"),
        // Unknown tracker operations count as plan updates; only the explicit rows above
        // distinguish reads and creates. This prefix is a classification heuristic.
        n if n.starts_with("tracker_") => (Plan, "update"),
        // plan mode
        "EnterPlanMode" | "enter_plan_mode" => (Plan, "enter"),
        "ExitPlanMode" | "exit_plan_mode" => (Plan, "exit"),
        // goal
        "CreateGoal" => (Goal, "create"),
        "UpdateGoal" | "update_goal" | "SetGoalBudget" => (Goal, "update"),
        "GetGoal" | "get_goal" => (Goal, "read"),
        // schedule
        "CronCreate" | "cron_create" | "scheduler_create" | "ScheduleWakeup" | "loop_wakeup" => (Schedule, "create"),
        "CronDelete" | "cron_delete" | "scheduler_delete" => (Schedule, "delete"),
        "CronList" | "cron_list" | "scheduler_list" => (Schedule, "list"),
        // background jobs
        "Bash" if flag(input, "run_in_background")? => (BackgroundJob, "start"),
        "run_terminal_command" if flag(input, "background")? || flag(input, "is_background")? => {
            (BackgroundJob, "start")
        }
        "run_shell_command" if flag(input, "is_background")? => (BackgroundJob, "start"),
        "Monitor" | "monitor" => (BackgroundJob, "start"),
        "BashOutput"
        | "TaskOutput"
        | "get_command_or_subagent_output"
        | "WaitFor"
        | "CheckBackgroundJob"
        | "write_stdin" => (BackgroundJob, "read"),
        "KillShell" | "TaskStop" | "kill_command_or_subagent" => (BackgroundJob, "stop"),
        // sub-agents
        n if spawn_returns(h, n).is_some() => (SubagentSpawn, "spawn"),
        // skills
        "Skill" | "activate_skill" | "load_skill" | "Skills" => (Skill, "load"),
        "skill_manage" => (Skill, "manage"),
        // questions to the person
        "AskUserQuestion"
        | "ask_user"
        | "ask_user_question"
        | "ask_followup_question"
        | "ask_question"
        | "AskQuestion"
        | "request_user_input"
        | "request_user_input_async" => (Question, "ask"),
        _ => return Ok(None),
    }))
}

/// Whether `(h, name)` spawns a sub-agent, and how its result relates to the return.
pub fn spawn_returns(h: Harness, name: &str) -> Option<Returns> {
    let one = |tool: &str, returns| (name == tool).then_some(returns);
    match h {
        // Claude marks a synchronous return itself (from `toolUseResult`), per record.
        Harness::Claude => matches!(name, "Agent" | "Task" | "Workflow").then_some(Returns::Later),
        Harness::Codex => one("spawn_agent", Returns::Later),
        Harness::OpenCode => match name {
            "task" => Some(Returns::Result),
            "subagent" => Some(Returns::ResultUnless("in the background")),
            _ => None,
        },
        Harness::Cline => match name {
            "spawn_agent" => Some(Returns::Result),
            "team_spawn_teammate" => Some(Returns::Later),
            _ => None,
        },
        Harness::Hermes => one("delegate_task", Returns::ResultUnless("\"dispatched\"")),
        Harness::OpenClaw => one("sessions_spawn", Returns::Later),
        Harness::Goose => one("delegate", Returns::Result),
        Harness::Grok => one("spawn_subagent", Returns::Later),
        Harness::Kimi => one("Agent", Returns::ResultUnless("background")),
        Harness::Roo => one("new_task", Returns::ResultInPrompt("new_task")),
        Harness::Devin => one("run_subagent", Returns::Result),
        Harness::Continue => one("Subagent", Returns::Result),
        Harness::Gemini
        | Harness::Cursor
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::LmStudio
        | Harness::Zed
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => None,
    }
}

/// Extract the spawned child agent identifier (`task_name` or `agent_id`) from a spawn tool's
/// non-error result payload when structured.
pub fn spawn_return_id(h: Harness, name: &str, result_text: &str, is_error: bool) -> Option<String> {
    if is_error {
        return None;
    }
    match h {
        Harness::Codex if name == "spawn_agent" => {
            let v: Value = serde_json::from_str(result_text.trim()).ok()?;
            for key in ["task_name", "agent_id"] {
                if let Some(id) = v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    return Some(id.to_string());
                }
            }
            None
        }
        Harness::Claude
        | Harness::Codex
        | Harness::Grok
        | Harness::OpenCode
        | Harness::Gemini
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => None,
    }
}

/// The deferred-tool wrapper Hermes, OpenClaw 10.1 and Qwen route lazily loaded tools through.
/// Hermes and Qwen spell it `tool_call{name, arguments}`, OpenClaw `tool_call{id, args}`. In the
/// IR the call is the inner tool's, with this namespace.
pub const DEFERRED: &str = "tool_call";

/// The keys of a store's deferred-tool wrapper ([`DEFERRED`]): the inner tool's name and its
/// arguments ([`crate::harness::Capabilities::deferred_wrapper`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredWrapper {
    pub name_key: &'static str,
    pub args_key: &'static str,
}

/// `tool_call{name, arguments}` (or OpenClaw's `{id, args}`) → the inner tool's call, namespaced
/// [`DEFERRED`]. Anything else (including a wrapper without a string name) is left alone.
pub fn unwrap_deferred(wrapper: DeferredWrapper, b: &mut Block) -> Result<()> {
    if let Block::ToolUse {
        name, input, namespace, ..
    } = b
    {
        if name != DEFERRED || namespace.is_some() {
            return Ok(());
        }
        let DeferredWrapper { name_key, args_key } = wrapper;
        let Some(inner) = input.with_view(|view| input::field_string(view, name_key))? else {
            return Ok(());
        };
        // Move an owned wrapper's child instead of cloning its argument tree. Source children
        // keep the validated range/identity and do not materialize descendants.
        let args = match input {
            Json::Owned(value) => Json::Owned(
                value
                    .as_object_mut()
                    .and_then(|object| object.remove(args_key))
                    .unwrap_or(Value::Null),
            ),
            Json::Source(_) => input.select(args_key)?.unwrap_or_default(),
        };
        *name = inner;
        *input = args;
        *namespace = Some(DEFERRED.to_string());
    }
    Ok(())
}

/// Complete replay boundary for stores that spell deferred calls as wrappers. The returned
/// owned wire object demands every argument; ordinary classification uses the unwrapped source
/// descriptor and never calls this materializing operation.
pub fn rewrap_deferred(wrapper: DeferredWrapper, b: &Block) -> Result<Option<Block>> {
    Ok(match b {
        Block::ToolUse {
            id,
            name,
            input,
            namespace: Some(ns),
        } if ns == DEFERRED => {
            let DeferredWrapper { name_key, args_key } = wrapper;
            let mut wrapped = serde_json::Map::new();
            wrapped.insert(name_key.into(), Value::String(name.clone()));
            wrapped.insert(args_key.into(), input.resolve()?.into_owned());
            Some(Block::ToolUse {
                id: id.clone(),
                name: DEFERRED.to_string(),
                input: Json::Owned(Value::Object(wrapped)),
                namespace: None,
            })
        }
        _ => None,
    })
}

/// A sink wrapper an adapter puts around its sink. It unwraps the deferred `tool_call` wrapper (for
/// an adapter whose reader leaves it, [`AffordanceSink::unwrapping`]), then: a `Reply` that calls a spawn tool becomes a
/// [`MessageKind::SubagentSpawn`], and the tool-result turn answering a synchronous spawn a
/// [`MessageKind::SubagentReturn`] with [`Origin::Subagent`]. The state is the set of open
/// synchronous spawn ids, so it stays O(spawns).
pub struct AffordanceSink<'a> {
    harness: Harness,
    deferred: Option<DeferredWrapper>,
    inner: &'a mut dyn MessageSink,
    open: std::collections::HashMap<String, Returns>,
    latch: crate::stream::Latch,
}

impl<'a> AffordanceSink<'a> {
    pub fn new(harness: Harness, inner: &'a mut dyn MessageSink) -> Self {
        AffordanceSink {
            harness,
            deferred: None,
            inner,
            open: Default::default(),
            latch: Default::default(),
        }
    }

    /// Unwrap `wrapper`'s deferred calls too: an adapter whose reader leaves them passes its
    /// [`crate::harness::Capabilities::deferred_wrapper`].
    pub fn unwrapping(mut self, wrapper: Option<DeferredWrapper>) -> Self {
        self.deferred = wrapper;
        self
    }
}

impl MessageSink for AffordanceSink<'_> {
    fn tier(&self) -> crate::stream::Tier {
        self.inner.tier()
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            self.inner.dependency(path);
        }
    }
    fn append_only_dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            self.inner.append_only_dependency(path);
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take_or(self.inner, "affordance consumer")
    }

    fn message(&mut self, mut m: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        if let Some(wrapper) = self.deferred {
            for block in &mut m.content {
                if let Err(error) = unwrap_deferred(wrapper, block) {
                    self.latch.fail(error);
                    return Flow::Stop;
                }
            }
        }
        if let Err(error) = refine_spawn(self.harness, &mut m, &mut self.open) {
            self.latch.fail(error);
            return Flow::Stop;
        }
        self.inner.try_message(m).unwrap_or_else(|error| {
            self.latch.fail(error);
            Flow::Stop
        })
    }

    fn meta(&mut self, s: &Session) {
        if self.latch.failed() {
            return;
        }
        self.inner.meta(s);
    }

    /// Never while a synchronous spawn waits for its result: which result returns it is state no
    /// checkpoint carries.
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        !self.latch.failed() && self.open.is_empty() && self.inner.wants_checkpoint(offset)
    }

    fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
        !self.latch.failed() && self.open.is_empty() && self.inner.wants_final_checkpoint(offset)
    }

    fn checkpoint(&mut self, cp: &mut crate::stream::Checkpoint) {
        if !self.latch.failed() {
            self.inner.checkpoint(cp);
        }
    }
}

/// Match launch receipts without retaining their decoded payload. Keep only the marker overlap
/// between chunks, and preserve checked source errors when an early match ends the demand.
fn source_contains(source: &crate::lazy::SourceText, marker: &str) -> Result<bool> {
    let mut window = Vec::new();
    let result = source.try_for_each_chunk(64 * 1024, |part| {
        window.extend_from_slice(part.as_bytes());
        if marker.is_empty()
            || window
                .windows(marker.len())
                .any(|candidate| candidate == marker.as_bytes())
        {
            return Err(crate::stream::Stop.into());
        }
        let keep = window.len().min(marker.len().saturating_sub(1));
        let start = window.len() - keep;
        window.copy_within(start.., 0);
        window.truncate(keep);
        Ok(())
    });
    match result {
        Err(error) if error.is::<crate::stream::Stop>() => Ok(true),
        Err(error) => Err(error),
        Ok(()) => Ok(marker.is_empty()),
    }
}

/// Apply the same native spawn classification before capturing a complete row witness.
#[cfg(any(feature = "sqlite", test))]
pub(crate) fn refine_complete_group(harness: Harness, messages: &mut [Message]) -> Result<()> {
    let mut open = std::collections::HashMap::new();
    for message in messages {
        refine_spawn(harness, message, &mut open)?;
    }
    Ok(())
}

fn refine_spawn(h: Harness, m: &mut Message, open: &mut std::collections::HashMap<String, Returns>) -> Result<()> {
    match (m.role, m.kind) {
        (Role::Assistant, MessageKind::Reply) => {
            let mut spawned = false;
            for b in &m.content {
                if let Block::ToolUse { id, name, .. } = b {
                    if let Some(ret) = spawn_returns(h, name) {
                        spawned = true;
                        if ret != Returns::Later {
                            open.insert(id.clone(), ret);
                        }
                    }
                }
            }
            if spawned {
                m.kind = MessageKind::SubagentSpawn;
            }
        }
        _ if !open.is_empty() && !m.content.is_empty() => {
            let all_results = m.content.iter().all(|b| matches!(b, Block::ToolResult { .. }));
            // This message can settle an open spawn when it is wholly results, or when a spawn
            // expects its return inside the next prompt beside the user's own text (Roo).
            let settles = all_results || open.values().any(|r| matches!(r, Returns::ResultInPrompt(_)));
            if !settles {
                return Ok(());
            }
            let mut returned = false;
            for b in &mut m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    tool_name,
                    ..
                } = b
                {
                    match open.remove(tool_use_id.as_str()) {
                        Some(Returns::Result) => returned = true,
                        Some(Returns::ResultInPrompt(spawn)) => {
                            returned = true;
                            if !all_results {
                                *tool_name = Some(spawn.into());
                            }
                        }
                        // A lazy span is a large result: an answer, not a launch receipt.
                        Some(Returns::ResultUnless(marker)) => {
                            returned |= match content {
                                crate::lazy::Text::Inline(t) => !t.contains(marker),
                                crate::lazy::Text::Span(_) => true,
                                crate::lazy::Text::Source(source) => !source_contains(source, marker)?,
                            }
                        }
                        _ => {}
                    }
                }
            }
            if returned && all_results {
                m.kind = MessageKind::SubagentReturn;
                m.origin = Origin::Subagent;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The entries of a plan/goal tool's input (or a store's JSON), whatever the list is called:
/// the first array under `todos`/`plan`/`items`/`tasks`/`checklist`, each element's text under
/// the first of `content`/`step`/`description`/`title`/`subject`/`text`/`task`/`name`; or a
/// markdown checklist / numbered list in a string under one of those keys.
pub(crate) fn items_of(v: &Value) -> Vec<Item> {
    items_of_view(JsonView::from(v)).expect("owned JSON items")
}

pub fn items_of_view(view: JsonView<'_>) -> Result<Vec<Item>> {
    const LISTS: &[&str] = &["todos", "plan", "items", "tasks", "checklist", "content", "markdown"];
    for &key in LISTS {
        let Some(value) = view.get(key)? else {
            continue;
        };
        if let Some(array) = value.array()? {
            return array.map(|v| item_of_view(v?)).filter_map(|v| v.transpose()).collect();
        }
        if key == "todos" {
            let mut out = Vec::new();
            if value.for_each_entry(|_, child| {
                if let Some(item) = item_of_view(child)? {
                    out.push(item);
                }
                Ok(())
            })? {
                return Ok(out);
            }
        }
        // Embedded JSON lists require their selected encoded string with the current backend.
        // This explicitly materializes that field, never unrelated tool arguments.
        if let Some(text) = input::string(value)? {
            if let Ok(Value::Array(array)) = serde_json::from_str::<Value>(&text) {
                return array
                    .iter()
                    .map(|v| item_of_view(JsonView::from(v)))
                    .filter_map(|v| v.transpose())
                    .collect();
            }
            let markdown = markdown_items(&text);
            if !markdown.is_empty() {
                return Ok(markdown);
            }
        }
    }
    if let Some(array) = view.array()? {
        return array.map(|v| item_of_view(v?)).filter_map(|v| v.transpose()).collect();
    }
    Ok(item_of_view(view)?.into_iter().collect())
}

pub(crate) fn item_of(v: &Value) -> Option<Item> {
    item_of_view(JsonView::from(v)).expect("owned JSON item")
}

/// The keys an item's text hides under, tried in order, wherever plan items are spelled.
const ITEM_TEXT_KEYS: &[&str] = &[
    "content",
    "subject",
    "title",
    "step",
    "description",
    "text",
    "task",
    "name",
    "goal",
];

fn item_text(view: JsonView<'_>) -> Result<Option<JsonView<'_>>> {
    if view.is_string()? {
        return Ok(Some(view));
    }
    for key in ITEM_TEXT_KEYS {
        if let Some(text) = view.get(key)? {
            if text.is_string()? {
                return Ok(Some(text));
            }
        }
    }
    Ok(None)
}

/// Reducer presence check without allocating a potentially large plan item's text.
pub(crate) fn item_present(view: JsonView<'_>) -> Result<bool> {
    let Some(text) = item_text(view)? else { return Ok(false) };
    let mut present = false;
    text.for_each_str_chunk(|chunk| {
        present |= !chunk.trim().is_empty();
        Ok(())
    })?;
    Ok(present)
}

pub fn item_of_view(view: JsonView<'_>) -> Result<Option<Item>> {
    let Some(text) = item_text(view)? else { return Ok(None) };
    let Some(content) = input::item_string(text, view.is_string()?)? else {
        return Ok(None);
    };
    let status = if view.is_string()? {
        None
    } else {
        match view.get("status")? {
            Some(value) => Some(value),
            None => view.get("state")?,
        }
        .map(input::string)
        .transpose()?
        .flatten()
    };
    Ok(Some(Item { content, status }))
}

/// `- [x] done`, `[ ] todo`, `[-] doing`, `1. step`, `* step` lines.
fn markdown_items(s: &str) -> Vec<Item> {
    let mut out = Vec::new();
    for line in s.lines() {
        let mut t = line.trim();
        let bullet = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "));
        let numbered = t
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            .map(|(_, rest)| rest);
        let listed = bullet.is_some() || numbered.is_some();
        t = bullet.or(numbered).unwrap_or(t).trim_start();
        let (status, rest) = if let Some(r) = t.strip_prefix("[x]").or_else(|| t.strip_prefix("[X]")) {
            (Some("completed"), r)
        } else if let Some(r) = t.strip_prefix("[ ]") {
            (Some("pending"), r)
        } else if let Some(r) = t.strip_prefix("[-]").or_else(|| t.strip_prefix("[~]")) {
            (Some("in_progress"), r)
        } else if listed {
            (None, t)
        } else {
            continue;
        };
        let rest = rest.trim();
        if !rest.is_empty() {
            out.push(Item {
                content: rest.to_string(),
                status: status.map(str::to_string),
            });
        }
    }
    out
}

/// A one-line count of a plan's entries by status: `3 items: 1 completed, 2 pending`.
pub fn items_summary(items: &[Item]) -> String {
    let mut by: std::collections::BTreeMap<&str, usize> = Default::default();
    for i in items {
        *by.entry(i.status.as_deref().unwrap_or("listed")).or_default() += 1;
    }
    let parts: Vec<String> = by.iter().map(|(s, n)| format!("{n} {s}")).collect();
    let n = items.len();
    format!("{n} item{}: {}", if n == 1 { "" } else { "s" }, parts.join(", "))
}

/// The summary line for one tool-call row.
fn item_summary(view: JsonView<'_>) -> Result<Option<String>> {
    if view.is_string()? {
        return Ok(input::trimmed_prefix(view, 120)?.filter(|text| !text.is_empty()));
    }
    for key in ITEM_TEXT_KEYS {
        if let Some(value) = view.get(key)? {
            if value.is_string()? {
                return input::nonblank_prefix(value, 120);
            }
        }
    }
    Ok(None)
}

fn call_summary(kind: Kind, view: JsonView<'_>, items: &[Item]) -> Result<String> {
    if !items.is_empty() {
        return Ok(items_summary(items));
    }
    if kind == Kind::Plan {
        if let Some(status) = view
            .get("status")?
            .map(|v| input::prefix(v, 120))
            .transpose()?
            .flatten()
        {
            for key in ["taskId", "task_id", "id", "subject"] {
                if let Some(which) = view
                    .get(key)?
                    .map(|v| input::scalar_prefix(v, 120))
                    .transpose()?
                    .flatten()
                {
                    let mut text = crate::projection::Head::bounded(120);
                    text.push(&which);
                    text.push(" → ");
                    text.push(&status);
                    return Ok(text.finish());
                }
            }
        }
    }
    let mut parts = Vec::new();
    for key in [
        "description",
        "subject",
        "prompt",
        "command",
        "cron",
        "schedule",
        "skill",
        "task_name",
        "name",
        "question",
        "title",
        "goal",
        "objective",
        "instructions",
        "message",
        "reason",
        "cell_id",
        "session_id",
        "id",
        "taskId",
    ] {
        if let Some(value) = view
            .get(key)?
            .map(|v| input::scalar_prefix(v, 120))
            .transpose()?
            .flatten()
        {
            parts.push(value);
            if parts.len() == 2 || kind != Kind::Schedule {
                break;
            }
        }
    }
    if parts.is_empty() {
        if let Some(questions) = view.get("questions")?.map(JsonView::array).transpose()?.flatten() {
            if let Some(first) = questions.into_iter().next() {
                if let Some(text) = first?
                    .get("question")?
                    .map(|v| input::prefix(v, 120))
                    .transpose()?
                    .flatten()
                {
                    parts.push(text);
                }
            }
        }
        if let Some(tasks) = view.get("tasks")?.map(JsonView::array).transpose()?.flatten() {
            if let Some(first) = tasks.into_iter().next() {
                if let Some(text) = item_summary(first?)? {
                    parts.push(text);
                }
            }
        }
        if let Some(status) = view
            .get("status")?
            .map(|v| input::prefix(v, 120))
            .transpose()?
            .flatten()
        {
            parts.push(status);
        }
    }
    let mut text = crate::projection::Head::bounded(120);
    for (index, part) in parts.iter().enumerate() {
        if index != 0 {
            text.push(" · ");
        }
        text.push(part);
    }
    Ok(text.finish())
}

/// Record-shaped affordances: messages the harness wrote that no `MessageKind` names, as the
/// adapter that owns the record reads them.
fn record(h: Harness, m: &Message, resolver: &crate::lazy::Resolver) -> Result<Option<RecordHit>> {
    let Some(adapter) = crate::harness::rootless(h) else {
        return Ok(None);
    };
    let text = match m.content.first() {
        Some(Block::Text { text }) => Some(text),
        _ => None,
    };
    adapter.record_affordance(m, &FirstText { text, resolver })
}

/// A message's one-line summary from its complete first text or tool output. The reducer searches
/// for the first complete summary/reminder wrapper and keeps only enough text for the final row.
fn text_head(m: &Message, resolver: &crate::lazy::Resolver) -> Result<String> {
    m.content
        .iter()
        .find_map(|b| match b {
            Block::Text { text } => Some(text),
            Block::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .map(|body| text::facts(body, resolver).map(|facts| facts.summary()))
        .transpose()
        .map(|head| head.unwrap_or_default())
}

/// What one message contributes: a tool call the table names, or the message itself (a kind, a
/// record, an origin). The one classification behind both [`of_session`] (which builds the rows)
/// and [`Counter`] (which only counts them), so the catalog's counts and the view cannot disagree.
enum Hit<'a> {
    Call {
        kind: Kind,
        action: &'static str,
        id: &'a str,
        name: &'a str,
        input: &'a Json,
    },
    Message {
        kind: Kind,
        action: &'static str,
        tool: Option<&'static str>,
    },
    /// A fact one block or one bag entry carries, with its own summary: a denied tool result, a
    /// file snapshot riding a turn.
    Detail {
        kind: Kind,
        action: &'static str,
        tool: Option<&'a str>,
        tool_use_id: Option<&'a str>,
        summary: String,
    },
}

/// How a harness words an error result that is the user (or a rule) refusing the call: the text
/// starts with `prefix` and, when given, also contains `also`
/// ([`crate::harness::Capabilities::denials`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denial {
    pub prefix: &'static str,
    pub also: Option<&'static str>,
}

/// One affordance a message's harness bag records outside its blocks
/// ([`crate::harness::Adapter::message_affordances`]): a row with no call of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BagHit {
    pub kind: Kind,
    pub action: &'static str,
    pub tool: &'static str,
    pub summary: String,
}

/// The affordance one of a harness's own records is (a skill load, a completion notice, a refused
/// call) when no `MessageKind` names it ([`crate::harness::Adapter::record_affordance`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHit {
    pub kind: Kind,
    pub action: &'static str,
    pub tool: &'static str,
}

/// A message's leading text block, read through its resolver, for an adapter's record test.
pub struct FirstText<'a> {
    text: Option<&'a crate::lazy::Text>,
    resolver: &'a crate::lazy::Resolver,
}

impl FirstText<'_> {
    /// Whether the text, leading whitespace aside, starts with `prefix`; false with no text.
    pub fn starts_with(&self, prefix: &str) -> Result<bool> {
        self.text
            .map_or(Ok(false), |text| text::starts_with(text, self.resolver, prefix))
    }

    /// Whether the text contains `needle`; false with no text.
    pub fn contains(&self, needle: &'static str) -> Result<bool> {
        self.text
            .map_or(Ok(false), |text| text::contains(text, self.resolver, needle))
    }
}

/// The first eight characters of a snapshot hash, for a summary.
pub(crate) fn short(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

/// The paths of a JSON list of strings, joined for a summary.
pub(crate) fn joined(v: Option<&Value>) -> String {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// The restorable points a message carries beside its content: OpenCode's `patch` parts (1.x) and
/// `snapshot{start, end, files}` (2.x) when the step changed files, Devin's `restorable` files on
/// an edit's result.
fn detail_hits<'a>(
    h: Harness,
    m: &'a Message,
    resolver: &crate::lazy::Resolver,
    summaries: bool,
    out: &mut Vec<Hit<'a>>,
) -> Result<()> {
    for b in &m.content {
        let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            details,
            ..
        } = b
        else {
            continue;
        };
        let in_prompt = tool_name
            .as_deref()
            .and_then(|name| spawn_returns(h, name))
            .is_some_and(|r| matches!(r, Returns::ResultInPrompt(_)));
        if in_prompt && m.kind != MessageKind::SubagentReturn {
            out.push(Hit::Detail {
                kind: Kind::SubagentReturn,
                action: "return",
                tool: tool_name.as_deref(),
                tool_use_id: Some(tool_use_id),
                summary: if summaries {
                    text::plain_summary(content, resolver)?
                } else {
                    String::new()
                },
            });
        }
        if *is_error && text::denied(content, resolver, h)? {
            out.push(Hit::Detail {
                kind: Kind::Approval,
                action: "deny",
                tool: tool_name.as_deref(),
                tool_use_id: Some(tool_use_id),
                summary: if summaries {
                    text::plain_summary(content, resolver)?
                } else {
                    String::new()
                },
            });
        }
        if !*is_error {
            if let Some(summary) = text::running_cell_summary(content, resolver, h, summaries)? {
                out.push(Hit::Detail {
                    kind: Kind::BackgroundJob,
                    action: "start",
                    tool: tool_name.as_deref(),
                    tool_use_id: Some(tool_use_id),
                    summary,
                });
            }
        }
        if let Some(files) = details
            .as_ref()
            .and_then(|d| d.get("restorable"))
            .filter(|f| !joined(Some(f)).is_empty())
        {
            out.push(Hit::Detail {
                kind: Kind::Checkpoint,
                action: "create",
                tool: tool_name.as_deref(),
                tool_use_id: Some(tool_use_id),
                summary: crate::ir::truncate(&joined(Some(files)), 120),
            });
        }
    }
    for hit in crate::harness::rootless(h)
        .map(|a| a.message_affordances(m))
        .unwrap_or_default()
    {
        out.push(Hit::Detail {
            kind: hit.kind,
            action: hit.action,
            tool: Some(hit.tool),
            tool_use_id: None,
            summary: hit.summary,
        });
    }
    Ok(())
}

fn hits<'a>(h: Harness, m: &'a Message, resolver: &crate::lazy::Resolver, summaries: bool) -> Result<Vec<Hit<'a>>> {
    let mut out = Vec::new();
    let mut spawn_calls = 0;
    for b in &m.content {
        let Block::ToolUse { id, name, input, .. } = b else {
            continue;
        };
        let Some((kind, action)) = classify_tool_json(h, name, input)? else {
            continue;
        };
        // A spawn is a spawn when the adapter said so (the message kind); a harness that marks
        // spawns with its own records (Codex, Kimi Code) is read from those.
        if kind == Kind::SubagentSpawn {
            if m.kind != MessageKind::SubagentSpawn {
                continue;
            }
            spawn_calls += 1;
        }
        out.push(Hit::Call {
            kind,
            action,
            id,
            name,
            input,
        });
    }
    let by_kind = match m.kind {
        MessageKind::SubagentSpawn if spawn_calls == 0 => Some((Kind::SubagentSpawn, "spawn")),
        MessageKind::SubagentReturn => Some((Kind::SubagentReturn, "return")),
        MessageKind::Branch => Some((Kind::Branch, "branch")),
        MessageKind::ModelChange => Some((Kind::ModelChange, "change")),
        _ => None,
    };
    let message = match by_kind {
        Some((kind, action)) => Some((kind, action, None)),
        None => record(h, m, resolver)?
            .map(|hit| (hit.kind, hit.action, Some(hit.tool)))
            .or(match m.origin {
                Origin::Hook => Some((Kind::Hook, "output", None)),
                Origin::Scheduler => Some((Kind::Schedule, "fire", None)),
                _ => None,
            }),
    };
    if let Some((kind, action, tool)) = message {
        out.push(Hit::Message { kind, action, tool });
    }
    detail_hits(h, m, resolver, summaries, &mut out)?;
    Ok(out)
}

/// A row the session carries once rather than at a message.
/// The approval-mode row: the modes a session passed through, in order, under `tool`.
pub(crate) fn mode_row(tool: &str, modes: &[&str]) -> Affordance {
    let summary = format!("mode: {}", modes.join(" → "));
    session_row(Kind::Approval, "state", tool, summary, Vec::new())
}

pub(crate) fn session_row(
    kind: Kind,
    action: &'static str,
    tool: &str,
    summary: String,
    items: Vec<Item>,
) -> Affordance {
    Affordance {
        kind,
        action,
        index: None,
        timestamp: None,
        tool: Some(tool.to_string()),
        tool_use_id: None,
        summary: crate::ir::truncate(&summary, 120),
        items,
        source: Source::Message,
    }
}

/// The facts a harness keeps once per session, as rows: the fork, the approval modes an adapter
/// noted ([`MODES_KEY`]), what the adapter reads off its own bag
/// ([`crate::harness::Adapter::session_affordances`]), and the restorable points an adapter tallied
/// ([`CHECKPOINTS_KEY`]). Read off the session's metadata, so a streamed pass that saw no message
/// of it still counts it ([`Counter::finish`]).
pub fn session_rows(s: &Session) -> Vec<Affordance> {
    let mut out = Vec::new();
    if let Some(from) = &s.lineage.forked_from {
        out.push(Affordance {
            timestamp: s.created_at,
            tool: None,
            ..session_row(Kind::Fork, "fork", "", format!("forked from {from}"), Vec::new())
        });
    }
    let Some(bag) = s.harness_extra(s.harness) else {
        return out;
    };
    let modes: Vec<&str> = bag
        .get(MODES_KEY)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !modes.is_empty() {
        out.push(mode_row(MODES_KEY, &modes));
    }
    out.extend(
        crate::harness::rootless(s.harness)
            .map(|a| a.session_affordances(s))
            .unwrap_or_default(),
    );
    if let Some(cp) = bag.get(CHECKPOINTS_KEY).and_then(Value::as_object) {
        let n = cp.get("count").and_then(Value::as_u64).unwrap_or(0);
        let files: Vec<Item> = cp
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|f| Item {
                content: f.to_string(),
                status: None,
            })
            .collect();
        let mut summary = format!("{n} checkpoint{}", if n == 1 { "" } else { "s" });
        if let Some(tag) = cp.get("tag").and_then(Value::as_str) {
            summary.push_str(&format!(" · tag {tag}"));
        }
        if !files.is_empty() {
            let k = files.len();
            summary.push_str(&format!(" · {k} file{} tracked", if k == 1 { "" } else { "s" }));
        }
        if n > 0 {
            out.push(session_row(Kind::Checkpoint, "state", CHECKPOINTS_KEY, summary, files));
        }
    }
    out
}

/// Every affordance the transcript shows: the session's own rows ([`session_rows`]) first, then
/// message order. Safe on a lazy parse: text is read through the session's resolver, a few KB at
/// most per message.
pub fn of_session(s: &Session) -> Result<Vec<Affordance>> {
    let resolver = s.resolver();
    let mut out = session_rows(s);
    for (i, m) in s.messages.iter().enumerate() {
        out.extend(message_rows(s.harness, m, i, &resolver)?);
    }
    Ok(out)
}

/// Render one message's hits identically for whole-session and streamed views.
fn message_rows(harness: Harness, m: &Message, i: usize, resolver: &crate::lazy::Resolver) -> Result<Vec<Affordance>> {
    let mut out = Vec::new();
    for hit in hits(harness, m, resolver, true)? {
        out.push(match hit {
            Hit::Call {
                kind,
                action,
                id,
                name,
                input,
            } => {
                let items = if matches!(kind, Kind::Plan | Kind::Goal) {
                    input.with_view(items_of_view)?
                } else {
                    Vec::new()
                };
                Affordance {
                    kind,
                    action,
                    index: Some(i),
                    timestamp: m.timestamp,
                    tool: Some(name.to_string()),
                    tool_use_id: Some(id.to_string()),
                    summary: input.with_view(|view| call_summary(kind, view, &items))?,
                    items,
                    source: Source::ToolCall,
                }
            }
            Hit::Message { kind, action, tool } => Affordance {
                kind,
                action,
                index: Some(i),
                timestamp: m.timestamp,
                tool: tool.map(str::to_string),
                tool_use_id: None,
                summary: match (kind, &m.model) {
                    (Kind::ModelChange, Some(model)) => model.clone(),
                    _ => text_head(m, resolver)?,
                },
                items: Vec::new(),
                source: Source::Message,
            },
            Hit::Detail {
                kind,
                action,
                tool,
                tool_use_id,
                summary,
            } => Affordance {
                kind,
                action,
                index: Some(i),
                timestamp: m.timestamp,
                tool: tool.map(str::to_string),
                tool_use_id: tool_use_id.map(str::to_string),
                summary,
                items: Vec::new(),
                source: Source::Message,
            },
        });
    }
    Ok(out)
}

/// The view's version is the one stored-format version ([`crate::source::VERSION`]): bump that
/// whenever a kind, a table row or a reader changes what [`Counter`] would count, so counts an
/// older view recorded no longer decode and are recounted, instead of answering `has:<new kind>`
/// with a stale no.
pub const VIEW_VERSION: u32 = crate::source::VERSION;

/// Affordance counts by kind: what the catalog stores per session so `has:<kind>` never parses.
pub type Counts = std::collections::BTreeMap<Kind, u32>;

/// Counts a session's affordances one message at a time, inside a pass that is reading it anyway
/// (`stream::stream_counted`), with the same classification as [`of_session`].
pub struct Counter {
    harness: Harness,
    resolver: crate::lazy::Resolver,
    counts: Counts,
}

impl Counter {
    pub fn new(r: &crate::ir::SessionRef) -> Counter {
        Counter {
            harness: r.harness,
            resolver: crate::lazy::Resolver::new(Some(r.path.clone())),
            counts: Counts::new(),
        }
    }

    pub fn note(&mut self, m: &Message) -> Result<()> {
        for hit in hits(self.harness, m, &self.resolver, false)? {
            let (Hit::Call { kind, .. } | Hit::Message { kind, .. } | Hit::Detail { kind, .. }) = hit;
            *self.counts.entry(kind).or_default() += 1;
        }
        Ok(())
    }

    /// What the messages so far showed (a resumable pass's checkpoint keeps it).
    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    /// Continue from the counts a checkpoint kept.
    pub fn restore(&mut self, counts: Counts) {
        self.counts = counts;
    }

    /// The whole session's counts: what the messages showed, the session's own rows
    /// ([`session_rows`]), and `store` (the adapter's side-store rows).
    pub fn finish(mut self, s: &Session, store: &[Affordance]) -> Counts {
        for a in session_rows(s).iter().chain(store) {
            *self.counts.entry(a.kind).or_default() += 1;
        }
        self.counts
    }
}

/// The transcript rows plus the adapter's side-store rows, for one session ref.
pub fn read(adapter: &dyn crate::harness::Adapter, r: &crate::ir::SessionRef) -> anyhow::Result<Vec<Affordance>> {
    let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
    let mut messages = Vec::new();
    let mut index = 0;
    let mut error = None;
    // Request classification facts and tool details while reducing complete content spans into
    // bounded summaries. Only the resulting rows survive each streamed message.
    let s = crate::stream::stream_counted(
        adapter,
        r,
        &crate::stream::Demand::FACTS.with_spans(),
        &mut |m: Message| {
            match message_rows(r.harness, &m, index, &resolver) {
                Ok(rows) => messages.extend(rows),
                Err(failure) => {
                    error = Some(failure);
                    return Flow::Stop;
                }
            }
            index += 1;
            Flow::Continue
        },
        crate::stream::Pass::RECORD,
    )?
    .session;
    if let Some(error) = error {
        return Err(error);
    }
    // Session facts can arrive at EOF; they still precede message rows, exactly as of_session.
    let mut rows = session_rows(&s);
    rows.extend(messages);
    rows.extend(adapter.store_affordances(r)?);
    Ok(rows)
}

/// Parse a `--kind` list of affordance words (`affordances` is all of them). `None` when any word
/// is not one.
pub fn parse_kinds(words: &[&str]) -> Option<HashSet<Kind>> {
    let mut set = HashSet::new();
    for w in words {
        if *w == "affordances" {
            set.extend(Kind::ALL.iter().copied());
        } else {
            set.insert(Kind::parse(w)?);
        }
    }
    Some(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The old whole-session oracle includes EOF session facts, inline summaries beyond the lazy
    /// prefix, result details and side-store rows. Streaming must preserve all of those in order.
    #[test]
    fn leftovers_roo_mixed_return_keeps_prompt_and_counts_once() -> Result<()> {
        let mut call = Message::new(Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "child".into(),
            name: "new_task".into(),
            input: json!({}).into(),
            namespace: None,
        });
        let mut mixed = Message::new(Role::User);
        mixed.content = vec![
            Block::ToolResult {
                tool_use_id: "child".into(),
                content: "child answer".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
            Block::Text {
                text: "environment metadata".into(),
            },
        ];
        let mut open = std::collections::HashMap::new();
        refine_spawn(Harness::Roo, &mut call, &mut open)?;
        refine_spawn(Harness::Roo, &mut mixed, &mut open)?;
        assert!(open.is_empty(), "mixed returns consume their matching pending ID");
        assert_eq!((mixed.kind, mixed.origin), (MessageKind::Prompt, Origin::Human));
        let resolver = crate::lazy::Resolver::new(None);
        let rows = message_rows(Harness::Roo, &mixed, 1, &resolver)?;
        assert_eq!(rows.iter().filter(|r| r.kind == Kind::SubagentReturn).count(), 1);
        assert_eq!(rows[0].tool_use_id.as_deref(), Some("child"));
        let r = crate::SessionRef::new("roo", Harness::Roo, "unused");
        let mut counter = Counter::new(&r);
        counter.note(&call)?;
        counter.note(&mixed)?;
        assert_eq!(
            counter.finish(&Session::new("roo", Harness::Roo), &[])[&Kind::SubagentReturn],
            1
        );
        Ok(())
    }

    #[test]
    fn leftovers_claude_slash_skill_body_is_not_a_second_tool_activation() -> Result<()> {
        let mut body = Message::of_kind(Role::User, MessageKind::InjectedContext, Origin::Harness);
        body.content.push(Block::Text {
            text: "Base directory for this skill: /skills/test\n\nDo the task".into(),
        });
        let resolver = crate::lazy::Resolver::new(None);
        assert_eq!(message_rows(Harness::Claude, &body, 0, &resolver)?[0].kind, Kind::Skill);
        body.harness_extra_mut(Harness::Claude)
            .insert("sourceToolUseID".into(), json!("tool-call"));
        assert!(message_rows(Harness::Claude, &body, 0, &resolver)?.is_empty());
        body.kind = MessageKind::Prompt;
        body.origin = Origin::Human;
        body.extra.clear();
        assert!(
            message_rows(Harness::Claude, &body, 0, &resolver)?.is_empty(),
            "literal human prose is not a skill activation"
        );
        Ok(())
    }

    #[test]
    fn streamed_read_matches_full_session_and_side_store() {
        let _world = crate::test_world("affordance-stream");
        struct Fixture {
            text_path: std::path::PathBuf,
        }
        impl crate::harness::Adapter for Fixture {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &crate::SessionRef,
                opts: &crate::Demand,
                sink: &mut dyn MessageSink,
            ) -> anyhow::Result<Session> {
                assert!(
                    opts.tier >= crate::stream::Tier::Facts,
                    "the view must preserve full metadata"
                );
                let mut s = Session::new(r.id.clone(), r.harness);
                sink.meta(&s);
                // Many large irrelevant outputs must be dropped; only affordance rows survive.
                for i in 0..16 {
                    if sink.message(result(&format!("large-{i}"), &"x".repeat(256 * 1024), false, None)) == Flow::Stop {
                        return Ok(s);
                    }
                }
                let mut call = Message::new(Role::Assistant);
                call.content.push(Block::ToolUse {
                    id: "plan".into(),
                    name: "TodoWrite".into(),
                    namespace: None,
                    input: json!({"todos":[{"content":"preserve fidelity", "status":"pending"}]}).into(),
                });
                sink.message(call);
                let mut hook = Message::new(Role::System);
                hook.origin = Origin::Hook;
                let text = if opts.spans {
                    crate::Text::Span(crate::Span {
                        source: Some(self.text_path.clone()),
                        offset: 1,
                        len: std::fs::metadata(&self.text_path)?.len() - 2,
                        escaped: false,
                    })
                } else {
                    serde_json::from_str::<String>(&std::fs::read_to_string(&self.text_path)?)?.into()
                };
                hook.content.push(Block::Text { text });
                sink.message(hook);
                sink.message(result("denied", "Permission to use Bash has been denied", true, None));
                sink.message(result(
                    "checkpoint",
                    "output",
                    false,
                    Some(json!({"restorable":["/tmp/result.txt"]})),
                ));
                s.lineage.forked_from = Some("parent-late".into());
                note_mode(s.harness_extra_mut(Harness::Claude), "plan");
                sink.meta(&s);
                Ok(s)
            }
            fn store_affordances(&self, _: &crate::SessionRef) -> anyhow::Result<Vec<Affordance>> {
                Ok(vec![Affordance::store(
                    Kind::Plan,
                    "todo.json",
                    "side store",
                    Vec::new(),
                )])
            }
        }
        let r = crate::SessionRef::new(
            "affordance-stream",
            Harness::Claude,
            "/nonexistent/affordance-stream.jsonl",
        );
        let text_path = std::env::temp_dir().join(format!("cv-affordance-stream-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &text_path,
            // A JSON string: the span covers its body.
            format!("\"<summary>{}late closing tag</summary>\"", " ".repeat(8192)),
        )
        .unwrap();
        let fixture = Fixture {
            text_path: text_path.clone(),
        };
        let adapter: &dyn crate::harness::Adapter = &fixture;
        let full = adapter.parse(&r).unwrap();
        let mut expected = of_session(&full).unwrap();
        expected.extend(adapter.store_affordances(&r).unwrap());
        let actual = read(adapter, &r).unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(actual.first().unwrap().kind, Kind::Fork);
        assert_eq!(actual.last().unwrap().source, Source::Store);
        assert!(actual.iter().any(|a| a.summary == "late closing tag"));
        assert!(actual.iter().any(|a| a.kind == Kind::Checkpoint));
        std::fs::remove_file(text_path).unwrap();
    }

    #[test]
    fn source_text_rows_and_counts_propagate_demands_atomically() {
        struct Fixture(Message);
        impl crate::harness::Adapter for Fixture {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &crate::SessionRef,
                _: &crate::Demand,
                sink: &mut dyn crate::MessageSink,
            ) -> Result<Session> {
                sink.try_message(self.0.clone())?;
                Ok(Session::new(&r.id, r.harness))
            }
        }
        let world = crate::test_world("affordance-source-text-rows");
        let path = world.base.join("original");
        for case in 0..3 {
            std::fs::write(&path, "snapshot provenance").unwrap();
            let body = match case {
                0 => format!(
                    "{}<task-notification><summary>finished🌙</summary></task-notification>",
                    "\u{2003}".repeat(30000)
                ),
                1 => format!(
                    "{}Permission to use Bash {} denied",
                    "\n".repeat(70000),
                    "x".repeat(70000)
                ),
                _ => format!("{}<summary>returned🌙</summary>", "x".repeat(70000)),
            };
            let capture = crate::json::JsonSource::capture(&path).unwrap();
            let backing = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
            let wire = serde_json::to_vec(&body).unwrap();
            let (offset, len) = backing
                .append(|out| {
                    out.write_all(&wire)?;
                    Ok(())
                })
                .unwrap();
            let text = capture
                .read(ReadInput::Backing(backing, offset, len), Validation::Value, |view| {
                    view.source_text()
                })
                .unwrap()
                .unwrap();
            let make = |text| {
                let mut message = Message::new(Role::Assistant);
                match case {
                    0 => {
                        message.kind = MessageKind::InjectedContext;
                        message.content.push(Block::Text { text });
                    }
                    1 => message.content.push(Block::ToolResult {
                        tool_use_id: "call".into(),
                        tool_name: None,
                        content: text,
                        is_error: true,
                        status: None,
                        details: None,
                    }),
                    _ => {
                        message.kind = MessageKind::SubagentReturn;
                        message.content.push(Block::Text { text });
                    }
                }
                message
            };
            let lazy_message = make(text);
            let mut lazy = Session::new("source-text", Harness::Claude);
            lazy.messages.push(lazy_message.clone());
            let mut owned = Session::new("source-text", Harness::Claude);
            owned.messages.push(make(body.into()));
            assert_eq!(
                serde_json::to_value(of_session(&lazy).unwrap()).unwrap(),
                serde_json::to_value(of_session(&owned).unwrap()).unwrap()
            );
            let reference = crate::SessionRef::new("source-text", Harness::Claude, &path);
            let mut counter = Counter::new(&reference);
            counter.note(&lazy_message).unwrap();
            let previous = counter.counts().clone();
            assert_eq!(previous.values().sum::<u32>(), 1);
            std::fs::write(&path, "changed snapshot").unwrap();
            assert!(of_session(&lazy).is_err());
            assert!(read(&Fixture(lazy_message.clone()), &reference).is_err());
            if case < 2 {
                assert!(counter.note(&lazy_message).is_err());
                assert_eq!(counter.counts(), &previous);
            } else {
                // Counted kind comes from metadata; summaries alone demand this text.
                counter.note(&lazy_message).unwrap();
            }
        }
    }

    #[test]
    fn source_arguments_match_owned_rows_and_fail_atomically() {
        let path = std::env::temp_dir().join(format!("cv-affordance-input-{}", uuid::Uuid::new_v4()));
        for value in [
            json!({"todos": {"b": {"content":"  keep spacing  ","status":null,"state":"done"}, "a":"  plain  "}, "ignored":"x".repeat(1024 * 1024)}),
            json!({"taskId":17,"subject":"λ".repeat(121),"status":"pending","ignored":"x".repeat(1024 * 1024)}),
            json!({"todos":"[{\"content\":\"a\",\"status\":\"done\"}]"}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            let source = Json::source(&path, 0, bytes.len() as u64).unwrap();
            let make = |input| {
                let mut session = Session::new("argument-oracle", Harness::Claude);
                let mut message = Message::new(Role::Assistant);
                message.content.push(Block::ToolUse {
                    id: "call".into(),
                    name: "TodoWrite".into(),
                    namespace: None,
                    input,
                });
                session.messages.push(message);
                session
            };
            let owned = make(value.into());
            let lazy = make(source);
            assert_eq!(
                serde_json::to_vec(&of_session(&owned).unwrap()).unwrap(),
                serde_json::to_vec(&of_session(&lazy).unwrap()).unwrap()
            );
            std::fs::remove_file(&path).unwrap();
            assert!(of_session(&lazy).is_err());
            let reference = crate::SessionRef::new("argument-oracle", Harness::Claude, &path);
            let mut counter = Counter::new(&reference);
            assert!(counter.note(&lazy.messages[0]).is_err());
            assert!(counter.finish(&lazy, &[]).is_empty());
        }
    }

    #[test]
    fn deferred_source_child_remains_lazy_and_failure_stops_delivery() {
        const NAME_ARGUMENTS: DeferredWrapper = DeferredWrapper {
            name_key: "name",
            args_key: "arguments",
        };
        let path = std::env::temp_dir().join(format!("cv-deferred-source-{}", uuid::Uuid::new_v4()));
        let wrapped = json!({"name":"delegate_task", "arguments":{"prompt":"λ".repeat(400_000),"id":7}});
        let raw = serde_json::to_vec(&wrapped).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let source = Json::source(&path, 0, raw.len() as u64).unwrap();
        let mut block = Block::ToolUse {
            id: "c".into(),
            name: DEFERRED.into(),
            namespace: None,
            input: source.clone(),
        };
        unwrap_deferred(NAME_ARGUMENTS, &mut block).unwrap();
        let Block::ToolUse {
            input, name, namespace, ..
        } = &block
        else {
            unreachable!()
        };
        assert!(matches!(input, Json::Source(_)));
        assert_eq!(name, "delegate_task");
        assert_eq!(namespace.as_deref(), Some(DEFERRED));
        let replay = rewrap_deferred(NAME_ARGUMENTS, &block).unwrap().unwrap();
        let Block::ToolUse { input, .. } = replay else {
            unreachable!()
        };
        assert_eq!(input.resolve().unwrap().as_ref(), &wrapped);
        std::fs::remove_file(path).unwrap();
        let mut delivered = 0;
        let mut inner = |_: Message| {
            delivered += 1;
            Flow::Continue
        };
        let mut sink = AffordanceSink::new(Harness::Hermes, &mut inner).unwrapping(Some(NAME_ARGUMENTS));
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "c".into(),
            name: DEFERRED.into(),
            namespace: None,
            input: source,
        });
        assert_eq!(sink.message(message), Flow::Stop);
        assert!(sink.take_error().is_some());
        assert_eq!(sink.message(Message::new(Role::Assistant)), Flow::Stop);
        drop(sink);
        assert_eq!(delivered, 0);
    }

    #[test]
    fn bounded_composed_summaries_match_original_owned_codec() {
        let path = std::env::temp_dir().join(format!("cv-summary-oracle-{}", uuid::Uuid::new_v4()));
        for value in [
            json!({"status":"λ\n".repeat(80),"taskId":"x".repeat(119)}),
            json!({"status":"done","taskId":false,"task_id":17}),
            json!({"description":"λ\n".repeat(100),"subject":"second"}),
            json!({"description":"","subject":"λ".repeat(121)}),
            json!({"questions":[{"question":""}],"tasks":["  λ  "],"status":"x".repeat(200)}),
            json!({"questions":[{"question":"first"}],"tasks":[{"content":" ","subject":"masked"}],"status":"done"}),
            json!({"tasks":[format!("{}λ{}", " ".repeat(80_000), "\n".repeat(80_000))]}),
            json!({"tasks":[{"content":"λ".repeat(121),"status":"ignored"}],"status":"tail"}),
            json!({"id":u64::MAX,"taskId":"later","status":null}),
            json!({"command":[],"prompt":null,"questions":[],"status":"pending"}),
        ] {
            let raw = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &raw).unwrap();
            let source = Json::source(&path, 0, raw.len() as u64).unwrap();
            for kind in [Kind::Plan, Kind::Schedule, Kind::Approval] {
                let expected = owned_summary_oracle(kind, &value, &[]);
                assert_eq!(call_summary(kind, JsonView::from(&value), &[]).unwrap(), expected);
                assert_eq!(
                    source.with_view(|view| call_summary(kind, view, &[])).unwrap(),
                    expected
                );
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    fn owned_summary_oracle(kind: Kind, input: &Value, items: &[Item]) -> String {
        if !items.is_empty() {
            return items_summary(items);
        }
        // An item-wise update: which item, and what it became.
        if let Some(status) = input
            .get("status")
            .and_then(Value::as_str)
            .filter(|_| kind == Kind::Plan)
        {
            let which = ["taskId", "task_id", "id", "subject"].iter().find_map(|k| {
                input.get(*k).and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
            });
            if let Some(which) = which {
                return crate::ir::truncate(&format!("{which} → {status}"), 120);
            }
        }
        const KEYS: [&str; 16] = [
            "description",
            "subject",
            "prompt",
            "command",
            "cron",
            "schedule",
            "skill",
            "name",
            "question",
            "goal",
            "objective",
            "instructions",
            "message",
            "reason",
            "id",
            "taskId",
        ];
        let mut parts = Vec::new();
        for k in KEYS {
            if let Some(s) = input.get(k).and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            }) {
                parts.push(s);
                if parts.len() == 2 || kind != Kind::Schedule {
                    break;
                }
            }
        }
        if parts.is_empty() {
            if let Some(q) = input.get("questions").and_then(Value::as_array).and_then(|a| a.first()) {
                if let Some(s) = q.get("question").and_then(Value::as_str) {
                    parts.push(s.to_string());
                }
            }
            if let Some(t) = input.get("tasks").and_then(Value::as_array).and_then(|a| a.first()) {
                if let Some(s) = item_of(t) {
                    parts.push(s.content);
                }
            }
            if let Some(s) = input.get("status").and_then(Value::as_str) {
                parts.push(s.to_string());
            }
        }
        crate::ir::truncate(&parts.join(" · "), 120)
    }

    #[test]
    fn every_todo_shape_normalizes_to_items() {
        let todo = json!({"todos": [{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}]});
        assert_eq!(items_of(&todo).len(), 2);
        let plan = json!({"plan": [{"step": "a", "status": "in_progress"}]});
        assert_eq!(items_of(&plan)[0].content, "a");
        let md = json!({"todos": "[x] one\n[ ] two\n[-] three"});
        let it = items_of(&md);
        assert_eq!(
            it.iter().map(|i| i.status.as_deref()).collect::<Vec<_>>(),
            [Some("completed"), Some("pending"), Some("in_progress")]
        );
        let goose = json!({"content": "1. Read a.txt\n2. Read b.txt"});
        assert_eq!(items_of(&goose).len(), 2);
        assert_eq!(items_summary(&items_of(&todo)), "2 items: 1 completed, 1 pending");
    }

    /// Each harness's `tool_call` spelling unwraps to the inner call and wraps back unchanged.
    #[test]
    fn deferred_calls_unwrap_and_rewrap_per_harness() {
        for (h, wrapped) in [
            (
                Harness::Hermes,
                json!({"name": "mcp__corpus__add", "arguments": {"a": 2}}),
            ),
            (
                Harness::OpenClaw,
                json!({"id": "sessions_spawn", "args": {"task": "ok"}}),
            ),
        ] {
            // Each adapter states its own spelling; Hermes is absent from a build without SQLite.
            let wrapper = crate::harness::rootless(h).and_then(|a| a.capabilities().deferred_wrapper);
            #[cfg(not(feature = "sqlite"))]
            if h == Harness::Hermes {
                assert_eq!(wrapper, None);
                continue;
            }
            let wrapper = wrapper.unwrap_or_else(|| panic!("{h} names its deferred wrapper"));
            let orig = Block::ToolUse {
                id: "c".into(),
                name: DEFERRED.into(),
                input: wrapped.clone().into(),
                namespace: None,
            };
            let mut b = orig.clone();
            unwrap_deferred(wrapper, &mut b).unwrap();
            let Block::ToolUse { name, namespace, .. } = &b else {
                unreachable!()
            };
            assert_ne!(name, DEFERRED);
            assert_eq!(namespace.as_deref(), Some(DEFERRED));
            let Some(Block::ToolUse { name, input, .. }) = rewrap_deferred(wrapper, &b).unwrap() else {
                panic!("rewraps")
            };
            assert_eq!(name, DEFERRED);
            assert_eq!(input.resolve().unwrap().as_ref(), &wrapped);
        }
        // Grok `plan.json` keys its todos by id.
        let grok = json!({"todos": {"t1": {"content": "a", "status": "pending"}}});
        assert_eq!(items_of(&grok).len(), 1);
        assert!(plan_state(Kind::Plan, "plan.json", items_of(&json!({"todos": {}}))).is_none());
    }

    #[test]
    fn names_are_scoped_where_they_collide() {
        let none = json!({});
        assert_eq!(
            classify_tool(Harness::OpenCode, "task", &none),
            Some((Kind::SubagentSpawn, "spawn"))
        );
        assert_eq!(classify_tool(Harness::Claude, "task", &none), None);
        assert_eq!(
            classify_tool(Harness::Qwen, "task_create", &none),
            Some((Kind::Plan, "create"))
        );
        assert_eq!(classify_tool(Harness::Claude, "Bash", &json!({"command": "ls"})), None);
        assert_eq!(
            classify_tool(Harness::Claude, "Bash", &json!({"run_in_background": true})),
            Some((Kind::BackgroundJob, "start"))
        );
        assert_eq!(
            classify_tool(Harness::Hermes, "cronjob", &json!({"action": "create"})),
            Some((Kind::Schedule, "create"))
        );
        assert_eq!(
            classify_tool(Harness::KimiCode, "TaskList", &none),
            Some((Kind::BackgroundJob, "read"))
        );
        assert_eq!(
            classify_tool(Harness::Claude, "TaskList", &none),
            Some((Kind::Plan, "read"))
        );
    }

    fn result(id: &str, text: &str, is_error: bool, details: Option<Value>) -> Message {
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: id.into(),
            content: text.into(),
            is_error,
            tool_name: Some("Bash".into()),
            status: None,
            details,
        });
        m
    }

    fn kinds_actions(rows: &[Affordance]) -> Vec<(Kind, &'static str)> {
        rows.iter().map(|a| (a.kind, a.action)).collect()
    }

    /// A refusal is an approval row only as the error result its harness writes; the same words
    /// in a successful result (a file that quotes them) or from another harness are not.
    #[test]
    fn a_permission_refusal_is_a_denied_approval() {
        let mut s = Session::new("s", Harness::Claude);
        s.messages = vec![
            result("a", "Permission to use Bash has been denied because …", true, None),
            result("b", "Permission to use Bash has been denied because …", false, None),
            result(
                "c",
                "The user doesn't want to proceed with this tool use. The tool use was rejected",
                true,
                None,
            ),
        ];
        let rows = of_session(&s).unwrap();
        assert_eq!(
            kinds_actions(&rows),
            [(Kind::Approval, "deny"), (Kind::Approval, "deny")]
        );
        assert_eq!(rows[0].tool_use_id.as_deref(), Some("a"));
        s.harness = Harness::Codex;
        assert!(of_session(&s).unwrap().is_empty());
        let mut oc = Session::new("o", Harness::OpenCode);
        oc.messages = vec![result(
            "d",
            "The user has specified a rule which prevents you from using this specific tool call.",
            true,
            None,
        )];
        assert_eq!(kinds_actions(&of_session(&oc).unwrap()), [(Kind::Approval, "deny")]);
    }

    /// What a harness keeps once per session (modes, a checkpoint tally, OpenCode's ruleset and
    /// revert) are `session` rows, and the catalog counter counts them the same.
    #[test]
    fn session_facts_are_rows_the_counter_counts() {
        let mut s = Session::new("s", Harness::Claude);
        let bag = s.harness_extra_mut(Harness::Claude);
        note_mode(bag, "default");
        note_mode(bag, "plan");
        note_mode(bag, "default");
        note_checkpoints(bag, 1, ["a.txt"]);
        note_checkpoints(bag, 1, ["a.txt", "b.txt"]);
        s.messages
            .push(result("x", "ok", false, Some(json!({"restorable": ["/w/a.txt"]}))));
        let rows = of_session(&s).unwrap();
        assert_eq!(
            kinds_actions(&rows),
            [
                (Kind::Approval, "state"),
                (Kind::Checkpoint, "state"),
                (Kind::Checkpoint, "create")
            ]
        );
        assert_eq!(rows[0].summary, "mode: default → plan");
        assert_eq!(rows[1].summary, "2 checkpoints · 2 files tracked");
        assert_eq!((rows[2].index, rows[2].summary.as_str()), (Some(0), "/w/a.txt"));

        let mut counter = Counter::new(&crate::ir::SessionRef::new(
            "s",
            Harness::Claude,
            std::path::PathBuf::from("/nonexistent"),
        ));
        s.messages.iter().for_each(|m| counter.note(m).unwrap());
        let counts = counter.finish(&s, &[]);
        assert_eq!(counts.get(&Kind::Approval), Some(&1));
        assert_eq!(counts.get(&Kind::Checkpoint), Some(&2));

        let mut oc = Session::new("o", Harness::OpenCode);
        let bag = oc.harness_extra_mut(Harness::OpenCode);
        bag.insert(
            "permission".into(),
            json!([{"permission": "bash", "pattern": "rm *", "action": "deny"}]),
        );
        bag.insert(
            "revert".into(),
            json!({"messageID": "msg_1", "snapshot": "912f5c898f9e"}),
        );
        let rows = of_session(&oc).unwrap();
        assert_eq!(
            kinds_actions(&rows),
            [(Kind::Approval, "state"), (Kind::Checkpoint, "restore")]
        );
        assert_eq!(rows[0].summary, "1 rule: 1 deny");
        assert_eq!(rows[1].summary, "to msg_1 · snapshot 912f5c89");
    }

    #[test]
    fn source_backed_subagent_receipts_match_inline_across_chunks() {
        let dir = std::env::temp_dir().join(format!("cv-receipt-source-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("receipt.json");
        for marker in [false, true] {
            let mut payload = "a".repeat(65530);
            if marker {
                payload.push_str("in the background");
            } else {
                payload.push_str("finished answer");
            }
            payload.push_str(&"é\n".repeat(50000));
            std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let backed = source
                .read(ReadInput::Document, Validation::Syntax, |view| view.source_text())
                .unwrap()
                .unwrap();
            let run = |text: crate::lazy::Text| {
                let mut messages = Vec::new();
                let mut target = |m| {
                    messages.push(m);
                    Flow::Continue
                };
                let mut sink = AffordanceSink::new(Harness::OpenCode, &mut target);
                let mut call = Message::new(Role::Assistant);
                call.content.push(Block::ToolUse {
                    id: "call".into(),
                    name: "subagent".into(),
                    input: serde_json::json!({}).into(),
                    namespace: None,
                });
                sink.try_message(call).unwrap();
                let mut result = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
                result.content.push(Block::ToolResult {
                    tool_use_id: "call".into(),
                    content: text,
                    is_error: false,
                    tool_name: Some("subagent".into()),
                    status: None,
                    details: None,
                });
                sink.try_message(result).unwrap();
                drop(sink);
                messages.into_iter().map(|m| (m.kind, m.origin)).collect::<Vec<_>>()
            };
            assert_eq!(run(backed), run(payload.into()));
        }
        // Stale retained backing is an error, rather than silently classifying the result.
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let backed = source
            .read(ReadInput::Document, Validation::Syntax, |view| view.source_text())
            .unwrap()
            .unwrap();
        std::fs::write(&path, b"\"changed\"").unwrap();
        let crate::lazy::Text::Source(backed) = backed else {
            panic!("large receipt backing")
        };
        assert!(source_contains(&backed, "in the background").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn spawn_sink_marks_the_call_and_a_synchronous_return() {
        let mut got = Vec::new();
        let mut inner = |m: Message| {
            got.push((m.kind, m.origin));
            Flow::Continue
        };
        let mut sink = AffordanceSink::new(Harness::Hermes, &mut inner);
        for (call, result) in [("c1", "{\"answer\": \"ok\"}"), ("c2", "{\"status\": \"dispatched\"}")] {
            let mut reply = Message::new(Role::Assistant);
            reply.content.push(Block::ToolUse {
                id: call.into(),
                name: "delegate_task".into(),
                input: json!({}).into(),
                namespace: None,
            });
            sink.message(reply);
            let mut res = Message::new(Role::Tool);
            res.content.push(Block::ToolResult {
                tool_use_id: call.into(),
                content: result.into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            });
            sink.message(res);
        }
        assert_eq!(
            got,
            [
                (MessageKind::SubagentSpawn, Origin::Model),
                (MessageKind::SubagentReturn, Origin::Subagent),
                (MessageKind::SubagentSpawn, Origin::Model),
                (MessageKind::ToolResult, Origin::Harness),
            ]
        );
    }

    #[test]
    fn codex_modern_affordances_cover_spawns_questions_cells_and_stdin() {
        assert_eq!(
            classify_tool(
                Harness::Codex,
                "spawn_agent",
                &json!({"task_name": "helper", "message": "check"})
            ),
            Some((Kind::SubagentSpawn, "spawn"))
        );
        assert_eq!(spawn_returns(Harness::Codex, "spawn_agent"), Some(Returns::Later));
        assert_eq!(
            spawn_return_id(Harness::Codex, "spawn_agent", r#"{"task_name":"/root/helper"}"#, false),
            Some("/root/helper".into())
        );
        assert_eq!(
            spawn_return_id(
                Harness::Codex,
                "spawn_agent",
                r#"{"agent_id":"019c-abc","nickname":"Zeno"}"#,
                false
            ),
            Some("019c-abc".into())
        );
        assert_eq!(spawn_return_id(Harness::Codex, "spawn_agent", "error", true), None);
        for q in ["request_user_input", "request_user_input_async"] {
            assert_eq!(
                classify_tool(Harness::Codex, q, &json!({"title": "Choose branch"})),
                Some((Kind::Question, "ask"))
            );
        }
        assert_eq!(
            classify_tool(Harness::Codex, "wait", &json!({"cell_id": "53"})),
            Some((Kind::BackgroundJob, "read"))
        );
        assert_eq!(classify_tool(Harness::Codex, "wait", &json!({"timeout_ms": 100})), None);
        assert_eq!(
            classify_tool(Harness::Codex, "write_stdin", &json!({"session_id": 42, "chars": ""})),
            Some((Kind::BackgroundJob, "read"))
        );
    }
}
