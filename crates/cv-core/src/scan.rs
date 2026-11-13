//! The live scan: substring search over the corpus with no search index at all.
//!
//! Only an explicit request takes it: `cv search --scan` and `cv serve`'s `/api/search?scan=1`
//! (`cv pack` before any `cv index`). A missing or unusable index is an error naming the fix,
//! never this. It is slower than the index by orders of magnitude (it streams every session), so
//! callers say so: the CLI prints a note on stderr, the daemon sets `X-Cv-Search-Source: live`.
//!
//! Streaming discipline: each session streams into [`ScanSink`], whose [`Matcher`] matches the
//! whole searchable text as it passes and keeps only a window around it: the bytes a match can
//! straddle, and the excerpt of the first match. Peak memory per session is O(window), never
//! O(session), and nothing the session says is left unsearched.

use crate::ir::{truncate, Harness, Message, SessionRef};
use crate::lazy::{Resolver, Text};
use crate::projection::Searchable;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use std::path::Path;

/// Bytes an excerpt keeps before its match.
const LEAD: usize = 40;
/// Bytes an excerpt keeps after its match.
const TRAIL: usize = 120;
/// Bytes of the text's start kept for an excerpt when no anchoring needle occurs (200 characters
/// and one more, of at most four bytes each, and more).
const HEAD: usize = 1024;
/// Text is matched in pieces of about this size.
const PIECE: usize = 4096;

/// Where an anchoring needle first matched: [`LEAD`] bytes before the match (fewer at the start of
/// the text), the match at `at..at + len`, and up to [`TRAIL`] bytes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    pub text: String,
    pub at: usize,
    pub len: usize,
}

impl Excerpt {
    fn wanted(&self) -> usize {
        self.at + self.len + TRAIL
    }
}

/// Case-insensitive substring matching over a stream of text in O(window) memory: which needles
/// occur anywhere in it, and an [`Excerpt`] around the first occurrence of each of the first
/// `anchors` needles. Matching lowercases the whole window, so it agrees with
/// `text.to_lowercase().find(needle)` on the whole text: an occurrence is first found in the
/// window that completes it, which still holds its lead-in.
pub struct Matcher {
    needles: Vec<String>,
    found: Vec<bool>,
    /// Per anchoring needle: where its first occurrence starts in the text, and its excerpt.
    excerpts: Vec<Option<(u64, Excerpt)>>,
    /// The last bytes matched: enough for a needle that straddles two pieces, with its lead-in.
    tail: String,
    /// Where `tail` starts in the text.
    tail_at: u64,
    keep: usize,
    /// Undecoded input (a JSON write can end inside a character), matched once a piece is full.
    stage: Vec<u8>,
    /// The first [`HEAD`] bytes: the excerpt when no anchoring needle occurs.
    head: String,
}

impl Matcher {
    /// Match `needles` (lowercased by the caller); the first `anchors` of them keep an excerpt.
    pub fn new(needles: Vec<String>, anchors: usize) -> Self {
        // An original character is at most four times its lowercase bytes ('K' is 3 bytes for 1).
        let guard = 4 * needles.iter().map(String::len).max().unwrap_or(0) + 4;
        Self {
            found: vec![false; needles.len()],
            excerpts: vec![None; anchors.min(needles.len())],
            needles,
            tail: String::new(),
            tail_at: 0,
            keep: LEAD + guard,
            stage: Vec::new(),
            head: String::new(),
        }
    }

    /// Every needle has occurred and every excerpt is complete: the rest changes nothing.
    pub fn done(&self) -> bool {
        self.found.iter().all(|f| *f)
            && self
                .excerpts
                .iter()
                .all(|e| e.as_ref().is_some_and(|(_, e)| e.text.len() >= e.wanted()))
            && self.head.len() >= HEAD
    }

    /// Whether needle `i` occurred.
    pub fn found(&self, i: usize) -> bool {
        self.found[i]
    }

    /// The excerpt of anchoring needle `i`'s first occurrence.
    pub fn excerpt_of(&self, i: usize) -> Option<&Excerpt> {
        self.excerpts.get(i)?.as_ref().map(|(_, e)| e)
    }

    /// The excerpt of the earliest anchoring occurrence, else the text's first [`HEAD`] bytes.
    pub fn excerpt(&self) -> Excerpt {
        self.excerpts
            .iter()
            .flatten()
            .min_by_key(|(at, _)| *at)
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| Excerpt {
                text: self.head.clone(),
                at: 0,
                len: 0,
            })
    }

    /// Whether an anchoring needle occurred.
    pub fn anchored(&self) -> bool {
        self.excerpts.iter().any(Option::is_some)
    }

    /// Bytes held, at most: the window, a staged piece, the head and the excerpts.
    pub fn held(&self) -> usize {
        self.tail.capacity()
            + self.stage.capacity()
            + self.head.capacity()
            + self
                .excerpts
                .iter()
                .flatten()
                .map(|(_, e)| e.text.capacity())
                .sum::<usize>()
    }

    /// Match `text`, a further piece of the stream.
    pub fn push(&mut self, text: &str) {
        self.feed(text.as_bytes());
    }

    fn feed(&mut self, bytes: &[u8]) {
        if self.done() {
            return;
        }
        for part in bytes.chunks(PIECE) {
            self.stage.extend_from_slice(part);
            if self.stage.len() >= PIECE {
                self.flush(false);
            }
        }
    }

    /// Match what is staged: its complete characters, or everything at the end of the text.
    fn flush(&mut self, end: bool) {
        let valid = match std::str::from_utf8(&self.stage) {
            Ok(_) => self.stage.len(),
            Err(e) if e.error_len().is_none() && !end => e.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(&self.stage).into_owned();
                self.stage.clear();
                return self.piece(&text, end);
            }
        };
        let rest = self.stage.split_off(valid);
        let staged = std::mem::replace(&mut self.stage, rest);
        // `valid` ends on a character boundary by construction.
        let text = String::from_utf8(staged).unwrap_or_default();
        self.piece(&text, end);
    }

    fn piece(&mut self, piece: &str, end: bool) {
        if self.head.len() < HEAD {
            let take = ceil_char(piece, HEAD - self.head.len());
            self.head.push_str(&piece[..take]);
        }
        for (_, e) in self.excerpts.iter_mut().flatten() {
            if e.text.len() < e.wanted() {
                let take = ceil_char(piece, e.wanted() - e.text.len());
                e.text.push_str(&piece[..take]);
            }
        }
        let mut window = std::mem::take(&mut self.tail);
        window.push_str(piece);
        let active_end = split_pending_final_sigma(&window, end, 64 * 1024);
        let active = &window[..active_end];
        if self.found.iter().any(|f| !f) && !active.is_empty() {
            let low = active.to_lowercase();
            for (i, needle) in self.needles.iter().enumerate() {
                if self.found[i] {
                    continue;
                }
                let Some(pos) = low.find(needle.as_str()) else { continue };
                self.found[i] = true;
                if let Some(slot) = self.excerpts.get_mut(i) {
                    let range = lowercase_source_range(active, pos..pos + needle.len());
                    let from = floor_char(&window, range.start.saturating_sub(LEAD));
                    let mut excerpt = Excerpt {
                        text: String::new(),
                        at: range.start - from,
                        len: range.len(),
                    };
                    let to = ceil_char(&window, from + excerpt.wanted());
                    excerpt.text = window[from..to].to_string();
                    *slot = Some((self.tail_at + range.start as u64, excerpt));
                }
            }
        }
        let from = ceil_char(&window, active.len().saturating_sub(self.keep)).min(active_end);
        window.drain(..from);
        self.tail_at += from as u64;
        self.tail = window;
    }

    /// End of the text: match what is staged; then read [`Self::excerpt`] or [`Self::excerpt_of`].
    pub fn finish(&mut self) -> Excerpt {
        self.flush(true);
        self.excerpt()
    }
}

/// Unicode Final_Sigma context class for `character` when following `Σ`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigmaClass {
    Cased,
    Ignorable,
    Boundary,
}

/// Query std's own Unicode context rules for Final_Sigma (`{c}Σ` vs `A{c}Σ`).
pub fn sigma_class(character: char) -> SigmaClass {
    if format!("{character}Σ").to_lowercase().ends_with('ς') {
        SigmaClass::Cased
    } else if format!("A{character}Σ").to_lowercase().ends_with('ς') {
        SigmaClass::Ignorable
    } else {
        SigmaClass::Boundary
    }
}

/// When `!end` and `window` ends with `'Σ'` followed only by `Case_Ignorable` characters (within
/// `max_defer_bytes`), return the byte index of `'Σ'` so callers defer lowercasing `'Σ'` until its
/// right-hand cased/boundary context arrives on the next chunk or EOF (`end = true`).
pub fn split_pending_final_sigma(window: &str, end: bool, max_defer_bytes: usize) -> usize {
    if end || !window.contains('Σ') {
        return window.len();
    }
    for (pos, ch) in window.char_indices().rev() {
        if window.len() - pos > max_defer_bytes {
            return window.len();
        }
        match sigma_class(ch) {
            SigmaClass::Ignorable => continue,
            SigmaClass::Cased if ch == 'Σ' => return pos,
            SigmaClass::Cased | SigmaClass::Boundary => return window.len(),
        }
    }
    window.len()
}

impl Searchable for Matcher {
    fn full(&self) -> bool {
        self.done()
    }
    fn str(&mut self, s: &str) {
        self.feed(s.as_bytes());
    }
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        crate::stream::stopped(text.try_for_each_chunk(resolver, PIECE, |part| {
            check()?;
            self.feed(part.as_bytes());
            if self.done() {
                Err(crate::stream::Stop.into())
            } else {
                Ok(())
            }
        }))
        .map(|_| ())
    }
    fn json(&mut self, json: &crate::json::Json, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        struct Fed<'a, 'b> {
            matcher: &'a mut Matcher,
            check: &'a mut (dyn FnMut() -> anyhow::Result<()> + 'b),
        }
        impl std::io::Write for Fed<'_, '_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                (self.check)().map_err(std::io::Error::other)?;
                self.matcher.feed(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        json.write_canonical(&mut Fed { matcher: self, check })
    }
}

/// Streams one session's searchable exchange through a [`Matcher`] (each message dropped as it
/// passes), keeping the first [`Message::prompt_title`] for the label; stops the parse once the
/// matcher is done and the label is known. Stream it under [`Demand::with_spans`] so large
/// content arrives as spans read in pieces.
///
/// The no-index live path for `cv pack`, `cv search` and `cv serve`'s `/api/search` alike.
pub struct ScanSink {
    pub matcher: Matcher,
    pub first_prompt: Option<String>,
    resolver: Resolver,
    error: Option<anyhow::Error>,
    failed: bool,
}

impl ScanSink {
    /// A fresh sink for one session at `path` (the resolver reads lazy spans back from it).
    pub fn new(path: &Path, matcher: Matcher) -> Self {
        ScanSink {
            matcher,
            first_prompt: None,
            resolver: Resolver::new(Some(path.to_path_buf())),
            error: None,
            failed: false,
        }
    }
}

impl MessageSink for ScanSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("live search source read failed")))
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        if self.first_prompt.is_none() {
            match m.prompt_title(&self.resolver) {
                Ok(title) => self.first_prompt = title,
                Err(error) => {
                    self.error = Some(error);
                    self.failed = true;
                    return Flow::Stop;
                }
            }
        }
        // Only the exchange is matched, as the index matches it: injected instruction files and
        // system prompts open nearly every session's head and would answer every query.
        if !m.kind.is_exchange() {
            return Flow::Continue;
        }
        if let Err(error) = crate::projection::searchable(&m.content, &self.resolver, &mut self.matcher, &mut || Ok(()))
        {
            self.error = Some(error);
            self.failed = true;
            return Flow::Stop;
        }
        if self.matcher.done() && self.first_prompt.is_some() {
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

/// The result of a live scan: the first `limit` hits in discovery order, how many sessions
/// matched in all (so a caller can say "N of M"), and the sources and sessions it could not read
/// (so a caller can say what it did not search).
pub struct LiveSearch {
    pub hits: Vec<LiveHit>,
    pub total: usize,
    pub unreadable: Vec<crate::Skipped>,
}

/// Case-insensitive substring search across every session's whole searchable exchange: the first
/// `limit` matches as hits, every match counted. Past `limit` a session is still scanned (to be
/// counted) but no hit is built for it.
///
/// Caller metadata selection is applied before payload reads.
pub fn live_search_filtered(
    query: &str,
    want: Option<Harness>,
    limit: usize,
    mut select: impl FnMut(&crate::SessionRef) -> bool,
) -> Result<LiveSearch> {
    let _sources = crate::source::read_scope();
    let needle = query.to_lowercase();
    let mut out = LiveSearch {
        hits: Vec::new(),
        total: 0,
        unreadable: Vec::new(),
    };
    for adapter in crate::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.roots().is_empty() {
            continue;
        }
        let found = adapter.discover()?;
        out.unreadable.extend(found.skipped);
        for r in found.refs {
            if !select(&r) {
                continue;
            }
            // One session that cannot be read is skipped and named, never the end of the scan; one
            // that vanished since discovery is simply gone.
            let mut sink = ScanSink::new(&r.path, Matcher::new(vec![needle.clone()], 1));
            let meta = match adapter
                .try_stream(&r, &Demand::LEAN.with_spans(), &mut sink)
                .with_context(|| format!("scanning {}:{}", r.harness, r.id))
            {
                Ok(meta) => meta,
                Err(error) => {
                    out.unreadable.extend(crate::Skipped::of(&r.path, &error));
                    continue;
                }
            };
            let excerpt = sink.matcher.finish();
            if !sink.matcher.anchored() {
                continue;
            }
            out.total += 1;
            if out.hits.len() < limit {
                out.hits.push(LiveHit {
                    title: crate::label_from(meta.title.as_deref(), sink.first_prompt.as_deref()),
                    snippet: snippet(&excerpt.text, excerpt.at, excerpt.len),
                    session: r,
                });
            }
        }
    }
    Ok(out)
}

/// Map a byte range in `source.to_lowercase()` back to enclosing original UTF-8 boundaries.
/// Matching must still use whole-string lowercase: Greek final sigma depends on its context.
/// Per-character lowercase lengths are sufficient for mapping because sigma's contextual forms
/// have equal encoded lengths. Matches within an expansion (such as İ → i + combining dot)
/// include the complete original character.
pub fn lowercase_source_range(source: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    if source.is_ascii() {
        return range.start.min(source.len())..range.end.min(source.len());
    }
    let mut folded = 0;
    let mut start = source.len();
    let mut end = source.len();
    for (offset, ch) in source.char_indices() {
        let width: usize = ch.to_lowercase().map(char::len_utf8).sum();
        if range.start >= folded && range.start < folded + width {
            start = offset;
        } else if range.start == folded + width {
            start = offset + ch.len_utf8();
        }
        if range.end <= folded {
            end = offset;
            break;
        }
        if range.end <= folded + width {
            end = offset + ch.len_utf8();
            break;
        }
        folded += width;
    }
    start..end
}

/// A one-line excerpt of `hay` around `pos`, with ~40 bytes of lead-in and lead-out.
pub fn snippet(hay: &str, pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(40);
    let end = pos.saturating_add(len).saturating_add(40).min(hay.len());
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
    use crate::ir::{Block, Role};

    /// A session the scan cannot read is skipped and named; the rest are still searched.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_session_is_named_not_the_end_of_the_scan() {
        use std::os::unix::fs::PermissionsExt;
        let world = crate::test_world("live-unreadable");
        let line = |text: &str| serde_json::json!({"type":"user", "message":{"role":"user", "content":text}});
        let locked = world.write_session("locked", &[line("match locked")]);
        world.write_session("open", &[line("match open")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let results = live_search_filtered("match", Some(Harness::Claude), 10, |_| true);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        let results = results.unwrap();
        assert_eq!(
            results.hits.iter().map(|h| h.session.id.as_str()).collect::<Vec<_>>(),
            ["open"]
        );
        assert_eq!(
            results.unreadable.iter().map(|s| &s.path).collect::<Vec<_>>(),
            [&locked]
        );
    }

    #[test]
    fn empty_live_window_counts_matches_and_snippets_saturate_extreme_positions() {
        let world = crate::test_world("live-empty-window");
        world.write_session(
            "match",
            &[serde_json::json!({"type":"user", "message":{"role":"user", "content":"match"}})],
        );
        // A zero window shows nothing and still counts what matched.
        let results = live_search_filtered("match", Some(Harness::Claude), 0, |_| true).unwrap();
        assert!(results.hits.is_empty());
        assert_eq!(results.total, 1);
        assert_eq!(snippet("short", usize::MAX, usize::MAX), "");
        assert_eq!(snippet("short", 0, usize::MAX), "short");
    }

    #[test]
    fn unicode_lowercase_ranges_preserve_source_matches_and_context() {
        for (source, needle, expected) in [
            ("İİ target tail", "target", "target"),
            ("ȺȺ target tail", "target", "target"),
            ("İstanbul", "i", "İ"),
            ("İstanbul", "\u{307}", "İ"),
            ("ΟΣ next", "ος", "ΟΣ"),
            ("ΟΣΑ next", "οσα", "ΟΣΑ"),
            ("ΟΣ next", "ς", "Σ"),
        ] {
            let lowered = source.to_lowercase();
            let pos = lowered.find(needle).unwrap();
            let range = lowercase_source_range(source, pos..pos + needle.len());
            assert_eq!(&source[range], expected, "{source:?} / {needle:?}");
        }
        // Old lowercased offsets omit the actual match after enough expanding characters.
        let source = format!("{}target {}", "İ".repeat(200), "end ".repeat(100));
        let lowered = source.to_lowercase();
        let pos = lowered.find("target").unwrap();
        let range = lowercase_source_range(&source, pos..pos + 6);
        let original = source.find("target").unwrap();
        assert_eq!(range, original..original + 6);
        assert_eq!(
            snippet(&source, range.start, range.len()),
            truncate(&source[original - 40..original + 6 + 40], 120)
        );
        assert!(!snippet(&source, pos, 6).contains("target"));
    }

    /// The matcher agrees with lowercasing the whole text and finding each needle in it, however
    /// the text arrives in pieces, and its excerpt is the whole text's window.
    #[test]
    fn matcher_agrees_with_the_whole_text_in_any_pieces() {
        let texts = [
            format!("{}NEEDLE{}", "a".repeat(5000), "b".repeat(300)),
            format!("{}target {}", "İ".repeat(3000), "end ".repeat(100)),
            "ΟΣ next ος tail".to_string(),
            "short".to_string(),
            format!("{}needle", "x".repeat(9000)),
            format!("{}Needle and NEEDLE", "🌙".repeat(1500)),
        ];
        let needle_sets: [&[&str]; 4] = [&["needle"], &["target", "end"], &["ος"], &["absent", "short"]];
        for text in &texts {
            for needles in needle_sets {
                let low = text.to_lowercase();
                let first = needles
                    .iter()
                    .filter_map(|n| low.find(n).map(|p| lowercase_source_range(text, p..p + n.len())))
                    .min_by_key(|r| r.start);
                for piece in [1usize, 3, 7, 64, 4096, usize::MAX] {
                    let mut m = Matcher::new(needles.iter().map(|n| n.to_string()).collect(), needles.len());
                    let bytes = text.as_bytes();
                    for part in bytes.chunks(piece.min(bytes.len()).max(1)) {
                        m.feed(part);
                    }
                    let excerpt = m.finish();
                    for (i, n) in needles.iter().enumerate() {
                        assert_eq!(m.found(i), low.contains(n), "{n:?} in {piece}-byte pieces");
                    }
                    match &first {
                        Some(r) => {
                            assert!(m.anchored());
                            assert_eq!(
                                snippet(&excerpt.text, excerpt.at, excerpt.len),
                                snippet(text, r.start, r.len()),
                                "{needles:?} in {piece}-byte pieces"
                            );
                        }
                        None => assert!(!m.anchored()),
                    }
                }
            }
        }
    }

    /// A match past any head is found, and what the scan holds is a window, not the session.
    #[test]
    fn scan_reads_the_whole_exchange_in_a_window() {
        let mut sink = ScanSink::new(Path::new("unused"), Matcher::new(vec!["zqlast".into()], 1));
        let input = serde_json::json!({"content": "🌙\n".repeat(1 << 20)});
        let mut message = Message::new(Role::User);
        message.content = vec![
            Block::Text { text: "prompt".into() },
            Block::ToolUse {
                id: "t".into(),
                name: "Edit".into(),
                input: input.into(),
                namespace: None,
            },
            Block::File {
                mime: None,
                path: Some(format!("{}ZQLAST tail", "later".repeat(1 << 20))),
                source: None,
            },
        ];
        assert_eq!(sink.message(message), Flow::Continue);
        let excerpt = sink.matcher.finish();
        assert!(sink.matcher.found(0));
        assert_eq!(&excerpt.text[excerpt.at..excerpt.at + excerpt.len], "ZQLAST");
        assert_eq!(sink.first_prompt.as_deref(), Some("prompt"));
        assert!(sink.matcher.held() < 64 * 1024, "{}", sink.matcher.held());
    }

    #[test]
    fn retained_source_text_is_matched_whole_and_guard_errors() {
        let dir = cv_testkit::temp_dir("live-head-retained-source");
        let path = dir.join("source.json");
        let conditional = dir.join("sidecar");
        for late_whitespace in [false, true] {
            let decoded = format!(
                "{}{}zqend",
                if late_whitespace {
                    " ".repeat(crate::ir::TITLE_HEAD as usize)
                } else {
                    "source label\n".into()
                },
                "é🙂".repeat(1 << 18)
            );
            std::fs::write(&path, serde_json::to_vec(&decoded).unwrap()).unwrap();
            let mut reads = crate::source::ReadSet::default();
            reads.record(&conditional);
            let value = crate::json::Json::source_with_reads(&path, 0, std::fs::metadata(&path).unwrap().len(), &reads)
                .unwrap();
            let text = value.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
            assert!(matches!(text, Text::Source(_)));
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.clone() });
            let mut sink = ScanSink::new(&path, Matcher::new(vec!["zqend".into()], 1));
            assert_eq!(sink.message(message), Flow::Continue);
            assert!(sink.take_error().is_none());
            sink.matcher.finish();
            assert!(sink.matcher.found(0));
            assert!(sink.matcher.held() < 64 * 1024, "{}", sink.matcher.held());
            assert_eq!(sink.first_prompt.is_none(), late_whitespace);
            std::fs::write(&conditional, format!("dependency {late_whitespace}")).unwrap();
            let mut stale = ScanSink::new(&path, Matcher::new(vec!["zqend".into()], 1));
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::Text { text });
            assert!(stale.try_message(message).is_err());
            assert!(stale.take_error().is_some());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_text_demand_stays_latched_for_body_and_label() {
        let dir = cv_testkit::temp_dir("live-head-text-error");
        let path = dir.join("absent");
        for role in [Role::User, Role::Assistant] {
            let mut sink = ScanSink::new(&path, Matcher::new(vec!["x".into()], 1));
            let mut message = Message::new(role);
            message.content.push(Block::Text {
                text: Text::Span(crate::Span {
                    source: Some(path.clone()),
                    offset: 0,
                    len: 8192,
                    escaped: false,
                }),
            });
            assert!(sink.try_message(message).is_err());
            assert_eq!(sink.message(Message::new(Role::User)), Flow::Stop);
            assert!(sink.take_error().is_some());
            assert!(!sink.matcher.anchored());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_source_is_not_a_successful_short_head() {
        let dir = cv_testkit::temp_dir("live-head-source-error");
        let path = dir.join("input.json");
        std::fs::write(&path, "{}").unwrap();
        let input = crate::json::Json::source(&path, 0, 2).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut sink = ScanSink::new(&path, Matcher::new(vec!["x".into()], 1));
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "t".into(),
            name: "Edit".into(),
            input,
            namespace: None,
        });
        assert!(sink.try_message(message).is_err());
        assert_eq!(sink.message(Message::new(Role::User)), Flow::Stop);
        assert!(sink.take_error().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

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

    #[test]
    fn streaming_matcher_resolves_final_sigma_across_chunk_boundary() {
        // Chunk 1 ends right after capital `'Σ'` (`PIECE = 4096`), followed by `'Β'` in chunk 2:
        // `'Σ'` before a cased letter lowercases to medial `'σ'`, NOT final `'ς'`.
        let pad = "a".repeat(PIECE - "ΟΣ".len());
        let mut m_false = Matcher::new(vec!["ος".into()], 1);
        m_false.str(&pad);
        m_false.str("ΟΣ");
        m_false.str("Β");
        m_false.finish();
        assert!(
            !m_false.anchored(),
            "medial ΟΣΒ across chunk boundary must not match final ος"
        );

        let mut m_medial = Matcher::new(vec!["οσβ".into()], 1);
        m_medial.str(&pad);
        m_medial.str("ΟΣ");
        m_medial.str("Β");
        m_medial.finish();
        assert!(m_medial.anchored(), "medial ΟΣΒ across chunk boundary must match οσβ");

        let mut m_final_eof = Matcher::new(vec!["ος".into()], 1);
        m_final_eof.str(&pad);
        m_final_eof.str("ΟΣ");
        m_final_eof.finish();
        assert!(m_final_eof.anchored(), "trailing ΟΣ at EOF must match final ος");
    }
}
