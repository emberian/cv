# 💛 Adding a harness to clustervision

So you run a coding agent we don't support yet — or you have **old logs in a format we've never seen**.
Adding a parser is one file. This guide walks you through it, and (just as important) how we handle the
fact that harnesses *change their format over time* and we want to parse **every historical variant**.

> 🙏 **We especially need your logs.** The maintainers don't have recent sessions from every harness, and we
> can only test the variants we can see. If you have a `~/.something/` full of transcripts, you are *exactly*
> the contributor this project needs. Even just attaching a redacted sample to an issue helps enormously.

## The shape of an adapter

Every harness implements one trait ([`crates/cv-core/src/harness/mod.rs`](crates/cv-core/src/harness/mod.rs)):

```rust
pub trait Adapter {
    fn harness(&self) -> Harness;
    fn storage_root(&self) -> Option<PathBuf>;   // where its sessions live ($HOME-relative); None = not installed
    fn storage_roots(&self) -> Vec<PathBuf>;     // optional: every root, if it reads several (default: storage_root)
    fn discover(&self) -> Result<Vec<SessionRef>>; // cheap: list sessions w/ id, cwd, title, times, count
    fn parse(&self, r: &SessionRef) -> Result<Session>; // full: produce the IR
}
```

(Being a *conversion target* — emitting the IR back out — is deliberately **not** part of the trait;
see step 6.)

You map the harness's on-disk data onto the IR ([`ir.rs`](crates/cv-core/src/ir.rs)) — see the
[OpenSession standard](docs/OPENSESSION.md) for what each field means:
`Session → Vec<Message> → Vec<Block{Text|Thinking|ToolUse|ToolResult|Image}>`, four roles
(`system`/`user`/`assistant`/`tool`).

## Step by step

1. **Find the storage.** Where does the harness write transcripts? (`~/.foo/`, `~/.local/share/foo/`,
   `~/.config/foo/`, a SQLite db, …). Document it in [`docs/FORMATS.md`](docs/FORMATS.md) as you go.
2. **Reverse-engineer the record format.** Read *real files*. Note: roles, how content is shaped (string vs
   array of blocks), how tool calls + results are linked, timestamps, ids/threading, model, and crucially
   **how the working directory is recorded** (it's almost always the key to discovery + the cause of dir-jailing).
3. **Write `crates/cv-core/src/harness/<name>.rs`.** Implement `discover` (cheap metadata scan) and `parse`.
   Add a pure `pub fn parse_str(id, text, …) -> Session` if the format is single-file — that's what powers the
   WASM web viewer (`ingest`), which has no filesystem.
4. **Register it** in `harness/mod.rs` (`pub mod <name>;` + push into `all()`) and add a `Harness` variant in
   `ir.rs` (`as_str` / `parse`). If you read SQLite, gate the module behind the `sqlite` feature.
5. **Test against real data.** `cargo run -p cv -- ls --harness <name>` then `cv show <id>`. Add a small
   redacted fixture + a unit test.
6. **(Optional) emit.** Make your harness a *conversion target* — then `cv convert … --to <name>` works.
   Write an emitter with the signature
   `pub fn emit(&Session, out_dir: &Path, &EmitOptions) -> Result<EmitResult>` (in your adapter module, or
   in `emit.rs` alongside the others) and register it in **`emitter_for` in
   [`crates/cv-core/src/emit.rs`](crates/cv-core/src/emit.rs)** — that match is the *single* emit registry;
   the dispatcher and the CLI's `supported_targets()` list both derive from it, so there is nothing else to
   keep in sync. The bar: `parse(emit(s)) == s` (round-trip). Add a round-trip test (see `emit.rs`).

## 🕰️ Historical variants — this is the hard, important part

Harnesses are living software. **Their on-disk format drifts**, and a real archive contains years of
overlapping variants. Codex alone has at least three shapes we handle (a `session_meta`-wrapped first line, a
bare `{id,timestamp}` header, and a 2025 single-JSON `{session, items[]}` file). OpenCode has a pre-`parts`
generation that stored an inline `summary` and a newer one with separate part files. **A core goal of
clustervision is to parse ALL historical variations accurately** — your five-year-old logs should open.

How we do that, and what we ask of you:

- **Sniff by content, not by a version field.** Version fields are often absent or lie. Detect the variant from
  the actual bytes (first-line shape, presence of a key field). See `cv-core::ingest::sniff` for the pattern.
- **Be relentlessly tolerant.** Treat every field as optional (`Option` / `serde(default)`). Skip a malformed
  line and keep going — never fail a whole session over one bad record. Unknown record types are ignored, not
  errors.
- **Branch, don't fork.** Add a new variant as another arm in the *same* adapter (a `match` on the sniffed
  shape), sharing the mapping code. Resist a second adapter for "the old format."
- **One fixture per variant.** When you encounter a new shape, commit a tiny redacted sample under
  `crates/cv-core/tests/fixtures/<harness>/<variant>.jsonl` and a test asserting it parses. That fixture is how
  we keep your variant working forever. **This is the single most valuable thing you can contribute.**
- **Prefer additive IR changes.** If a harness has a concept the IR can't express, propose a new `Block` kind
  (additive, forward-compatible) rather than overloading an existing one. Until then, stash it in `Message.extra`.
- **Redact before sharing.** Scrub API keys, tokens, paths you don't want public. A structurally-faithful but
  redacted fixture is perfect.

## Modeling the rich stuff

Transcripts are full of structured objects — diffs, file edits, todo lists, sub-agent spawns, rate-limit
events, citations. v0.1 funnels these into `text`/`toolUse`/`toolResult` + `extra`. If you find a structured
object that deserves first-class modeling, open an issue — we'd love to grow the IR thoughtfully (additively).

## Checklist

- [ ] storage location documented in `FORMATS.md`
- [ ] adapter in `harness/<name>.rs` with `discover` + `parse` (+ `parse_str` if single-file)
- [ ] registered in `mod.rs` + `Harness` variant in `ir.rs`
- [ ] a redacted fixture + test for **each** format variant you've seen
- [ ] `cargo test` green; `cv ls --harness <name>` shows your sessions
- [ ] (optional) an emitter registered in `emitter_for` (`emit.rs`) + a round-trip test

Thank you for making the archive a little more universal. 🔮💜
