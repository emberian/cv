<div align="center">

# 🔮 clustervision (`cv`)

### Session, provenance, and fleet infrastructure for AI coding agents.

**Read, search, compress, port, blame, and aggregate every coding-agent session across 23 harnesses, multiple worktrees, sandboxes, and machines — through one streaming IR and one API.**

`claude` · `codex` · `antigravity` · `devin` · `hermes` · `opencode` · `gemini` · `openclaw` · `cursor` · `pi` · `grok` · `qwen` · …

*BM25 + verified code-trigram search · deterministic distillation · N-way verified port · context-wall revival · commit-to-conversation git blame · multi-place delta custody & semiauthoritative fleet queries*

No external database server, no mandatory import step, no telemetry: `cv` reads your **live on-disk JSONL and SQLite WAL stores directly** in bounded memory (with `--frozen` when you require strict zero-touch reads).

<br>

<img src="manual/src/screenshot-timeline.png" alt="clustervision timeline — an activity heatmap of every agent session across every harness, with a constellation feed below" width="840">

<sub>the desktop & web app reading thousands of local sessions across harnesses — activity heatmap, sub-agent forests, splice composer, and live lane attention</sub>

</div>

---

## Why `cv` exists

Once you run more than one coding agent — across Claude Code, Codex, Antigravity, Devin, Hermes, ephemeral containers, and remote build boxes — transcripts stop being chat logs and become **operational state**:

- **Context gets trapped or compacted away.** A three-hour debugging arc hits the context wall, gets summarized into mush, or sits stranded inside one harness's directory-keyed store when you need another model's basin on the problem.
- **Swarms multiply copies and blind spots.** Folded continuations, sub-agent trees, and concurrent worktrees scatter institutional memory across gigabytes of `.jsonl` and `.db` files; agents reach for `rg` over raw transcript directories and blow out their own context windows.
- **Multi-machine fleets lack coherent visibility.** When agents run across laptops, devboxes, and sandboxes, no single machine has the whole picture — yet naive central collectors re-ship full transcripts on every turn or lie by pretending a lagged host had nothing to report.

`cv` turns scattered harness stores into **addressable, portable, verifiable engineering evidence**.

---

## Quick tour

```sh
# 🔎 Search past sessions (BM25 + live unindexed tails, or verified code substrings)
cv index                                         # incremental index (--threads / $CV_THREADS bounded)
cv search "wal_checkpoint"                       # BM25; fold/splice chains collapse to tip by default
cv search --exact "MmapOptions::new()"           # trigram candidate index + streaming source verify

# 🧬 Trace code back to the exact conversation and turn that wrote it
cv touched crates/cv-core/src/ir.rs              # every session + sub-agent that edited this file
cv blame crates/cv-core/src/ir.rs -L 40,90       # commit -> session -> turn (tracks cross-file moves)

# 🧪 Distill or compile context without LLM summarization drift
cv distill da9174f4 --handoff escalate           # 5-7x deterministic compression; reversible via cv cat
cv pack "fix WAL snapshot contention"            # credential-redacted brief from relevant past spans

# 🚀 Port a session across harnesses (verified before publication) or break directory jail
cv port da9174f4 --harness codex                 # -> codex resume 019e75e0-...
cv port da9174f4 --harness devin                 # -> devin -r <id>
cv port da9174f4 --harness antigravity           # cold SQLite trajectory + live loopback delivery
cv port da9174f4 --cwd ~/dev/other-worktree      # re-home cwd + carry project memory

# 🪦 Revive a maxed-out session stuck at the context wall, or rewind to a known-good commit
cv prune da9174f4 --drop-thinking                # sidecars bulky old tool outputs + rewrites stale usage
cv rewind da9174f4 --at 6edc8472                 # rebuilds the agent right as it landed that commit

# 🧵 Compose spans across sessions & harnesses, or rescue live sub-agents from a crashed parent
cv splice da9174f4:0..18 codex:019e75e0:6.. --harness claude
cv adopt a5d8fd7aa0b73f0e7 --into 945e6fa2       # re-parents an orphaned sub-agent transcript intact
```

---

## 🪐 23 harnesses, 14 verified continuation targets, one IR

Every adapter implements one streaming contract (`roots + discover + stream`) over shared wire/store codecs; every emitter stages output in isolation and runs **pre-publication unit-equivalence verification** ($\operatorname{parse}_B(\operatorname{emit}_B(S)) \equiv_\rho \pi_B(S)$) before committing a byte (`--strict` refuses any lossy target projection).

| Harness | Read | `cv port` Target | Store Kind | Harness | Read | `cv port` Target | Store Kind |
|---|:--:|:--:|---|---|---|:--:|:--:|---|
| **Claude Code** *(+ Desktop Code)* | ✅ | ✅ | JSONL + forest | **Qwen Code** | ✅ | ✅ | JSONL |
| **Codex CLI** | ✅ | ✅ | JSONL + SQLite | **LM Studio** | ✅ | ✅ | JSON |
| **Antigravity** (`agy` / IDE) | ✅ | ✅ | SQLite protobuf + JSONL | **Cline** | ✅ | ✅ | JSON task dir |
| **Devin CLI** | ✅ | ✅ | SQLite (`sessions.db`) | **Roo Code** | ✅ | ✅ | JSON task dir |
| **Hermes** (Nous) | ✅ | ✅ | SQLite (`state.db`) | **Continue** | ✅ | ✅ | JSON |
| **OpenCode** (1.x & 2.x) | ✅ | ✅ | SQLite + JSON tree | **Pi** | ✅ | *branch¹* | JSONL tree |
| **OpenClaw** | ✅ | ✅ | JSONL + SQLite | **Cursor** (IDE & CLI) | ✅ | — | SQLite KV (`vscdb`) |
| **Gemini CLI** | ✅ | ✅ | JSON (`$set`/`$rewindTo`) | **Zed** (agent panel) | ✅ | — | SQLite + zstd |
| **Grok CLI** | ✅ | ✅ | JSONL | **Goose** (Block) | ✅ | — | SQLite |
| **Kimi Code** | ✅ | — | JSONL (`wire.jsonl`) | **Kimi CLI** *(legacy)* | ✅ | —² | JSONL |
| **OpenSession** (`export --format json`) | ✅ | *export* | Portable IR JSON | **ChatGPT / Claude.ai** | ✅ | — | Account `.zip`/dir export |

<sub>¹ `Pi` supports native record-level `cv prune` and `cv rewind` branch surgery (`pi::branch`). ² `kimi-cli` froze `~/.kimi` in 2026-06 in favor of `~/.kimi-code`. Cloud-only chat tabs in the Claude/ChatGPT desktop apps are server-held or encrypted at rest (Claude Desktop's local *Code* sessions in `~/.claude/projects` read and port natively); reverse-engineered wire specs for all 23 stores live in [`docs/FORMATS.md`](docs/FORMATS.md). Adding a 24th? See [Adding a harness](manual/src/adding-a-harness.md).</sub>

---

## Core capabilities

### 1. Reshape & context engineering without losing receipts
All reshape verbs are **non-destructive**: they never mutate the source transcript, preserve provenance links (`derived_from`), and place elided payloads in sidecars recoverable on demand with `cv cat <session> <id>`.

- **`cv distill <id>`** — deterministic structural compression (`5.4–6.7×` token reduction with zero LLM hallucination). Preserves verbatim human intent, final state of edited/named state files, resolved vs. open items, and key tool outcomes; supports `--uncompact` (recovering pre-compaction turns) and `--handoff peer|escalate|downshift`.
- **`cv pack "<task>"`** — searches your corpus, condenses duplicate fold/splice chains to their tip, bounds multiline shell heredocs (`--command-bytes`, `--max-commands`), **redacts credentials by default** (`--no-redact` to opt out), and emits a Markdown brief, system prompt, `--review` bundle, or resumable target session (`--format session --harness <h>`).
- **`cv prune <id>`** — surgical custom compaction that replaces heavy historical tool outputs/thinking with `[PRUNED id=…]` pointers and repairs inherited near-full `usage` records (`--revive-estimate` for opaque external folds) so sessions wedged at the context limit resume immediately.
- **`cv splice` · `cv rewind` · `cv adopt` · `cv retire`** — compose multi-session spans (`A:0..12 B:6..`), rewind a session to a message index or git commit SHA (`--carry-agents` brings sub-agents along), re-parent live sub-agents out of a dead orchestrator, or retire noisy transcripts from default views without deleting evidence.

### 2. Multi-place delta collection & semiauthoritative fleet queries
Operators running agents across multiple workstations, SSH build hosts, and ephemeral containers can aggregate into a single `cv` hub (`cv serve --hub`) without quadratic re-uploads or false completeness claims:

```sh
# 1. Persistent workstation / fleet node: push scrubbed Zstd-Seekable delta extents
cv watch --to https://cv-hub.internal:7777

# 2. Ephemeral container / CI sandbox (holds zero hub credentials): flush & seal to a mounted spool
cv archive sync --spool /mnt/cv-spool --seal
cv archive sync --ingest-spool /mnt/cv-spool     # trusted host ingests sealed spool idempotently

# 3. Agentless devbox: pull deltas over SSH without mutating the remote store (--frozen)
cv archive sync --pull devbox.internal
```

- **One index, one API**: collected custody packs (`2 MiB` Zstd-Seekable frames, `~274 µs` verified random seeks via `ExtentChain` with zero disk staging) index directly into the same Tantivy FTS and SQLite `events` catalog as local sessions.
- **Semiauthoritative watermarks**: queries accept `--place`, `--as-of <generation>`, and `--max-staleness <dur>`, reporting per-place contiguous frontiers, lag, `Sealed` sandbox completion, and privacy-filter omissions (`Complete | IndexedOnly | Partial`) rather than silently treating partitioned hosts as empty.

### 3. Cybernetic hooks, live lane attention & flock-CAS tasks
```sh
cv hooks install claude                          # also supports codex; --no-hint, --nudge, --receipts
cv lanes --unfinished                            # 5-state attention: NeedsYou > Working > Stale > Idle > Done
cv doctor <id>                                   # spin episodes, post-compaction churn, context overhead
cv doctor --store                                # index, catalog, custody, and store health
```

- **Hooks that earn their milliseconds**: `SessionStart` injects a `<40`-token zero-I/O verb map (`cv schema --hint`) so agents reach for `cv search` / `cv touched` / `cv pack` instead of grepping raw `.jsonl` folders; opt-in `--nudge` attaches that hint on `PreToolUse` if a tool targets a raw transcript root; `edit` warns or blocks when two live lanes overlap on the same file hunks.
- **Honest coordination (`cv task` & `cv serve --open`)**: an append-only task and human-decision log (`~/.clustervision/tasks/events.jsonl`, folded in `state.db`) under `flock` CAS:
  ```sh
  cv task decide "Storage: keep trigram opt-in?" --for ember \
      --default "keep behind --exact" --option "build in default cv index" --by 2d --body -
  cv task inbox ember                            # decisions owed first, then claimed/open actions
  cv task resolve <id> --accept-default          # or click directly in `cv serve --open`
  ```
  `cv` never launders agent self-reports as ground truth: completions record receipt chains and exit codes while labeling prose claims `self-reported`, and structured `FileAt` / `ChoiceIs` / `SessionAt` predicates automatically flip stale claims when a file or human decision moves.

### 4. Four doors onto one read API
- **CLI (`cv`)** — 39 commands in 5 groups (`Read`, `Reshape`, `Export`, `Fleet & live`, `System`); `--json` everywhere (`snake_case`, one schema per noun via `cv schema --json`); `--frozen` (`CV_FROZEN=1`) guarantees zero store or `-shm` writes.
- **HTTP + SSE server (`cv serve`)** — serves the web UI, task inbox, live SSE stream, and hub control routes from the same binary.
- **Native desktop app (`app/src-tauri`) & zero-upload WASM viewer (`cv-web`)** — inspect local stores natively, or drop a `.zip` / `.cvbundle` into the browser with 100% client-side WASM execution.
- **Offline HTML shares (`cv share <id>`)** — single-file, redacted-by-default, collapsible thinking/tool transcript pinned with a strict CSP that cannot phone home.

---

## 🛠️ Install

**Prebuilt binaries** for macOS, Linux, and Windows (`arm64` · `x64` · `x86`) are published on **[Releases](https://github.com/emberian/cv/releases/latest)**:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

Or build from source (Rust toolchain pinned in `rust-toolchain.toml`):

```sh
cargo build --release                            # -> target/release/cv
make install                                     # -> ~/.cargo/bin/cv
cv --version                                     # cv 0.20.0 (<git sha>)
```

Full documentation, architecture notes, and generated command reference: **[User Manual](https://emberian.github.io/clustervision/manual/)** (`manual/`) · **[Interface Contract (`INTERFACE-V2.md`)](docs/INTERFACE-V2.md)** · **[Wire Formats (`FORMATS.md`)](docs/FORMATS.md)** · **[OpenSession Spec (`OPENSESSION.md`)](docs/OPENSESSION.md)**.

---

## 🏗️ Architecture in one glance

```text
23 on-disk stores (JSONL / JSON / SQLite WAL / Zstd packs / .zip)
  └─► bounded streaming codecs (lazy JsonView, ReadScope WAL leases, RotationalHdd-safe IoAdmission)
        └─► canonical IR: Session → Message → Block { Text | Thinking | ToolUse | ToolResult | File | Image }
              ├─► Read & Provenance: ls · show · cat · search (BM25 + CVTRG) · events · touched · blame · tools · lanes · doctor
              ├─► Reshape & Verify:  distill · pack · prune · rewind · splice · adopt · port (staged ≡_ρ verifier)
              └─► Fleet & Surfaces:  watch · archive (NativeExtents) · task (CAS log) · hooks · serve · share · web (WASM) · Tauri
```

Workspace crates:
- **`crates/cv-core`** — canonical IR, 23 adapters + 14 staged emitters, streaming JSON/SQLite/Zstd store layer, redaction (`secrets@2`), distillation, lineage, blame, doctor, tasks, lanes, custody, and the single read API.
- **`crates/cv-search`** — Tantivy BM25 + live-suffix search and opt-in `CVTRG` exact code-trigram index.
- **`crates/cv`** — the single `cv` binary (CLI, `serve`, `watch`, `archive`, `task`, `hooks`).
- **`crates/cv-web`** & **`app/src-tauri`** — standalone client-side WASM bundle and native Tauri v2 desktop shell.
- **`crates/cv-testkit`** — deterministic multi-harness test world, real-corpus golden regression harness, and live Rust heap probes.

PRs and weird real-world harness transcripts deeply welcome. 💜

<div align="center"><sub>MIT OR Apache-2.0 · built for people and agents working side by side</sub></div>
