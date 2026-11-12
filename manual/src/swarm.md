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
forest, and the inbox is what the human owes.

## The instruments, and what each is for

| instrument | one-line purpose | the rule that came with it |
|---|---|---|
| `cv task open/claim/note/done` | every obligation is a task before the reply that mentions it | "a *later* in chat is a dropped obligation" — open the task before writing the sentence that mentions it |
| `cv task decide --for <who> --default … --option …` | a decision for the human, with a default the swarm can proceed on | the orchestrator may *act on the default* and say so; the inbox shows it as owed until resolved |
| `cv task inbox <who>` / `cv serve --open` (the inbox page) | what the human owes, drained in one sitting | pose decisions with defaults *before* the human arrives; poll `cv task events --by <who>` at every check-in |
| `cv lanes <session> --running` | the forest as a status table: model, start, tokens, calls, last tool, stranded | a row whose token count is frozen for an hour is DEAD even if it says running: stop it, relaunch from its clone |
| `cv lanes --stranded` | lanes that stopped while still holding an open obligation (an unreturned sub-agent, an open tool call, a claimed task, an unanswered decision) or whose process died mid-turn | brief lanes to FOREGROUND-wait; resume a stranded lane with one message naming what finished |
| `cv task show ID --notes-grep PAT` / `--notes-last N` | query a long task without loading it | a 100-note integrator task dumped whole killed a 13-hour lane; **query, never dump** |
| `cv show <session> --kind prompt,answer` | only what the human said | re-read the brief before any status to the human |
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

**Obligations go into the store before the reply.** "Later" in a chat message is
where obligations die. One rule saved several: open the task, then write the sentence that mentions it.

**Status is printed by a run, never typed.** Keep one NOW page (one screen: the bar, the frontier from
journeys, the lanes, what waits on the human) and rewrite it at every landing. A narrative state file
is for the successor after a compaction; the NOW page is for the human.

## What the instruments caught, and what they did not

Caught: a lane whose journey was pending when it reported (stranded); two lanes wedged on a shell
call that never returned (frozen token counts); an
inbox item posed without options (a lane tagged `decision` instead of posing one).

Not caught, and now rules: the page's "needs discussion" left the decision owed, so the human
clicked an option to clear it — three resolutions that were clicks (fixed 10-01: discussing is its
own inbox state); notes on a terminal task are refused, so lanes that marked `done` before writing
their note lost the evidence (since fixed: a post-close note appends and says so); two lanes opened a task
that already existed under another title.
