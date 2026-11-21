# 0.20 candidate: reliable live histories and trustworthy handoffs

Status: candidate `0.20.0`, source-reader version 16, unreleased and untagged. The current
feature set has passed local integration gates, including the source-selection and second/third-pass
follow-ups recorded in [BACKLOG.md](BACKLOG.md). This remains the existing
adapter, IR, query API and command surfaces.

## Implemented outcomes

1. **Antigravity histories from native stores and exports.** SQLite-only discovery
   (filtering empty shell databases), per-session `conversation_summaries.db` row
   certificates, 1.5.13 `CascadeTrajectorySummary` (`raw_summary`) and `parent_references` /
   `trajectory_meta` fallbacks, richer `.pbtxt` annotations, `<USER_SETTINGS_CHANGE>` model
   fallbacks, chunked/SQLite archive ingest, full/compact/chunked history,
   decoded compaction checkpoints, all 19 direct `1.0.x`–`1.5.13` `gemini_coder.Step` variants,
   all 17 `exa.cortex.core.tools.*` `Any` payloads, non-permission `56`/`133`/`147` interactions,
   stripped SQLite `steps` side-column fallbacks, revised steps and retained rendered output are
   supported. Historical enrichment requires matching evidence; timestamps alone
   cannot transfer usage between revisions. A local or cross-root native tail joins
   an export only across a verified overlap; conflicting or unanchored history is
   disclosed as `unreconciled_history`. Swarm `waiting`/`working` lane status,
   `swarm/board.jsonl` killed-worker title/summary recovery, `swarm add` spawn lineage and
   profile-aware `resume` plans are supported. Native `Media`, legacy `ImageData` and binary
   signatures retain explicit representations. This is reading support, not native round-trip emission.
2. **Last-good search revisions survive failed updates.** Reads stage before
   publication; incomplete discovery updates healthy sessions without pruning
   unseen history, and empty unmounted mount-point directories on another `st_dev`
   keep their indexed sessions. Compatible rebuilds retain failed revisions, and schema upgrades
   publish a replacement generation only after complete validation. Derived-state
   failures remain retryable. Tiny truncated Tantivy segments produce errors rather
   than panics. Query freshness uses the same held generation as ranking.
3. **Repeated search has an explicit snapshot contract.** `PreparedIndex` and
   `cv search --batch` reuse one index reader, support opt-in in-place snapshot
   refresh (`{"query": …, "refresh": true}` and `{"refresh": true}`), and expose
   logical work counters. NDJSON framing isolates malformed requests and failed
   refreshes while preserving the active snapshot; filters, counts and refresh behavior
   have regression coverage. Batch does no discovery or source reads and explicitly
   does not assess source freshness. Ordinary frozen search retains its separate
   freshness and snippet behavior. The second pass reproduced and fixed issue 33’s
   severe catalog-WAL rediscovery failure; ordinary per-query freshness work remains.
4. **Handoffs enforce budgets and preserve unresolved work.** Aggregate byte and
   estimated-token ceilings are checked before publication/retirement. Typed harness
   lifecycle observations correlate supported Claude, Antigravity and Codex jobs
   with initiating calls, including rewritten native results, Antigravity `schedule`
   timers/bounded crons, and nested PTYs.
   Failed/stopped children cannot become successful merely because siblings finish.
   Resumable distill refuses unsettled calls unless an explicit snapshot is requested.
   Claude emitted parent chains also reject cycles.
5. **Known coverage limits are visible.** Show, Markdown, doctor and API metadata
   preserve optional read coverage, including late-discovered gaps and metadata-only
   members. Unknown identifiers survive JSON round trips; absence means unassessed.
   Frozen state reads use stable private SQLite snapshots without source-side writes,
   with snapshot ownership retained until the connection closes, and `--frozen`
   listing/lookup retains refreshed discovery rows in memory without mutating `catalog.db`.
6. **Select and orient around known sources.** Repeated `pack --session` selections bypass
   corpus search and need no index. A task is optional: without one, the shared overview
   selects initial/recent human requests (excluding `Origin::Subagent` delegation briefs
   across Claude, Codex and Antigravity), worker replies, file events and commands with source
   locations and omission counts. `show --kind overview`, the overview API route and the
   reader's on-demand Overview button expose the same bounded evidence. Imported browser
   sessions require WASM and disclose that worker discovery/file checks were not performed.
7. **Inspect saved context, follow workers, and resolve sub-agents quickly.** `show --kind context`
   and matching API windows expose recorded usage and compaction rows, deduplicating repeated provider
   records and preserving unknown values. `lanes --follow` emits initial workers and changed
   status/results through a deadline or interruption while skipping children unlinked mid-pass.
   `catalog.db` indexes sub-agent stems with directory-stat guards for fast `agent-<id>` lookup.
   `resume --json` returns literal program/arguments, cwd, environment, hints and warnings
   (including archived Codex rollouts and shadowed cross-root IDs) without launching a process.
   Doctor and resume accept individual transcript paths; Claude, Codex and Antigravity resume plans select
   the source's profile home relative to the caller's environment (Claude's and Antigravity's default CLI
   profiles are left unnamed), while unsupported source selections fail explicitly.
8. **Read Pi sessions, rejoin split Claude workers, and carry selective reshapes.**
   Pi coding-agent v3 `.jsonl` transcripts discover, stream, ingest and produce `resume` plans as
   the 23rd harness. Claude reads unrendered `queued_command` attachments and `isSynthetic` turns,
   bridges skipped-record parents, and joins sub-agents split across in-place `/resume`s.
   `dataset --format cycle`, replayable Claude `splice`, per-record `prune --plan` with
   last-request context reporting, `--carry-agents[=meta|full]` / `adopt --join`, and `rewind`
   same-stat prefix verification complete the reshape surface.

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
| pi_reader | Bounded 2-pass Pi v3 `.jsonl` reader, exact turn counts, compaction/context projection, ingest and resume |
| dataset_claude_codec | Unrendered `queued_command` reading, `isSynthetic` turns, skipped-parent bridging and `dataset --format cycle` |
| splice_prune_rewind | Replayable Claude `splice` carriers, per-record `prune --plan`, last-request context reporting and `rewind` prefix hash check |
| claude_chain_carry | Split sub-agent chain joining across in-place `/resume`, `--carry-agents[=meta\|full]` and `adopt --join` |
| edges_and_ci | `macos-15` CI runner, `search --batch` in-place snapshot refresh, vanished-child skip, archived/shadowed caveats and unmounted-dir guard |
| fast_subagent_frozen_ls | `catalog::SCHEMA` 12 directory-stat-guarded sub-agent stem index and in-memory `--frozen` discovery refresh |
| agy_native_fidelity | Checkpoint/payload decoding, per-session summary row certificates, cross-root native tails, chunked/SQLite zip ingest and `resume_env` |
| agy_swarm_lanes_lifecycle | Swarm `waiting`/`working` lanes, `swarm add` spawn lineage, `schedule` reminder tracking and `Origin::Subagent` delegation prompts |
| p3_agy_native_sidecars | 1.0.x–1.5.13 direct steps, 17/17 `Any` payloads, `56`/`133`/`147` interactions, `ImageData`, SQLite side columns, 1.5.13 `raw_summary`, `.pbtxt` and `swarm/board.jsonl` |
| p3_classify_events | Antigravity 1.0/1.5 and Pi v3 tool classification, hunk fingerprints, compact lane labels and affordances |
| integration | Read/selection additions, source-routing follow-ups and Round 2/3/Pass 3 cross-branch integrations; local gates passed |

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
- Workspace build/tests/clippy, focused integration follow-ups, portable/WASM and corpus
  checks, package verification, compatibility/upgrade, pinned-Codex hooks, manual and
  browser gates passed. Desktop check/tests passed on macOS. Heap gates passed; controlled
  baseline/candidate parse comparisons passed after a load-sensitive concurrent run.
  These local checks do not establish support on every operating system.

## Remaining work and limits

The second pass closed the retirement fallback, the reproducible form of issue 33, persisted-output
containment, absent-root pruning, live-session rereads and the Antigravity export reconciliation,
and added seven real-writer captures. What remains is listed, in order of consequence, under
"Still open after this pass" in the backlog. Ordinary search freshness remains fleet-proportional;
`cv --frozen` now retains refreshed discovery rows in memory when catalog writes are skipped, while
removing Fleet’s dual listing workaround remains separate consumer work. Overview
bounds output, not parent traversal or worker discovery cost. No claim of complete Antigravity
fidelity or native emission follows from indexing.

Fleet’s `d58a1cce` reader correction is a separate repository commit, not a cv change. Its exact-source
and native-resume follow-up is awaiting composition checks; no Fleet merge or deployment is claimed.

[BACKLOG.md](BACKLOG.md) contains reconciled task dispositions and acceptance criteria
for these follow-ups. The read-only inventory found 15 nominally open local tasks
and one open GitHub issue, [33](https://github.com/emberian/cv/issues/33); no real task
store was changed. Measurements belong in commit messages, not committed result
files. Candidate status remains in effect; local validation is not publication.
