# Antigravity support

Owner: Codex. Development spike, 2026-11-14.

The target is faithful discovery, reading, search, live following, tool/event and
affordance extraction, subagent forests, export and resume across Antigravity CLI,
desktop and IDE stores. Static transcript success does not establish native feature
completeness. The adapter uses the shared IR and checked readers; no parallel API or
Antigravity-only command family is needed.

## Implemented in this spike

- Replay the latest record at each step index, in step order. Export append order
  is not conversation order: parallel tools complete out of order and existing
  indices can be revised. Disable incremental message checkpoints for this adapter.
- Keep pending tool identities across interleaved system deliveries.
- Recover full history from contiguous numbered byte chunks when the monolithic
  full export is missing; prefer it over a compact export. Chunks can split JSON
  strings and UTF-8. Reject gaps rather than silently publish incomplete history.
- Read owned SQLite metadata under a pinned snapshot. Updating a title or WAL no
  longer invalidates every lazy text span from an unchanged transcript. Metadata
  dependencies still participate in cache invalidation.
- Recover parent/root lineage and subagent specification from native trajectory
  metadata, independently of the summary catalog. Supplement exported usage with
  native cache-write and reasoning counters when the step timestamp matches.
- Merge native swarm membership with older subagent descriptors. Preserve killed
  or unavailable children as metadata-only lanes instead of erasing their existence.
  `ALIVE` is not a claim that the worker is currently running.
- Recognize action-sensitive swarm spawning/model changes, approval requests and
  board messages. Background task deliveries are job completions, not timers.
- Preserve selected known and future JSONL records under COMPLETE demand.
- Retry changed index sources once. Keep the last committed body when a transient
  failure happens before replacement output begins. Report transient failures in
  the aggregate instead of printing one misleading parse error per deleted worker.

The new `installed-1.5.9-lifecycle-2026-11-14` corpus contains selected harness-written ranges
with free text and argument strings redacted (protocol actions retained).
The label identifies the installed CLI at capture, not an inferred writer version
for historical rows. It preserves duplicate revisions and
parallel completion order. Synthetic tests cover chunk boundary splits, missing
chunks, metadata changes during reads, missing children, swarm-only registries,
native metadata fallback and index disappearance/reconciliation. The original
1.5.6 store remains unchanged.

## Native storage contract

The inspected CLI is 1.5.9 and embeds protobuf descriptors. `conversations/<uuid>.db` contains
plaintext `gemini_coder.Step` payloads, not the encrypted `.pb` format. Its tables
include `steps`, `gen_metadata`, `executor_metadata`, `parent_references`,
`trajectory_metadata_blob` and `battle_mode_infos`.

The full JSONL export retains historical text that SQLite may clear at compaction
(`CLEARED` step status). Switching wholesale to SQLite loses history. A native
reader must reconcile historical logs with native identities and retained payloads.
Export timestamps have only second precision; native metadata has finer precision.
Index plus timestamp alone is insufficient evidence for arbitrary payload merging.

Native fields absent from the exported projection include tool call IDs, original
arguments, thinking signatures, requested/completed interactions, permissions,
media bytes, task details, render information and typed step payloads. Known metadata
counter enrichment is deliberately narrower than claiming native payload fidelity.

The native swarm registry lives at `.system_generated/swarm/members.json`. Member
phase, killed state, desired state and wake mode are distinct. The old
`.system_generated/subagents/<id>.json` registry can coexist with it. Removing a
worker can delete its transcript while leaving both parent membership and a summary
catalog row. Summary catalog presence is not evidence of readable conversation text.

The exported log is a projection the writer does not keep current. It never
receives a cancelled tool step or an upstream ERROR_MESSAGE step, it is re-created
from the resume point when a conversation is resumed after its `brain/<id>` directory
was removed, and a step is not rewritten after export: a backgrounded command stays
`RUNNING` there while the store revises it to `DONE` or `CANCELED`. Reading both
layouts therefore replays every step the export lacks from the store, in step order,
and takes a terminal status from the store for a result whose call ID a verified
planner step issued. The exported body is kept. Acceptance still requires the newest
comparable step to verify; a typed request is compared inside its `<USER_REQUEST>`
envelope.

Permission interactions live on the tool step (`requested_interaction` 133,
`completed_interaction` 147 with the allow flag) and nowhere in the export. Print
mode cannot ask and cancels the gated step with no interaction recorded.

A child's reply is delivered by the system sender as an idle notice naming the
child. A swarm board wake names a member as sender that is not reliably the poster,
and a worker's log opens with one before its assignment. `members.json` lists the
coordinator itself, and `added_by` is the only record that a worker added a helper:
the helper's native parent is the coordinator.

`agy --conversation <id>` is the installed CLI's resume contract, matching the
existing `cv resume` implementation. A resumed conversation keeps its ID and root in
the store (verified from a native-only resume). No native import/export command was found.
The adapter remains read-only; native emission is explicitly unsupported. IR export
is available and is not advertised as an executable native conversation.

## Outstanding work

These are engineering tasks owned by Codex, not permission requests or completed
capabilities. The spike does **not** establish support for every native feature.

| Area | Required work and acceptance condition |
|---|---|
| Native step reader | Stream selected protobuf fields through bounded SQLite cell readers; handle SQLite-only sessions. Reconcile cleared payloads with full logs without losing history or pairing unrelated revisions. |
| Exact tool identity | Preserve native call IDs, original names/arguments and explicit result associations; cover parallel same-name calls and reused indices. Retries and interrupted calls are covered by `1.5.9-interruption`. FIFO inference remains a JSONL fallback. |
| Native payload fidelity | Permission state, stop reasons, task identity and prompt media are read. Still open: structured tool results from the writer, other interaction kinds, the upstream error's status text (only its user-facing message is normalized), and recovering a missing exported upload from the store. COMPLETE must preserve unsupported native fields with provenance. |
| Live replacement atomicity | Stage a session replacement until all source guards pass. A rewrite after documents begin currently removes partial output and may remove the old indexed body; it must retain the prior committed revision. |
| Root and format transitions | Test log disappearance with a surviving native DB, full/compact/chunk promotion, mirror divergence, CLI/IDE archives and summary-only records. Discovery must choose coherent readable evidence. |
| Metadata freshness | Use per-session summary revision certificates rather than invalidating every session on the shared summary WAL. Verify title/lineage updates with no transcript write. |
| Swarm semantics | Spawn call IDs, board delivery versus child returns and registry member state are read (`1.5.9-swarm`, `1.5.9-subagents`). Still open: waiting versus working (the registry's `phase` does not say; no member was captured mid-turn), a lane status for parked or idle children, the forest's `tool_use_id` (derived from `spawnStepIndex`, which matches no call), renamed members (the CLI has no rename). |
| Native feature corpus | Captured: permissions, cancellation and retry, background tasks, swarm, children, prompt media, manual compaction and native-only resume. Still to capture: skills, modes, questions, writer-cleared rows, audio/video. Add portable/no-SQLite expectations explicitly. |
| Resume and emission | Test root-aware resume for mirrored CLI/desktop/IDE stores. Do not enable native writing until a supported import contract and round-trip tests exist. |

Measurements belong in commit messages, not committed result files. Private
transcripts and extracted application schemas remain outside the repository.
