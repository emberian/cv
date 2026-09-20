# clustervision — web viewer

A static, **client-side** web app for browsing, comparing, and **splicing** agent
sessions. Drop one or more `.zip`s of a harness directory (a `.claude/projects/…`
tree, `.codex/sessions/…`, `.grok/sessions/…`, an OpenCode `storage/` tree, etc.)
**or** OpenSession `.json` files — they all merge into one pool you can explore,
chart, diff, and weave into new transcripts. Entirely in your browser. Nothing is
uploaded.

It also runs inside the **Tauri desktop shell** (the `app/` wrapper) and degrades
gracefully to a plain browser — see **Desktop (Tauri) integration** below.

## Keyboard shortcuts

Press **`?`** (or the header `?` button) for an in-app overlay. The essentials:

| Key | Action |
| --- | --- |
| `1`–`7` | Switch view (Sessions … OpenSession) |
| `/` | Focus the session search |
| `j` / `k` · `↓` / `↑` | Move the selection in the session list |
| `Enter` | Open the focused session |
| `t` | Cycle theme (dark → light → auto) |
| `Esc` | Close help · back to the session list (narrow screens) |
| `?` | Toggle the help overlay |

Shortcuts never fire while you're typing in an input, and respect focus.

## How it works

- `index.html` loads `main.js` as an ES module.
- `main.js` dynamically imports the WASM module at `./pkg/cv_web.js` (built by the
  `cv-web` crate via `wasm-pack`). If present, dropped `.zip` bytes are handed to
  `ingest_zip(bytes: Uint8Array)`, which returns a JSON string — an array of
  `Session` objects.
- Dropped `.json` files are parsed **directly in JS** (no wasm needed): an
  OpenSession document, an array of sessions, or a `{ sessions: [...] }` wrapper.
- If the WASM module **isn't** present (the static-only fallback deploy), the app
  runs in **demo mode** using the bundled `sample.js` dataset. `.zip` ingest is
  disabled in that mode, but OpenSession `.json` drops still work.

Everything dropped is **normalized** to one internal shape (`components/util.js`),
so the internal IR (snake_case, blocks tagged `type`: `tool_use`, `created_at`,
`data_ref`, …) and the OpenSession interchange shape (camelCase, blocks tagged
`kind`: `toolUse`, `createdAt`, `dataRef`, `parentId`, …) both load and render
identically. See [Session schema](#session-schema).

## Views

A tab bar (`<cv-app>`) switches between views, all reading from the merged pool:

- **🗂 Sessions** — the classic two-pane list + transcript.
- **📈 Timeline** (`<cv-timeline>`) — every session across every harness on one
  chronological axis, grouped by day, colored by harness. Click to open.
- **🔍 Compare** (`<cv-compare>`) — pick two sessions side-by-side; messages are
  aligned and divergence is highlighted (shared prefix dims, the split is marked).
  Great for inspecting loom branches.
- **📊 Stats** (`<cv-stats>`) — two clearly separated halves, because two
  different questions were being answered as if they were one. **The archive**
  (session and message totals, per-harness bars, top working directories, date
  range, activity histogram) comes from `/api/stats` — exactly what
  `cv stats --json` emits — and falls back to tallying the loaded pool, saying
  which it did. **Opened transcripts** (tokens, reasoning tokens, provider cost,
  message kinds, origins, block types, roles) is headed with how many transcripts
  it actually covers, e.g. "3 of 6,873 downloaded". A corpus number and a
  one-session number never share a panel. Hand-rolled CSS bars + inline SVG — no
  chart library.
- **🌳 Structure** (`<cv-forest>`) — the session-structure explorer for one root
  session: the sub-agent **forest** (direct + workflow agents), **workflow phase
  lanes**, **tool** histograms + a phase×tool **heatmap**, and the **compaction**
  seams (with the carried-forward summaries). See **Structure explorer** below.
- **✨ Loom** (`<cv-loom>`) — the headline. A three-pane composer: pick a source
  session, click **＋ loom** on any message to collect it, then reorder (▲ ▼ or
  drag), remove (✕), or **fork from here** (⑂). A live preview renders the
  composition as you build it. **Download** it as an OpenSession `.json` or
  Markdown — pure client-side. Now it also *looms*: see **Generation & the branch
  tree** below.
- **📡 Fleet** (`<cv-fleet>`) — a live dashboard over a running `cvd serve` HTTP
  API. See **Fleet dashboard** below.
- **🧬 OpenSession** (`<cv-opensession>`) — the OpenSession standard, featured
  in-app. Also available as a standalone page at **`openSession.html`**.

## ⚡ Loom: generation & the branch tree

The loom no longer only *rearranges* messages — with an OpenRouter API key (or the
desktop runtime) it **generates** continuations, so you can fork a transcript and
grow alternate futures with a model.

The **branch tree** at the top of the loom is a real fork-structure
visualization, not a flat bar: roots and their forked descendants are indented by
depth with `├`/`└` guide rails, each child shows its fork point (`⑂N` = messages
shared with its parent), the active branch is highlighted, and rows are
keyboard-navigable (`↑`/`↓` to move, `Enter`/`Space` to switch). Deleting a branch
re-parents its children so the tree stays connected.

- **⚙ generation** (top-right of the loom) opens a settings panel: paste an
  **OpenRouter API key** and pick a **model** (free-text, with presets like
  `anthropic/claude-3.5-sonnet`, `openai/gpt-4o-mini`). The key is stored in
  `localStorage` on your device only and is sent **only** to
  `https://openrouter.ai/api/v1/chat/completions` over HTTPS (Bearer auth) —
  nothing else ever sees it.
- **⚡ generate continuation** converts the current composition to OpenRouter
  chat format (IR roles → `user`/`assistant`/`system`; `tool` → user context;
  text/thinking/tool blocks flattened to text) and **appends** the model's reply
  as a new IR `assistant` message in the lane. The reply **streams** in
  (`fetch` + `ReadableStream` over SSE) when the provider supports it, with a
  **stop** button; otherwise it awaits the full response. Inside the desktop
  shell this routes through the native `generate` command instead (no key — the
  bar shows **🖥 native**).
- **Branches.** The branch tree holds multiple named variants, each with its own
  lane. **＋ branch** duplicates the active branch into a child; **⑂** on any lane
  message forks a *child branch* from that prefix. Switch branches, generate
  divergent continuations, and compare them side-by-side in the **🔍 Compare**
  view (export each branch to OpenSession `.json` and drop them back in).
- Errors (bad key `401/403`, no credits `402`, rate limit `429`, …) surface
  inline and never block the UI; the key always stays client-side.

Implementation: `components/cv-loom.js` plus the dependency-free helper
`openrouter.js` (credential storage, IR→chat conversion, streaming request).

## 🌳 Structure explorer

`<cv-forest>` turns one session inside-out. A claude-code session isn't a flat
transcript — it fans out into a **forest** of sub-agents, its tool use has shape,
and its history is punctuated by compaction seams. The view has a **root picker**
and a strip of sub-views:

- **◉ Overview** — stat tiles (messages, agents `direct/in-workflows`, workflows,
  tool kinds, compactions, tokens), the session header, and a **timeline strip**:
  message-density bars (tool turns tinted separately) with every compaction
  marked as an amber ✂ cut, placed along the *full* session length (the sampled
  region is shaded when the transcript is larger than the loaded window).
- **🌳 Forest** — the agent tree: the root → **directly-spawned** (`Agent`/`Task`)
  sub-agents, each with its agent-type chip and journaled-outcome badge → then
  **workflows** as collapsible groups (the header carries a status-pip strip — the
  whole run's health at a glance). Click any agent to expand its **full transcript
  inline** (a nested `<cv-transcript>`).
- **🧬 Workflows** — each run as a **phase-lane** card grid: agents are columns by
  outcome (done / partial / open / blocked / no-status), colored, showing the
  journaled result summary. **⌗ view driving script** opens the orchestrating
  `.js` the harness recorded for that run.
- **🔧 Tools** — a **histogram** of tool calls by name across the session, plus a
  **phase × tool heatmap** (amethyst cells, opacity = intensity) showing *when*
  each tool was reached for.
- **✂ Compaction** — every boundary with its `trigger`, the context size rebuilt,
  duration, the compacted-away message span, and — collapsibly — the **summary
  that carried the context forward** (the load-bearing artifact for retrieving
  pre-compaction context). In-window boundaries get **↥ before / ↧ after** span
  previews.

The outcome vocabulary across workflows is wild (`done`, `GREEN`, `welded`,
`partial`, `YELLOW`, `blocked`, plain strings, nothing) — it's folded onto five
semantic buckets (good · warn · bad · open · neutral) so the whole forest reads at
a glance. Data comes from cvd's `/subagents`, `/messages`, `/compactions`, and
`/workflow/<wf>/script` endpoints (or the desktop's native commands); the heavy
work (the full-transcript compaction scan) is server-side, so even a
12k-message / 940-agent session stays responsive.

## 📡 Fleet dashboard

`<cv-fleet>` connects to a running **`cvd serve`** HTTP API and live-displays the
fleet by **polling every ~2s** (CORS is enabled server-side, so cross-origin
`fetch` from this static page works). Enter a **base URL** (default
`http://localhost:7777`) and a **channel** (default `fleet`, i.e. the `#fleet`
activity channel the daemon mirrors into).

It renders, all auto-refreshing with a **pause** toggle:

- an **active-agents** strip — present agents from `/api/who/<channel>`
  (recent heartbeats), harness-colored;
- a **claims table** — the distributed locks from `/api/claims/<channel>`
  (`key → owner → expires`), with soon/expired highlighting;
- a **#channel board feed** — messages from `/api/board/<channel>`
  (`{id,channel,from,ts,kind,body,tags,session_ref}`), newest first, colored by
  `kind` (`status`/`event`/`request`/`reply`/`presence`/`claim`);
- a **recent-sessions** activity feed — `/api/sessions?limit=N` (IR `Session[]`),
  harness-badged.

Polled endpoints: `/api/health`, `/api/sessions?limit=`, `/api/channels`,
`/api/board/<channel>`, `/api/claims/<channel>`, `/api/who/<channel>`.

If the API is unreachable it shows a friendly **"start `cvd serve`"** hint and
lets you **load a static board export** (`.json`: an array of `BoardMessage`s, or
an object `{ board?, claims?, who?, sessions?, channels?, channel? }`) to explore
the layout offline.

## Components

All are plain native custom elements (no framework, no build step), in `components/`:

- **`<cv-app>`** — shell: header, theme toggle, **multi-file** dropzone (with
  per-source counts), view tabs, and the active view. Merges all sources into one
  de-duplicated pool. When a `cvd` is answering, the dropzone **folds to one
  line** — the archive is already here, so the invitation stops charging every
  view ~110px; the fold keeps the source chip, still takes a drop (anywhere in
  the app), reopens from `+ add .zip / .json`, and remembers which way you left
  it. On the static demo it stays prominent, because there it *is* the way in.
- **`<cv-session-list>`** — sortable, filterable list, rendered as a **window**:
  only the rows near the viewport exist in the DOM, while the `<ul>` is stretched
  to the height the whole result set would occupy, so the scrollbar still measures
  the corpus (6,873 sessions went from 54,255 nodes on landing to ~200). `j`/`k`
  walk the *result set*, not the rendered rows. It searches two ways and says
  which in the placeholder: with a daemon, `/api/search` over **every message in
  the archive** (with the daemon's snippet in the row, best-match ordering, and a
  `≈ meaning` toggle for `semantic=1`); without one, it filters the metadata stubs
  it downloaded, which can only match titles and paths.
- **`<cv-transcript>`** — renders one `Session`, keyed on the message **kind**
  rather than the role, so a typed prompt, harness-injected context, a slash-command
  notice and an API error no longer all read as one grey "System" turn:
  - a **structural** kind (compaction boundary, model change, branch, sub-agent
    spawn/return) draws a **rule across the column** carrying what happened —
    trigger, token counts, the new model — read from the harness bag;
  - a **quiet** kind (injected context, system prompt, notice, carrier) folds to
    one line with a peek that names it (Claude's `attachment_type` when present);
  - everything else is a full turn, with an **origin chip** when the origin is not
    the obvious one (a prompt from the *scheduler*, a result from a *sub-agent*).
  Plus: a **filter strip** that hides injected context / thinking / tool calls with
  CSS alone (no re-render); the session's **`system_prompt`** in a fold and its
  **`lineage`** as navigable chips; **Markdown prose** (headings, lists,
  blockquotes, emphasis, links, inline + fenced code with a light highlighter —
  see `markdown.js`, XSS-safe by escaping before injecting); **collapsible
  thinking** (with encrypted/redacted/signature handling); `tool_use` (highlighted
  JSON, auto-collapsed when large, with its `namespace`); `tool_result` (error
  styling, `status`, `tool_name`, and structured `details` as fact chips);
  **`file`** blocks; images; and a loud fallback for unknown block types and
  unknown message kinds. Shows per-message **token usage** — including reasoning
  tokens and provider cost where recorded — and a session-total. **Very long transcripts
  are virtualized** — only a sliding window of messages is in the DOM, so a
  12k-message session renders in ~30 ms instead of freezing the tab. Has
  per-session **Markdown / OpenSession-JSON export** buttons, and an optional
  `pickMode` (used by the loom).
- **`<cv-timeline>`**, **`<cv-compare>`**, **`<cv-stats>`**, **`<cv-loom>`**,
  **`<cv-fleet>`**, **`<cv-opensession>`** — the views above.
- **`<cv-harness-badge>`** — a small per-harness colored pill.

`openrouter.js` is a dependency-free OpenRouter client used by the loom (key
storage in `localStorage`, IR→chat conversion, and a streaming
`chat/completions` request).

`components/util.js` holds shared helpers: HTML escaping, time formatting, search
indexing, **normalization** (`normalizeSessions`, accepts both shapes),
**export** (`toOpenSession`, `toMarkdown`, `downloadFile`), and token summing.
`markdown.js` is the dependency-free, XSS-safe Markdown renderer + tiny code
highlighter used by the transcript. `tauri.js` is the desktop-integration shim
(see below). `styles.css` is a hand-written "lite" stylesheet with light/dark
themes (the theme toggle cycles dark → light → auto and persists to
`localStorage`).

## Desktop (Tauri) integration

The same `web/` runs unchanged in a plain browser **and** inside the Tauri desktop
shell (the `app/` wrapper). All of `tauri.js` degrades to no-ops when
`window.__TAURI__` is absent, so the browser path is never affected.

When running under Tauri, clustervision:

- **routes generation through native commands.** The loom's **⚡ generate** calls
  `window.__TAURI__.core.invoke('generate', { messages, model })` instead of the
  in-JS OpenRouter path — **no API key needed** (the desktop env / a local LM
  Studio does the work). The gen bar shows **🖥 native**. (The same `invoke`
  pathway is ready for `distill` / `redact`.) Streaming is supported if the native
  side emits `cv://generate-token` events; otherwise the full reply is awaited.
- **loads sessions from the native File → Open.** It listens for the
  `cv://open-sessions` Tauri event (payload = a JSON array of `Session`), via
  `window.__TAURI__.event.listen`, and merges the opened sessions into the pool —
  exactly like a drop. The `app/` agent emits that event from its native menu.

In a browser none of this exists: `isTauri()` is `false`, `canInvokeNative()` is
`false`, the loom falls back to OpenRouter, and the event listener is a no-op.

## Session schema

The internal IR (serde of `cv-core`, **IR v2** as of cv 0.11). Each `Session`:

```
{ id, harness, cwd?, path?, title?, display_title?, created_at?, updated_at?, model?,
  git?{branch?,commit?,remote?}, system_prompt?, lineage?, extra?, size_bytes?,
  messages: [ Message ], source_path? }

Message = { id?, parent_id?, role, kind, origin, timestamp?, model?, usage?,
            content: [Block], extra? }

lineage = { forked_from?, parent?, spawned_by_tool_use?, continued_in?, continues?, agent_path? }
usage   = { input_tokens?, output_tokens?, cache_read_tokens?, cache_creation_tokens?,
            reasoning_tokens?, cost_usd? }
```

`role` (WHO speaks) is `system | user | assistant | tool`.

`kind` (WHAT the message is) is `prompt | reply | tool_result | injected_context |
system_prompt | notice | compaction_boundary | compaction_summary | model_change |
error | subagent_spawn | subagent_return | branch | carrier`.

`origin` (WHERE it came from) is `human | model | harness | hook | scheduler |
subagent | import | unknown`.

**A block is tagged `type`; a message is tagged `kind`.** They are different
questions and the UI must never confuse them — `m.kind` is the message kind,
`b.type` is the block type, and an *event* (from `/api/…/events`) has its own
`kind` (`file_edit | file_read | command | tool | error`) that is neither.

- `{ type: "text", text }`
- `{ type: "thinking", text, signature?, encrypted?, redacted? }`
- `{ type: "tool_use", id, name, input, namespace? }`
- `{ type: "tool_result", tool_use_id, content, is_error, tool_name?, status?, details? }`
- `{ type: "file", mime?, path?, source? }`
- `{ type: "image", media_type?, data_ref? }`

`extra` is **nested by harness** — `m.extra.claude.attachment_type`, never a flat
`m.extra.attachment_type`. Reach it with `harnessExtra(m, harness)` from
`components/util.js`.

Dropped OpenSession `.json` still uses the interchange spelling — blocks tagged
`kind`, camelCase fields (`toolUse`, `toolResult`, `createdAt`, `parentId`,
`dataRef`, `inputTokens`, …) — and pre-0.11 cv output tagged blocks `kind` too.
`normalizeSession` in `components/util.js` is the single place that folds all
three onto the shape above; no component should read a block's `kind`. Unknown
block types and unknown message kinds are rendered with their raw record rather
than dropped.

### Self-test

`selftest.html` runs the components against `sample.js` and asserts
these invariants — every message renders exactly one node, every block type is
recognised, `extra` is read through the harness namespace, the OpenSession export
round-trips; the session list renders a window whose geometry matches the whole
result set and whose keyboard cursor walks it; the search box's placeholder never
promises more than the deployment can do, and a 404 from `/api/search` falls back
to the local filter; the stats view never mixes a corpus number with a
one-session number. No build step, no dependencies; it does need a static server,
because browsers refuse ES modules over `file://`:

    cvd serve --web ./web      # then open /selftest.html
    # or, from web/:
    python3 -m http.server     # then open /selftest.html

Run it after any IR change.

## Export & the loom

- From any transcript header: **⬇ .md** and **⬇ .json** (OpenSession).
- From the loom: the composed lane downloads as
  `{ openSession: "0.2", harness: "openSession", id, title, messages: [...] }`.

All exports are client-side `Blob` downloads — nothing leaves the page.

> **Sharing a transcript?** The CLI's `cv share <id>` produces the flagship
> artifact: one self-contained, redacted-by-default `.html` file (inline
> CSS/JS, collapsible tool calls, keyboard nav) that anyone can open offline —
> no wasm, no viewer, no install. See the manual's "Sharing transcripts" page.

## Preview locally

No build step is needed for the JS. From the repo root:

```sh
python3 -m http.server --directory web 8080
# then open http://localhost:8080
```

It starts in demo mode (sample dataset, which includes an OpenSession-format
session to exercise the normalizer) unless a local **`cvd serve`** is reachable
on `http://localhost:7777` (then the viewer loads your real on-disk sessions).

**One-command hub.** `cvd serve --web` hosts *both* the dashboard and the JSON API
from a single origin — your real sessions, the live **Fleet** view, and the
**Structure** explorer with zero extra setup:

```sh
cvd serve --web ./web        # UI at http://localhost:7777/ , API at /api/*
```

Served this way, the frontend talks to the API at its own origin automatically
(so any `--port` works); a static deploy / `file://` / the desktop app fall back
to `http://localhost:7777`, overridable via `window.__CVD_BASE__`.

To build the WASM (only needed for client-side `.zip` ingest in demo/static
mode):

```sh
wasm-pack build crates/cv-web --target web --out-dir ../../web/pkg --no-default-features
```

(run from the repo root). Then reload and drop one or more `.zip` / `.json` files.

> A plain `file://` open won't work — ES module imports require an HTTP origin.

### Demo the loom generation

1. Open the **✨ Loom** tab and click **＋ loom** on a few source messages.
2. Click **⚙ generation**, paste an OpenRouter API key, pick a model.
3. Click **⚡ generate continuation** — the model's reply streams in as a new
   assistant turn. Use **⑂** on a message to fork a sibling **branch**, switch to
   it, and generate a divergent continuation. Compare branches in **🔍 Compare**.

### Demo the fleet dashboard

Run the daemon's HTTP API alongside the static site:

```sh
cvd serve --addr 127.0.0.1:7777   # CORS-enabled
```

Open the **📡 Fleet** tab (base URL `http://localhost:7777`, channel `fleet`).
The active-agents strip, claims table, board feed, and recent-sessions feed
refresh every ~2s; use **⏸ pause** to freeze. No server? The offline panel lets
you load a static board `.json` to explore the layout.

## Deployment

`.github/workflows/pages.yml` builds the WASM module (best-effort), then publishes
the whole `web/` directory to GitHub Pages on every push to `main`. If the WASM
build fails or the crate isn't present, the static site still deploys and runs in
demo mode (with `.json` drops still functional).
