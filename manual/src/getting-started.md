# 🔮 clustervision

> *Vibecoding is a clusterfuck. When it gets hazy, you need clustervision.*

**Find, read, search, port, and visualize every AI coding-agent session you've ever run — across every harness.**

<img src="screenshot-timeline.png" alt="the activity-heatmap timeline" width="100%">

You spend hours teaching an agent your codebase — the dead ends, the decisions, the hard-won understanding. Then it becomes one of hundreds of `.jsonl` files in a folder named after a path, and you never find it again. Most harnesses only resume from the *exact directory* a session ran in. And every tool speaks its own dialect, so a Claude session can't be continued in Codex.

clustervision fixes all three. It parses [every harness it knows](harnesses.md) into one unified representation, then lets you:

- 🔎 **Search** every session — by keyword or by *meaning*.
- 🖥️ **Browse** your whole corpus in a desktop/web app — a Projects lens, an activity heatmap, side-by-side compare, stats, and **sub-agent trees**.
- 🚀 **Port** a session into another harness (any of the [emit targets](conversion.md)) or out of its directory jail — one verb, `cv port`.
- 🧠 **Let running agents coordinate** via hooks and a task substrate: durable dispatch objects and the decisions posed on them, every completion labeled as the agent's own word.
- 📦 **Pack** your whole history into context for the task you're about to start, so you never explain your codebase to an agent twice.

It's open source (MIT/Apache-2.0). Reading, indexing, packing and browser uploads stay local.
Semantic indexing downloads its embedding model on first use.

## How to read this manual

- New here? [Install](#install) and take [the 60-second tour](#60-second-tour) below.
- Living in the terminal? **[The CLI](cli.md)** is your map, and the **[command reference](reference.md)** has every flag.
- Running a fleet of agents? See **[`cv serve` / `cv watch`](serving.md)**, **[hooks](hooks.md)**, and **[tasks](tasks.md)**.
- Want the pretty pictures? **[The app](viewing-and-sharing.md)**.
- Curious how it all works? **[Architecture](architecture.md)**.

## Install

**Prebuilt binaries** (macOS / Linux / Windows · arm64 · x64 · x86) ship on every [release](https://github.com/emberian/cv/releases/latest). One-liner:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/emberian/cv/releases/latest/download/cv-installer.sh | sh
```

This installs `cv`, the one binary (`cv serve`, `cv watch` and `cv archive` are its subcommands).

Or build from source (uses the toolchain pinned in `rust-toolchain.toml`):

```sh
git clone https://github.com/emberian/cv && cd cv
cargo build --release      # → target/release/cv
make install               # cargo install --path crates/cv --force --target-dir target → ~/.cargo/bin/cv
cv --version               # cv 0.17.0 (<git sha>) — lags the checkout? run make install again
```

## 60-second tour

```sh
cv ls                         # list recent sessions across every harness
cv index                      # build/refresh the search index
cv search "retry backoff"     # full-text search
cv show <id> --last 40        # print the last 40 turns (prefix-match on the id is fine)
cv show <id> --tree           # the message thread as a tree
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

> **If you're an agent**, run `cv recipes` first: one command line per task, with the exact JSON
> keys each one returns.

## The desktop app

Prefer a GUI? The desktop app reads **all your local sessions natively** and lays them out — a
Projects lens, an activity-heatmap timeline, compare, stats, a splice composer, a live fleet
dashboard, and sub-agent trees. See **[Viewing & sharing](viewing-and-sharing.md)**.
