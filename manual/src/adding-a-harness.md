# Adding a harness

An adapter says where a harness keeps sessions, how to list them cheaply, and how to stream one.
Sizes: store description 60–150 lines (+25 per SQL table), quirks 0–400, codec 0 (Anthropic or
OpenAI content) to 1,000, emitter 150–450. Copy
[Grok's](https://github.com/emberian/cv/blob/main/crates/cv-core/src/harness/grok.rs).

1. **Name it**: `Yours => "yours",` in `harnesses!` (`crates/cv-core/src/ir.rs`). The macro
   embeds `formats/yours.toml`; the crate does not compile without step 5.
2. **Write `harness/yours.rs`**:

   ```rust
   pub struct Yours { root: Option<PathBuf> }

   impl Yours {
       /// Resolve the store from `$HOME` / the env (existing paths only).
       pub fn detect() -> Self {
           Yours { root: crate::env::home_dir().map(|h| h.join(".yours/sessions")).filter(|p| p.exists()) }
       }
   }

   impl Adapter for Yours {
       fn harness(&self) -> Harness { Harness::Yours }
       fn roots(&self) -> Vec<PathBuf> { self.root.clone().into_iter().collect() }
       fn discover(&self) -> Result<Discovered> { /* walk the root, one SessionRef per session */ }
       fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
           /* `store::jsonl::drive` with your `jsonl::Codec`; sink each Message */
       }
   }
   ```

   - **`roots`**: directories read from, primary first (emitters write there; the catalog
     watches each); for SQLite, the database's directory.
   - **`discover`**: `SessionRef { cwd, title, ..SessionRef::new(id, h, path) }` per session;
     unopenable sources go to `Discovered::skipped` (`Discovered::keep`). `message_count` (prompts
     and replies) stays `None` unless free.
   - **`stream`**: sink each `Message` as read, starting from `Session::new(id, h)`. JSONL: a
     `store::jsonl::Codec` ([`CODEC.md`](https://github.com/emberian/cv/blob/main/docs/design/CODEC.md))
     run by `store::jsonl::drive`, which counts `skipped_lines`, checkpoints and guards the source.
     SQLite: `store::sqlite::Cells`. Whole-only stores: `stream::replay`. `adapter.parse(&r)` is
     `stream::collect` over `stream`.

3. **Decode through the wire codec**: Anthropic content via `wire::anthropic::block_from_view_with`,
   OpenAI via `wire::openai::blocks_from_view` and `tool_call_from_view`; a renamed key
   (OpenClaw's `toolCall`) is a `Quirks` constant in `harness/wire/`. `wire::joined` flattens
   string-or-parts. SQLite: `store::sqlite::open_ro` (read-only, no lock), `columns`,
   `table_exists`. XDG: `store::xdg_data_dir`.
4. **Register it**: one arm in `harness::for_harness`, one entry in `ORDER` (`harness/mod.rs`).
5. **Add `formats/yours.toml`** (below).
6. *(Optional)* **Make it a port target**: `harness/yours/emit.rs` with
   `pub fn emit(&Session, &Path, &EmitOptions) -> Result<EmitResult>` (e.g.
   `anthropic::assistant_blocks` plus a writer), one arm in `emitter_for` (`emit.rs`); target
   lists and `round_trip_all_supported_targets` derive from it. Declare what the store carries in
   `target_holds` (`emit/fidelity.rs`) for `port --strict`.

## Three rules the IR asks of you

Tests check all three.

**1. Set `kind` and `origin` on every message.** `Message::new(role)` defaults them
(User→`Prompt`/`Human`, Assistant→`Reply`/`Model`, Tool→`ToolResult`/`Harness`,
System→`Notice`/`Harness`); override where the store says more: `InjectedContext`,
`CompactionBoundary`, `Origin::Hook`. Consumers read `kind`, never your private fields.

**2. Harness-specific facts go in `harness_extra_mut(Harness::Yours)`** (`extra["<harness>"]`, on
a message or session), keeping the harness's key spelling; invented names are snake_case.

```rust
// yes
msg.harness_extra_mut(Harness::Yours).insert("thread_state".into(), json!(state));

// no — a flat key every other harness's code now has to not-collide with
msg.extra.insert("thread_state".into(), json!(state));
```

A message's only other top-level keys are `_record` (verbatim source, `Demand::COMPLETE`) and
`cv_byte_offset`. cv's own facts go in `extra["cv"]` (`Session::cv_extra_mut`).
`harness::assert_no_flat_keys` checks a session.

**3. Shared concepts use their IR field**: `Session::system_prompt`, `Session::lineage` (fork,
parent, continuation, sub-agent), `Block::ToolResult::details`, `Usage`, `MessageKind::Error`. A
bag fact another harness shares means the IR lacks a field; say so.

## The format manifest

`formats/<harness>.toml` records the verified upstream, the store path, and vocabulary the
adapter does not match by name.

```toml
harness = "yours"
source_files = ["harness/yours.rs", "harness/wire/anthropic.rs", "harness/wire/mod.rs"]
ignore_literals = ["jsonl"]
store = ["~/.yours/sessions/<id>.jsonl"]

[upstream]
repo = "https://github.com/someone/yours"
commit = "a1b2c3d"
date = "2026-09-19"

[types]
"record.checkpoint" = { status = "carried", note = "replayed on same-harness port" }
"record.telemetry" = { status = "ignored", note = "UI-only" }
"system.away_summary" = { status = "generic", note = "the `_` arm makes every other subtype a notice" }
"table.events" = { status = "handled", note = "a SQL identifier, not a match arm" }
```

A type with a match arm is `handled` by inference; do not list it. Write `generic` (blanket
rule), `carried` (verbatim carrier under `Demand::COMPLETE`), `ignored`, and `handled` only for
what no arm names (SQL table, store path, tool name). `ignore_literals`: arm literals that are not
vocabulary. `source_files` includes the wire codec.

`cargo test -p clustervision-core --test formats_manifest` checks it: listed `handled` types
appear in the source, none restates an arm, every `ignore_literals` entry is an arm literal.
[`cv formats`](cli.md#cv-formats) names record types real sessions carry that the manifest lacks.

A harness `cv port` writes can add `[load]`: how the harness itself continues a session, with
`${session}`, `${prompt}`, `${base_url}`, `${api_key}` and `${home}` filled in by the caller.

```toml
[load]
protocol = "anthropic_messages"   # or openai_responses, openai_chat, gemini_generate_content, antigravity_gateway
binary = "yours"
verified = "1.2.3"                # the version last run
argv = ["--resume", "${session}", "--print", "${prompt}"]
store_env = { YOURS_HOME = "${home}/.yours" }
installed_dirs = ["${home}/.yours/sessions"]
provider_env = { YOURS_BASE_URL = "${base_url}", YOURS_API_KEY = "${api_key}" }
env = { YOURS_NO_UPDATE = "1" }
```

`CV_L2=yours cargo test -p clustervision --test l2_continuation_matrix` ports a session into a
scratch store, runs that command against a loopback provider, and checks the prior turns, the
paired tool call and result, and the working directory reached the model.

## Send us your transcripts

Open an issue or PR with a few real session files (`cv export --redact`); old format variants
are welcome. 💜

`python3 -I tools/check-size.py` reports line drift since the last release tag, with no targets.
Design documents stay under 250 lines.
