//! User config — a human-editable TOML file at `$XDG_CONFIG_HOME/clustervision/config.toml` (falling
//! back to `~/.config/clustervision/config.toml`).
//!
//! Today it holds the **export source index**: the dirs (or `conversations.json` files) where your
//! account data exports (ChatGPT / Claude.ai) live. Account exports have no fixed home like the CLI
//! harnesses, so rather than scan a hardcoded path, you register the ones you care about — as many as
//! you like — and the [`export`](crate::harness::export) adapters read them from here. (`$CV_EXPORTS`,
//! a `:`-separated list, is still honored as an ad-hoc union on top — handy for one-off runs.)
//!
//! It also holds `[search] live_bytes`: the live-read budget a full-text search uses when the
//! caller names none (below `--live-bytes`/`live_bytes=` and `$CV_LIVE_BYTES`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub use crate::redact::{PathRewrite, PrincipalPolicy, PrivacyPolicy, RedactClasses, ScrubProfile, SessionFilter};

/// The parsed config (missing/extra fields tolerated; absent file ⇒ defaults).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Config {
    /// Dirs or `conversations.json` files holding account data exports to discover. Scanned ≤2 deep.
    #[serde(default)]
    pub exports: Vec<PathBuf>,
    /// `[search]`: defaults for full-text search.
    #[serde(default, skip_serializing_if = "Search::is_unset")]
    pub search: Search,
    /// `[privacy]`: default scrubbing & session-admission profile for sharing/exports.
    #[serde(default, skip_serializing_if = "ScrubProfile::is_unset")]
    pub privacy: ScrubProfile,
    /// `[replication.<name>]`: per-destination scrubbing & filter profiles for fleet/hub sync.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replication: BTreeMap<String, ScrubProfile>,
    /// Explicit per-target keys present in `[replication.<name>]` when loaded, allowing unset
    /// fields to inherit `[privacy]` while preserving explicit `false`/`"keep"` overrides.
    #[serde(skip)]
    pub replication_overrides: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireConfig {
            #[serde(default)]
            exports: Vec<PathBuf>,
            #[serde(default)]
            search: Search,
            #[serde(default)]
            privacy: ScrubProfile,
            #[serde(default)]
            replication: BTreeMap<String, serde_json::Value>,
        }
        let wire = WireConfig::deserialize(deserializer)?;
        let mut replication = BTreeMap::new();
        let mut replication_overrides = BTreeMap::new();
        for (name, raw_val) in wire.replication {
            if let serde_json::Value::Object(ref map) = raw_val {
                replication_overrides.insert(name.clone(), map.clone());
            }
            let prof = serde_json::from_value::<ScrubProfile>(raw_val)
                .map_err(|e| serde::de::Error::custom(format!("in [replication.{name}]: {e}")))?;
            replication.insert(name, prof);
        }
        Ok(Config {
            exports: wire.exports,
            search: wire.search,
            privacy: wire.privacy,
            replication,
            replication_overrides,
        })
    }
}

/// The `[search]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Search {
    /// Bytes of unindexed source a search reads live when neither `--live-bytes` (`live_bytes=`)
    /// nor `$CV_LIVE_BYTES` names a budget. Unset: every session the index has not seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_bytes: Option<u64>,
}

impl Search {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// `$XDG_CONFIG_HOME/clustervision` (or `~/.config/clustervision`). `None` only if there's no home.
pub fn config_dir() -> Option<PathBuf> {
    Some(crate::env::Env::current().config_home()?.join("clustervision"))
}

/// The config file path.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Load the config, tolerantly: a missing file yields [`Config::default`]; an unreadable or
/// malformed one does too, said once per process ([`crate::env::warn_once`]).
pub fn load() -> Config {
    if config_path().is_none() {
        return Config::default();
    }
    match load_for_update() {
        Ok(config) => config,
        Err(error) => {
            crate::env::warn_once("the config file", format!("{error:#}"));
            Config::default()
        }
    }
}

/// Load before an explicit update. Missing files start with defaults; malformed or unreadable
/// existing files are errors so an update cannot overwrite registrations it failed to load.
pub fn load_for_update() -> Result<Config> {
    let path = config_path().context("no home directory for the config file")?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("reading config {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).with_context(|| format!("reading config {}", path.display())),
    }
}

/// Persist the config (creating the dir). Returns the path written.
pub fn save(cfg: &Config) -> Result<PathBuf> {
    anyhow::ensure!(!crate::env::frozen(), "--frozen forbids writing config");
    let p = config_path().context("no home directory for the config file")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Keep unknown keys from a newer binary or a user's extension when updating known fields.
    // Re-read strictly here too: direct save callers must not overwrite malformed config.
    let mut document: toml::Value = match std::fs::read_to_string(&p) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("reading config {}", p.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Value::Table(Default::default()),
        Err(error) => return Err(error).with_context(|| format!("reading config {}", p.display())),
    };
    let known = toml::Value::try_from(cfg).context("serializing config")?;
    let table = document.as_table_mut().context("config must be a TOML table")?;
    for (key, value) in known.as_table().context("serialized config must be a TOML table")? {
        table.insert(key.clone(), value.clone());
    }
    let body = toml::to_string_pretty(&document).context("serializing config")?;
    // Replaced whole, never rewritten in place. A symlinked config (a dotfiles checkout) is
    // replaced at its target, so the link keeps pointing at the new file.
    let target = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
    crate::publish::publish_reader(&target, "config", body.as_bytes(), crate::publish::Claim::Replace)
        .with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_preserve_unknown_keys_and_reject_malformed_existing_files() {
        let _world = crate::test_world("config-update-preservation");
        assert!(load_for_update().unwrap().exports.is_empty());
        let path = save(&Config::default()).unwrap();
        std::fs::write(&path, "exports = ['/old']\nfuture = 42\n[extension]\nlabel = 'keep'\n").unwrap();
        let mut cfg = load_for_update().unwrap();
        cfg.exports.push("/new".into());
        save(&cfg).unwrap();
        let document: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["future"].as_integer(), Some(42));
        assert_eq!(document["extension"]["label"].as_str(), Some("keep"));
        assert_eq!(load_for_update().unwrap().exports, cfg.exports);
        let malformed = "exports = [unfinished";
        std::fs::write(&path, malformed).unwrap();
        assert!(load_for_update().is_err());
        assert!(save(&cfg).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
        // The tolerant read degrades to the defaults (said once on stderr).
        assert!(load().exports.is_empty());
    }

    /// The config is replaced whole; a symlinked config (dotfiles) is replaced at its target and
    /// stays a symlink.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_replaced_at_its_target() {
        let world = crate::test_world("config-symlink");
        let path = save(&Config::default()).unwrap();
        let dotfile = world.base.join("dotfiles-config.toml");
        std::fs::rename(&path, &dotfile).unwrap();
        std::os::unix::fs::symlink(&dotfile, &path).unwrap();
        let cfg = Config {
            exports: vec![PathBuf::from("/linked")],
            ..Default::default()
        };
        save(&cfg).unwrap();
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(load_for_update().unwrap().exports, cfg.exports);
        assert!(std::fs::read_to_string(&dotfile).unwrap().contains("/linked"));
    }

    #[test]
    fn round_trips_exports() {
        let cfg = Config {
            exports: vec![PathBuf::from("/a/exports"), PathBuf::from("/b/conversations.json")],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        // An unset [search] table is not written into a config that never had one.
        assert!(!text.contains("search"), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.exports, cfg.exports);
        // empty/missing tolerated
        let empty: Config = toml::from_str("").unwrap();
        assert!(empty.exports.is_empty());
        assert_eq!(empty.search.live_bytes, None);
    }

    #[test]
    fn reads_and_keeps_the_search_live_bytes_default() {
        let _world = crate::test_world("config-search-live-bytes");
        let path = save(&Config::default()).unwrap();
        std::fs::write(&path, "[search]\nlive_bytes = 52428800\n").unwrap();
        assert_eq!(load().search.live_bytes, Some(52_428_800));
        // Registering an export keeps it.
        let mut cfg = load_for_update().unwrap();
        cfg.exports.push("/new".into());
        save(&cfg).unwrap();
        assert_eq!(load().search.live_bytes, Some(52_428_800));
    }

    #[test]
    fn round_trips_privacy_and_replication_profiles_without_polluting_default_toml() {
        let default_toml = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!default_toml.contains("privacy"), "{default_toml}");
        assert!(!default_toml.contains("replication"), "{default_toml}");

        let raw = r#"
exports = ["/tmp/exports"]

[privacy]
private_ips = true
hostnames = true
ssh_principals = true
rewrite_home = true
rewrite_repo = true
principal = "drop"

[privacy.classes]
emails = false

[[privacy.path_rewrites]]
pattern = "/corp/[A-Za-z0-9_-]+"
replacement = "$CORP"

[privacy.filter]
allow_cwds = ["/Users/ember/dev/*"]
deny_cwds = ["/Users/ember/dev/secret"]
harnesses = ["claude", "codex"]
tags = ["public"]

[replication.team-hub]
private_ips = true
rewrite_home = true
principal = { mode = "hash", salt = "hub-v1" }

[replication.team-hub.filter]
harnesses = ["claude"]
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert!(parsed.privacy.private_ips);
        assert!(parsed.privacy.hostnames);
        assert!(parsed.privacy.ssh_principals);
        assert!(parsed.privacy.rewrite_home);
        assert!(parsed.privacy.rewrite_repo);
        assert!(!parsed.privacy.classes.emails);
        assert!(parsed.privacy.classes.api_keys);
        assert_eq!(parsed.privacy.principal, PrincipalPolicy::Drop);
        assert_eq!(parsed.privacy.path_rewrites.len(), 1);
        assert_eq!(parsed.privacy.filter.harnesses, vec!["claude", "codex"]);

        let hub = parsed.replication.get("team-hub").expect("team-hub profile");
        assert!(hub.private_ips);
        assert_eq!(
            hub.principal,
            PrincipalPolicy::Hash {
                salt: Some("hub-v1".into())
            }
        );

        let serialized = toml::to_string_pretty(&parsed).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.privacy, parsed.privacy);
        assert_eq!(reparsed.replication, parsed.replication);
    }
}
