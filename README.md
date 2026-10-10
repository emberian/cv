<div align="center">

# 🔮 clustervision

### *Vibecoding is a clusterfuck. When it gets hazy, you need clustervision.*

### Your AI coding sessions are gold. Stop letting them rot in scattered folders.

**Find, search, port, prune, and even *resurrect* every AI coding-agent session you've ever run — across 23 harnesses, through one unified format.**

`claude` · `codex` · `grok` · `opencode` · `gemini` · `hermes` · `openclaw` · `cursor` · …

*search by meaning · read behind every compaction · port across harnesses · resurrect a maxed-out session · pack a new task from your whole history*

No database, no import, no telemetry: clustervision reads your **real local session storage** and turns the scattered pile of `.jsonl` into one searchable, lossless corpus. Full-text **and** semantic search across everything you've ever run; the verbatim detail **behind every compaction boundary**; one-command porting between harnesses; and a custom compaction that pulls a maxed-out session back from the context wall.

<br>

<img src="docs/screenshot-timeline.png" alt="clustervision timeline — a GitHub-style activity heatmap of every agent session across every harness, with a constellation feed below" width="840">

<sub>the desktop app reading 2,245 real local sessions across 8 harnesses — heatmap, harness legend, and a live feed</sub>

</div>

---

## The problem (you've felt this)

You spent three hours teaching an agent your codebase. That context — the dead ends, the decisions, the hard-won understanding — is some of the most valuable output you produce. And then:

- 🙈 **You can't find it.** It's one of hundreds of `.jsonl` files in a folder named after a path. (Yes, you keep the important session IDs in a notes app. Everyone does.)
- ⛓️ **It's trapped.** Most harnesses only resume a session from the *exact directory* it ran in. Move the project, lose the thread.
- 🏝️ **It's stranded.** Started in Claude Code but want to continue in Codex? Tough. Every tool speaks its own dialect and none of them talk.

Restarting from scratch is the most expensive thing you do all day. **clustervision makes sure you never have to.**

## What you can actually do with it

```sh
# 🔎 find that session from last week by what it was ABOUT, not where it lived
cv search "the flux inference refactor"        # full-text, instant
cv search --semantic "formalizing proofs"      # by meaning — no keyword overlap needed

# 🚀 take a Claude session and continue it in Codex. for real.
cv port da9174f4 --harness codex
#   ✦ wrote ~/.codex/sessions/2026/…/rollout-….jsonl
#   ↳ codex resume 019e75e0-…

# 🧳 break a session out of its directory jail (brings CLAUDE.md / MEMORY.md along)
cv port da9174f4 --cwd ~/new/home

# 🪦 a session hit the context wall and won't reopen? prune the bulk and revive it
cv prune da9174f4 --drop-thinking
#   ✦ recorded context 976k → 149k · resume with: claude --resume <new-id>

# 🛟 the orchestrator crashed mid-swarm? move its lanes into your live session, context intact
cv adopt a5d8fd7aa0b73f0e7 --dry-run
#   ↳ resume, from session 945e6fa2: SendMessage {"to": "a5d8fd7aa0b73f0e7", …}

# 👁️ watch every agent on your machine work, live, in one feed
cv scry
```

…and a couple that didn't exist before:

- 🧠 **An MCP server** so a *running* agent can read **other** agents' sessions — "what happened in this project before?", "what's my sibling agent doing right now?", "have I solved this before?" — mid-task, without leaving its harness.
- 🌐 **A zero-install web viewer**: drag a zip of any harness folder into your browser and explore it. Nothing uploaded, all WASM.

## 🪐 23 harnesses, one IR

| | Harness | Parse | Port *to* | | | Harness | Parse | Port *to* |
|---|---|:--:|:--:|---|---|---|:--:|:--:|
| ✅ | **Claude Code** | ✅ | ✅ | | ✅ | **Cursor** | ✅ | — |
| ✅ | **Codex CLI** | ✅ | ✅ | | ✅ | **Kimi CLI** | ✅ | ✅ |
| ✅ | **Grok CLI** | ✅ | ✅ | | ✅ | **Qwen Code** | ✅ | ✅ |
| ✅ | **OpenCode** | ✅ | ✅ | | ✅ | **LM Studio** | ✅ | ✅ |
| ✅ | **Gemini / Antigravity** | ✅ | ✅ | | ✅ | **Cline** | ✅ | ✅ |
| ✅ | **Hermes** (Nous) | ✅ | ✅ | | ✅ | **Roo Code** | ✅ | ✅ |
| ✅ | **OpenClaw** | ✅ | ✅ | | ✅ | **Continue** | ✅ | ✅ |
| 🔒 | **Claude / ChatGPT apps** | detected¹ | — | | ✅ | **Goose** (Block) | ✅ | — |
| ✅ | **Zed** (agent panel) | ✅ | — | | ✅ | **Devin CLI** (Cognition) | ✅ | — |

<sub>¹ The Claude app keeps transcripts server-side; the ChatGPT app keeps them locally but encrypted at rest. We detect the install and document exactly why neither is readable — see [`docs/FORMATS.md`](docs/FORMATS.md).</sub>

**Conversion is N-way** among the 13 emit-capable harnesses: any → any, mediated by one unified IR. Every format reverse-engineered in [`docs/FORMATS.md`](docs/FORMATS.md). Bringing your own? → [`ADDING_HARNESS.md`](ADDING_HARNESS.md) 💛

## ✨ More than a viewer

- **🧵 Splice & loom** — compose a new session from spans of others (`cv splice A:0..12 B:6..`), or *fork-and-graft* a branch and **generate** its continuation with an LLM (`cv loom … --generate`). Works via OpenRouter / Anthropic / **LM Studio (free, local, offline)**. Loom agent transcripts like a [Janus loom](https://generative.ink/posts/loom-interface-to-the-multiverse/), across any harness.
- **🔮 Semantic search** — "have I solved this before?", answered by *meaning* rather than keywords: `cv search --semantic <query>` on the CLI, and the same ranking behind the MCP `search` tool a running agent can call mid-task.
- **🧬 Provenance** — search answers what was *said*; the **event catalog** answers what was *done*. Every tool call across every session is classified and queryable: `cv touched <file>` lists every session that ever edited a file, and **`cv blame <file>`** ties a file's git history back to the agent conversation that wrote it — "why does this code exist?", answered by the actual reasoning that produced it (with a `cv show --range` jump to the moment of the edit, and an exact match when the session's own tool output shows it making the commit).
- **🌳 Anatomy of a run** — a deep agent session isn't a flat transcript, it's a *forest*. **`cv workflow <id>`** renders a `Workflow`-tool run as its real shape — the phase tree, the agents under each phase, their journaled outcomes/tokens/tool-calls, and the driving script. **`cv tools <id>`** is cross-agent tool analytics over the whole orchestrator+sub-agent forest (per-agent histograms, *which agent used what*, a wall-clock timeline). **`cv compaction <id>`** finds every context-compaction seam — trigger, pre-compaction size, and the summary that seeded the next window — and `cv show --pre-compaction` reads back the span the continued agent *lost*. (`cv dataset --subagents` pulls the whole forest into a training set.)
- **✂️ Prune** — `cv prune <id>` is *custom compaction*: instead of abandoning a giant session to the summarizer (which rewrites your history into a lossy paragraph), it snips the bulky **old** tool payloads — large file reads, command logs, screenshots — into a sidecar and leaves a tiny `[PRUNED id=…]` marker, producing a **new, resumable** session (`claude --resume <new-id>`). Your prompts and the exact flow stay verbatim; the most recent turns stay sharp; originals are one `cv cat <new-id> <tool_use_id>` away. Add `--drop-thinking` to also lift the oldest reasoning, and `--revive` to **resurrect a session already stuck at the context wall** — Claude Code's resume gate trusts a stale `usage` number recorded in the file, so a maxed session refuses to reopen even once its real content fits; `--revive` rewrites that number to the honest post-prune size and the gate lets you back in. (Algorithm adapted from the validated [flatten-mcp](https://github.com/shayaShav/flatten-mcp).)
- **⏪ Rewind** — `cv rewind <id> --at <sha>` reconstructs an agent *as it was when it landed a commit*: the source's records up to the tool result that shows the commit being made, from the last compaction before it (the context the agent actually had), under a new session id — so `claude --resume <new-id> --fork-session` wakes up as the author, ready to review whoever changed its code next. Works on sub-agents too (extracted as standalone sessions), cuts by message index as well, and leaves a provenance sidecar. `cv blame` finds these for you: a session whose own tool output shows the commit ranks first as `exact: commit created here`, with the `cv rewind` line to paste.
- **🛟 Adopt** — a Claude Code session crashed with lanes in flight? `cv adopt <agent-id>… [--into <live-session>]` moves each sub-agent into a live session so it can `SendMessage` the agent and the agent resumes with its whole transcript. It copies `subagents/agent-<id>.jsonl` with the top-level `sessionId` restamped on every line (byte surgery: no other byte changes) and copies the `.meta.json` verbatim. `--into` defaults to the project's newest session and is printed before anything is written. `--from` names the dead session; otherwise every session's `subagents/` is searched. `--dry-run` prints the plan, and an agent file already in the target is refused unless `--force`. `cv adopt --list <session>` shows a session's agents with status, model, size and whether a meta exists. `cv adopt --orphans` lists the unfinished agents (running, stopped, killed, failed or stranded, as `cv lanes` reads them) in the project's recent sessions other than the live one. **The limit:** only the transcript moves. The agent's persisted tool outputs stay in the dead session's `tool-results/` (the transcript names them by absolute path), and anything `cv prune` snipped from the dead session stays in its sidecar, reachable with `cv cat <dead-id> <tool_use_id>`. Keep the dead session's directory; `adopt` prints this reminder. Workflow-run agents (`subagents/workflows/…`) are not adoptable this way.
- **🔒 Redact** — `cv redact <id>` scrubs secrets/PII so a transcript is safe to share.
- **🎁 Share** — `cv share <id>` → one self-contained, redacted-by-default HTML artifact: dark crystal-ball theme, collapsible thinking/tool folds, opens offline in any browser, uploads nothing (CSP-pinned so it *can't*).
- **📦 Pack** — `cv pack "<task>"` compiles a context bundle from your whole corpus: relevant past spans + what files those sessions actually touched (event catalog), as a CLAUDE.md digest, a system prompt, or a synthetic *resumable session* in any harness. Never explain your codebase to an agent twice.
- **🖥️ Desktop app + 🌐 web viewer** — the Tauri app reads **all your local sessions natively** (zero setup) and lays the corpus out beautifully:
  - a **Projects** lens — every repo, every agent that touched it, over time;
  - a GitHub-style **activity heatmap** timeline (a constellation of your working days);
  - side-by-side **Compare**, a **Stats** dashboard, and a visual **loom composer** (OpenRouter *or* free local LM Studio generation);
  - **sub-agent trees** — a Claude Task session's children, nested and lazy-loaded inline, each labeled with its task prompt;
  - a **Structure** explorer (`<cv-forest>`) — Overview / Forest / Workflows / Tools / Compaction tabs that turn a run's anatomy into something you can drill through.

  The browser build is zero-install — drop a harness zip, nothing uploaded (all WASM).
- **🔌 Harness integrations** — plug clustervision into the agents' own hooks/MCP/plugins ([`integrations/`](integrations/)): SessionEnd → archive + post to the board, SessionStart → `cv pack` the prior context back in.

## 📖 Manual

Full docs — every CLI command, the MCP tools, the daemon's HTTP API, the app, cross-harness conversion, and the harness table — live in the **[user manual](https://emberian.github.io/clustervision/manual/)** (mdBook, also under `manual/`).

## 🛠️ Install

**Prebuilt binaries** for macOS / Linux / Windows (arm64 · x64 · x86) ship on every release — grab the latest from **[Releases](https://github.com/emberian/cv/releases/latest)** (`cv` · `cv-mcp` · `cvd` · `cv-tui` · `cv-search`), or one-line it:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

Or from source:

```sh
cargo build --release          # → target/release/{cv, cv-mcp, cvd, cv-tui, cv-search}
make install                   # = cargo install --path crates/cv --force --target-dir target  → ~/.cargo/bin/cv
cv --version                   # cv 0.12.0 (<git sha>) — compare against `git rev-parse --short=12 HEAD`
```

`cv --version` prints the commit the binary was built from (`-dirty` when the tree had edits), so
an installed `cv` that lags the checkout is visible at a glance; `make install` reuses the
workspace's `target/release` instead of rebuilding from scratch in a temp dir.

## 🧠 Let agents read each other's minds (MCP)

```sh
claude mcp add clustervision -- /path/to/target/release/cv-mcp
```

The MCP tools are **generated from the CLI** — same names, same flags, same JSON — so they can't drift from what `cv` actually does. Plus the live-coordination primitives that only make sense over MCP: `await_omen` (block until a sibling agent's message matches a regex), `observe_stream` (its non-blocking sibling — poll a junior agent's live message tail on your own cadence), and the board/task tools. The current roster is in the manual's **[MCP chapter](https://emberian.github.io/clustervision/manual/mcp.html)**, or run `cv schema --commands` to see the tree they're generated from.

## 📋 Dispatch work you can trust (`cv task`)

**If you are the human the fleet is waiting on**, these are the commands you run:

```sh
cv task serve --open                                   # a web inbox served by cv itself: decisions as buttons, notes, Done
cv task inbox ember                                    # what needs you: decisions first (default + options), then actions
cv task show <id> --brief                              # one task, one line per note
cv task resolve <id> --accept-default --from ember     # answer a decision (or --choice "<option>"; --note - reads stdin)
```

`export CV_ENDPOINT=ember` once and `--from` is implied. `cv task inbox ember --md` renders the
whole inbox as a Markdown page (bodies, the first line of every note) to paste anywhere;
`--unread` keeps only items someone else touched last. Bare `list`/`inbox` show the last 14 days
(or anything involving you) and say how many older tasks they hid — `--all` / `--since 90d`.

**Decisions are a kind of task.** An orchestrator poses one with a default that stands if you say
nothing, the alternatives, and an optional deadline; resolving records *who* chose, which is why
`resolve` wants an identity. A task that already carries `DECIDE (…): …` notes becomes decisions
with `cv task split <id>`.

```sh
cv task decide "K-PORTAL: who births the guest cell" --for ember \
    --default "the concierge births it" --option "the receiver allocates it" --by 3d --body -
cv task split 01a0f54c                                 # every leading-DECIDE note → its own decision, blocking the task
cv task events --since <last-event-id> --kind resolved,done,noted   # the poll surface: JSON lines, next cursor on stderr
cv task watch --assignee ember --since 2h              # what ember did on their tasks, minus your own events
```

For agents, the substrate is built on one law: **landing state is observed, never attested**. An
agent can claim a task and propose a reviewed branch — but `landed` is only ever written by cv
itself, after running git (`merge-base` ancestry, `git cherry` patch-id equivalence, whole-branch
range patch-id). An agent *saying* "done" moves nothing.

```sh
cv task open "port the auth module" --repo ~/proj --body -   # dispatch (stdin body; --issue paths are absolutized)
cv task claim <id> --from agent:claude-1               # first writer wins (flock CAS)
cv task propose <id> --branch task/auth                # sha + patch-id read FROM git
cv task pass <id> --from agent:codex-1 --session <sid> # cross-family check read from transcripts
cv task verify --all                                   # cv observes what actually landed
cv task debt                                           # reviewed-but-unlanded work, loudly
```

Same substrate over MCP (`task_open` … `task_verify`), cvd HTTP (`/api/tasks`, `/api/tasks/debt`,
`/api/tasks/inbox/{who}`) and `cv task serve` (`/api/inbox`, `/api/task/<id>/{resolve,done,note,
discuss,claim,release,reopen}`, `/api/events` — the same query as `cv task events`). State lives in
a replayed event log (`~/.clustervision/tasks/events.jsonl`) that refuses events its own reducer
would reject; every append is one event, never an edit.

**`task` and `board`:** the board is the fleet's chat; tasks are its commitments. Every task event
posts a one-line notification (`task 01a0f54c: resolved — K-PORTAL: who births…`) to the task's
channel (`tasks` by default), so `cv board read tasks` is the task store as a timeline — the
human-readable twin of `cv task events`, which is the same history as JSON lines with a cursor.
The board never holds task state; the event log does.

## 📡 Archive your whole fleet (`cvd`)

```sh
cvd sync     # snapshot every session into ~/.clustervision
cvd watch    # follow live + archive as sessions change → a fleet activity feed
cvd watch --verify-interval 300   # + run the task git-verifier every 5 min
```

## 🧬 The OpenSession standard

After staring into seven different transcript formats, we wrote down the one they *should* have agreed on: **[OpenSession](docs/OPENSESSION.md)** — a small, honest, harness-neutral interchange format (the key heresy: *cwd is metadata, not identity*). clustervision's IR is its reference implementation. If you ship a harness, emit OpenSession and everyone's sessions become portable by construction. 🤝

## 🏗️ Under the hood

One IR (`Session → Message → Block{Text|Thinking|ToolUse|ToolResult|File|Image}`), one `Adapter` per harness (`discover` + `parse` + `emit`), plus `loom` / `redact` / `prune` / `watch` / `ingest` modules — and small crates on top: **`cv`** (CLI) · **`cv-mcp`** (MCP) · **`cvd`** (daemon + `serve`) · **`cv-search`** (tantivy + `model2vec`) · **`cv-llm`** (LLM digests for `pack`, `--generate` for the loom) · **`cv-web`** (WASM) · **`app/`** (Tauri desktop).

```
parse(any harness) → 🔮 unified IR → search · port · prune · loom · pack · archive · view
```

## 🧪 Status

Built in a wild few sessions, much of it by a swarm of agents working disjoint files. ✨ Honest about the edges:

- **23 harnesses parse**; 13 also **emit** (N-way conversion). The rest — Cursor, Goose, Zed, Devin CLI, the Claude/ChatGPT desktop apps, and the ChatGPT/Claude.ai **account data exports** (`chatgpt-export`/`claude-export` — register their location with `cv config --add-export <path>`) — are parse-only for now.
- **Robustness:** 2000+ real sessions parse with **0 panics**; parsers are fuzz-tested against hostile input.
- The full-text index trades disk for speed; Gemini's protobuf `.pb` is opaque; a few sidecar tool-call streams aren't merged yet.
- Historical format variants are an explicit goal — see [`ADDING_HARNESS.md`](ADDING_HARNESS.md), and **please send your own harness logs** (we can only test what we can see).

PRs and weird old transcripts deeply welcome. 💜

<div align="center"><sub>made with 🔮 and an unreasonable amount of enthusiasm · MIT/Apache-2.0</sub></div>
