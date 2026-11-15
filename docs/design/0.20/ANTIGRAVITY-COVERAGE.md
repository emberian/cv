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

Still requiring dedicated writer-generated scenarios and independent assertions:

- Permission requests and their responses; interruption, cancellation and retries.
- Inline prompt image/audio/video payloads, linked attachments, missing attachment files,
  and lazy output resolution after the source changes.
- Structured generic results and writer-generated render-only payloads after native clearing.
- Multi-child spawns, nested/renamed swarms, peer-board versus final-result delivery,
  and waiting versus actively running workers.
- Full/compact/chunk/native promotion during a live read; native DB-only resume roots.
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
