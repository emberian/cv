# The CLI: `cv`

`cv` is the front door to clustervision. One small binary that can find, read, search, reshape, port and stream every AI coding session on your machine — across every harness it knows how to parse.

Everything `cv` does, it does over your *real* local session storage (`~/.claude`, `~/.codex`, `~/.config/opencode`, and friends). There's no database to set up and nothing to import; discovery happens by scanning the harnesses you already have installed. (Search gets an optional index — more on that below.)

```sh
cv --help          # every command, grouped
cv <cmd> --help    # flags for any one command
cv recipes         # the agent quickstart: ten command lines and the JSON they return
```

## Renamed in 0.11.0

0.11.0 is a **clean break**: the old names are gone, with no aliases. Each one errors with a pointer to its replacement, so a stale script tells you exactly what to type instead.

| old | new |
| --- | --- |
| `cv convert <id> --to <h>` | `cv port <id> --harness <h>` |
| `cv port --to <h>` / `--to-dir <d>` | `cv port --harness <h>` / `--cwd <d>` |
| `cv query` | [`cv schema`](#cv-schema) |
| `cv recall`, `cv distill` | removed — [`cv search`](#cv-search) to find content, [`cv pack`](#cv-pack) to build context |
| `cv prune <id> --retrieve <tool_use_id>` | [`cv cat <session> <tool_use_id>`](#cv-cat) |
| `cv prune --range A..B` | [`cv prune --keep A..B`](#cv-prune) |
| `cv show --range <start>-<end>` / `--range -N` | the [window flags](#message-windows): `--range A..B`, `--first N`, `--last N` |
| `cv splice <id>:A-B` | `cv splice <id>:A..B` |
| `cv splice`/`cv loom`/`cv pack --to <h>` | `--harness <h>` |
| `cv help`'s agent footer | [`cv recipes`](#cv-recipes), a real command |

The principle behind all of it: **one name, one meaning, no grammar to memorize.** A verb names one act; a flag means the same thing on every command that has it; a JSON key is spelled one way.

## The map

`cv --help` prints these five groups, and nothing is hidden from it.

| Group | What they have in common | Commands |
| --- | --- | --- |
| **[Read](#read)** | read-only over existing sessions | [`ls`](#cv-ls) [`show`](#cv-show) [`cat`](#cv-cat) [`search`](#cv-search) [`events`](#cv-events) [`touched`](#cv-touched) [`tools`](#cv-tools) [`tree`](#cv-tree) [`workflow`](#cv-workflow) [`compaction`](#cv-compaction) [`timeline`](#cv-timeline) [`stats`](#cv-stats) [`diff`](#cv-diff) [`blame`](#cv-blame) [`doctor`](#cv-doctor) |
| **[Reshape](#reshape)** | produce a **new** session id from existing ones; the source is never touched | [`prune`](#cv-prune) [`splice`](#cv-splice) [`loom`](#cv-loom) [`port`](#cv-port) [`redact`](#cv-redact) [`resume`](#cv-resume) |
| **[Export](#export)** | produce something that is *not* a session | [`export`](#cv-export) [`dataset`](#cv-dataset) [`pack`](#cv-pack) |
| **[Fleet & live](#fleet--live)** | multi-agent coordination and live views | [`task`](#cv-task) [`board`](#cv-board) [`scry`](#cv-scry) [`share`](#cv-share) |
| **[System](#system)** | cv's own state and reference | [`index`](#cv-index) [`config`](#cv-config) [`schema`](#cv-schema) [`formats`](#cv-formats) [`recipes`](#cv-recipes) |

## Things that work everywhere

**Session ids.** Almost every command takes a session id, and a unique **prefix** is enough. The ids are long UUIDs; you'll usually paste the short 8-character form `cv ls` prints. You can also write `harness:id` anywhere an id goes.

```sh
cv show da9174f4            # resolved by prefix
cv show codex:019e75e0      # or fully qualified
cv show da91 --harness codex
```

An **ambiguous** prefix is never resolved by guessing: cv lists the candidates as `harness:full-id` lines and exits `2`. Surprising you with the wrong session is worse than making you type three more characters.

**The shared flags.** A word means the same thing on every command that has it:

| flag | meaning | on |
| --- | --- | --- |
| `--harness <h>` | filter to, or target, one harness — never spelled `--to` | `ls` `show` `cat` `search` `events` `tools` `tree` `workflow` `compaction` `timeline` `diff` `doctor` `prune` `splice` `loom` `port` `redact` `resume` `export` `dataset` `pack` `scry` `share` |
| `--cwd <dir>` | a working directory: a filter when reading, the new home when reshaping | `ls` `timeline` `splice` `loom` `port` `scry` |
| `--out <dir\|file>` | write somewhere other than the real store | `splice` `loom` `port` `dataset` `pack` `share` |
| `--json` | machine output | `ls` `show` `search` `events` `touched` `tools` `workflow` `compaction` `timeline` `stats` `doctor` `prune` `schema` `task list/show/inbox/debt/stats` `board read/unanswered` |
| `--limit <n>` | how many rows/records | `ls` `search` `timeline` `dataset` `pack` `board read` |
| `--fresh` | bypass the catalog and re-discover | `ls` |
| `--all` | include what is hidden by default | `task list` `task verify` |
| `--thinking <mode>` | what an emit does with the model's reasoning: `native` (default), `text`, `drop` | `port` `splice` `loom` `pack` |
| `--strict` | fail if a loss the target format *could* have carried happens | `port` |

**`--thinking` and why the default drops some reasoning.** Thinking blocks come in two shapes. A few
carry plaintext. Most carry only a provider **signature** — an opaque, vendor-bound blob with no
text at all (on one real 900-message session, 177 of 204). No format but the one that produced it
can hold that blob, so "carry the reasoning across" is not on the table for the majority. The only
question is whether the *turn* survives, and the mode answers it:

- `native` (default) — structured reasoning where the target holds it, dropped where it does not.
  An assistant turn whose only content was a signature blob disappears, reported as
  `unrepresentable_turns`.
- `text` — never lose the turn. Reasoning with text becomes a text block; a signed blob becomes a
  short placeholder naming what it was. The conversation's alternation survives intact. The lost
  signature is still reported, so a lossy port does not start looking clean.
- `drop` — omit reasoning entirely, even where the target could hold it, for handing someone a
  session without your chain of thought.

Note that `cv prune --drop-thinking` is a different thing: it flattens old reasoning to shrink a
session you intend to resume. It was spelled `--thinking` before 0.11.0, and was renamed precisely
so that one word does not mean two things.

**JSON is snake_case, everywhere.** Every `--json` output and every MCP payload uses snake_case keys, and timestamps are RFC 3339 strings. There is **one** session-row shape, and `ls`, `search`, `timeline` and the MCP tools all emit it:

```json
{ "id": "…", "harness": "claude", "path": "…", "cwd": "…", "title": "…",
  "created_at": "2026-05-28T09:14:02.451+00:00", "updated_at": "…",
  "message_count": 87, "size_bytes": 412998 }
```

`ls --enrich` adds `display_title` and `git`; `search --json` adds `score`, `snippet` and the sub-agent provenance trio `agent_id`/`parent_id`/`workflow`. A **message** is the IR `Message` (`role`, `kind`, `origin`, `content`, `usage`, …) and a **block** is the IR `Block`, tagged by `type` — see [the IR vocabulary](architecture.md#the-unified-ir), which is the one place the words are defined. [`cv schema --json`](#cv-schema) publishes all of it machine-readably.

**`-q`, the query calculus.** [`ls`](#cv-ls), [`timeline`](#cv-timeline), [`stats`](#cv-stats) and [`dataset`](#cv-dataset) take a `-q`/`--query` selector — one small boolean language for picking sessions: `harness:claude (model:fable OR model:opus) msgs>=50 -title:test`. The full reference lives under [`cv schema`](#cv-schema).

## Message windows

Five flags select *which messages* to render. They mean the same thing on [`cv show`](#cv-show), [`cv export`](#cv-export) and the MCP `show` tool, and all indices are **0-based and end-exclusive**.

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

Only **one** of `--first`/`--last`/`--range`/`--around` may be given; clap refuses the combination rather than silently picking one. `--max-bytes` composes with any of them, and prints its own pointer at the truncation:

```text
… continue with --range 4..
```

so paging a huge session is a loop of copy-paste with no arithmetic. Messages outside the window are never resolved, so a windowed view of a multi-gigabyte transcript reads only the bytes it shows.

Two relatives use the same `A..B` grammar without being read-windows: [`cv prune --keep A..B`](#cv-prune) selects the turns to *keep* in a new session, and [`cv splice`](#cv-splice)'s specs are `<id>:A..B`.

`--pre-compaction [N]` on [`cv show`](#cv-show) sets the window *for* you (to the span before the Nth compaction boundary), so it excludes the window flags.

---

## Read

Fifteen commands that only ever read. None of them writes to a session store.

### `cv ls`

List discovered sessions, newest-updated first.

```sh
cv ls                         # 40 most-recent across all harnesses
cv ls --harness claude        # only Claude Code sessions
cv ls --cwd flux              # only sessions whose cwd contains "flux"
cv ls --limit 100
cv ls --sort-by messages      # updated (default) | created | messages
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

Each row shows the session's `created → last-active` span in **local time** (same-day sessions compress the right side to a bare time). The span is what separates a long-lived orchestrator from a one-shot — and, after a crash, the resumed sessions from the dropped ones. When a session has no title, `cv` falls back to a dimmed cwd so the row still tells you *where* it happened.

- `--json` — the rows as one JSON array on stdout and nothing else, so it pipes cleanly. Keys are the shared [session row](#things-that-work-everywhere): `id`, `harness`, `path`, `cwd`, `title`, `created_at`, `updated_at`, `message_count`, `size_bytes`. Transcript-derived text (`title`) is emitted **raw** — the machine contract, not the sanitized terminal view.
- `--enrich` (with `--json`) — add two transcript-derived fields to each row:
  - `git` — the same object `cv show --json` emits (`branch`/`commit`/`remote`, each omitted when absent), read from the transcript's recorded git context. This is the branch the session *ran on*, a historical fact stored in the transcript — **not** the cwd's current branch.
  - `display_title` — the row's `title` with a fallback synthesized from the first real user turn (peeling a leading `<system-reminder>` block and skipping the `Caveat:` command preamble, bare command wrappers, and tool-result turns). Explicit `null` only when a session carries neither an explicit title nor any user prose. `title` itself is left untouched, so a consumer can tell "no title anywhere" from "not enriched".

  Enrichment costs **one lazy transcript parse per emitted row** — O(`--limit`), not the whole fleet. Plain `cv ls --json` stays a catalog-only, milliseconds-warm read; opt into `--enrich` when you need those fields on a bounded window.

#### Freshness — what `ls` guarantees

`cv ls` reads the **probed catalog**, not a full fleet scan: warm, it answers in milliseconds. The catalog's freshness rests on a staleness probe plus a time backstop (`CLUSTERVISION_MAX_STALE_SECS`, default **900s**):

- **A brand-new session is always seen.** Creating a session writes a new file, which bumps its directory's mtime; the probe re-stats the watched directories and re-discovers that harness before reading. Worst-case lag for a *just-created* session is one probe cycle, not the backstop window.
- **The bounded blind spot is an in-place append** to an *existing, older* session (one outside the 50 most-recently-updated files the probe re-stats): its `updated_at`/`message_count` can lag until the backstop forces a full re-discovery — at most `CLUSTERVISION_MAX_STALE_SECS`.
- **`--fresh` forces a full re-discovery** instead of trusting the probe — the escape hatch when you cannot accept even the bounded append lag. `CLUSTERVISION_MAX_STALE_SECS=0` has the same effect for every read.

A row whose file was deleted since the probe is dropped at read time (the `stat` that also yields `size_bytes`), so a stale row never survives into the output.

### `cv show`

Print a single session as a readable transcript, or as the raw IR.

```sh
cv show da9174f4
cv show da91 --harness codex
cv show da9174f4 --last 40             # a window — see above
cv show da9174f4 --json --last 40      # the IR for that window
cv show da9174f4 --pre-compaction      # the span a compaction discarded
cv show da9174f4 --subagents           # the sub-agent forest instead of the transcript
cv show da9174f4 --agent af19c2f1      # one sub-agent's transcript
```

The header line gives you harness, full id and cwd; then each message is printed turn by turn with tool calls and results inlined. A turn's label carries its [`kind`](architecture.md#the-unified-ir) when that is more than its role says — `── system · injected_context · bash_output_audience_note ──` tells you at a glance that the harness put this in the model's input and which reminder it was.

- `--json` — the unified IR as pretty JSON: session metadata including `system_prompt` and `lineage`, then `messages[]` with `role`, `kind`, `origin`, `content[]` (each block tagged by `type`), `usage`, and any harness-specific facts under `extra["<harness>"]`.
- the five [window flags](#message-windows).
- `--pre-compaction [N]` — read the span *before* a compaction boundary (the context a continued agent lost); defaults to the first boundary, `--pre-compaction 2` for the Nth. It sets the window for you, so it can't be combined with the window flags. Pair with [`cv compaction`](#cv-compaction) to see where the seams are.
- `--subagents` — instead of the transcript, list the sub-agent forest this session spawned (each child's type, journaled outcome, and return value). `--agent <agent-id>` renders one specific sub-agent's transcript, resolved through this parent. Sub-agents aren't in the main pool, so they're read through their parent.
- `cv show <agent-id>` also works **without** the parent: an id that matches no session is resolved as a sub-agent id across the fleet — one parent renders the agent directly with a provenance banner; several (fork lineages share sidecars) list ready-to-paste `cv show <parent> --agent …` commands.

A piped `cv show` with no window selector will not spray a whole giant transcript at you: past ~200 KB it prints the first and last 20 messages with a hint in between. Pick a window instead.

### `cv cat`

Print one tool call's full output — wherever it lives.

```sh
cv cat da9174f4 toolu_013rosc…            # the output
cv cat da9174f4 toolu_013rosc… --input    # the call's arguments, as JSON
```

The point is that "the tool's output" has three homes and you shouldn't have to care which one applies: inline in the transcript, in a [`prune`](#cv-prune) sidecar after you snipped it, or in a persisted-output file the harness wrote on the side (`<session>/tool-results/…`, Kimi's `tool-results/*.txt`). `cv cat` looks in all of them.

Output goes to stdout raw, so it redirects and pipes cleanly. `--input` prints a small header and then the arguments as JSON. An unknown id is an error that points you at the listing, and **exits 2**:

```text
no tool result "toolu_nope" in 406247d8 — `cv tools 406247d8 --timeline` lists its tool calls
```

Ids come from [`cv show`](#cv-show) and [`cv tools --timeline`](#cv-tools). This replaces the old `cv prune --retrieve`: fetching a tool's output is a Read, not an option on a reshape.

### `cv search`

Full-text search across the *content* of every session. See [Search](search.md) for indexing, BM25 and embeddings.

```sh
cv search "flux inference refactor"
cv search "nom parser" --harness codex --limit 10
cv search "formalizing proofs" --semantic
cv search "flux inference" --json
```

```text
claude    da9174f4  2026-05-28  the flux inference refactor
          … so the new flux inference path replaces the old type-walker …
```

By default `cv` uses the tantivy full-text index if you've built one (real tokenization + BM25, instant). With no index it does a live scan and nudges you to run [`cv index`](#cv-index). When an index exists it's authoritative — no matches means no matches, not "scan harder."

- `--semantic` — rank by *meaning* using stored embeddings instead of keywords. Requires `cv index --semantic` first; it downloads a small embedding model on first use. Unlike full-text it does **not** silently degrade: with no embeddings it errors and tells you what to run.
- `--json` — the same hits in the same order as one JSON array: a [session row](#things-that-work-everywhere) plus `score` (BM25, cosine for `--semantic`, `null` on a live scan), `snippet` (untruncated), and `agent_id`/`parent_id`/`workflow` (populated for hits inside the sub-agent forest when the index was built with `cv index --subagents`; `null` for a top-level hit — the keys are always present).

### `cv events`

Search answers *what was said*; the event catalog answers *what was done*. `cv events` lists one session's classified events — every file it edited or read, every command it ran, every tool error it hit.

```sh
cv events da9174f4                  # everything, in message order
cv events da9174f4 --kind error     # file_edit | file_read | command | tool | error
cv events da9174f4 --subagents      # the whole forest, attributed per agent
cv events da9174f4 --json
```

Events are ingested during [`cv index`](#cv-index); a session that isn't cataloged yet (or whose file changed since) is ingested on the spot, in one streamed pass with large content left on disk — so this works before a full index.

### `cv touched`

Every session that ever touched a file — the question you actually have when you're staring at code wondering where it came from.

```sh
cv touched crates/cv-core/src/ir.rs       # matched absolutely and by suffix
cv touched src/ir.rs --edits-only         # only sessions that *wrote* it
cv touched src/ir.rs --json
```

```text
claude    1d1465fa  2026-05-31  13 edit(s), 1 read(s)  Plan architecture refactoring…
```

### `cv tools`

Cross-agent tool analytics across the orchestrator **and** its whole sub-agent forest: per-agent histograms, which-agent-used-what, aggregate usage, and a wall-clock tool-call timeline.

```sh
cv tools 3b829648                          # aggregate histogram (orchestrator + forest)
cv tools 3b829648 --across                 # one row per agent
cv tools 3b829648 --agent af19c2f1         # one agent's histogram (<orchestrator> for the parent)
cv tools 3b829648 --tool Bash              # which agents used Bash, ranked
cv tools 3b829648 --workflow wf_ab915970   # restrict to one run's agents
cv tools 3b829648 --timeline               # chronological tool-call feed, tagged by agent
cv tools 3b829648 --json
```

`--timeline` is also how you find the `tool_use_id` to hand to [`cv cat`](#cv-cat).

### `cv tree`

Show how a session's messages thread together. If any message carries a `parent_id`, you get an indented DAG; otherwise a clean numbered list. Tool turns and sub-agent spawns are flagged.

```sh
cv tree da9174f4
```

```text
# the flux inference refactor
claude · da9174f4-… · 87 msg

• user  walk the type checker and find where flux is ignored
• assistant [🔧 tool, ↳ sub-agent (Task)]  dispatching a search…
  • tool [↩ result]  found 3 call sites …
  • assistant  here's the plan …
```

### `cv workflow`

A [`Workflow`](https://docs.claude.com/en/docs/claude-code)-tool run, first-class: its phase tree, the agents under each phase (with state, tokens, tool-calls, duration, and a result preview), run totals, the run's `log()` lines and aggregated **result**, and the driving script. Without a `<run_id>`, it lists the session's runs.

```sh
cv workflow 3b829648                        # list every workflow run in the session
cv workflow 3b829648 wf_ab915970            # render one run (run-id prefix is enough)
cv workflow 3b829648 census                 # …or address the run by its NAME
cv workflow stark-kill-emit-swarm           # no session id? the name resolves fleet-wide
cv workflow 3b829648 wf_ab915970 --script   # print the driving JS instead
cv workflow 3b829648 wf_ab915970 --results  # each agent's FULL journaled return value
cv workflow 3b829648 wf_ab915970 --follow   # stream a LIVE run's agent transitions (-f)
cv workflow 3b829648 wf_ab915970 --revive   # a resume prompt per unfinished lane
cv workflow 3b829648 wf_ab915970 --json     # the structured Workflow IR
```

Both arguments accept names: the `<run_id>` position matches a workflow name (exact, else unique prefix; a re-run name resolves to its newest run), and a first argument that matches no session id is resolved as a workflow name across the whole catalog — session titles are auto-generated and rarely mention the workflow you actually remember. A fleet-wide name miss falls through to a ghost-launch scan, so a swarm that died before its state was persisted is still findable by name.

Agents that didn't finish (`error`/`progress`) show where they last were — `↪ last: Bash · cd /x && grep … @ 07-06 23:05` — which is exactly what you want after a crash or rate-limit hit.

The default agent lines carry ~400-char result *previews* (that's all the state file keeps); `--results` (and `--json`) read the run's `journal.jsonl` and print each agent's **complete** journaled return — the full lane harvest.

`--follow` (`-f`) tails a live run: the harness flushes the state file as agents progress, so cv polls it and prints one line per agent state transition (`18:37:15  ✓ gate1:verify → done (63,198 tok)`), then emits the full render when the run reaches a terminal status. If the run hasn't registered yet it waits.

`--revive` salvages a dead or interrupted run: it emits a ready-to-paste standalone `Agent` prompt per lane — the FULL original task plus the work that lane already landed (files written, commands run, its own last note) — so lanes come back *resumed* rather than restarted. `--revive-all` includes the lanes that succeeded too.

Ghost launches are reported with everything the crash left behind: the run id recovered from the orphaned script file (written at launch, so it survives the crash that ate the state JSON) and the **debris counts** — `· run wf_be00… · DEBRIS: 10 agent transcript(s), 1 journaled result(s)` — pointing straight at the harvestable work under `subagents/workflows/<run_id>/`. The list view cross-checks the transcript's `Workflow` invocations against the state files to find them:

```text
⚠ 1 launch(es) with NO recorded run — state never persisted (crash/kill before write?):
   stark-kill-bugfix-swarm  launched 2026-07-08 01:02
   → any sub-agent debris sits under the session dir's subagents/workflows/
```

In `--json` the list form emits `{"runs": […], "ghost_launches": […]}`.

### `cv compaction`

Every context-compaction boundary in a session: each `/compact` (or auto-compaction), its trigger, the pre-compaction context size, and the summary that seeded the next window.

```sh
cv compaction 77230e3d               # list boundaries (trigger · pre-size · summary head)
cv compaction 77230e3d --summaries   # print each summary in full
cv compaction 77230e3d --json        # boundaries + each one's pre-compaction span
```

Boundaries are found by [`MessageKind::CompactionBoundary`](architecture.md#the-unified-ir), not by any harness's private field, so this works the same across harnesses that record compaction at all. To *read* the span a compaction discarded, jump straight to it with [`cv show --pre-compaction`](#cv-show).

### `cv timeline`

The same corpus, but read as a *feed*: oldest → newest, grouped by day. Good for "what was I doing last Tuesday, across all my tools."

```sh
cv timeline
cv timeline --harness codex --cwd clustervision --limit 80
cv timeline -q "msgs>=50" --json
```

```text
── 2026-05-27 ──
  09:14  codex     019e75e0  ~/pug/clustervision       porting the parser to nom
  16:40  claude    4f2a0c11  ~/pug/clustervision       wiring up the MCP server
── 2026-05-28 ──
  11:02  claude    da9174f4  ~/pug/clustervision       the flux inference refactor

2245 session(s)
```

It shows the most-recent `--limit` rows (default `60`) but prints them oldest-first, like a chat log. Each row sits at the session's **last activity** (local time); a session that started on an earlier day carries a `⇠ since 05-25` marker so a long-lived orchestrator isn't mistaken for one that just began. `--json` emits the same feed as [session rows](#things-that-work-everywhere), oldest first.

### `cv stats`

Fleet analytics: totals, a per-harness breakdown, your busiest directories, and the date range your corpus spans.

```sh
cv stats
cv stats -q "touched:src/ir.rs has:errors"   # analytics over a slice
cv stats --json
```

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

### `cv diff`

Compare two sessions message-by-message: a shared prefix marked `=`, then the divergence — `<` for messages only in A, `>` for messages only in B. Great for inspecting two [`loom`](#cv-loom) branches that started from the same root.

```sh
cv diff da9174f4 4f2a0c11
cv diff claude:da91 codex:019e            # per-side harness prefixes
```

```text
A claude   da9174f4  87 msg
B claude   4f2a0c11  91 msg

= user      walk the type checker and find where flux is ignored
= assistant here's the plan …
< assistant let's start with the walker
> assistant let's start with the constraint solver

42 shared, 45 only-in-A, 49 only-in-B
```

Each side may carry its own `harness:id` prefix, so you can diff sessions that live in *different* harnesses. A side without a recognized prefix falls back to the shared `--harness`.

### `cv blame`

Correlate a file's git history with the event catalog: which agent session's reasoning produced each commit — and jump straight into the conversation at the moment of the edit.

```sh
cv blame crates/cv-core/src/ir.rs         # commits ↦ matching sessions
cv blame src/ir.rs -L 42                  # who wrote *this line* (also `-L 42,80`)
cv blame src/ir.rs --show                 # print the conversation around the best match
```

Each matched commit prints the session, the message index of the nearest edit, and a copy-pasteable `cv show <id> --range A..B` centered on it. Time-correlation is honest about its limits: rebases and squashes shift commit times away from the edits that produced them, so matches are ranked, not asserted. Run [`cv index`](#cv-index) first to ingest events.

### `cv doctor`

Why does this session's context keep filling up? `cv doctor` attributes context pressure by source, sizes the fixed system+tools overhead from recorded token usage, and pairs it with compaction frequency.

```sh
cv doctor da9174f4
cv doctor                       # the most recent session for the current directory
cv doctor --recent 20           # aggregate the last 20 sessions for this cwd
cv doctor da9174f4 --json
```

```text
# compaction doctor — session 406247d8

Compaction:  never compacted across 650 message(s) — healthy ✅
Overhead:    ~261.2k of fixed context every turn before any conversation
Peak window: 284.9k observed

Conversational context by source (278.5k measured):
  thinking          42%  ██████████               117.9k
  tool-call args    26%  ██████                   71.6k
  tool results      20%  █████                    55.6k
  system reminders   9%  ██                       23.9k
  user text          2%                           5.0k
  assistant text     2%                           4.6k
```

It then ranks the top tool-result consumers, breaks the system reminders down by kind, and ends with a verdict naming the lever that would actually help. The "system reminders" bucket is [`MessageKind::InjectedContext`](architecture.md#the-unified-ir) — context the *harness* put into the model's input — so the number means the same thing on every harness, and the per-kind names come from the harness's own bag (`extra["claude"]["attachment_type"]` and friends).

The fixed overhead is *sized*, not itemized, and the report says so: the transcript records the conversation, not the system block, so cv infers the block's size from usage counts rather than pretending to enumerate it.

---

## Reshape

Six commands that produce a **new** session id from existing ones. The source session is never modified — not by `prune`, not by `port`, not by anything. (`resume` is the odd one out: it launches an existing session rather than making a new one, but it belongs with the "get me back into this" verbs.)

### `cv prune`

*Custom, lossless compaction* of a Claude Code session into a **new, resumable** session. The standard answer to a full context window is compaction — the model rewrites your whole history into a shorter summary, which is lossy by construction. `cv prune` does the opposite: it changes *nothing* about what was said, and instead lifts the **bulky old tool payloads** (large file reads, command logs, base64 screenshots) out of the conversation into a sidecar, leaving a small `[PRUNED id=…]` marker in their place. Prompts and the chronological flow are preserved verbatim; the most recent turns are kept untouched so the model stays sharp on the task at hand.

The output is a brand-new session (a fresh id stamped across every line) — resume it with `claude --resume <new-id>`. Nothing is lost: each snipped original stays in `<new-id>.flat.jsonl` and comes back out with [`cv cat`](#cv-cat).

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

- `--min-size <bytes>` — only snip a tool payload larger than this (default `2048`).
- `--keep-last <N>` — keep the last N conversational turns' payloads verbatim (default `25`).
- `--to <id>` — the new session id (default: a fresh UUID).
- `--drop-thinking` — also flatten the **oldest** assistant reasoning (thinking blocks); recent thinking (within `--keep-last`) stays verbatim. On a long Claude session the chain-of-thought *signatures* dominate the loaded context (Claude keeps a ~600-byte signature per thinking turn even when the reasoning text is omitted), so this is the biggest lever for shrinking what a resume loads. Lossless. Real example: it took a 976k-token session from ~98% to ~36% of a 1M window.
- `--drop` — discard payloads entirely instead of stashing them (smallest output, irreversible; the source is never touched regardless).
- `--window <tokens>` — a sliding window: keep only the newest turns totalling ≤ this many **real** tokens, dropping older turns. Sized from Claude's own recorded `usage` counts rather than a tokenizer estimate, so the budget lands true; the resumed session loads roughly this budget plus ~30k of system overhead. Lossy in the new session — the source keeps the full history.
- `--keep <A..B>` — the same thing by *index*: keep only this turn-index window (0-based, end-exclusive; `A..` through the last), dropping everything outside it. This is the flag that used to be spelled `--range`; it was renamed because it selects what to **keep**, while `--range` is the read-window flag everywhere else.
- `--copy-resources` — also copy the session's `subagents/`/`workflows/` dir under the new id (off by default; can be hundreds of MB). `claude --resume` doesn't need it — only cv's forest features on the pruned session do.
- `--no-revive` — opt *out* of the resume-gate fix. By default prune **resurrects an already-maxed session**: Claude Code's resume gate reads the last turn's recorded `usage` (input + cache tokens) as the session's current size, and checks it *before* re-sending anything, so a session sitting at the wall refuses to resume even after pruning has made the real content fit. Revive recomputes the honest size of the loaded window (everything after the last compaction boundary — what Claude actually re-sends) and rewrites the stale `usage` records to that figure. A no-op when the recorded size is already honest. `--no-revive` preserves the original records byte-for-byte.
- `--declassify` — also snip conversational *prose* (user prompts + assistant text blocks) dense in caller-supplied terms, into the sidecar with a `[PRUNED …]` marker (lossless). A message is snipped iff it holds ≥ 2 distinct terms, matched case-insensitively as substrings. Unlike the tool/`--drop-thinking` passes this ignores `--keep-last` — recent prose is snipped too, because the use case (a scorer reading the *whole* loaded context) doesn't care about recency. cv ships **no built-in term list**: supply terms with `--declassify-tokens t1,t2,…` and/or `--declassify-tokens-file <path>` (one per line, `#` comments and blanks ignored), else `--declassify` warns and snips nothing.
- `--dry-run` — compute and report without writing.
- `--json` — also emit the report as **one JSON object on stdout** (the human report stays on stderr, so stdout is pure JSON): `source_id`/`new_id` (FULL ids), `harness`, `before_bytes`/`after_bytes`, `snipped_payloads`, `image_blocks`, `tokens_freed`, `dropped_turns`/`window_real_tokens`, `revived`, `warnings`, `new_path`/`sidecar_path`/`copied_resources`, `dry_run`, `note`. Dry-run honest: nothing was written, so the paths — and `new_id`, unless `--to` pinned it — are explicit nulls with a `note` saying so.

Claude Code only for now (it operates on the raw JSONL to stay byte-faithful).

### `cv splice`

Stitch a new session together from spans of existing ones. Each spec is `<id>:A..B`, `<id>:A..` (through the last), `<id>:..B`, or just `<id>` (the whole session). Indices are 0-based and end-exclusive, and `<id>` may be `harness:id`.

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

Without `--harness` or `--out`, splice composes in memory and prints a summary; add `--export md|json` to dump the result. With `--harness`/`--out` it materializes the session for a harness.

- `<specs>…` — one or more span specs (**required**).
- `--harness <h>` — target harness (defaults to the first spec's harness).
- `--out <dir>` — write under this directory instead of the target's real storage root.
- `--export <md|json>` — print the composed session instead of emitting it.
- `--cwd <dir>` — rehome the composed session.
- `--generate` / `--gen-model <model>` — append an LLM-generated continuation.

### `cv loom`

A focused two-session graft: take `base[..N]`, then graft `graft[M..]` onto it, producing one new branched session. Where `splice` is general stitching, `loom` is the classic "rewind to message N, then continue along a different path."

```sh
cv loom da9174f4 --at 20 --graft 4f2a0c11 --from 35 --export md
cv loom da9174f4 --at 20 --graft 4f2a0c11 --from 35 --harness codex
cv loom da9174f4 --at 20 --graft 4f2a0c11 --from 35 --generate   # 🔮 grow a new branch
```

- `<base>` — base session id (positional). `--at <N>` — keep `base[..N]`.
- `--graft <id>` — the session to graft from. `--from <M>` — start grafting at `graft[M..]`.
- `--harness <h>` — target harness (defaults to the base's harness). `--out <dir>`, `--export <md|json>`, `--cwd <dir>`, `--generate`, `--gen-model` — as on `splice`.

> **`--generate` needs an LLM provider.** Set one of `OPENROUTER_API_KEY` (preferred), `ANTHROPIC_API_KEY`, or `LMSTUDIO_API_BASE=local` (a free local LM Studio server at `localhost:1234`). It appends a single generated assistant turn to the composed branch.

### `cv port`

**One verb for "produce a copy of this session that runs elsewhere."** Elsewhere can mean another harness (`--harness`), another working directory (`--cwd`), or both. This is the command that absorbed `cv convert` in 0.11.0 — they were always the same act.

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

By default it writes into the target harness's *real* storage root, so the session shows up in that tool immediately. Point `--out` at a scratch directory for a safe dry run. If the target harness can't be emitted to, `cv` says so plainly and lists the harnesses that can. See [Cross-harness conversion](conversion.md) for the emit-target list and the gory IR details.

- `--harness <h>` — target harness. Omitted, it defaults to the source harness — a pure rehome.
- `--cwd <dir>` — the new working directory. Rehoming rewrites the cwd baked into the target format (Claude's encoded project-dir name, Grok's percent-encoded path, Cline/Roo's `<environment_details>` line, …).
- `--out <dir>` — write under this directory instead of the target's storage root.
- `--no-context` — don't copy project context files. By default `port` also copies `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `GEMINI.md`, `MEMORY.md`, `.cursorrules` and `.windsurfrules` into the new cwd, so the ported session lands with its memory intact. It never overwrites an existing file at the target.
- `--strict` — fail if the fidelity check finds a loss the target format *could* have carried. Losses the format inherently cannot hold are still only reported, under `⚠ lost`.

Every port re-parses its own output and diffs it against the source IR per message — role and `kind` sequences, block-type counts, thinking (text / signature-only / encrypted), tool names on results, `is_error`, `details`, usage, model, timestamps, ids, plus the session's title, cwd, model, `system_prompt` and `lineage`. Each delta is classified as *expected for this target* (from a per-emitter table of what the format cannot carry) or *unexpected*; `--strict` fails on the latter.

### `cv redact`

Scrub secrets and PII — API keys, private keys, JWTs, emails, opaque blobs, `KEY=value` assignments — then export the cleaned session. The thing to reach for before you paste a transcript into an issue.

```sh
cv redact da9174f4 > safe.md
cv redact da9174f4 --format json --stats
```

```text
✦ redacted 7 item(s): 2 api_key, 1 private_key, 0 jwt, 3 email, 1 blob, 0 assignment
```

- `--format <md|json>` — output format (default `md`). `--stats` — per-class counts to stderr.

For a shareable artifact rather than a text dump, use [`cv share`](#cv-share).

### `cv resume`

Print the exact incantation to resume a session in its native harness — or, with `--launch`, run it for you (cd-ing to the session's cwd first).

```sh
cv resume da9174f4
cv resume da9174f4 --launch
```

```text
cd ~/pug/clustervision
claude --resume da9174f4-…
```

`cv` knows the resume command for the CLI harnesses it supports (`claude --resume`, `codex resume`, `opencode --session`, …). For desktop/IDE-only harnesses there's no documented CLI resume, so it prints a friendly note instead.

---

## Export

Three commands that produce something which is *not* a session.

### `cv export`

Render a session to a file format on stdout. Redirect it wherever you like.

```sh
cv export da9174f4 > session.md               # markdown (default)
cv export da9174f4 --format json > s.json
cv export da9174f4 --format html > s.html     # one self-contained file
cv export da9174f4 --last 100 --format md     # windows work here too
```

- `--format <md|json|html>` — output format (default `md`). `html` is a single self-contained page you can open or send to someone.
- the five [window flags](#message-windows) — identical to `cv show`'s.

(`show` and `export` are siblings: `show` is for your terminal, `export` is for a file or a pipe.)

### `cv dataset`

Export the corpus as a fine-tuning dataset — JSONL, one session per line. `chatml` (default) emits `{"messages":[…]}`; `sharegpt` emits `{"conversations":[…]}`. Both import directly into Unsloth Studio / TRL / HuggingFace `datasets` with no adapter. Streamed one session at a time, so memory stays flat over a multi-GB corpus.

```sh
cv dataset --out corpus.jsonl                                # whole corpus, chatml
cv dataset --format sharegpt --harness claude                # one harness, ShareGPT shape
cv dataset -q "model:fable" --subagents --out fable.jsonl    # a queried slice + its forest
cv dataset -q "harness:claude" --redact-only private_key     # strip PEM keys, keep the rest
```

- `-q`/`--query <calculus>` — only sessions matching the [query calculus](#cv-schema).
- `--subagents` — also emit each Claude session's sub-agent transcripts. A parent on one model often spawns sub-agents on another, and most model usage lives in the forest, so a `model:` query usually wants this.
- `--min-messages <n>` — drop sessions with fewer than `n` messages (default `2`).
- `--redact` — scrub every secret/PII class before emitting. `--redact-only <classes>` scopes it to a comma list (`private_key`, `api_key`, `jwt`, `email`, `blob`, `assignment`).
- `--limit <n>` — stop after `n` emitted records. `--out <file>` — write to a file instead of stdout.

### `cv pack`

Compile a context bundle for a new task out of your whole corpus — the one "build context from the corpus" verb, and the command that absorbed the old `recall` and `distill`.

```sh
cv pack "tantivy chunked indexing"                       # CLAUDE.md-style bundle → stdout
cv pack "fix the parser" --format prompt                 # shaped as a system prompt
cv pack "fix the parser" --format session --harness claude   # a synthetic resumable session
cv pack "migrate the daemon" --limit 3 --out CONTEXT.md
```

- `--format <md|prompt|session>` — output shape (default `md`).
- `--harness <h>` — target harness, for (and only for) `--format session`.
- `--limit <n>` — max past sessions to draw from (default `8`). `--out <path>` — write to a file.

It gets its own chapter: **[`cv pack` — the context compiler](pack.md)**.

---

## Fleet & live

### `cv task`

The fleet's durable dispatch objects: open/claim/note/done plus reviewed code **revisions** whose landing is *observed from git by cv* (`cv task verify`), never asserted by an agent. The verbs:

```text
open · list · show · claim · release · note · done · abandon · supersede
propose · reroute · pass · refute · verify · inbox · debt · stats
```

This is a big enough topic to get its own chapter — see **[the task substrate](tasks.md)** for the lifecycle, the four laws, the verifier, and a worked end-to-end example.

### `cv board`

The agent coordination board: a tiny message bus your agents (and you) use to post status, ask questions, hand off work, and claim keys so two agents don't stomp the same file. It's the same board the MCP server and [the daemon](daemon.md) expose; see [the board](board.md) for the bigger picture. Everything is organized into named channels.

```sh
cv board post build "tests are green on the nom branch" --tag ci --kind status
cv board read build
cv board channels
cv board watch build --match "deploy done"
```

The full set of actions:

| Action | What it does |
| --- | --- |
| `post <channel> <body>` | post a message. `--from`, `--kind`, `--tag` (repeatable), `--session-ref` |
| `read <channel>` | read messages. `--since <id>`, `--limit` (default `50`), `--json` |
| `channels` | list every channel |
| `watch <channel>` | follow live; `--match <substr>` exits when a body matches. `--since`, `--interval` |
| `request <channel> <body>` | post a question others can `reply` to; prints the request id. `--from` |
| `reply <channel> <in-reply-to> <body>` | answer a request by its id. `--from` |
| `replies <channel> <request-id>` | collect every reply to a request |
| `unanswered <channel>` | requests with **zero** replies, oldest first, with age — dropped questions made visible. `--within-secs` (default `86400`), `--json` |
| `claim <channel> <key>` | try to claim a key (a soft lease); exits non-zero on contention. `--from`, `--ttl-secs` (default `300`) |
| `release <channel> <key>` | release a claim you hold. `--from` |
| `claims <channel>` | list the active (un-expired) claims |
| `who <channel>` | list agents seen recently (and post your own heartbeat). `--within-secs` (default `60`) |
| `ack <channel> <message-id>` | acknowledge a message with a tiny ack note. `--from` |

A request/reply round-trip looks like this:

```sh
$ cv board request build "should I bump the MSRV to 1.82?"
✦ requested 3f9c0a21 on #build
  ↳ reply with: cv board reply build 3f9c0a21-… <body>

$ cv board reply build 3f9c0a21 "yes — CI already runs 1.82" --from codex-agent
✦ replied a7b1… to 3f9c0a21 on #build

$ cv board replies build 3f9c0a21
14:22:31  codex-agent  (reply) yes — CI already runs 1.82
```

And the claim/lease dance, for when two agents might race on the same work:

```sh
$ cv board claim build refactor-parser --from agent-a --ttl-secs 600
GRANTED  refactor-parser → agent-a (expires 2026-05-29 14:32:18)

$ cv board claim build refactor-parser --from agent-b
CONTENDED  refactor-parser is held by agent-a        # exits non-zero

$ cv board release build refactor-parser --from agent-a
✦ released refactor-parser on #build
```

`claim` exits non-zero when the key is already held, so it composes cleanly in scripts: `cv board claim … && do_the_work`.

### `cv scry`

`tail -f` for agent activity, across every harness at once. Run it and watch new sessions appear and existing ones grow, in real time. 🔮

```sh
cv scry
cv scry --harness claude --cwd clustervision
cv scry --existing            # also emit sessions already present at startup
cv scry --interval 1
```

```text
✦ scrying for agent activity… (Ctrl-C to stop)
✷ new  claude   da9174f4  ~/pug/clustervision  (3 msg)
      user walk the type checker and find where flux is ignored
   +  claude   da9174f4  ~/pug/clustervision  (1 msg)
      assistant found it — the walker drops flux constraints here …
```

- `--harness <h>` / `--cwd <substr>` — narrow what you follow.
- `--interval <secs>` — poll interval (default `2`).
- `--existing` — also emit sessions that already exist at startup (default: only new activity).

This is the CLI face of [the daemon](daemon.md), which mirrors the same activity into a live feed for your whole fleet (and powers the MCP `await_omen` block-until-match primitive).

### `cv share`

Redact a session and emit one self-contained HTML artifact anyone can open — offline, with nothing installed.

```sh
cv share da9174f4                  # → ./da9174f4….html, redacted
cv share da91 --out incident.html  # pick the filename
cv share da91 --no-redact          # keep secrets (loud stderr warning)
```

It gets its own chapter: **[Sharing transcripts](share.md)**.

---

## System

cv's own state, and the reference material.

### `cv index`

Build (or refresh) the search index. Run it once, re-run it whenever you want fresh sessions to be searchable.

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

Incremental by default: only changed/new sessions are re-indexed and vanished ones reaped.

- `--semantic` — also compute embeddings (downloads a ~30 MB model the first time).
- `--subagents` — also index the **sub-agent / workflow forest** (the transcripts under each Claude session's `subagents/`), tagged with their parent session, workflow run, and agent id. Without it, `cv search`/`cv touched` see only top-level sessions; with it, a hit can point you *inside* a workflow agent. It can add hundreds of MB to the index, so it's opt-in.
- `--rebuild` — clear and rebuild instead of updating incrementally.

`cv index` also ingests the **event catalog** in the same streaming pass — every tool call is classified (`file_edit`, `file_read`, `command`, `error`, `tool`) and stored in `catalog.db`, powering [`events`](#cv-events), [`touched`](#cv-touched) and [`blame`](#cv-blame). One read per changed session feeds both stores.

### `cv config`

View the user config (`$XDG_CONFIG_HOME/clustervision/config.toml`, falling back to `~/.config/…`) and manage the **export-source index**. Account data exports (the ChatGPT / Claude.ai "Export data" archives) have no fixed home, so you register where they live and the [`chatgpt-export`](harnesses.md)/[`claude-export`](harnesses.md) harnesses discover them from there.

```sh
cv config                              # print the config path + registered export sources
cv config --add-export ~/Downloads     # register a dir to scan (or a specific conversations.json)
cv config --add-export ~/exports/chatgpt-2026/conversations.json
cv config --rm-export ~/Downloads      # unregister
```

The file is plain TOML (`exports = ["…", "…"]`) — edit it by hand if you prefer. `$CV_EXPORTS` (a `:`-separated list) is honored as an ad-hoc union on top, for one-off runs. With nothing registered, export discovery is a no-op, so it never slows the default `cv ls` (these archives are large).

### `cv schema`

**The reference.** Not a session command: it prints what cv's vocabulary *is*. Bare, it's the human reference for the `-q` query calculus; `--json` is the machine-readable schema of every shape cv emits; `--commands` is the command tree.

```sh
cv schema                    # the full query-language reference
cv schema --json             # session_row · session · message · block · query, as JSON
cv schema --commands         # every command, grouped, with its one-line about
cv schema --commands --json  # the whole tree with every argument — what generates the MCP tools
```

`cv schema --json` publishes five shapes: `session_row` (keys, `enrich_keys`, `search_extra_keys`, timestamps), `session` (fields, `lineage`, `extra`), `message` (fields, and the `role`/`kind`/`origin`/`usage` vocabularies), `block` (its `type` tag and every type), and `query`. `cv schema --commands --json` emits, for every visible command and subcommand, `{ name, group, about, args: [{ name, kind, value_type, possible_values?, default?, help, required }] }` — produced from clap itself, which is why [the MCP tools](mcp.md) can be generated rather than hand-maintained and so cannot drift from the CLI.

This command used to be called `cv query`, which read like it ran one. It doesn't; it describes the language.

#### The query calculus

`ls`, `timeline`, `stats` and `dataset` take `-q`. The language is a boolean conjunction of terms — implicit `AND`, plus `OR`/`|`, `NOT`/`-`, and `( )` grouping:

```sh
cv ls -q "harness:claude model:fable"                 # fable sessions from Claude
cv ls -q "(model:fable OR model:opus) -title:test"    # either model, excluding tests
cv ls -q "msgs>=50 after:2026-01-01 tool:Bash"        # big recent sessions that ran Bash
cv ls -q 'title~"fly\.?io|aws"'                       # ~ is case-insensitive regex
cv stats -q "touched:src/ir.rs has:errors"            # analytics over a slice
cv ls -q "harness:claude agent:Explore subtool:Bash"  # a forest query
```

Fields, by what they cost to answer:

- **catalog** (free, no parse): `harness`, `cwd`, `title`, `id`, `msgs`, `created`, `updated` (`before:`/`after:`/`since:`/`until:` are sugar).
- **parse** (reads the transcript): `model` (any turn's model), `git` (branch/remote), `thread` (a message-tree path — below).
- **index** (needs `cv index`): `text` — full-text over content.
- **events**: `tool` (a tool *this session* ran), `touched` (a file it read/edited), `has` (flags: `subagents`, `errors`, `tools`, `images`, `compacted`, `workflows`).
- **forest** (walks the [sub-agent forest / workflows / compaction seams](#cv-workflow) — the priciest): `subtool` (a tool a *sub-agent* ran), `agent` (spawned a sub-agent of this type), `agents` (forest size), `workflow` (ran a matching `Workflow`), `workflows` (run count), `compactions` (boundary count).

Operators: `:` (contains), `=` (exact), `~` (case-insensitive regex, local string fields only), and `> >= < <=` for numbers/dates; values can be comma-OR-lists (`a,b`) or ranges (`lo..hi`). A bare word matches title/cwd/id. The evaluator prunes with the catalog-cheap terms **first**, so a forest/parse/index term only ever runs on what survives — always pair one with a `harness:`/`cwd:`/`msgs` term to stay fast.

```sh
cv ls -q "harness:claude agents>=5 subtool:Bash compactions>=1"   # deep, tool-heavy, compacted runs
cv ls -q "workflow:census OR workflows>=3"                        # ran a census workflow, or 3+ runs
```

**`thread:` — a CSS-y message-tree path.** Match a parent→child chain through the session's message DAG (the loom/threading dimension), using the CSS child combinator `>`. Each step matches one message; a step is a role (`user`/`assistant`/`tool`/`system`), `tool:NAME`, `text:WORD`, or a bare word (content contains). Quote the whole value so the spaces and `>` belong to the path, not the outer query:

```sh
cv ls -q 'harness:claude thread:"text:refactor > assistant"'   # a "refactor" turn whose reply is an assistant turn
cv ls -q 'thread:"user > assistant > tool:Bash"'               # a user turn that led to a Bash tool-use
```

Each `>` is a *direct* child (a reply), resolved via `parent_id`, so it follows the real threading — including loom branches and sidechains. (Harnesses that don't record message ids can't thread, so `thread:` simply won't match there.)

### `cv formats`

The format census and the manifest check — cv's own honesty about how much of each harness's on-disk vocabulary it actually understands.

```sh
cv formats census                  # what real sessions hold, per harness
cv formats census --harness codex  # just one
cv formats check                   # every adapter against formats/<harness>.toml
```

**`census`** parses recent sessions per harness in *format-complete* mode, where every record an adapter doesn't interpret is carried through as a [`carrier`](architecture.md#the-unified-ir) message tagged with its record type. It then reports, per harness, the record/part/column vocabulary actually seen with counts — marking anything absent from the manifest as **new**. That "new" list is the drift signal: it's what a harness shipped since cv last looked.

**`check`** compares each adapter's source against its manifest, `formats/<harness>.toml`, in both directions: a type the manifest calls `handled` that never appears in the adapter's match arms, and a type-like literal the adapter matches on that the manifest doesn't list. A manifest records the upstream it was verified against (`repo`, `commit`, `date`), the store paths, and every persisted type with a status:

| status | meaning |
| --- | --- |
| `handled` | the adapter has a per-item arm for it |
| `generic` | interpreted by a blanket rule, so no per-item literal exists |
| `carried` | kept verbatim as a carrier under `ParseOptions::complete`, dropped otherwise |
| `ignored` | known and deliberately skipped (transient, UI-only, or a duplicate) |

The manifests ship embedded in the binary, so an installed `cv` carries the vocabulary it was built against. A test asserts the two directions agree, which means a new match arm in an adapter fails the build until the manifest names it — the check is a gate, not a report you can forget to read.

### `cv recipes`

The agent quickstart: the ten things agents actually do with cv, each as one command line with the JSON keys it returns.

```sh
cv recipes
```

It covers finding your own session by cwd, reading the last N turns, reading a sub-agent's return, fleet search, getting a workflow lane's prompt, doctoring a session, pruning and resuming a maxed-out one, porting, fetching one tool call's output, and listing what a session touched — each with its `→` output shape. It also restates the id rules and the [window flags](#message-windows) up front, because those are the two things every agent gets wrong first.

`cv --help` ends by pointing at it. If you are an agent reading this manual, run `cv recipes` first and come back here for the detail.
