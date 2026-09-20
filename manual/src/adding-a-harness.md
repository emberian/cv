# Adding a harness

Got a coding agent we don't support yet? Adding one is a single new module implementing the
[`Adapter`](architecture.md) trait. The full walkthrough is in
**[ADDING_HARNESS.md](https://github.com/emberian/cv/blob/main/ADDING_HARNESS.md)**; the shape:

1. **`discover()`** — cheaply enumerate sessions on disk into `SessionRef`s (id, path, cwd, title,
   timestamps, a message count). No full parse.
2. **`parse()`** — turn one `SessionRef` into the unified [IR](architecture.md#the-unified-ir)
   (`Session → Message → Block`). This is the only *required* heavy lifting.
3. **`emit()`** *(optional)* — write the IR back into the harness's native on-disk format, making it
   a [port target](conversion.md). Pair it with a round-trip test (emit → re-parse → compare).
4. Register it in `harness::all()` and add a `Harness` enum variant.
5. Add a manifest at `formats/<harness>.toml` (below).

## Three rules the IR asks of you

These are what keep an adapter from becoming a pile of special cases every other module has to know
about. All three are checked by tests, not just convention.

**1. Set `kind` and `origin` on every message.** `role` says *who* speaks; `kind` says *what the
turn is* and `origin` says *where it came from*. `Message::new(role)` fills in a default
(User→`Prompt`/`Human`, Assistant→`Reply`/`Model`, Tool→`ToolResult`/`Harness`,
System→`Notice`/`Harness`) so the type is always inhabited — but leaving the default in place where
the store actually told you more is a bug. A system reminder the harness injected is
`MessageKind::InjectedContext`, not `Notice`. A compaction seam is `CompactionBoundary`, not a flag
in your `extra` bag. A hook's stdout is `Origin::Hook`. Consumers — `cv doctor`, `compaction.rs`,
the dataset exporter, every emitter — read `kind`, and they will never look for your harness's
private field.

**2. Put harness-specific facts in the harness bag.** Use
`msg.harness_extra_mut(Harness::Yours)` / `session.harness_extra_mut(Harness::Yours)`, which hand
you the `extra["<harness>"]` object. Never write a flat key at the top level of `extra`. Keep the
harness's own spelling for the keys inside (`attachment_type`, `history_mode`); where you invent a
name, make it snake_case.

```rust
// yes
msg.harness_extra_mut(Harness::Yours).insert("thread_state".into(), json!(state));

// no — a flat key every other harness's code now has to not-collide with
msg.extra.insert("thread_state".into(), json!(state));
```

The one top-level key that is *not* a harness name is `_record`, the verbatim source record carried
under `ParseOptions::complete`.

**3. Shared concepts get their first-class home, not a bag entry.** Before you reach for `extra`,
check whether the IR already has the field: the system prompt is `Session::system_prompt`; fork,
parent, continuation and sub-agent pointers are `Session::lineage`; structured tool-result state is
`Block::ToolResult::details`; token counts and cost are `Usage`; an unanswered API error is
`MessageKind::Error`. Something in `extra` that another harness also has is a sign the IR is missing
a field — say so rather than burying it.

## The format manifest

Every adapter is paired with `formats/<harness>.toml`, which records the upstream it was verified
against and every persisted type it knows about:

```toml
harness = "yours"
source_files = ["harness/yours.rs"]
store = ["~/.yours/sessions/<id>.jsonl"]

[upstream]
repo = "https://github.com/someone/yours"
commit = "a1b2c3d"
date = "2026-09-19"

[types]
"record.message" = { status = "handled", note = "" }
"record.checkpoint" = { status = "carried", note = "replayed on same-harness port" }
"record.telemetry" = { status = "ignored", note = "UI-only" }
```

`status` is one of `handled` (a per-item match arm in the adapter), `generic` (interpreted by a
blanket rule, so no literal appears), `carried` (kept verbatim as a `carrier` message under
`ParseOptions::complete`), or `ignored` (known and deliberately skipped).

A test asserts the manifest and the adapter agree in **both** directions: every `handled` type
appears in the adapter's match arms, and every type-like literal the adapter matches on appears in
the manifest. So a new match arm fails the build until the manifest names it, and a manifest entry
for a type you removed fails too. [`cv formats check`](cli.md#cv-formats) runs the same comparison
on demand, and [`cv formats census`](cli.md#cv-formats) parses real sessions in complete mode and
tells you what the harness is writing that your manifest has never heard of — which is how you find
out upstream shipped a new record type.

## Send us your transcripts

The fastest way to get it right: **send us your transcripts.** We can only test what we can see, and
historical format variants are an explicit goal. Open an issue or a PR with a few real (redacted, if
you like — `cv redact` helps) session files. 💜
