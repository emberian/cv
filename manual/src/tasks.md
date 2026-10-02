# The task substrate

The [board](board.md) lets a fleet *talk*. Tasks let a fleet **commit to work** — and let a
human see what is actually *true* about that work. 🔮

A **task** is a durable dispatch object: open it, claim it, note progress on it, finish it.
Code tasks additionally carry reviewed **revisions**, and here's the part that makes the whole
substrate worth having: whether a revision has *landed* is **observed by running git**, never
taken on an agent's word. An agent saying "merged, done!" changes nothing; `cv` looking at the
repository does.

Storage is one append-only event log — `$CLUSTERVISION_HOME/tasks/events.jsonl` — with the same
crash-safe flock recipe as the board. Every state you ever see is **replay-derived**: fold the
events through a pure reducer and you get the truth; there is no second store to drift out of
sync. The substrate is exposed three ways, all rendering the same shapes: the `cv task` CLI
(this page), the [MCP `task_*` tools](mcp.md#the-task-substrate-over-mcp), and
[`cvd serve`'s task routes](daemon.md#task-endpoints).

## The four laws

The substrate is built on four laws (quoted from the module doc, which is the canonical text):

1. **Observed, not attested.** No agent claim ever produces `MergedLocal` or `Landed`; only
   the cv-side git verifier emits those events, and agent-facing append paths reject them at
   the store seam.
2. **Independence is read, not asserted.** Reviewer independence (cross-family review) is
   determined by reading the reviewer's transcript harness from cv's catalog — advisory-warn,
   never a gate.
3. **No authority machinery.** No seats, grants, or debts. Landing authority is whoever can
   push to the repo; cv only tracks and verifies.
4. **Small.** This substrate stays a few thousand lines. Complexity requests go to the
   design-notes graveyard, next to the 200k-line system this replaced.

> Law 1 is about the **land facet** — `merged_local`/`landed` on code revisions, which git
> observes. Base-lifecycle `done` is **self-reported unless a completion check is attached** to
> verify it; `--observed` on a check-less non-code task is free text (pinned by
> `adversary_gym.rs::pin_done_completion_is_self_reported`).

## The base lifecycle

Every task has a base lifecycle, whether or not code is involved:

```text
open ──claim──► claimed ──done──► done
  ▲                │
  └───release──────┘         (abandon / supersede: terminal from open or claimed)
```

- **`open`** — anyone can open a task (`cv task open <title>`), optionally with `--repo`
  (required later for revisions), `--assignee`, `--issue`, a `--body`, and a `--channel` for
  board notifications (default `tasks` — every task event posts a one-liner there).
- **`claim` / `release`** — claiming is a durable, race-free **first-writer-wins**: the store
  validates each append against a replay of the log *under the lock*, so when N agents race,
  exactly one claim lands and the rest get a rejection, not a duplicate.
- **`note`** — progress notes; never change state. **A note is accepted on a terminal task too**:
  terminal is about *state*, not the record. A note appended after `done` / `resolved` /
  `abandoned` / `superseded` is marked `post_close: true` and every surface says so — `show`,
  the inbox Markdown and the web page render it "(after close)" — and the task's effective state
  does not move (pinned by `reduce.rs::post_close_note_appends_and_never_changes_effective_state`).
  Four lanes and the orchestrator lost or re-homed evidence before this, because they wrote
  `done` first and the note second. Tags stay refused on a terminal task: they change views.
- **`done`** — completes a task, optionally pointing at observable evidence
  (`--observed <url-or-path>`). `--note TEXT` / `--note-file F` (and the same on `abandon`)
  appends the note and the terminal event as **one unit** — validated together under the lock
  and written with one `write`, the note first: both land or neither does. **Refused while a code revision is live** — you can always
  *kill* a task, but you can never silently complete one that has unlanded reviewed code. On a
  non-code task `done` is **self-reported unless a completion check is attached** to verify it —
  `--observed` alone is free text (law 1 covers landing, not completion; see the note above).
- **`status`** — `cv task status <id> --file F` (or `-` for stdin, or the text inline) pins the
  task's **STATUS**: one `status_set` event that *replaces* the pinned status in the projection
  (every earlier one stays in the events; `revisions` counts them). It is the relay hand-off
  field — a generation's tip, what is merged, what runs (pids and paths), what is next — so a
  successor reads one field, not the newest of a hundred notes. `show` prints it first, under
  `STATUS`; `show --brief` on a task with a STATUS prints only the title and the STATUS; `show
  --status` prints just the text (exit 1 when none is pinned). Unlike a note it is
  state-bearing: allowed on an open or claimed task, **by its assignee or its opener** (an
  identity-bearing verb), and refused on a terminal task.
- **`abandon` / `supersede`** — the terminals. Abandon is always available on a non-terminal
  task, live revision or not.

Task ids are time-sortable UUIDs (v7) and, as everywhere in clustervision, a **unique prefix
is enough** on the command line. Because v7 ids opened within the same second share their first
eight hex digits, every list renders ids at the shortest length that keeps them distinct (never
below eight) — a batch of 23 tasks opened by one orchestrator used to print as 23 copies of
`01a0f52e`, none of which `show` would accept.

### Decisions, and provisional resolutions

A **decision** is a task with a posed question: `cv task decide "<title>" --for <who> --default
<option> [--option <alt>]… [--by <when>]` appends `opened` + `tagged decision` + `posed`, and the
decider answers with `cv task resolve <id> --accept-default | --choice "<option>"` — identity-
bearing, terminal (`resolved`). `done` is refused on a decision; a second `posed` is refused.

**There is one way to pose a decision.** `cv task open … --tags decision` and `cv task tag <id>
decision` on a task with nothing posed are **refused**, with the message naming `cv task decide`
and its required `--default`: a tag-only decision gave the decider an inbox card with no options
and no buttons (two of them, 10-01). Stores that already hold tag-only decisions keep showing them
under "decisions owed" — the page offers "my last note is the answer — close it" — and nothing is
migrated.

**Provisional resolutions** are how an orchestrator proceeds on a default without leaving the
decision to rot as owed: `cv task decide … --provisional` (or `cv task resolve <id>
--accept-default --provisional` on an existing one) resolves it **on its default, by the poser**,
with a veto window. The task is `resolved` (work proceeds), `resolution.provisional` is `true`,
and the decider's inbox lists it in its own group, **made for you (veto?)** — not counted as owed.
The decider answers with `cv task resolve <id> --confirm` (keep it) or `--choice "<other>"` (veto),
from the page's "confirm" or alternative buttons too. That answer is **the one state-bearing event
a terminal task accepts**, and the rule is narrow: a *non-provisional* `resolved`, *by the
decision's assignee* (`web:<who>` counts as `<who>`), *over a provisional resolution*. It
replaces the resolution; the provisional one is kept as `decision.superseded_provisional` (the
last non-provisional resolution wins). A third party's override, a provisional choice other than
the default, and a second non-provisional resolve are all refused
(`reduce.rs::provisional_resolution_is_overridden_only_by_the_decider`).

### Bodies, tags and relations

- **`--body-file <path>`** (`-` for stdin) reads the body from a file; `note --file` does the same
  for notes. A brief is a document, not a 500-character shell string.
- **`--tags a,b`** labels a task; `list --tag <t>` filters on a label, and `cv task tag <id> a,b`
  adds labels later (additive, never removes). One label is reserved: **`decision`** is written by
  `cv task decide` and refused on a task with nothing posed (see *Decisions* above).
- **`--blocked-by <id>`** records that this task waits on another; **`--blocks <id>`** records the
  same relation on the *other* task (`cv task block <id> --by <blocker>` after the fact). Both are
  resolved before the open is written, so a typo refuses the command rather than opening a task
  with half its relations. Whether a task is blocked *now* is computed when you look — any
  blocker still in a non-terminal state — so a blocker finishing, or being abandoned, unblocks
  without another event. `list` marks blocked rows with `⊘`; `show` prints `blocked by:` (with
  each blocker's state) and `blocks:` (the reverse).
- **`list --tsv`** prints one tab-separated row per task — full id, state, assignee, repo
  basename, age, title, blocked_by — for `cut`/`awk`/`sort`; **`list --wide`** adds a second line
  per task with its tags, repo, blockers and the body's first line.

Tags and relations are events like everything else (`tagged`, `blocked_by`), folded by the
reducer into `tags` / `blocked_by` on the projection; both are omitted from the wire when empty,
so a log that never used them serializes exactly as before.

## The land facet: revisions

A code task carries revisions — reviewed snapshots of a branch. The grammar:

```text
propose ──► awaiting_review ──pass──► ready ──(verifier observes)──► merged_local ──► landed
                 │                      │                                              ▲
               refute                 refute                                           │
                 ▼                      ▼                    ready ────(forge PR land)─┘
              refuted (terminal)     refuted (terminal)
```

- **`cv task propose <id> --branch <b>`** attaches revision *n*. Proposing again supersedes
  the live prior revision — that is the **only cure for a refute**: a REFUTE is terminal for
  its revision, and a later pass on it fails closed. New content means a new revision, full
  stop.
- **Who may judge:** only the **active reviewer**'s verdict counts. The reviewer is bound at
  propose time (`--reviewer agent:rex`), or — if none was named — by the *first* verdict.
  `cv task reroute <id> --to <who>` is the only reassignment.
- **`pass`** moves the revision to `ready`; **`refute`** ends it (from `awaiting_review` *or*
  `ready` — a reviewer can retract a pass by refuting, but never the reverse).
- **`merged_local` is NOT `landed`.** A merge observed on the local `main`/`master` is
  actionable state, not a terminal: the reviewed patch still isn't on the upstream ref
  (`origin/main` by default). Only when the verifier observes the reviewed *content* on the
  upstream does the revision become `landed` — and `ready → landed` directly is legal too,
  because a branch routinely lands via a forge PR with no local-integration step.
- The five verifier-only events (`source_unavailable`, `merge_failed`, `merged_local`,
  `reconcile_failed`, `landed`) **cannot be appended by agents at all** — the CLI verbs and
  every MCP tool go through a store path that rejects them (law 1). Trying earns you:

```text
event kind 'landed' is verifier-only: landing state is observed by `cv task verify`, never asserted
```

Displays show the *effective* state — the base state unless a revision is live (or landed), in
which case the revision layer is the truth that matters, rendered with a `rev:` prefix
(`rev:ready`, `rev:landed`). Filters use the bare names.

## Propose: revision identity is observed from git

You never type a sha into the log. At propose time cv runs git itself:

- resolves the **branch tip** (a `--sha` you pass is only an assertion — refused if it isn't
  the tip; a branch with no commits over upstream is refused too),
- records the **merge-base** of upstream and the tip at propose time (so the identity stays
  recomputable between two fixed commits forever, even after a fast-forward land),
- computes the **range patch-id**: `git diff <base> <tip> | git patch-id --stable` — the
  cumulative content identity of the *whole branch*. A tampered or dropped earlier commit
  changes it, which is exactly the hole tip-only patch-ids leave open.

`branch` and `worktree` are locators, never identity; `review_sha` + `patch_id` are identity.

Propose also runs an advisory **collision scan**: if another live task's current revision
already carries this branch (or worktree) in the same repo, you get a warning — two tasks
proposing one branch usually means two agents about to trample each other. Recorded and
warned, never blocked.

## Verify: the observation pass

```sh
cv task verify <id>          # one task
cv task verify --all         # everything verifiable
cv task verify --all --fetch # git fetch each upstream's remote first
```

For every `ready`/`merged_local` revision the verifier asks git — never an agent — what is
true:

1. **Is the reviewed content on the upstream?** By ancestry first, then by `git cherry`
   patch-id equivalence (so a cherry-picked/rebased land under a *new* sha still counts). If
   yes: recompute the range patch-id from the recorded base and append `landed`. The reducer
   cross-checks the observed patch-id against the revision's — a tampered record fails loudly.
2. **Merged into the local `main`/`master` but not pushed?** Consulted by explicit refs, never
   `HEAD` (a checkout sitting on the task branch must not self-certify) → `merged_local`.
3. **Otherwise, why not?** Findings, which never change state: repo missing
   (`source unavailable`), worktree missing, branch deleted, **branch tip no longer the
   reviewed sha** (someone committed after review), upstream moved past the recorded base
   (`not fast-forwardable`). A finding identical to a recent one is deduped so a broken world
   doesn't spam the log; a reviewed, intact, fast-forwardable branch that simply hasn't landed
   yet appends nothing at all. A git *error* is never treated as landed.

### The heartbeat, `verified as of`, and SUSPECT rows

Silence is never evidence of health. Every verify pass writes a heartbeat
(`tasks/last_verify.json`), and every debt view carries it as `verified_as_of` plus a
`verify_warning` when trust isn't warranted:

- **No heartbeat at all** is the loudest state: *"landing state has NEVER been verified — run
  `cv task verify --all` or enable `cvd watch --verify-interval`"*.
- **A stale heartbeat** (older than 2× its own recorded interval) means the periodic verifier
  is probably dead, and says so.

The periodic driver is [`cvd watch`](daemon.md#cvd-watch--follow-live--archive-as-it-happens),
which runs the same engine every `--verify-interval` seconds (default **300**, `0` disables).

Each full pass also **re-observes revisions recorded `landed`**. Landed-ness is monotone-true
for genuine lands, so reviewed content that is *no longer observed* on its upstream contradicts
the record — either a forged `landed` event (someone echoing JSON into the log; replay already
warns when a verifier-only event carries a non-verifier author) or a genuinely rolled-back
land. Those become **SUSPECT rows**: persisted in the heartbeat, rendered on the debt view as
visible debt again, and *not* laundered away by a partial or `--skip-landed` pass.

One more honesty property of the log itself: reads are best-effort and **loud** — an interior
line the reducer refuses is quarantined with a warning naming the line, and every surface
ships those warnings; appends **fail closed** on a degraded log rather than validating against
an incomplete model.

### Provenance & freshness: every fact knows how it was learned and when

A fact is never just true — it is *observed true, by someone, as of some moment*, and that
observation has a shelf life. The task substrate makes this structural: every landing and every
self-reported completion carries a small **provenance** shape — `{ observed_at, source,
freshness }` — so a reader can always answer "as observed when, by which pass?"

- **`source`** is how the fact was learned: `git-verify` for a land the verifier read from git,
  or `self-report` for a base-lifecycle `Done`. A self-reported completion is *labeled* as such,
  never silently equal to a git-verified land — the `Done` carve-out made visible rather than
  hidden.
- **`observed_at`** is the moment of observation (the `Landed` event's timestamp; the verifier's
  last pass over an unlanded row; `null` when nothing has observed it yet).
- **`freshness`** is derived from the heartbeat, and **`Unknown` is first-class**: `Fresh` (a
  pass covered it recently), `Stale { age_secs }` (the periodic verifier's last pass is older
  than 2× its interval — it may be dead), or `Unknown` (the verifier has *never* checked this
  revision since it became verifiable — not implicitly fine, said out loud).

This surfaces on the debt view (`landed · observed 4m ago` / `SUSPECT · last checked 2h ago` /
`ready … · NEVER verified`), on `cv task show` (the landed line reads `git-verified, observed 4m
ago`; a `Done` reads `self-reported`), and as additive `provenance` keys on `cv task debt
--json`, the MCP `task_debt` tool, and `GET /api/tasks/debt`. Freshness derivation is a **pure**
function of the heartbeat's fields — no extra git calls, no I/O in the projection layer.

## The read surfaces

**`cv task list`** — non-terminal tasks by default, oldest first, with an age column off the
last event. `--state <s>` filters by effective state, and a typo is an error naming the whole
vocabulary, never a silently-empty list:

```text
Error: unknown state "redy" (expected one of open|claimed|done|abandoned|superseded|awaiting_review|ready|merged_local|landed|refuted)
```

**`cv task inbox [who]`** — "what needs me", **grouped by reason and stalest first within a
group** (age is the escalation mechanism; there is no other). Bare `cv task inbox` means *my*
inbox via `$CV_ENDPOINT`. The groups print in this order — `decisions owed`, `in discussion`,
`made for you (veto?)`, `assigned actions`, `claimed work`, `reviews`, `unlanded` — because a
decision nobody sees is the slowest blocker a fleet has. Each reason has an honest aging anchor:

| Reason | You appear because | Ages since |
| --- | --- | --- |
| `DecisionOwed` | a live decision (posed, or tagged `decision`) is assigned to you | the **pose** (the last event for a tag-only one) |
| `Discussing` | you parked a decision for discussion — open, not owed | the last event |
| `Provisional` | a decision assigned to you was resolved *for* you, provisionally — resolved, not owed | the **provisional resolve** |
| `AssignedOpen` | an open task is assigned to you, unclaimed | the last event |
| `ClaimedByYou` | you claimed it; it's yours to finish | the last event |
| `AwaitingYourReview` | a revision awaits your verdict | the **propose** |
| `YourUnlandedWork` | your reviewed revision isn't observed landed | the **pass** |

Rows waiting more than 24 hours lead with a `⏰`. A dead reviewer is honest state that nobody
sees unless it ages somewhere the owner reads — this is that somewhere.

**`cv task debt`** — the honest ledger: reviewed-but-unlanded work grouped by repo, oldest
first, with recorded findings; then aged awaiting-review rows; then SUSPECT rows; then the
heartbeat line. If this view isn't empty, something finished isn't on main yet.

```text
$ cv task debt
/Users/you/code/demo:
  019f6e4c  rev1 task/retry-backoff [ready] unlanded for 0h — add retry backoff
verified as of 2026-07-17 00:18:46
```

**`cv task sweep --repo <path>`** — candidates for closing, observed rather than asserted:
every non-terminal task (in that repo, or with no repo) whose `--issue` path no longer exists
(tried against the task's repo, the swept repo, the directory above it and the cwd), or whose
title/body names a branch now merged into the repo's main (`git branch --merged`, local and
remote-tracking; a name has to look like a branch — a separator, a digit, or eight characters —
so a branch called `fix` does not sweep every task that says "fix"), or whose proposed revision's
branch is merged. It prints them as *probably done* with the reason and **never closes one**;
`done --observed` or `abandon --reason` is still a decision someone makes having read why.

All of these take `--json` for the full wire shapes (the same rows MCP and the HTTP API serve).

## Identity: `CV_ENDPOINT`

The fleet convention: the spawner sets `CV_ENDPOINT=agent:<name>` in each agent's environment,
and every cv front-end derives its default actor from it — the *bare* command records the
right identity. Nothing verifies the string (law 3); it exists so a forgotten flag can't
misattribute the durable record forever.

- **Chat-grade verbs** (`open`, `note`, `done`, `abandon`, `supersede`; board posts) fall back
  to a deliberate default sink — `cv` on the CLI (a human at a shell owes no ceremony),
  `agent` over MCP.
- **Identity-bearing verbs** — `claim`, `release`, `propose`, `pass`, `refute`, whose endpoint
  string *keys inbox and reviewer semantics* — refuse to run without an identity, naming the
  cure:

```text
$ cv task claim 019f6e4c
Error: set CV_ENDPOINT or pass --from; identity-bearing events must record who acted
```

Review independence (law 2) rides on identity plus transcripts: pass `--session <your-cv-session-id>`
when reviewing and cv reads the author's and reviewer's harness families from its catalog.
Same-family review is **recorded and warned about, never blocked** — and cross-family review
is the value the warning protects. Receipts are a **heuristic signal, not proof**: `saw_change`
requires structural engagement (a content read of a path under the repo, or a real
`git diff`/`git show`/`git log`), so quoting the sha alone (`echo <sha>`) is undetermined, not a
pass — but a reviewer who pointlessly opens a repo file still passes (`adversary_gym.rs::
saw_change_rejects_bare_sha_echo` and `receipts_remain_a_heuristic_by_design`). The bar is raised
past the trivial forgery, never made a guarantee.

## A worked example

Two agents, one repo. `agent:mira` authors; `agent:rex` reviews. (Transcript from a real run;
the repo path is abbreviated.)

```text
$ export CV_ENDPOINT=agent:mira
$ cv task open "switch task ids to uuid v7" --repo ~/code/demo
✦ opened 019f6e4b → open
019f6e4b-74c5-74f0-a493-1f8131ca38d5

$ cv task claim 019f6e4b
✦ claimed 019f6e4b → claimed
```

Mira does the work on a branch, then proposes — note that the sha and range patch-id are
*observed*, printed back from git:

```text
$ cv task propose 019f6e4b --branch task/uuid-ids --upstream main --reviewer agent:rex
observed: task/uuid-ids tip c889a0236d6c range-patch-id f408d8649ec0
✦ revision_proposed 019f6e4b → rev:awaiting_review
```

Rex's inbox now carries the review, aging from the propose:

```text
$ cv task inbox agent:rex
   019f6e4b    0s  AwaitingYourReview       switch task ids to uuid v7

$ CV_ENDPOINT=agent:rex cv task pass 019f6e4b
⚠ no reviewer session given: reviewer independence not checked (recorded as unknown)
✦ review_passed 019f6e4b → rev:ready
```

Ready is not landed. A verify pass before anything reaches `main` observes exactly nothing,
and the debt view carries the revision:

```text
$ cv task verify --all
(nothing new observed)

$ cv task debt
/Users/you/code/demo:
  019f6e4b  rev1 task/uuid-ids [ready] unlanded for 0h — switch task ids to uuid v7
verified as of 2026-07-17 00:17:56
```

Now the land actually happens — by whoever has push rights (law 3), outside cv entirely — and
the next verify pass *observes* it:

```text
$ git -C ~/code/demo merge --ff-only task/uuid-ids

$ cv task verify --all
✦ observed landed on 019f6e4b

$ cv task done 019f6e4b
✦ done 019f6e4b → rev:landed
```

(`done` was refused while the revision was live; after the observed land it goes through.)
The full projection keeps the whole story — review evidence, the landed observation with the
upstream head and the re-computed patch-id:

```text
$ cv task show 019f6e4b
task 019f6e4b-74c5-74f0-a493-1f8131ca38d5  [rev:landed]
  title:    switch task ids to uuid v7
  repo:     /Users/you/code/demo
  channel:  #tasks
  assignee: agent:mira
  opened:   2026-07-17 00:17 by agent:mira
  rev 1: task/uuid-ids [landed] c889a0236d6c → main
         reviewer: agent:rex
         landed: upstream c889a0236d6c (patch-id f408d8649ec0)
```

`cv task show <id> --events` (or `GET /api/task/{id}/events`) prints the raw durable history —
for this task: `opened`, `claimed`, `revision_proposed`, `review_passed`, `landed`, `done`.

## Growth & retention

Honesty section: `tasks/events.jsonl` is **append-only and currently unbounded** — every event
ever appended stays in the file, and every read replays all of it. At fleet-task volumes that
is cheap for a long time, but it does grow monotonically. Compaction is planned as
**snapshot + tail**: fold the log's stable prefix into a read-model snapshot, keep the live
tail as raw events, and retire the prefix to an archive file — preserving the audit prefix
(retired events stay on disk and verifiable; they just stop being replayed on every read).
Until that lands, the log is a single file you can archive by hand, and replay cost grows
linearly with event count. The board's channels have the same property; see
[the board chapter](board.md#growth--retention).
