//! The one way a selector becomes a session or a sub-agent: a transcript path, `harness:id`, a
//! unique id prefix, an agent id anywhere in the fleet, or `<parent>/<agent>`. The CLI, the HTTP
//! API and library callers share these functions, so they accept the same spellings and refuse the
//! same ones.

use crate::{harness, Adapter, AgentRef, FoundSession, Harness, SessionRef, SubagentDemand};
use anyhow::{Context, Result};
use std::path::Path;

/// A selector that names nothing, or more than one thing. Travels inside [`anyhow::Error`]
/// ([`ResolveError::of`] recovers it): the CLI exits 2 on either, HTTP answers 404 or 400.
#[derive(Debug)]
pub enum ResolveError {
    NotFound(String),
    Ambiguous(String),
}

impl ResolveError {
    pub fn of(e: &anyhow::Error) -> Option<&ResolveError> {
        e.downcast_ref()
    }

    /// The HTTP status of `e`: 404 unknown, 400 ambiguous, 500 for any other failure.
    pub fn status(e: &anyhow::Error) -> u16 {
        match Self::of(e) {
            Some(ResolveError::NotFound(_)) => 404,
            Some(ResolveError::Ambiguous(_)) => 400,
            None => 500,
        }
    }

    /// `what {id:?}` is ambiguous among `candidates`, listed one pasteable selector per line.
    pub(crate) fn ambiguous(what: &str, id: &str, among: &str, mut candidates: Vec<String>) -> anyhow::Error {
        candidates.sort();
        candidates.dedup();
        ResolveError::Ambiguous(format!(
            "ambiguous {what} {id:?} — {} {among}:\n{}",
            candidates.len(),
            candidates.join("\n")
        ))
        .into()
    }
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (ResolveError::NotFound(msg) | ResolveError::Ambiguous(msg)) = self;
        f.write_str(msg)
    }
}

impl std::error::Error for ResolveError {}

/// Split an optional `<harness>:<id>` prefix off a selector. The part before the first `:` is a
/// harness only when it names one; otherwise the whole string is the id and `fallback` applies.
pub fn split_harness_id(spec: &str, fallback: Option<Harness>) -> (Option<Harness>, &str) {
    match spec
        .split_once(':')
        .and_then(|(head, rest)| Some((Harness::parse(head)?, rest)))
    {
        Some((harness, id)) => (Some(harness), id),
        None => (fallback, spec),
    }
}

/// The first 8 characters of an id, past any `agent-` prefix: what a person reads and types back.
pub fn short_id(id: &str) -> String {
    short_id_len(id, 8)
}

/// [`short_id`] keeping at least `min_bare` characters, never fewer than 8.
pub fn short_id_len(id: &str, min_bare: usize) -> String {
    let take = min_bare.max(8);
    match id.strip_prefix("agent-") {
        Some(bare) => format!("agent-{}", bare.chars().take(take).collect::<String>()),
        None => id.chars().take(take).collect(),
    }
}

/// How `want` names a forest member: `Some(true)` exactly, `Some(false)` as a prefix, `None` not
/// at all. A member id is the agent's own (Claude's `agent-<stem>`) or `<parent>/<agent>` (Kimi,
/// Kimi Code); a caller types the whole id, its agent part, or either without `agent-`.
pub fn agent_match(member: &str, want: &str) -> Option<bool> {
    let agent = member.rsplit('/').next().unwrap_or(member);
    let forms = [member, agent, agent.strip_prefix("agent-").unwrap_or(agent)];
    if forms.contains(&want) {
        return Some(true);
    }
    forms.iter().any(|form| form.starts_with(want)).then_some(false)
}

/// Keep the exact matches when there are any, else the prefix matches: an exact id always beats
/// a longer id it is a prefix of.
pub fn exact_first<T>(candidates: impl IntoIterator<Item = T>, rank: impl Fn(&T) -> Option<bool>) -> Vec<T> {
    let (exact, prefix): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .filter_map(|c| rank(&c).map(|exact| (exact, c)))
        .partition(|(exact, _)| *exact);
    let hits = if exact.is_empty() { prefix } else { exact };
    hits.into_iter().map(|(_, c)| c).collect()
}

/// One session by `harness:id` or a unique id prefix, optionally constrained to `harness`.
pub fn session(spec: &str, harness: Option<Harness>) -> Result<FoundSession> {
    let (want, id) = split_harness_id(spec, harness);
    crate::find(id, want)?.ok_or_else(|| ResolveError::NotFound(format!("no session matching {id:?}")).into())
}

/// One sub-agent of `parent` by its transcript id, its agent id, or a unique prefix of either.
pub fn subagent(parent: &SessionRef, agent: &str) -> Result<AgentRef> {
    let forest = crate::subagent_tree_filtered_of(parent, SubagentDemand::Rich, None, |id, _| {
        agent_match(id, agent).is_some()
    })?;
    let mut hits = exact_first(forest, |s| agent_match(&s.session.id, agent));
    let parent = format!("{}:{}", parent.harness, parent.id);
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(ResolveError::NotFound(format!(
            "no sub-agent matching {agent:?} under {parent} (try `cv show {parent} --subagents`)"
        ))
        .into()),
        _ => Err(ResolveError::ambiguous(
            "sub-agent id",
            agent,
            &format!("candidates under {parent}"),
            hits.iter()
                .map(|s| format!("{}:{}", s.session.harness, s.session.id))
                .collect(),
        )),
    }
}

/// A sub-agent found without naming its parent.
pub struct FleetAgent {
    pub parent: SessionRef,
    pub agent: AgentRef,
    /// Sessions holding their own copy of this agent (`cv adopt`); `parent` is the newest.
    pub holders: usize,
}

/// The sub-agent `agent` names anywhere in the fleet, with the session that spawned it. `None`
/// when no session spawned a match; several distinct matches are [`ResolveError::Ambiguous`].
pub fn fleet_agent(agent: &str, harness: Option<Harness>) -> Result<Option<FleetAgent>> {
    if harness.is_some_and(|h| crate::workflow_store(h).is_none()) {
        return Ok(None);
    }
    let mut parents = crate::find_subagent_parents(agent);
    parents.retain(|p| harness.is_none_or(|h| p.harness == h));
    // Copies of one agent across sessions read through the newest parent's copy; one agent split
    // across sessions by a resume has a single tip.
    let copies = match parents.len() {
        0 => return Ok(None),
        1 => Some(1),
        _ => crate::shared_agent_copies(&parents, agent)?,
    };
    let Some(copies) = copies else {
        return Err(ResolveError::ambiguous(
            "sub-agent id",
            agent,
            "sessions spawned a match",
            parents
                .iter()
                .map(|p| {
                    let title = p.title.as_deref().unwrap_or("untitled");
                    format!("{}:{}   # cv show {} --agent {agent} ({title})", p.harness, p.id, p.id)
                })
                .collect(),
        ));
    };
    let holders = if copies > 1 { parents.len() } else { 1 };
    let parent = parents.swap_remove(0);
    let agent = subagent(&parent, agent)?;
    Ok(Some(FleetAgent { parent, agent, holders }))
}

/// How a [`Target`] was reached, for the notes a front-end prints about it.
pub enum Via {
    /// A transcript path.
    File,
    /// A top-level session id.
    Session,
    /// `<parent>/<agent>`.
    Child(SessionRef),
    /// An agent id found under the session that spawned it.
    Fleet { parent: SessionRef, holders: usize },
}

/// A resolved selector: the session to read, its adapter, and the sub-agent it is, if any.
pub struct Target {
    pub session: SessionRef,
    pub adapter: Box<dyn Adapter>,
    pub agent: Option<AgentRef>,
    pub via: Via,
}

impl Target {
    fn agent(agent: AgentRef, via: Via) -> Result<Target> {
        let h = agent.session.harness;
        Ok(Target {
            session: agent.session.clone(),
            adapter: harness::for_harness(h).with_context(|| format!("no adapter for {h}"))?,
            agent: Some(agent),
            via,
        })
    }

    fn top((session, adapter): FoundSession, via: Via) -> Target {
        Target {
            session,
            adapter,
            agent: None,
            via,
        }
    }

    /// The sub-agent `agent` of this target.
    pub fn child(&self, agent: &str) -> Result<Target> {
        Target::agent(subagent(&self.session, agent)?, Via::Child(self.session.clone()))
    }
}

/// Resolve any selector a command accepts: an existing transcript path (read as `harness`'s, else
/// by its location, else the derived host's), `harness:id` or a unique id prefix, an agent id
/// (`agent-` optional) wherever it was spawned, or `<parent>/<agent>`.
pub fn target(spec: &str, harness: Option<Harness>) -> Result<Target> {
    let (want, id) = split_harness_id(spec, harness);
    let file = [(spec, harness), (id, want)]
        .into_iter()
        .find(|(p, _)| Path::new(p).is_file());
    if let Some((path, want)) = file {
        let path = Path::new(path);
        if let Some(found) = harness::resolve_file_source(path, want) {
            return Ok(Target::top(found, Via::File));
        }
        let h = want.unwrap_or_else(harness::derived_host);
        let adapter = harness::for_harness(h).with_context(|| format!("no adapter for {h}"))?;
        return Ok(Target::top((SessionRef::from_path(path, h), adapter), Via::File));
    }
    let fleet = || match fleet_agent(id, want)? {
        Some(FleetAgent { parent, agent, holders }) => Target::agent(agent, Via::Fleet { parent, holders }).map(Some),
        None => Ok(None),
    };
    // An `agent-…` id reads as an agent before a session; any other id the other way round.
    let agent_shaped = id.starts_with("agent-");
    if agent_shaped {
        if let Some(found) = fleet()? {
            return Ok(found);
        }
    }
    let (hit, exhausted) = crate::find_top_level_cheap(id, want)?;
    if let Some(hit) = hit {
        return Ok(Target::top(hit, Via::Session));
    }
    if !agent_shaped {
        if let Some(found) = fleet()? {
            return Ok(found);
        }
    }
    if let Some((parent, agent)) = id.split_once('/') {
        return Target::top(session(parent, want)?, Via::Session).child(agent);
    }
    if let Some(hit) = crate::find_top_level_escalate(id, want, exhausted)? {
        return Ok(Target::top(hit, Via::Session));
    }
    // The escalation rediscovered the fleet, so an agent spawned since the last probe is now visible.
    if !exhausted && want.is_none() && !crate::env::frozen() {
        if let Some(found) = fleet()? {
            return Ok(found);
        }
    }
    Err(ResolveError::NotFound(format!("no session (and no sub-agent) matching {id:?}")).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_match_by_whole_id_agent_part_or_bare_stem() {
        for want in ["agent-ab12", "ab12"] {
            assert_eq!(agent_match("agent-ab12", want), Some(true), "{want}");
            assert_eq!(agent_match("agent-ab12cd", want), Some(false), "{want}");
        }
        assert_eq!(agent_match("sess-1/agent-2", "agent-2"), Some(true));
        assert_eq!(agent_match("sess-1/agent-2", "2"), Some(true));
        assert_eq!(agent_match("sess-1/agent-2", "sess-1/agent"), Some(false));
        assert_eq!(agent_match("agent-ab12", "b12"), None);
        assert_eq!(agent_match("sess-1/agent-2", "sess-9"), None);
    }

    #[test]
    fn an_exact_id_beats_the_longer_ids_it_prefixes() {
        let rank = |want: &'static str| move |id: &&str| agent_match(id, want);
        let ids = ["agent-ab", "agent-abc", "agent-xyz"];
        assert_eq!(exact_first(ids, rank("ab")), ["agent-ab"]);
        assert_eq!(exact_first(ids, rank("agent-a")), ["agent-ab", "agent-abc"]);
        assert!(exact_first(ids, rank("q")).is_empty());
    }
}
