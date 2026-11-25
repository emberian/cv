//! Format manifests and the format census (`docs/INTERFACE-V2.md` §7).
//!
//! A harness's on-disk format drifts silently: three of the six harnesses we have source for moved
//! their live store out from under cv in 2026 (OpenClaw → sqlite, OpenCode → `opencode.db`,
//! Kimi → `~/.kimi-code`) and the adapters degraded to "old data only" without a single error. The
//! 2026-09-19 re-verification was a manual census with `jq`; this module makes it a command.
//!
//! Three instruments, each cheap and mechanical:
//!
//! 1. **Manifests** — `formats/<harness>.toml`, embedded at compile time (one per
//!    [`Harness::ALL`] entry, by the macro that declares the harness list): the upstream commit the
//!    adapter was verified against, the store paths, and what the source cannot say about the
//!    persisted vocabulary — `generic` (interpreted by a blanket rule, so no per-item literal
//!    exists), `carried` (kept verbatim under `Demand::COMPLETE` only), `ignored` (known and
//!    deliberately skipped), and `handled` only for an item no match arm names (a SQL identifier,
//!    a store path, a tool name). An item the adapter matches on is `handled` by inference
//!    ([`handled`]): the arm is the spec. `docs/FORMATS.md` stays the prose.
//! 2. **[`check`]** — source vs manifest, both directions: every explicit `handled` type must
//!    appear as a string literal in the adapter's source (or the shared `harness/wire/` codec it
//!    names); and no `handled` entry may restate one of the adapter's own match arms, nor
//!    `ignore_literals` name a literal no arm uses. Runs as an integration test
//!    (`tests/formats_manifest.rs`) on a source tree.
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
use crate::ir::{Harness, SessionRef};
use crate::stream::{self, Demand};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How an adapter treats one vocabulary item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The adapter interprets it with a per-item arm (the literal appears in the source).
    Handled,
    /// The adapter interprets it through a generic rule (e.g. every rendered Claude attachment becomes
    /// injected context whatever its kind), so no per-item literal exists in the source.
    Generic,
    /// Kept verbatim as a carrier under `Demand::COMPLETE`, dropped otherwise.
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
        // Exact wire spellings take precedence over families, independently of key ordering.
        fn candidates(key: &str) -> [Option<&str>; 2] {
            [Some(key), key.split_once('.').map(|(_, wire)| wire)]
        }
        for (key, entry) in &self.types {
            if candidates(key).into_iter().flatten().any(|candidate| candidate == wire) {
                return Some((key.as_str(), entry));
            }
        }
        let mut family = None;
        let mut longest = 0;
        for (key, entry) in &self.types {
            for candidate in candidates(key).into_iter().flatten() {
                if let Some(prefix) = candidate.strip_suffix('*') {
                    if prefix.len() > longest && wire.starts_with(prefix) {
                        longest = prefix.len();
                        family = Some((key.as_str(), entry));
                    }
                }
            }
        }
        family
    }
}

/// The embedded manifest for `h`.
pub fn manifest(h: Harness) -> Result<Manifest> {
    let name = h.as_str();
    let m: Manifest = toml::from_str(h.manifest_toml()).with_context(|| format!("parsing formats/{name}.toml"))?;
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

/// One inconsistency between an adapter's source and its manifest is a [`Divergence`]: `what` the
/// manifest item, `a` its [`FindingKind`] tag, `b` the detail, `where` the harness, never expected.
pub use crate::emit::Divergence;

fn finding(harness: &str, kind: FindingKind, item: impl Into<String>, detail: impl Into<String>) -> Divergence {
    Divergence {
        what: item.into(),
        a: kind.as_str().into(),
        b: detail.into(),
        expected: false,
        why: None,
        at: harness.into(),
    }
}

/// The classes of manifest/source disagreement (a finding's `a`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// The manifest says `handled`, but the literal never appears in the adapter source.
    HandledMissingInSource,
    /// The manifest lists as `handled` a type the adapter already names in a match arm — the arm
    /// says it; the entry only restates it (and goes stale the day the arm is renamed).
    HandledRestatesArm,
    /// An `ignore_literals` entry no match arm uses any more.
    IgnoreLiteralUnused,
    /// The manifest names a source file that does not exist.
    SourceFileMissing,
    /// The manifest itself does not parse (or names another harness).
    ManifestInvalid,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::HandledMissingInSource => "handled_missing_in_source",
            FindingKind::HandledRestatesArm => "handled_restates_arm",
            FindingKind::IgnoreLiteralUnused => "ignore_literal_unused",
            FindingKind::SourceFileMissing => "source_file_missing",
            FindingKind::ManifestInvalid => "manifest_invalid",
        }
    }
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
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let lit = r#""([A-Za-z][A-Za-z0-9_.\-]*)""#;
        [
            format!(r"{lit}\s*(?:=>|\|)"),
            format!(r"\|\s*{lit}"),
            format!(r"Some\({lit}\)"),
            format!(r"Some\(\w+ @ \({lit}"),
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("static regex"))
        .collect()
    });
    let body = non_test_source(source);
    let mut out = BTreeSet::new();
    for re in patterns {
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
pub fn read_sources(m: &Manifest, src_root: &Path) -> (Vec<(String, String)>, Vec<Divergence>) {
    let mut sources = Vec::new();
    let mut findings = Vec::new();
    for f in &m.source_files {
        match std::fs::read_to_string(src_root.join(f)) {
            Ok(text) => sources.push((f.clone(), text)),
            Err(e) => findings.push(finding(
                &m.harness,
                FindingKind::SourceFileMissing,
                f.clone(),
                e.to_string(),
            )),
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

/// A source file under `harness/wire/`: the shared Anthropic/OpenAI codecs (see [`check`]).
fn is_shared_codec(file: &str) -> bool {
    file.starts_with("harness/wire/")
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
pub fn check(m: &Manifest, sources: &[(String, String)]) -> Vec<Divergence> {
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
            findings.push(finding(
                &m.harness,
                FindingKind::HandledMissingInSource,
                ty.clone(),
                "a `.*` family key names no single literal; use carried/ignored/generic",
            ));
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
            findings.push(finding(
                &m.harness,
                FindingKind::HandledMissingInSource,
                ty.clone(),
                format!(
                    "manifest says handled, but none of {} appears in {:?}",
                    quoted.join(" / "),
                    m.source_files
                ),
            ));
        }
    }
    // 2. the adapter's own match arms (not a shared `harness/wire/` codec, which carries every
    //    dialect's spellings at once) ARE its `handled` vocabulary, so the manifest must not say
    //    it again, and every `ignore_literals` entry must still be an arm literal. (A `generic`
    //    entry is not held to "has no arm": its namespace-stripped literal — `role.user` → `user`
    //    — may name an unrelated arm, and the check cannot tell which.)
    let arms = arm_literals(sources);
    for (ty, entry) in &m.types {
        if entry.status != Status::Handled {
            continue;
        }
        if let Some(arm) = key_candidates(ty).into_iter().find(|c| arms.contains(c)) {
            findings.push(finding(
                &m.harness,
                FindingKind::HandledRestatesArm,
                ty.clone(),
                format!("the adapter has a match arm for {arm:?}: it is handled; drop the entry"),
            ));
        }
    }
    for lit in &m.ignore_literals {
        if !arms.contains(lit) {
            findings.push(finding(
                &m.harness,
                FindingKind::IgnoreLiteralUnused,
                lit.clone(),
                "no match arm uses this literal any more; drop it from ignore_literals",
            ));
        }
    }
    findings
}

/// The adapter's own match-arm literals across `sources` (a shared `harness/wire/` codec
/// excluded): the `handled` vocabulary, inferred.
fn arm_literals(sources: &[(String, String)]) -> BTreeSet<String> {
    sources
        .iter()
        .filter(|(f, _)| !is_shared_codec(f))
        .flat_map(|(_, t)| type_literals(t))
        .collect()
}

/// The `handled` vocabulary of `m`: every match-arm literal of its adapter (minus
/// `ignore_literals`, which are arm literals that are not vocabulary — file extensions, role words)
/// plus the explicit `handled` entries the arms cannot show (SQL identifiers, store paths, tool
/// names). The manifest itself spells only what the source cannot say.
pub fn handled(m: &Manifest, sources: &[(String, String)]) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = arm_literals(sources)
        .into_iter()
        .filter(|l| !m.ignore_literals.contains(l))
        .collect();
    out.extend(
        m.types
            .iter()
            .filter(|(_, e)| e.status == Status::Handled)
            .map(|(k, _)| k.clone()),
    );
    out
}

/// [`check`] for every harness whose manifest parses, reading sources under `src_root`; a
/// manifest that does not parse is itself a finding.
pub fn check_all(src_root: &Path) -> Vec<Divergence> {
    manifests()
        .into_iter()
        .flat_map(|(h, m)| check_harness(h, m, src_root))
        .collect()
}

fn check_harness(h: Harness, m: Result<Manifest>, src_root: &Path) -> Vec<Divergence> {
    let m = match m {
        Ok(m) => m,
        Err(e) => {
            return vec![finding(
                h.as_str(),
                FindingKind::ManifestInvalid,
                format!("formats/{}.toml", h.as_str()),
                format!("{e:#}"),
            )];
        }
    };
    let (sources, mut out) = read_sources(&m, src_root);
    // With NONE of a harness's sources readable there is nothing to compare against,
    // and running the check anyway reported every `handled` type as "missing in
    // source" — hundreds of findings that read like real drift but only mean "I could
    // not open the file". That is exactly what a RELEASED binary does: it has no
    // checkout, and `src_root` defaults to the one it was built in. Say why once.
    if !sources.is_empty() {
        out.append(&mut check(&m, &sources));
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
            findings.iter().all(|f| f.a == FindingKind::SourceFileMissing.as_str()),
            "only file-missing findings, got: {:?}",
            findings
                .iter()
                .filter(|f| f.a != FindingKind::SourceFileMissing.as_str())
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
    /// The first parse error, `<path>: <error>`: the census counts failed samples, and says why
    /// the first one failed rather than dropping it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
    /// Message kinds seen (`MessageKind::as_str`).
    pub kinds: BTreeMap<String, u64>,
    /// Block types seen (`text`, `thinking`, …).
    pub block_types: BTreeMap<String, u64>,
    /// The record vocabulary the adapter did NOT interpret: carriers, keyed by their record type.
    pub records: BTreeMap<String, RecordStat>,
    /// Which of the manifest's `carried` types did not show up in these sessions
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

/// The record type a carrier message stands for: `extra[<harness>]["record_type"]` (the v2
/// contract), else the verbatim record's own `type` under `_record` (what adapters wrote before
/// the contract), else `(untyped)`.
fn carrier_record_type(m: &crate::ir::Message, h: Harness) -> Option<String> {
    if !m.is_carrier() {
        return None;
    }
    let from_bag = m
        .harness_extra(h)
        .and_then(|b| b.get("record_type"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    Some(from_bag.unwrap_or_else(|| {
        m.extra
            .get(crate::ir::CARRIER_KEY)
            .and_then(|r| r.get("type"))
            .and_then(serde_json::Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| "(untyped)".into())
    }))
}

/// The N most recent sessions per harness (catalog-backed, newest first), keyed by harness name.
fn recent_refs(harness: Option<Harness>, recent: usize) -> BTreeMap<&'static str, (Harness, Vec<SessionRef>)> {
    if recent == 0 {
        return BTreeMap::new();
    }
    select_recent_refs(crate::sessions_for(harness), harness, recent)
}

fn select_recent_refs(
    refs: impl IntoIterator<Item = SessionRef>,
    harness: Option<Harness>,
    recent: usize,
) -> BTreeMap<&'static str, (Harness, Vec<SessionRef>)> {
    type Rank = (std::cmp::Reverse<Option<chrono::DateTime<chrono::Utc>>>, usize);
    let mut by: BTreeMap<&'static str, (Harness, BTreeMap<Rank, SessionRef>)> = BTreeMap::new();
    if recent == 0 {
        return BTreeMap::new();
    }
    for (position, reference) in refs.into_iter().enumerate() {
        if harness.is_some_and(|h| h != reference.harness) {
            continue;
        }
        let rank = (std::cmp::Reverse(reference.updated_at), position);
        let (_, candidates) = by
            .entry(reference.harness.as_str())
            .or_insert_with(|| (reference.harness, BTreeMap::new()));
        if candidates.len() == recent && candidates.last_key_value().is_some_and(|(last, _)| rank >= *last) {
            continue;
        }
        candidates.insert(rank, reference);
        if candidates.len() > recent {
            candidates.pop_last();
        }
    }
    by.into_iter()
        .map(|(name, (harness, candidates))| (name, (harness, candidates.into_values().collect())))
        .collect()
}

/// Keep tally state only; failed streams never contribute a partial sample.
fn census_session(
    adapter: &dyn harness::Adapter,
    r: &SessionRef,
    h: Harness,
    manifest: Option<&Manifest>,
) -> Result<HarnessCensus> {
    let mut sample = HarnessCensus::default();
    let mut sink = stream::TryFnSink::new(|message: crate::Message| {
        *sample.kinds.entry(message.kind.as_str().to_string()).or_default() += 1;
        for block in &message.content {
            *sample.block_types.entry(block.type_tag().to_string()).or_default() += 1;
        }
        if let Some(ty) = carrier_record_type(&message, h) {
            let status = manifest.and_then(|m| m.lookup(&ty)).map(|(_, entry)| entry.status);
            sample
                .records
                .entry(ty)
                .or_insert_with(|| RecordStat {
                    count: 0,
                    first_seen: r.path.clone(),
                    status,
                    new: status.is_none(),
                })
                .count += 1;
        }
        Ok(stream::Flow::Continue)
    });
    let mut opts = Demand::COMPLETE;
    opts.spans = true;
    adapter.try_stream(r, &opts, &mut sink)?;
    Ok(sample)
}

fn unseen_carried_types(manifest: &Manifest, records: &BTreeMap<String, RecordStat>) -> Vec<String> {
    let seen: BTreeSet<&str> = records
        .keys()
        .filter_map(|wire| manifest.lookup(wire).map(|(key, _)| key))
        .collect();
    manifest
        .types
        .iter()
        .filter(|(key, entry)| entry.status == Status::Carried && !seen.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect()
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
            match census_session(adapter.as_ref(), r, h, manifest.as_ref()) {
                Err(e) => {
                    c.parse_errors += 1;
                    c.first_error
                        .get_or_insert_with(|| format!("{}: {e:#}", r.path.display()));
                }
                Ok(sample) => {
                    c.sessions += 1;
                    for (kind, count) in sample.kinds {
                        *c.kinds.entry(kind).or_default() += count;
                    }
                    for (block, count) in sample.block_types {
                        *c.block_types.entry(block).or_default() += count;
                    }
                    for (ty, record) in sample.records {
                        c.records
                            .entry(ty)
                            .and_modify(|seen| seen.count += record.count)
                            .or_insert(record);
                    }
                }
            }
        }
        if let Some(m) = &manifest {
            // A manifest type is "unseen" when no record in the sample resolves to it — compared
            // through `lookup`, not by key equality, for the same reason.
            c.unseen_manifest_types = unseen_carried_types(m, &c.records);
        }
        out.harnesses.insert(name.to_string(), c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, MessageKind};

    #[test]
    fn bounded_recent_selection_matches_stable_sort_with_ties_and_missing_dates() {
        let refs: Vec<_> = (0..256)
            .map(|position| {
                let harness = if position % 3 == 0 {
                    Harness::Gemini
                } else {
                    Harness::Codex
                };
                let mut reference = SessionRef::new(
                    format!("id-{position}"),
                    harness,
                    PathBuf::from(format!("path-{position}")),
                );
                if position % 5 != 0 {
                    reference.updated_at = chrono::DateTime::from_timestamp((position % 7) as i64, 0);
                }
                reference
            })
            .collect();
        for harness in [None, Some(Harness::Gemini)] {
            for limit in [0, 1, 10, 128, 512] {
                let mut expected: BTreeMap<&str, (Harness, Vec<SessionRef>)> = BTreeMap::new();
                if limit != 0 {
                    for reference in refs.iter().filter(|r| harness.is_none_or(|h| h == r.harness)) {
                        expected
                            .entry(reference.harness.as_str())
                            .or_insert_with(|| (reference.harness, Vec::new()))
                            .1
                            .push(reference.clone());
                    }
                    for (_, candidates) in expected.values_mut() {
                        candidates.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
                        candidates.truncate(limit);
                    }
                }
                let actual = select_recent_refs(refs.clone(), harness, limit);
                let ids = |selection: &BTreeMap<&str, (Harness, Vec<SessionRef>)>| {
                    selection
                        .iter()
                        .map(|(name, (_, candidates))| {
                            (
                                name.to_string(),
                                candidates.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
                            )
                        })
                        .collect::<BTreeMap<_, _>>()
                };
                assert_eq!(ids(&actual), ids(&expected));
            }
        }
    }

    #[test]
    fn unseen_carried_types_matches_nested_lookup_oracle_on_wide_vocabulary() {
        let mut manifest: Manifest = toml::from_str(r#"harness = "test""#).unwrap();
        let mut records = BTreeMap::new();
        for index in 0..256 {
            let key = format!("record.kind-{index}");
            manifest.types.insert(
                key,
                TypeEntry {
                    status: Status::Carried,
                    note: String::new(),
                },
            );
            if index % 2 == 0 {
                records.insert(
                    format!("kind-{index}"),
                    RecordStat {
                        count: 1,
                        first_seen: PathBuf::new(),
                        status: Some(Status::Carried),
                        new: false,
                    },
                );
            }
        }
        manifest.types.insert(
            "record.family.*".into(),
            TypeEntry {
                status: Status::Carried,
                note: String::new(),
            },
        );
        records.insert(
            "family.member".into(),
            RecordStat {
                count: 1,
                first_seen: PathBuf::new(),
                status: Some(Status::Carried),
                new: false,
            },
        );
        let expected: Vec<_> = manifest
            .types
            .iter()
            .filter(|(_, entry)| entry.status == Status::Carried)
            .filter(|(key, _)| {
                !records
                    .keys()
                    .any(|wire| manifest.lookup(wire).is_some_and(|(found, _)| found == key.as_str()))
            })
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(unseen_carried_types(&manifest, &records), expected);
        assert_eq!(expected.len(), 128);
    }

    #[test]
    fn lookup_prefers_exact_entries_then_the_most_specific_family() {
        let m: Manifest = toml::from_str(
            r#"
            harness = "test"
            [types]
            "record.permission.*" = { status = "ignored" }
            "record.permission.set_mode" = { status = "handled" }
            "record.permission.scope.*" = { status = "carried" }
        "#,
        )
        .unwrap();
        assert_eq!(m.lookup("permission.set_mode").unwrap().1.status, Status::Handled);
        assert_eq!(m.lookup("permission.scope.read").unwrap().1.status, Status::Carried);
        assert_eq!(m.lookup("permission.other").unwrap().1.status, Status::Ignored);
    }

    #[test]
    fn census_streams_tallies_and_discards_failed_samples() {
        struct Fixture {
            fail: bool,
        }
        impl harness::Adapter for Fixture {
            fn harness(&self) -> Harness {
                Harness::Gemini
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &Demand,
                sink: &mut dyn stream::MessageSink,
            ) -> Result<crate::Session> {
                assert!(opts.tier == crate::stream::Tier::Complete && opts.spans);
                let mut prompt = crate::Message::new(crate::Role::User);
                prompt.content.push(Block::from_text("body".into()));
                sink.message(prompt);
                let mut carrier =
                    crate::Message::of_kind(crate::Role::System, MessageKind::Carrier, crate::Origin::Harness);
                carrier
                    .harness_extra_mut(Harness::Gemini)
                    .insert("record_type".into(), "novel".into());
                sink.message(carrier);
                if self.fail {
                    anyhow::bail!("late source failure");
                }
                Ok(crate::Session::new(r.id.clone(), Harness::Gemini))
            }
        }
        let reference = SessionRef::new("sample", Harness::Gemini, PathBuf::from("fixture.json"));
        let sample = census_session(&Fixture { fail: false }, &reference, Harness::Gemini, None).unwrap();
        assert_eq!(sample.kinds.values().sum::<u64>(), 2);
        assert_eq!(sample.block_types["text"], 1);
        assert_eq!(sample.records["novel"].count, 1);
        assert!(sample.records["novel"].new);
        assert!(census_session(&Fixture { fail: true }, &reference, Harness::Gemini, None).is_err());
        assert!(recent_refs(None, 0).is_empty());
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_its_own_finding() {
        let findings = check_harness(Harness::Gemini, Err(anyhow::anyhow!("bad toml")), Path::new("."));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].a, FindingKind::ManifestInvalid.as_str());
        assert_eq!(findings[0].what, "formats/gemini.toml");
    }

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
        let src = r#"let t = "context.append_message";"#;
        let f = check(&m, &[("x.rs".into(), src.into())]);
        let items: Vec<&str> = f.iter().map(|f| f.what.as_str()).collect();
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
        let items: Vec<&str> = f.iter().map(|f| f.what.as_str()).collect();
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
        m.types.insert(
            "blanket".into(),
            TypeEntry {
                status: Status::Generic,
                note: String::new(),
            },
        );
        m.ignore_literals.push("stale".into());
        let src = r#"match ty { "user" => 1, "jsonl" => 2, "novel" => 3, "blanket" => 4, _ => 0 }"#;
        let sources = [("x.rs".to_string(), src.to_string())];
        let f = check(&m, &sources);
        let items: Vec<(&str, &str)> = f.iter().map(|f| (f.a.as_str(), f.what.as_str())).collect();
        assert!(items.contains(&(FindingKind::HandledMissingInSource.as_str(), "ghost")));
        assert!(items.contains(&(FindingKind::HandledRestatesArm.as_str(), "user")));
        assert!(items.contains(&(FindingKind::IgnoreLiteralUnused.as_str(), "stale")));
        assert_eq!(
            items.len(),
            3,
            "an unlisted arm literal is handled by inference, a generic one is a judgment: {items:?}"
        );
        let h = handled(&m, &sources);
        assert!(h.contains("novel") && h.contains("ghost") && !h.contains("jsonl"));
    }
}
