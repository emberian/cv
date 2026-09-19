//! Redact secrets / PII from a [`Session`] before sharing or export.
//!
//! The contribution loop ("send us your logs") and any enterprise use both hinge on being able to
//! scrub a transcript first. This returns a copy with secrets replaced by placeholders.
//!
//! # Approach
//!
//! cv-core is deliberately dep-light and does **not** depend on the `regex` crate, so every matcher
//! here is a hand-rolled scanner over `&str`. We walk the text once, and at each position try a set
//! of recognizers (`-----BEGIN PRIVATE KEY-----` blocks, known token prefixes, JWTs, emails,
//! `key = "value"` assignments, and conservative high-entropy blobs). When one matches, the matched
//! span is replaced with a stable placeholder of the form `[REDACTED:kind]` and scanning resumes
//! after it.
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
use serde_json::Value;
use std::borrow::Cow;

/// Knobs for [`redact_with`]. Defaults scrub everything.
#[derive(Debug, Clone, Default)]
pub struct RedactOptions {
    /// If `false` (the default), email addresses are redacted. Set `true` to keep them.
    pub keep_emails: bool,
    /// Which classes of secret to scrub. Defaults to all of them; narrow it to scrub only some
    /// (e.g. only PEM private keys while leaving identities/fixtures intact).
    pub classes: RedactClasses,
}

/// A toggle per secret class. `Default` enables every class (so `RedactOptions::default()` scrubs
/// everything, unchanged); use [`RedactClasses::none`]/[`RedactClasses::parse_only`] to scope it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactClasses {
    pub api_keys: bool,
    pub private_keys: bool,
    pub jwts: bool,
    pub emails: bool,
    pub blobs: bool,
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

/// Return a copy of `session` with secrets/PII scrubbed from all message + tool content.
pub fn redact(session: &Session) -> Session {
    redact_with(session, &RedactOptions::default()).0
}

/// Like [`redact`], but with [`RedactOptions`]; also returns [`RedactStats`].
///
/// # Scope
///
/// Scrubbed: the title; every message (all text/thinking/tool blocks via [`redact_message`], plus
/// each message's harness-specific `extra` map); the session-level `extra` map; and `git.remote` —
/// remotes can embed credentials (`https://x-access-token:ghs_…@github.com/o/r.git`). Note that an
/// scp-style `git@host:…` remote has its `user@host` scrubbed as an email; that's cosmetic, not a
/// leak.
///
/// Not scrubbed: `cwd` and `source_path` (filesystem paths — a placeholder would corrupt them, and
/// path privacy is a separate concern from secret redaction), `model`, ids, timestamps, and
/// `git.branch`/`git.commit` (user-visible refs, not credential carriers).
pub fn redact_with(session: &Session, opts: &RedactOptions) -> (Session, RedactStats) {
    let mut out = session.clone();
    let mut stats = RedactStats::default();

    if let Some(title) = out.title.as_mut() {
        *title = scrub(title, opts, &mut stats);
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
        redact_message(msg, opts, &mut stats);
    }

    (out, stats)
}

/// Redact secrets from a single string. The core building block.
pub fn redact_text(s: &str) -> String {
    let mut stats = RedactStats::default();
    scrub(s, &RedactOptions::default(), &mut stats)
}

/// Redact secrets from a single message in place. Public so streaming consumers (dataset export) can
/// scrub one message at a time without materializing the whole session.
pub fn redact_message(msg: &mut Message, opts: &RedactOptions, stats: &mut RedactStats) {
    for block in &mut msg.content {
        match block {
            Block::Text { text } | Block::Thinking { text, .. } => {
                // `scrub_cow` borrows when clean, so untouched text (the common case) is not
                // reallocated — only actually-redacted fields are rewritten.
                if let Cow::Owned(scrubbed) = scrub_cow(text, opts, stats) {
                    *text = scrubbed.into();
                }
            }
            Block::ToolUse { input, .. } => {
                scrub_value(input, opts, stats);
            }
            Block::ToolResult { content, details, .. } => {
                if let Cow::Owned(scrubbed) = scrub_cow(content, opts, stats) {
                    *content = scrubbed.into();
                }
                if let Some(d) = details {
                    scrub_value(d, opts, stats);
                }
            }
            Block::Image { .. } | Block::File { .. } => {}
        }
    }
    // Harness-specific passthrough fields can carry anything the harness logged (env snapshots,
    // request metadata) — scrub their string leaves too.
    for (_k, v) in msg.extra.iter_mut() {
        scrub_value(v, opts, stats);
    }
}

/// Recursively scrub string values inside a JSON value, leaving keys and structure intact. Public
/// so per-block redacting renderers (dataset export) can scrub a tool input without going through
/// [`redact_message`] on a cloned message.
pub fn scrub_value(v: &mut Value, opts: &RedactOptions, stats: &mut RedactStats) {
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
            for (_k, val) in map.iter_mut() {
                scrub_value(val, opts, stats);
            }
        }
        // numbers, bools, null: nothing to scrub.
        _ => {}
    }
}

const PLACEHOLDER_OPEN: &str = "[REDACTED:";

fn placeholder(kind: &str) -> String {
    format!("{PLACEHOLDER_OPEN}{kind}]")
}

// ---------------------------------------------------------------------------
// Core scanner
// ---------------------------------------------------------------------------

/// Walk `s` once, replacing every recognized secret span with a placeholder.
fn scrub(s: &str, opts: &RedactOptions, stats: &mut RedactStats) -> String {
    scrub_cow(s, opts, stats).into_owned()
}

/// [`scrub`], allocation-aware: returns `Cow::Borrowed(s)` when nothing matched (the overwhelmingly
/// common case for ordinary transcript text), and copies clean stretches between matches in bulk
/// rather than char-by-char. Same output as `scrub` byte-for-byte.
pub fn scrub_cow<'a>(s: &'a str, opts: &RedactOptions, stats: &mut RedactStats) -> Cow<'a, str> {
    let bytes = s.as_bytes();
    // The redacted output, allocated lazily on the first match; `copied` is how much of `s` has
    // already been flushed into it.
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        // Skip any existing placeholder verbatim so we stay idempotent and don't recurse into
        // already-redacted spans (it's copied with the surrounding clean stretch).
        if let Some(end) = match_placeholder(s, i) {
            i = end;
            continue;
        }

        if let Some(m) = match_at(s, i, opts) {
            let out = out.get_or_insert_with(|| String::with_capacity(s.len()));
            // Flush the clean stretch plus any prefix the matcher preserves verbatim (e.g.
            // `Authorization: `), then emit the placeholder for the sensitive span.
            out.push_str(&s[copied..i + m.keep_prefix]);
            out.push_str(&placeholder(m.kind.label()));
            m.kind.bump(stats);
            i = m.end;
            copied = i;
            continue;
        }

        // No match: advance one UTF-8 char (copied later, in bulk).
        i += utf8_len(bytes[i]).min(bytes.len() - i);
    }

    match out {
        None => Cow::Borrowed(s),
        Some(mut out) => {
            out.push_str(&s[copied..]);
            Cow::Owned(out)
        }
    }
}

/// Length in bytes of a UTF-8 sequence starting with `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[derive(Clone, Copy)]
enum Kind {
    ApiKey,
    PrivateKey,
    Jwt,
    Email,
    Blob,
    Assignment,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::ApiKey => "api_key",
            Kind::PrivateKey => "private_key",
            Kind::Jwt => "jwt",
            Kind::Email => "email",
            Kind::Blob => "secret",
            Kind::Assignment => "secret",
        }
    }
    fn bump(self, stats: &mut RedactStats) {
        match self {
            Kind::ApiKey => stats.api_keys += 1,
            Kind::PrivateKey => stats.private_keys += 1,
            Kind::Jwt => stats.jwts += 1,
            Kind::Email => stats.emails += 1,
            Kind::Blob => stats.blobs += 1,
            Kind::Assignment => stats.assignments += 1,
        }
    }
}

/// A successful match: replace `[i + keep_prefix, end)` with a placeholder, keeping `[i, i+keep_prefix)`.
struct Match {
    /// Bytes from the match start to preserve verbatim (e.g. `Authorization: ` or `password = "`).
    keep_prefix: usize,
    /// End offset of the matched (and redacted) span.
    end: usize,
    kind: Kind,
}

fn m(kind: Kind, end: usize) -> Option<Match> {
    Some(Match {
        keep_prefix: 0,
        end,
        kind,
    })
}

/// Try every recognizer at byte offset `i`. Returns the first match.
fn match_at(s: &str, i: usize, opts: &RedactOptions) -> Option<Match> {
    // Order matters: private keys first (they swallow a big block), then assignments / headers
    // (which keep a prefix), then bare tokens, JWTs, emails, then conservative blobs. Each class is
    // gated by `opts.classes` so redaction can be scoped (e.g. PEM keys only).
    let c = &opts.classes;
    if c.private_keys {
        if let Some(end) = match_private_key(s, i) {
            return m(Kind::PrivateKey, end);
        }
    }
    if c.assignments {
        if let Some(hit) = match_assignment(s, i) {
            return Some(hit);
        }
        if let Some(hit) = match_url_userinfo_password(s, i) {
            return Some(hit);
        }
    }
    if c.api_keys {
        if let Some(hit) = match_authorization_header(s, i) {
            return Some(hit);
        }
        if let Some(end) = match_bearer(s, i) {
            return m(Kind::ApiKey, end);
        }
        if let Some(end) = match_token_prefix(s, i) {
            return m(Kind::ApiKey, end);
        }
    }
    if c.jwts {
        if let Some(end) = match_jwt(s, i) {
            return m(Kind::Jwt, end);
        }
    }
    if c.emails && !opts.keep_emails {
        if let Some(end) = match_email(s, i) {
            return m(Kind::Email, end);
        }
    }
    if c.blobs {
        if let Some(end) = match_keyworded_blob(s, i) {
            return m(Kind::Blob, end);
        }
    }
    None
}

/// If `s[i..]` starts with our placeholder syntax, return the end of it (so we copy it verbatim).
fn match_placeholder(s: &str, i: usize) -> Option<usize> {
    if !s[i..].starts_with(PLACEHOLDER_OPEN) {
        return None;
    }
    // find the closing ']'
    let rest = &s[i + PLACEHOLDER_OPEN.len()..];
    let close = rest.find(']')?;
    Some(i + PLACEHOLDER_OPEN.len() + close + 1)
}

// ---------------------------------------------------------------------------
// Character classes
// ---------------------------------------------------------------------------

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_base64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn is_base64_std_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
}

fn is_hex_char(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// True if the byte just before `i` is not part of a token (so we match at a boundary).
fn boundary_before(s: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = s.as_bytes()[i - 1];
    !is_token_char(prev)
}

// ---------------------------------------------------------------------------
// Recognizers
// ---------------------------------------------------------------------------

/// `-----BEGIN ... PRIVATE KEY-----` ... `-----END ... PRIVATE KEY-----`
///
/// Transcripts routinely clip tool output, losing the `-----END …-----` marker. A truncated key is
/// still a key: when no END marker exists, we redact the header plus the contiguous run of
/// base64-ish body lines that follows it (stopping at the first line that doesn't look like PEM
/// body), rather than leaving lines 2..n to leak.
fn match_private_key(s: &str, i: usize) -> Option<usize> {
    let begin = "-----BEGIN ";
    if !s[i..].starts_with(begin) {
        return None;
    }
    // The header line must mention "PRIVATE KEY".
    let after = &s[i..];
    let header_end = after.find("-----")? + 5; // end of "-----BEGIN "
    let header_close = after[header_end..].find("-----")? + header_end + 5;
    let header = &after[..header_close];
    if !header.contains("PRIVATE KEY") {
        return None;
    }
    // Find the matching END marker.
    let end_marker = "PRIVATE KEY-----";
    let search_from = i + header_close;
    if let Some(end_pos) = s[search_from..].find(end_marker) {
        return Some(search_from + end_pos + end_marker.len());
    }
    // No END marker: truncated key. Consume the base64 body that's present.
    truncated_pem_body_end(s, search_from)
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

/// `Authorization: <value>` — keeps the header name + colon, redacts the value to end of line.
/// Also fires mid-word after a `-`, so `Proxy-Authorization:` / `X-Authorization:` are covered.
fn match_authorization_header(s: &str, i: usize) -> Option<Match> {
    if i > 0 {
        let prev = s.as_bytes()[i - 1];
        // Allow '-' (compound header names) but reject e.g. `FooAuthorization:`.
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let rest = &s[i..];
    let prefix = "Authorization:";
    if !rest.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(prefix)) {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut j = prefix.len();
    // skip spaces (kept verbatim as part of the prefix)
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] == b'\n' || bytes[j] == b'\r' {
        return None; // no value
    }
    let value_start = j;
    // Idempotency: if the value is already a placeholder, leave it alone.
    if rest[value_start..].starts_with(PLACEHOLDER_OPEN) {
        return None;
    }
    // value runs to end of line
    let mut end = value_start;
    while end < bytes.len() && bytes[end] != b'\n' && bytes[end] != b'\r' {
        end += 1;
    }
    Some(Match {
        keep_prefix: value_start,
        end: i + end,
        kind: Kind::ApiKey,
    })
}

/// `Bearer <token>` — the scheme is case-insensitive (RFC 7235), so `bearer`/`BEARER` count too.
fn match_bearer(s: &str, i: usize) -> Option<usize> {
    if !boundary_before(s, i) {
        return None;
    }
    let rest = &s[i..];
    if !rest.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("Bearer ")) {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut j = 7;
    while j < bytes.len() && bytes[j] == b' ' {
        j += 1;
    }
    let start = j;
    while j < bytes.len() && (is_token_char(bytes[j]) || bytes[j] == b'.') {
        j += 1;
    }
    if j - start < 8 {
        return None; // too short to be a real token
    }
    // Prose guard: matching lowercase "bearer" means "bearer instruments are…" would otherwise
    // redact the next word. Real bearer tokens are long or contain digits/symbols.
    let token = &rest[start..j];
    if token.len() < 16 && token.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some(i + j)
}

/// What characters a token's tail may contain after its prefix.
#[derive(Clone, Copy)]
enum Tail {
    /// Alphanumeric plus `-` and `_`.
    Token,
    /// Strictly alphanumeric. Used for short prefixes that collide with ordinary identifiers
    /// (`npm_config_registry`, `hf_hub_download`): the real token tails are pure base62, so
    /// stopping at `_`/`-` rejects the identifier without missing keys.
    Alnum,
}

/// Known credential prefixes: sk-, sk-ant-, sk_live_, xox?-, xapp-, gh?_, glpat-, npm_, hf_,
/// gsk_, xai-, dop_v1_, shpat_/shpss_, AKIA…, AIza…
fn match_token_prefix(s: &str, i: usize) -> Option<usize> {
    if !boundary_before(s, i) {
        return None;
    }
    let rest = &s[i..];
    let bytes = rest.as_bytes();

    // AWS access key: AKIA + 16 uppercase alnum. Real key IDs are exactly 20 chars, but if the
    // run continues past that we redact the *whole* run rather than slicing 20 chars out of it —
    // a placeholder plus a leftover tail would leak most of a concatenated/overlong key.
    if rest.starts_with("AKIA") {
        let mut j = 4;
        while j < bytes.len() && (bytes[j].is_ascii_uppercase() || bytes[j].is_ascii_digit()) {
            j += 1;
        }
        if j - 4 >= 16 {
            return Some(i + j);
        }
        return None;
    }

    // Prefixes that are followed by a run of tail chars. Min tail lengths sit safely below each
    // family's real length so we don't miss keys, but high enough that prose/identifiers don't
    // trip them. Order matters only for prefix-of-prefix pairs (sk-ant- before sk-).
    const PREFIXES: &[(&str, usize, Tail)] = &[
        ("sk-ant-", 10, Tail::Token),
        ("sk-", 10, Tail::Token),
        ("sk_live_", 20, Tail::Alnum), // Stripe secret key (live)
        ("rk_live_", 20, Tail::Alnum), // Stripe restricted key (live)
        ("xoxb-", 8, Tail::Token),
        ("xoxp-", 8, Tail::Token),
        ("xoxa-", 8, Tail::Token),
        ("xoxr-", 8, Tail::Token),
        ("xoxc-", 8, Tail::Token), // Slack client token
        ("xoxs-", 8, Tail::Token), // Slack session token
        ("xapp-", 8, Tail::Token), // Slack app-level token
        ("ghp_", 20, Tail::Token),
        ("gho_", 20, Tail::Token),
        ("ghs_", 20, Tail::Token),
        ("ghu_", 20, Tail::Token),
        ("ghr_", 20, Tail::Token),
        ("github_pat_", 20, Tail::Token),
        ("glpat-", 20, Tail::Token),  // GitLab personal access token
        ("npm_", 36, Tail::Alnum),    // npm token: npm_ + exactly 36 base62
        ("hf_", 30, Tail::Alnum),     // Hugging Face token: hf_ + 34 base62
        ("gsk_", 40, Tail::Alnum),    // Groq: gsk_ + 52 base62
        ("xai-", 40, Tail::Alnum),    // xAI: xai- + ~80 base62
        ("dop_v1_", 40, Tail::Alnum), // DigitalOcean: dop_v1_ + 64 hex
        ("shpat_", 32, Tail::Alnum),  // Shopify private app token: + 32 hex
        ("shpss_", 32, Tail::Alnum),  // Shopify shared secret: + 32 hex
        ("AIza", 30, Tail::Token),
    ];
    for (pfx, min_tail, tail) in PREFIXES {
        if rest.starts_with(pfx) {
            let mut j = pfx.len();
            let start_tail = j;
            while j < bytes.len()
                && match tail {
                    Tail::Token => is_token_char(bytes[j]),
                    Tail::Alnum => bytes[j].is_ascii_alphanumeric(),
                }
            {
                j += 1;
            }
            if j - start_tail >= *min_tail {
                return Some(i + j);
            }
            return None;
        }
    }
    None
}

/// JWT: three base64url segments joined by dots, the whole thing long.
fn match_jwt(s: &str, i: usize) -> Option<usize> {
    if !boundary_before(s, i) {
        return None;
    }
    // JWT header almost always starts with "eyJ" (base64 of `{"`).
    let rest = &s[i..];
    if !rest.starts_with("eyJ") {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut j = 0;
    let mut dots = 0usize;
    let mut seg_len = 0usize;
    let mut seg_lens = [0usize; 3];
    while j < bytes.len() {
        let b = bytes[j];
        if b == b'.' {
            if dots >= 2 {
                break; // already have 3 segments; this dot ends the third
            }
            seg_lens[dots] = seg_len;
            dots += 1;
            seg_len = 0;
            j += 1;
            continue;
        }
        if is_base64url_char(b) {
            seg_len += 1;
            j += 1;
        } else {
            break;
        }
    }
    // record final (third) segment length
    if dots == 2 {
        seg_lens[2] = seg_len;
    }
    // Need exactly two dots (three segments), each non-trivial, total long.
    if dots == 2 && seg_lens[0] >= 4 && seg_lens[1] >= 4 && seg_lens[2] >= 4 && j >= 40 {
        return Some(i + j);
    }
    None
}

/// Email: localpart@domain.tld
fn match_email(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let b = bytes[i];
    // Match must start at the beginning of the local part. Require a boundary before.
    if !is_email_local(b) {
        return None;
    }
    if i > 0 && is_email_local(bytes[i - 1]) {
        return None;
    }
    let mut j = i;
    while j < bytes.len() && is_email_local(bytes[j]) {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'@' || j == i {
        return None;
    }
    let local_end = j;
    j += 1; // skip @
    let domain_start = j;
    let mut has_dot = false;
    while j < bytes.len() && is_email_domain(bytes[j]) {
        if bytes[j] == b'.' {
            has_dot = true;
        }
        j += 1;
    }
    if !has_dot || j == domain_start {
        return None;
    }
    // Domain must end with at least a 2-char TLD of letters.
    // Trim a trailing dot (not part of TLD).
    let mut end = j;
    while end > domain_start && bytes[end - 1] == b'.' {
        end -= 1;
    }
    // Find last dot to validate TLD.
    let domain = &s[domain_start..end];
    let last_dot = domain.rfind('.')?;
    let tld = &domain[last_dot + 1..];
    if tld.len() < 2 || !tld.bytes().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let _ = local_end;
    Some(end)
}

fn is_email_local(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'-'
}

/// `key = "value"` / `key: "value"` / `key=value` for a small allowlist of secret-ish keys.
/// Redacts the *value*, keeping `key = "` and the closing quote intact.
fn match_assignment(s: &str, i: usize) -> Option<Match> {
    if !boundary_before(s, i) {
        return None;
    }
    const KEYS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "secret_key",
        "client_secret",
        "auth_token",
        "token",
    ];
    let rest = &s[i..];
    let lower_prefix_ok = KEYS.iter().find(|k| {
        rest.get(..k.len()).is_some_and(|p| p.eq_ignore_ascii_case(k)) && {
            // next char must not be a token char (so "tokenize" doesn't match "token")
            let nb = rest.as_bytes().get(k.len()).copied();
            match nb {
                Some(c) => !is_token_char(c),
                None => true,
            }
        }
    })?;
    let bytes = rest.as_bytes();
    let mut j = lower_prefix_ok.len();
    // optional whitespace
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    // separator: = or :
    if j >= bytes.len() || (bytes[j] != b'=' && bytes[j] != b':') {
        return None;
    }
    j += 1;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    // rest[..j] (the "key = " part) is kept verbatim. The value follows.
    // `inner_start`/`inner_end` bound exactly the bytes we replace. For quoted values, both quotes
    // are left outside the span (kept verbatim — the opening quote via `keep_prefix`, the closing
    // quote because the scanner resumes copying right after `inner_end`).
    let value_start = j;
    let quote = bytes.get(value_start).copied();
    let (inner_start, inner_end) = if quote == Some(b'"') || quote == Some(b'\'') {
        let q = quote.unwrap();
        let mut k = value_start + 1;
        while k < bytes.len() && bytes[k] != q {
            k += 1;
        }
        if k >= bytes.len() {
            return None; // unterminated
        }
        (value_start + 1, k)
    } else {
        // unquoted: value runs to whitespace / end-of-line / common delimiters
        let mut k = value_start;
        while k < bytes.len() && !matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r' | b',' | b';' | b')') {
            if bytes[k] == b'(' {
                // `let token = get_token();` — a call expression, not a literal secret. Redacting
                // the value side would corrupt code.
                return None;
            }
            k += 1;
        }
        // `password: use a strong one` — a short bare word after the key is prose (YAML comments,
        // docs), not a credential. Real inline secrets are longer or contain digits/symbols.
        if k - value_start < 8 && bytes[value_start..k].iter().all(|b| b.is_ascii_alphabetic()) {
            return None;
        }
        (value_start, k)
    };

    // Require the value to be non-trivial (avoid `token = ""` / `key: x`).
    if inner_end - inner_start < 3 {
        return None;
    }
    // Idempotency: if the value is already a placeholder, leave it alone.
    if rest[inner_start..].starts_with(PLACEHOLDER_OPEN) {
        return None;
    }
    Some(Match {
        keep_prefix: inner_start,
        end: i + inner_end,
        kind: Kind::Assignment,
    })
}

/// Long pure hex/base64 blob with a secret-ish keyword nearby.
fn match_keyworded_blob(s: &str, i: usize) -> Option<usize> {
    if !boundary_before(s, i) {
        return None;
    }
    let bytes = s.as_bytes();
    if !is_base64_std_char(bytes[i]) && !is_base64url_char(bytes[i]) {
        return None;
    }

    // Measure a maximal run of base64/hex-ish characters. Decide inclusion *before* updating the
    // purity flags: the terminating character (a closing quote, space, newline…) is not part of
    // the run and must not poison them — it used to, which limited this matcher to blobs sitting
    // at the very end of the input.
    let mut j = i;
    let mut all_hex = true;
    let mut all_b64url = true;
    let mut all_b64std = true;
    while j < bytes.len() {
        let b = bytes[j];
        if !is_hex_char(b) && !is_base64url_char(b) && !is_base64_std_char(b) {
            break;
        }
        all_hex &= is_hex_char(b);
        all_b64url &= is_base64url_char(b);
        all_b64std &= is_base64_std_char(b);
        j += 1;
    }
    let len = j - i;
    if !(all_hex || all_b64url || all_b64std) {
        return None;
    }
    // Pure hex gets a lower floor (32-hex secrets — MD5-width, `secrets.token_hex(16)` — are
    // common); base64 keeps the 40 floor since short base64-ish runs show up in ordinary code.
    let min_len = if all_hex { 32 } else { 40 };
    if len < min_len {
        return None;
    }
    // Guardrail: must have a secret-ish keyword within ~48 chars before the blob (enough to span
    // JSON like `"credentials": {"value": "…"}`).
    if !keyword_before(s, i, 48) {
        return None;
    }
    Some(j)
}

/// `scheme://user:password@host` — redacts the userinfo password, keeping scheme, user, and host
/// visible. Any scheme counts (postgres, mysql, redis, amqp, https git remotes, …).
///
/// Gated under the `assignments` class (it's a key/value credential, counted as one). The user
/// part must end at `:` before any `/`, whitespace, or `@`-less end, so `https://host:8080/path`
/// (a port) and path colons never match. Passwords starting with `$` (env-var references like
/// `${DB_PASSWORD}`) are left alone.
fn match_url_userinfo_password(s: &str, i: usize) -> Option<Match> {
    if !boundary_before(s, i) {
        return None;
    }
    let bytes = s.as_bytes();
    if !bytes[i].is_ascii_alphabetic() {
        return None;
    }
    // scheme: alpha, then alnum / '+' / '-' / '.'
    let mut j = i + 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], b'+' | b'-' | b'.')) {
        j += 1;
    }
    if !s[j..].starts_with("://") {
        return None;
    }
    j += 3;
    // user: runs to ':'; hitting '@', '/', whitespace, or a quote first means no password present.
    while j < bytes.len()
        && !matches!(
            bytes[j],
            b':' | b'@' | b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\''
        )
    {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b':' {
        return None;
    }
    j += 1;
    let pass_start = j;
    // Idempotency: an already-redacted password stays put; env-var refs aren't secrets.
    if s[pass_start..].starts_with(PLACEHOLDER_OPEN) || bytes.get(pass_start) == Some(&b'$') {
        return None;
    }
    // password: anything up to the '@' that introduces the host. A '/' or whitespace first means
    // what we took for a user was a host and the ':' introduced a port/path — no match.
    while j < bytes.len() && !matches!(bytes[j], b'@' | b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'') {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'@' || j == pass_start {
        return None;
    }
    Some(Match {
        keep_prefix: pass_start - i,
        end: j, // leave '@host' intact
        kind: Kind::Assignment,
    })
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
            id: "t".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
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
    fn keep_emails_option() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: "ping a@b.com".into(),
        });
        sess.messages.push(m);
        let (out, _stats) = redact_with(
            &sess,
            &RedactOptions {
                keep_emails: true,
                ..Default::default()
            },
        );
        let t = out.messages[0].text().unwrap();
        assert!(t.contains("a@b.com"), "got: {t}");
    }

    #[test]
    fn scoped_classes_only_private_keys() {
        // The exact ask: scrub PEM blocks, leave email/identity and api-key fixtures untouched.
        let opts = RedactOptions {
            classes: RedactClasses::parse_only("private_key").unwrap(),
            ..Default::default()
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
            input,
            namespace: None,
        });
        sess.messages.push(m);

        let out = redact(&sess);
        let block = &out.messages[0].content[0];
        let Block::ToolUse { input, .. } = block else {
            panic!("expected tool use");
        };
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

        let out = redact(&sess);
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

        let out = redact(&sess);
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
        let (_out, stats) = redact_with(&sess, &RedactOptions::default());
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

        let (out, stats) = redact_with(&sess, &RedactOptions::default());
        let remote = out.git.as_ref().unwrap().remote.as_deref().unwrap();
        assert_eq!(remote, "https://x-access-token:[REDACTED:secret]@github.com/o/r.git");
        assert_eq!(out.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        let extra = serde_json::Value::Object(out.extra.clone()).to_string();
        assert!(!extra.contains("sk-abcDEF"), "session extra leaked: {extra}");
        let mextra = serde_json::Value::Object(out.messages[0].extra.clone()).to_string();
        assert!(!mextra.contains("ghp_"), "message extra leaked: {mextra}");
        assert!(stats.total() >= 3, "stats: {stats:?}");

        // Idempotent across the whole session too.
        let (again, again_stats) = redact_with(&out, &RedactOptions::default());
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
}
