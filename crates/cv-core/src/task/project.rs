//! Read-side projections over the [`TaskReadModel`]: filtered lists, per-assignee inboxes, and
//! the worktree-debt view. Pure functions over the model — no I/O.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{RevisionState, TaskState};
use super::reduce::{EffectiveState, TaskProjection, TaskReadModel};

/// Filter for [`list`]. Empty filter = all tasks.
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    /// Match against the *effective* state string (`open`, `claimed`, `ready`, `landed`, ...).
    pub state: Option<String>,
    pub assignee: Option<String>,
    pub repo: Option<PathBuf>,
    /// When false (default), terminal tasks (done/abandoned/superseded, landed included) are
    /// hidden unless `state` explicitly asks for them.
    pub include_terminal: bool,
    /// Only tasks carrying this tag (exact, case-sensitive — tags are stored as given).
    pub tag: Option<String>,
    /// Scope: keep a task only if its last event is at or after this instant, OR it involves
    /// `or_involving`'s endpoint (see [`involves`]). `None` = no time scope (every surface that
    /// predates scoping — MCP, HTTP — passes `None` and sees the whole store).
    pub touched_since: Option<DateTime<Utc>>,
    /// The endpoint whose own tasks escape the `touched_since` window.
    pub or_involving: Option<String>,
    /// Only decisions (`Some(true)`), only actions (`Some(false)`), or both (`None`).
    pub decisions: Option<bool>,
}

/// The effective-state vocabulary [`TaskFilter::state`] accepts: every base [`TaskState`] plus
/// every revision-layer [`super::model::RevisionState`] string, as [`list`] matches them.
pub const STATE_VOCABULARY: [&str; 11] = [
    "open",
    "claimed",
    "done",
    "abandoned",
    "superseded",
    "resolved",
    "awaiting_review",
    "ready",
    "merged_local",
    "landed",
    "refuted",
];

/// List tasks matching `filter`, oldest first (task ids are time-sortable uuid v7).
///
/// An unknown `state` string is an error naming the whole vocabulary, never a silent empty
/// result — a typo'd `--state redy` used to look exactly like "no matching tasks" on every
/// surface (CLI, MCP, HTTP), which all inherit this rejection.
pub fn list<'m>(model: &'m TaskReadModel, filter: &TaskFilter) -> Result<Vec<&'m TaskProjection>, String> {
    if let Some(state) = &filter.state {
        if !STATE_VOCABULARY.contains(&state.as_str()) {
            return Err(format!(
                "unknown state {state:?} (expected one of {})",
                STATE_VOCABULARY.join("|")
            ));
        }
    }
    Ok(model
        .tasks
        .values()
        .filter(|t| {
            if let Some(state) = &filter.state {
                if t.effective_state().as_str() != state {
                    return false;
                }
            } else if !filter.include_terminal && t.state.is_terminal() {
                return false;
            }
            if let Some(assignee) = &filter.assignee {
                if t.assignee.as_deref() != Some(assignee.as_str()) {
                    return false;
                }
            }
            if let Some(repo) = &filter.repo {
                if t.repo.as_deref() != Some(repo.as_path()) {
                    return false;
                }
            }
            if let Some(tag) = &filter.tag {
                if !t.tags.iter().any(|have| have == tag) {
                    return false;
                }
            }
            if let Some(want_decision) = filter.decisions {
                if t.is_decision() != want_decision {
                    return false;
                }
            }
            in_scope(t, filter.touched_since, filter.or_involving.as_deref())
        })
        .collect())
}

/// The scope rule every default listing applies: a task is in scope when its last event is at or
/// after `since`, or when `endpoint` is involved in it. `since = None` keeps everything.
pub fn in_scope(t: &TaskProjection, since: Option<DateTime<Utc>>, endpoint: Option<&str>) -> bool {
    match since {
        None => true,
        Some(since) => t.last_ts >= since || endpoint.is_some_and(|e| involves(t, e)),
    }
}

/// Is `endpoint` a party to this task: its opener, assignee, a note's author, a revision's
/// author or reviewer, or the decision's poser/resolver? `web:<name>` is `<name>` acting through
/// `cv task serve`, so it matches `<name>` too.
pub fn involves(t: &TaskProjection, endpoint: &str) -> bool {
    let is = |by: &str| same_actor(by, endpoint);
    is(&t.opened_by)
        || t.assignee.as_deref().is_some_and(is)
        || t.notes.iter().any(|n| is(&n.by))
        || t.revisions.iter().any(|r| {
            is(&r.proposed_by)
                || r.active_reviewer.as_deref().is_some_and(is)
                || r.pass.as_ref().is_some_and(|p| is(&p.reviewer))
                || r.refute.as_ref().is_some_and(|p| is(&p.reviewer))
        })
        || t.decision
            .as_ref()
            .is_some_and(|d| is(&d.posed_by) || d.resolution.as_ref().is_some_and(|r| is(&r.by)))
}

/// Two actor strings name the same party when they are equal, or when one is the other acting
/// through the web inbox (`web:ember` ≡ `ember`). Everything else is distinct — `lane:x` is not
/// `orchestrator:x`.
pub fn same_actor(a: &str, b: &str) -> bool {
    a == b || a.strip_prefix("web:") == Some(b) || b.strip_prefix("web:") == Some(a)
}

/// Parse a `--since` argument into an instant: a duration back from `now` (`30m`, `2h`, `3d`,
/// `1w`), a date (`2026-10-01`, local midnight), a local datetime (`2026-10-01T14:30` or with a
/// space), an RFC 3339 timestamp, or a UUID v7 event/task id (its embedded millisecond
/// timestamp — `cv task events` prints the last id it saw so a poller can hand it back as the
/// cursor; the instant is *exclusive* of that event when the caller compares `>`).
pub fn parse_since(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty --since".into());
    }
    if let Some(d) = parse_duration(s) {
        return Ok(now - d);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(t) = Local.from_local_datetime(&ndt).single() {
                return Ok(t.with_timezone(&Utc));
            }
        }
    }
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = nd.and_hms_opt(0, 0, 0).expect("midnight exists");
        if let Some(t) = Local.from_local_datetime(&ndt).single() {
            return Ok(t.with_timezone(&Utc));
        }
    }
    if let Some(t) = uuid_v7_timestamp(s) {
        return Ok(t);
    }
    Err(format!(
        "cannot read {s:?} as a duration (30m, 2h, 3d, 1w), a date (2026-10-01), a datetime \
         (2026-10-01T14:30), an RFC 3339 timestamp, or an event id"
    ))
}

/// `30m` / `2h` / `3d` / `1w` / `45s` → a duration. Whole numbers only.
pub fn parse_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: i64 = num.trim().parse().ok()?;
    if n < 0 {
        return None;
    }
    match unit {
        "s" => Some(chrono::Duration::seconds(n)),
        "m" => Some(chrono::Duration::minutes(n)),
        "h" => Some(chrono::Duration::hours(n)),
        "d" => Some(chrono::Duration::days(n)),
        "w" => Some(chrono::Duration::weeks(n)),
        _ => None,
    }
}

/// The millisecond timestamp a UUID v7 carries in its first 48 bits, if `s` is one (or a prefix
/// long enough to hold them: the first 12 hex digits).
pub fn uuid_v7_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let hex: String = s.chars().filter(|c| *c != '-').take(13).collect();
    if hex.len() < 13 || !hex.chars().all(|c| c.is_ascii_hexdigit()) || &hex[12..13] != "7" {
        return None;
    }
    let ms = i64::from_str_radix(&hex[..12], 16).ok()?;
    DateTime::from_timestamp_millis(ms)
}

/// Is `task` blocked *right now*: does any task it recorded a `blocked_by` on still sit in a
/// non-terminal base state? A blocker the log does not know (opened in another store, or a typo
/// that slipped past the front-end) counts as blocking — an unknown dependency is not a cleared
/// one. A blocker that finished, was abandoned or superseded no longer blocks; nothing has to be
/// appended to unblock.
pub fn is_blocked(model: &TaskReadModel, task: &TaskProjection) -> bool {
    task.blocked_by
        .iter()
        .any(|id| model.tasks.get(id).is_none_or(|b| !b.state.is_terminal()))
}

/// The tasks that recorded a `blocked_by` on `task_id` — the reverse relation (`blocks:` in
/// `cv task show`). Oldest first (task ids are time-sortable).
pub fn blocks<'m>(model: &'m TaskReadModel, task_id: &str) -> Vec<&'m TaskProjection> {
    model
        .tasks
        .values()
        .filter(|t| t.blocked_by.iter().any(|b| b == task_id))
        .collect()
}

/// The tag that marks a task as a *decision owed* by its assignee rather than work to do — the
/// inbox groups these first (`decisions owed`), because a decision nobody sees is the slowest
/// blocker a fleet has.
pub const DECISION_TAG: &str = "decision";
/// A decision the decider parked for discussion (`cv task discuss` / the page's "needs discussion"):
/// it stays open and resolvable, but it is no longer *owed* — the ball is with whoever posed it.
pub const DISCUSS_TAG: &str = "discuss";

/// The shortest id-prefix length (never below `min`, never above the full id) at which every id
/// in `ids` is distinguishable from every other. UUID v7 task ids open within the same second
/// share their first eight hex digits, so an 8-char prefix — fine for session ids — collides
/// across any batch of tasks opened together; every row renderer sizes its prefix with this.
pub fn unique_prefix_len<'a>(ids: impl IntoIterator<Item = &'a str>, min: usize) -> usize {
    let ids: Vec<&str> = ids.into_iter().collect();
    let longest = ids.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut len = min.min(longest.max(min));
    while len < longest {
        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        let distinct = ids.iter().all(|id| seen.insert(id.get(..len).unwrap_or(id)));
        if distinct {
            break;
        }
        len += 1;
    }
    len
}

/// Why a task appears in someone's inbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxReason {
    /// A live decision (a posed one, or a task tagged [`DECISION_TAG`]) assigned to you: someone
    /// is waiting on your call, not your work. Listed before everything else.
    DecisionOwed,
    /// A decision you parked with [`DISCUSS_TAG`]: open, still yours to resolve, but the next
    /// move is the poser's answer to your note — it is not counted as owed.
    Discussing,
    /// A decision made FOR you: the poser resolved it provisionally on its default, work went
    /// ahead, and your confirmation or veto is wanted. Resolved, so not counted as owed; listed
    /// right after the decisions you owe.
    Provisional,
    /// Open task assigned to you, not yet claimed.
    AssignedOpen,
    /// You claimed it; it is yours to finish.
    ClaimedByYou,
    /// A revision awaits your review verdict.
    AwaitingYourReview,
    /// Your reviewed revision is ready/merged-local but not observed landed — land it.
    YourUnlandedWork,
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxEntry<'m> {
    pub task: &'m TaskProjection,
    pub reason: InboxReason,
    /// When this entry started waiting — the aging anchor (G8): propose time for a review,
    /// pass time for unlanded work, the task's last event otherwise.
    pub since: DateTime<Utc>,
}

/// The per-agent "what needs me" view, **oldest first** (stalest at the top — age is the
/// escalation mechanism, so the longest-waiting obligation is the first line an agent reads).
pub fn inbox<'m>(model: &'m TaskReadModel, endpoint: &str) -> Vec<InboxEntry<'m>> {
    let mut entries = Vec::new();
    for task in model.tasks.values() {
        if task.state.is_terminal() {
            // The one terminal row an inbox carries: a provisional resolution awaiting its
            // decider's veto. It ages from the provisional resolve.
            if let Some(r) = task
                .decision
                .as_ref()
                .filter(|d| d.awaiting_veto())
                .and_then(|d| d.resolution.as_ref())
            {
                if task.assignee.as_deref().is_some_and(|a| same_actor(a, endpoint)) {
                    entries.push(InboxEntry {
                        task,
                        reason: InboxReason::Provisional,
                        since: r.ts,
                    });
                }
            }
            continue;
        }
        let reason_since = if let Some(rev) = task.current_revision() {
            match rev.state {
                RevisionState::AwaitingReview if rev.active_reviewer.as_deref() == Some(endpoint) => {
                    Some((InboxReason::AwaitingYourReview, rev.proposed_at))
                }
                RevisionState::Ready | RevisionState::MergedLocal
                    if task.assignee.as_deref() == Some(endpoint) || task.opened_by == endpoint =>
                {
                    let since = rev.pass.as_ref().map(|p| p.ts).unwrap_or(task.last_ts);
                    Some((InboxReason::YourUnlandedWork, since))
                }
                _ => base_inbox_reason(task, endpoint),
            }
        } else {
            base_inbox_reason(task, endpoint)
        };
        if let Some((reason, since)) = reason_since {
            entries.push(InboxEntry { task, reason, since });
        }
    }
    entries.sort_by_key(|e| e.since);
    entries
}

fn base_inbox_reason(task: &TaskProjection, endpoint: &str) -> Option<(InboxReason, DateTime<Utc>)> {
    if task.assignee.as_deref() == Some(endpoint)
        && !task.state.is_terminal()
        && (task.is_decision() || task.tags.iter().any(|t| t == DECISION_TAG))
    {
        if task.tags.iter().any(|t| t == DISCUSS_TAG) {
            // Parked: ages from the discussion request (its last event), not from the pose.
            return Some((InboxReason::Discussing, task.last_ts));
        }
        // A posed decision ages from when it was asked, not from the last note on it.
        let since = task.decision.as_ref().map(|d| d.posed_at).unwrap_or(task.last_ts);
        return Some((InboxReason::DecisionOwed, since));
    }
    match task.state {
        TaskState::Open if task.assignee.as_deref() == Some(endpoint) => {
            Some((InboxReason::AssignedOpen, task.last_ts))
        }
        TaskState::Claimed if task.assignee.as_deref() == Some(endpoint) => {
            Some((InboxReason::ClaimedByYou, task.last_ts))
        }
        _ => None,
    }
}

/// One row of the worktree-debt view: reviewed work not observed on its upstream.
#[derive(Clone, Debug, Serialize)]
pub struct DebtEntry<'m> {
    pub task: &'m TaskProjection,
    pub revision_n: u32,
    pub branch: String,
    pub upstream: String,
    pub state: RevisionState,
    /// When the revision reached its current unlanded-but-reviewed state (pass time), for aging.
    pub since: DateTime<Utc>,
    pub issues: Vec<String>,
}

/// Unlanded reviewed work, grouped by repo (`None` key = tasks with no repo recorded), oldest
/// first within each group. This is the "unlanded work must be loudly visible" surface.
pub fn debt(model: &TaskReadModel) -> BTreeMap<Option<PathBuf>, Vec<DebtEntry<'_>>> {
    let mut groups: BTreeMap<Option<PathBuf>, Vec<DebtEntry<'_>>> = BTreeMap::new();
    for task in model.tasks.values() {
        if task.state.is_terminal() {
            continue;
        }
        let Some(rev) = task.current_revision() else { continue };
        if !matches!(rev.state, RevisionState::Ready | RevisionState::MergedLocal) {
            continue;
        }
        let since = rev.pass.as_ref().map(|p| p.ts).unwrap_or(task.last_ts);
        groups.entry(task.repo.clone()).or_default().push(DebtEntry {
            task,
            revision_n: rev.revision.n,
            branch: rev.revision.branch.clone(),
            upstream: rev.revision.upstream.clone(),
            state: rev.state,
            since,
            issues: rev.issues.iter().map(|i| i.describe()).collect(),
        });
    }
    for entries in groups.values_mut() {
        entries.sort_by_key(|e| e.since);
    }
    groups
}

/// One aged awaiting-review row: a proposed revision whose reviewer has not spoken. The sibling
/// of the debt view (G8) — a dead reviewer is honest state that nobody sees unless it ages on a
/// surface the owner reads.
#[derive(Clone, Debug, Serialize)]
pub struct AwaitingReviewEntry<'m> {
    pub task: &'m TaskProjection,
    pub revision_n: u32,
    pub branch: String,
    /// The only endpoint whose verdict can advance the revision (None until a verdict binds).
    pub reviewer: Option<String>,
    /// Propose time — age since the review was requested.
    pub since: DateTime<Utc>,
}

/// Revisions currently awaiting review, oldest first. Pure projection: it renders age, it never
/// escalates.
pub fn awaiting_review(model: &TaskReadModel) -> Vec<AwaitingReviewEntry<'_>> {
    let mut rows: Vec<AwaitingReviewEntry<'_>> = model
        .tasks
        .values()
        .filter(|t| !t.state.is_terminal())
        .filter_map(|task| {
            let rev = task.current_revision()?;
            (rev.state == RevisionState::AwaitingReview).then(|| AwaitingReviewEntry {
                task,
                revision_n: rev.revision.n,
                branch: rev.revision.branch.clone(),
                reviewer: rev.active_reviewer.clone(),
                since: rev.proposed_at,
            })
        })
        .collect();
    rows.sort_by_key(|e| e.since);
    rows
}

/// Compact human age for terminal rows: `42s`, `12m`, `5h`, `3d`. Truncating division at each
/// unit boundary; future timestamps (clock skew) saturate to `0s`, never a negative.
pub fn age_short(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(since).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Non-terminal tasks in `repo` whose CURRENT revision carries `branch` (or, when `worktree` is
/// given, the same worktree path). Pure scan for the propose-time collision warning: two live
/// tasks pointing at one branch usually means two agents about to trample each other.
pub fn branch_carriers<'m>(
    model: &'m TaskReadModel,
    repo: &Path,
    branch: &str,
    worktree: Option<&Path>,
) -> Vec<&'m TaskProjection> {
    model
        .tasks
        .values()
        .filter(|t| !t.state.is_terminal())
        .filter(|t| t.repo.as_deref() == Some(repo))
        .filter(|t| {
            t.current_revision().is_some_and(|r| {
                r.revision.branch == branch || (worktree.is_some() && r.revision.worktree.as_deref() == worktree)
            })
        })
        .collect()
}

/// Advisory propose-time collision warnings (never a block): one line per OTHER non-terminal
/// task whose current revision already carries this revision's branch or worktree in `repo`.
/// Shared by the CLI and MCP propose paths; callers sanitize at render.
pub fn propose_collision_warnings(
    model: &TaskReadModel,
    task_id: &str,
    repo: &Path,
    revision: &super::model::Revision,
) -> Vec<String> {
    branch_carriers(model, repo, &revision.branch, revision.worktree.as_deref())
        .into_iter()
        .filter(|t| t.task_id != task_id)
        .map(|t| {
            format!(
                "branch {} is already carried by task {} ({}) — two tasks proposing one branch \
                 usually means a collision",
                revision.branch,
                &t.task_id[..t.task_id.len().min(8)],
                t.title
            )
        })
        .collect()
}

/// Resolve a task-id prefix (short id) to the unique matching task id.
pub fn resolve_id<'m>(model: &'m TaskReadModel, prefix: &str) -> Result<&'m str, String> {
    let matches: Vec<&str> = model
        .tasks
        .keys()
        .filter(|id| id.starts_with(prefix))
        .map(|s| s.as_str())
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no task matches '{prefix}'")),
        many => Err(format!(
            "'{prefix}' is ambiguous ({} matches: {} ...)",
            many.len(),
            many[..many.len().min(3)].join(", ")
        )),
    }
}

/// Effective-state helper for callers that need a display string with the layer visible.
pub fn effective_display(task: &TaskProjection) -> String {
    match task.effective_state() {
        EffectiveState::Base(s) => s.as_str().to_string(),
        EffectiveState::Revision(s) => format!("rev:{}", s.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{Revision, TaskEvent, TaskEventKind};
    use crate::task::reduce::TaskReducer;

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    struct Log {
        events: Vec<TaskEvent>,
        n: u64,
    }

    impl Log {
        fn new() -> Self {
            Log {
                events: Vec::new(),
                n: 0,
            }
        }
        fn next_id(&mut self) -> String {
            self.n += 1;
            format!("00000000-0000-7000-8000-{:012}", self.n)
        }
        fn push(&mut self, task_id: &str, by: &str, kind: TaskEventKind) {
            self.push_at(task_id, by, "2026-07-16T12:00:00Z", kind);
        }
        fn push_at(&mut self, task_id: &str, by: &str, ts: &str, kind: TaskEventKind) {
            let id = self.next_id();
            self.events.push(TaskEvent {
                id,
                task_id: task_id.to_string(),
                ts: ts.parse().unwrap(),
                by: by.to_string(),
                kind,
            });
        }
        fn open(&mut self, by: &str, repo: Option<&str>, assignee: Option<&str>) -> String {
            let id = self.next_id();
            self.events.push(TaskEvent {
                id: id.clone(),
                task_id: id.clone(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: by.to_string(),
                kind: TaskEventKind::Opened {
                    title: "t".into(),
                    body: String::new(),
                    repo: repo.map(PathBuf::from),
                    issue: None,
                    channel: "tasks".into(),
                    assignee: assignee.map(String::from),
                },
            });
            id
        }
        fn model(&self) -> TaskReadModel {
            TaskReducer::reduce(&self.events).unwrap()
        }
    }

    fn propose_and_pass(log: &mut Log, task: &str) {
        log.push(
            task,
            "agent:author",
            TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/x".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: sha('0'),
                    review_sha: sha('1'),
                    patch_id: sha('b'),
                    reviewer: Some("agent:reviewer".into()),
                    session_ref: None,
                },
            },
        );
        log.push(
            task,
            "agent:reviewer",
            TaskEventKind::ReviewPassed {
                reviewer: "agent:reviewer".into(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
        );
    }

    #[test]
    fn list_filters_by_effective_state_and_hides_terminal_by_default() {
        let mut log = Log::new();
        let open = log.open("h", None, None);
        let done = log.open("h", None, None);
        log.push(&done, "a", TaskEventKind::Claimed { assignee: "a".into() });
        log.push(
            &done,
            "a",
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        let ready = log.open("h", Some("/tmp/repo"), None);
        log.push(
            &ready,
            "agent:author",
            TaskEventKind::Claimed {
                assignee: "agent:author".into(),
            },
        );
        propose_and_pass(&mut log, &ready);

        let model = log.model();
        let all = list(&model, &TaskFilter::default()).unwrap();
        assert_eq!(all.len(), 2, "done task hidden by default");
        assert!(all.iter().any(|t| t.task_id == open));

        let ready_only = list(
            &model,
            &TaskFilter {
                state: Some("ready".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ready_only.len(), 1);
        assert_eq!(ready_only[0].task_id, ready);

        let done_only = list(
            &model,
            &TaskFilter {
                state: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(done_only.len(), 1);
        assert_eq!(done_only[0].task_id, done);
    }

    #[test]
    fn inbox_covers_all_four_reasons() {
        let mut log = Log::new();
        let assigned = log.open("human", None, Some("agent:a"));
        let claimed = log.open("human", None, None);
        log.push(
            &claimed,
            "agent:a",
            TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        );
        let review = log.open("human", Some("/tmp/r"), None);
        log.push(
            &review,
            "agent:a",
            TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        );
        log.push(
            &review,
            "agent:a",
            TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/r".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: sha('7'),
                    review_sha: sha('2'),
                    patch_id: sha('c'),
                    reviewer: Some("agent:b".into()),
                    session_ref: None,
                },
            },
        );
        let unlanded = log.open("human", Some("/tmp/r"), None);
        log.push(
            &unlanded,
            "agent:a",
            TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        );
        propose_and_pass(&mut log, &unlanded);

        let model = log.model();

        let a: Vec<_> = inbox(&model, "agent:a");
        let reasons: Vec<(String, InboxReason)> = a.iter().map(|e| (e.task.task_id.clone(), e.reason)).collect();
        assert!(reasons.contains(&(assigned.clone(), InboxReason::AssignedOpen)));
        assert!(reasons.contains(&(claimed.clone(), InboxReason::ClaimedByYou)));
        assert!(reasons.contains(&(unlanded.clone(), InboxReason::YourUnlandedWork)));
        // The review task is claimed by a, but its revision awaits b — for a it shows as claimed.
        assert!(reasons.contains(&(review.clone(), InboxReason::ClaimedByYou)));

        let b: Vec<_> = inbox(&model, "agent:b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].reason, InboxReason::AwaitingYourReview);
        assert_eq!(b[0].task.task_id, review);
    }

    #[test]
    fn debt_lists_unlanded_reviewed_work_by_repo() {
        let mut log = Log::new();
        let landed = log.open("h", Some("/tmp/r1"), None);
        propose_and_pass(&mut log, &landed);
        log.push(
            &landed,
            "verifier",
            TaskEventKind::Landed {
                upstream_head: sha('f'),
                observed_patch_id: sha('b'),
            },
        );
        let owed = log.open("h", Some("/tmp/r1"), None);
        propose_and_pass(&mut log, &owed);
        let owed2 = log.open("h", Some("/tmp/r2"), None);
        propose_and_pass(&mut log, &owed2);
        log.push(
            &owed2,
            "verifier",
            TaskEventKind::MergedLocal {
                from_sha: sha('e'),
                to_sha: sha('1'),
            },
        );

        let model = log.model();
        let groups = debt(&model);
        assert_eq!(groups.len(), 2);
        let r1 = &groups[&Some(PathBuf::from("/tmp/r1"))];
        assert_eq!(r1.len(), 1, "landed task owes nothing");
        assert_eq!(r1[0].task.task_id, owed);
        assert_eq!(r1[0].state, RevisionState::Ready);
        let r2 = &groups[&Some(PathBuf::from("/tmp/r2"))];
        assert_eq!(r2[0].state, RevisionState::MergedLocal);
    }

    fn revision(n: u32, reviewer: Option<&str>) -> Revision {
        Revision {
            n,
            branch: format!("task/x{n}"),
            worktree: None,
            upstream: "origin/main".into(),
            base: sha('0'),
            review_sha: sha('1'),
            patch_id: sha('b'),
            reviewer: reviewer.map(String::from),
            session_ref: None,
        }
    }

    #[test]
    fn age_short_boundaries() {
        let t0: chrono::DateTime<chrono::Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
        assert_eq!(age_short(t0, t0), "0s");
        assert_eq!(age_short(t0, at(59)), "59s");
        assert_eq!(age_short(t0, at(60)), "1m");
        assert_eq!(age_short(t0, at(3_599)), "59m");
        assert_eq!(age_short(t0, at(3_600)), "1h");
        assert_eq!(age_short(t0, at(86_399)), "23h");
        assert_eq!(age_short(t0, at(86_400)), "1d");
        assert_eq!(age_short(t0, at(3 * 86_400 + 7_200)), "3d");
        // Future timestamp (clock skew) saturates, never renders a negative.
        assert_eq!(age_short(at(60), t0), "0s");
    }

    #[test]
    fn awaiting_review_ages_since_propose_and_clears_on_verdict() {
        let mut log = Log::new();
        let old = log.open("h", Some("/tmp/r"), None);
        log.push_at(
            &old,
            "agent:author",
            "2026-07-13T12:00:00Z",
            TaskEventKind::RevisionProposed {
                revision: revision(1, Some("agent:reviewer")),
            },
        );
        let newer = log.open("h", Some("/tmp/r"), None);
        log.push_at(
            &newer,
            "agent:author",
            "2026-07-16T09:00:00Z",
            TaskEventKind::RevisionProposed {
                revision: revision(1, None),
            },
        );

        let model = log.model();
        let rows = awaiting_review(&model);
        assert_eq!(rows.len(), 2);
        // Oldest first, propose time is the anchor, reviewer endpoint rides along.
        assert_eq!(rows[0].task.task_id, old);
        assert_eq!(rows[0].since.to_rfc3339(), "2026-07-13T12:00:00+00:00");
        assert_eq!(rows[0].reviewer.as_deref(), Some("agent:reviewer"));
        assert_eq!(rows[1].task.task_id, newer);
        assert_eq!(rows[1].reviewer, None);

        // A pass clears the entry (it moves to the debt view instead) ...
        log.push(
            &old,
            "agent:reviewer",
            TaskEventKind::ReviewPassed {
                reviewer: "agent:reviewer".into(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
        );
        // ... a refute clears the other.
        log.push(
            &newer,
            "agent:b",
            TaskEventKind::ReviewRefuted {
                reviewer: "agent:b".into(),
                session_ref: None,
                receipts: None,
            },
        );
        let model = log.model();
        assert!(awaiting_review(&model).is_empty());
    }

    #[test]
    fn awaiting_review_follows_supersede() {
        let mut log = Log::new();
        let t = log.open("h", Some("/tmp/r"), None);
        log.push_at(
            &t,
            "agent:author",
            "2026-07-10T00:00:00Z",
            TaskEventKind::RevisionProposed {
                revision: revision(1, Some("agent:reviewer")),
            },
        );
        // Re-propose: rev1 is auto-superseded, only rev2 should age.
        log.push_at(
            &t,
            "agent:author",
            "2026-07-15T00:00:00Z",
            TaskEventKind::RevisionProposed {
                revision: revision(2, Some("agent:reviewer")),
            },
        );
        let model = log.model();
        let rows = awaiting_review(&model);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].revision_n, 2);
        assert_eq!(rows[0].since.to_rfc3339(), "2026-07-15T00:00:00+00:00");
    }

    #[test]
    fn inbox_is_oldest_first_with_waiting_anchors() {
        let mut log = Log::new();
        // Reviewed-unlanded work: `since` must be the PASS time, not the propose time.
        let unlanded = log.open("h", Some("/tmp/r"), None);
        log.push(
            &unlanded,
            "agent:a",
            TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        );
        log.push_at(
            &unlanded,
            "agent:a",
            "2026-07-14T00:00:00Z",
            TaskEventKind::RevisionProposed {
                revision: revision(1, Some("agent:r")),
            },
        );
        log.push_at(
            &unlanded,
            "agent:r",
            "2026-07-15T00:00:00Z",
            TaskEventKind::ReviewPassed {
                reviewer: "agent:r".into(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
        );
        // A review awaiting agent:a, proposed EARLIER than the pass above — sorts first.
        let review = log.open("h", Some("/tmp/r"), None);
        log.push_at(
            &review,
            "agent:b",
            "2026-07-12T00:00:00Z",
            TaskEventKind::RevisionProposed {
                revision: revision(1, Some("agent:a")),
            },
        );

        let model = log.model();
        let entries = inbox(&model, "agent:a");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].task.task_id, review, "stalest at top");
        assert_eq!(entries[0].reason, InboxReason::AwaitingYourReview);
        assert_eq!(entries[0].since.to_rfc3339(), "2026-07-12T00:00:00+00:00");
        assert_eq!(entries[1].task.task_id, unlanded);
        assert_eq!(entries[1].reason, InboxReason::YourUnlandedWork);
        assert_eq!(
            entries[1].since.to_rfc3339(),
            "2026-07-15T00:00:00+00:00",
            "unlanded work ages from the pass, not the propose"
        );
    }

    #[test]
    fn branch_carriers_flags_live_same_branch_same_repo_only() {
        let mut log = Log::new();
        let rev_on = |branch: &str, worktree: Option<&str>| Revision {
            n: 1,
            branch: branch.into(),
            worktree: worktree.map(PathBuf::from),
            upstream: "origin/main".into(),
            base: sha('0'),
            review_sha: sha('1'),
            patch_id: sha('b'),
            reviewer: None,
            session_ref: None,
        };
        let propose = |log: &mut Log, task: &str, branch: &str, wt: Option<&str>| {
            log.push(
                task,
                "agent:author",
                TaskEventKind::RevisionProposed {
                    revision: rev_on(branch, wt),
                },
            );
        };

        let carrier = log.open("h", Some("/tmp/r"), None);
        propose(&mut log, &carrier, "task/x", Some("/wt/x"));
        // Same branch but a DIFFERENT repo: not a collision.
        let other_repo = log.open("h", Some("/tmp/other"), None);
        propose(&mut log, &other_repo, "task/x", None);
        // Same repo, different branch, same WORKTREE: still a collision.
        let wt_clash = log.open("h", Some("/tmp/r"), None);
        propose(&mut log, &wt_clash, "task/y", Some("/wt/x"));
        // Same branch, same repo, but the task is terminal: not a collision.
        let dead = log.open("h", Some("/tmp/r"), None);
        propose(&mut log, &dead, "task/x", None);
        log.push(&dead, "h", TaskEventKind::Abandoned { reason: "gone".into() });

        let model = log.model();
        let repo = PathBuf::from("/tmp/r");
        let by_branch = branch_carriers(&model, &repo, "task/x", None);
        assert_eq!(by_branch.len(), 1);
        assert_eq!(by_branch[0].task_id, carrier);

        let by_worktree = branch_carriers(&model, &repo, "task/z", Some(Path::new("/wt/x")));
        let ids: Vec<&str> = by_worktree.iter().map(|t| t.task_id.as_str()).collect();
        assert!(
            ids.contains(&carrier.as_str()) && ids.contains(&wt_clash.as_str()),
            "{ids:?}"
        );

        // The warning builder excludes the proposing task itself and names the carrier.
        let proposing = log.open("h", Some("/tmp/r"), None);
        let model = log.model();
        let w = propose_collision_warnings(&model, &proposing, &repo, &rev_on("task/x", None));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("task/x") && w[0].contains(&carrier[..8]), "{}", w[0]);
        // The carrier proposing on ITS OWN branch again warns about nobody.
        let w = propose_collision_warnings(&model, &carrier, &repo, &rev_on("task/x", None));
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn resolve_id_prefixes() {
        let mut log = Log::new();
        let a = log.open("h", None, None);
        let model = log.model();
        assert_eq!(resolve_id(&model, &a[..8]).unwrap(), a);
        assert!(resolve_id(&model, "ffffffff").is_err());
    }
}
