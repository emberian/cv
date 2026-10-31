# Inside the harness: MCP and hooks

`cv mcp` gives a **running coding agent** cv's powers: read other agents' sessions in this
project, across time and harnesses, and coordinate with live siblings. It is a
[Model Context Protocol](https://modelcontextprotocol.io) server (line-delimited JSON-RPC 2.0
over stdio) exposing the Read, Reshape, Export, System and task commands as tools, plus two
MCP-only tools. [Hooks](#hooks-cv-inside-a-running-session) let cv act inside a running session
with no tool call at all.

## Motivating use cases

- Prior work here: `ls(cwd='/myproj')`, then `show(id=…)`.
- Prior solutions: `search(query=…)`, or `pack(task=…)` for a context bundle ([search](search.md)).
- Live siblings: `ls`, `observe_stream`, `lanes`.
- Block until a session prints X: `await_omen(regex=…)`.
- Context pressure by source: `doctor(id=<your session>)`.

## Registering it

For Claude Code:

```sh
claude mcp add clustervision -- /absolute/path/to/cv mcp
```

Any MCP host: command is the absolute path to `cv`, argument `mcp`. stdout is the protocol;
diagnostics go to stderr. It implements `initialize`, `ping`, `tools/list` and `tools/call`
(protocol `2025-06-18`). A generated tool returns stdout as `content[0].text` (parse that for
JSON) and, on success, nonempty stderr as a second block.

## The tools are generated from the CLI

At startup the server reads its own command tree, the same data `cv schema --commands --json`
prints. Every command in the **Read**, **Reshape**, **Export** and **System** groups, and every
`cv task` subcommand, is one tool; bare parent commands and `hook` are not.

- Name: the command (`ls`; subcommands join with `_`: `task_open`). Description: clap's `about`.
- Each argument, positionals included, is a same-named property (`cat`: `session`,
  `tool_use_id`) with clap's help, `possible_values` as `enum`, and the CLI default.
- Required arguments are in `required`; a member of a one-of group (`task resolve`'s `choice` /
  `accept_default` / `confirm`) is not required alone.
- Repeatable arguments take a scalar or an array (`options: ["a", "b"]` → `--option a --option
  b`; `splice`'s `specs` fills the positional). Names stay when the flag differs: `for_who` is
  `--for`, `by_task` is `--by`.

A call runs `cv <command> <args…> --json` and returns its stdout. `--json` is appended only where
the command has it (`cat`, `export`, `blame`, `resume` return text), so `json` is not a property.
A non-zero exit is a tool error (`isError: true`) carrying stderr verbatim. An unknown argument,
missing required value or malformed boolean is JSON-RPC `-32602 Invalid params`; other
validation (conflicting options, bad numbers) is a tool error from the child. Tools omit
`--follow`: a `follow` argument is refused, naming `for`.

If this page and a live `tools/list` disagree, `tools/list` is right.

## Windows: `show` returns what you ask for

The [window flags](cli.md) are properties on `show` and `export`:

| Property | Meaning |
|---|---|
| `first` | the first N messages |
| `last` | the last N messages |
| `range` | `"A..B"` — messages A (inclusive) to B (exclusive); `"A.."`, `"..B"` |
| `around` + `context` | message N with K either side (default 5) |
| `max_bytes` | stop after N bytes of rendered output, then print a `… continue with --range <next>..` line |

A `show` naming none of these returns the whole transcript, as `cv show` does. The byte budget
cuts at message boundaries and always admits the first message, so it is not a hard cap.

## The roster

Generated from a live `tools/list`; all arguments are in [the command reference](reference.md).
"Runs" is the command a call becomes.
Reshape tools preserve their sources and most mint a **new** session id; `adopt` copies an agent
under its existing id into another parent; `resume` prints or runs a launch command.

{{#cv-mcp-tools}}

## What was removed in 0.11.0

No aliases: `list_sessions` and `project_sessions(cwd)` → `ls(cwd=…)`; `search_sessions` →
`search`; `read_session` → `show`; `recall` → `pack` or `search(semantic=true)`; `prune_session`
→ `prune`; `prune_retrieve` → `cat(session, tool_use_id)`; `doctor` → the generated `doctor`.

## The MCP-only tools

Requests run concurrently: a parked `await_omen` never blocks a later `show`.

### `await_omen`

Blocks until a new or appended message (text, thinking, tool use or result) in any session
matches a regex, then returns it. Only activity from now on counts, not the backlog.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `regex` | string | **required** | Rust regex matched against newly-emitted message text. |
| `harness` | string | — | Only watch this harness. |
| `cwd_contains` | string | — | Only watch sessions whose cwd contains this substring. |
| `timeout_secs` | number | `120` | Give up after this many seconds. |
| `interval_secs` | number | `2` | Poll interval (minimum 1). |

A match returns `{ matched: true, harness, id, cwd, role, matched_text, event }` (`event`:
`new_session` or `appended`); a timeout returns `{ matched: false, timed_out: true,
waited_secs }`. Example: `await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj')`.
For structured coordination use the task tools.

### `observe_stream`

Non-blocking: returns the messages appended since an opaque cursor, plus a new cursor. The first
call (no cursor) sets a baseline and returns no backlog.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `cwd_contains` | string | — | Only observe sessions whose cwd contains this substring. |
| `harness` | string | — | Only observe this harness. |
| `since_cursor` | string | — | Cursor from a previous call. Omit to baseline from the current tail. |
| `max_messages` | number | all | Max messages this call; the rest are flagged `more_pending`. |
| `char_cap` | number | none | Cap on total characters; the message crossing it ends in `…`, and the output gives `char_cap` and `truncated` (messages cut). |

It writes nothing. The cursor (version 2; a 0.17 cursor rebaselines) holds positions; reader
state stays in the `cv mcp` process. `rereads` counts sessions reread from their start, once per
changed session for a cursor from another process.

## The task substrate over MCP

Each `cv task <sub>` is the tool `task_<sub>` in [the roster](#the-roster), with the same
arguments and help ([tasks](tasks.md)). Results are what the CLI prints:

- A write returns its confirmation line (`✦ claimed 01a1… → claimed`; `task_open` ends with the
  new task's full id). A refused write is a tool error with the reducer's reason (`event
  rejected: … cannot apply claimed` for a lost claim race).
- `task_list` and `task_show` return `--json`; `task_list` covers non-terminal tasks from the
  last 14 days or involving you unless `all: true`. `task_inbox` returns `cv serve`'s inbox page,
  `{who, items, closed, hidden, counts}`.
- `task_events` returns JSON lines, the poll surface.

**Identity.** Calls record `CV_ENDPOINT=agent:<name>` from the agent's environment;
`task_claim`, `task_release`, `task_status` and `task_resolve` error without it or `from`.

## A typical flow

1. `ls(cwd='/myproj')`, `task_inbox()`.
2. `task_claim`, `task_note`.
3. `pack(task='refresh token rotation')`; `show(id=…, around=…)` or `cat(session, tool_use_id)`.
4. `task_done(id=…, observed='auth done, tests green in <session>')` (recorded as self-reported).
5. A sibling's `await_omen(regex='auth done', cwd_contains='/myproj')` wakes; the orchestrator
   sees the close in `cv task events --since <id>`.

The thirteen board tools (post, read, await, request, reply, replies, unanswered, claim,
release, claims, who, heartbeat, ack) were removed in 0.15.0; calling one errors with a pointer
to the task tools.

## Hooks: cv inside a running session

Hooks surface compactions with a way back, brief a new session on this directory's history,
ledger lanes, surface owed decisions at each prompt, and ask before editing a file another live
session is editing (Codex: deny).

```sh
cv hooks install claude                 # ~/.claude/settings.json (or $CLAUDE_CONFIG_DIR)
cv hooks install claude --scope project # ./.claude/settings.json
cv hooks install codex                  # ~/.codex/hooks.json (or $CODEX_HOME)
cv hooks install codex --scope project  # ./.codex/hooks.json
cv hooks show claude                    # what install would change; --json for the before/after
cv hooks remove claude                  # exactly cv's entries and the skill, nothing else
```

`install` is idempotent: it merges six entries (`lane` serves both sub-agent events) beside
existing `hooks` and copies the [`cv` skill](#the-skill) to `skills/cv/SKILL.md`. Each runs
`<absolute cv> hook <name> --harness <h>`; a reinstall from a moved binary replaces them. Invalid
JSON settings are left untouched and the command errors. Hooks apply to new sessions; `cv doctor`
shows them. Any `--harness` spelling works (`cc`, `claude-code`).

Verified contracts: Claude Code 2.1.296 ([hooks reference](https://code.claude.com/docs/en/hooks))
and Codex 0.163.0. Other harnesses error.

Codex skips untrusted handlers silently: accept cv's entries in `codex`'s startup hooks review
(automation: `codex exec --dangerously-bypass-hook-trust`); project scope also needs the
directory trusted. A moved binary or edited matcher makes an entry *modified*, untrusted again.
`cv hooks install|show codex` and `cv doctor` report each entry as trusted, untrusted or modified. cv reads Codex's own `[hooks.state]` table in
`~/.codex/config.toml` (key and hash checked against Codex 0.162.1's `hooks/list`).

### The five hooks

Each `cv hook <name>` reads hook JSON on stdin, prints one line or nothing, and exits 0 (errors
go to stderr); with a terminal on stdin it exits 2 with usage. None scans the fleet.

| event · matcher | command | prints |
|---|---|---|
| `SessionStart` · `compact` | `cv hook compact` | `additionalContext`: `context compacted (auto, 180,412 tokens) at message N — compaction #k`, then `cv show <id> --pre-compaction k`, `cv show <id> --kind prompt,answer --pre-compaction k` and `cv rewind <id> --at N-1` |
| `SessionStart` · `startup\|resume` | `cv hook start` | `additionalContext`: up to six of this directory's earlier sessions (id, age, messages, title, the last commit sha its transcript's tail shows) and the count of decisions owed |
| `SubagentStart`, `SubagentStop` (`async`) | `cv hook lane` | nothing; appends `{ts, event, session_id, agent_id, agent_type, transcript_path, agent_transcript_path, cwd, last_text}` to `$CLUSTERVISION_HOME/lanes.jsonl` |
| `UserPromptSubmit` | `cv hook decisions` | `additionalContext` only when `cv task inbox` has decisions owed to `$CV_ENDPOINT` (else `cv`): the count and the commands that answer them |
| `PreToolUse` · every tool (opt-in: `cv hooks install --retired`) | `cv hook retired` | `permissionDecision: "deny"` when the calling sub-agent (`agent_id`) is retired in `$CLUSTERVISION_HOME/retired.jsonl` (written by `cv distill --agent-of`, listed and reversed by `cv retire`), telling it its successor and to end its turn; the same for a `SendMessage` whose `to` is retired, naming the successor to send to; nothing (allow) otherwise |
| `PreToolUse` · `Edit\|Write` | `cv hook edit` | `permissionDecision: "ask"` with the session id and minutes since, when another session edited this exact path and its transcript changed in the last 30 minutes; nothing (allow) otherwise |

- Hooks answer inside the harness's turn, so each reads at most 8 MiB of transcript (streaming
  costs ~4.5 ms/MB against a 100 ms budget) and says in its text what it left unread.
- **compact** gives the exact index and ordinal up to 8 MiB; above, it reads the last 8 MiB for
  trigger and size, says so, and points at `cv show <id> --kind compaction_boundary,compaction_summary`.
  [`cv show --pre-compaction`](cli.md#cv-show) reads the compacted records;
  [`cv rewind --at`](cli.md#cv-rewind) resumes before them.
- **start** reads the catalog without a freshness probe: sessions newer than the last `cv ls` /
  `cv archive sync` are missing. The sha is the last `[branch sha]` line in the transcript's tail,
  the 8 MiB split across the rows; a row with none there says how far back it looked.
- **Codex** uses the same events; the edit matcher is `apply_patch`, and the guard checks every
  added, updated, removed or moved path. Codex rejects `ask`, so a live conflict is `deny`.
  Threads under one root (`parent_thread_id` headers, or the lane ledger for an unindexed child)
  count as one session, as a Claude session's lanes do, so a swarm editing its own files is never
  denied. To override, remove the edit handler from `hooks.json` (`cv hooks remove codex` removes
  all five). `cv rewind` supports only Claude sessions. Codex's forest includes children
  registered by installed hooks; older unregistered children are absent.
- **lane** writes the ledger [`cv lanes`](swarm.md) reads. A `stop` row at or after a lane's last
  turn makes it `stopped` with `status_source: hook_ledger`, before the transcript records it.
- **edit** looks up the exact path in the event index (`cv touched`'s table), so sessions not
  ingested by `cv index` or `cv events <id>` are invisible. It skips the calling session and agent.

Measured at 0.16.1 on a 6,704-session, 37 GB store: each hook takes 10–30 ms; `compact` takes
70–90 ms on a 16.5 MB transcript (hence the 8 MiB limit), 10 ms on the tail path.

### The skill

`crates/cv/skill/SKILL.md`, which `install` copies: when `rewind`, `show --pre-compaction`,
`distill`, `splice`, `adopt` and `lanes` apply, plus the table above. The copy must match the
binary's embedded bytes; `cv hooks show` flags a stale copy and `install` rewrites it.

### The compaction policy

`cv doctor`'s `Policy:` line combines user and project settings (project wins) and the
environment (wins over a settings `env` block): `autoCompactEnabled` (default on),
`idleCompaction` (default off; when enabled server-side, fires at 90% of a 200K-token floor),
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `DISABLE_AUTO_COMPACT`.
No hook can supply or replace the summary (`PreCompact` can only block), so `compact` makes the
loss visible and reversible, not preventable.

### Every other harness

Reading needs no hooks. cv ships hooks only for harnesses checked against their documentation.
