# Architecture

For contributors: how clustervision is built. 🔮

cv parses each of 21 harness dialects **into one IR**; everything else is a function over it:

```text
parse(any harness) → IR → search · port · prune · splice · pack · archive · coordinate · view
```

## The unified IR

[`cv-core/src/ir.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/ir.rs) is a
**superset** of every harness; absent fields are `None`/empty. Every message says **who** speaks
(`role`), **what** the turn is (`kind`) and **where** it came from (`origin`). Shared concepts
(compaction, system prompt, lineage, tool details, usage) have first-class fields; the rest lives
in `extra[<harness>]`, never at the top level.

The spine is `Session → Message → Block`:

```rust
pub struct Session {
    pub principal: Option<Principal>,           // the opaque identity the session acts as, when a store records one; never inferred
    pub id: String,
    pub harness: Harness,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,                  // the session default; each message carries the model it ran under
    pub git: Option<GitInfo>,
    pub system_prompt: Option<String>,          // the prompt the harness sent, when the store keeps it
    pub lineage: Lineage,                       // forked_from · parent · spawned_by_tool_use · continued_in · continues · agent_path
    pub messages: Vec<Message>,
    pub source_path: Option<PathBuf>,           // provenance: where it was read from
    pub source_stamp: Option<SourceStamp>,      // source identity at parse time (never serialized)
    pub extra: serde_json::Map<String, serde_json::Value>,   // extra["claude"], extra["codex"], … only
}

pub struct Message {
    pub id: Option<String>,
    pub parent_id: Option<String>,              // for branching / splice
    pub role: Role,                             // System | User | Assistant | Tool — WHO speaks
    pub kind: MessageKind,                      // WHAT the turn is (below)
    pub origin: Origin,                         // WHERE it came from: human | model | harness | hook | scheduler | subagent | import | unknown
    pub timestamp: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    pub extra: serde_json::Map<String, serde_json::Value>,   // extra["<harness>"] bag (+ "_record" under `Demand::COMPLETE`)
}
```

`MessageKind`:

| kind | meaning |
|---|---|
| `prompt` | a human's typed prompt |
| `reply` | the model's reply: text, thinking, tool calls |
| `tool_result` | tool output fed back to the model |
| `injected_context` | harness-added model input (system reminders, hook output, environment) |
| `system_prompt` | the system prompt, when the store keeps it as a message |
| `notice` | a harness notice shown to the user, never sent to the model |
| `compaction_boundary` / `compaction_summary` | where the harness compacted; the summary seeding the next window |
| `model_change` | the model or effort changed from here on |
| `error` | an error the model never answered (API error, refusal, retries exhausted) |
| `subagent_spawn` / `subagent_return` | a sub-agent was started here / its final return arrived |
| `branch` | a rewind, reset or fork: what follows does not continue what precedes |
| `carrier` | a verbatim non-conversational record, carried only under `Demand::COMPLETE` |

`Message::is_model_visible()` covers prompts, replies, tool results, injected context, system
prompts, compaction summaries, and non-System sub-agent spawns and returns.

`Origin` answers what `role` cannot (a `user` turn may be a person, a hook, or a cron wakeup):

| origin | meaning |
|---|---|
| `human` | typed by a person |
| `model` | produced by the model |
| `harness` | the harness: reminders, notices, compaction, environment |
| `hook` | a user-configured hook's output |
| `scheduler` | cron / loop wakeups and other automation |
| `subagent` | another agent: inter-agent messages, sub-agent returns |
| `import` | the harness's own importer (Goose, Hermes) |
| `unknown` | the store does not say |

Both are **mandatory and never guessed by a consumer.** `MessageKind::for_role` and
`Origin::for_role` give `Message::new(role)` and legacy IR JSON a value (User→`prompt`/`human`,
Assistant→`reply`/`model`, Tool→`tool_result`/`harness`, System→`notice`/`harness`); an adapter
leaving that default where the store says more is a bug.

### `extra` is nested by harness, always

Harness-specific facts live in `extra[<harness>]` (`extra["claude"]["attachment_type"]`), through
`harness_extra(h)` / `harness_extra_mut(h)`, keeping the harness's key spelling (cv-invented names
are snake_case). The only flat message keys are `_record` (verbatim source, `Demand::COMPLETE`)
and `cv_byte_offset` (offset-enabled reads).

**Shared concepts leave `extra`** (compaction is a `kind`; see `system_prompt`, `lineage`,
`details`, `kind: error`). An emitter reads `extra[…]` only for its own harness.

cv-defined JSON fields are snake_case; principals, harness extras and native payloads keep their
keys. [`cv schema --json`](cli.md#cv-schema) publishes the vocabulary.

A message's content is `Block`s, tagged `#[serde(tag = "type")]` (a block has a `type`, a message
a `kind`):

```rust
pub enum Block {
    Text       { text: String },
    Thinking   { text: String, signature: Option<String>,        // signature: Anthropic-bound
                 encrypted: Option<String>, redacted: bool },    // encrypted: OpenAI-bound
    ToolUse    { id: String, name: String, input: serde_json::Value, namespace: Option<String> },
    ToolResult { tool_use_id: String, content: String, is_error: bool,
                 tool_name: Option<String>, status: Option<String>,
                 details: Option<serde_json::Value> },          // structured result state, attachments, persisted-output pointer
    Image      { media_type: Option<String>, data_ref: Option<String> },
    File       { mime: Option<String>, path: Option<String>, source: Option<String> },
}
```

Text fields are shown as `String`; they are `lazy::Text`: `Inline`, `Span` (wire form) or
`Source` (checked bytes in the transcript), so a windowed `cv show` reads only what it prints.
`Text` serializes as a plain JSON string.

- **`Harness`**: `Copy` enum; `Harness::ALL`, `as_str()` (also the `extra` key), `parse()` with
  aliases (`cc`, `oc`, `kimi2`, …).
- **`Role`**: `System | User | Assistant | Tool`; `Tool` stays distinct so conversions re-encode
  harnesses that store results as `user` turns.
- **`Usage`**: `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`,
  `reasoning_tokens`, `cost_usd`.
- **`Lineage`**: `forked_from`, `parent` (a sub-agent's owner), `spawned_by_tool_use`,
  `continued_in`, `continues`, `agent_path`.
- **`GitInfo`**: `branch`, `commit`, `remote`.
- **`SessionRef`**: a handle from discovery (`id`, `harness`, `path`, `cwd`, `title`, timestamps,
  `message_count`), produced without parsing the transcript.

`Session::label()` is the listing title, falling back to the first prompt via
`Message::prompt_title`.

## The `Adapter` trait

One `Adapter` per harness
([`cv-core/src/harness/mod.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/harness/mod.rs)):

```rust
pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;

    /// Every on-disk root this adapter reads (resolved against $HOME, existing ones only),
    /// the primary one first.
    fn roots(&self) -> Vec<PathBuf>;

    /// Cheaply enumerate sessions, naming the sources found but unreadable.
    fn discover(&self) -> Result<Discovered>;

    /// Stream one session's messages into `sink`, one at a time; returns the metadata.
    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session>;
}
```

`adapter.parse(&r)` is `stream::collect` over `stream`, so streaming and whole reads cannot
diverge. Defaulted methods: `try_stream`, `transcript` (whole, from a verified checkpoint, or an
append-only file's prefix), `dependencies` (side files freshness watches), `store_affordances`
(plans, goals, schedules in a side store), `copied_before` (turns a fork copied). Shared pieces:
`harness/wire/{anthropic,openai}` (the two common content shapes, both directions) and
`harness/store` (`jsonl::Read` and `jsonl::drive`, the one JSONL reader and transcript lifecycle;
`json::validate_value`; `sqlite::Cells` and read-only `open_ro`; staged JSONL writes;
`xdg_data_dir`). `docs/design/CODEC.md` is the codec rule.

An emitter is a free function, `harness/<h>/emit.rs`'s
`emit(&Session, &Path, &EmitOptions) -> Result<EmitResult>`, registered in `emit::emitter_for`,
from which `supported_targets()`, the `--harness` target list and the round-trip test derive.
`EmitResult { path, new_id, resume_hint }`.

`harness::for_harness(h)` maps each `Harness` to its `detect()`ed adapter; `harness::all()`
applies it to `ORDER`. `detect()` resolves the store from `$HOME` and the environment; a
per-session lookup detects only its harness. SQLite-primary adapters (Cursor, Hermes, Goose, Zed,
Devin) need the `sqlite` feature, so WASM drops them. See [adding a harness](adding-a-harness.md).

## Discovery & the incremental cache

`discover_all()` runs every rooted adapter's `discover()` **in parallel via rayon** (`Send +
Sync`; sequential without the `parallel` feature). `cv index` reuses a Codex rollout decoded for
discovery. A failing adapter keeps its previous catalog rows and its
unreadable sources are named by `cv doctor`; other harnesses list normally.

### The cache

[`discover_cache.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/discover_cache.rs)
keeps the `scan_cache` table in `$CLUSTERVISION_HOME/catalog.db`: per path, `(mtime_ns, size)`,
the adapter's interpretation version, the file identity (`source::FileId`: length, mtime, device,
inode, ctime), and the `SessionRef`s produced. A hit (identities and dependencies rechecked) skips
the parse; a grown `.jsonl` continues from its stored position. On a 5,421-session, 78 GB fleet a
first `cv ls` took 158–186 s, a warm one 0.06–0.09 s (`docs/design/PERF.md`).

Adapters wrap their per-file scan in `cached_scan_with_reads` (one session per file, side-file
reads captured first) or `cached_scan_many` (several sessions per file, as Gemini's logs):

```rust
crate::discover_cache::cached_scan_with_reads(&transcript, reads, || scan(&transcript))
```

Correctness never depends on it: a rewrite with a restored timestamp misses on identity, and a
missing or corrupt cache rescans. Rows are per environment, shared with the app and the freshness
probe. Empty results are not cached.

### Growing transcripts resume

Discovery reads a transcript once, then stores the fold's state at the last finished line with
the file's identity and a prefix sample. Unchanged costs a stat; grown costs the appended records
(Claude transcripts, plain Codex rollouts). Codex discovery leaves `message_count` `null`,
compressed `.jsonl.zst` rollouts included; `0` means a counted session with no turns. A full read
or `cv index` certifies counts.

### Sub-agents

Sub-agents (each adapter's `forest`: Claude, Codex, Devin, Kimi, Kimi Code, Hermes) are not in
the main pool; `subagent_tree_of(&SessionRef)` fetches them per parent.

### Finding one session

`find(id, harness)`: an exact `id` wins; otherwise a unique prefix; several prefix hits are an
error asking for a longer id or `--harness`.

## The crates

`cv-core` is the library; the other crates are front-ends over it.

```text
                 ┌─────────────────────────────────────────────┐
                 │                  cv-core                     │
                 │  IR · Adapter registry · discover_all +      │
                 │  cache · emit/convert · splice · tasks · the   │
                 │  read API · render · ingest · redact · watch │
                 └─────────────────────────────────────────────┘
                     ▲              ▲              ▲          ▲
          ┌──────────┴─────┐   ┌────┴─────┐  ┌─────┴────┐ ┌───┴────┐
          │       cv       │   │cv-search │  │  cv-web  │ │  app/  │
          │  CLI · serve   │──▶│ (tantivy,│  │  (WASM)  │ │(Tauri) │
          │ watch · archive│   │ trigram) │  │          │ │        │
          └────────────────┘   └──────────┘  └──────────┘ └────────┘
```

| Crate | Responsibility |
|-------|----------------|
| **`cv`** | The binary: the CLI, `cv serve` (HTTP API, task inbox, dashboard, fleet hub), `cv watch` / `cv archive`, and hooks. |
| **`cv-search`** | Full-text via [tantivy](https://github.com/quickwit-oss/tantivy) (BM25, snippets), exact trigram acceleration, and derived columnar event segments. |
| **`cv-web`** | WASM: ingests an uploaded harness `.zip` in-browser, returns sessions as JSON. |
| **`app/`** | Tauri v2 desktop shell around the web UI, reading `cv_core::api` in-process. |

**`cv-core` owns the IR, the adapters and discovery**, so an adapter fix reaches the CLI, server,
web UI and app at once.

## Where to go next

- [Cross-harness conversion](conversion.md): the `emit` side and what survives a round-trip.
- [Adding a harness](adding-a-harness.md): one `Adapter`, one `for_harness` arm.
