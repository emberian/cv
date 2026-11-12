//! Writer versions observed during normal discovery, compared with frozen corpus coverage.
use crate::Harness;
use std::cmp::Ordering;

/// The newest writer version the corpus verifies for `h`, for a harness whose discovery reads a
/// writer declaration (Claude's record `version`, Codex's `cli_version`); `None` for the rest. The
/// roster lives inside the published crate; the corpus test checks it against the manifests and
/// against what each adapter's discovery declares, both ways.
pub fn verified(h: Harness) -> Option<&'static str> {
    match h {
        Harness::Claude => Some("2.1.304"),
        Harness::Codex => Some("0.163.0"),
        Harness::Grok
        | Harness::OpenCode
        | Harness::Gemini
        | Harness::Antigravity
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => None,
    }
}

/// Every harness with a verified writer, in [`Harness::ALL`] order, with its version.
pub fn roster() -> impl Iterator<Item = (Harness, &'static str)> {
    Harness::ALL.iter().filter_map(|h| Some((*h, verified(*h)?)))
}

pub struct Observed {
    pub harness: &'static str,
    /// [`verified`] for this harness.
    pub verified: &'static str,
    pub newest: Option<String>,
    pub sessions: usize,
    pub unknown: usize,
}

pub(crate) fn empty_observations() -> Vec<Observed> {
    roster()
        .map(|(harness, verified)| Observed {
            harness: harness.as_str(),
            verified,
            newest: None,
            sessions: 0,
            unknown: 0,
        })
        .collect()
}

/// The writer declarations `adapter`'s discovery reads, one per declaring session: what the
/// corpus test holds the roster to.
#[doc(hidden)]
pub fn declared_by(adapter: &dyn crate::Adapter) -> anyhow::Result<Vec<String>> {
    let context = crate::discover_cache::read::Context::current();
    context.run(|| adapter.discover())?;
    Ok(context
        .writer_versions(adapter.harness())
        .into_iter()
        .filter_map(|(_, _, version)| version)
        .collect())
}

fn numeric(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn parts(version: &str) -> Option<(Vec<&str>, Option<Vec<&str>>)> {
    let mut build = version.split('+');
    let version = build.next()?;
    if let Some(build) = build.next() {
        if !identifiers(build) {
            return None;
        }
    }
    if build.next().is_some() {
        return None;
    }
    let (core, pre) = version.split_once('-').map_or((version, None), |(c, p)| (c, Some(p)));
    let core: Vec<_> = core.split('.').collect();
    if core.len() != 3
        || core
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let pre = match pre {
        Some(p) if identifiers(p) => Some(p.split('.').collect()),
        Some(_) => return None,
        None => None,
    };
    Some((core, pre))
}

fn identifiers(text: &str) -> bool {
    text.split('.')
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Numeric release components, then SemVer prerelease ordering; build metadata has no precedence.
/// Unsupported declarations are unknown rather than lexicographically newer releases.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let (a, ap) = parts(a)?;
    let (b, bp) = parts(b)?;
    for (a, b) in a.iter().zip(&b) {
        let order = numeric(a, b);
        if order != Ordering::Equal {
            return Some(order);
        }
    }
    Some(match (ap, bp) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let mut order = Ordering::Equal;
            for (a, b) in a.iter().zip(&b) {
                order = match (
                    a.bytes().all(|c| c.is_ascii_digit()),
                    b.bytes().all(|c| c.is_ascii_digit()),
                ) {
                    (true, true) => numeric(a, b),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => a.cmp(b),
                };
                if order != Ordering::Equal {
                    break;
                }
            }
            if order == Ordering::Equal {
                a.len().cmp(&b.len())
            } else {
                order
            }
        }
    })
}

/// Fold an actual declaration into historical knowledge. Returns whether it is understood.
pub(crate) fn observe(newest: &mut Option<String>, declaration: Option<&str>) -> bool {
    let Some(version) = declaration.filter(|v| parts(v).is_some()) else {
        return false;
    };
    if newest
        .as_deref()
        .is_none_or(|old| compare(version, old).is_none_or(|o| o.is_gt()))
    {
        *newest = Some(version.to_owned());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_prerelease_and_unknown() {
        for (a, b) in [
            ("2.10.0", "2.9.99"),
            ("0.1.0", "0.1.0-rc.9"),
            ("0.1.0-rc.10", "0.1.0-rc.2"),
            ("0.1.0-beta", "0.1.0-9"),
        ] {
            assert_eq!(compare(a, b), Some(Ordering::Greater));
        }
        assert_eq!(compare("1.2.3+a", "1.2.3+b"), Some(Ordering::Equal));
        assert_eq!(compare("unknown", "1.0.0"), None);
        let mut newest = None;
        for v in [Some("2.10.0"), None, Some("unknown"), Some("2.9.0")] {
            observe(&mut newest, v);
        }
        assert_eq!(newest.as_deref(), Some("2.10.0"));
    }

    #[test]
    fn packaged_roster_matches_newest_corpus_manifests() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        for (harness, verified) in roster() {
            let harness = harness.as_str();
            let mut newest = None;
            for entry in std::fs::read_dir(corpus.join(harness)).unwrap() {
                let path = entry.unwrap().path().join("MANIFEST.toml");
                if !path.exists() {
                    continue;
                }
                let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                assert_eq!(manifest["harness"].as_str(), Some(harness));
                assert!(observe(&mut newest, manifest["version"].as_str()));
            }
            assert_eq!(newest.as_deref(), Some(verified), "update {harness}'s packaged roster");
        }
    }
}
