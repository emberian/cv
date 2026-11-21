# Antigravity coverage contracts

These independent acceptance checks define the native reading guarantees for
0.20. The outstanding scenarios below remain separate from those guarantees.

The synthetic cases in `tests/antigravity_lifecycle.rs` use protobuf encodings
derived from installed 1.5.9 field descriptors without sharing the production
encoder/decoder. They are explicitly separate from the real-writer corpus case.
Every test uses an isolated CV home and owned SQLite files. Private conversation
content and extracted descriptor sets are not repository fixtures.

| Contract | Independent acceptance check |
|---|---|
| Partial fleet corruption | A malformed sibling database cannot hide a healthy native session. |
| SQLite-only history | Discovery finds `conversations/<id>.db`; user text survives parsing. |
| Empty export | An empty full-export file does not hide usable native history. |
| Tool identity | Two same-name calls with results in reverse order retain explicit native call IDs. |
| Export/native merge | Exact body and call arguments recover native IDs; a changed planner body prevents cross-revision enrichment. |
| Cleared historical payload | An available full export preserves text cleared from SQLite. |
| Cleared native-only payload | Empty/cleared planner text is not invented; coverage reports `missing_payloads`. |
| Replaced native row | Same index and second with different body must not transfer usage from a different revision. |
| Storage format drift | Unknown `step_format` fails or explicitly reports unsupported/missing payloads. |
| Malformed wire input | Truncated varints, lengths beyond EOF, and overflowing varints fail or report unavailable content. |
| Mirrored roots | Full exported history wins over a compact mirror without duplicating the session. |
| Native-only child | Surviving child DB is read instead of returning a metadata-only swarm stub; killed state survives. |
| Thinking fidelity | A native textual thinking signature is retained in the IR. |
| Future native type | COMPLETE demand reports uninterpreted native steps rather than silently succeeding empty. |
| Future nested field | COMPLETE retains the original wire bytes and discloses unsupported normalization. |
| Retained render after clearing | Native terminal output remains readable and discloses omitted lines and missing original payloads. |

The existing `installed-1.5.9-lifecycle-2026-11-14` corpus contains scrubbed
harness-written full-log ranges covering duplicate running steps, parallel
completion order, and revised step indices. Its label identifies the installed
CLI at capture, not a proven writer version for every historical row. It does
not cover native-only, media, or broader swarm scenarios.

The `1.5.9-native-background-media` corpus is a new real CLI 1.5.9 capture
from an isolated scratch home, driven by a deterministic local Gemini endpoint.
It includes native-only SQLite history, a background command and its completion
message, and an inline image tool result. The generator verifies the writer
version, records the executable hash, and enforces OS-level write/network bounds.
The lifecycle test checks the recovered image bytes and background output.

### Real-writer scenarios

Seven further 1.5.9 captures come from the same generator (`--scenario`), driven
through `--print` and the CLI's ACP server. `tests/antigravity_writer.rs` reads each
in the layouts it can have (exported log beside the store, and the store alone) and
takes its expectations from the scripted scenario and a schema-less walk of the
stored bytes, not from the adapter's decoder.

| Capture | Asserted from writer bytes |
|---|---|
| `1.5.9-permissions` | An approved and a denied command carry `interaction` (action, target, decision); result status `completed` / `error`; `approval/approve` and `approval/deny` rows. Print mode records no interaction: the gated step is `canceled`, without error text. |
| `1.5.9-interruption` | The cancelled tool (step 2) and the upstream error (step 8) are absent from the export and read from the store, in step order, in both layouts; each call has one result; the retry completes; a cancelled stream is `interrupted` (stop reason 18), a dropped stream has no stop reason. |
| `1.5.9-background-tasks` | Three launches finishing out of launch order; the killed one is `canceled` in both layouts although the export still says `RUNNING`; its notice is `background_job/stop`, the others `complete`; before any notice only the killed launch is settled in the exported layout. |
| `1.5.9-swarm` | The coordinator is not its own child; three members with registry state (`member`, `added_by`, `phase`, `desired_state`, `wake_mode`, `killed`); `lanes` and `overview` report parked members as `waiting` (`killed` for `gamma`) with `post_message.text` `last_text` fallback; `swarm add` call IDs per child (`spawned_by_tool_use`) and `/coordinator/…/<member>` `agent_path` (`gamma` -> `beta`'s call ID and `/coordinator/beta/gamma`); board wakes are never returns or prompts; each worker's prompt is `Origin::Subagent`; the coordinator is every worker's native parent. |
| `1.5.9-subagents` | Idle notices from the system sender are the children's returns (`returned_child`, delivered body is the reply, `lanes` status `completed`); spawn call IDs (`spawned_by_tool_use`); child prompts are `Origin::Subagent` and child titles are their briefs. |
| `1.5.9-prompt-media` | An inline prompt image is read from the row when the writer's stored copy is gone; linked files are scope items; an audio block stores nothing. |
| `1.5.9-compaction-resume` | A conversation resumed from its store alone re-creates its export from step 7; steps 0-6, the compaction boundary (`checkpoint_details`) and its summary included, are read from the store; `/compact` is the prompt text; root and ID are unchanged. |

Every public consumer was run once on each capture in each layout (`ls`, `show`,
`show --json`, `--kind affordances`, `--tree`, `--subagents`, `export` md and json,
`doctor`, `distill`, `events`, `lanes`, `tools`, `index`, `search`, `formats`):
353 runs, none failed. Text from store-only steps is found by `cv search` through
the exported layout.

What these captures establish about the writer, as distinct from the reader:

- The exported log omits cancelled tool steps and upstream error steps, restarts at
  the resume point when its directory was removed, and never revises a step after
  writing it. The store has the latest revision of every step.
- A kill leaves three records: the `manage_task` result `Task "<id>" cancelled.`,
  the launch step revised to `CANCELED`, and a low-priority `was canceled` notice.
- A child's reply reaches its parent as `Subagent <id> has gone idle…` from the
  system sender. A swarm worker's final reply reaches nobody; only board posts do.
- A board wake's `sender` is a member ID that is not reliably the poster.
- A helper a worker adds has the coordinator as its native parent; `added_by` in the
  registry is the only record of who added it.
- `phase` is `RUNNING` for every member that is not killed. The words `waiting` and
  `working` appear only in the `swarm status` tool result.

Added with the fleet-measured reconciliation (`tests/antigravity_reconcile.rs`,
`antigravity_native_shapes.rs`, `antigravity_roots.rs`; shapes and counts from 501
real stores, all text synthetic) and on the captures:

| Contract | Check |
|---|---|
| Rendered export | A result under its header, a tagged prompt and an exported `error` verify their native steps; a body that is not a line-aligned block does not. |
| Native-only steps | Head, interior and tail steps are read in step order beside verified steps; beside a divergent revision or an unreadable store the read reports `unreconciled_history`. |
| Cleared rows, typed results | A row cleared to `Step.type` or `steps.step_type` keeps kind and recovers SQLite side-column metadata/permissions/task/render cells; all 17 `exa.cortex.core.tools.*` `Any` payloads and all 19 direct `1.0.x`–`1.5.13` `gemini_coder.Step` variants reach `details.payload` / IR without unread fields; every `InvokeSubagent` child links to its call. |
| Sidecars & 1.5.13 summaries | `conversation_summaries.raw_summary` (`CascadeTrajectorySummary`), `parent_references`/`trajectory_meta` fallbacks, `.pbtxt` annotations, `<USER_SETTINGS_CHANGE>` model fallbacks, and `swarm/board.jsonl` killed-worker title and `last_text` recovery compose across discovery, forests, and `lanes`. |
| Removed log, mirrors | A session whose discovered log disappeared reads from what survives; mirrored roots yield one session. |
| Bounded reads | Discovery and a header read stay under 4 MiB beside a 16 MiB `gen_metadata` row; id, tool and text sinks stay under 8 MiB beside a 24 MiB image; a history stages in a fixed number of files. |
| Forest call, title, upload, accounting | Capture tests in `antigravity_writer.rs`: the forest names the stored spawning call for direct `invoke_subagent` and `swarm add` children while child sessions carry `spawned_by_tool_use` and `agent_path`; a resumed session is titled by step 0 in both layouts; a missing exported upload is read from the store; none of the 8 real-writer captures reports `unsupported_native_fields`. |

Still requiring dedicated writer-generated scenarios and independent assertions:

- Audio/video prompt payloads; lazy output resolution after the source changes.
- Writer-generated structured generic results and render-only payloads after native
  clearing. Across these captures every step type is one the reader interprets, and no
  writer capture used the generic `struct`/`value` fields (10, 11) that synthetic
  `antigravity_fidelity` tests verify (`details.result`). 16 of 33 capture results carry
  a typed `Any` (field 6: `RunCommand`, `ManageTask`, `InvokeSubagent`); scalar specs
  for `DefineSubagent`, `GenerateImage` and `AskQuestion` are also decoded into
  `details.payload`.
- Renamed swarm members and a worker captured mid-turn (`wake_mode` while working).
  Parked members in `1.5.9-swarm` report `waiting` in `cv lanes` and `overview` from
  turn-boundary state and `swarm status` notices, and direct/helper `swarm add` calls
  populate `spawned_by_tool_use` and `agent_path`.
- Native `CLEARED` rows from the writer; writer-driven non-permission interaction kinds
  (`ask_question` is decoded on `Tier::Rich`+ in synthetic fidelity tests, not yet driven
  through the real writer).
- Full/compact/chunk/native promotion during a live read.
- Giant native cells, malformed length limits and portable reader equivalence.

A successful index pass establishes neither payload completeness nor native
round-trip emission. Any uncovered or unrecoverable content must remain visible
through the shared read-coverage contract.

### Inline media allocation boundary

The native protobuf reader and base64 projector use bounded buffers, but public
image payloads are not lazy IR fields: `Block::Image.data_ref` is an owned string.
An explicit inline image therefore needs approximately `4 * ceil(bytes / 3)`
bytes plus its data-URI header, with additional copies possible during collection
or JSON serialization. RICH/COMPLETE reads, selected public `/messages` windows
with either `extra=false` or `extra=true`, and requested render/export reads retain
that data in their IR. Markdown still renders its existing `_[image]_` marker;
it does not serialize the image bytes. Generic LEAN reads are not silently changed
to image placeholders.

A span read whose consumer explicitly requests reducer facts keeps the image
block and MIME with the existing `base64:inline` marker. Counting, API head reads,
FTS/trigram ingestion, exact verification, and live text snippets advertise that
reducer demand; internal USAGE/FACTS span reads also avoid decoding the inline URI into a string. This is a consumer distinction,
not a promise that every LEAN or span read has bounded media allocations.

`antigravity_media_budget` exercises the complete native SQLite-to-counting path
with a 24 MiB inline image, requiring less than 8 MiB additional peak live Rust
allocation while preserving its image count. It also checks real data URIs through
public API windows and inline parses, and preserves Markdown's existing image
marker. The allocator excludes
SQLite/C allocations, mappings, and stacks. Lazy first-class media payloads would
require a separate IR design; they remain outside this release's boundedness claim.
