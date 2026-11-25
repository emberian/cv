//! Currently waiting edges, grouped by strongly connected component.
use super::{TaskProjection, TaskReadModel, TaskState};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashSet};

pub fn waiting(model: &TaskReadModel, task: &TaskProjection, blocker: &str) -> bool {
    let Some(other) = model.tasks.get(blocker) else {
        return true;
    };
    if task
        .prerequisites
        .get(blocker)
        .is_some_and(|e| e.require_success || e.final_only)
    {
        return !(other.state == TaskState::Done
            || other.state == TaskState::Resolved
                && other
                    .decision
                    .as_ref()
                    .and_then(|d| d.resolution.as_ref())
                    .is_some_and(|r| !r.provisional));
    }
    !other.state.is_terminal()
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stall {
    pub cycle: Vec<String>,
    pub blocked_lanes: usize,
    pub seconds: i64,
    pub score: i64,
}

/// Iterative Kosaraju. Both passes visit each current edge at most once, including back edges.
pub fn projection(replay: &super::ReplayOutcome, now: DateTime<Utc>) -> BTreeMap<String, std::rc::Rc<Stall>> {
    use super::TaskEventKind;
    let model = &replay.model;
    let mut assigned = BTreeMap::new();
    let mut since = BTreeMap::new();
    let mut seen_events = HashSet::new();
    let mut terminal = HashSet::new();
    for ev in &replay.events {
        if !seen_events.insert(&ev.id) {
            continue;
        }
        match &ev.kind {
            TaskEventKind::Opened { assignee: Some(a), .. } | TaskEventKind::Claimed { assignee: a } => {
                let assignment = assigned.entry(ev.task_id.as_str()).or_insert((a, ev.ts));
                if assignment.0 != a {
                    *assignment = (a, ev.ts);
                }
            }
            TaskEventKind::Released {} => {
                assigned.remove(ev.task_id.as_str());
            }
            TaskEventKind::BlockedBy {
                task,
                require_success,
                final_only,
            } => {
                let strict = *require_success || *final_only;
                let edge = since
                    .entry((ev.task_id.as_str(), task.as_str()))
                    .or_insert((ev.ts, strict));
                if strict && !edge.1 {
                    // A finished blocker can begin a new wait when its requirement strengthens.
                    if terminal.contains(task.as_str()) {
                        edge.0 = ev.ts;
                    }
                    edge.1 = true;
                }
            }
            TaskEventKind::Done { .. }
            | TaskEventKind::Abandoned { .. }
            | TaskEventKind::Resolved { .. }
            | TaskEventKind::Superseded { .. } => {
                terminal.insert(ev.task_id.as_str());
            }
            _ => {}
        }
    }
    let tasks: Vec<_> = model.tasks.values().collect();
    let index: BTreeMap<_, _> = tasks.iter().enumerate().map(|(i, t)| (t.task_id.as_str(), i)).collect();
    let mut edges = vec![Vec::new(); tasks.len()];
    let mut reverse = edges.clone();
    let mut waiters = edges.clone();
    for (i, t) in tasks.iter().enumerate().filter(|(_, t)| !t.state.is_terminal()) {
        for id in &t.blocked_by {
            if let Some(&j) = index.get(id.as_str()).filter(|_| waiting(model, t, id)) {
                edges[i].push(j);
                reverse[j].push(i);
                waiters[j].push(i);
            }
        }
    }
    for (i, t) in tasks.iter().enumerate() {
        if let Some(&j) = t.superseded_by.as_deref().and_then(|id| index.get(id)) {
            edges[i].push(j);
            reverse[j].push(i);
        }
    }
    let mut seen = vec![false; tasks.len()];
    let mut order = Vec::new();
    for i in 0..tasks.len() {
        if seen[i] {
            continue;
        }
        let mut stack = vec![(i, false)];
        while let Some((node, finish)) = stack.pop() {
            if finish {
                order.push(node)
            } else if !std::mem::replace(&mut seen[node], true) {
                stack.push((node, true));
                stack.extend(edges[node].iter().map(|&v| (v, false)));
            }
        }
    }
    seen.fill(false);
    let mut out = BTreeMap::new();
    for i in order.into_iter().rev() {
        if seen[i] {
            continue;
        }
        let mut members = Vec::new();
        let mut stack = vec![i];
        seen[i] = true;
        while let Some(node) = stack.pop() {
            members.push(node);
            for &v in &reverse[node] {
                if !std::mem::replace(&mut seen[v], true) {
                    stack.push(v)
                }
            }
        }
        let mut stall = Stall::default();
        if members.len() > 1 || edges[i].contains(&i) {
            stall.cycle = members.iter().map(|&m| tasks[m].task_id.clone()).collect();
            stall.cycle.sort();
        }
        let mut lanes = HashSet::new();
        for &m in &members {
            for &p in &waiters[m] {
                let t = tasks[p];
                let Some(actor) = t.assignee.as_deref() else { continue };
                lanes.insert(super::project::canonical_actor(actor));
                let edge_time = since
                    .get(&(t.task_id.as_str(), tasks[m].task_id.as_str()))
                    .map_or(t.opened_at, |e| e.0);
                let assigned_time = assigned.get(t.task_id.as_str()).map_or(t.opened_at, |a| a.1);
                stall.seconds = stall.seconds.max((now - edge_time.max(assigned_time)).num_seconds());
            }
        }
        stall.blocked_lanes = lanes.len();
        stall.score = stall.seconds.saturating_mul(lanes.len() as i64);
        let stall = std::rc::Rc::new(stall);
        for m in members {
            out.insert(tasks[m].task_id.clone(), stall.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{new_event, opened_kind, ReplayOutcome, TaskEventKind, TaskReducer};

    #[test]
    fn task_stall_measures_from_edges_and_current_assignments() {
        let at = |s| DateTime::<Utc>::from_timestamp(s, 0).unwrap();
        let mut events = vec![];
        let mut event = |id: Option<&str>, kind, time| {
            let mut e = new_event(id, "owner", kind);
            e.ts = at(time);
            let task = e.task_id.clone();
            events.push(e);
            task
        };
        let blocker = event(None, opened_kind("gate", None), 0);
        for (actor, since) in [("a", 4), ("web:a", 7), ("b", 6)] {
            let id = event(None, opened_kind("consumer", Some(actor)), 0);
            event(
                Some(&id),
                TaskEventKind::BlockedBy {
                    task: blocker.clone(),
                    require_success: false,
                    final_only: false,
                },
                since,
            );
        }
        let m = TaskReducer::reduce(&events).unwrap();
        let r = ReplayOutcome {
            model: m,
            events,
            warnings: vec![],
            quarantined: 0,
        };
        let rank = &projection(&r, at(14))[&blocker];
        assert_eq!((rank.blocked_lanes, rank.seconds, rank.score), (2, 10, 20));
        assert_eq!(projection(&r, at(1))[&blocker].score, 0);
    }
}
