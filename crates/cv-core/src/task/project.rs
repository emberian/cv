//! Read-side projections over the [`TaskReadModel`]: filtered lists and per-assignee inboxes.
//! Pure functions over the model — no I/O.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::TaskState;
use super::reduce::{TaskProjection, TaskReadModel};

/// Filter for [`list`]. Empty filter = all tasks.
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    /// Match against the state string (`open`, `claimed`, `resolved`, `done`, ...).
    pub state: Option<String>,
    pub assignee: Option<String>,
    pub repo: Option<PathBuf>,
    /// When false (default), terminal tasks (done/abandoned/superseded/resolved) are hidden
    /// unless `state` explicitly asks for them.
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

/// The state vocabulary [`TaskFilter::state`] accepts: every [`TaskState`]'s spelling, in
/// [`TaskState::ALL`] order.
pub const STATE_VOCABULARY: [&str; 6] = {
    let mut words = [""; 6];
    let mut i = 0;
    while i < TaskState::ALL.len() {
        words[i] = TaskState::ALL[i].as_str();
        i += 1;
    }
    words
};

/// List tasks matching `filter`, oldest first (task ids are time-sortable uuid v7).
///
/// An unknown `state` string is an error naming the whole vocabulary, never a silent empty
/// result — a typo'd `--state redy` used to look exactly like "no matching tasks" on every
/// surface (CLI, MCP, HTTP), which all inherit this rejection.
pub fn list<'m>(model: &'m TaskReadModel, filter: &TaskFilter) -> Result<Vec<&'m TaskProjection>, String> {
    let state = filter.state.as_deref().map(str::parse::<TaskState>).transpose()?;
    Ok(model
        .tasks
        .values()
        .filter(|t| {
            if let Some(state) = state {
                if t.state != state {
                    return false;
                }
            } else if !filter.include_terminal && t.state.is_terminal() {
                return false;
            }
            if let Some(assignee) = &filter.assignee {
                if !t.assignee.as_deref().is_some_and(|have| same_actor(have, assignee)) {
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

/// Is `endpoint` a party to this task: its opener, assignee, a note's author, or the decision's
/// poser/resolver? `web:<name>` is `<name>` acting through `cv serve`'s inbox, so it matches
/// `<name>` too.
pub fn involves(t: &TaskProjection, endpoint: &str) -> bool {
    let is = |by: &str| same_actor(by, endpoint);
    is(&t.opened_by)
        || t.assignee.as_deref().is_some_and(is)
        || t.notes.iter().any(|n| is(&n.by))
        || t.decision
            .as_ref()
            .is_some_and(|d| is(&d.posed_by) || d.resolution.as_ref().is_some_and(|r| is(&r.by)))
}

/// Two actor strings name the same party when they are equal, or when one is the other acting
/// through the web inbox (`web:ember` ≡ `ember`). Everything else is distinct — `lane:x` is not
/// `orchestrator:x`.
pub fn same_actor(a: &str, b: &str) -> bool {
    canonical_actor(a) == canonical_actor(b)
}

/// The endpoint an actor string stands for: `web:ember` acts as `ember`. This is the ONE place the
/// alias is defined; the reducer's holder checks ([`same_actor`]) resolve through it.
pub fn canonical_actor(a: &str) -> &str {
    a.strip_prefix("web:").unwrap_or(a)
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
        return now
            .checked_sub_signed(d)
            .ok_or_else(|| format!("duration {s:?} is outside the supported date range"));
    }
    if let Some(t) = parse_stamp(s) {
        return Ok(t);
    }
    use chrono::{Local, NaiveDate, TimeZone};
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

/// An RFC 3339 timestamp or one of the four local datetime spellings, as UTC.
fn parse_stamp(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    use chrono::{Local, NaiveDateTime, TimeZone};
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(t) = Local.from_local_datetime(&ndt).single() {
                return Some(t.with_timezone(&Utc));
            }
        }
    }
    None
}

/// Parse a `--by` deadline: a duration from `now` (`3d`, `48h`, `1w`), a bare date (`2026-10-03`
/// = the end of that local day), a local datetime (`2026-10-03T18:00`), or RFC 3339.
pub fn parse_deadline(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if let Some(d) = parse_duration(s) {
        return now
            .checked_add_signed(d)
            .ok_or_else(|| format!("deadline {s:?} is outside the supported date range"));
    }
    if let Some(t) = parse_stamp(s) {
        return Ok(t);
    }
    use chrono::{Local, NaiveDate, TimeZone};
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = nd.and_hms_opt(23, 59, 59).expect("end of day exists");
        if let Some(t) = Local.from_local_datetime(&ndt).single() {
            return Ok(t.with_timezone(&Utc));
        }
    }
    Err(format!(
        "cannot read {s:?} as a deadline: a duration (3d, 48h, 1w), a date (2026-10-03), a datetime \
         (2026-10-03T18:00) or RFC 3339"
    ))
}

/// `30m` / `2h` / `3d` / `1w` / `45s` → a duration. Whole numbers only.
pub fn parse_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at_checked(s.len().checked_sub(1)?)?;
    let n: i64 = num.trim().parse().ok()?;
    if n < 0 {
        return None;
    }
    match unit {
        "s" => chrono::Duration::try_seconds(n),
        "m" => chrono::Duration::try_minutes(n),
        "h" => chrono::Duration::try_hours(n),
        "d" => chrono::Duration::try_days(n),
        "w" => chrono::Duration::try_weeks(n),
        _ => None,
    }
}

/// The millisecond timestamp a UUID v7 carries in its first 48 bits, if `s` is one (or a prefix
/// long enough to hold them plus the version nibble: at least 13 hex digits, ignoring hyphens).
/// Shorter task-id prefixes can select tasks but cannot supply a --since timestamp.
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
    task.blocked_by.iter().any(|id| super::stall::waiting(model, task, id))
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

/// The tag that marked a task as a decision before `posed` existed (0.15.0). Writing it to a task
/// with nothing posed is refused ([`super::ReduceError::BareDecisionTag`]): a decision is posed,
/// with options, never tagged.
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
    /// A live posed decision assigned to you: someone
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
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxEntry<'m> {
    pub task: &'m TaskProjection,
    pub reason: InboxReason,
    /// When this entry started waiting — the aging anchor (G8): the pose for a decision, the
    /// provisional resolve for a veto window, the task's last event otherwise.
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
        if let Some((reason, since)) = base_inbox_reason(task, endpoint) {
            entries.push(InboxEntry { task, reason, since });
        }
    }
    entries.sort_by_key(|e| e.since);
    entries
}

fn base_inbox_reason(task: &TaskProjection, endpoint: &str) -> Option<(InboxReason, DateTime<Utc>)> {
    let assigned = task.assignee.as_deref().is_some_and(|have| same_actor(have, endpoint));
    if assigned && !task.state.is_terminal() && task.is_decision() {
        if task.tags.iter().any(|t| t == DISCUSS_TAG) {
            // Parked: ages from the discussion request (its last event), not from the pose.
            return Some((InboxReason::Discussing, task.last_ts));
        }
        // A posed decision ages from when it was asked, not from the last note on it.
        let since = task.decision.as_ref().map_or(task.last_ts, |d| d.posed_at);
        return Some((InboxReason::DecisionOwed, since));
    }
    match task.state {
        TaskState::Open if assigned => Some((InboxReason::AssignedOpen, task.last_ts)),
        TaskState::Claimed if assigned => Some((InboxReason::ClaimedByYou, task.last_ts)),
        _ => None,
    }
}

/// Compact human age for inbox items, deadline phrases and terminal rows: `42s`, `12m`, `5h`, `3d`. Truncating division at each
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

/// Resolve a task-id prefix (short id) to the unique matching task id.
pub fn resolve_id<'m>(model: &'m TaskReadModel, prefix: &str) -> Result<&'m str, String> {
    use std::ops::Bound;
    // Prefix matches form one contiguous run in the task map. Seek that run and retain only
    // the diagnostic's first three ids, while still counting every match for the same error.
    let mut first = Vec::with_capacity(3);
    let mut count = 0usize;
    for (id, _) in model.tasks.range::<str, _>((Bound::Included(prefix), Bound::Unbounded)) {
        if !id.starts_with(prefix) {
            break;
        }
        count += 1;
        if first.len() < 3 {
            first.push(id.as_str());
        }
    }
    match count {
        0 => Err(format!("no task matches '{prefix}'")),
        1 => Ok(first[0]),
        _ => Err(format!(
            "'{prefix}' is ambiguous ({count} matches: {} ...)",
            first.join(", ")
        )),
    }
}

/// The task's state as the string every surface prints and filters on.
pub fn effective_display(task: &TaskProjection) -> String {
    task.state.as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{TaskEvent, TaskEventKind};
    use crate::task::reduce::TaskReducer;

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
                    channel: None,
                    assignee: assignee.map(String::from),
                },
            });
            id
        }
        fn model(&self) -> TaskReadModel {
            TaskReducer::reduce(&self.events).unwrap()
        }
    }

    #[test]
    fn ordered_prefix_resolution_preserves_exact_ambiguity_and_missing_diagnostics() {
        let mut log = Log::new();
        let task = log.open("author", None, None);
        let template = log.model().tasks[&task].clone();
        let mut model = TaskReadModel::default();
        for id in ["aaa", "abc", "abc-1", "abc-2", "abc-3", "abc-4", "b", "éa", "éb", "z"] {
            let mut task = template.clone();
            task.task_id = id.into();
            model.tasks.insert(id.into(), task);
        }
        let oracle = |prefix: &str| -> Result<&str, String> {
            let ids: Vec<_> = model
                .tasks
                .keys()
                .filter(|id| id.starts_with(prefix))
                .map(String::as_str)
                .collect();
            match ids.as_slice() {
                [] => Err(format!("no task matches '{prefix}'")),
                [id] => Ok(*id),
                _ => Err(format!(
                    "'{prefix}' is ambiguous ({} matches: {} ...)",
                    ids.len(),
                    ids[..ids.len().min(3)].join(", ")
                )),
            }
        };
        for prefix in [
            "", "a", "aaa", "ab", "abc", "abc-", "abc-3", "abcd", "b", "c", "é", "éa", "ê", "zz",
        ] {
            assert_eq!(resolve_id(&model, prefix), oracle(prefix), "{prefix}");
        }
        assert_eq!(
            resolve_id(&TaskReadModel::default(), ""),
            Err("no task matches ''".into())
        );
    }

    #[test]
    fn assignee_filters_and_live_inboxes_share_actor_aliases() {
        let mut log = Log::new();
        let assigned = log.open("orchestrator", None, Some("web:ember"));
        let claimed = log.open("orchestrator", None, None);
        log.push(
            &claimed,
            "web:ember",
            TaskEventKind::Claimed {
                assignee: "web:ember".into(),
            },
        );
        let decision = log.open("orchestrator", None, Some("ember"));
        log.push(
            &decision,
            "orchestrator",
            TaskEventKind::Posed {
                options: vec!["keep".into()],
                default_choice: "keep".into(),
                deadline: None,
                source: None,
            },
        );
        log.open("orchestrator", None, Some("lane:ember"));
        let model = log.model();
        for who in ["ember", "web:ember"] {
            let ids: Vec<_> = list(
                &model,
                &TaskFilter {
                    assignee: Some(who.into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|t| t.task_id.as_str())
            .collect();
            assert_eq!(ids, [assigned.as_str(), claimed.as_str(), decision.as_str()]);
            let items = inbox(&model, who);
            assert_eq!(items.len(), 3);
            assert_eq!(items[0].reason, InboxReason::AssignedOpen);
            assert_eq!(items[1].reason, InboxReason::ClaimedByYou);
            assert_eq!(items[2].reason, InboxReason::DecisionOwed);
        }
    }

    #[test]
    fn list_filters_by_state_and_hides_terminal_by_default() {
        let mut log = Log::new();
        let open = log.open("h", None, None);
        let done = log.open("h", None, None);
        log.push(&done, "a", TaskEventKind::Claimed { assignee: "a".into() });
        log.push(
            &done,
            "a",
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: None,
                check: None,
            },
        );
        let claimed = log.open("h", Some("/tmp/repo"), None);
        log.push(
            &claimed,
            "agent:author",
            TaskEventKind::Claimed {
                assignee: "agent:author".into(),
            },
        );

        let model = log.model();
        let all = list(&model, &TaskFilter::default()).unwrap();
        assert_eq!(all.len(), 2, "done task hidden by default");
        assert!(all.iter().any(|t| t.task_id == open));

        let claimed_only = list(
            &model,
            &TaskFilter {
                state: Some("claimed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(claimed_only.len(), 1);
        assert_eq!(claimed_only[0].task_id, claimed);

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

        let err = list(
            &model,
            &TaskFilter {
                state: Some("ready".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("ready") && err.contains("resolved"), "{err}");

        // `--decisions` / `--actions`: a posed task is a decision, everything else an action.
        let question = log.open("h", None, None);
        log.push(
            &question,
            "h",
            TaskEventKind::Posed {
                options: vec!["a".into()],
                default_choice: "a".into(),
                deadline: None,
                source: None,
            },
        );
        let model = log.model();
        let kind = |decisions| {
            list(
                &model,
                &TaskFilter {
                    decisions: Some(decisions),
                    ..Default::default()
                },
            )
            .unwrap()
            .iter()
            .map(|t| t.task_id.clone())
            .collect::<Vec<_>>()
        };
        assert_eq!(kind(true), std::slice::from_ref(&question));
        assert!(!kind(false).contains(&question) && kind(false).contains(&open));
    }

    #[test]
    fn inbox_covers_assigned_and_claimed() {
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
        let someone_elses = log.open("human", None, Some("agent:b"));

        let model = log.model();
        let a: Vec<_> = inbox(&model, "agent:a");
        let reasons: Vec<(String, InboxReason)> = a.iter().map(|e| (e.task.task_id.clone(), e.reason)).collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons.contains(&(assigned.clone(), InboxReason::AssignedOpen)));
        assert!(reasons.contains(&(claimed.clone(), InboxReason::ClaimedByYou)));

        let b: Vec<_> = inbox(&model, "agent:b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].reason, InboxReason::AssignedOpen);
        assert_eq!(b[0].task.task_id, someone_elses);
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
    fn inbox_is_oldest_first_with_waiting_anchors() {
        let mut log = Log::new();
        // A decision ages from its POSE, not from the notes that follow it.
        let decision = log.open("h", None, Some("agent:a"));
        log.push_at(
            &decision,
            "h",
            "2026-07-12T00:00:00Z",
            TaskEventKind::Posed {
                options: vec!["keep".into(), "drop".into()],
                default_choice: "keep".into(),
                deadline: None,
                source: None,
            },
        );
        log.push_at(
            &decision,
            "h",
            "2026-07-16T00:00:00Z",
            TaskEventKind::Noted {
                claim: None,
                text: "still thinking?".into(),
                session_ref: None,
            },
        );
        // A claimed task whose last event is between the pose and the note.
        let claimed = log.open("h", None, None);
        log.push_at(
            &claimed,
            "agent:a",
            "2026-07-14T00:00:00Z",
            TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        );

        let model = log.model();
        let entries = inbox(&model, "agent:a");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].task.task_id, decision, "stalest at top");
        assert_eq!(entries[0].reason, InboxReason::DecisionOwed);
        assert_eq!(
            entries[0].since.to_rfc3339(),
            "2026-07-12T00:00:00+00:00",
            "a decision ages from the pose, not the last note"
        );
        assert_eq!(entries[1].task.task_id, claimed);
        assert_eq!(entries[1].reason, InboxReason::ClaimedByYou);
        assert_eq!(entries[1].since.to_rfc3339(), "2026-07-14T00:00:00+00:00");
    }

    #[test]
    fn deadlines_read_durations_dates_and_datetimes() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        assert_eq!(parse_deadline("3d", now).unwrap(), now + chrono::Duration::days(3));
        assert_eq!(
            parse_deadline("2026-10-05T10:00:00Z", now).unwrap(),
            "2026-10-05T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        let d = parse_deadline("2026-10-03", now).unwrap();
        assert!(
            d > "2026-10-02T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            "end of local day"
        );
        assert!(parse_deadline("whenever", now).is_err());
    }

    #[test]
    fn invalid_and_out_of_range_durations_return_errors_without_panicking() {
        let now = DateTime::from_timestamp(1_000, 0).unwrap();
        for input in [
            "",
            "λ",
            "12💥",
            "-1h",
            "9223372036854775807s",
            "9223372036854775807m",
            "9223372036854775807h",
            "9223372036854775807d",
            "9223372036854775807w",
        ] {
            assert!(parse_duration(input).is_none(), "{input}");
            assert!(parse_since(input, now).is_err(), "{input}");
            assert!(parse_deadline(input, now).is_err(), "{input}");
        }
        // Representable durations can still exceed the timestamp range.
        assert!(parse_duration("1000000000000000s").is_some());
        assert!(parse_since("1000000000000000s", now).is_err());
        assert!(parse_deadline("1000000000000000s", now).is_err());
        assert!(parse_since("1s", DateTime::<Utc>::MIN_UTC).is_err());
        assert!(parse_deadline("1s", DateTime::<Utc>::MAX_UTC).is_err());
        assert_eq!(parse_duration(" 2h "), Some(chrono::Duration::hours(2)));
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
