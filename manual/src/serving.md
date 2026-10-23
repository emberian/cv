# The server and the archive: `cv serve`, `cv watch`, `cv archive`

These commands collect per-machine, per-harness session files into one durable **archive**, and
serve live state as JSON for [the app](viewing-and-sharing.md)'s dashboard and
[the task inbox](tasks.md). (They were the `cvd` binary until 0.15.0.)

- **`cv archive sync`**: snapshot every discovered session into the archive, then exit.
- **`cv watch`**: follow live activity, archiving sessions as they change.
- **`cv serve`**: one local HTTP server for the read API, the task inbox and optionally the dashboard.
- `cv archive ls` lists the archive; `cv archive path` prints its location.

## The archive

The **archive home** is `--home <DIR>` (on `cv archive …` and `cv watch`), else
`$CLUSTERVISION_HOME`, else `~/.clustervision`:

```
~/.clustervision/
├── catalog.jsonl                 # cheap metadata, one row per (harness, id)
└── archive/
    └── <harness>/<id>.json       # the full parsed session, harness-agnostic IR
```

`catalog.jsonl` is append-only, deduped by `(harness, id)` (newest `archived_at` wins) and
self-compacting; unchanged bytes are never rewritten. Several machines can share one archive home
(a synced folder or network share).

```sh
cv archive path   # print the archive location
cv archive ls     # list archived sessions, newest activity first
```

`cv archive ls` counts turns like `cv ls` (`catalog.jsonl` keeps the raw count as
`messages_written`; 0.17 rows show a dash until resynced). Unreadable resume state is said once on
stderr and the session reread.

## `cv archive sync` — snapshot everything once

```sh
cv archive sync
```

```text
sync complete: 42 archived, 318 skipped (unchanged) of 360 discovered
archive: /Users/you/.clustervision
```

Reruns are cheap (`skipped` = unchanged). Per-session failures go to stderr without aborting.
Run it from cron or launchd for backups without a resident process.

## `cv watch` — follow live + archive as it happens

```sh
cv watch                           # follow everything
cv watch --interval 5              # poll every 5s (default 3)
cv watch --harness claude          # only one harness
cv watch --cwd clustervision       # only sessions whose cwd contains this substring
```

`cv watch` archives everything on its first poll (no `sync` needed), then each session as it
changes. Its ready line reaches stderr within a second. `cv scry` prints the same feed without
archiving.

`--interval` is seconds, fractional allowed (minimum 0.25), as on `cv scry`; negative exits 2.
Watch cursors are version 2: a 0.17 cursor starts a fresh baseline. A byte mark on a compressed
Codex rollout stays unresolved.

```text
cv watch: watching (interval 3s) -> /Users/you/.clustervision
[new] claude   a1b2c3d4  +12 msgs  ~/code/clustervision
[archived] codex    9f8e7d6c  +3 msgs  ~/code/other-project
```

## `cv serve` — the local HTTP server

```sh
cv serve                       # http://127.0.0.1:7777 — the inbox at /, the API at /api/*
cv serve --web ./web           # also host the dashboard at / (the inbox moves to /inbox)
cv serve --who ember --open    # whose inbox the page opens on; open it in a browser
cv serve --port 8080           # pick a port
cv serve --token s3cret        # require `Authorization: Bearer s3cret` on /api/* ($CV_SERVE_TOKEN works too)
cv serve --host 0.0.0.0 --token s3cret   # expose beyond loopback (demands a token or --insecure-expose)
```

Binds `127.0.0.1:7777` and runs until killed; the desktop app calls the same API in-process.

`--port 0` binds a free port; the banner, on stderr within a second and before any request, names
the real one:

```text
cv serve on http://127.0.0.1:54573 — inbox at /, API at /api/* (no --web)
```

- **It reads live sessions, not the archive**; archived snapshots never appear in the API or `cv index`.
- **Transcripts can hold secrets; access is local by default.**
  - **CORS is an allow-list, never `*`:** only `http://localhost:*`, `http://127.0.0.1:*`,
    `[::1]`, your `--host` and the desktop app's Tauri origins get `Access-Control-Allow-Origin`.
    `OPTIONS` preflight answers `204`.
  - **`Host` must name this machine** (`127.0.0.1`, `localhost`, `[::1]` or `--host`), else
    `403`; this defeats DNS rebinding.
  - **Bearer token:** with `--token <t>` (or `$CV_SERVE_TOKEN`), `/api/*` without
    `Authorization: Bearer <t>` is a `401`.
  - **Writes:** `POST /api/task/<id>/<verb>` must be `application/json` with an allowed `Origin`
    (if any), else `403` before the store is touched.
  - **Non-loopback binds need a token or `--insecure-expose`**, and warn either way.

  Only `GET` is served, plus `POST` on the inbox write routes; anything else is `405`. Responses
  are JSON except the event feed and export downloads.

### Routes

Unknown paths are `404`; a bad `harness` is `400`. Rows are what `cv <cmd> --json` prints;
`cv schema --json` lists every route under `routes`.

#### `GET /api/health`

Liveness and the harnesses this build knows: `{"ok": true, "harnesses": ["claude", "codex", …]}`.

#### `GET /api/sessions`

Session rows, newest activity first. Optional `harness`, `cwd` (substring), `limit` (after
sorting). `message_count` is `null` until counted.

```sh
curl 'http://127.0.0.1:7777/api/sessions?harness=claude&limit=1'
```

```json
[
  {
    "id": "a1b2c3d4-5e6f-7890-abcd-ef0123456789",
    "harness": "claude",
    "path": "/Users/you/.claude/projects/-Users-you-code-clustervision/a1b2c3d4-5e6f-7890-abcd-ef0123456789.jsonl",
    "size_bytes": 412998,
    "cwd": "/Users/you/code/clustervision",
    "title": "wire up the fleet dashboard",
    "updated_at": "2026-05-30T18:42:10+00:00",
    "created_at": "2026-05-30T17:05:01+00:00",
    "message_count": 87
  }
]
```

#### `GET /api/search`

The hits `cv search --json` emits, as an array (session row plus `score`, `snippet` and the
sub-agent provenance trio); `X-Cv-Search-Total` is its `total`, every session that matched. Params: `q` (required, non-empty), `limit`, `harness`, `cwd`, `kind`
(`exchange`, `context`, `all`), `live_bytes`, `semantic=1`, `scan=1`. Without an index it answers
from a live read of every session (`live_bytes` bounds it, else the server's `$CV_LIVE_BYTES`
or `[search] live_bytes`); an unusable index is a `503` naming
the fix. `scan=1` runs `cv search --scan`'s live scan. `kind` with `semantic=1` or
`scan=1` is a `400`. `X-Cv-Search-Source` is `index`, `semantic` or `live`; `X-Cv-Search-Note`
says when the answer may be partial, and `N of M matches` when `limit` cut it.

#### `GET /api/stats`

`cv stats --json` (`sessions, messages, uncounted, by_harness, top_cwds, earliest_created,
latest_updated`). Optional `q=<query calculus>`: a parse error is a `400`; a `text:` term with no
full-text index is a `503` naming `cv index`.

#### `GET /api/touched`

`cv touched --json`: sessions whose tool events touched a file, newest first. `path` (required;
exact or suffix), `edits_only=1`. Empty on a cold catalog.

#### `GET /api/session/{harness}/{id}`

The full parsed session as IR JSON, or `404`.

#### `GET /api/session/{harness}/{id}/head`

The session row plus `model`, `git`, `system_prompt`, `lineage` and exact message `total`, in
one pass that keeps no messages.

#### `GET /api/session/{harness}/{id}/messages`

Messages `[start, end)` as IR JSON, like `cv show --range`. A malformed `start` or `end` is a
`400`; `extra=1` keeps each message's `extra` map. Messages before `start` are skipped unread.
The reply has the actual indices, `total_known` (+ `total` at EOF) and `has_more`.

#### `GET /api/session/{harness}/{id}/events`

`cv events --json` rows in transcript order (`ts` RFC 3339, plus the provenance trio); optional
`kind=`. A stale or missing catalog row is re-ingested first.

#### `GET /api/session/{harness}/{id}/compactions`

`cv show --kind compaction_boundary --json` rows (`boundary_msg_idx, summary_msg_idx, trigger,
pre_tokens, duration_ms, summary, pre_compaction_span`) plus a `headline`. `pre_compaction_span`
uses `/messages` numbering.

#### `GET /api/session/{harness}/{id}/subagents`

The spawned forest (`Agent`/`Task` agents and workflow agents): session rows plus `agent_id,
agent_type, description, tool_use_id, workflow, result_status, result_summary`; `[]` if none.

#### `GET /api/session/{harness}/{parent}/subagent/{agent}`

One sub-agent's full transcript, found through its parent; `404` if either is missing.

#### `GET /api/session/{harness}/{id}/workflow/{wf}/script`

A `Workflow` run's driving script, `{ workflow, name, source }` (from
`<session>/workflows/scripts/<slug>-<wf>.js`); `404` if none.

#### Board endpoints

`GET /api/board/{channel}`, `/api/board/{channel}/unanswered`, `/api/claims/{channel}`,
`/api/who/{channel}` and `/api/channels` answer `410` with a pointer to the task routes (removed
in 0.15.0).

#### Task endpoints

[Task](tasks.md) rows as in `cv task --json` and the MCP `task_*` tools. Every reply carries
`warnings`: the task log's replay warnings.

- **`GET /api/tasks`**: `{ "tasks": [...], "warnings": [] }`, oldest first; rows carry `id`,
  `title`, `effective_state`, `assignee`, `repo`, `opened_at`, `last_ts`. Optional `state`,
  `assignee`, `repo`, `all=1` (include terminal); an unknown `state` is a `400`.

- **`GET /api/task/{id}`** (prefix ok, else `404`): `{ "task": {...}, "effective_state",
  "warnings" }`, the full read model (state, assignee, notes, tags, blockers, decision, pinned
  STATUS). `?events=1` adds the history.
- **`GET /api/task/{id}/events`**: `{ "events": [...], "warnings": [] }`, each with `id`,
  `task_id`, `ts`, `by` and a tagged `event` (`opened`, `claimed`, `noted`, `posed`, `resolved`,
  …; pre-0.15.0 kinds read unchanged).
- **`GET /api/tasks/inbox/{who}`**: `{ "inbox": [ … ], "warnings": [] }`, stalest first, over the
  whole store (no 14-day window). Each row is the inbox row every surface serves: `id`, `short`,
  `title`, `kind`, `reason`, `effective_state`, `since`, `age`, `stale`, `assignee`, `opened_by`,
  `opened_at`, `body`, `blocked`, `last_by`, `last_ts`, `unread`, `notes`, and when present `tags`,
  `repo`, `issue`, `decision` and `status`. `reason` is `decision_owed`, `discussing`,
  `provisional`, `assigned_open` or `claimed_by_you`.
- `GET /api/tasks/debt` and `/api/tasks/stats` answer `410` (removed in 0.15.0).

#### Inbox endpoints

Used by the `/inbox` page. Writes record the CLI's event, stamped `web:<who>` (`who` from the
body, else `?who=`, else `--who`).

- **`GET /api/inbox?who=`**: the page model (inbox items, counts, closed); `all=1` lifts the
  14-day window, `since=` narrows it.
- **`GET /api/events?since=&kind=&by=&not_by=&assignee=&task=`**: JSON lines, as `cv task
  events`; an unknown `kind` is a `400` naming the known kinds.
- **`GET /api/lanes?session=<id>`**: `cv lanes --tasks`.
- **`GET /api/tasks?detail=1&repo=&state=&tag=&all=1`**: every task with backlog columns (owner,
  blockers, age), newest first. `GET /api/backlog` answers `410` with this pointer.
- **`POST /api/task/{id}/{resolve,done,note,discuss,claim,release}`**: `{choice}`,
  `{accept_default: true}` or `{confirm: true}` for `resolve` (optional `note`); `{observed}` for
  `done`; `{text}` for `note`/`discuss`.

## Putting it together

```sh
cv watch &                     # archive every session as it changes, forever
cv serve --web ./web           # the dashboard, the inbox and the API on :7777
```

Then browse in [the app](viewing-and-sharing.md), tail with `cv scry` ([the CLI](cli.md)), and
coordinate through [tasks](tasks.md).
