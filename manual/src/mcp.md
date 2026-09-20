# MCP — agents reading each other's minds

Most of clustervision is about *you* reading sessions after the fact. `cv-mcp` is the
other direction: it hands the same powers to a **running coding agent** so it can read
*other* agents' sessions — within this project, back through time, and across harnesses
— and coordinate with siblings live. 🔮

It's a [Model Context Protocol](https://modelcontextprotocol.io) server speaking
line-delimited JSON-RPC 2.0 over stdio. Point Claude Code (or Codex, Gemini, …) at it and
the agent gets the whole `cv` command surface as tools, plus a handful of MCP-only tools
for live coordination.

## Motivating use cases

- **"What happened in this project before?"** — `ls(cwd='/myproj')` lists every session
  (any harness) whose recorded working directory is this project, newest first. The agent
  reads the relevant one with `show(id=…)`.
- **"Has anyone solved this before?"** — `search(query=…)` finds it by what was literally
  said; `pack(task=…)` compiles a whole context bundle out of the corpus. See
  [search](search.md).
- **"What's my sibling agent doing right now?"** — `ls` surfaces live sessions;
  `observe_stream` tails their output; `board_who` shows who's present on a channel.
- **"Block until another session prints X."** — `await_omen(regex=…)` watches live
  message streams and returns when one matches; `board_await` does the same for explicit
  [board](board.md) posts.
- **"Why does my own context keep filling up?"** — `doctor(id=…)` attributes the pressure
  by source. Point it at your *own* session id.

## Registering it

Build the binaries, then register the server with your host. For Claude Code:

```sh
cargo build -p clustervision -p cv-mcp --release
claude mcp add clustervision -- /absolute/path/to/cv-mcp
```

Any MCP host works — give it the absolute path to the `cv-mcp` binary as the command (no
arguments needed). **stdout is the protocol channel**, so all diagnostics go to stderr. It
implements `initialize`, `ping`, `tools/list` and `tools/call` against protocol version
`2025-06-18`, and every tool returns its result as a single text content block.

`cv-mcp` needs the `cv` binary at runtime (see below), so build and install them together.

## The tools are generated from the CLI

There is no hand-maintained list of what cv can do inside `cv-mcp`. At startup the server
runs, once:

```sh
cv schema --commands --json
```

which dumps clap's own introspection of the command tree — every visible command and
subcommand with its group, `about`, and each argument's kind (`flag` / `option` /
`positional`), value type, possible values, default, help text and required-ness. Each
command in the **Read**, **Reshape**, **Export** and **System** groups becomes one MCP
tool:

- the **tool name is the command name** (`ls`, `show`, `cat`, …; a subcommand joins with
  an underscore, `formats_census`);
- the **tool description is clap's `about`**;
- **every argument becomes a JSON-schema property named after the argument** —
  positionals included, so `cat` takes `session` and `tool_use_id` as properties — with
  clap's help as the property description, `possible_values` as an `enum`, and the CLI
  default as `default`;
- **required arguments land in the schema's `required` list.**

A call shells out: `cv <command> <args…> --json`, and the child's stdout comes back as the
tool result. The `--json` is appended only for commands that have the flag (so `cat`,
`tree`, `export`, `blame`, `resume`, … return text), and `json` is therefore *not* a
property you set. A **non-zero exit is an MCP tool error** (`isError: true`) carrying the
child's stderr, so `cv`'s exit-2 "no session matching …" reaches the agent verbatim. A
malformed or unknown argument never reaches `cv` at all: it comes back as JSON-RPC
`-32602 Invalid params`.

Because the schema is the binary's own, **tool names, flags, harness lists and output
shapes cannot drift from the CLI**. Add a command or a flag to `cv` and it appears over
MCP with no change here. The page you are reading is the one place that can go stale: if
it and a live `tools/list` ever disagree, the live reply is the authority.

### Finding the `cv` binary

In order, first one that actually answers `cv schema --commands --json`:

1. **`$CV_BIN`** — an explicit absolute path. Set this when `cv` is somewhere unusual, or
   to pin a specific build.
2. **A `cv` next to the running `cv-mcp`** (via `std::env::current_exe`). The two are
   built and installed together, so the sibling is the matching version.
3. **`cv` on `$PATH`.**

If none answers, the server logs the failure to stderr and **serves only the MCP-only
tools** — it does not die. `tools/list` is then short, and calling a command tool returns
an error telling you to set `$CV_BIN`.

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
**none** of those five, cv-mcp adds `--last 50 --max-bytes 200000`. A 400-message session
would otherwise land in the agent's context in one shot, which is exactly the problem
`doctor` exists to diagnose. Naming any selector (including `max_bytes` alone, or
`pre_compaction`, which sets its own window) suppresses the defaults entirely — asking for
the whole thing is allowed, it just has to be asked for. The `ls` / `search` `limit`
defaults are the CLI's, unchanged.

## The generated tools

Exactly the CLI's Read / Reshape / Export / System groups. Arguments are listed in full by
the live `tools/list`; only the **required** ones are shown here. "JSON" says whether the
call gets `--json` appended (and so returns machine output rather than rendered text).

### Read

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `ls` | — | yes | List discovered sessions across all harnesses. `cwd` filters to a project; `query` takes the filter calculus. |
| `show` | `id` | yes | Print a single session (by `harness:id` or a unique id prefix). Windowed by default — see above. |
| `cat` | `session`, `tool_use_id` | no | Print one tool call's full output, wherever it lives: inline, in a `prune` sidecar, or in a persisted-output file. `input: true` prints the call's arguments instead. |
| `search` | `query` | yes | Full-text search across all session content. |
| `events` | `id` | yes | What a session DID: its extracted events (file edits/reads, commands, errors). |
| `touched` | `path` | yes | Every session with a file_edit/file_read event on that path. |
| `tools` | `id` | yes | Cross-agent tool analytics across the orchestrator and its whole sub-agent forest. |
| `tree` | `id` | no | A session's message threading (DAG if parent ids exist, else a numbered list). |
| `workflow` | `id` | yes | A `Workflow` run, first-class: phase tree, agents and outcomes, totals, driving script. Without `run_id`, lists the session's runs. |
| `compaction` | `id` | yes | Every compaction boundary, its trigger, pre-compaction size, and the seeding summary. |
| `timeline` | — | yes | Unified chronological feed across all harnesses. |
| `stats` | — | yes | Fleet analytics over all discovered sessions. |
| `diff` | `a`, `b` | no | Compare two sessions message-by-message (great for loom branches). |
| `blame` | `file` | no | Which agent session wrote this code, and what was it thinking? |
| `doctor` | — | yes | Why a context window keeps filling: pressure by source, overhead, compaction frequency. |

### Reshape

Each produces a **new** session id; the source is never touched.

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `prune` | `id` | yes | Lossless compaction into a new resumable session: bulky old tool payloads go to a sidecar behind a `[PRUNED]` marker. Retrieve one with `cat`. |
| `splice` | `specs` | no | Compose a new session from spans of existing ones (`<id>:A..B`). |
| `loom` | `base`, `at`, `graft`, `from` | no | Graft: `base[..N]` then `other[M..]`, as one new branched session. |
| `port` | `id` | no | A copy that runs elsewhere — another harness (`harness`), another working directory (`cwd`), or both. |
| `redact` | `id` | no | Scrub secrets/PII and export (safe to share). |
| `resume` | `id` | no | The resume incantation for a session in its native harness. |

### Export

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `export` | `id` | no | Export a session to markdown, JSON, or self-contained HTML. Takes the window flags. |
| `dataset` | — | no | The corpus as a fine-tuning dataset (JSONL, one session per line). |
| `pack` | `task` | no | Compile a context bundle for a new task out of the whole corpus. |

### System

| Tool | Required | JSON | What it does |
|---|---|---|---|
| `index` | — | no | Build/refresh the full-text index that makes `search` instant. |
| `config` | — | no | View the user config and manage the export-source index. |
| `schema` | — | yes | The reference: query calculus, and the machine-readable schema of every shape cv emits. |
| `formats` | — | yes | The format census and manifest check. |
| `recipes` | — | no | The agent quickstart: the ten things agents do with cv. |

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

These have **no CLI equivalent** and stay hand-written, because they block, hold a cursor,
or arbitrate between agents — things a one-shot subprocess cannot do.

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
For explicit, structured coordination, prefer the board (below).

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

Read-only — it never writes the board.

Requests are dispatched concurrently, so a parked `await_omen` or `board_await` never
head-of-line-blocks a `show` issued after it.

## The coordination board

The board is a lightweight pub/sub + locking layer agents use to talk to each other
directly. Full conceptual treatment is in [the board](board.md); here are the MCP tools.
Most take a `channel` (a room name, often a project path or topic) and an optional `from`
(who's posting, default `"agent"`). Results come back as pretty JSON.

### Messaging

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_post` | `channel`, `body` | `from`, `kind` (`msg`\|`status`\|`event`, default `msg`), `tags[]`, `session_ref` | Broadcast status, leave a note, or hand off work so other agents see it. |
| `board_read` | `channel` | `since` (id cursor), `limit` (default `50`, `0`=all) | Read recent posts; pass `since` to get only newer ones. |
| `board_await` | `channel`, `regex` | `since` (default: current tail), `timeout_secs` (`120`), `interval_secs` (`2`) | Block until a **new** post matches the regex (or timeout). The clean way to wait on a sibling: `board_await(channel='myproj', regex='BUILD (PASSED|FAILED)')`. |
| `board_ack` | `channel`, `message_id` | `from` | Acknowledge a message by id with a tiny ack note, so the sender knows it was seen. |

`board_await` returns `{ matched, message?, cursor }` on a hit, or
`{ matched: false, timed_out: true, cursor }` on timeout.

### Request / reply

A small Q&A protocol layered on the board, correlated by message id.

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_request` | `channel`, `body` | `from` | Post a question other agents can answer. Returns the message **including its `id`** — keep it to collect answers. |
| `board_reply` | `channel`, `in_reply_to`, `body` | `from` | Answer a prior request, correlated by its id. |
| `board_replies` | `channel`, `request_id` | — | Collect all replies to a request id, in chronological order. |
| `board_unanswered` | `channel` | `within_secs` (`86400`) | Requests with **zero** replies, oldest first, each with its age in seconds — the dropped-questions view. Any reply excludes a request. |

### Claims (soft distributed locks)

So two agents don't grab the same task.

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_claim` | `channel`, `key` | `from`, `ttl_secs` (`300`) | Try to claim a task `key`. Returns `{granted: bool, lease?}`; if `granted` is false someone else holds it, so pick a different task. Renews your own claim if you already hold it. |
| `board_release` | `channel`, `key` | `from` | Release a key you claimed, freeing it for others. Idempotent; only releases a claim you own (returns `{released: false, reason}` otherwise). |
| `board_claims` | `channel` | — | List active (un-expired) claims as `{key, owner, expires_at}` — see who's working on what. |

A claimed `key` is typically a file path or task id. Always `board_release` when done
(or let the `ttl_secs` lease expire so it can be stolen).

### Presence

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_heartbeat` | `channel`, `from` | — | Announce you're alive on a channel. Call periodically to stay "active". |
| `board_who` | `channel` | `within_secs` (`60`) | List agents that heartbeat within the window — active presence. |

## The task substrate over MCP

The [task substrate](tasks.md) — durable dispatch objects whose landing state is *observed
from git*, never asserted — is fully drivable over MCP. Fourteen `task_*` tools wrap the same
core paths as the `cv task` CLI, and results come back as pretty JSON in the same shared row
shapes. Two things to know up front:

- **Identity.** The MCP server inherits the agent's environment, so a spawner-set
  `CV_ENDPOINT=agent:<name>` makes every bare call record the right actor. Identity-*bearing*
  tools (`task_claim`, `task_release`, `task_propose`, `task_pass`, `task_refute`, and
  `task_inbox`'s default `who`) **error** when neither `from` nor `CV_ENDPOINT` is set;
  bookkeeping tools fall back to the `"agent"` sink.
- **Law 1 at the tool surface.** There is deliberately *no* tool that records a merge or a
  land. `task_verify` runs the git verifier; that is the only way landing state changes.

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `task_open` | `title` | `body`, `repo`, `issue`, `channel` (`tasks`), `assignee`, `from` | Open a durable task. Pass `repo` (absolute path) to enable propose/verify/debt. |
| `task_list` | — | `state`, `assignee`, `repo`, `all` (`false`) | List tasks (non-terminal by default). An unknown `state` is an error naming the vocabulary, never a silent `[]`. |
| `task_show` | `id` | — | One task's full projection: state, revisions, review evidence, landed observation, notes, recorded issues. |
| `task_claim` | `id` | `from`* | Claim an open task — durable, race-free, first writer wins. |
| `task_release` | `id` | `from`* | Release your claim back to open. |
| `task_note` | `id`, `text` | `session_ref`, `from` | Progress note; never changes state. |
| `task_done` | `id` | `observed`, `from` | Complete a **non-code** task. Refused while a revision is live. |
| `task_abandon` | `id` | `reason`, `from` | Kill a task (always allowed on a non-terminal task). |
| `task_propose` | `id`, `branch` | `upstream` (`origin/main`), `sha`, `worktree`, `reviewer`, `session_ref`, `from`* | Attach a reviewed revision: cv resolves the tip and computes the range patch-id **from git itself**. Re-proposing supersedes the prior revision (the only cure for a refute). |
| `task_pass` | `id` | `session`, `from`* | Review PASS (you must be the active reviewer). Pass your cv session id so the advisory cross-family independence check can run. |
| `task_refute` | `id` | `session`, `from`* | Review REFUTE — terminal for the revision. |
| `task_verify` | — | `id` (else all), `fetch` (`false`) | Run the git verifier: observe landings/local merges/findings. The **only** way landing state changes. |
| `task_inbox` | — | `who` (default `$CV_ENDPOINT`) | What needs `who`: assigned, claimed, awaiting-their-review, their unlanded work. Stalest first. |
| `task_debt` | — | `repo` | Reviewed-but-unlanded work by repo, plus awaiting-review rows, SUSPECT lands, and the verifier heartbeat (`verified_as_of` / `verify_warning`). |

\* identity-bearing: defaults to `$CV_ENDPOINT`, errors when neither is set.

This table is transcribed from the server's own `tools/list` reply (the `task_tool_list()`
function in `cv-mcp`); if this page and a live `tools/list` ever disagree, the live reply is
the authority.

## A typical flow

1. An agent starts work in `/myproj`. It calls `ls(cwd='/myproj')` to see prior history,
   and `board_who(channel='myproj')` to see which siblings are live.
2. It `board_heartbeat`s, then `board_claim(channel='myproj', key='src/auth.rs')` so no
   one else touches that file.
3. Mid-task it hits a wall and calls `pack(task='refresh token rotation')` to compile
   prior work on it from the whole corpus, then `show(id=…, around=…)` on the winner to
   read the span in context — or `cat(session, tool_use_id)` to pull one tool output back
   in full.
4. It hands off: `board_post(channel='myproj', kind='status', body='auth done, tests green')`
   and `board_release(channel='myproj', key='src/auth.rs')`.
5. A sibling that ran `board_await(channel='myproj', regex='auth done')` unblocks and
   picks up the next task.
