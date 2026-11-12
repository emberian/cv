//! Redact secrets / PII from a [`Session`] before sharing or export.
//!
//! The contribution loop ("send us your logs") and any enterprise use both hinge on being able to
//! scrub a transcript first. This returns a copy with secrets replaced by placeholders.
//!
//! # Approach
//!
//! One regex per recognizer finds candidates (`-----BEGIN PRIVATE KEY-----` blocks, known token
//! prefixes, `Authorization:`/`Bearer`, JWTs, emails, `key = "value"` assignments, URL
//! passwords, conservative high-entropy blobs), and a guard beside each pattern applies the
//! false-positive rules below to every candidate. The leftmost accepted candidate is replaced
//! with a stable placeholder of the form `[REDACTED:kind]`, and scanning resumes after it.
//!
//! This replaced a 627-line hand-rolled byte scanner (2026-10-11). On the 40 unit cases and on
//! 50 real sessions found by `cv search` for secret-ish words (5,828 redactions), the two produced
//! byte-identical output; the regex form is 365 code lines to 627 and 38× faster over that
//! sample (2.9 s against 109.8 s, most of it the scanner's quadratic blob walk).
//!
//! ## False-positive guardrails
//!
//! Ordinary prose and code must survive untouched. To that end:
//! * Token recognizers require a known, distinctive prefix (`sk-`, `ghp_`, `AKIA…`, etc.); short
//!   prefixes that collide with identifiers (`npm_`, `hf_`) additionally require a long, pure
//!   alphanumeric tail.
//! * Generic hex/base64 blobs are only redacted when they're long (≥ 32 chars for pure hex, ≥ 40
//!   for base64), are *pure* hex/base64url, AND a secret-ish keyword (`secret`, `token`,
//!   `password`, `api_key`, `key`) appears within ~48 chars before them. This avoids nuking
//!   commit hashes, code identifiers, and base64 data that isn't actually a credential.
//! * Assignment redaction (`password = "…"`) only fires for a small allowlist of key names, and
//!   skips values that look like code (`token = get_token()`) or short prose words
//!   (`password: use a strong one`).
//! * Connection-string passwords (`scheme://user:password@host`) are redacted; user and host stay
//!   visible.
//!
//! The transform is idempotent: a `[REDACTED:kind]` placeholder contains no characters that any
//! matcher will re-trigger on, so redacting twice yields the same string.
//!
//! OWNED BY the redact work.

use crate::ir::{Block, Message, Session};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Knobs for [`redact_with`]. Defaults scrub everything.
#[derive(Debug, Clone, Default)]
pub struct RedactOptions {
    /// Which classes of secret to scrub. Defaults to all of them; narrow it to scrub only some
    /// (e.g. only PEM private keys while leaving identities/fixtures intact).
    pub classes: RedactClasses,
}

const fn default_true() -> bool {
    true
}

/// A toggle per secret class. `Default` enables every class (so `RedactOptions::default()` scrubs
/// everything, unchanged); use [`RedactClasses::none`]/[`RedactClasses::parse_only`] to scope it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactClasses {
    #[serde(default = "default_true")]
    pub api_keys: bool,
    #[serde(default = "default_true")]
    pub private_keys: bool,
    #[serde(default = "default_true")]
    pub jwts: bool,
    #[serde(default = "default_true")]
    pub emails: bool,
    #[serde(default = "default_true")]
    pub blobs: bool,
    #[serde(default = "default_true")]
    pub assignments: bool,
}

impl Default for RedactClasses {
    fn default() -> Self {
        RedactClasses {
            api_keys: true,
            private_keys: true,
            jwts: true,
            emails: true,
            blobs: true,
            assignments: true,
        }
    }
}

impl RedactClasses {
    /// No class enabled (a base to turn specific ones on).
    pub fn none() -> Self {
        RedactClasses {
            api_keys: false,
            private_keys: false,
            jwts: false,
            emails: false,
            blobs: false,
            assignments: false,
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Parse a comma-separated class list into an "only these" set, accepting singular/plural and the
    /// placeholder labels: `api_key`, `private_key`, `jwt`, `email`, `blob`/`secret`, `assignment`.
    pub fn parse_only(spec: &str) -> Result<Self, String> {
        let mut c = RedactClasses::none();
        for raw in spec.split(',') {
            let name = raw.trim().trim_end_matches('s').to_ascii_lowercase();
            match name.as_str() {
                "api_key" | "apikey" | "key" => c.api_keys = true,
                "private_key" | "privatekey" | "pem" => c.private_keys = true,
                "jwt" => c.jwts = true,
                "email" => c.emails = true,
                "blob" | "secret" => c.blobs = true,
                "assignment" => c.assignments = true,
                "" => {}
                other => return Err(format!("unknown redaction class {other:?}")),
            }
        }
        Ok(c)
    }
}

/// Regex path/token rewrite rule (`pattern` -> `replacement`) in a [`ScrubProfile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRewrite {
    pub pattern: String,
    pub replacement: String,
}

/// How `Session::principal` is handled under a [`ScrubProfile`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PrincipalPolicy {
    /// Preserve `principal` structure (still scrubbing embedded secret tokens if enabled).
    #[default]
    Keep,
    /// Remove `Session::principal` (`None`).
    Drop,
    /// Replace principal string leaves with deterministic `sha256:<hex>` digests (optionally salted).
    Hash { salt: Option<String> },
}

impl PrincipalPolicy {
    pub fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }
}

impl Serialize for PrincipalPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PrincipalPolicy::Keep => serializer.serialize_str("keep"),
            PrincipalPolicy::Drop => serializer.serialize_str("drop"),
            PrincipalPolicy::Hash { salt: None } => serializer.serialize_str("hash"),
            PrincipalPolicy::Hash { salt: Some(salt) } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("mode", "hash")?;
                map.serialize_entry("salt", salt)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PrincipalPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Bare(String),
            Tagged {
                mode: String,
                #[serde(default)]
                salt: Option<String>,
            },
        }
        match Wire::deserialize(deserializer)? {
            Wire::Bare(mode) => match mode.trim().to_ascii_lowercase().as_str() {
                "keep" => Ok(PrincipalPolicy::Keep),
                "drop" => Ok(PrincipalPolicy::Drop),
                "hash" => Ok(PrincipalPolicy::Hash { salt: None }),
                other => Err(serde::de::Error::custom(format!(
                    "unknown principal policy {other:?} (expected keep, drop, or hash)"
                ))),
            },
            Wire::Tagged { mode, salt } => match mode.trim().to_ascii_lowercase().as_str() {
                "keep" => Ok(PrincipalPolicy::Keep),
                "drop" => Ok(PrincipalPolicy::Drop),
                "hash" => Ok(PrincipalPolicy::Hash { salt }),
                other => Err(serde::de::Error::custom(format!(
                    "unknown principal policy mode {other:?} (expected keep, drop, or hash)"
                ))),
            },
        }
    }
}

/// Session admission predicates for `[privacy.filter]` and `[replication.<name>.filter]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilter {
    /// When non-empty, `session.cwd` must live under (or match) at least one allowed directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_cwds: Vec<PathBuf>,
    /// Sessions whose `cwd` lives under (or matches) any denied directory are excluded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_cwds: Vec<PathBuf>,
    /// When non-empty, `session.harness.as_str()` must match one of these canonical names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
    /// When non-empty, `session.extra["cv"]["tags"]` (or `"tag"`) must include at least one tag.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl SessionFilter {
    pub fn is_empty(&self) -> bool {
        self.allow_cwds.is_empty() && self.deny_cwds.is_empty() && self.harnesses.is_empty() && self.tags.is_empty()
    }

    /// Evaluate whether `session` passes this filter without materializing lazy content.
    pub fn matches(&self, session: &Session) -> bool {
        if !self.harnesses.is_empty() {
            let actual = session.harness.as_str();
            let accepted = self.harnesses.iter().any(|want| {
                let trimmed = want.trim();
                trimmed.eq_ignore_ascii_case(actual)
                    || crate::ir::Harness::parse(trimmed).is_some_and(|h| h.as_str() == actual)
            });
            if !accepted {
                return false;
            }
        }
        if !self.deny_cwds.is_empty() {
            if let Some(cwd) = session.cwd.as_deref() {
                if self.deny_cwds.iter().any(|deny| path_matches_prefix_or_glob(cwd, deny)) {
                    return false;
                }
            }
        }
        if !self.allow_cwds.is_empty() {
            let Some(cwd) = session.cwd.as_deref() else {
                return false;
            };
            if !self
                .allow_cwds
                .iter()
                .any(|allow| path_matches_prefix_or_glob(cwd, allow))
            {
                return false;
            }
        }
        if !self.tags.is_empty() {
            let session_tags = extract_session_tags(session);
            if !self
                .tags
                .iter()
                .any(|want| session_tags.iter().any(|have| have.eq_ignore_ascii_case(want.trim())))
            {
                return false;
            }
        }
        true
    }
}

fn path_matches_prefix_or_glob(cwd: &Path, pattern: &Path) -> bool {
    let pat_str = pattern.to_string_lossy();
    if let Some(prefix) = pat_str.strip_suffix("/*").or_else(|| pat_str.strip_suffix("/**")) {
        return cwd.starts_with(Path::new(prefix));
    }
    cwd.starts_with(pattern)
}

fn extract_session_tags(session: &Session) -> Vec<&str> {
    let mut out = Vec::new();
    if let Some(cv) = session.cv_extra() {
        if let Some(tag) = cv.get("tag").and_then(Value::as_str) {
            out.push(tag);
        }
        if let Some(tags) = cv.get("tags") {
            match tags {
                Value::String(s) => out.extend(s.split(',').map(str::trim).filter(|t| !t.is_empty())),
                Value::Array(items) => out.extend(items.iter().filter_map(Value::as_str)),
                _ => {}
            }
        }
    }
    out
}

/// Configurable privacy & scrubbing profile (`[privacy]` and `[replication.<name>]` in `config.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubProfile {
    /// Hub endpoint URL (`http://host:port`) when configured under `[replication.<target>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Bearer credential for `[replication.<target>]` (overridden by `--token` or `$CV_SERVE_TOKEN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Explicit producer identity enrolled on `[replication.<target>]` (default: `$CV_PRODUCER_ID` or persistent `prod-<12hex>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_id: Option<String>,
    /// Toggleable credential & secret classes.
    #[serde(default, skip_serializing_if = "RedactClasses::is_default")]
    pub classes: RedactClasses,
    /// Scrub RFC1918 / RFC6598 (Tailscale CGNAT) / IPv6 ULA private addresses to `[REDACTED:ip]`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private_ips: bool,
    /// Scrub internal/mDNS/Tailscale hostnames (`.local`, `.internal`, `.lan`, `.corp`, `.ts.net`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hostnames: bool,
    /// Scrub `user@host` SSH targets (`ssh user@host`, `scp user@host:path`, `ssh://user@host`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ssh_principals: bool,
    /// Rewrite `$HOME` and `/Users/<user>` / `/home/<user>` / `C:\Users\<user>` prefixes to `$HOME`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewrite_home: bool,
    /// Rewrite `repo_root` (or `session.cwd`) prefixes to `$REPO` before `$HOME` rewrites.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewrite_repo: bool,
    /// Explicit repository root for `rewrite_repo` when `session.cwd` is absent or narrower.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<PathBuf>,
    /// Custom ordered regex path/text rewrites (`pattern` -> `replacement`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_rewrites: Vec<PathRewrite>,
    /// Policy for `Session::principal` (`keep`, `drop`, or `hash`).
    #[serde(default, skip_serializing_if = "PrincipalPolicy::is_keep")]
    pub principal: PrincipalPolicy,
    /// Session admission filter (`allow_cwds`, `deny_cwds`, `harnesses`, `tags`).
    #[serde(default, skip_serializing_if = "SessionFilter::is_empty")]
    pub filter: SessionFilter,
}

/// Alias for [`ScrubProfile`] used by privacy/share/replication callers.
pub type PrivacyPolicy = ScrubProfile;

impl ScrubProfile {
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    pub fn matches_session(&self, session: &Session) -> bool {
        self.filter.matches(session)
    }

    /// Compile custom regex rules once for repeated text or multi-session scrubbing.
    pub fn compile(&self) -> anyhow::Result<CompiledProfile> {
        let mut custom = Vec::with_capacity(self.path_rewrites.len());
        for rule in &self.path_rewrites {
            let re = Regex::new(&rule.pattern)
                .map_err(|e| anyhow::anyhow!("invalid path_rewrites regex {:?}: {e}", rule.pattern))?;
            custom.push((re, rule.replacement.clone()));
        }
        Ok(CompiledProfile {
            profile: self.clone(),
            env_home: crate::env::home_dir().map(|p| p.to_string_lossy().into_owned()),
            custom,
        })
    }
}

/// Pre-compiled [`ScrubProfile`] ready for high-throughput text & session scrubbing.
#[derive(Debug, Clone)]
pub struct CompiledProfile {
    profile: ScrubProfile,
    env_home: Option<String>,
    custom: Vec<(Regex, String)>,
}

/// Counts of how many spans of each class were redacted across a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactStats {
    pub api_keys: usize,
    pub private_keys: usize,
    pub jwts: usize,
    pub emails: usize,
    pub blobs: usize,
    pub assignments: usize,
}

impl RedactStats {
    /// Total number of redactions across all classes.
    pub fn total(&self) -> usize {
        self.api_keys + self.private_keys + self.jwts + self.emails + self.blobs + self.assignments
    }
}

/// Extended redaction & rewrite statistics returned by [`scrub_session_with_profile`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubStats {
    pub secrets: RedactStats,
    pub private_ips: usize,
    pub hostnames: usize,
    pub ssh_principals: usize,
    pub path_rewrites: usize,
    pub principal_transformed: bool,
}

impl ScrubStats {
    pub fn total(&self) -> usize {
        self.secrets.total()
            + self.private_ips
            + self.hostnames
            + self.ssh_principals
            + self.path_rewrites
            + usize::from(self.principal_transformed)
    }
}

/// Return a copy of `session` with secrets/PII scrubbed from all message + tool content.
pub fn redact(session: &Session) -> anyhow::Result<Session> {
    Ok(redact_with(session, &RedactOptions::default())?.0)
}

/// Like [`redact`], but with [`RedactOptions`]; also returns [`RedactStats`].
///
/// # Scope
///
/// Scrubbed: the title and stored system prompt; every message (all text/thinking/tool blocks via [`redact_message`], plus
/// each message's harness-specific `extra` map and complete replay record); the session-level `extra` map; and `git.remote` —
/// remotes can embed credentials (`https://x-access-token:ghs_…@github.com/o/r.git`). Note that an
/// scp-style `git@host:…` remote has its `user@host` scrubbed as an email; that's cosmetic, not a
/// leak.
///
/// Not scrubbed: `cwd` and `source_path` (filesystem paths — a placeholder would corrupt them, and
/// path privacy is a separate concern from secret redaction), `model`, ids, timestamps, and
/// `git.branch`/`git.commit` (user-visible refs, not credential carriers). `principal` is also
/// preserved like other identifiers: it is an opaque identity, and its arbitrary object fields
/// carry no cv-defined semantics. Consumers must apply their own identity privacy policy before
/// sharing a principal; secret redaction does not anonymize it.
pub fn redact_with(session: &Session, opts: &RedactOptions) -> anyhow::Result<(Session, RedactStats)> {
    let mut out = session.clone();
    let resolver = session.resolver();
    let mut stats = RedactStats::default();

    if let Some(title) = out.title.as_mut() {
        *title = scrub(title, opts, &mut stats);
    }
    if let Some(prompt) = out.system_prompt.as_mut() {
        if let Cow::Owned(scrubbed) = scrub_cow(prompt, opts, &mut stats) {
            *prompt = scrubbed;
        }
    }
    if let Some(remote) = out.git.as_mut().and_then(|g| g.remote.as_mut()) {
        if let Cow::Owned(scrubbed) = scrub_cow(remote, opts, &mut stats) {
            *remote = scrubbed;
        }
    }
    for (_k, v) in out.extra.iter_mut() {
        scrub_value(v, opts, &mut stats);
    }
    for msg in &mut out.messages {
        redact_message(msg, opts, &mut stats, &resolver)?;
    }

    Ok((out, stats))
}

/// Redact secrets from a single string. The core building block.
pub fn redact_text(s: &str) -> String {
    let mut stats = RedactStats::default();
    scrub(s, &RedactOptions::default(), &mut stats)
}

/// Zero-allocation pre-scan over a single `&str`: returns `true` iff [`scrub_cow`] would redact at
/// least one non-placeholder span under `opts`.
pub fn str_needs_scrub(s: &str, opts: &RedactOptions) -> bool {
    if s.is_empty() {
        return false;
    }
    const MAX_RULES: usize = 16;
    let all = rules();
    debug_assert!(all.len() <= MAX_RULES);
    let mut slots: [Option<(&Rule, Option<Hit>)>; MAX_RULES] = [None; MAX_RULES];
    let mut count = 0usize;
    for r in all {
        if r.kind.enabled(opts) {
            slots[count] = Some((r, r.next_hit(s, 0)));
            count += 1;
        }
    }
    let active = &mut slots[..count];
    let mut cur = 0usize;
    loop {
        for (r, h) in active.iter_mut().flatten() {
            if h.as_ref().is_some_and(|hit| hit.start < cur) {
                *h = r.next_hit(s, cur);
            }
        }
        let Some(hit) = active
            .iter()
            .filter_map(|slot| slot.as_ref().and_then(|(_, h)| *h))
            .min_by_key(|h| h.start)
        else {
            return false;
        };
        cur = hit.end.max(hit.start + 1);
        if hit.kind.label().is_some() {
            return true;
        }
    }
}

/// Zero-allocation pre-scan over a [`crate::json::JsonView`]: checks string leaves in place without
/// building a `serde_json::Value` tree. Source identity and bounds are still verified by the view.
pub fn json_view_needs_scrub(view: crate::json::JsonView<'_>, opts: &RedactOptions) -> anyhow::Result<bool> {
    json_view_any_str(view, &mut |s| str_needs_scrub(s, opts))
}

/// Zero-allocation pre-scan over a [`crate::json::Json`] (`Owned` or `Source`) before calling
/// [`crate::json::Json::make_owned`]. Clean `Json::Source` descriptors stay `Json::Source` with
/// zero heap cloning while still failing on missing or mutated backing files.
pub fn json_needs_scrub(json: &crate::json::Json, opts: &RedactOptions) -> anyhow::Result<bool> {
    json.with_view(|view| json_view_needs_scrub(view, opts))
}

fn json_view_any_str(view: crate::json::JsonView<'_>, pred: &mut dyn FnMut(&str) -> bool) -> anyhow::Result<bool> {
    if view.is_string()? {
        let mut chunks = 0usize;
        let mut matched = false;
        let _ = crate::stream::stopped(view.for_each_str_chunk(|chunk| {
            chunks += 1;
            if chunks == 1 {
                matched = pred(chunk);
                if matched {
                    return Err(crate::stream::Stop.into());
                }
                Ok(())
            } else {
                // Multi-chunk (>32 KiB) string leaf: materialize this single string leaf so
                // patterns spanning chunk boundaries are evaluated identically to owned strings.
                Err(crate::stream::Stop.into())
            }
        }))?;
        if chunks <= 1 {
            return Ok(matched);
        }
        return Ok(view.string()?.is_some_and(|full| pred(&full)));
    }
    if let Some(arr) = view.array()? {
        for item in arr {
            if json_view_any_str(item?, pred)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let mut found = false;
    let _ = crate::stream::stopped(
        view.for_each_entry(|key, val| {
            if pred(key) || json_view_any_str(val, pred)? {
                found = true;
                return Err(crate::stream::Stop.into());
            }
            Ok(())
        })
        .map(|_| ()),
    )?;
    Ok(found)
}

/// Redact secrets from a single message in place. Public so streaming consumers (dataset export) can
/// scrub one message at a time without materializing the whole session.
pub fn redact_message(
    msg: &mut Message,
    opts: &RedactOptions,
    stats: &mut RedactStats,
    resolver: &crate::Resolver,
) -> anyhow::Result<()> {
    // Resolve fallibly and pre-scan every source-backed JSON payload before changing text or
    // statistics. Clean `Json::Source` tool inputs and clean `complete_record`s never allocate an
    // owned `Value` tree; unreadable or mutated sources still fail upfront.
    msg.try_materialize_text(resolver)?;
    let record_dirty = match &msg.complete_record {
        Some(record) => json_needs_scrub(&record.value, opts)?,
        None => false,
    };
    let mut dirty_inputs = Vec::new();
    for (idx, block) in msg.content.iter().enumerate() {
        if let Block::ToolUse { input, .. } = block {
            if json_needs_scrub(input, opts)? {
                dirty_inputs.push(idx);
            }
        }
    }
    for (idx, block) in msg.content.iter_mut().enumerate() {
        match block {
            Block::Text { text } | Block::Thinking { text, .. } => {
                // `scrub_cow` borrows when clean, so untouched text (the common case) is not
                // reallocated — only actually-redacted fields are rewritten.
                if let Cow::Owned(scrubbed) = scrub_cow(text, opts, stats) {
                    *text = scrubbed.into();
                }
            }
            Block::ToolUse { input, .. } => {
                if dirty_inputs.binary_search(&idx).is_ok() {
                    scrub_value(input.make_owned()?, opts, stats);
                }
            }
            Block::ToolResult { content, details, .. } => {
                if let Cow::Owned(scrubbed) = scrub_cow(content, opts, stats) {
                    *content = scrubbed.into();
                }
                if let Some(d) = details {
                    scrub_value(d, opts, stats);
                }
            }
            // A reference is a path or URL (a signed URL carries its credential in the query);
            // inline `data:` bytes are payload, not text, and stay intact.
            Block::Image { data_ref, .. } => {
                if let Some(r) = data_ref.as_mut().filter(|r| !is_inline_data_uri(r)) {
                    if let Cow::Owned(scrubbed) = scrub_cow(r, opts, stats) {
                        *r = scrubbed;
                    }
                }
            }
            Block::File { path, source, .. } => {
                for r in [path, source].into_iter().flatten() {
                    if is_inline_data_uri(r) {
                        continue;
                    }
                    if let Cow::Owned(scrubbed) = scrub_cow(r, opts, stats) {
                        *r = scrubbed;
                    }
                }
            }
        }
    }
    if record_dirty {
        if let Some(record) = &mut msg.complete_record {
            let record = std::sync::Arc::make_mut(record);
            // Redaction invalidates the original replay witness; the scrubbed owned native record
            // remains available to codec comparison without retaining secret-bearing descriptors.
            record.projection.clear();
            scrub_value(record.value.make_owned()?, opts, stats);
        }
    }
    // Harness-specific passthrough fields can carry anything the harness logged (env snapshots,
    // request metadata) — scrub their string leaves too.
    for (_k, v) in msg.extra.iter_mut() {
        scrub_value(v, opts, stats);
    }
    Ok(())
}

/// Apply a [`ScrubProfile`] (filter + secret classes + IP/hostname/SSH scrubbing + `$REPO`/`$HOME`/regex
/// path rewrites + `principal` policy) to `session`. Returns `Ok(None)` without materializing lazy
/// payloads when `profile.filter` excludes the session.
pub fn filter_and_scrub_session(
    session: &Session,
    profile: &ScrubProfile,
) -> anyhow::Result<Option<(Session, ScrubStats)>> {
    if !profile.matches_session(session) {
        return Ok(None);
    }
    let compiled = profile.compile()?;
    compiled.scrub_session(session).map(Some)
}

/// Scrub `session` unconditionally under `profile` (ignoring `profile.filter`).
pub fn scrub_session_with_profile(session: &Session, profile: &ScrubProfile) -> anyhow::Result<(Session, ScrubStats)> {
    profile.compile()?.scrub_session(session)
}

/// Scrub a standalone string under `profile` (using `repo_root` when `rewrite_repo` is set).
pub fn scrub_text_with_profile(s: &str, profile: &ScrubProfile) -> anyhow::Result<String> {
    let compiled = profile.compile()?;
    let mut stats = ScrubStats::default();
    Ok(compiled
        .scrub_text_cow(s, profile.repo_root.as_deref(), &mut stats)
        .into_owned())
}

impl CompiledProfile {
    /// Scrub a session with zero-allocation `JsonView` pre-scanning on clean `ToolUse` inputs.
    pub fn scrub_session(&self, session: &Session) -> anyhow::Result<(Session, ScrubStats)> {
        let mut out = session.clone();
        let resolver = session.resolver();
        let mut stats = ScrubStats::default();
        let repo_root = self.profile.repo_root.as_deref().or(session.cwd.as_deref());

        if let Some(title) = out.title.as_mut() {
            if let Cow::Owned(s) = self.scrub_text_cow(title, repo_root, &mut stats) {
                *title = s;
            }
        }
        if let Some(prompt) = out.system_prompt.as_mut() {
            if let Cow::Owned(s) = self.scrub_text_cow(prompt, repo_root, &mut stats) {
                *prompt = s;
            }
        }
        if let Some(remote) = out.git.as_mut().and_then(|g| g.remote.as_mut()) {
            if let Cow::Owned(s) = self.scrub_text_cow(remote, repo_root, &mut stats) {
                *remote = s;
            }
        }
        if self.profile.rewrite_repo || self.profile.rewrite_home || !self.custom.is_empty() {
            if let Some(cwd) = out.cwd.as_mut() {
                let raw = cwd.to_string_lossy();
                if let Cow::Owned(rewritten) = self.rewrite_paths_cow(&raw, repo_root, &mut stats) {
                    *cwd = PathBuf::from(rewritten);
                }
            }
            if let Some(src) = out.source_path.as_mut() {
                let raw = src.to_string_lossy();
                if let Cow::Owned(rewritten) = self.rewrite_paths_cow(&raw, repo_root, &mut stats) {
                    *src = PathBuf::from(rewritten);
                }
            }
        }

        // Apply PrincipalPolicy.
        match &self.profile.principal {
            PrincipalPolicy::Keep => {
                if let Some(principal) = out.principal.as_mut() {
                    match principal {
                        crate::ir::Principal::String(s) => {
                            if let Cow::Owned(clean) = self.scrub_text_cow(s, repo_root, &mut stats) {
                                *s = clean;
                            }
                        }
                        crate::ir::Principal::Object(map) => {
                            scrub_object_keys(map, |key| self.scrub_text_cow(key, repo_root, &mut stats).into_owned());
                            for (_k, val) in map.iter_mut() {
                                self.scrub_value(val, repo_root, &mut stats);
                            }
                        }
                    }
                }
            }
            PrincipalPolicy::Drop => {
                if out.principal.take().is_some() {
                    stats.principal_transformed = true;
                }
            }
            PrincipalPolicy::Hash { salt } => {
                if let Some(principal) = out.principal.as_mut() {
                    hash_principal(principal, salt.as_deref());
                    stats.principal_transformed = true;
                }
            }
        }

        for (_k, v) in out.extra.iter_mut() {
            self.scrub_value(v, repo_root, &mut stats);
        }
        for msg in &mut out.messages {
            self.scrub_message(msg, repo_root, &mut stats, &resolver)?;
        }
        Ok((out, stats))
    }

    pub fn scrub_message(
        &self,
        msg: &mut Message,
        repo_root: Option<&Path>,
        stats: &mut ScrubStats,
        resolver: &crate::Resolver,
    ) -> anyhow::Result<()> {
        msg.try_materialize_text(resolver)?;
        let record_dirty = match &msg.complete_record {
            Some(record) => record
                .value
                .with_view(|v| json_view_any_str(v, &mut |s| self.text_needs_scrub(s, repo_root)))?,
            None => false,
        };
        let mut dirty_inputs = Vec::new();
        for (idx, block) in msg.content.iter().enumerate() {
            if let Block::ToolUse { input, .. } = block {
                let dirty = input.with_view(|v| json_view_any_str(v, &mut |s| self.text_needs_scrub(s, repo_root)))?;
                if dirty {
                    dirty_inputs.push(idx);
                }
            }
        }
        for (idx, block) in msg.content.iter_mut().enumerate() {
            match block {
                Block::Text { text } | Block::Thinking { text, .. } => {
                    if let Cow::Owned(s) = self.scrub_text_cow(text, repo_root, stats) {
                        *text = s.into();
                    }
                }
                Block::ToolUse { input, .. } => {
                    if dirty_inputs.binary_search(&idx).is_ok() {
                        self.scrub_value(input.make_owned()?, repo_root, stats);
                    }
                }
                Block::ToolResult { content, details, .. } => {
                    if let Cow::Owned(s) = self.scrub_text_cow(content, repo_root, stats) {
                        *content = s.into();
                    }
                    if let Some(d) = details {
                        self.scrub_value(d, repo_root, stats);
                    }
                }
                // File and image references are paths and URLs: exactly what `$HOME`/`$REPO`
                // rewriting and host/IP scrubbing exist for. Inline `data:` bytes stay intact.
                Block::Image { data_ref, .. } => {
                    if let Some(r) = data_ref.as_mut().filter(|r| !is_inline_data_uri(r)) {
                        if let Cow::Owned(scrubbed) = self.scrub_text_cow(r, repo_root, stats) {
                            *r = scrubbed;
                        }
                    }
                }
                Block::File { path, source, .. } => {
                    for r in [path, source].into_iter().flatten() {
                        if is_inline_data_uri(r) {
                            continue;
                        }
                        if let Cow::Owned(scrubbed) = self.scrub_text_cow(r, repo_root, stats) {
                            *r = scrubbed;
                        }
                    }
                }
            }
        }
        if record_dirty {
            if let Some(record) = &mut msg.complete_record {
                let record = std::sync::Arc::make_mut(record);
                record.projection.clear();
                self.scrub_value(record.value.make_owned()?, repo_root, stats);
            }
        }
        for (_k, v) in msg.extra.iter_mut() {
            self.scrub_value(v, repo_root, stats);
        }
        Ok(())
    }

    fn scrub_value(&self, v: &mut Value, repo_root: Option<&Path>, stats: &mut ScrubStats) {
        match v {
            Value::String(s) => {
                if let Cow::Owned(scrubbed) = self.scrub_text_cow(s, repo_root, stats) {
                    *s = scrubbed;
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    self.scrub_value(item, repo_root, stats);
                }
            }
            Value::Object(map) => {
                scrub_object_keys(map, |key| self.scrub_text_cow(key, repo_root, stats).into_owned());
                for val in map.values_mut() {
                    self.scrub_value(val, repo_root, stats);
                }
            }
            _ => {}
        }
    }

    pub fn text_needs_scrub(&self, s: &str, repo_root: Option<&Path>) -> bool {
        if s.is_empty() {
            return false;
        }
        let secret_opts = RedactOptions {
            classes: self.profile.classes,
        };
        if str_needs_scrub(s, &secret_opts) {
            return true;
        }
        let mut probe = ScrubStats::default();
        matches!(
            self.scrub_network_and_paths_cow(s, repo_root, &mut probe),
            Cow::Owned(_)
        )
    }

    pub fn scrub_text_cow<'a>(&self, s: &'a str, repo_root: Option<&Path>, stats: &mut ScrubStats) -> Cow<'a, str> {
        let secret_opts = RedactOptions {
            classes: self.profile.classes,
        };
        let step1 = scrub_cow(s, &secret_opts, &mut stats.secrets);
        match step1 {
            Cow::Borrowed(clean) => self.scrub_network_and_paths_cow(clean, repo_root, stats),
            Cow::Owned(mut changed) => {
                if let Cow::Owned(next) = self.scrub_network_and_paths_cow(&changed, repo_root, stats) {
                    changed = next;
                }
                Cow::Owned(changed)
            }
        }
    }

    fn scrub_network_and_paths_cow<'a>(
        &self,
        s: &'a str,
        repo_root: Option<&Path>,
        stats: &mut ScrubStats,
    ) -> Cow<'a, str> {
        let mut current: Cow<'a, str> = Cow::Borrowed(s);
        if self.profile.ssh_principals {
            if let Cow::Owned(next) = scrub_ssh_principals_cow(&current, &mut stats.ssh_principals) {
                current = Cow::Owned(next);
            }
        }
        if self.profile.private_ips {
            if let Cow::Owned(next) = scrub_private_ips_cow(&current, &mut stats.private_ips) {
                current = Cow::Owned(next);
            }
        }
        if self.profile.hostnames {
            if let Cow::Owned(next) = scrub_internal_hostnames_cow(&current, &mut stats.hostnames) {
                current = Cow::Owned(next);
            }
        }
        if self.profile.rewrite_repo || self.profile.rewrite_home || !self.custom.is_empty() {
            if let Cow::Owned(next) = self.rewrite_paths_cow(&current, repo_root, stats) {
                current = Cow::Owned(next);
            }
        }
        current
    }

    fn rewrite_paths_cow<'a>(&self, s: &'a str, repo_root: Option<&Path>, stats: &mut ScrubStats) -> Cow<'a, str> {
        let mut current: Cow<'a, str> = Cow::Borrowed(s);

        // 1. $REPO first (more specific than $HOME when repo sits inside $HOME).
        if self.profile.rewrite_repo {
            if let Some(repo) = repo_root {
                let repo_str = repo.to_string_lossy();
                let trimmed = repo_str.trim_end_matches(['/', '\\']);
                if let Cow::Owned(rewritten) = rewrite_path_prefix(&current, trimmed, "$REPO", &mut stats.path_rewrites)
                {
                    current = Cow::Owned(rewritten);
                }
            }
        }

        // 2. $HOME (explicit env home first, then canonical /Users/<u...>, /home/<u...>, C:\Users\<u...>).
        if self.profile.rewrite_home {
            if let Some(home) = self.env_home.as_deref() {
                let trimmed = home.trim_end_matches(['/', '\\']);
                if let Cow::Owned(rewritten) = rewrite_path_prefix(&current, trimmed, "$HOME", &mut stats.path_rewrites)
                {
                    current = Cow::Owned(rewritten);
                }
            }
            let re = home_path_regex();
            if re.is_match(&current) {
                let mut count = 0usize;
                let replaced = re
                    .replace_all(&current, |caps: &Captures| {
                        count += 1;
                        format!("{}$HOME", &caps[1])
                    })
                    .into_owned();
                if count > 0 {
                    stats.path_rewrites += count;
                    current = Cow::Owned(replaced);
                }
            }
        }

        // 3. Custom regex rules in declared order.
        // Environment-style placeholders (`$NAS`, `$WORK`, `$SCRATCH`) stay literal unless they
        // match a numbered capture (`$1`) or an explicit named capture in `re`.
        for (re, replacement) in &self.custom {
            if re.is_match(&current) {
                let mut count = 0usize;
                let next = if re.captures_len() <= 1 && !replacement.contains("$$") {
                    count = re.find_iter(&current).count();
                    re.replace_all(&current, regex::NoExpand(replacement.as_str()))
                        .into_owned()
                } else {
                    re.replace_all(&current, |caps: &Captures| {
                        count += 1;
                        expand_path_replacement(caps, re, replacement)
                    })
                    .into_owned()
                };
                if next != current.as_ref() {
                    stats.path_rewrites += count;
                    current = Cow::Owned(next);
                }
            }
        }

        current
    }
}

fn expand_path_replacement(caps: &Captures, re: &Regex, replacement: &str) -> String {
    let bytes = replacement.as_bytes();
    let mut out = String::with_capacity(replacement.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let ch = replacement[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if i + 1 >= bytes.len() {
            out.push('$');
            break;
        }
        if bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        let (token, end, raw_span) = if bytes[i + 1] == b'{' {
            if let Some(rel_close) = replacement[i + 2..].find('}') {
                let close = i + 2 + rel_close;
                (&replacement[i + 2..close], close + 1, &replacement[i..=close])
            } else {
                out.push('$');
                i += 1;
                continue;
            }
        } else {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j == i + 1 {
                out.push('$');
                i += 1;
                continue;
            }
            (&replacement[i + 1..j], j, &replacement[i..j])
        };

        if let Ok(idx) = token.parse::<usize>() {
            if idx < caps.len() {
                if let Some(m) = caps.get(idx) {
                    out.push_str(m.as_str());
                }
                i = end;
                continue;
            }
        } else if re.capture_names().flatten().any(|n| n == token) {
            if let Some(m) = caps.name(token) {
                out.push_str(m.as_str());
            }
            i = end;
            continue;
        }

        out.push_str(raw_span);
        i = end;
    }
    out
}

fn hash_principal_str(raw: &str, salt: Option<&str>) -> String {
    if let Some(rest) = raw.strip_prefix("sha256:") {
        if rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return raw.to_string();
        }
    }
    let mut material = Vec::with_capacity(salt.map_or(0, str::len) + 1 + raw.len());
    if let Some(salt) = salt {
        material.extend_from_slice(salt.as_bytes());
        material.push(b':');
    }
    material.extend_from_slice(raw.as_bytes());
    format!("sha256:{}", crate::digest::sha256_hex(&material))
}

fn hash_principal(p: &mut crate::ir::Principal, salt: Option<&str>) {
    match p {
        crate::ir::Principal::String(s) => *s = hash_principal_str(s, salt),
        crate::ir::Principal::Object(map) => {
            for (_k, val) in map.iter_mut() {
                hash_principal_value(val, salt);
            }
        }
    }
}

fn hash_principal_value(v: &mut Value, salt: Option<&str>) {
    match v {
        Value::String(s) => *s = hash_principal_str(s, salt),
        Value::Array(items) => {
            for item in items {
                hash_principal_value(item, salt);
            }
        }
        Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                hash_principal_value(val, salt);
            }
        }
        other => {
            let encoded = other.to_string();
            *other = Value::String(hash_principal_str(&encoded, salt));
        }
    }
}

/// `data:` URIs carry inline payload bytes (often megabytes of base64), not a path or URL.
fn is_inline_data_uri(reference: &str) -> bool {
    reference
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

/// A user home (`/Users/<u>`, `/home/<u>`, `C:\Users\<u>`) that *starts* a path: at the start of
/// the text, after `file://`, or after any character that cannot continue a path or name. That
/// keeps `example.com/home/page` and `/var/home/x` intact while catching a home behind any
/// delimiter (backtick, `<`, `>`, `{`, `|`, CJK brackets, ...), not just an allow-list of them. The
/// user name runs to the next separator or delimiter and nothing after it is consumed, so two
/// homes separated by a single character are both rewritten.
fn rewrite_path_prefix<'a>(text: &'a str, prefix: &str, replacement: &str, count: &mut usize) -> Cow<'a, str> {
    if prefix.len() <= 1 {
        return Cow::Borrowed(text);
    }
    let path_char = |c: char| c.is_alphanumeric() || "._~-".contains(c);
    let mut out = String::new();
    let mut copied = 0;
    for (start, _) in text.match_indices(prefix) {
        let end = start + prefix.len();
        let before = &text[..start];
        let after = &text[end..];
        if !(before.is_empty()
            || before.ends_with("file://")
            || before
                .chars()
                .next_back()
                .is_some_and(|c| !path_char(c) && c != '/' && c != '\\'))
            || after.chars().next().is_some_and(path_char)
        {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str(replacement);
        copied = end;
        *count += 1;
    }
    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        out.push_str(&text[copied..]);
        Cow::Owned(out)
    }
}

// Renaming keys must not silently discard a second entry whose name scrubs identically.
pub(crate) fn scrub_object_keys(map: &mut serde_json::Map<String, Value>, mut scrub: impl FnMut(&str) -> String) {
    let original = std::mem::take(map);
    for (key, value) in original {
        let base = scrub(&key);
        let mut key = base.clone();
        let mut suffix = 2;
        while map.contains_key(&key) {
            key = format!("{base}#{suffix}");
            suffix += 1;
        }
        map.insert(key, value);
    }
}

fn home_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(^|[^A-Za-z0-9._~/\\-]|file://)(?:/Users/|/home/|[A-Za-z]:\\Users\\)[^/\\\s"'`<>|:;,(){}\[\]]+"#)
            .expect("home path regex")
    })
}

fn scrub_private_ips_cow<'a>(s: &'a str, count: &mut usize) -> Cow<'a, str> {
    static IPV4_RE: OnceLock<Regex> = OnceLock::new();
    static IPV6_RE: OnceLock<Regex> = OnceLock::new();
    let v4 = IPV4_RE.get_or_init(|| Regex::new(r"\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b").expect("ipv4 regex"));
    let v6 = IPV6_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:fd[0-9a-f]{2}|fc[0-9a-f]{2}|fe80):[0-9a-f:]{2,}\b").expect("ipv6 ula regex")
    });

    let mut out: Option<String> = None;
    let mut copied = 0usize;
    for caps in v4.captures_iter(s) {
        let m = caps.get(0).unwrap();
        let (start, end) = (m.start(), m.end());
        if start > 0 && matches!(s.as_bytes()[start - 1], b'.' | b'v' | b'V') {
            continue;
        }
        if end < s.len() && s.as_bytes()[end] == b'.' && end + 1 < s.len() && s.as_bytes()[end + 1].is_ascii_digit() {
            continue;
        }
        let octets: Option<[u8; 4]> = (|| {
            Some([
                caps[1].parse().ok()?,
                caps[2].parse().ok()?,
                caps[3].parse().ok()?,
                caps[4].parse().ok()?,
            ])
        })();
        let Some([a, b, _, _]) = octets else { continue };
        let is_private = a == 10
            || (a == 172 && (16..=31).contains(&b))
            || (a == 192 && b == 168)
            || (a == 100 && (64..=127).contains(&b));
        if !is_private {
            continue;
        }
        let buf = out.get_or_insert_with(String::new);
        buf.push_str(&s[copied..start]);
        buf.push_str("[REDACTED:ip]");
        *count += 1;
        copied = end;
    }
    let intermediate: Cow<'a, str> = match out {
        None => Cow::Borrowed(s),
        Some(mut buf) => {
            buf.push_str(&s[copied..]);
            Cow::Owned(buf)
        }
    };
    if v6.is_match(&intermediate) {
        let hits = v6.find_iter(&intermediate).count();
        *count += hits;
        Cow::Owned(v6.replace_all(&intermediate, "[REDACTED:ip]").into_owned())
    } else {
        intermediate
    }
}

fn scrub_internal_hostnames_cow<'a>(s: &'a str, count: &mut usize) -> Cow<'a, str> {
    static HOST_RE: OnceLock<Regex> = OnceLock::new();
    let re = HOST_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:local|internal|lan|corp|home\.arpa|ts\.net|localdomain)\b",
        )
        .expect("internal hostname regex")
    });
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    for m in re.find_iter(s) {
        if m.start() > 0 && s.as_bytes()[m.start() - 1] == b'@' {
            continue;
        }
        let buf = out.get_or_insert_with(String::new);
        buf.push_str(&s[copied..m.start()]);
        buf.push_str("[REDACTED:host]");
        *count += 1;
        copied = m.end();
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut buf) => {
            buf.push_str(&s[copied..]);
            Cow::Owned(buf)
        }
    }
}

fn scrub_ssh_principals_cow<'a>(s: &'a str, count: &mut usize) -> Cow<'a, str> {
    static SSH_RE: OnceLock<Regex> = OnceLock::new();
    let re = SSH_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:((?:git\+)?ssh://)|(\b(?:ssh|scp|sftp|rsync)\s+(?:-[A-Za-z0-9]+\s+)*))([A-Za-z0-9._-]+@[A-Za-z0-9._-]+)"#,
        )
        .expect("ssh target regex")
    });
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    for caps in re.captures_iter(s) {
        let target = caps.get(3).unwrap();
        // Public forge git@github.com / git@gitlab.com is not a personal SSH principal.
        let lower = target.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "git@github.com" | "git@gitlab.com" | "git@bitbucket.org" | "git@codeberg.org"
        ) {
            continue;
        }
        let buf = out.get_or_insert_with(String::new);
        buf.push_str(&s[copied..target.start()]);
        buf.push_str("[REDACTED:ssh]");
        *count += 1;
        copied = target.end();
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut buf) => {
            buf.push_str(&s[copied..]);
            Cow::Owned(buf)
        }
    }
}

/// Recursively scrub JSON strings and object keys. Colliding scrubbed keys retain every value.
/// Crate-visible so per-block redacting renderers (dataset export) can scrub a tool input without
/// going through [`redact_message`] on a cloned message.
pub(crate) fn scrub_value(v: &mut Value, opts: &RedactOptions, stats: &mut RedactStats) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(scrubbed) = scrub_cow(s, opts, stats) {
                *s = scrubbed;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_value(item, opts, stats);
            }
        }
        Value::Object(map) => {
            scrub_object_keys(map, |key| scrub_cow(key, opts, stats).into_owned());
            for val in map.values_mut() {
                scrub_value(val, opts, stats);
            }
        }
        // numbers, bools, null: nothing to scrub.
        _ => {}
    }
}

const PLACEHOLDER_OPEN: &str = "[REDACTED:";

// ---------------------------------------------------------------------------
// Core scanner
// ---------------------------------------------------------------------------

/// Walk `s` once, replacing every recognized secret span with a placeholder.
fn scrub(s: &str, opts: &RedactOptions, stats: &mut RedactStats) -> String {
    scrub_cow(s, opts, stats).into_owned()
}

/// [`scrub`], allocation-aware: returns `Cow::Borrowed(s)` when nothing matched (the overwhelmingly
/// common case for ordinary transcript text), and copies clean stretches between matches in bulk.
///
/// Uses a fixed stack state array (no heap allocation on clean strings). Each enabled [`Rule`] keeps
/// its earliest guarded hit at or after the cursor; the leftmost hit wins, and at a tie the earlier
/// rule (the order of [`rules`]) wins. An existing placeholder is a rule too (the first), consumed
/// verbatim, which is what makes the transform idempotent.
pub(crate) fn scrub_cow<'a>(s: &'a str, opts: &RedactOptions, stats: &mut RedactStats) -> Cow<'a, str> {
    if s.is_empty() {
        return Cow::Borrowed(s);
    }
    const MAX_RULES: usize = 16;
    let all = rules();
    debug_assert!(all.len() <= MAX_RULES);
    let mut slots: [Option<(&Rule, Option<Hit>)>; MAX_RULES] = [None; MAX_RULES];
    let mut count = 0usize;
    for r in all {
        if r.kind.enabled(opts) {
            slots[count] = Some((r, r.next_hit(s, 0)));
            count += 1;
        }
    }
    let active = &mut slots[..count];
    // The redacted output, allocated lazily on the first match; `copied` is how much of `s` has
    // already been flushed into it.
    let (mut out, mut copied, mut cur) = (None::<String>, 0, 0);
    loop {
        for (r, h) in active.iter_mut().flatten() {
            if h.as_ref().is_some_and(|h| h.start < cur) {
                *h = r.next_hit(s, cur);
            }
        }
        let Some(hit) = active
            .iter()
            .filter_map(|slot| slot.as_ref().and_then(|(_, h)| *h))
            .min_by_key(|h| h.start)
        else {
            break;
        };
        let Hit { start, keep, end, kind } = hit;
        cur = end.max(start + 1);
        let Some(label) = kind.label() else { continue }; // a placeholder: leave it in place
        let o = out.get_or_insert_with(String::new);
        // The clean stretch plus any prefix the rule preserves (e.g. `Authorization: `).
        o.push_str(&s[copied..keep]);
        o.push_str(PLACEHOLDER_OPEN);
        o.push_str(label);
        o.push(']');
        kind.bump(stats);
        copied = end;
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut o) => {
            o.push_str(&s[copied..]);
            Cow::Owned(o)
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// An existing `[REDACTED:…]` placeholder, kept as is.
    Placeholder,
    ApiKey,
    PrivateKey,
    Jwt,
    Email,
    Blob,
    Assignment,
}

impl Kind {
    fn enabled(self, opts: &RedactOptions) -> bool {
        let c = &opts.classes;
        match self {
            Kind::Placeholder => true,
            Kind::PrivateKey => c.private_keys,
            Kind::Assignment => c.assignments,
            Kind::ApiKey => c.api_keys,
            Kind::Jwt => c.jwts,
            Kind::Email => c.emails,
            Kind::Blob => c.blobs,
        }
    }
    fn label(self) -> Option<&'static str> {
        Some(match self {
            Kind::Placeholder => return None,
            Kind::ApiKey => "api_key",
            Kind::PrivateKey => "private_key",
            Kind::Jwt => "jwt",
            Kind::Email => "email",
            Kind::Blob | Kind::Assignment => "secret",
        })
    }
    fn bump(self, stats: &mut RedactStats) {
        match self {
            Kind::Placeholder => {}
            Kind::ApiKey => stats.api_keys += 1,
            Kind::PrivateKey => stats.private_keys += 1,
            Kind::Jwt => stats.jwts += 1,
            Kind::Email => stats.emails += 1,
            Kind::Blob => stats.blobs += 1,
            Kind::Assignment => stats.assignments += 1,
        }
    }
}

/// A guarded match: replace `[keep, end)` with a placeholder, keeping `[start, keep)` verbatim.
#[derive(Clone, Copy)]
struct Hit {
    start: usize,
    keep: usize,
    end: usize,
    kind: Kind,
}

/// A candidate pattern plus the guard that accepts or rejects each candidate. A rejected
/// candidate is retried one character later, so a guard sees every start a char-by-char scan
/// would have tried.
struct Rule {
    kind: Kind,
    re: Regex,
    /// `Some((keep, end))` (absolute offsets in `s`) accepts the candidate.
    guard: fn(&str, &Captures) -> Option<(usize, usize)>,
}

impl Rule {
    /// The earliest accepted hit at or after `from`.
    fn next_hit(&self, s: &str, mut from: usize) -> Option<Hit> {
        if self.kind == Kind::Blob {
            return next_blob(&self.re, s, from);
        }
        while from <= s.len() {
            let c = self.re.captures_at(s, from)?;
            let start = c.get(0)?.start();
            if let Some((keep, end)) = (self.guard)(s, &c) {
                return Some(Hit {
                    start,
                    keep,
                    end,
                    kind: self.kind,
                });
            }
            from = start + s[start..].chars().next().map_or(1, char::len_utf8);
        }
        None
    }
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_base64_std_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
}

/// True if the byte just before `i` is not part of a token (so we match at a boundary).
fn boundary_before(s: &str, i: usize) -> bool {
    i == 0 || !is_token_char(s.as_bytes()[i - 1])
}

/// The whole match, at a token boundary.
fn whole(s: &str, c: &Captures) -> Option<(usize, usize)> {
    let m = c.get(0)?;
    boundary_before(s, m.start()).then_some((m.start(), m.end()))
}

/// Group `g` as the redacted value (the rest of the match is kept), unless already redacted.
fn value(c: &Captures, g: usize) -> Option<(usize, usize)> {
    let v = c.get(g)?;
    (!v.as_str().starts_with(PLACEHOLDER_OPEN)).then_some((v.start(), v.end()))
}

/// Known credential prefixes with the minimum tail each needs, and whether the tail is strictly
/// alphanumeric. Min tails sit safely below each family's real length so keys aren't missed, but
/// high enough that prose and identifiers don't trip them. Short prefixes that collide with
/// ordinary identifiers (`npm_config_registry`, `hf_hub_download`) take an alphanumeric tail:
/// real tails are pure base62, so stopping at `_`/`-` rejects the identifier. Order matters only
/// for prefix-of-prefix pairs (`sk-ant-` before `sk-`).
const PREFIXES: &[(&str, usize, bool)] = &[
    ("sk-ant-", 10, false),
    ("sk-", 10, false),
    ("sk_live_", 20, true), // Stripe secret key (live)
    ("rk_live_", 20, true), // Stripe restricted key (live)
    ("xoxb-", 8, false),
    ("xoxp-", 8, false),
    ("xoxa-", 8, false),
    ("xoxr-", 8, false),
    ("xoxc-", 8, false), // Slack client token
    ("xoxs-", 8, false), // Slack session token
    ("xapp-", 8, false), // Slack app-level token
    ("ghp_", 20, false),
    ("gho_", 20, false),
    ("ghs_", 20, false),
    ("ghu_", 20, false),
    ("ghr_", 20, false),
    ("github_pat_", 20, false),
    ("glpat-", 20, false), // GitLab personal access token
    ("npm_", 36, true),    // npm: npm_ + exactly 36 base62
    ("hf_", 30, true),     // Hugging Face: hf_ + 34 base62
    ("gsk_", 40, true),    // Groq: gsk_ + 52 base62
    ("xai-", 40, true),    // xAI: xai- + ~80 base62
    ("dop_v1_", 40, true), // DigitalOcean: dop_v1_ + 64 hex
    ("shpat_", 32, true),  // Shopify private app token: + 32 hex
    ("shpss_", 32, true),  // Shopify shared secret: + 32 hex
    ("AIza", 30, false),
];

/// The recognizers, in priority order at a shared start: placeholders, private keys (they swallow
/// a block), assignments and URL passwords (which keep a prefix), headers and bare tokens, JWTs,
/// emails, then conservative blobs.
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let rule = |kind, pat: &str, guard| Rule {
            kind,
            re: Regex::new(pat).expect("redact pattern"),
            guard,
        };
        let prefixes: Vec<String> = PREFIXES.iter().map(|(p, _, _)| regex::escape(p)).collect();
        vec![
            rule(Kind::Placeholder, r"\[REDACTED:[^\]]*\]", |_, c| {
                let m = c.get(0)?;
                Some((m.start(), m.end()))
            }),
            // `-----BEGIN … PRIVATE KEY-----` through its END marker. Transcripts routinely clip
            // tool output, losing the END line; a truncated key is still a key, so without one
            // the header plus the base64 body that follows is redacted.
            rule(Kind::PrivateKey, r"-----BEGIN ", |s, c| {
                let start = c.get(0)?.start();
                let after = &s[start..];
                let close = after[5..].find("-----")? + 10;
                after[..close].contains("PRIVATE KEY").then_some(())?;
                let from = start + close;
                let footer = s[from..].match_indices("-----END ").find_map(|(offset, _)| {
                    let marker = &s[from + offset..];
                    let close = marker[5..].find("-----")? + 10;
                    marker[..close].contains("PRIVATE KEY").then_some(from + offset + close)
                });
                let end = match footer {
                    Some(end) => end,
                    None => truncated_pem_body_end(s, from)?,
                };
                Some((start, end))
            }),
            // `key = "value"` / `key: value` for an allowlist of secret-ish keys; the value goes,
            // `key = "` and the closing quote stay. Skips calls (`token = get_token()`), short
            // bare words (`password: use a strong one`), an unterminated quote, and values under
            // three characters.
            rule(
                Kind::Assignment,
                r#"(?i)(?:password|passwd|secret|api_key|apikey|api-key|access_token|secret_key|client_secret|auth_token|token)[ \t]*[=:][ \t]*(?:"([^"]*)"|'([^']*)'|([^ \t\r\n,;)]+))"#,
                |s, c| {
                    boundary_before(s, c.get(0)?.start()).then_some(())?;
                    let v = match c.get(1).or_else(|| c.get(2)) {
                        Some(quoted) => quoted,
                        None => {
                            let v = c.get(3)?;
                            let t = v.as_str().as_bytes();
                            if t.contains(&b'(') || t[0] == b'"' || t[0] == b'\'' {
                                return None;
                            }
                            if t.len() < 8 && t.iter().all(u8::is_ascii_alphabetic) {
                                return None;
                            }
                            v
                        }
                    };
                    (v.len() >= 3 && !v.as_str().starts_with(PLACEHOLDER_OPEN)).then_some((v.start(), v.end()))
                },
            ),
            // `scheme://user:password@host`: the password goes, scheme, user and host stay. A port
            // (`https://host:8080/path`) never matches, and an env-var reference (`${DB_PASS}`)
            // is not a secret.
            rule(
                Kind::Assignment,
                r#"[A-Za-z][A-Za-z0-9+.-]*://[^:@/ \t\r\n"']*:([^$@/ \t\r\n"'][^@/ \t\r\n"']*)@"#,
                |s, c| {
                    boundary_before(s, c.get(0)?.start()).then_some(())?;
                    value(c, 1)
                },
            ),
            // `Authorization: <value>` to end of line; `Proxy-Authorization:` too, not
            // `FooAuthorization:`.
            rule(Kind::ApiKey, r"(?i)authorization:[ \t]*([^ \t\r\n][^\r\n]*)", |s, c| {
                let i = c.get(0)?.start();
                (i == 0 || !matches!(s.as_bytes()[i - 1], b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_'))
                    .then_some(())?;
                value(c, 1)
            }),
            // `Bearer <token>`, any case (RFC 7235). Prose guard: "bearer instruments are…" —
            // real tokens are long or contain digits/symbols.
            rule(Kind::ApiKey, r"(?i)bearer +([A-Za-z0-9_.-]{8,})", |s, c| {
                let t = c.get(1)?.as_str();
                (t.len() >= 16 || !t.bytes().all(|b| b.is_ascii_alphabetic())).then_some(())?;
                whole(s, c)
            }),
            // AWS access key ids (the whole uppercase run, so an overlong key leaves no tail),
            // then the prefix table.
            rule(
                Kind::ApiKey,
                &format!(r"AKIA[A-Z0-9]{{16,}}|(?:{})[A-Za-z0-9_-]*", prefixes.join("|")),
                |s, c| {
                    let m = c.get(0)?;
                    if m.as_str().starts_with("AKIA") {
                        return whole(s, c);
                    }
                    let (p, min, alnum) = PREFIXES.iter().find(|(p, _, _)| m.as_str().starts_with(p))?;
                    let tail = &m.as_str()[p.len()..];
                    let n = if *alnum {
                        tail.bytes().take_while(u8::is_ascii_alphanumeric).count()
                    } else {
                        tail.len()
                    };
                    (n >= *min && boundary_before(s, m.start())).then_some((m.start(), m.start() + p.len() + n))
                },
            ),
            // JWT: three base64url segments (the header starts `eyJ`, base64 of `{"`), 40+ chars.
            rule(
                Kind::Jwt,
                r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}",
                |s, c| {
                    let m = c.get(0)?;
                    (m.len() >= 40 && m.as_str().find('.')? >= 4).then_some(())?;
                    whole(s, c)
                },
            ),
            // localpart@domain.tld, the TLD two or more letters.
            rule(Kind::Email, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]*\.[A-Za-z0-9.-]*", |s, c| {
                let m = c.get(0)?;
                let local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
                (m.start() == 0 || !local(s.as_bytes()[m.start() - 1])).then_some(())?;
                let t = m.as_str().trim_end_matches('.');
                let tld = t.rsplit('.').next()?;
                (tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic())).then_some((m.start(), m.start() + t.len()))
            }),
            // Runs of base64/hex characters; see [`next_blob`].
            rule(Kind::Blob, r"[A-Za-z0-9+/=_-]{32,}", |_, _| None),
        ]
    })
}

/// A long, *pure* hex (≥ 32) or base64 (≥ 40) run with a secret-ish keyword within ~48 chars
/// before it — commit hashes, identifiers and base64 data that isn't a credential survive. Every
/// token boundary inside a run is a candidate start (after a `/`, `+` or `=`), so the purity of
/// each suffix is computed once per run: a per-start re-measure was quadratic, 21 s on one 8 MB
/// transcript.
fn next_blob(re: &Regex, s: &str, mut from: usize) -> Option<Hit> {
    let b = s.as_bytes();
    loop {
        let m = re.find_at(s, from)?;
        let (start, end) = (m.start(), m.end());
        // Each predicate stays true for every suffix beginning after its last invalid byte.
        // Retain three offsets, not a tuple for every byte of a potentially giant clean token.
        let mut pure_from = [start; 3];
        for (q, &c) in b.iter().enumerate().take(end).skip(start) {
            for (index, valid) in [c.is_ascii_hexdigit(), is_token_char(c), is_base64_std_char(c)]
                .into_iter()
                .enumerate()
            {
                if !valid {
                    pure_from[index] = q + 1;
                }
            }
        }
        for p in start..end.saturating_sub(31) {
            let [h, u, d] = pure_from.map(|from| p >= from);
            if (h || u || d) && end - p >= if h { 32 } else { 40 } && boundary_before(s, p) && keyword_before(s, p, 48)
            {
                return Some(Hit {
                    start: p,
                    keep: p,
                    end,
                    kind: Kind::Blob,
                });
            }
        }
        from = end;
    }
}

/// End offset of the contiguous PEM body following a truncated `-----BEGIN …-----` header at
/// `body_start`, or `None` if no body follows (a bare marker in prose stays untouched).
///
/// A body line is a whole line of base64 characters. To avoid eating prose after a marker
/// mention, the first body lines must be ≥ 16 chars; once one real body line is seen, a single
/// shorter trailing base64 fragment (the clipped tail) is also consumed. Encrypted-PEM header
/// lines (`Proc-Type: …`, `DEK-Info: …` — the latter carries the IV) before the body are
/// consumed too.
fn truncated_pem_body_end(s: &str, body_start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut pos = body_start;
    let mut end: Option<usize> = None;
    let mut seen_body = false;
    while pos < bytes.len() {
        // Skip line terminators between lines.
        while pos < bytes.len() && (bytes[pos] == b'\n' || bytes[pos] == b'\r') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let line_start = pos;
        let mut line_end = pos;
        while line_end < bytes.len() && bytes[line_end] != b'\n' && bytes[line_end] != b'\r' {
            line_end += 1;
        }
        let line = s[line_start..line_end].trim();
        if line.is_empty() {
            pos = line_end;
            continue;
        }
        if line.bytes().all(is_base64_std_char) {
            if line.len() >= 16 {
                seen_body = true;
                end = Some(line_end);
                pos = line_end;
                continue;
            }
            if seen_body {
                // A short base64 fragment right after real body lines: the clipped tail.
                end = Some(line_end);
            }
            break;
        }
        // Encrypted-PEM header line ("Proc-Type: 4,ENCRYPTED" / "DEK-Info: AES-128-CBC,…")
        // before the body: key has no spaces, then ": ".
        if !seen_body {
            if let Some(colon) = line.find(": ") {
                if !line[..colon].contains(' ') && line.len() < 64 {
                    end = Some(line_end);
                    pos = line_end;
                    continue;
                }
            }
        }
        break;
    }
    if seen_body {
        end
    } else {
        None
    }
}

/// Does a secret-ish keyword appear in the `window` chars (roughly) before `i`?
fn keyword_before(s: &str, i: usize, window: usize) -> bool {
    let mut start = i.saturating_sub(window);
    // Snap `start` back to a char boundary so slicing never panics on multi-byte UTF-8.
    while start > 0 && !s.is_char_boundary(start) {
        start -= 1;
    }
    let ctx = s[start..i].to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "secret",
        "token",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "key",
        "credential",
        "auth",
    ];
    NEEDLES.iter().any(|n| ctx.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, GitInfo, Harness, Message, Role, Session};

    fn empty_session() -> Session {
        Session {
            ..Session::new("t", Harness::Claude)
        }
    }

    fn redact_one(s: &str) -> String {
        redact_text(s)
    }

    #[test]
    fn redacts_openai_key() {
        let out = redact_one("my key is sk-abcDEF1234567890ghijkl done");
        assert!(!out.contains("sk-abcDEF1234567890ghijkl"), "got: {out}");
        assert!(out.contains("[REDACTED:api_key]"), "got: {out}");
        assert!(out.starts_with("my key is "));
        assert!(out.ends_with(" done"));
    }

    #[test]
    fn redacts_anthropic_key() {
        let out = redact_one("sk-ant-api03-aaaaaaaaaaaaaaaaaaaabbbb");
        assert_eq!(out, "[REDACTED:api_key]");
    }

    #[test]
    fn redacts_slack_tokens() {
        for t in ["xoxb-1234-5678-abcdEFGHijklmnop", "xoxp-1111-2222-zzzzzzzzzzzz"] {
            let out = redact_one(t);
            assert_eq!(out, "[REDACTED:api_key]", "token {t}");
        }
    }

    #[test]
    fn redacts_github_tokens() {
        let out = redact_one("ghp_0123456789abcdefABCDEF0123456789abcd");
        assert_eq!(out, "[REDACTED:api_key]");
        let out2 = redact_one("gho_0123456789abcdefABCDEF0123456789abcd");
        assert_eq!(out2, "[REDACTED:api_key]");
    }

    #[test]
    fn redacts_aws_access_key() {
        let out = redact_one("AKIAIOSFODNN7EXAMPLE rest");
        assert!(out.starts_with("[REDACTED:api_key]"), "got: {out}");
        assert!(out.ends_with(" rest"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_google_key() {
        let out = redact_one("AIzaSyA1234567890abcdefghijklmnopqrstuv");
        assert_eq!(out, "[REDACTED:api_key]");
    }

    #[test]
    fn redacts_bearer_and_authorization() {
        let out = redact_one("Bearer abcdef1234567890XYZ");
        assert!(out.contains("[REDACTED:api_key]"), "got: {out}");
        assert!(!out.contains("abcdef1234567890XYZ"));
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_one(jwt);
        assert_eq!(out, "[REDACTED:jwt]", "got: {out}");
    }

    #[test]
    fn redacts_email() {
        let out = redact_one("contact me at jane.doe+spam@example.co.uk please");
        assert!(!out.contains("jane.doe+spam@example.co.uk"), "got: {out}");
        assert!(out.contains("[REDACTED:email]"));
        assert!(out.starts_with("contact me at "));
        assert!(out.ends_with(" please"));
    }

    #[test]
    fn emails_class_off_keeps_emails() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: "ping a@b.com".into(),
        });
        sess.messages.push(m);
        let (out, _stats) = redact_with(
            &sess,
            &RedactOptions {
                classes: RedactClasses {
                    emails: false,
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let t = out.messages[0].text().unwrap();
        assert!(t.contains("a@b.com"), "got: {t}");
    }

    #[test]
    fn scoped_classes_only_private_keys() {
        // The exact ask: scrub PEM blocks, leave email/identity and api-key fixtures untouched.
        let opts = RedactOptions {
            classes: RedactClasses::parse_only("private_key").unwrap(),
        };
        let text = "mail me@example.com key sk-abcDEF1234567890ghijkl\n-----BEGIN PRIVATE KEY-----\nMIIBVgIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----\ndone";
        let mut stats = RedactStats::default();
        let out = scrub_cow(text, &opts, &mut stats);
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
        assert!(out.contains("me@example.com"), "email must survive: {out}");
        assert!(out.contains("sk-abcDEF1234567890ghijkl"), "api key must survive: {out}");
        assert_eq!(stats.private_keys, 1);
        assert_eq!(stats.emails, 0);
        assert_eq!(stats.api_keys, 0);
    }

    #[test]
    fn parse_only_rejects_garbage_and_trims_plurals() {
        assert_eq!(
            RedactClasses::parse_only("private_keys, emails").unwrap(),
            RedactClasses {
                private_keys: true,
                emails: true,
                ..RedactClasses::none()
            }
        );
        assert!(RedactClasses::parse_only("nope").is_err());
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890\nabcdefghijklmnopqrstuvwxyz\n-----END RSA PRIVATE KEY-----\nafter";
        let out = redact_one(pem);
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "got: {out}");
        assert!(!out.contains("BEGIN RSA PRIVATE KEY"), "got: {out}");
        assert!(out.starts_with("before\n"));
        assert!(out.ends_with("\nafter"));
    }

    #[test]
    fn redacts_assignment_value() {
        let out = redact_one(r#"password = "hunter2supersecret""#);
        assert!(!out.contains("hunter2supersecret"), "got: {out}");
        assert!(out.contains("[REDACTED:"), "got: {out}");

        let out2 = redact_one(r#"api_key: "abcdEFGH12345678""#);
        assert!(!out2.contains("abcdEFGH12345678"), "got: {out2}");
    }

    #[test]
    fn redacts_keyworded_blob() {
        let out = redact_one("secret: 0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(out.contains("[REDACTED:"), "got: {out}");
        assert!(
            !out.contains("0123456789abcdef0123456789abcdef0123456789abcdef"),
            "got: {out}"
        );
    }

    #[test]
    fn preserves_ordinary_prose_and_code() {
        let inputs = [
            "The quick brown fox jumps over the lazy dog.",
            "let x = compute_hash(); // returns a u64",
            "fn main() { println!(\"hello, world\"); }",
            "Visit https://example.com/path?q=1 for docs.",
            "The commit is abc123 and the file is src/main.rs",
            "tokenize the input and return tokens",
        ];
        for input in inputs {
            let out = redact_one(input);
            assert_eq!(out, input, "ordinary text was altered");
        }
    }

    #[test]
    fn preserves_plain_hex_without_keyword() {
        // A long hex string (e.g. a git blob hash) with NO secret keyword nearby must survive.
        let input = "object 0123456789abcdef0123456789abcdef0123456789abcdef committed";
        let out = redact_one(input);
        assert_eq!(out, input, "got: {out}");
    }

    /// Join a token prefix to its fixture tail at runtime so secret-shaped literals never appear
    /// in the source blob — GitHub push protection (and every downstream clone's scanner) would
    /// flag them as real credentials.
    fn fixture(prefix: &str, tail: &str) -> String {
        format!("{prefix}{tail}")
    }

    #[test]
    fn idempotent() {
        let inputs = [
            "sk-abcDEF1234567890ghijkl".to_string(),
            "email a@b.com and key sk-zzzzzzzzzzzzzzzzzzzz".to_string(),
            "-----BEGIN PRIVATE KEY-----\nAAAA\nBBBB\n-----END PRIVATE KEY-----".to_string(),
            "-----BEGIN PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\nqrstuvwx".to_string(), // truncated
            r#"password = "hunter2supersecret""#.to_string(),
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c".to_string(),
            "postgres://app_user:hunter2secret@db.example.com:5432/prod".to_string(),
            "npm_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
            fixture("glpat-", "abcDEF1234567890ghij"),
            "Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==".to_string(),
            "bearer abcdef1234567890XYZ".to_string(),
            "key AKIAIOSFODNN7EXAMPLEEXTRA9 tail".to_string(),
        ];
        for input in &inputs {
            let once = redact_one(input);
            let twice = redact_one(&once);
            assert_eq!(once, twice, "not idempotent for {input}");
        }
    }

    #[test]
    fn redacts_tool_use_input_values_keeping_structure() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::Assistant);
        let input = serde_json::json!({
            "command": "curl",
            "headers": {
                "Authorization": "Bearer abcdef1234567890XYZ",
                "X-Api-Key": "sk-abcDEF1234567890ghijkl"
            },
            "count": 5,
            "nested": ["sk-ant-api03-aaaaaaaaaaaaaaaaaaaabbbb", "plain text"]
        });
        m.content.push(Block::ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: input.into(),
            namespace: None,
        });
        sess.messages.push(m);

        let out = redact(&sess).unwrap();
        let block = &out.messages[0].content[0];
        let Block::ToolUse { input, .. } = block else {
            panic!("expected tool use");
        };
        let input = input.resolve().unwrap();
        // structure + keys intact
        assert!(input.get("command").is_some());
        assert_eq!(input["command"], "curl");
        assert_eq!(input["count"], 5);
        assert!(input["headers"].get("Authorization").is_some());
        // secret values gone
        let serialized = input.to_string();
        assert!(!serialized.contains("sk-abcDEF1234567890ghijkl"), "got: {serialized}");
        assert!(!serialized.contains("sk-ant-api03"), "got: {serialized}");
        assert!(serialized.contains("[REDACTED:api_key]"), "got: {serialized}");
        // non-secret string preserved
        assert!(serialized.contains("plain text"));
    }

    #[test]
    fn redacts_tool_result_content_and_details() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: "1".into(),
            content: "leaked: sk-abcDEF1234567890ghijkl".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: Some(serde_json::json!({ "stderr": "token ghp_0123456789abcdefABCDEF0123456789abcd" })),
        });
        sess.messages.push(m);

        let out = redact(&sess).unwrap();
        let Block::ToolResult { content, details, .. } = &out.messages[0].content[0] else {
            panic!();
        };
        assert!(!content.contains("sk-abcDEF1234567890ghijkl"), "got: {content}");
        assert!(content.contains("[REDACTED:api_key]"));
        let d = details.as_ref().unwrap().to_string();
        assert!(!d.contains("ghp_0123456789abcdef"), "got: {d}");
    }

    #[test]
    fn redacts_title_and_thinking() {
        let mut sess = empty_session();
        sess.title = Some("debugging sk-abcDEF1234567890ghijkl".into());
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::Thinking {
            text: "the user's key is sk-zzzzzzzzzzzzzzzzzzzz hmm".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        sess.messages.push(m);

        let out = redact(&sess).unwrap();
        assert!(!out.title.as_ref().unwrap().contains("sk-abcDEF"));
        let Block::Thinking { text, .. } = &out.messages[0].content[0] else {
            panic!();
        };
        assert!(!text.contains("sk-zzzz"), "got: {text}");
        assert!(text.starts_with("the user's key is "));
        assert!(text.ends_with(" hmm"));
    }

    #[test]
    fn stats_count() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: "sk-abcDEF1234567890ghijkl and a@b.com".into(),
        });
        sess.messages.push(m);
        let (_out, stats) = redact_with(&sess, &RedactOptions::default()).unwrap();
        assert_eq!(stats.api_keys, 1);
        assert_eq!(stats.emails, 1);
        assert_eq!(stats.total(), 2);
    }

    /// `scrub_cow` is the zero-copy core: clean text comes back `Borrowed` (no allocation),
    /// and dirty text produces exactly what `scrub`/`redact_text` produce.
    #[test]
    fn scrub_cow_borrows_clean_and_matches_scrub_when_dirty() {
        let mut stats = RedactStats::default();
        let clean = "ordinary prose, a commit abc123, and [REDACTED:api_key] already placed";
        match scrub_cow(clean, &RedactOptions::default(), &mut stats) {
            Cow::Borrowed(s) => assert_eq!(s, clean),
            Cow::Owned(s) => panic!("clean text must not allocate, got owned {s:?}"),
        }
        assert_eq!(stats.total(), 0);

        let dirty = "prefix sk-abcDEF1234567890ghijkl middle a@b.com suffix";
        let cow = scrub_cow(dirty, &RedactOptions::default(), &mut stats);
        assert!(matches!(cow, Cow::Owned(_)));
        assert_eq!(cow.as_ref(), redact_text(dirty));
        assert_eq!(stats.total(), 2);
    }

    // --- R1: truncated PEM ---------------------------------------------------

    #[test]
    fn redacts_truncated_private_key() {
        // Tool-output clipping loses the END marker; the body must still be scrubbed.
        let pem = "output:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdefMIIEowIBAAKCAQEA1234567890abcdef\nqrstuvwxyzABCDEF0123456789abcdefqrstuvwxyzABCDEF0123456789abcdef\nZZZZclipped\n[output clipped]";
        let out = redact_one(pem);
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "line 1 leaked: {out}");
        assert!(!out.contains("qrstuvwxyzABCDEF"), "line 2 leaked: {out}");
        assert!(!out.contains("ZZZZclipped"), "clipped tail leaked: {out}");
        assert!(out.starts_with("output:\n"));
        assert!(out.ends_with("\n[output clipped]"));
    }

    #[test]
    fn redacts_truncated_encrypted_private_key_headers() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,A1B2C3D4E5F60718\n\nMIIEowIBAAKCAQEA1234567890abcdefMIIEowIBAAKCAQEA1234567890abcdef";
        let out = redact_one(pem);
        assert!(!out.contains("DEK-Info"), "IV header leaked: {out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "body leaked: {out}");
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
    }

    #[test]
    fn bare_begin_marker_in_prose_survives() {
        // A mention of the marker with no base64 body following is prose, not a key.
        let input = "PEM blocks start with -----BEGIN PRIVATE KEY----- and look like\nthis line";
        assert_eq!(redact_one(input), input);
    }

    // --- R2: new token families + connection strings -------------------------

    #[test]
    fn redacts_new_token_families() {
        let tokens = [
            fixture("sk_live_", "4eC39HqLyjWDarjtT1zdp7dc"),
            fixture("rk_live_", "4eC39HqLyjWDarjtT1zdp7dc"),
            fixture("glpat-", "abcDEF1234567890ghij"),
            fixture("xoxc-", "1234567890-abcdefghij"),
            fixture("xoxs-", "1234567890-abcdefghij"),
            fixture("xapp-", "1-A0123456789-abcdefghijklmnop"),
            fixture("npm_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            fixture("hf_", "abcdefghijklmnopqrstuvwxyzABCDEFGH"),
            fixture("gsk_", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKL"),
            fixture(
                "xai-",
                "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd",
            ),
            fixture(
                "dop_v1_",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
            fixture("shpat_", "0123456789abcdef0123456789abcdef"),
            fixture("shpss_", "0123456789abcdef0123456789abcdef"),
        ];
        for t in &tokens {
            let out = redact_one(&format!("deploy with {t} now"));
            assert!(!out.contains(t.as_str()), "{t} leaked: {out}");
            assert!(out.contains("[REDACTED:api_key]"), "{t}: got {out}");
            assert!(out.starts_with("deploy with "), "{t}: got {out}");
            assert!(out.ends_with(" now"), "{t}: got {out}");
        }
    }

    #[test]
    fn short_prefix_families_skip_identifiers() {
        let inputs = [
            "npm_config_registry points at the default registry",
            "npm_package_version is set by npm",
            "use hf_hub_download to fetch the model",
            "hf_transfer speeds things up",
            "the sk_live_key variable name",
        ];
        for input in inputs {
            assert_eq!(redact_one(input), input, "identifier was eaten");
        }
    }

    #[test]
    fn redacts_connection_string_password() {
        let out = redact_one("DATABASE_URL=postgres://app_user:sup3rS3cret!@db.internal:5432/prod");
        assert!(!out.contains("sup3rS3cret!"), "got: {out}");
        assert_eq!(
            out,
            "DATABASE_URL=postgres://app_user:[REDACTED:secret]@db.internal:5432/prod"
        );

        // Empty user (redis style) still counts; user stays visible in the general case.
        let out2 = redact_one("redis://:opensesame123@cache.local:6379");
        assert_eq!(out2, "redis://:[REDACTED:secret]@cache.local:6379");

        // Git remote with an embedded installation token.
        let out3 = redact_one("https://x-access-token:ghs_0123456789abcdefABCD@github.com/o/r.git");
        assert_eq!(out3, "https://x-access-token:[REDACTED:secret]@github.com/o/r.git");
    }

    #[test]
    fn connection_string_false_positives_survive() {
        let inputs = [
            "https://example.com:8080/path?q=1",
            "see http://localhost:3000 for the dev server",
            "mongodb://replica1.example.com:27017,replica2.example.com:27017/db",
            "postgres://app:$DB_PASSWORD@db/prod",   // env ref, not a secret
            "postgres://app:${DB_PASSWORD}@db/prod", // env ref, not a secret
        ];
        for input in inputs {
            assert_eq!(redact_one(input), input, "false positive");
        }
    }

    // --- R3: header/bearer case + compound header names ----------------------

    #[test]
    fn redacts_compound_authorization_headers() {
        let out = redact_one("Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==");
        assert_eq!(out, "Proxy-Authorization: [REDACTED:api_key]");

        let out2 = redact_one("X-Authorization: t0ken-value-1234");
        assert_eq!(out2, "X-Authorization: [REDACTED:api_key]");
    }

    #[test]
    fn redacts_lowercase_bearer() {
        let out = redact_one("sending bearer abcdef1234567890XYZ along");
        assert!(!out.contains("abcdef1234567890XYZ"), "got: {out}");
        assert_eq!(out, "sending [REDACTED:api_key] along");
    }

    #[test]
    fn bearer_prose_survives() {
        let inputs = ["bearer instruments are negotiable", "the bearer of this note"];
        for input in inputs {
            assert_eq!(redact_one(input), input, "prose was eaten");
        }
    }

    // --- R4: assignment code/prose false positives ---------------------------

    #[test]
    fn assignment_skips_code_and_prose() {
        let inputs = [
            "let token = get_token();",
            "let token = client.fetch_token(scope);",
            "password: use a strong one",
            "# token: set via the environment",
        ];
        for input in inputs {
            assert_eq!(redact_one(input), input, "code/prose was corrupted");
        }
    }

    // --- R5: wider keyword window + hex floor ---------------------------------

    #[test]
    fn redacts_hex_secret_behind_wider_keyword_window() {
        // 39-hex value, keyword ~26 chars back (old 24-char window missed it).
        let input = r#""credentials": {"value": "0123456789abcdef0123456789abcdef0123456"}"#;
        let out = redact_one(input);
        assert!(
            !out.contains("0123456789abcdef0123456789abcdef0123456"),
            "39-hex leaked: {out}"
        );

        // 32-hex secret with the keyword >24 but <48 chars before it.
        let input2 = "the api key for that environment is 0123456789abcdef0123456789abcdef";
        let out2 = redact_one(input2);
        assert!(
            !out2.contains("0123456789abcdef0123456789abcdef"),
            "32-hex leaked: {out2}"
        );
    }

    #[test]
    fn preserves_multiline_base64_without_keyword() {
        // Certificate-style base64 (public data, no secret keyword nearby) must survive.
        let cert = "MIIDdzCCAlgAwIBAgIEbGRkbDANBgcshgiG9w0BAQsFADBsMQswCQYDVQQGEwJV\nUzEQMA4GA1UECBMHQXJpem9uYTETMBEGA1UEBxMSU2NvdHRzZGFsZTEaMBgGA1UE\n";
        assert_eq!(redact_one(cert), cert);
    }

    // --- R6: scope beyond title + message blocks -----------------------------

    /// Documents the redaction scope of [`redact_with`]: `git.remote`, `Session.extra`, and
    /// `Message.extra` are scrubbed; `cwd`/`source_path` deliberately are not (see the fn docs).
    #[test]
    fn scrubs_git_remote_and_extra_maps() {
        let mut sess = empty_session();
        sess.git = Some(GitInfo {
            branch: Some("main".into()),
            commit: Some("abc1234".into()),
            remote: Some("https://x-access-token:ghs_0123456789abcdefABCD@github.com/o/r.git".into()),
        });
        sess.extra
            .insert("apiKey".into(), serde_json::json!("sk-abcDEF1234567890ghijkl"));
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text { text: "hi".into() });
        m.extra.insert(
            "env".into(),
            serde_json::json!({ "GITHUB_TOKEN": "ghp_0123456789abcdefABCDEF0123456789abcd" }),
        );
        sess.messages.push(m);

        let (out, stats) = redact_with(&sess, &RedactOptions::default()).unwrap();
        let remote = out.git.as_ref().unwrap().remote.as_deref().unwrap();
        assert_eq!(remote, "https://x-access-token:[REDACTED:secret]@github.com/o/r.git");
        assert_eq!(out.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        let extra = serde_json::Value::Object(out.extra.clone()).to_string();
        assert!(!extra.contains("sk-abcDEF"), "session extra leaked: {extra}");
        let mextra = serde_json::Value::Object(out.messages[0].extra.clone()).to_string();
        assert!(!mextra.contains("ghp_"), "message extra leaked: {mextra}");
        assert!(stats.total() >= 3, "stats: {stats:?}");

        // Idempotent across the whole session too.
        let (again, again_stats) = redact_with(&out, &RedactOptions::default()).unwrap();
        assert_eq!(again.git.as_ref().unwrap().remote, out.git.as_ref().unwrap().remote);
        assert_eq!(again.extra, out.extra);
        assert_eq!(again.messages[0].extra, out.messages[0].extra);
        assert_eq!(again_stats.total(), 0);
    }

    // --- R7: AKIA over-long runs ----------------------------------------------

    #[test]
    fn akia_overlong_run_fully_redacted() {
        // Slicing exactly 20 chars out of a longer run would leave a tail of the key behind.
        let out = redact_one("key AKIAIOSFODNN7EXAMPLEEXTRA9 tail");
        assert!(!out.contains("AKIA"), "got: {out}");
        assert!(!out.contains("EXTRA9"), "tail leaked: {out}");
        assert_eq!(out, "key [REDACTED:api_key] tail");
    }

    #[test]
    fn no_panic_on_weird_input() {
        let weird = [
            "",
            "@",
            "@@@@",
            "sk-",
            "-----BEGIN PRIVATE KEY-----",
            "Bearer ",
            "café résumé naïve 日本語 sk-abcDEF1234567890ghijkl",
            "....",
            "eyJ",
        ];
        for w in weird {
            let _ = redact_one(w); // must not panic
        }
    }

    /// Property-ish: every matcher must survive arbitrary truncation/suffixing of real-looking
    /// secrets (clipped tool output produces exactly these shapes), including through multi-byte
    /// UTF-8, and redacting any fragment must stay idempotent.
    #[test]
    fn no_panic_on_mutated_inputs() {
        let corpus = [
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nMIIEowIBAAKCAQEA1234567890abcdef\nqrstuvwx\n-----END RSA PRIVATE KEY-----".to_string(),
            "postgres://app_user:sup3rS3cret!@db.example.com:5432/prod".to_string(),
            "Proxy-Authorization: bearer abcdef1234567890XYZ".to_string(),
            "password = \"hunter2supersecret\" et café 日本語".to_string(),
            fixture("npm_", "abcdefghijklmnopqrstuvwxyz0123456789 ") + &fixture("hf_", "abcdefghijklmnopqrstuvwxyzABCDEFGH"),
            "key AKIAIOSFODNN7EXAMPLEEXTRA9 [REDACTED:api_key] eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig".to_string(),
        ];
        for base in &corpus {
            for cut in 0..=base.len() {
                if !base.is_char_boundary(cut) {
                    continue;
                }
                for frag in [&base[..cut], &base[cut..]] {
                    let once = redact_one(frag);
                    let twice = redact_one(&once);
                    assert_eq!(once, twice, "not idempotent for fragment {frag:?}");
                }
            }
        }
    }

    #[test]
    fn redaction_resolves_lazy_text_and_scrubs_system_prompt_and_complete_record() -> anyhow::Result<()> {
        let world = crate::test_world("redact-checked-record");
        let path = world.base.join("record.json");
        let secret = fixture("sk-", "testabcdefghijklmnopqrstuvwxyz");
        let body = format!("{} {secret}", "ordinary ".repeat(2000));
        let value = serde_json::json!({"type":"assistant","text":body,"unknown":{"credential":secret}});
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let text = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                view.get("text")?.unwrap().source_text()
            })?
            .unwrap();
        let raw = crate::Json::source(&path, 0, source.len())?;
        let mut session = Session::new("redaction", crate::Harness::OpenCode);
        session.system_prompt = Some(format!("Authorization: Bearer {secret}"));
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(Block::Text { text });
        session.messages.push(message);
        crate::ir::CompleteRecord::attach_replay(
            &mut session.messages,
            crate::Harness::OpenCode,
            raw,
            Default::default(),
        );
        let original = session.messages[0].complete_record.as_ref().unwrap();
        assert!(!original.projection.is_empty());
        let (redacted, stats) = redact_with(&session, &RedactOptions::default())?;
        assert!(!serde_json::to_string(&redacted)?.contains(&secret));
        assert!(serde_json::to_string(&session)?.contains(&secret));
        assert!(
            !original.projection.is_empty(),
            "copy-on-write must not alter the input witness"
        );
        assert!(redacted.messages[0]
            .complete_record
            .as_ref()
            .unwrap()
            .projection
            .is_empty());
        assert!(stats.total() >= 3);
        assert_eq!(std::fs::read(&path)?, serde_json::to_vec(&value)?);
        // Direct message calls propagate stale text instead of panicking or returning clean data.
        let mut stale = session.messages[0].clone();
        std::fs::write(&path, b"replacement")?;
        let mut failed_stats = RedactStats::default();
        assert!(redact_message(
            &mut stale,
            &RedactOptions::default(),
            &mut failed_stats,
            &session.resolver()
        )
        .is_err());
        assert_eq!(failed_stats.total(), 0);
        Ok(())
    }

    #[test]
    fn redaction_uses_session_resolver_for_implicit_spans() -> anyhow::Result<()> {
        let world = crate::test_world("redact-implicit-span");
        let path = world.base.join("body.txt");
        let secret = fixture("sk-", "testabcdefghijklmnopqrstuvwxyz");
        std::fs::write(&path, serde_json::to_string(&secret)?)?;
        let mut session = Session::new("implicit", crate::Harness::Claude);
        session.source_path = Some(path);
        let mut message = Message::new(crate::Role::User);
        message.content.push(Block::Text {
            text: crate::Text::Span(crate::lazy::Span {
                source: None,
                offset: 1,
                len: secret.len() as u64,
                escaped: false,
            }),
        });
        session.messages.push(message);
        assert!(!serde_json::to_string(&redact(&session)?)?.contains(&secret));
        let resolver = session.resolver();
        let mut message = session.messages[0].clone();
        redact_message(
            &mut message,
            &RedactOptions::default(),
            &mut RedactStats::default(),
            &resolver,
        )?;
        assert!(!serde_json::to_string(&message)?.contains(&secret));
        Ok(())
    }

    #[test]
    fn a_second_pem_header_is_never_mistaken_for_the_private_key_footer() {
        let body = "ABCDEFGHIJKLMNOP";
        let input = format!(
            "-----BEGIN PRIVATE KEY-----\n{body}\n-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----"
        );
        let output = redact_one(&input);
        assert!(!output.contains(body));
        assert!(output.contains("[REDACTED:private_key]"));
        assert_eq!(redact_one(&output), output);
    }

    #[test]
    fn blob_suffix_offsets_match_the_recorded_table() {
        let rule = rules().iter().find(|r| r.kind == Kind::Blob).unwrap();
        let mut frozen = crate::emit::frozen::Frozen::new("redact-blob-offsets");
        for prefix in ["", "key ", "secret = ", "ordinary ", "clé key "] {
            for stem in ["abcdef", "abcdefghi", "abc-def_", "abc/def+", "abc==="] {
                for repeats in [1, 5, 8, 20, 1000] {
                    let input = format!("{prefix}{}", stem.repeat(repeats));
                    let actual = next_blob(&rule.re, &input, 0).map(|h| (h.start, h.end));
                    frozen.insert(format!("{prefix:?} {stem} x{repeats}"), serde_json::json!(actual));
                }
            }
        }
        frozen.check().unwrap();
        assert!(matches!(
            scrub_cow(
                &"x".repeat(1_000_000),
                &RedactOptions::default(),
                &mut RedactStats::default()
            ),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn clean_source_tool_inputs_stay_unmaterialized_after_zero_alloc_prescan() -> anyhow::Result<()> {
        let world = crate::test_world("redact-zero-alloc-prescan");
        let clean_path = world.base.join("clean_input.json");
        let dirty_path = world.base.join("dirty_input.json");
        let secret = fixture("sk-", "testabcdefghijklmnopqrstuvwxyz");
        let clean_bytes = serde_json::to_vec(&serde_json::json!({
            "command": "cargo test -p clustervision-core",
            "nested": ["alpha", 42, {"flag": true}]
        }))?;
        let dirty_bytes = serde_json::to_vec(&serde_json::json!({
            "command": format!("Authorization: Bearer {secret}")
        }))?;
        std::fs::write(&clean_path, &clean_bytes)?;
        std::fs::write(&dirty_path, &dirty_bytes)?;

        let clean_json = crate::Json::source(&clean_path, 0, clean_bytes.len() as u64)?;
        let dirty_json = crate::Json::source(&dirty_path, 0, dirty_bytes.len() as u64)?;
        assert!(!json_needs_scrub(&clean_json, &RedactOptions::default())?);
        assert!(json_needs_scrub(&dirty_json, &RedactOptions::default())?);

        let mut session = Session::new("prescan", Harness::Claude);
        let mut msg = Message::new(Role::Assistant);
        msg.content.push(Block::ToolUse {
            id: "c1".into(),
            name: "Bash".into(),
            input: clean_json,
            namespace: None,
        });
        msg.content.push(Block::ToolUse {
            id: "c2".into(),
            name: "Bash".into(),
            input: dirty_json,
            namespace: None,
        });
        session.messages.push(msg);

        let (scrubbed, stats) = redact_with(&session, &RedactOptions::default())?;
        assert_eq!(stats.api_keys, 1);
        // Clean ToolUse MUST remain Json::Source (zero Value tree cloning!).
        assert!(matches!(
            &scrubbed.messages[0].content[0],
            Block::ToolUse {
                input: crate::Json::Source(_),
                ..
            }
        ));
        // Dirty ToolUse became Json::Owned and was scrubbed.
        match &scrubbed.messages[0].content[1] {
            Block::ToolUse {
                input: crate::Json::Owned(v),
                ..
            } => assert!(!v.to_string().contains(&secret)),
            other => panic!("expected scrubbed Owned ToolUse, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn configurable_privacy_profile_scrubs_paths_ips_hosts_ssh_principal_and_filters() -> anyhow::Result<()> {
        let profile = ScrubProfile {
            classes: RedactClasses {
                emails: false,
                ..RedactClasses::default()
            },
            private_ips: true,
            hostnames: true,
            ssh_principals: true,
            rewrite_home: true,
            rewrite_repo: true,
            repo_root: Some(PathBuf::from("/Users/alice/dev/secret-proj")),
            path_rewrites: vec![PathRewrite {
                pattern: r"/mnt/nas/[A-Za-z0-9_-]+".into(),
                replacement: "$NAS".into(),
            }],
            principal: PrincipalPolicy::Hash {
                salt: Some("fleet-salt".into()),
            },
            filter: SessionFilter {
                allow_cwds: vec![PathBuf::from("/Users/alice/dev/*")],
                deny_cwds: vec![PathBuf::from("/Users/alice/dev/private-vault")],
                harnesses: vec!["claude".into()],
                tags: vec!["shareable".into()],
            },
            ..ScrubProfile::default()
        };

        let mut s = Session::new("prof-1", Harness::Claude);
        s.cwd = Some(PathBuf::from("/Users/alice/dev/secret-proj"));
        s.principal = Some(crate::ir::Principal::Object(
            serde_json::json!({"user": "alice", "machine": "mbp"})
                .as_object()
                .unwrap()
                .clone(),
        ));
        s.cv_extra_mut()
            .insert("tags".into(), serde_json::json!(["shareable", "rust"]));
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: "Connect via ssh deploy@bastion-01 to 10.42.0.19 or 100.88.1.5 (keep 127.0.0.1 and v10.2.3.4), check db.internal and node.tailnet.ts.net, read /Users/alice/dev/secret-proj/src/lib.rs, /Users/alice/.zshrc, and /mnt/nas/vol9/data. Keep alice@example.com.".into(),
        });
        s.messages.push(m);

        let (scrubbed, stats) = filter_and_scrub_session(&s, &profile)?.expect("matching session should pass filter");
        let rendered = scrubbed.messages[0].text().expect("text block");
        assert!(rendered.contains("ssh [REDACTED:ssh]"), "{rendered}");
        assert!(!rendered.contains("10.42.0.19"), "{rendered}");
        assert!(!rendered.contains("100.88.1.5"), "{rendered}");
        assert!(rendered.contains("127.0.0.1"), "loopback must survive: {rendered}");
        assert!(rendered.contains("v10.2.3.4"), "version tag must survive: {rendered}");
        assert!(!rendered.contains("db.internal"), "{rendered}");
        assert!(!rendered.contains("node.tailnet.ts.net"), "{rendered}");
        assert!(rendered.contains("$REPO/src/lib.rs"), "{rendered}");
        assert!(rendered.contains("$HOME/.zshrc"), "{rendered}");
        assert!(rendered.contains("$NAS/data"), "{rendered}");
        assert!(
            rendered.contains("alice@example.com"),
            "emails=false must keep email: {rendered}"
        );
        assert_eq!(scrubbed.cwd.as_deref(), Some(Path::new("$REPO")));

        let crate::ir::Principal::Object(pmap) = scrubbed.principal.as_ref().unwrap() else {
            panic!("expected object principal");
        };
        let principal_user = pmap["user"].as_str().unwrap();
        assert!(principal_user.starts_with("sha256:"), "{principal_user}");
        assert!(!principal_user.contains("alice"));
        assert!(stats.total() >= 6);

        // Denied cwd or non-matching harness is filtered out before materializing.
        let mut denied = s.clone();
        denied.cwd = Some(PathBuf::from("/Users/alice/dev/private-vault/sub"));
        assert!(filter_and_scrub_session(&denied, &profile)?.is_none());

        let mut wrong_harness = s.clone();
        wrong_harness.harness = Harness::Codex;
        assert!(filter_and_scrub_session(&wrong_harness, &profile)?.is_none());
        Ok(())
    }
    #[test]
    fn privacy_profile_scrubs_file_and_image_blocks_and_home_paths_behind_any_delimiter() -> anyhow::Result<()> {
        let profile = ScrubProfile {
            private_ips: true,
            hostnames: true,
            rewrite_home: true,
            ..ScrubProfile::default()
        };
        let mut s = Session::new("prof-blocks", Harness::Claude);
        let mut m = Message::new(Role::User);
        // Foreign users, so only the structural `/Users/<u>` / `/home/<u>` rule can catch them.
        m.content.push(Block::Text {
            text: "see `/Users/carol/notes.md`, <path>/home/bob/out.txt</path>, file:///Users/carol/a.png, \
                   log>/home/bob/build.log and {/Users/carol/x} | /home/bob/y; ls /home/bob /Users/carol; keep https://example.com/home/page"
                .into(),
        });
        m.content.push(Block::File {
            mime: Some("text/markdown".into()),
            path: Some("/Users/carol/work/secret-plan.md".into()),
            source: Some("http://10.20.30.40/files/secret-plan.md".into()),
        });
        m.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("file:///home/bob/Desktop/screenshot.png".into()),
        });
        let inline = format!("data:image/png;base64,{}", "QUJD".repeat(64));
        m.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some(inline.clone()),
        });
        s.messages.push(m);

        let (scrubbed, _stats) = scrub_session_with_profile(&s, &profile)?;
        let wire = serde_json::to_string(&scrubbed)?;
        for leak in ["carol", "bob", "10.20.30.40"] {
            assert!(
                !wire.contains(leak),
                "{leak:?} leaked through the privacy profile: {wire}"
            );
        }
        assert!(wire.contains("https://example.com/home/page"), "{wire}");
        assert!(wire.contains(&inline), "inline image bytes must stay intact");

        // The secrets-only redactor covers the same fields (a signed URL is a credential carrier).
        let mut signed = Session::new("signed", Harness::Claude);
        let mut m = Message::new(Role::User);
        let key = fixture("sk-ant-", "api03-abcdefghijklmnopqrstuvwxyz0123456789ABCD");
        m.content.push(Block::File {
            mime: None,
            path: Some("report.pdf".into()),
            source: Some(format!("https://files.example.com/report.pdf?api_key={key}")),
        });
        m.content.push(Block::Image {
            media_type: None,
            data_ref: Some(format!("https://img.example.com/a.png?token={key}")),
        });
        signed.messages.push(m);
        let wire = serde_json::to_string(&redact(&signed)?)?;
        assert!(!wire.contains(&key), "{wire}");
        Ok(())
    }
    #[test]
    fn privacy_prefixes_respect_path_segments_and_scrub_object_keys() {
        let _env = crate::env::Env::from_vars([("HOME", "/Users/alice")]).enter();
        let profile = ScrubProfile {
            rewrite_home: true,
            rewrite_repo: true,
            repo_root: Some(PathBuf::from("/work/repo")),
            ..Default::default()
        };
        let compiled = profile.compile().unwrap();
        let mut stats = ScrubStats::default();
        assert_eq!(
            compiled.scrub_text_cow(
                "/Users/alice2/file /work/repository /work/repo/file",
                profile.repo_root.as_deref(),
                &mut stats
            ),
            "$HOME/file /work/repository $REPO/file"
        );
        let mut value = serde_json::json!({"/Users/alice/a": 1, "/Users/bob/a": 2});
        assert!(crate::json::Json::from(value.clone())
            .with_view(|v| json_view_any_str(v, &mut |s| compiled.text_needs_scrub(s, None)))
            .unwrap());
        compiled.scrub_value(&mut value, None, &mut stats);
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(!value.to_string().contains("Users"));
    }
}
