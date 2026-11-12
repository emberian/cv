//! `cv retire`: the retired-agent ledger by hand. `cv distill --agent-of` writes a row for the
//! sub-agent it replaces; this lists the rows, adds one (an agent replaced some other way), and
//! reverses one. `cv hook retired` (`cv hooks install --retired`) is what enforces them.

use anyhow::{Context, Result};
use serde_json::json;

pub(crate) fn cmd_retire(agent: Option<String>, successor: Option<String>, undo: bool, json_out: bool) -> Result<()> {
    match agent {
        None => {
            print!("{}", format_list(json_out)?);
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

fn format_list(json_out: bool) -> Result<String> {
    use std::fmt::Write as _;
    let mut rows: Vec<_> = cv_core::retired::current().into_values().collect();
    rows.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.agent_id.cmp(&b.agent_id)));
    if json_out {
        let mut s = serde_json::to_string_pretty(&rows)?;
        s.push('\n');
        return Ok(s);
    }
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "no retired agents ({})", cv_core::retired::ledger_path().display());
        return Ok(out);
    }
    for r in rows {
        let _ = writeln!(
            out,
            "{}  → {}  {}{}",
            r.agent_id,
            r.successor.as_deref().unwrap_or("-"),
            r.ts.format("%Y-%m-%d %H:%M UTC"),
            r.session_id.map(|s| format!("  (session {s})")).unwrap_or_default()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn retire_list_cli_flag_and_in_place_pending_update() {
        let root = std::env::temp_dir().join(format!("cv-retire-cmd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = cv_core::env::Env::from_vars([("CLUSTERVISION_HOME", &root)]).enter();

        // `--list` and `--list --json` parse cleanly and conflict with `<agent>` / `--undo`.
        // Debug-mode clap subcommand construction exceeds libtest's 2 MiB worker stack on macOS.
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                for args in [
                    &["cv", "retire"][..],
                    &["cv", "retire", "--list"][..],
                    &["cv", "retire", "--list", "--json"][..],
                ] {
                    let cli = crate::Cli::try_parse_from(args).unwrap();
                    match cli.cmd {
                        crate::Cmd::Retire { agent, .. } => assert!(agent.is_none()),
                        _ => panic!("expected Cmd::Retire"),
                    }
                }
                assert!(crate::Cli::try_parse_from(["cv", "retire", "--list", "a1"]).is_err());
            })
            .unwrap()
            .join()
            .unwrap();

        assert!(format_list(false).unwrap().contains("no retired agents"));
        let empty_json: serde_json::Value = serde_json::from_str(&format_list(true).unwrap()).unwrap();
        assert_eq!(empty_json, json!([]));

        // Retire a1 -> PENDING, then update a1 -> b2 and b2 -> c3 without `--undo`.
        cmd_retire(Some("agent-a1".into()), Some("PENDING".into()), false, false).unwrap();
        cmd_retire(Some("a1".into()), Some("b2".into()), false, false).unwrap();
        cmd_retire(Some("b2".into()), Some("c3".into()), false, false).unwrap();

        let human = format_list(false).unwrap();
        assert!(human.contains("a1  → c3"), "{human}");
        assert!(human.contains("b2  → c3"), "{human}");

        let rows: Vec<cv_core::retired::Retirement> = serde_json::from_str(&format_list(true).unwrap()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.successor.as_deref() == Some("c3")));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
