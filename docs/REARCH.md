# Rearchitecture: streaming, seekable, on-demand core

*Draft — ember + claude, 2026-05-31. Follows PRs #1 (hermes profiles), #2 (stream ingest),
#3 (cv dataset).*

> **Historical.** This is the design as argued in May 2026; the CLI it uses (`cv show --range
> 100-120`) predates the 0.11.0 window grammar, and one rule here — the "IR diet", which nulled a
> message model that merely repeated the session default — was **withdrawn** for seekable harnesses
> because it made a record's parse depend on how much of the file preceded it. The current contract
> is [`INTERFACE-V2.md`](INTERFACE-V2.md).

## The one structural fact

`Adapter::parse(&self, r) -> Result<Session>` materializes the **entire** transcript into one
owned `Session { messages: Vec<Message> }`. PR #2 made the *corpus* stream (one `Session` resident
at a time) but a single `Session` is still whole-file: the 1.35 GB Claude transcript expands to
several GB of `Vec<Message>` + owned `String`s + `serde_json::Value` trees. The residual ~27 GB
peak PR #2 flagged is exactly this — `parse` has no sub-session granularity.

Everything downstream inherits it. The fix is to make **the stream of messages**, not the whole
`Session`, the fundamental unit — and to make any single message reachable **by offset** without
re-reading from the top.

## Hotspot map (what actually costs)

| site | cost today | why |
|---|---|---|
| `Adapter::parse` | O(file) RAM per session | whole `Vec<Message>`; no streaming/seek granularity |
| `claude::collect_extra` | `toolUseResult.clone()` per tool turn | a session of 100 file-reads holds 100 file copies in `extra` — and index/search/dataset never read `extra` |
| `searchable_text()` | a 2nd full-corpus-sized `String` | concatenates everything; live-search then `.to_lowercase()` clones it a 3rd time |
| `fts add_doc` | body added twice (`body` + `body_store STORED`) | `body_store` is the ~1.3 GB on-disk index bloat |
| `semantic::embed_all` | builds full `searchable_text` per session | model2vec truncates to **512 tokens** — 99.99% of a big body is built then thrown away |
| `find(id)` | O(all sessions × all harnesses) | discovers every adapter to resolve one id (`cv show` walks the whole fleet) |
| `cmd_search` render | a 2nd full `discover_all()` | `session_date_map()` re-discovers just to get dates that are **already** stored fast-fields in tantivy |
| `cv index` | full `delete_all` + rebuild every run | no incremental path; re-parses unchanged sessions |
| `model` field | duplicated per assistant `Message` | same model string repeated thousands of times |

## The new core abstraction

Two primitives replace "parse the whole thing":

### 1. Forward streaming (the bulk path) — object-safe visitor

```rust
/// What to materialize. Everything off = cheapest. Bulk consumers opt in only to what they read.
#[derive(Default, Clone)]
pub struct ParseOptions {
    pub extra:    bool,             // harness sidecars (toolUseResult …) — fat, rarely needed
    pub thinking: bool,             // reasoning blocks
    pub tool_io:  bool,             // tool_use input + tool_result bodies
    pub range:    Option<Range<usize>>, // message-index window (random access; see §2)
    pub head_bytes: Option<usize>,  // stop after ~N bytes of text (semantic only needs ~512 tok)
}

pub enum Flow { Continue, Stop }

pub trait MessageSink {
    fn meta(&mut self, _m: &SessionMeta) {}     // called once, early
    fn message(&mut self, m: Message) -> Flow;  // called per message; Stop = early-exit
}

pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;
    fn storage_root(&self) -> Option<PathBuf>;
    fn discover(&self) -> Result<Vec<SessionRef>>;

    /// THE primitive. Stream messages into `sink` honoring `opts`. Peak RAM = O(largest message).
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink)
        -> Result<SessionMeta>;

    // Back-compat conveniences with DEFAULT impls — adapters override exactly one:
    fn parse(&self, r: &SessionRef) -> Result<Session> { /* CollectSink over stream(all-on) */ }
    fn emit(&self, …) -> Result<EmitResult> { … }
}
```

**Migration is cheap and incremental.** Each adapter's `parse` already loops lines/rows pushing
`Message`s; converting to `stream` is mechanical (`messages.push(m)` → `match sink.message(m)`).
And because `parse` has a default that collects `stream`, *and* we can give `stream` a default
that replays `parse`, **each adapter overrides only one**. Migrate the heavy file-based ones
(claude, codex, grok, kimi, qwen, gemini, opencode, …) to native `stream` first; the sqlite ones
(hermes, cursor, goose — smaller sessions) can keep `parse` and inherit the default `stream` until
later. Zero big-bang.

Consumers that are a single forward pass move to `stream` + a tiny sink and **never hold the whole
session**: `cv show`, `cv export`, `cv dataset`, `cv index`, `cv redact` (to stdout), live search,
`searchable_text`.

### 2. Random access (the on-demand path) — seekable sessions

Discovery's `scan()` *already reads every line*. Have it also record the byte offset of each
message-bearing line → `offsets: Vec<u64>`. Persist alongside the existing `(mtime,size)` cache
entry. Then:

```rust
fn parse_range(&self, r, range: Range<usize>) -> Result<Vec<Message>>; // seek, read only those lines
```

For jsonl: `seek(offsets[start])`, read `end-start` lines. O(range), not O(file). This is the
keystone for **on-demand**:

- `cv show --range 100-120` on a 1 GB session reads ~20 lines.
- **Web viewer pagination** — wasm fetches message windows instead of a 1 GB blob.
- `cv splice <id>:<start>-<end>` / `cv loom` extract spans without parsing the whole source.
- `cv diff` windows two sessions instead of fully materializing both.

(sqlite adapters: `LIMIT/OFFSET` or rowid range — same interface.)

## IR diet

- **`extra` behind `ParseOptions.extra`.** Biggest single win on tool-heavy sessions — stop
  cloning fat `toolUseResult` sidecars into RAM for paths that never read them.
- **Dedup `model`.** Keep `session.model`; set `Message::model` only when it differs from the
  session default.
- **`Box<str>` for immutable text fields** (shaves capacity slop) — only if profiling justifies.

## Catalog: O(1) `find`, no redundant discovery

Promote `discover_cache` into a real catalog with a reverse **id → (harness, path, offsets)**
index built at load:

- `find(id)` exact = hashmap lookup; prefix = range over a sorted id list. Kills the
  whole-fleet scan. Lazy fallback: catalog miss → one `discover` (which refreshes the catalog).
- Carry `created_at`/`updated_at` **on `Hit`** (already stored as tantivy fast fields) → delete
  `session_date_map()` and its second `discover_all()`.

Open fork: extend the JSON cache vs. move to an embedded KV (`redb`/`sqlite`). The JSON is loaded
whole today; at fleet scale (≈6k+ sessions, the memory notes flag an O(n²) catalog rewrite in
`cvd sync`) a real store earns its keep.

## Search/index efficiency

- **Stop storing full bodies.** Drop `body_store STORED`. Snippet the **top-K hits only** by
  lazily re-parsing those K sessions (`recall_excerpt` already does this as a fallback). K≈20 →
  index shrinks ~1.3 GB → tens of MB; cost is K fast re-parses per query.
- **Incremental indexing.** Per-path `(mtime,size,indexed_gen)`; `cv index` deletes+re-adds only
  changed/new sessions (tantivy `delete_term(id)` + add). Full rebuild becomes `--rebuild`.
  Pairs naturally with `cvd` watch for live incremental indexing.
- **Semantic reads only the head.** Pass `head_bytes ≈ 4 KB` (≳512 tokens) and `Stop` early — the
  1.35 GB session embeds from its first few KB. Lower priority: `embeddings.json` is whole-loaded
  + JSON-parsed per query; fine at current scale, revisit with the catalog.

## Implemented (2026-05-31, session 2)

**Streaming core (Phase 1).** `Adapter::stream(r, opts, &mut dyn MessageSink) -> Session` (returns
metadata, messages go to the sink one-at-a-time then drop). `ParseOptions { extra }` gates the fat
sidecars; `MessageSink::meta()` delivers session metadata ahead of the body for header consumers.
`parse` defaults to `collect(stream)`; un-migrated adapters keep `parse` and inherit a bridging
`stream`. Bulk consumers moved onto streaming: `cv index`, `cv dataset`, `cv show`/`export md`
(hold-back header so a multi-GB transcript renders at O(largest message)), live search.

**Adapters migrated to native streaming:** claude, codex, grok, openclaw — the CLI JSONL harnesses
that produce large sessions. codex needed a two-pass (`has_events` is whole-file) and skips its
`token_count` usage back-ref on the stream path (bulk consumers don't read usage). The rest stay on
the correct bridge by design: **whole-document JSON** (cline, roo, continue, lmstudio, gemini,
chatgpt_app, claude_app — one JSON doc, the `Value` is unavoidable) and **whole-vec post-pass / small
corpus** (kimi positional `attach_*`; hermes/cursor/goose sqlite). No memory pressure there.

**Lean + incremental index (Phase 3 down-payment).** Dropped the stored `body_store`; snippets are
generated **live** from the top hits (re-read capped at 256 KB, `path` stored in the index). Indexing
is **incremental** (skip unchanged by file mtime, reap vanished; `--rebuild` forces full) with periodic
commits. Dates ride on `Hit` (killed the second `discover_all` in search render). Schema auto-migrates
an older on-disk index.

**Measured (real corpus: ~2.2k sessions, 4.1 GB claude + 3.8 GB codex):**

| | before | after |
|---|---|---|
| `cv show` (146 MB session) peak RSS | 183 MB | **30 MB** |
| `cv export md` (146 MB) peak RSS | 198 MB | **31 MB** |
| `cv dataset` (whole corpus) peak RSS | 5.4 GB | **3.7 GB** |
| `cv index --rebuild` peak RSS | 8.3 GB | **4.2 GB** |
| tantivy index on disk | 1.1 GB | **457 MB** |
| `cv index` (no changes) | full rebuild, ~30 s | **incremental, ~10 s** |
| `cv search` | instant | instant, 21 MB, dates+snippets from index |

Remaining floor: one codex session with a single **699 MB JSONL record** (a tool dumped a huge blob
into one line) — O(largest record) can't beat that without intra-record streaming; it's only parsed
when that session changes (incremental). All `cv show`/`export`/search output verified byte-identical
to the pre-change binary across sampled sessions (codex differs only by dropping spurious empty
`token_count` carrier turns — an intended cleanup). Full workspace builds; all tests pass.

## The lazy-content IR (the "never considerable memory" endgame) — foundation landed (Steps 1–2); Phases 2–4 open

The deepest remaining inefficiency: a fully-parsed `Session` **owns every byte** of its content
(`Block::Text { text: String }`, `Thinking`, `ToolResult { content: String }`, plus `extra`/`input`
Values). Streaming drops it per-message, but `parse()` (convert/port/splice/loom/redact/`--json`)
holds the whole thing, and tantivy's `add_text` wants the field once — so the floor for a kept
C-byte field is C. mmap + simd-json only kill the *redundant* work (whole-file read, the Value tree's
copies of discarded keys); they don't beat C for one giant record. A **cap** would hide it behind a
footgun (rejected). The real fix (ember's call): **the IR holds large content as a span, not bytes.**

Design (decided shape: *owned span + lazy resolve*, no lifetime infection):

```rust
pub enum Text { Inline(String), Span(Span) }              // content fields become Text
pub struct Span { source: Option<PathBuf>, offset: u64, len: u64, escaped: bool }
pub struct Resolver { /* mmaps each source on first use */ }
impl Text { fn resolve(&self, &Resolver) -> Cow<str>; fn inline_str(&self) -> Option<&str>; }
impl Session { fn resolver(&self) -> Resolver; fn materialize(&mut self); }
```

- Content `≤ INLINE_MAX` (≈4 KB) stays `Inline`; larger becomes a `Span` into the source. The held
  IR for a 700 MB record is a 16-byte span.
- `Span` covers the **raw** bytes; `escaped` marks a JSON string body to unescape on resolve
  (`serde_json::value::RawValue` at parse time gives the raw span for free, including escapes).
- `Resolver` memory-maps the source (only touched pages page in); unescaped spans resolve by
  borrowing the mmap (zero-copy), escaped ones allocate only the unescaped string.
- Streaming consumers resolve per-block (peak = one field, and can resolve in chunks → a giant field
  never fully materializes). Whole-session consumers `materialize()` first (their choice to hold it).
- `Text` serializes as a plain string when inline; whole-session JSON paths `materialize()` first, so
  `--json`/export stay byte-identical. `Text: Deref<Target=str>` (panics on an unresolved span — the
  invariant: resolve/materialize before reading) keeps most read sites unchanged.

Scope: this flips the central type — **~324 sites** across every adapter + emit + render + the
downstream crates need the `String → Text` migration (mostly `.into()` at constructions; resolve at
the few read-as-`String` sites).

**Step 1 — LANDED (all-inline, byte-identical, green).** The `Text`/`Span`/`Resolver`/`materialize`
foundation is in `crates/cv-core/src/lazy.rs`; the three `Block` content fields are `Text`; all ~324
sites migrated (a structural brace-matched scanner wrapped constructions with `.into()`, patterns and
reads fixed by hand). Parsers still produce `Inline`, so behavior is unchanged — verified
byte-identical to the streaming binary across sampled `show`/`export json`, full workspace + 216+
tests green. This unblocks span production as a *localized* per-adapter change (the broad churn is
done).

**Step 2 — remaining (the actual memory win), bigger than first specced.** The subtlety: today every
adapter parses a record with `serde_json::from_slice::<Value>` (or `from_str`), which **materializes
the big string into the `Value` before** we could span it — so a span built afterward saves nothing.
To actually avoid owning the bytes, the parse must keep large content as `serde_json::value::RawValue`
(it borrows the raw slice without unescaping/owning), and only *then* decide span-vs-inline by the raw
length. That means each adapter's content parse moves off `Value` for the big fields — e.g. a typed
record `{ message: { content: RawContent<'a> }, #[serde(flatten)] rest: Value }` so the heavy
`content` defers to `RawValue` while the small fields stay dynamic `Value` (watch serde's
flatten-vs-borrow interaction). Combined with reading via `BufRead::read_until` (tracking the file
byte offset) so the `RawValue`'s position gives `Span { offset = file_off + ptr_delta, len, escaped }`.

So step 2 is a **per-adapter parse refactor** (Value → RawValue-deferred for large content), not a
localized tweak — and it carries format-drift risk (claude's record is heterogeneous/tolerant).

**Step 2 — LANDED for claude (opt-in), with a sobering empirical finding.** claude's streaming parse
now reads via `read_until` (tracking file offsets), keeps large content as `serde_json::value::
RawValue`, and emits `Span { offset, len, escaped }` (a `debug_assert` re-resolves each span from the
same bytes and checks it equals the inline parse — it held across the *whole corpus* in a debug run).
Spans are **opt-in** via `ParseOptions::lazy()` (`spans: true`); `bulk()`/`full()` stay inline.
Consumers that hold spans resolve per block (dataset) or per message (show/index/live-search/fts
snippet); `parse`/`collect` and emit/`--json`/redact `materialize()` to inline. Verified
byte-identical across 35 sessions (`show` + `export json`) and dataset content (sorted); 217 tests
green; **zero regression** (show 146 MB session 27 MB, dataset-claude 235 MB — same as before).

**The finding:** spans **do not** reduce memory for consumers that *read all content*
(show/dataset/index materialize what they read — for C bytes the floor is C; spans even add mmap
overhead, which is why they're opt-in/off for these). Their payoff is **partial access**: a consumer
that reads a *window* of messages (and metadata-only holds) never touches the unread giant fields, so
they stay 16-byte spans. That's **Phase 2** (`show --range`, web-viewer pagination, splice/loom span
extraction, `diff` windows). Realizing the giant-record win for *full*-content ops instead needs
**chunked content streaming** (resolver → `Read`, consumers write in chunks) — deeper than spans, and
tantivy's `add_text` wants the whole field anyway. Only claude is spanned so far; codex (the 699 MB
record) wants the same `RawValue` parse to make its giant sessions partial-access-cheap.

Net: the lazy-IR machinery is **landed, correct, and ready** (opt-in, no regression); the visible win
arrives with Phase 2 partial-access consumers + codex span production.

### Chunked tantivy ingestion — LANDED (Part A)

The index no longer builds a whole-session body string. `index_all` streams each session through a
`ChunkSink` that flushes a tantivy document every `CHUNK_BYTES` (4 MB) — so a large *multi-message*
session becomes several small docs (all sharing the session id), and the indexer never holds its
whole body. Search **dedups** hits back to one row per id (over-fetch + keep best-scoring doc);
`delete_term(id)` reaps all of a session's chunks on reindex; the per-doc metadata is tiny so the
duplication is cheap. Verified: dedup correct (the only "dup" short-ids are genuinely distinct
codex sessions with colliding 8-char time-prefixes), search finds the right sessions, 224 tests
green. Index rebuild peak 4.2 → 3.9 GB.

### Codex spans + chunk-resolve — LANDED (Part B)

The 699 MB record is a *single* codex `function_call_output` with a plain-string `output`. Codex now
has a span path: with `ParseOptions::lazy()` it **mmaps** the file, iterates line slices (tracking
byte offsets), and for a giant `function_call_output`/`custom_tool_call_output` whose `output` is a
plain JSON string, emits a lazy `Span` of that output (via `serde_json::value::RawValue`, borrowing
the mmap) — **never reading or materializing** the line. `output_is_error` is `false` for a string
output, so the `ToolResult` is reproduced exactly (`is_error=false`, `status="completed"`); a unit
test confirms the span resolves byte-identically to the inline parse (incl. escapes). `ChunkSink`
chunk-*resolves* span content (`Resolver::for_each_chunk` hands out ≤4 MB pieces straight off the
mmap), so the 699 MB output feeds tantivy in 4 MB chunks.

Result: indexing the 699 MB codex output no longer **owns** it — it's reclaimable file-backed mmap,
streamed in 4 MB pieces (the original 65 GB OOM cause is gone). Verified: searching `SandboxDenied`
(a term *only* inside that 699 MB output) finds session `019c6a2b`, so the span→chunk→index pipeline
indexed it correctly. Index rebuild peak 4.2 → 3.4 GB (the residual is the *reclaimable* mmap pages
of the 699 MB output while tantivy tokenizes it — irreducible: indexing C bytes reads C bytes — plus
tantivy's term buffers; none of it is owned/OOM-prone). 218 tests green.

`cv dataset` of that one session still materializes the 699 MB (it's emitting it as a training
example — inherent); everything else (parse, index, search, show) is now streaming/chunked/mmap and
never owns a giant field.

Lower-churn partial win available without the IR change: **incremental tantivy field-fill** — add each
message's text to the doc as it streams instead of concatenating a whole-session `body` String (drops
the index path from O(session) to O(largest message); byte-identical, no new deps).

## Phased plan

- **Phase 0 — bench harness.** `/usr/bin/time -v` (peak RSS) + wall-clock scripts over the real
  multi-harness corpus *and* the 1.35 GB transcript, for `index`, `show`, `dataset`, `search`.
  Numbers gate every later phase (matches the "robustness over real logs" bar).
- **Phase 1 — streaming trait + `ParseOptions`.** Add `stream`/`MessageSink`/options with default
  bridges. Migrate claude first (reference), then the other file adapters. Move `cv index`,
  `dataset`, `show`, `export`, live-search, `redact`-stdout onto `stream`. *Target: `cv index` /
  `cv show` peak RAM O(largest message); tool-heavy sessions stop cloning sidecars.*
- **Phase 2 — seekable sessions.** Offsets in the cache; `parse_range`; `cv show --range`. *Powers
  web pagination + lazy splice/loom/diff.*
- **Phase 3 — catalog + lean/incremental index.** id-indexed `find`; dates on `Hit`; drop
  `body_store` + top-K live snippets; incremental `cv index`; semantic head-only.
- **Phase 4 — streaming emit + web on-demand.** Emit target formats as messages flow (huge-session
  convert/port); wasm viewer fetches ranges via offsets.

## Decisions (ember, 2026-05-31)

1. **Trait shape — Both.** Visitor/push (`stream` + `MessageSink`) is the object-safe core every
   adapter implements; a thin **pull adapter** (bounded channel + worker thread turning the push
   stream into `impl Iterator<Item = Result<Message>>`) is layered on top for the multi-session
   consumers that want to interleave lazily (`diff`, `splice`, `loom`). Targeted windowed access
   still goes through `parse_range`.
2. **Catalog store — SQLite.** Reuse the `rusqlite` dep already in the tree. A `sessions` table
   (`id, harness, path, mtime_ns, size, created_at, updated_at, message_count, indexed_gen`) plus
   an `offsets` blob/side-table. SQL gives incremental upserts (no whole-file rewrite → kills the
   `cvd sync` O(n²)) and the id/prefix lookups `find` needs for free. *Note:* this is the **catalog**,
   distinct from the legacy `cv-core::index` SQLite **FTS** index the roadmap retires in favor of
   tantivy — no conflict. The wasm viewer never opens it directly; it consumes ranges via offsets /
   a served API, so the native-only `rusqlite` bound is fine.
3. **Snippets — live top-K re-snippet.** Drop `body_store STORED` entirely. For the ~K shown hits,
   lazily re-parse + snippet (the `recall_excerpt` path generalized). Index ~1.3 GB → tens of MB;
   cost is K fast re-parses per query. Offsets (§2 seekable) make even those re-parses cheap.

Offsets land in Phase 2, stored in the SQLite catalog (per decision 2).
