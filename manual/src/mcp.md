# Inside the harness: MCP and hooks

Most of clustervision is about *you* reading sessions after the fact. `cv mcp` is the
other direction: it hands the same powers to a **running coding agent** so it can read
*other* agents' sessions — within this project, back through time, and across harnesses
— and coordinate with siblings live. 🔮

It's a [Model Context Protocol](https://modelcontextprotocol.io) server speaking
line-delimited JSON-RPC 2.0 over stdio. Point Claude Code (or Codex, Gemini, …) at it and
the agent gets the Read, Reshape, Export, System and task commands as tools, plus two MCP-only tools
for live coordination. [Hooks](#hooks-cv-inside-a-running-session), further down, are the other
direction again: cv acting inside a running Claude Code session with no tool call at all.

## Motivating use cases

- **"What happened in this project before?"** — `ls(cwd='/myproj')` lists every session
  (any harness) whose recorded working directory is this project, newest first. The agent
  reads the relevant one with `show(id=…)`.
- **"Has anyone solved this before?"** — `search(query=…)` finds it by what was literally
  said; `pack(task=…)` compiles a whole context bundle out of the corpus. See
  [search](search.md).
- **"What's my sibling agent doing right now?"** — `ls` surfaces live sessions;
  `observe_stream` tails their output; `lanes` shows a session's sub-agent forest.
- **"Block until another session prints X."** — `await_omen(regex=…)` watches live
  message streams and returns when one matches.
- **"Why does my own context keep filling up?"** — `doctor(id=…)` attributes the pressure
  by source. Point it at your *own* session id.

## Registering it

The server is a `cv` subcommand. Register it with your host; for Claude Code:

```sh
claude mcp add clustervision -- /absolute/path/to/cv mcp
```

Any MCP host works — give it the absolute path to `cv` as the command and `mcp` as its one
argument. **stdout is the protocol channel**, so all diagnostics go to stderr. It implements
`initialize`, `ping`, `tools/list` and `tools/call` against protocol version `2025-06-18`, and
generated tools return stdout as the first text content block and, on success, nonempty stderr
as a second block. Parse `content[0].text` for JSON; keep diagnostics separate.
(Until 0.15.0 this was a separate
`cv-mcp` binary; the 0.15 shim was removed in 0.16.0.)

## The tools are generated from the CLI

There is no hand-maintained list of what cv can do inside `cv mcp`. At startup the server
reads its own command tree, in-process — exactly what

```sh
cv schema --commands --json
```

prints: clap's own introspection of the command tree — every visible command and
subcommand with its group, `about`, and each argument's kind (`flag` / `option` /
`positional`), actual long flag, repeatability, value type, possible values, default, help text and required-ness. Each
command in the **Read**, **Reshape**, **Export** and **System** groups, and every `cv task`
subcommand becomes one MCP tool, except bare parent commands and `hook` (its stdin is supplied
by Claude Code, not by an MCP call):

- the **tool name is the command name** (`ls`, `show`, `cat`, …; a subcommand joins with
  an underscore, `task_open`);
- the **tool description is clap's `about`**;
- **every argument becomes a JSON-schema property named after the argument** —
  positionals included, so `cat` takes `session` and `tool_use_id` as properties — with
  clap's help as the property description, `possible_values` as an `enum`, and the CLI
  default as `default`;
- **required arguments land in the schema's `required` list** (an argument in a one-of
  group, like `task resolve`'s `choice` / `accept_default` / `confirm`, is not required alone);
- repeatable arguments advertise **a scalar or an array**: a list repeats the actual CLI option
  (`options: ["a", "b"]` becomes `--option a --option b`) or fills a repeatable positional
  (`splice`'s `specs`). Property names stay unchanged when the CLI flag differs:
  `for_who` maps to `--for`, and `by_task` to `--by`.

A call runs the same `cv` executable as `cv <command> <args…> --json`, and its stdout comes
back as the tool result. The `--json` is appended only for commands that have the flag (so `cat`,
`export`, `blame`, `resume`, … return text), and `json` is therefore *not* a
property you set. A **non-zero exit is an MCP tool error** (`isError: true`) carrying the
child's stderr, so `cv`'s exit-2 "no session matching …" reaches the agent verbatim. An
unknown argument, missing required value or malformed boolean flag comes back as JSON-RPC
`-32602 Invalid params`. Other CLI validation, including conflicting options and invalid
numeric strings, runs in the child and returns a tool error.

Because the schema is the running binary's own, **tool names, flags, harness lists and
output shapes cannot drift from the CLI** — there is no second binary to find, and so none to
be stale. Add a command or a flag to `cv` and it appears over
MCP with no change here. The page you are reading is the one place that can go stale: if
it and a live `tools/list` ever disagree, the live reply is the authority.

## Windows: `show` never dumps a whole transcript by default

The [window flags](cli.md) are the same five words everywhere in cv, and they are
properties on the `show` tool (and on `export`, which has them too):

| Property | Meaning |
|---|---|
| `first` | the first N messages |
| `last` | the last N messages |
| `range` | `"A..B"` — messages A (inclusive) to B (exclusive); `"A.."`, `"..B"` |
| `around` + `context` | message N with K either side (default 5) |
| `max_bytes` | stop after N bytes of rendered output, then print a `… continue with --range <next>..` line |

Over MCP — and **only** over MCP — `show` gets a default window: when the caller passes
**none** of those five, `cv mcp` adds `--last 50 --max-bytes 200000`. A 400-message session
would otherwise land in the agent's context in one shot, which is exactly the problem
`doctor` exists to diagnose. Naming any selector (including `max_bytes` alone, or
`pre_compaction`, which sets its own window) suppresses the defaults entirely — asking for
the whole thing is allowed, it just has to be asked for. The byte budget cuts at message
boundaries and always admits the first selected message, so it is not a hard response-size cap.
A `kind` row view also suppresses
the defaults: it covers the whole session unless given a supported window. The `ls` / `search` `limit`
defaults are the CLI's, unchanged.

## The roster

Generated when this manual is built, from a live `tools/list` of the `cv mcp` beside it, so it
is the server's own answer. "Required" is the schema's `required` list (the full argument list is
in [the command reference](reference.md)); "Runs" is the command line a call becomes, with
`--json` where the command has it. Reshape tools preserve their sources; most produce a **new**
session id. `adopt` copies an
agent under its existing id into another parent; `resume` prints or runs a launch command.

{{#cv-mcp-tools}}

## What was removed in 0.11.0

The old hand-written session tools are gone, with no aliases. Each had a CLI command doing
the same job better; now there is one of each.

| Removed tool | Use instead |
|---|---|
| `list_sessions` | `ls` |
| `project_sessions(cwd)` | `ls(cwd=…)` |
| `search_sessions` | `search` |
| `read_session` | `show` (windowed by default) |
| `recall` | `pack` — the one "build context from the corpus" verb. For plain meaning-based hits, `search(semantic=true)`. |
| `prune_session` | `prune` |
| `prune_retrieve` | `cat(session, tool_use_id)` |

The hand-written `doctor` tool was replaced by the generated `cv doctor`, which takes the
same `id` and returns the same report plus everything the CLI has grown since.

## The MCP-only tools

These two are hand-written tools over the shared watcher: one waits for a regex match;
the other returns immediately with a cursor for the next poll. The CLI's `scry` and `watch`
commands expose continuous feeds instead of this request/response interface.

### `await_omen`

Block until any other agent's session emits a message matching a regex, then return it.
Watches **live** for new and appended messages — text, thinking, tool results, tool uses
— across harnesses, reacting only to activity from *now on* (not the backlog).

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `regex` | string | **required** | Rust regex matched against newly-emitted message text. |
| `harness` | string | — | Only watch this harness. |
| `cwd_contains` | string | — | Only watch sessions whose cwd contains this substring. |
| `timeout_secs` | number | `120` | Give up after this many seconds. |
| `interval_secs` | number | `2` | Poll interval (minimum 1). |

On a match returns `{ matched: true, harness, id, cwd, role, matched_text, event }`
(`event` is `new_session` or `appended`); on timeout returns
`{ matched: false, timed_out: true, waited_secs }`. *When to use:* wait on a sibling
agent's raw transcript — e.g. `await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj')`.
For explicit, structured coordination, use the task tools (below).

### `observe_stream`

The **non-blocking** sibling of `await_omen`: drain the newly-appended messages since an
opaque cursor and return immediately with a fresh one, so a senior orchestrator can poll a
junior's live activity on its own cadence. The first call (no cursor) records a baseline
and returns no backlog.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `cwd_contains` | string | — | Only observe sessions whose cwd contains this substring. |
| `harness` | string | — | Only observe this harness. |
| `since_cursor` | string | — | Cursor from a previous call. Omit to baseline from the current tail. |
| `max_messages` | number | `50` | Max messages this call; the rest are flagged `more_pending`. |
| `char_cap` | number | `16000` | Cap on total characters returned; the last message is truncated to fit. |

Read-only — it writes nothing.

Requests are dispatched concurrently, so a parked `await_omen` never
head-of-line-blocks a `show` issued after it.

## The task substrate over MCP

The [task substrate](tasks.md) is fully drivable over MCP, and the tools are **generated
from `cv task`** like every other command tool: each `cv task <sub>` is `task_<sub>` in
[the roster](#the-roster), with the same arguments and help as `cv task <sub> --help`. What
comes back is what the CLI prints:

- a write answers with its confirmation line (`✦ claimed 01a1… → claimed`; `task_open` puts the
  new task's full id on its own last line), and a refused one is a tool error carrying the
  reducer's reason (`event rejected: … cannot apply claimed` for the loser of a claim race);
- `task_list` and `task_show` answer with `--json` (the full task projection), and `task_inbox`
  with the one inbox page `cv serve` serves too (`{who, items, closed, hidden, counts}`, each item
  carrying `reason`, `effective_state`, `since` and `age`); `task_list` has the CLI's default scope (non-terminal,
  the last 14 days or involving you), and `all: true` lifts it;
- `task_events` answers with JSON lines, the poll surface.

**Identity.** The server inherits the agent's environment, so a spawner-set
`CV_ENDPOINT=agent:<name>` makes every bare call record the right actor. Identity-*bearing*
tools (`task_claim`, `task_release`, `task_status`, `task_resolve`) **error** when neither
`from` nor `CV_ENDPOINT` is set, naming the cure.

## A typical flow

1. An agent starts work in `/myproj`. It calls `ls(cwd='/myproj')` to see prior history, and
   `task_inbox()` to see what is assigned to it.
2. It `task_claim`s its task so no sibling picks it up, and `task_note`s what it is about to do.
3. Mid-task it hits a wall and calls `pack(task='refresh token rotation')` to compile
   prior work on it from the whole corpus, then `show(id=…, around=…)` on the winner to
   read the span in context — or `cat(session, tool_use_id)` to pull one tool output back
   in full.
4. It hands off: `task_done(id=…, observed='auth done, tests green in <session>')` — recorded as its
   own word, labeled self-reported.
5. A sibling that ran `await_omen(regex='auth done', cwd_contains='/myproj')` unblocks and picks
   up the next task; the orchestrator sees the close in `cv task events --since <id>`.

The thirteen board tools (post, read, await, request, reply, replies, unanswered, claim,
release, claims, who, heartbeat, ack) were removed in 0.15.0 with the coordination board; calling
one errors with a pointer at the task tools.

## Hooks: cv inside a running session

clustervision reads every harness's own on-disk store, so nothing has to be wired for it to
*see* your sessions. Hooks make it *act* inside a running session: a compaction becomes visible
and reversible from the model's own context, a new session starts knowing what happened in this
directory before, lanes land in a ledger with no model action, owed decisions surface when you
type, and an edit to a file another live session is working on asks first.

One command sets it up, and an agent can run it for itself:

```sh
cv hooks install claude                 # ~/.claude/settings.json (or $CLAUDE_CONFIG_DIR)
cv hooks install claude --scope project # ./.claude/settings.json
cv hooks show claude                    # what install would change; --json for the before/after
cv hooks remove claude                  # exactly cv's entries and the skill, nothing else
```

`install` is idempotent: a second run writes nothing. It merges six entries (five hooks; `lane`
serves both sub-agent events) into the settings file's `hooks` key, beside whatever is already
there, and copies the [`cv` skill](#the-skill) to `skills/cv/SKILL.md` under the same `.claude`
dir. Each entry's command is this `cv` binary's absolute path plus `hook <name>`. That shape is how
`remove` and a reinstall recognize cv's entries, so a reinstall from a moved binary replaces
them instead of adding more. A settings file that is not valid JSON is left untouched and the
command errors. The hooks take effect in new sessions. `cv doctor` prints whether they are
installed and which compaction policy is in force.

Claude Code (checked against 2.1.296 and its [hooks reference](https://code.claude.com/docs/en/hooks))
is the one harness with a verified contract. `cv hooks install codex` (or any other name) errors
rather than writing a guess.

### The five hooks

Each `cv hook <name>` reads the hook's JSON on stdin and prints one line of hook JSON, or nothing.
Every hook exits 0, so a hook can never block the harness: bad input or an unreadable store goes
to stderr. Run by hand with a terminal on stdin, it does not wait for JSON that will never come:
it prints usage and exits 2. Each one reads the catalog, the event index or one transcript's tail. None of them
ever scans the fleet, and each finishes in 10–30 ms on a real store (one compaction case is
slower; measurements below).

| event · matcher | command | prints |
|---|---|---|
| `SessionStart` · `compact` | `cv hook compact` | `additionalContext`: `context compacted (auto, 180,412 tokens) at message N — compaction #k`, then `cv show <id> --pre-compaction k`, `cv show <id> --kind prompt,answer --pre-compaction k` and `cv rewind <id> --at N-1` |
| `SessionStart` · `startup\|resume` | `cv hook start` | `additionalContext`: up to six of this directory's earlier sessions (id, age, messages, title, the last commit sha its transcript's tail shows) and the count of decisions owed; capped at 4,000 characters |
| `SubagentStart`, `SubagentStop` (`async`) | `cv hook lane` | nothing; appends `{ts, event, session_id, agent_id, agent_type, transcript_path, agent_transcript_path, cwd, last_text}` to `$CLUSTERVISION_HOME/lanes.jsonl` |
| `UserPromptSubmit` | `cv hook decisions` | `additionalContext` only when `cv task inbox` has decisions owed to `$CV_ENDPOINT` (else `cv`): the count and the commands that answer them |
| `PreToolUse` · `Edit\|Write` | `cv hook edit` | `permissionDecision: "ask"` with the session id and minutes since, when another session edited this exact path and its transcript changed in the last 30 minutes; nothing (allow) otherwise |

Details that matter:

- **compact** streams the transcript the way `cv show --kind compaction_boundary` does, giving the exact message
  index and ordinal, for transcripts up to 8 MiB. Above that it reads only the last 4 MiB for the
  newest boundary's trigger and size, and points at `cv show <id> --kind compaction_boundary,compaction_summary` for the index. The
  pre-compaction records never leave the jsonl: [`cv show --pre-compaction`](cli.md#cv-show)
  reads them and [`cv rewind --at`](cli.md#cv-rewind) resumes from before them.
- **start** reads the catalog as it stands, without a freshness probe. A session newer than the
  last `cv ls` / `cv archive sync` is missing until one runs. The commit sha comes from the last
  `[branch sha]` line in each transcript's final 512 KiB.
- **lane** writes the ledger [`cv lanes`](swarm.md) reads. A `stop` row at or after a lane's last
  turn makes it read `stopped` with `status_source: hook_ledger`, even before the transcript
  records the `SubagentStop` attachment.
- **edit** reads the event index (`cv touched`'s table) by exact path. Sessions it has not
  ingested (`cv index`, `cv events <id>`) are invisible to it. "Live" means the transcript's mtime
  falls within the last 30 minutes. The hook skips the calling session and, inside a sub-agent,
  its own agent id.

Measured at the 0.16.1 release on the maintainer's store (6,704 sessions, 2.7M events, 37 GB of
Claude transcripts), release build, five runs each: `start` 20–30 ms, `decisions` 10 ms, `edit` 10–30 ms, `lane`
under 10 ms, `compact` 10 ms on a 1 MB transcript, 70–90 ms at 16.5 MB (why the full scan stops
at 8 MiB), and 10 ms on the tail path at 190 MB.

### The skill

`crates/cv/skill/SKILL.md` in the repository, which `install` copies into place: when `rewind`,
`show --pre-compaction`, `distill`, `splice`, `adopt` and `lanes` apply, and the table above. The
copy is byte-for-byte what this `cv` binary embeds, so `cv hooks show` marks a stale copy and
`install` rewrites it.

### The compaction policy

`cv doctor` prints a `Policy:` line built from the user and project settings (project wins) and
the environment, which wins over a settings `env` block: `autoCompactEnabled` (default on),
`idleCompaction` (default off; when the server enables it, it fires at 90% of a 200K-token floor),
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW` and `DISABLE_AUTO_COMPACT`.
No hook can supply or replace the compaction summary (`PreCompact` can only block it), so the
`compact` hook makes the loss visible and reversible rather than preventing it.

### Every other harness

Nothing to install for reading: `cv watch` follows every harness's store, and `cv ls`, `cv
search` and the MCP tools read the stores directly. cv ships no hook configuration for any other
harness until one has been checked against that harness's own documentation the way Claude Code's
was.
