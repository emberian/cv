# The CLI: `cv`

`cv` is the front door to clustervision. One small binary that can find, read, search, reshape, port and stream every AI coding session on your machine — across every harness it knows how to parse.

Everything `cv` does, it does over your *real* local session storage (`~/.claude`, `~/.codex`, `~/.config/opencode`, and friends). There's no database to set up and nothing to import; discovery happens by scanning the harnesses you already have installed. (Search gets an optional index — more on that below.)

```sh
cv --help          # every command, grouped
cv <cmd> --help    # flags for any one command
cv recipes         # the agent quickstart: one command line per task, and the JSON it returns
```

This chapter is what the commands are *for*: the examples, the sample output, and the reasons.
Every flag of every command is in the [command reference](reference.md), which is generated from
the binary when this manual is built, so it cannot fall behind it.

## Renamed in 0.11.0

0.11.0 is a **clean break**: the old names are gone, with no aliases. Each one errors with a pointer to its replacement, so a stale script tells you exactly what to type instead.

| old | new |
| --- | --- |
| `cv convert <id> --to <h>` | `cv port <id> --harness <h>` |
| `cv port --to <h>` / `--to-dir <d>` | `cv port --harness <h>` / `--cwd <d>` |
| `cv query` | [`cv schema`](#cv-schema) |
| `cv recall` | removed — [`cv search`](#cv-search) to find content, [`cv pack`](#cv-pack) to build context. (`cv distill` went with it and came back in 0.14 with a new meaning: a deterministic lane reshaping, see [`cv distill`](#cv-distill).) |
| `cv prune <id> --retrieve <tool_use_id>` | [`cv cat <session> <tool_use_id>`](#cv-cat) |
| `cv prune --range A..B` | [`cv prune --keep A..B`](#cv-prune) |
| `cv show --range <start>-<end>` / `--range -N` | the [window flags](#message-windows): `--range A..B`, `--first N`, `--last N` |
| `cv splice <id>:A-B` | `cv splice <id>:A..B` |
| `cv splice`/`cv pack --to <h>` | `--harness <h>` |
| `cv help`'s agent footer | [`cv recipes`](#cv-recipes), a real command |

The principle behind all of it: **one name, one meaning, no grammar to memorize.** A verb names one act; a flag means the same thing on every command that has it; a JSON key is spelled one way.

## The map

`cv --help` prints these groups, and nothing is hidden from it. **Read** commands only ever read;
**Reshape** commands produce a **new** session id from existing ones and never touch the source;
**Export** commands produce something that is *not* a session; **Fleet & live** is
multi-agent coordination, live views and the servers; **System** is cv's own state and reference.

{{#cv-command-map}}

## Things that work everywhere

**Session ids.** Almost every command takes a session id, and a unique **prefix** is enough. The ids are long UUIDs; you'll usually paste the short 8-character form `cv ls` prints. You can also write `harness:id` anywhere an id goes.

```sh
cv show da9174f4            # resolved by prefix
cv show codex:019e75e0      # or fully qualified
cv show da91 --harness codex
```

An **ambiguous** prefix is never resolved by guessing: cv lists the candidates as `harness:full-id` lines and exits `2`. Surprising you with the wrong session is worse than making you type three more characters.

**The shared flags.** A word means the same thing on every command that has it (which
commands have it is in the [reference](reference.md)):

| flag | meaning |
| --- | --- |
| `--harness <h>` | filter to, or target, one harness — never spelled `--to` |
| `--cwd <dir>` | a working directory: a filter when reading, the new home when reshaping |
| `--out <dir\|file>` | write somewhere other than the real store |
| `--json` | machine output |
| `--limit <n>` | how many rows/records |
| `--all` | include what is hidden by default |
| `--thinking <mode>` | reasoning policy: `native`, `portable`, `text`, `drop`; rewind accepts native/portable |
| `--strict` | fail if a loss the target format *could* have carried happens |
| `--skills-policy <p>` | restored skills in a new Claude session: `keep`, `drop-all`, `drop-stale` |

**`--thinking` and replay.** Thinking blocks come in two shapes. A few carry plaintext. Most carry
only a provider **signature** — an opaque, vendor-bound blob with no text at all (on one real
900-message session, 177 of 204). A session recorded through a proxy carries the proxied model's
reasoning and tool IDs, which the Anthropic API was never meant to read back.

`cv rewind` and `cv port` choose per session: `native` when every recorded model is `claude-*`
(rewind looks at the window it copies), `portable` otherwise. An explicit `--thinking` wins.

- `native` — structured reasoning where the target holds it, dropped where it does not.
  An assistant turn whose only content was a signature blob disappears, reported as
  `unrepresentable_turns`. Claude output keeps its signatures and tool IDs byte for byte. The
  default for the other emit commands and the library.
- `portable` — for Claude, readable reasoning becomes ordinary text, reasoning without plaintext
  (signature-only, redacted) is removed, and a record left empty goes too, its children relinked
  to its parent. Illegal tool IDs receive collision-free replacements at both ends of every
  call/result pair. Other port targets use the `text` policy.
- `text` — preserve structured reasoning where supported; otherwise keep readable reasoning as text
  and opaque reasoning as a placeholder. This mode does not remove signatures from Claude output.
  Lost signatures are still reported.
- `drop` — omit reasoning entirely, even where the target could hold it, for handing someone a
  session without your chain of thought.

Claude exports repair illegal tool IDs in every non-native mode. Rewind supports only `native` and
`portable`; other emit commands expose the full policy set. Checked live with Claude Code 2.1.299
against `claude-opus-4-8`: `claude -p --resume <id> --fork-session` answered from a native rewind of
a Claude session, a portable rewind of the same session, and a portable rewind of a proxied
`grok-4.7-build` session, each cut on a tool result.

Note that `cv prune --drop-thinking` is a different thing: it omits old reasoning to shrink a
session you intend to resume. It was spelled `--thinking` before 0.11.0, and was renamed precisely
so that one word does not mean two things.

**`--skills-policy` and restored skills.** Claude Code records each skill a session invokes with
the text it had then, and every compaction writes all of them back as one `invoked_skills` record;
`--resume` rebuilds the set from those records after the last compaction. Nothing forgets an
entry, so a skill invoked once comes back with its old text at every later compaction, even after
its SKILL.md changed or the skill was removed. `cv prune`, `cv rewind` and `cv port` (to Claude)
take `--skills-policy`: `keep` (the default) copies the records, `drop-all` empties them, and
`drop-stale` drops each skill whose SKILL.md (a user skill under the config dir, a project skill
under the recorded cwd) is gone or differs from the stored text. Bundled and plugin skills have no
file to compare and are kept. A rewritten record loses its stored rendering, so Claude Code renders
it again from the kept list; a dropped skill can still be invoked and then gets its current text.
The report and `--json` carry a `skills` object (`dropped`, `dropped_chars`, `kept`, `records`).

**JSON is snake_case, everywhere.** Every `--json` output and every MCP payload uses snake_case keys, and timestamps are RFC 3339 strings. There is **one** session-row shape, and `ls`, `search` and the MCP tools all emit it:

```json
{ "id": "…", "harness": "claude", "path": "…", "cwd": "…", "title": "…",
  "created_at": "2026-05-28T09:14:02.451+00:00", "updated_at": "…",
  "message_count": 87, "size_bytes": 412998 }
```

`ls --enrich` adds `display_title` and `git`; `search --json` adds `score`, `snippet` and the sub-agent provenance trio `agent_id`/`parent_id`/`workflow`. A **message** is the IR `Message` (`role`, `kind`, `origin`, `content`, `usage`, …) and a **block** is the IR `Block`, tagged by `type` — see [the IR vocabulary](architecture.md#the-unified-ir), which is the one place the words are defined. [`cv schema --json`](#cv-schema) publishes all of it machine-readably.

**`-q`, the query calculus.** [`ls`](#cv-ls), [`stats`](#cv-stats) and [`dataset`](#cv-dataset) take a `-q`/`--query` selector — one small boolean language for picking sessions: `harness:claude (model:fable OR model:opus) msgs>=50 -title:test`. The full reference lives under [`cv schema`](#cv-schema).

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

The commands that only ever read. None of them writes to a session store.

### `cv ls`

List discovered sessions, newest-updated first.

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

Each row shows the session's `created → last-active` span in **local time** (same-day sessions compress the right side to a bare time). The span is what separates a long-lived orchestrator from a one-shot — and, after a crash, the resumed sessions from the dropped ones. When a session has no title, `cv` falls back to a dimmed cwd so the row still tells you *where* it happened.

`--asc` flips the order of the window, never its membership: `--limit N --asc` is still the newest
N sessions, read like a chat log ("what was I doing last Tuesday, across all my tools"). `--json`
prints the shared [session row](#things-that-work-everywhere) with transcript-derived text
(`title`) **raw** — the machine contract, not the sanitized terminal view. `--enrich` adds `git`
(the branch the session *ran on*, read from the transcript — not the cwd's current branch) and
`display_title`: the row's `title` when it has one, else a title synthesized from the first real
user turn (peeling a leading `<system-reminder>` block and skipping harness-injected turns such as
Codex's AGENTS.md and environment context, the `Caveat:` preamble, bare command wrappers and
tool-result turns); it is explicit `null` only when a session has neither. `title` itself is left
untouched, so a consumer can tell "no title anywhere" from "not enriched". Enrichment costs one
lazy transcript parse per emitted row — O(`--limit`), not the fleet — so plain `cv ls --json`
stays a catalog-only read.

#### Freshness — what `ls` guarantees

`cv ls` reads the **probed catalog**, not a full fleet scan. On a 7,000-session, 95 GB fleet a warm
`cv ls` takes 0.2–1 s (it checks every primary source, below). The first read on a new home, or
after an upgrade that changes how sessions are read, re-discovers the whole fleet: 4.5 minutes and
14 CPU-minutes on that fleet. cv says so on stderr when an upgrade causes it; run `cv index` right
after upgrading to pay it up front. The catalog's freshness rests on a staleness probe plus a time backstop (`CLUSTERVISION_MAX_STALE_SECS`, default **900s**):

- **A new session in a watched directory is seen on the next read.** Creating a session bumps its directory's mtime; the probe re-stats watched directories and re-discovers the affected harness before reading. A first session in a previously empty deeper directory can escape those watches and remain unseen until the time backstop forces full discovery — at most `CLUSTERVISION_MAX_STALE_SECS` after the last full sync, on the next read.
- **A new storage root is seen too.** When a root appears that the catalog does not watch yet (a new agent under a `*` entry in `claude-roots`, or a line added to that file — see [Harnesses](harnesses.md#claude-code-sessions-outside-claude)), the probe re-discovers that harness.
- **Changes to existing sessions are checked regardless of age.** The probe checks every recorded primary source's captured metadata identity, plus recorded auxiliary dependencies such as DB sidecars. An append to a quiet session therefore triggers re-discovery of its harness on the next read. These checks compare metadata identities, not content hashes.
- **`--fresh` forces a full re-discovery** instead of trusting the probe: `cv ls --json --fresh --limit N` is a current inventory on every call. `CLUSTERVISION_MAX_STALE_SECS=0` has the same effect for every read.

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
cv show da9174f4 --tree                # the message threading, then the sub-agent forest
cv show da9174f4 --kind prompt,answer  # only what the person said
cv show da9174f4 --kind compaction_boundary,compaction_summary   # the compaction seams
```

The header line gives you harness, full id and cwd; then each message is printed turn by turn with tool calls and results inlined. A turn's label carries its [`kind`](architecture.md#the-unified-ir) when that is more than its role says — `── system · injected_context/bash_output_audience_note ──` tells you at a glance that the harness put this in the model's input and which reminder it was.

`--json` is the unified IR: session metadata including `system_prompt` and `lineage`, then
`messages[]` with `role`, `kind`, `origin`, `content[]` (each block tagged by `type`), `usage`,
and harness-specific facts under `extra["<harness>"]`. The [window flags](#message-windows) apply;
`--pre-compaction [N]` sets the window for you (pair it with [`--kind compaction_boundary`](#views)
to see where the seams are). `--subagents` lists the forest instead of the transcript, and
`--agent <agent-id>` renders one child through its parent. `cv show <agent-id>` also works
**without** the parent: an id that matches no session is resolved as a sub-agent id across the
fleet — one parent renders the agent directly with a provenance banner; several (fork lineages
share sidecars) list ready-to-paste `cv show <parent> --agent …` commands.

A piped `cv show` with no window selector will not spray a whole giant transcript at you: past ~200 KB it prints the first and last 20 messages with a hint in between. Pick a window instead.

#### Views

`--subagents`, `--agent`, `--tree` and `--kind` select views of the same session.
`--kind` accepts three families: `prompt,answer`; `compaction_boundary,compaction_summary`;
and affordances such as `plan`, `goal`, `schedule`, `background_job`, `skill`, `hook`,
`question`, `approval`, `checkpoint`, `branch`, `fork`, `model_change`, `subagent_spawn`,
and `subagent_return` (`affordances` selects all of them). Unknown kinds or a mix of
families exit 2. Prompt and affordance rows accept message windows; compaction rows
cover the whole session. These row views print whole rows and reject `--max-bytes`.

#### The threading: `--tree`

`cv show <id> --tree` shows how a session's messages thread together. If any message carries a `parent_id`, you get an indented DAG; otherwise a clean numbered list. Tool turns and sub-agent spawns are flagged, and the sub-agent forest follows. It covers the whole session; with `--json` you get the IR itself, whose messages carry the same `parent_id`.

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

Only what the person said. Every human-typed prompt (`kind: prompt`, `origin: human`) and every `AskUserQuestion` answer (the tool results that begin `The user answered` / `Your questions have been answered`), in order, each with its message index and local timestamp. An orchestrator re-reading its brief after a compaction, or a reviewer asking "what was actually asked for?", gets the 40 lines that matter out of a 10,000-record transcript.

```sh
cv show 0c315aee --kind prompt,answer                    # [idx] time  user|answer, then the full text
cv show 0c315aee --kind prompt,answer --json             # [{index, timestamp, kind: prompt|answer, text}]
cv show 0c315aee --kind prompt,answer --pre-compaction   # only the span the first compaction discarded
cv show 0c315aee --kind prompt --range 0..500            # typed prompts only, in a window
```

The bookkeeping rows a slash command leaves behind (`<command-name>/compact</command-name>`, `<local-command-stdout>…`) are not prompts — Claude Code ≥ 2.1 writes them as `user` records, and the adapter classifies them as harness notices — so `/compact` appears once, as the line the person typed. Prompts are printed whole, never truncated; `cv show --around <index>` jumps to the conversation around any of them.

#### The compaction seams: `--kind compaction_boundary,compaction_summary`

Every context-compaction boundary in a session: each `/compact` (or auto-compaction), its trigger, the pre-compaction context size, and the summary that seeded the next window.

```sh
cv show 77230e3d --kind compaction_boundary,compaction_summary               # trigger · pre-size · summary head
cv show 77230e3d --kind compaction_boundary,compaction_summary --summaries   # each summary in full
cv show 77230e3d --kind compaction_boundary --json   # boundaries + each one's pre-compaction span
```

Boundaries are found by [`MessageKind::CompactionBoundary`](architecture.md#the-unified-ir), not by any harness's private field, so this works the same across harnesses that record compaction at all. Either kind selects the same rows, each boundary paired with its summary, over the whole session. To *read* the span a compaction discarded, jump straight to it with `cv show --pre-compaction`.

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

`cv search` answers from the tantivy full-text index (real tokenization + BM25, instant), and the index is authoritative: no matches means no matches, not "scan harder." With no index, or one it cannot open, it fails at once naming the cause and the fix ([`cv index`](#cv-index)); `--scan` asks for a slow, head-capped live scan instead and says so on stderr. It matches the exchange (prompts, replies, tool calls and results); `--kind context` matches the injected instruction files, system prompts, hook output and notices around it instead, and `--kind all` both.

`--semantic` ranks by *meaning* using stored embeddings (`cv index --semantic` first; a small
model downloads on first use). Unlike full-text it does **not** silently degrade: with no
embeddings it errors and tells you what to run. `--json` is a [session row](#things-that-work-everywhere)
per hit plus `score` (BM25, cosine for `--semantic`, `null` on a live scan), the untruncated
`snippet`, and `agent_id`/`parent_id`/`workflow` (set for hits inside the sub-agent forest when the
index was built with `cv index --subagents`; `null` for a top-level hit, and always present).

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

`--timeline` is also how you find the `tool_use_id` to hand to [`cv cat`](#cv-cat): each line (and each `--json` entry's `tool_use_id`) carries its call's id.

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

### `cv lanes`

The sub-agent forest as a status table — what `cv show --subagents` lists, with the numbers an orchestrator actually wants per lane: model, start, duration, tokens (deduplicated by API message id, like `cv stats --tokens`), tool calls, status, and either the last line of its return or, for a running lane, its last tool call.

```sh
cv lanes 0c315aee                 # launch order, one row + one detail line per lane
cv lanes 0c315aee --running       # no stop recorded: still at it (shows the last tool call)
cv lanes 0c315aee --done          # finished for real
cv lanes 0c315aee --stranded      # the strand class, each with `→ resume: SendMessage to <agentId>`
cv lanes 0c315aee --tasks         # each lane joined to the tasks its endpoint holds, with the last note
cv lanes 0c315aee --json          # [{agent_id, model, tokens: {total, …}, status, status_source, last_text, stranded, …}]
cv lanes --recent 10 --unfinished # the project's 10 most recent sessions: every lane not finished for real
```

`--recent N` reads the forests of the N most recently active Claude sessions whose cwd is `--cwd` (default: the current directory) instead of one session's, and tags each row with its session. `--unfinished` keeps the lanes that did not finish for real — running, stopped, killed, failed or stranded. Together they are the rescue list for `cv adopt`: the agents a dead session left behind, with the `cv adopt <agent-id>… --into <live-session>` hint under the table.

```text
AGENT     STATUS     MODEL          STARTED         DUR  TOKENS CALLS  DESCRIPTION
acd03d62  completed  opus-4-1       09-30 23:05  1h12m    4.8M   203  K-STREAM: append payload + receiver leg
          ↩ `git status` shows only the three prover files and `build-logs/` that were already there before…
a1e1ce3d  running    opus-4-1       10-01 00:11  2h01m    2.1M    88  K-SPK: kernel items for SPK hosting
          ↪ Bash · ssh persvati 'cd /home/ember/build/mini-datamodel-20260930/k-spk/src && lake build'
```

**Status** comes from the most authoritative source available, and the JSON says which (`status_source`): a `Workflow` run's journal (`done` / `partial` / …), else the parent transcript's last `<task-notification>` for the agent (`completed` / `failed` / `killed` / `stopped` — read from the `queue-operation` records the harness writes the moment a child stops, so it does not depend on the notification ever reaching the conversation), else the child's own `SubagentStop` hook (`stopped`), else `running`.

**Why a lane died.** A `failed` or `killed` lane carries a `failure_cause` read from its
transcript's last API-error notice that no real turn followed (a resume clears it): an API 429 /
"session limit" / "usage limit" is **`rate-limited`** — with `resets_at` from the record's
`quotaLimits.resetsAt` and the notice's own text, and the remedy *resume with one message after
the reset*; "Prompt is too long" / a context-length error is **`context`** — it cannot be resumed,
relaunch from its clone with a STATUS hand-off; no notice but a `SubagentStop` after the last turn
is **`stopped`**; anything else stays plain `failed`. The STATUS column reads `failed:rate-limited`,
the header counts the causes (`4 other (2 context)`), and the page's status dot carries the cause,
the notice and the reset in its title.

**Stranded** is the class that parked four lanes in one day: the harness reports the lane *completed*, and its final text says it is waiting — `Waiting on notifications`, `I'll continue when the monitor fires`, `waiting for the … verdict`. Nothing will wake it. `--stranded` lists exactly those, with the resume hint; a stranded lane never counts as done (`--done` and the header's `completed` exclude it). The phrase set is `cv_core::lanes::STRAND_PATTERNS`; only the last two sentences of the text are consulted, so a report that mentions waiting and then concludes is not stranded.

**`--tasks`** joins the lane table to the task store. A lane's endpoint is the first
`CV_ENDPOINT=<kind>:<name>` its own shell commands exported (exact — `endpoint_source: transcript`,
and reported in `--json` even without `--tasks`; a child's export quoted in an `Agent` prompt or a
written brief is the child's, not this lane's); failing that, its description's leading token
(`FIX-KICK: a kick ends…` → `lane:fix-kick`) matched case-insensitively against the store's
assignees (`description`, a guess by name, labelled `(by description)`). Under each lane: the
endpoint and up to five tasks it holds — short id, state, title, the last note's first line. The
serve page's Lanes pane shows the newest one in a `task` column (the last note on hover, all of
them in the expanded row).

```text
a06df2ba  running    opus-5-5       10-01 20:26   3h34m   68.9M   223  FIX-KICK: a kick ends authority in the room
          ↪ Bash · ssh persvati 'for i in $(seq 1 58); do grep -qE "^STEP J17 (PASS|FAIL)" /home/e…
          ⚑ lane:fix-kick: 4 task(s)
            01a0f9e2-5fde [claimed] AUDIT: sealing a room cell ends kick, invite and re-law whi…
```

### `cv deferrals`

A linter for promises. Every place the assistant put something off — "later lane", "follow-up", "not tonight", "queued for …", "your call", "decision for", "integrator item", "when X lands", "after FINAL", "deferred", "a later pass", "out of scope" by default — with its message index, timestamp and ~100 characters of context. The complaint it answers: *we can't just remark these things into the context log.*

```sh
cv deferrals 0c315aee                      # every deferral, oldest first
cv deferrals 0c315aee --since 1591         # only the post-compaction region
cv deferrals 0c315aee --open-tasks         # cross-reference the task store; exit 1 while any is UNMATCHED
cv deferrals 0c315aee --open-tasks --json  # [{index, timestamp, phrase, context, matched?: {task_id, title, state, shared}}]
cv deferrals 0c315aee --phrases-file deferrals.txt   # your project's phrase table instead of the built-in one
```

**The phrase table is yours.** The built-in list is a default, not a vocabulary cv insists on: a project's own idiom ("parking lot", "post-launch", the name of its integration phase) goes in a file of `label = regex` lines (`#` comments, regexes matched case-insensitively), passed with `--phrases-file` or kept at `~/.config/clustervision/deferrals.txt`, which is used whenever it exists. `cv schema --json` lists the default labels under `deferral.default_phrases`.

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

`--open-tasks` scores each deferral's sentence against every task's title, body and issue (terminal tasks included — a deferral may already be done): three or more shared significant words is a MATCH, the task with the most wins, ties to the newer task. It is a heuristic and says so (`shared:` lists the words), so a closeout reads the matches rather than trusting them. The non-zero exit is the point: `cv deferrals <id> --open-tasks || echo "open the rest first"`.

### `cv stats`

Fleet analytics: totals, a per-harness breakdown, your busiest directories, and the date range your corpus spans.

```sh
cv stats
cv stats -q "touched:src/ir.rs has:errors"   # analytics over a slice
cv stats --json
cv stats -q "cwd:dev/fn" --tokens            # + token usage per harness and model
```

`--tokens` parses every matched transcript, and every Claude sub-agent and `Workflow` agent under
it, and totals token usage per harness and model: uncached input, cache writes, cache reads,
output, and provider-reported cost where the harness records one. The counts are disjoint, so rows
from different harnesses add up; `uncached` is input + cache writes + output, the part that was not
re-read from cache. A response Claude Code wrote as several lines (or copied into a resumed
session) is counted once. It is slower than plain `stats`, which reads only the catalog: about a
minute over 1,600 transcripts. With `--json` the payload gains a `tokens` object
(`total`, `by_harness[]`, `by_model[]`).

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

Correlate a file's git history with the event catalog: which agent session's reasoning produced each commit — and jump straight into the conversation at the moment of the edit.

```sh
cv blame crates/cv-core/src/ir.rs         # commits ↦ matching sessions
cv blame src/ir.rs -L 42                  # who wrote *this line* (also `-L 42,80`)
cv blame src/ir.rs --show                 # print the conversation around the best match
```

Each matched commit prints the session, the message index of the nearest edit, and a copy-pasteable `cv show <id> --range A..B` centered on it. Rebases and squashes shift commit times away from the edits that produced them, so time-correlated matches are ranked, not asserted. Run [`cv index`](#cv-index) first to ingest events.

Some matches are not heuristic. When a candidate session ran the commit itself through a tool — a `git commit` (or `merge`, `cherry-pick`, `revert`) whose output names the sha, or a `git push` that does — blame reads that straight out of the transcript and ranks it above every timed match, labeled `exact: commit created here` (or `exact: commit pushed here`). That line also carries a [`cv rewind`](#cv-rewind) hint, which resumes that agent as it was at the moment of the commit:

```text
◆ d41c0de 2026-10-09  gate plan: an undeclared linked worktree names its repository's project …
  claude   7b2e19a4  2026-10-09  "Refactor the request router"  commit at msg 1240 (exact: commit created here)
    ↳ cv rewind 7b2e19a4 --at d41c0de
    ↳ cv show 7b2e19a4 --range 1237..1243
```

The candidates are the sessions with an edit on the file (plus, for sub-agents cataloged by `cv index --subagents`, the session that spawned them) whose transcript was written to after the commit; a session that committed without ever editing the file through a tool is not scanned. The scan reads the transcripts themselves, so it doesn't need a fresh index — only the candidate list comes from the catalog. A rebased or cherry-picked commit has a sha no session ever printed, and a commit made with silenced output (`-q` and no `rev-parse`) leaves no proof.

### `cv doctor`

Why does this session's context keep filling up? `cv doctor` sizes the transcript's content by source, reads the prompt sizes the provider recorded, and pairs them with compaction frequency.

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

It then ranks the largest tool-result payloads, breaks the system reminders down by kind, and names the sources worth cutting. The "system reminders" bucket is [`MessageKind::InjectedContext`](architecture.md#the-unified-ir) — context the *harness* put into the model's input — so the number means the same thing on every harness, and the per-kind names come from the harness's own bag (`extra["claude"]["attachment_type"]` and friends).

Sources are sized with cv's one token estimator over the whole transcript, compacted history
included, so the percentages say where the session's bytes went, not what the current window
holds. Each content kind has its own rate, calibrated against the API's `count_tokens` on 42
requests Claude Code built to resume 14 sessions: prose 2.5 bytes per token, tool input 2.1, tool
results 2.2, thinking signatures 3.6, prune markers 1.5, plus 12 tokens per content block and 3k
per image. On those requests the estimate read between 7% under and 46% over the count, the middle
within 10%; it reads high on claude-opus-4-6 (a sparser tokenizer) and on sessions whose
signatures the API no longer counts. The thinking bucket counts signature and encrypted bytes as
well as plaintext; on Claude it is mostly signatures. The first prompt is the first recorded prompt size: the system prompt and tools plus
whatever conversation that turn carried (a resumed session starts with its history); with
`--recent`, doctor shows the largest. The peak prompt is the largest recorded. Both are the
provider's numbers unless `cv prune` rewrote them.

JSON keeps its field names: `conversational` and the per-tool/attachment values are the byte
estimates (`thinking` included), `peak_context` is the peak recorded prompt and
`fixed_overhead_est` the first. `cv stats --tokens` sums recorded usage instead; its `reasoning`
is zero on Claude, which records no separate reasoning-token count.

---

## Reshape

The commands that produce a **new** session id from existing ones. The source session is never modified — not by `prune`, not by `port`, not by anything. (`resume` is the odd one out: it launches an existing session rather than making a new one, but it belongs with the "get me back into this" verbs.)

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

The defaults snip payloads over 2 KB and keep the last 25 turns verbatim. `--drop-thinking`
omits older thinking blocks outright, with no marker in their place (a marker would give the
model nothing); each goes to the sidecar under `<record uuid>#think<index>`, where `cv cat` finds
it, and a record left empty is removed with its children relinked to its parent. On one
977k-token `claude-opus-5` session (Claude Code 2.1.299) it took the resumed request from 981k to
817k tokens, 98% to 82% of the 1M window, as the API counted Claude Code's own request. How much
it frees depends on the model: on claude-fable-5-1, claude-opus-5-5 and claude-sonnet-5 sessions
the API no longer counts old signatures, so dropping them freed nothing. `--window` and `--keep` drop whole turns (lossy in the new session; the source keeps everything):
`--window` uses recorded prompt-usage deltas when available and byte estimates otherwise;
`--keep A..B` selects by index. The flag spelled `--range` before 0.11.0 was renamed because
it selects what to **keep**.

By default prune also **revives** an already-maxed session: Claude Code's resume gate reads the
last turn's recorded `usage` as the session's size *before* re-sending anything, so a session at
the wall refuses to resume even after pruning made it fit. Revive rewrites recorded usage above
the estimated post-prune prompt down to that estimate. The estimate starts from the last recorded
prompt, subtracts most of what pruning removed on the branch Claude Code resumes (removed
signatures only for models measured to count them), adds what follows, and is floored by an
outright estimate plus the fixed overhead the recorded prompts imply, then 5% on top. On 28
calibration prunes it read between 1% under and 52% over the request Claude Code then built,
so a session near the window is held back rather than refused; the session above was estimated
at 966k and resumes. `--no-revive` keeps the original records byte for byte. `--declassify` snips conversational
*prose* dense in terms you supply (`--declassify-tokens`, `--declassify-tokens-file`); cv ships no
term list. `--json` puts the report on stdout as one object (the human report stays on stderr),
and under `--dry-run` the paths it would write are `null`, with a `note` saying so.

Claude Code only for now (it operates on the raw JSONL to stay byte-faithful).

### `cv rewind`

Reconstruct an agent **as it was at a past moment** — a message, or the commit it landed — as a new, resumable Claude Code session. The use case is review by the author: when one agent changes code another session wrote, resume the writer *as of the moment it committed that code* and ask it to review the change. `claude --resume` alone can't do that — it loads the session's current tail, after every later compaction — and most writers are sub-agents, whose transcripts can't be resumed at all.

```sh
cv rewind 7b2e19a4 --at d41c0de              # through the tool result that shows that commit being made
cv rewind 7b2e19a4 --at 1240               # through message 1240 (as `cv show --range` counts)
cv rewind 7b2e19a4 --at d41c0de --full       # from the first record, not the last compaction
cv rewind agent-b81e242c50c0b9654            # a sub-agent, extracted as a standalone session
cv rewind 7b2e19a4 --agent b81e2 --at c0ffee1
cv rewind 7b2e19a4 --at d41c0de --dry-run --json

claude --resume <new-id> --fork-session -p "Another agent changed the code you just landed. Review its diff: …"
```

`--at` is a message index or a commit sha (7+ hex digits; an all-digit value is an index). For a
sha the cut is the tool result whose `git commit`/`git push` output names it, and rewind fails
with an explanation if none does. If the cut leaves parallel tool calls open, the copy runs on to
their results (they happened at the same moment, and the API rejects a call with no result).

By default the new session starts at the last compaction boundary before the cut — exactly the
context the agent had, since a resume loads from there anyway — moved back to the boundary's
preserved segment when partial compaction kept recent messages verbatim. On a month-long session
that is the difference between one window (8k lines) and 900 MB; `--full` starts at the first
record. A sub-agent (`--agent`, `agent-<id>`, or its transcript path) comes out standalone: its
records get their own session id and `isSidechain: false`, lose `agentId`, and land in the
parent's project dir. `claude --resume` only finds a session in the project dir of the directory
it is launched from, so with `--out` cv says where to copy it.

The derived session uses the source's raw records `[start, cut]` with every `sessionId` rewritten
and `parentUuid` chains untouched. A window whose models are all `claude-*` is copied as is; any
other model makes the rewind portable (above), which demotes or removes reasoning and repairs
illegal tool IDs. `--thinking native|portable` overrides the choice; the report and `--json` name it.
Next to it goes `<new-id>.rewind.json`, the provenance: source id, path, bytes and sha256 (of the prefix read — a live transcript keeps growing), the cut and start (message index, byte offset, line), the commit evidence, reasoning policy, how many lines of the source come after the cut, when, and which cv. The resume line `cd`s to the directory Claude files the session under (the launch dir), which is not always where the agent was working at the cut — cv prints both.

Claude Code only (it copies raw records). The source is never modified.

### `cv distill`

Distill a long session — typically a sub-agent lane — into a compact context the same agent can resume: the brief and every message *to* the agent verbatim, its own words (text, messages sent, notes written) verbatim, an index of commits/branches/hosts/paths/verdicts/errors, a one-line-per-call ledger, and its last N tool calls as real turns. Deterministic, no model calls; elided outputs go to a sidecar [`cv cat`](#cv-cat) reads. The rules, and why they are what they are, are in [Reshaping a lane](reshaping.md).

```sh
cv distill agent-a5e2da77f3df2384b                      # the markdown pack on stdout
cv distill agent-a5e2da77 --pack lane.md                # …to a file
cv distill agent-a5e2da77 --session --cwd ~/work/repo   # a resumable session: claude -p --resume <id>
cv distill agent-a5e2da77 --agent-of c1e4d723           # a new sub-agent of that root; SendMessage to the printed id
cv distill agent-a5e2da77 --upto 400 --with agent-a04ff45c --json
```

The pack prints on stdout (or to `--pack FILE`); `--session` emits a resumable main-thread session
instead, and `--agent-of <root>` a new sub-agent of that root, which the root resumes with
`SendMessage` to the printed agent id. `--upto N` distills the session as it stood at message N —
a distilled fork.

### `cv adopt`

Rescue the sub-agents of a dead Claude Code session into a live one, so the live session can `SendMessage` them and they resume with their whole transcript. It copies `subagents/agent-<id>.jsonl` (with `sessionId` restamped on every line, nothing else changed) and its `.meta.json` into the live session's `subagents/`.

```sh
cv lanes --recent 20 --unfinished       # find them: unfinished agents of the project's recent sessions
cv adopt a5d8 b81e                      # into the project's newest session other than the one holding them (printed first)
cv adopt a5d8 --into 7b2e19a4 --from c1e4d723
cv adopt a5d8 --dry-run --json
cv adopt a5d8 --model sonnet            # resume it on another model
```

Without `--into`, the adopter is the project's newest session other than the one holding the
agent, printed before acting; without `--from`, every session's `subagents/` is searched and the
newest copy wins. The meta keeps the agent's own `model` unless `--model <id>` replaces it;
`inherit` (what a meta without `model` also means) runs it on the live root's current model.

Claude Code only; the dead session's files are never modified.

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

A span may come from a sub-agent (`agent-<id>`): it joins the new session's main thread, shedding the sidechain markers that would make Claude skip it.

Without `--harness` or `--out`, splice composes in memory and prints a summary; add `--export md|json` to dump the result. With `--harness`/`--out` it materializes the session for a harness.

A verbatim prefix `<id>:..N` forks a lane at message N; `<base>:..N <other>:M..` grafts one
session's continuation onto another's head. `--session` and `--agent-of <root>` emit a resumable
Claude session or a new sub-agent of that root, as for [`distill`](#cv-distill). Threading, open
calls and sidechain identity are the emitter's: the cut points re-link, and a call whose result
falls outside the spans is dropped (see [the dangling-parent trap](reshaping.md#the-dangling-parent-trap)).

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

`--thinking` defaults to `native` when every model the source recorded is `claude-*` and to
`portable` otherwise (see `--thinking` above). Expected reasoning losses still appear in the
fidelity report, including with `--strict`.

Without `--harness` the target is the source harness — a pure rehome. Rehoming rewrites the cwd
baked into the target format (Claude's encoded project-dir name, Grok's percent-encoded path,
Cline/Roo's `<environment_details>` line, …), and copies the project's context files (`CLAUDE.md`,
`CLAUDE.local.md`, `AGENTS.md`, `GEMINI.md`, `MEMORY.md`, `.cursorrules`, `.windsurfrules`) into the
new cwd, never over an existing one; `--no-context` skips them.

Every port re-parses its own output and diffs it against the source IR per message — role and `kind` sequences, block-type counts, thinking (text / signature-only / encrypted), tool names on results, `is_error`, `details`, usage, model, timestamps, ids, plus the session's title, cwd, model, `system_prompt` and `lineage`. Each delta is classified as *expected for this target* (from a per-emitter table of what the format cannot carry) or *unexpected*; `--strict` fails on the latter.

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
cv export da9174f4 --last 100 --format md     # windows work here too
cv export da9174f4 --redact > safe.md         # secrets/PII scrubbed first; counts on stderr
```

```text
✦ redacted 7 item(s): 2 api_key, 1 private_key, 0 jwt, 3 email, 1 blob, 0 assignment
```

For a self-contained HTML page, [`cv share`](#cv-share) is the renderer: it streams and redacts.
`--redact` runs the same scrubbing pass `share` does — API keys, private keys, JWTs, emails, opaque
blobs, `KEY=value` assignments — with the per-class tally on stderr so stdout stays the document;
the thing to reach for before you paste a transcript into an issue. The five
[window flags](#message-windows) are `cv show`'s.

(`show` and `export` are siblings: `show` is for your terminal, `export` is for a file or a pipe.)

### `cv dataset`

Export the corpus as a fine-tuning dataset — JSONL, one session per line. `chatml` (default) emits `{"messages":[…]}`; `sharegpt` emits `{"conversations":[…]}`. Both import directly into Unsloth Studio / TRL / HuggingFace `datasets` with no adapter. Streamed one session at a time, so memory stays flat over a multi-GB corpus.

```sh
cv dataset --out corpus.jsonl                                # whole corpus, chatml
cv dataset --format sharegpt --harness claude                # one harness, ShareGPT shape
cv dataset -q "model:fable" --subagents --out fable.jsonl    # a queried slice + its forest
cv dataset -q "harness:claude" --redact-only private_key     # strip PEM keys, keep the rest
```

`--subagents` also emits each Claude session's sub-agent transcripts: a parent on one model often
spawns sub-agents on another, and most model usage lives in the forest, so a `model:` query
usually wants it. `--redact-only <classes>` scopes the scrub to a comma list (`private_key`,
`api_key`, `jwt`, `email`, `blob`, `assignment`).

### `cv pack`

Compile a context bundle for a new task out of your whole corpus — the one "build context from the corpus" verb, and the command that absorbed the old `recall`.

```sh
cv pack "tantivy chunked indexing"                       # CLAUDE.md-style bundle → stdout
cv pack "fix the parser" --format prompt                 # shaped as a system prompt
cv pack "fix the parser" --format session --harness claude   # a synthetic resumable session
cv pack "migrate the daemon" --limit 3 --out CONTEXT.md
```

It gets its own chapter: **[`cv pack` — the context compiler](pack.md)**.

---

## Fleet & live

### `cv task`

The fleet's durable dispatch objects: open/claim/note/done, and the decisions posed on them. Completion is self-reported and labeled so — cv never claims to have observed what it has not. The verbs are under [`cv task` in the reference](reference.md#cv-task); the inbox page is served by [`cv serve`](serving.md).

`propose`, `reroute`, `pass`, `refute`, `verify`, `debt` and `stats` were removed in 0.15.0 with the land facet, `split` and `watch` with it; each errors with a pointer. This is a big enough topic to get its own chapter — see **[the task substrate](tasks.md)** for the lifecycle, the three laws, and a worked end-to-end example.

### The coordination board (removed)

The coordination board went away in 0.15.0: in 85 days of real swarms, 3,317 of its 3,319 lines
were auto-mirrored task events. The task log is the fleet's record — `cv task note` to say
something, `cv task events --since <id>` to follow it, `cv task claim` to hold work. The old
`board` spelling errors with that pointer and exit 2.

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

This is the CLI face of [`cv watch`](serving.md), which mirrors the same activity into a live feed for your whole fleet (and powers the MCP `await_omen` block-until-match primitive).

### `cv share`

Redact a session and emit one self-contained HTML artifact anyone can open — offline, with nothing installed.

```sh
cv share da9174f4                  # → ./da9174f4….html, redacted
cv share da91 --out incident.html  # pick the filename
cv share da91 --no-redact          # keep secrets (loud stderr warning)
```

The rest is in **[Viewing & sharing](viewing-and-sharing.md#sharing-a-transcript-cv-share)**.

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

`--semantic` also computes embeddings (a ~30 MB model downloads the first time). `--subagents`
also indexes the **sub-agent / workflow forest**, tagged with parent session, workflow run and
agent id, so a `cv search`/`cv touched` hit can point *inside* a workflow agent; it can add
hundreds of MB, so it is opt-in.

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

`ls`, `stats` and `dataset` take `-q`. The language is a boolean conjunction of terms — implicit `AND`, plus `OR`/`|`, `NOT`/`-`, and `( )` grouping:

```sh
cv ls -q "harness:claude model:fable"                 # fable sessions from Claude
cv ls -q "(model:fable OR model:opus) -title:test"    # either model, excluding tests
cv ls -q "msgs>=50 after:2026-01-01 tool:Bash"        # big recent sessions that ran Bash
cv stats -q "touched:src/ir.rs has:errors"            # analytics over a slice
cv ls -q "harness:claude agent:Explore subtool:Bash"  # a forest query
```

Fields, by what they cost to answer:

- **catalog** (free, no parse): `harness`, `cwd`, `title`, `id`, `msgs`, `created`, `updated` (`before:`/`after:`/`since:`/`until:` are sugar).
- **parse** (reads the transcript): `model` (any turn's model), `git` (branch/remote).
- **index** (needs `cv index`): `text` — full-text over content.
- **events**: `tool` (a tool *this session* ran), `touched` (a file it read/edited), `has` (flags: `subagents`, `errors`, `tools`, `images`, `compacted`, `workflows`).
- **forest** (walks the [sub-agent forest / workflows / compaction seams](#cv-workflow) — the priciest): `subtool` (a tool a *sub-agent* ran), `agent` (spawned a sub-agent of this type), `agents` (forest size), `workflow` (ran a matching `Workflow`), `workflows` (run count), `compactions` (boundary count).

Operators: `:` (contains), `=` (exact), and `> >= < <=` for numbers/dates; values can be comma-OR-lists (`a,b`) or ranges (`lo..hi`). A bare word matches title/cwd/id. The evaluator prunes with the catalog-cheap terms **first**, so a forest/parse/index term only ever runs on what survives — always pair one with a `harness:`/`cwd:`/`msgs` term to stay fast.

```sh
cv ls -q "harness:claude agents>=5 subtool:Bash compactions>=1"   # deep, tool-heavy, compacted runs
cv ls -q "workflow:census OR workflows>=3"                        # ran a census workflow, or 3+ runs
```

The `thread:` message-path language and the `~` regex operator were removed in 0.15.0; each
errors with a pointer. Select the session with `tool:`, `text:` and `has:`, then read the turns
with [`cv show`](#cv-show).

### `cv formats`

The format census — cv's own honesty about how much of each harness's on-disk vocabulary it actually understands.

```sh
cv formats                         # what real sessions hold, per harness
cv formats --harness codex         # just one
cv formats --recent 50 --json
```

It parses recent sessions per harness in *format-complete* mode, where every record an adapter doesn't interpret is carried through as a [`carrier`](architecture.md#the-unified-ir) message tagged with its record type. It then reports, per harness, the record/part/column vocabulary actually seen with counts — marking anything the manifest (`formats/<harness>.toml`) does not name as `NEW`. That is drift arriving on disk: a harness started writing something cv has never seen.

The other direction — does each adapter's source still match its manifest — needs a checkout, so it is a test, not a command: `cargo test -p clustervision-core --test formats_manifest`. A type the adapter has a match arm for is `handled` by inference (the arm is the spec), so a manifest lists only what the source cannot say; the test fails on a listed `handled` type that never appears in the adapter, on an entry that merely restates a match arm, and on a stale `ignore_literals` entry. The manifests ship embedded in the binary, so an installed `cv` carries the vocabulary it was built against.

| status | meaning |
| --- | --- |
| `handled` | interpreted per item — inferred from a match arm, or listed when no arm names it (a SQL identifier, a path) |
| `generic` | interpreted by a blanket rule, so no per-item literal exists |
| `carried` | kept verbatim as a carrier under `ParseOptions::complete`, dropped otherwise |
| `ignored` | known and deliberately skipped (transient, UI-only, or a duplicate) |

### `cv recipes`

The agent quickstart: the things agents actually do with cv, each as one command line with the JSON keys it returns.

```sh
cv recipes
```

It covers finding your own session by cwd, reading the last N turns, reading a sub-agent's return, fleet search, getting a workflow lane's prompt, doctoring a session, pruning and resuming a maxed-out one, porting, fetching one tool call's output, and listing what a session touched — each with its `→` output shape. It also restates the id rules and the [window flags](#message-windows) up front, because those are the two things every agent gets wrong first.

`cv --help` ends by pointing at it. If you are an agent reading this manual, run `cv recipes` first and come back here for the detail.
