//! `cv retire`: the retired-agent ledger by hand. `cv distill --agent-of` writes a row for the
//! sub-agent it replaces; this lists the rows, adds one (an agent replaced some other way), and
//! reverses one. `cv hook retired` (`cv hooks install --retired`) is what enforces them.

use anyhow::{Context, Result};
use serde_json::json;

pub(crate) fn cmd_retire(agent: Option<String>, successor: Option<String>, undo: bool, json_out: bool) -> Result<()> {
    match agent {
        None => {
            let mut rows: Vec<_> = cv_core::retired::current().into_values().collect();
            rows.sort_by_key(|r| r.ts);
            if json_out {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("no retired agents ({})", cv_core::retired::ledger_path().display());
            }
            for r in rows {
                println!(
                    "{}  → {}  {}{}",
                    r.agent_id,
                    r.successor.as_deref().unwrap_or("-"),
                    r.ts.format("%Y-%m-%d %H:%M UTC"),
                    r.session_id.map(|s| format!("  (session {s})")).unwrap_or_default()
                );
            }
            Ok(())
        }
        Some(agent) if undo => {
            let was = cv_core::retired::undo(&agent).context("writing the retired ledger")?;
            let bare = cv_core::retired::bare(&agent);
            if json_out {
                println!("{}", json!({"agent_id": bare, "was_retired": was}));
            } else if was {
                println!("{bare} is no longer retired");
            } else {
                println!("{bare} was not retired; nothing written");
            }
            Ok(())
        }
        Some(agent) => {
            let row =
                cv_core::retired::retire(&agent, successor.as_deref(), None).context("writing the retired ledger")?;
            if json_out {
                println!("{}", serde_json::to_string(&row)?);
            } else {
                println!(
                    "retired {} → {} (`cv hook retired` enforces it; `cv retire --undo {}` reverses)",
                    row.agent_id,
                    row.successor.as_deref().unwrap_or("no named successor"),
                    row.agent_id
                );
            }
            Ok(())
        }
    }
}
