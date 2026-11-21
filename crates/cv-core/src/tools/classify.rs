//! Unified cross-harness tool classification and Codex Code Mode (`exec`) JavaScript extraction.
//!
//! All six tool consumers in `cv-core` (`events`, `affordance`, `lanes`, `distill`, `codex/hook`,
//! and `claude/hook`) share this one streaming layer over [`JsonView`]. Standard JSON tool inputs
//! project only the keys they need without materializing sibling fields; Codex Code Mode `exec`
//! scripts scan `tools.<ident>(...)` calls and patch headers in one linear pass without an AST.

use crate::affordance::{input, Item, Kind};
use crate::ir::{truncate, Harness};
use crate::json::JsonView;
use anyhow::Result;
use std::borrow::Cow;
use std::collections::HashMap;

/// Canonical cross-harness semantic operation extracted from a tool call.
/// One `Block::ToolUse` yields 1 `ToolSemantic` for standard tools, or `1..N`
/// for compound calls (`MultiEdit`, shell `apply_patch`, or Codex Code Mode `exec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSemantic<'a> {
    /// Effective tool name (`"exec_command"`, `"apply_patch"`, `"Bash"`, etc.).
    pub tool: Cow<'a, str>,
    pub op: ToolOp<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOp<'a> {
    /// Shell / process execution (`Bash`, `exec_command`, `shell_command`, `local_shell`,
    /// `run_shell_command`, `run_command`, `execute_command`, `terminal`, etc.).
    Command {
        /// Assembled command text when [`ClassifyOpts::commands`] is set and statically known.
        cmd: Option<Cow<'a, str>>,
        workdir: Option<Cow<'a, str>>,
        background: bool,
    },
    /// File mutation (`Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `apply_patch`,
    /// `write_file`, `replace`, `edit_file`, `save-file`, `str_replace_based_edit_tool`, etc.).
    FileEdit { path: Cow<'a, str>, is_full_write: bool },
    /// File / notebook / image inspection (`Read`, `read_file`, `view_image`, `view_file`, etc.).
    FileRead { path: Cow<'a, str> },
    /// Search / directory listing (`Grep`, `Glob`, `grep_search`, `glob`, `web_search`, `web__run`).
    Search {
        path: Option<Cow<'a, str>>,
        query: Option<Cow<'a, str>>,
    },
    /// Sub-agent spawn (`Agent`, `Task`, `spawn_agent`, `invoke_subagent`, `delegate_to_agent`, etc.).
    SubagentSpawn {
        target_hint: Option<Cow<'a, str>>,
        has_prompt: bool,
    },
    /// Peer / child agent message (`SendMessage`, `send_message`, `followup_task`, `send_input`).
    AgentMessage { recipient: Option<Cow<'a, str>> },
    /// Background job / PTY / cell interaction (`TaskOutput`, `TaskStop`, `KillShell`,
    /// `write_stdin`, `wait`, `wait_agent`, `interrupt_agent`, `command_status`, etc.).
    Background {
        job_id: Option<Cow<'a, str>>,
        action: BgAction,
    },
    /// Structured orchestration affordance (`Plan`, `Goal`, `Question`, `Schedule`, `Skill`).
    Affordance {
        kind: Kind,
        action: &'static str,
        summary: String,
        items: Vec<Item>,
    },
    /// Unclassified / MCP / custom tool (`mcp__*`, `clock__curr_time`, or ambiguous file-keyed tool).
    Other {
        path: Option<Cow<'a, str>>,
        summary: Option<Cow<'a, str>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgAction {
    Spawn,
    Poll,
    Interact,
    Stop,
}

/// Controls which string projections [`visit_tool_ops`] materializes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifyOpts {
    /// Materialize full command strings on [`ToolOp::Command`]. When `false`, command chunks
    /// stream only for patch header detection and `cmd` is `None`.
    pub commands: bool,
    /// Retain at most this many UTF-8 bytes of command text; `None` keeps it whole.
    pub command_limit: Option<usize>,
    /// Populate human-readable summaries, search queries, spawn hints, and message recipients.
    pub summaries: bool,
}

impl ClassifyOpts {
    pub const COUNTS: Self = Self {
        commands: false,
        command_limit: None,
        summaries: false,
    };
    pub const EVENTS: Self = Self {
        commands: true,
        command_limit: None,
        summaries: false,
    };
    pub const FULL: Self = Self {
        commands: true,
        command_limit: None,
        summaries: true,
    };
}

/// Name fragments that mark an unknown file-keyed tool as writing its target.
const WRITE_HINTS: &[&str] = &["edit", "write", "patch", "notebook", "replace", "create", "save"];
/// Name fragments that mark an unknown file-keyed tool as reading its target.
const READ_HINTS: &[&str] = &["read", "grep", "glob", "search", "cat", "view", "list"];

/// Input keys (across harnesses) whose string value is the tool's file target.
pub(crate) const FILE_KEYS: &[&str] = &[
    "file_path",
    "absolute_path",
    "filePath",
    "notebook_path",
    "target_file",
    "path",
    "TargetFile",
    "Path",
    "AbsolutePath",
    "AbsolutePathUri",
    "File",
];

const SEARCH_PATH_KEYS: &[&str] = &[
    "file_path",
    "absolute_path",
    "filePath",
    "notebook_path",
    "target_file",
    "path",
    "SearchPath",
    "SearchDirectory",
    "DirectoryPath",
    "dir_path",
    "Path",
    "AbsolutePath",
    "AbsolutePathUri",
];

const HEADERS: &[&str] = &["*** Update File:", "*** Add File:", "*** Delete File:", "*** Move to:"];

fn bare_tool_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

pub(crate) fn is_codex_exec_call(harness: Harness, name: &str) -> bool {
    is_codex_exec(Some(harness), bare_tool_name(name))
}

fn is_codex_exec(harness: Option<Harness>, bare: &str) -> bool {
    if bare != "exec" {
        return false;
    }
    match harness {
        None | Some(Harness::Codex) => true,
        Some(
            Harness::Claude
            | Harness::Grok
            | Harness::OpenCode
            | Harness::Gemini
            | Harness::Antigravity
            | Harness::Hermes
            | Harness::OpenClaw
            | Harness::Pi
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
            | Harness::OpenSession,
        ) => false,
    }
}

fn is_spawn_tool(harness: Option<Harness>, bare: &str) -> bool {
    if let Some(h) = harness {
        if crate::affordance::spawn_returns(h, bare).is_some() {
            return true;
        }
    }
    match harness {
        Some(Harness::Gemini | Harness::Antigravity) => {
            matches!(bare, "delegate_to_agent" | "invoke_subagent" | "invoke_agent")
        }
        Some(Harness::Cursor) => matches!(bare, "Task" | "Agent" | "mcp_task"),
        Some(Harness::KimiCode) => matches!(bare, "Agent" | "AgentSwarm"),
        Some(
            Harness::Claude
            | Harness::Codex
            | Harness::Grok
            | Harness::OpenCode
            | Harness::Hermes
            | Harness::OpenClaw
            | Harness::Pi
            | Harness::Kimi
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
            | Harness::OpenSession,
        ) => false,
        None => matches!(
            bare,
            "Agent"
                | "Task"
                | "Workflow"
                | "spawn_agent"
                | "team_spawn_teammate"
                | "delegate_to_agent"
                | "invoke_subagent"
                | "invoke_agent"
                | "delegate_task"
                | "sessions_spawn"
                | "spawn_subagent"
                | "run_subagent"
                | "Subagent"
                | "subagent"
                | "AgentSwarm"
                | "mcp_task"
                | "new_task"
        ),
    }
}

/// Whether `name` carries potentially multi-megabyte file bodies, diffs, or JS scripts that
/// consumers like `distill` must inspect via [`JsonView`] rather than eagerly materializing.
pub fn is_large_payload_tool(harness: Option<Harness>, name: &str) -> bool {
    let bare = bare_tool_name(name);
    if is_codex_exec(harness, bare) {
        return true;
    }
    let lname = bare.to_ascii_lowercase();
    lname.contains("patch")
        || matches!(
            bare,
            "Write"
                | "write"
                | "write_file"
                | "save-file"
                | "create_file"
                | "write_to_file"
                | "Edit"
                | "edit"
                | "MultiEdit"
                | "NotebookEdit"
                | "edit_file"
                | "replace"
                | "replace_file_content"
                | "multi_replace_file_content"
                | "str_replace_based_edit_tool"
                | "str_replace_editor"
        )
}

/// Primary streaming visitor: invokes `emit` for each semantic operation in a tool call.
pub fn visit_tool_ops<'a>(
    harness: Option<Harness>,
    name: &'a str,
    input: JsonView<'a>,
    opts: ClassifyOpts,
    mut emit: impl FnMut(ToolSemantic<'a>),
) -> Result<()> {
    visit_tool_ops_internal(harness, name, input, opts, false, |sem, _| emit(sem))
}

/// Streaming visitor that also extracts normalized 64-bit line-hunk fingerprints for every
/// [`ToolOp::FileEdit`] operation (used by [`crate::events`] and derived columnar event segments).
pub fn visit_tool_ops_with_hunks<'a>(
    harness: Option<Harness>,
    name: &'a str,
    input: JsonView<'a>,
    opts: ClassifyOpts,
    emit: impl FnMut(ToolSemantic<'a>, Vec<u64>),
) -> Result<()> {
    visit_tool_ops_internal(harness, name, input, opts, true, emit)
}

fn is_swarm_add(bare: &str, input: JsonView<'_>) -> Result<bool> {
    if bare != "swarm" {
        return Ok(false);
    }
    Ok(input
        .get("action")?
        .map(|v| input::word(v, &["add"]))
        .transpose()?
        .flatten()
        .is_some())
}

fn visit_tool_ops_internal<'a>(
    harness: Option<Harness>,
    name: &'a str,
    input: JsonView<'a>,
    opts: ClassifyOpts,
    extract_hunks: bool,
    mut emit: impl FnMut(ToolSemantic<'a>, Vec<u64>),
) -> Result<()> {
    let bare = bare_tool_name(name);

    // 1. Codex Code Mode `exec` (`custom_tool_call` raw JS string or `{"raw": "..."}`).
    if is_codex_exec(harness, bare) {
        let script = if input.is_string()? {
            input::string(input)?
        } else if input.get("command")?.is_none() && input.get("cmd")?.is_none() {
            match input.get("raw")? {
                Some(raw) if raw.is_string()? => input::string(raw)?,
                _ => None,
            }
        } else {
            None
        };
        if let Some(script) = script {
            scan_codex_exec(&script, opts, extract_hunks, &mut |sem, hunks| {
                emit(sem.into_owned(), hunks)
            });
            return Ok(());
        }
    }

    let workdir = file_key(input, &["workdir", "cwd", "Cwd"])?;

    // 2. Orchestration tools: sub-agent spawns, peer/child messages, and background polling/stopping.
    let spawn_by_affordance = match harness {
        Some(h) => crate::affordance::classify_tool_view(h, bare, input)?
            .is_some_and(|(kind, _)| kind == crate::affordance::Kind::SubagentSpawn),
        None => is_swarm_add(bare, input)?,
    };
    if is_spawn_tool(harness, bare) || spawn_by_affordance {
        let target_hint = if opts.summaries {
            first_string_prefix(
                input,
                &[
                    "description",
                    "task_name",
                    "Role",
                    "subagent_type",
                    "TypeName",
                    "agent_name",
                    "name",
                ],
                120,
            )?
            .map(Cow::Owned)
        } else {
            None
        };
        let has_prompt = if opts.summaries {
            has_nonempty_string(input, &["prompt", "message", "Prompt", "task", "instructions", "brief"])?
        } else {
            false
        };
        emit(
            ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::SubagentSpawn {
                    target_hint,
                    has_prompt,
                },
            },
            Vec::new(),
        );
        return Ok(());
    }

    if matches!(
        bare,
        "SendMessage" | "send_message" | "followup_task" | "send_input" | "team_send_message" | "post_message"
    ) {
        let recipient = if opts.summaries {
            match first_string_prefix(
                input,
                &["to", "target", "Recipient", "recipient", "agent_id", "id"],
                120,
            )? {
                Some(r) => Some(Cow::Owned(r)),
                None if bare == "post_message" => Some(Cow::Borrowed("board")),
                None => None,
            }
        } else {
            None
        };
        emit(
            ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::AgentMessage { recipient },
            },
            Vec::new(),
        );
        return Ok(());
    }

    if let Some(action) = classify_bg_tool(harness, bare, input)? {
        let job_id = if opts.summaries {
            first_scalar_prefix(
                input,
                &[
                    "cell_id",
                    "session_id",
                    "task_id",
                    "shell_id",
                    "bash_id",
                    "CommandId",
                    "TaskId",
                    "job_id",
                    "id",
                ],
                80,
            )?
            .map(Cow::Owned)
        } else {
            None
        };
        emit(
            ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::Background { job_id, action },
            },
            Vec::new(),
        );
        return Ok(());
    }

    // 3. Patch tools (`apply_patch` and `*patch*`).
    let lname = bare.to_ascii_lowercase();
    if lname.contains("patch") {
        let files = patch_rows(input, extract_hunks)?;
        if !files.is_empty() {
            for (file, hunks) in files {
                let path = resolve_workdir(&file, workdir.as_deref());
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed(name),
                        op: ToolOp::FileEdit {
                            path: Cow::Owned(path),
                            is_full_write: false,
                        },
                    },
                    hunks,
                );
            }
            return Ok(());
        }
        if bare == "apply_patch" {
            let summary = if opts.summaries {
                fallback_summary(input, 80)?.map(Cow::Owned)
            } else {
                None
            };
            emit(
                ToolSemantic {
                    tool: Cow::Borrowed(name),
                    op: ToolOp::Other { path: None, summary },
                },
                Vec::new(),
            );
            return Ok(());
        }
    }

    // 4. Dedicated search, directory, and URL inspection tools.
    if matches!(
        bare,
        "Grep"
            | "grep"
            | "Glob"
            | "glob"
            | "find"
            | "ls"
            | "grep_search"
            | "find_by_name"
            | "list_dir"
            | "search_web"
            | "web_search"
            | "read_url_content"
    ) {
        let path = file_key(input, SEARCH_PATH_KEYS)?.map(|p| Cow::Owned(resolve_workdir(&p, workdir.as_deref())));
        let query = if opts.summaries {
            first_string_prefix(
                input,
                &["pattern", "query", "Query", "Pattern", "url", "Url", "domain"],
                120,
            )?
            .map(Cow::Owned)
        } else {
            None
        };
        emit(
            ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::Search { path, query },
            },
            Vec::new(),
        );
        return Ok(());
    }

    // 5. File-keyed tools (preserving `events.rs` exact key-masking precedence).
    if let Some(raw_path) = file_key(input, FILE_KEYS)? {
        let path = Cow::Owned(resolve_workdir(&raw_path, workdir.as_deref()));
        let is_editor_view = matches!(bare, "str_replace_based_edit_tool" | "str_replace_editor")
            && input
                .get("command")?
                .map(|v| input::word(v, &["view"]))
                .transpose()?
                .flatten()
                .is_some();
        let (op, hunks) = if is_editor_view {
            (ToolOp::FileRead { path }, Vec::new())
        } else if matches!(
            bare,
            "Write" | "write_file" | "save-file" | "create_file" | "write_to_file"
        ) || lname == "write"
        {
            let is_full_write = !(bare == "write_file" && (bool_flag(input, "Append")? || bool_flag(input, "append")?));
            let hunks = if extract_hunks {
                extract_file_edit_hunks(input, is_full_write)?
            } else {
                Vec::new()
            };
            (ToolOp::FileEdit { path, is_full_write }, hunks)
        } else if matches!(
            bare,
            "Edit"
                | "edit"
                | "MultiEdit"
                | "NotebookEdit"
                | "replace"
                | "edit_file"
                | "replace_file_content"
                | "multi_replace_file_content"
                | "str_replace_based_edit_tool"
                | "str_replace_editor"
        ) || WRITE_HINTS.iter().any(|hint| lname.contains(hint))
        {
            let hunks = if extract_hunks {
                extract_file_edit_hunks(input, false)?
            } else {
                Vec::new()
            };
            (
                ToolOp::FileEdit {
                    path,
                    is_full_write: false,
                },
                hunks,
            )
        } else if matches!(
            bare,
            "Read"
                | "read"
                | "NotebookRead"
                | "read_file"
                | "view_file"
                | "view_file_outline"
                | "view_code_item"
                | "view_image"
                | "read_many_files"
        ) || READ_HINTS.iter().any(|hint| lname.contains(hint))
        {
            (ToolOp::FileRead { path }, Vec::new())
        } else {
            (
                ToolOp::Other {
                    path: Some(path),
                    summary: None,
                },
                Vec::new(),
            )
        };
        emit(
            ToolSemantic {
                tool: Cow::Borrowed(name),
                op,
            },
            hunks,
        );
        return Ok(());
    }

    // 6. Command tools (`command` / `cmd` / `script` / `CommandLine` / `action.command`).
    if let Some((command, files)) = command_rows_bounded(input, opts.commands, extract_hunks, opts.command_limit)? {
        let background = bool_flag(input, "run_in_background")?
            || bool_flag(input, "is_background")?
            || bool_flag(input, "background")?
            || bool_flag(input, "IsDaemon")?;
        emit(
            ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::Command {
                    cmd: command.map(Cow::Owned),
                    workdir: workdir.clone().map(Cow::Owned),
                    background,
                },
            },
            Vec::new(),
        );
        for (file, hunks) in files {
            let resolved = resolve_workdir(&file, workdir.as_deref());
            emit(
                ToolSemantic {
                    tool: Cow::Borrowed(name),
                    op: ToolOp::FileEdit {
                        path: Cow::Owned(resolved),
                        is_full_write: false,
                    },
                },
                hunks,
            );
        }
        return Ok(());
    }

    // 7. Structured affordance tools when harness is known.
    if let Some(h) = harness {
        if let Some((kind, action)) = crate::affordance::classify_tool_view(h, name, input)? {
            let summary = if opts.summaries {
                match first_string_prefix(
                    input,
                    &[
                        "explanation",
                        "objective",
                        "progress_summary",
                        "skill",
                        "subject",
                        "title",
                        "prompt",
                        "Prompt",
                        "question",
                        "CronExpression",
                        "cron",
                    ],
                    120,
                )? {
                    Some(s) => s,
                    None => match first_question_prefix(input, 120)? {
                        Some(q) => q,
                        None => {
                            first_scalar_prefix(input, &["DurationSeconds", "name", "model"], 120)?.unwrap_or_default()
                        }
                    },
                }
            } else {
                String::new()
            };
            emit(
                ToolSemantic {
                    tool: Cow::Borrowed(name),
                    op: ToolOp::Affordance {
                        kind,
                        action,
                        summary,
                        items: Vec::new(),
                    },
                },
                Vec::new(),
            );
            return Ok(());
        }
    }

    // 8. Fallback for unclassified / MCP tools.
    let summary = if opts.summaries {
        fallback_summary(input, 80)?.map(Cow::Owned)
    } else {
        None
    };
    emit(
        ToolSemantic {
            tool: Cow::Borrowed(name),
            op: ToolOp::Other { path: None, summary },
        },
        Vec::new(),
    );
    Ok(())
}

impl ToolSemantic<'_> {
    pub fn into_owned(self) -> ToolSemantic<'static> {
        ToolSemantic {
            tool: Cow::Owned(self.tool.into_owned()),
            op: match self.op {
                ToolOp::Command {
                    cmd,
                    workdir,
                    background,
                } => ToolOp::Command {
                    cmd: cmd.map(|c| Cow::Owned(c.into_owned())),
                    workdir: workdir.map(|w| Cow::Owned(w.into_owned())),
                    background,
                },
                ToolOp::FileEdit { path, is_full_write } => ToolOp::FileEdit {
                    path: Cow::Owned(path.into_owned()),
                    is_full_write,
                },
                ToolOp::FileRead { path } => ToolOp::FileRead {
                    path: Cow::Owned(path.into_owned()),
                },
                ToolOp::Search { path, query } => ToolOp::Search {
                    path: path.map(|p| Cow::Owned(p.into_owned())),
                    query: query.map(|q| Cow::Owned(q.into_owned())),
                },
                ToolOp::SubagentSpawn {
                    target_hint,
                    has_prompt,
                } => ToolOp::SubagentSpawn {
                    target_hint: target_hint.map(|t| Cow::Owned(t.into_owned())),
                    has_prompt,
                },
                ToolOp::AgentMessage { recipient } => ToolOp::AgentMessage {
                    recipient: recipient.map(|r| Cow::Owned(r.into_owned())),
                },
                ToolOp::Background { job_id, action } => ToolOp::Background {
                    job_id: job_id.map(|j| Cow::Owned(j.into_owned())),
                    action,
                },
                ToolOp::Affordance {
                    kind,
                    action,
                    summary,
                    items,
                } => ToolOp::Affordance {
                    kind,
                    action,
                    summary,
                    items,
                },
                ToolOp::Other { path, summary } => ToolOp::Other {
                    path: path.map(|p| Cow::Owned(p.into_owned())),
                    summary: summary.map(|s| Cow::Owned(s.into_owned())),
                },
            },
        }
    }
}

fn classify_bg_tool(harness: Option<Harness>, bare: &str, input: JsonView<'_>) -> Result<Option<BgAction>> {
    let codex_wait = match harness {
        None | Some(Harness::Codex) => bare == "wait" && input.get("cell_id")?.is_some(),
        Some(
            Harness::Claude
            | Harness::Grok
            | Harness::OpenCode
            | Harness::Gemini
            | Harness::Antigravity
            | Harness::Hermes
            | Harness::OpenClaw
            | Harness::Pi
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
            | Harness::OpenSession,
        ) => false,
    };
    if codex_wait {
        return Ok(Some(BgAction::Poll));
    }
    Ok(match bare {
        "TaskOutput"
        | "BashOutput"
        | "CheckBackgroundJob"
        | "WaitFor"
        | "get_command_or_subagent_output"
        | "command_status"
        | "wait_agent"
        | "bg_output" => Some(BgAction::Poll),
        "write_stdin" => Some(BgAction::Interact),
        "send_command_input" => Some(if bool_flag(input, "Terminate")? || bool_flag(input, "terminate")? {
            BgAction::Stop
        } else {
            BgAction::Interact
        }),
        "manage_task" => {
            let action_view = match input.get("Action")? {
                Some(v) => Some(v),
                None => input.get("action")?,
            };
            let action = match action_view
                .map(|v| input::word(v, &["kill", "send_input", "list", "status"]))
                .transpose()?
                .flatten()
            {
                Some(0) => BgAction::Stop,
                Some(1) => BgAction::Interact,
                _ => BgAction::Poll,
            };
            Some(action)
        }
        "TaskStop" | "KillShell" | "kill_command_or_subagent" | "interrupt_agent" | "close_agent" | "bg_kill" => {
            Some(BgAction::Stop)
        }
        _ => None,
    })
}

fn lexical_clean(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    let is_abs = path.is_absolute();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    out.pop();
                } else if !is_abs {
                    out.push("..");
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    if out.as_os_str().is_empty() && !path.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Canonicalize the deepest existing ancestor of `path` (after joining relative paths onto `cwd`
/// when provided) and append any remaining non-existent lexical tail components.
///
/// Resolves OS directory symlinks (such as macOS `/var` <-> `/private/var` and `/tmp` <->
/// `/private/tmp`) identically whether the target file already exists on disk or has not been
/// created yet, and degrades cleanly to lexical normalization on targets without filesystem access
/// (`wasm32-unknown-unknown`) or relative paths with `cwd == None`.
pub fn normalize_path(path: &std::path::Path, cwd: Option<&std::path::Path>) -> std::path::PathBuf {
    if path.as_os_str().is_empty() {
        return std::path::PathBuf::new();
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base) = cwd.filter(|c| !c.as_os_str().is_empty()) {
        base.join(path)
    } else {
        return lexical_clean(path);
    };
    if !joined.is_absolute() {
        return lexical_clean(&joined);
    }
    if let Ok(canon) = std::fs::canonicalize(&joined) {
        return canon;
    }
    let cleaned = lexical_clean(&joined);
    let mut cur: &std::path::Path = &cleaned;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(name) = cur.file_name() {
        // On macOS, `/home` is an autofs/APFS symlink to `/System/Volumes/Data/home`. Skip bare
        // `/home` when its child does not exist on disk so Linux transcript paths (`/home/user/…`)
        // are not rewritten to `/System/Volumes/Data/home/user/…`.
        let macos_autofs_home = cfg!(target_os = "macos") && cur == std::path::Path::new("/home") && !tail.is_empty();
        if !macos_autofs_home {
            if let Ok(canon) = std::fs::canonicalize(cur) {
                let mut out = canon;
                for seg in tail.into_iter().rev() {
                    out.push(seg);
                }
                return out;
            }
        }
        tail.push(name);
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    if let Ok(canon) = std::fs::canonicalize(cur) {
        let mut out = canon;
        for seg in tail.into_iter().rev() {
            out.push(seg);
        }
        return out;
    }
    cleaned
}

/// String wrapper around [`normalize_path`]: absolutizes relative `p` against `cwd` (or preserves
/// trimmed relative `p` when `cwd` is `None`) and ancestor-canonicalizes absolute paths.
pub fn absolutize_path(p: &str, cwd: Option<&std::path::Path>) -> String {
    if p.is_empty() {
        return String::new();
    }
    if !std::path::Path::new(p).is_absolute() && cwd.filter(|c| !c.as_os_str().is_empty()).is_none() {
        return p.trim_start_matches("./").to_string();
    }
    normalize_path(std::path::Path::new(p), cwd)
        .to_string_lossy()
        .into_owned()
}

fn resolve_workdir(path: &str, workdir: Option<&str>) -> String {
    if std::path::Path::new(path).is_absolute() {
        return path.to_string();
    }
    match workdir {
        Some(dir) if !dir.is_empty() => {
            let trimmed = path.trim_start_matches("./");
            std::path::Path::new(dir).join(trimmed).to_string_lossy().into_owned()
        }
        _ => path.to_string(),
    }
}

fn bool_flag(view: JsonView<'_>, key: &str) -> Result<bool> {
    view.get(key)?
        .map(|v| v.as_bool())
        .transpose()
        .map(|v| v.flatten() == Some(true))
}

fn first_string_prefix(view: JsonView<'_>, keys: &[&str], cap: usize) -> Result<Option<String>> {
    for key in keys {
        let Some(value) = view.get(key)? else { continue };
        if let Some(head) = crate::projection::Head::of_string(value, cap)? {
            if !head.is_empty() {
                return Ok(Some(head));
            }
        }
    }
    Ok(None)
}

fn first_scalar_prefix(view: JsonView<'_>, keys: &[&str], cap: usize) -> Result<Option<String>> {
    for key in keys {
        let Some(value) = view.get(key)? else { continue };
        if let Some(head) = input::scalar_prefix(value, cap)? {
            if !head.is_empty() {
                return Ok(Some(head));
            }
        }
    }
    Ok(None)
}

fn has_nonempty_string(view: JsonView<'_>, keys: &[&str]) -> Result<bool> {
    Ok(first_string_prefix(view, keys, 8)?.is_some())
}

fn first_question_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    let Some(questions) = view.get("questions")?.map(JsonView::array).transpose()?.flatten() else {
        return Ok(None);
    };
    for q in questions {
        let q = q?;
        if let Some(head) = first_string_prefix(q, &["question", "Question"], cap)? {
            return Ok(Some(head));
        }
    }
    Ok(None)
}

fn fallback_summary(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    for key in [
        "command",
        "cmd",
        "CommandLine",
        "file_path",
        "path",
        "Path",
        "pattern",
        "query",
        "Query",
        "description",
        "Description",
        "ScriptPath",
        "Name",
        "prompt",
        "Prompt",
        "question",
        "message",
        "Message",
        "text",
        "url",
        "Url",
        "skill",
        "objective",
        "explanation",
    ] {
        let Some(value) = view.get(key)? else { continue };
        if let Some(head) = crate::projection::Head::of_string(value, cap)? {
            if !head.is_empty() {
                return Ok(Some(head));
            }
        }
    }
    if let Some(q) = first_question_prefix(view, cap)? {
        return Ok(Some(q));
    }
    let head = crate::projection::Head::of_json(view, cap)?;
    Ok((!head.is_empty() && head != "{}").then_some(head))
}

fn structured_compact_arg(bare: &str, input: JsonView<'_>, max: usize) -> Result<Option<Option<String>>> {
    match bare {
        "swarm" => {
            let act = first_string_prefix(input, &["action"], max)?;
            let target = first_string_prefix(input, &["name", "title", "model"], max)?;
            Ok(Some(match (act, target) {
                (Some(a), Some(t)) => Some(collapse_line(&format!("{a} {t}"), max)),
                (Some(a), None) => Some(collapse_line(&a, max)),
                (None, Some(t)) => Some(collapse_line(&t, max)),
                (None, None) => None,
            }))
        }
        "post_message" => {
            if let Some(text) = first_string_prefix(input, &["text", "message", "Message"], max)? {
                let line = collapse_line(&text, max);
                if !line.is_empty() {
                    return Ok(Some(Some(line)));
                }
            }
            Ok(None)
        }
        "manage_task" | "manage_subagents" => {
            let act = first_string_prefix(input, &["Action", "action"], max)?;
            let id = first_scalar_prefix(
                input,
                &["TaskId", "task_id", "ConversationId", "conversation_id", "id"],
                max,
            )?;
            Ok(Some(match (act, id) {
                (Some(a), Some(i)) => Some(collapse_line(&format!("{a} {i}"), max)),
                (Some(a), None) => Some(collapse_line(&a, max)),
                (None, Some(i)) => Some(collapse_line(&i, max)),
                (None, None) => None,
            }))
        }
        "read_board" => {
            if bool_flag(input, "status")? {
                return Ok(Some(Some(truncate("status", max))));
            }
            if let Some(since) = first_scalar_prefix(input, &["since"], max)? {
                return Ok(Some(Some(truncate(&format!("since {since}"), max))));
            }
            Ok(Some(None))
        }
        "swarm_wait" => {
            if let Some(since) = first_scalar_prefix(input, &["since"], max)? {
                return Ok(Some(Some(truncate(&format!("since {since}"), max))));
            }
            Ok(Some(None))
        }
        _ => Ok(None),
    }
}

/// One-line human-readable summary of a tool call (`"{name} · {arg}"`, or `"{name}"` when empty).
pub fn compact_label(harness: Option<Harness>, name: &str, input: JsonView<'_>, max: usize) -> Result<String> {
    let bare = bare_tool_name(name);
    let is_str = input.is_string()?;
    if !is_str {
        if let Some(explicit) = structured_compact_arg(bare, input, max)? {
            return Ok(match explicit {
                Some(arg) if !arg.is_empty() => format!("{name} · {arg}"),
                _ => name.to_string(),
            });
        }
    }
    let mut first_arg: Option<String> = None;
    let mut op_count = 0usize;
    visit_tool_ops(harness, name, input, ClassifyOpts::FULL, |sem| {
        op_count += 1;
        if first_arg.is_some() {
            return;
        }
        first_arg = match sem.op {
            ToolOp::Command { cmd: Some(cmd), .. } => {
                let text = collapse_line(&cmd, max);
                (!text.is_empty()).then_some(text)
            }
            ToolOp::Command {
                cmd: None,
                workdir: Some(dir),
                ..
            } => Some(truncate(&dir, max)),
            ToolOp::Command { .. } => None,
            ToolOp::FileEdit { path, .. } | ToolOp::FileRead { path } => Some(truncate(&path, max)),
            ToolOp::Search { query: Some(q), .. } => Some(collapse_line(&q, max)),
            ToolOp::Search { path: Some(p), .. } => Some(truncate(&p, max)),
            ToolOp::Search { .. } => None,
            ToolOp::SubagentSpawn {
                target_hint: Some(hint),
                ..
            } => Some(collapse_line(&hint, max)),
            ToolOp::SubagentSpawn { .. } => None,
            ToolOp::AgentMessage {
                recipient: Some(to), ..
            } => Some(truncate(&to, max)),
            ToolOp::AgentMessage { .. } => None,
            ToolOp::Background { job_id: Some(id), .. } => Some(truncate(&id, max)),
            ToolOp::Background { .. } => None,
            ToolOp::Affordance { summary, .. } if !summary.is_empty() => Some(collapse_line(&summary, max)),
            ToolOp::Affordance { .. } => None,
            ToolOp::Other { summary: Some(s), .. } => Some(collapse_line(&s, max)),
            ToolOp::Other { path: Some(p), .. } => Some(truncate(&p, max)),
            ToolOp::Other { .. } => None,
        };
    })?;
    // Preserve fast direct key lookup if an affordance/other tool had a standard descriptive field.
    if first_arg.is_none() && !is_str {
        first_arg = fallback_summary(input, max)?.map(|s| collapse_line(&s, max));
    }
    Ok(match first_arg {
        Some(arg) if !arg.is_empty() => {
            if op_count > 1 && is_codex_exec(harness, bare) {
                format!("{name} · {arg} (+{})", op_count - 1)
            } else {
                format!("{name} · {arg}")
            }
        }
        _ => name.to_string(),
    })
}

fn collapse_line(text: &str, max: usize) -> String {
    let mut head = crate::projection::Head::trimmed(max);
    head.push(text);
    head.finish()
}

/// Maximum distinct 64-bit hunk/line fingerprints emitted for one [`ToolOp::FileEdit`], keeping
/// per-row memory strictly bounded even on multi-megabyte full-file `Write` payloads.
pub const MAX_HUNK_HASHES_PER_EDIT: usize = 256;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;
const HUNK_HASH_DOMAIN: u8 = 0x01;
const LINE_HASH_DOMAIN: u8 = 0x02;

#[inline]
fn fnv_update(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[inline]
fn splitmix_finalize(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let out = x ^ (x >> 31);
    if out == 0 {
        1
    } else {
        out
    }
}

/// Deterministic 64-bit fingerprint of a single nontrivial normalized line (`line.trim()`).
/// Returns `None` for empty lines or short punctuation-only boilerplate (`{`, `}`, `};`, `);`).
pub fn hash_normalized_line(line: &str) -> Option<u64> {
    let trimmed = line.trim();
    if trimmed.len() < 3 || !trimmed.chars().any(char::is_alphanumeric) {
        return None;
    }
    let mut h = fnv_update(FNV_OFFSET_BASIS, &[LINE_HASH_DOMAIN]);
    h = fnv_update(h, trimmed.as_bytes());
    Some(splitmix_finalize(h))
}

/// Deterministic 64-bit fingerprint of a contiguous block of added/replacement lines.
/// Empty/whitespace-only lines are skipped and remaining lines are trimmed, so indentation changes,
/// rebases, cherry-picks, and amends preserve the exact hunk fingerprint.
pub fn hash_normalized_hunk<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<u64> {
    let mut h = fnv_update(FNV_OFFSET_BASIS, &[HUNK_HASH_DOMAIN]);
    let mut any = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if any {
            h = fnv_update(h, b"\n");
        }
        h = fnv_update(h, trimmed.as_bytes());
        any = true;
    }
    any.then(|| splitmix_finalize(h))
}

/// Bounded streaming accumulator of 64-bit hunk-block and nontrivial-line fingerprints.
/// Retains only the current in-flight line fragment (`<= 8 KiB` effective prefix) and up to
/// [`MAX_HUNK_HASHES_PER_EDIT`] deduplicated `u64` hashes.
#[derive(Default)]
struct HunkCollector {
    line_buf: String,
    line_overflow: bool,
    block_state: u64,
    block_has_lines: bool,
    line_hashes: Vec<u64>,
    block_hashes: Vec<u64>,
}

impl HunkCollector {
    const MAX_LINE_BYTES: usize = 8192;

    fn push_hash_unique(vec: &mut Vec<u64>, cap: usize, hash: u64) {
        if vec.len() < cap && !vec.contains(&hash) {
            vec.push(hash);
        }
    }

    fn feed_normalized_added_line(&mut self, raw_line: &str) {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.block_has_lines {
            self.block_state = fnv_update(FNV_OFFSET_BASIS, &[HUNK_HASH_DOMAIN]);
        } else {
            self.block_state = fnv_update(self.block_state, b"\n");
        }
        self.block_state = fnv_update(self.block_state, trimmed.as_bytes());
        self.block_has_lines = true;

        if self.line_hashes.len() < MAX_HUNK_HASHES_PER_EDIT {
            if let Some(lh) = hash_normalized_line(trimmed) {
                Self::push_hash_unique(&mut self.line_hashes, MAX_HUNK_HASHES_PER_EDIT, lh);
            }
        }
    }

    fn flush_block(&mut self) {
        if self.block_has_lines {
            let bh = splitmix_finalize(self.block_state);
            Self::push_hash_unique(&mut self.block_hashes, MAX_HUNK_HASHES_PER_EDIT, bh);
            self.block_has_lines = false;
            self.block_state = 0;
        }
    }

    fn push_content_chunk(&mut self, chunk: &str) {
        for part in chunk.split_inclusive('\n') {
            let fragment = part.strip_suffix('\n').unwrap_or(part);
            if !self.line_overflow {
                let room = Self::MAX_LINE_BYTES.saturating_sub(self.line_buf.len());
                if fragment.len() <= room {
                    self.line_buf.push_str(fragment);
                } else {
                    let safe = fragment.floor_char_boundary(room);
                    self.line_buf.push_str(&fragment[..safe]);
                    self.line_overflow = true;
                }
            }
            if part.ends_with('\n') {
                let line = std::mem::take(&mut self.line_buf);
                self.line_overflow = false;
                self.feed_normalized_added_line(&line);
            }
        }
    }

    fn end_content_segment(&mut self) {
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.line_overflow = false;
            self.feed_normalized_added_line(&line);
        }
        self.flush_block();
    }

    fn finish(mut self) -> Vec<u64> {
        self.end_content_segment();
        let mut out = self.block_hashes;
        for lh in self.line_hashes {
            if out.len() >= MAX_HUNK_HASHES_PER_EDIT {
                break;
            }
            if !out.contains(&lh) {
                out.push(lh);
            }
        }
        out
    }
}

/// Extract normalized 64-bit hunk-block and line fingerprints from an added/replacement content string.
pub fn extract_content_hunk_hashes(content: &str) -> Vec<u64> {
    let mut collector = HunkCollector::default();
    collector.push_content_chunk(content);
    collector.finish()
}

/// Extract normalized 64-bit hunk-block and line fingerprints from unified diff or `apply_patch` text.
/// Contiguous `+` lines (excluding `+++` file headers) form hunk blocks; context/deletion/header lines
/// flush the current hunk block.
pub fn extract_diff_hunk_hashes(patch_text: &str) -> Vec<u64> {
    let mut collector = HunkCollector::default();
    for raw_line in patch_text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if let Some(added) = line.strip_prefix('+') {
            if line.starts_with("+++ ") || line == "+++" {
                collector.flush_block();
            } else {
                collector.feed_normalized_added_line(added);
            }
        } else {
            collector.flush_block();
        }
    }
    collector.finish()
}

/// Extract every file path added, updated, deleted, or moved to in an `apply_patch` or unified diff.
pub fn patch_paths(patch_text: &str) -> Vec<String> {
    let mut patch = Patch::new(false);
    patch.push(patch_text);
    patch.finish_paths()
}

/// Extract `(path, hunk_hashes)` pairs in first-seen file order from an `apply_patch` or unified diff.
pub fn patch_paths_and_hunks(patch_text: &str) -> Vec<(String, Vec<u64>)> {
    let mut patch = Patch::new(true);
    patch.push(patch_text);
    patch.finish()
}

fn extract_file_edit_hunks(view: JsonView<'_>, is_full_write: bool) -> Result<Vec<u64>> {
    let mut collector = HunkCollector::default();
    let scalar_keys: &[&str] = if is_full_write {
        &[
            "content",
            "Content",
            "CodeContent",
            "code_content",
            "file_text",
            "text",
            "new_string",
            "new_str",
            "newText",
            "new_text",
        ]
    } else {
        &[
            "new_string",
            "new_str",
            "newText",
            "new_text",
            "replacement",
            "Replacement",
            "ReplacementContent",
            "replacement_content",
            "CodeContent",
            "code_content",
            "new_source",
            "content",
            "Content",
        ]
    };
    for &key in scalar_keys {
        if let Some(val) = view.get(key)? {
            if val.is_string()? {
                val.for_each_str_chunk(|chunk| {
                    collector.push_content_chunk(chunk);
                    Ok(())
                })?;
                collector.end_content_segment();
            }
        }
    }
    if !is_full_write {
        for (arr_key, item_keys) in [
            (
                "edits",
                &[
                    "new_string",
                    "new_str",
                    "newText",
                    "new_text",
                    "replacement",
                    "Replacement",
                    "content",
                ][..],
            ),
            (
                "ReplacementChunks",
                &["ReplacementContent", "replacement_content", "new_string"][..],
            ),
            (
                "replacement_chunks",
                &["replacement_content", "ReplacementContent", "new_string"][..],
            ),
            (
                "replacements",
                &[
                    "new_string",
                    "new_str",
                    "newText",
                    "new_text",
                    "replacement",
                    "Replacement",
                    "content",
                ][..],
            ),
        ] {
            if let Some(arr_view) = view.get(arr_key)? {
                if let Some(items) = arr_view.array()? {
                    for item in items {
                        let item = item?;
                        for &ik in item_keys {
                            if let Some(val) = item.get(ik)? {
                                if val.is_string()? {
                                    val.for_each_str_chunk(|chunk| {
                                        collector.push_content_chunk(chunk);
                                        Ok(())
                                    })?;
                                    collector.end_content_segment();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(collector.finish())
}

/// Extract the spawned child agent identifier (`task_name`, `agent_id`, `agentId`, or
/// `conversationId`) from a spawn tool's non-error result payload.
pub fn spawn_return_id(h: Harness, name: &str, result_text: &str, is_error: bool) -> Option<String> {
    if is_error {
        return None;
    }
    let bare = bare_tool_name(name);
    let trimmed = result_text.trim();
    match h {
        Harness::Codex if bare == "spawn_agent" => json_id_field(trimmed, &["task_name", "agent_id"]),
        Harness::Claude if matches!(bare, "Agent" | "Task" | "Workflow") => {
            claude_agent_id(trimmed).or_else(|| json_id_field(trimmed, &["agentId", "agent_id", "task_id"]))
        }
        Harness::Gemini | Harness::Antigravity
            if matches!(bare, "delegate_to_agent" | "invoke_subagent" | "invoke_agent") =>
        {
            embedded_json_id_field(trimmed, &["conversationId", "conversation_id", "agentId", "agent_id"])
                .or_else(|| claude_agent_id(trimmed))
        }
        Harness::Claude
        | Harness::Codex
        | Harness::Grok
        | Harness::OpenCode
        | Harness::Gemini
        | Harness::Antigravity
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Pi
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
        | Harness::OpenSession => {
            if is_spawn_tool(Some(h), bare) {
                json_id_field(
                    trimmed,
                    &["task_name", "agent_id", "agentId", "conversationId", "session_id"],
                )
                .or_else(|| claude_agent_id(trimmed))
            } else {
                None
            }
        }
    }
}

fn embedded_json_id_field(trimmed: &str, keys: &[&str]) -> Option<String> {
    if let Some(id) = json_id_field(trimmed, keys) {
        return Some(id);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (end > start)
        .then(|| json_id_field(&trimmed[start..=end], keys))
        .flatten()
}

fn json_id_field(trimmed: &str, keys: &[&str]) -> Option<String> {
    if !trimmed.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    for key in keys {
        if let Some(id) = v
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(id.to_string());
        }
    }
    None
}

fn claude_agent_id(text: &str) -> Option<String> {
    for marker in ["agentId:", "agent_id:", "conversationId:"] {
        if let Some(rest) = text.split_once(marker).map(|(_, r)| r.trim_start()) {
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'))
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    if let Some((_, rest)) = text.split_once("<agent_id>") {
        if let Some((id, _)) = rest.split_once("</agent_id>") {
            let id = id.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

// ── Streaming Patch & Command Projections (from `events/arguments.rs`) ──

#[derive(Default)]
struct Header {
    probe: String,
    value: Option<String>,
    ignored: bool,
    started: bool,
}

impl Header {
    fn push(&mut self, mut text: &str, prefixes: &[&str], trim_start: bool, capture: bool) {
        if self.ignored {
            return;
        }
        if let Some(value) = &mut self.value {
            value.push_str(text);
            return;
        }
        if trim_start && !self.started {
            text = text.trim_start();
            self.started = !text.is_empty();
        }
        for (offset, ch) in text.char_indices() {
            self.probe.push(ch);
            if prefixes.contains(&self.probe.as_str()) {
                self.value = Some(if capture {
                    text[offset + ch.len_utf8()..].to_string()
                } else {
                    String::new()
                });
                return;
            }
            if !prefixes.iter().any(|prefix| prefix.starts_with(&self.probe)) {
                self.ignored = true;
                self.probe.clear();
                return;
            }
        }
    }
}

#[derive(Default)]
struct Old {
    probe: String,
    ignored: bool,
    found: bool,
}

impl Old {
    fn push(&mut self, line: &str) {
        if self.ignored || self.found {
            return;
        }
        for ch in line.chars() {
            self.probe.push(ch);
            if self.probe.as_str() == "--- " {
                self.found = true;
                return;
            }
            if !"--- ".starts_with(&self.probe) {
                self.ignored = true;
                self.probe.clear();
                return;
            }
        }
    }
}

/// Streaming patch-header state machine: retains only candidate header lines, deduplicated
/// output paths in first-seen order, and (when `collect_hunks` is enabled) per-file normalized
/// 64-bit hunk/line fingerprints.
#[derive(Default)]
pub(crate) struct Patch {
    codex: Header,
    unified: Header,
    old: Old,
    primary: HashMap<String, usize>,
    fallback: HashMap<String, usize>,
    has_old: bool,
    collect_hunks: bool,
    line_buf: String,
    line_overflow: bool,
    active_primary: Option<String>,
    active_fallback: Option<String>,
    primary_hunks: HashMap<String, HunkCollector>,
    fallback_hunks: HashMap<String, HunkCollector>,
}

impl Patch {
    pub(crate) fn new(collect_hunks: bool) -> Self {
        Self {
            collect_hunks,
            ..Self::default()
        }
    }

    pub(crate) fn push(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            let line = part.strip_suffix('\n').unwrap_or(part);
            self.codex.push(line, HEADERS, true, true);
            self.unified.push(line, &["+++ "], false, true);
            if !self.has_old {
                self.old.push(line);
                self.has_old = self.old.found;
            }
            if self.collect_hunks && !self.line_overflow {
                let room = HunkCollector::MAX_LINE_BYTES.saturating_sub(self.line_buf.len());
                if line.len() <= room {
                    self.line_buf.push_str(line);
                } else {
                    let safe = line.floor_char_boundary(room);
                    self.line_buf.push_str(&line[..safe]);
                    self.line_overflow = true;
                }
            }
            if part.ends_with('\n') {
                self.end_line();
            }
        }
    }

    fn end_line(&mut self) {
        fn clean_path(value: &str) -> Option<String> {
            let value = value.trim();
            (!value.is_empty() && value != "/dev/null" && !value.contains("${")).then(|| value.to_string())
        }
        fn add(out: &mut HashMap<String, usize>, value: &str) -> Option<String> {
            let clean = clean_path(value)?;
            if !out.contains_key(&clean) {
                out.insert(clean.clone(), out.len());
            }
            Some(clean)
        }
        let mut switched_file = false;
        if let Some(value) = self.codex.value.take() {
            if let Some(prev) = &self.active_primary {
                if let Some(c) = self.primary_hunks.get_mut(prev) {
                    c.flush_block();
                }
            }
            self.active_primary = add(&mut self.primary, &value);
            switched_file = true;
        }
        if let Some(value) = self.unified.value.take() {
            if let Some(prev) = &self.active_fallback {
                if let Some(c) = self.fallback_hunks.get_mut(prev) {
                    c.flush_block();
                }
            }
            self.active_fallback = add(&mut self.fallback, value.trim().trim_start_matches("b/"));
            switched_file = true;
        }
        if self.collect_hunks {
            let full_line = std::mem::take(&mut self.line_buf);
            let full_line = full_line.strip_suffix('\r').unwrap_or(&full_line);
            self.line_overflow = false;
            if !switched_file && !full_line.is_empty() {
                let trimmed_start = full_line.trim_start();
                if trimmed_start.starts_with("*** End Patch") || trimmed_start.starts_with("diff --git ") {
                    if let Some(prev) = &self.active_primary {
                        if let Some(c) = self.primary_hunks.get_mut(prev) {
                            c.flush_block();
                        }
                    }
                    if let Some(prev) = &self.active_fallback {
                        if let Some(c) = self.fallback_hunks.get_mut(prev) {
                            c.flush_block();
                        }
                    }
                    self.active_primary = None;
                    self.active_fallback = None;
                } else if let Some(added) = full_line.strip_prefix('+') {
                    if !(full_line.starts_with("+++ ") || full_line == "+++") {
                        if let Some(active) = &self.active_primary {
                            self.primary_hunks
                                .entry(active.clone())
                                .or_default()
                                .feed_normalized_added_line(added);
                        }
                        if let Some(active) = &self.active_fallback {
                            self.fallback_hunks
                                .entry(active.clone())
                                .or_default()
                                .feed_normalized_added_line(added);
                        }
                    }
                } else {
                    if let Some(active) = &self.active_primary {
                        if let Some(c) = self.primary_hunks.get_mut(active) {
                            c.flush_block();
                        }
                    }
                    if let Some(active) = &self.active_fallback {
                        if let Some(c) = self.fallback_hunks.get_mut(active) {
                            c.flush_block();
                        }
                    }
                }
            }
        }
        self.codex = Header::default();
        self.unified = Header::default();
        self.old = Old::default();
    }

    pub(crate) fn finish_paths(self) -> Vec<String> {
        self.finish().into_iter().map(|(p, _)| p).collect()
    }

    pub(crate) fn finish(mut self) -> Vec<(String, Vec<u64>)> {
        if !self.codex.probe.is_empty()
            || self.codex.value.is_some()
            || !self.unified.probe.is_empty()
            || self.unified.value.is_some()
            || !self.line_buf.is_empty()
        {
            self.end_line();
        }
        let (paths, mut hunks_map) = if !self.primary.is_empty() {
            (self.primary, self.primary_hunks)
        } else if self.has_old {
            (self.fallback, self.fallback_hunks)
        } else {
            return Vec::new();
        };
        let mut ordered: Vec<_> = paths.into_iter().collect();
        ordered.sort_unstable_by_key(|(_, position)| *position);
        ordered
            .into_iter()
            .map(|(path, _)| {
                let hunks = hunks_map.remove(&path).map(HunkCollector::finish).unwrap_or_default();
                (path, hunks)
            })
            .collect()
    }
}

fn command_field(view: JsonView<'_>) -> Result<Option<JsonView<'_>>> {
    for key in ["command", "cmd", "script", "CommandLine"] {
        if let Some(value) = view.get(key)? {
            return Ok(Some(value));
        }
    }
    view.get("action")?
        .map(|action| action.get("command"))
        .transpose()
        .map(Option::flatten)
}

fn command_chunks(view: JsonView<'_>, mut chunk: impl FnMut(&str) -> Result<()>) -> Result<bool> {
    let Some(value) = command_field(view)? else {
        return Ok(false);
    };
    if value.is_string()? {
        return value.for_each_str_chunk(chunk);
    }
    let Some(parts) = value.array()? else {
        return Ok(false);
    };
    let mut found = false;
    for part in parts {
        let part = part?;
        if !part.is_string()? {
            continue;
        }
        if found {
            chunk(" ")?;
        }
        found = true;
        part.for_each_str_chunk(&mut chunk)?;
    }
    Ok(found)
}

pub(crate) fn command_string(view: JsonView<'_>) -> Result<Option<String>> {
    let mut text = String::new();
    Ok(command_chunks(view, |chunk| {
        text.push_str(chunk);
        Ok(())
    })?
    .then_some(text))
}

#[derive(Default)]
struct Trigger {
    tail: Vec<u8>,
    marker: bool,
    head: String,
    started: bool,
    decided: bool,
    apply: bool,
}

impl Trigger {
    fn push(&mut self, text: &str) {
        const MARKER: &[u8] = b"*** Begin Patch";
        const HEADS: [&str; 2] = ["apply_patch", "*** Begin Patch"];
        if !self.marker {
            let boundary_len = text.len().min(MARKER.len() - 1);
            self.tail.extend_from_slice(&text.as_bytes()[..boundary_len]);
            self.marker = self.tail.windows(MARKER.len()).any(|window| window == MARKER)
                || memchr::memmem::find(text.as_bytes(), MARKER).is_some();
            if text.len() >= MARKER.len() - 1 {
                self.tail.clear();
                let start = text.len() - (MARKER.len() - 1);
                self.tail.extend_from_slice(&text.as_bytes()[start..]);
            } else {
                let excess = self.tail.len().saturating_sub(MARKER.len() - 1);
                self.tail.drain(..excess);
            }
        }
        if self.decided {
            return;
        }
        let text = if !self.started { text.trim_start() } else { text };
        self.started |= !text.is_empty();
        for ch in text.chars() {
            self.head.push(ch);
            if HEADS.contains(&self.head.as_str()) {
                self.apply = true;
                self.decided = true;
                break;
            }
            if !HEADS.iter().any(|h| h.starts_with(&self.head)) {
                self.decided = true;
                break;
            }
        }
    }
}

pub(crate) type CommandWithPatches = (Option<String>, Vec<(String, Vec<u64>)>);

fn command_rows_bounded(
    view: JsonView<'_>,
    whole: bool,
    extract_hunks: bool,
    limit: Option<usize>,
) -> Result<Option<CommandWithPatches>> {
    let mut command = whole.then(String::new);
    let mut command_full = false;
    let mut trigger = Trigger::default();
    let exists = command_chunks(view, |chunk| {
        if let Some(command) = &mut command {
            if !command_full {
                let mut end = chunk
                    .len()
                    .min(limit.unwrap_or(usize::MAX).saturating_sub(command.len()));
                while !chunk.is_char_boundary(end) {
                    end -= 1;
                }
                command.push_str(&chunk[..end]);
                command_full = end < chunk.len();
            }
        }
        trigger.push(chunk);
        Ok(())
    })?;
    if !exists {
        return Ok(None);
    }
    let files = if trigger.marker || trigger.apply {
        let mut patch = Patch::new(extract_hunks);
        command_chunks(view, |chunk| {
            patch.push(chunk);
            Ok(())
        })?;
        patch
            .finish()
            .into_iter()
            .filter(|(path, _)| trigger.apply || std::path::Path::new(path).is_absolute())
            .collect()
    } else {
        Vec::new()
    };
    Ok(Some((command, files)))
}

fn command_patch_files(cmd: &str, extract_hunks: bool) -> Vec<(String, Vec<u64>)> {
    let mut trigger = Trigger::default();
    trigger.push(cmd);
    if !(trigger.marker || trigger.apply) {
        return Vec::new();
    }
    let mut patch = Patch::new(extract_hunks);
    patch.push(cmd);
    patch
        .finish()
        .into_iter()
        .filter(|(path, _)| trigger.apply || std::path::Path::new(path).is_absolute())
        .collect()
}

pub(crate) fn patch_rows(view: JsonView<'_>, extract_hunks: bool) -> Result<Vec<(String, Vec<u64>)>> {
    let mut body = view.is_string()?.then_some(view);
    if body.is_none() {
        for key in ["input", "patch"] {
            if let Some(value) = view.get(key)? {
                if value.is_string()? {
                    body = Some(value);
                    break;
                }
            }
        }
    }
    if body.is_none() {
        if let Some(cmd) = view.get("command")? {
            if starts_with_patch_header(cmd)? {
                body = Some(cmd);
            }
        }
    }
    let mut patch = Patch::new(extract_hunks);
    if let Some(body) = body {
        body.for_each_str_chunk(|chunk| {
            patch.push(chunk);
            Ok(())
        })?;
    }
    Ok(patch.finish())
}

fn starts_with_patch_header(view: JsonView<'_>) -> Result<bool> {
    const PREFIX: &str = "*** Begin Patch";
    let mut buf = String::new();
    let mut trimmed = false;
    let is_str = crate::projection::string_until(view, |chunk| {
        let rest = if trimmed {
            chunk
        } else {
            let s = chunk.trim_start();
            if !s.is_empty() {
                trimmed = true;
            }
            s
        };
        if !rest.is_empty() {
            for ch in rest.chars() {
                buf.push(ch);
                if buf.len() > PREFIX.len() + 4 {
                    break;
                }
            }
        }
        buf.len() > PREFIX.len() + 4
    })?;
    if !is_str {
        return Ok(false);
    }
    Ok(buf.lines().next().map(str::trim_end) == Some(PREFIX))
}

fn strip_file_uri(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    let path_part = if rest.len() >= 3
        && rest.as_bytes()[0] == b'/'
        && rest.as_bytes()[1].is_ascii_alphabetic()
        && rest.as_bytes()[2] == b':'
    {
        &rest[1..]
    } else {
        rest
    };
    let bytes = path_part.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                    decoded.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

pub(crate) fn file_key(view: JsonView<'_>, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        if let Some(value) = view.get(key)? {
            if let Some(path) = input::string(value)? {
                if path.is_empty() {
                    return Ok(None);
                }
                if let Some(uri_path) = strip_file_uri(&path) {
                    return Ok((!uri_path.is_empty()).then_some(uri_path));
                }
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

// ── Zero-AST Codex Code Mode (`exec`) JavaScript Scanner ──

#[derive(Debug, Clone)]
enum JsCall {
    ExecCommand {
        cmd: Option<String>,
        workdir: Option<String>,
    },
    ApplyPatch {
        direct_body: Option<String>,
        workdir: Option<String>,
    },
    WriteStdin {
        session_id: Option<String>,
        has_chars: bool,
    },
    ViewImage {
        path: Option<String>,
        workdir: Option<String>,
    },
    UpdatePlan {
        explanation: Option<String>,
        items: Vec<Item>,
    },
    Goal {
        tool: &'static str,
        action: &'static str,
        summary: String,
    },
    SpawnAgent {
        target_hint: Option<String>,
        has_prompt: bool,
    },
    AgentMessage {
        tool: &'static str,
        recipient: Option<String>,
    },
    BgControl {
        tool: &'static str,
        job_id: Option<String>,
        action: BgAction,
    },
    Question {
        tool: &'static str,
        summary: String,
    },
    WebRun {
        query: Option<String>,
    },
    Other {
        tool: String,
        summary: Option<String>,
    },
}

fn scan_codex_exec(
    script: &str,
    opts: ClassifyOpts,
    extract_hunks: bool,
    emit: &mut dyn FnMut(ToolSemantic<'_>, Vec<u64>),
) {
    let mut scanner = JsScanner::new(script);
    scanner.scan_all();

    if scanner.calls.is_empty() {
        let summary = opts.summaries.then(|| Cow::Owned(collapse_line(script, 80)));
        emit(
            ToolSemantic {
                tool: Cow::Borrowed("exec"),
                op: ToolOp::Other { path: None, summary },
            },
            Vec::new(),
        );
        return;
    }

    // Lazily computed script-wide patch header fallback for variable-bound `tools.apply_patch(patch)`.
    let mut fallback_patch_edits: Option<Vec<(String, Vec<u64>)>> = None;

    for call in scanner.calls {
        match call {
            JsCall::ExecCommand { cmd, workdir } => {
                let patch_files = cmd
                    .as_deref()
                    .map(|c| command_patch_files(c, extract_hunks))
                    .unwrap_or_default();
                let clean_workdir = workdir.filter(|w| !w.is_empty() && !w.contains("${"));
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed("exec_command"),
                        op: ToolOp::Command {
                            cmd: if opts.commands {
                                cmd.map(|mut cmd| {
                                    if let Some(limit) = opts.command_limit {
                                        let mut end = limit.min(cmd.len());
                                        while !cmd.is_char_boundary(end) {
                                            end -= 1;
                                        }
                                        cmd.truncate(end);
                                    }
                                    Cow::Owned(cmd)
                                })
                            } else {
                                None
                            },
                            workdir: clean_workdir.clone().map(Cow::Owned),
                            background: false,
                        },
                    },
                    Vec::new(),
                );
                for (file, hunks) in patch_files {
                    let path = resolve_workdir(&file, clean_workdir.as_deref());
                    emit(
                        ToolSemantic {
                            tool: Cow::Borrowed("exec_command"),
                            op: ToolOp::FileEdit {
                                path: Cow::Owned(path),
                                is_full_write: false,
                            },
                        },
                        hunks,
                    );
                }
            }
            JsCall::ApplyPatch { direct_body, workdir } => {
                let clean_workdir = workdir.filter(|w| !w.is_empty() && !w.contains("${"));
                let direct_edits = direct_body
                    .as_deref()
                    .map(|body| {
                        let mut p = Patch::new(extract_hunks);
                        p.push(body);
                        p.finish()
                    })
                    .unwrap_or_default();
                let edits = if !direct_edits.is_empty() {
                    direct_edits
                } else {
                    fallback_patch_edits
                        .get_or_insert_with(|| {
                            let mut patch = Patch::new(extract_hunks);
                            for lit in &scanner.literals {
                                patch.push(lit);
                                patch.push("\n");
                            }
                            patch.finish()
                        })
                        .clone()
                };
                if edits.is_empty() {
                    emit(
                        ToolSemantic {
                            tool: Cow::Borrowed("apply_patch"),
                            op: ToolOp::Other {
                                path: None,
                                summary: None,
                            },
                        },
                        Vec::new(),
                    );
                } else {
                    for (file, hunks) in edits {
                        let path = resolve_workdir(&file, clean_workdir.as_deref());
                        emit(
                            ToolSemantic {
                                tool: Cow::Borrowed("apply_patch"),
                                op: ToolOp::FileEdit {
                                    path: Cow::Owned(path),
                                    is_full_write: false,
                                },
                            },
                            hunks,
                        );
                    }
                }
            }
            JsCall::WriteStdin { session_id, has_chars } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed("write_stdin"),
                        op: ToolOp::Background {
                            job_id: if opts.summaries {
                                session_id.map(Cow::Owned)
                            } else {
                                None
                            },
                            action: if has_chars { BgAction::Interact } else { BgAction::Poll },
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::ViewImage { path, workdir } => {
                let clean_workdir = workdir.filter(|w| !w.is_empty() && !w.contains("${"));
                if let Some(p) = path.filter(|p| !p.is_empty() && !p.contains("${")) {
                    let resolved = resolve_workdir(&p, clean_workdir.as_deref());
                    emit(
                        ToolSemantic {
                            tool: Cow::Borrowed("view_image"),
                            op: ToolOp::FileRead {
                                path: Cow::Owned(resolved),
                            },
                        },
                        Vec::new(),
                    );
                } else {
                    emit(
                        ToolSemantic {
                            tool: Cow::Borrowed("view_image"),
                            op: ToolOp::Other {
                                path: None,
                                summary: None,
                            },
                        },
                        Vec::new(),
                    );
                }
            }
            JsCall::UpdatePlan { explanation, items } => {
                let summary = if opts.summaries {
                    let mut parts = Vec::new();
                    if let Some(exp) = explanation.filter(|s| !s.is_empty()) {
                        parts.push(collapse_line(&exp, 120));
                    }
                    if !items.is_empty() {
                        let first = collapse_line(&items[0].content, 80);
                        if items.len() == 1 {
                            parts.push(format!("1 item · {first}"));
                        } else {
                            parts.push(format!("{} items · {first}", items.len()));
                        }
                    }
                    truncate(&parts.join(" · "), 120)
                } else {
                    String::new()
                };
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed("update_plan"),
                        op: ToolOp::Affordance {
                            kind: Kind::Plan,
                            action: "write",
                            summary,
                            items: if opts.summaries { items } else { Vec::new() },
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::Goal { tool, action, summary } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed(tool),
                        op: ToolOp::Affordance {
                            kind: Kind::Goal,
                            action,
                            summary: if opts.summaries {
                                truncate(&summary, 120)
                            } else {
                                String::new()
                            },
                            items: Vec::new(),
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::SpawnAgent {
                target_hint,
                has_prompt,
            } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed("spawn_agent"),
                        op: ToolOp::SubagentSpawn {
                            target_hint: if opts.summaries {
                                target_hint.map(|h| Cow::Owned(truncate(&h, 120)))
                            } else {
                                None
                            },
                            has_prompt: opts.summaries && has_prompt,
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::AgentMessage { tool, recipient } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed(tool),
                        op: ToolOp::AgentMessage {
                            recipient: if opts.summaries {
                                recipient.map(|r| Cow::Owned(truncate(&r, 120)))
                            } else {
                                None
                            },
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::BgControl { tool, job_id, action } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed(tool),
                        op: ToolOp::Background {
                            job_id: if opts.summaries {
                                job_id.map(|j| Cow::Owned(truncate(&j, 80)))
                            } else {
                                None
                            },
                            action,
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::Question { tool, summary } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed(tool),
                        op: ToolOp::Affordance {
                            kind: Kind::Question,
                            action: "ask",
                            summary: if opts.summaries {
                                truncate(&summary, 120)
                            } else {
                                String::new()
                            },
                            items: Vec::new(),
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::WebRun { query } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Borrowed("web__run"),
                        op: ToolOp::Search {
                            path: None,
                            query: if opts.summaries { query.map(Cow::Owned) } else { None },
                        },
                    },
                    Vec::new(),
                );
            }
            JsCall::Other { tool, summary } => {
                emit(
                    ToolSemantic {
                        tool: Cow::Owned(tool),
                        op: ToolOp::Other {
                            path: None,
                            summary: if opts.summaries { summary.map(Cow::Owned) } else { None },
                        },
                    },
                    Vec::new(),
                );
            }
        }
    }
}

struct JsScanner<'s> {
    src: &'s str,
    bytes: &'s [u8],
    pos: usize,
    regex_allowed: bool,
    nesting: usize,
    bindings: HashMap<String, String>,
    literals: Vec<String>,
    calls: Vec<JsCall>,
}

#[derive(Default)]
struct JsObj {
    props: HashMap<String, JsVal>,
    plan_items: Vec<Item>,
    search_query: Option<String>,
}

#[derive(Clone)]
enum JsVal {
    Str(String),
    Num(String),
    Bool(bool),
    Ident(String),
    Other,
}

impl JsVal {
    fn resolve_str(&self, bindings: &HashMap<String, String>) -> Option<String> {
        match self {
            JsVal::Str(s) => Some(s.clone()),
            JsVal::Ident(name) => bindings.get(name).cloned(),
            _ => None,
        }
    }
    fn resolve_scalar(&self, bindings: &HashMap<String, String>) -> Option<String> {
        match self {
            JsVal::Str(s) | JsVal::Num(s) => Some(s.clone()),
            JsVal::Bool(b) => Some(b.to_string()),
            JsVal::Ident(name) => bindings.get(name).cloned(),
            JsVal::Other => None,
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

// Bound recursive semantic extraction; deeper syntax is still scanned without descent.
const JS_MAX_NESTING: usize = 64;

impl<'s> JsScanner<'s> {
    fn new(src: &'s str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            regex_allowed: true,
            nesting: 0,
            bindings: HashMap::new(),
            literals: Vec::new(),
            calls: Vec::new(),
        }
    }

    fn scan_all(&mut self) {
        while self.pos < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            self.step_statement_or_expr();
        }
    }

    fn skip_ws_and_comments(&mut self) {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_whitespace() {
                self.pos += 1;
                continue;
            }
            if b == b'/' && self.pos + 1 < self.bytes.len() {
                let next = self.bytes[self.pos + 1];
                if next == b'/' {
                    self.pos += 2;
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    continue;
                }
                if next == b'*' {
                    self.pos += 2;
                    while self.pos + 1 < self.bytes.len() {
                        if self.bytes[self.pos] == b'*' && self.bytes[self.pos + 1] == b'/' {
                            self.pos += 2;
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.pos + 1 >= self.bytes.len() {
                        self.pos = self.bytes.len();
                    }
                    continue;
                }
            }
            // UTF-8 multi-byte whitespace (e.g. \u{2003})
            if b >= 0x80 {
                if let Some(ch) = self.src[self.pos..].chars().next() {
                    if ch.is_whitespace() {
                        self.pos += ch.len_utf8();
                        continue;
                    }
                }
            }
            break;
        }
    }

    fn step_statement_or_expr(&mut self) {
        let b = self.bytes[self.pos];
        match b {
            b'\'' | b'"' => {
                let s = self.read_quoted_string(b);
                self.record_literal(s);
                self.regex_allowed = false;
            }
            b'`' => {
                let s = self.read_template_literal();
                self.record_literal(s);
                self.regex_allowed = false;
            }
            b'/' if self.regex_allowed => {
                self.skip_regex_literal();
                self.regex_allowed = false;
            }
            b'(' | b'[' | b'{' | b',' | b';' | b':' | b'=' | b'!' | b'&' | b'|' | b'?' | b'+' | b'-' | b'*' | b'%'
            | b'~' | b'^' | b'<' | b'>' => {
                self.pos += 1;
                self.regex_allowed = true;
            }
            b')' | b']' | b'}' => {
                self.pos += 1;
                self.regex_allowed = false;
            }
            b if b.is_ascii_digit() => {
                while self.pos < self.bytes.len()
                    && (self.bytes[self.pos].is_ascii_alphanumeric() || matches!(self.bytes[self.pos], b'.' | b'_'))
                {
                    self.pos += 1;
                }
                self.regex_allowed = false;
            }
            b if is_ident_start(b) => {
                let prev_dot = self.preceded_by_dot();
                let ident = self.read_ident();
                if !prev_dot && matches!(ident, "const" | "let" | "var") {
                    self.regex_allowed = true;
                    self.try_read_binding();
                    return;
                }
                if !prev_dot && ident == "tools" && self.try_read_tools_call() {
                    self.regex_allowed = false;
                    return;
                }
                self.regex_allowed = matches!(
                    ident,
                    "return"
                        | "throw"
                        | "case"
                        | "typeof"
                        | "void"
                        | "delete"
                        | "in"
                        | "of"
                        | "instanceof"
                        | "new"
                        | "await"
                        | "yield"
                );
            }
            _ => {
                let step = self.src[self.pos..].chars().next().map_or(1, char::len_utf8);
                self.pos += step;
            }
        }
    }

    fn preceded_by_dot(&self) -> bool {
        let mut i = self.pos;
        while i > 0 && self.bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        i > 0 && self.bytes[i - 1] == b'.'
    }

    fn read_ident(&mut self) -> &'s str {
        let start = self.pos;
        while self.pos < self.bytes.len() && is_ident_continue(self.bytes[self.pos]) {
            let step = if self.bytes[self.pos] < 0x80 {
                1
            } else {
                self.src[self.pos..].chars().next().map_or(1, char::len_utf8)
            };
            self.pos += step;
        }
        &self.src[start..self.pos]
    }

    fn record_literal(&mut self, s: String) {
        if self.literals.len() < 4096 {
            self.literals.push(s);
        }
    }

    fn skip_regex_literal(&mut self) {
        debug_assert_eq!(self.bytes[self.pos], b'/');
        self.pos += 1;
        let mut in_class = false;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'\n' || b == b'\r' {
                break;
            }
            if b == b'\\' {
                self.pos += 1;
                if self.pos < self.bytes.len() && !matches!(self.bytes[self.pos], b'\n' | b'\r') {
                    let step = self.src[self.pos..].chars().next().map_or(1, char::len_utf8);
                    self.pos += step;
                }
                continue;
            }
            if b == b'[' {
                in_class = true;
                self.pos += 1;
                continue;
            }
            if b == b']' && in_class {
                in_class = false;
                self.pos += 1;
                continue;
            }
            if b == b'/' && !in_class {
                self.pos += 1;
                while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_alphabetic() {
                    self.pos += 1;
                }
                return;
            }
            let step = self.src[self.pos..].chars().next().map_or(1, char::len_utf8);
            self.pos += step;
        }
    }

    fn read_quoted_string(&mut self, quote: u8) -> String {
        debug_assert_eq!(self.bytes[self.pos], quote);
        self.pos += 1;
        let start = self.pos;
        let mut out: Option<String> = None;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == quote {
                let res = match out {
                    Some(s) => s,
                    None => self.src[start..self.pos].to_string(),
                };
                self.pos += 1;
                return res;
            }
            if b == b'\\' {
                let buf = out.get_or_insert_with(|| self.src[start..self.pos].to_string());
                self.pos += 1;
                if self.pos >= self.bytes.len() {
                    break;
                }
                self.read_escape_into(buf);
                continue;
            }
            if let Some(buf) = &mut out {
                let ch = self.src[self.pos..].chars().next().unwrap_or('\u{FFFD}');
                buf.push(ch);
                self.pos += ch.len_utf8();
            } else {
                let step = if b < 0x80 {
                    1
                } else {
                    self.src[self.pos..].chars().next().map_or(1, char::len_utf8)
                };
                self.pos += step;
            }
        }
        out.unwrap_or_else(|| self.src[start..self.pos].to_string())
    }

    fn read_escape_into(&mut self, buf: &mut String) {
        let esc = self.bytes[self.pos];
        self.pos += 1;
        match esc {
            b'n' => buf.push('\n'),
            b'r' => buf.push('\r'),
            b't' => buf.push('\t'),
            b'0' => buf.push('\0'),
            b'\\' => buf.push('\\'),
            b'\'' => buf.push('\''),
            b'"' => buf.push('"'),
            b'`' => buf.push('`'),
            b'$' => buf.push('$'),
            b'\n' => {}
            b'\r' => {
                if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\n' {
                    self.pos += 1;
                }
            }
            b'x' if self.pos + 2 <= self.bytes.len() => {
                if let Ok(code) = u8::from_str_radix(self.src.get(self.pos..self.pos + 2).unwrap_or(""), 16) {
                    buf.push(code as char);
                    self.pos += 2;
                }
            }
            b'u' => {
                if self.pos < self.bytes.len() && self.bytes[self.pos] == b'{' {
                    self.pos += 1;
                    let hex_start = self.pos;
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'}' {
                        self.pos += 1;
                    }
                    if let Ok(code) = u32::from_str_radix(&self.src[hex_start..self.pos], 16) {
                        if let Some(ch) = char::from_u32(code) {
                            buf.push(ch);
                        }
                    }
                    if self.pos < self.bytes.len() && self.bytes[self.pos] == b'}' {
                        self.pos += 1;
                    }
                } else if self.pos + 4 <= self.bytes.len() {
                    if let Ok(code) = u32::from_str_radix(self.src.get(self.pos..self.pos + 4).unwrap_or(""), 16) {
                        if let Some(ch) = char::from_u32(code) {
                            buf.push(ch);
                        }
                        self.pos += 4;
                    }
                }
            }
            _ => {
                let ch = self.src[self.pos - 1..].chars().next().unwrap_or('\u{FFFD}');
                buf.push(ch);
                if ch.len_utf8() > 1 {
                    self.pos += ch.len_utf8() - 1;
                }
            }
        }
    }

    fn read_template_literal(&mut self) -> String {
        if self.nesting >= JS_MAX_NESTING {
            // At the cap, preserve the literal but do not descend into interpolations.
            return self.read_quoted_string(b'`');
        }
        self.nesting += 1;
        debug_assert_eq!(self.bytes[self.pos], b'`');
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'`' {
                self.pos += 1;
                self.nesting -= 1;
                return out;
            }
            if b == b'\\' {
                self.pos += 1;
                if self.pos >= self.bytes.len() {
                    break;
                }
                self.read_escape_into(&mut out);
                continue;
            }
            if b == b'$' && self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1] == b'{' {
                let expr_start = self.pos;
                self.pos += 2;
                self.regex_allowed = true;
                let mut depth = 1usize;
                while self.pos < self.bytes.len() && depth > 0 {
                    self.skip_ws_and_comments();
                    if self.pos >= self.bytes.len() {
                        break;
                    }
                    match self.bytes[self.pos] {
                        b'{' => {
                            depth += 1;
                            self.pos += 1;
                            self.regex_allowed = true;
                        }
                        b'}' => {
                            depth -= 1;
                            self.pos += 1;
                            self.regex_allowed = false;
                        }
                        _ => self.step_statement_or_expr(),
                    }
                }
                out.push_str(&self.src[expr_start..self.pos]);
                continue;
            }
            let ch = self.src[self.pos..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            self.pos += ch.len_utf8();
        }
        self.nesting -= 1;
        out
    }

    /// Track simple `const x = "..."` or `const patch = ["...", "..."].join("\n")` bindings.
    fn try_read_binding(&mut self) {
        if self.nesting >= JS_MAX_NESTING {
            return;
        }
        self.skip_ws_and_comments();
        if self.pos >= self.bytes.len() || !is_ident_start(self.bytes[self.pos]) {
            return;
        }
        let name = self.read_ident().to_string();
        self.skip_ws_and_comments();
        if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'=' {
            return;
        }
        if self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1] == b'=' {
            return;
        }
        self.pos += 1;
        self.regex_allowed = true;
        self.skip_ws_and_comments();
        if self.pos >= self.bytes.len() {
            return;
        }
        // Array elements and `.join(...)` arguments can contain nested declarations.
        // Count this descent too, even when no tools call, object, or template is involved.
        self.nesting += 1;
        if let Some(val) = self.try_read_string_or_array_join() {
            if self.bindings.len() < 256 {
                self.bindings.insert(name, val);
            }
        }
        self.nesting -= 1;
    }

    fn try_read_string_or_array_join(&mut self) -> Option<String> {
        let b = self.bytes.get(self.pos).copied()?;
        match b {
            b'\'' | b'"' => {
                let s = self.read_quoted_string(b);
                self.record_literal(s.clone());
                self.regex_allowed = false;
                Some(s)
            }
            b'`' => {
                let s = self.read_template_literal();
                self.record_literal(s.clone());
                self.regex_allowed = false;
                Some(s)
            }
            b'[' => {
                self.pos += 1;
                self.regex_allowed = true;
                let mut parts = Vec::new();
                let mut all_strings = true;
                while !self.finish_container(b']') {
                    let cur = self.bytes[self.pos];
                    if cur == b',' {
                        self.pos += 1;
                        self.regex_allowed = true;
                        continue;
                    }
                    if matches!(cur, b'\'' | b'"') {
                        let s = self.read_quoted_string(cur);
                        self.record_literal(s.clone());
                        parts.push(s);
                        self.regex_allowed = false;
                    } else if cur == b'`' {
                        let s = self.read_template_literal();
                        self.record_literal(s.clone());
                        parts.push(s);
                        self.regex_allowed = false;
                    } else {
                        all_strings = false;
                        self.skip_balanced_until(b",]");
                    }
                }
                // Consume optional `.join("\n")`
                let saved = self.pos;
                self.skip_ws_and_comments();
                let mut sep = "\n".to_string();
                if self.pos < self.bytes.len() && self.bytes[self.pos] == b'.' {
                    self.pos += 1;
                    self.skip_ws_and_comments();
                    if self.pos < self.bytes.len()
                        && is_ident_start(self.bytes[self.pos])
                        && self.read_ident() == "join"
                    {
                        self.skip_ws_and_comments();
                        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'(' {
                            self.pos += 1;
                            self.skip_ws_and_comments();
                            if self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'\'' | b'"') {
                                let q = self.bytes[self.pos];
                                sep = self.read_quoted_string(q);
                            }
                            self.skip_balanced_until(b")");
                            if self.pos < self.bytes.len() && self.bytes[self.pos] == b')' {
                                self.pos += 1;
                            }
                            self.regex_allowed = false;
                        }
                    } else {
                        self.pos = saved;
                    }
                } else {
                    self.pos = saved;
                }
                (all_strings && !parts.is_empty()).then(|| parts.join(&sep))
            }
            _ => None,
        }
    }

    fn try_read_tools_call(&mut self) -> bool {
        if self.nesting >= JS_MAX_NESTING {
            return false;
        }
        let saved = self.pos;
        self.skip_ws_and_comments();
        if self.pos >= self.bytes.len() {
            return false;
        }
        let tool_name = if self.bytes[self.pos] == b'.' {
            self.pos += 1;
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() || !is_ident_start(self.bytes[self.pos]) {
                self.pos = saved;
                return false;
            }
            self.read_ident().to_string()
        } else if self.bytes[self.pos] == b'[' {
            self.pos += 1;
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() || !matches!(self.bytes[self.pos], b'\'' | b'"') {
                self.pos = saved;
                return false;
            }
            let q = self.bytes[self.pos];
            let name = self.read_quoted_string(q);
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b']' {
                self.pos = saved;
                return false;
            }
            self.pos += 1;
            name
        } else {
            self.pos = saved;
            return false;
        };

        self.skip_ws_and_comments();
        // Optional chaining `tools.foo?.(...)`
        if self.pos + 1 < self.bytes.len() && self.bytes[self.pos] == b'?' && self.bytes[self.pos + 1] == b'.' {
            self.pos += 2;
            self.skip_ws_and_comments();
        }
        if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'(' {
            self.pos = saved;
            return false;
        }
        self.pos += 1;
        self.regex_allowed = true;
        self.parse_tool_call_args(&tool_name);
        true
    }

    fn parse_tool_call_args(&mut self, tool_name: &str) {
        self.nesting += 1;
        self.skip_ws_and_comments();
        let mut obj = JsObj::default();
        let mut direct_str: Option<String> = None;

        if self.pos < self.bytes.len() && self.bytes[self.pos] != b')' {
            let b = self.bytes[self.pos];
            if b == b'{' {
                obj = self.parse_object_literal();
            } else if let Some(s) = self.try_read_string_or_array_join() {
                direct_str = Some(s);
            } else if is_ident_start(b) {
                let ident = self.read_ident().to_string();
                self.regex_allowed = false;
                if let Some(bound) = self.bindings.get(&ident).cloned() {
                    direct_str = Some(bound);
                }
            }
            // Consume any remaining arguments up to matching `)` while still scanning nested calls/literals.
            self.skip_balanced_until(b")");
        }
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b')' {
            self.pos += 1;
        }

        let prop_str = |key: &str| obj.props.get(key).and_then(|v| v.resolve_str(&self.bindings));
        let prop_scalar = |key: &str| obj.props.get(key).and_then(|v| v.resolve_scalar(&self.bindings));

        let call = match tool_name {
            "exec_command" => JsCall::ExecCommand {
                cmd: prop_str("cmd").or_else(|| prop_str("command")).or(direct_str),
                workdir: prop_str("workdir").or_else(|| prop_str("cwd")),
            },
            "apply_patch" => {
                let direct_body = direct_str
                    .or_else(|| prop_str("input"))
                    .or_else(|| prop_str("patch"))
                    .or_else(|| prop_str("cmd"));
                JsCall::ApplyPatch {
                    direct_body,
                    workdir: prop_str("workdir").or_else(|| prop_str("cwd")),
                }
            }
            "write_stdin" => {
                let has_chars = prop_str("chars").is_some_and(|c| !c.is_empty());
                JsCall::WriteStdin {
                    session_id: prop_scalar("session_id"),
                    has_chars,
                }
            }
            "view_image" => JsCall::ViewImage {
                path: prop_str("path").or(direct_str),
                workdir: prop_str("workdir").or_else(|| prop_str("cwd")),
            },
            "update_plan" => JsCall::UpdatePlan {
                explanation: prop_str("explanation"),
                items: obj.plan_items,
            },
            "create_goal" => JsCall::Goal {
                tool: "create_goal",
                action: "create",
                summary: prop_str("objective").unwrap_or_default(),
            },
            "update_goal" => {
                let mut parts = Vec::new();
                if let Some(obj_text) = prop_str("objective").filter(|s| !s.is_empty()) {
                    parts.push(obj_text);
                }
                if let Some(prog) = prop_str("progress_summary").filter(|s| !s.is_empty()) {
                    parts.push(prog);
                }
                if let Some(st) = prop_str("status").filter(|s| !s.is_empty()) {
                    parts.push(st);
                }
                JsCall::Goal {
                    tool: "update_goal",
                    action: "update",
                    summary: parts.join(" · "),
                }
            }
            "get_goal" => JsCall::Goal {
                tool: "get_goal",
                action: "read",
                summary: String::new(),
            },
            "spawn_agent" => JsCall::SpawnAgent {
                target_hint: prop_str("task_name")
                    .or_else(|| prop_str("description"))
                    .or_else(|| prop_str("agent_type")),
                has_prompt: prop_str("message")
                    .or_else(|| prop_str("prompt"))
                    .is_some_and(|s| !s.trim().is_empty()),
            },
            "send_message" => JsCall::AgentMessage {
                tool: "send_message",
                recipient: prop_str("target").or_else(|| prop_str("to")).or_else(|| prop_str("id")),
            },
            "send_input" => JsCall::AgentMessage {
                tool: "send_input",
                recipient: prop_str("target").or_else(|| prop_str("id")),
            },
            "wait" if prop_scalar("cell_id").is_some() => JsCall::BgControl {
                tool: "wait",
                job_id: prop_scalar("cell_id"),
                action: BgAction::Poll,
            },
            "wait_agent" => JsCall::BgControl {
                tool: "wait_agent",
                job_id: prop_scalar("id").or_else(|| prop_scalar("target")),
                action: BgAction::Poll,
            },
            "interrupt_agent" => JsCall::BgControl {
                tool: "interrupt_agent",
                job_id: prop_scalar("id").or_else(|| prop_scalar("target")),
                action: BgAction::Stop,
            },
            "close_agent" => JsCall::BgControl {
                tool: "close_agent",
                job_id: prop_scalar("id").or_else(|| prop_scalar("target")),
                action: BgAction::Stop,
            },
            "request_user_input" => JsCall::Question {
                tool: "request_user_input",
                summary: prop_str("question").or_else(|| prop_str("prompt")).unwrap_or_default(),
            },
            "request_user_input_async" => JsCall::Question {
                tool: "request_user_input_async",
                summary: prop_str("question").or_else(|| prop_str("prompt")).unwrap_or_default(),
            },
            "web__run" => JsCall::WebRun {
                query: obj.search_query.or_else(|| prop_str("q")).or_else(|| prop_str("query")),
            },
            other => {
                let summary = prop_str("uri")
                    .or_else(|| prop_str("prompt"))
                    .or_else(|| prop_str("query"))
                    .or_else(|| prop_str("path"))
                    .or_else(|| prop_str("url"))
                    .or(direct_str);
                JsCall::Other {
                    tool: other.to_string(),
                    summary,
                }
            }
        };
        self.calls.push(call);
        self.nesting -= 1;
    }

    /// End a container at EOF or any closing delimiter. Leave mismatched closers for the
    /// enclosing reader: retrying `skip_balanced_until` there would never advance.
    fn finish_container(&mut self, close: u8) -> bool {
        self.skip_ws_and_comments();
        match self.bytes.get(self.pos).copied() {
            None => true,
            Some(b @ (b')' | b']' | b'}')) => {
                if b == close {
                    self.pos += 1;
                    self.regex_allowed = false;
                }
                true
            }
            _ => false,
        }
    }

    fn parse_object_literal(&mut self) -> JsObj {
        debug_assert_eq!(self.bytes[self.pos], b'{');
        self.pos += 1;
        self.regex_allowed = true;
        if self.nesting >= JS_MAX_NESTING {
            self.skip_balanced_until(b"}");
            if self.bytes.get(self.pos) == Some(&b'}') {
                self.pos += 1;
            }
            self.regex_allowed = false;
            return JsObj::default();
        }
        self.nesting += 1;
        let mut obj = JsObj::default();

        while !self.finish_container(b'}') {
            let b = self.bytes[self.pos];
            if b == b',' {
                self.pos += 1;
                self.regex_allowed = true;
                continue;
            }
            // Spread `...other`
            if self.pos + 2 < self.bytes.len() && &self.bytes[self.pos..self.pos + 3] == b"..." {
                self.pos += 3;
                self.skip_balanced_until(b",}");
                continue;
            }
            let key = if matches!(b, b'\'' | b'"') {
                let s = self.read_quoted_string(b);
                self.record_literal(s.clone());
                Some(s)
            } else if is_ident_start(b) {
                Some(self.read_ident().to_string())
            } else {
                None
            };
            let Some(key) = key else {
                self.skip_balanced_until(b",}");
                continue;
            };
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            // Shorthand property `{ cmd, workdir }`
            if matches!(self.bytes[self.pos], b',' | b'}') {
                let ident = key.clone();
                obj.props.insert(key, JsVal::Ident(ident));
                continue;
            }
            if self.bytes[self.pos] != b':' {
                self.skip_balanced_until(b",}");
                continue;
            }
            self.pos += 1;
            self.regex_allowed = true;
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            if key == "plan" && self.bytes[self.pos] == b'[' {
                obj.plan_items = self.parse_plan_items_array();
                continue;
            }
            if key == "search_query" && self.bytes[self.pos] == b'[' {
                obj.search_query = self.parse_search_query_array();
                continue;
            }
            let val = self.parse_prop_value();
            obj.props.insert(key, val);
            self.skip_balanced_until(b",}");
        }
        self.nesting -= 1;
        obj
    }

    fn parse_prop_value(&mut self) -> JsVal {
        if self.pos >= self.bytes.len() {
            return JsVal::Other;
        }
        let b = self.bytes[self.pos];
        if let Some(s) = self.try_read_string_or_array_join() {
            return JsVal::Str(s);
        }
        if b.is_ascii_digit()
            || (b == b'-' && self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1].is_ascii_digit())
        {
            let start = self.pos;
            if b == b'-' {
                self.pos += 1;
            }
            while self.pos < self.bytes.len() && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'.')
            {
                self.pos += 1;
            }
            self.regex_allowed = false;
            return JsVal::Num(self.src[start..self.pos].to_string());
        }
        if is_ident_start(b) {
            let ident = self.read_ident().to_string();
            self.regex_allowed = false;
            if ident == "true" {
                return JsVal::Bool(true);
            }
            if ident == "false" {
                return JsVal::Bool(false);
            }
            if ident == "tools" && self.try_read_tools_call() {
                return JsVal::Other;
            }
            // Distinguish bare `ident` vs `ident.prop` / `ident(...)`
            self.skip_ws_and_comments();
            if self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'.' | b'(' | b'[') {
                return JsVal::Other;
            }
            return JsVal::Ident(ident);
        }
        JsVal::Other
    }

    fn parse_plan_items_array(&mut self) -> Vec<Item> {
        debug_assert_eq!(self.bytes[self.pos], b'[');
        self.pos += 1;
        let mut items = Vec::new();
        while !self.finish_container(b']') {
            let b = self.bytes[self.pos];
            if b == b',' {
                self.pos += 1;
                continue;
            }
            if b == b'{' {
                let sub = self.parse_object_literal();
                let step = sub
                    .props
                    .get("step")
                    .or_else(|| sub.props.get("content"))
                    .and_then(|v| v.resolve_str(&self.bindings));
                let status = sub.props.get("status").and_then(|v| v.resolve_str(&self.bindings));
                if let Some(content) = step.filter(|s| !s.trim().is_empty()) {
                    items.push(Item { content, status });
                }
            } else {
                self.skip_balanced_until(b",]");
            }
        }
        items
    }

    fn parse_search_query_array(&mut self) -> Option<String> {
        debug_assert_eq!(self.bytes[self.pos], b'[');
        self.pos += 1;
        let mut first_q = None;
        while !self.finish_container(b']') {
            let b = self.bytes[self.pos];
            if b == b',' {
                self.pos += 1;
                continue;
            }
            if b == b'{' {
                let sub = self.parse_object_literal();
                if first_q.is_none() {
                    first_q = sub
                        .props
                        .get("q")
                        .or_else(|| sub.props.get("query"))
                        .and_then(|v| v.resolve_str(&self.bindings));
                }
            } else if matches!(b, b'\'' | b'"') {
                let s = self.read_quoted_string(b);
                self.record_literal(s.clone());
                if first_q.is_none() {
                    first_q = Some(s);
                }
            } else {
                self.skip_balanced_until(b",]");
            }
        }
        first_q
    }

    /// Advance until one of `terminators` is reached at depth 0, while still scanning nested
    /// `tools.*` calls and string/template literals.
    fn skip_balanced_until(&mut self, terminators: &[u8]) {
        let mut paren = 0usize;
        let mut bracket = 0usize;
        let mut brace = 0usize;
        while self.pos < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            let b = self.bytes[self.pos];
            if paren == 0 && bracket == 0 && brace == 0 && terminators.contains(&b) {
                return;
            }
            match b {
                b'(' => {
                    paren += 1;
                    self.pos += 1;
                    self.regex_allowed = true;
                }
                b')' => {
                    if paren == 0 {
                        return;
                    }
                    paren -= 1;
                    self.pos += 1;
                    self.regex_allowed = false;
                }
                b'[' => {
                    bracket += 1;
                    self.pos += 1;
                    self.regex_allowed = true;
                }
                b']' => {
                    if bracket == 0 {
                        return;
                    }
                    bracket -= 1;
                    self.pos += 1;
                    self.regex_allowed = false;
                }
                b'{' => {
                    brace += 1;
                    self.pos += 1;
                    self.regex_allowed = true;
                }
                b'}' => {
                    if brace == 0 {
                        return;
                    }
                    brace -= 1;
                    self.pos += 1;
                    self.regex_allowed = false;
                }
                _ => self.step_statement_or_expr(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collect_ops(harness: Option<Harness>, name: &str, val: &serde_json::Value) -> Vec<(String, ToolOp<'static>)> {
        let mut out = Vec::new();
        visit_tool_ops(harness, name, JsonView::from(val), ClassifyOpts::FULL, |sem| {
            let owned = sem.into_owned();
            out.push((owned.tool.into_owned(), owned.op));
        })
        .unwrap();
        out
    }

    #[test]
    fn wide_patch_dedup_preserves_first_seen_order_and_primary_precedence() {
        let mut patch = Patch::default();
        patch.push("--- a/fallback\n+++ b/fallback\n");
        for i in (0..2000).rev() {
            patch.push(&format!("*** Update File: file-{i}\n*** Update File: file-{i}\n"));
        }
        assert_eq!(patch.primary.len(), 2000);
        assert_eq!(
            patch.finish().into_iter().map(|(p, _)| p).collect::<Vec<_>>(),
            (0..2000).rev().map(|i| format!("file-{i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn trigger_matches_whole_string_across_every_boundary() {
        for text in [
            "λ *** Begin Patch",
            "  apply_patch",
            "  *** Begin Patch\n*** Update File: a.rs",
            "apply_patch_suffix",
            "apply_patc",
            "ordinary",
            "\u{2003}apply_patch",
        ] {
            let expected = text.contains("*** Begin Patch") || text.trim_start().starts_with("apply_patch");
            for split in text.char_indices().map(|(at, _)| at).chain(std::iter::once(text.len())) {
                let mut trigger = Trigger::default();
                trigger.push(&text[..split]);
                trigger.push(&text[split..]);
                assert_eq!(trigger.marker || trigger.apply, expected, "{text:?}, split {split}");
            }
            let mut trigger = Trigger::default();
            for ch in text.chars() {
                trigger.push(ch.encode_utf8(&mut [0; 4]));
            }
            assert_eq!(trigger.marker || trigger.apply, expected);
        }
    }

    #[test]
    fn chunked_patch_yields_the_same_targets_at_every_width() {
        for (text, expected) in [
            (
                "--- a/a\n+++ b/a\n*** Add File: later\n+body\n*** Move to: renamed\n*** Add File: later\n",
                vec!["later", "renamed"],
            ),
            ("--- a/a\n+++ b/a\n+++ b/b\n+++ /dev/null\n", vec!["a", "b"]),
            ("+++ b/not-a-patch\n+irrelevant\n", vec![]),
            (
                "\u{2003}*** Update File: λ.rs\r\n*** Delete File: /dev/null\n",
                vec!["λ.rs"],
            ),
        ] {
            for width in 1..32 {
                let mut patch = Patch::default();
                let mut start = 0;
                for (offset, _) in text.char_indices() {
                    if offset - start >= width {
                        patch.push(&text[start..offset]);
                        start = offset;
                    }
                }
                patch.push(&text[start..]);
                assert_eq!(
                    patch.finish().into_iter().map(|(p, _)| p).collect::<Vec<_>>(),
                    expected,
                    "width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn codex_exec_recovers_from_mismatched_container_closers() {
        // Each specialized container reader must yield on an enclosing delimiter, including
        // when the expression skipper reaches it after a property, spread, or nested value.
        let prefixes = [
            "tools.exec_command({",
            "tools.exec_command({cmd:",
            "tools.exec_command({cmd: value",
            "tools.exec_command({...value",
            "tools.exec_command({[key]: value",
            "tools.exec_command({method() {}",
            "const parts = [",
            "const parts = [value",
            "tools.update_plan({plan: [",
            "tools.update_plan({plan: [value",
            "tools.web__run({search_query: [",
            "tools.web__run({search_query: [value",
        ];
        for prefix in prefixes {
            for close in [")", "]", "}", "", " /* unfinished"] {
                let script = format!("{prefix}{close}\n; tools.exec_command({{cmd: 'recovered'}});");
                let mut scanner = JsScanner::new(&script);
                scanner.scan_all();
                assert_eq!(scanner.pos, script.len(), "{script}");
                if !close.contains("/*") {
                    assert!(
                        scanner.calls.iter().any(|call| matches!(call,
                            JsCall::ExecCommand { cmd: Some(cmd), .. } if cmd == "recovered"
                        )),
                        "{script}"
                    );
                }
            }
        }
    }

    #[test]
    fn codex_exec_deep_nesting_resumes_after_bounded_extraction() {
        for (open, close) in [
            ("tools.exec_command({cmd:", "})"),
            ("{plan:[", "]}"),
            ("{search_query:[", "]}"),
            ("`${", "}`"),
            ("const a = [(() => {", "return 0; })()];"),
            ("const a = ['x'].join((() => {", "return ''; })());"),
        ] {
            let script = "tools.update_plan(".to_owned()
                + &open.repeat(5000)
                + "'leaf'"
                + &close.repeat(5000)
                + ")"
                + "; tools.exec_command({cmd: 'after'});";
            let mut scanner = JsScanner::new(&script);
            scanner.scan_all();
            assert_eq!(scanner.pos, script.len());
            assert_eq!(scanner.nesting, 0);
            assert!(scanner.calls.iter().any(|call| matches!(call,
                JsCall::ExecCommand { cmd: Some(cmd), .. } if cmd == "after"
            )));
        }
    }

    #[test]
    fn codex_exec_malformed_hex_escapes_preserve_utf8_boundaries() {
        for escape in [r"\x🦀", r"\x€", r"\u€aa", r"\u🦀a", r"\u{🦀}", r"\x", r"\u"] {
            for quote in ["'", "\"", "`"] {
                let script = format!(
                    "tools.exec_command({{cmd: {quote}{escape}{quote}}}); tools.exec_command({{cmd: 'after'}});"
                );
                let mut scanner = JsScanner::new(&script);
                scanner.scan_all();
                assert_eq!(scanner.pos, script.len(), "{script}");
                assert!(
                    scanner.calls.iter().any(|call| matches!(call,
                        JsCall::ExecCommand { cmd: Some(cmd), .. } if cmd == "after"
                    )),
                    "{script}"
                );
            }
        }
        let mut scanner = JsScanner::new(r"'\x41\u0042\u{1F980}'");
        scanner.scan_all();
        assert_eq!(scanner.literals, ["AB🦀"]);
    }

    #[test]
    fn codex_exec_extracts_unquoted_commands_variable_bound_patches_and_multi_tool_batches() {
        // 1. Variable-bound array-join `apply_patch(patch)` (76.1% real-world shape) + `exec_command` gate.
        let script = r#"
// @exec: {"yield_time_ms": 30000}
const root = "/repo/project";
const patch = [
  "*** Begin Patch",
  "*** Update File: crates/cv-core/src/lib.rs",
  "*** Move to: crates/cv-core/src/new_lib.rs",
  "@@",
  "-old();",
  "+new();",
  "*** Add File: /abs/extra.rs",
  "+pub fn extra() {}",
  "*** End Patch",
].join("\n");
const clean = patch.replace(/"/g, '"');
const res = await tools.apply_patch(patch);
const test = await tools.exec_command({
  cmd: `cargo test -p clustervision-core --lib`,
  workdir: root,
});
text(test.output ?? res);
"#;
        let ops = collect_ops(Some(Harness::Codex), "exec", &json!(script));
        assert_eq!(
            ops,
            vec![
                (
                    "apply_patch".into(),
                    ToolOp::FileEdit {
                        path: "crates/cv-core/src/lib.rs".into(),
                        is_full_write: false,
                    }
                ),
                (
                    "apply_patch".into(),
                    ToolOp::FileEdit {
                        path: "crates/cv-core/src/new_lib.rs".into(),
                        is_full_write: false,
                    }
                ),
                (
                    "apply_patch".into(),
                    ToolOp::FileEdit {
                        path: "/abs/extra.rs".into(),
                        is_full_write: false,
                    }
                ),
                (
                    "exec_command".into(),
                    ToolOp::Command {
                        cmd: Some("cargo test -p clustervision-core --lib".into()),
                        workdir: Some("/repo/project".into()),
                        background: false,
                    }
                ),
            ]
        );

        // 2. Dynamic mutated array fallback: `lines.push(...)` before `tools.apply_patch(lines.join("\n"))`.
        let dynamic_patch = r#"
const lines = ["*** Begin Patch"];
lines.push("*** Delete File: obsolete.rs");
lines.push("*** End Patch");
await tools.apply_patch(lines.join("\n"));
"#;
        let ops = collect_ops(Some(Harness::Codex), "exec", &json!({"raw": dynamic_patch}));
        assert_eq!(
            ops,
            vec![(
                "apply_patch".into(),
                ToolOp::FileEdit {
                    path: "obsolete.rs".into(),
                    is_full_write: false,
                }
            )]
        );

        // 3. Multi-tool orchestration (`update_plan` + `create_goal` + `write_stdin` + `view_image` + `web__run`).
        let multi = r#"
await tools.update_plan({
  explanation: "Verify parser",
  plan: [
    { step: "Audit JS lexer", status: "completed" },
    { step: "Run corpus tests", status: "in_progress" },
  ],
});
await tools.create_goal({ objective: "Zero blind exec calls" });
await Promise.all([
  tools.write_stdin({ session_id: 17487, chars: "" }),
  tools.view_image({ path: "/tmp/diagram.png" }),
  tools.web__run({ search_query: [{ q: "rust tantivy" }] }),
]);
"#;
        let ops = collect_ops(Some(Harness::Codex), "exec", &json!(multi));
        assert_eq!(ops.len(), 5);
        assert!(matches!(
            &ops[0],
            (tool, ToolOp::Affordance { kind: Kind::Plan, action: "write", items, .. })
                if tool == "update_plan" && items.len() == 2
        ));
        assert!(matches!(
            &ops[1],
            (tool, ToolOp::Affordance { kind: Kind::Goal, action: "create", summary, .. })
                if tool == "create_goal" && summary == "Zero blind exec calls"
        ));
        assert_eq!(
            ops[2],
            (
                "write_stdin".into(),
                ToolOp::Background {
                    job_id: Some("17487".into()),
                    action: BgAction::Poll,
                }
            )
        );
        assert_eq!(
            ops[3],
            (
                "view_image".into(),
                ToolOp::FileRead {
                    path: "/tmp/diagram.png".into()
                }
            )
        );
        assert_eq!(
            ops[4],
            (
                "web__run".into(),
                ToolOp::Search {
                    path: None,
                    query: Some("rust tantivy".into()),
                }
            )
        );
    }

    #[test]
    fn cross_harness_antigravity_and_codex_tools_classify_uniformly() {
        assert_eq!(
            collect_ops(
                Some(Harness::Gemini),
                "run_command",
                &json!({"CommandLine": "cargo check", "Cwd": "/repo"})
            ),
            vec![(
                "run_command".into(),
                ToolOp::Command {
                    cmd: Some("cargo check".into()),
                    workdir: Some("/repo".into()),
                    background: false,
                }
            )]
        );
        assert_eq!(
            collect_ops(
                Some(Harness::Gemini),
                "write_file",
                &json!({"Path": "/repo/notes.md", "Content": "# Notes"})
            ),
            vec![(
                "write_file".into(),
                ToolOp::FileEdit {
                    path: "/repo/notes.md".into(),
                    is_full_write: true,
                }
            )]
        );
        assert_eq!(
            collect_ops(
                Some(Harness::Gemini),
                "invoke_subagent",
                &json!({"TypeName": "research", "Role": "Auditor", "Prompt": "check"})
            ),
            vec![(
                "invoke_subagent".into(),
                ToolOp::SubagentSpawn {
                    target_hint: Some("Auditor".into()),
                    has_prompt: true,
                }
            )]
        );
        assert_eq!(
            collect_ops(
                Some(Harness::Codex),
                "send_message",
                &json!({"target": "/root/worker", "message": "proceed"})
            ),
            vec![(
                "send_message".into(),
                ToolOp::AgentMessage {
                    recipient: Some("/root/worker".into()),
                }
            )]
        );
    }

    #[test]
    fn normalized_hunk_hashes_survive_indentation_rebase_and_multi_file_patches() {
        let collect_hunks = |name: &str, payload: &serde_json::Value| {
            let j = crate::Json::from(payload.clone());
            j.with_view(|view| {
                let mut out = Vec::new();
                visit_tool_ops_with_hunks(None, name, view, ClassifyOpts::EVENTS, |sem, hunks| {
                    if let ToolOp::FileEdit { path, .. } = sem.op {
                        out.push((path.into_owned(), hunks));
                    }
                })?;
                Ok::<_, anyhow::Error>(out)
            })
            .unwrap()
        };

        // 1. Edit vs re-indented git diff hunk: exact block hash AND nontrivial line hashes match,
        // while trivial brace-only lines (`}`) do not emit standalone line hashes.
        let edit_hunks = collect_hunks(
            "Edit",
            &json!({
                "file_path": "/repo/src/lib.rs",
                "old_string": "fn old() {}",
                "new_string": "  pub fn verify_hunk(x: u64) -> bool {\n      x != 0\n  }\n"
            }),
        );
        assert_eq!(edit_hunks.len(), 1);
        let rebased_diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,1 +42,3 @@
-    fn old() {}
+        pub fn verify_hunk(x: u64) -> bool {
+            x != 0
+        }
";
        let diff_hashes = extract_diff_hunk_hashes(rebased_diff);
        assert_eq!(
            edit_hunks[0].1, diff_hashes,
            "re-indented diff produces identical 64-bit hunk & line hashes"
        );
        // 1 block hash + 2 nontrivial lines (`pub fn verify_hunk...`, `x != 0`); `}` is excluded as standalone line.
        assert_eq!(diff_hashes.len(), 3);
        let line_hash = hash_normalized_line("            x != 0").unwrap();
        assert!(
            edit_hunks[0].1.contains(&line_hash),
            "-L line-range lookup matches normalized line hash"
        );
        assert!(
            hash_normalized_line("   }  ").is_none(),
            "brace-only boilerplate must not match standalone"
        );

        // 2. Multi-file apply_patch isolates hunk fingerprints per target file.
        let patch = "\
*** Begin Patch
*** Update File: crates/a.rs
@@
-old_a
+let alpha_signal = compute_alpha();
*** Update File: crates/b.rs
@@
-old_b
+let beta_signal = compute_beta();
*** End Patch";
        let patched = collect_hunks("apply_patch", &json!({ "patch": patch }));
        assert_eq!(patched.len(), 2);
        assert_eq!(patched[0].0, "crates/a.rs");
        assert_eq!(patched[1].0, "crates/b.rs");
        assert!(patched[0]
            .1
            .contains(&hash_normalized_line("let alpha_signal = compute_alpha();").unwrap()));
        assert!(!patched[0]
            .1
            .contains(&hash_normalized_line("let beta_signal = compute_beta();").unwrap()));
        assert!(patched[1]
            .1
            .contains(&hash_normalized_line("let beta_signal = compute_beta();").unwrap()));

        // 3. MultiEdit / Antigravity multi_replace_file_content aggregates distinct chunk hashes cleanly.
        let multi = collect_hunks(
            "multi_replace_file_content",
            &json!({
                "TargetFile": "/repo/src/main.rs",
                "ReplacementChunks": [
                    { "TargetContent": "a", "ReplacementContent": "const FIRST_CHUNK: usize = 1;" },
                    { "TargetContent": "b", "ReplacementContent": "const SECOND_CHUNK: usize = 2;" }
                ]
            }),
        );
        assert_eq!(multi.len(), 1);
        assert!(multi[0]
            .1
            .contains(&hash_normalized_line("const FIRST_CHUNK: usize = 1;").unwrap()));
        assert!(multi[0]
            .1
            .contains(&hash_normalized_line("const SECOND_CHUNK: usize = 2;").unwrap()));
    }

    #[test]
    fn antigravity_1_0_and_1_5_and_pi_v3_tools_classify_hunks_and_compact_labels() {
        // Large payload guards cover Antigravity 1.0.x/1.5.x and Pi v3 file-mutation tools.
        for (h, tool) in [
            (Some(Harness::Antigravity), "write_file"),
            (Some(Harness::Antigravity), "write_to_file"),
            (Some(Harness::Antigravity), "edit_file"),
            (Some(Harness::Antigravity), "replace_file_content"),
            (Some(Harness::Antigravity), "multi_replace_file_content"),
            (Some(Harness::Pi), "write"),
            (Some(Harness::Pi), "edit"),
        ] {
            assert!(is_large_payload_tool(h, tool), "{tool} must stream via JsonView");
        }

        // Antigravity file reads across 1.0.x (AbsolutePath, AbsolutePathUri, File) and 1.5.x (Path).
        for (tool, payload, want_path) in [
            ("read_file", json!({"Path": "/repo/src/lib.rs"}), "/repo/src/lib.rs"),
            (
                "view_file",
                json!({"AbsolutePath": "/repo/src/view.rs", "StartLine": 1, "EndLine": 50}),
                "/repo/src/view.rs",
            ),
            (
                "view_file",
                json!({"AbsolutePathUri": "file:///repo/my%20dir/uri.rs"}),
                "/repo/my dir/uri.rs",
            ),
            (
                "view_file_outline",
                json!({"AbsolutePath": "/repo/src/outline.rs"}),
                "/repo/src/outline.rs",
            ),
            (
                "view_code_item",
                json!({"File": "/repo/src/item.rs", "NodePaths": ["Foo.bar"]}),
                "/repo/src/item.rs",
            ),
        ] {
            assert_eq!(
                collect_ops(Some(Harness::Antigravity), tool, &payload),
                vec![(tool.into(), ToolOp::FileRead { path: want_path.into() })]
            );
        }

        // Antigravity file edits + `Append` semantics + normalized hunk fingerprints.
        let collect_edit = |h: Harness, name: &str, payload: serde_json::Value| {
            let dir = std::env::temp_dir().join(format!("cv-classify-src-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("arg.json");
            let raw = serde_json::to_vec(&payload).unwrap();
            std::fs::write(&p, &raw).unwrap();
            let src = crate::Json::source(&p, 0, raw.len() as u64).unwrap();
            let res = src
                .with_view(|view| {
                    let mut out = Vec::new();
                    visit_tool_ops_with_hunks(Some(h), name, view, ClassifyOpts::FULL, |sem, hunks| {
                        out.push((sem.into_owned().op, hunks));
                    })?;
                    Ok(out)
                })
                .unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            res
        };

        let wf_full = collect_edit(
            Harness::Antigravity,
            "write_file",
            json!({"Path": "/repo/a.rs", "Content": "pub fn full_write_alpha() {}\n"}),
        );
        assert_eq!(
            wf_full[0].0,
            ToolOp::FileEdit {
                path: "/repo/a.rs".into(),
                is_full_write: true
            }
        );
        assert!(wf_full[0]
            .1
            .contains(&hash_normalized_line("pub fn full_write_alpha() {}").unwrap()));

        let wf_append = collect_edit(
            Harness::Antigravity,
            "write_file",
            json!({"Path": "/repo/a.rs", "Content": "pub fn appended_tail() {}\n", "Append": true}),
        );
        assert_eq!(
            wf_append[0].0,
            ToolOp::FileEdit {
                path: "/repo/a.rs".into(),
                is_full_write: false
            }
        );
        assert!(wf_append[0]
            .1
            .contains(&hash_normalized_line("pub fn appended_tail() {}").unwrap()));

        let wtf = collect_edit(
            Harness::Antigravity,
            "write_to_file",
            json!({"TargetFile": "/repo/b.rs", "CodeContent": "pub fn legacy_write_to_file() {}\n"}),
        );
        assert_eq!(
            wtf[0].0,
            ToolOp::FileEdit {
                path: "/repo/b.rs".into(),
                is_full_write: true
            }
        );
        assert!(wtf[0]
            .1
            .contains(&hash_normalized_line("pub fn legacy_write_to_file() {}").unwrap()));

        let ef = collect_edit(
            Harness::Antigravity,
            "edit_file",
            json!({"Path": "/repo/c.rs", "Target": "old()", "Replacement": "new_edit_file_call();"}),
        );
        assert_eq!(
            ef[0].0,
            ToolOp::FileEdit {
                path: "/repo/c.rs".into(),
                is_full_write: false
            }
        );
        assert!(ef[0]
            .1
            .contains(&hash_normalized_line("new_edit_file_call();").unwrap()));

        let rfc = collect_edit(
            Harness::Antigravity,
            "replace_file_content",
            json!({
                "TargetFile": "/repo/d.rs",
                "TargetContent": "old_block()",
                "ReplacementContent": "new_replace_file_content();"
            }),
        );
        assert_eq!(
            rfc[0].0,
            ToolOp::FileEdit {
                path: "/repo/d.rs".into(),
                is_full_write: false
            }
        );
        assert!(rfc[0]
            .1
            .contains(&hash_normalized_line("new_replace_file_content();").unwrap()));

        // Pi v3 `write` and `edit` (`oldText`/`newText` + `edits[]`) hunk extraction.
        let pi_edit_single = collect_edit(
            Harness::Pi,
            "edit",
            json!({"path": "src/pi.rs", "oldText": "x = 1", "newText": "let x = compute_pi_single();"}),
        );
        assert_eq!(
            pi_edit_single[0].0,
            ToolOp::FileEdit {
                path: "src/pi.rs".into(),
                is_full_write: false
            }
        );
        assert!(pi_edit_single[0]
            .1
            .contains(&hash_normalized_line("let x = compute_pi_single();").unwrap()));

        let pi_edit_multi = collect_edit(
            Harness::Pi,
            "edit",
            json!({
                "path": "src/pi.rs",
                "edits": [
                    {"oldText": "a", "newText": "const PI_MULTI_ONE: u32 = 1;"},
                    {"oldText": "b", "newText": "const PI_MULTI_TWO: u32 = 2;"}
                ]
            }),
        );
        assert!(pi_edit_multi[0]
            .1
            .contains(&hash_normalized_line("const PI_MULTI_ONE: u32 = 1;").unwrap()));
        assert!(pi_edit_multi[0]
            .1
            .contains(&hash_normalized_line("const PI_MULTI_TWO: u32 = 2;").unwrap()));

        // Commands & background task controls (`run_command`, `manage_task`, `command_status`, `send_command_input`).
        let mut daemon_events = Vec::new();
        visit_tool_ops(
            Some(Harness::Antigravity),
            "run_command",
            JsonView::from(&json!({"CommandLine": "cargo watch -x check", "Cwd": "/repo", "IsDaemon": true})),
            ClassifyOpts::EVENTS,
            |sem| daemon_events.push(sem.into_owned().op),
        )
        .unwrap();
        assert_eq!(
            daemon_events,
            vec![ToolOp::Command {
                cmd: Some("cargo watch -x check".into()),
                workdir: Some("/repo".into()),
                background: true,
            }]
        );

        for (tool, payload, want_id, want_action) in [
            ("manage_task", json!({"Action": "list"}), None, BgAction::Poll),
            (
                "manage_task",
                json!({"Action": "status", "TaskId": "s/task-2"}),
                Some("s/task-2"),
                BgAction::Poll,
            ),
            (
                "manage_task",
                json!({"Action": "send_input", "TaskId": "s/task-2", "Input": "y\n"}),
                Some("s/task-2"),
                BgAction::Interact,
            ),
            (
                "manage_task",
                json!({"Action": "kill", "TaskId": "s/task-4"}),
                Some("s/task-4"),
                BgAction::Stop,
            ),
            (
                "command_status",
                json!({"CommandId": "cmd-9"}),
                Some("cmd-9"),
                BgAction::Poll,
            ),
            (
                "send_command_input",
                json!({"CommandId": "cmd-9", "Input": "ok\n", "Terminate": false}),
                Some("cmd-9"),
                BgAction::Interact,
            ),
            (
                "send_command_input",
                json!({"CommandId": "cmd-9", "Terminate": true}),
                Some("cmd-9"),
                BgAction::Stop,
            ),
        ] {
            assert_eq!(
                collect_ops(Some(Harness::Antigravity), tool, &payload),
                vec![(
                    tool.into(),
                    ToolOp::Background {
                        job_id: want_id.map(Into::into),
                        action: want_action,
                    }
                )]
            );
        }

        // Search / web / directory across Antigravity and Pi.
        for (h, tool, payload, want_path, want_query) in [
            (
                Some(Harness::Antigravity),
                "grep_search",
                json!({"Query": "ToolSemantic", "SearchPath": "/repo/crates"}),
                Some("/repo/crates"),
                Some("ToolSemantic"),
            ),
            (
                Some(Harness::Antigravity),
                "find_by_name",
                json!({"Pattern": "*.rs", "SearchDirectory": "/repo/src"}),
                Some("/repo/src"),
                Some("*.rs"),
            ),
            (
                Some(Harness::Antigravity),
                "list_dir",
                json!({"DirectoryPath": "/repo/docs"}),
                Some("/repo/docs"),
                None,
            ),
            (
                Some(Harness::Antigravity),
                "search_web",
                json!({"query": "rust tantivy", "domain": "docs.rs"}),
                None,
                Some("rust tantivy"),
            ),
            (
                Some(Harness::Antigravity),
                "read_url_content",
                json!({"Url": "https://docs.rs/tantivy"}),
                None,
                Some("https://docs.rs/tantivy"),
            ),
            (
                Some(Harness::Pi),
                "grep",
                json!({"pattern": "fn main", "path": "src"}),
                Some("src"),
                Some("fn main"),
            ),
            (
                Some(Harness::Pi),
                "find",
                json!({"pattern": "*.toml", "path": "."}),
                Some("."),
                Some("*.toml"),
            ),
            (
                Some(Harness::Pi),
                "ls",
                json!({"path": "crates/cv-core"}),
                Some("crates/cv-core"),
                None,
            ),
        ] {
            assert_eq!(
                collect_ops(h, tool, &payload),
                vec![(
                    tool.into(),
                    ToolOp::Search {
                        path: want_path.map(Into::into),
                        query: want_query.map(Into::into),
                    }
                )]
            );
        }

        // Orchestration: post_message / send_message / ask_question / schedule / run_workflow.
        assert_eq!(
            collect_ops(
                Some(Harness::Antigravity),
                "post_message",
                &json!({"text": "@coordinator [done] green"})
            ),
            vec![(
                "post_message".into(),
                ToolOp::AgentMessage {
                    recipient: Some("board".into())
                }
            )]
        );
        assert_eq!(
            collect_ops(
                Some(Harness::Antigravity),
                "send_message",
                &json!({"Recipient": "worker-uuid", "Message": "proceed"})
            ),
            vec![(
                "send_message".into(),
                ToolOp::AgentMessage {
                    recipient: Some("worker-uuid".into())
                }
            )]
        );
        assert_eq!(
            collect_ops(
                Some(Harness::Antigravity),
                "ask_question",
                &json!({"questions": [{"question": "Use zero-copy view?", "options": ["Yes", "No"]}]})
            ),
            vec![(
                "ask_question".into(),
                ToolOp::Affordance {
                    kind: Kind::Question,
                    action: "ask",
                    summary: "Use zero-copy view?".into(),
                    items: Vec::new(),
                }
            )]
        );
        assert_eq!(
            collect_ops(
                Some(Harness::Antigravity),
                "run_workflow",
                &json!({"Description": "Fan out crate checks", "ScriptPath": "/repo/wf.py"})
            ),
            vec![(
                "run_workflow".into(),
                ToolOp::Other {
                    path: None,
                    summary: Some("Fan out crate checks".into()),
                }
            )]
        );

        // Crisp `compact_label` summaries for `cv lanes` `last_tool` on both Antigravity and Pi.
        let label = |h: Option<Harness>, name: &str, val: serde_json::Value| {
            compact_label(h, name, JsonView::from(&val), 80).unwrap()
        };
        assert_eq!(
            label(Some(Harness::Pi), "read", json!({"path": "src/lib.rs", "offset": 1})),
            "read · src/lib.rs"
        );
        assert_eq!(
            label(
                Some(Harness::Pi),
                "edit",
                json!({"path": "src/lib.rs", "oldText": "a", "newText": "b"})
            ),
            "edit · src/lib.rs"
        );
        assert_eq!(
            label(
                Some(Harness::Pi),
                "bash",
                json!({"command": "cargo test -p cv-core", "timeout": 10})
            ),
            "bash · cargo test -p cv-core"
        );
        assert_eq!(
            label(
                Some(Harness::Antigravity),
                "run_command",
                json!({"CommandLine": "cargo test", "WaitMsBeforeAsync": 5000})
            ),
            "run_command · cargo test"
        );
        assert_eq!(
            label(
                Some(Harness::Antigravity),
                "post_message",
                json!({"text": "@coordinator [done] all tests pass"})
            ),
            "post_message · @coordinator [done] all tests pass"
        );
        assert_eq!(
            label(
                Some(Harness::Antigravity),
                "swarm",
                json!({"action": "add", "name": "alpha", "brief": "audit parser"})
            ),
            "swarm · add alpha"
        );
        assert_eq!(
            label(Some(Harness::Antigravity), "swarm", json!({"action": "status"})),
            "swarm · status"
        );
        assert_eq!(
            label(
                Some(Harness::Antigravity),
                "swarm",
                json!({"action": "kill", "name": "gamma"})
            ),
            "swarm · kill gamma"
        );
        assert_eq!(
            label(
                Some(Harness::Antigravity),
                "manage_task",
                json!({"Action": "kill", "TaskId": "task-4"})
            ),
            "manage_task · kill task-4"
        );
        assert_eq!(
            label(Some(Harness::Antigravity), "read_board", json!({"since": 42})),
            "read_board · since 42"
        );
        assert_eq!(
            label(Some(Harness::Antigravity), "swarm_wait", json!({"since": 42})),
            "swarm_wait · since 42"
        );
        assert_eq!(
            label(
                Some(Harness::Antigravity),
                "ask_question",
                json!({"questions": [{"question": "Proceed with merge?", "options": ["Yes", "No"]}]})
            ),
            "ask_question · Proceed with merge?"
        );
    }
}
