# The CLI: `cv`

`cv` finds, reads, searches, reshapes, ports and streams every AI coding session on your machine,
straight from each harness's own store (`~/.claude`, `~/.codex`, …); only search has an optional index.

```sh
cv --help          # every command, grouped
cv <cmd> --help    # flags for any one command
cv recipes         # the agent quickstart: one command line per task, and the JSON it returns
```

Every flag is in the [command reference](reference.md), generated from the binary.

## Renamed in 0.11.0

The old names have no aliases; each errors with a pointer to its replacement.

| old | new |
| --- | --- |
| `cv convert <id> --to <h>` | `cv port <id> --harness <h>` |
| `cv port --to <h>` / `--to-dir <d>` | `cv port --harness <h>` / `--cwd <d>` |
| `cv query` | [`cv schema`](#cv-schema) |
| `cv recall` | removed — [`cv search`](#cv-search) to find content, [`cv pack`](#cv-pack) to build context. (`cv distill` returned in 0.14 as a deterministic lane reshaping, see [`cv distill`](#cv-distill).) |
| `cv prune <id> --retrieve <tool_use_id>` | [`cv cat <session> <tool_use_id>`](#cv-cat) |
| `cv prune --range A..B` | [`cv prune --keep A..B`](#cv-prune) |
| `cv show --range <start>-<end>` / `--range -N` | the [window flags](#message-windows): `--range A..B`, `--first N`, `--last N` |
| `cv splice <id>:A-B` | `cv splice <id>:A..B` |
| `cv splice`/`cv pack --to <h>` | `--harness <h>` |
| `cv help`'s agent footer | [`cv recipes`](#cv-recipes), a real command |

## The map

**Read** only reads; **Reshape** makes a **new** session and never touches the source;
**Export** produces something that is not a session; **Fleet & live** is coordination, live views
and servers; **System** is cv's own state and reference.

{{#cv-command-map}}

## Things that work everywhere

**Session ids.** A unique **prefix** is enough; `harness:id` works anywhere.

```sh
cv show da9174f4            # resolved by prefix
cv show codex:019e75e0      # or fully qualified
cv show da91 --harness codex
```

An **ambiguous** prefix lists the candidates (`harness:full-id`) and exits `2`.

**Shared flags** mean the same on every command that has them:

| flag | meaning |
| --- | --- |
| `--harness <h>` | filter to, or target, one harness — never spelled `--to` |
| `--cwd <dir>` | a working directory: a filter when reading, the new home when reshaping |
| `--out <dir\|file>` | write somewhere other than the real store |
| `--json` | machine output, never shortened |
| `--wide` | print table cells whole instead of ending a cut one in `…` |
| `--frozen` | leave cv stores untouched (also `CV_FROZEN=1`); stream stale events, retain unknown counts, reject store/session writers |
| `--limit <n>` | how many rows/records |
| `--all` | include what is hidden by default |
| `--thinking <mode>` | reasoning policy: `native`, `portable`, `text`, `drop`; rewind accepts native/portable |
| `--strict` | `port`: fail on unexpected fidelity loss; `index`: fail on any unreadable session |
| `--skills <p>` | restored skills in a new Claude session: `keep`, `drop`, `drop-stale` |

**`--thinking` and replay.** Most thinking blocks carry only an opaque provider **signature**
(177 of 204 on one 900-message session), not text; a proxied session carries another model's
reasoning and tool IDs the Anthropic API cannot read back. `cv rewind` and `cv port` default to
`native` when every recorded model (rewind: in the copied window) is `claude-*`, else `portable`.

- `native` — structured reasoning where the target holds it, else dropped; a signature-only turn
  disappears (`unrepresentable_turns`). Claude output keeps signatures and tool IDs byte for byte.
  Default for other emit commands and the library.
- `portable` — Claude: readable reasoning becomes text; signature-only or redacted reasoning, and
  any record left empty, is removed (children relinked); illegal tool IDs get collision-free
  replacements on both ends. Other targets use `text`.
- `text` — structured where supported, else readable reasoning as text and opaque as a placeholder;
  keeps Claude signatures.
- `drop` — omit all reasoning.

Non-native Claude exports repair illegal tool IDs. Verified with Claude Code 2.1.299: native and
portable rewinds, including a proxied `grok-4.7-build` session, resumed with
`claude -p --resume <id> --fork-session`. `cv prune --drop-thinking` is different: it omits old
reasoning to shrink a session.

**`--skills` and restored skills.** Every compaction writes each skill the session invoked, with
its text then, into one `invoked_skills` record, and `--resume` restores them, stale text included.
`cv prune`, `cv rewind` and `cv port` (to Claude) take `--skills`: `keep` (default), `drop`, or
`drop-stale`, dropping each skill whose file is gone or differs: `skills/<name>/SKILL.md` (user
skills under the config dir, project skills under the cwd), else legacy `commands/<name>.md`
(`a:b` is `commands/a/b.md`). Bundled and plugin skills have no file and are kept; invocation-time
substitutions can make a current file differ. A dropped skill can still be invoked, with its
current text. Non-default policies add `skills` (`dropped`, `dropped_chars`, `kept`, `records`) to
the report and `--json`.

**JSON is snake_case** with RFC 3339 timestamps; `ls`, `search` and MCP share **one** session row:

```json
{ "id": "…", "harness": "claude", "path": "…", "cwd": "…", "title": "…",
  "created_at": "2026-05-28T09:14:02.451+00:00", "updated_at": "…",
  "message_count": 87, "size_bytes": 412998 }
```

`ls --enrich` adds `display_title` and `git`; `search --json` adds `score`, `snippet` and
`agent_id`/`parent_id`/`workflow`. Messages and blocks are [the IR](architecture.md#the-unified-ir),
published by [`cv schema --json`](#cv-schema).

**`-q`, the query calculus.** [`ls`](#cv-ls), [`stats`](#cv-stats) and [`dataset`](#cv-dataset)
take `-q`: `harness:claude (model:fable OR model:opus) msgs>=50 -title:test` ([reference](#the-query-calculus)).

## Message windows

On [`cv show`](#cv-show), [`cv export`](#cv-export) and MCP `show`; indices are **0-based, end-exclusive**.

| flag | meaning |
| --- | --- |
| `--first N` | the first N messages |
| `--last N` | the last N messages |
| `--range A..B` | messages A (inclusive) through B (exclusive); `A..` runs to the end, `..B` from the start |
| `--around N [--context K]` | message N with K messages either side (default `5`) |
| `--max-bytes N` | stop after N bytes of rendered output and print a continuation line |

```sh
cv show da9174f4 --first 20
cv show da9174f4 --last 40
cv show da9174f4 --range 120..160
cv show da9174f4 --range 900..          # 900 to the end
cv show da9174f4 --around 412 --context 8
cv export da9174f4 --range 0..50 --format md > head.md
```

One of `--first`/`--last`/`--range`/`--around`; `--max-bytes` composes with any and prints:

```text
… continue with --range 4..
```

It cuts at message boundaries, always admits the first message, and is not a hard size limit
(headers and hints add bytes); reading can still traverse past the window. `--pre-compaction [N]`
sets the window itself, so it excludes these flags.

---

## Read

Transcripts stay untouched; cv's own catalogs and caches may refresh unless [`--frozen`](#things-that-work-everywhere).

### `cv ls`

Sessions, newest-updated first. `message_count` is `null` (a dash) until metadata, a full read or
`cv index` counts it, and is dropped when the source changes; `0` means no turns. `--sort-by
messages` puts uncounted last.

```sh
cv ls                         # 40 most-recent across all harnesses
cv ls --harness claude        # only Claude Code sessions
cv ls --cwd flux              # only sessions whose cwd contains "flux"
cv ls --limit 100
cv ls --sort-by messages      # updated (default) | created | messages
cv ls --asc --limit 80        # the newest 80, printed oldest-first: the corpus as a feed
cv ls -q "harness:claude msgs>=50"
cv ls --json                  # the same rows as one JSON array
cv ls --json --enrich         # + display_title and git per row
```

```text
2245 session(s)

claude    da9174f4  26-05-28 09:14 → 17:03            87 msg  the flux inference refactor
codex     019e75e0  26-05-25 11:20 → 26-05-27 22:41  142 msg  porting the parser to nom
grok      7c0b1a3e  26-05-26 14:02 → 14:09            19 msg  ~/scratch/throwaway
…
… 2205 more (use --limit)
```

Spans are local time; an untitled row shows its cwd. `--asc` flips order, not membership.
`--json` keeps `title` **raw**. `--enrich` parses one transcript per row to add `git` (the branch
it ran on) and `display_title` (`title`, else the first real user prompt, skipping injected
context; `null` if neither), or `enrich_error`.

#### Freshness — what `ls` guarantees

`cv ls` reads the **probed catalog**: 0.06–0.09 s warm on a 5,421-session, 78 GB fleet. The first
read on a new home or after an upgrade that changes reading rediscovers the fleet (158–186 s
there), said on stderr; `cv index` pays it ahead. A probe plus a backstop
(`CLUSTERVISION_MAX_STALE_SECS`, default **900s**) keep it fresh:

- **New sessions in watched directories, and new storage roots** (e.g. a
  [`claude-roots`](harnesses.md#claude-code-sessions-outside-claude) entry), show on the next read;
  a first session in an unwatched deeper directory waits for the backstop.
- **Changed sessions of any age** are caught by metadata identity (not content hash), sidecars included.
- **`--fresh`** (or `CLUSTERVISION_MAX_STALE_SECS=0`) forces full rediscovery.

A deleted file's row is dropped at read time.

### `cv show`

Print a session as a readable transcript, or as the raw IR.

```sh
cv show da9174f4
cv show da91 --harness codex
cv show da9174f4 --last 40             # a window — see above
cv show da9174f4 --json --last 40      # the IR for that window
cv show da9174f4 --pre-compaction      # the span a compaction discarded
cv show da9174f4 --subagents           # the sub-agent forest instead of the transcript
cv show da9174f4 --agent af19c2f1      # one sub-agent's transcript
cv show da9174f4 --tree                # the message threading, then the sub-agent forest
cv show da9174f4 --kind prompt,answer  # only what the person said
cv show da9174f4 --kind compaction_boundary,compaction_summary   # the compaction seams
```

Labels show the [`kind`](architecture.md#the-unified-ir) when it adds to the role
(`── system · injected_context/bash_output_audience_note ──`). `--json` is the IR, harness facts
under `extra["<harness>"]`. A bare `cv show <agent-id>` resolves the sub-agent fleet-wide; several
parents list `cv show <parent> --agent …` commands. With no window it prints the whole transcript,
on a terminal or a pipe; a window or `--max-bytes` bounds it.

#### Views

`--kind` takes one of three families: `prompt,answer`; `compaction_boundary,compaction_summary`; or affordances
(`plan`, `goal`, `schedule`, `background_job`, `skill`, `hook`, `question`, `approval`,
`checkpoint`, `branch`, `fork`, `model_change`, `subagent_spawn`, `subagent_return`, or all as
`affordances`). Unknown or mixed exits 2; compaction rows ignore windows; row views reject
`--max-bytes`. A slash-loaded Claude skill counts once per activation.

#### The threading: `--tree`

The `parent_id` DAG, indented once per fork (a straight run stays flat), else a numbered list;
tool turns and spawns flagged, then the forest.

```sh
cv show da9174f4 --tree
```

```text
# the flux inference refactor
claude · da9174f4-… · 87 msg

• user  walk the type checker and find where flux is ignored
• assistant [🔧 tool, ↳ sub-agent (Task)]  dispatching a search…
  • tool [↩ result]  found 3 call sites …
  • assistant  here's the plan …
```

#### What the person said: `--kind prompt,answer`

Every human prompt and `AskUserQuestion` answer, whole, with index and time.

```sh
cv show 0c315aee --kind prompt,answer                    # [idx] time  user|answer, then the full text
cv show 0c315aee --kind prompt,answer --json             # [{index, timestamp, kind: prompt|answer, text}]
cv show 0c315aee --kind prompt,answer --pre-compaction   # only the span the first compaction discarded
cv show 0c315aee --kind prompt --range 0..500            # typed prompts only, in a window
```

Slash-command bookkeeping records are notices, so `/compact` appears once, as typed.

#### The compaction seams: `--kind compaction_boundary,compaction_summary`

Each compaction: trigger, pre-compaction size, and the summary that seeded the next window.

```sh
cv show 77230e3d --kind compaction_boundary,compaction_summary               # trigger · pre-size · summary head
cv show 77230e3d --kind compaction_boundary,compaction_summary --summaries   # each summary in full
cv show 77230e3d --kind compaction_boundary --json   # boundaries + each one's pre-compaction span
```

It works on every harness that records compaction; either kind selects the same paired rows.

### `cv cat`

Print one tool call's full output, wherever it lives.

```sh
cv cat da9174f4 toolu_013rosc…            # the output
cv cat da9174f4 toolu_013rosc… --input    # the call's arguments, as JSON
```

Inline, in a [`prune`](#cv-prune) sidecar, or in harness side files (`tool-results/`). Raw on
stdout. An unknown id **exits 2**:

```text
no tool result "toolu_nope" in 406247d8 — `cv tools 406247d8 --timeline` lists its tool calls
```


### `cv search`

Full-text search over session content. Indexing, BM25 and embeddings: [Search](search.md).

```sh
cv search "flux inference refactor"
cv search "nom parser" --harness codex --limit 10
cv search "formalizing proofs" --semantic
cv search "flux inference" --json
```

```text
claude    da9174f4  2026-05-28  the flux inference refactor
          … so the new flux inference path replaces the old type-walker …

1 of 1 matches
```

The index (BM25) plus a live read of everything it has not seen (`--live-bytes N` bounds it,
else `$CV_LIVE_BYTES` or the config's `[search] live_bytes`, newest first, and the footer says
what was left out; none under `--frozen`). An unopenable index
fails naming [`cv index`](#cv-index); `--scan` is a slow live scan of every session that names what it could
not read. It matches the exchange; `--kind context` matches injected context, `--kind all` both.
`--semantic` needs `cv index --semantic` and errors without it. `--limit` (default 20) bounds the
hits shown, never the count: the footer says `N of M matches`, M every session that matched (for
`--semantic`, every embedded session the filters admit). `--json` prints `{"hits": [...],
"total": M}`, each hit a session row plus `score` (`null` on a live scan), full `snippet`, and
`agent_id`/`parent_id`/`workflow` (set for hits indexed with `--subagents`).

### `cv events`

What a session did: files edited or read, commands, tool errors.

```sh
cv events da9174f4                  # everything, in message order
cv events da9174f4 --kind error     # file_edit | file_read | command | tool | error
cv events da9174f4 --subagents      # the whole forest, attributed per agent
cv events da9174f4 --wide            # every target and detail whole in the table
cv events da9174f4 --json
```

[`cv index`](#cv-index) ingests events into `catalog.db`; a stale session is ingested on read
(streamed, not stored, under `--frozen`). Repeats within a message are kept. A `command` target
is the whole command and an `error` detail the whole error text; the table shortens them to 90
and 100 characters with `…` unless `--wide`, `--json` never does. `events`, `touched`
and `blame` fail on a catalog error rather than answer empty.

### `cv touched`

Every session that touched a file.

```sh
cv touched crates/cv-core/src/ir.rs       # matched absolutely and by suffix
cv touched src/ir.rs --edits-only         # only sessions that *wrote* it
cv touched src/ir.rs --json
```

```text
claude    1d1465fa  2026-05-31  13 edit(s), 1 read(s)  Plan architecture refactoring…
```

### `cv tools`

Tool use across the orchestrator **and** its forest.

```sh
cv tools 3b829648                          # aggregate histogram (orchestrator + forest)
cv tools 3b829648 --across                 # one row per agent
cv tools 3b829648 --agent af19c2f1         # one agent's histogram (<orchestrator> for the parent)
cv tools 3b829648 --tool Bash              # which agents used Bash, ranked
cv tools 3b829648 --workflow wf_ab915970   # restrict to one run's agents
cv tools 3b829648 --timeline               # chronological tool-call feed, tagged by agent
cv tools 3b829648 --json
```

`--agent` takes an id or unique prefix, else exits 2; `--across` excludes `--agent`, `--tool`,
`--timeline`. Timeline lines carry the `tool_use_id` for [`cv cat`](#cv-cat).

### `cv workflow`

A [`Workflow`](https://docs.claude.com/en/docs/claude-code)-tool run: phase tree, agents (state,
tokens, calls, duration, preview), totals, `log()` lines, **result** and script; without `<run_id>`,
the session's runs.

```sh
cv workflow 3b829648                        # list every workflow run in the session
cv workflow 3b829648 wf_ab915970            # render one run (run-id prefix is enough)
cv workflow 3b829648 census                 # …or address the run by its NAME
cv workflow stark-kill-emit-swarm           # no session id? the name resolves fleet-wide
cv workflow 3b829648 wf_ab915970 --script   # print the driving JS instead
cv workflow 3b829648 wf_ab915970 --results  # each agent's FULL journaled return value
cv workflow 3b829648 wf_ab915970 --follow   # stream a LIVE run's agent transitions (-f)
cv workflow 3b829648 wf_ab915970 --for 10m  # follow for ten minutes, then render the run as it stands
cv workflow 3b829648 wf_ab915970 --revive   # a resume prompt per unfinished lane
cv workflow 3b829648 wf_ab915970 --json     # the structured Workflow IR
```

Both arguments accept workflow names (fleet-wide for the first; a re-run name is its newest run).
Unfinished agents show their last step (`↪ last: Bash · … @ 07-06 23:05`). `--results`/`--json`
read each agent's complete return from `journal.jsonl` (lines preview ~400 characters).
`--follow` waits for the run, announces within a second, prints one line per agent transition,
then the final render; `--for <duration>` bounds it. `--revive` prints a standalone `Agent` prompt
per unfinished lane (task plus work landed, and whether its read stopped early); `--revive-all`
adds finished ones. Exit 2: `--revive` with `--json`, `--revive-all` without `--revive`, run-only
flags without a run id.

A name that matches nothing falls through to **ghost launches**: runs that died before persisting
state, found by cross-checking `Workflow` calls against state files, with the run id from the
orphaned script and the debris under `subagents/workflows/<run_id>/` (an unreadable state file
claims no ghost and is named on stderr):

```text
⚠ 1 launch(es) with NO recorded run — state never persisted (crash/kill before write?):
   stark-kill-bugfix-swarm  launched 2026-07-08 01:02
   → any sub-agent debris sits under the session dir's subagents/workflows/
```

`--json` lists as `{"runs": […], "ghost_launches": […]}`.

### `cv lanes`

The forest as a status table: per lane, model, start, duration, tokens (deduplicated like
`cv stats --tokens`), calls, status, and its last line or, while running, last tool call.

```sh
cv lanes 0c315aee                 # launch order, one row + one detail line per lane
cv lanes 0c315aee --running       # no stop recorded: still at it (shows the last tool call)
cv lanes 0c315aee --done          # finished for real
cv lanes 0c315aee --stranded      # the strand class, each with `→ resume: SendMessage to <agentId>`
cv lanes 0c315aee --tasks         # each lane joined to the tasks its endpoint holds, with the last note
cv lanes 0c315aee --json          # [{agent_id, model, tokens: {total, …}, status, status_source, last_text, stranded, …}]
cv lanes --recent 10 --unfinished # the project's 10 most recent sessions: every lane not finished for real
```

`--recent N` reads the N most recent Claude Code and Codex sessions in `--cwd` (default: here).
With `--unfinished` (running, stopped, killed, failed, stranded) it is the rescue list for
`cv adopt`, hint included.

```text
AGENT     STATUS     MODEL          STARTED         DUR  TOKENS CALLS  DESCRIPTION
acd03d62  completed  opus-4-1       09-30 23:05  1h12m    4.8M   203  K-STREAM: append payload + receiver leg
          ↩ `git status` shows only the three prover files and `build-logs/` that were already there before…
a1e1ce3d  running    opus-4-1       10-01 00:11  2h01m    2.1M    88  K-SPK: kernel items for SPK hosting
          ↪ Bash · ssh persvati 'cd /home/ember/build/mini-datamodel-20260930/k-spk/src && lake build'
```

**Status** (`status_source`) is the `Workflow` journal, else the parent's last
`<task-notification>` (from `queue-operation` records), else the child's `SubagentStop` hook
(`stopped`), else `running`. A `failed`/`killed` lane's `failure_cause` comes from its last
unanswered API-error notice: **`rate-limited`** (429 or usage/session limit; `resets_at` given —
resume with one message after it), **`context`** (prompt too long; relaunch from its clone with a
STATUS hand-off), **`stopped`** (no notice, `SubagentStop` after the last turn), else `failed`.
STATUS shows `failed:rate-limited`; the header counts causes.

**Stranded**: reported *completed*, but its last two sentences say it is waiting
(`Waiting on notifications`, …; `cv_core::lanes::text_is_waiting`). Nothing will wake it; it never
counts as done, and `--stranded` lists it with a resume hint.

**`--tasks`** shows up to five tasks each lane's endpoint holds (id, state, title, last note). The
endpoint is the first `CV_ENDPOINT=<kind>:<name>` the lane's own shell exported
(`endpoint_source: transcript`, always in `--json`), else its description's leading token matched
to an assignee (`FIX-KICK: …` → `lane:fix-kick`, labelled `(by description)`). The serve page's
Lanes pane has a `task` column.

```text
a06df2ba  running    opus-5-5       10-01 20:26   3h34m   68.9M   223  FIX-KICK: a kick ends authority in the room
          ↪ Bash · ssh persvati 'for i in $(seq 1 58); do grep -qE "^STEP J17 (PASS|FAIL)" /home/e…
          ⚑ lane:fix-kick: 4 task(s)
            01a0f9e2-5fde [claimed] AUDIT: sealing a room cell ends kick, invite and re-law whi…
```

### `cv deferrals`

A linter for promises: every "later lane", "follow-up", "your call", "when X lands", "out of
scope", … with index, time and context. `--at <msg>` starts there (`--since` exits 2 with a pointer).

```sh
cv deferrals 0c315aee                      # every deferral, oldest first
cv deferrals 0c315aee --at 1591            # only the post-compaction region
cv deferrals 0c315aee --open-tasks         # cross-reference the task store; exit 1 while any is UNMATCHED
cv deferrals 0c315aee --open-tasks --json  # [{index, timestamp, phrase, context, matched?: {task_id, title, state, shared}}]
cv deferrals 0c315aee --phrases-file deferrals.txt   # your project's phrase table instead of the built-in one
```

Your phrase table (`label = regex`, case-insensitive) goes in `--phrases-file` or
`~/.config/clustervision/deferrals.txt`; defaults are `deferral.default_phrases` in `cv schema --json`.

```text
# ~/.config/clustervision/deferrals.txt
parking lot   = \bparking lot\b
after launch  = \b(after|post)[ -]launch\b
```

```text
[1423] 2026-09-30 20:13  "after FINAL"    …Host umbrella — is queued for the standing integrator right after FINAL-1 lands, since it…
       MATCHED   01a0f52e-76a0 [open] STANDING INTEGRATOR on final: merge landed branches continuously…  (shared: integrator, landed, umbrella)
[3707] 2026-09-30 22:31  "integrator item" …ranch into the persvati base, note the collisions for the integrator, record it.
       UNMATCHED

✗ 30 deferral(s) have no task — `cv task open` each, or say why not
```

`--open-tasks` matches the task (terminal included) sharing the most significant words, at least
three (ties: newer); `shared:` shows them. Any UNMATCHED exits 1.

### `cv stats`

Fleet totals, per harness, busiest directories, date range, and "N not counted yet" (`uncounted`
in `--json` and `/api/stats`).

```sh
cv stats
cv stats -q "touched:src/ir.rs has:errors"   # analytics over a slice
cv stats --json
cv stats -q "cwd:dev/fn" --tokens            # + token usage per harness and model
```

`--tokens` parses every match and its sub-agents (about a minute per 1,600 transcripts) and totals
disjoint usage per harness and model: uncached input, cache writes, cache reads, output, cost where
recorded; `uncached` is input + cache writes + output; each response counts once. `--json` gains
`tokens` (`total`, `by_harness[]`, `by_model[]`).

```text
✦ clustervision fleet stats

2245 session(s) · 318940 message(s)

by harness:
  claude         1204
  codex           611
  opencode        298
  …

top cwds:
    412  ~/pug/clustervision
    188  ~/work/api
  …

date range:
  earliest created: 2025-09-02 10:11
  latest updated:   2026-05-29 18:44
```

### `cv blame`

Which session produced each commit of a file (after [`cv index`](#cv-index)).

```sh
cv blame crates/cv-core/src/ir.rs         # commits ↦ matching sessions
cv blame src/ir.rs -L 42                  # who wrote *this line* (also `-L 42,80`)
cv blame src/ir.rs --show                 # print the conversation around the best match
```

Each commit lists sessions with the nearest edit and a `cv show` range; time matches are ranked,
not asserted (rebases shift times). `-L` says when it cut the list; an empty message is
`(no message)`. A session whose `git commit`/`merge`/`cherry-pick`/`revert`/`push` output names the
sha ranks first as `exact: commit created here` (or `pushed here`), with a [`cv rewind`](#cv-rewind) hint:

```text
◆ d41c0de 2026-10-09  gate plan: an undeclared linked worktree names its repository's project …
  claude   7b2e19a4  2026-10-09  "Refactor the request router"  commit at msg 1240 (exact: commit created here)
    ↳ cv rewind 7b2e19a4 --at d41c0de
    ↳ cv show 7b2e19a4 --range 1237..1243
```

Only sessions that edited the file (or spawned, with `--subagents`, an agent that did) and were
written after the commit are scanned. Rebased, cherry-picked or silenced (`-q`) commits leave no proof.

### `cv doctor`

Why does context keep filling up? `cv doctor` sizes the transcript by source against recorded
prompt sizes and compactions. It also shows **Writer versions** (newest Claude/Codex writer seen
versus newest verified, plus unknown-version sessions) and skipped sources (`skipped_sources` in
`--json`; the previous list if recording failed).

```sh
cv doctor da9174f4
cv doctor                       # the most recent session for the current directory
cv doctor --recent 20           # aggregate the last 20 sessions for this cwd
cv doctor da9174f4 --json
```

```text
# compaction doctor — session 406247d8

Compaction:  never compacted across 650 message(s) — healthy ✅
First prompt: 261.2k recorded (includes conversation; heuristic proxy for fixed overhead)
Peak prompt:  284.9k recorded in transcript

Transcript content by source (278.5k estimated token equivalents):
(an estimate: bytes at calibrated per-kind rates, ~2.2–2.5 bytes/token, images ~3k; totals include history across compactions)
(thinking is mostly signature bytes; requests carry them, and most models count them)
  thinking payload   42%  ██████████               117.9k
  tool-call args    26%  ██████                   71.6k
  tool results      20%  █████                    55.6k
  system reminders   9%  ██                       23.9k
  user text          2%                           5.0k
  assistant text     2%                           4.6k
```

It then ranks the largest tool results, breaks down system reminders
([`InjectedContext`](architecture.md#the-unified-ir)) by the harness's own kinds, and names what to
cut. Sizes cover the whole transcript, compacted history included, at per-kind rates calibrated on
42 Claude Code resume requests (prose 2.5 bytes/token, tool input 2.1, results 2.2, signatures 3.6,
prune markers 1.5; +12 per block, 3k per image): 7% under to 46% over, middle within 10%. First
and peak prompt are the provider's recorded sizes (largest first under `--recent`) unless
`cv prune` rewrote them. JSON: `conversational` and per-tool values are byte estimates,
`peak_context` the peak, `fixed_overhead_est` the first prompt.

---

## Reshape

A **new** session id; the source is never modified (`resume` excepted: it launches one).

### `cv prune`

Lossless compaction of a Claude Code session into a **new, resumable** one: bulky old tool payloads
move to the `<new-id>.flat.jsonl` sidecar behind `[PRUNED id=…]` markers ([`cv cat`](#cv-cat)
fetches them); prompts and recent turns stay verbatim.

```sh
cv prune 3b829648                                  # default: snip >2KB payloads, keep the last 25 turns
cv prune 3b829648 --keep-last 40 --min-size 4096   # spare more recent context; only snip bigger payloads
cv prune 3b829648 --dry-run                        # report the savings without writing
cv prune 3b829648 --to my-tidy-session             # choose the new id
cv prune 3b829648 --drop                           # hard-drop payloads (no sidecar, irreversible)
cv prune 3b829648 --drop-thinking                 # the resurrection: flatten old reasoning + revive
cv prune 3b829648 --window 120000                  # keep the newest turns fitting a real-token budget
cv prune 3b829648 --keep 0..400                    # …or keep an explicit turn-index window
cv prune 3b829648 --json                           # + the report as one JSON object on stdout
cv prune 3b829648 --declassify --declassify-tokens-file terms.txt

cv cat <new-id> toolu_abc123                       # fetch a stashed original back out
```

`--drop-thinking` moves old thinking to the sidecar (`<record uuid>#think<index>`) with no marker,
removing emptied records: 981k → 817k tokens on a `claude-opus-5` session, nothing on
claude-fable-5-1, claude-opus-5-5 and claude-sonnet-5, whose old signatures the API no longer
counts. `--window` and `--keep` drop whole turns.

By default prune **revives** a maxed session: Claude Code's resume gate reads the last recorded
`usage`, so prune lowers recorded usage to its estimate of the new prompt (+5%; 1% under to 52% over
on 28 calibration prunes). `--no-revive` keeps records byte for byte. `--declassify` snips prose
dense in your terms; cv ships no list. `--json` reports on stdout (`null` paths under `--dry-run`).

Prune and adoption refuse a source line that is not one JSON record, and an empty transcript.
`--force` restores a replaced sidecar or metadata file if publishing fails.

### `cv rewind`

An agent **as it was** at a message or commit, as a new resumable Claude Code session, even a
sub-agent: resume the author as of its commit to review a later change.

```sh
cv rewind 7b2e19a4 --at d41c0de              # through the tool result that shows that commit being made
cv rewind 7b2e19a4 --at 1240               # through message 1240 (as `cv show --range` counts)
cv rewind 7b2e19a4 --at d41c0de --full       # from the first record, not the last compaction
cv rewind agent-b81e242c50c0b9654            # a sub-agent, extracted as a standalone session
cv rewind 7b2e19a4 --agent b81e2 --at c0ffee1
cv rewind 7b2e19a4 --at d41c0de --dry-run --json

claude --resume <new-id> --fork-session -p "Another agent changed the code you just landed. Review its diff: …"
```

`--at` is an index or a sha (7+ hex; all digits is an index): the cut is the tool result whose
`git commit`/`git push` output names it; parallel calls open there run on to their results. The copy
starts at the last compaction before the cut (`--full`: the first record; `--from-compaction` exits
2). A sub-agent comes out standalone (own id, `isSidechain: false`) in the parent's project dir.
`claude --resume` looks only in the launch directory's project dir, so cv prints that directory
beside the agent's working one (and, with `--out`, where to copy the session).

Records keep their `parentUuid` chains with `sessionId` rewritten; a non-`claude-*` window becomes
portable ([above](#things-that-work-everywhere)). `<new-id>.rewind.json` records provenance
(source, sha256 of the prefix read, cut, start, commit evidence, policy). Without a cut it copies
to EOF; malformed lines are refused (an explicit cut omits later ones); an unterminated last line
asks for a rerun; a bodyless turn is refused only under `--thinking portable`.

### `cv distill`

A long lane as a compact resumable context: messages to and from the agent verbatim, an index of
commits/paths/verdicts/errors, a call ledger, and its last tool calls as turns. Deterministic;
elided output in a [`cv cat`](#cv-cat) sidecar. Rules: [Reshaping a lane](reshaping.md).

```sh
cv distill agent-a5e2da77f3df2384b                      # the markdown pack on stdout
cv distill agent-a5e2da77 --pack lane.md                # …to a file
cv distill agent-a5e2da77 --session --cwd ~/work/repo   # a resumable session: claude -p --resume <id>
cv distill agent-a5e2da77 --agent-of c1e4d723           # a new sub-agent of that root; SendMessage to the printed id
cv distill agent-a5e2da77 --upto 400 --with agent-a04ff45c --json
```

`--upto N` distills as of message N. Emitted agents keep the source agent type, else `general-purpose`.

### `cv adopt`

Copy a dead Claude Code session's sub-agents (transcript, `sessionId` restamped, and `.meta.json`)
into a live session's `subagents/`, so it can `SendMessage` them.

```sh
cv lanes --recent 20 --unfinished       # find them: unfinished agents of the project's recent sessions
cv adopt a5d8 b81e                      # into the project's newest session other than the one holding them (printed first)
cv adopt a5d8 --into 7b2e19a4 --from c1e4d723
cv adopt a5d8 --dry-run --json
cv adopt a5d8 --model sonnet            # resume it on another model
```

Defaults: `--into` the project's newest other session; `--from` everywhere, newest copy wins.
`--model inherit` (or no `model`) runs on the live root's model.

### `cv splice`

A session from spans of others: `<id>:A..B`, `<id>:A..`, `<id>:..B` or `<id>`.

```sh
# first 20 messages of one session, then messages 50+ of another
cv splice da9174f4:0..20 4f2a0c11:50.. --export md

# materialize it into a real Codex session
cv splice da9174f4:0..20 4f2a0c11:50.. --harness codex

# stitch, then let an LLM continue the thread
cv splice da9174f4:0..20 4f2a0c11:50.. --generate
```

```text
✦ composed 7b3e0a91 (claude) · 71 msg
  ↳ claude da9174f4[0..20]
  ↳ claude 4f2a0c11[50..91]
```

Sub-agent spans join the main thread. Without `--harness`/`--out` it only composes
(`--export md|json` dumps). `--session` and `--agent-of` work as in [`distill`](#cv-distill). A call
whose result falls outside the spans is dropped ([the dangling-parent trap](reshaping.md#the-dangling-parent-trap)).

`--generate` appends one assistant turn (an empty one is refused) from `OPENROUTER_API_KEY`,
`ANTHROPIC_API_KEY` or `LMSTUDIO_API_BASE=local`.

### `cv port`

Copy a session to run elsewhere: another harness (`--harness`), another directory (`--cwd`), or both.

```sh
cv port da9174f4 --harness codex                  # same cwd, now runs in Codex
cv port da9174f4 --cwd ~/new/home                 # same harness, new home
cv port da9174f4 --harness codex --cwd ~/new/home # both
cv port da9174f4 --harness gemini --out /tmp/try  # dry run: write under --out
cv port codex:019e75e0 --harness claude           # the source harness rides on the id
```

```text
✦ wrote ~/.codex/sessions/2026/05/28/rollout-….jsonl (019e75e0-…)
  ↳ codex resume 019e75e0-…
```

It writes to the target's real store (`--out`: a scratch dir); targets:
[Cross-harness conversion](conversion.md). A rehome (no `--harness`) rewrites the baked-in cwd and
copies context files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, …) into the new cwd, never over
existing ones (`--no-context` skips). Every port re-parses its output and diffs it with the source
per message and session; `--strict` fails on a delta not expected for the target.

### `cv resume`

The native resume command; `--launch` runs it from the session's cwd.

```sh
cv resume da9174f4
cv resume da9174f4 --launch
```

```text
cd ~/pug/clustervision
claude --resume da9174f4-…
```

Desktop/IDE-only harnesses get a note instead.

---

## Export

Commands that produce something that is not a session.

### `cv export`

Render a session to stdout.

```sh
cv export da9174f4 > session.md               # markdown (default)
cv export da9174f4 --format json > s.json
cv export da9174f4 --last 100 --format md     # windows work here too
cv export da9174f4 --redact > safe.md         # secrets/PII scrubbed first; counts on stderr
```

```text
✦ redacted 7 item(s): 2 api_key, 1 private_key, 0 jwt, 3 email, 1 blob, 0 assignment
```

`--redact` is [`cv share`](#cv-share)'s scrub (keys, JWTs, emails, blobs, `KEY=value`), tallied on stderr.

### `cv dataset`

Fine-tuning JSONL, one session per line: `chatml` (default, `{"messages":[…]}`) or `sharegpt`
(`{"conversations":[…]}`), loadable by TRL and HuggingFace `datasets`.

```sh
cv dataset --file corpus.jsonl                              # whole corpus, chatml
cv dataset --format sharegpt --harness claude                # one harness, ShareGPT shape
cv dataset -q "model:fable" --subagents --file fable.jsonl   # a queried slice + its forest
cv dataset -q "harness:claude" --redact-only private_key     # strip PEM keys, keep the rest
```

`--subagents` adds the forests (most model usage; a `model:` query wants it). `--redact-only` takes
`private_key`, `api_key`, `jwt`, `email`, `blob`, `assignment`. `--out` exits 2, pointing at `--file`.

### `cv pack`

Compile a context bundle for a new task from the whole corpus.

```sh
cv pack "tantivy chunked indexing"                       # CLAUDE.md-style bundle → stdout
cv pack "fix the parser" --format prompt                 # shaped as a system prompt
cv pack "fix the parser" --format session --harness claude   # a synthetic resumable session
cv pack "migrate the daemon" --limit 3 --out CONTEXT.md
```

`--format` is `md`, `prompt` or `session`. Details: **[`cv pack` — the context compiler](pack.md)**.

---

## Fleet & live

### `cv task`

Durable dispatch: open/claim/note/done and posed decisions; completion is self-reported. See
**[the task substrate](tasks.md)** and [the reference](reference.md#cv-task). Verbs removed in 0.15.0
(`propose`, `reroute`, `pass`, `refute`, `verify`, `debt`, `stats`, `split`, `watch`) error with a pointer.

### The coordination board (removed)

Removed in 0.15.0 (exit 2): use `cv task note`, `cv task events --since <id>`, `cv task claim`.

### `cv scry`

`tail -f` for agent activity across every harness. 🔮

```sh
cv scry
cv scry --harness claude --cwd clustervision
cv scry --existing            # also emit sessions already present at startup
cv scry --interval 1
cv scry --for 5m              # follow for five minutes, then exit 0
cv scry | tee feed.txt        # a pipe gets the current state once …
cv scry --follow | tee feed.txt   # … unless told to follow
```

```text
cv scry: following live agent activity; stop with Ctrl-C or --for <duration>
cv scry: following 212 sessions
✷ new  claude   da9174f4  ~/pug/clustervision  (3 msg)
      user walk the type checker and find where flux is ignored
   +  claude   da9174f4  ~/pug/clustervision  (1 msg)
      assistant found it — the walker drops flux constraints here …
```

On a terminal it follows until Ctrl-C, announced within a second. On a pipe it prints the
sessions once (`   ·` lines; `--existing`: the backlog) and exits 0 unless `--follow` or
`--for 30s|5m|2h`. A negative `--interval` exits 2. Built on [`cv watch`](serving.md).

### `cv share`

A redacted, self-contained offline HTML file.

```sh
cv share da9174f4                  # → ./da9174f4….html, redacted
cv share da91 --out incident.html  # pick the filename
cv share da91 --no-redact          # keep secrets (loud stderr warning)
```

More in **[Viewing & sharing](viewing-and-sharing.md#sharing-a-transcript-cv-share)**.

---

## System

cv's own state and reference.

### `cv index`

Prepays the live read search would do; run it whenever convenient.

```sh
cv index                # full-text (tantivy) index (top-level sessions)
cv index --semantic     # also build embeddings for `cv search --semantic`
cv index --subagents    # also fold the sub-agent / workflow forest into the index + events
cv index --rebuild      # clear and rebuild from scratch
```

```text
✦ building full-text index…
indexed 2245 session(s) → ~/.clustervision/tantivy
✦ embedding sessions (downloads a small model on first use)…
embedded 2245 session(s) → ~/.clustervision/embeddings.bin
```

Incremental; `--json` reports `indexed`, `changed`, `removed`, `errors`. `--semantic` downloads a
~30 MB model once. `--subagents` tags forest hits with parent, run and agent (can add hundreds of
MB). The same pass fills the **event catalog** for [`events`](#cv-events), [`touched`](#cv-touched)
and [`blame`](#cv-blame).

### `cv config`

Shows `config.toml` (`$XDG_CONFIG_HOME/clustervision/`, else `~/.config/…`) and registers where
ChatGPT / Claude.ai export archives live ([`chatgpt-export`/`claude-export`](harnesses.md)).

```sh
cv config                              # print the config path + registered export sources
cv config --add-export ~/Downloads     # register a dir to scan (or a specific conversations.json)
cv config --add-export ~/exports/chatgpt-2026/conversations.json
cv config --rm-export ~/Downloads      # unregister
```

Plain TOML (`exports = [...]`); `$CV_EXPORTS` (`:`-separated) adds more per run. A bad config warns
once. `[search] live_bytes = N` bounds the live read of every search that names no budget
(`--live-bytes`, then `$CV_LIVE_BYTES`, override it; `CV_LIVE_BYTES=all` lifts it). `$CLAURDVOYANT_*` variables exit 2 naming their `CLUSTERVISION_` replacements.

### `cv schema`

Bare: the `-q` reference. `--json`: every shape cv emits. `--commands`: the command tree.

```sh
cv schema                    # the full query-language reference
cv schema --json             # session_row · session · message · block · query, as JSON
cv schema --commands         # every command, grouped, with its one-line about
cv schema --commands --json  # the whole tree with every argument — what generates the MCP tools
```

`--json` shapes: `session_row`, `session`, `message`, `block`, `prompt_row`, `lane`, `deferral`,
`query`, `subagent_row`, `"task_row"`, and `routes` (every `cv serve` method/path). `--commands --json`
gives `{ name, group, about, args: [{ name, kind, value_type, possible_values?, default?, help,
required }] }` per command, from clap; it generates [the MCP tools](mcp.md).

#### The query calculus

Implicit `AND`, `OR`/`|`, `NOT`/`-`, `( )`.

```sh
cv ls -q "harness:claude model:fable"                 # fable sessions from Claude
cv ls -q "(model:fable OR model:opus) -title:test"    # either model, excluding tests
cv ls -q "msgs>=50 after:2026-01-01 tool:Bash"        # big recent sessions that ran Bash
cv stats -q "touched:src/ir.rs has:errors"            # analytics over a slice
cv ls -q "harness:claude agent:Explore subtool:Bash"  # a forest query
```

Fields, by cost:

- **catalog** (free): `harness`, `cwd`, `title`, `id`, `msgs`, `created`, `updated` (`before:`/`after:`/`since:`/`until:` are sugar).
- **parse** (reads the transcript): `model` (any turn's), `git` (branch/remote).
- **index**: `text`, full-text over content.
- **events**: `tool` (a tool this session ran), `touched` (a file it read or edited), `has` (`subagents`, `errors`, `tools`, `images`, `compacted`, `workflows`, and every affordance kind — `plan`, `schedule`, `background_job`, `approval`, … — see [`--kind`](#views)).
- **forest** (walks [sub-agents, workflows and compaction seams](#cv-workflow); priciest): `subtool` (a tool a sub-agent ran), `agent` (spawned this agent type), `agents` (forest size), `workflow` (ran a matching `Workflow`), `workflows` (run count), `compactions` (boundary count).

`:` contains, `=` exact, `> >= < <=` numbers and dates; `a,b` is OR, `lo..hi` a range; a bare word
matches title, cwd or id. Cheap terms run first: pair costly ones with `harness:`/`cwd:`/`msgs`.

```sh
cv ls -q "harness:claude agents>=5 subtool:Bash compactions>=1"   # deep, tool-heavy, compacted runs
cv ls -q "workflow:census OR workflows>=3"                        # ran a census workflow, or 3+ runs
```

`thread:` and `~` (removed in 0.15.0) error with a pointer. `text:` reads like `cv search`;
without an index the live read must cover every session, else it exits 2 naming `cv index`
(`/api/stats`: 503).

### `cv formats`

How much of each harness's on-disk vocabulary cv understands.

```sh
cv formats                         # what real sessions hold, per harness
cv formats --harness codex         # just one
cv formats --recent 50 --json
```

It counts the record vocabulary in recent sessions, uninterpreted records included (as
[`carrier`](architecture.md#the-unified-ir) messages), marking `NEW` what the embedded manifest
(`formats/<harness>.toml`) lacks, and the first parse error (`first_error`). The adapter-side check is
`cargo test -p clustervision-core --test formats_manifest`.

| status | meaning |
| --- | --- |
| `handled` | interpreted per item — inferred from a match arm, or listed when no arm names it (a SQL identifier, a path) |
| `generic` | interpreted by a blanket rule, so no per-item literal exists |
| `carried` | kept verbatim as a carrier under a complete read (`Demand::COMPLETE`), dropped otherwise |
| `ignored` | known and deliberately skipped (transient, UI-only, or a duplicate) |

### `cv recipes`

The agent quickstart: one command line per common task, with the JSON it returns.

```sh
cv recipes
```

Agents: run it first.
