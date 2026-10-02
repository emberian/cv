//! 📋 The task substrate — durable, replayable dispatch objects for agent fleets.
//!
//! Where the [`crate::board`] lets agents *talk*, tasks let a fleet *commit to work* and let a
//! human see what is actually true about it. A task is a durable object with a replay-derived
//! lifecycle; code tasks additionally carry reviewed **revisions** whose landing is *observed*
//! by running git, never taken on an agent's word.
//!
//! # The four laws
//!
//! 1. **Observed, not attested.** No agent claim ever produces `MergedLocal` or `Landed`; only
//!    the cv-side git verifier emits those events, and agent-facing append paths reject them at
//!    the store seam.
//! 2. **Independence is read, not asserted.** Reviewer independence (cross-family review) is
//!    determined by reading the reviewer's transcript harness from cv's catalog — advisory-warn,
//!    never a gate.
//! 3. **No authority machinery.** No seats, grants, or debts. Landing authority is whoever can
//!    push to the repo; cv only tracks and verifies.
//! 4. **Small.** This substrate stays a few thousand lines. Complexity requests go to the
//!    design-notes graveyard, next to the 200k-line system this replaced.
//!
//! Law 1 governs the **land facet** — `MergedLocal`/`Landed` on code revisions — and now extends
//! to base-lifecycle completion: `cv task done --check-*` makes cv RUN a completion predicate, so a
//! non-code `Done` can be OBSERVED (provenance `checked`, [`check::CheckSpec`]) rather than taken
//! on the agent's word. A check-less `Done { observed }` stays **self-reported** — the honest
//! fallback for tasks with no runnable predicate, LABELED as such (defended by
//! `crates/cv-sim/tests/adversary_gym.rs::done_with_a_check_is_verified_not_attested`).
//!
//! Storage is a single append-only event log (`$CLUSTERVISION_HOME/tasks/events.jsonl`) with the
//! same crash-safe flock recipe as the board.

#[cfg(not(target_family = "wasm"))]
pub mod check;
pub mod decide;
pub(crate) mod identity;
pub mod model;
pub mod project;
pub mod provenance;
pub mod reduce;
#[cfg(not(target_family = "wasm"))]
pub mod stats;
pub mod store;
#[cfg(not(target_family = "wasm"))]
pub mod verify;
pub mod views;

#[cfg(not(target_family = "wasm"))]
pub use check::CheckSpec;
pub use model::{
    harness_family, model_family, DoneCheck, DoneCheckKind, IndependenceCheck, MergeFailure, ReviewReceipts, Revision,
    RevisionState, TaskEvent, TaskEventKind, TaskState, VERIFIER_BY,
};
pub use decide::{is_decide_note, options_of, parse_deadline, parse_decide_note, DecideNote, AS_PROPOSED};
pub use project::{
    age_short, awaiting_review, blocks, branch_carriers, debt, effective_display, in_scope, inbox, involves,
    is_blocked, list, parse_duration, parse_since, propose_collision_warnings, resolve_id, same_actor,
    unique_prefix_len, uuid_v7_timestamp, AwaitingReviewEntry, DebtEntry, InboxEntry, InboxReason, TaskFilter,
    DECISION_TAG, DISCUSS_TAG, STATE_VOCABULARY,
};
pub use provenance::{freshness_from_heartbeat, Freshness, Provenance};
pub use reduce::{
    Decision, EffectiveState, Note, PassEvidence, PinnedStatus, ReduceError, RefuteEvidence, RerouteEvidence, Resolution,
    RevisionProjection, TaskIssue, TaskProjection, TaskReadModel, TaskReducer,
};
#[cfg(not(target_family = "wasm"))]
pub use stats::{EndpointRow, FamilyRow, FleetStats, ReviewerRow};
pub use store::{new_event, replay, ReplayOutcome, TaskStore};
pub use views::{AwaitingRow, DebtRow, InboxDecision, InboxRow, TaskRow};
#[cfg(not(target_family = "wasm"))]
pub use views::{DebtReport, SuspectRow};

/// Advisory reviewer-independence check (law 2): read harness families from cv's catalog for the
/// reviewer's session and the current revision's recorded author session. Never blocks anything —
/// the result is recorded on the pass event and surfaced as a warning by the front-ends.
pub fn independence_check(t: &reduce::TaskProjection, reviewer_session: Option<&str>) -> Option<IndependenceCheck> {
    let reviewer_session = reviewer_session?;
    let reviewer_family = session_family(reviewer_session);
    Some(independence_from(t, reviewer_family))
}

/// The model family a session id resolves to: harness family when the harness is single-vendor,
/// else the transcript's per-message `model` ids (multi-model harnesses like Cursor/OpenCode).
/// `None` when the session can't be found/read or the family can't be named — never guessed.
fn session_family(session_id: &str) -> Option<String> {
    let (sref, adapter) = crate::find_cheap(session_id, None).ok()??;
    if let Some(f) = harness_family(sref.harness) {
        return Some(f.to_string());
    }
    session_model(&*adapter, &sref)
        .as_deref()
        .and_then(model_family)
        .map(String::from)
}

/// Assemble the recorded independence observation from the author side of `t` and an
/// already-determined reviewer family (shared by [`independence_check`] and
/// [`review_observation`], whose reviewer family comes out of the single receipts scan).
fn independence_from(t: &reduce::TaskProjection, reviewer_family: Option<String>) -> IndependenceCheck {
    let author_family = t
        .current_revision()
        .and_then(|r| r.revision.session_ref.as_deref())
        .and_then(session_family);
    let independent = match (&author_family, &reviewer_family) {
        (Some(a), Some(r)) => Some(a != r),
        _ => None,
    };
    IndependenceCheck {
        author_family,
        reviewer_family,
        independent,
    }
}

/// Command substrings that count as "ran checks" for [`ReviewReceipts::ran_checks`]. A **named,
/// documented heuristic**, not a semantic analysis: a tool input containing one of these looks
/// like a test/build run. Grow it deliberately; matching is plain case-sensitive substring.
pub const CHECK_COMMAND_PATTERNS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "pytest",
    "npm test",
    "make test",
    "go test",
];

/// Shell commands that read file CONTENT (not merely list/stat a path). A tool call invoking one of
/// these against a path under the reviewed repo is engagement evidence for
/// [`ReviewReceipts::saw_change`] — the reviewer opened a file in the change's tree, not just quoted
/// its sha. Deliberately narrow; grow it like [`CHECK_COMMAND_PATTERNS`].
const READ_COMMANDS: &[&str] = &[
    "cat", "less", "more", "head", "tail", "bat", "nl", "od", "xxd", "hexdump", "grep", "egrep", "fgrep", "rg", "ag",
    "sed", "awk", "view",
];

/// Structured tool NAMES (Claude Code / harness read-and-search tools) that carry a file/path in
/// their input. Same engagement weight as [`READ_COMMANDS`] when that path targets the repo.
const READ_TOOL_NAMES: &[&str] = &["Read", "Grep", "Glob", "NotebookRead"];

/// Split a shell command into the pipeline/sequence segments a real invocation is built from
/// (`;`, `|`, `&`, newline), each trimmed and non-empty. Classification then looks at each
/// segment's COMMAND POSITION (its leading token), never a bare substring — this is what defeats
/// `echo "git diff"` / `echo <sha>`: the segment's first token is `echo`, not `git`/`cat`.
fn command_segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A command segment with leading `NAME=value` env-assignments stripped, so `RUST_LOG=x cargo test`
/// and `GIT_PAGER=cat git diff` classify by their real leading command. Only whitespace-free
/// assignment tokens are stripped (the common shell case); anything else stops the strip.
fn command_head(segment: &str) -> &str {
    let mut rest = segment.trim_start();
    while let Some((head, tail)) = rest.split_once(char::is_whitespace) {
        let is_env = head
            .split_once('=')
            .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        if is_env {
            rest = tail.trim_start();
        } else {
            break;
        }
    }
    rest
}

/// Is `segment` a real `git diff` / `git show` / `git log` invocation? The leading token (after any
/// env prefix) must be `git`, and the first non-flag token after it must be diff/show/log — so
/// `git --no-pager show <sha>` counts while `echo "git diff"` (leading token `echo`) does not.
fn is_git_inspection(segment: &str) -> bool {
    let mut toks = command_head(segment).split_whitespace();
    if toks.next() != Some("git") {
        return false;
    }
    toks.find(|t| !t.starts_with('-'))
        .is_some_and(|sub| matches!(sub, "diff" | "show" | "log"))
}

/// Does `segment` run a content-reading command ([`READ_COMMANDS`]) against a path under `repo`?
/// The leading token (after any env prefix, and after stripping any binary path prefix) must be a
/// read command, AND the repo path must appear as an argument. `echo /repo/x` is not a read; `cat
/// /repo/x` is.
fn reads_under_repo(segment: &str, repo: Option<&str>) -> bool {
    let Some(repo) = repo else { return false };
    let head = command_head(segment);
    let Some(first) = head.split_whitespace().next() else {
        return false;
    };
    let cmd = first.rsplit('/').next().unwrap_or(first);
    READ_COMMANDS.contains(&cmd) && segment.contains(repo)
}

/// What one pass over the reviewer's transcript observed: the model that spoke (for the
/// independence family) plus the receipts. Internal — the public results are
/// [`ReviewObservation`] and [`review_receipts`].
struct ReviewerScan {
    last_model: Option<String>,
    receipts: ReviewReceipts,
}

/// Both advisory review-time observations, computed from ONE parse of the reviewer's transcript
/// (law 2's posture for each: read, record, warn — never gate).
#[derive(Debug)]
pub struct ReviewObservation {
    pub independence: Option<IndependenceCheck>,
    pub receipts: Option<ReviewReceipts>,
}

/// Observe reviewer independence AND reviewer receipts for a verdict on `t`'s current revision,
/// parsing the reviewer's session once. With no session id, both observations are `None`
/// (recorded as unknown, warned about by the shared wording helpers).
pub fn review_observation(t: &reduce::TaskProjection, reviewer_session: Option<&str>) -> ReviewObservation {
    let Some(sid) = reviewer_session else {
        return ReviewObservation {
            independence: None,
            receipts: None,
        };
    };
    let scanned = scan_reviewer_session(sid, t);
    let reviewer_family = scanned.as_ref().and_then(|(harness, scan)| {
        harness_family(*harness)
            .map(str::to_string)
            .or_else(|| scan.last_model.as_deref().and_then(model_family).map(String::from))
    });
    // Session named but unreadable: all-None receipts — "we looked, undetermined", distinct on
    // the record from "no session given" (receipts absent entirely).
    let receipts = Some(scanned.map(|(_, scan)| scan.receipts).unwrap_or_default());
    ReviewObservation {
        independence: Some(independence_from(t, reviewer_family)),
        receipts,
    }
}

/// Receipts-only observation (the refute path, which records no independence check). Same
/// single-parse scan as [`review_observation`]; `None` only when no session id was given.
pub fn review_receipts(t: &reduce::TaskProjection, reviewer_session: Option<&str>) -> Option<ReviewReceipts> {
    let sid = reviewer_session?;
    Some(
        scan_reviewer_session(sid, t)
            .map(|(_, scan)| scan.receipts)
            .unwrap_or_default(),
    )
}

/// Resolve the reviewer's session and scan it. `None` when the session can't be found — the
/// caller records all-None receipts for that.
fn scan_reviewer_session(session_id: &str, t: &reduce::TaskProjection) -> Option<(crate::ir::Harness, ReviewerScan)> {
    let (sref, adapter) = crate::find_cheap(session_id, None).ok()??;
    let scan = scan_session(&*adapter, &sref, t)?;
    Some((sref.harness, scan))
}

/// How strongly one tool call ties the reviewer to the change under review — the `saw_change`
/// evidence hierarchy, weakest to strongest. Ordered so `max` over a session keeps the best signal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Engagement {
    /// Nothing ties this call to the change.
    None,
    /// The change's IDENTITY (review sha, its 12-prefix, the branch, or the repo path) appears as a
    /// bare substring only — the `echo <sha>` case. Suggestive, but NOT proof the reviewer opened
    /// the change. Downgraded on purpose: this alone leaves `saw_change` undetermined, not `true`.
    Mention,
    /// STRUCTURAL evidence the reviewer actually engaged the change: a content read
    /// ([`READ_COMMANDS`]/[`READ_TOOL_NAMES`]) against a path under the repo, or a real
    /// `git diff`/`git show`/`git log` invocation. This — and only this — satisfies `saw_change`.
    Engaged,
}

/// Classify one tool call against the change's identity ([`Engagement`]). `repo` is the reviewed
/// repo's display path (the read-target anchor); `needles` are the identity strings for the bare
/// [`Engagement::Mention`] fallback. Shell invocations (`command`/`cmd`) are read segment-by-
/// segment in command position; structured read/search tools ([`READ_TOOL_NAMES`]) engage when
/// their rendered path targets the repo.
fn classify_engagement(name: &str, input: &serde_json::Value, needles: &[String], repo: Option<&str>) -> Engagement {
    if let Some(command) = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(|v| v.as_str())
    {
        if command_segments(command).any(|seg| is_git_inspection(seg) || reads_under_repo(seg, repo)) {
            return Engagement::Engaged;
        }
    }
    let rendered = format!("{name} {input}");
    if READ_TOOL_NAMES.contains(&name) {
        if let Some(repo) = repo {
            if rendered.contains(repo) {
                return Engagement::Engaged;
            }
        }
    }
    if needles.iter().any(|n| rendered.contains(n.as_str())) {
        return Engagement::Mention;
    }
    Engagement::None
}

/// Did this tool call RUN a check (test/build) command — in command position, not merely quoting
/// one? Same segment/env-prefix discipline as the engagement signals, so `echo "cargo test"` does
/// not count.
fn ran_check_command(input: &serde_json::Value) -> bool {
    input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(|v| v.as_str())
        .is_some_and(|command| {
            command_segments(command).any(|seg| CHECK_COMMAND_PATTERNS.iter().any(|p| command_head(seg).starts_with(p)))
        })
}

/// One forward pass over a transcript, observing everything the review-time advisories need:
/// the last assistant `model` (independence family fallback), the assistant turn count, and the
/// contact/checks heuristics over tool inputs. Streamed under [`ParseOptions::lazy`] — tool
/// inputs are always inline; giant text/tool-result content stays unresolved on disk.
///
/// Honesty rules: `None` means undetermined. `saw_change` is only judged when the task has a
/// current revision (branch, review sha + 12-prefix, repo path). It is a HEURISTIC, not proof, and
/// runs an evidence hierarchy over tool-call **inputs** ([`Engagement`], strongest wins):
/// - [`Engagement::Engaged`] → `Some(true)`: a content read under the repo, or a real
///   `git diff`/`git show`/`git log` invocation. The reviewer opened the change, not just its name.
/// - [`Engagement::Mention`] → `None`: the change's identity (sha/branch/repo path) appears only as
///   a bare argument (`echo <sha>`). Suggestive but unproven — undetermined, deliberately NOT
///   `true`. This is the echo downgrade that narrows the old Goodhart hole.
/// - [`Engagement::None`] with revision context → `Some(false)`: we scanned and saw no contact at
///   all (a genuine rubber stamp).
///
/// It remains gameable by a determined adversary who pointlessly opens a file in the repo — the bar
/// is raised past the trivial echo, not made a guarantee. `ran_checks` uses the same command-
/// position discipline over [`CHECK_COMMAND_PATTERNS`]. Both look at tool inputs only; prose claims
/// are exactly what this observation refuses to take at face value.
fn scan_session(
    adapter: &dyn crate::harness::Adapter,
    sref: &crate::ir::SessionRef,
    t: &reduce::TaskProjection,
) -> Option<ReviewerScan> {
    // Identity strings for the bare-mention tier; `repo` anchors the read-target signal. Structural
    // contact (git inspection / repo-path reads) is detected in command position, not via needles.
    let mut needles: Vec<String> = Vec::new();
    let mut repo: Option<String> = None;
    let has_revision = t.current_revision().is_some();
    if let Some(rev) = t.current_revision() {
        needles.push(rev.revision.branch.clone());
        needles.push(rev.revision.review_sha.clone());
        needles.push(rev.revision.review_sha.chars().take(12).collect());
        if let Some(r) = &t.repo {
            let r = r.display().to_string();
            needles.push(r.clone());
            repo = Some(r);
        }
    }

    let mut last_model: Option<String> = None;
    let mut turns: u32 = 0;
    let mut best = Engagement::None;
    let mut ran_checks = false;
    let mut sink = |m: crate::ir::Message| {
        if m.role == crate::ir::Role::Assistant {
            turns += 1;
            if let Some(model) = &m.model {
                last_model = Some(model.clone());
            }
        }
        for b in &m.content {
            if let crate::ir::Block::ToolUse { name, input, .. } = b {
                best = best.max(classify_engagement(name, input, &needles, repo.as_deref()));
                ran_checks = ran_checks || ran_check_command(input);
            }
        }
        crate::stream::Flow::Continue
    };
    adapter
        .stream(sref, &crate::stream::ParseOptions::lazy(), &mut sink)
        .ok()?;
    // No revision context → contact undeterminable (None). Otherwise: structural engagement is the
    // only thing that reads `true`; a bare mention stays undetermined (the echo downgrade); nothing
    // at all is an honest observed `false`.
    let saw_change = if !has_revision {
        None
    } else {
        match best {
            Engagement::Engaged => Some(true),
            Engagement::Mention => None,
            Engagement::None => Some(false),
        }
    };
    Some(ReviewerScan {
        last_model,
        receipts: ReviewReceipts {
            saw_change,
            ran_checks: Some(ran_checks),
            turns: Some(turns),
        },
    })
}

/// The model id that actually spoke in a session: the LAST assistant message's `model`, falling
/// back to the session-level metadata model. Streamed under [`ParseOptions::lazy`] — large
/// content arrives as unresolved spans, so the pass costs one forward read of record metadata,
/// never a materialized transcript. (The adapter API has no tail read; this is the cheapest
/// faithful path.) Any parse failure is `None` — advisory callers degrade to "undetermined".
fn session_model(adapter: &dyn crate::harness::Adapter, sref: &crate::ir::SessionRef) -> Option<String> {
    let mut last: Option<String> = None;
    let mut sink = |m: crate::ir::Message| {
        if m.role == crate::ir::Role::Assistant {
            if let Some(model) = &m.model {
                last = Some(model.clone());
            }
        }
        crate::stream::Flow::Continue
    };
    let session = adapter
        .stream(sref, &crate::stream::ParseOptions::lazy(), &mut sink)
        .ok()?;
    last.or(session.model)
}

/// Human-readable advisory line for an independence result (shared CLI/MCP wording).
pub fn independence_warning(check: Option<&IndependenceCheck>) -> Option<String> {
    match check {
        None => Some(
            "no reviewer session given: reviewer independence not checked (recorded as unknown)"
                .to_string(),
        ),
        Some(c) => match c.independent {
            Some(true) => None,
            Some(false) => Some(format!(
                "SAME-FAMILY REVIEW: author and reviewer are both {} — this pass is not an independent verdict (recorded, not blocked)",
                c.reviewer_family.as_deref().unwrap_or("?")
            )),
            None => Some(format!(
                "independence undetermined (author {:?}, reviewer {:?}) — recorded, not blocked",
                c.author_family, c.reviewer_family
            )),
        },
    }
}

/// Human-readable advisory line for a receipts observation (shared CLI/MCP wording). `None`
/// receipts means no reviewer session was given; observed contact produces no warning at all —
/// but note "observed contact" is the gameable substring heuristic (see [`ReviewReceipts`]), so
/// the *absence* of a warning is a pre-Goodhart signal, not proof the reviewer read the change.
/// Always advisory: recorded, never a gate.
pub fn receipts_warning(receipts: Option<&ReviewReceipts>) -> Option<String> {
    match receipts {
        None => Some("no reviewer session given: review receipts not observed".to_string()),
        Some(r) => match r.saw_change {
            Some(true) => None,
            Some(false) => {
                Some("reviewer session shows no observable contact with the change — recorded, not blocked".to_string())
            }
            None => Some(
                "review receipts undetermined (reviewer session unreadable or revision context \
                 missing) — recorded, not blocked"
                    .to_string(),
            ),
        },
    }
}

/// Best-effort board notification for an appended task event (never called while the store lock
/// is held — the append has already returned). Failures are returned for the caller to warn
/// about, never to fail the operation. The body names the task's title (truncated) so
/// `cv board read tasks` reads as a timeline, not a list of ids.
pub fn notify_board(event: &TaskEvent, channel: &str, title: Option<&str>) -> anyhow::Result<()> {
    let short = &event.task_id[..event.task_id.len().min(8)];
    let body = match title {
        Some(t) => format!(
            "task {short}: {} — {}",
            event.kind.tag(),
            crate::ir::truncate(&crate::sanitize::sanitize_line(t), 80)
        ),
        None => format!("task {short}: {}", event.kind.tag()),
    };
    crate::board::post(
        channel,
        &event.by,
        &body,
        Some("task"),
        vec![format!("task:{}", event.task_id), format!("ev:{}", event.kind.tag())],
        Some(event.task_id.clone()),
    )?;
    Ok(())
}

/// The fleet identity convention (audit G4): the spawner sets `CV_ENDPOINT=agent:<pane>` in each
/// agent's environment, and cv front-ends derive their default `--from` here — the *bare*
/// command records the right actor. This is environment, not assertion: nothing verifies the
/// string (law 3, no authority machinery); it only stops a forgotten flag from silently
/// detaching an agent from its own inbox and misattributing the durable record forever.
pub fn default_endpoint() -> Option<String> {
    let v = std::env::var("CV_ENDPOINT").ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Identity resolution (G4) for verbs where the actor is bookkeeping, not semantics
/// (open/note/done/abandon/supersede, board chat): explicit actor wins, else the spawner-set
/// `CV_ENDPOINT`, else the surface's deliberate default sink — `"cv"` for CLI chat-grade verbs
/// (a human at a shell owes no ceremony), `"agent"` for MCP (the legacy anonymous sink).
pub fn actor(explicit: Option<String>, default_sink: &str) -> String {
    explicit
        .or_else(default_endpoint)
        .unwrap_or_else(|| default_sink.to_string())
}

/// The TOFU per-worker credential (audit: identity impersonation): the spawner mints a
/// `CV_TOKEN` per worker beside `CV_ENDPOINT`, and identity-bearing front-ends resolve it here —
/// explicit `--token`/`token` param wins, else the environment. `None` means "no token presented"
/// (the solo/human path); an endpoint that has bound a token must present the matching one, an
/// endpoint that never bound stays trusted. This is authentication of the `by` claim, not
/// authority — see [`identity`].
pub fn token(explicit: Option<String>) -> Option<String> {
    explicit
        .or_else(|| std::env::var("CV_TOKEN").ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Identity resolution for identity-BEARING verbs (claim/release/propose/pass/refute), whose
/// endpoint string keys inbox and reviewer semantics: no resolvable identity is an error naming
/// the cure, never a silent fall-through to a shared sink. `flag` is the surface's spelling of
/// the explicit parameter (`--from` for the CLI, `` `from` `` for MCP), so the error names the
/// exact knob the caller has.
pub fn require_actor(explicit: Option<String>, flag: &str) -> anyhow::Result<String> {
    explicit
        .or_else(default_endpoint)
        .ok_or_else(|| anyhow::anyhow!("set CV_ENDPOINT or pass {flag}; identity-bearing events must record who acted"))
}

/// What one agent-facing append produced: the durable event, the task's new effective state, and
/// the advisory warnings to surface.
#[derive(Debug)]
pub struct AppendOutcome {
    pub event: TaskEvent,
    /// `None` only if the appended event's task vanished from the replay (should not happen —
    /// the append CAS validated against it).
    pub effective_state: Option<String>,
    /// The caller's carried-in warnings plus a failed board notification, if any. This is the
    /// set MCP ships in its JSON reply.
    pub warnings: Vec<String>,
    /// Warnings from the post-append replay (log corruption etc.) — kept separate so surfaces
    /// that already showed the pre-append replay's warnings don't double-report.
    pub replay_warnings: Vec<String>,
}

/// The one agent-facing append path shared by the CLI and MCP: append the event through the
/// store's CAS, replay for the task's channel + new effective state, best-effort notify the
/// task's board channel (a notification failure is a warning, never an error — task state is
/// already durable).
pub fn append_and_notify(
    store: &TaskStore,
    task_id: Option<&str>,
    from: &str,
    kind: TaskEventKind,
    mut warnings: Vec<String>,
) -> anyhow::Result<AppendOutcome> {
    let event = store.append_agent_event(new_event(task_id, from, kind))?;
    let outcome = store.replay()?;
    let proj = outcome.model.tasks.get(&event.task_id);
    let channel = proj.map(|t| t.channel.clone()).unwrap_or_else(|| "tasks".into());
    if let Err(e) = notify_board(&event, &channel, proj.map(|t| t.title.as_str())) {
        warnings.push(format!("board notification failed (task state is durable): {e}"));
    }
    Ok(AppendOutcome {
        effective_state: proj.map(effective_display),
        event,
        warnings,
        replay_warnings: outcome.warnings,
    })
}

/// [`append_and_notify`] for several events by one actor, appended as ONE unit
/// ([`TaskStore::append_agent_events`]): all land or none does. Each event is notified on its
/// task's board channel. The outcome's `event` is the LAST event and `effective_state` is its
/// task's state after the batch; `events` holds all of them in append order.
pub fn append_batch_and_notify(
    store: &TaskStore,
    from: &str,
    kinds: Vec<(Option<String>, TaskEventKind)>,
    mut warnings: Vec<String>,
) -> anyhow::Result<BatchOutcome> {
    let candidates: Vec<TaskEvent> = kinds
        .into_iter()
        .map(|(task_id, kind)| new_event(task_id.as_deref(), from, kind))
        .collect();
    let events = store.append_agent_events(candidates)?;
    let outcome = store.replay()?;
    for event in &events {
        let proj = outcome.model.tasks.get(&event.task_id);
        let channel = proj.map(|t| t.channel.clone()).unwrap_or_else(|| "tasks".into());
        if let Err(e) = notify_board(event, &channel, proj.map(|t| t.title.as_str())) {
            warnings.push(format!("board notification failed (task state is durable): {e}"));
        }
    }
    let effective_state = events
        .last()
        .and_then(|e| outcome.model.tasks.get(&e.task_id))
        .map(effective_display);
    Ok(BatchOutcome {
        events,
        effective_state,
        warnings,
        replay_warnings: outcome.warnings,
    })
}

/// What one batch append produced (see [`append_batch_and_notify`]).
#[derive(Debug)]
pub struct BatchOutcome {
    pub events: Vec<TaskEvent>,
    /// The last event's task, after the batch.
    pub effective_state: Option<String>,
    pub warnings: Vec<String>,
    pub replay_warnings: Vec<String>,
}

use std::path::PathBuf;

/// Root dir for the task log: `$CLUSTERVISION_HOME/tasks` (or `~/.clustervision/tasks`).
pub fn tasks_dir() -> PathBuf {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".clustervision")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tasks")
}

#[cfg(test)]
mod tests {
    use super::default_endpoint;
    use super::*;

    /// All env cases live in ONE test: `set_var` is process-global and lib tests run in
    /// parallel threads, so sequencing inside a single test is the serialization.
    #[test]
    fn default_endpoint_reads_cv_endpoint_trimmed_nonempty() {
        std::env::remove_var("CV_ENDPOINT");
        assert_eq!(default_endpoint(), None, "unset → no identity");

        std::env::set_var("CV_ENDPOINT", "agent:demo");
        assert_eq!(default_endpoint().as_deref(), Some("agent:demo"));

        std::env::set_var("CV_ENDPOINT", "  agent:padded \n");
        assert_eq!(default_endpoint().as_deref(), Some("agent:padded"), "trimmed");

        std::env::set_var("CV_ENDPOINT", "   ");
        assert_eq!(default_endpoint(), None, "whitespace-only → no identity");

        std::env::set_var("CV_ENDPOINT", "");
        assert_eq!(default_endpoint(), None, "empty → no identity");

        std::env::remove_var("CV_ENDPOINT");
    }

    // ── reviewer receipts: observation over a constructed fake transcript ────────────────────

    /// A task with one proposed revision (branch `task/demo`, review sha `aaaa…`, repo
    /// `/repo/proj`) — the contact needles for the scan.
    fn task_with_revision() -> reduce::TaskProjection {
        let ts: chrono::DateTime<chrono::Utc> = "2026-07-16T12:00:00Z".parse().unwrap();
        let open = TaskEvent {
            id: "00000000-0000-7000-8000-000000000001".into(),
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            ts,
            by: "h".into(),
            kind: TaskEventKind::Opened {
                title: "t".into(),
                body: String::new(),
                repo: Some(std::path::PathBuf::from("/repo/proj")),
                issue: None,
                channel: "tasks".into(),
                assignee: None,
            },
        };
        let propose = TaskEvent {
            id: "00000000-0000-7000-8000-000000000002".into(),
            task_id: open.task_id.clone(),
            ts,
            by: "agent:author".into(),
            kind: TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/demo".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: "0".repeat(40),
                    review_sha: "a".repeat(40),
                    patch_id: "b".repeat(40),
                    reviewer: Some("agent:r".into()),
                    session_ref: None,
                },
            },
        };
        let model = reduce::TaskReducer::reduce(&[open, propose]).unwrap();
        model.tasks.values().next().unwrap().clone()
    }

    /// A task with no revision at all — contact is undeterminable.
    fn task_without_revision() -> reduce::TaskProjection {
        let mut t = task_with_revision();
        t.revisions.clear();
        t
    }

    /// Write a claude-format transcript whose assistant turns carry the given tool inputs;
    /// return a SessionRef pointing straight at it (no catalog/discovery involved).
    fn fake_session(tag: &str, tool_commands: &[&str]) -> crate::ir::SessionRef {
        let dir = std::env::temp_dir().join(format!("cv-receipts-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reviewsess.jsonl");
        let mut body = String::new();
        body.push_str(
            &serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "reviewsess",
                "timestamp": "2026-07-16T12:00:00Z", "cwd": "/repo/proj",
                "message": {"role": "user", "content": "please review the change"}
            })
            .to_string(),
        );
        body.push('\n');
        for (i, cmd) in tool_commands.iter().enumerate() {
            body.push_str(
                &serde_json::json!({
                    "type": "assistant", "uuid": format!("a{i}"), "sessionId": "reviewsess",
                    "timestamp": "2026-07-16T12:01:00Z",
                    "message": {"role": "assistant", "model": "claude-test-1", "content": [
                        {"type": "tool_use", "id": format!("t{i}"), "name": "Bash",
                         "input": {"command": cmd}},
                    ]}
                })
                .to_string(),
            );
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        crate::ir::SessionRef {
            id: "reviewsess".into(),
            harness: crate::ir::Harness::Claude,
            path,
            cwd: Some(std::path::PathBuf::from("/repo/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1 + tool_commands.len(),
        }
    }

    fn scan(sref: &crate::ir::SessionRef, t: &reduce::TaskProjection) -> ReviewerScan {
        let adapter = crate::harness::for_harness(crate::ir::Harness::Claude).unwrap();
        scan_session(&*adapter, sref, t).expect("fixture session should parse")
    }

    #[test]
    fn receipts_observe_contact_and_checks() {
        let t = task_with_revision();

        // Contact via branch name + a test run: both observed true.
        let sref = fake_session("contact", &["git diff main...task/demo", "cargo test -p demo"]);
        let s = scan(&sref, &t);
        assert_eq!(
            s.receipts,
            ReviewReceipts {
                saw_change: Some(true),
                ran_checks: Some(true),
                turns: Some(2)
            }
        );
        assert_eq!(s.last_model.as_deref(), Some("claude-test-1"));

        // Contact via the review sha's 12-prefix alone.
        let sref = fake_session("sha", &[&format!("git show {}", "a".repeat(12))]);
        assert_eq!(scan(&sref, &t).receipts.saw_change, Some(true));

        // Contact via the repo path alone (a file read under the repo).
        let sref = fake_session("repo", &["cat /repo/proj/src/lib.rs"]);
        assert_eq!(scan(&sref, &t).receipts.saw_change, Some(true));

        // No contact, no checks: observed FALSE (we looked, it isn't there) — never a guess.
        let sref = fake_session("nocontact", &["ls -la /somewhere/else"]);
        let s = scan(&sref, &t);
        assert_eq!(
            s.receipts,
            ReviewReceipts {
                saw_change: Some(false),
                ran_checks: Some(false),
                turns: Some(1)
            }
        );
    }

    #[test]
    fn receipts_undetermined_cases_are_none_not_false() {
        // No revision on the task → no needles → contact undeterminable (None), while checks
        // and turns are still honest observations.
        let sref = fake_session("norev", &["cargo test"]);
        let s = scan(&sref, &task_without_revision());
        assert_eq!(
            s.receipts,
            ReviewReceipts {
                saw_change: None,
                ran_checks: Some(true),
                turns: Some(1)
            }
        );

        // Unreadable transcript → no scan at all; callers record all-None receipts.
        let mut sref = fake_session("gone", &[]);
        sref.path = std::path::PathBuf::from("/nonexistent/reviewsess.jsonl");
        let adapter = crate::harness::for_harness(crate::ir::Harness::Claude).unwrap();
        assert!(scan_session(&*adapter, &sref, &task_with_revision()).is_none());
    }

    #[test]
    fn receipts_warning_wording() {
        assert_eq!(
            receipts_warning(None).as_deref(),
            Some("no reviewer session given: review receipts not observed")
        );
        let contact = ReviewReceipts {
            saw_change: Some(true),
            ran_checks: Some(false),
            turns: Some(3),
        };
        assert_eq!(receipts_warning(Some(&contact)), None, "contact observed → no warning");
        let no_contact = ReviewReceipts {
            saw_change: Some(false),
            ran_checks: None,
            turns: Some(1),
        };
        let w = receipts_warning(Some(&no_contact)).unwrap();
        assert!(w.contains("no observable contact") && w.contains("not blocked"), "{w}");
        let unknown = ReviewReceipts::default();
        let w = receipts_warning(Some(&unknown)).unwrap();
        assert!(w.contains("undetermined") && w.contains("not blocked"), "{w}");
    }
}
