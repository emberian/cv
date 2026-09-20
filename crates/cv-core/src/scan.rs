//! The no-index live scan: substring search over the corpus with no search index at all.
//!
//! This is the *degraded* path — `cv search` takes it before `cv index` has ever run, and `cvd`'s
//! `/api/search` takes it when the tantivy index is missing or unreadable, so a fresh machine and
//! a broken index both still answer a query instead of erroring. It is slower than the index by
//! orders of magnitude (it streams every session) and it only sees each session's capped head, so
//! callers say so: the CLI prints a `(no index yet — scanning live…)` note, the daemon sets
//! `X-Cv-Search-Source: live`.
//!
//! Streaming discipline: each session streams into [`CapSink`], which keeps at most
//! [`LIVE_HAY_BYTES`] of searchable text and drops every message as it passes. Peak memory per
//! session is O(cap), never O(session) — an unbounded haystack here once meant multi-GB RSS on a
//! real corpus.

use crate::ir::{truncate, Block, Harness, Message, Role, SessionRef};
use crate::lazy::{Resolver, Text};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use std::fmt::Write as _;
use std::path::Path;

/// Searchable-haystack cap per session for the no-index live scan.
pub const LIVE_HAY_BYTES: usize = 256 * 1024;

/// Builds a capped searchable head (`hay`, at most [`LIVE_HAY_BYTES`]) as messages stream past
/// (each dropped immediately), grabbing the first user text for the label fallback; stops the
/// parse once the cap is hit and a first-user label candidate exists. Stream it under
/// [`ParseOptions::lazy`] so large content arrives as spans resolved only up to the remaining
/// budget — peak per session is O(cap), never O(session).
///
/// The no-index live path for `cv pack`, `cv search` and `cvd`'s `/api/search` alike.
pub struct CapSink {
    pub hay: String,
    pub first_user: Option<String>,
    resolver: Resolver,
}

impl CapSink {
    /// A fresh sink for one session at `path` (the resolver reads lazy spans back from it).
    pub fn new(path: &Path) -> Self {
        CapSink {
            hay: String::new(),
            first_user: None,
            resolver: Resolver::new(Some(path.to_path_buf())),
        }
    }

    fn push_text(&mut self, text: &Text) {
        let remaining = LIVE_HAY_BYTES.saturating_sub(self.hay.len());
        if remaining == 0 {
            return;
        }
        if let Some(sp) = text.as_span() {
            self.hay.push_str(&self.resolver.resolve_prefix(sp, remaining as u64));
        } else if let Some(t) = text.inline_str() {
            self.hay.push_str(&t[..floor_char(t, remaining.min(t.len()))]);
        }
        self.hay.push('\n');
    }
}

impl MessageSink for CapSink {
    fn message(&mut self, m: Message) -> Flow {
        if self.first_user.is_none() && m.role == Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    if let Some(t) = text.inline_str().filter(|t| !t.trim().is_empty()) {
                        self.first_user = Some(t.to_string());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            match b {
                Block::Text { text } | Block::Thinking { text, .. } => self.push_text(text),
                Block::ToolUse { name, input, .. } => {
                    if self.hay.len() < LIVE_HAY_BYTES {
                        self.hay.push_str(name);
                        self.hay.push(' ');
                        let _ = write!(self.hay, "{input}");
                        self.hay.push('\n');
                    }
                }
                Block::ToolResult { content, .. } => self.push_text(content),
                Block::File { path, source, .. } => {
                    if let Some(p) = path.as_deref().or(source.as_deref()) {
                        self.hay.push_str(p);
                        self.hay.push('\n');
                    }
                }
                Block::Image { .. } => {}
            }
        }
        if self.hay.len() >= LIVE_HAY_BYTES && self.first_user.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// One live-scan match: the discovered ref, the label to show for it, and the excerpt around the
/// first occurrence of the needle.
pub struct LiveHit {
    pub session: SessionRef,
    pub title: String,
    pub snippet: String,
}

/// The result of a live scan: the hits in discovery order, and whether the scan stopped early
/// because it filled `limit` (so a caller can say "there may be more").
pub struct LiveSearch {
    pub hits: Vec<LiveHit>,
    pub truncated: bool,
}

/// Case-insensitive substring search across every session's capped head, stopping at `limit` hits.
///
/// Trade-off, stated plainly: a match beyond a session's capped head is missed here; finding those
/// is the index's job (`cv index`).
pub fn live_search(query: &str, want: Option<Harness>, limit: usize) -> Result<LiveSearch> {
    let needle = query.to_lowercase();
    let mut out = LiveSearch {
        hits: Vec::new(),
        truncated: false,
    };
    for adapter in crate::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = CapSink::new(&r.path);
            let meta = match adapter.stream(&r, &ParseOptions::lazy(), &mut sink) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Lowercase only the bounded head (the match check), windowing the snippet from the
            // original-case haystack.
            let low = sink.hay.to_lowercase();
            let Some(pos) = low.find(&needle) else { continue };
            out.hits.push(LiveHit {
                title: crate::label_from(meta.title.as_deref(), sink.first_user.as_deref()),
                snippet: snippet(&sink.hay, pos.min(sink.hay.len()), needle.len()),
                session: r,
            });
            if out.hits.len() >= limit {
                out.truncated = true;
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// A one-line excerpt of `hay` around `pos`, with ~40 bytes of lead-in and lead-out.
pub fn snippet(hay: &str, pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(40);
    let end = (pos + len + 40).min(hay.len());
    let s = &hay[floor_char(hay, start)..ceil_char(hay, end)];
    truncate(&s.replace('\n', " "), 120)
}

/// Round `i` down/up to the nearest char boundary of `s` (both clamp to `s.len()` first).
pub fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// See [`floor_char`].
pub fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_window_the_match_and_stay_on_char_boundaries() {
        let hay = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
        let s = snippet(&hay, 100, 6);
        assert!(s.contains("NEEDLE"), "{s}");
        assert!(s.starts_with(&"a".repeat(40)) && s.len() <= 120, "{s}");
        // Multi-byte text must not be sliced mid-codepoint.
        let uni = "日本語テキスト NEEDLE 日本語テキスト";
        let pos = uni.find("NEEDLE").unwrap();
        assert!(snippet(uni, pos, 6).contains("NEEDLE"));
        // An out-of-range position clamps to an empty window instead of panicking.
        assert_eq!(snippet("short", 900, 6), "");
    }
}
