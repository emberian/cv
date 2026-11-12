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
    /// Populate human-readable summaries, search queries, spawn hints, and message recipients.
    pub summaries: bool,
}

impl ClassifyOpts {
    pub const COUNTS: Self = Self {
        commands: false,
        summaries: false,
    };
    pub const EVENTS: Self = Self {
        commands: true,
        summaries: false,
    };
    pub const FULL: Self = Self {
        commands: true,
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
        Some(Harness::Gemini) => matches!(bare, "delegate_to_agent" | "invoke_subagent" | "invoke_agent"),
        Some(Harness::Cursor) => matches!(bare, "Task" | "Agent" | "mcp_task"),
        Some(Harness::KimiCode) => matches!(bare, "Agent" | "AgentSwarm"),
        Some(
            Harness::Claude
            | Harness::Codex
            | Harness::Grok
            | Harness::OpenCode
            | Harness::Hermes
            | Harness::OpenClaw
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
                | "write_file"
                | "save-file"
                | "create_file"
                | "write_to_file"
                | "Edit"
                | "MultiEdit"
                | "NotebookEdit"
                | "edit_file"
                | "replace"
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
            scan_codex_exec(&script, opts, &mut |sem| emit(sem.into_owned()));
            return Ok(());
        }
    }

    let workdir = file_key(input, &["workdir", "cwd", "Cwd"])?;

    // 2. Orchestration tools: sub-agent spawns, peer/child messages, and background polling/stopping.
    if is_spawn_tool(harness, bare) {
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
            has_nonempty_string(input, &["prompt", "message", "Prompt", "task", "instructions"])?
        } else {
            false
        };
        emit(ToolSemantic {
            tool: Cow::Borrowed(name),
            op: ToolOp::SubagentSpawn {
                target_hint,
                has_prompt,
            },
        });
        return Ok(());
    }

    if matches!(
        bare,
        "SendMessage" | "send_message" | "followup_task" | "send_input" | "team_send_message"
    ) {
        let recipient = if opts.summaries {
            first_string_prefix(input, &["to", "target", "Recipient", "recipient", "agent_id"], 120)?.map(Cow::Owned)
        } else {
            None
        };
        emit(ToolSemantic {
            tool: Cow::Borrowed(name),
            op: ToolOp::AgentMessage { recipient },
        });
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
        emit(ToolSemantic {
            tool: Cow::Borrowed(name),
            op: ToolOp::Background { job_id, action },
        });
        return Ok(());
    }

    // 3. Patch tools (`apply_patch` and `*patch*`).
    let lname = bare.to_ascii_lowercase();
    if lname.contains("patch") {
        let files = patch_rows(input)?;
        if !files.is_empty() {
            for file in files {
                let path = resolve_workdir(&file, workdir.as_deref());
                emit(ToolSemantic {
                    tool: Cow::Borrowed(name),
                    op: ToolOp::FileEdit {
                        path: Cow::Owned(path),
                        is_full_write: false,
                    },
                });
            }
            return Ok(());
        }
        if bare == "apply_patch" {
            let summary = if opts.summaries {
                fallback_summary(input, 80)?.map(Cow::Owned)
            } else {
                None
            };
            emit(ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::Other { path: None, summary },
            });
            return Ok(());
        }
    }

    // 4. Dedicated search tools (`Grep`, `Glob`, `grep_search`, `find_by_name`, `glob`, `web_search`).
    if matches!(
        bare,
        "Grep" | "Glob" | "grep_search" | "find_by_name" | "glob" | "search_web" | "web_search"
    ) {
        let path = file_key(input, SEARCH_PATH_KEYS)?.map(|p| Cow::Owned(resolve_workdir(&p, workdir.as_deref())));
        let query = if opts.summaries {
            first_string_prefix(input, &["pattern", "query", "Query", "Pattern", "url"], 120)?.map(Cow::Owned)
        } else {
            None
        };
        emit(ToolSemantic {
            tool: Cow::Borrowed(name),
            op: ToolOp::Search { path, query },
        });
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
        let op = if is_editor_view {
            ToolOp::FileRead { path }
        } else if matches!(
            bare,
            "Write" | "write_file" | "save-file" | "create_file" | "write_to_file"
        ) || lname == "write"
        {
            ToolOp::FileEdit {
                path,
                is_full_write: true,
            }
        } else if matches!(
            bare,
            "Edit"
                | "MultiEdit"
                | "NotebookEdit"
                | "replace"
                | "edit_file"
                | "multi_replace_file_content"
                | "str_replace_based_edit_tool"
                | "str_replace_editor"
        ) || WRITE_HINTS.iter().any(|hint| lname.contains(hint))
        {
            ToolOp::FileEdit {
                path,
                is_full_write: false,
            }
        } else if matches!(
            bare,
            "Read"
                | "NotebookRead"
                | "read_file"
                | "view_file"
                | "view_file_outline"
                | "view_code_item"
                | "view_image"
                | "read_many_files"
        ) || READ_HINTS.iter().any(|hint| lname.contains(hint))
        {
            ToolOp::FileRead { path }
        } else {
            ToolOp::Other {
                path: Some(path),
                summary: None,
            }
        };
        emit(ToolSemantic {
            tool: Cow::Borrowed(name),
            op,
        });
        return Ok(());
    }

    // 6. Command tools (`command` / `cmd` / `script` / `CommandLine` / `action.command`).
    if let Some((command, files)) = command_rows(input, opts.commands)? {
        let background = if opts.summaries {
            bool_flag(input, "run_in_background")?
                || bool_flag(input, "is_background")?
                || bool_flag(input, "background")?
                || bool_flag(input, "IsDaemon")?
        } else {
            false
        };
        emit(ToolSemantic {
            tool: Cow::Borrowed(name),
            op: ToolOp::Command {
                cmd: command.map(Cow::Owned),
                workdir: workdir.clone().map(Cow::Owned),
                background,
            },
        });
        for file in files {
            let resolved = resolve_workdir(&file, workdir.as_deref());
            emit(ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::FileEdit {
                    path: Cow::Owned(resolved),
                    is_full_write: false,
                },
            });
        }
        return Ok(());
    }

    // 7. Structured affordance tools when harness is known.
    if let Some(h) = harness {
        if let Some((kind, action)) = crate::affordance::classify_tool_view(h, name, input)? {
            let summary = if opts.summaries {
                first_string_prefix(
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
                    ],
                    120,
                )?
                .unwrap_or_default()
            } else {
                String::new()
            };
            emit(ToolSemantic {
                tool: Cow::Borrowed(name),
                op: ToolOp::Affordance {
                    kind,
                    action,
                    summary,
                    items: Vec::new(),
                },
            });
            return Ok(());
        }
    }

    // 8. Fallback for unclassified / MCP tools.
    let summary = if opts.summaries {
        fallback_summary(input, 80)?.map(Cow::Owned)
    } else {
        None
    };
    emit(ToolSemantic {
        tool: Cow::Borrowed(name),
        op: ToolOp::Other { path: None, summary },
    });
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
        "write_stdin" | "send_command_input" => Some(BgAction::Interact),
        "TaskStop" | "KillShell" | "kill_command_or_subagent" | "interrupt_agent" | "close_agent" | "bg_kill" => {
            Some(BgAction::Stop)
        }
        _ => None,
    })
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
        "prompt",
        "Prompt",
        "message",
        "Message",
        "url",
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
    let head = crate::projection::Head::of_json(view, cap)?;
    Ok((!head.is_empty() && head != "{}").then_some(head))
}

/// One-line human-readable summary of a tool call (`"{name} · {arg}"`, or `"{name}"` when empty).
pub fn compact_label(harness: Option<Harness>, name: &str, input: JsonView<'_>, max: usize) -> Result<String> {
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
            ToolOp::Search { query: Some(q), .. } => Some(truncate(&q, max)),
            ToolOp::Search { path: Some(p), .. } => Some(truncate(&p, max)),
            ToolOp::Search { .. } => None,
            ToolOp::SubagentSpawn {
                target_hint: Some(hint),
                ..
            } => Some(truncate(&hint, max)),
            ToolOp::SubagentSpawn { .. } => None,
            ToolOp::AgentMessage {
                recipient: Some(to), ..
            } => Some(truncate(&to, max)),
            ToolOp::AgentMessage { .. } => None,
            ToolOp::Background { job_id: Some(id), .. } => Some(truncate(&id, max)),
            ToolOp::Background { .. } => None,
            ToolOp::Affordance { summary, .. } if !summary.is_empty() => Some(truncate(&summary, max)),
            ToolOp::Affordance { .. } => None,
            ToolOp::Other { summary: Some(s), .. } => Some(truncate(&s, max)),
            ToolOp::Other { path: Some(p), .. } => Some(truncate(&p, max)),
            ToolOp::Other { .. } => None,
        };
    })?;
    // Preserve fast direct key lookup if an affordance/other tool had a standard descriptive field.
    if first_arg.is_none() && !input.is_string()? {
        first_arg = fallback_summary(input, max)?;
    }
    Ok(match first_arg {
        Some(arg) if !arg.is_empty() => {
            if op_count > 1 && is_codex_exec(harness, bare_tool_name(name)) {
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

/// Extract every file path added, updated, deleted, or moved to in an `apply_patch` or unified diff.
pub fn patch_paths(patch_text: &str) -> Vec<String> {
    let mut patch = Patch::default();
    patch.push(patch_text);
    patch.finish()
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
        Harness::Gemini if matches!(bare, "delegate_to_agent" | "invoke_subagent" | "invoke_agent") => {
            json_id_field(trimmed, &["conversationId", "conversation_id", "agentId", "agent_id"])
                .or_else(|| claude_agent_id(trimmed))
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

/// Streaming patch-header state machine: retains only candidate header lines and deduplicated
/// output paths in first-seen order.
#[derive(Default)]
pub(crate) struct Patch {
    codex: Header,
    unified: Header,
    old: Old,
    primary: HashMap<String, usize>,
    fallback: HashMap<String, usize>,
    has_old: bool,
}

impl Patch {
    pub(crate) fn push(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            let line = part.strip_suffix('\n').unwrap_or(part);
            self.codex.push(line, HEADERS, true, true);
            self.unified.push(line, &["+++ "], false, true);
            if !self.has_old {
                self.old.push(line);
                self.has_old = self.old.found;
            }
            if part.ends_with('\n') {
                self.end_line();
            }
        }
    }

    fn end_line(&mut self) {
        fn add(out: &mut HashMap<String, usize>, value: &str) {
            let value = value.trim();
            if !value.is_empty() && value != "/dev/null" && !value.contains("${") && !out.contains_key(value) {
                out.insert(value.to_string(), out.len());
            }
        }
        if let Some(value) = self.codex.value.take() {
            add(&mut self.primary, &value);
        }
        if let Some(value) = self.unified.value.take() {
            add(&mut self.fallback, value.trim().trim_start_matches("b/"));
        }
        self.codex = Header::default();
        self.unified = Header::default();
        self.old = Old::default();
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        self.end_line();
        let paths = if !self.primary.is_empty() {
            self.primary
        } else if self.has_old {
            self.fallback
        } else {
            return Vec::new();
        };
        let mut ordered: Vec<_> = paths.into_iter().collect();
        ordered.sort_unstable_by_key(|(_, position)| *position);
        ordered.into_iter().map(|(path, _)| path).collect()
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

pub(crate) fn command_rows(view: JsonView<'_>, whole: bool) -> Result<Option<(Option<String>, Vec<String>)>> {
    let mut command = whole.then(String::new);
    let mut trigger = Trigger::default();
    let exists = command_chunks(view, |chunk| {
        if let Some(command) = &mut command {
            command.push_str(chunk);
        }
        trigger.push(chunk);
        Ok(())
    })?;
    if !exists {
        return Ok(None);
    }
    let files = if trigger.marker || trigger.apply {
        let mut patch = Patch::default();
        command_chunks(view, |chunk| {
            patch.push(chunk);
            Ok(())
        })?;
        patch
            .finish()
            .into_iter()
            .filter(|path| trigger.apply || std::path::Path::new(path).is_absolute())
            .collect()
    } else {
        Vec::new()
    };
    Ok(Some((command, files)))
}

fn command_patch_files(cmd: &str) -> Vec<String> {
    let mut trigger = Trigger::default();
    trigger.push(cmd);
    if !(trigger.marker || trigger.apply) {
        return Vec::new();
    }
    let mut patch = Patch::default();
    patch.push(cmd);
    patch
        .finish()
        .into_iter()
        .filter(|path| trigger.apply || std::path::Path::new(path).is_absolute())
        .collect()
}

pub(crate) fn patch_rows(view: JsonView<'_>) -> Result<Vec<String>> {
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
    let mut patch = Patch::default();
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

pub(crate) fn file_key(view: JsonView<'_>, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        if let Some(value) = view.get(key)? {
            if let Some(path) = input::string(value)? {
                return Ok((!path.is_empty()).then_some(path));
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
        direct_paths: Vec<String>,
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

fn scan_codex_exec(script: &str, opts: ClassifyOpts, emit: &mut dyn FnMut(ToolSemantic<'_>)) {
    let mut scanner = JsScanner::new(script);
    scanner.scan_all();

    if scanner.calls.is_empty() {
        let summary = opts.summaries.then(|| Cow::Owned(collapse_line(script, 80)));
        emit(ToolSemantic {
            tool: Cow::Borrowed("exec"),
            op: ToolOp::Other { path: None, summary },
        });
        return;
    }

    // Lazily computed script-wide patch header fallback for variable-bound `tools.apply_patch(patch)`.
    let mut fallback_patch_paths: Option<Vec<String>> = None;

    for call in scanner.calls {
        match call {
            JsCall::ExecCommand { cmd, workdir } => {
                let patch_files = cmd.as_deref().map(command_patch_files).unwrap_or_default();
                let clean_workdir = workdir.filter(|w| !w.is_empty() && !w.contains("${"));
                emit(ToolSemantic {
                    tool: Cow::Borrowed("exec_command"),
                    op: ToolOp::Command {
                        cmd: if opts.commands { cmd.map(Cow::Owned) } else { None },
                        workdir: clean_workdir.clone().map(Cow::Owned),
                        background: false,
                    },
                });
                for file in patch_files {
                    let path = resolve_workdir(&file, clean_workdir.as_deref());
                    emit(ToolSemantic {
                        tool: Cow::Borrowed("exec_command"),
                        op: ToolOp::FileEdit {
                            path: Cow::Owned(path),
                            is_full_write: false,
                        },
                    });
                }
            }
            JsCall::ApplyPatch { direct_paths, workdir } => {
                let clean_workdir = workdir.filter(|w| !w.is_empty() && !w.contains("${"));
                let paths = if !direct_paths.is_empty() {
                    direct_paths
                } else {
                    fallback_patch_paths
                        .get_or_insert_with(|| {
                            let mut patch = Patch::default();
                            for lit in &scanner.literals {
                                patch.push(lit);
                                patch.push("\n");
                            }
                            patch.finish()
                        })
                        .clone()
                };
                if paths.is_empty() {
                    emit(ToolSemantic {
                        tool: Cow::Borrowed("apply_patch"),
                        op: ToolOp::Other {
                            path: None,
                            summary: None,
                        },
                    });
                } else {
                    for file in paths {
                        let path = resolve_workdir(&file, clean_workdir.as_deref());
                        emit(ToolSemantic {
                            tool: Cow::Borrowed("apply_patch"),
                            op: ToolOp::FileEdit {
                                path: Cow::Owned(path),
                                is_full_write: false,
                            },
                        });
                    }
                }
            }
            JsCall::WriteStdin { session_id, has_chars } => {
                emit(ToolSemantic {
                    tool: Cow::Borrowed("write_stdin"),
                    op: ToolOp::Background {
                        job_id: if opts.summaries {
                            session_id.map(Cow::Owned)
                        } else {
                            None
                        },
                        action: if has_chars { BgAction::Interact } else { BgAction::Poll },
                    },
                });
            }
            JsCall::ViewImage { path, workdir } => {
                let clean_workdir = workdir.filter(|w| !w.is_empty() && !w.contains("${"));
                if let Some(p) = path.filter(|p| !p.is_empty() && !p.contains("${")) {
                    let resolved = resolve_workdir(&p, clean_workdir.as_deref());
                    emit(ToolSemantic {
                        tool: Cow::Borrowed("view_image"),
                        op: ToolOp::FileRead {
                            path: Cow::Owned(resolved),
                        },
                    });
                } else {
                    emit(ToolSemantic {
                        tool: Cow::Borrowed("view_image"),
                        op: ToolOp::Other {
                            path: None,
                            summary: None,
                        },
                    });
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
                emit(ToolSemantic {
                    tool: Cow::Borrowed("update_plan"),
                    op: ToolOp::Affordance {
                        kind: Kind::Plan,
                        action: "write",
                        summary,
                        items: if opts.summaries { items } else { Vec::new() },
                    },
                });
            }
            JsCall::Goal { tool, action, summary } => {
                emit(ToolSemantic {
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
                });
            }
            JsCall::SpawnAgent {
                target_hint,
                has_prompt,
            } => {
                emit(ToolSemantic {
                    tool: Cow::Borrowed("spawn_agent"),
                    op: ToolOp::SubagentSpawn {
                        target_hint: if opts.summaries {
                            target_hint.map(|h| Cow::Owned(truncate(&h, 120)))
                        } else {
                            None
                        },
                        has_prompt: opts.summaries && has_prompt,
                    },
                });
            }
            JsCall::AgentMessage { tool, recipient } => {
                emit(ToolSemantic {
                    tool: Cow::Borrowed(tool),
                    op: ToolOp::AgentMessage {
                        recipient: if opts.summaries {
                            recipient.map(|r| Cow::Owned(truncate(&r, 120)))
                        } else {
                            None
                        },
                    },
                });
            }
            JsCall::BgControl { tool, job_id, action } => {
                emit(ToolSemantic {
                    tool: Cow::Borrowed(tool),
                    op: ToolOp::Background {
                        job_id: if opts.summaries {
                            job_id.map(|j| Cow::Owned(truncate(&j, 80)))
                        } else {
                            None
                        },
                        action,
                    },
                });
            }
            JsCall::Question { tool, summary } => {
                emit(ToolSemantic {
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
                });
            }
            JsCall::WebRun { query } => {
                emit(ToolSemantic {
                    tool: Cow::Borrowed("web__run"),
                    op: ToolOp::Search {
                        path: None,
                        query: if opts.summaries { query.map(Cow::Owned) } else { None },
                    },
                });
            }
            JsCall::Other { tool, summary } => {
                emit(ToolSemantic {
                    tool: Cow::Owned(tool),
                    op: ToolOp::Other {
                        path: None,
                        summary: if opts.summaries { summary.map(Cow::Owned) } else { None },
                    },
                });
            }
        }
    }
}

struct JsScanner<'s> {
    src: &'s str,
    bytes: &'s [u8],
    pos: usize,
    regex_allowed: bool,
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

impl<'s> JsScanner<'s> {
    fn new(src: &'s str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            regex_allowed: true,
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
                if let Ok(code) = u8::from_str_radix(&self.src[self.pos..self.pos + 2], 16) {
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
                    if let Ok(code) = u32::from_str_radix(&self.src[self.pos..self.pos + 4], 16) {
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
        debug_assert_eq!(self.bytes[self.pos], b'`');
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'`' {
                self.pos += 1;
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
        out
    }

    /// Track simple `const x = "..."` or `const patch = ["...", "..."].join("\n")` bindings.
    fn try_read_binding(&mut self) {
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
        if let Some(val) = self.try_read_string_or_array_join() {
            if self.bindings.len() < 256 {
                self.bindings.insert(name, val);
            }
        }
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
                while self.pos < self.bytes.len() {
                    self.skip_ws_and_comments();
                    if self.pos >= self.bytes.len() {
                        break;
                    }
                    let cur = self.bytes[self.pos];
                    if cur == b']' {
                        self.pos += 1;
                        self.regex_allowed = false;
                        break;
                    }
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
                let mut direct_paths = Vec::new();
                if let Some(patch_body) = direct_str
                    .or_else(|| prop_str("input"))
                    .or_else(|| prop_str("patch"))
                    .or_else(|| prop_str("cmd"))
                {
                    direct_paths = patch_paths(&patch_body);
                }
                JsCall::ApplyPatch {
                    direct_paths,
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
    }

    fn parse_object_literal(&mut self) -> JsObj {
        debug_assert_eq!(self.bytes[self.pos], b'{');
        self.pos += 1;
        self.regex_allowed = true;
        let mut obj = JsObj::default();

        while self.pos < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            let b = self.bytes[self.pos];
            if b == b'}' {
                self.pos += 1;
                self.regex_allowed = false;
                break;
            }
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
        while self.pos < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            let b = self.bytes[self.pos];
            if b == b']' {
                self.pos += 1;
                self.regex_allowed = false;
                break;
            }
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
        while self.pos < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() {
                break;
            }
            let b = self.bytes[self.pos];
            if b == b']' {
                self.pos += 1;
                self.regex_allowed = false;
                break;
            }
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
            patch.finish(),
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
                assert_eq!(patch.finish(), expected, "width {width}: {text:?}");
            }
        }
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
}
