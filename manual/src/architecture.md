# Architecture

This chapter is for contributors and the curious: how clustervision is built, and why it's shaped the way it is. 🔮

The whole project rests on one idea. Every harness — Claude Code, Codex, Gemini, Cursor, seventeen of them and counting — stores its sessions in a different on-disk dialect. clustervision parses every one of those dialects **into a single unified representation** (the IR), and then *everything else* — search, porting, packing, the board, the app — is just a function over that IR. The data-flow one-liner is:

```text
parse(any harness) → IR → search · port · prune · loom · pack · archive · coordinate · view
```

Get the IR right and the rest composes. So we start there.

## The unified IR

The IR lives in [`cv-core/src/ir.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/ir.rs). It is deliberately a **superset** of what any single harness records: fields a harness doesn't have are `None`/empty. Three questions are answered by three fields on every message — **who** speaks (`role`), **what** the turn is (`kind`), **where** it came from (`origin`) — and every shared concept (compaction, system prompt, lineage, tool details, usage) has a first-class home, so conversions never have to guess from harness-specific keys. What is genuinely specific to one harness lives in `extra[<harness>]`, a bag keyed by the harness name — never at the top level.

The spine is `Session → Message → Block`:

```rust
pub struct Session {
    pub id: String,
    pub harness: Harness,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,                  // the session default; messages carry a model only when it differs
    pub git: Option<GitInfo>,
    pub system_prompt: Option<String>,          // the prompt the harness sent, when the store keeps it
    pub lineage: Lineage,                       // forked_from · parent · spawned_by_tool_use · continued_in · continues · agent_path
    pub messages: Vec<Message>,
    pub source_path: Option<PathBuf>,           // provenance: where it was read from
    pub extra: serde_json::Map<String, serde_json::Value>,   // extra["claude"], extra["codex"], … only
}

pub struct Message {
    pub id: Option<String>,
    pub parent_id: Option<String>,              // for branching / loom
    pub role: Role,                             // System | User | Assistant | Tool — WHO speaks
    pub kind: MessageKind,                      // WHAT the turn is (below)
    pub origin: Origin,                         // WHERE it came from: human | model | harness | hook | scheduler | subagent | import | unknown
    pub timestamp: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    pub extra: serde_json::Map<String, serde_json::Value>,   // extra["<harness>"] bag (+ "_record" under `complete`)
}
```

`MessageKind` is the vocabulary conversions and analyses key off:

| kind | meaning |
|---|---|
| `prompt` | a human's typed prompt |
| `reply` | the model's reply: text, thinking, tool calls |
| `tool_result` | tool output fed back to the model |
| `injected_context` | context the harness put in the model's input (system reminders, hook output, environment context) |
| `system_prompt` | the system prompt, when the store keeps it as a message |
| `notice` | a harness notice shown to the user, never sent to the model |
| `compaction_boundary` / `compaction_summary` | where the harness compacted, and the summary that seeds the next window |
| `model_change` | the model or effort changed from here on |
| `error` | an error the model never answered (API error, refusal, retry exhaustion) |
| `subagent_spawn` / `subagent_return` | a sub-agent was started here / its final return arrived |
| `branch` | a rewind, reset or fork marker: what follows does not continue what precedes |
| `carrier` | a verbatim non-conversational record, carried only under `ParseOptions::complete` |

`MessageKind::is_model_visible()` says whether a turn was part of what the model saw or said — prompts, replies, tool results, injected context and the system prompt are; the rest is bookkeeping around the conversation. `cv doctor`, the dataset exporter and the emitters all use it instead of reading harness keys.

`Origin` is the orthogonal question — not *what* the turn is but **where it came from**, which `role` alone can't tell you (a `user`-role turn might be a person typing, a hook's stdout, or a cron wakeup):

| origin | meaning |
|---|---|
| `human` | typed by a person |
| `model` | produced by the model |
| `harness` | produced by the harness itself: reminders, notices, compaction, environment context |
| `hook` | a user-configured hook's output |
| `scheduler` | cron / loop wakeups and other automation |
| `subagent` | another agent: inter-agent messages, sub-agent returns |
| `import` | imported from another harness by the harness (the Goose / Hermes importers) |
| `unknown` | the store does not say |

Both are **mandatory on every message and never guessed by a consumer.** `MessageKind::for_role` and `Origin::for_role` exist only so `Message::new(role)` and legacy IR JSON have a defined value (User→`prompt`/`human`, Assistant→`reply`/`model`, Tool→`tool_result`/`harness`, System→`notice`/`harness`); an adapter that leaves the default in place is a bug, not a shortcut.

### `extra` is nested by harness, always

Everything genuinely specific to one harness lives in `extra[<harness name>]` — `extra["claude"]["attachment_type"]`, `extra["codex"]["history_mode"]` — reached through `harness_extra(h)` / `harness_extra_mut(h)`, never written flat at the top level. The keys inside keep the harness's own spelling; where cv invents a name it is snake_case. Exactly one other top-level key is allowed on a message: `_record`, the verbatim source record carried under `ParseOptions::complete`.

The rule that makes this work is that **shared concepts leave `extra`**. Compaction is a `kind`, not a Claude field. The system prompt is `Session::system_prompt`. Parent/fork/continuation pointers are `Session::lineage`. Structured tool-result state is `Block::ToolResult::details`. An error is `kind: error`. So `compaction.rs`, `doctor.rs`, `render.rs`, the dataset exporter and every emitter key off the IR, and an emitter consults `extra[…]` only for its *own* harness, when replaying a session back into the format it came from.

Every `--json` output cv produces speaks exactly this vocabulary, in snake_case, and [`cv schema --json`](cli.md#cv-schema) publishes it machine-readably.

A `Message`'s content is a list of `Block`s — the atoms of a conversation. The enum is tagged `#[serde(tag = "type")]` (a block has a `type`; a message has a `kind`) so it round-trips to clean JSON:

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

(The text-bearing fields are shown as `String` above for readability. They are really `lazy::Text`, which may be a resolved string *or* a byte span into the transcript on disk — which is what lets a windowed `cv show` read only the bytes it prints, and lets a multi-gigabyte session be listed without materializing it. `Text` serializes as a plain JSON string either way.)

A few supporting types complete the picture:

- **`Harness`** — a `Copy` enum naming the source harness (`Claude`, `Codex`, `KimiCode`, `Grok`, `OpenCode`, `Gemini`, `Cursor`, `ClaudeApp`, `Goose`, …). It carries the canonical string forms: `Harness::ALL` lists every one, `as_str()` gives the lowercase name (also the key of its `extra` bag), and `parse()` accepts aliases (`cc` → `Claude`, `oc` → `OpenCode`, `kimi2` → `KimiCode`, …).
- **`Role`** — `System | User | Assistant | Tool`. We keep `Tool` distinct even though some harnesses model a tool result as a `user` turn, so conversions can re-encode it correctly.
- **`Usage`** — optional counts: `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `reasoning_tokens`, and `cost_usd` when the harness records a cost.
- **`Lineage`** — where a session came from and went: `forked_from`, `parent` (the owning session of a sub-agent), `spawned_by_tool_use`, `continued_in`, `continues`, `agent_path`.
- **`GitInfo`** — the `branch` / `commit` / `remote` a session ran against, when the harness records it.
- **`SessionRef`** — a *lightweight handle* to a session discovered on disk (`id`, `harness`, `path`, `cwd`, `title`, timestamps, `message_count`). It's cheap to produce for listings and search **without parsing the whole transcript**. Discovery yields `SessionRef`s; a full `Session` is parsed only when you actually open one.

`Session` also carries a little behavior for free: `label()` (a short title for listings, falling back to the first user message), `first_user_text()`, `searchable_text()` (every text/thinking/tool block concatenated — what feeds the search index), and `harness_extra(h)` / `harness_extra_mut(h)` for the per-harness bags.

## The `Adapter` trait

Every harness is one `Adapter` ([`cv-core/src/harness/mod.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/harness/mod.rs)). The trait is small on purpose:

```rust
pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;

    /// The on-disk root this adapter reads from (resolved against $HOME), if it exists.
    fn storage_root(&self) -> Option<PathBuf>;

    /// Cheaply enumerate sessions without fully parsing them.
    fn discover(&self) -> Result<Vec<SessionRef>>;

    /// Fully parse one discovered session into the IR.
    fn parse(&self, r: &SessionRef) -> Result<Session>;

    /// Emit an IR session into this harness's native format. Default: unsupported.
    fn emit(&self, _session: &Session, _out_dir: &Path) -> Result<EmitResult> { /* bail */ }

    /// Whether `emit` is implemented (so the CLI can list valid `--to` targets).
    fn can_emit(&self) -> bool { false }
}
```

`discover` + `parse` are the read path; `emit` + `can_emit` are the optional write path that makes a harness a **conversion target**. An adapter that only implements the first two is read-only — you can search and port *out of* it, but not *into* it. Emitting returns an `EmitResult { path, new_id, resume_hint }`, where `resume_hint` is a human nudge like `claude --resume <id>`.

The `Send + Sync` bound is load-bearing: it's what lets discovery fan out across adapters in parallel. Every adapter holds only path data (connections are opened per call), so the bound is always satisfied.

Adapters are registered in one place — `harness::all()`:

```rust
pub fn all() -> Vec<Box<dyn Adapter>> {
    let mut adapters: Vec<Box<dyn Adapter>> = vec![
        Box::new(claude::Claude::new()),
        Box::new(codex::Codex::new()),
        Box::new(grok::Grok::new()),
        Box::new(opencode::OpenCode::new()),
        // … gemini, openclaw, claude_app, chatgpt_app, kimi, qwen,
        //   lmstudio, cline, roo, continuedev …
    ];
    #[cfg(feature = "sqlite")]
    { adapters.push(Box::new(hermes::Hermes::new()));      // SQLite-backed harnesses
      adapters.push(Box::new(cursor::Cursor::new()));      // are gated behind a feature
      adapters.push(Box::new(goose::Goose::new())); }
    adapters
}
```

The SQLite-backed adapters (Cursor, Hermes, Goose) are behind the `sqlite` feature so the WASM build can drop them. `for_harness(h)` returns just the one adapter you asked for. Adding a harness is, essentially, "write one `Adapter` and add a line to `all()`" — the [adding a harness](adding-a-harness.md) chapter walks the whole thing.

## Discovery & the incremental cache

`discover_all()` is the front door. It collects every registered adapter whose `storage_root()` actually exists on this machine, then runs their `discover()` methods **in parallel via rayon**, so the cost is the slowest single adapter rather than the sum:

```rust
#[cfg(feature = "parallel")]
let out: Vec<SessionRef> = adapters
    .par_iter()
    .flat_map_iter(|a| run(a.as_ref()))
    .collect();
```

(Turn the `parallel` feature off — e.g. on `wasm32` — and you get an identical sequential pass. The file-heavy adapters use the same trick *internally* via `par_filter_map` / `par_flat_map` to scan many transcripts at once.)

A failing adapter logs and contributes an empty list rather than sinking the whole scan — one harness's corrupt file never breaks discovery of the other sixteen.

### The cache

Cold discovery of a large corpus means reading and JSON-parsing every transcript just to pull a handful of metadata fields. That's slow. But transcripts only change by being **appended to**, so a file whose `(mtime, size)` is unchanged since last time has unchanged metadata.

That's the whole trick behind [`discover_cache.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/discover_cache.rs): a persistent JSON cache under the user's cache dir, keyed on `(mtime_ns, size)`, storing the `SessionRef`s each file produced. A hit returns the stored refs **without touching the file** — steady-state discovery costs one `stat` per file instead of a full read+parse. In practice that's the difference between **~37s cold and ~0.25s warm**.

Adapters opt in by wrapping their per-file scan in `cached_scan` / `cached_scan_many`:

```rust
crate::discover_cache::cached_scan(&path, || match scan(&path) {
    Ok(r) => Some(r),
    Err(e) => { eprintln!("cv: skipping {}: {e:#}", path.display()); None }
})
```

Some careful properties make it safe to lean on:

- **Correctness never depends on it.** An entry is reused *only* when `(mtime, size)` match exactly, so any real change misses and re-scans. A missing or corrupt cache just means everything is re-scanned (and the cache rebuilt).
- **It's process-global and shared** by `cv`, `cvd`, and the desktop app via one JSON file.
- **Writes are atomic** (temp file + rename). `persist()` runs after a full `discover_all()`, flushes only if something changed, and drops entries whose files no longer exist so the cache can't grow without bound.
- **Empty results aren't cached** — a non-transcript path is cheap to re-reject, and caching it would bloat the file.

### The head/tail cap

One more guard keeps discovery fast on pathological inputs. Some transcripts are *enormous* — Codex rollout logs can run to hundreds of megabytes. Fully parsing those during a *listing* would defeat the point of the cheap-`SessionRef` design.

So the file-based adapters cap how much they read during discovery. The Codex adapter is the clearest example: files up to a `FULL_SCAN_CAP` (8 MiB) are parsed in full; anything larger is **sampled head + tail** (1 MiB each). The head carries `id` / `cwd` / `title` / `created_at`; the tail's last record carries the real `updated_at`. The middle is never read — `message_count` becomes a head-density estimate that stays roughly monotonic with file growth, and the *exact* contents are parsed lazily only when you actually open the session. Discovery never reads, or JSON-parses, gigabytes just to list.

### Sub-agents

Sub-agent sessions (currently Claude Code's `Task` sub-agents) are **not** in the main pool — there can be thousands of them, so pulling them all into every listing would be wasteful. They're fetched lazily per parent via `subagents_of(&SessionRef)`, which is what powers the sub-agent trees in the app.

### Finding one session

`find(id, harness)` is the lookup the CLI and MCP server use. An exact `id` match wins and returns immediately; otherwise the `id` is treated as a **prefix**. A single prefix hit is returned — but *multiple* distinct prefix hits are an **error**, not a silent "first one wins," so you're told to pass a longer id or a `--harness` to disambiguate. Surprising the user with the wrong session is worse than making them type three more characters.

## The crates

clustervision is a Rust workspace. `cv-core` is the library every other crate is built on; the rest are thin front-ends over it.

```text
                         ┌─────────────────────────────────────────────┐
                         │                  cv-core                     │
                         │  IR · Adapter registry · discover_all +      │
                         │  cache · emit/convert · loom · board ·       │
                         │  render/html · ingest · redact · watch       │
                         └─────────────────────────────────────────────┘
                            ▲    ▲    ▲    ▲    ▲    ▲    ▲     ▲     ▲
            ┌───────────────┘    │    │    │    │    │    │     │     └──────────┐
            │             ┌──────┘    │    │    │    │    │     └────┐           │
         ┌──┴──┐      ┌───┴──┐   ┌────┴─┐ ┌┴───┐ ┌─┴──┐ ┌┴────┐ ┌───┴───┐   ┌───┴───┐
         │ cv  │      │cv-mcp│   │ cvd  │ │ cv │ │ cv │ │ cv  │ │  cv   │   │  app/ │
         │(CLI)│      │(MCP) │   │(daem │ │-tui│ │-sea│ │-llm │ │ -web  │   │(Tauri)│
         │     │      │      │   │+serve│ │    │ │rch │ │     │ │(WASM) │   │       │
         └─────┘      └──────┘   └──────┘ └────┘ └────┘ └─────┘ └───────┘   └───────┘
```

| Crate | Responsibility |
|-------|----------------|
| **`cv`** | The CLI — read, search, reshape, and port sessions across harnesses. |
| **`cv-mcp`** | An MCP server: lets a *running* agent search and read *other* agents' sessions (same project, across time, across harnesses). |
| **`cvd`** | The daemon — watches every harness's storage root and archives sessions (incl. cloud-fleet logs) into a central store; also hosts the `serve` HTTP API. |
| **`cv-search`** | Pure-Rust search: full-text via [tantivy](https://github.com/quickwit-oss/tantivy) (BM25 + snippets) and semantic via embeddings (model2vec). |
| **`cv-llm`** | LLM-backed features — the optional abstractive digests behind `cv pack` and the `--generate` continuations for `splice`/`loom`, via OpenRouter / Anthropic / a local LM Studio. |
| **`cv-web`** | WASM bindings — ingest an uploaded harness `.zip` entirely in-browser and hand back sessions as JSON. |
| **`cv-tui`** | A terminal browser for sessions (search, read, scry, board), built on ratatui. |
| **`app/`** | The desktop app — a Tauri v2 shell around the web UI with native `cv-core`/`cv-llm` power. |

The shape to take away: **`cv-core` owns the IR, the adapters, and discovery; everything else is a lens onto it.** A bug fixed in an adapter improves the CLI, the daemon, the TUI, and the app at once — because they all see the same `Session`.

## Where to go next

- [Cross-harness conversion](conversion.md) — the `emit` side of the trait, and what survives a round-trip.
- [Adding a harness](adding-a-harness.md) — write one `Adapter`, add one line to `all()`, done.
