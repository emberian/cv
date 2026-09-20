//! 📣 The coordination board — a lightweight append-only message bus for agents.
//!
//! clustervision lets agents *read* each other's sessions; the board lets them *talk*: post status,
//! leave notes, request/answer, broadcast events. A "channel" is a room (often a project path or a
//! named topic). Messages are appended to `$CLUSTERVISION_HOME/board/<channel>.jsonl`. The daemon
//! (`cvd`) can mirror session activity onto channels to build a fleet activity feed; the MCP server
//! exposes post/read and an await-until-regex on top of `read`.
//!
//! OWNED BY the messageboard work. The storage API lives here (dependency-light, no regex); the
//! regex/await loop lives in the `cv` CLI and `cv-mcp` on top of [`read`].
//!
//! # Concurrency & atomicity
//!
//! The board is designed to be safe under many simultaneous writers across *processes* (parallel
//! Claude Codes, Codex, a cloud fleet), not just threads. Each [`post`] does two things:
//!
//! 1. Acquires a per-channel advisory lock: an OS `flock`-style **exclusive lock** on a sibling
//!    `<channel>.lock` file, blocking until the current holder releases. The kernel ties the lock
//!    to the holding process, so a crashed/killed holder's lock evaporates automatically — no
//!    stale-lock timeouts, no steal heuristics, and never two simultaneous holders. This
//!    serializes writers to a given channel. (The lockfile itself is left in place between uses;
//!    only the kernel lock state comes and goes.)
//! 2. Opens the channel file in append mode and writes the serialized message **plus its trailing
//!    newline in a single `write_all`**. A single `write` of less than `PIPE_BUF` bytes to a file
//!    opened `O_APPEND` is atomic on POSIX, so even without the lock individual lines never tear or
//!    interleave; the lock additionally guarantees ordering and covers the (rare) over-`PIPE_BUF`
//!    line. We then `flush`.
//!
//! Readers ([`read`]) tolerate partially-written/garbage lines by skipping anything that fails to
//! parse, so a reader concurrent with a writer never errors — at worst it misses an in-flight line.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Per-channel advisory lock: a [`crate::lockfile::FileLock`] on the `<channel>.lock` file. See
/// that module for the crash-safety and never-unlink rationale.
use crate::lockfile::FileLock as ChannelLock;

/// Root dir for board channels: `$CLUSTERVISION_HOME/board` (or `~/.clustervision/board`).
pub fn board_dir() -> PathBuf {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".clustervision")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("board")
}

/// A single message on a board channel. One of these is serialized per line of `<channel>.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardMessage {
    /// Unique, time-sortable id (uuid v7).
    pub id: String,
    /// The channel (room) this message belongs to. Stored un-slugged; the slug is the filename.
    pub channel: String,
    /// Who posted it (agent/session/human identifier — caller's choice).
    pub from: String,
    /// When it was posted.
    pub ts: DateTime<Utc>,
    /// Message kind: `"msg"` | `"status"` | `"event"` (free-form, but those are the conventions).
    pub kind: String,
    /// The message text / payload.
    pub body: String,
    /// Optional tags for filtering. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Optional reference to a session (e.g. a `SessionRef` id) the message is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
}

/// Slugify a channel name into a safe filename stem: keep `[A-Za-z0-9._-]`, replace the rest with `-`.
fn slug(channel: &str) -> String {
    let s: String = channel
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Avoid empty / dot-only stems that would collide with the directory or hidden files.
    if s.is_empty() || s.chars().all(|c| c == '.') {
        format!("-{s}")
    } else {
        s
    }
}

/// Path to a channel's jsonl file inside `dir`.
fn channel_path(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{}.jsonl", slug(channel)))
}

/// Path to a channel's lockfile inside `dir`.
fn lock_path(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{}.lock", slug(channel)))
}

/// Post a message to `channel` (in the default [`board_dir`]). See [`post_to_dir`].
pub fn post(
    channel: &str,
    from: &str,
    body: &str,
    kind: Option<&str>,
    tags: Vec<String>,
    session_ref: Option<String>,
) -> Result<BoardMessage> {
    post_to_dir(&board_dir(), channel, from, body, kind, tags, session_ref)
}

/// Core of [`post`], parameterized on the board directory (for testing and embedding).
///
/// Generates a uuid v7 `id` and `ts = Utc::now()`, defaults `kind` to `"msg"`, creates `dir` if
/// needed, then appends the serialized message under a per-channel lock (see module docs).
pub fn post_to_dir(
    dir: &Path,
    channel: &str,
    from: &str,
    body: &str,
    kind: Option<&str>,
    tags: Vec<String>,
    session_ref: Option<String>,
) -> Result<BoardMessage> {
    fs::create_dir_all(dir).with_context(|| format!("creating board dir {}", dir.display()))?;

    let msg = BoardMessage {
        id: uuid::Uuid::now_v7().to_string(),
        channel: channel.to_string(),
        from: from.to_string(),
        ts: Utc::now(),
        kind: kind.unwrap_or("msg").to_string(),
        body: body.to_string(),
        tags,
        session_ref,
    };

    // Serialize to a single line (no embedded newlines: serde_json::to_string is single-line).
    let mut line = serde_json::to_string(&msg).context("serializing board message")?;
    line.push('\n');

    let path = channel_path(dir, channel);
    let _lock = ChannelLock::acquire(lock_path(dir, channel))?;

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening board channel {}", path.display()))?;
    // Single write_all of `line` keeps the record atomic vs. other appenders (O_APPEND).
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to board channel {}", path.display()))?;
    f.flush()
        .with_context(|| format!("flushing board channel {}", path.display()))?;

    Ok(msg)
}

/// Read messages from `channel` (in the default [`board_dir`]). See [`read_from_dir`].
pub fn read(channel: &str, since: Option<&str>, limit: usize) -> Result<Vec<BoardMessage>> {
    read_from_dir(&board_dir(), channel, since, limit)
}

/// Core of [`read`], parameterized on the board directory.
///
/// Reads the channel's jsonl, **skipping** any line that fails to parse (tolerates concurrent
/// writers / corruption). Returns messages in file (chronological) order.
///
/// - A missing channel returns an empty `Vec` (not an error).
/// - `since` is a message **id**: when given, only messages *after* the line with that id are
///   returned (cursor/tail semantics for polling). If the id isn't found, all messages are returned.
/// - `limit == 0` means unlimited; otherwise the most recent `limit` messages (after `since`) are
///   returned, still in chronological order.
pub fn read_from_dir(dir: &Path, channel: &str, since: Option<&str>, limit: usize) -> Result<Vec<BoardMessage>> {
    let path = channel_path(dir, channel);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("opening board channel {}", path.display())),
    };

    let mut msgs: Vec<BoardMessage> = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // tolerate read hiccups
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<BoardMessage>(line) {
            msgs.push(m);
        }
        // else: malformed / partially-written line — skip.
    }

    // Apply the `since` cursor: drop everything up to and including the matching id.
    if let Some(cursor) = since {
        if let Some(pos) = msgs.iter().position(|m| m.id == cursor) {
            msgs.drain(..=pos);
        }
        // If not found, leave msgs as-is (return everything).
    }

    // Apply the limit: keep the most recent `limit`, preserving chronological order.
    if limit != 0 && msgs.len() > limit {
        let start = msgs.len() - limit;
        msgs.drain(..start);
    }

    Ok(msgs)
}

/// Convenience: read messages strictly newer than `after` by timestamp (in the default board dir).
pub fn read_since_ts(channel: &str, after: DateTime<Utc>) -> Result<Vec<BoardMessage>> {
    read_since_ts_from_dir(&board_dir(), channel, after)
}

/// Core of [`read_since_ts`], parameterized on the board directory.
pub fn read_since_ts_from_dir(dir: &Path, channel: &str, after: DateTime<Utc>) -> Result<Vec<BoardMessage>> {
    let mut msgs = read_from_dir(dir, channel, None, 0)?;
    msgs.retain(|m| m.ts > after);
    Ok(msgs)
}

/// List channel names (file stems of `*.jsonl`) under the default [`board_dir`].
pub fn channels() -> Result<Vec<String>> {
    channels_in_dir(&board_dir())
}

// ───────────────────────────── coordination primitives ─────────────────────────────
//
// The fns below are *conventions on top of the existing storage*: they are ordinary board
// messages with agreed-upon `kind`s and tags, plus (for claims) one sibling JSON file guarded by
// the very same per-channel lock. Nothing here introduces a new lock or a new race — every write
// reuses [`post_to_dir`]'s O_APPEND atomicity, and the claims file is only ever touched while
// holding [`ChannelLock`].

/// Tag prefix carrying the id a `reply`/`ack` refers to: `reply-to:<request_id>`.
const REPLY_TO_TAG: &str = "reply-to:";

/// Build the `reply-to:<id>` tag.
fn reply_to_tag(id: &str) -> String {
    format!("{REPLY_TO_TAG}{id}")
}

/// Post a `kind="request"` message — a question/ask others can [`reply`] to. See [`request_to_dir`].
pub fn request(channel: &str, from: &str, body: &str) -> Result<BoardMessage> {
    request_to_dir(&board_dir(), channel, from, body)
}

/// Core of [`request`], parameterized on the board directory.
///
/// A request is just a normal message with `kind="request"`; its `id` is the correlation key that
/// replies carry. Collect answers with [`replies_to_dir`].
pub fn request_to_dir(dir: &Path, channel: &str, from: &str, body: &str) -> Result<BoardMessage> {
    post_to_dir(dir, channel, from, body, Some("request"), vec![], None)
}

/// Post a `kind="reply"` answering `in_reply_to`. See [`reply_to_dir`].
pub fn reply(channel: &str, from: &str, in_reply_to: &str, body: &str) -> Result<BoardMessage> {
    reply_to_dir(&board_dir(), channel, from, in_reply_to, body)
}

/// Core of [`reply`], parameterized on the board directory.
///
/// The reply records the request id in **both** `session_ref` (the structured slot) and a
/// `reply-to:<id>` tag (so it survives tag-only filtering); [`replies_to_dir`] matches either.
pub fn reply_to_dir(dir: &Path, channel: &str, from: &str, in_reply_to: &str, body: &str) -> Result<BoardMessage> {
    post_to_dir(
        dir,
        channel,
        from,
        body,
        Some("reply"),
        vec![reply_to_tag(in_reply_to)],
        Some(in_reply_to.to_string()),
    )
}

/// Collect all replies to `request_id` on `channel` (default board dir). See [`replies_to_dir`].
pub fn replies(channel: &str, request_id: &str) -> Result<Vec<BoardMessage>> {
    replies_to_dir(&board_dir(), channel, request_id)
}

/// Core of [`replies`], parameterized on the board directory.
///
/// Returns `kind="reply"` messages whose `session_ref` is `request_id` *or* which carry the
/// `reply-to:<request_id>` tag, in chronological order.
pub fn replies_to_dir(dir: &Path, channel: &str, request_id: &str) -> Result<Vec<BoardMessage>> {
    let want_tag = reply_to_tag(request_id);
    let mut msgs = read_from_dir(dir, channel, None, 0)?;
    msgs.retain(|m| m.kind == "reply" && (m.session_ref.as_deref() == Some(request_id) || m.tags.contains(&want_tag)));
    Ok(msgs)
}

/// Requests on `channel` with zero replies, oldest first (default board dir). See
/// [`unanswered_requests_in_dir`].
pub fn unanswered_requests(channel: &str, within: Duration) -> Result<Vec<(BoardMessage, chrono::Duration)>> {
    unanswered_requests_in_dir(&board_dir(), channel, within)
}

/// Core of [`unanswered_requests`], parameterized on the board directory.
///
/// A dropped question is honest state nobody sees unless it ages on a surface someone reads:
/// returns every `kind="request"` posted within the last `within` that has **zero** replies
/// (correlated exactly as [`replies_to_dir`] does — `session_ref` or the `reply-to:` tag),
/// oldest first, each with its age at read time. Pure projection: it renders age, never escalates.
pub fn unanswered_requests_in_dir(
    dir: &Path,
    channel: &str,
    within: Duration,
) -> Result<Vec<(BoardMessage, chrono::Duration)>> {
    let now = Utc::now();
    let cutoff = now - chrono::Duration::from_std(within).unwrap_or_else(|_| chrono::Duration::zero());
    let msgs = read_from_dir(dir, channel, None, 0)?;
    let answered: Vec<&str> = msgs
        .iter()
        .filter(|m| m.kind == "reply")
        .flat_map(|m| {
            m.session_ref
                .as_deref()
                .into_iter()
                .chain(m.tags.iter().filter_map(|t| t.strip_prefix(REPLY_TO_TAG)))
        })
        .collect();
    let mut out: Vec<(BoardMessage, chrono::Duration)> = msgs
        .iter()
        .filter(|m| m.kind == "request" && m.ts >= cutoff && !answered.contains(&m.id.as_str()))
        .map(|m| (m.clone(), now.signed_duration_since(m.ts)))
        .collect();
    // File order is append order; sort by post time so "oldest first" holds even for
    // out-of-order appends (backfills, clock skew across writers).
    out.sort_by_key(|(m, _)| m.ts);
    Ok(out)
}

/// Acknowledge a message id with a tiny `kind="ack"` note. See [`ack_to_dir`].
pub fn ack(channel: &str, from: &str, msg_id: &str) -> Result<BoardMessage> {
    ack_to_dir(&board_dir(), channel, from, msg_id)
}

/// Core of [`ack`], parameterized on the board directory.
///
/// An ack is a `kind="ack"` message tagged `reply-to:<msg_id>` (and carrying it in `session_ref`),
/// so the same correlation machinery used for replies finds it. Body is left empty.
pub fn ack_to_dir(dir: &Path, channel: &str, from: &str, msg_id: &str) -> Result<BoardMessage> {
    post_to_dir(
        dir,
        channel,
        from,
        "",
        Some("ack"),
        vec![reply_to_tag(msg_id)],
        Some(msg_id.to_string()),
    )
}

// ───────────────────────────── presence / heartbeat ─────────────────────────────

/// Post a `kind="presence"` heartbeat for `from` on `channel`. See [`heartbeat_to_dir`].
pub fn heartbeat(channel: &str, from: &str) -> Result<BoardMessage> {
    heartbeat_to_dir(&board_dir(), channel, from)
}

/// Core of [`heartbeat`], parameterized on the board directory.
///
/// Just a `kind="presence"` message; its `ts` is the heartbeat time. [`who_in_dir`] reads recent
/// ones back. Heartbeats are ordinary append-only lines (no cleanup); callers poll [`who`].
pub fn heartbeat_to_dir(dir: &Path, channel: &str, from: &str) -> Result<BoardMessage> {
    post_to_dir(dir, channel, from, "", Some("presence"), vec![], None)
}

/// The one presence window every surface (CLI `cv board who`, MCP `board_who`, cvd
/// `/api/who/{channel}`) defaults to: an agent is "present" if it heartbeat within this many
/// seconds. Callers can still widen/narrow per call.
pub const WHO_WINDOW_SECS: u64 = 60;

/// List agents that heartbeat on `channel` within the last `within` (default board dir).
pub fn who(channel: &str, within: Duration) -> Result<Vec<String>> {
    who_in_dir(&board_dir(), channel, within)
}

/// Core of [`who`], parameterized on the board directory.
///
/// Returns the distinct `from` of every `kind="presence"` message newer than `now - within`,
/// sorted. An agent with no recent heartbeat simply isn't listed.
pub fn who_in_dir(dir: &Path, channel: &str, within: Duration) -> Result<Vec<String>> {
    let cutoff = Utc::now() - chrono::Duration::from_std(within).unwrap_or_else(|_| chrono::Duration::zero());
    let msgs = read_from_dir(dir, channel, None, 0)?;
    let mut seen: Vec<String> = msgs
        .into_iter()
        .filter(|m| m.kind == "presence" && m.ts >= cutoff)
        .map(|m| m.from)
        .collect();
    seen.sort();
    seen.dedup();
    Ok(seen)
}

// ───────────────────────────── claim / lease ─────────────────────────────
//
// A soft distributed lock so two agents don't grab the same task `key`. Claims live in a sibling
// `<channel>.claims.json` (a small map `key -> {owner, expires_at}`), and *every* mutation of that
// file happens under the existing per-channel [`ChannelLock`] using a read-modify-write. Because
// the lock serializes the whole compare-and-set, the "is there a live claim?" check and the
// "record my claim" write are one atomic step across threads *and* processes — so exactly one
// claimer can win a free (or expired) key. Expired claims are stealable; releasing removes the
// entry. We reuse the lock the board already uses for appends, so a claim and a post never race.

/// One record in a channel's claims file.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClaimRecord {
    /// The task key being claimed (caller-defined, e.g. a file path or task id).
    key: String,
    /// Who holds the claim.
    owner: String,
    /// When the claim lapses; after this instant any other agent may steal `key`.
    expires_at: DateTime<Utc>,
}

/// A held claim. Carries enough to [`release`] it. Cloneable but releasing twice is harmless.
#[derive(Clone, Debug)]
pub struct Lease {
    dir: PathBuf,
    channel: String,
    /// The claimed key.
    pub key: String,
    /// The owner that holds it.
    pub owner: String,
    /// When the claim expires (unless released/renewed first).
    pub expires_at: DateTime<Utc>,
}

/// Path to a channel's sibling claims file.
fn claims_path(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{}.claims.json", slug(channel)))
}

/// Load the claims map (key → record) for a channel. Missing/garbage file → empty map.
fn load_claims(dir: &Path, channel: &str) -> Vec<ClaimRecord> {
    let path = claims_path(dir, channel);
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Atomically persist the claims map: write to a temp sibling then rename over the target so a
/// reader never sees a half-written file. Caller MUST hold the channel lock.
fn store_claims(dir: &Path, channel: &str, claims: &[ClaimRecord]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = claims_path(dir, channel);
    // Per-process-unique temp name: even though writes are channel-locked, a fixed `.json.tmp` left
    // behind by a crashed writer (or a future unlocked caller) could be picked up mid-write. pid+seq
    // makes the temp ours alone, and the rename onto `path` stays atomic.
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let body = serde_json::to_string(claims).context("serializing claims")?;
    fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Try to claim `key` on `channel` for `from`, holding it for `ttl`. See [`claim_in_dir`].
pub fn claim(channel: &str, from: &str, key: &str, ttl: Duration) -> Result<Option<Lease>> {
    claim_in_dir(&board_dir(), channel, from, key, ttl)
}

/// Core of [`claim`], parameterized on the board directory.
///
/// Under the per-channel lock: load claims, drop any that have expired, and check whether `key` is
/// still held by *someone else*. If so, return `Ok(None)`. Otherwise record `{key, from, now+ttl}`
/// (overwriting our own prior/expired entry), persist, and return the [`Lease`]. The whole
/// load→check→write is serialized by [`ChannelLock`], so only one of N concurrent claimers wins.
/// Also emits a `kind="claim"` board event for the activity feed (best-effort, inside the lock).
pub fn claim_in_dir(dir: &Path, channel: &str, from: &str, key: &str, ttl: Duration) -> Result<Option<Lease>> {
    fs::create_dir_all(dir).with_context(|| format!("creating board dir {}", dir.display()))?;
    let _lock = ChannelLock::acquire(lock_path(dir, channel))?;

    let now = Utc::now();
    let mut claims = load_claims(dir, channel);
    // Drop expired entries so they don't linger and so a stale claim is stealable.
    claims.retain(|c| c.expires_at > now);

    if let Some(existing) = claims.iter().find(|c| c.key == key) {
        if existing.owner != from {
            // A live claim by someone else — caller loses the race.
            return Ok(None);
        }
        // We already hold it: fall through to renew (refresh the expiry).
    }

    let expires_at = now + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::zero());
    claims.retain(|c| c.key != key); // remove our own prior entry, if any
    claims.push(ClaimRecord {
        key: key.to_string(),
        owner: from.to_string(),
        expires_at,
    });
    store_claims(dir, channel, &claims)?;

    // No board-feed event here on purpose: we hold the channel lock and `post_to_dir` re-acquires
    // the *same* lockfile, which would deadlock. Callers who want a visible "claim" event can
    // `post` one after this returns (the lock is already released by then).

    Ok(Some(Lease {
        dir: dir.to_path_buf(),
        channel: channel.to_string(),
        key: key.to_string(),
        owner: from.to_string(),
        expires_at,
    }))
}

/// Release a held [`Lease`], freeing its key for others. Idempotent.
///
/// Under the channel lock, removes the claim for `lease.key` **only if we still own it** (so we
/// never yank a claim another agent legitimately stole after ours expired).
pub fn release(lease: &Lease) -> Result<()> {
    let _lock = ChannelLock::acquire(lock_path(&lease.dir, &lease.channel))?;
    let mut claims = load_claims(&lease.dir, &lease.channel);
    let before = claims.len();
    claims.retain(|c| !(c.key == lease.key && c.owner == lease.owner));
    if claims.len() != before {
        store_claims(&lease.dir, &lease.channel, &claims)?;
    }
    Ok(())
}

/// List the currently-active (un-expired) claims on `channel` (default board dir).
pub fn active_claims(channel: &str) -> Result<Vec<(String, String, DateTime<Utc>)>> {
    active_claims_in_dir(&board_dir(), channel)
}

/// Core of [`active_claims`], parameterized on the board directory.
///
/// Returns `(key, owner, expires_at)` for every claim not yet expired, sorted by key. Reads under
/// the channel lock so it never observes a half-written claims file.
pub fn active_claims_in_dir(dir: &Path, channel: &str) -> Result<Vec<(String, String, DateTime<Utc>)>> {
    // A board dir that was never created means nobody ever claimed anything — empty, not an
    // error. (Without this, acquiring the lock `create_new`s a file inside the missing dir and
    // fails with NotFound — the one reader that errored on a fresh $CLUSTERVISION_HOME.)
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let _lock = ChannelLock::acquire(lock_path(dir, channel))?;
    let now = Utc::now();
    let mut out: Vec<_> = load_claims(dir, channel)
        .into_iter()
        .filter(|c| c.expires_at > now)
        .map(|c| (c.key, c.owner, c.expires_at))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Core of [`channels`], parameterized on the board directory. Missing dir → empty list.
pub fn channels_in_dir(dir: &Path) -> Result<Vec<String>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing board dir {}", dir.display())),
    };

    let mut names = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway board dir under the system temp dir, unique per test.
    fn tmp_board() -> PathBuf {
        std::env::temp_dir().join(format!("cv-board-test-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn slug_sanitizes_unsafe_chars() {
        assert_eq!(slug("hello"), "hello");
        assert_eq!(slug("a.b_c-d"), "a.b_c-d");
        assert_eq!(slug("/Users/ember/pug"), "-Users-ember-pug");
        assert_eq!(slug("team chat!"), "team-chat-");
        assert_eq!(slug("café☕"), "caf--"); // non-ascii -> '-'
                                             // Empty / dot-only get a leading '-' so they don't collide with the dir.
        assert_eq!(slug(""), "-");
        assert_eq!(slug(".."), "-..");
    }

    #[test]
    fn slugged_channels_share_a_file() {
        let dir = tmp_board();
        post_to_dir(&dir, "team chat!", "a", "hi", None, vec![], None).unwrap();
        post_to_dir(&dir, "team-chat-", "b", "yo", None, vec![], None).unwrap();
        // Both slug to "team-chat-" so they land in the same channel file.
        let msgs = read_from_dir(&dir, "team chat!", None, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(channels_in_dir(&dir).unwrap(), vec!["team-chat-".to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn post_then_read_in_order() {
        let dir = tmp_board();
        let m1 = post_to_dir(&dir, "ch", "alice", "first", None, vec![], None).unwrap();
        let m2 = post_to_dir(&dir, "ch", "bob", "second", Some("status"), vec!["x".into()], None).unwrap();
        let m3 = post_to_dir(
            &dir,
            "ch",
            "carol",
            "third",
            Some("event"),
            vec![],
            Some("sess-123".into()),
        )
        .unwrap();

        let msgs = read_from_dir(&dir, "ch", None, 0).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].id, m1.id);
        assert_eq!(msgs[0].body, "first");
        assert_eq!(msgs[0].kind, "msg"); // default kind
        assert_eq!(msgs[1].id, m2.id);
        assert_eq!(msgs[1].kind, "status");
        assert_eq!(msgs[1].tags, vec!["x".to_string()]);
        assert_eq!(msgs[2].id, m3.id);
        assert_eq!(msgs[2].kind, "event");
        assert_eq!(msgs[2].session_ref.as_deref(), Some("sess-123"));

        // ids should be monotonically sortable (uuid v7).
        assert!(m1.id < m2.id && m2.id < m3.id);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn since_cursor_returns_only_newer() {
        let dir = tmp_board();
        let m1 = post_to_dir(&dir, "ch", "a", "1", None, vec![], None).unwrap();
        let m2 = post_to_dir(&dir, "ch", "a", "2", None, vec![], None).unwrap();
        let m3 = post_to_dir(&dir, "ch", "a", "3", None, vec![], None).unwrap();

        let after_m1 = read_from_dir(&dir, "ch", Some(&m1.id), 0).unwrap();
        assert_eq!(after_m1.len(), 2);
        assert_eq!(after_m1[0].id, m2.id);
        assert_eq!(after_m1[1].id, m3.id);

        let after_m3 = read_from_dir(&dir, "ch", Some(&m3.id), 0).unwrap();
        assert!(after_m3.is_empty());

        // Unknown cursor -> return everything.
        let unknown = read_from_dir(&dir, "ch", Some("nope"), 0).unwrap();
        assert_eq!(unknown.len(), 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limit_keeps_most_recent() {
        let dir = tmp_board();
        for i in 0..5 {
            post_to_dir(&dir, "ch", "a", &format!("m{i}"), None, vec![], None).unwrap();
        }
        let last2 = read_from_dir(&dir, "ch", None, 2).unwrap();
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].body, "m3");
        assert_eq!(last2[1].body, "m4");

        // since + limit compose: messages after index 0, limited to most-recent 2.
        let all = read_from_dir(&dir, "ch", None, 0).unwrap();
        let after_first = read_from_dir(&dir, "ch", Some(&all[0].id), 2).unwrap();
        assert_eq!(after_first.len(), 2);
        assert_eq!(after_first[1].body, "m4");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_channel_is_empty_not_error() {
        let dir = tmp_board();
        assert!(read_from_dir(&dir, "nope", None, 0).unwrap().is_empty());
        assert!(read_since_ts_from_dir(&dir, "nope", Utc::now()).unwrap().is_empty());
        assert!(channels_in_dir(&dir).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tmp_board();
        post_to_dir(&dir, "ch", "a", "good", None, vec![], None).unwrap();
        // Append complete-but-garbage lines, plus a final un-terminated partial line (which is the
        // only torn shape `post` can ever leave: an in-flight write at EOF).
        let path = channel_path(&dir, "ch");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"this is not json\n   \n{\"partial\": ").unwrap();
        f.flush().unwrap();

        let msgs = read_from_dir(&dir, "ch", None, 0).unwrap();
        // Only the one valid record survives; garbage + blank + partial are skipped.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "good");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_since_ts_filters_by_time() {
        let dir = tmp_board();
        post_to_dir(&dir, "ch", "a", "old", None, vec![], None).unwrap();
        let cut = Utc::now();
        std::thread::sleep(Duration::from_millis(5));
        post_to_dir(&dir, "ch", "a", "new", None, vec![], None).unwrap();

        let newer = read_since_ts_from_dir(&dir, "ch", cut).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].body, "new");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_appends_preserve_all_lines() {
        let dir = tmp_board();
        let n_threads = 8;
        let per_thread = 25;
        std::thread::scope(|s| {
            for t in 0..n_threads {
                let dir = dir.clone();
                s.spawn(move || {
                    for i in 0..per_thread {
                        post_to_dir(
                            &dir,
                            "race",
                            &format!("t{t}"),
                            &format!("msg-{t}-{i}"),
                            None,
                            vec![],
                            None,
                        )
                        .unwrap();
                    }
                });
            }
        });

        let msgs = read_from_dir(&dir, "race", None, 0).unwrap();
        assert_eq!(msgs.len(), n_threads * per_thread);
        // All ids unique (no torn/duplicated lines).
        let mut ids: Vec<_> = msgs.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n_threads * per_thread);
        // The lock must be free again: an immediate re-acquire succeeds. (The lockfile itself
        // stays on disk by design — only the kernel lock state is released.)
        drop(ChannelLock::acquire(lock_path(&dir, "race")).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn abandoned_lockfile_is_harmless_and_single_winner() {
        let dir = tmp_board();
        fs::create_dir_all(&dir).unwrap();
        // A crashed holder left the lockfile behind. With a real advisory lock the file alone
        // holds nothing (the kernel dropped the dead process's lock), so claimers must neither
        // stall on it nor let more than one of them win.
        fs::write(lock_path(&dir, "ch"), b"stale").unwrap();

        let started = std::time::Instant::now();
        let n = 8;
        let winners = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..n {
                let dir = dir.clone();
                let winners = &winners;
                s.spawn(move || {
                    if claim_in_dir(&dir, "ch", &format!("agent-{t}"), "the-key", Duration::from_secs(60))
                        .unwrap()
                        .is_some()
                    {
                        winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
        let active = active_claims_in_dir(&dir, "ch").unwrap();
        assert_eq!(active.len(), 1);
        // The old steal path waited ~20s of backoff before (multiply) stealing; a real lock
        // sails straight through the leftover file.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "claimers stalled on an abandoned lockfile: {:?}",
            started.elapsed()
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ───────────────────────── request / reply / ack ─────────────────────────

    #[test]
    fn request_reply_round_trip() {
        let dir = tmp_board();
        let req = request_to_dir(&dir, "ch", "alice", "what's the build status?").unwrap();
        assert_eq!(req.kind, "request");

        let r1 = reply_to_dir(&dir, "ch", "bob", &req.id, "green").unwrap();
        let r2 = reply_to_dir(&dir, "ch", "carol", &req.id, "still green").unwrap();
        assert_eq!(r1.kind, "reply");
        assert_eq!(r1.session_ref.as_deref(), Some(req.id.as_str()));
        assert!(r1.tags.iter().any(|t| t == &reply_to_tag(&req.id)));

        // A reply to a *different* request must not be collected.
        let other = request_to_dir(&dir, "ch", "alice", "unrelated").unwrap();
        reply_to_dir(&dir, "ch", "dave", &other.id, "nope").unwrap();

        let got = replies_to_dir(&dir, "ch", &req.id).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, r1.id);
        assert_eq!(got[0].body, "green");
        assert_eq!(got[1].id, r2.id);
        assert_eq!(got[1].from, "carol");

        // Unknown request id -> no replies.
        assert!(replies_to_dir(&dir, "ch", "missing").unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replies_matches_tag_even_without_session_ref() {
        let dir = tmp_board();
        let req = request_to_dir(&dir, "ch", "alice", "q").unwrap();
        // Hand-craft a reply that carries only the tag (no session_ref) to prove tag-matching.
        post_to_dir(
            &dir,
            "ch",
            "bob",
            "tagged answer",
            Some("reply"),
            vec![reply_to_tag(&req.id)],
            None,
        )
        .unwrap();
        let got = replies_to_dir(&dir, "ch", &req.id).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "tagged answer");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unanswered_requests_excludes_answered_includes_aged_oldest_first() {
        let dir = tmp_board();
        let day = Duration::from_secs(86_400);

        // An answered request is excluded; an unanswered one is included with a real age.
        let answered = request_to_dir(&dir, "ch", "alice", "answered q").unwrap();
        let open1 = request_to_dir(&dir, "ch", "bob", "open q one").unwrap();
        let open2 = request_to_dir(&dir, "ch", "carol", "open q two").unwrap();
        reply_to_dir(&dir, "ch", "dave", &answered.id, "here you go").unwrap();
        // Non-reply chatter (an ack, a plain msg) does NOT count as an answer.
        ack_to_dir(&dir, "ch", "dave", &open1.id).unwrap();
        post_to_dir(&dir, "ch", "dave", "noise", None, vec![], None).unwrap();

        let got = unanswered_requests_in_dir(&dir, "ch", day).unwrap();
        let ids: Vec<&str> = got.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![open1.id.as_str(), open2.id.as_str()],
            "oldest first, answered gone"
        );
        assert!(
            got.iter().all(|(_, age)| age.num_seconds() >= 0),
            "ages are non-negative"
        );

        // A request older than the window is not surfaced (hand-crafted old ts, appended raw).
        let old = BoardMessage {
            id: "00000000-0000-7000-8000-00000000dead".into(),
            channel: "ch".into(),
            from: "eve".into(),
            ts: Utc::now() - chrono::Duration::days(30),
            kind: "request".into(),
            body: "ancient q".into(),
            tags: vec![],
            session_ref: None,
        };
        let mut f = OpenOptions::new().append(true).open(channel_path(&dir, "ch")).unwrap();
        writeln!(f, "{}", serde_json::to_string(&old).unwrap()).unwrap();
        let got = unanswered_requests_in_dir(&dir, "ch", day).unwrap();
        assert!(!got.iter().any(|(m, _)| m.id == old.id), "outside the window: {got:?}");
        // Widen the window and it shows, first (oldest), with a ~30d age.
        let got = unanswered_requests_in_dir(&dir, "ch", Duration::from_secs(90 * 86_400)).unwrap();
        assert_eq!(got[0].0.id, old.id);
        assert!(got[0].1.num_days() >= 29, "age reflects the old ts: {:?}", got[0].1);

        // A tag-only reply (no session_ref) still answers it.
        post_to_dir(
            &dir,
            "ch",
            "dave",
            "tagged",
            Some("reply"),
            vec![reply_to_tag(&open2.id)],
            None,
        )
        .unwrap();
        let got = unanswered_requests_in_dir(&dir, "ch", day).unwrap();
        assert!(!got.iter().any(|(m, _)| m.id == open2.id), "{got:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ack_is_correlated_to_target() {
        let dir = tmp_board();
        let m = post_to_dir(&dir, "ch", "alice", "deploy done", None, vec![], None).unwrap();
        let a = ack_to_dir(&dir, "ch", "bob", &m.id).unwrap();
        assert_eq!(a.kind, "ack");
        assert_eq!(a.session_ref.as_deref(), Some(m.id.as_str()));
        assert!(a.tags.iter().any(|t| t == &reply_to_tag(&m.id)));
        fs::remove_dir_all(&dir).ok();
    }

    // ───────────────────────── presence / heartbeat ─────────────────────────

    #[test]
    fn who_lists_recent_heartbeaters() {
        let dir = tmp_board();
        heartbeat_to_dir(&dir, "ch", "alice").unwrap();
        heartbeat_to_dir(&dir, "ch", "bob").unwrap();
        heartbeat_to_dir(&dir, "ch", "alice").unwrap(); // dup -> deduped

        let now = who_in_dir(&dir, "ch", Duration::from_secs(60)).unwrap();
        assert_eq!(now, vec!["alice".to_string(), "bob".to_string()]);

        // A zero window excludes everyone (no heartbeat is `>= now`).
        let none = who_in_dir(&dir, "ch", Duration::from_millis(0)).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let none2 = who_in_dir(&dir, "ch", Duration::from_millis(0)).unwrap();
        assert!(none.is_empty() || none2.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    // ───────────────────────── claim / lease ─────────────────────────

    #[test]
    fn claim_blocks_second_claimer_until_release() {
        let dir = tmp_board();
        let lease = claim_in_dir(&dir, "ch", "alice", "task-1", Duration::from_secs(60))
            .unwrap()
            .expect("alice should win the free key");
        assert_eq!(lease.owner, "alice");
        assert_eq!(lease.key, "task-1");

        // A different key is independent and grantable.
        assert!(claim_in_dir(&dir, "ch", "bob", "task-2", Duration::from_secs(60))
            .unwrap()
            .is_some());

        // bob cannot take task-1 while alice's live claim stands.
        assert!(claim_in_dir(&dir, "ch", "bob", "task-1", Duration::from_secs(60))
            .unwrap()
            .is_none());

        // alice re-claiming her own key just renews (still Some).
        assert!(claim_in_dir(&dir, "ch", "alice", "task-1", Duration::from_secs(60))
            .unwrap()
            .is_some());

        // active_claims reflects both holders.
        let active = active_claims_in_dir(&dir, "ch").unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].0, "task-1");
        assert_eq!(active[0].1, "alice");
        assert_eq!(active[1].0, "task-2");

        // After alice releases, bob can take task-1.
        release(&lease).unwrap();
        let bob = claim_in_dir(&dir, "ch", "bob", "task-1", Duration::from_secs(60)).unwrap();
        assert!(bob.is_some());
        assert_eq!(bob.unwrap().owner, "bob");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expired_claim_is_stealable() {
        let dir = tmp_board();
        // A short ttl so it lapses during the test, but long enough that the "still held" check
        // below cannot lose a race with it: under a loaded test runner a 20ms budget expired
        // before bob's first attempt ran, and the assertion failed spuriously.
        let ttl = Duration::from_millis(300);
        let lease = claim_in_dir(&dir, "ch", "alice", "k", ttl).unwrap().unwrap();
        // While alice's lease is live, bob is blocked.
        assert!(claim_in_dir(&dir, "ch", "bob", "k", Duration::from_secs(60))
            .unwrap()
            .is_none());
        // Wait past the ttl; now bob can steal it.
        std::thread::sleep(ttl + Duration::from_millis(100));
        let stolen = claim_in_dir(&dir, "ch", "bob", "k", Duration::from_secs(60)).unwrap();
        assert!(stolen.is_some());
        assert_eq!(stolen.unwrap().owner, "bob");

        // alice releasing her now-superseded lease must NOT yank bob's claim.
        release(&lease).unwrap();
        let active = active_claims_in_dir(&dir, "ch").unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].1, "bob");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_claim_has_exactly_one_winner() {
        let dir = tmp_board();
        let n = 16;
        let winners = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..n {
                let dir = dir.clone();
                let winners = &winners;
                s.spawn(move || {
                    if claim_in_dir(
                        &dir,
                        "ch",
                        &format!("agent-{t}"),
                        "the-one-key",
                        Duration::from_secs(60),
                    )
                    .unwrap()
                    .is_some()
                    {
                        winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
        });
        // Exactly one of the N racing claimers may own the key.
        assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
        let active = active_claims_in_dir(&dir, "ch").unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, "the-one-key");
        fs::remove_dir_all(&dir).ok();
    }
}
