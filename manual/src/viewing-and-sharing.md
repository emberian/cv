# Viewing & sharing: the app and `cv share`

The app reads, searches, compares and composes sessions. 🔮 The app is one static web UI — a header, a drop zone, a row of view tabs, and the active view — that runs two ways, with no code differences between them:

- **The Tauri desktop app.** A thin native shell around that same UI. It reads **all** of your real local sessions natively (no zip, no upload, zero setup), bundles a `cv serve` for the live Fleet dashboard, and adds native redaction and zip ingest. This is the one to install if it's your machine.
- **The browser build.** Zero-install, hosted at the GitHub Pages site (<https://emberian.github.io/clustervision/>). You **drop a harness `.zip`** (or an OpenSession `.json`) onto the page and explore. Nothing is uploaded — parsing happens in your browser via WASM. Great for a session a teammate sent you, or for poking around without installing anything.

Both open on a small **bundled sample** so the app is never empty, then quietly try to load your real sessions (see [Loading your sessions](#loading-your-sessions) below).

## The views

The tab bar across the top switches between the views. They all read from one shared **pool** of sessions, so whatever you've loaded — your local corpus, a dropped zip, a File → Open import — is visible everywhere at once.

### 🗂 Sessions — the reading room

The classic two-pane view: a searchable, filterable list on the left, the full transcript on the right. This is where you actually *read* a session.

The **list** uses indexed exchange search when a usable server index is available. Otherwise it
filters downloaded metadata and loaded message content, and labels that narrower scope. It also
has per-harness filter chips and sorting by relevance / recency / oldest / title / message count. The count line ("142 of 1,203 sessions") tells you how hard your filters are biting.

The **transcript** renders a session as role-labeled turns: prose as safe Markdown, collapsible thinking blocks (including honest "[encrypted reasoning blob]" placeholders for harnesses like Codex and Grok), `tool_use` as labeled JSON with a copy button, `tool_result` with error styling, and images/files as labeled cards. The header shows model, cwd, branch/commit, token totals, and id, with one-click export to **OpenSession `.json`** or **Markdown**. Large sessions render in chunks; server-backed transcripts load message pages on demand.

### ◈ Projects — the repo lens

Sessions belong to repositories (their `cwd`), and over months many agents across many harnesses work in the same repo — but a flat list hides that entirely. Projects groups your whole corpus by working directory. Each project is a card with a **per-harness breakdown bar** (a stacked bar tinted by harness, biggest slice first), a time span ("Mar '26 → 2h ago"), and an **expandable** list of its sessions (lazily rendered on first open). The question it answers: *"What has happened in this repo, across all my agents, ever?"* Click any session to jump to it in the Sessions view.

### 📈 Timeline — the scrying glass

A bird's-eye view of every session across all harnesses and all of time. Up top is a **GitHub-style activity heatmap** — a constellation of your working days, one cell per day, shaded by how many sessions you ran. Click a lit cell to jump that day into the feed below (it flashes). Above the heatmap sit a few at-a-glance numbers (active days, busiest day, days spanned), and below it a **day-grouped feed** — every session as a node, newest first, with a harness legend and totals. Click a node to open it. (The screenshot on the [first page](getting-started.md) is this view.)

### 🔍 Compare — side-by-side, aligned

Pick **two** sessions and view them side by side with message-level alignment. The app walks both message lists in parallel: while they match (same role + same flattened content) the rows are marked **shared** (a calm amethyst tint, ✓ in the gutter); from the first divergence onward everything is flagged **diverged** (warm tint, ≠ or •). The summary tells you the shape at a glance — *"37 shared messages, then diverges"*. This is exactly what you want for [Splice](#-splice--the-composer) branches that share a common prefix and then split. Use the **⇄** button to swap A and B. (Picks aren't auto-aligned — aligning two huge arbitrary sessions on every visit was useless and slow, so *you* choose the pair.)

### 📊 Stats — the dashboard

A small, tasteful dashboard over the loaded pool: total sessions / messages / projects / harnesses, input & output token sums, sessions-per-harness bars, messages-by-role and block-kind breakdowns, top working directories, and an activity histogram over the date range. Charts are hand-rolled CSS bars and a tiny inline SVG — no chart library. It's **honest about lazy loading**: role, block, and token breakdowns reflect only the sessions whose transcripts are actually loaded, and it says so ("…breakdowns reflect the 12 opened sessions — open more to enrich them").

### ✨ Splice — the composer

The headline, and [`cv splice`](cli.md#cv-splice) in the browser. A three-pane workspace for **weaving new transcripts** out of old ones:

1. **Source** — pick any loaded session; its transcript renders with a **＋ splice** button on every message.
2. **Composition** — the messages you've collected, in order. Reorder with ▲ ▼ or drag-and-drop, remove with ✕, retitle, and **⑂ branch from here** to spawn a new branch keeping everything up to that point.
3. **Live preview** — the composed transcript, rendered exactly as it will export.

Beyond rearranging, the composer can generate: **⚡ generate continuation** sends the current composition to an LLM and appends the model's reply as a new assistant turn (streamed in live). Combined with branching, that grows a transcript into **alternate futures** — a branch tree you can navigate and then drop into [Compare](#-compare--side-by-side-aligned). Export the active branch as OpenSession `.json` or `.md`, entirely client-side.

Generation calls **OpenRouter** directly, in the desktop app and the browser alike. Open **⚙ generation** to paste an API key (stored only in this device's `localStorage`, sent only to `openrouter.ai`) and choose a model.

### 📡 Fleet — the live dashboard

A live window onto a running [`cv serve`](serving.md). It polls the server's HTTP API (default `http://localhost:7777`) every ~2 seconds and shows a **recent-sessions** feed across every harness. Pause/resume and reconnect controls live in the toolbar.

In the **desktop app** this Just Works — it bundles and launches `cv serve` on startup, so the Fleet tab is live the moment you open the app. In a **browser** it works too, as long as you have `cv serve` running locally. If the API is unreachable, Fleet shows a friendly "start `cv serve`" hint and lets you load a **static sessions `.json`** export instead. See [the server chapter](serving.md) for what `cv serve` exposes; what the agents are committing to lives in [tasks](tasks.md).

## Loading your sessions

There are three ways sessions get into the pool, and they all merge into one searchable corpus.

### 1. Your real local sessions (automatic)

On startup the app loads the machine's real on-disk sessions — **everything**, every harness, with zero configuration:

- **Desktop:** via the native `api` command (the server's read API, in-process). No HTTP, no CORS, no mixed-content surprises.
- **Browser:** by trying a local `cv serve` over HTTP (`/api/sessions`). If it isn't running — the normal case for the static web deploy — it falls back to the bundled sample.

### 2. Lazy hydration (metadata stubs → full transcripts on demand)

Loading thousands of sessions stays fast because the pool is filled with lightweight **stubs**: id, harness, cwd, title, dates, message count — but **no messages**. The list, search, Projects, Timeline, and Stats all work on stubs alone. Opening a server-backed transcript fetches its head and first message page; **load more** fetches
the next page. Compare and Splice can request the complete transcript. Stats labels breakdowns
that reflect only loaded messages.

### 3. Drop a zip, or File → Open

Drag one or more **`.zip`** harness exports or OpenSession **`.json`** files onto the drop zone — multiple at once, both formats, all merging into the pool. In the **browser** the zip is unpacked and parsed by the WASM module; in the **desktop app** zips are ingested natively in Rust (no WASM needed), and there's a native **File → Open zip…** menu item (`Cmd/Ctrl+O`) that runs a real file dialog and surfaces errors in a native message box. (For the gory details of how exports become OpenSession, see [Cross-harness conversion](conversion.md).)

The drop zone shows a **source chip** per loaded source ("local (cv serve) **1203**", "myexport.zip **8**") so you always know what's in your pool. Dropping your first real data clears the sample automatically.

## Sub-agent trees

When a session spawned sub-agents — Claude Code's `Task` sub-agents — the transcript shows a collapsible **"N sub-agents spawned"** panel just below the header. Each child is **lazy-loaded**: expanding a row fetches that sub-agent's full transcript and renders it inline (recursively, as a nested transcript). Once loaded, the row is **relabeled with the sub-agent's task prompt** (its first user message) instead of a bare id, so you can see what each one was asked to do. Sub-agents live relative to their parent rather than in the main pool, so they don't clutter the session list.

## The searchable session picker

Anywhere you pick a session from many — Compare's two sides, Splice's source — you get a **command-palette-style picker** instead of a hopeless 2,000-option dropdown. Click the trigger (which shows the current pick as a harness badge + title) to open a filterable popover. Type to narrow: it does an **AND over your terms** across title, project, and harness ("claude payments" narrows to both), shows the most recent matches with badge · title · project · when · message count, and footers a "…and 1,140 more — keep typing to narrow" when there's overflow. Full keyboard nav: ↑/↓ to move, Enter to pick, Esc to close.

## Keyboard shortcuts

Press **?** anytime for the in-app cheat sheet.

| Key | Action |
| --- | --- |
| `1` – `8` | Switch view (Sessions, Projects, Timeline, Compare, Stats, Structure, Splice, Fleet) |
| `/` | Focus the session search (switches to Sessions first) |
| `j` / `k` · `↓` / `↑` | Move the selection in the session list |
| `Enter` | Open the focused session |
| `t` | Cycle theme (dark → light → auto) |
| `Esc` | Close help · back to the session list |
| `?` | Toggle the keyboard-shortcut help |

(Shortcuts never fire while you're typing in a search box or input.)

## What the desktop app adds

The browser build is fully capable, but the Tauri desktop shell adds the things a sandboxed web page can't do:

- **Native access to all your local sessions** — no zip, no HTTP round-trip, no upload — via one native `api` command that runs the server's read API in-process.
- **A bundled `cv serve`** launched on startup (and toggleable from **View → Toggle cv serve**), so the Fleet dashboard is live out of the box. It's reaped when you quit.
- **Native zip ingest** — File → Open zip… reads the zip in Rust, without the WASM module. (Generation in the splice composer is the OpenRouter path in both the app and the browser.)
- A **native menu** (File / Edit / View / Help — Edit so `Cmd+C`/`Cmd+V` work in the transcript, which is the whole point of a viewer), DevTools, and **window state persistence** across launches.

**Parsing stays local.** Generating a continuation sends the selected composition to OpenRouter.

## Sharing a transcript: `cv share`

`cv share` is asciinema for agent sessions: one command, one `.html` file, and your whole transcript is something you can post to a forum, attach to a bug report, or DM to a friend. The recipient needs **nothing installed** — they double-click the file and read it in any browser, offline.

```sh
cv share da9174f4                       # → ./da9174f4….html, redacted
cv share da91 --out incident.html      # pick the filename
cv share da91 --harness codex          # disambiguate, like everywhere else
cv share da91 --no-redact              # keep secrets (loud stderr warning)
```

### What you get

A single self-contained document — inline CSS, inline JS, inline data. No CDN links, no fetches, no telemetry; a `Content-Security-Policy` meta tag in the file forbids the document from loading anything remote even if it wanted to. Open it from `file://` and everything works.

The page itself:

- **a sticky header** — 🔮, the session title, a color-coded harness badge, model, date, working directory (shortened to `~/…`), short id, and a live message count;
- **role-styled message cards** with timestamps — user / assistant / system / tool each get their own accent;
- **collapsible folds** for thinking blocks, tool calls, and tool results — each fold's summary line previews the content (the command being run, the file being edited, the result size) so a collapsed transcript still *reads*;
- **code blocks** with a tiny built-in syntax highlighter (no JS dependencies — it's ~40 lines of inline script operating on text only);
- **keyboard nav**: `j`/`k` move between messages, `x` toggles the focused message's folds, and the header button expands/collapses everything;
- **dark theme by default** (the 🔮 violet), light theme automatically via `prefers-color-scheme`;
- **a footer** crediting the `clustervision` version that produced the file.

Every session-derived string is HTML-escaped on the way in — a transcript containing `<script>` renders as text, never as markup.

### Redaction is the default

Before anything is written, every message passes through the same scrubber as [`cv export --redact`](cli.md#cv-export): API keys, private-key blocks, JWTs, emails, `password = "…"` assignments, and keyword-adjacent high-entropy blobs are replaced with `[REDACTED:kind]` placeholders. The artifact wears its provenance honestly:

- redacted (the default): a **🛡 redacted** badge in the header, and a footer line — *"this transcript was redacted — N secrets scrubbed"*;
- `--no-redact`: a **⚠ unredacted** badge, a *"shared without redaction"* footer, and a stderr warning at generation time.

The scrub counts are printed to stderr either way you'd expect:

```
🔮 wrote incident.html — 400 message(s), 593.6 KB
🛡 redacted 13 item(s): 0 api_key, 0 private_key, 0 jwt, 13 email, 0 blob, 0 assignment
```

Redaction is a best-effort pattern scrubber, not a guarantee — for anything sensitive, skim the artifact before posting it. (It's a single readable HTML file; skimming is the point.)

### Big sessions

Rendering **streams**: each message is parsed, scrubbed, rendered, written to disk, and dropped, so peak memory stays at roughly the largest single message no matter how long the transcript is. A multi-thousand-message session shares fine.

The honest limit is the *output* size, not memory: the artifact contains the full session, so a session with megabytes of tool output becomes an HTML file of megabytes (a ~2,700-message codex session lands around 7 MB). Browsers open that without complaint, but it's a chunky attachment. There's no `--range` on `share` yet; if you want to share a slice of a huge session, [`splice`](cli.md#cv-splice) the span you care about into a new session and share *that*.
