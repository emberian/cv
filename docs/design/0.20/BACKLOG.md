# 0.20 backlog: reconciled scope and remaining work

This is the maintained release backlog, not a dump of historical open labels. Reconciled on
2026-11-14: initial inventory at `d95e2143`, implementation reconciliation at candidate
`d7e8506c`, using the read-only local task ledger (`cv task list --repo … --all
--json --detail`, `CV_FROZEN=1`), [GitHub issue 33](https://github.com/emberian/cv/issues/33),
current implementations and focused tests. The local ledger had 24 tasks: 15 open, nine done;
seven open tasks were tagged `landed-verify`. No ledger state was changed by this audit.

Update a row when its implementation and acceptance evidence land. A historical `done`, closed
issue or agent note is a lead, not a current compatibility or performance guarantee. Concrete
release outcomes and their integration gates belong in the sibling release plan; this page
keeps their provenance and separates unresolved work from obsolete requests.

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
| P1 | Reuse repeated indexed-query setup | Implemented `PreparedIndex` and NDJSON `--batch`, pinned snapshots, filters/counts, per-line errors and logical work counters. Ordinary frozen search validates its index before discovery and binds freshness to the held generation; issue 33's original cost report remains unverified |
| P1 | Budgeted handoffs and explicit unresolved work | Implemented supported lifecycle protocols, aggregate budgets, unsettled snapshots and cycle rejection. [HANDOFF.md](HANDOFF.md) separates these guarantees from semantic supersession, unknown lifecycle patterns and tokenizer accounting |
| P1 | Current composed fleet performance evidence | Still open. Separate ordinary frozen/live and batch queries, cold/warm setup, unchanged/retry indexing, compressed Codex, native Agy and state snapshot costs. Record measurements and conditions in commit messages |

Native coverage follow-up is concrete: the [outstanding writer-generated scenarios and media
allocation boundary](ANTIGRAVITY-COVERAGE.md) cover permissions/cancellation/retries, prompt media
and linked attachments, structured generic/render-only results, nested swarms/peer delivery,
live source promotion, native-only resume roots and giant/portable native reads. The independent
[`antigravity_fidelity` tests](../../../crates/cv-core/tests/antigravity_fidelity.rs) and
[`antigravity_lifecycle` tests](../../../crates/cv-core/tests/antigravity_lifecycle.rs) are current
witnesses, not proof of those remaining scenarios. Reading native metadata does not establish
that cv can plant an Agy session or recover identities the producer never recorded.

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
| `01a0fab1-9b64-75d2-a57a-f60324aebb07` | Default task show to three notes | Open, P2 UX. `render_show` still defaults to all notes; explicit `--notes-last`, grep and brief already exist. Choose/document a default and preserve an explicit complete-history option |
| `01a0fab1-9b7a-7e90-8744-db9a305bc7cb` | Warn on near-duplicate task titles | Open, P2 UX. Current Open validates relations/tags and appends without title similarity checks. A warning must preserve intentional duplicates and avoid a hidden rejection threshold |
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

**Issue 33: repeated frozen search.** The 0.18 cost report lacks an exact query, invocation,
revision, corpus and counter source; it does not establish transcript I/O. Measure index setup,
discovery, query evaluation, stored documents and live/snippet reads separately. Implemented
batch queries explicitly hold a snapshot and say freshness was not assessed;
ordinary frozen search retains its freshness contract. Tests cover no-hit, broad, malformed,
stale and absent-index behavior. Early index validation and held-generation freshness also
landed for ordinary search. These changes do not constitute a reproduction of the original
report or a fleet-wide cost measurement.

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

**Agent lookup remains a real scaling candidate.** `workflow_sessions` reads the freshness-probed
catalog, then `find_subagent_parents` checks each workflow session's sidecar directory. The test
`agent_lookups_read_the_catalog_not_a_rediscovery` rules out rediscovery, not directory work
proportional to sessions. A by-ID inventory needs directory guards so newly written/copied
agents and duplicate IDs are not missed. Measure candidate-directory work first; acceptance
should cover exact and prefix matches, ambiguous IDs, new children and changed roots. P2 unless
fleet evidence makes this a frequent-user bottleneck.

**Fleet numbers remain dated.** `PERF.md` reports dated 0.17 measurements and explicitly leaves
first listing, full recount, repeated unchanged indexing and upgrade cost open. Rust heap gates
do not bound native SQLite allocations or whole-process footprint; compressed and decoded
bytes are different quantities. Neither the offset test nor a new query counter closes these
measurement gaps. Keep raw result artifacts out of the repository; record conditions and results
in commit messages. Arbitrary prefix-edit detection remains the documented sampled-mark limit,
not a new unacknowledged 0.20 guarantee.

**Frozen state snapshot cost, open.** `StateStore::open_readonly` captures a stable private
copy for each frozen open, including relevant WAL/journal sidecars. This protects source files
and holds cleanup ownership until SQLite closes, but on filesystems without cheap cloning it
can repeatedly copy the complete database. Measure repeated task/retirement reads separately
from query evaluation; investigate a reusable, identity-validated snapshot or shared read scope.
Any optimization must preserve no source writes and consistency under checkpoint/replacement.

**Retirement fallback under churn, open correctness work.** `retired::raw_current` starts with
legacy JSONL and returns that map if `open_readonly` fails; SQLite-only retirements can therefore
be omitted when a stable state snapshot cannot be obtained. Make unavailable/indeterminate
state explicit rather than presenting the legacy subset as complete. Add tests with SQLite-only
retirements, concurrent replacement/checkpoint churn and forced read failure, including hook
consumers. Successful frozen-state tests do not close this fallback gap.

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

Full workspace build/clippy/tests, final portable/WASM/corpus checks, package, compatibility,
upgrade, pinned-Codex hooks, manual build/names and dist plan have passed locally. Earlier native
desktop and browser checks are recorded in the release plan. Installation, main integration and
push remain pending; no public release is asserted here.
