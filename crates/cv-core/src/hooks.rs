//! A harness's hook call as one shape. Each adapter decodes its own stdin payload
//! ([`Adapter::hook_event`](crate::harness::Adapter::hook_event)) into an [`Event`], so the
//! hook bodies (`cv hook <name>`) read sessions, agents and edits without naming a harness.

use crate::ir::{Harness, SessionRef};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One hook call, as the harness's adapter decoded it.
#[derive(Debug, Clone)]
pub struct Event {
    pub harness: Harness,
    /// The session the hook speaks for: the payload's own, or for a sub-agent event the session
    /// that owns the agent. `path` is its transcript, empty when the payload names none
    /// ([`Event::transcript`]).
    pub session: SessionRef,
    /// The harness's event name (`SessionStart`, `PreToolUse`, `SubagentStop`, …).
    pub event: Option<String>,
    pub cwd: Option<PathBuf>,
    /// The sub-agent the event is about, or the one it fired inside.
    pub agent: Option<Agent>,
    /// The files the tool call would write, as the harness's tool input names them (relative
    /// paths are the harness's, against `cwd`). Empty for anything but a file-writing tool.
    pub edits: Vec<String>,
}

/// A sub-agent named by a hook payload.
#[derive(Debug, Clone)]
pub struct Agent {
    /// The id the harness addresses it by (Claude's without its `agent-` file prefix).
    pub id: String,
    pub kind: Option<String>,
    pub transcript: Option<PathBuf>,
    /// The agent's final answer (on its stop event).
    pub last_message: Option<String>,
}

impl Event {
    /// The session's transcript, when the payload named one.
    pub fn transcript(&self) -> Option<&Path> {
        (!self.session.path.as_os_str().is_empty()).then_some(self.session.path.as_path())
    }

    /// Ancestor-canonicalized working directory for consistent path matching across symlinked
    /// roots (e.g. macOS `/var` <-> `/private/var`).
    pub fn normalized_cwd(&self) -> Option<PathBuf> {
        self.cwd
            .as_deref()
            .filter(|c| !c.as_os_str().is_empty())
            .map(|c| crate::tools::classify::normalize_path(c, None))
    }

    /// Every candidate edit target absolutized against [`Self::cwd`] and ancestor-canonicalized
    /// identically to [`crate::events::absolutize`], even when the target file does not exist yet.
    pub fn normalized_edits(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.edits.len());
        for f in &self.edits {
            if Path::new(f).is_absolute() || self.cwd.is_some() {
                let norm = crate::tools::classify::absolutize_path(f, self.cwd.as_deref());
                if !norm.is_empty() && !out.contains(&norm) {
                    out.push(norm);
                }
            }
        }
        out
    }
}

/// The Claude Code hook payload's fields cv reads; Codex sends the same names. Every other
/// field is ignored.
#[derive(Deserialize, Default)]
pub(crate) struct Payload {
    #[serde(default)]
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub hook_event_name: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub agent_transcript_path: Option<PathBuf>,
    pub last_assistant_message: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
}

impl Payload {
    pub(crate) fn decode(v: &Value) -> Result<Payload> {
        Payload::deserialize(v).context("stdin is not the hook JSON")
    }

    /// `transcript_path` with a leading `~` expanded (the docs' payloads spell it so).
    pub(crate) fn transcript(&self) -> Option<PathBuf> {
        self.transcript_path.as_deref().map(expand)
    }

    pub(crate) fn agent_transcript(&self) -> Option<PathBuf> {
        self.agent_transcript_path.as_deref().map(expand)
    }

    pub(crate) fn is_subagent_event(&self) -> bool {
        matches!(self.hook_event_name.as_deref(), Some("SubagentStart" | "SubagentStop"))
    }
}

fn expand(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), crate::env::var_os("HOME")) {
        (Ok(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => p.to_path_buf(),
    }
}

/// What a harness's edit hook can answer for a conflicting edit
/// ([`Capabilities::edit_guard`](crate::harness::Capabilities::edit_guard)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditGuard {
    /// The harness asks its user (Claude Code's `ask`).
    Ask,
    /// The harness can only block the call (Codex hooks have no `ask`).
    Deny,
}

impl EditGuard {
    /// The `permissionDecision` the hook answers with.
    pub fn as_str(self) -> &'static str {
        match self {
            EditGuard::Ask => "ask",
            EditGuard::Deny => "deny",
        }
    }
}

/// The edit guard for the session an [`Event`] speaks for, and its one lineage rule: an edit the
/// same writer already made is never a conflict. Where the guard can ask, the writer is the
/// session's own agent (a sibling, the parent or a child gets the question); where it can only
/// deny, the writer is the whole lineage tree, every session the `Session::lineage` parents lead
/// to the same root (a swarm shares its files on purpose, and a deny would stop it).
pub struct Guard {
    pub decision: EditGuard,
    adapter: &'static dyn crate::harness::Adapter,
    root: String,
    agent: Option<String>,
    ledger: std::sync::Arc<crate::lanes::LedgerIndex>,
}

impl Guard {
    /// `None` when the harness has no edit guard.
    pub fn of(ev: &Event) -> Option<Guard> {
        let adapter = crate::harness::rootless(ev.harness)?;
        let decision = adapter.capabilities().edit_guard?;
        let (root, ledger) = match decision {
            EditGuard::Ask => (ev.session.id.clone(), Default::default()),
            EditGuard::Deny => {
                let ledger = crate::lanes::ledger_index();
                (lineage_root(adapter, &ev.session.id, ev.transcript(), &ledger), ledger)
            }
        };
        Some(Guard {
            decision,
            adapter,
            root,
            agent: ev.agent.as_ref().map(|a| a.id.clone()),
            ledger,
        })
    }

    /// Whether `t`'s edits (its transcript at `transcript`) are this writer's own.
    pub fn exempts(&self, t: &crate::events::TouchedSession, transcript: &Path) -> bool {
        if t.harness != self.adapter.harness().as_str() {
            return false;
        }
        match self.decision {
            EditGuard::Ask => t.parent_id.as_deref().unwrap_or(&t.session_id) == self.root && t.agent_id == self.agent,
            EditGuard::Deny => lineage_root(self.adapter, &t.session_id, Some(transcript), &self.ledger) == self.root,
        }
    }
}

/// The root of `id`'s lineage: each transcript's recorded parent
/// ([`Adapter::hook_lineage`](crate::harness::Adapter::hook_lineage)) followed up through the
/// catalog, the lane ledger's `start` rows standing in for a child the catalog has not indexed
/// yet. A session without a parent is its own root. Bounded, so a cyclic ledger ends.
fn lineage_root(
    adapter: &dyn crate::harness::Adapter,
    id: &str,
    path: Option<&Path>,
    ledger: &crate::lanes::LedgerIndex,
) -> String {
    let mut id = id.to_string();
    let mut path = path.map(Path::to_path_buf);
    for _ in 0..16 {
        let registered = ledger.latest_start(&id);
        let parent = path
            .as_deref()
            // An unreadable header falls back to the ledger's registration.
            .and_then(|p| adapter.hook_lineage(p).ok().flatten())
            .and_then(|l| l.parent)
            .or_else(|| registered.map(|row| row.session_id.clone()));
        let Some(parent) = parent else { return id };
        path = crate::catalog::lookup(&parent, Some(adapter.harness()))
            .into_iter()
            .find(|r| r.id == parent)
            .map(|r| r.path)
            .or_else(|| registered.and_then(|row| row.transcript_path.clone()));
        id = parent;
    }
    id
}

/// The transcript behind a session the edit index names, without a scan: the parent's own store
/// for a sub-agent ([`Adapter::agent_transcript`](crate::harness::Adapter::agent_transcript)),
/// the catalog for anything else.
pub fn transcript_of(t: &crate::events::TouchedSession) -> Option<PathBuf> {
    let adapter = Harness::parse(&t.harness).and_then(crate::harness::rootless);
    if let (Some(parent), Some(adapter)) = (t.parent_id.as_deref(), adapter) {
        let parent = crate::catalog::lookup(parent, None).into_iter().next()?;
        if let Some(path) = adapter.agent_transcript(&parent, &t.session_id, t.workflow.as_deref()) {
            return Some(path);
        }
    }
    crate::catalog::lookup(&t.session_id, None)
        .into_iter()
        .find(|r| r.id == t.session_id)
        .map(|r| r.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_asking_guard_exempts_the_same_writer_only() {
        let event = |agent: Option<&str>| Event {
            harness: Harness::Claude,
            session: SessionRef::new("parent", Harness::Claude, ""),
            event: Some("PreToolUse".into()),
            cwd: None,
            agent: agent.map(|id| Agent {
                id: id.into(),
                kind: None,
                transcript: None,
                last_message: None,
            }),
            edits: Vec::new(),
        };
        let exempts = |agent: Option<&str>, t: &crate::events::TouchedSession| {
            let guard = Guard::of(&event(agent)).unwrap();
            assert_eq!(guard.decision, EditGuard::Ask);
            guard.exempts(t, Path::new("/t.jsonl"))
        };
        let mut touched = crate::events::TouchedSession {
            harness: "claude".into(),
            session_id: "parent".into(),
            title: None,
            edits: 1,
            reads: 0,
            last_ts: None,
            parent_id: None,
            agent_id: None,
            workflow: None,
        };
        assert!(exempts(None, &touched));
        assert!(
            !exempts(Some("self"), &touched),
            "a child and parent are separate writers"
        );
        touched.session_id = "agent-self".into();
        touched.parent_id = Some("parent".into());
        touched.agent_id = Some("self".into());
        assert!(exempts(Some("self"), &touched));
        assert!(!exempts(Some("sibling"), &touched));
        assert!(!exempts(None, &touched));
        touched.parent_id = Some("another-parent".into());
        assert!(!exempts(Some("self"), &touched));
        touched.parent_id = Some("parent".into());
        touched.harness = "codex".into();
        assert!(!exempts(Some("self"), &touched));
    }
}
