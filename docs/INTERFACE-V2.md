# Interface v2 (cv 0.11.0) — the clean break

Decided with ember, 2026-09-19: **clean break** (no aliases for old names), **snake_case
everywhere**, **full IR restructure**. This document is the contract every change builds against.
When code and this document disagree, fix the code; when the document turns out to be wrong about a
harness, fix the document in the same commit as the code. §1 and §6 are generated from the clap
tree and tested; the rest is prose, and prose decays, so each claim below names where it is checked.

Principle: **one name, one meaning, no grammar to memorize.** A verb names one act; a flag means
the same thing on every command that has it; a JSON key is spelled one way; a message has a
`kind`, a block has a `type`; harness-specific data lives under the harness's own key.

---

## The shape

*From the 2026-10 parsimony review, which measured cv at 110K product lines growing 20K a month.
It is the direction every change steers by: a change that moves away from it needs a reason
written next to it. The reviewed 0.17 tree is 102.4K product lines (`python3 -I tools/check-size.py`,
which reports every crate's and module's drift since the last release tag). Since 0.18 size drift
informs design review, with no line targets or percentage ratchets.*

**Three product libraries, one binary, one API, one UI, 37 commands, two reshape primitives.**
The workspace also contains the CLI crate and `cv-testkit`.

- `cv-core`: IR, adapters, emit, the read API that every door maps onto, tasks, lanes, watch.
  `cv-search` stays the one satellite only because tantivy does not belong inside the dependency
  fence. `cv-web` is the wasm cdylib.
- `cv`: `cv <verb>`, `cv mcp`, `cv serve`, `cv watch`. The former `cvd` and `cv-mcp` binaries are
  these subcommands. The MCP roster is generated from the in-process clap tree, so it cannot skew
  from the CLI (it once shelled out to whatever `cv` was on PATH, and found a stale one).
- **An adapter is `roots + discover + record-to-Message stream`, plus its emitter.** Optional
  checkpoint/resume methods default to declining; source/dependency methods bound safe reuse
  (`harness::Adapter`). This incremental store glue is part of **G** below. Its size is
  `S + Q + C + G + E`: the store description **S** (60-150 lines, plus about 25 per SQL table
  queried); genuine format quirks **Q** (0-400: Gemini's `$set`/`$rewindTo` replay, Kimi's
  rotation fingerprints, Codex's held `token_count`, OpenClaw's tree selection, Devin's forest);
  the codec **C**, 0 when the adapter rides a shared wire codec (`harness/wire/{anthropic,openai,genai}`)
  and 480-1,000 for a format with its own record grammar (Codex, Claude, Kimi Code, OpenClaw,
  Hermes, OpenCode 1.x + 2.x); glue to the shared drivers **G** (at most 40); and the emitter **E**
  (150-450: the shared encoder, the format's fields, a `JsonlStage` or SQLite writer). Roo (132
  lines over Cline) is the model store description; Grok (1,068) the model JSONL adapter.
- **Two reshape primitives, each spelled once.** Record-level and lossless for Claude:
  `derive(src, Select, Rewrite)`, where prune, rewind and adopt are 30 to 80-line selectors.
  IR-level for any harness in: `select messages, then emit(Claude)`, where the emitter itself
  guarantees linear parents, no orphan `tool_use`, correct sidechain identity and the sidecar, so
  fork, distill, splice and `pack --format session` are 20 to 50-line selectors.
- **A task is an append-only log of about eleven event kinds** folded by a pure reducer under one
  CAS. A decision is a task with a `posed`; one event poses, one answers, provisional-then-confirm
  is the one override. Completion is self-reported and labeled so. One wake primitive (`Watcher`
  with a cursor), one notify channel (`task events --since`), no board.
- **One hand-written guide**, one generated command reference from `cv schema --commands --json`,
  one contract with a test that fails when its command table disagrees with the clap tree, a
  CHANGELOG whose every heading is a tag, tests where the rule lives (reducer rules in the reducer,
  renders beside the command, one `cv-testkit` World, one CLI smoke file).

A session mid-work asks: is this a `--kind`, a `--sort`, a selector, a route on the one API, or
a reducer event? It is never a new top-level command, binary, server, renderer, scanner or
harness.

**Where the tree still differs from the shape (2026-10-16, c7aa0c22).** Product lines as
`check-size.py` counts them. Adapters: claude 4,095 (+ claude_workflow 934), openclaw 3,509,
codex 3,032, opencode 3,014, hermes 2,386, gemini 2,291, cursor 1,761, qwen 1,755, kimi_code
1,613, goose 1,607, kimi 1,446, devin 1,395, cline 1,285 (+ cline_sdk 680), lmstudio 1,106, grok
1,068, export 895, zed 867, opensession 439, harmony 253, roo 132; the shared store layer is 4,041
and the wire codecs 1,313. Every family reads through one record codec over one byte source
(`docs/design/CODEC.md`), but discovery still has its own grammar in Codex, Claude, Grok, Kimi and
Kimi Code, and six heap-gate scenario paths (OpenClaw window groups, Goose fork inventory, Qwen
`TaskInventory`, Hermes dedup, the export cache) are bespoke SQL or file walks. Anthropic block publication and Qwen/Gemini part fields now share their wire grammar;
store wrappers and output selection remain beside each adapter. `derive` now owns native record
selection/writing for prune, rewind and adoption. Prune uses
checked views at every size; untouched records keep their bytes except for selected identity
changes. Rewind retains evidence/window selection and adoption retains destination/meta policy.
`splice::Composition` (74 lines) still composes splice outside the emitter. Message window selection lives in
`api::MessageWindow`, shared by JSON, text rendering and pack relevance excerpts.
CLI and HTTP search now share
`cv_search::Searcher::search`; each door renders its own response. The clap tree in
`cv/src/main.rs` is 1,605 lines. The task reducer is 664 lines
plus 224 of model inside a 2,182-line `task` module; `lanes.jsonl` is stored state (hook stops
arrive before transcripts). The manual is sixteen pages; the CLI smoke file is 3,847 lines.

---

## 1. Commands

Grouped help (`cv --help` shows every current command; removed names are hidden error stubs).
**Read** commands are read-only over existing sessions; **Reshape** commands produce a NEW
session from existing ones and never touch the source (`adopt` copies an agent under its existing
id into another parent; `resume` prints a launch hint or launches an existing session); **Export**
commands produce something that is not a session; **Fleet & live** is multi-agent coordination,
live views and the servers; **System** is cv's own state and reference.

The two tables below are generated from the clap tree, and a unit test
(`cmd::schema::tests::contract_tables_are_the_clap_tree`) fails when they disagree with it;
`CV_REGEN_CONTRACT=1 cargo test -p clustervision --bin cv contract_tables` rewrites them.

<!-- begin: cv schema --commands -->
| group | commands |
|---|---|
| **Read** | `ls` `show` `cat` `search` `events` `touched` `tools` `workflow` `lanes` `stats` `blame` `doctor` |
| **Reshape** | `prune` `rewind` `distill` `adopt` `retire` `splice` `port` `resume` |
| **Export** | `export` `dataset` `pack` |
| **Fleet & live** | `task` `watch` `archive` `serve` `share` |
| **System** | `index` `config` `schema` `formats` `recipes` `hooks` `hook` |

| command | subcommands |
|---|---|
| `task` | `open` `list` `show` `claim` `release` `note` `done` `status` `abandon` `supersede` `tag` `block` `decide` `resolve` `events` `sweep` `inbox` |
| `archive` | `sync` `ls` `path` |
| `hooks` | `install` `remove` `show` |
<!-- end: cv schema --commands -->

A removed or renamed name (`convert`, `query`, `recall`, `board`, `prune --retrieve`, `--to`, …) is
GONE: it errors with a pointer at its replacement and exits 2. The old → new table is the manual's
"Renamed in 0.11.0" (`manual/src/cli.md`); `cv help`'s agent footer became the `recipes` command.

## 2. Flags — the same word means the same thing everywhere

**Windows** (on `show`, `export` and `/api/session/{h}/{id}/messages`; all 0-based, end-exclusive): `--first N`,
`--last N`, `--range A..B` (`A..` to the end, `..B` from the start), `--around N|ID [--context K]`
(default 5; `ID` is a message's own id, such as `[m:ID]`, or `id:<raw>` for numeric message IDs), `--max-bytes N` (a message-boundary budget, with the first selected message always admitted; print
`… continue with --range <next>..` when more remain). Only one of `--first/--last/--range/--around` may be given. `prune --keep A..B`
and `splice <id>:A..B` use the same `A..B` grammar.

**Everywhere else:** `--harness <h>` (filter or target, never "--to"), `--cwd <dir>`, `--out <dir>`
(write somewhere other than the real store), `--json` (machine output where supported; never shortened), `--wide` (a table prints each cell
whole instead of ending a cut one in `…`), `--limit N`, `--fresh` (bypass the catalog), `--all` (include what is
hidden by default: finished lanes, archived sessions, terminal tasks).

**Restored skills:** `prune`, `rewind`, and Claude-target `port` accept `--skills keep|drop|drop-stale`,
parallel to `--thinking`. The default `keep` preserves existing transcript and report bytes.

**Carried sub-agents:** `prune`, `rewind`, and `splice --session` accept `--carry-agents[=meta|full]`
(bare flag defaults to `full`; `=` required for a value so a positional selector is never consumed)
to copy a Claude session's `subagents/` spawn metadata and restamped transcripts under the new
session id (joining any transcript the destination already started); `adopt --join --from <old>
--into <new>` runs the same join after an in-place `/resume`.

**Global:** `--frozen` (`CV_FROZEN=1`) leaves cv stores untouched; commands whose purpose is to
write stores or sessions error with exit 2.

**Session selectors:** accept a unique id prefix or `harness:id`. An ambiguous prefix lists the
candidates as `harness:full-id` lines and exits 2.

**Blocking is opt-in and announced.** No command waits silently. When stdout is not a terminal, a
command that would wait (`workflow --follow`) prints its current state once, exits 0, and
says on stderr how to follow: `--follow` keeps following, `--for <duration>` (`30s`, `5m`, `2h`)
bounds it and implies it; unbounded following needs `--follow` spelled out. Following announces
what it waits for within a second, before the first discovery, and ends with Ctrl-C or `--for`;
on a terminal the interactive default stays and announces the same way. The daemons (`serve`,
`watch`) keep running and print their ready line (address, home, what they do) to stderr
within a second. A reader of protocol input on stdin says so when stdin is a terminal: `hook` exits
2 with usage, and `task … -` announces what it is reading. `crates/cv/tests/announce.rs` runs
every command of the clap tree on a pipe under this contract.

## 3. JSON — snake_case, one shape per noun

- cv-defined JSON fields use **snake_case**. Opaque `principal`, harness extras, tool arguments
  and native carriers preserve their own keys. Timestamps are RFC 3339: session rows use
  `DateTime::to_rfc3339()` (`+00:00`); serialized IR sessions/messages use chrono serde (`Z`).
- A **session row** (from `ls`, `search`, `/api/sessions`, …) always has the same keys:
  `id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes`
  (`message_count` is `null` for a session no listing, full read or index has counted yet; `0` is a
  session with no turns)
  (+ `display_title`, `git` under `--enrich`; + `score`, `snippet`, `agent_id`, `parent_id`,
  `workflow` from `search`). `cv schema --json` publishes this list as `session_row`, with
  `subagent_row` (a forest member: the session row + its sidecar facts), `task_row`, `session`,
  `message`, `block`, `prompt_row`, `context_row`, `file_touch`, `overview`, `lane` and `query`; the manual's architecture
  chapter is the prose.
- A **message** is the IR `Message`. A **block** is the IR `Block`, tagged by `type`.
- A **context row** (`show --kind context --json`, `GET /api/session/{harness}/{id}/context`)
  is `{ index, timestamp, kind, model, usage, usage_basis, prompt_tokens, context_window,
  pre_compaction_tokens, trigger }`. Both interfaces return an array in transcript order;
  `kind` is `usage`, `compaction_boundary`, or `context_limit`, and `index` is the original
  message index. Unknown or inapplicable row values are `null`; missing components inside the
  normalized IR `usage` object are omitted. An empty selection or no usable records gives `[]`.
  Prompt counts sum recorded uncached input, cache reads and cache writes; repeated cumulative
  notices and split records for the same provider response contribute once. Limits are shown
  only on records that explicitly carry them. `usage_basis: estimated` marks saved
  `prune --revive` estimates; `recorded` means saved counts, not independently measured context.
  This projection computes no new estimates or remaining-context guesses and stores no history.
  `--first/--last/--range` and API `first/last/start-end` select projected rows; `--around` / API
  `around` anchors to an original message index or ID, with `context` rows on each side (default
  five). API `[start,end)` is half-open and mixed selector families are rejected.
- A **file touch** (`show --kind files --json`, `GET /api/session/{harness}/{id}/files`) is
  `{ msg_idx, timestamp, op, path, tool, tool_use_id, succeeded, lines_changed, verification }`,
  in transcript order and selected like context rows. `verification.kind` is `verified_git`
  (`commit`, `blob`), `verified_disk` (`blake3`), `reconstructed` or `unverified`.

### Session overview

`show --kind overview --json` and `/api/session/{harness}/{id}/overview` return one object:
`session, requests, workers, files, commands, repair_episodes, omitted, warnings`, with optional `read_coverage`.
`session` keeps the existing session-row shape. Requests, files and commands carry zero-based
source `msg_idx` and nullable RFC 3339 `ts`; those indices open the full surrounding exchange.
Requests contain `text, truncated`; commands contain `tool, command, truncated`, with a null
command when the tool's dynamic input cannot be reconstructed statically. File entries contain
`kind, path, present`: `present` describes current local metadata, not the file's state when the
agent read it, and is null when unavailable or unchecked. No file bodies are read for this view.

`repair_episodes` counts a failed validation followed by a successful file edit and a successful
rerun of the same command and working directory. A commit-creating command ends the interval.
`reverted_checks` counts immediate verbatim replacement reversals separately from `repairs`.
`recent` retains the twelve latest episodes with failure, first edit and pass message indices.
Repeated message IDs keep their first occurrence and all indices refer to the original stream.

The default selects the first and last four human requests without duplication, twelve recent
file-event occurrences, eight recent commands, and twelve newest discoverable workers. Worker
rows contain `session_id, agent_id, path, status, status_source, description, last_text, truncated`;
`truncated` covers either text field. Existing lane status semantics apply: a recorded return
is not evidence of integration. Workers launched outside the adapter's known forest can be absent.
`omitted` gives counts for each list; file counts are occurrences, not unique paths. Native parent
read failures are errors; independent worker-discovery failures appear in `warnings`. Imported
sessions disclose that worker discovery and local file availability were not checked.

Excerpts default to 1,200 UTF-8 bytes; full evidence remains available through ordinary message
and worker views. Selection bounds output, not the source traversal or worker-forest read cost.
`read_coverage` has the same meanings as the parent read's coverage below, including unassessed
when absent. This view reuses the message stream and event classifier without a new identity or
relations store.

## 4. The IR (cv-core `ir.rs`)

`Session { principal, id, harness, cwd, title, created_at, updated_at, model, git, system_prompt,
lineage, messages, source_path, extra }` → `Message { id, parent_id, role, kind, origin, timestamp, model,
content, usage, extra }` → `Block` (`text | thinking | tool_use | tool_result | image | file`,
serde tag `type`). `Role` says who speaks, `MessageKind` what the message is (`prompt`, `reply`,
`tool_result`, `injected_context`, `system_prompt`, `notice`, `compaction_boundary`,
`compaction_summary`, `model_change`, `error`, `subagent_spawn`, `subagent_return`, `branch`,
`carrier`), `Origin` where it came from (`human`, `model`, `harness`, `hook`, `scheduler`,
`subagent`, `import`, `unknown`). `Lineage` holds `forked_from`, `parent`, `spawned_by_tool_use`,
`continued_in`, `continues`, `agent_path`. The four `Usage` token counts are disjoint (Anthropic's
convention); providers that report cached tokens as a subset of input (Codex, Gemini) are
normalized on parse and re-inclusived on emit. The definitions, with their doc comments, are the
source; `manual/src/architecture.md` renders them.

Rules:

- **`kind` is mandatory and never guessed by consumers.** Defaults by role exist only for legacy
  construction (`Message::new(role)`); adapters MUST set the precise kind.
- **`extra` is nested by harness, always.** `m.harness_extra_mut(Harness::Claude)` returns the
  `extra["claude"]` object. A namespace key is a canonical harness name or `cv`
  (`ir::CV_NAMESPACE`: parse diagnostics such as `skipped_lines`, provenance cv stamps on a
  session it synthesized). Exactly two flat keys survive, both message-level and both cv's own
  streaming bookkeeping: `_record` (the verbatim carrier under `ParseOptions::complete`) and
  `cv_byte_offset`. A session's `extra` has no flat keys. `harness::assert_no_flat_keys(&Session)`
  states this once and every adapter's tests call it; `loom` once wrote a flat key and made this
  sentence false, so cv's own session producers are held to it too.
- **Shared concepts leave `extra`.** Compaction → `kind`; system prompt → `Session::system_prompt`;
  parent/fork/continued → `Session::lineage`; tool details → `Block::ToolResult::details`;
  error → `kind: Error` + `extra[h]["error"]`; injected context origin → `origin`.
- **Emitters read kinds, never harness keys.** An emitter may consult `extra[<its own harness>]`
  only when round-tripping into the same harness (format-complete replay), where the non-first-class
  fields are mirrored into the harness bag and the carrier stays at `_record`.
- **A shared module names a harness only inside an exhaustive match.** A harness fact is an
  `Adapter` method with a "no opinion" default (a static one is a field of `Adapter::capabilities`),
  asked of `harness::rootless(h)` when no store is needed; per-harness data is a `match` naming
  every variant (`tests/harness_fence.rs`, over cv-core, the binary and cv-search).
- **Shared core modules key off `kind`.** `compaction.rs` detects the two compaction kinds;
  `doctor.rs` buckets `InjectedContext` as system reminders (kind name from the harness bag when
  present); `render.rs`/`html.rs` label a System turn by its `kind`.
- **`Session::model`** is the session default and `Message::model` the model that turn ran under.
  The "IR diet" rule that nulled a repeated message model is withdrawn (2026-09-19): a record's
  parse must not depend on how much of the file preceded it, and a windowed read would otherwise
  have no way to learn the model. An adapter may omit a model its store never recorded, never one
  it did.

### Read coverage

`Session.extra.cv.read_coverage` records evidence about a read using two extensible string sets:
`sources` (`exported_history`, `native_store`, `member_registry`) and `limitations`
(`metadata_only`, `missing_history`, `unreconciled_history`, `missing_payloads`, `missing_attachments`,
`unsupported_native_fields`, `inferred_tool_identity`). Absence means unassessed; a source set
or an empty limitation set does not certify complete support. Missing data is reported only
when observed, separately from fields cv does not interpret. `unreconciled_history` means newer
native records exist but cannot safely be joined to the selected history, rather than absent
stored bytes. These facts survive lean/facts
reads and OpenSession storage. Unknown identifiers remain readable and round-trip unchanged.

Plain show and Markdown report the sources and consequences in a compact line; a gap found
after streaming the header appears at the end. Doctor reports each assessed session once,
with structured `read_coverage` entries in JSON. Windowed API reads retain the same namespaced
session metadata even when message extras are disabled. Coverage describes evidence consulted
by that read, so a partial window cannot certify the unread tail.

## 5. Fidelity verifier v2 (`emit_verified`)

After emitting, re-parse the output and diff per message: role sequence, kind sequence, block type
counts, thinking (text / signature-only / encrypted), tool names on results, `is_error`, `details`
present, usage, model, timestamps, ids, plus session title, cwd, model, system_prompt, lineage.
Each delta is **expected for this target** (from a per-emitter table of what the format cannot
carry) or **unexpected**. `port --strict` fails on any unexpected delta; the default prints them
under `⚠ lost`.

## 6. MCP (removed in 0.19)

`cv mcp` was removed in 0.19; running agents invoke `cv` commands directly (`--json`) or read via hooks.

## 7. Format census and manifests (`cv formats`)

Under `ParseOptions::complete` every adapter carries unknown records as `Carrier` messages with
`extra[h]["record_type"]`. `cv formats` parses recent sessions per harness in complete mode and
reports the record/part/column vocabulary seen with counts, marking anything absent from the
manifest as **new**. A manifest `formats/<harness>.toml` carries `upstream = { repo, commit,
date }`, `store = [...]` and `[types]` with what the source cannot say (`generic | carried |
ignored`, and `handled` only for an item no match arm names; an item the adapter matches on is
`handled` by inference). `cargo test -p clustervision-core --test formats_manifest` holds the
manifest to the source; `tools/harness-drift.sh` greps the `~/pug` checkouts against the manifests.

## 8. Where this is going (the attractor)

*Named 2026-09-20 in a parsimony pass. Not a plan; a direction.* cv keeps discovering one idea and
naming it anew: **two representations of the same thing disagree, and something must judge whether
that is expected.** Six checkers do it today (`formats_manifest`, `cv formats`, `emit_verified`,
`web/selftest.html`, `assert_no_flat_keys`, `harness-drift.sh`), each over a different pair, each
reporting in its own shape. The end-state is one `Divergence { what, a, b, expected, where }` and
one renderer, so adding a surface means declaring a pair; `formats::Finding` and `emit::Delta` are
already that type with two spellings.

## 9. Design history

The documents that argued their way to this contract live in git, not in `docs/`:

- `git show 5abb404:docs/REARCH.md` — streaming parse, lazy spans, the seek store, the withdrawn "IR diet".
- `git show ffcbefb:docs/PORTABILITY-REVIEW.md` — the source-backed harness audit that became `port --strict` / `emit_verified`.
- `git show c04695b:docs/DESIGN-GAP-AUDIT.md` — the task-substrate gap audit (G1–G11); what landed is in `manual/src/tasks.md`.
- `git show ffc3ef0:docs/design/DISTILL.md` — the distill/fork measurements on three real lanes; the rules are in `manual/src/reshaping.md`.
- The 0.11.0 work-package roster and the IR as a Rust listing are in this file's history: `git log -p -- docs/INTERFACE-V2.md`.
