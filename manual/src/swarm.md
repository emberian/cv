# Running a swarm: the orchestrator's instruments

This chapter is written from one long session (2026-09-30 → 10-02) in which one orchestrator ran
between eight and twenty-four sub-agent lanes against two build boxes for about forty hours, landed
some sixty branches, and used every task and lane instrument `cv` has. It records what the
instruments are FOR, the rules that fell out of using them, and the defects they caught (and the
ones they did not). Where a rule has a date, something broke that day.

The shape of the session: an **orchestrator** (one long-lived harness session) spawns **lanes**
(sub-agents, each with a brief, a clone of the tree, and a `CV_ENDPOINT` of its own), lanes
land branches into a base repository and write reports, an **integrator** lane merges them, and a
human (the **decider**) answers decisions through the inbox. `cv` is the memory the orchestrator does
not have: the task store is the backlog and the obligations, the lane table is the live state of the
forest, the deferral linter is the closeout gate, and the inbox is what the human owes.

## The instruments, and what each is for

| instrument | one-line purpose | the rule that came with it |
|---|---|---|
| `cv task open/claim/note/done` | every obligation is a task before the reply that mentions it | "a *later* in chat is a dropped obligation" — `cv deferrals <session> --open-tasks` exits 1 on any unmatched one |
| `cv task decide --for <who> --default … --option …` | a decision for the human, with a default the swarm can proceed on | the orchestrator may *act on the default* and say so; the inbox shows it as owed until resolved |
| `cv task inbox <who>` / `cv task serve --open` | what the human owes, drained in one sitting | pose decisions with defaults *before* the human arrives; poll `cv task events --by <who>` at every check-in |
| `cv lanes <session> --running` | the forest as a status table: model, start, tokens, calls, last tool, stranded | a row whose token count is frozen for an hour is DEAD even if it says running: stop it, relaunch from its clone |
| `cv lanes --stranded` | lanes that stopped saying they are waiting for a notification nothing will send | brief lanes to FOREGROUND-wait; resume a stranded lane with one message naming what finished |
| `cv deferrals <session> --open-tasks` | every promise in the transcript, matched against the store | run it at every closeout; an UNMATCHED line is a task to open now |
| `cv task show ID --notes-grep PAT` / `--notes-last N` | query a long task without loading it | a 100-note integrator task dumped whole killed a 13-hour lane; **query, never dump** |
| `cv prompts <session>` | only what the human said | re-read the brief before any status to the human |
| `cv show <session> --pre-compaction` | the span a compaction summary lost | after any compaction, read the full turns, not the summary: tactics live in the turns |
| `cv prune` | a resumable copy with bulky payloads stashed | the account-move case: a running process follows the rotated credential, so prune is for context, not for accounts |

## Rules that paid for themselves

**Briefs are claims.** The orchestrator is the least-checked source in the swarm. Mark every claim in a
brief *measured* / *read* / *assumed-by-me*; paste real signatures and absolute paths; name the
QUESTION, not the target. A lane that cannot see the foundation reconstructs it from the prose and
verifies against its own reconstruction.

**Verdicts come from artifacts.** A lane's "done" is a claim. The gate is a journey row (expected /
got on a fresh store), a theorem statement read by someone else, or an adversarial audit. Three
times in one day a sibling lane found a flaw in a landed branch that the branch's own journey had
passed; one audit lane found ten in a merged tree. Budget an audit after every braid.

**Warm bases, never parallel rebuilds.** Build a tip once per box; clone lanes from the warm tree by
hard links and privatise what they write (`cp -al` → unlink the small files → unlink the sources,
and the `.git` bookkeeping, and the clone's top-level helper scripts: each of those leaked between
clones once). Prune a landed clone (its build caches) only after its HEAD is in the base.

**Keep the forest between eight and fifteen.** Count after every landing; below the floor, launch
before replying. The ceiling is not ideas: it is disk, warm trees, and the account's usage window
(ten Opus lanes plus a Fable scholar emptied a five-hour window in about four and a half hours,
twice; the harness reports the kill as `failed` with the API error text — resume each lane with one
message naming its clone's HEAD, its dirty files, and which of its processes kept running).

**The relay integrator.** A standing integrator dies of context after ten to fourteen hours. Replace it
with generations: each owns the tree, does at most three merges, rewrites a STATUS block (tip, what is
merged, what runs with pids and paths, what is next, the collisions it knows) and hands off; the
orchestrator launches the next generation from that block. Pre-merge each family of branches onto the
tree in a **braid** lane so the integrator takes one branch per family. Allocate shared numbers (op
tables, codec tags, version labels) centrally before launch: three version collisions and two tag
collisions in one day were all two lanes picking the same free number.

**Decisions with defaults.** The human's inbox was the longest-waiting blocker in the swarm (nine
hours, once). Pose every taste fork, key-material question and weakened check as a decision with a
default; proceed on the default where the work does not weaken a check; record every decision the
orchestrator makes on the human's behalf as such ("my decision, veto-able") on its task. When the
human drains the inbox, read `cv task events --by <who>` and act on every answer that changes a brief.

**Deferrals go into the store before the reply.** The linter exists because "later" in a chat message is
where obligations die. One rule saved several: open the task, then write the sentence that mentions it.

**Status is printed by a run, never typed.** Keep one NOW page (one screen: the bar, the frontier from
journeys, the lanes, what waits on the human) and rewrite it at every landing. A narrative state file
is for the successor after a compaction; the NOW page is for the human.

## What the instruments caught, and what they did not

Caught: a lane whose journey was pending when it reported (stranded); two lanes wedged on a shell
call that never returned (frozen token counts); seven unmatched deferrals across the session; an
inbox item posed without options (a lane tagged `decision` instead of posing one).

Not caught, and now rules: the page's "needs discussion" left the decision owed, so the human
clicked an option to clear it — three resolutions that were clicks (fixed 10-01: discussing is its
own inbox state); notes on a terminal task are refused, so lanes that marked `done` before writing
their note lost the evidence (write the note first; see the edits below); two lanes opened a task
that already existed under another title.

## Edits to `cv` that this session asks for

Ranked by how often the gap bit:

1. ~~**Notes on terminal tasks.** Terminal is about *state*, not about the record: `cv task note` on a
   done or resolved task should append (marked post-close), and `cv task done --note FILE` should be
   one event. Four lanes and the orchestrator lost or re-homed notes on this.~~ **Landed:** a note on
   a terminal task appends with `post_close: true` ("(after close)" on every surface) and never moves
   the state; `done` / `abandon` take `--note` / `--note-file` and append note + close as one unit.
2. **Provisional resolutions.** `cv task decide … --provisional` resolves a decision on its default *by
   the orchestrator* with a veto window; the inbox shows it under "made for you — veto?" and the
   decider's `resolve` overrides. Today this is prose on a note.
3. **One way to pose a decision.** A task tagged `decision` with no options should be refused or
   upgraded to a `decide` with a required default.
4. **Lanes joined to tasks.** `cv lanes --tasks`: the lane table knows the agent and its description;
   the store knows which `lane:<name>` endpoint claimed which task. Join them, and the page's Lanes
   pane shows each lane's task and last note.
5. **A pinned STATUS note.** `cv task status ID --file F` replaces a task's current-state note (shown at
   the top of `show`), so a relay's hand-off is a field, not the newest of a hundred notes.
6. **Failure cause from the transcript.** A lane that died of an API rate limit, a context-length
   error, or a stop should show as `rate-limited` / `context` / `stopped`, not all as `failed`: the
   remedy differs (resume vs relaunch).
7. **`show` defaults.** `cv task show` prints the body and the last three notes with a count; `--all`
   dumps. The dump is what killed the integrator.
8. **Dedupe on open.** `cv task open` warns when an open task's title is near the new one.
