# 0.20 backlog: reconciled scope and remaining work

This is the maintained release backlog, not a dump of historical open labels. The read-only
2026-11-14 inventory at `d95e2143` was reconciled at `d7e8506c`, through the second pass below,
and against the subsequent approved feature additions. Sources are the local ledger
(`CV_FROZEN=1 cv task list --repo … --all --json --detail`),
[GitHub issue 33](https://github.com/emberian/cv/issues/33), current code/manuals and focused tests.
The inventory had 24 tasks: 15 open, nine done; seven open tasks were tagged `landed-verify`.
No ledger state was changed by this audit.

Update a row when its implementation and acceptance evidence land. A historical `done`, closed
issue or agent note is a lead, not a current compatibility or performance guarantee. Concrete
release outcomes and their integration gates belong in the sibling release plan; this page
keeps their provenance and separates unresolved work from obsolete requests.

## Current additions

Implemented in cv, with local workspace and focused integration gates completed.

| User action | Implemented contract | Code/test witness |
|---|---|---|
| Select known conversations with `pack --session` | Repeated IDs/harness IDs/paths bypass recall and need no index. Optional task focuses excerpts; no task uses bounded overview. All selected sources participate, distinct paths sharing a native ID remain separate, and read failure precedes output publication | `cmd/pack.rs`; `tests/pack.rs` selected-source, no-task, duplicate-ID and partial-publication tests; [pack manual](../../../manual/src/pack.md) |
| Open `show --kind overview`, the overview API, or reader Overview | Shared initial/recent requests, workers, file events and commands retain source indexes, omission counts and coverage. File presence is current metadata; file bodies are not read. Imported sessions disclose unavailable worker/file checks | `cv-core/src/overview.rs`, `api.rs`, `cv-web/src/lib.rs`; CLI/API parity tests and `web/selftest.html`; [viewing manual](../../../manual/src/viewing-and-sharing.md#session-overview) |
| Inspect `show --kind context` and API windows | Saved per-call usage and compaction rows retain original indexes, deduplicate repeated provider records and label saved estimates. Unknown limits remain null; no live remaining-context estimate | `cv-core/src/context.rs`; `tests/context.rs`; [context contract](../../../manual/src/cli.md#saved-token-usage---kind-context) |
| Follow `lanes --follow [--for …] [--json]` | One parent's initial and changed rows, including final transitions after a selection filter stops matching; no idle duplicates; deadline/interrupt exits | `cmd/session_reports.rs`; `tests/worker_follow.rs`; [lanes manual](../../../manual/src/cli.md#cv-lanes) |
| Read `resume --json` | Literal program/argument array, cwd, `env` to set and `unset_env` to remove relative to the caller's environment, hint and warning; GUI-only targets remain explicit. Claude's default profile is never named in `CLAUDE_CONFIG_DIR`. Never launches; conflicts with `--launch` | `cmd/port.rs` resume JSON tests, `tests/source_resume.rs`; [resume manual](../../../manual/src/cli.md#cv-resume) |
| Read, ingest and resume Pi coding-agent v3 sessions | Bounded 2-pass `jsonl::Read` + `RangeSource` codec over `~/.pi/agent/sessions` (`PI_CODING_AGENT_DIR`), exact active-chain turn counts, compaction/context_edit projection, `Sniff::Pi` ingest and `pi --session <id>` resume plans | `harness/pi.rs`, `ingest.rs`, `cmd/port.rs`; `tests/{pi_synthetic,pi_mechanical_contract,pi_fixtures,source_resume}.rs` |
| Read split Claude sub-agents across in-place `/resume` | Memoized `ProjectAgents` + exact parsed-`uuid` byte-range `RunSlice` joining (`A -> B` and `A -> B -> A` re-entry), cross-parent `task_notices` / `SubagentStop` merging in `lanes`, and checkpoint-aware `claude.discovery.v3` counting | `harness/claude/chain.rs`, `lanes.rs`, `cmd/view.rs`; `tests/{forest,selection}.rs` |
| Export human-turn cycles with `dataset --format cycle` | Streaming `ByteStage` flat JSONL rows per post-initial human turn with `Adapter::human_turn` attestation, `JsonView` tool histograms, commit/push flags, and `spill::Ordered` `--active-branch` spooling; rejects `--clean-outcome` | `dataset/cycles.rs`, `cmd/reshape.rs`; `tests/cli.rs`; [dataset manual](../../../manual/src/cli.md#cv-dataset) |
| Replay Claude system and attachment records in `splice` | Emitted splices read via `emit::prepare_replayable` (`Demand::records`), replaying Claude `system`/`attachment` `_record` carriers with threaded `parentUuid`s and restamped session IDs while keeping `--export json` free of `_record` | `harness/claude/emit.rs`, `splice.rs`, `cmd/reshape.rs`; `tests/selection.rs`; [splice manual](../../../manual/src/cli.md#cv-splice) |
| Shape records and inspect context in `prune --plan` | Per-record `messages` alongside `results`, `pointer`/`drop` actions, 1-based inclusive `lines` excerpts, sidecar stashes recoverable via `cv cat`, and `resume_reads_tokens_before`/`_after` reporting the last surviving request's context size | `prune/plan.rs`, `prune.rs`, `cmd/reshape.rs`; `tests/selection.rs`; [reshaping manual](../../../manual/src/reshaping.md) |
| Carry or join Claude sub-agents with `--carry-agents` and `adopt --join` | `--carry-agents[=meta\|full]` on `prune`, `rewind` and `splice --session` copies spawn records and restamped/joined root-to-tip transcripts with containment checks; `cv adopt --join` prepends earlier turns idempotently | `harness/claude/{carry,adopt}.rs`, `cmd/{reshape,adopt}.rs`; `tests/selection.rs`; [adopt manual](../../../manual/src/cli.md#cv-adopt) |
| Verify `rewind` prefix hashes on same-stat rewrites | `PrefixReadGuard` verifies captured BLAKE3 prefix digests across `source.len()` even when `FileId` metadata matches (sub-jiffy same-size `O_TRUNC` rewrites) | `json.rs`, `rewind.rs` `prefix_read_guard_rejects_same_file_id_when_prefix_hash_differs` |
| Refresh `search --batch` snapshots in place | NDJSON batch stdin accepts `{"query": …, "refresh": true\|false}` and standalone `{"refresh": true}` alongside bare query strings, isolating invalid objects or failed refreshes while keeping the prior snapshot active | `cv-search/src/fts/prepared.rs`, `cmd/search.rs`; `tests/cli.rs`; [search manual](../../../manual/src/search.md) |
| Index sub-agent stems and retain `--frozen` refreshes in memory | `catalog::SCHEMA` 12 `subagent_parents`/`subagents` tables with directory-stat guards (`WATCH_FUDGE = 2s`, zero-alloc `Probe::file_id`) resolve `agent-<id>` without fleet `read_dir` scans; unpersisted `--frozen` refreshes splice in memory | `catalog.rs`, `catalog/{order,reader}.rs`, `lib.rs`, `source.rs`; `copy_authority_tests`, `tests/{forest,cli}.rs` |
| Read Antigravity checkpoints, summary certificates, zip stores, swarm lanes and schedules | `CortexStepCheckpoint` and all 17 `exa.cortex.core.tools.*` `Any` payloads + 19 direct `1.0.x`–`1.5.13` steps + `56`/`133`/`147` interactions + legacy `ImageData` + SQLite `steps` side columns decode with zero unread fields; `conversation_summaries.db` `raw_summary` + row certificates, `.pbtxt` annotations, `swarm/board.jsonl` killed-worker recovery, cross-root native tails, chunked/SQLite zip ingest, `resume_env`, swarm `waiting` lanes/`swarm add` lineage, `schedule` timers, `Origin::Subagent` prompts, and Antigravity 1.0/1.5 + Pi v3 tool/event/affordance semantics | `harness/antigravity*`, `tools/classify.rs`, `events.rs`, `affordance.rs`, `ingest.rs`, `lanes.rs`, `overview.rs`; `tests/{antigravity_*,pi_synthetic,source_resume}.rs` |

**Separate Fleet consumer work:** `d58a1cce`, `6def5a7e` and `73586c1b` in the Fleet repository preserve distinct
harness/ID/path sources, timestamp precision and unknown counts, use the configured cv binary
and shared roots/TMPDIR, and refuse ambiguous exact IDs. Scoped tests passed there. The
exact-source map/doctor selectors and native `cv resume --json` consumer passed composition
checks against the new cv binary, including duplicate IDs across Claude/Codex profile homes.
Shared-store paths use IDs and unaddressable duplicates are refused. No Fleet merge, push or
deployment is claimed by this tracker.

**Still open:** `cv --frozen` now retains refreshed discovery rows in memory when catalog writes are
skipped (`a246351c`); retiring Fleet's consumer-side dual frozen/unfrozen listing workaround remains
separate consumer work. Ordinary search retains fleet-proportional freshness work;
repeated-search consumer measurements remain separate from the batch snapshot and in-place refresh
API. Overview's bounded output does not bound parent traversal or worker-forest reads.
The [native coverage matrix](ANTIGRAVITY-COVERAGE.md) and [handoff limits](HANDOFF.md) remain the
source of unresolved Agy and lifecycle cases.

**Resolved from the read-feature review (2026-11-14):**

1. **Cross-root duplicate IDs (closed):** `cv_core::find` preserves newest-copy `preferred_copy_cmp`
   for HTTP/Tauri/pack compatibility, while CLI ID resolution (`show`, `doctor`, `distill`, `resume`,
   `lanes`) emits a non-fatal shadowed-copy `stderr` note (and `resume` `warning`/comment) naming the
   selected path and `<harness>:<path>`; explicit path selectors silence it (`tests/source_resume.rs`).
2. **`lanes --follow` vanished child (closed):** `harness::vanished_at` and `lanes::lane_or_vanished`
   skip children unlinked between directory listing and reading (`NotFound` on open and
   `symlink_metadata`), while broken symlinks, `EACCES` and corrupt `.meta.json` sidecars still
   return errors (`tests/worker_follow.rs`).
3. **Codex `archived_sessions` rollout caveat (closed):** `Codex::resume_caveat` warns on rollouts
   under `archived_sessions/` in text comments and `resume --json` `warning` (`tests/source_resume.rs`).
4. **Sub-agent delegation prompts in overview (closed):** Claude (`isSidechain: true`), Codex
   (post-`inherit_before` child prompts) and Antigravity (`USER_INPUT` with parent lineage, including
   up-front sidecar resolution) classify parent-delegated prompts as `Origin::Subagent`, excluding
   them from human `requests` in `overview` (`overview.rs`, `tests/antigravity_writer.rs`).

## Second pass (Claude Fable, 2026-11-15)

An independent review of the integrated candidate, with four delegated lanes run against the real
fleet on one machine (10,201 sessions, 499 to 509 of them Antigravity). Every row is implemented
and tested on main; numbers and conditions are in the commit messages and [PERF.md](../PERF.md).

| Area | Defect found in the candidate | Outcome |
|---|---|---|
| Persisted-output pointers | A stub's path was opened as written: any tool output could put a private file into the index, and a transcript tail read as a path failed the session (the original ENAMETOOLONG report, fixed before only in one Antigravity parser) | Claude, Antigravity and Kimi Code resolve a claim only to the file shape their harness spills, beside the transcript (`source::contained_sidecar`); anything else is `persistedOutput.unresolved` and unread |
| Retirement ledger | An unreadable `state.db` read as the legacy rows alone (1 to 5 of 150 frozen reads under a looping writer) | `retired::current` is fallible; the hook refuses calls that wake an agent, `cv retire` fails; frozen state snapshots accept a copy within one WAL generation, 0 refusals in the same churn |
| Issue 33 | Reproduced: a frozen command beside any cv writer could not open the catalog and rediscovered the fleet (21 s, every session called stale) | Read from the leased private copy; 4 s and 66 stale on the same workload. The fixed fleet-proportional freshness cost of an ordinary frozen search (about 7.8 G instructions) remains and is the contract; `--batch` avoids it |
| Index pruning | An unmounted or moved store root reaped every session indexed from it, exit 0 | Kept and named; a deleted file is still reaped |
| Live sessions | Every pass and every non-frozen search reread each live session whole (no baseline published under growth; no mark on a held Codex reply) | Resume marks; live search 11.5 to 13.3 s became 1.8 to 3.5 s |
| Antigravity reconciliation | Exports wrap every non-planner step, so no tool result recovered a native call id and 294 interior native-only steps were dropped silently | Matched as the writer renders them; `inferred_tool_identity` 486 to 50 sessions, step gaps 104 to 2, fleet parse 102 s to 10.4 s |
| Antigravity writer coverage | One real capture | Seven more real 1.5.9 captures (permissions, interruption, background tasks, swarm, sub-agents, prompt media, compaction and resume); eight reader defects fixed; see the [coverage matrix](ANTIGRAVITY-COVERAGE.md) |
| Handoff lifecycle | On 58 Claude transcripts, 59 launches reported settled without evidence and 322 ended ones left unsettled; Codex compound polls gave a running PTY another command's exit | Correlated by task identity per harness; see [HANDOFF.md](HANDOFF.md) for the remaining 129 Claude rows |
| Distill budgets | Checked against the Markdown pack for every target | Measured on what the target loads, and named |

Still open after this pass, most consequential first:

1. **Agent lookup by id (closed)**: `catalog::SCHEMA` 12 indexes `subagent_parents` and `subagents`
   (`idx_subagents_stem`, `idx_subagents_parent`) with compact directory-stat guards (`WATCH_FUDGE = 2s`
   and cached `Probe::file_id`), resolving exact/prefix agent IDs with zero warm `read_dir` calls,
   preserving ambiguity errors, handling `--frozen` post-warm spawns/deletions in memory, and falling
   back to parallel filename scans when `catalog.db` or `sqlite` is unavailable (`copy_authority_tests`,
   `tests/forest.rs`).
2. **Unrendered `queued_command` task notifications (closed; origin-less plain user text remains bounded)**:
   Claude's record codec now reads unrendered `queued_command` attachments (`attachment.prompt`) as
   `InjectedContext` with `commandMode` and `origin` under `extra["claude"]["attachment"]` at every
   tier, settling lifecycle and `task_notices` while keeping task notifications out of `distill` and
   affordance false positives (`unrendered_queued_commands_settle_lifecycle_without_polluting_distill_or_affordances`).
   Replayed plain user text lacking any notification marker or non-human origin remains governed by
   [HANDOFF.md](HANDOFF.md).
3. **Antigravity (partially closed; writer-capture gaps remain)**: `cv lanes` and `overview` now
   report parked swarm workers as `waiting` (with `post_message.text` `last_text` fallback and `swarm add`
   `spawned_by_tool_use` + `agent_path` lineage); `CortexStepCheckpoint` (`checkpoint_details`), scalar
   `GenerateImage`/`DefineSubagent`/`AskQuestion`/`InvokeSubagent` `subagents` payloads, generic
   `struct`/`value` results and non-permission `Tier::Rich`+ interactions decode with zero
   `unsupported_native_fields` across all 8 real-writer captures; `conversation_summaries.db` uses
   per-session row certificates, empty `0`-row DBs filter out in discovery, chunked/SQLite archives
   ingest from zip, and `Antigravity::resume_env` selects/unsets profile env vars. Still open:
   `FileEdit.diff` and blob references remain `COMPLETE`-only; `ask_question` and native `CLEARED` rows
   still lack real-writer captures; audio/video prompts are refused by the CLI's ACP server; a rename
   action does not exist; live `agy` process launch through Fleet remains external.
4. **Index contracts not adversarially tested here**: schema-migration crash boundaries,
   a derived write failing after FTS publication and the reverse, held-generation freshness. The
   existing tests were read, none added.
5. **First index on the fixed binary, cold-cache costs and physical I/O** are unmeasured
   (`getrusage` block counts are zero on this host).
6. **Empty mount-point directory inside a root (closed)**: `Presence::absent_store_with_dev` checks
   the nearest existing directory's `st_dev` against the indexed primary's recorded `FileId.dev`,
   retaining indexed sessions under empty unmounted mount-point stubs while still reaping same-device
   deletions (`source::tests`, `fts.rs`).
7. Free-prose supersession and tokenizer-backed budgets, as HANDOFF.md states.

## Candidate implementation and outstanding scope

The implementation lanes are integrated. This remains an unpublished 0.20.0 candidate;
see [the release plan](README.md) for completed and pending gates. No historical task was
closed in the real ledger by this audit.

| Priority | Outcome | Current disposition and acceptance evidence |
|---|---|---|
| P0 | Read Agy across native stores, exports and disappearing payloads | Implemented for the documented contracts. Independent `antigravity_lifecycle` and `antigravity_fidelity` tests cover identity, source precedence, cleared payloads and guarded tails; see [coverage matrix](ANTIGRAVITY-COVERAGE.md) for unsupported and untested cases |
| P0 | Preserve last-good search state on failed updates | Implemented staging, compatible rebuild retention, incomplete-discovery pruning guards and validated schema replacement. `fts.rs` failure/rebuild tests and `fts/migration.rs` tests retain retryability and detect incomplete derived state |
| P0 | Preserve native tool identity, usage and payload attribution | Implemented verified enrichment; timestamp-only matching cannot transfer usage across revisions. Conflicting/unanchored native tails disclose `unreconciled_history`; native emission and universal fidelity remain outside the claim |
| P1 | Make read sources and known gaps visible | Implemented optional `extra.cv.read_coverage` through text, JSON/API and doctor, including late metadata. Independent fidelity and media-budget tests cover public windows; absence remains unassessed |
| P1 | Reuse repeated indexed-query setup | Implemented `PreparedIndex` and NDJSON `--batch` (including opt-in `{"query": …, "refresh": true}` and standalone `{"refresh": true}` in-place snapshot refresh), pinned snapshots, filters/counts, per-line errors and logical work counters. Ordinary frozen search validates its index before discovery and binds freshness to the held generation; the severe issue 33 catalog-WAL rediscovery failure was reproduced and fixed; per-query freshness cost remains |
| P1 | Budgeted handoffs and explicit unresolved work | Implemented supported lifecycle protocols (including Antigravity `schedule` timers/bounded crons), aggregate budgets, unsettled snapshots and cycle rejection. [HANDOFF.md](HANDOFF.md) separates these guarantees from semantic supersession, unknown lifecycle patterns and tokenizer accounting |
| P1 | Performance follow-through | Warm-fleet second-pass evidence and `catalog::SCHEMA` 12 sub-agent stem indexing landed. Cold/first-index costs, physical I/O, ordinary freshness work and consumer dual reads remain open; measure new overview/selected-pack paths separately. Record results in commit messages |

The [coverage matrix](ANTIGRAVITY-COVERAGE.md) now includes real permission, interruption/retry,
background, swarm/subagent, prompt-image and compaction/resume captures, with zero
`unsupported_native_fields` across all 8 captures, parked `waiting` swarm lanes and `swarm add`
lineage. Remaining writer-capture gaps include audio/video, writer-driven structured generic/render-only
and native-cleared rows, renamed or mid-turn swarm members, non-permission interactions (`ask_question`),
live source promotion and giant/portable native reads. See the matrix for the exact boundaries, including owned inline-media allocations.
[`antigravity_writer`](../../../crates/cv-core/tests/antigravity_writer.rs),
[`antigravity_fidelity`](../../../crates/cv-core/tests/antigravity_fidelity.rs) and
[`antigravity_lifecycle`](../../../crates/cv-core/tests/antigravity_lifecycle.rs) are current
witnesses, not proof of all native fidelity or round-trip emission.

## Local task reconciliation

IDs below are stable ledger locators. “Implemented” names current witnesses; it is a suggested
tracker disposition, not a claim that the audit changed the ledger.

| Task ID | Original request | Disposition and evidence |
|---|---|---|
| `019f6e94-5c35-7b10-a238-f0b8cde8f118` | Synthetic fleet / task replay bench | Superseded form. `c9859bca` added `cv-sim`; `c08a00fc` deliberately removed that land-facet fixture. Keep present performance coverage through `tools/perf-fixture.py`, heap checks and profiling tools; define any new task-scale benchmark against today's reducer |
| `019f6e94-5c6a-70e0-9b72-ce1ef166bd10` | Receipt/Done/trust wording | Superseded by the 0.15 task contract: Done is self-reported; identity recorded, never authenticated. `manual/src/tasks.md`, `task_retired_verbs_and_flags_exit_two_and_done_labels_self_reported` |
| `019f6eef-dc51-73e3-88e7-7fc012afc37c` | Debt/landed verification freshness | Superseded: land/debt/verification semantics intentionally removed in 0.15; old events replay without changing state |
| `019f6ef7-0455-7c70-a234-3ef4107a40e2` | TOFU worker credentials / CV_TOKEN | Superseded: authentication is outside the current task contract; do not restore it as unfinished 0.20 work |
| `019f6ef7-0469-7680-a8af-0bcf921aa706` | Verifiable Done checks | Superseded: `--check-*` retired; filesystem/git observations are sweep candidates, not automatic task closure |
| `019f6ef7-047c-7b33-b8ab-1807bbaafdf2` | Harden saw_change receipts | Superseded with removed receipt/land authority; no current sensor-hardening feature is implied by this old task |
| `019f6f36-7715-7bb0-8ac7-bf44ddf9ce32` | `ls --json` consumer gaps / issue 15 | Implemented with explicit costs: `size_bytes` in base rows; git/display-title via `--enrich`; `--fresh` refreshes discovery. `browse.rs`, `session_row_has_the_contract_keys`, `ls_json_enrich_adds_git_branch_and_synthesized_title`, `ls_json_fresh_enumerates_a_previously_empty_project_and_drops_deleted_rows`. Do not restore historical camelCase keys |
| `01a0fab1-9b64-75d2-a57a-f60324aebb07` | Default task show to three notes | Implemented. The text view prints the newest three and counts the rest; `--all` prints every note, `--json` is whole. `task_show_notes_last_and_grep` |
| `01a0fab1-9b7a-7e90-8744-db9a305bc7cb` | Warn on near-duplicate task titles | Implemented as a stderr warning after the open, never a refusal: same words, a word-prefix, or three shared words at 60%. `task_open_warns_on_a_similar_open_title_and_opens_anyway` |
| `01a10891-5134-7191-a4c0-eca1bbe18da4` | Keep reshape parentUuid chains connected | Implemented at the shared emitter. `reshape_verbs_emit_resumable` walks all emitted parents for main and subagent output across distill, fork, splice and port; `claude_emit_relinks_parents_over_dropped_system_records` covers omitted records. Preserve this invariant while changing handoff behavior |
| `01a114d0-35f5-7871-9196-9b12085bedb8` | Refuse distill over max tokens | Implemented aggregate `--max-bytes` / `--max-estimated-tokens` checks before writes or retirement; `distill_budget_checks_aggregate_before_publishing`. Exact tokenizer/target overhead accounting remains open |
| `01a114d0-3614-7990-9951-876d176ddbdd` | Do not preserve superseded facts | Partially addressed: newest-per-key git observations say “last seen”; historical facts retain indices. Arbitrary prose decision/open-item supersession remains open; see HANDOFF.md |
| `01a114d0-3634-7ce2-8f88-458ac05e1bcb` | Attribute background results correctly | Implemented for supported Claude notifications, Agy task identities and Codex cells/nested PTYs. `distill.rs` completion/ownership tests; ambiguous polls or lost native evidence remain uncertain, as listed in HANDOFF.md |
| `01a114d0-3652-7813-a96c-e6fcc5c02bd0` | Avoid planting with work in flight | Implemented primary-source unsettled/growing-capture refusal and explicit `--allow-unsettled` snapshots; `distill_unsettled_refuses_resume_but_allows_explicit_snapshot`. This is not a fleet-wide lock or job cancellation |
| `01a137d0-6603-71d0-8255-a64b9b1e7e61` | Stale search misses recent projects | Original report remains unreproduced. Live suffix search exists; 0.20 adds failed-publication/rebuild retention, complete-discovery pruning guards and held-generation freshness tests. Those fixes do not alone establish the cause of this older report |

The five retired authority tasks plus removed `cv-sim` are not six new product features to
rebuild. Their modern replacement is a small, explicit task contract and measured storage
behavior. Standard task storage has used authoritative `state.db` plus a reconciled JSONL
companion since 0.19; portable builds retain JSONL authority. The task manual and datapath table
are corrected alongside this audit.

## Architecture and performance follow-through

**Issue 33: repeated frozen search, reproduced and fixed in its severe form.** A frozen command
beside a running cv writer found frames in `catalog.db-wal`, could not open the catalog and
rediscovered the fleet with no scan cache. It now reads the leased private copy. The remaining
per-search cost (session rows, meta docs, dependency table and one stat per session, to say how
far the index trails) is ordinary frozen search's freshness contract; batch queries skip it. The
byte count in the original report was not measured.

**Fleet repeated-query follow-up (pack_selection, 2026-11-14).** Fleet's `agentmap` issues
individual `cv --frozen search --json` processes under `cv_max_inflight`; later background
refreshes should see later index commits. There is no existing query-cohort scheduler.
`cv serve` currently invokes the same stateless search path, including discovery and source
freshness assessment, so keeping that server alive alone does not remove this work.
An isolated new-session/rewrite probe confirmed that `PreparedIndex::refresh()` sees later
commits, while an unreopened batch reader remains pinned. Prepared queries also skip source
freshness assessment and use stored snippets: they are not a drop-in semantic replacement.
`cv search --batch` now supports explicit per-line `{"query": …, "refresh": true}` and standalone
`{"refresh": true}` snapshot reopens while preserving the active snapshot on error.
Next: specify which freshness notes and snippet behavior Fleet needs, then evaluate a bounded
worker pool using that explicit per-request refresh surface.
Keep admission limits, deadlines, restart/error handling, and the existing JSON consumer
contract explicit. Do not silently pin a Fleet reader or add a separate search service.
The probe's timings and conditions are in the accompanying commit, not a fleet-wide speedup claim.

**Exports: old quadratic claim is stale, with qualifications.**
`harness/export/native.rs` already builds a source-validated ID/range map after the metadata
roster exceeds admission. Selected reads reuse it in-process; SQLite catalog persistence
permits reuse by later processes. `selected_reads_past_the_roster_cache_scan_the_archive_once`
checks one initial scan, catalog reuse after clearing process memory and invalidation on
archive change. Duplicate selection remains first accepted entry across admission.
A portable build, an unavailable catalog or a frozen cold process with no persisted record
still scans once per process. The offset map itself grows with IDs and archives. These are
remaining costs, not the old claim that each selected conversation always rescans its archive.
A persistent portable index or bounded offset-store redesign should follow a measured workload,
with selection/parity and freshness tests intact.

**Agent lookup by ID, closed via directory-stat-guarded catalog index.** See item 1 of the second pass:
`catalog::SCHEMA` 12 indexes sub-agent stems in `catalog.db`, eliminating session-proportional
`read_dir` scans on warm exact and prefix lookups (`copy_authority_tests`, `tests/forest.rs`).

**Fleet numbers are current for warm caches.** `PERF.md` records the 0.20 measurements on the
10,185-session fleet. First index on the fixed binary, a full recount, cold caches and physical
I/O remain unmeasured. Rust heap gates do not bound native SQLite allocations or whole-process
footprint. Keep raw result artifacts out of the repository.

**Frozen state snapshot cost, measured small.** Frozen task commands open the state snapshot zero
times and `cv retire` once (about 1.7 ms on APFS, debug build, 299 KiB store). A catalog with
uncheckpointed frames is copied once per source state per process and released at exit. On a
filesystem without cloning each such command still copies the whole file; a reusable
identity-validated copy across processes is the remaining idea, unmeasured.

**Retirement fallback under churn, closed.** An unreadable store is an error, not the legacy
subset; see the second pass. `an_unreadable_store_is_an_error_not_the_legacy_subset`,
`frozen_reads_under_writer_churn_never_omit_a_committed_retirement`,
`retired_guard_and_list_report_an_unreadable_ledger`.

**Tiny corrupted Tantivy files now fail at the read boundary.** Migration testing reproduced an
upstream Tantivy 0.26.1 panic: `Footer::extract_footer` checks for four bytes before slicing
eight. A 4–7-byte truncated segment can therefore panic instead of returning a corruption
error. The shared directory wrapper now checks the mapped handle before footer parsing.
Actual 0, 3, 4, 5, 6, 7 and 8-byte files exercise read-only and writable readers, public search,
incremental refusal and explicit rebuild. This reproduced panic is fixed without catching
panics or forking Tantivy; arbitrary corruption recovery is not implied.

## Compatibility contract and remaining gates

- Keep stored logs and older session envelopes readable. Read coverage is optional metadata;
  absent means unassessed, not complete. Preserve unknown source/limitation names and unrelated
  `extra.cv` keys. Lean API windows must expose relevant coverage without requiring full extras.
- A new batch search format is opt-in. Normal `cv search --json`, filtering, frozen behavior and
  exact/scan modes retain their contracts. Snapshot IDs identify a held reader, not a content
  hash or proof of source freshness; failed refresh must leave the previous reader usable.
- The 0.20 index publication boundary must preserve retryability across existing indexes and
  source records, not merely pass clean-home tests. Test failure before/after FTS publication,
  changed dependencies, deletion and repair; batch work must remain bounded without per-session
  commits becoming the normal performance cost.
- Native adapter fidelity and readable coverage must compose with search, show/API windows,
  usage accounting, doctor and complete-mode exports. Added native-only sessions must not steal
  an existing exported session's identity or silently duplicate tool/usage records.

Local workspace, portable/WASM/corpus, package, compatibility/upgrade, pinned-Codex, manual,
macOS desktop and browser gates passed, including focused source-selection follow-ups.
Heap gates and controlled parse comparisons passed. No public release or Fleet deployment
is asserted here.
