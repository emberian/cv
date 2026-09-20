# 🔮 clustervision

> *Vibecoding is a clusterfuck. When it gets hazy, you need clustervision.*

**Find, read, search, port, and visualize every AI coding-agent session you've ever run — across every harness.**

<img src="screenshot-timeline.png" alt="the activity-heatmap timeline" width="100%">

You spend hours teaching an agent your codebase — the dead ends, the decisions, the hard-won understanding. Then it becomes one of hundreds of `.jsonl` files in a folder named after a path, and you never find it again. Most harnesses only resume from the *exact directory* a session ran in. And every tool speaks its own dialect, so a Claude session can't be continued in Codex.

clustervision fixes all three. It parses **20 harnesses** into one unified representation, then lets you:

- 🔎 **Search** every session — by keyword or by *meaning*.
- 🖥️ **Browse** your whole corpus in a desktop/web app — a Projects lens, an activity heatmap, side-by-side compare, stats, and **sub-agent trees**.
- 🚀 **Port** a session into another harness (N-way among 13 emit targets) or out of its directory jail — one verb, `cv port`.
- 🧠 **Let running agents read each other's minds** via an MCP server, and coordinate via a shared board — plus a task substrate whose landings are *verified from git*, never taken on an agent's word.
- 📦 **Pack** your whole history into context for the task you're about to start, so you never explain your codebase to an agent twice.

It's open source (MIT/Apache-2.0) and runs **entirely locally** — nothing is uploaded.

## How to read this manual

- New here? Start with **[Install & quick start](getting-started.md)**.
- Living in the terminal? **[The CLI](cli.md)** is your map.
- Running a fleet of agents? See **[`cvd`](daemon.md)**, **[MCP](mcp.md)**, **[the board](board.md)**, and **[tasks](tasks.md)**.
- Want the pretty pictures? **[The app](app.md)**.
- Curious how it all works? **[Architecture](architecture.md)**.
