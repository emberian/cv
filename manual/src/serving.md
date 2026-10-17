# The server and the archive: `cv serve`, `cv watch`, `cv archive`

Where most of [`cv`](cli.md) is your interactive map of *this* machine, these commands
turn a pile of per-machine, per-harness session files into
a single durable **archive**, and serve the live state as JSON so [the app](viewing-and-sharing.md)'s dashboard and
[the task inbox](tasks.md) can render it. 🔮 (They were the `cvd` binary until 0.15.0; its 0.15 shim
was removed in 0.16.0.)

- **`cv archive sync`** — one-shot: snapshot every discovered session into the archive, then exit.
- **`cv watch`** — long-running: follow live activity and archive sessions as they change.
- **`cv serve`** — one local HTTP server: the read API, the task inbox, and optionally the dashboard.

Plus two small utilities: `cv archive ls` (list what's in the archive) and `cv archive path` (print
the archive location).

## The archive

Everything the archive holds lives under one **archive home**, resolved in this order:

1. the `--home <DIR>` flag (on `cv archive …` and `cv watch`),
2. the `$CLUSTERVISION_HOME` environment variable,
3. otherwise `~/.clustervision`.

Inside it:

```
~/.clustervision/
├── catalog.jsonl                 # cheap metadata, one row per (harness, id)
└── archive/
    └── <harness>/<id>.json       # the full parsed session, harness-agnostic IR
```

`catalog.jsonl` is an append-only JSONL index — lightweight rows for archive listings — while the
full transcript for each session lives in `archive/<harness>/<id>.json` as clustervision's unified
representation. Storing is **idempotent**: if a session's serialized bytes are byte-identical to
what's already on disk, the write is skipped entirely. The catalog dedupes by `(harness, id)` with
the newest `archived_at` winning, and self-compacts once it's carrying too many superseded lines.

Point several machines at the same archive home (a synced folder, a network share) and you get one
centralized, harness-agnostic store of every agent session you've ever run.

```sh
cv archive path   # print the archive location
cv archive ls     # list archived sessions, newest activity first
```

## `cv archive sync` — snapshot everything once

```sh
cv archive sync
```

`sync` discovers every session across every supported harness, parses each into the unified
representation, and stores it. It's a one-shot: it archives, prints a summary, and exits.

```text
sync complete: 42 archived, 318 skipped (unchanged) of 360 discovered
archive: /Users/you/.clustervision
```

`skipped` means "already archived and unchanged" (the idempotent fast path), so re-running `sync`
is cheap. Parse/store failures are logged to stderr per-session and don't abort the run. This is the
command to put on a cron / launchd timer if you just want periodic backups without a resident
process.

## `cv watch` — follow live + archive as it happens

```sh
cv watch                           # follow everything
cv watch --interval 5              # poll every 5s (default 3)
cv watch --harness claude          # only one harness
cv watch --cwd clustervision       # only sessions whose cwd contains this substring
```

`cv watch` is the resident counterpart to `sync` (`cv scry` is the same live feed printed, without the archive). On its first poll it archives whatever already
exists (so it's self-sufficient — no need to `sync` first), then it follows live activity and
re-archives each session as it changes. It runs until you kill it, and like every daemon prints
its ready line (the line below) to stderr within a second, before its first poll.

Every time it archives a changed session it logs the delta to stderr:

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

`serve` is a tiny synchronous HTTP server (a small worker pool, no async stack). It answers the
read API [the app](viewing-and-sharing.md)'s dashboard polls (and the desktop app calls in-process), serves the
[task inbox](tasks.md) page with the routes its buttons use, and with `--web` hosts the dashboard. It binds
`127.0.0.1:7777` by default and runs until killed.

`--port 0` binds an **ephemeral port**, and the startup banner — printed to stderr within a second
of starting, before any request is served — always carries the *real* bound port, so a script or
test harness can parse the address instead of racing to guess a free one:

```text
cv serve on http://127.0.0.1:54573 — inbox at /, API at /api/* (no --web)
```

Two things worth noting up front:

- **It reads *live* sessions, not the archive.** `serve`'s session endpoints call into the same
  discovery/parse engine `cv` uses, so they reflect the current on-disk state of each harness in
  real time. `sync`/`watch` build the archive for durability; `archive ls` lists its snapshots.
  `cv index` and the session API do not automatically include archived snapshots.
- **API responses use JSON, except the event feed and export downloads.** Access defaults to local callers. Your transcripts can contain
  secrets, so `serve` treats them that way:
  - **CORS is an allow-list, never `*`.** Only local origins (`http://localhost:*`,
    `http://127.0.0.1:*`, `[::1]`, your configured `--host`) and the desktop app's Tauri origins get
    an `Access-Control-Allow-Origin` echo; a random website you happen to have open gets nothing,
    so its scripts can't read the corpus. `OPTIONS` preflight is answered with a `204`.
  - **The `Host` header must name this machine** (`127.0.0.1`, `localhost`, `[::1]`, or your
    `--host`) — anything else is a `403`. This defeats DNS rebinding, where a hostile page points
    its own domain at `127.0.0.1` to sidestep CORS entirely.
  - **Optional bearer token.** Set `--token <t>` (or `$CV_SERVE_TOKEN`) and every `/api/*` request
    must carry `Authorization: Bearer <t>` or get a `401`.
  - **Writes are JSON from a local origin.** The inbox's `POST /api/task/<id>/<verb>` must be
    `application/json` and, when it carries an `Origin`, an allowed one; anything else is a `403`
    before the store is touched. (A `text/plain` POST needs no CORS preflight, so the allow-list
    alone would only hide the response after the event was written.)
  - **Non-loopback binds demand an opt-in.** `--host 0.0.0.0` (or any non-loopback address) is
    refused unless you set a token or pass `--insecure-expose`, and warns loudly either way —
    anyone who can reach the port can read every transcript.

  Only `GET` is supported, plus `POST` on the inbox's write routes; anything else is a `405`.

### Routes

The read routes below are `GET` requests under `/api`; the inbox also has the `POST` routes
described above and in [the task guide](tasks.md). Unknown paths return `404`; a bad `harness` name returns
`400`. The routes are `cv_core::api::route_json` (plus `/api/search` and `/api/stats`); its rows are the
functions `cv <cmd> --json` prints with, so the two doors cannot disagree.

#### `GET /api/health`

Liveness plus the set of harnesses this build knows about.

```http
GET /api/health
```

```json
{
  "ok": true,
  "harnesses": ["claude", "codex", "gemini", "..."]
}
```

#### `GET /api/sessions`

Session metadata, filtered and **newest-activity-first** (sorted by `updated_at`, falling back to
`created_at`). Query parameters, all optional:

| param     | meaning                                              |
|-----------|------------------------------------------------------|
| `harness` | only this harness (an unknown name → `400`)          |
| `cwd`     | only sessions whose cwd *contains* this substring    |
| `limit`   | keep at most this many (applied after sorting)       |

```sh
curl 'http://127.0.0.1:7777/api/sessions?harness=claude&limit=2'
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
  },
  {
    "id": "9f8e7d6c-1234-5678-9abc-def012345678",
    "harness": "claude",
    "path": "/Users/you/.claude/projects/-Users-you-code-other-project/9f8e7d6c-1234-5678-9abc-def0123456789.jsonl",
    "size_bytes": 15342,
    "cwd": "/Users/you/code/other-project",
    "title": "debug the retry backoff",
    "updated_at": "2026-05-30T16:20:44+00:00",
    "created_at": "2026-05-30T15:58:12+00:00",
    "message_count": 31
  }
]
```

#### `GET /api/search`

Full-text (or `semantic=1`) search over the whole corpus, as the array of rows `cv search --json`
emits: the session row plus `score`, `snippet` and the sub-agent provenance trio. Params: `q`
(required, non-empty), `limit`, `harness`, `cwd`, `kind` (`exchange`, `context` or `all`, as `cv search --kind`), `semantic=1`, `scan=1`. With no usable index it is
a `503` naming the cause and the fix, as `cv search` errors; `scan=1` asks for the same head-capped
live scan `cv search --scan` runs. Both semantic and live searches cover only the exchange;
`kind=context` or `kind=all` with either mode returns 400. The response header `X-Cv-Search-Source` is `index` | `semantic`
| `live`, with a human sentence in `X-Cv-Search-Note` whenever the answer may be partial (a stale
index, a live scan, a truncation).

#### `GET /api/stats`

Corpus-wide statistics, exactly what `cv stats --json` prints (`sessions, messages, by_harness,
top_cwds, earliest_created, latest_updated`). Optional `q=<query calculus>` (the string `cv stats
-q` takes; a parse error is a `400` quoting the core's message).

#### `GET /api/touched`

Sessions whose tool events touched a file, newest activity first — `cv touched --json`. Params:
`path` (required; exact or suffix match), `edits_only=1`. Empty on a cold catalog.

#### `GET /api/session/{harness}/{id}`

The full parsed session — the complete unified-representation transcript as JSON — or `404` if no
such session is found.

```sh
curl 'http://127.0.0.1:7777/api/session/claude/a1b2c3d4-5e6f-7890-abcd-ef0123456789'
```

#### `GET /api/session/{harness}/{id}/head`

Everything a session knows about *itself* and no messages: the session row plus `model`, `git`,
`system_prompt`, `lineage` and the exact message `total`. One cheap pass that keeps no messages,
so a session-level fact recorded late in the transcript (Claude's `prompt_snapshot`) is reached
without a windowed read having to scan the whole file.

#### `GET /api/session/{harness}/{id}/messages`

The window `[start, end)` of the session's messages as IR JSON — `cv show --range` through the
other door. Params: `start`, `end` (a malformed index is a `400`, never a silent full read),
`extra=1` to keep each message's harness `extra` map. Streams lazily, skipping the messages before
`start` without reading their content, and stops at `end`. The reply carries the actual window indices,
`total_known` (+ `total` once EOF was reached) and `has_more`.

#### `GET /api/session/{harness}/{id}/events`

The session's extracted tool events (file edits/reads, commands, errors) in transcript order, as
`cv events --json` rows (`ts` RFC 3339, plus the sub-agent provenance trio); optional `kind=`. A
stale or missing catalog row is re-ingested on the spot.

#### `GET /api/session/{harness}/{id}/compactions`

Every compaction boundary in transcript order, as `cv show --kind compaction_boundary --json` rows (`boundary_msg_idx,
summary_msg_idx, trigger, pre_tokens, duration_ms, summary, pre_compaction_span`) plus a
`headline`. `summary` is the text that seeded the next window; `pre_compaction_span` is the
message range compacted away, in the numbering `/messages` uses.

#### `GET /api/session/{harness}/{id}/subagents`

The sub-agent forest this session spawned (directly-spawned `Agent`/`Task` agents and the workflow
tier): each a session row plus `agent_id, agent_type, description, tool_use_id, workflow,
result_status, result_summary`. An empty array if there are none.

```sh
curl 'http://127.0.0.1:7777/api/session/claude/a1b2c3d4.../subagents'
```

#### `GET /api/session/{harness}/{parent}/subagent/{agent}`

The full parsed transcript of **one** sub-agent. Sub-agents don't live in the main session pool, so
they're loaded relative to their `{parent}`. `404` if either the parent or the named sub-agent isn't
found.

#### `GET /api/session/{harness}/{id}/workflow/{wf}/script`

The driving script Claude Code recorded for a `Workflow` run, as `{ workflow, name, source }`
(from `<session>/workflows/scripts/<slug>-<wf>.js`). `404` when the run has no script.

#### Board endpoints

`GET /api/board/{channel}`, `/api/board/{channel}/unanswered`, `/api/claims/{channel}`,
`/api/who/{channel}` and `/api/channels` were removed in 0.15.0 with the coordination board;
they answer `410` with a pointer at the task routes below.

#### Task endpoints

These serve the [task substrate](tasks.md) — the same shared row shapes as `cv task --json`
and the MCP `task_*` tools. Every reply carries a `warnings` array: the task log's replay
warnings ride every response, so a corrupted coordination log is visible from any consumer.

- **`GET /api/tasks`** — task rows, oldest first. Optional `?state=`, `?assignee=`, `?repo=`,
  and `?all=1` (include terminal tasks). An unknown `state` is a `400` naming the whole
  vocabulary, never a silently-empty list.

  ```json
  {
    "tasks": [
      {
        "id": "019f6e4b-74c5-74f0-a493-1f8131ca38d5",
        "title": "switch task ids to uuid v7",
        "effective_state": "claimed",
        "assignee": "agent:mira",
        "repo": "/Users/you/code/demo",
        "opened_at": "2026-07-17T04:17:46.694052Z",
        "last_ts": "2026-07-17T04:18:05.206768Z"
      }
    ],
    "warnings": []
  }
  ```

- **`GET /api/task/{id}`** — one task's full projection (id prefix ok; `404` otherwise), as
  `{ "task": {...}, "effective_state": "claimed", "warnings": [] }`. The `task` object is
  the complete read model: state, assignee, notes, tags, blockers, the decision facet, the
  pinned STATUS.
- **`GET /api/task/{id}/events`** — the raw durable history, `{ "events": [...], "warnings": [] }`,
  each event carrying `id`, `task_id`, `ts`, `by`, and its tagged `event` payload
  (`opened`, `claimed`, `noted`, `posed`, `resolved`, …; a pre-0.15.0 log's land-facet kinds
  read through unchanged). `GET /api/task/{id}?events=1` returns the projection with the history
  beside it.
- **`GET /api/tasks/inbox/{who}`** — what needs this endpoint, stalest first:
  `{ "inbox": [ … ], "warnings": [] }`, each row the one inbox row every surface serves (`cv task
  inbox --json`'s `items`, MCP `task_inbox`, the `/inbox` page): `id`, `short`, `title`,
  `kind`, `reason`, `effective_state`, `since` (the aging anchor), `age`, `stale`, `assignee`,
  `opened_by`, `opened_at`, `body`, `blocked`, `last_by`, `last_ts`, `unread`, `notes`, and when
  present `tags`, `repo`, `issue`, `decision` (default, options, alternatives, deadline and its
  phrase, the resolution) and `status`. `reason` is one of `decision_owed`, `discussing`,
  `provisional`, `assigned_open`, `claimed_by_you`; the whole store is in scope (no 14-day window).
- `GET /api/tasks/debt` and `GET /api/tasks/stats` were removed in 0.15.0 with the task land
  facet; they answer `410` with a pointer.

#### Inbox endpoints

The routes the inbox page (`/inbox`) uses — what `cv task serve` served until 0.15.0. Every write
records the same event the CLI would, stamped `web:<who>` (`who` from the body, else `?who=`, else
`--who`).

- **`GET /api/inbox?who=`** — the inbox page model (`cv task inbox --json`'s items plus counts and
  the closed ones); `all=1` lifts the 14-day window, `since=` narrows it.
- **`GET /api/events?since=&kind=&by=&not_by=&assignee=&task=`** — JSON lines, the same query as
  `cv task events`.
- **`GET /api/lanes?session=<id>`** — a session's sub-agent forest joined to the tasks each lane
  holds (`cv lanes --tasks`).
- **`GET /api/tasks?detail=1&repo=&state=&tag=&all=1`** — every task with the backlog columns
  (owner, blockers, age), newest activity first; the inbox page's Backlog pane reads it.
  `GET /api/backlog` is gone (410 with this pointer).
- **`POST /api/task/{id}/{resolve,done,note,discuss,claim,release}`** — `{choice}` |
  `{accept_default: true}` | `{confirm: true}` for `resolve` (with an optional `note`),
  `{observed}` for `done`, `{text}` for `note`/`discuss`.

## Putting it together

A common setup on a workstation that's part of a fleet:

```sh
cv watch &                     # archive every session as it changes, forever
cv serve --web ./web           # the dashboard, the inbox and the API on :7777
```

…with the archive home on a synced folder so every machine contributes to one corpus. From there:

- browse and visualize it all in [the app](viewing-and-sharing.md),
- tail the live feed with `cv scry` (see [the CLI](cli.md)),
- and coordinate running agents through [tasks](tasks.md).
