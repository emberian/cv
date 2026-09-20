//! `cv config` — view the user config (`~/.config/clustervision/config.toml`) and manage the **export
//! source index** (where the `chatgpt-export`/`claude-export` harnesses look for account data exports).

use anyhow::Result;
use std::path::PathBuf;

pub(crate) fn cmd_config(add_export: Option<PathBuf>, rm_export: Option<PathBuf>) -> Result<()> {
    let mut cfg = cv_core::config::load();
    let mut changed = false;

    if let Some(p) = add_export {
        let p = p.canonicalize().unwrap_or(p);
        if cfg.exports.contains(&p) {
            println!("already registered: {}", p.display());
        } else {
            println!("＋ export source: {}", p.display());
            cfg.exports.push(p);
            changed = true;
        }
    }
    if let Some(p) = rm_export {
        let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
        let before = cfg.exports.len();
        cfg.exports.retain(|e| e != &canon && e != &p);
        if cfg.exports.len() == before {
            println!("not registered: {}", p.display());
        } else {
            println!("－ export source: {}", canon.display());
            changed = true;
        }
    }

    if changed {
        let path = cv_core::config::save(&cfg)?;
        println!("saved → {}", path.display());
        return Ok(());
    }

    // No mutation requested → show the config.
    match cv_core::config::config_path() {
        Some(p) => println!(
            "config: {}{}",
            p.display(),
            if p.exists() { "" } else { "  (not created yet)" }
        ),
        None => println!("config: <no home directory>"),
    }
    if cfg.exports.is_empty() {
        println!("\nno export sources registered. register one with:\n  cv config --add-export ~/Downloads");
        println!("(then `cv ls --harness chatgpt-export` / `claude-export` will find your account exports,");
        println!(" and `--harness opensession` any `*.opensession.json` you have been handed)");
    } else {
        println!("\nexport sources ({}):", cfg.exports.len());
        for e in &cfg.exports {
            let missing = if e.exists() { "" } else { "  (missing)" };
            println!("  {}{missing}", e.display());
        }
    }
    Ok(())
}
