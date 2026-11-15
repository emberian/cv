# Antigravity coverage contracts

These independent acceptance checks define the native reading guarantees for
0.20. The outstanding scenarios below remain separate from those guarantees.

`tests/antigravity_lifecycle.rs` uses synthetic protobuf encodings derived from the
installed 1.5.9 field descriptors. It deliberately avoids sharing the production
encoder/decoder, and it is not a harness-written corpus version. Every test uses
an isolated CV home and owned SQLite files. Private conversation content and
extracted descriptor sets are not repository fixtures.

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

The existing `installed-1.5.9-lifecycle-2026-11-14` corpus contains scrubbed
harness-written full-log ranges covering duplicate running steps, parallel
completion order, and revised step indices. Its label identifies the installed
CLI at capture, not a proven writer version for every historical row. It does
not cover the native-only, media, or broader swarm scenarios below.

Still requiring dedicated writer-generated scenarios and independent assertions:

- Permission requests and their responses; interruption, cancellation and retries.
- Inline image/audio/video payloads, linked attachments, missing attachment files,
  and lazy output resolution after the source changes.
- Structured generic results and render-only payloads after native clearing.
- Multi-child spawns, nested/renamed swarms, peer-board versus final-result delivery,
  and waiting versus actively running workers.
- Full/compact/chunk/native promotion during a live read; native DB-only resume roots.
- Giant native cells, malformed length limits and portable reader equivalence.

A successful index pass establishes neither payload completeness nor native
round-trip emission. Any uncovered or unrecoverable content must remain visible
through the shared read-coverage contract.
