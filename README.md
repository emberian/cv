# clustervision (`cv`)

Twenty-three coding-agent harnesses store sessions in incompatible local files and SQLite databases. **`cv` reads all twenty-three in place into one typed stream (`Session → Message → Block`), indexes them once, reshapes and ports them with pre-publication verification, and replicates byte-delta custody across machines with explicit completeness watermarks.**

No daemon is required for local use, no import step duplicates your stores, and `--frozen` (`CV_FROZEN=1`) guarantees zero writes—including zero SQLite `-shm` or `-wal` mutations.

<p align="center">
  <img src="manual/src/screenshot-timeline.png" alt="clustervision activity heatmap, harness breakdown, and session feed" width="840"><br>
  <sub>Local multi-harness session store rendered by the desktop and web UI.</sub>
</p>

## Guarantees

- **Bounded streaming reads.** Parsers operate over positioned `pread` windows and lazy `JsonView` byte spans (`O(largest active record)` heap, immune to `SIGBUS` when a live agent truncates a file). Uncheckpointed SQLite WAL stores open via kernel-leased snapshots.
- **Verified continuation.** Reshaping never mutates the source session. Record-level surgery (`prune`, `rewind`, `adopt`) preserves untouched native bytes verbatim; cross-harness emission (`port`, `distill`, `splice`, `pack --format session`) stages output in isolation and checks $\operatorname{parse}_B(\operatorname{emit}_B(S)) \equiv_\rho \pi_B(S)$ before publishing (`--strict` rejects any lossy projection).
- **Reversible compression.** `prune` and `distill` move elided tool outputs and turns into `.flat.jsonl` sidecars addressed by `cv cat <session> <id>`, keeping human prompts verbatim and rewriting stale context-window usage so wedged sessions reopen.
- **Semiauthoritative fleet queries.** Custody packs (`2 MiB` Zstd-Seekable frames, BLAKE3-verified on read without staging) replicate only appended byte or row extents. Multi-place queries (`--place`, `--as-of`, `--max-staleness`) report per-origin frontiers (`Complete | IndexedOnly | Partial`): a sealed sandbox is complete; a silent host is unknown, never empty.
- **Coordination without theater.** Tasks and human decisions (`cv task`, `cv serve --open`) fold an append-only `flock`-CAS log. Agent completions carry tool receipts and exit codes while prose stays labeled `self-reported`; `FileAt`, `ChoiceIs`, and `SessionAt` predicates mark claims `Stale` automatically when files or decisions change.

## Usage

```sh
# Recall & code provenance
cv index                                         # incremental BM25 + event index (--threads / $CV_THREADS)
cv search "wal_checkpoint"                       # BM25 + live unindexed tails; fold chains collapse to tip
cv search --exact "MmapOptions::new()"           # homomorphic trigram candidates + streaming byte verify
cv touched crates/cv-core/src/ir.rs              # every session and sub-agent that edited this path
cv blame crates/cv-core/src/ir.rs -L 40,90       # commit -> session -> turn, across file splits and moves

# Cross-harness portability & task briefs
cv port da9174f4 --harness devin                 # verified emit -> devin -r <id> (or codex, antigravity, ...)
cv pack "fix WAL snapshot contention"            # credential-redacted brief with bounded command history
cv adopt a5d8fd7aa0b73f0e7 --into 945e6fa2       # re-parent an orphaned sub-agent into a live session

# Multi-place collection & live swarm instruments
cv watch --to https://cv-hub.internal:7777       # workstation: stream scrubbed delta extents to a hub
cv archive sync --spool /mnt/cv-spool --seal     # credential-free sandbox: flush & seal on exit
cv archive sync --pull devbox.internal           # agentless SSH: pull deltas under --frozen
cv hooks install claude                          # <1ms SessionStart verb hint + live edit-overlap guard
cv lanes --unfinished                            # attention order: NeedsYou > Working > Stale > Idle > Done
cv doctor <id>                                   # spin episodes, post-compaction churn, context overhead
```

## Successor sessions & trajectory looming

Auto-compaction replaces lived reasoning with a lossy summary; prose handoff briefs play telephone. **Eclosion (`eclo`)** treats transcripts as immutable values: before hitting the context ceiling, an agent or steward constructs a verified **successor session** from existing trajectory spans and resumes inside it—sustaining multi-day work with zero harness compactions.

- **Rolling-window fold.** `cv prune --carry-agents` (or caller-authored `--plan plan.json`, or `cv distill --session --bricks`) keeps human prompts and the active tail verbatim, moves acted-on tool payloads to `.flat.jsonl` behind `cv cat` pointers (stable across repeated folds), rewrites nested token `usage` so `/resume` does not trigger immediate auto-compaction, and restamps `subagents/*.meta.json` so helper models survive. Resuming `/resume <new_id>` **in place** inside the live pane keeps background shells and sub-agents attached; `cv adopt --join --from <old> --into <new>` reconciles helper turns written mid-cutover.
- **Bad-path recut.** When an exploratory suffix hits a dead end, `cv rewind <id> --at <msg|sha>` or `cv splice <id>:..N --session` cuts back to the last clean prefix so subsequent turns condition on an unpolluted trajectory while the shed branch remains addressable via `cv show` and `cv cat`.
- **Trajectory looming & transcript-seeded sub-agents.** `cv splice A:0..18 B:42..60 codex:C:6.. --session` weaves spans across sessions and harnesses into one history, relinking parent chains and dropping half-open tool calls. Replacing `--session` with `--agent-of <root>` plants a distilled or spliced trajectory directly into `<root>/subagents/`, immediately addressable via `SendMessage` without serializing its state into a prompt brief.
- **Pre-cutover probe & corpus experts.** Candidate successors can be interviewed non-destructively via `--agent-of <root>` or `claude -p --resume <new_id> --fork-session` before switching a live seat onto them; `cv pack "<topic>" --format session --harness claude` compiles a standing domain-expert session from prior corpus spans ([`manual/src/reshaping.md`](manual/src/reshaping.md)).

```sh
# Fold ahead of auto-compact, probe non-destructively, then /resume <next> in place
next=$(cv prune "$SID" --plan fold.json --carry-agents --json | jq -r .new_id)
claude -p --resume "$next" --fork-session "state open invariants and next step"
cv adopt --join --from "$SID" --into "$next"     # reconcile any helper turns written mid-switch

# Loom cross-session spans or plant a trajectory-seeded sub-agent into a live root
cv rewind da9174f4 --at 6edc8472                 # session state right as commit 6edc8472 landed
cv splice da9174f4:0..18 codex:019e75e0:6.. --session --carry-agents
cv distill agent-a5e2da77 --handoff escalate --agent-of "$ROOT_SID"
cv pack "WAL snapshot locking" --format session --harness claude
```

## Harnesses (23 read, 14 `port` targets)

| `Read` + `Port` (14) | On-disk store | `Read`-only (9) | On-disk store |
|---|---|---|---|
| `claude` *(CLI + Desktop Code)* | JSONL + sub-agent forest | `pi` *(native `prune`/`rewind`)* | JSONL entry tree |
| `codex` | JSONL + SQLite | `cursor` *(IDE + CLI)* | SQLite KV (`state.vscdb`) |
| `antigravity` *(`agy` + IDE)* | SQLite protobuf + loopback RPC | `zed` | SQLite + zstd blobs |
| `devin` | SQLite (`sessions.db`, v17) | `goose` | SQLite (`sessions.db`) |
| `hermes` | SQLite (`state.db`) | `kimi-code` | JSONL (`wire.jsonl`) |
| `opencode` *(1.x + 2.x)* | SQLite + JSON tree | `kimi` *(frozen 2026-06)* | JSONL |
| `openclaw` | JSONL + SQLite | `opensession` *(`export --format json`)* | Portable IR JSON |
| `gemini` · `qwen` · `grok` | JSON (`$set`/`$rewindTo`) / JSONL | `claude-export` · `chatgpt-export` | Account `.zip` / dir |
| `cline` · `roo` · `continue` · `lmstudio` | JSON task dirs / session files | | |

Wire grammars for all 23 formats are documented in [`docs/FORMATS.md`](docs/FORMATS.md); the portable IR schema is [`docs/OPENSESSION.md`](docs/OPENSESSION.md).

## Architecture

One interface contract ([`docs/INTERFACE-V2.md`](docs/INTERFACE-V2.md), checked against the `clap` tree in CI) governs 39 commands across five groups (`Read`, `Reshape`, `Export`, `Fleet & live`, `System`) and four surfaces: CLI (`cv`), HTTP + SSE (`cv serve`), client-side WASM (`crates/cv-web`), and Tauri desktop (`app/src-tauri`), plus CSP-locked offline HTML shares (`cv share`).

- **`crates/cv-core`** — canonical IR, 23 streaming adapters, 14 staged emitters + $\equiv_\rho$ equivalence verifier, `secrets@2` redaction, distillation, lineage, blame, doctor, tasks, lanes, custody, and the single read API.
- **`crates/cv-search`** — Tantivy BM25 + live-suffix search and opt-in `CVTRG` exact trigram index.
- **`crates/cv`** — the single binary (`cv <verb>`, `cv serve`, `cv watch`, `cv archive`, `cv task`, `cv hooks`).
- **`crates/cv-testkit`** — real-corpus golden regression harness, mutation/truncation fuzz checks, and live Rust heap gates ([`docs/design/PERF.md`](docs/design/PERF.md)).

## Install

Prebuilt binaries for macOS, Linux, and Windows (`arm64` · `x64` · `x86`) are on **[Releases](https://github.com/emberian/cv/releases/latest)**:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

From source (`rust-toolchain.toml` pins `1.99.0`):

```sh
cargo build --release && make install            # -> ~/.cargo/bin/cv
```

Documentation: **[User Manual](https://emberian.github.io/clustervision/manual/)** · **[Interface Contract](docs/INTERFACE-V2.md)** · **[Datapaths](docs/design/DATAPATHS.md)** · **[Adding a Harness](manual/src/adding-a-harness.md)** · License: MIT OR Apache-2.0.
