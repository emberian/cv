# Install & quick start

## Install

**Prebuilt binaries** (macOS / Linux / Windows · arm64 · x64 · x86) ship on every [release](https://github.com/emberian/cv/releases/latest). One-liner:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

This installs `cv`, `cv-mcp`, `cvd`, `cv-tui`, and `cv-search`.

Or build from source (needs a recent Rust toolchain):

```sh
git clone https://github.com/emberian/cv && cd cv
cargo build --release      # → target/release/{cv, cv-mcp, cvd, cv-tui, cv-search}
```

## 60-second tour

```sh
cv ls                         # list recent sessions across every harness
cv search "retry backoff"     # full-text search
cv show <id> --last 40        # print the last 40 turns (prefix-match on the id is fine)
cv tree <id>                  # the message thread as a tree
cv port <id> --harness codex  # port a session into another harness
cv scry                       # live-follow every agent on your machine
```

Most commands take a **session id prefix** (the first few characters are usually enough), or a
fully-qualified `harness:id`, plus an optional `--harness <name>` to disambiguate. An ambiguous
prefix lists the candidates instead of guessing.

`cv --help` groups every command under **Read**, **Reshape**, **Export**, **Fleet & live** and
**System**; `cv <command> --help` has the full flag set. Reading a long session? The same five
[window flags](cli.md#message-windows) — `--first`, `--last`, `--range A..B`, `--around`,
`--max-bytes` — work on `cv show` and `cv export` alike.

> **If you're an agent**, run `cv recipes` first: ten command lines with the exact JSON keys each
> one returns.

## The desktop app

Prefer a GUI? The desktop app reads **all your local sessions natively** and lays them out — a
Projects lens, an activity-heatmap timeline, compare, stats, a loom composer, a live fleet
dashboard, and sub-agent trees. See **[The desktop & web app](app.md)**.
