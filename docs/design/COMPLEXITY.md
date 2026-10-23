# Complexity: what each path costs, and the no-mode-switch rule

The 0.18 complexity audit, done by reading the code. Use this page to check a change: find the
row for the path, and make sure its cost after the change is still what the row says.

Variables: s is the session's size, n the number of sessions, S the store's size, k the
sub-agents, e the events, and L the lane-ledger rows. "One change" means one session appended
or rewritten. Paths are relative to `crates/`.

## The rule

1. **Cost is set by the question.** A listing is O(n). `tools --across` is O(messages it
   folds). One change rereads exactly the session that changed, so its cost is O(s) or the
   appended suffix. Nothing is O(n²), O(n·k), O(k²) or O(n·S) unless the question itself is.
   Fork inference over siblings is not inherently n².
2. **Shared state is read once per store state.** Something one file, table or directory
   holds is parsed once per `source::FileId` (or per store state) and then reused. It is never
   reread inside a loop over sessions. See `lanes::read_ledger`, `lanes::ledger_index`,
   `claude::configured_roots` and `fork::head`.
3. **No mode switches.** No cache or fast path may stop applying past N bytes or N items while
   the work silently becomes more expensive. Every threshold falls into one of three classes:
   - **budget**: bounds the work and says so in the output or on stderr. Fine.
   - **safety cap**: bounds memory. Fine only if the fallback is also bounded.
   - **mode switch**: silently changes complexity. Not allowed. Replace it with an
     always-on structure, or with a budget that reports itself.
4. **Gate a fix with a test of its shape**: one change produces one reread, or a counter
   assertion. Byte identity is shown by `tools/dump-hash.sh` base vs branch.

## Cost per path, as landed

| Path | Cold | Warm | One change | Set by |
|---|---|---|---|---|
| `discover_all` | O(S_meta + n) | O(n) stats + scan-cache hits | O(n_h) refresh reads, O(changed rows) writes | `lib.rs` discover; `catalog/order.rs` |
| `sessions()` / `cv ls` | as above | O(n) probe + rows, O(n log n) sort | as above | `catalog.rs` `probe_stale_in` |
| Claude discovery | O(S) | O(n) | O(appended) through the continuation | `claude.rs` `cached_metadata` |
| Codex discovery | O(S) + zst decode | O(n) | O(appended) through the continuation | `codex.rs` `cached_metadata` |
| Claude sub-agent forest, any demand | O(Σ s_k) | O(k) stats, in any later process | O(appended) for the changed agent | `claude.rs` `agents_in_dir`, saved by `subagent_tree_inner` |
| Codex hook forest | O(L) index + that forest's rows | same | same | `codex.rs` `hook_forest`, `lanes::ledger_index` |
| Edit guard, per edit | O(touching sessions × hops) lookups | same | same | `hooks.rs` `lineage_root` |
| `lanes_of` / `cv lanes` | O(the parent's rows + Σ s_k) | same | same | `lanes.rs` `lanes_of` |
| `cv index`, unchanged | — | O(n + dependencies) stats | O(n) plan + one session read | `fts.rs` `PlanState`; `sweep.rs` |
| `cv index` sweep queue | O(Z log Z) for Z groups | — | — | `sweep.rs` `Queue` |
| `cv search` | O(postings) + `limit` snippets | same | + suffix read (all; `--live-bytes` bounds it) | `cv-search` `search_suffixed` |
| `stats --tokens` | O(S) | O(n) cache loads + a bounded sweep | one source reparsed | `stats/usage_cache.rs` |
| Sessions API page / head | O(start) / O(s) | O(page) from its continuation / a hit | O(start) / O(s) | `api/read.rs` |
| `cv scry` / watch poll | O(n) stats per tick | O(n) | O(appended) from the anchor | `watch.rs`, `watch/read.rs` |
| MCP `observe_stream` | O(n) stats | same | O(appended); a cursor from another process rereads each changed session once | `watch.rs` `Readers` |
| `cv watch` archive | O(S) | O(n) baseline checks + 2n stats | O(appended) | `watch.rs` `offer_sources`, `archive/reuse.rs` |
| `events ingest` | O(S) | O(n) | one reparse | `events.rs` `ingest_all` |
| Workflow listing | O(r) state parses + r stats | same | same | `claude_workflow.rs` |
| `agent-…` lookup, workflow by name | O(n) catalog rows + a sidecar readdir per session | same | same | `lib.rs` `workflow_sessions` |
| `show --tree` | O(e) rows, indented by forks above each | — | — | `view.rs` `walk_tree` |
| distill | O(s) | — | — | `distill.rs` (commit set) |
| prune / rewind / emit / dataset | O(s) per session | — | — | linear passes, hashed sets |
| Task log, model readers (hook, mutations, list) | O(E) | O(state) fold load | O(state) + the appended events | `task/store.rs` `Fold` |
| Task log, event listers (`events`, `show`, inbox) | O(E) | O(E) | O(E) | `task/store.rs` `replay` |

## Open (structural, not yet fixed)

- **Exports roster cache** (`harness/export/native.rs` `CACHE_BYTES` 8 MiB, 8 entries): a mode
  switch. Past it every conversation read re-parses its whole archive, O(n_a·S_a). Owned by the
  open-cliffs lane (the exports offset index), as are that lane's other items.

- **`agent-…` lookups** read the catalog, not a rediscovery, but still list each session's
  sidecar dir. A by-id index cannot replace that: the scan cache holds an agent only after a
  forest holding it was read, and an agent written since (or a copy) is in no record.

## Surviving thresholds

ember's rule (0.18): no hidden constant decides what the user gets. A limit is either a flag
whose default is the complete answer and whose effect, when set, the output states; or a
cost-only bound that never changes a result, listed here with its fallback.

Flags, complete by default (the output states a bound the caller set):
- `cv search --limit` 20 (also `limit`): bounds the hits shown, never the count; the footer says
  `N of M matches` and `--json` carries `total` (`X-Cv-Search-Total` over HTTP).
- `--live-bytes` / `live_bytes` (search), else `$CV_LIVE_BYTES`, else config `[search] live_bytes`:
  unset reads every stale session; set, the footer names the budget, its source and what it left
  out (`fts/live.rs` `LiveRead`).
- `show` windows and `--max-bytes`; MCP `show` adds none; `observe_stream` `max_messages`
  (`more_pending`) and `char_cap` (`char_cap`, `truncated` in the output).

- `cv pack --excerpt-bytes` (default 1600; was `MSG_RESOLVE_BYTES`, `EXCERPT_SOFT/HARD_CHARS`)
  and the curation caps `MAX_COMMANDS`, `COMMAND_CHARS`, `MAX_FILES_PER_SOURCE`, `MAX_ROLLUP`: a
  bundle bounded by design, whose `Bounds:` header line names each bound and what it cut.

Derived from an outside limit, and said so:
- Semantic head: the model's input, 512 tokens × the vocabulary's median token length
  (`semantic.rs` `input_chars`); `cv index --semantic` prints it.
- Pack's `CV_PACK_LLM` digest (`llm.rs` `TranscriptLimit`): the model's context window as the
  provider reports it, less `DIGEST_MAX_TOKENS` and the prompt, in bytes (a token is at least a
  byte); `--context-bytes` overrides. The bundle prints bytes sent of the transcript and the limit.
- Hooks: `hooks.rs` `HOOK_READ_BYTES` 8 MiB, from the 100 ms hook latency and ~4.5 ms/MB. The
  compact hook's text names the tail it read; a start-pack row names its commit search span.

Cost-only (results identical; fallback at file:line):
- `api/read.rs` `ENTRIES` 1024 continuations, LRU: an evicted read restarts from the session's
  start (read.rs:194); stderr says so once.
- `source/scope.rs` `RETAINED_BYTES` 16 MiB per namespace (`ENTRY_BYTES` per entry): past it a
  value is used but not retained, reloaded on next use (scope.rs:99, :150); stderr says so once.
  OpenClaw charges `Selection::RETAINED_BYTES` 1 MiB, an admission estimate (openclaw.rs:882).
- `fts/sweep.rs` `IN_FLIGHT_BYTES` / `DECODED_WINDOW` 2 GiB: an oversized job runs alone
  (sweep.rs:68). `decoded_slots` 2 × workers (`available_parallelism`): decoded rollouts handed
  ahead, two descriptors each; discovery waits for a free slot (codex_handoff.rs:129).
  `query/facts.rs` `SOURCE_BYTES` 2 GiB likewise (facts.rs:787).
- `ingest.rs` `DOCUMENT_BYTES` 64 MiB: a larger document is admitted alone (ingest.rs:52).
  `COMMIT_EVERY` 256 docs: at 8× the workers drain for a commit (sweep.rs:409).
- `spill/exact.rs`, `spill/ordered.rs` `MEMORY_BYTES`, `store/bytes.rs` `MEMORY_LIMIT`: memory
  spills to disk with the same complexity (exact.rs:130, ordered.rs:119, bytes.rs:112).
- `store/jsonl.rs` `MAP_WINDOW` 8 MiB, `MAPPED_RECORD` 64 KiB (jsonl.rs:543, :650),
  `document/range.rs` `ENDPOINTS` 1024 (range.rs:66): at most one extra read per traversal.
- `catalog/reader.rs` probe reset every 256 refs (reader.rs:170): one extra stat per 256.
- `usage_cache.rs` `COMMIT_BYTES` 16 MiB / 64 sources: a transaction size (usage_cache.rs:505).
- Chunking and representation, not bounds: `fts.rs` `CHUNK_BYTES`, `CHECKPOINT_*`;
  `lazy.rs` `INLINE_MAX`; `json_frames.rs` `KEY_BYTES` (cv's own field names);
  `dataset.rs` `SPAN_CHUNK`, `deferred.rs` `PREFIX_LIMIT`; `scan.rs` window constants.
- Input guards, never output: HTTP `MAX_LINE` / `MAX_HEAD` (http.rs:275, :313) and the write
  `BODY_LIMIT` 1 MiB (serve.rs:258) refuse a request with a stated 4xx; `DRAIN_LIMIT` decides
  only whether a connection is kept. `sqlite/snapshots.rs` `RECORD_LIMIT` refuses a corrupt record.

Open (not yet decided against the rule; each bounds what the user sees): none. The exports
roster cache (`export/native.rs` `CACHE_BYTES`) is structural, above.

Removed mode switches:
- Watch `METADATA_BYTES` 16 KiB: a completed read's session metadata was kept only under it, so
  every later bounded read of a session with more went on to EOF. It is kept whatever its
  size, one per followed session, the metadata the read already held.
- Codex `FULL_SCAN_CAP` 8 MiB: past it a listing took title, cwd and times from 1 MiB head and
  tail samples. Discovery now reads every record once and resumes a grown rollout.
- API read cache: 32 entries FIFO, 256 KiB per entry and 2 MiB in all, past which a session
  silently paid O(start) per page.
- MCP observe cursor: reader state for eight sessions under 64 KiB each. It is now one state
  per session in the serving process (`watch::Readers`), and the output's `rereads` counts the rest.
- Usage cache: the FIFO eviction at 256 MiB / 65,536 sources, and the 16 MiB per-source limit.
- Watch: the 64 KiB anchor limit.
- Archive: the 1 MiB resume-state limit.
- The `.zst` whole-guard charge.
- The shared inventory budget.

Removed result limits (0.18, ember's rule):
- `events.rs` `TARGET_MAX` 300 / `DETAIL_MAX` 200: an event holds the whole command and error
  text (a lazy one read from source); only the table shortens a cell, with `…`, unless `--wide`.
- `opensession.rs` `SNIFF_BYTES` 64 KiB: a plain `*.json` is a document when its top-level object
  has the version key, found by a streaming pass over top-level keys that stops at it.
- Search: the 256 MiB default `LIVE_BYTES` (the live index is now on disk, its heap one arena),
  `LIVE_HAY_BYTES` 256 KiB per `--scan`ned session, `PERSISTED_INDEX_CAP` 1 MiB per indexed
  persisted output, `SNIPPET_SCAN_CAP` 256 KiB: all now stream through bounded windows.
- `EMBED_HEAD_BYTES` 64 KiB; the hooks' three read constants and `START_PACK_MAX`; `show`'s
  `PIPE_GUARD_*`; MCP `SHOW_DEFAULT_*` and `observe_stream`'s 50 / 16,000 defaults.
- `usage_cache_floor` 64 KiB: a CPU trade by size; a small source caches as one small record.
