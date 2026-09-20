//! Format manifests and the format census (`docs/INTERFACE-V2.md` §7).
//!
//! A harness's on-disk format drifts silently: three of the six harnesses we have source for moved
//! their live store out from under cv in 2026 (OpenClaw → sqlite, OpenCode → `opencode.db`,
//! Kimi → `~/.kimi-code`) and the adapters degraded to "old data only" without a single error. The
//! 2026-09-19 re-verification was a manual census with `jq`; this module makes it a command.
//!
//! Three instruments, each cheap and mechanical:
//!
//! 1. **Manifests** — `formats/<harness>.toml`, embedded at compile time: the upstream commit the
//!    adapter was verified against, the store paths, and the persisted vocabulary (record types,
//!    payload types, part types, columns) with each item's status: `handled` (the adapter
//!    interprets it with an arm of its own), `generic` (interpreted by a blanket rule, so no
//!    per-item literal exists), `carried` (kept verbatim under `ParseOptions::complete` only),
//!    `ignored` (known and deliberately skipped). The manifest is the human-readable spec the
//!    adapter is checked against; `docs/FORMATS.md` stays the prose.
//! 2. **[`check`]** — source vs manifest, both directions: every `handled` type must appear as a
//!    string literal in the adapter's source, and every type-like literal in the adapter's match
//!    arms must be in the manifest (or its explicit `ignore_literals`). Runs as an integration test
//!    (`tests/formats_manifest.rs`) and as `cv formats check` on a source tree.
//!
//! 3. **[`census`]** — real data vs manifest: parse the N most recent sessions per harness in
//!    format-complete mode and tally message kinds, block types and — the point — the record
//!    vocabulary the adapter did NOT interpret (carriers), marking anything absent from the
//!    manifest as **new**. Drift shows up the day it lands on disk.
//!
//! ## Manifest key grammar (the rule `check` implements)
//!
//! A `[types]` key is **`<namespace>.<literal>`** or a bare **`<literal>`**. The namespace is ONE
//! leading segment naming the *kind* of vocabulary item — `record`, `event`, `entry`, `block`,
//! `part`, `content`, `result`, `role`, `column`, `table`, `file`, `system`, `agent`, `extension`,
//! `display_kind` — and **everything after the first dot is the wire spelling, verbatim**. The wire
//! spelling may itself be dotted, and then it stays dotted: Kimi Code persists
//! `{"type":"context.append_message"}`, so the manifest key is `record.context.append_message` and
//! the literal `check` looks for is `"context.append_message"`. Codex's `image_gen.generation`
//! likewise becomes `extension.image_gen.generation`.
//!
//! So `check` tests a key against exactly two candidates: the key itself, and the key with its
//! first segment stripped. (The older "last dotted segment" rule looked for `"append_message"`
//! and found nothing — it is wrong for every harness whose type names contain dots.) A key ending
//! in `.*` names a family whose members are enumerated upstream, not in cv; such a key may not be
//! `handled`, because there is no single literal to find.
//!
//! One exception to "appears as a quoted literal": keys in the **`table.`** and **`column.`**
//! namespaces are SQL identifiers, and a SQL identifier lives *inside* a query string
//! (`"SELECT event_json FROM transcript_events …"`), never as a Rust literal of its own. Those keys
//! are matched as whole words anywhere in the source. It is a looser test, and deliberately so:
//! calling OpenClaw's `transcript_events` `generic` in order to satisfy a rule about quote
//! characters would be a lie about what the adapter does.

use crate::harness;
use crate::ir::{Block, Harness, MessageKind, SessionRef};
use crate::stream::{self, ParseOptions};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The manifests, embedded so an installed `cv` carries the vocabulary it was built against.
/// Order matches [`Harness::ALL`]; a missing entry is a build-time error in [`manifest`]'s test.
macro_rules! manifests {
    ($($name:literal),+ $(,)?) => {
        pub const MANIFEST_SOURCES: &[(&str, &str)] = &[
            $( ($name, include_str!(concat!("../formats/", $name, ".toml"))) ),+
        ];
    };
}
manifests!(
    "claude",
    "codex",
    "grok",
    "opencode",
    "gemini",
    "hermes",
    "openclaw",
    "cursor",
    "claude-app",
    "chatgpt-app",
    "kimi",
    "kimi-code",
    "qwen",
    "lmstudio",
    "cline",
    "roo",
    "continue",
    "goose",
    "zed",
    "chatgpt-export",
    "claude-export",
    "opensession",
);

/// How an adapter treats one vocabulary item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The adapter interprets it with a per-item arm (the literal appears in the source).
    Handled,
    /// The adapter interprets it through a generic rule (e.g. every rendered Claude attachment becomes
    /// injected context whatever its kind), so no per-item literal exists in the source.
    Generic,
    /// Kept verbatim as a carrier under `ParseOptions::complete`, dropped otherwise.
    Carried,
    /// Known and deliberately skipped (transient, UI-only, or a duplicate of another item).
    Ignored,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Handled => "handled",
            Status::Generic => "generic",
            Status::Carried => "carried",
            Status::Ignored => "ignored",
        }
    }
}

/// One vocabulary item's status and a one-line note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeEntry {
    pub status: Status,
    #[serde(default)]
    pub note: String,
}

/// The upstream the adapter was verified against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Upstream {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub date: String,
}

/// `formats/<harness>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub harness: String,
    /// Adapter source files, relative to `crates/cv-core/src/`, that [`check`] reads.
    #[serde(default)]
    pub source_files: Vec<String>,
    /// Match-arm literals that are NOT vocabulary (file extensions, env names, role words the
    /// manifest lists elsewhere) — listed explicitly so `check` never guesses.
    #[serde(default)]
    pub ignore_literals: Vec<String>,
    /// Store path globs, for humans and for the drift script.
    #[serde(default)]
    pub store: Vec<String>,
    #[serde(default)]
    pub upstream: Upstream,
    /// The vocabulary: record `type`s, payload/part types, subtypes, columns — one flat namespace
    /// per harness (a dotted prefix disambiguates where a harness reuses a word).
    #[serde(default)]
    pub types: BTreeMap<String, TypeEntry>,
}

impl Manifest {
    pub fn count(&self, status: Status) -> usize {
        self.types.values().filter(|t| t.status == status).count()
    }

    /// The entry for a type as REAL DATA spells it (`context.append_message`), not as the manifest
    /// keys it (`record.context.append_message`). Tries the key verbatim, then the key with its
    /// namespace stripped, then a `.*` family key as a prefix — the same grammar [`check`] uses, so
    /// the census and the source check cannot disagree about whether a type is known.
    ///
    /// Without this the census called every Kimi Code record NEW, which is the one thing it must
    /// never get wrong: a census that cries drift on 27 known types is a census nobody reads.
    pub fn lookup(&self, wire: &str) -> Option<(&str, &TypeEntry)> {
        if let Some((k, e)) = self.types.get_key_value(wire) {
            return Some((k.as_str(), e));
        }
        for (k, e) in &self.types {
            for cand in key_candidates(k) {
                let hit = match cand.strip_suffix('*') {
                    Some(prefix) => !prefix.is_empty() && wire.starts_with(prefix),
                    None => cand == wire,
                };
                if hit {
                    return Some((k.as_str(), e));
                }
            }
        }
        None
    }
}

/// The embedded manifest for `h`.
pub fn manifest(h: Harness) -> Result<Manifest> {
    let name = h.as_str();
    let src = MANIFEST_SOURCES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .with_context(|| format!("no embedded manifest for harness {name}"))?;
    let m: Manifest = toml::from_str(src).with_context(|| format!("parsing formats/{name}.toml"))?;
    anyhow::ensure!(
        m.harness == name,
        "formats/{name}.toml declares harness {:?}",
        m.harness
    );
    Ok(m)
}

/// Every harness's manifest (parse errors surface per harness, never hide the rest).
pub fn manifests() -> Vec<(Harness, Result<Manifest>)> {
    Harness::ALL.iter().map(|h| (*h, manifest(*h))).collect()
}

// ───────────────────────────── check: source vs manifest ─────────────────────────────

/// One inconsistency between an adapter's source and its manifest.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub harness: String,
    pub kind: FindingKind,
    pub item: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// The manifest says `handled`, but the literal never appears in the adapter source.
    HandledMissingInSource,
    /// The adapter matches on a type-like literal the manifest does not list (or ignore).
    LiteralMissingInManifest,
    /// The manifest names a source file that does not exist.
    SourceFileMissing,
}

/// `source` with its `#[cfg(test)] mod tests` block removed.
///
/// Truncating at the first `#[cfg(test)]` — which is what this did originally — cuts an adapter off
/// at its first test-only *helper* instead: `kimi_code.rs` has `#[cfg(test)] fn for_root` on line
/// 109 of 1409, so 93% of the adapter was invisible to the checker and the manifest was being
/// verified against nothing. The test module is the one that starts a `mod`, so that is what we
/// look for.
fn non_test_source(source: &str) -> &str {
    static TEST_MOD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = TEST_MOD
        .get_or_init(|| regex::Regex::new(r"(?m)^[ \t]*#\[cfg\(test\)\][ \t]*\r?\n[ \t]*mod\s").expect("static regex"));
    match re.find(source) {
        Some(m) => &source[..m.start()],
        None => source,
    }
}

/// Type-like string literals in **match-arm position** of `source` (its `#[cfg(test)]` module
/// stripped): `"x" =>`, `"x" |`, `| "x"`, `Some("x")`, `Some(t @ ("x"`. This is deliberately
/// narrow — field names, env vars and paths are not vocabulary — and it is the SAME rule the
/// manifests are written against, so a new arm in an adapter fails the check until the manifest
/// names it.
pub fn type_literals(source: &str) -> BTreeSet<String> {
    let body = non_test_source(source);
    let lit = r#""([A-Za-z][A-Za-z0-9_.\-]*)""#;
    let patterns = [
        format!(r"{lit}\s*(?:=>|\|)"),
        format!(r"\|\s*{lit}"),
        format!(r"Some\({lit}\)"),
        format!(r"Some\(\w+ @ \({lit}"),
    ];
    let mut out = BTreeSet::new();
    for p in patterns {
        let re = regex::Regex::new(&p).expect("static regex");
        for c in re.captures_iter(body) {
            if let Some(m) = c.get(1) {
                out.insert(m.as_str().to_string());
            }
        }
    }
    out
}

/// Read the manifest's `source_files` under `src_root` (`crates/cv-core/src`). A missing file
/// becomes a [`FindingKind::SourceFileMissing`] rather than an error, so one bad path never hides
/// the vocabulary findings of the others.
pub fn read_sources(m: &Manifest, src_root: &Path) -> (Vec<(String, String)>, Vec<Finding>) {
    let mut sources = Vec::new();
    let mut findings = Vec::new();
    for f in &m.source_files {
        match std::fs::read_to_string(src_root.join(f)) {
            Ok(text) => sources.push((f.clone(), text)),
            Err(e) => findings.push(Finding {
                harness: m.harness.clone(),
                kind: FindingKind::SourceFileMissing,
                item: f.clone(),
                detail: e.to_string(),
            }),
        }
    }
    (sources, findings)
}

/// The literals a manifest key may appear as in the source: the key itself, and — since the first
/// dotted segment is a namespace cv adds for readability, not part of the wire name — the key with
/// that first segment stripped. See the module docs for the grammar.
pub fn key_candidates(key: &str) -> Vec<String> {
    let mut v = vec![key.to_string()];
    if let Some((_, rest)) = key.split_once('.') {
        v.push(rest.to_string());
    }
    v
}

/// A `table.` / `column.` key: a SQL identifier, matched as a bare word (see the module docs).
fn is_sql_key(key: &str) -> bool {
    key.starts_with("table.") || key.starts_with("column.")
}

/// Does `word` occur in `text` delimited by non-identifier characters?
fn contains_word(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(i) = text[from..].find(word).map(|i| i + from) {
        let before = text[..i].chars().next_back().is_none_or(|c| !ident(c));
        let after = text[i + word.len()..].chars().next().is_none_or(|c| !ident(c));
        if before && after {
            return true;
        }
        // word is ASCII (manifest keys are), so this stays on a char boundary.
        from = i + word.len();
    }
    false
}

/// Compare `m` against its adapter sources (`(relative path, text)`), both directions.
pub fn check(m: &Manifest, sources: &[(String, String)]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let all_text: String = sources
        .iter()
        .map(|(_, t)| non_test_source(t))
        .collect::<Vec<_>>()
        .join("\n");
    // 1. handled ⇒ one of the key's two candidates appears as a quoted literal in the source.
    for (ty, entry) in &m.types {
        if entry.status != Status::Handled {
            continue;
        }
        if ty.ends_with(".*") {
            findings.push(Finding {
                harness: m.harness.clone(),
                kind: FindingKind::HandledMissingInSource,
                item: ty.clone(),
                detail: "a `.*` family key names no single literal; use carried/ignored/generic".into(),
            });
            continue;
        }
        let cands = key_candidates(ty);
        let found = if is_sql_key(ty) {
            cands.iter().any(|c| contains_word(&all_text, c))
        } else {
            cands.iter().any(|c| all_text.contains(&format!("\"{c}\"")))
        };
        if !found {
            let quoted: Vec<String> = cands.iter().map(|c| format!("{c:?}")).collect();
            findings.push(Finding {
                harness: m.harness.clone(),
                kind: FindingKind::HandledMissingInSource,
                item: ty.clone(),
                detail: format!(
                    "manifest says handled, but none of {} appears in {:?}",
                    quoted.join(" / "),
                    m.source_files
                ),
            });
        }
    }
    // 2. every match-arm literal ⇒ known to the manifest, by the same rule the census uses.
    for (file, text) in sources {
        for lit in type_literals(text) {
            if m.lookup(&lit).is_none() && !m.ignore_literals.contains(&lit) {
                findings.push(Finding {
                    harness: m.harness.clone(),
                    kind: FindingKind::LiteralMissingInManifest,
                    item: lit.clone(),
                    detail: format!("{file} matches on {lit:?}; add it to [types] or ignore_literals"),
                });
            }
        }
    }
    findings
}

/// [`check`] for every harness whose manifest parses, reading sources under `src_root`.
pub fn check_all(src_root: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for (h, m) in manifests() {
        match m {
            Ok(m) => {
                let (sources, mut missing) = read_sources(&m, src_root);
                let readable = !sources.is_empty();
                out.append(&mut missing);
                // With NONE of a harness's sources readable there is nothing to compare against,
                // and running the check anyway reported every `handled` type as "missing in
                // source" — hundreds of findings that read like real drift but only mean "I could
                // not open the file". That is exactly what a RELEASED binary does: it has no
                // checkout, and `src_root` defaults to the one it was built in. Say why once.
                if readable {
                    out.append(&mut check(&m, &sources));
                }
            }
            Err(e) => out.push(Finding {
                harness: h.as_str().to_string(),
                kind: FindingKind::SourceFileMissing,
                item: format!("formats/{}.toml", h.as_str()),
                detail: format!("{e:#}"),
            }),
        }
    }
    out
}

#[cfg(test)]
mod check_all_tests {
    use super::*;

    /// With a source root that does not exist, every manifest reports its unreadable files and
    /// NOTHING else. Running the vocabulary comparison against zero sources used to report every
    /// `handled` type as missing — hundreds of findings that read like real drift but only meant
    /// the file could not be opened, which is precisely what a released binary (built elsewhere,
    /// shipped without a checkout) produced.
    #[test]
    fn an_unreadable_source_root_reports_the_files_not_every_type() {
        let findings = check_all(Path::new("/nonexistent/cv/crates/cv-core/src"));
        assert!(!findings.is_empty(), "the missing root is still reported");
        assert!(
            findings.iter().all(|f| f.kind == FindingKind::SourceFileMissing),
            "only file-missing findings, got: {:?}",
            findings
                .iter()
                .filter(|f| f.kind != FindingKind::SourceFileMissing)
                .take(3)
                .collect::<Vec<_>>()
        );
    }
}

// ───────────────────────────── census: real data vs manifest ─────────────────────────────

/// One record type as seen in real data, with the manifest's opinion of it.
#[derive(Debug, Clone, Serialize)]
pub struct RecordStat {
    pub count: u64,
    pub first_seen: PathBuf,
    /// The manifest's status, or `None` when the manifest does not know the type.
    pub status: Option<Status>,
    /// `true` when the manifest does not list it — the drift signal.
    pub new: bool,
}

/// What the N most recent sessions of one harness hold.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HarnessCensus {
    pub sessions: usize,
    pub parse_errors: usize,
    /// Message kinds seen (`MessageKind::as_str`).
    pub kinds: BTreeMap<String, u64>,
    /// Block types seen (`text`, `thinking`, …).
    pub block_types: BTreeMap<String, u64>,
    /// The record vocabulary the adapter did NOT interpret: carriers, keyed by their record type.
    pub records: BTreeMap<String, RecordStat>,
    /// Which of the manifest's `handled`/`carried` types did not show up in these sessions
    /// (informational: either rare, or the sample is small).
    pub unseen_manifest_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Census {
    pub recent: usize,
    pub harnesses: BTreeMap<String, HarnessCensus>,
}

impl Census {
    /// Every record type the manifest does not know, across harnesses.
    pub fn new_types(&self) -> Vec<(String, String, u64)> {
        let mut v = Vec::new();
        for (h, c) in &self.harnesses {
            for (t, s) in &c.records {
                if s.new {
                    v.push((h.clone(), t.clone(), s.count));
                }
            }
        }
        v
    }
}

fn block_type(b: &Block) -> &'static str {
    match b {
        Block::Text { .. } => "text",
        Block::Thinking { .. } => "thinking",
        Block::ToolUse { .. } => "tool_use",
        Block::ToolResult { .. } => "tool_result",
        Block::Image { .. } => "image",
        Block::File { .. } => "file",
    }
}

/// The record type a carrier message stands for: `extra[<harness>]["record_type"]` (the v2
/// contract), else the verbatim record's own `type` under `_record` (what adapters wrote before
/// the contract), else `(untyped)`.
fn carrier_record_type(m: &crate::ir::Message, h: Harness) -> Option<String> {
    let is_carrier = m.kind == MessageKind::Carrier || m.extra.contains_key("_record");
    if !is_carrier {
        return None;
    }
    let from_bag = m
        .harness_extra(h)
        .and_then(|b| b.get("record_type"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    Some(from_bag.unwrap_or_else(|| {
        m.extra
            .get("_record")
            .and_then(|r| r.get("type"))
            .and_then(serde_json::Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| "(untyped)".into())
    }))
}

/// The N most recent sessions per harness (catalog-backed, newest first), keyed by harness name.
fn recent_refs(harness: Option<Harness>, recent: usize) -> BTreeMap<&'static str, (Harness, Vec<SessionRef>)> {
    let mut by: BTreeMap<&'static str, (Harness, Vec<SessionRef>)> = BTreeMap::new();
    for r in crate::sessions() {
        if harness.is_some_and(|h| h != r.harness) {
            continue;
        }
        by.entry(r.harness.as_str())
            .or_insert_with(|| (r.harness, Vec::new()))
            .1
            .push(r);
    }
    for (_, refs) in by.values_mut() {
        refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        refs.truncate(recent);
    }
    by
}

/// Parse the `recent` most recent sessions per harness in format-complete mode and tally what
/// they hold against the manifests.
pub fn census(harness: Option<Harness>, recent: usize) -> Census {
    let mut out = Census {
        recent,
        harnesses: BTreeMap::new(),
    };
    for (name, (h, refs)) in recent_refs(harness, recent) {
        let Some(adapter) = harness::for_harness(h) else {
            continue;
        };
        let manifest = manifest(h).ok();
        let mut c = HarnessCensus::default();
        for r in &refs {
            let session = match stream::collect_with(adapter.as_ref(), r, &ParseOptions::complete()) {
                Ok(s) => s,
                Err(_) => {
                    c.parse_errors += 1;
                    continue;
                }
            };
            c.sessions += 1;
            for m in &session.messages {
                *c.kinds.entry(m.kind.as_str().to_string()).or_default() += 1;
                for b in &m.content {
                    *c.block_types.entry(block_type(b).to_string()).or_default() += 1;
                }
                if let Some(ty) = carrier_record_type(m, h) {
                    let status = manifest.as_ref().and_then(|m| m.lookup(&ty)).map(|(_, e)| e.status);
                    let e = c.records.entry(ty).or_insert_with(|| RecordStat {
                        count: 0,
                        first_seen: r.path.clone(),
                        status,
                        new: status.is_none(),
                    });
                    e.count += 1;
                }
            }
        }
        if let Some(m) = &manifest {
            // A manifest type is "unseen" when no record in the sample resolves to it — compared
            // through `lookup`, not by key equality, for the same reason.
            c.unseen_manifest_types = m
                .types
                .iter()
                .filter(|(_, e)| e.status == Status::Carried)
                .filter(|(t, _)| {
                    !c.records
                        .keys()
                        .any(|seen| m.lookup(seen).is_some_and(|(k, _)| k == t.as_str()))
                })
                .map(|(t, _)| t.clone())
                .collect();
        }
        out.harnesses.insert(name.to_string(), c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_manifest_parses_and_names_its_harness() {
        for (h, m) in manifests() {
            let m = m.unwrap_or_else(|e| panic!("{}: {e:#}", h.as_str()));
            assert_eq!(m.harness, h.as_str());
            assert!(
                !m.source_files.is_empty() || m.types.is_empty(),
                "{}: source_files",
                h.as_str()
            );
        }
        assert_eq!(MANIFEST_SOURCES.len(), Harness::ALL.len(), "one manifest per harness");
    }

    #[test]
    fn type_literals_take_match_arms_only() {
        let src = r#"
            match ty {
                "user" | "assistant" => 1,
                "ai-title" => 2,
                _ => {}
            }
            let x = v.get("cwd").and_then(Value::as_str);        // a field name: not vocabulary
            if v.get("type").and_then(Value::as_str) == Some("summary") { }
            match p.get("kind").and_then(Value::as_str) { Some(t @ ("image" | "document")) => {}, _ => {} }
            #[cfg(test)]
            mod tests { fn t() { match x { "never-seen" => {} } } }
        "#;
        let got = type_literals(src);
        let want: BTreeSet<String> = ["user", "assistant", "ai-title", "summary", "image", "document"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn test_module_is_stripped_but_test_only_helpers_are_not() {
        let src = "#[cfg(test)]\nfn helper() { match x { \"early\" => {} } }\n\n#[cfg(test)]\nmod tests {\n    fn t() { match x { \"late\" => {} } }\n}\n";
        let got = type_literals(src);
        assert!(got.contains("early"), "a #[cfg(test)] fn is still adapter source");
        assert!(!got.contains("late"), "the #[cfg(test)] mod tests block is not");
    }

    #[test]
    fn dotted_keys_keep_the_wire_spelling_after_the_namespace() {
        assert_eq!(
            key_candidates("record.context.append_message"),
            vec![
                "record.context.append_message".to_string(),
                "context.append_message".to_string()
            ]
        );
        assert_eq!(key_candidates("human"), vec!["human".to_string()]);
        let mut m = Manifest {
            harness: "x".into(),
            source_files: vec!["x.rs".into()],
            ignore_literals: vec![],
            store: vec![],
            upstream: Upstream::default(),
            types: BTreeMap::new(),
        };
        m.types.insert(
            "record.context.append_message".into(),
            TypeEntry {
                status: Status::Handled,
                note: String::new(),
            },
        );
        m.types.insert(
            "record.turn.*".into(),
            TypeEntry {
                status: Status::Handled,
                note: String::new(),
            },
        );
        let src = r#"match ty { "context.append_message" => 1, _ => 0 }"#;
        let f = check(&m, &[("x.rs".into(), src.into())]);
        let items: Vec<&str> = f.iter().map(|f| f.item.as_str()).collect();
        assert_eq!(
            items,
            vec!["record.turn.*"],
            "the dotted key resolves; the family key cannot be handled"
        );
    }

    #[test]
    fn sql_identifiers_are_matched_inside_query_strings() {
        let mut m = Manifest {
            harness: "x".into(),
            source_files: vec!["x.rs".into()],
            ignore_literals: vec![],
            store: vec![],
            upstream: Upstream::default(),
            types: BTreeMap::new(),
        };
        m.types.insert(
            "table.transcript_events".into(),
            TypeEntry {
                status: Status::Handled,
                note: String::new(),
            },
        );
        m.types.insert(
            "table.gone".into(),
            TypeEntry {
                status: Status::Handled,
                note: String::new(),
            },
        );
        let src = r#"conn.prepare("SELECT event_json FROM transcript_events WHERE session_id = ?1")"#;
        let f = check(&m, &[("x.rs".into(), src.into())]);
        let items: Vec<&str> = f.iter().map(|f| f.item.as_str()).collect();
        assert_eq!(items, vec!["table.gone"]);
        assert!(
            !contains_word("transcript_events_archive", "transcript_events"),
            "whole words only"
        );
    }

    #[test]
    fn lookup_resolves_wire_spellings_and_family_keys() {
        let m: Manifest = toml::from_str(
            r#"
            harness = "kimi-code"
            [types]
            "record.context.append_message" = { status = "handled" }
            "record.permission.*" = { status = "carried" }
            "record.metadata" = { status = "carried" }
            "#,
        )
        .unwrap();
        assert_eq!(
            m.lookup("context.append_message").map(|(k, _)| k),
            Some("record.context.append_message")
        );
        assert_eq!(
            m.lookup("permission.set_mode").map(|(k, _)| k),
            Some("record.permission.*")
        );
        assert_eq!(m.lookup("metadata").map(|(_, e)| e.status), Some(Status::Carried));
        assert!(m.lookup("plugin.session_start").is_none(), "genuinely new stays new");
        assert!(m.lookup("permission").is_none(), "a family prefix needs its dot");
    }

    #[test]
    fn check_reports_both_directions() {
        let mut m = Manifest {
            harness: "x".into(),
            source_files: vec!["x.rs".into()],
            ignore_literals: vec!["jsonl".into()],
            store: vec![],
            upstream: Upstream::default(),
            types: BTreeMap::new(),
        };
        m.types.insert(
            "user".into(),
            TypeEntry {
                status: Status::Handled,
                note: String::new(),
            },
        );
        m.types.insert(
            "ghost".into(),
            TypeEntry {
                status: Status::Handled,
                note: String::new(),
            },
        );
        let src = r#"match ty { "user" => 1, "jsonl" => 2, "novel" => 3, _ => 0 }"#;
        let f = check(&m, &[("x.rs".into(), src.into())]);
        let items: Vec<(FindingKind, &str)> = f.iter().map(|f| (f.kind, f.item.as_str())).collect();
        assert!(items.contains(&(FindingKind::HandledMissingInSource, "ghost")));
        assert!(items.contains(&(FindingKind::LiteralMissingInManifest, "novel")));
        assert!(
            !items.iter().any(|(_, i)| *i == "jsonl"),
            "ignored literal is not a finding"
        );
        assert!(!items.iter().any(|(_, i)| *i == "user"));
    }
}
