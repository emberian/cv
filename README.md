<div align="center">

# 🔮 clustervision

### *Vibecoding is a clusterfuck. When it gets hazy, you need clustervision.*

### Your AI coding sessions are gold. Stop letting them rot in scattered folders.

**Find, search, port, prune, and even *resurrect* every AI coding-agent session you've ever run — across 22 harnesses, through one unified format.**

`claude` · `codex` · `grok` · `opencode` · `gemini` · `antigravity` · `hermes` · `openclaw` · `cursor` · …

*search full-text or exact code · read behind every compaction · port across harnesses · resurrect a maxed-out session · pack a new task from your whole history*

No database server, no import step, no telemetry: clustervision reads your **real local session storage**. `cv index` builds full-text search, `cv index --exact` adds trigram substring acceleration.

<br>

<img src="manual/src/screenshot-timeline.png" alt="clustervision timeline — a GitHub-style activity heatmap of every agent session across every harness, with a constellation feed below" width="840">

<sub>the desktop app reading 2,245 real local sessions across 8 harnesses — heatmap, harness legend, and a live feed</sub>

</div>

---

## The problem (you've felt this)

You spent three hours teaching an agent your codebase: the dead ends, the decisions, the understanding. Then:

- 🙈 **You can't find it.** It's one of hundreds of `.jsonl` files in a folder named after a path.
- ⛓️ **It's trapped.** Most harnesses resume a session only from the *exact directory* it ran in.
- 🏝️ **It's stranded.** Started in Claude Code, want to continue in Codex? Every tool speaks its own dialect.

**clustervision means you never restart from scratch.**

## What you can actually do with it

```sh
# 🔎 find that session from last week by what it was ABOUT, not where it lived
cv index                                     # build/refresh the search index first
cv search "the flux inference refactor"        # full-text, instant
cv search --exact "getattrlistbulk"            # literal code substring, trigram-verified

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

# 👁️ archive and follow every agent on your machine live, or replicate to a fleet hub
cv watch
```

…and a couple that didn't exist before:

- 🛰️ **A fleet hub & archive**: `cv watch --to` and `cv archive sync --to` replicate scrubbed Zstd-Seekable session packs and events to `cv serve --hub`.
- 🌐 **A zero-install web viewer**: drop a zip of any harness folder into your browser. Nothing uploaded, all WASM.

## 🪐 22 harnesses, one IR

| | Harness | Parse | Port *to* | | | Harness | Parse | Port *to* |
|---|---|:--:|:--:|---|---|---|:--:|:--:|
| ✅ | **Claude Code** | ✅ | ✅ | | ✅ | **Cursor** | ✅ | — |
| ✅ | **Codex CLI** | ✅ | ✅ | | ✅ | **Kimi CLI** | ✅ | —² |
| ✅ | **Grok CLI** | ✅ | ✅ | | ✅ | **Qwen Code** | ✅ | ✅ |
| ✅ | **OpenCode** | ✅ | ✅ | | ✅ | **LM Studio** | ✅ | ✅ |
| ✅ | **Gemini / Antigravity** | ✅ | ✅ | | ✅ | **Cline** | ✅ | ✅ |
| ✅ | **Hermes** (Nous) | ✅ | ✅ | | ✅ | **Roo Code** | ✅ | ✅ |
| ✅ | **OpenClaw** | ✅ | ✅ | | ✅ | **Continue** | ✅ | ✅ |
| ✅ | **Zed** (agent panel) | ✅ | — | | ✅ | **Goose** (Block) | ✅ | — |
| ✅ | **Devin CLI** (Cognition) | ✅ | — | | ✅ | **Kimi Code** | ✅ | — |
| ✅ | **OpenSession** (`cv export --format json`) | ✅ | — | | ✅ | **ChatGPT / Claude.ai exports** | ✅ | — |

<sub>The Claude and ChatGPT desktop apps are not harnesses: one keeps transcripts server-side, the other encrypts them at rest; why neither is readable is recorded in [`docs/FORMATS.md`](docs/FORMATS.md). ² Kimi CLI's `~/.kimi` store has been frozen since 2026-06 (kimi-cli migrates it to `~/.kimi-code`), so cv stopped writing there in 0.14.</sub>

**Conversion is N-way** among the 12 emit-capable harnesses, through one IR. Every format is reverse-engineered in [`docs/FORMATS.md`](docs/FORMATS.md). Bringing your own? → [Adding a harness](manual/src/adding-a-harness.md) 💛

## ✨ More than a viewer

- **🧵 Splice & loom** — compose a session from spans of others (`cv splice A:0..12 B:6..`), fork a lane (`cv splice A:..40 --session`), or graft branches (`cv splice A:..20 B:35..`). A [Janus loom](https://generative.ink/posts/loom-interface-to-the-multiverse/) for agent transcripts, across harnesses.
- **🔎 Full-text & exact search** — Tantivy BM25 over sessions plus live unindexed suffixes (`cv search <query>`), and positional trigram substring search (`cv search --exact <literal>`) verified against source text.
- **🧬 Provenance** — the **event catalog** classifies every tool call in every session. `cv touched <file>` lists every session that edited a file; **`cv blame <file>`** ties each commit and hunk to the conversation that wrote it, with a `cv show --range` jump to the edit, and an exact match when the session's tool output shows the commit.
- **🌳 Anatomy of a run** — **`cv workflow <id>`** renders a `Workflow`-tool run: phase tree, agents per phase, their outcomes, tokens and tool calls, and the driving script. **`cv tools <id>`** gives per-agent tool histograms and a timeline over the whole forest. **`cv show <id> --kind compaction_boundary,compaction_summary`** finds every compaction seam; `cv show --pre-compaction` reads back what the continued agent *lost*. `cv dataset --subagents` exports the forest.
- **✂️ Prune** — *custom compaction*: `cv prune <id>` moves bulky **old** tool payloads (file reads, logs, screenshots) into a sidecar behind a `[PRUNED id=…]` marker and writes a **new, resumable** session. Prompts stay verbatim, recent turns intact; `cv cat <new-id> <tool_use_id>` restores any payload. `--drop-thinking` lifts the oldest reasoning too. It also **revives a session stuck at the context wall** by rewriting the stale recorded `usage` Claude Code's resume gate trusts (`--no-revive` opts out). (Adapted from [flatten-mcp](https://github.com/shayaShav/flatten-mcp).)
- **⏪ Rewind** — `cv rewind <id> --at <sha>` rebuilds an agent *as it was when it landed a commit*: from the last compaction before it to the tool result showing the commit, under a new id, so `claude --resume <new-id> --fork-session` wakes up as the author. Works on sub-agents and message indexes; leaves a provenance sidecar. `cv blame` ranks such sessions first (`exact: commit created here`) with the `cv rewind` line to paste.
- **🛟 Adopt** — a Claude Code session crashed with lanes in flight? `cv adopt <agent-id>… [--into <live-session>]` moves each sub-agent into a live session, where `SendMessage` resumes it with its whole transcript. The transcript is copied with `sessionId` restamped (no other byte changes) and `.meta.json` verbatim. `--into` defaults to the project's newest session; `--from` names the dead one; `--dry-run` prints the plan; `--force` overwrites; `--model <ID>` sets the model it resumes on — and for an agent already in the live session (say one that hit a usage limit) `--into <S> --model <ID>` rewrites its meta in place and copies nothing. `cv lanes --recent N --unfinished` lists candidates. **Only the transcript moves:** keep the dead session's directory, which holds `tool-results/` and any prune sidecar. Workflow-run agents are not adoptable.
- **🔒 Redact** — `cv export <id> --redact` scrubs secrets and PII.
- **🎁 Share** — `cv share <id>` writes one self-contained, redacted-by-default HTML file: collapsible thinking and tool folds, opens offline, CSP-pinned so it *can't* upload.
- **📦 Pack** — `cv pack "<task>"` compiles relevant past spans and the files those sessions touched into a CLAUDE.md digest, a system prompt, or a *resumable session* in any harness.
- **🖥️ Desktop app + 🌐 web viewer** — the Tauri app reads **all your local sessions natively**, zero setup:
  - a **Projects** lens: every repo and every agent that touched it, over time;
  - an **activity heatmap** timeline;
  - **Compare**, **Stats**, and a visual **splice composer**;
  - **sub-agent trees**, nested inline and labeled with their task prompts;
  - a **Structure** explorer (`<cv-forest>`): Overview, Forest, Workflows, Tools and Compaction tabs.
- **🔌 Hooks** — `cv hooks install claude` (or `codex`) installs five hooks (plus opt-in `--retired` and `--receipts`): compactions become visible and reversible, a new session starts knowing this directory's history, lanes land in `state.db`, owed decisions surface, and an edit to a file another live session holds asks first (Codex: denies). See [hooks](manual/src/hooks.md).

## 📖 Manual

Every CLI command, the HTTP API, the app, conversion and the harness table: the **[user manual](https://emberian.github.io/clustervision/manual/)** (mdBook, under `manual/`).

## 🛠️ Install

**Prebuilt binaries** for macOS, Linux and Windows (arm64 · x64 · x86) are on **[Releases](https://github.com/emberian/cv/releases/latest)**, or:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

Or from source:

```sh
cargo build --release          # → target/release/cv
make install                   # = cargo install --path crates/cv --force --target-dir target  → ~/.cargo/bin/cv
cv --version                   # cv 0.19.0 (<git sha>[-dirty]): the commit it was built from
```

## 📋 Dispatch work you can trust (`cv task`)

**If you are the human the fleet is waiting on:**

```sh
cv serve --open                                        # a web inbox served by cv itself: decisions as buttons, notes, Done
cv task inbox ember                                    # what needs you: decisions first (default + options), then actions
cv task show <id> --brief                              # one task, one line per note
cv task resolve <id> --accept-default --from ember     # answer a decision (or --choice "<option>"; --note - reads stdin)
```

`export CV_ENDPOINT=ember` implies `--from`. `cv task inbox ember --md` renders the inbox as Markdown;
`--unread` keeps items someone else touched last. Bare `list`/`inbox` show 14 days (or anything
involving you) and count what they hid: `--all` or `--since 90d`.

**Decisions are tasks** with a default that stands if you say nothing, alternatives, and an
optional deadline; `resolve` records *who* chose.

```sh
cv task decide "K-PORTAL: who births the guest cell" --for ember \
    --default "the concierge births it" --option "the receiver allocates it" --by 3d --body -
cv task events --since <last-event-id> --kind resolved,done,noted   # the poll surface: JSON lines, next cursor on stderr
cv task events --assignee ember --not-by $CV_ENDPOINT --since 2h   # what ember did on their tasks, minus your own
```

For agents: **cv never claims to have observed what it has not**. A completion is labeled
`self-reported`; what git suggests (a merged branch, a vanished issue file) is a `sweep` candidate, never a close.

```sh
cv task open "port the auth module" --repo ~/proj --body -   # dispatch (stdin body; --issue paths are absolutized)
cv task claim <id> --from agent:claude-1               # first writer wins (flock CAS)
cv task done <id> --observed "tests green in <sid>"    # self-reported, labeled so
cv task sweep --repo ~/proj                            # what git says is probably done; closes nothing
```

The same over `cv serve` (`/api/tasks`, `/api/tasks/inbox/{who}`,
`/api/inbox`, `/api/task/<id>/{resolve,done,note,discuss,claim,release}`, `/api/events`). State lives in
`~/.clustervision/state.db` (SQLite WAL), refusing events its reducer would reject.

## 📡 Archive your whole fleet (`cv watch`)

```sh
cv archive sync   # snapshot every session into ~/.clustervision/packs
cv watch          # follow live + archive as sessions change → a fleet activity feed
cv serve --web web  # the dashboard, the task inbox and the read API on :7777
```

## 🧬 The OpenSession standard

**[OpenSession](docs/OPENSESSION.md)** is the format seven harnesses *should* have agreed on: small, harness-neutral, and *cwd is metadata, not identity*. clustervision's IR is its reference implementation. Ship a harness? Emit OpenSession and its sessions are portable by construction. 🤝

## 🏗️ Under the hood

One IR (`Session → Message → Block{Text|Thinking|ToolUse|ToolResult|File|Image}`), one `Adapter` per harness (`roots` + `discover` + `stream`, reading through one record codec per wire format) with an emitter beside the twelve that are port targets, plus `splice` / `redact` / `prune` / `watch` / `ingest` modules — and small crates on top: **`cv`** (the one binary: the CLI, `cv serve`, `cv watch`, `cv archive`, and hooks) · **`cv-search`** (tantivy + trigram + columnar events) · **`cv-web`** (WASM) · **`app/`** (Tauri desktop).

```
parse(any harness) → 🔮 unified IR → search · port · prune · splice · pack · archive · view
```

## 🧪 Status

Built largely by swarms of agents. ✨ The edges:

- **22 harnesses parse**; 12 also **emit** (N-way conversion). The rest — Antigravity, Cursor, Goose, Zed, Devin CLI, Kimi CLI (frozen store), Kimi Code, OpenSession, and the ChatGPT/Claude.ai **account data exports** (`chatgpt-export`/`claude-export` — register their location with `cv config --add-export <path>`) — are parse-only for now.
- **Robustness:** the maintainer's fleet (about 7,000 sessions, 95 GB) lists and indexes without a panic; a per-version corpus of real harness output is golden-tested; parsers are checked against malformed input (`crates/cv-core/tests/robustness.rs`).
- The full-text index trades disk for speed; Gemini's protobuf `.pb` is opaque; a few sidecar tool-call streams aren't merged yet.
- Historical format variants are a goal: see [Adding a harness](manual/src/adding-a-harness.md), and **please send your harness logs**.

PRs and weird old transcripts deeply welcome. 💜

<div align="center"><sub>made with 🔮 and an unreasonable amount of enthusiasm · MIT/Apache-2.0</sub></div>
