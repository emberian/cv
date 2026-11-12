# The task substrate

Tasks let a fleet commit to work, and let a human see what was said about it, by whom, when. 🔮

A **task** is a dispatch object: open, claim, note, finish. A **decision** is a task with a posed
question: one event poses it (with the default that stands if nobody speaks), one answers it.

Everything lives in one append-only log, `$CLUSTERVISION_HOME/tasks/events.jsonl`, written under a
flock. Every state you see is folded from those events; there is no second store. Two
front-ends render the same shapes: `cv task` (this page) and
[`cv serve`'s task routes](serving.md#task-endpoints), whose `/inbox` page is the human's door.

## The three laws

1. **cv never claims to have observed what it has not.** `cv task done --observed <evidence>`
   records the agent's word, labeled `self-reported`. What git and the filesystem say is a
   [`sweep`](#the-read-surfaces) candidate, never a close.
2. **No authority machinery.** Identity is `--from` / `CV_ENDPOINT`: recorded, never
   authenticated.
3. **Small.** A few thousand lines.

The land facet (reviewed revisions, tokens, receipts) was removed in 0.15.0: `propose`, `pass`,
`refute`, `reroute`, `verify`, `debt`, `stats`, `--token`/`CV_TOKEN` and `--check-*` exit with a
pointer. Old logs still replay; those events change no state.

## The base lifecycle

```text
open ──claim──► claimed ──done──► done
  ▲                │
  └───release──────┘         (abandon / supersede: terminal from open or claimed)
```

- **`open`** `<title>` takes `--repo` (filtered by `list --repo`, read by `sweep`),
  `--assignee`, `--issue` and `--body`.
- **`claim` / `release`**: first writer wins. Each append is validated against the log under the
  lock, so of N racing claims exactly one lands and the rest are refused.
- **`note`** never changes state and is accepted on a terminal task too: it is marked
  `post_close: true` and rendered "(after close)" by `show`, the inbox and the page. Tags are
  refused on a terminal task.
- **`done`** takes optional `--observed <url-or-path>`; `show` prints
  `done: self-reported 4m ago (tests green)`. `--note TEXT` / `--note-file F` (also on `abandon`)
  writes the note and the terminal event as one unit: both land or neither. Refused on a
  decision (use `resolve`).
- **`status`** `<id> --file F` (`-` for stdin, or inline text) pins the task's **STATUS**, the
  relay hand-off (tip, what is merged, what runs, what is next). Each `status_set` replaces the
  pinned text; `revisions` counts them. `show` prints it first; `show --brief` prints only title
  and STATUS; `show --status` prints the text (exit 1 if none). Only the assignee or opener may
  set it, and not on a terminal task.
- **`abandon` / `supersede`**: the terminals.

Task ids are UUID v7; a unique prefix is enough. Lists print ids at the shortest distinct length,
never below eight hex digits.

### Decisions, and provisional resolutions

`cv task decide "<title>" --for <who> --default <option> [--option <alt>]… [--by <when>]` writes
`opened` and `posed` as one unit. The decider answers with
`cv task resolve <id> --accept-default | --choice "<option>"`, which needs an identity and is
terminal (`resolved`). A second `posed` is refused.

A `decision` tag on a task with nothing posed is refused (CLI and page), naming
`cv task decide` and its `--default`. A tag-only decision already in a log reads as an assigned
action. `cv task split` is gone: pose the question.

**Provisional resolutions** let an orchestrator proceed on the default: `cv task decide …
--provisional` (or `cv task resolve <id> --accept-default --provisional`) resolves it on its
default, by the poser. The task is `resolved`, `resolution.provisional` is `true`, and the
decider's inbox lists it under **made for you (veto?)**, not as owed. The decider answers with
`--confirm` or `--choice "<other>"` (or the page's buttons). That non-provisional resolve by the
decision's assignee (`web:<who>` counts as `<who>`) is the one state-bearing event a terminal task
accepts; the provisional resolution is kept as `decision.superseded_provisional`. A third
party's override, a provisional non-default choice and a second non-provisional resolve are
refused.

### Bodies, tags and relations

- **`--body-file <path>`** (`-` for stdin) reads the body; `note --file` does the same for notes.
- **`--tags a,b`** labels a task; `list --tag <t>` filters; `cv task tag <id> a,b` adds (never
  removes). `decision` is reserved (above).
- **`--blocked-by <id>`** records that this task waits on another; **`--blocks <id>`** records it
  on the other task; `cv task block <id> --by <blocker>` adds one later. A typo refuses the whole
  open. A task is blocked while any blocker is non-terminal, computed when you look. `list` marks
  blocked rows `⊘`; `show` prints `blocked by:` (with states) and `blocks:`.
- `open --tags --blocked-by --blocks` lands as one unit.
- **`list --tsv`**: full id, state, assignee, repo basename, age, title, blocked_by.

`tags` and `blocked_by` are omitted from the wire when empty.

## The log is loud, never silent

A line the reducer refuses is quarantined with a warning naming the line, and every surface
shows the warnings. Appends fail closed on a degraded log. Retired land-facet events
(`revision_proposed`, `review_passed`, `landed`, …) are kept and printed by `show --events`, but
change no state.

## The read surfaces

**`cv task list`**: non-terminal tasks touched in the last 14 days or involving you, oldest
first, with an age column; the last line counts what was hidden, and `--all` lifts both filters.
An unknown `--state` is an error naming the vocabulary:

```text
Error: unknown state "redy" (expected one of open|claimed|done|abandoned|superseded|resolved)
```

**`cv task inbox [who]`** (bare: `$CV_ENDPOINT`): what needs you, grouped by reason, stalest first
within a group, in this order:

| Reason | You appear because | Ages since |
| --- | --- | --- |
| `DecisionOwed` | a live posed decision is assigned to you | the **pose** |
| `Discussing` | you parked a decision for discussion — open, not owed | the last event |
| `Provisional` | a decision assigned to you was resolved *for* you, provisionally — resolved, not owed | the **provisional resolve** |
| `AssignedOpen` | an open task is assigned to you, unclaimed | the last event |
| `ClaimedByYou` | you claimed it; it's yours to finish | the last event |

Rows older than 24 hours lead with `⏰`.

**`cv task sweep --repo <path>`** lists non-terminal tasks (in that repo or none) whose `--issue`
path no longer exists (tried against the task's repo, the swept repo, its parent and the cwd), or
whose title or body names a branch merged into main (`git branch --merged`, local and
remote-tracking; a branch name needs a separator, a digit or eight characters). It prints each as
*probably done* with the reason and never closes one.

All take `--json`: the same rows the HTTP API serves.

## Identity: `CV_ENDPOINT`

The spawner sets `CV_ENDPOINT=agent:<name>` in each agent's environment; every front-end takes
its default actor from it. Nothing verifies it (law 2).

- **Chat-grade verbs** (`open`, `note`, `done`, `abandon`, `supersede`) default to `cv` on the CLI.
- **Identity-bearing verbs** (`claim`, `release`, `status`, `resolve`) refuse without one:

```text
$ cv task claim 019f6e4c
Error: set CV_ENDPOINT or pass --from; identity-bearing events must record who acted
```

## A worked example

```text
$ export CV_ENDPOINT=orchestrator:o
$ cv task open "switch task ids to uuid v7" --repo ~/code/demo --assignee lane:mira
✦ opened 019f6e4b → open
019f6e4b-74c5-74f0-a493-1f8131ca38d5

$ CV_ENDPOINT=lane:mira cv task claim 019f6e4b
✦ claimed 019f6e4b → claimed
$ CV_ENDPOINT=lane:mira cv task note 019f6e4b "ids are v7 on task/uuid-ids; tests green"
✦ noted 019f6e4b → claimed
$ CV_ENDPOINT=lane:mira cv task done 019f6e4b --observed "task/uuid-ids @ c889a02, cargo test green"
✦ done 019f6e4b → done
```

```text
$ cv task show 019f6e4b
task 019f6e4b-74c5-74f0-a493-1f8131ca38d5  [done]  action
  done:     self-reported 2m ago (task/uuid-ids @ c889a02, cargo test green)
  title:    switch task ids to uuid v7
  repo:     /Users/you/code/demo
  channel:  #tasks
  assignee: lane:mira
  opened:   2026-07-17 00:17 by orchestrator:o
  note (lane:mira, 2026-07-17 00:31): ids are v7 on task/uuid-ids; tests green
```

`cv task show <id> --events` (`GET /api/task/{id}/events`) prints the raw history: here `opened`,
`claimed`, `noted`, `done`. An unknown `cv task events --kind` exits 2 listing the known kinds;
`/api/events?kind=` returns 400.

## Growth & retention

`tasks/events.jsonl` is append-only and unbounded. The prompt hook, mutations, `cv task list` and
`GET /api/tasks` resume from a stored fold and parse only events appended since; `events`, `show`
and the inbox still replay the whole log. There is no compaction yet; the log is one file you can archive by hand.
