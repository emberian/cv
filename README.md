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

Coordinator prose summaries play telephone (`0.5/24` blind state recall vs `21.5/24` for deterministic `cv distill` and `24/24` for the full transcript; `6%` median fact carriage from briefs alone across 156 takeovers). Yet leaving a single session unpruned triggers **context inertia**, a **75%** drop in per-turn reasoning tokens (`arXiv:2610.20563`), and **87–100%** compaction laundering when harnesses flatten untrusted tool outputs into summaries (`arXiv:2610.29869`). **Eclosion (`eclo`)** treats transcripts as immutable values: ahead of the ceiling, an agent constructs a verified **successor session** from existing trajectory spans and resumes in first-person motion—sustaining multi-day runs with zero compactions.

- **Rewind beats tail repair—and unlocks keeper reviewers.** Arguing at the end of a polluted trace conditions on every wrong turn and bloated early draft (`arXiv:2505.06120`); mid-trajectory checkpoint forking flips **18–49%** of failed runs where tail retry flips **0%** (`arXiv:2512.06749`). `cv rewind <id> --at <msg|sha>` cuts dead-end suffixes back to a clean prefix—or pairs with `cv blame -L` to fork the exact veteran session *at the commit it wrote a module* so it reviews incoming changes with its mental model intact.
- **Fold observations early; keep prompts, orientation & cache prefixes.** Tool outputs and opaque thinking signatures exceed **80%** of long-session tokens (`arXiv:2508.21433`), yet agents voluntarily call self-pruning tools **0%** of the time (`arXiv:2509.23586`) and ignore bare opaque recall IDs **56.3%** of the time (Meno's paradox, `arXiv:2609.20804`). `cv prune --carry-agents` and `cv distill --session --bricks` keep human prompts and active turns verbatim, replace acted-on outputs with **semantically labeled** `cv cat` receipts inside byte-stable dyadic prefix bricks (preserving prompt-cache hits and original session IDs across repeated folds), rewrite nested `usage` so `/resume` never auto-compacts on arrival, and carry `subagents/*.meta.json` across in-place `/resume` (`cv adopt --join` reconciles mid-switch helper turns).
- **Respect handoff direction & fork cost (`escalate` ≠ `downshift`).** Passing a weak model's raw noisy trace—or an orchestrator's entire 700K history—into a frontier model costs **2.2×**/step and solves less than starting from clean workspace facts (`arXiv:2608.24358`), whereas stripping a frontier model's trajectory when handing off to a fast worker forces **1.6–2.0×** rework. `cv distill --handoff escalate` drops predecessor narrative in favor of consolidated user prompts, verified build/test verdicts, and quarantined `[UNVERIFIED]` notes; `--handoff downshift` preserves the frontier checklist and tail.
- **Loom trajectories & interview successors before cutover.** `cv splice A:0..18 B:42..60 codex:C:6.. --session` weaves spans across sessions and harnesses while relinking parent chains and dropping half-open tool calls. Passing `--agent-of <root>` (or probing with `claude -p --resume <new_id> --fork-session`, keeping tool schemas identical so the prompt cache hits) plants a trajectory into `<root>/subagents/`—addressable via `SendMessage` before cutover, or compiled from corpus history via `cv pack "<topic>" --format session` ([full empirical notes & pitfalls](manual/src/reshaping.md#why-rewinding-and-folding-beat-telephone)).

```sh
# Fold at ~55% window, probe non-destructively, then /resume <next> in place
next=$(cv prune "$SID" --plan fold.json --carry-agents --json | jq -r .new_id)
claude -p --resume "$next" --fork-session "state open invariants and next step"
cv adopt --join --from "$SID" --into "$next"     # reconcile any helper turns written mid-switch

# Keeper-reviewer rewind, direction-aware escalation, and cross-harness looming
cv blame crates/cv-core/src/emit.rs -L 40,90     # -> commit 6edc8472 in session da9174f4
cv rewind da9174f4 --at 6edc8472                 # fork author session as of that commit
cv distill agent-a5e2da77 --handoff escalate --agent-of "$ROOT_SID"
cv splice da9174f4:0..18 codex:019e75e0:6.. --session --carry-agents
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
