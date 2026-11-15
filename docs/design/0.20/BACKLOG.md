# 0.20 backlog: reconciled scope and remaining work

This is the maintained release backlog, not a dump of historical open labels. Reconciled on
2026-11-14 against `d95e2143`, the read-only local task ledger (`cv task list --repo … --all
--json --detail`, `CV_FROZEN=1`), [GitHub issue 33](https://github.com/emberian/cv/issues/33),
current implementations and focused tests. The local ledger had 24 tasks: 15 open, nine done;
seven open tasks were tagged `landed-verify`. No ledger state was changed by this audit.

Update a row when its implementation and acceptance evidence land. A historical `done`, closed
issue or agent note is a lead, not a current compatibility or performance guarantee. Concrete
release outcomes and their integration gates belong in the sibling release plan; this page
keeps their provenance and separates unresolved work from obsolete requests.

## Release priorities

| Priority | User outcome / work | Acceptance boundary |
|---|---|---|
| P0 | Antigravity sessions remain readable while native stores, exports and persisted payloads change or disappear | Native-only and mixed-store fixtures; identity, source precedence and invalidation; missing files yield an explicit limitation or recoverable failure, never a fabricated empty success |
| P0 | Failed or changing-session index reads cannot publish partial search state as complete | Failure after emitted chunks, changed dependencies, missing transcript and repaired retry; documents, events, certified counts and marks describe the same accepted read |
| P0 | Agy native replay preserves tool identity, result attribution, usage and timestamps | Evidence fixtures for each recognized native field; timestamp-only matching must not transfer native usage between revisions. Unknown fields and inferred identities remain visible limitations |
| P1 | Users can distinguish exported history, native data and metadata-only discovery | Additive read coverage in ordinary text, JSON/API and doctor; absence means unassessed; unknown future values survive; coverage never implies lossless export or write support |
| P1 | Repeated indexed queries reuse setup and report their actual work | Prepared reader and NDJSON batch on the existing search surface; explicit index-snapshot freshness; stable answers/filters, per-line errors, bounded framing and source-read counters |
| P1 | Resumed distilled sessions carry current facts and unresolved work correctly | Token-budget refusal; supersession fixtures; background result attribution; refuse planting unresolved calls/jobs unless their native continuation is preserved |
| P1 | The release's performance claims describe the composed system | Separate frozen and live runs, cold and warm setup, unchanged/retry indexing, compressed Codex and native Agy; report logical work, native/Rust allocation limits and dated fleet measurements in commit messages |

“Supporting all Agy features” needs a capability inventory, not a parser-success count. For each
native record family and artifact, state whether discovery, read, search, usage, lineage,
attachment access, complete replay, export and resume are supported, partial or unsupported.
In particular, reading native metadata does not establish that cv can plant an Agy session or
reconstruct tool calls whose identity the producer never recorded. The fixture matrix should
exercise native-only history, exported history, metadata-only sessions and mixed-source reads.

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
| `01a114d0-35f5-7871-9196-9b12085bedb8` | Refuse distill over max tokens | Active P1 handoff work; validate selected output and error before any destination mutation |
| `01a114d0-3614-7990-9951-876d176ddbdd` | Do not preserve superseded facts | Active P1 handoff work; explicit contradictory/superseded evidence fixtures, with conservative attribution |
| `01a114d0-3634-7ce2-8f88-458ac05e1bcb` | Attribute background results correctly | Active P1 handoff work; join identities across launch, progress and completion without inventing a call |
| `01a114d0-3652-7813-a96c-e6fcc5c02bd0` | Avoid planting with work in flight | Active P1 handoff work; unresolved tools and background jobs both matter; no success merely because synchronous calls balanced |
| `01a137d0-6603-71d0-8255-a64b9b1e7e61` | Stale search misses recent projects | Unresolved historical symptom. Live suffix search exists (`fts/live.rs`), so this is not evidence that freshness is absent. Reproduce discovery/index/live-boundary behavior and relate it to the failed-session publication work; retain regression coverage for new projects |

The five retired authority tasks plus removed `cv-sim` are not six new product features to
rebuild. Their modern replacement is a small, explicit task contract and measured storage
behavior. Standard task storage has used authoritative `state.db` plus a reconciled JSONL
companion since 0.19; portable builds retain JSONL authority. The task manual and datapath table
are corrected alongside this audit.

## Architecture and performance follow-through

**Issue 33: repeated frozen search.** The report names roughly 1.3 GB of read activity and
2.5 seconds per query on 0.18, but has no exact query, invocation, revision, corpus or counter
source. It does not establish transcript I/O. Measure index setup, discovery, query evaluation,
stored documents and live/snippet reads separately. Prepared queries must not silently turn
ordinary frozen search's freshness assessment into an index-only promise. A batch mode can
explicitly hold a snapshot and say freshness was not assessed. Include no-hit, broad, malformed,
stale and absent-index cases; preserve the ordinary search output contract.

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

**Tiny corrupted Tantivy files now fail at the read boundary.** Migration testing reproduced an
upstream Tantivy 0.26.1 panic: `Footer::extract_footer` checks for four bytes before slicing
eight. A 4–7-byte truncated segment can therefore panic instead of returning a corruption
error. The shared directory wrapper now checks the mapped handle before footer parsing.
Actual 0, 3, 4, 5, 6, 7 and 8-byte files exercise read-only and writable readers, public search,
incremental refusal and explicit rebuild. This reproduced panic is fixed without catching
panics or forking Tantivy; arbitrary corruption recovery is not implied.

## Compatibility checks for integration

- Keep stored logs and older session envelopes readable. Read coverage is optional metadata;
  absent means unassessed, not complete. Preserve unknown source/limitation names and unrelated
  `extra.cv` keys. Lean API windows must expose relevant coverage without requiring full extras.
- A new batch search format is opt-in. Normal `cv search --json`, filtering, frozen behavior and
  semantic/scan modes retain their contracts. Snapshot IDs identify a held reader, not a content
  hash or proof of source freshness; failed refresh must leave the previous reader usable.
- The 0.20 index publication boundary must preserve retryability across existing indexes and
  source records, not merely pass clean-home tests. Test failure before/after FTS publication,
  changed dependencies, deletion and repair; batch work must remain bounded without per-session
  commits becoming the normal performance cost.
- Native adapter fidelity and readable coverage must compose with search, show/API windows,
  usage accounting, doctor and complete-mode exports. Added native-only sessions must not steal
  an existing exported session's identity or silently duplicate tool/usage records.
