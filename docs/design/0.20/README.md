# 0.20 candidate: reliable live histories and trustworthy handoffs

Status: candidate `0.20.0`, source-reader version 13, unreleased and untagged. The current
feature reconciliation is against `6809bfa5`; final integration gates are underway. The earlier
candidate passed its gates before these additions. Claude Fable's second review pass fixed
further real-fleet defects, recorded in [BACKLOG.md](BACKLOG.md). This remains the existing
adapter, IR, query API and command surfaces; `fn` is outside this release's scope.

## Implemented outcomes

1. **Antigravity histories from native stores and exports.** SQLite-only discovery,
   full/compact/chunked history, revised steps and retained rendered output are
   supported. Historical enrichment requires matching evidence; timestamps alone
   cannot transfer usage between revisions. A native tail joins an export only
   across a verified overlap; conflicting or unanchored history is disclosed as
   `unreconciled_history`. Native media and binary signatures retain explicit
   representations. This is reading support, not native round-trip emission.
2. **Last-good search revisions survive failed updates.** Reads stage before
   publication; incomplete discovery updates healthy sessions without pruning
   unseen history. Compatible rebuilds retain failed revisions, and schema upgrades
   publish a replacement generation only after complete validation. Derived-state
   failures remain retryable. Tiny truncated Tantivy segments produce errors rather
   than panics. Query freshness uses the same held generation as ranking.
3. **Repeated search has an explicit snapshot contract.** `PreparedIndex` and
   `cv search --batch` reuse one index reader and expose logical work counters.
   NDJSON framing isolates malformed requests; filters, counts and refresh behavior
   have regression coverage. Batch does no discovery or source reads and explicitly
   does not assess source freshness. Ordinary frozen search retains its separate
   freshness and snippet behavior. The second pass reproduced and fixed issue 33’s
   severe catalog-WAL rediscovery failure; ordinary per-query freshness work remains.
4. **Handoffs enforce budgets and preserve unresolved work.** Aggregate byte and
   estimated-token ceilings are checked before publication/retirement. Typed harness
   lifecycle observations correlate supported Claude, Antigravity and Codex jobs
   with initiating calls, including rewritten native results and nested PTYs.
   Failed/stopped children cannot become successful merely because siblings finish.
   Resumable distill refuses unsettled calls unless an explicit snapshot is requested.
   Claude emitted parent chains also reject cycles.
5. **Known coverage limits are visible.** Show, Markdown, doctor and API metadata
   preserve optional read coverage, including late-discovered gaps and metadata-only
   members. Unknown identifiers survive JSON round trips; absence means unassessed.
   Frozen state reads use stable private SQLite snapshots without source-side writes,
   with snapshot ownership retained until the connection closes.
6. **Select and orient around known sources.** Repeated `pack --session` selections bypass
   corpus search and need no index. A task is optional: without one, the shared overview
   selects initial/recent requests, worker replies, file events and commands with source
   locations and omission counts. `show --kind overview`, the overview API route and the
   reader's on-demand Overview button expose the same bounded evidence. Imported browser
   sessions require WASM and disclose that worker discovery/file checks were not performed.
7. **Inspect saved context and follow workers.** `show --kind context` and matching API
   windows expose recorded usage and compaction rows, deduplicating repeated provider
   records and preserving unknown values. `lanes --follow` emits initial workers and changed
   status/results through a deadline or interruption. `resume --json` returns literal
   program/arguments, cwd, hints and warnings without launching a process.

## Completed implementation lanes

| Lane | Integrated result |
|---|---|
| agy_native | Bounded wire reading, native discovery/reconciliation, payloads, identities and guarded native tails |
| agy_coverage | Independent lifecycle checks and separate scrubbed real-writer lifecycle/background/media corpora |
| atomic_index | Bounded staging, last-good replacement/rebuild behavior, complete-discovery pruning and retryable derived state |
| index_migration | Validated replacement generations, migration failure retention and truncated-segment refusal |
| search_work | Prepared/batch queries, work counters, early index validation and held-generation freshness |
| handoff | Aggregate budgets, typed job ownership/completion, explicit unsettled snapshots and cycle rejection |
| fidelity | Shared coverage metadata, final-metadata propagation, independent API/doctor/Markdown/media tests |
| backlog_audit | Historical task reconciliation, corrected design claims and independent tool/web/desktop gates |
| selected pack / overview | Explicit sources, optional-task overview, shared CLI/API/reader projection and source links |
| context / worker following | Saved context rows, `lanes --follow` changes and non-launching resume JSON |
| integration | Additions integrated through `6809bfa5`; final composed gates underway |

## Acceptance evidence and its limits

- `antigravity_lifecycle` independently exercises native-only and mirrored stores,
  missing children, revision identities, cleared payloads, malformed wire input and
  unknown native fields. `antigravity_fidelity` checks public read surfaces, guarded
  native tails, signatures and media. Real-writer captures are identified separately
  from synthetic fixtures in [the coverage matrix](ANTIGRAVITY-COVERAGE.md).
- Search tests cover failed replacement, incomplete discovery, compatible rebuild,
  schema migration, changing dependencies, retry, held-reader freshness, corruption,
  prepared-reader parity and bounded staging. This is not a claim that independent
  databases share one crash-atomic transaction; incomplete derived state must stay
  detectable and repairable.
- [HANDOFF.md](HANDOFF.md) records exact protocol coverage and unresolved cases.
  Newest-per-key git facts do not solve semantic supersession of arbitrary prose.
  Token estimates are not tokenizer counts or target-harness context guarantees.
- The native-image counting gate bounds additional live Rust allocations;
  public image reads still own data-URI strings. It does not bound SQLite/C memory,
  mappings or total process footprint. See the coverage matrix's media boundary.
- The earlier candidate passed workspace, portable/WASM, corpus, package, compatibility,
  upgrade, pinned-Codex, manual, browser and native desktop gates. The newly integrated
  selection/overview/context/follow/resume changes have focused regression tests; final
  composed gates are underway. Earlier passes do not certify the new integrated tree.

## Remaining work and limits

The second pass closed the retirement fallback, the reproducible form of issue 33, persisted-output
containment, absent-root pruning, live-session rereads and the Antigravity export reconciliation,
and added seven real-writer captures. What remains is listed, in order of consequence, under
"Still open after this pass" in the backlog. Ordinary search freshness remains fleet-proportional;
Fleet’s dual frozen/unfrozen listing workaround remains pending incident reproduction. Overview
bounds output, not parent traversal or worker discovery cost. No claim of complete Antigravity
fidelity or native emission follows from indexing.

Fleet’s `d58a1cce` reader correction is a separate repository commit, not a cv change. Its exact-source
and native-resume follow-up is awaiting composition checks; no Fleet merge or deployment is claimed.

[BACKLOG.md](BACKLOG.md) contains reconciled task dispositions and acceptance criteria
for these follow-ups. The read-only inventory found 15 nominally open local tasks
and one open GitHub issue, [33](https://github.com/emberian/cv/issues/33); no real task
store was changed. Measurements belong in commit messages, not committed result
files. Candidate status remains in effect; local validation is not publication.
