# Working in this repo (for any agent)

ClusterVision (`cv`) reads every coding-agent harness's on-disk session store into one IR. One
binary (`crates/cv`), one core library (`crates/cv-core`: IR, adapters under `harness/`, emitters,
the read API, tasks, lanes, watch), `cv-search` (tantivy), `cv-web` (wasm), `cv-testkit` (test
harness). The shape it is converging on is `docs/INTERFACE-V2.md` "The shape"; a new concept is a
`--kind`, a `--sort`, a selector or a route on the one API, never a new command family.

Datapath design and open gaps: `docs/design/DATAPATHS.md`; goal, budgets and measured numbers:
`docs/design/PERF.md`. Measurements go in commit messages, never committed result files.

## Rules

- Toolchain is pinned to 1.99.0 (`rust-toolchain.toml`); CI runs fmt, clippy `-D warnings`, the
  workspace build and tests, the no-default-features and wasm32 builds of cv-core, `cargo package`
  of the published crates, the manual build and its names check, the corpus tools test and the web
  selftest. Run them before pushing (the complete list is in `.github/workflows/ci.yml`).
- Never write to ember's real stores: `~/.clustervision` (copy it and set `CLUSTERVISION_HOME`),
  `~/.claude`, `~/.codex`, and every other harness home. Transcripts are read-only. Set a scratch
  `CLUSTERVISION_HOME` for cargo tests too: adapter parse helpers can update the counts catalog.
- Work in your own git worktree with your own `CARGO_TARGET_DIR`; two worktrees sharing a target
  dir corrupt each other's builds. Set `RUSTC_WRAPPER=sccache` so parallel worktrees share compiled
  dependencies instead of each building the whole tree. Never `git stash`. Never kill processes by pattern; stop only
  PIDs you started.
- Never run an unfiltered `cargo test -p <crate>` for exploration; filter to what you changed. The
  one full `cargo test --workspace` before pushing is the gate.
- `python3 -I tools/check-size.py` reports product lines per crate and module, and test lines
  per crate, as drift since the last release tag (read from git; nothing is committed or refreshed
  per commit). Drift never fails CI and there are no percentage ratchets or line-count targets;
  explain a consequential size change against "The shape" in the commit. No committed evidence
  files.
- A file embedded with `include_str!` must live inside its crate, or `cargo publish` fails.
- `crates/cv-core/tests/corpus/<harness>/<version>/` is real harness-written data, scrubbed. Never
  regenerate a committed version in place; add a new version. Summaries regenerate with
  `CV_REGEN_CORPUS=1` and every diff is explained in the commit.
- Stored logs and transcripts keep reading across releases; removed CLI/HTTP shims are deleted
  cleanly rather than left as tombstones.
- Judge disk pressure by free bytes and expected writes, not percentage alone. With adequate
  headroom, continue small edits and reuse warm targets. If planned writes risk exhausting space,
  stop those writes and report sizes. Delete only your own artifacts within authorized cleanup scope.
- Commits: small, one change each, unsigned is fine; say what changed shape, why, what breaks. One
  CHANGELOG bullet per user-visible change, at most 25 words. Shell is zsh: an unquoted `$var`
  does not word-split (use `${=var}`).
