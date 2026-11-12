//! Retired sub-agents: an agent `cv distill --agent-of` replaced with a successor. The source
//! transcript stays resumable, so any `SendMessage` to its id (from the root or from another lane
//! that still holds it) wakes a second instance of the same lane. The ledger here records each
//! retirement; the `cv hook retired` PreToolUse body reads it to refuse every tool call the
//! retired agent makes and every `SendMessage` addressed to it, naming the successor.
//!
//! `$CLUSTERVISION_HOME/retired.jsonl`, append-only; the last row per agent wins, and an `undone`
//! row reverses a retirement (`cv retire --undo`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Retirement {
    pub ts: DateTime<Utc>,
    /// The bare agent id (`agent-` stripped, as `SendMessage` addresses it).
    pub agent_id: String,
    /// The agent that continues its work, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
    /// The root session that owns both, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A reversal: the agent is no longer retired.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undone: bool,
}

/// `$CLUSTERVISION_HOME/retired.jsonl` (default `~/.clustervision/retired.jsonl`).
pub fn ledger_path() -> PathBuf {
    crate::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("retired.jsonl")
}

/// The bare id: `agent-` stripped.
pub fn bare(id: &str) -> &str {
    id.strip_prefix("agent-").unwrap_or(id)
}

fn append(row: &Retirement) -> std::io::Result<()> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing the retired ledger"));
    }
    use std::io::Write;
    let path = ledger_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(row).map_err(std::io::Error::other)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(line.as_bytes())
}

/// Record `agent` as retired, continued by `successor`. Calling `retire` on an already-retired
/// agent (for example, replacing `--successor PENDING` once the replacement agent is spawned)
/// updates its retirement in place and preserves an existing `session_id` when `session_id` is
/// `None`.
pub fn retire(agent: &str, successor: Option<&str>, session_id: Option<&str>) -> std::io::Result<Retirement> {
    let agent_id = bare(agent).to_string();
    let session_id = session_id
        .map(str::to_string)
        .or_else(|| raw_current().get(&agent_id).and_then(|prev| prev.session_id.clone()));
    let row = Retirement {
        ts: Utc::now(),
        agent_id,
        successor: successor.map(|s| bare(s).to_string()),
        session_id,
        undone: false,
    };
    append(&row)?;
    Ok(lookup(&row.agent_id).unwrap_or(row))
}

/// Reverse a retirement. Returns whether `agent` was retired.
pub fn undo(agent: &str) -> std::io::Result<bool> {
    let agent = bare(agent);
    let was = raw_current().contains_key(agent);
    if was {
        append(&Retirement {
            ts: Utc::now(),
            agent_id: agent.to_string(),
            successor: None,
            session_id: None,
            undone: true,
        })?;
    }
    Ok(was)
}

fn raw_current() -> HashMap<String, Retirement> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(ledger_path()) else {
        return out;
    };
    for row in text.lines().filter_map(|l| serde_json::from_str::<Retirement>(l).ok()) {
        if row.undone {
            out.remove(&row.agent_id);
        } else {
            out.insert(row.agent_id.clone(), row);
        }
    }
    out
}

/// Every agent retired now, by bare id, with transitive `A -> B -> C` successor chains resolved
/// to the live terminal successor (cycle-guarded). Empty when there is no ledger; a torn line is
/// skipped.
pub fn current() -> HashMap<String, Retirement> {
    let raw = raw_current();
    let mut out = HashMap::with_capacity(raw.len());
    for (id, mut row) in raw.iter().map(|(k, v)| (k.clone(), v.clone())) {
        let mut resolved = row.successor.clone();
        if let Some(mut cur) = resolved.clone() {
            let mut seen = std::collections::HashSet::new();
            seen.insert(id.clone());
            while seen.insert(cur.clone()) {
                let Some(next_row) = raw.get(&cur) else {
                    break;
                };
                match &next_row.successor {
                    Some(next) if !seen.contains(next) => {
                        resolved = Some(next.clone());
                        cur = next.clone();
                    }
                    Some(_) | None => {
                        resolved = None;
                        break;
                    }
                }
            }
            if resolved.as_deref() == Some(id.as_str()) {
                resolved = None;
            }
        }
        row.successor = resolved;
        out.insert(id, row);
    }
    out
}

/// The retirement of `agent`, if it is retired now.
pub fn lookup(agent: &str) -> Option<Retirement> {
    current().remove(bare(agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retire_lookup_undo_round_trip() {
        let _world = crate::test_world("retired-ledger");
        assert!(lookup("a1").is_none());
        retire("agent-a1", Some("agent-b2"), Some("root")).unwrap();
        let r = lookup("a1").expect("retired");
        assert_eq!(r.successor.as_deref(), Some("b2"));
        assert_eq!(r.session_id.as_deref(), Some("root"));
        assert!(
            lookup("agent-a1").is_some(),
            "the agent- prefix addresses the same agent"
        );
        assert!(lookup("b2").is_none(), "the successor is not retired");
        assert!(undo("a1").unwrap());
        assert!(lookup("a1").is_none());
        assert!(!undo("a1").unwrap(), "undoing twice reports it was not retired");
        // A torn trailing line does not hide the rows before it.
        retire("a3", None, None).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(ledger_path())
            .unwrap()
            .write_all(b"{\"ts\":")
            .unwrap();
        assert!(lookup("a3").is_some());
    }

    #[test]
    fn transitive_chains_pending_in_place_update_and_cycle_guard() {
        let _world = crate::test_world("retired-chains");
        // Start with PENDING, then update in place without --undo; session_id is preserved.
        retire("agent-a", Some("PENDING"), Some("root-1")).unwrap();
        assert_eq!(lookup("a").unwrap().successor.as_deref(), Some("PENDING"));
        retire("a", Some("agent-b"), None).unwrap();
        let a = lookup("a").unwrap();
        assert_eq!(a.successor.as_deref(), Some("b"));
        assert_eq!(a.session_id.as_deref(), Some("root-1"));

        // Transitive A -> B -> C -> D resolves every retired hop to live D.
        retire("b", Some("PENDING"), None).unwrap();
        assert_eq!(lookup("a").unwrap().successor.as_deref(), Some("PENDING"));
        retire("b", Some("c"), None).unwrap();
        retire("c", Some("d"), None).unwrap();
        let all = current();
        assert_eq!(all["a"].successor.as_deref(), Some("d"));
        assert_eq!(all["b"].successor.as_deref(), Some("d"));
        assert_eq!(all["c"].successor.as_deref(), Some("d"));
        assert!(!all.contains_key("d"));

        // Terminal retirement without a successor clears stale pointers to dead intermediates.
        retire("d", None, None).unwrap();
        assert_eq!(lookup("a").unwrap().successor, None);
        assert!(undo("d").unwrap());
        assert_eq!(lookup("a").unwrap().successor.as_deref(), Some("d"));

        // Cycle guard: X -> Y -> X terminates cleanly with no live successor.
        retire("x", Some("y"), None).unwrap();
        retire("y", Some("x"), None).unwrap();
        assert_eq!(lookup("x").unwrap().successor, None);
        assert_eq!(lookup("y").unwrap().successor, None);
        retire("self", Some("self"), None).unwrap();
        assert_eq!(lookup("self").unwrap().successor, None);
    }
}
