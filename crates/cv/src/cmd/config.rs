//! `cv config` — view the user config (`~/.config/clustervision/config.toml`) and manage the **export
//! source index** (where the `chatgpt-export`/`claude-export` harnesses look for account data exports).

use anyhow::Result;
use std::path::PathBuf;

pub(crate) fn cmd_config(add_export: Option<PathBuf>, rm_export: Option<PathBuf>) -> Result<()> {
    let mut cfg = if add_export.is_some() || rm_export.is_some() {
        cv_core::config::load_for_update()?
    } else {
        cv_core::config::load()
    };
    let mut changed = false;

    if let Some(p) = add_export {
        let p = p.canonicalize().or_else(|_| std::path::absolute(&p))?;
        if cfg.exports.contains(&p) {
            println!("already registered: {}", p.display());
        } else {
            println!("＋ export source: {}", p.display());
            cfg.exports.push(p);
            changed = true;
        }
    }
    if let Some(p) = rm_export {
        let canon = p.canonicalize().or_else(|_| std::path::absolute(&p))?;
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
    if let Some(n) = cfg.search.live_bytes {
        println!("\nsearch.live_bytes: {n} (a search reads at most this much unindexed source live)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_update_rejects_malformed_config_without_overwriting_it() {
        let _world = crate::test_world("config-cli-malformed");
        let path = cv_core::config::save(&cv_core::config::Config::default()).unwrap();
        let original = "exports = [broken";
        std::fs::write(&path, original).unwrap();
        assert!(cmd_config(Some(PathBuf::from("/new")), None).is_err());
        assert!(cmd_config(None, Some(PathBuf::from("/old"))).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
