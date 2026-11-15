# 0.20: reliable live histories and trustworthy handoffs

Status: active implementation, authorized by Ember on 2026-11-14. Integration owner:
Codex. This is a release target and acceptance contract, not a claim that the work
has landed. The existing adapter, IR, query API and routes remain the product shape.

## Release outcomes

1. **Faithful Antigravity histories.** Read native SQLite as well as full/compact/
   chunked exports; reconcile native identities with historical text retained only
   in logs. Preserve exact tool associations, meaningful native payloads and unknown
   records. Report missing or unsupported data explicitly. A successful index pass
   alone is not the acceptance test.
2. **Safe live publication.** A failed read, rewrite, cancellation or replacement
   cannot erase the last committed searchable session or publish fresh derived
   state for an incomplete body. Stage with bounded memory; preserve coherent
   counts, events, source baselines and resume marks through publication failures.
3. **Efficient repeated search.** Reuse an indexed reader for batch/persistent
   callers. Measure query work separately from discovery, source reads, index setup
   and snippet enrichment. Preserve matching/filter semantics and the read-only
   frozen contract; state indexed coverage and freshness assessment accurately.
4. **Trustworthy handoffs.** Distill identifies pending work and distinguishes
   superseded observations from current facts. Preserve call/result attribution,
   validate resumability, and enforce explicit output budgets before publishing
   artifacts or retiring sources. Estimates must be labelled as estimates.
5. **Visible fidelity.** Existing show/doctor/API surfaces tell readers when history
   is export-only, metadata-only, missing observed payloads, or uses inferred
   identity. Lack of an assessment never means complete. Keep old stored IR readable.

## Work ownership

| Lane | Scope | State |
|---|---|---|
| agy_native | Bounded protobuf reader, SQLite-only discovery, reconciliation, exact native associations and payloads | Implementing |
| agy_coverage | Native contract/feature matrix, scrubbed real-store fixtures, adversarial lifecycle tests and independent review | Implementing |
| atomic_index | Shared bounded staging and coherent FTS/event/count/baseline publication | Foundation integrated; compatible rebuild retention in progress |
| index_migration | Build and validate replacement schema before atomically publishing it | Implementing |
| search_work | Prepared/batch search, frozen work accounting and repeated-query regression checks | Prepared reader and batch CLI integrated; default-query investigation continues |
| handoff | Distill budgets, unfinished work, supersession and resumability | Implementing |
| fidelity | Shared coverage contract and show/doctor/API visibility | Contract and consumers integrated; native composition tests in progress |
| backlog_audit | Reconcile historical tasks and independently review consumer contracts | Inventory and corrections integrated |
| integration | Resolve interfaces, reconcile backlog, integrate, run composed release gates and install | Active |

Each lane uses its own worktree, target and scratch CV home. Harness homes are
read-only. Scoped checks run in lanes; the full workspace and release gates run on
integrated code. Research output and private native schemas/transcripts stay outside
Git. New corpus versions preserve provenance; synthetic fixtures are labelled as such.

## Acceptance scenarios

- Parallel same-name tool calls complete out of order; reused step indices replace
  earlier revisions without fabricating returns or merging unrelated native records.
- Compaction clears native payloads while logs retain history; export disappearance
  leaves a readable native store; missing history is visible rather than invented.
- Source rewrite/delete after discovery, during parsing, and during publication:
  previous committed text survives failed replacement, failed work stays stale,
  and a successful later pass replaces it without duplicates.
- Repeated bounded indexed queries over increasing unrelated data: accounting
  distinguishes setup/evaluation/metadata/source work. Include broad matches,
  no hits, malformed queries, stale/absent indexes and equivalent filters.
- Nested or renamed swarm members, killed workers without transcripts, waiting
  workers and peer messages remain distinguishable from completed child returns.
- A handoff with pending, canceled, failed and completed jobs preserves those
  distinctions. Aggregate output budgets fail before writes; later evidence does
  not leave an earlier conclusion presented as current.
- JSON, text, doctor, search, events and portable builds agree on relevant semantics;
  native-only functionality states its availability in builds without SQLite.

## Backlog reconciliation

Read-only inventory on 2026-11-14 found one open GitHub issue:
[#33](https://github.com/emberian/cv/issues/33), repeated frozen-search work and
accounting. Its reported expensive query is not yet a controlled reproduction.
There were no open PRs at inventory time.

The local task ledger had 15 open cv tasks, seven tagged `landed-verify`, and nine
completed tasks. Open labels are leads, not proof of missing implementation. In
particular, distill budgets, superseded facts, background result attribution,
in-flight handoff and parent-chain validation need code/test reconciliation.
Task display and duplicate-title UX are secondary candidates, not substitutes for
release outcomes above. Real task stores are not mutated by this work.

The audit verified the existing export offset-index fallback and corrected stale
roster-cache, retired-command and task-authority claims. Remaining costs and exact
task dispositions are in [BACKLOG.md](BACKLOG.md). Fresh fleet measurements belong
in commit messages, not committed result files.

## Integration findings

- Independent lifecycle tests exposed a pre-existing merge error: index plus
  second-resolution timestamp can attach native usage from another revision.
  Enrichment must require verified identity; usage is not exempt from that rule.
- Index publication must batch completed staged sessions and defer derived state
  until durable commit. A commit for every session would introduce a new scaling
  problem and is not the intended production design.
- Several historical task-ledger items describe machinery intentionally removed
  by the current task contract. Reconciliation must distinguish removed designs,
  implemented work needing closure, and reproduced defects.
- Native `CLEARED` is not a completion verdict and does not mean every field was
  removed. Real records retain useful rendered text; decode available content while
  reporting the historical payload loss.
- Prepared search holds an explicitly indexed snapshot. Ordinary frozen search
  still assesses source freshness and can enrich snippets; its remaining costs
  must be measured separately rather than declared fixed by the batch mode.

## Integration and release

Resolve shared interfaces before merging dependent lanes. Use small commits with
scoped evidence, then test composition on the integration branch. Run the project
CI checks, native/portable/WASM builds, package verification, manual names/build,
web/inbox tests, compatibility checks and relevant heap/performance gates. Validate
against scratch copies/indexes with live harness stores read-only. Only then bump
the release version and install the integrated binary; do not call unfinished native
coverage complete merely to make a release label.
