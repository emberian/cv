# clustervision — desktop app (Tauri v2)

A single clickable cross-platform desktop application that wraps clustervision:
the existing **static web UI** (session viewer, timeline, compare, stats, OpenSession,
splice/loom composer, fleet dashboard) plus **native Rust backend power** (distill,
redact, generative loom, **native zip ingest**), a **native application menu**,
**persisted window state**, and an auto-launched local fleet API.

## Layout

```
app/
├── README.md            ← this file
└── src-tauri/           ← the Tauri v2 Rust app (its OWN cargo workspace)
    ├── Cargo.toml       ← declares `[workspace]` so it's EXCLUDED from the repo-root workspace
    ├── build.rs         ← tauri-build codegen
    ├── tauri.conf.json  ← v2 config; identifier `town.lunar.clustervision`
    ├── capabilities/
    │   └── default.json ← grants core + shell:allow-open + dialog:default to the main window
    ├── icons/           ← app icon set (crystal-ball theme)
    └── src/
        ├── main.rs      ← thin binary entry point
        └── lib.rs       ← Tauri runtime: native commands + cvd-serve lifecycle
```

### Why its own workspace
`src-tauri/Cargo.toml` has a top-level `[workspace]` table. That makes it a **standalone
cargo workspace**, so it is *excluded* from the main clustervision workspace at the repo
root. Building or CI-testing the main crates never compiles the app (which pulls in the
heavy WebKit/Tauri stack), and vice-versa. The app still depends on the real crates by
relative path: `cv-core`, `cv-llm`, `cv-search` at `../../crates/...`. Note that `cv-core`'s
**package** name is `clustervision-core` (renamed for crates.io in 0.10), so the manifest says
`cv-core = { path = "../../crates/cv-core", package = "clustervision-core" }` — the same form
`crates/cv/Cargo.toml` uses. Asking for a bare `cv-core` fails resolution instantly, and because
this workspace is excluded from the root one, that went unnoticed for a whole release. CI's
`desktop` job now compiles and tests it on every push.

### Frontend
No build step. `frontendDist` in `tauri.conf.json` points at the existing static web app
(`../../web`). The desktop window simply loads `web/index.html`. We do **not** modify `web/`.

## Native commands

Invoke from JS with `window.__TAURI__.core.invoke("<name>", { ... })`. Keeping these in
Rust means LLM API keys live in the desktop *process* env, never in JS:

| Command         | Args                                          | Returns                         | Backed by              |
|-----------------|-----------------------------------------------|---------------------------------|------------------------|
| `distill`       | `{ sessionJson: string }`                     | Markdown digest (`string`)      | `cv_llm::distill`      |
| `redact`        | `{ sessionJson: string }`                     | redacted Session as JSON string | `cv_core::redact`      |
| `generate`      | `{ sessionJson: string, model?: string }`     | next assistant turn (`string`)  | `cv_llm::generate`     |
| `ingest_zip`    | `{ path: string }`                            | `Session[]` as a JSON string    | `cv_core::ingest::ingest_files` |
| `provider_info` | `{}`                                          | `{ provider, available }`       | `cv_llm::available_provider` |

### Reading the machine's sessions — the `local_*` commands

The desktop app answers session reads **natively**, out of `cv_core`, instead of fetching
`http://localhost:7777` (the webview's cross-origin fetch to localhost is unreliable). Every
shape below is the one `cvd serve` returns for the same thing (`crates/cvd/src/serve.rs`), so a
session is indistinguishable between the daemon's HTTP API and this app — verified against both
`cv <cmd> --json` and a live `cvd` across all 12 harnesses present on a real machine.

| Command                 | Args                                                                   | Returns |
|-------------------------|------------------------------------------------------------------------|---------|
| `local_sessions`        | `{ limit?: number, harness?: string, cwd?: string }`                    | `SessionRow[]` |
| `local_session`         | `{ harness, id }`                                                       | the IR `Session` (identical to `cv show --json`) |
| `local_session_head`    | `{ harness, id }`                                                       | `SessionRow` + `model, git, system_prompt, lineage, extra, total` |
| `local_messages`        | `{ harness, id, start?, end?, extra?: boolean }`                        | `{ harness, id, start, end, has_more, total_known, total, message_count, session, messages }` |
| `local_events`          | `{ harness, id, kind?: string }`                                        | `{ msg_idx, ts, kind, tool, target, detail }[]` |
| `local_compactions`     | `{ harness, id }`                                                       | `{ harness, id, compactions: { index, summary_index, trigger, pre_tokens, duration_ms, summary, headline, pre_span }[] }` |
| `local_touched`         | `{ path, editsOnly?: boolean }`                                         | `{ harness, session_id, title, edits, reads, last_ts }[]` |
| `local_subagents`       | `{ harness, id }`                                                       | `SessionRow[]` + `agent_id, agent_type, description, tool_use_id, workflow, result_status, result_summary` |
| `local_subagent`        | `{ harness, parent, agent }`                                            | the sub-agent's IR `Session` |
| `local_workflow_script` | `{ harness, id, workflow }`                                             | `{ workflow, name, source }` |

Each returns a **JSON string** (`JSON.parse` it), or rejects with a readable message.

A **`SessionRow`** is `INTERFACE-V2.md` §3's session row, exactly as `cv ls --json` prints it:
`{ id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes }` —
timestamps RFC 3339, `null` when unknown.

Two IR-v2 notes for the UI:

- **`local_messages`' `extra: true`** keeps each message's harness `extra` bag *and* the
  structured `Block::ToolResult.details` sidecar (both gated on `ParseOptions::extra`, because the
  sidecar routinely dwarfs the transcript). Bulk paging leaves it off; a structure/detail view
  turns it on.
- **`local_session_head`** is the cheap way to get the session-level fields 0.11 added —
  `system_prompt` and `lineage` (`forked_from`, `parent`, `spawned_by_tool_use`, `continued_in`,
  `continues`, `agent_path`) — plus `git`, `model` and the exact message `total`, in one pass that
  keeps no messages. It is the only way to get them for Claude, whose stream emits no `meta()`.

`distill`/`generate` need a provider in the environment — see [LLM providers](#llm-providers).
`redact` and `ingest_zip` are pure and offline (no key needed). All take/return the **same
Session IR JSON** the web UI already speaks (`serde_json` round-trip of `cv_core::Session`).

`ingest_zip` reads a `.zip` from disk, unzips it **in-memory** (the `zip` crate, deflate only),
and runs the same `cv_core::ingest::ingest_files` the browser's WASM path uses — so native
file-open works **without** the WASM module. It returns the `Session[]` array as a JSON string,
the exact shape the web UI's WASM `ingest_zip` produces.

## Native application menu

The app installs a native [`Menu`](https://docs.rs/tauri/2/tauri/menu/) in `setup` (handled in
`on_menu_event`):

- **File → Open zip…** (`Cmd/Ctrl+O`) — native file dialog (`tauri-plugin-dialog`) → in-memory
  unzip + ingest → **emits the `cv://open-sessions` Tauri event** with the `Session[]` JSON
  payload. The `web/` UI can `listen("cv://open-sessions", …)` to load the sessions without WASM.
  Ingest/read errors surface in a native error dialog. The picker + ingest run off the main
  thread so the event loop never blocks.
- **File → Quit** — predefined quit item.
- **View → Reload** (`Cmd/Ctrl+R`) reloads the webview, **Toggle DevTools** (`Cmd/Ctrl+Alt+I`;
  the `tauri` `devtools` feature is enabled so this works in release bundles too),
  **Toggle Fullscreen**, and **Toggle cvd serve** (start/stop the sidecar at runtime).
- **Help → About clustervision** (predefined, with name/version/description) and
  **Open the web demo** (opens the public demo in the default browser via `shell:allow-open`).

The native dialog is driven from **Rust** (`app.dialog().file()…`), so no JS-side permission is
strictly required; `dialog:default` is still granted in `capabilities/default.json` for any
future JS use.

## Window state

Window size/position (and maximized/fullscreen flags) persist across launches via the official
[`tauri-plugin-window-state`](https://docs.rs/tauri-plugin-window-state) — registered as
`tauri_plugin_window_state::Builder::new().build()`. It writes a small `.window-state.json` in
the app config dir, restores on startup, and saves on close automatically. No manual save/restore
code needed.

## cvd serve wiring

On startup the app **spawns `cvd serve --port 7777`** so the bundled fleet dashboard (which
fetches `http://localhost:7777`) works unchanged. The child handle is stored in Tauri managed
state and **killed on app exit** (`RunEvent::Exit`).

**Approach chosen: `std::process::Command`** (not a bundled sidecar) — simplest thing that
compiles and runs with no extra bundling step. Binary resolution order:

1. the repo's build output: `../../target/release/cvd`, then `../../target/debug/cvd`
2. `cvd` on `PATH`

So in dev you only need:

```sh
cargo build -p cvd            # from the repo root → target/debug/cvd
# or: cargo build -p cvd --release
```

If `cvd` can't be found, the app still runs; only the live fleet dashboard is unavailable
(a clear note is logged to stderr). You can also start/stop the sidecar at runtime from
**View → Toggle cvd serve**. The child is always killed on app exit (`RunEvent::Exit`).

> **Optional: make it a real sidecar for a self-contained bundle.** Build `cvd`
> (`cargo build -p cvd --release`), copy it to `app/src-tauri/binaries/cvd-<target-triple>`
> (e.g. `cvd-aarch64-apple-darwin`), add `"externalBin": ["binaries/cvd"]` under `bundle` in
> `tauri.conf.json`, add the `shell:allow-execute`/sidecar permission to `capabilities/default.json`,
> and spawn it via `tauri_plugin_shell`'s sidecar API instead of `std::process::Command`. The
> `std::process` path above is intentionally the default because it needs no bundle prep.

## LLM providers

`distill` and `generate` pick a provider from the desktop process environment:

- `OPENROUTER_API_KEY` — preferred when set
- `ANTHROPIC_API_KEY`
- `LMSTUDIO_API_BASE` — **free, offline, no key.** Set to `local` (→ `http://localhost:1234/v1`)
  or a full base URL of any OpenAI-compatible local server (LM Studio / Ollama / vLLM).

Example, free local looming:

```sh
LMSTUDIO_API_BASE=local cargo tauri dev
```

## Build & run

From `app/src-tauri/`:

```sh
# Compile-check the Rust (fast; no bundling, no system bundler deps):
cargo build

# Run the app in dev (opens the window, loads web/, launches cvd serve):
cargo tauri dev

# Produce a distributable bundle (.app / .dmg on macOS, etc.):
cargo tauri build
```

Requirements:
- **Rust** (stable or nightly) + `tauri-cli` v2 (`cargo install tauri-cli --version '^2'`).
- A **WebKit-based webview**: macOS uses the system WebKit (already present); Linux needs
  `libwebkit2gtk-4.1-dev` + `libgtk-3-dev` etc.; Windows uses WebView2.
- For the live fleet dashboard: a built `cvd` (see [cvd serve wiring](#cvd-serve-wiring)).

## Verified in this environment

- `cargo build` **and** `cargo build --release` in `app/src-tauri` **compile cleanly** — Tauri v2
  (with the `devtools` feature) + system WebKit on macOS; the dialog, window-state, and shell
  plugins; the `zip` crate; and cv-core / cv-llm / cv-search via relative paths. No warnings from
  `clustervision-app` itself.
- The app is **excluded from the main repo workspace** (`cargo metadata` at the repo root does
  not list `clustervision-app`).

A full `cargo tauri dev` / `cargo tauri build` bundle (.app/.dmg) needs a display and (for
distribution) codesigning, so it was not produced here — run it on your machine with `tauri-cli`
installed (`cargo install tauri-cli --version '^2'`). Getting it to **compile** with the menu,
dialog, native ingest, and window-state is what's verified above.
