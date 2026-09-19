# Interface v2 (cv 0.11.0) — the clean break

Decided with ember, 2026-09-19: **clean break** (no aliases for old names), **snake_case
everywhere**, **full IR restructure**. This document is the contract every work package builds
against. When code and this document disagree during the initiative, fix the code; when the
document turns out to be wrong about a harness, fix the document in the same commit as the code.

Principle: **one name, one meaning, no grammar to memorize.** A verb names one act; a flag means
the same thing on every command that has it; a JSON key is spelled one way; a message has a
`kind`, a block has a `type`; harness-specific data lives under the harness's own key.

---

## 1. Commands

Grouped help (`cv --help` shows every command under these headings; nothing is hidden):

| group | commands | what they have in common |
|---|---|---|
| **Read** | `ls` `show` `cat` `search` `events` `touched` `tools` `tree` `workflow` `compaction` `timeline` `stats` `diff` `blame` `doctor` | read-only over existing sessions |
| **Reshape** | `prune` `splice` `loom` `port` `redact` `resume` | produce a NEW session id from existing ones (the source is never touched); `resume` launches one |
| **Export** | `export` `dataset` `pack` | produce something that is not a session |
| **Fleet & live** | `task` `board` `scry` `share` | multi-agent coordination and live views |
| **System** | `index` `config` `schema` `formats` `recipes` | cv's own state and reference |

Renames and removals (old → new; the old name is GONE, it errors with a pointer):

| old | new | why |
|---|---|---|
| `convert <id> --to <h>` | `port <id> --harness <h>` | convert and port were the same act (produce a copy that runs elsewhere); one verb |
| `port --to <h>` / `--to-dir <d>` | `port --harness <h>` / `--cwd <d>` | say what the value is, not where it goes |
| `query` | `schema` | `query` prints the field/operator reference; it does not run a query |
| `recall`, `distill` | removed | superseded by `pack` (`pack` is the one "build context from the corpus" verb) |
| `prune --retrieve <id>` | `cat <session> <tool_use_id>` | fetching a tool's output is a Read, not a prune option |
| `prune --range` | `prune --keep A..B` | it selects what to KEEP; `--range` is the read-window flag elsewhere |
| `show --range -N` | `show --first N` | `-N` looked like "last N" to everyone who used it |
| `show --pre-compaction [N]` | unchanged | |
| `cv help` footer | `recipes` | the agent quickstart is a real command |

New:

- **`cat <session-id> <tool_use_id>`** — print one tool call's full output, wherever it lives:
  inline in the transcript, in a `prune` sidecar, or in a persisted-output file
  (`<session>/tool-results/…`, Kimi `tool-results/*.txt`). `--input` prints the call's arguments
  instead. Exit 2 if the id is unknown.
- **`formats`** — the format census and manifest check (see §7). `formats census [--harness h]`
  lists unknown record/part/column vocabulary seen in real data; `formats check` compares each
  adapter against `formats/<harness>.toml`.
- **`recipes`** — the agent quickstart: the ten things agents do with cv, each as one command
  line with its JSON shape. Also printed by `cv --help`'s last line as "run `cv recipes`".

## 2. Flags — the same word means the same thing everywhere

**Windows** (on `show`, `export`, `cat --input` n/a, MCP `show`; all 0-based, end-exclusive):

| flag | meaning |
|---|---|
| `--first N` | the first N messages |
| `--last N` | the last N messages |
| `--range A..B` | messages A (inclusive) to B (exclusive); `A..` to the end, `..B` from the start |
| `--around N [--context K]` | message N with K messages either side (default 5) |
| `--max-bytes N` | stop after N bytes of rendered output and print a continuation line `… continue with --range <next>..` |

Only one of `--first/--last/--range/--around` may be given. `prune --keep A..B` uses the same
`A..B` grammar (turn indices, as today).

**Everywhere else:** `--harness <h>` (filter or target, never "--to"), `--cwd <dir>`, `--out <dir>`
(write somewhere other than the real store), `--json` (machine output; every command that prints a
list or a session has it), `--limit N`, `--fresh` (bypass the catalog), `--all` (include what is
hidden by default: finished lanes, archived sessions, terminal tasks).

**Ids:** every command accepts a unique id prefix or `harness:id`. An ambiguous prefix lists the
candidates as `harness:full-id` lines and exits 2.

## 3. JSON — snake_case, one shape per noun

- Every `--json` output and every MCP payload uses **snake_case** keys. `ls --json` changes:
  `messageCount → message_count`, `createdAt → created_at`, `updatedAt → updated_at`,
  `sizeBytes → size_bytes`, `displayTitle → display_title`. `search --json` and `workflow --json`
  likewise. Timestamps are RFC 3339 strings.
- A **session row** (from `ls`, `search`, `project_sessions`, …) always has the same keys:
  `id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes` (+ `display_title`,
  `git` under `--enrich`).
- A **message** is the IR `Message` (below). A **block** is the IR `Block`, tagged by `type`.
- `cv schema --json` publishes the machine-readable schema of every shape above; `recipes` links it.

## 4. The IR (cv-core `ir.rs`)

```rust
pub struct Session {
    pub id: String,
    pub harness: Harness,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub git: Option<GitInfo>,
    /// The system prompt the harness sent (when it stores it: Hermes, Kimi Code, OpenClaw,
    /// Claude `prompt_snapshot`, Codex `base_instructions`). NOT a message.
    pub system_prompt: Option<String>,
    /// Where this session came from and where it went.
    pub lineage: Lineage,
    pub messages: Vec<Message>,
    pub source_path: Option<PathBuf>,
    /// Harness-specific session facts, nested under the harness name:
    /// `extra["claude"]["custom_title"]`, `extra["codex"]["history_mode"]`, … Never flat.
    pub extra: Map<String, Value>,
}

#[derive(Default)]
pub struct Lineage {
    /// The session this one was forked or branched from (Codex `forked_from_id`, OpenClaw fork,
    /// Hermes `_branched_from`, Claude `--fork-session`).
    pub forked_from: Option<String>,
    /// The session that owns this one as a sub-agent (Codex `parent_thread_id`, Claude parent
    /// session of an `agent-*` transcript, Kimi `parentAgentId`, Hermes `_delegate_from`).
    pub parent: Option<String>,
    /// The tool call in the parent that spawned this session (Claude `toolUseId`), if known.
    pub spawned_by_tool_use: Option<String>,
    /// The session this one continued in (Claude `continued-in`, Hermes compression rotation).
    pub continued_in: Option<String>,
    /// The session this one continues (the inverse pointer, when the store records it).
    pub continues: Option<String>,
    /// Sub-agent path or nickname when the harness has one (Codex `agent_path`, Kimi `agent-N`).
    pub agent_path: Option<String>,
}

pub struct Message {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub role: Role,          // System | User | Assistant | Tool — WHO speaks
    pub kind: MessageKind,   // WHAT the message is — see below
    pub origin: Origin,      // WHERE it came from
    pub timestamp: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    /// Harness-specific message facts, nested under the harness name: `extra["claude"]["attachment_type"]`.
    /// Exactly one other key is allowed at the top level: `"_record"` (the verbatim carrier record
    /// under `ParseOptions::complete`).
    pub extra: Map<String, Value>,
}

/// What a message IS. Every adapter sets it; every emitter reads it.
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A human's typed prompt (Role::User, Origin::Human).
    Prompt,
    /// The model's reply (Role::Assistant): text, thinking and tool calls.
    Reply,
    /// Tool output fed back to the model (Role::Tool).
    ToolResult,
    /// Context the HARNESS injected into the model's input: Claude attachments/system reminders,
    /// Kimi injections, Goose `<turn-context>`, Codex `<environment_context>`, hook stdout.
    InjectedContext,
    /// The system prompt, when the store keeps it as a message (Kimi `profile.bind`, Hermes system
    /// row, OpenClaw). Session-level copy in `Session::system_prompt`.
    SystemPrompt,
    /// A harness notice shown to the user, not sent to the model: slash-command output, task
    /// terminated, turn aborted, "usage limit reset", Codex `item_completed` notes.
    Notice,
    /// The point where the harness compacted; `compaction` carries the metadata.
    CompactionBoundary,
    /// The summary that seeds the next window after a compaction.
    CompactionSummary,
    /// The model (or effort) changed from here on; `model` holds the new one.
    ModelChange,
    /// An error the model never answered: API error, refusal fallback, retry exhaustion.
    Error,
    /// A sub-agent was spawned (the `Agent`/`spawn` call); `lineage`-like pointers in `extra[h]`.
    SubagentSpawn,
    /// A sub-agent's final return delivered to the parent.
    SubagentReturn,
    /// A branch/rewind/reset marker: what follows does not continue what precedes.
    Branch,
    /// A verbatim non-conversational record carried only under `ParseOptions::complete`.
    Carrier,
}

#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,      // typed by a person
    Model,      // produced by the model
    Harness,    // produced by the harness itself (reminders, notices, compaction)
    Hook,       // a user-configured hook's output
    Scheduler,  // cron / loop wakeups, automation
    Subagent,   // another agent (inter-agent messages, sub-agent returns)
    Import,     // imported from another harness by the harness (Goose/Hermes importers)
    Unknown,
}

pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    /// Reasoning/thinking tokens when the provider reports them separately.
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported cost, when the harness stores it (Goose, OpenCode, Codex).
    pub cost_usd: Option<f64>,
}

/// Content blocks — serde tag is `type` (the word every harness uses on the wire).
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text },
    Thinking { text, signature, encrypted, redacted },   // signature = Anthropic-bound, encrypted = OpenAI-bound
    ToolUse { id, name, input, namespace: Option<String> },
    ToolResult { tool_use_id, content, is_error, tool_name, status, details },
    Image { media_type, data_ref },
    File { mime, path, source },
}
```

Rules:

- **`kind` is mandatory and never guessed by consumers.** Defaults by role only for legacy
  construction (`Message::new(role)`): User→Prompt, Assistant→Reply, Tool→ToolResult,
  System→Notice. Adapters MUST set the precise kind.
- **`extra` is nested by harness, always.** `m.harness_extra_mut(Harness::Claude)` returns the
  `extra["claude"]` object. The keys inside keep the harness's own spelling (`attachment_type`,
  `history_mode`, `display_kind`), snake_case where cv invents a name. The only top-level key
  besides harness names is `_record`.
- **Shared concepts leave `extra`.** Compaction → `kind`; system prompt → `Session::system_prompt`;
  parent/fork/continued → `Session::lineage`; tool details → `Block::ToolResult::details`;
  error → `kind: Error` + `extra[h]["error"]`; injected context origin → `origin`.
- **Emitters read kinds, never harness keys.** An emitter may consult `extra[<its own harness>]`
  when round-tripping into the same harness (format-complete replay), nothing else.
- **Format-complete replay fields live in the harness bag too.** Under `ParseOptions::complete`
  the Claude adapter mirrors a record's non-first-class fields into `extra["claude"]` (same keys
  as today, including the `message.`-prefixed ones) and the verbatim carrier record stays at the
  top-level `_record`. `emit_claude` reads them from `extra["claude"]`. Same pattern for every
  other harness that round-trips.
- **Shared core modules key off `kind`, not harness keys.** `compaction.rs` detects
  `MessageKind::CompactionBoundary`/`CompactionSummary`; `doctor.rs` buckets
  `MessageKind::InjectedContext` as system reminders (kind name from `extra["claude"]["attachment_type"]`
  when present); `events.rs`/`tools.rs` read Claude's `toolUseResult` from `extra["claude"]`;
  `render.rs`/`html.rs` and `cv show` label a System turn by its `kind` (plus the attachment kind).
- **`Session::model`** is the session default; `Message::model` is set only when it differs
  (REARCH "IR diet").

## 5. Fidelity verifier v2 (`emit_verified`)

Replaces the six-count check. After emitting, re-parse the output and diff per message:
role sequence, kind sequence, block type counts, thinking (with text / signature-only / encrypted),
tool names on results, `is_error`, `details` present, usage present, model present, timestamps
present, ids present, plus session title, cwd, model, system_prompt, lineage. Report every
delta; classify each as **expected for this target** (from a per-emitter table of what the
format cannot carry, e.g. vendor-bound thinking, per-message ids) or **unexpected**.
`port --strict` fails on any unexpected delta; the default prints them under `⚠ lost`.

## 6. MCP (`cv-mcp`)

Tools are **generated from the CLI**, not hand-written, so names, flags, harness lists and output
shapes cannot drift. Mechanism (no shared crate; the clap tree lives in the `cv` binary):

- `cv schema --commands --json` dumps the command tree: for every visible command and subcommand,
  `{ name, group, about, args: [{ name, kind: "flag"|"option"|"positional", value_type:
  "string"|"integer"|"number"|"boolean"|"path", possible_values?, default?, help, required }] }`,
  produced from clap's `Command` introspection (`CommandFactory`), including the window flags.
- `cv-mcp` runs that once at startup (the `cv` binary is looked up next to `cv-mcp`, then on
  `PATH`, overridable with `CV_BIN`) and registers one tool per Read/Reshape/Export/System command
  with the same name (`ls`, `show`, `search`, `cat`, …); each argument becomes a JSON-schema
  property with the clap help as its description. Positionals map to properties named after the
  argument. A call runs `cv <cmd> <args> --json` (or without `--json` for commands that have no
  JSON form, returning text) and returns stdout; a non-zero exit returns stderr as the error.
- Over MCP, `show` defaults to `last: 50` and `max_bytes: 200000` (a full transcript is never the
  default there); `ls`/`search` default `limit` stays the CLI default.
- MCP-only tools keep their names and hand-written schemas: `observe_stream`, `await_omen`,
  `board_*`, `task_*` (which mirror `cv task <sub>`). `read_session`, `search_sessions`,
  `list_sessions`, `project_sessions`, `recall`, `prune_session`, `prune_retrieve` are removed
  (their replacements are `show`, `search`, `ls`, `ls --cwd`, `pack`, `prune`, `cat`).

## 7. Format census and manifests (`cv formats`)

- **Census.** Under `ParseOptions::complete` every adapter carries unknown records as `Carrier`
  messages with `extra[h]["record_type"]`. `cv formats census` parses recent sessions per harness
  in complete mode and reports, per harness, the record/part/column vocabulary seen with counts,
  marking anything absent from the manifest as **new**.
- **Manifests.** `formats/<harness>.toml`: `upstream = { repo, commit, date }`, `store = [...]`
  paths, and `[types]` with each persisted type and its status `handled | carried | ignored`. A
  test asserts every `handled` type appears in the adapter's match arms and every match arm
  appears in the manifest.
- **Drift script.** `tools/harness-drift.sh` fast-forwards the `~/pug` checkouts and greps the
  persistence enums against the manifests (what the 2026-09-19 survey did by hand).
- **Fixture generators** live in `tools/harness-fixtures/<harness>/` (Goose stub provider + env,
  Hermes store script, OpenClaw driver, Codex resume smoke) so every real-writer fixture is one
  command to regenerate.

## 8. Migration notes

- **sesh** (`~/dev/sesh`, `server/catalog.py`, `server/sesh.py`) reads `cv ls --json`,
  `cv search --json` and `cv show --json --range`: update to snake_case keys and `--first/--last`.
- **ember's `~/.claude/CLAUDE.md`** references `cv workflow`, `cv scry`, `cv show/export`: all
  survive. `prune --retrieve` → `cat`.
- **Manual** (`manual/src/*.md`), `README.md`, `docs/FORMATS.md`, `CHANGELOG.md` (0.11.0 section
  headed "clean break").

## 9. Work packages and file ownership

| WP | owner | files | depends on |
|---|---|---|---|
| 0 IR core + compile | coordinator | `cv-core/src/ir.rs`, minimal shims in every crate | — |
| 1 CLI surface | fork | `cv/src/main.rs`, `cv/src/cmd/*` (incl. `schema --commands --json`, `cat`, `recipes`, window flags, snake_case JSON, `view.rs` kind labels), `manual/src/cli.md`, `README.md`, sesh consumers | 0 |
| 2a adapters: claude, claude_workflow + the shared core modules that key off Claude fields today: `compaction.rs`, `events.rs`, `doctor.rs`, `tools.rs`, `render.rs`, `html.rs` | fork | those files | 0 |
| 2b adapters: codex | fork | `codex.rs` | 0 |
| 2c adapters: kimi, kimi_code, opencode, openclaw | fork | those files | 0 |
| 2d adapters: gemini, qwen, hermes, goose | fork | those files | 0 |
| 2e adapters: grok, cursor, zed, cline, roo, continuedev, lmstudio, claude_app, chatgpt_app, export | fork | those files | 0 |
| 3 emitters + verifier v2 + OpenCode sqlite emitter | fork | `cv-core/src/emit.rs` only | 0 (contract), verified after 2 |
| 4 MCP generation + windows | fork | `cv-mcp/src/*` | 1 (`cv schema --commands --json`) |
| 5 formats census/manifests/drift/generators | fork | `cv-core/src/formats.rs`, `formats/*.toml`, `tools/*`, `cv/src/cmd/formats.rs` (wired by WP1) | 0 |
| 6 docs + FORMATS + CHANGELOG + version 0.11.0 | coordinator | docs, manual, Cargo versions | all |

Rules for every fork: only your files; no commits; no `git stash`/checkout; filtered tests only
(`-E 'test(/<area>/)'`); if the crate fails to compile in a file you do not own, wait and retry.
