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
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Dirs or `conversations.json` files holding account data exports to discover. Scanned ≤2 deep.
    pub exports: Vec<PathBuf>,
    /// `[search]`: defaults for full-text search.
    pub search: Search,
    /// `[privacy]`: default scrubbing & session-admission profile for sharing/exports.
    pub privacy: ScrubProfile,
    /// `[replication.<name>]`: per-destination scrubbing & filter profiles for fleet/hub sync.
    pub replication: BTreeMap<String, ScrubProfile>,
    /// Explicit per-target keys present in `[replication.<name>]` when loaded, allowing unset
    /// fields to inherit `[privacy]` while preserving explicit `false`/`"keep"`/`true`/`[]` overrides.
    pub replication_overrides: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
}

impl Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut len = 1usize;
        if !self.search.is_unset() {
            len += 1;
        }
        if !self.privacy.is_unset() {
            len += 1;
        }
        if !self.replication.is_empty() {
            len += 1;
        }
        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("exports", &self.exports)?;
        if !self.search.is_unset() {
            map.serialize_entry("search", &self.search)?;
        }
        if !self.privacy.is_unset() {
            map.serialize_entry("privacy", &self.privacy)?;
        }
        if !self.replication.is_empty() {
            let mut rep_out = BTreeMap::new();
            for (name, prof) in &self.replication {
                rep_out.insert(
                    name,
                    serialize_replication_entry(prof, self.replication_overrides.get(name)),
                );
            }
            map.serialize_entry("replication", &rep_out)?;
        }
        map.end()
    }
}

fn serialize_replication_entry(
    prof: &ScrubProfile,
    raw_override: Option<&serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    let mut val = serde_json::to_value(prof).unwrap_or_else(|_| serde_json::json!({}));
    let (serde_json::Value::Object(out), Some(raw)) = (&mut val, raw_override) else {
        return val;
    };
    for (k, raw_v) in raw {
        match k.as_str() {
            "private_ips" => {
                out.insert(k.clone(), serde_json::Value::Bool(prof.private_ips));
            }
            "hostnames" => {
                out.insert(k.clone(), serde_json::Value::Bool(prof.hostnames));
            }
            "ssh_principals" => {
                out.insert(k.clone(), serde_json::Value::Bool(prof.ssh_principals));
            }
            "rewrite_home" => {
                out.insert(k.clone(), serde_json::Value::Bool(prof.rewrite_home));
            }
            "rewrite_repo" => {
                out.insert(k.clone(), serde_json::Value::Bool(prof.rewrite_repo));
            }
            "principal" => {
                if let Ok(pv) = serde_json::to_value(&prof.principal) {
                    out.insert(k.clone(), pv);
                }
            }
            "path_rewrites" => {
                if let Ok(prv) = serde_json::to_value(&prof.path_rewrites) {
                    out.insert(k.clone(), prv);
                }
            }
            "classes" => {
                if let Some(raw_classes) = raw_v.as_object() {
                    let cls_entry = out
                        .entry("classes")
                        .or_insert_with(|| serde_json::Value::Object(Default::default()));
                    if let Some(cls_obj) = cls_entry.as_object_mut() {
                        for (ck, cv) in raw_classes {
                            let flag = match ck.as_str() {
                                "api_keys" => Some(serde_json::Value::Bool(prof.classes.api_keys)),
                                "private_keys" => Some(serde_json::Value::Bool(prof.classes.private_keys)),
                                "jwts" => Some(serde_json::Value::Bool(prof.classes.jwts)),
                                "emails" => Some(serde_json::Value::Bool(prof.classes.emails)),
                                "blobs" => Some(serde_json::Value::Bool(prof.classes.blobs)),
                                "assignments" => Some(serde_json::Value::Bool(prof.classes.assignments)),
                                _ if !cv.is_null() => Some(cv.clone()),
                                _ => None,
                            };
                            if let Some(v) = flag {
                                cls_obj.insert(ck.clone(), v);
                            }
                        }
                    }
                }
            }
            "filter" => {
                if let Some(raw_filter) = raw_v.as_object() {
                    let flt_entry = out
                        .entry("filter")
                        .or_insert_with(|| serde_json::Value::Object(Default::default()));
                    if let Some(flt_obj) = flt_entry.as_object_mut() {
                        for (fk, fv_raw) in raw_filter {
                            let fv = match fk.as_str() {
                                "allow_cwds" => serde_json::to_value(&prof.filter.allow_cwds).ok(),
                                "deny_cwds" => serde_json::to_value(&prof.filter.deny_cwds).ok(),
                                "harnesses" => serde_json::to_value(&prof.filter.harnesses).ok(),
                                "tags" => serde_json::to_value(&prof.filter.tags).ok(),
                                _ if !fv_raw.is_null() => Some(fv_raw.clone()),
                                _ => None,
                            };
                            if let Some(v) = fv {
                                flt_obj.insert(fk.clone(), v);
                            }
                        }
                    }
                }
            }
            _ => {
                if !out.contains_key(k) && !raw_v.is_null() {
                    out.insert(k.clone(), raw_v.clone());
                }
            }
        }
    }
    val
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
    let known_table = known.as_table().context("serialized config must be a TOML table")?;
    let table = document.as_table_mut().context("config must be a TOML table")?;
    update_search_section(table, known_table.get("search"));
    update_privacy_section(table, known_table.get("privacy"));
    if cfg.replication.is_empty() {
        table.remove("replication");
    } else if let Some(rep) = known_table.get("replication") {
        table.insert("replication".into(), rep.clone());
    }
    if let Some(exports) = known_table.get("exports") {
        table.insert("exports".into(), exports.clone());
    }
    let body = toml::to_string_pretty(&document).context("serializing config")?;
    // Replaced whole, never rewritten in place, and staged with 0o600 because
    // `[replication.<name>].token` can hold hub bearer credentials. A symlinked config (dotfiles)
    // is replaced at its target so the link keeps pointing at the new file.
    let target = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
    let mut staged =
        crate::publish::Staged::beside(&target, "config", 0o600).with_context(|| format!("staging {}", p.display()))?;
    std::io::Write::write_all(&mut staged, body.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    staged
        .publish(&target, crate::publish::Claim::Replace)
        .with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

fn update_search_section(doc: &mut toml::map::Map<String, toml::Value>, known: Option<&toml::Value>) {
    if let Some(existing) = doc.get_mut("search").and_then(toml::Value::as_table_mut) {
        existing.remove("live_bytes");
        if let Some(known_tbl) = known.and_then(toml::Value::as_table) {
            for (k, v) in known_tbl {
                existing.insert(k.clone(), v.clone());
            }
        }
        if existing.is_empty() {
            doc.remove("search");
        }
    } else if let Some(known_val) = known {
        doc.insert("search".into(), known_val.clone());
    } else {
        doc.remove("search");
    }
}

fn update_privacy_section(doc: &mut toml::map::Map<String, toml::Value>, known: Option<&toml::Value>) {
    const PRIVACY_SCALARS: &[&str] = &[
        "url",
        "token",
        "producer_id",
        "private_ips",
        "hostnames",
        "ssh_principals",
        "rewrite_home",
        "rewrite_repo",
        "principal",
        "path_rewrites",
    ];
    const CLASS_KEYS: &[&str] = &["api_keys", "private_keys", "jwts", "emails", "blobs", "assignments"];
    const FILTER_KEYS: &[&str] = &["allow_cwds", "deny_cwds", "harnesses", "tags"];

    if let Some(existing) = doc.get_mut("privacy").and_then(toml::Value::as_table_mut) {
        for k in PRIVACY_SCALARS {
            existing.remove(*k);
        }
        prune_known_subtable(existing, "classes", CLASS_KEYS);
        prune_known_subtable(existing, "filter", FILTER_KEYS);
        if let Some(known_tbl) = known.and_then(toml::Value::as_table) {
            for (k, v) in known_tbl {
                match (k.as_str(), v.as_table()) {
                    ("classes" | "filter", Some(sub_known)) => {
                        let sub_entry = existing
                            .entry(k.clone())
                            .or_insert_with(|| toml::Value::Table(Default::default()));
                        if let Some(sub_tbl) = sub_entry.as_table_mut() {
                            for (sk, sv) in sub_known {
                                sub_tbl.insert(sk.clone(), sv.clone());
                            }
                        } else {
                            *sub_entry = v.clone();
                        }
                    }
                    _ => {
                        existing.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if existing.is_empty() {
            doc.remove("privacy");
        }
    } else if let Some(known_val) = known {
        doc.insert("privacy".into(), known_val.clone());
    } else {
        doc.remove("privacy");
    }
}

fn prune_known_subtable(parent: &mut toml::map::Map<String, toml::Value>, key: &str, known_keys: &[&str]) {
    let remove_whole = if let Some(sub) = parent.get_mut(key).and_then(toml::Value::as_table_mut) {
        for kk in known_keys {
            sub.remove(*kk);
        }
        sub.is_empty()
    } else {
        false
    };
    if remove_whole {
        parent.remove(key);
    }
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

    #[test]
    fn save_preserves_explicit_default_valued_replication_overrides_across_updates() {
        let _world = crate::test_world("config-replication-override-save");
        let path = save(&Config::default()).unwrap();
        let raw = r#"
exports = ["/tmp/old-export"]

[privacy]
private_ips = true
hostnames = true
principal = "drop"

[privacy.classes]
jwts = false

[privacy.filter]
deny_cwds = ["/secret"]

[replication.team-hub]
url = "http://127.0.0.1:9123/"
private_ips = false
principal = "keep"

[replication.team-hub.classes]
jwts = true

[replication.team-hub.filter]
deny_cwds = []
"#;
        std::fs::write(&path, raw).unwrap();

        // Simulate `cv config --add-export /tmp/new-export`: load_for_update -> mutate exports -> save.
        let mut cfg = load_for_update().unwrap();
        cfg.exports.push(PathBuf::from("/tmp/new-export"));
        save(&cfg).unwrap();

        let reloaded = load_for_update().unwrap();
        assert_eq!(
            reloaded.exports,
            vec![PathBuf::from("/tmp/old-export"), PathBuf::from("/tmp/new-export")]
        );
        let resolved = reloaded.replication["team-hub"]
            .inherit_from(&reloaded.privacy, reloaded.replication_overrides.get("team-hub"));
        assert!(resolved.hostnames, "unset hostnames must still inherit [privacy]");
        assert!(!resolved.private_ips, "explicit private_ips = false must survive save");
        assert_eq!(
            resolved.principal,
            PrincipalPolicy::Keep,
            "explicit principal = 'keep' must survive save"
        );
        assert!(resolved.classes.jwts, "explicit classes.jwts = true must survive save");
        assert!(
            resolved.filter.deny_cwds.is_empty(),
            "explicit filter.deny_cwds = [] must survive save"
        );
    }

    #[test]
    fn save_preserves_unknown_nested_keys_and_stages_restrictive_mode() {
        let _world = crate::test_world("config-unknown-nested-and-0600");
        let path = save(&Config::default()).unwrap();
        let raw = r#"
exports = ["/tmp/exp"]

[search]
future_search_flag = "keep-search"

[privacy]
future_privacy_flag = "keep-privacy"

[privacy.classes]
future_class = true

[replication.team-hub]
url = "http://127.0.0.1:9123/"
token = "secret-bearer-token"

[replication.team-hub.classes]
future_rep_class = false

[replication.team-hub.filter]
future_rep_filter = ["x"]
"#;
        std::fs::write(&path, raw).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let mut cfg = load_for_update().unwrap();
        cfg.exports.push(PathBuf::from("/tmp/exp2"));
        save(&cfg).unwrap();

        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["search"]["future_search_flag"].as_str(), Some("keep-search"));
        assert_eq!(doc["privacy"]["future_privacy_flag"].as_str(), Some("keep-privacy"));
        assert_eq!(doc["privacy"]["classes"]["future_class"].as_bool(), Some(true));
        assert_eq!(
            doc["replication"]["team-hub"]["classes"]["future_rep_class"].as_bool(),
            Some(false)
        );
        assert_eq!(
            doc["replication"]["team-hub"]["filter"]["future_rep_filter"][0].as_str(),
            Some("x")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "saved config.toml must be mode 0600, got {mode:#o}");
        }
    }
}
