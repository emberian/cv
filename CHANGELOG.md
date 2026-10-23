# Changelog

## Unreleased

- Search's live-read budget defaults from `$CV_LIVE_BYTES` (bytes or `all`), then config `[search] live_bytes`; `--live-bytes` overrides; the footer names the source.

## 0.18.0 — 2026-10-23 — the release an agent can trust
ClusterVision 0.18.0 is the release an agent can trust. Every listing, count and search answer
is either exact or says what it skipped: counts are prompts and replies on every harness, a
session the store has not counted shows a dash, and search reads what the index has not seen.
One stored-format version replaces five; upgrading reads every session once. A write to a store
costs the session it touched, not the store: SQLite stores carry per-session certificates and
transcripts resume from their last mark, so `cv index` is cheap enough to run hourly or not at
all. No hidden limit decides what you get; the bounds that remain are flags you set or caps that
never change a result. Removed names exit 2 with a pointer; stored logs keep reading. Hooks cover
Codex as well as Claude, and `cv doctor` reports a harness release cv has not verified.

**Breaking: commands, MCP, HTTP and JSON.** A removed name exits 2 with a pointer to its replacement.
- `cv search --json` (and MCP `search`) prints `{"hits": [...], "total": M}`, not an array; the table footer reads `N of M matches`.
- `/api/search` adds `X-Cv-Search-Total`, every session that matched; its note says `N of M matches` when `limit` cut it.
- `cv events --json` (`/api/events`, MCP) holds whole commands and error texts, newlines kept; the table marks cuts with `…`, and `--wide` prints them whole.
- `cv search --scan` (`scan=1`, `cv pack` without an index) matches each session's whole exchange, streamed, instead of its first 256 KiB.
- `cv index --semantic` reads each session as far as the model embeds (512 tokens × median token length) and prints it; the 64 KiB head is gone.
- The full-text index holds a persisted tool output whole, not its first MiB, and snippets find a term anywhere; `source::VERSION` 9 rereads once.
- Hooks read at most 8 MiB of transcript and say what they left unread: `compact` names the tail it read, `start` a row's commit search span.
- `cv show` on a pipe prints the whole transcript (no 200 KB head+tail guard); MCP `show` adds no default `last`/`max_bytes` window.
- MCP `observe_stream` returns everything appended by default; `char_cap`, when given, is stated with a `truncated` count.
- Cursor counts only bubbles a read keeps (capability markers dropped); a composer with external bubble cells counts at `cv index`.
- `source::VERSION` is 8: listings rediscover once, so rc.2's cached Cursor counts are recounted.
- `message_count` counts prompts and replies only (no tool results or sub-agent returns) and is `null`, not `0`, until counted: `ls --json`, `/api/sessions`, search rows.
- `cv archive ls` counts turns like `cv ls`; `catalog.jsonl` keeps the raw count as `messages_written`; 0.17 rows show a dash until resynced.
- `extra.openclaw` keys keep native camelCase (`stopReason`, `customType`, `error.errorCode`); `custom_details` and `branch_from_id` become `details` and `fromId`.
- An IR or OpenSession 0.3 message without `kind` or `origin` is refused, not inferred from its role.
- Fidelity-report `deltas` and `cv formats` findings are `{what, a, b, expected, where}`, replacing `{field, before, after}` and the old finding keys.
- MCP tools omit `--follow`; a `follow` argument is refused, naming `for`.
- `cv scry` on a pipe prints the sessions it would follow and exits 0; `--follow` follows, `--for 30s|5m|2h` bounds it.
- Watch and `observe_stream` cursors are version 2; a 0.17 cursor starts a fresh baseline.
- A tag-only `decision` (pre-0.15, nothing posed) reads as an assigned action, not a decision owed; the page's tagged-decision card is gone.
- `$CLAURDVOYANT_HOME` and `$CLAURDVOYANT_MAX_STALE_SECS` exit 2, naming the `CLUSTERVISION_` variables; `claurdvoyant` directories are no longer read.
- Renamed: `rewind --from-compaction` (the default; `--full` for everything), `deferrals --since` is `--at`, `dataset --out` is `--file`.
- `cv tools --agent` takes an exact id or unique prefix, else exits 2; `--across` with `--agent`, `--tool` or `--timeline` exits 2.
- An unknown `cv task events --kind` (`/api/events?kind=`) exits 2 (400) listing the known kinds, instead of listing nothing.
- `cv workflow` exits 2 on flags it dropped: run-only flags without a run, `--revive-all` without `--revive`, `--revive` with `--json`.
- `-q text:` without an index reads live; past `--live-bytes` or under `--frozen` it exits 2 naming `cv index` (`/api/stats`: 503).
- `cv events`, `cv touched`, `cv blame` and their HTTP routes fail on an event-catalog error instead of answering empty.

**Breaking (library): cv-core, cv-search and cv-testkit.**
- `ParseOptions`/`ExtraProjection` → `Demand { tier, spans, offsets }`; `MessageSink::extra_projection` → `tier`; the five `Watcher` poll methods → `poll`/`try_poll(Poll { .. })`.
- `Adapter::resume`/`try_resume`/`resume_format` → `Adapter::transcript(r, demand, Start, sink)`; `append_only` returns `bool`.
- `Adapter::discover` returns `Discovered { refs, skipped }` (out-of-tree: `Ok(refs.into())`); `cline::discover_in` returns it, `cline_sdk::discover` fills `&mut Discovered`.
- `Watcher::run` is gone (callers schedule `poll`); `poll_sources` → `offer_sources`.
- `Text`: over 4 KiB is `Text::Source`; `Span` is wire form only; malformed escapes and unresolved formatting error; `matches_str` and `is_lazy` replace equality and `is_span`.
- `lazy::json_string_span` is gone; `try_resolve_prefix` budgets decoded bytes (`max_raw` → `max`); `lazy::read_head` → `read_head_checked` (`Result`).
- `ir::Head` → `projection::Head` (separate byte and character budgets; `push` returns `true` when full); `Counted::new` builds `Counted`; `hashing()` captures SHA-256.
- `emit::Delta` and `formats::Finding` → `emit::Divergence {what, a, b, expected, at}`; `FindingKind::ManifestInvalid` replaces `source_file_missing` for unparseable manifests.
- `SubagentInfo`/`claude::subagent_refs` → `ir::AgentRef`/`subagent_tree_of`; `claude::CARRIER_KEY` → `ir::CARRIER_KEY`; `SubagentEnd`, `ApiErrorNotice`, `TaskNotice` move to `lanes`.
- Return `Result`: agent end, return, task-notice and workflow-launch reads; forest and subagent-reference APIs; `one_agent_across`; `wire::openai::tool_calls`; `store_affordances`; `json_files`.
- Return `Result`: `codex::parse_str`, `gemini::parse_all_str[_for]`, `ingest::ingest_files`, `events::{events_for, sessions_touching, sessions_editing, edits_touching_file}`.
- Renamed modules: `handoff` → `codex_handoff`, `loom` → `splice`, `harness::harmony` → `harness::wire::harmony`; `decode_content` takes a call-id generator (`ir::derived_call_id`).
- `export::{ChatGptExport, ClaudeExport}` are gone: `harness::for_harness_with_root(Harness::ChatGptExport, dir)`.
- `affordance`: `classify_tool`, `items_of`, `item_of` → `classify_tool_json`/`_view`, `items_of_view`, `item_of_view`; `Returns::ResultInPrompt` is new (Roo's `new_task`).
- `affordance::{unwrap,rewrap}_deferred` take a `DeferredWrapper`; `wire::anthropic`'s owned `Texter`, `block_from*` and `blocks_from` are gone.
- `distill::distill` → `prepare_distill` + `distill_pack`; `Session::first_user_text` and `searchable_text` are gone.
- `SearchRow::to_json` → `to_json_with(&SearchRowContext)`; `scan::live_search` → `live_search_filtered`.
- Redaction and HTML rendering take a resolver (`share_message(.., resolver)`); Markdown uses `write_md_message`/`write_md_block`; `scrub_value`/`scrub_cow` are private (`redact_with`, `redact_message`, `redact_text`).
- `RedactOptions.keep_emails` → `classes.emails`, inverted; `task::AppendOutcome.replay_warnings` is gone.
- `source::Published` → `Record<T>` (`Table<T: Payload>`, `admit` replaces `published`/`fresh`); `source::rename_consumer` is gone; `catalog::fill_count` is gone.
- `resume::version` returns `String`; `resume::MarkUpdate` is `{session, mark}`; `publish::retract` is gone (`publish::Set` owns rollback).
- New fields break exhaustive literals: `EmitOptions.parsed_at`, `PruneOptions`/`RewindOptions.skills_policy` and results' `skills`, `Stats.uncounted`, `HarnessCensus.first_error`, `LaneWork.unread`.
- `EmitOptions.sidechain_of` is `Option<SidechainOf {root, agent}>`; `prune::prune_session` takes the `Harness` first.
- `events::ingest_all` takes `IngestOptions`; `query::WindowPresence` is gone (`filter_refs_limited` drops it); `query::HAS_FLAGS` rows are `(name, help, Cost)`.
- `lanes::Lane.status` is `LaneStatus`, `LaneEvent.event` is `LaneEventKind`; `STRAND_PATTERNS` is private (`text_is_waiting`).
- `ForestTools::for_workflow` → `tools::forest_tools_selected(r, None, Some(run_id), None)`; `task::store::append_agent_event` → `TaskStore::default_store().append_agent_event`.
- cv-search: `text_search*`, `enrich_text_snippets`, `semantic_search_filtered` → `Searcher::{search, search_in}`; `embed_all` → `embed_sessions`; `Searcher::semantic` is private (`Searcher::from`).
- `fts::enrich_text_snippets` returns `Result<Option<String>>` (a degradation note).
- cv-testkit: `heap::begin`/`end` return `Measurement`; `Env::enter_isolated` returns `Isolated(armed, Scope)`; `claude::user(sid, uuid, ts, text)`.

**After upgrading, once.**
- `source::VERSION` is 7: the first listing rebuilds the catalog (said on stderr; `cv index` does it ahead) and every mark, checkpoint and watch state.
- The first `cv index` rebuilds the full-text index whole.

**Listing and counts.**
- A plain `.json` OpenSession document is discovered wherever its top-level version key sits, not only within the first 64 KiB.
- A first `cv ls` leaves Codex, Grok, Kimi, Kimi Code, Qwen, Hermes, Goose, Zed, Continue and OpenCode uncounted until a read or `cv index`.
- `cv stats` reports "N not counted yet" (`uncounted` in `--json`, `/api/stats`); `--sort-by messages` ranks uncounted last.
- A count survives refreshes of an unchanged source and is dropped when it changes; rewound or superseded Hermes rows no longer inflate it.
- Kimi, Kimi Code and Grok titles are the recorded native title; Hermes, Goose and Zed list from metadata alone.
- Cursor legacy counts exclude empty and malformed bubbles.
- A listing racing `cv index` waits up to 5 s for it instead of discarding its update.
- `created:` without a recorded creation time no longer matches on update time.
- `ls --json --asc` with a query selects the same sessions as descending; absent sessions take no window slot; refreshed harnesses keep their tie order.
- `serve`, `mcp` and followers see Gemini and Qwen projects registered after they started.
- A read-only catalog still serves warm listings; builds without `sqlite` no longer spool unpublishable events.
- Semantic indexing caps transcript heads at 64 KiB of decoded text.
- `cv search`, `/api/search` and `-q text:` also read what the index has not seen, within `--live-bytes` (256 MiB); `--frozen` reads none.
- `cv search` without an index answers from that live read instead of exiting 1.
- Search staleness counts each session's preferred copy, not older duplicates.
- Cold SQLite snapshots stage beside the catalog instead of in a multi-gigabyte tmpfs copy.
- MCP `observe_stream` keeps every followed session's reader state server-side and reports `rereads`.
- Codex rollouts over 8 MiB list the title and cwd a full read gives, not a head-and-tail sample.

- An append or rewrite rereads only that session: Claude forks, Grok cwd groups and Kimi Code sub-agents no longer reread; Codex rereads appended records.
- A store write rereads only changed sessions: Hermes, Goose, OpenCode, Devin, Zed, Cursor, OpenClaw, Cline SDK databases, Gemini and Qwen `logs.json`.
- Discovery rescans only changed sources: Kimi, Kimi Code, Goose legacy, Cline, Roo, Continue, OpenSession files and OpenClaw stores.
- Sub-agent trees (`show --tree`, `--subagents`, lanes) reuse unchanged agents' saved scans; Codex, Hermes, Devin and OpenClaw read their registries once per state.
- `cv index` and live search read only what was appended since the last index; compressed Codex rollouts are read in parallel.
- One changed session rewrites only its catalog row; `cv show agent-…`, workflow-name and ghost lookups read the catalog, not rediscovery.
- The prompt hook, task mutations, `cv task list` and `GET /api/tasks` replay only task-log events appended since a stored fold.
- The sessions API resumes its 1024 most recent sessions of any size; `cv stats --tokens` keeps every live session's cached usage.
- Watch checkpoints and archive resume states of any size are kept (past 64 KiB or 1 MiB they were dropped).
- A WAL-backed SQLite copy happens once per command; an export archive past the 8 MB cache is scanned once.
- `cv show --tree` indents once per fork above a message, so a straight chain prints flat.

**Reading.**
- `cv index` holds a few open files per worker, not two per compressed Codex rollout, so a cold index stays under macOS's 256-file limit.
- `cv search`, `/api/search`, MCP search and `-q text:` read every session the index has not seen; `--live-bytes` (`live_bytes`) now only bounds it when given.
- Global `--frozen` (`CV_FROZEN=1`) reads without updating cv stores, streaming stale events; commands that write stores or sessions exit 2.
- Boundaries carry no invented text (Claude, OpenClaw, Goose, Hermes, Kimi, Kimi Code, Qwen); `show` labels them by kind and marks inferred ones synthesized.
- Cursor and LM Studio (Harmony) tool calls without native ids get `cv:<format>:<position>`.
- Codex: tool results carry only recorded status; usage-only records are notices; `[error] ` outputs stay verbatim unless cv wrote the rollout.
- Zed and Goose report the recorded model; the provider (`extra.zed.provider`, Goose metadata) never substitutes for it.
- Gemini, Qwen, Goose, Grok, LM Studio, OpenClaw and OpenCode report the model on every turn that records it, windowed reads included.
- `cv distill` and `cv adopt --model` default the agent type to `general-purpose` only when the source names none.
- A source replaced or deleted mid-read is an error, checked after slow callbacks, per mapped window, at checkpoints and at EOF.
- Native Codex file-change records, code-mode edits included, feed `cv touched`, tool histograms and query facts without duplicating patch calls.
- Qwen checkpoint facts live under the `qwen` bag, not `gemini`.
- Compressed Codex rollouts need the `zstd` feature; without it they are unreadable, not parsed as JSONL.
- ZIP uploads over 10,000 entries are refused.
- `cv blame -L` says when its commit list was cut and marks an empty message `(no message)`; `cv show` hints spell sub-agent ids in full.
- Claude Code 2.1.304 is the verified writer: request-ledger, `isolation-latch` and unknown records are carried, never read as turns.
- A Claude fork sub-agent's `fork-context-ref` sets `lineage.forked_from` and `extra.claude.fork_context`; a retried request's `api_error` renders as an error.
- Claude fork inference counts only a copied first prompt or boundary, not a SessionStart hook's earlier timestamp.
- A workflow agent turns stale when its session's `tool-results` gains spilled outputs.

**Titles and affordances.**
- `has:subagents` matches the forest `agents` counts: Claude workflow agents and Codex and Devin forests too.
- Kimi, Kimi Code and Hermes sub-agents join the forest; `dataset --subagents` reads every harness's forest.
- Emitted sessions' fallback title is the `cv ls` title; Cursor CLI titles drop the leading `<timestamp>`.
- `has:skill` and `--kind affordances` count a Claude skill loaded by slash command, once per activation.
- Kimi Code: the latest `TodoList` is a `plan/state` row; legacy `Agent`/`AgentSwarm` calls are spawns, their results returns.
- Roo: a `new_task` result beside user text is a `subagent_return` detail; the turn stays a prompt.
- Tool-filtered histograms keep edit counts; native edits reconcile by call and resolved path, never guessing relative paths in compound shell commands.

**Reshaping.**
- `cv pack --excerpt-bytes N` (default 1600) bounds each excerpt message; the bundle's `Bounds:` line names every bound and what it cut.
- `CV_PACK_LLM` digests send up to the model's provider-reported context window (was 60K chars), or `--context-bytes N`; the bundle states bytes sent and the limit.
- `cv port --strict` help names the actual `⚠ lossy:` output marker for losses the target format cannot hold.
- `prune`, `rewind` and `port` take `--skills keep|drop|drop-stale` (default `keep`), legacy command-file skills included.
- `cv prune`, `cv rewind`, `cv adopt` and Claude ports check the staged session resumes before publishing; else they refuse and leave nothing.
- `cv prune`, `cv rewind` and adoption refuse a non-JSON-record line; `cv prune` refuses an empty transcript; minted ids are UUID v7.
- `cv rewind` copies to the end of the file (tail records like `cost-state` were dropped).
- `cv rewind` at a cut keeps parallel calls, closes open ones, drops a malformed later line, and asks for a rerun on an unterminated one.
- `cv rewind` refuses a bodyless turn only with `--thinking portable`; a native copy keeps it.
- `cv port` to Codex writes the source's `cli_version`; to OpenClaw and LM Studio, no invented `usage.totalTokens` or `tokenCount: 0`.
- `cv port` to Gemini derives message ids (UUID v5) from session and turn, so re-emits match; to Continue, a tool turn without result is refused.
- Gemini, Grok, OpenClaw, Cline, Roo and Continue emits stage their files, publish the transcript or index last, and roll back on failure.
- Staged emits are not crash-atomic: a crash can leave a published prefix; a failed rollback is reported.
- OpenClaw port and emit refuse an unreadable or concurrently changed `sessions.json`; an unwritable Gemini `projects.json` fails the emit.
- Hermes and OpenCode exports reject unreadable sources before creating any output; `--force` restores a replaced sidecar when the transcript fails.
- Claude, Gemini and Qwen resume hints are one runnable line: `cd <dir> && <harness> --resume <id>`.
- Qwen emission keeps absent cache counts absent; OpenCode keeps an unknown cwd empty instead of cv's own.
- `cv splice --generate` rejects an empty completion; `pack --json` returns content, artifact paths and resume hints; `cv pack` names `LMSTUDIO_API_BASE`.
- A refused write names the failed JSONL record, or the OS error behind a refused exclusive rename.

**Doctor and hooks.**
- `cv doctor` adds Writer versions: the newest Claude and Codex versions seen versus the newest verified, plus unknown sessions.
- `cv hooks install codex` (user or `--scope project`) installs Claude's five hooks: compaction recovery, start context, lanes, decisions, edit guard.
- `cv hooks install|show codex` and `cv doctor` report each Codex hook as trusted, untrusted or modified, with Codex's review step.
- Codex's edit guard denies a conflicting edit (Codex has no `ask`), but not one over a file the same root thread's agents edited.
- `cv hooks` and `cv hook --harness` accept every `--harness` spelling (`cc`, `claude-code`); `cv lanes --recent N` reads Codex sessions too.

**Waiting commands.**
- Every waiting command says on stderr within a second what it waits for; on a pipe, each exits within 5 s or announces itself.
- `cv workflow --follow` takes `--for <duration>`, then renders the run as it stands.
- `cv hook` with a terminal on stdin exits 2; `cv mcp` and `cv task … -` say what they read.
- Followers announce a session unreadable at start once it reads, instead of guessing its message count.
- `cv events` keeps every extracted event (the per-message `(kind, target)` dedup is gone).
- A follower's byte mark on a compressed Codex rollout is unresolved, not resumed from a decoded prefix.

**Removed.**
- `cv watch --interval` takes seconds like `scry`; a negative `--interval` exits 2.
- An unknown `cv pack --format` exits 2 naming `md`, `prompt`, `session`.
- Mentions of a leftover `index.sqlite` or pre-0.9.12 `discover.json` are gone; delete either by hand.
- MCP errors for removed properties name their replacements.

**Skips: counted, reported, or warned once.**
- `cv index` defers a session whose old search baseline cannot be invalidated (catalog unopenable), names it, and completes the pass instead of aborting.
- A source discovery cannot open (unreadable dir, locked or corrupt database, bad export or `sessions.json`) is named by `cv doctor`, not silently empty.
- `cv doctor --json` gains `skipped_sources`; per-file `cv: skipping` lines become one stderr summary.
- An unreadable session file errors instead of reading empty: Continue, OpenCode, Cline, Roo, Devin, Hermes, Goose, legacy Codex `.json`.
- Undecodable records count as `skipped_lines`: Claude (trailing partial included), Cursor, Continue, OpenCode, Cline, Roo, Goose, Zed.
- A catalog, usage-cache, scan-cache or `config.toml` failure is said once per process and retried; nothing partial is published.
- A Hermes compression continuation that cannot be queried errors instead of dropping `continued_in`.
- Search names a hit whose source cannot be read; `--scan` skips unreadable sessions and names them.
- `cv formats` reports each harness's first parse error (`first_error` in `--json`).
- `ls --json --enrich` carries `enrich_error` for an unreadable transcript.
- `cv workflow --revive` says when a lane's read stopped early; an unreadable run-state file is named, not claimed as a ghost launch.
- `cv archive`, `cv pack`, `cv cat` and `cv events --subagents` name their read failures instead of dropping them.
- `cv serve` closes a connection whose body exceeds its Content-Length.

**Tooling and CI.**
- The viewing-and-sharing manual links Compare branches to the renamed Splice composer section.
- `cv schema --json` lists `routes` (every `cv serve` method and path) and the `subagent_row` and `task_row` keys.
- `cv index --json` reports `changed`, `removed` and `errors` beside `indexed`.
- Release gates: 0.17.0 compatibility, 0.16.1 upgrade over every corpus, real Codex 0.162.1 hooks, and macOS nightly and on rc tags.
- Contributors: `tools/size-budget.txt` and `check-size.py --write` are gone (drift is reported); `tools/dump-hash.sh` is the byte-identity gate.

rc.1 tagged 2026-10-22; rc.2 2026-10-23.

## 0.17.0 — 2026-10-17 — stream, never materialize

cv's readers now stream: a giant tool output, a 60 MB escaped string or a 64 MiB SQLite cell
stays in its source file and is read through bounded windows, by one record codec per wire format.
`cv index` parses on every core and decodes compressed Codex rollouts with libzstd. Freshness is
verified on use, one record per consumer, over every primary source, sidecar and WAL, so a quiet
session's append is never missed. Search matches the exchange (what was asked, answered and run);
injected files, system prompts and hooks are `--kind context`. Untitled sessions are titled by
their first prompt. `cv serve` runs on a small built-in server. Measured on a frozen 5,421-session
fleet (Claude 38 GB, Codex 40 GB compressed, 89 GB decoded): a full index from an empty home takes
866 s at a 715 MB footprint, an unchanged index 0.4-0.8 s. No command is renamed or removed; the
table lists the flags, defaults, routes and MCP tool that changed.

| old | new |
|---|---|
| `GET /api/backlog` | `GET /api/tasks?detail=1` (the old route answers 410 with this pointer) |
| `POST /api/task/{id}/reopen` | gone (410): a closed task stays closed; open a new task and note the old id |
| MCP `hook` | gone; a call points at `hooks_install` (it read Claude Code's hook JSON on stdin, so a call did nothing) |
| `cv search` with no usable index scanned transcripts live | an error naming the cause and the fix; `cv search --scan` (`/api/search?scan=1`) asks for the live scan |
| Codex plain rollouts over 8 MiB had extrapolated discovery counts | `message_count: 0` means not counted; compressed rollouts are counted in full; indexing supplies exact catalog counts |
| `cv search` matched injected files, system prompts and hooks | matches the exchange; `--kind context` (`/api/search?kind=context`) for those, `--kind all` for both |
| `/api/search?semantic=1&kind=context` | 400: vectors embed the exchange only |
| `cv port --thinking native\|text\|drop`, default `native`; `cv rewind` copied reasoning byte for byte | `--thinking portable` on `port`, `rewind`, `splice`, `pack`; `port` and `rewind` default `native` when every recorded model is `claude-*`, else `portable` |
| `prune --drop-thinking` left an inline marker per flattened block | omits the block; `cv cat <new-id> <record-uuid>#think<i>` returns it from the sidecar |
| `prune --to <taken id>`; `dataset`, `pack`, `share`, `distill --pack` onto an existing name wrote into it | exit 2, nothing written; `--force` (`prune`, `dataset`, `pack`, `share`, `distill`) replaces by atomic rename |
| `rewind --to` and `adopt` refused a taken target with exit 1 | exit 2; `adopt --force` still replaces; rewind requires another id |
| `cv index` exit 1 when any session was unreadable | exit 0 with warnings when some sessions succeed; all unreadable still fails; `--strict` fails on any; `--json` lists `unreadable` |
| `cv index --semantic --subagents` | a usage error before anything is written: run the two separately |
| a pre-binary `embeddings.json` semantic store | not read or migrated; `cv index --semantic` rebuilds |

`cv tree`, `cv prompts`, `cv compaction`, `cv timeline`, `cv board` and the other 0.15 names still
exit 2 with their pointers; the `cvd` and `cv-mcp` shims went in 0.16.0.

- Inbox table headers no longer cover the first task row, restoring clicks to expand Backlog task details.
- `cv index` publishes search freshness baselines per session: one row's failure is reported and reread alone, no longer losing the sweep's.
- One freshness record per consumer and session, verified on load (counts, events, search, usage); watches record file identities. Each rereads once after upgrading.
- **Breaking:** `GET /api/backlog` is removed (410 pointer); the inbox page reads `/api/tasks?detail=1`, which adds the backlog columns.
- **Breaking:** `POST /api/task/{id}/reopen` is removed (410): a closed task stays closed; open a new task and note the old id.
- **Breaking:** MCP no longer offers `hook` (it reads Claude Code's hook JSON on stdin; a call did nothing); calling it points at `hooks_install`.
- `cv search`: when live snippets cannot be generated the indexed previews stand in, with a note, as `/api/search` already did.
- `/api/tasks/inbox/{who}` takes `since=` and `all=` like `/api/inbox`.
- `cv archive sync` takes its inventory from the index sweep (one corpus walk, the index refreshed on the way); a home whose index cannot be written falls back to discovery.
- A live `.jsonl` beside its writer is admitted by content (same file, sampled prefix intact), never by waiting; index, events, counts, vectors reread once.
- Pre-binary `embeddings.json` semantic stores are no longer read or migrated (querying them already failed); `cv index --semantic` rebuilds.

- HTTP rejects duplicate Host headers before routing requests.

- HTTP search rejects conflicting kind, scan and semantic parameters consistently with the CLI.

- Reject premature EOF in positioned JSONL reads instead of treating missing captured bytes as a partial final record.
- Preserve concurrent directory claims during fallback publication and restoration; retain both trees when restoration is blocked.

**What reads differently** (`--json` consumers):
- **Codex `message_count` counts logical turns**, as the parser does: no duplicated event
  echoes, injected instructions or inherited history. OpenCode, Goose, Hermes, Devin and Qwen
  discovery counts match their parsers too; cached counts refresh once. Plain Codex rollouts over
  8 MiB report 0 until counted, replacing the old extrapolation.
- **An untitled session's `title` is its first exchange prompt**, in index and scan hits, labels,
  Markdown, share and archive, never an injected AGENTS.md or environment turn; `ls --json --enrich`
  `display_title` prefers the row's `title`.
- **Search hits match the exchange.** Injected instructions, system prompts, skill bodies, hook
  output, notices and compaction summaries match only with `--kind context` (or `all`); a fork's
  copied turns hit their source once; snippets keep the original Unicode. Every indexed answer, hit
  or miss, says how many sessions the index trails and by how long: stderr under `--json`, the
  `X-Cv-Search-Note` header on `/api/search`.
- **Claude `rewind` and `port` default to `native` when every recorded model is `claude-*`**, else
  `portable`: plaintext thinking demoted to text, signature-only reasoning removed (no placeholder),
  illegal tool IDs mapped collision-free at both ends. `rewind` reports which. `--thinking native`
  writes what 0.16 wrote.
- `prune --drop-thinking` omits old thinking blocks with no inline marker, removing emptied records
  and relinking their children; `cv cat <new-id> <record-uuid>#think<i>` still retrieves each.
- `cv doctor` and `prune` separate byte estimates from recorded prompt usage and rewritten resume
  metadata; opaque thinking no longer counts as measured reasoning tokens. One estimator with
  per-kind rates calibrated against `count_tokens` replaces 3.5 bytes/token; prune's revive estimate
  anchors on recorded usage and never read more than 1% low against the API. Human labels change;
  `--json` keys do not.
- Session JSON (`show --json`, `/api/session/…/head`, OpenSession) may carry a top-level
  `principal`, a string or an object, absent when unset; a splice keeps it only when every source
  agrees (#4).
- Generated MCP tools return a successful command's stderr (continuation hints such as
  `task_events`'s `next: --since`) as a second content block. Read `content[0]`; do not concatenate.
- `-q`: `a|b` is OR and `field="x"` exact metadata equality; text equality errors naming the syntax
  to use. A harness-qualified id (`claude:abc`) discovers only that harness on a catalog miss; id
  prefixes stay case-sensitive.
- `cv serve` answers 400 to malformed numeric parameters (`/api/sessions?limit=abc` was ignored),
  framing, headers and authorities. Task POSTs reject invalid JSON (400), oversized bodies (413),
  and a non-JSON media type or forbidden origin (403).
  `GET /api/session/{harness}/{id}/export` downloads the complete session as Markdown or OpenSession.
- `ls --json --limit 0` prints `[]` after argument validation without touching the inventory;
  `cv index --json` prints `{indexed, path, unreadable: [{id, harness, reason}]}`.
- `cv dataset` keeps a session-level system prompt without duplicating explicit system messages.
  `cv port --harness opencode` writes OpenCode 2.x rows and can bootstrap a fresh store; 1.x stores
  still read.
- Claude checkpoint counts keep restorable results, so `has:checkpoint` can change after the
  recount. Cursor Agent CLI chat and ACP stores appear as sessions.

**One-time costs after upgrading** (`docs/design/PERF.md`, 2026-10-14, release builds; frozen: an
APFS clone of 5,421 sessions, Claude 38 GB, Codex 40 GB compressed; live: ~7,000 sessions):
- **Run `cv index` first.** The full-text index rebuilds once for the exchange/context split, the
  title rule and per-session metadata guards; afterwards a grown transcript is read from where the
  last pass stopped. A full build from an empty home: 866 s at a 715 MB footprint frozen; 966 s,
  RSS 3.4 GB live. Unchanged afterwards: 0.4-0.8 s frozen, 1.0-1.6 s live.
- The first fleet read rediscovers every harness (the catalog's interpretation version changed) and
  may print a stderr note suggesting `cv index` when the old catalog records its interpretation
  version: `cv ls`, first run, frozen, 158-186 s. Do not pay it inside an agent's MCP call.
- Indexing refreshes counts, events and search baselines together; usage refreshes on its first
  token-statistics read. Affordance counts (`has:`) are recounted (Claude checkpoints, OpenCode
  turns); watch cursors record file identities on their next pass.
- `cv search --semantic` needs `cv index --semantic` once more: vectors embed the exchange only and
  a pre-binary `embeddings.json` store is no longer read (not measured).
- The task log needs no migration. The catalog upgrades automatically: legacy counts and watch
  tables are recreated, cache columns added and discovery invalidated. No manual migration command
  is needed. Moving a home preserves its index when transcript paths and identities stay valid.

### All changes

**Reading**
- Schema includes `hook_ledger` among lane status sources and documents returned lanes; runtime lane JSON is unchanged.
- Cursor Agent CLI chat and ACP blob stores are read: prompts, replies, tool calls, summaries, modes and lineage; numeric modes read as names.
- Untitled sessions are titled by their first prompt everywhere (index and scan hits, labels, Markdown, share, archive), never injected AGENTS.md or environment context.
- `ls --json --enrich` `display_title` uses the row's `title` when present and never picks harness-injected turns such as Codex AGENTS.md instructions.
- Codex discovery counts logical turns as parsing does, excluding event echoes, injected instructions and inherited history; OpenCode, Goose, Hermes, Devin and Qwen likewise.
- OpenCode: custom database names are discovered, WAL changes noticed, conflicting native ids refused; non-UTF-8-JSON message and part cells are skipped, not fatal.
- OpenClaw: an empty `event_json` row is skipped; exports keep native records, branch links, compaction metadata and grouped tool results; escaped branch controls are recognized.
- Gemini accepts escaped wrapper metadata and keeps saved-chat message classification; checkpoint replay and composite tool results read with bounded memory.
- Kimi deferred wire outputs keep their source identity and reject replaced files; Kimi Code keeps model switches across resumed reads.
- LM Studio exports keep empty completions that carry token usage, plus selected and alternate version identities, sparse indices and distinct alternate models.
- Hermes exports keep modern metadata and reasoning modes; a Hermes lean read omits undemanded provider extras while keeping reasoning facts.
- Cursor, Cline and Roo keep native timestamps, errors and model choices; a Cursor IDE shell command that failed keeps its error status.
- Continue: exports keep native usage and cost, every grouped tool result and unrelated index entries; unsupported cache-creation counts are reported, overflowing conversions rejected.
- Grok: a sub-agent whose metadata appears after its parent was read is picked up; Grok and Qwen replays keep physical checkpoint boundaries.
- Goose lean reads omit undemanded native sidecars while keeping usage, model and turn kinds.
- Claude checkpoint counts retain restorable result facts; cached counts from older views are recomputed.
- OpenSession keeps an optional opaque `principal` through reads and transformations; a mixed-source composition keeps it only when every source agrees (#4).
- `cv cat`, rendered views, previews and pack excerpts report text they cannot read instead of printing it empty or panicking.
- Compaction summary scans propagate lazy read failures and keep the full summary text; affordance rows and counts do the same instead of misclassifying.
- `cv tools --timeline` composes workflow, agent and tool filters; `cv workflow` views skip unrelated child files and journals.
- Claude workflow revival streams selected histories and recovers valid siblings when another journal is incomplete.
- Harness-qualified lookups discover only that harness on a catalog miss; id prefixes stay case-sensitive and ambiguity is preserved.
- Resume hints quote shell arguments safely, including unusual paths and session identifiers.

**Search**
- HTTP live search rejects context/all scopes with 400 instead of silently searching the exchange; use indexed search for those scopes.
- Search matches the exchange; injected instructions, system prompts and hooks are `--kind context`; fork copies hit their source once. The index rebuilds once.
- Semantic vectors embed the exchange only, not injected context or fork copies; `cv index --semantic` re-embeds once. `/api/search?semantic=1&kind=context` is a 400.
- `cv search` with no usable index errors at once naming the cause and fix, never scanning unasked; `--scan` (`/api/search?scan=1`) asks for the live scan.
- Every indexed `cv search` answer, hit or miss, names how many sessions the index trails and by how long (stderr under `--json`; `X-Cv-Search-Note`).
- `cv search`: when live snippets cannot be generated the indexed previews stand in, with a note, as `/api/search` already did.
- `cv search`, `ls` and `show` work in a home they may not write (a sandbox deny-write rule): the index and catalog open read-only.
- `-q` supports compact OR expressions (`a|b`) and exact metadata equality (`field="x"`); unsupported text equality reports the syntax to use.
- Search fills the requested session limit despite dominant matching chunks or rare harness/cwd filters; equal scores rank by session id in any index order.
- Incremental search rewrites changed session metadata, fixing stale title conjunctions; snippets map casefolded matches back to the original Unicode.
- `cv index` exits 0 when only some sessions are unreadable: a changed source is retried once, the count warned; `--json` lists them, `--strict` fails.
- A persisted tool output cv cannot open (another user's file) no longer fails its session's indexing: the session is indexed with a note.
- `cv index --semantic --subagents` fails before writing anything, naming the two separate operations to run.
- Indexing and search see one copy of a duplicated session, adopted agents included; duplicate index records and old duplicate vector rows are repaired.
- Live search resolves lazy labels and reports demanded-text failures instead of returning incomplete matches.
- HTTP and desktop semantic search reuse checked model and vector state, reload a changed store and reject stale model artifacts.

**Reshaping**
- Claude rewind and port default to native for all-`claude-*` sessions, portable otherwise; portable removes signature-only reasoning instead of writing placeholders; rewind reports which.
- `--thinking portable` on `port`, `rewind`, `splice` and `pack` demotes plaintext Claude reasoning to text and maps illegal tool IDs collision-free at both ends (#21).
- `prune --drop-thinking` omits old thinking blocks with no inline marker, removing emptied records and relinking their children; `cv cat` still retrieves them.
- Prune's revive estimate anchors on recorded usage and the resumed branch, erring high; calibrated against the API it never read more than 1% low.
- One token estimator with per-kind rates calibrated against `count_tokens` replaces the flat 3.5 bytes/token in doctor, prune and distill.
- Doctor and prune distinguish stored-payload estimates, recorded prompt usage and rewritten resume metadata; opaque thinking payloads no longer imply measured reasoning tokens.
- Prune accepts a sub-agent's transcript id or path, counts its own turns and writes a standalone session, reporting child and parent identities.
- Prune, restamp and resume verification treat a line of Unicode spaces as malformed, as the reader does; only JSON whitespace makes a line blank.
- Emitted sessions, distill sidecars and packs, dataset, pack and share files are published whole, never into an existing file; a taken name exits 2.
- Rewind, adopt and share exit 2 when their output name is already taken; `prune --to` refuses an existing session or sidecar name.
- `--force` on `prune --to`, dataset, pack, distill `--pack` and share replaces a taken file by atomic rename; hard links keep the old bytes.
- `prune --copy-resources` refuses an existing resource directory; with `--force` the new directory replaces it whole instead of merging into it.
- Prune, rewind, adopt, share and archive outputs publish fsynced, without needing hard links (FAT, exFAT); rewind outputs keep the source's permissions.
- Prune, rewind and adopt validate source aliases, native identifiers and publication boundaries, verify source identity throughout, and refuse an output that aliases its source.
- Harness indexes cv updates on emit (Continue, OpenClaw, Gemini) and cv's config are replaced by rename; a symlinked config is replaced at its target.
- Ported project context files publish only after copying succeeds; failed copies leave no partial destination and preserve existing files.
- `cv port --harness opencode` writes OpenCode 2.x stores and bootstraps a fresh resumable database; existing 1.x stores still read.
- OpenCode rehomes keep embedded attachments; an edit invalidates stale replay and an unavailable external image reports fidelity loss.
- Distill derives findings from structured records, keeps transitive tool dependencies, and materializes only the messages `--upto` selects.
- Dataset exports keep session-level system prompts without duplicating explicit system messages, honor `--limit 0` without parsing, and reject transcript inode aliases.
- Pack excerpts include checked source-backed text and report unavailable prefixes; fallback recall centers Unicode snippets on their source matches.
- Markdown preserves literal newlines and safe fences; redaction covers complete records and system text; a fatal source error leaves stdout empty.
- `splice --generate` bounds the prompt content it selects, verifies sources, rejects an empty provider response and names the configured remote destination accurately.

**Tasks and lanes**
- Task appends recover an incomplete final record after validating new events, keep valid history and refuse an ambiguous complete tail.
- Task writes return the state captured during their append, so a later concurrent write cannot change what a command reports.
- Task inboxes project brief text without copying rich histories; aliases, unread cursors and terminal output keep consistent semantics.
- Lane views show task-log warnings and retryable loading errors; an empty selection skips task enrichment while keeping forest totals.
- Lane summaries index task assignments once instead of rescanning per lane; an invalid or overflowing duration is an error, not a panic.
- `/api/tasks/inbox/{who}` takes `since=` and `all=` like `/api/inbox`.
- Deferral matching reuses each command's task word sets; shared-word output is built only for the winning match.

**Hooks**
- Hook settings and skills publish by atomic replacement, preserving existing readers, permissions, and dotfile symlinks.
- A lane's hook stop is scoped to its parent session; the edit-conflict hook distinguishes sibling agents from copied ones.

**Serve, MCP and web**
- `cv serve` runs on a small built-in server: idle connections close after 5 s, excess connections get 503, accept errors no longer stop serving.
- HTTP limits concurrent connections and incomplete request lifetimes; overload shedding bounds retained sockets and drain work; keepalive bursts no longer strand queued requests.
- HTTP rejects malformed framing, headers, authorities and numeric parameters; a body framing error closes the connection; task requests enforce body limits and media types.
- `GET /api/session/{harness}/{id}/export` and the browser's download export the complete session as Markdown or OpenSession, including messages beyond the loaded page.
- `cv archive sync` takes its inventory from the index sweep (one corpus walk, the index refreshed on the way); a home whose index cannot be written falls back to discovery.
- MCP cancellation targets one request, releases its resources and keeps unrelated work and the connection running; a disconnect cancels and reaps children.
- MCP bounds concurrent tools and the response backlog, preserves control replies, and releases oversized request buffers between frames.
- MCP validates envelopes, arguments and observation filters before execution; `observe_stream` polls within the remaining timeout and matches messages incrementally.
- Generated MCP tools stage large output in bounded memory and return successful stderr, including continuation hints, as a second content block.
- MCP task tools use the actual CLI flags; generated schemas describe repeatable arguments, and the command reference shows their correct spellings.
- Web: a session's identity includes its harness; search drops stale responses and merges results across the selected harnesses.
- Web: transcript pages render before full metadata arrives, closed folds render on open, a failed page shows a retry instead of truncating or duplicating.
- Web: compare and splice load complete histories with retries; structure views expose every root; ZIP imports are bounded and complete in native and Wasm.

**Performance** (output unchanged)
- Large transcripts read through 8 MiB windows and single spans, never whole: `cv doctor` on 1.3 GB peaks at 22 MB, not 2.6 GB.
- `cv index` parses on every core: a full counts recount of a 95 GB fleet went from 329 s to 61-68 s.
- `cv index` rebuilds the full-text index once after upgrading; afterwards a grown Claude or Codex transcript is read from where the last pass stopped.
- Native builds decode zstd (Codex rollouts, Zed and OpenClaw blobs) with libzstd: a cold index of compressed rollouts takes 39% fewer instructions.
- A compressed Codex rollout is decoded once and read from one mapping: cold index 15% fewer instructions, first listing and `show` about half.
- Claude transcripts parse through one record codec in every mode and build, using about 20% fewer instructions; malformed records get one acceptance rule everywhere.
- Codex, Grok, Qwen, Kimi, Kimi Code and Continue read through one record codec per format: cold Codex index 13% fewer instructions; bad lines skip.
- Gemini recordings read through one message codec; a 314 MB session's lazy read takes 3.4x fewer instructions.
- Cline, Roo, LM Studio, OpenSession and account exports read through one codec each; lazy text follows Claude's span rule (decoded length over 4 KiB).
- `ls -q has:` and unchanged `cv index` detect only the adapter a session needs, once per operation: 2.9x and 1.7x fewer instructions.
- Devin, OpenCode, Hermes, Zed and Goose discovery reuse cached results while their database, WAL and journal are unchanged.
- Listing beside a live Claude transcript reads only its appended bytes: a discovery pass raced by the writer is kept, not restarted.
- Claude discovery resumes verified appends from metadata checkpoints; Kimi, Qwen and Codex discovery project counts and titles without owning transcript payloads.
- Every harness reader keeps large bodies in the source file and decodes them in bounded windows, with or without mmap; SQLite cells stream incrementally.
- `show`, `export`, `dataset`, `splice`, `prune`, `distill`, `doctor`, Markdown, native JSONL exports and every rehome stream checked payloads through bounded staging, never whole-session strings.
- Semantic indexing reuses unchanged vectors, loads the embedding model only when a session needs refreshing, and decodes stores through bounded buffers.
- Full-text indexing streams tool arguments into bounded documents, caps its queued backlog, keeps segments over 8 MiB out of the bottom tier.
- Catalog connections are reused across discovery, refresh and publication; unchanged rows are not rewritten; limited queries evaluate only their ranked candidates.
- Watch and archive reuse verified record checkpoints; unchanged sources are not reparsed; archive catalog cleanup scans grow geometrically.
- Browser windows, API pagination and session-list responses reuse bounded continuations and completed rows instead of rescanning or copying.
- CI caps live Rust heap on generated large Claude, Codex and Gemini sessions and escaped span decoding.

**Fixes**
- One freshness record per consumer and session, verified on load (counts, events, search, usage); watches record file identities. Each rereads once after upgrading.
- A live `.jsonl` beside its writer is admitted by content (same file, sampled prefix intact), never by waiting; index, events, counts, vectors reread once.
- Reading a live session no longer fails with "JSON source changed" when its writer appends: `.jsonl` sources may grow; truncation and rewrites still fail.
- Large text spanned from a whole-document `.json` transcript fails its demand once the file changes, grown or not; only `.jsonl` logs may grow.
- A transcript replaced or rewritten after parsing fails its first lazy read instead of returning the replacement's bytes; heads and windows reject changed sources.
- Positioned (no-mmap) builds resolve large transcript text from a live, growing session, as mapped builds do; a replaced or shrunk file still fails.
- Catalog freshness checks every primary source identity, noticing quiet-session appends, same-size edits and timestamp-preserving replacements; counts, events and search track sidecar and WAL changes.
- Dependency freshness follows conditional fork siblings and external outputs, including missing files and relative paths.
- Reading SQLite harness stores no longer rewrites their `-shm` or creates WAL sidecars: WAL stores open `immutable=1` when frameless, otherwise through a private snapshot.
- A copied or moved `CLUSTERVISION_HOME` keeps its full-text index: baselines follow an identity stored in the index, not its path.
- The first fleet read after an upgrade that invalidates the catalog prints one stderr note that it is re-discovering, suggesting `cv index`.
- A failed prune, rewind or adopt retracts its companion file only if still cv's own, on every platform, and reports a failed retract.
- Replacing a directory without atomic swap no longer deletes the new tree when restoring the old one fails; both are kept and named.
- New-name publication fails safely when neither hard links nor exclusive rename are supported, instead of risking overwriting a concurrent writer.
- Watch cursors and Kimi, Kimi Code and Qwen checkpoints serialize in a stable order; watch forgets vanished sessions, so a reappearing session starts fresh.
- Event publication acquires write ownership before validation; event caches bind rows to source copy, attribution and cwd; large batches spill to disk, retryable.
- Index publication reuses its catalog connection while keeping independent transactions and reporting partial failures for retry.
- Doctor rejects unavailable lazy text instead of counting zero tokens; commit evidence scans report unreadable sources instead of silently losing matches.
- Resume guard sampling preserves open transcript read positions on Unix.
- Semantic score ties use stable session identities, preserving result order and limits across incremental refreshes.
- `cv --version` build provenance recognizes linked worktrees and release archives.
- Manual: search defaults and fallbacks, affordance views, MCP window exceptions, the calibrated estimator, `--drop-thinking` and the prompt-row schema read as the binary behaves.

**Removals**
- **Breaking:** `GET /api/backlog` is removed (410 pointer); the inbox page reads `/api/tasks?detail=1`, which adds the backlog columns.
- **Breaking:** `POST /api/task/{id}/reopen` is removed (410): a closed task stays closed; open a new task and note the old id.
- **Breaking:** MCP no longer offers `hook` (it reads Claude Code's hook JSON on stdin; a call did nothing); calling it points at `hooks_install`.
- Pre-binary `embeddings.json` semantic stores are no longer read or migrated (querying them already failed); `cv index --semantic` rebuilds.

## 0.16.1 — 2026-10-12 — approvals, checkpoints, and the side files

Two new affordance kinds, `approval` (asks, denials, the approval mode in force) and `checkpoint`
(restorable points, tallies, restores), and the side files 0.16.0 named as unread: Qwen schedules,
Cline schedule fires and team runs, OpenClaw cron and sub-agent registries, Kimi todos, Grok hook
runs; Claude `--fork-session` and Goose forks now name their source. Catalog counts carry a view
version, so `has:` recounts after an upgrade instead of answering from 0.16.0's counts: run
`cv index` once.


- New affordance kinds `approval` (asks, denials, the session's approval mode) and `checkpoint` (restorable points, restores) in `show --kind` and `has:`.
- Roo `ui_messages.json` checkpoints and permission asks, and Grok `rewind_points.jsonl`, are now read.
- Session JSON gains `permission_modes` and `checkpoints` in the harness bag (Claude, Codex, Kimi, Kimi Code, Qwen, Gemini saved chats).
- Devin edit results carry `details.restorable`, the files `/revert` can restore.
- Catalog affordance counts from 0.16.0 are recounted on the next full read, so `has:approval` / `has:checkpoint` never answer from stale counts.
- Side stores read: Qwen `scheduled_tasks.json`, OpenClaw cron and sub-agent registries, Cline schedule runs and `teams.db`, Kimi `state.json` todos, Grok hook runs.
- Catalog affordance counts are recounted once more, to include these side stores and forks.
- Claude `--fork-session` and Goose `--fork` sessions name their source in `lineage.forked_from`, inferred from the records the fork copied.

## 0.16.0 — 2026-10-12 — every harness, every version, every affordance

cv now carries a corpus of real sessions written by 17 harnesses at the versions in use and their
latest releases, re-read by a golden test, so a format change shows up as a test diff and old
formats keep reading. Building it found sessions cv could not see at all (OpenCode 2.x, Qwen Code
0.25, the Cline CLI store) and data it silently dropped (OpenClaw 10's compressed events, kimi-cli's
rotated segments, Grok reasoning). On top of it, one vocabulary for what swarms run on: plans,
goals, schedules, background jobs, sub-agent spawns and returns, skills, hooks, questions, branches,
forks and model changes, queried the same way on every harness with `cv show --kind <affordance>`
and `-q has:<affordance>`.

**What reads differently:** turns that spawn a sub-agent are `subagent_spawn` (and synchronous
results `subagent_return`) where they were `reply`/`tool_result`; Claude task-notifications and
interrupt/meta rows are no longer `prompt`; Hermes and OpenClaw `tool_call` wrappers read as the
tool they call. `--json` consumers keying on those kinds should update.

**Run `cv index` once after upgrading**: it counts every session's affordances so `has:` answers
from the catalog (minutes on a large fleet, incremental after).

### All changes
- **The `cvd` and `cv-mcp` shims are gone**, as 0.15 announced: `cv archive|watch|serve` and `cv mcp`. A launchd job or MCP registration that names them needs the new spelling.

- `has:plan` and the other affordance flags answer from per-session counts in the catalog (filled by `cv index` and any full read); only uncounted sessions are parsed.
- `cv tools --timeline` shows each call's `tool_use_id` (in `--json` too), the id `cv cat <session> <id>` takes.
- `cv tools --timeline` shows one line per tool call, matching `cv tools` counts; a multi-file patch's other files are in `more_targets`.
- `cv port --harness qwen` writes Qwen Code 0.25 session JSONL under `~/.qwen/projects`; it wrote the gemini-cli shape 0.25 no longer reads.
- `cv show <id> --kind plan,schedule,background_job,…` (`affordances` for all) lists plans, goals, schedules, jobs, spawns, skills, hooks and forks across harnesses, side stores included.
- `-q has:plan`, `has:schedule`, `has:background_job`, …: select sessions by affordance on any harness (`cv ls`, MCP, `cv serve`).
- Turns that spawn a sub-agent are `subagent_spawn`, and synchronous results `subagent_return`, in Claude, OpenCode, Hermes, OpenClaw, Goose, Grok, Kimi, Roo, Cline, Devin, Continue.
- Claude: task-notifications are `subagent_return` (agents, workflows) or `injected_context` (commands); interrupt markers and `isMeta` rows are `injected_context`, not `prompt`.
- Hermes and OpenClaw 10.1: a `tool_call` reads as the tool it calls, namespace `tool_call`; their emitters wrap it back.
- Side stores read: Claude `tasks/`, Grok `plan.json`, OpenCode `todo`, Gemini `tracker/` and `plans/`, Goose's todo extension, Hermes goals, loops and heartbeats.
- A reply that spawns a sub-agent counts as a turn and stays in `dataset`, `distill` and lane message counts.
- The `compact` hook names the command that reads back the session's plan, schedules and background jobs.
- Qwen Code 0.25: sessions in `~/.qwen/projects/*/chats/*.jsonl` and their sub-agent transcripts are read; cv previously found none.
- Gemini: tool results and thoughts no longer appear twice after a resume, and a resumed 0.46 session is no longer hidden by its empty twin.
- Gemini: hook context is its own turn; `invoke_agent` calls are sub-agent spawns and returns.
- Gemini and Qwen corpus: real stores from Gemini CLI 0.46.0 and 0.63.0 and Qwen Code 0.25.1 in the golden test.
- Codex 0.161/0.162: `turn_aborted` notes keep the abort `error` and `root_turn_id`; `cv formats` names carried records `event_msg.<type>` as the manifest does.
- Codex corpus: real rollouts from Codex 0.161.0 and 0.162.1, re-read by a golden test so every recorded format keeps reading.
- `cv index` merges its segments itself, by size, to completion: no more abandoned ~1 GB background merge on long runs.
- `cv show --last N` and `cv export --last N` read the session once instead of counting it first: about half the time.
- Kimi discovery reuses the scan cache: an unchanged transcript costs a stat, not a full read.
- A follower of a Codex session holds only the new messages, not the whole parsed rollout (595 MB to 124 MB on a 708 MB rollout).
- A `cv search` miss checks index staleness against the catalog instead of rescanning every harness: 0.53 s to 0.21 s.
- `cv ls --enrich` keeps only each session's user turns while reading it: 720 MB to 80 MB peak on the largest sessions.
- `cv cat` stops reading at the tool call it prints instead of holding the whole session.
- OpenCode 2.x: sessions in its new `session_v2`/`session_message` tables are read (2.x databases looked empty before); a migrated 1.x database shows each session once.
- OpenCode corpus: real stores from 1.18.20 and 2.0.25, plus 2.0.25's migration of the 1.18.20 store.
- kimi-cli: sessions read every `context_N.jsonl` segment, not just the live file; `/clear` is a branch; `subagents/` parse as child sessions.
- Kimi Code 2.x: `subagent.spawned`/`completed` are spawn/return turns, `/undo` is a branch, `kimi fork` sets `lineage.forked_from`.
- Kimi corpus: real stores from kimi-cli 1.48.0/1.51.0, Kimi Code 0.39.1/2.1.1, and a 0.39.1 store reopened by 2.1.1.
- Cline: sessions from the CLI 3.x and the 4.x extension (`~/.cline/data/sessions/`) are read, including spawned sub-agents and teammates.
- Roo: Roo CLI tasks (`~/.vscode-mock/global-storage/tasks/`) are found; reasoning blocks, condense summaries and `new_task` parents are read.
- Continue: the cn CLI's tool results (`toolCallStates`) and per-turn usage and model are read; they were missing.
- Devin: a `run_subagent` child, which has no `subagent_heads` row, is listed as its own session.
- Corpora for Cline 3.0.70, Roo CLI 0.1.17, Continue cn 1.5.47 and Devin 3000.11.3; schema snapshots for Zed 1.16.1 and LM Studio 0.4.21.
- Claude Code 2.1.296: `ScheduleWakeup` turns read as `origin: scheduler`; its corpus (18 sessions, 6 sub-agents, a workflow) joins the golden test.
- Grok 1.0.50: reasoning records read as thinking; forks and sub-agents carry their parent; its corpus joins the golden test.
- Goose: a real compaction (agent-visibility flip plus agent-only summary) reads as a compaction boundary; delegate results keep the child session id.
- Goose corpus: real stores from Goose 1.51.0 and 1.54.0, covering every persisted affordance a headless run can drive.
- OpenClaw 2026.10: zstd-compressed transcript events are read; before, every event of 1 KiB or more (most tool results) was silently dropped.
- OpenClaw: sub-agent sessions name their parent (`lineage.parent`); cron and sub-agent prompts get scheduler/subagent origin; sub-agent announces are `subagent_return`.
- OpenClaw corpus: real stores from 2026.1.29 (JSONL) and 2026.10.1 (sqlite), re-read by the golden test.
- Hermes 0.21: a `failed_turn` row (a turn that got no reply) reads as an error; v29–v31 identity and source columns ride in `extra.hermes`.
- Hermes corpus: real `state.db` stores from Hermes 2026.8.31 and 0.21.6, re-read by the golden test.

## 0.15.1 — 2026-10-11

- The `clustervision` crate packages: the `cv` skill file lives inside the crate, so `cargo install clustervision` builds (0.15.0's CLI crate never reached crates.io).
- CI packages every publishable crate on each push, so a file the published tarball lacks fails before a tag.

## 0.15.0 — 2026-10-11 — the clean break, part two

cv went from 110K lines to the shape in `docs/INTERFACE-V2.md` "The shape": one binary, one read
API every door maps onto, two reshape primitives, adapters over shared wire codecs, a task log of
twelve event kinds, and a manual generated where it can be. Every removed or renamed name errors
with a pointer and exits 2; every stored log and transcript still reads.

| old | new |
|---|---|
| `cvd sync\|ls\|path`, `cvd watch`, `cvd serve`, `cv task serve` | `cv archive sync\|ls\|path`, `cv watch`, `cv serve` (`cvd` is a one-release shim) |
| `cv-mcp` | `cv mcp` (`cv-mcp` is a one-release shim) |
| `cv fork`, `cv loom` | `cv splice <id>:..N --session\|--agent-of`, `cv splice <a>:..N <b>:M..` |
| `cv tree`, `cv prompts`, `cv compaction` | `cv show --tree`, `--kind prompt,answer`, `--kind compaction_boundary,compaction_summary` |
| `cv timeline` | `cv ls --sort-by updated --asc` |
| `cv redact`, `cv export --format html` | `cv export --redact`, `cv share` |
| `cv diff` | export both sessions and diff the files |
| `cv adopt --list\|--orphans` | `cv lanes <session>`, `cv lanes --recent N --unfinished` |
| `cv board …` (13 verbs, 13 MCP tools) | the task log (`cv task`, `task events --since`) |
| `cv task propose\|pass\|refute\|reroute\|verify\|debt\|stats\|split\|watch` | gone with the land facet; `decide --blocks`, `events --assignee` |
| `cv-tui`, the `cv-search` binary, `cv-llm`, `cv-sim` | gone (`cv ls`/`show`/web; `cv index`/`search`) |
| `$CVD_TOKEN` | `$CV_SERVE_TOKEN` |

New: `cv hooks install|remove|show claude` and five `cv hook` bodies with the `cv` skill;
`--model` on `adopt` and `distill --agent-of`; `cv lanes` reads the hook ledger.

### All changes

- `cv tools` counts tool calls, as `cv doctor` does: a multi-file patch through the shell is one call, not one per file. `calls`/`total_calls` drop.
- `cv adopt --model <id>` and `cv distill --agent-of … --model <id>` set the new agent meta's `model` (`inherit`: the root's current model).
- Web UI: the Loom tab is **Splice** (`<cv-splice>`, `＋ splice`, `⑂ branch from here`), the browser face of `cv splice`.
- Desktop app: the unused native `redact` and `ingest_zip` commands and the `cv-search` dependency are gone (116 lock entries); File → Open zip is unchanged.
- **`cv tree`, `cv prompts`, `cv compaction` are `show` views**: `show --tree`, `show --kind prompt,answer`, `show --kind compaction_boundary,compaction_summary [--summaries]`; old names exit 2.
- Manual: a command reference and the MCP roster generated from the binary at build time; `cli.md` keeps examples and reasons, no flag lists.
- Manual: twenty chapters are fifteen (intro, app, share, daemon, integrations, opensession merged into neighbours); the old page URLs redirect.
- Redaction runs on per-recognizer regexes with the same guards: identical output, and a long base64 run no longer takes seconds (110 → 3 s on 50 sessions).
- `cv doctor` streams each session instead of parsing it whole (154 → 24 MB resident on a 190 MB transcript); output unchanged.
- The seek store is gone (it saved under 2 s and served a stale window): `cv index` records no offsets; the `msg_offsets` table is dropped.
- One inbox row: `cv task inbox --json` prints the page `serve` serves; MCP `task_inbox` and cvd's inbox route return its `items` (`since`, `age`, decision, notes).
- **Removed `cv task split`** (it mined `DECIDE:` notes); `cv task decide … --blocks <id>` poses the question. Exit 2 with that pointer.
- `cv task decide` writes `opened` + `posed` only — no `decision` tag; the bare-tag refusal moved into the reducer, so MCP `task_open` gets it too.
- `task open --tags/--blocked-by/--blocks` and the page's `discuss`/`reopen` are each one atomic append; `resolve` reports the reducer's own refusals.
- **`cv hooks install|remove|show claude`**: five Claude Code hooks (`cv hook compact|start|lane|decisions|edit`) plus the `cv` skill, merged idempotently and removed exactly.
- `cv lanes` reads the hook ledger (`lanes.jsonl`): a lane whose stop only the `SubagentStop` hook saw reads `stopped`, `status_source: hook_ledger`.
- `cv doctor` prints whether the hooks are installed and the compaction policy in force (settings and environment).
- cv-mcp runs generated tools with stdin closed, so a command that reads stdin never consumes the JSON-RPC stream.
- `cv ls` shows the real turn count for Codex rollouts over 8 MiB after their first full read (`show`, `index`, any parse); before, 0.
- **Removed `cv fork` and `cv loom`**: `cv splice <id>:..N --session|--agent-of <root>` is the fork, `cv splice <base>:..N <other>:M..` the graft; both names exit 2 with that pointer.
- The Claude emitter owns resumability: dangling `parentUuid` re-linked, unanswered `tool_use` dropped, one sidechain identity — for `distill`, `splice`, `port` and `pack` alike; `emit_verified` asserts it.
- `cv distill`'s pack shows a trailing call that never got its result (the lane stopped mid-call); the emitted session still drops it.
- `cv distill` token estimates count bytes (not chars) at its 2.4/token, a call's `input` only and encrypted thinking: one estimator with `doctor`.
- MCP: every `task_*` tool is generated from `cv task` (17, was 9 hand-written); results are the CLI's output. Only `await_omen`/`observe_stream` are hand-written.
- Manual, README and web/app docs name `cv serve`, `cv watch`, `cv archive` and `cv mcp`; the daemon chapter documents the inbox routes.
- Web UI reads and writes OpenSession 0.3 only; a dropped 0.2 document errors with the CLI's pointer instead of being half-read.
- `cv schema --commands --json`: an argument in a one-of group is no longer `required` (`task resolve --choice`).
- **`cv-mcp` is `cv mcp`**: the roster comes from the in-process clap tree (no `$CV_BIN`, no stale `cv` on PATH); `cv-mcp` is a one-release shim. No tokio.
- **`cvd` is `cv archive sync|ls|path`, `cv watch`, `cv serve`**; `cvd` is a one-release shim that execs `cv`. `$CVD_TOKEN` is `$CV_SERVE_TOKEN`.
- **`cv task serve` is gone**: `cv serve` carries the inbox at `/inbox` (`/` without `--web`) under its Host/CORS/token model; writes must be JSON from a local origin.
- Desktop app: the ten `local_*` commands are one `api(path, query)` over the server's read API; `cvd/tests/parity.rs` goes (both doors call the same functions).
- One read API (`cv_core::api::route_json`) behind every HTTP door; `/events` `ts` and `/touched` `last_ts` are RFC 3339 like the CLI's; `/subagents` rows are §3 session rows.
- **`cv-llm` is gone**: `crates/cv/src/llm.rs` on `ureq` (no reqwest/hyper/tokio stack; 381 → 346 lock entries); its `distill` is now `digest`. The app loses native distill/generate; the loom's OpenRouter path stays.
- Manual: `distill`, `fork`, `adopt` documented and a "Reshaping a lane" page; MCP tools and `cvd` routes match the binaries; harness counts corrected.
- `integrations/` is gone (its recipes called MCP tools removed in 0.11); the manual's [hooks section](manual/src/mcp.md#hooks-cv-inside-a-running-session) replaces it.
- `cv show`, `cv export --format md` and `cv share` spell a turn's label one way (`system · injected_context/hook_success`); the md header gains `- started:`, cwd is home-relative.
- `cv show --subagents` lists each agent on `cv tree`'s one line (id, type, status, count, summary or task) plus its return; both sanitized.
- **`-q` sheds its sub-languages**: `thread:` paths, the `~` regex operator and the field-name typo guard are gone; `thread:`/`~` exit 2 with a pointer.
- A malformed `-q` query is a caller mistake: exit 2, not 1.
- **Removed `cv diff`** (a role+text prefix compare): `cv export <id> --format md` each session and diff the files; the name exits 2 with that pointer.
- OpenSession reader reads 0.3 only (byte-identical round trip); a 0.2 document errors naming the version and the renames.
- Removed the `claude-app` and `chatgpt-app` harnesses (they never had a session to read); the names now error with a pointer, exit 2.
- **Removed `cv-tui`**: its three views are `cv ls`, `cv show` and the web Sessions/Fleet tabs; one release binary fewer.
- **Removed the `cv-search` demo binary**: `cv index` / `cv search [--semantic]` are the commands; the crate stays a library.
- **Removed `cv timeline`**: `cv ls --sort-by updated --asc` prints the same rows oldest-first; the old name errors with that pointer.
- **Removed `cv redact`**: `cv export <id> --redact [--format md|json]` runs the same scrub; counts go to stderr.
- **Removed `cv export --format html`**: `cv share` is the one HTML renderer (streaming, redacted); the whole-session `to_html` goes.
- **`cv lanes --recent N [--cwd DIR] --unfinished`** replaces `cv adopt --list` / `--orphans`: a project's recent forests in one table, each row tagged with its session.
- **`cv formats` is flat** (the census); `formats check` is the `formats_manifest` test — it always needed a checkout.
- The 0.11 tombstones are gone: `cv convert|query|recall`, `prune --thinking|--retrieve|--range`, `port --to|--to-dir|--from`, `--to` on `splice|loom|pack` are now plain unknown names.
- `cvd --version` no longer goes stale on a branch (its build.rs watches the resolved ref, as `cv`'s does); cv-mcp's unknown-harness error names all 23 harnesses.
- **`cv deferrals --phrases-file`** (or `~/.config/clustervision/deferrals.txt`): the phrase table is yours; the built-in default carries no maintainer's name.
- Web: the OpenSession tab and `openSession.html` are gone; the format's home is the manual chapter, which the footer links.
- `cv task list --wide` is gone; the flag points at `show <id> --brief` and `list --tsv`.
- `cv task watch` is gone (it was `events --assignee <who> --not-by $CV_ENDPOINT`); the name errors with that spelling.
- `cv task open/decide --channel` and MCP `task_open.channel` are gone with the board (pointer, exit 2); task rows lose `channel`. Old logs still read.
- `cv board` (13 verbs), the 13 MCP `board_*` tools and cvd's `/api/board|claims|who|channels` are gone; each errors with a pointer at the task log. The fleet pane shows sessions only.
- Task log: the nine land-facet event kinds replay as state-neutral events; `show --json`/`list --json` lose `revisions` and `done_check`. Old logs still read.
- `cv task propose/reroute/pass/refute/verify/debt/stats`, `--token`/`CV_TOKEN` and `done --check-*` are gone; each errors with a pointer. `done` is labeled `self-reported`.
- MCP: `task_propose`, `task_pass`, `task_refute`, `task_verify`, `task_debt`, `task_stats` and the `token`/`check_*` params are gone; calls error with a pointer.
- `cvd watch --verify-interval`, `GET /api/tasks/debt` and `/api/tasks/stats` are gone with the task verifier; the flag exits 2, the routes answer 410.
- Adapters read content through one Anthropic and one OpenAI codec; a tool call's scalar or unparseable `arguments` string is now kept verbatim in every harness.
- Epoch timestamps read one way: zero means unset (not 1970), and Goose's millisecond timestamps keep their milliseconds.
- `formats/<harness>.toml` lists only what source can't say: a type with a match arm is `handled` by inference; `cv formats` stops counting `handled`.

## 0.14.0 — 2026-10-10

0.12.0 and 0.13.0 were bumped in `Cargo.toml` but never tagged or published; tags and crates.io
stopped at v0.11.2. Everything since ships here. Every change is in the manual; this is the index.

### Read

- **`cv prompts <session>`**: only what the person typed and every `AskUserQuestion` answer, with indices; `--pre-compaction [N]`, `--json`.
- **`cv lanes <session>`**: the sub-agent forest as a status table (model, duration, tokens, calls, last line); `--running`, `--done`, `--json`.
- `cv lanes --stranded`: lanes that completed saying they are waiting; each row carries `→ resume: SendMessage to <agentId>`; never counts as done.
- `cv lanes`: status source named in JSON (`Workflow` journal, parent notification, `SubagentStop`, else `running`).
- `cv lanes --since 2h`; a quiet lane whose transcript ends in text reads `returned`; a running lane quiet an hour says `⚠ quiet`.
- `cv lanes` failure cause from the transcript's last API-error notice: `failed:rate-limited` (with `resets_at`), `failed:context`, `stopped`.
- `cv lanes --tasks`: each lane's endpoint (its own `CV_ENDPOINT=` export, else its description's first token) joined to the tasks it holds.
- **`cv deferrals <session>`**: every deferral phrase with index and context; `--open-tasks` cross-references the store and exits 1 while any is unmatched; `--since`.
- `cv blame`: a session whose own tool output shows the commit ranks first as `exact: commit created here`, with a `cv rewind` hint.
- `cv blame`: sub-agent edit rows bring their parent session along as a candidate; reads transcripts directly, needs no fresh index.
- `cv show`/`cv cat agent-<id>` read the newest copy when two sessions hold one agent (after `adopt`) instead of erroring.
- `cv schema --json` publishes `prompt_row`, `lane` and `deferral`; `cv recipes` has three more entries.

### Reshape

- **`cv rewind <id> --at <MSG_IDX|SHA>`**: a new resumable session as of a message or the commit a tool result shows; `.rewind.json` provenance alongside.
- `cv rewind` starts at the last compaction before the cut (`--full` from the top); open parallel tool calls run on to their results.
- `cv rewind agent-<id>` / `--agent`: a sub-agent comes out as a standalone top-level session in the parent's project dir.
- **`cv distill <id>`**: a long lane as a compact deterministic context: brief, every message to it, its own words, an index, its last N calls.
- `cv distill` emits the markdown pack (`--pack`), a resumable session (`--session`), or a new sub-agent of a root (`--agent-of`); `--upto N`, `--with <other>`.
- **`cv fork <id> --at N (--session | --agent-of <root>)`**: the verbatim prefix as a new session or sub-agent, to run variants from one point.
- The name `distill` returns with a new meaning; the 0.11 stub pointing `cv distill` at `cv pack` is gone.
- `distill` and `fork` clear `parentUuid` so Claude Code resumes the whole context (it stops silently at the first missing parent).
- `cv splice` of a sub-agent span joins the new main thread: `isSidechain`/`agentId` markers dropped.
- **`cv adopt <agent-id>… [--into <live>] [--from <dead>]`**: moves sub-agents of a dead Claude session into a live one; `sessionId` restamped by byte offset.
- `cv adopt --list <session>`, `--orphans` (running/stopped/killed/failed/stranded agents of recent dead sessions), `--dry-run`, `--force`.
- `cv adopt --into` defaults to the project's newest session other than the agent's own and is printed first; looks across every Claude root.

### Fleet & live

- **Decisions are a kind of task**: `cv task decide "<q>" --for <who> --default … [--option …]… [--by <when>]`; `done` is refused on one.
- `cv task resolve <id> --accept-default | --choice "<option>" [--note …]`: answers it; identity-bearing, so it needs `--from` or `CV_ENDPOINT`.
- `cv task decide … --provisional` / `resolve --accept-default --provisional`: resolved on the default by the poser; the decider's `resolve --confirm | --choice` replaces it.
- `open --tags decision` and `tag <id> decision` on an unposed task refuse, naming `cv task decide … --default …`.
- **`cv task split <id>`**: every leading-`DECIDE` note becomes its own decision, assigned like the parent and blocking it; a second run creates nothing.
- **`cv task inbox <who>`** leads with decisions (default, alternatives, deadline), then actions, claimed, reviews, unlanded; `--md`, `--unread`, `--json`.
- **`cv task serve [--bind] [--assignee] [--open]`**: a web inbox served by cv; every button records the CLI's event as `web:<who>`; loopback unless `--bind 0.0.0.0`.
- **`cv task events [--since <when|id>] [--kind] [--by] [--not-by] [--assignee] [--task] [--text]`**: one JSON line per event, next cursor on stderr.
- `cv task watch --assignee <who> --since …`: `events` minus the caller's own.
- **`cv task status <id>`**: a pinned STATUS (`status_set`), by assignee or opener, refused when terminal; `show` leads with it; `--brief`, `--status`.
- Notes on terminal tasks append with `post_close: true` ("(after close)"); `done|abandon --note|--note-file` append note and close as one unit.
- Bare `cv task list` / `inbox` show 14 days or anything involving `$CV_ENDPOINT`, and print the hidden count; `--all`, `--since 90d`.
- `cv task show --brief`, `--notes-last N`, `--notes-grep PATTERN`; a decision block with the resolve command.
- `cv task open --body-file`, `note --file`; `note <id> -`, `--body -`, `--note -` read stdin.
- `open --tags a,b`, `cv task tag <id> a,b`, `list --tag <t>`.
- `open --blocked-by <id>` / `--blocks <id>`, `cv task block <id> --by <id>`: blocked-ness computed from the blocker's live state; `list` marks `⊘`.
- `list --tsv`, `list --wide`.
- **`cv task sweep --repo <path>`**: tasks git and the filesystem say are probably done (missing `--issue` path, merged branch); never closes one.
- `--issue` is absolutized at open time.
- Every list renders the shortest id prefix that keeps ids distinct; a batch opened within one second rendered as one prefix.
- Board notifications carry the task's title, so `cv board read tasks` reads as a timeline.
- ⚠ **Wire, additive.** New events `tagged`, `blocked_by`, `posed`, `resolved`, `status_set`; optional `resolved.provisional`; a `noted` may follow a terminal event.
- ⚠ **Older cv on a 0.14 log**: `status_set` or post-close `noted` → warning, appends refused; provisional `resolved` → silently firm. Upgrade every writer first.
- Projection fields `tags`, `blocked_by` (omitted when empty); `TaskFilter.tag` on MCP `task_list` and `GET /api/tasks`; `InboxReason::decision_owed` / `provisional`.
- 0.14 reads every older log unchanged; the golden log gained specimens of each new event, every earlier line byte-identical.

### Harnesses
- `cv port --to kimi` is gone: `~/.kimi` is frozen (kimi-cli moved to `~/.kimi-code`), so the port went nowhere. Errors, exit 2.
- `cv ls` counts for grok, gemini, openclaw, goose and codex are user + assistant turns only; Codex rollouts over 8 MiB show 0, not an estimate.

- **Devin CLI (Cognition) parses**: `~/.local/share/devin/cli/sessions.db` (schema 17) read-only; the `main_chain_id`→root walk of the message forest.
- Devin: thinking (`sealed.v1`), parallel tool calls, per-call `Usage`, `generation_model`, system prefix, failed-tool marks, `cv prompts` answers; `cv resume` prints `devin --resume <id>`.
- Devin: `subagent_heads` rows are child sessions (`<session>/<agent>`) with lineage; compaction pairs become `CompactionBoundary`, so `cv compaction` sees them.
- **Claude Code sessions outside `~/.claude` are read**: `$CLAUDE_CONFIG_DIR/projects`, each `$CLUSTERVISION_CLAUDE_ROOTS` entry, each line of `$CLUSTERVISION_HOME/claude-roots`.
- A root entry may be a config dir or a `projects/` dir, use `~`, and hold `*` segments expanded every scan; the catalog watches all.
- One session id in two roots: both listed; `cv show` and MCP `show` read the most recently updated copy; the prefix is not ambiguous.
- Claude Code ≥ 2.1 slash-command bookkeeping (`<command-name>…`, `<local-command-stdout>…`) classifies as `notice`/`harness`; the typed `/compact` stays a prompt.

### Fixes
- `web:<who>` resolves to `<who>` at the token gate; `cv task serve --token`/`$CV_TOKEN` presents it on every page action.
- `posed`/`resolved` refuse over a live revision, as `done` does: no decision closes unlanded reviewed code.
- Rendered-attachment messages were never stamped with a byte offset, so any current Claude session fell back to a full stream under `cv show --range`.
- `cv_core::digest::Sha256` (the vendored SHA-256) is incremental.
- The clap-tree unit tests run on a 16 MB thread: `Cli::command()` overflowed the 2 MB test stack in a debug build.
- Test harnesses that fake `$HOME` also clear `CLAUDE_CONFIG_DIR` and `CLUSTERVISION_CLAUDE_ROOTS`, so a run inside an agent with its own config dir stays hermetic.

### Build

- `make install` (`cargo install --path … --target-dir target` for `cv`, `cv-mcp`, `cvd`) and `make version` compare the installed binary with the checkout.
- `cargo fmt --check` and `cargo clippy -- -D warnings` are CI gates, run before the build; both were advisory. Toolchain pinned to 1.99.0 (`rust-toolchain.toml`).
- CI runs `dist plan`: the misconfiguration that produced zero artifacts for every tag from v0.9.22 to 0.11.0 now fails before a tag.

## 0.11.2 — `cv stats --tokens`

- **`cv stats --tokens`** totals token usage per harness and model over any query slice:
  uncached input, cache writes, cache reads, output, reasoning, provider cost. It widens each Claude
  session to its sub-agent and `Workflow`-agent forest, which the catalog does not list and which
  on an orchestrating session holds most of the spend. It counts a Claude response once even though
  Claude Code writes one line per content block and copies history into resumed sessions (deduped
  by API `message.id`). `--json` adds a `tokens` object; without the flag the payload is unchanged.
- **`Usage` has one meaning across harnesses.** `input_tokens` is now the *uncached* prompt
  everywhere (Anthropic's convention), with `Usage::prompt_tokens()` / `total_tokens()` for the
  sums. Codex and Gemini report cached tokens as a subset of input; their adapters now subtract on
  parse and their emitters add back on emit. Before, a Codex → Claude port wrote cached tokens
  twice into the Claude `usage` block, and `cv doctor` sized a Codex session's peak context at
  roughly double. **Library consumers reading a Codex or Gemini `Usage.input_tokens` see a smaller
  number now; add `cache_read_tokens` for the old one.**
- **Codex: a re-emitted `token_count` no longer counts as a second call.** Codex repeats its last
  snapshot (same running `total_token_usage`), for example after a user message. The adapter paired
  only the first copy with its `token_usage_record`, so each repeat attached stale usage to the next
  reply or to a synthetic carrier. On one 7,700-snapshot rollout that inflated usage from 834.5M to
  1.06B; after the fix cv matches the rollout's own `thread_token_usage` to the token.

## 0.11.1 — packaging fix

`clustervision-core` 0.11.0 could not be published to crates.io. `formats.rs` embeds the 22 format
manifests with `include_str!`, reaching from `crates/cv-core/src/` up to a `formats/` directory at
the workspace root — and `cargo package` only puts files from *inside* the crate into the tarball,
so the packaged crate had no manifests and would not compile. The manifests are cv-core's data, and
`cv formats census` needs them on a machine with no checkout, so they now live in
`crates/cv-core/formats/`.

Nothing about the binaries changes; v0.11.0's release assets are unaffected. This version exists so
that the tag, the release binaries and the published crate are all the same source.

Worth recording: nothing caught this for a whole release, and the only reason CI did not discover
it halfway through a six-crate publish is that `CARGO_REGISTRY_TOKEN` was never set. `cargo package
-p <crate>` belongs in the pre-tag checklist, since it is the only thing that builds a crate from
the tarball it would actually ship rather than from the working tree.

## 0.11.0 — the clean break

This release renames, regroups and restructures on purpose, with **no aliases for old names**
(an old command or flag errors with a pointer to the new one). The contract is
`docs/INTERFACE-V2.md`. Principle: one name, one meaning, no grammar to memorize.

### Breaking changes

- **Commands.** `convert` is folded into `port` (`port <id> --harness <h> [--cwd <dir>] [--out <dir>]`);
  `query` is `schema`; `recall` and `distill` are gone (`pack` is the one "build context from the
  corpus" verb); `prune --retrieve` is `cat <session> <tool_use_id>`; `prune --range` is
  `prune --keep A..B`; `prune --thinking` is `prune --drop-thinking`, because `--thinking` now
  names what an *emit* does with reasoning and one word may not mean two things. Every command is
  visible in `cv --help`, grouped Read / Reshape / Export / Fleet & live / System; `cv recipes` is
  the agent quickstart.
- **Windows.** `--first N`, `--last N`, `--range A..B` (0-based, end-exclusive; `A..`, `..B`),
  `--around N [--context K]`, `--max-bytes N` — the same five on `show`, `export` and the MCP `show`.
  The old `--range -N` (which meant the FIRST N) is gone.
- **JSON is snake_case everywhere.** `ls --json`: `message_count`, `created_at`, `updated_at`,
  `size_bytes`, `display_title`; `search`/`timeline`/`workflow --json` likewise; every session row
  has the same keys. Blocks in `show --json` are tagged `type` (was `kind`); a message has a `kind`.
- **The IR.** `Message` gains `kind` (prompt · reply · tool_result · injected_context · system_prompt
  · notice · compaction_boundary · compaction_summary · model_change · error · subagent_spawn ·
  subagent_return · branch · carrier) and `origin` (human · model · harness · hook · scheduler ·
  subagent · import); `Session` gains `system_prompt` and `lineage` (forked_from, parent,
  spawned_by_tool_use, continued_in, continues, agent_path); `Block::ToolUse` gains `namespace`;
  `Usage` gains `reasoning_tokens` and `cost_usd`. Harness-specific facts live under
  `extra["<harness>"]` (one bag per harness) — never as flat keys. Facts cv produces itself rather
  than reading from a store (parse diagnostics, and the provenance stamped on a session that `loom`
  or `splice` synthesized) live under `extra["cv"]`, so a session's `extra` has no flat keys at
  all.
- **MCP.** Tools are generated from the CLI (`cv schema --commands --json`): same names, flags and
  output as the commands; `show` defaults to the last 50 messages / 200 KB over MCP.
  `read_session`, `search_sessions`, `list_sessions`, `project_sessions`, `recall`,
  `prune_session`, `prune_retrieve` are gone (`show`, `search`, `ls`, `ls --cwd`, `pack`, `prune`,
  `cat`).
- **Downstream consumers** of `cv ls --json` / `cv show --json` must switch to the snake_case keys
  and the `type` block tag. Ours (`sesh`) is updated separately, after this release.

### New

- **`cv cat <session> <tool_use_id>`** prints one tool call's full output wherever it lives:
  inline in the transcript, in a `prune` sidecar, or in a persisted-output file. `--input` prints
  the call's arguments instead. This replaces `prune --retrieve`, which could only reach sidecars.
- **`cv recipes`** is the agent quickstart: the ten things an agent does with cv, each as one
  command line plus the JSON keys it returns. `cv --help` points at it.
- **`cv schema --commands --json`** dumps the whole command tree, every command and flag with its
  help text. The MCP server builds its tool list from it, so the two cannot drift.
- **`cv formats census` and `cv formats check`** audit cv against the harnesses. `census` parses
  recent real sessions in format-complete mode and reports the record vocabulary each adapter did
  NOT interpret, so a harness changing its format shows up on real data the day it lands. `check`
  compares every adapter's match arms against a pinned manifest in `formats/<harness>.toml`, which
  names the upstream commit it was verified against. `tools/harness-drift.sh` re-checks the
  manifests against the upstream checkouts, and `tools/harness-fixtures/` regenerates each
  real-writer fixture. All of it is now run rather than merely shipped, which is how we learned
  that the Codex smoke check had been proving nothing (it exited before the model call) and that
  `OPENAI_BASE_URL` does not redirect Codex 0.155.1, so the "dead endpoint" it relied on was
  letting a real request leave the machine. Containment is a provider override now, the drift
  script is read-only by default, and every generator writes where you tell it instead of over the
  committed fixture.
- **`cv port --strict`** fails the port when the fidelity check finds a loss the target format
  could have carried. Losses the format inherently cannot hold are still only reported.
- **`--thinking <native|text|drop>`**, on every command that writes a session out (`port`,
  `splice`, `loom`, `pack --format session`). Thinking blocks come in two shapes: a few carry
  plaintext, most carry only an opaque provider signature with no text at all (on one real session,
  177 of 204). Nothing but the store that produced it can hold that blob, so the only question a
  mode can answer is whether the *turn* survives. `native`, the default, keeps structured reasoning
  where the target holds it and drops it elsewhere, which can quietly cost a third of the assistant
  turns. `text` never loses a turn: reasoning with text becomes text, a signed blob becomes a short
  placeholder, and the lost signature is still reported so a lossy port does not start looking
  clean. `drop` omits reasoning entirely, for handing someone a session without your chain of
  thought.

### Fidelity: conversions now carry what they always should have

The round-trip verifier used to compare six counts, so it reported "clean" on paths that were
dropping most of the session. A per-field diff of every conversion path found that nearly all of
them silently lost every usage record, every per-message model, signature-bearing thinking, the
tool name on every result, and every structured result payload; `claude → hermes` also lost the
working directory and `claude → codex` the title. All of that is now carried where the target
format can hold it, and the verifier diffs every field it could have carried, classifying each
loss as expected for that format or unexpected. On a real 900-message session `--strict` now passes
into **all seven** emit targets. Every remaining "inherent" verdict is backed by evidence rather
than assumed: Hermes's schema-30 store has no column for a tool result's error flag and its own
writers take no error argument; Grok records the model it will resume against, so a foreign model
id there is a deliberate rewrite; OpenCode and Gemini fold a tool result into its originating
call's record, so a tool turn never had an id of its own to carry, and Gemini's reader applies
gemini-cli's own rule that a `/`-leading turn is a command typed at the client rather than a
prompt, which re-tags the turn without losing it.

Getting there turned up real bugs that no test had caught, because nothing had compared a
conversion field by field on a real session before: an assistant turn whose only content was
provider-signed reasoning vanished, taking a third of the assistant turns with it; a tool result
whose call had been pruned away was dropped by three emitters and made Codex log an orphan on every
resume; a Gemini tool call that completed with empty output lost its whole turn; and usage never
crossed into Claude from another harness at all. The OpenCode emitter writes OpenCode's SQLite
store, which is what OpenCode 1.18 actually reads; it had been writing the superseded JSON tree.

### The release pipeline, the desktop app and the web UI

Three things had been quietly broken for a release or more, all of them invisible because nothing
in CI looked at them. They are fixed, and CI looks at them now.

- **Tagging produced nothing.** `dist-workspace.toml` said `publish-jobs = ["cargo"]`, which
  cargo-dist rejects while parsing the file — it has no built-in crates.io job, only `homebrew`,
  `npm` and custom `./name` jobs. The `plan` step therefore died before a single artifact was
  built, so every tag since just after v0.9.22 shipped neither binaries nor a crates.io upload, and
  the failure read like a TOML error rather than a missing feature. There is now a real
  `./publish-crates` job that publishes the six library crates in dependency order and tolerates a
  re-run. Verified by running `dist plan` and a full local `dist build` for the host target, which
  produce the seven-target manifest and a working packaged binary.
- **The desktop app had not compiled since the 0.10 crate rename.** It declares its own
  `[workspace]`, so `cargo check --workspace` never saw it, and it asked for a package named
  `cv-core` after that package became `clustervision-core`. Behind that one-line break sat five
  semantic bugs: its session rows were not the §3 shape, `local_messages` ignored the `extra`
  argument the UI had always passed (so tool-result `details` could never reach the desktop), the
  sub-agent listing missed the entire workflow tier, and two commands the UI has invoked since
  0.9.12 did not exist and silently returned null. A new `local_session_head` exposes a session's
  `system_prompt` and `lineage`, which is otherwise unreachable for Claude.
- **The web UI read the pre-0.11.0 block tag,** so every transcript, diff and forest screen was
  broken. It reads `type` now, and uses the new vocabulary rather than painting every
  non-assistant turn the same: a prompt, harness-injected context, a notice, an error and a
  compaction boundary are visually distinct, an unknown kind renders honestly instead of
  vanishing, and lineage is navigable. `web/selftest.html` carries 37 rendering invariants, run in
  a real browser, so the next IR change fails loudly.
- **The daemon's rows now match everyone else's.** `/api/sessions` was missing `path` and
  `size_bytes`, and `/api/touched` dropped the `agent_id`/`parent_id`/`workflow` trio it had always
  carried, so a consumer could tell which door a row came through. Both are pinned by tests.
- **`GET /api/session/<harness>/<id>/head`** answers what a session knows about *itself* — the
  session row plus `model`, `git`, `system_prompt`, `lineage` and the exact message `total` — with
  no messages. A windowed read structurally cannot: it stops once its window is full, and Claude
  writes its system prompt well after the opening turns, so opening a transcript at the top could
  never learn it. The desktop app's `local_session_head` is the same thing through the other door.
- **A missing static asset is a 404 again.** The dashboard server answered any missing path with
  the single-page index, so a module the browser could not find came back as HTML under a `.js`
  name and was rejected with "Expected a JavaScript-or-Wasm module script" — which reads like a
  server misconfiguration rather than the truth. Routes still fall back; assets do not.

### OpenSession 0.3 — the spec cv can actually speak

cv publishes [`docs/OPENSESSION.md`](docs/OPENSESSION.md), a proposed interchange format for agent
sessions, and told you "clustervision's in-memory IR is the reference implementation, and
`cv export --format json` emits it". That was false on every axis: the document specified camelCase
with blocks tagged `kind: "toolResult"` while the export emitted snake_case with
`type: "tool_result"` and no version marker at all — and **no Rust code could read or write the
format**. The only implementation was a dozen lines of translation in the browser.

0.3 resolves it in the direction that leaves one vocabulary: the spec adopts the IR. It gains the
concepts IR v2 added — message `kind` and `origin`, session `system_prompt` and `lineage`, tool
`namespace`, reasoning tokens and cost — and promotes system prompts and cost out of the old
"deliberately not in scope" list, since every harness that records them records the same thing.
`cv export --format json` now emits a valid `open_session: "0.3"` document, and a new `opensession`
harness reads one back through the registered export sources, tolerant of the 0.2 spellings. A real
900-message session round-trips with an identical message, kind and block census; the one field
that differs is `source_path`, which honestly names the document the reader actually opened. The
browser's whole camelCase translation layer is gone with it.

### One name per thing, enforced

The release's thesis applied to itself. cv answers the same questions through several doors —
`cv <cmd> --json`, the daemon's HTTP API, the MCP tools, the desktop app — and a consumer should
never have to ask which door a row came through before reading it. Asking each door the same
questions, and sweeping every type that carries both a serde spelling and a canonical accessor,
turned up six places where it mattered:

- **A harness had two spellings.** `Harness` derived `rename_all = "lowercase"`, which spells the
  *variant*, so `KimiCode` serialized as `"kimicode"` while `as_str()`, `cv --harness` and
  `cv ls --json` all said `"kimi-code"`. Filtering on the name returned nothing from
  `cv show --json`. Serde goes through the canonical name now, for all five hyphenated harnesses.
- **An instant had three spellings**: `…Z` from `/api/sessions`, `…+00:00` from `/api/search` and
  from the CLI. Every door uses `to_rfc3339()`.
- **Sub-agent provenance was dropped twice.** `/api/touched` and `/api/session/…/events` both
  omitted the `agent_id`/`parent_id`/`workflow` trio they had always carried, so a caller could not
  tell a top-level run from one lane of a workflow.
- **Compactions were the odd list out.** That route was the only one wrapped in an object rather
  than served bare, and renamed two fields on the way: the CLI's `boundary_msg_idx` was `index`,
  its `pre_compaction_span` was `pre_span`. The daemon serializes the same struct the CLI does now,
  and adds `headline` rather than renaming anything.
- **Session rows were missing two keys** (`path`, `size_bytes`) from the daemon, so its rows were
  not the row §3 describes.
- **Codex session sizes were badly wrong in `cv ls`.** A rollout over 8 MB is sampled rather than
  read, and the estimate extrapolated from the head — exactly where a rollout keeps its largest and
  least representative records. A real 10.9 MB session reported 74 messages against a true 198.
  Extrapolating from every sampled byte gives 189; across three rollouts the error fell from 63%,
  58% and 25% to 4.5%, 19% and 1.6%.

`crates/cvd/tests/parity.rs` is what notices next time: it asks both doors the same questions and
compares key sets, allowing a door to ADD a field but never to rename or drop one, and pins the
timestamp spelling and the rule that a list is a bare array. It was verified the way a guard should
be — rename a field, watch it fail, put it back, watch it pass. The enum sweep found no remaining
type that spells itself two ways.

### Also in this release

- **`cv workflow <session> <run> --revive` salvages a dead run.** A `Workflow` run that dies
  mid-flight (session limit, kill, crash) loses every in-progress lane at once, and the
  orchestrator's state file keeps only a ~400-char preview of each prompt, so re-running the
  script restarts every lane from zero. `--revive` mines each lane's own transcript
  (`<session>/subagents/workflows/<run>/agent-<id>.jsonl`) for the FULL original prompt plus the
  work it already landed — files written or edited, distinct commands run, its last substantive
  note — and prints a ready-to-paste standalone `Agent` prompt per unfinished lane
  (`--revive-all` includes finished ones), so lanes come back resumed rather than restarted and
  without the workflow runtime.

- **`cv prune --revive` works again on Claude Code ≥ 2.1.277 — the pin now lands in
  `usage.iterations` too.** Claude Code's resume gate sizes the loaded context from
  the last real assistant record's `usage`, and since 2.1.277 it prefers the last
  request entry of that record's `iterations` array (skipping `advisor_message` /
  `compaction` iterations) over the top-level counters. Revive pinned only the top
  level, so the stale wall figure survived inside `iterations`; the gate read it,
  and — with the total at or above *context window − max-output reserve (≤ 20k) −
  3k* — refused the (now small) session client-side with a synthesized `Prompt is
  too long` (no `errorDetails`, no request ever sent), on every resume attempt,
  whatever `--window` was. Now `usage_total` reads a record exactly as Claude Code
  does (the last non-auxiliary iteration when well-formed and non-zero, else the
  top level), so `--window` sizing, the honest figure, stale-record detection and
  the `_cv_orig_ctx` stash all agree with the gate, and revive pins every
  non-auxiliary iteration alongside the top level (output counts untouched, caches
  zeroed). Regression test: `revive_pins_usage_iterations_too`.

- **Claude Code fidelity catch-up (2.1.23x → 2.1.278).** The transcript format moved
  under cv; this brings the adapter, prune and doctor back in line with what the
  files actually hold:
  - *System reminders are content, not UI state.* The `<system-reminder>` text Claude
    Code appends to the prompt — hook output, edited-file notices, queued task
    notifications, CLAUDE.md `instructions`, skill/agent listings, … — has lived in
    separate `attachment` records (with `rendered[].content`) since ~2.1.23x, and cv
    dropped them. Rendered attachments now parse as System turns carrying exactly that
    text (`extra.attachmentType` names the kind; the full object under `full`/`complete`;
    large text spans lazily), so `cv show`, `search`, `dataset` and `doctor` see what the
    model saw. `cv doctor` itemizes them as **system reminders** by kind instead of
    misreading them as fixed system-prompt overhead (~90k of a 327k "overhead" in one
    live session). Unrendered attachments stay bookkeeping.
  - *Synthetic notices are not turns.* Claude Code's client-side `assistant` rows with
    `model: "<synthetic>"` ("Prompt is too long", "No response requested.", …) are never
    sent to the API. The lean passes now surface them as System notices
    (`extra.subtype` = `api_error`/`synthetic`, with `error`/`errorDetails`), they no
    longer count toward `cv ls` message counts, `--keep-last`, `--range` indices or
    `--window` sizing, and they never name the session model. `complete` keeps their
    original shape for round-trips.
  - *Titles and bookkeeping records.* `custom-title` (`/rename`) now outranks `ai-title`
    in `cv ls` and `Session.title`; `agent-name`, `tag`, `relocated`, `continued-in`
    (session lineage) and `pr-link` land in `Session.extra`; every other non-conversational
    record (`cost-state`, `atis-latch`, `frame-link`, `content-replacement`, artifact
    watches, and anything future without a `message`) round-trips as a carrier under
    `complete` instead of being silently dropped. `summary` records — which Claude Code no
    longer writes — are still read.
  - *Persisted tool outputs.* A `<persisted-output>` stub (the real output went to
    `<session>/tool-results/<id>.txt`) keeps the stub as content — that is what the model
    saw — and records the path and size in the block's `details.persistedOutput`; `cv show`
    prints the on-disk pointer under the result, and `cv index`/`search` index the file's
    head (up to 1 MiB) and scan it for live snippets, so text the tool produced is findable
    even though the transcript holds only the pointer. `cv doctor`'s verdict names system
    reminders when they are ≥ 10% of context.
  - *prune:* a windowed tail keeps the LAST of each session-level singleton record
    (`custom-title`, `ai-title`, `tag`, `agent-name`, `relocated`, `cost-state`,
    `atis-latch`, `continued-in`, legacy `summary`) instead of only `summary`; both id
    spellings (`sessionId` and `session_id`) are restamped; byte estimates (the honest
    figure and the no-usage fallback) count rendered attachments as prompt text; and
    `--thinking` snips every old thinking block regardless of `--min-size` — Fable-era
    signature-only blocks are ~700 bytes on disk but hundreds of tokens on the wire
    (Claude Code's own `thinking_drop` freed ~105k for 236 of them).

- **Harness catch-up, from source (2026-09-19).** Every harness we have a checkout for
  (`~/pug/*`) was fast-forwarded and its persistence code diffed against what the adapter
  targeted; three of six had moved their live store out from under cv without any error.
  `docs/FORMATS.md` is re-verified throughout.
  - *Codex (0.154).* The parser handles the paginated history mode (`item_completed`
    items as System notes with the full item in `extra`, since the `event_msg`
    user/agent twins are gone), the inter-agent `agent_message` channel, `token_usage_record`
    as the authoritative per-response usage (`cache_write_input_tokens` mapped), fork/
    subagent rollouts (the embedded parent prefix is skipped in lean passes and tagged under
    `complete`; `create_time` fixes fork-stamped timestamps), the grown `session_meta`
    (swarm fields into `Session.extra`), `thread_settings_applied`/effort model tracking,
    `turn_aborted`/`thread_rolled_back` notes, tool `namespace`, and `.jsonl.zst` rollouts.
    The emitter now writes rollouts Codex can resume: local-time file names, a complete
    `turn_context`, string-form error outputs (`[error] ` prefix, restored on parse), a
    guaranteed `cwd`, `history_mode: legacy`, `model_provider`, summary-only reasoning.
  - *OpenCode.* Reads the canonical `opencode.db` (the JSON tree importer was deleted
    2026-06-02); JSON is a fallback deduped by id; session facts, tool attachments and
    `state.{title,metadata,time}` are preserved; the db is file-watched for freshness.
  - *Hermes (schema 30).* In-place compaction visibility (`active`/`compacted`), `ORDER BY
    id`, compaction summaries as boundary pairs, `system_prompts` table, lineage markers,
    listing parity, `cwd`/`git_branch` first-class; old-schema dbs unchanged.
  - *Goose (schema 16).* Titles from `name`; `metadata_json` usage/model; `document` and
    `error` blocks; bare-array and rmcp-3 tool results; millisecond timestamps.
  - *Gemini/Qwen.* Second (sandbox) storage root; cwd from `projects.json`/`.project_root`.
  - *OpenClaw.* Live sessions and transcripts moved to `agents/<id>/agent/openclaw-agent.sqlite`
    on 2026-07-11; discovery now reads `session_windows`/`session_nodes` and replays
    `transcript_events` (each row is the old JSONL line) through the same parser, deduping
    legacy JSONL twins; v4 `leaf`/`appendMode: side` branch controls are followed (a port of
    OpenClaw's tree navigation) so only the visible branch is emitted in lean passes;
    `compaction`/`reset`/`branch_summary`/`custom_message` become System notes,
    `session_info.name` the title, `model_change` the model; checkpoint/trajectory/archive
    siblings are excluded from discovery.
  - *Kimi Code (new harness `kimi-code`).* kimi-cli is deprecated and `~/.kimi` frozen since
    the 2026-06 migration; its successor writes `~/.kimi-code/sessions/wd_*/session_*/agents/
    <id>/wire.jsonl` (flat records, `time` in ms, object `args`, camelCase usage, `cwd` in
    `state.json`) — 34 sessions on this machine were invisible. The new adapter discovers the
    workspace tree (honouring `session_index.jsonl` tombstones), buffers each LLM step into one
    Assistant turn plus Tool turns (usage from `step.end`), maps compaction, task-termination and
    cancel notes, system prompts from `profile.bind`, injected prompts with their `origin`, and
    persisted `tool-results`/`tasks` outputs as on-disk pointers; sub-agents ride in
    `Session.extra.agents`. `cv resume` launches `kimi --session session_<id>`.
  - *Verified against the harnesses themselves, not just their source.* Real stores were
    generated by running each harness (in throwaway homes) and are now fixtures with tests
    pinning what the real writer produced: a Goose 1.51.0 `sessions.db` (stub-provider turns,
    a retry-exhaustion error, a `goose session import` of a Claude transcript), a Hermes
    schema-30 `state.db` written through Hermes's own store API (in-place compaction, branch/
    reset/delegate children, `system_prompts`, a foreign import), an OpenClaw
    `openclaw-agent.sqlite` written by its store modules (branches, fork, legacy import), and
    Gemini 0.46.0 registry files. Codex 0.155.1 itself resumed a cv-emitted rollout (session
    header and history loaded; only the model call failed, by design), its compression worker
    produced `.jsonl.zst` files that cv lists and parses, and `$CODEX_HOME` is honoured.
    Findings folded back: Goose hides `userVisible: false` rows the way Goose does; Hermes lists
    archived/hidden sessions (Hermes keeps them resumable) with the flags in `extra`, titles a
    rotation tip from its root, and surfaces `origin_json.imported_from`.

## 0.10.0 (2026-07-17)

- **`cv ls --json` closes the consumer gaps (#15).** Every row now carries
  `sizeBytes` (the transcript file's length — free from the same `stat` that
  already guards a since-deleted row, so it is always present and needs no extra
  I/O). A new `--enrich` flag (with `--json`) adds two transcript-derived fields:
  `git` — the same `branch`/`commit`/`remote` object `cv show --json` emits, read
  from the transcript's recorded git context (the branch the session *ran on*, a
  historical fact — not the cwd's current branch) — and `displayTitle`, the row's
  `title` with a fallback synthesized from the first real user turn (peeling a
  leading `<system-reminder>` block and skipping the `Caveat:` preamble, bare
  command wrappers, and tool-result turns; explicit `null` only when a session
  has neither an explicit title nor any user prose). `title` itself is left
  untouched, so existing consumers see byte-identical keys. Enrichment costs one
  lazy transcript parse per emitted row — O(`--limit`), not the whole fleet — so
  plain `cv ls --json` stays a catalog-only, milliseconds-warm read. The `ls`
  freshness contract is now documented (manual `cli.md`): brand-new sessions are
  always seen on the next read (a new file bumps a watched dir mtime); the only
  bounded blind spot is an in-place append to an older session outside the
  top-50, which lags at most `CLUSTERVISION_MAX_STALE_SECS` (default 900s); and
  `--fresh` forces a full re-discovery.
- **Every observed fact now carries its source and freshness — `Unknown` is
  first-class.** A land is no longer a bare boolean; it is "observed landed,
  git-verified, as of a pass 4m ago". A new pure `Provenance { observed_at,
  source, freshness }` shape (cv-core `task/provenance.rs`) rides the debt and
  show surfaces: `source` distinguishes `git-verify` from a `self-report`ed
  `Done` (the completion carve-out made structural, never silently equal to a
  verified land), and `freshness` (`Fresh` / `Stale{age}` / `Unknown`) is
  derived purely from the verifier heartbeat — a revision the verifier has never
  checked since it became verifiable reads `Unknown`, not implicitly fine. Human
  output gains `landed · observed 4m ago` / `ready … · NEVER verified`, `cv task
  show` labels the landed line `git-verified` and a `Done` `self-reported`, and
  additive `provenance` keys ride `cv task debt --json`, MCP `task_debt`, and
  `GET /api/tasks/debt`. No new git calls; freshness takes the heartbeat as input.
- **The task substrate (`cv task`, `task_*` MCP tools, `/api/tasks*`).** Durable,
  replayable dispatch objects for agent fleets, designed against the failure mode
  that killed a 200k-line predecessor (mission-control): progress trackers that
  trust agents' own claims. Four laws: (1) landing state is **observed, not
  attested** — only cv's git verifier (ancestry + `git cherry` patch-id
  equivalence + whole-branch range patch-id recomputed from a recorded base) can
  write `merged_local`/`landed`, and agent-facing append paths reject those
  kinds at the store seam; (2) reviewer independence is **read from transcripts**
  (harness family via the catalog), advisory-warn, never a gate; (3) **no
  authority machinery** — landing authority is whoever can push, cv only tracks
  and verifies; (4) **small**. Lifecycle grammar and its 9-test invariant roster
  ported from mission-control's one pure module (`land_request.rs`): reviewer
  binding, refute-is-terminal, `MergedLocal ≠ Landed`, issues-without-state-change,
  merge/land evidence must match reviewed content. Storage is a locked CAS
  append log (`tasks/events.jsonl`, board's flock recipe via the extracted
  `lockfile::FileLock`) that never contains an event replay would refuse;
  interior corruption is loud, torn tails tolerated. Projections: `list`,
  per-agent `inbox`, and `debt` — reviewed-but-unlanded work, the honest number.
  `cvd watch --verify-interval N` runs the verifier as a daemon; board channels
  carry the notification trail.
- **The trust layer watches itself.** `run_verify` writes a heartbeat
  (`tasks/last_verify.json`); every debt surface renders verified-as-of and
  warns NEVER/STALE, and `cvd`'s verify interval defaults on (300s). Replay
  **quarantines** reducer-refused events loudly instead of bricking every
  consumer at once (appends fail closed on degraded reads; log format header
  v1). Landed revisions are **re-observed every pass**: a forged `Landed`
  contradicts observation within one tick and becomes a SUSPECT debt row —
  and suspects persist across partial verifies, so a targeted re-verify
  cannot launder one away. `cv`/`cvd --version` and the cvd startup log embed
  the build commit.
- **The adversary gym, and three closed holes.** `crates/cv-sim` ships a
  deterministic synthetic-fleet generator, a replay-cost bench (the O(n²) append
  curve *measured*, not hand-waved), and an **adversary gym**: attack tests that
  assert each defense fires, alongside honest `pin_*` tests that named the
  sensors' known weaknesses so closing one is a measurable git event. This
  release closes three of them:
  - **Review receipts require engagement, not a quoted sha.** `saw_change` now
    demands structural evidence the reviewer opened the change — a repo
    file-read, or a real `git diff`/`show`/`log` in *command position* — so
    `echo <sha>` no longer passes (it reads `undetermined`). Still a heuristic
    (a pointless repo read passes), but the trivial forgery is dead.
  - **Verifiable completion.** `cv task done --check-cmd/--check-file/--check-http`
    makes cv RUN a completion predicate: a passing check records the `Done` as
    *observed* (provenance `checked`), a failing one refuses it and leaves the
    task open. Law 1 now reaches non-code tasks; a check-less `Done` stays
    self-reported (and is labeled as such).
  - **Per-endpoint identity (TOFU).** An endpoint binds a token on first use
    (`--token` / `$CV_TOKEN`); thereafter an identity-bearing event
    (claim/release/propose/pass/refute) stamped as that endpoint must present
    the matching token or the CAS append is rejected. Unbound endpoints stay
    trusted (the solo/human case) — authentication of the `by` claim, not
    authorization; no seats, no roles, and only the token's hash is stored.

  Receipts and independence remain **advisory, never a gate**; the sensors
  inform judgment, they do not control it. Review **receipts are a heuristic
  signal, not proof**, and law 1 is spelled out as covering **landing** — and
  now, via `--check-*`, verifiable **completion** — with a check-less `Done`
  honestly labeled self-reported. Doc-comments and the manual state all of this
  plainly instead of implying more.
- **Task identity comes from the environment.** `CV_ENDPOINT` is the identity
  convention (the spawner sets it; `--from` overrides); identity-bearing verbs
  (claim/release/propose/pass/refute) refuse to act with neither present
  rather than silently recording a shared sink. Age is the escalation
  mechanism: list/inbox gain age columns (oldest first, ⏰ past 24h) and the
  debt view gains an awaiting-review section anchored on the new
  `proposed_at` — a dead reviewer is now visible, aging state on the owner
  surface. `cv_core::sanitize` strips ESC/OSC/control characters at every
  terminal render seam (tasks, plus `ls`/`timeline`/`search`/`recall`/`show
  --subagents`; JSON transports stay raw — escaping is the transport's job).
- **Below-the-line audit followups.** `propose` warns when another live task
  carries the same branch or worktree (observation, never a block). Verifier
  issue dedup widens to the last 5, so alternating findings stop growing the
  log forever. `cv board unanswered` / `board_unanswered` / `GET
  …/unanswered` surface requests nobody answered, oldest first with age.
  Reviewer-independence checks read the *last assistant model* from
  multi-model harness transcripts and map model-id prefixes to families —
  Cursor-style harnesses no longer force `undetermined`.
- **One shape, one identity across CLI/MCP/HTTP.** `task/views.rs` owns the
  row types (`TaskRow`/`InboxRow`/`DebtRow`/`AwaitingRow`) and
  `DebtReport::compute`; all three front-ends consume them, so the surfaces
  cannot drift. `--state` now rejects unknown vocabulary naming the valid set
  on every surface (was a silent empty result). MCP board identity routes
  through `CV_ENDPOINT` (one-release legacy-owner transition on
  `board_release`). New coverage: MCP task-tool race smoke + `cvd`
  `/api/tasks*` route tests.
- **Agent-id drill-down everywhere.** `find`/`find_cheap` fall back to
  workflow sub-agent resolution after normal lookups miss, so MCP
  `read_session` and every front-end inherit `cv show <agent-id>` behavior.
  `cv workflow --results` falls back to per-agent transcript returns when
  `journal.jsonl` is absent — crashed runs still harvest fully. Test-suite
  flake classes killed: cvd tests bind port 0 and parse the real port; git
  fixtures neutralize global/system gitconfig (and the verify suite runs 5x
  faster).
- **`cv search` shows sub-agent hits as lanes.** A folded-in sub-agent hit
  renders its bare `agentId` (what `cv show <id>` resolves) with a
  `⤷ sub-agent of <parent>` tag (+ `⟐<workflow>` when it belongs to one); the
  empty-result path nudges toward `cv index --subagents` only when the index
  genuinely holds no forest.
- **`cv` dies quietly on closed pipes.** SIGPIPE resets to `SIG_DFL` on unix,
  so `cv task list | head` behaves like every other pipeline citizen instead
  of panicking on broken-pipe writes.
- **`cv ls --json`** — the listing as one JSON array on stdout (same rows,
  filters, sort, and `--limit` as the table; camelCase, OpenSession-aligned
  fields), so downstream tools can consume the catalog instead of scraping
  the table or re-scanning transcript files. (@akapug)
- **`cv search --json`** — the hits as one JSON array on stdout (same hits,
  order, and `--harness`/`--limit`/`--semantic` handling as the table), with
  camelCase fields, FULL session ids (the table truncates to 8 chars), the
  untruncated snippet, the relevance score, and the sub-agent provenance trio
  `agentId`/`parentId`/`workflow` — so downstream tools can consume hits
  instead of regex-scraping the table. (@akapug)
- **`cv prune --json`** — the prune report as one JSON object on stdout
  (camelCase; FULL `sourceId`/`newId`, before/after bytes, snipped-payload
  and freed-token counts, revive detail, `newPath`/`sidecarPath`), for
  downstream resume-optimizers that today regex-scrape the human report off
  stderr. Dry-run honest: nothing is written, so paths — and `newId`, unless
  `--to` pinned it — are explicit nulls plus a `note`. The human report stays
  on stderr (compose-family convention), so stdout is pure JSON. (@akapug)
- **`cv prune --declassify`** — snip conversational message *prose* (user
  prompts + assistant text blocks) whose density of caller-supplied terms is
  high, into the sidecar with a `[PRUNED …]` marker — lossless and
  `--retrieve`-able, like every other prune pass. A message is snipped iff it
  holds ≥ 2 distinct terms; unlike the tool/`--thinking` passes it ignores
  `keep_last` (recency is no shield when the whole loaded context is scored).
  The terms are **always external** — `--declassify-tokens` (CSV) and/or
  `--declassify-tokens-file` (one per line, `#` comments) — cv ships no
  built-in list, so `--declassify` without terms is a warned no-op. Also on
  the `prune_session` MCP tool (`declassify`/`declassify_tokens`). (@akapug)
- **Release mechanics.** `clustervision-core` compiles for
  `wasm32-unknown-unknown` standalone again (target-gated `uuid` `js`
  feature; CI checks it). Internal dependency pins track `0.10` so a
  crates.io build can never silently resolve an old core. `cv-tui` and
  `cv-web` are explicitly unpublished (`cv-tui` still ships as a release
  binary); releases now publish to crates.io via cargo-dist `publish-jobs`,
  so crates.io stops lagging the tag.

## v0.9.22 (2026-07-08)

Crash-forensics release, paid for the hard way: a real power loss killed a
~7-lane swarm mid-run, and reconstructing "which orchestrators resumed, which
dropped, and what died holding what" surfaced three blind spots. All three are
now closed:

- **Local time everywhere.** Every human-facing time cv prints (`ls`,
  `timeline`, `events`, `touched`, `tools --timeline`, `stats`, `search`,
  `blame`, `board`) now renders in the local timezone instead of UTC. The
  transcripts store UTC, but `git log` and human memory are local — silently
  mixing the two skewed the reconstructed outage window by the UTC offset
  (a real 4-hour miss during the recovery).
- **`cv ls` shows each session's `created → last-active` span** (minute
  granularity, same-day sessions compress the right side) instead of a bare
  date. A long-lived orchestrator vs a one-shot — and post-crash, resumed vs
  dropped — is now visible in the listing itself.
- **`cv timeline` marks multi-day sessions** with `⇠ since MM-DD`. Feed rows
  sit at last-activity; without the marker they read as start times.
- **Workflows are addressable by NAME.** `cv workflow <session> <name>`
  resolves a run by its workflow name (exact, else unique prefix; a re-run
  name → its newest run), and `cv workflow <name>` with no session resolves
  the name across the whole catalog — session titles are auto-generated and
  rarely mention the workflow you remember ("the stark-kill session" was
  titled "Review recent work across three projects"). A fleet-wide miss falls
  through to a ghost-launch scan, so a swarm that died before persisting is
  still findable by name. Run-not-found errors now list the session's run
  names.
- **The by-name and by-agent fallbacks are fast.** A cheap run-name index
  (script filenames + a byte-scan for `"workflowName"`) means the fleet-wide
  name search parses only matching state files; and the fallbacks now resolve
  through `find_cheap` (catalog + probe, no full re-discovery) with the full
  fleet scan reserved for the genuinely-unknown-id case. Fleet name hit:
  3.9s → 0.3s; direct agent open: 2.5s → 0.1s; ghost hunt: 10s → 1s.
- **Ghosts carry their harvest map.** A ghost launch is enriched from what
  DID survive the crash: its orphaned script file (written at launch)
  recovers the run id, which keys the `subagents/workflows/<runId>/` debris
  dir — reported as `· run wf_… · DEBRIS: 10 agent transcript(s), 1 journaled
  result(s)`. The power-loss ghosts turned out to be sitting on 16
  transcripts + 4 journaled results nobody could see.
- **`cv workflow --follow` (`-f`) tails a live run**: one line per agent
  state transition as the harness flushes the state file, then the full
  render (honoring `--json`/`--script`/`--results`) at terminal status.
  Waits for a run that hasn't registered yet.
- **Full per-agent lane returns.** `cv workflow <sess> <run> --results` reads
  the run's `journal.jsonl` and prints each agent's **complete** journaled
  return value (the state file keeps only a ~400-char `resultPreview`); the
  run's `--json` now carries them as `journal_result` per agent. This closes
  the long-standing "per-agent returns truncated to ~400-char previews" gap.
- **`cv show <agent-id>` works without knowing the parent.** An id that
  matches no session resolves as a sub-agent id fleet-wide (parallel filename
  scan of `subagents/` sidecars, both tiers); one parent → the agent renders
  directly with a provenance banner, several (fork lineages share sidecars) →
  ready-to-paste disambiguation commands. Closes the "workflow sub-agents
  aren't indexed as sessions" gap.
- **The workflow parser stops dropping load-bearing fields.** Now parsed and
  rendered: the run's aggregated **`result`** (the script's return value —
  the harvest payload, previously discarded entirely), `logs` (the script's
  `log()` narration; tail shown, full in `--json`), `args`, `taskId`,
  `startTime`; per-agent `attempt`, `startedAt`, `lastProgressAt`, and
  `lastToolName`/`lastToolSummary` — for a dead or interrupted agent, exactly
  where it was when it stopped. Also fixed a hot-loop prefilter in the
  transcript launch scan (pending-id set instead of parsing every tool-result
  line).
- **`cv workflow <session>` detects ghost launches**: `Workflow` tool
  invocations visible in the transcript with **no persisted run state** — the
  signature of a crash/power loss/hard kill before the harness wrote
  `workflows/wf_*.json`. (The power loss left two such swarms completely
  invisible to the old list — including the one whose uncommitted debris most
  needed finding.) Errored-at-launch calls and `scriptPath` resumes of
  recorded runs are excluded, so no false positives. New core API:
  `workflow_launches`/`ghost_launches` + `WorkflowLaunch`. The list form's
  `--json` output is now `{"runs": […], "ghost_launches": […]}` (was a bare
  array).

## v0.9.21 (2026-07-02)

Security-advisory dependency bumps on top of the v0.9.20 audit, plus a CI fix.

- **anyhow ≥ 1.0.103** — clears RUSTSEC-2026-0190 (`downcast_mut` unsoundness).
- **memmap2 ≥ 0.9.11** — clears RUSTSEC-2026-0186 (affects the optional `mmap`
  feature of `clustervision-core`).
- CI: the no-default-features wasm-config build referenced the pre-rename
  package name `cv-core`; corrected to `clustervision-core` so the check runs.

(Both advisory fixes were contributed by @akapug. The known follow-up: the
default read path memory-maps live, externally-truncatable transcript files,
which is unsound per memmap2's contract — a snapshot/immutability guard before
mmap is the real fix, tracked separately.)

## v0.9.20 — the takeover audit (2026-07-02)

A fresh set of eyes (Claude Fable 5) read the whole codebase, then a swarm of
agents fixed what the review found — in parallel, in one tree, using cv to read
its own sessions along the way. Everything below shipped with regression tests;
the full workspace suite, clippy, the wasm target, and the real-corpus
invariant tests are green.

### Security

- **`cvd serve` no longer trusts the whole internet.** The wildcard
  `Access-Control-Allow-Origin: *` — which let any website a user visited fetch
  their entire transcript corpus off `127.0.0.1:7777` — is gone. CORS is now an
  allow-list (tauri + same-host origins, echoed with `Vary: Origin`); the
  `Host` header is validated on loopback binds (kills DNS rebinding); binding a
  non-loopback address requires `--token`/`$CVD_TOKEN` or an explicit
  `--insecure-expose`; `/api/*` supports bearer-token auth; workers are
  panic-isolated.
- **Redaction got real teeth.** Truncated PEM bodies (the common
  clipped-tool-output case) are now caught; new token families: Stripe live
  and restricted keys, GitLab personal access tokens, Slack session/app
  tokens, npm, Hugging Face, Groq, xAI, DigitalOcean, and Shopify tokens
  (prefixes spelled out in `redact.rs` — not here, because a changelog that
  lists secret-shaped strings gets flagged as a secret itself, which is
  rather the point of this feature); connection-string passwords
  (passwords embedded in connection-string URLs); case-insensitive `bearer`;
  `Proxy-Authorization`; `git.remote` and session/message `extra` maps are
  scrubbed. Root-cause fix: the keyword-blob scanner only ever matched blobs
  at end-of-input — quoted mid-sentence secrets now redact. Assignment
  matching no longer mangles code like `let token = get_token();`.
- **The web viewer bounds zip extraction** (256 MB/entry, 512 MB total,
  header sizes distrusted) and markdown links reject protocol-relative
  `//evil.com` navigation.
- **`cv distill`/`loom --generate` print an egress notice** naming the
  provider, model, and payload size before any transcript leaves the machine.

### Index integrity

- A parse error mid-session no longer commits truncated index docs stamped
  fresh-forever; the error path deletes the partial docs.
- Plain `cv index` no longer silently deletes the sub-agent forest folded in by
  a previous `cv index --subagents`.
- An FTS-fresh but events-stale sub-agent no longer loses its search docs.
- The event catalog keys freshness on `(mtime, size)` like the FTS index — a
  mass mtime bump no longer triggers a whole-corpus re-parse.
- `cv search` without an index caps its in-memory haystack (256 KB/session)
  instead of materializing multi-GB sessions; with a stale index, "no matches"
  now says how far behind the index is.
- Semantic search validates the stored model id and vector dimensions instead
  of silently ranking garbage after a model switch.
- A failed index open during *search* propagates the error instead of deleting
  and recreating the index directory.

### Prune / resume safety

- `--window N` now honors its contract: the kept tail is the largest one
  **≤ N** real tokens (it previously overshot by up to one turn), with a loud
  warning when a single turn alone exceeds the budget.
- Revive derives its "honest" context figure from recorded usage deltas
  (byte-estimate only as a fallback floor) and never rewrites records below
  what the evidence supports — a revived session can no longer sail through the
  resume gate and blow the real context limit.
- Sub-agent sidechain records no longer poison window sizing, revive
  arithmetic, or re-root selection.
- Sidecar payload ids are unique (no more unretrievable payloads behind
  colliding `unknown` ids); `--retrieve` errors on duplicates instead of
  silently returning the last one.
- Windowing keeps the session title record and records trailing the final
  turn; `--drop` markers no longer advertise a retrieval that can't happen.
- Pruning parses each line once instead of ~6× (roughly half the peak memory
  on the GB-scale sessions prune exists for).

### Conversion fidelity

- **`cv convert`/`cv port` actually verify now**: the previously-dead
  `emit_verified` machinery runs on every conversion and prints what the
  target format loses.
- Same-harness ports parse in format-complete mode: Claude→Claude ports replay
  system records (compact boundaries, slash-commands, hooks) with parent chains
  re-linked — no more dangling `parentUuid`s in a rehomed transcript.
- →Codex: the model now survives (emitted as a real `turn_context` record, not
  a misused `model_provider` field) and `is_error` tool results round-trip
  (object form with `success:false`).
- →Claude: user turns with images/files/mixed content emit faithful array
  content instead of being flattened to text.
- `serde_json/preserve_order` is on workspace-wide: ChatGPT exports missing
  `current_node` and Zed multi-tool-result messages no longer get shuffled
  into key-sorted order.
- One emit registry: `Adapter::emit`/`can_emit` (never called, frequently
  lying) are gone; the dispatch table in `emit.rs` is the single source of
  truth and `supported_targets()` derives from it.
- Emitting a lazily-parsed session materializes spans first instead of
  panicking or serializing span structs into the output.

### Core

- Query calculus: bare URLs are needles (with a did-you-mean guard for real
  typos); quoted phrases with commas stay literal; `until:`/`since:` work;
  `updated:2026-06-01` means the whole day, not exactly-midnight; `msgs`
  counts user+assistant consistently across prefilter and full match.
- Board claims use real advisory file locks (kernel-released on process
  death) — the 20-second lock-steal path that could crown two winners is gone.
- The Claude sniffer tolerates transcripts opening with long runs of meta
  records; discovery timestamps are min/max rather than first/last (clock-skew
  robustness); `find()`'s slow path filters vanished files like the fast path;
  the claude seek path re-checks file size after opening (mirroring codex).
- MCP server: requests dispatch concurrently — a 120s `await_omen` no longer
  blocks every other tool call; `list_sessions`/`search_sessions`/
  `observe_stream`/`project_sessions` use the catalog fast path instead of a
  full fleet scan; malformed numeric args are protocol errors instead of
  silent defaults.

### App / UX

- Transcripts from dropped files render windowed (200 at a time) instead of
  freezing the tab on 30k-message sessions; chunk wiring is O(chunk) not
  O(session); large tool blocks have copy buttons; the activity heatmap clamps
  to 6 years so one 1970-dated session can't explode the DOM.
- `cv prune --range` accepts the same grammar as `cv show --range`;
  `cv ls --harness` help lists the full harness set; prune warnings surface in
  the CLI and MCP results.

## v0.9.18 and earlier

See git tags.
