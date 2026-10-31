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

/// Record `agent` as retired, continued by `successor`.
pub fn retire(agent: &str, successor: Option<&str>, session_id: Option<&str>) -> std::io::Result<Retirement> {
    let row = Retirement {
        ts: Utc::now(),
        agent_id: bare(agent).to_string(),
        successor: successor.map(|s| bare(s).to_string()),
        session_id: session_id.map(str::to_string),
        undone: false,
    };
    append(&row)?;
    Ok(row)
}

/// Reverse a retirement. Returns whether `agent` was retired.
pub fn undo(agent: &str) -> std::io::Result<bool> {
    let agent = bare(agent);
    let was = current().contains_key(agent);
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

/// Every agent retired now, by bare id. Empty when there is no ledger; a torn line is skipped.
pub fn current() -> HashMap<String, Retirement> {
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
}
