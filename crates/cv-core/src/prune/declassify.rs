//! `--declassify`: snip conversational text dense in caller-supplied terms.
//!
//! `--declassify` is a token-AGNOSTIC primitive. cv-core ships NO built-in token list — the
//! terms are always caller-supplied (CLI `--declassify-tokens` / `--declassify-tokens-file`, i.e.
//! external config/data). This keeps cv itself free of any domain "shitlist" (Ember, PR #9): a
//! list of security badwords baked into the source would (a) be un-adaptable and (b) trip the very
//! classifier this feature exists to dodge whenever an agent reads cv's own code. `declassify` with
//! no tokens is a safe no-op. Composing a security-declassify tool = supply your token file.
use super::{PruneOptions, SourceTextStats};
use anyhow::Result;

/// Match lowercase substrings with token-sized overlap, retaining valid UTF-8 boundaries.
pub(super) struct DeclassMatcher<'a> {
    tokens: &'a [String],
    matched: Vec<bool>,
    overlap: usize,
    tail: String,
}
impl<'a> DeclassMatcher<'a> {
    fn new(tokens: &'a [String]) -> Self {
        Self {
            tokens,
            matched: vec![false; tokens.len()],
            overlap: tokens.iter().map(String::len).max().unwrap_or(0).saturating_sub(1),
            tail: String::new(),
        }
    }
    fn feed(&mut self, lowered: &str) {
        self.tail.push_str(lowered);
        for (hit, token) in self.matched.iter_mut().zip(self.tokens) {
            *hit |= !token.is_empty() && self.tail.contains(token);
        }
        let mut start = self.tail.len().saturating_sub(self.overlap);
        while !self.tail.is_char_boundary(start) {
            start -= 1;
        }
        self.tail.drain(..start);
    }
    fn copy_from(&mut self, source: &Self) {
        self.matched.clone_from(&source.matched);
        self.tail.clone_from(&source.tail);
    }
    fn complete(&self) -> bool {
        self.matched
            .iter()
            .zip(self.tokens)
            .all(|(hit, token)| token.is_empty() || *hit)
    }
    fn hits(&self) -> usize {
        self.matched.iter().filter(|hit| **hit).count()
    }
    fn fold_chunks(&mut self, mut text: &str) {
        while !text.is_empty() && !self.complete() {
            let mut end = text.len().min(64 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.feed(&text[..end].to_lowercase());
            text = &text[end..];
        }
    }
}

pub(super) use crate::scan::{sigma_class, SigmaClass};

/// Only capital Σ needs contextual lowercase. A pending sigma can precede an arbitrarily long
/// ignorable run: advance two bounded matcher states instead of retaining that run's text.
pub(super) struct SigmaMatcher<'a> {
    current: DeclassMatcher<'a>,
    normal: DeclassMatcher<'a>,
    final_sigma: DeclassMatcher<'a>,
    pending: bool,
    before_cased: bool,
    classes: [Option<(char, SigmaClass)>; 128],
}
impl<'a> SigmaMatcher<'a> {
    fn new(tokens: &'a [String]) -> Self {
        Self {
            current: DeclassMatcher::new(tokens),
            normal: DeclassMatcher::new(tokens),
            final_sigma: DeclassMatcher::new(tokens),
            pending: false,
            before_cased: false,
            classes: [None; 128],
        }
    }
    fn class(&mut self, character: char) -> SigmaClass {
        let index = character as usize % self.classes.len();
        if let Some((cached, class)) = self.classes[index] {
            if cached == character {
                return class;
            }
        }
        let class = sigma_class(character);
        self.classes[index] = Some((character, class));
        class
    }
    fn complete(&self) -> bool {
        if self.pending {
            self.normal.complete() && self.final_sigma.complete()
        } else {
            self.current.complete()
        }
    }
    fn resolve(&mut self, class: SigmaClass) {
        if self.pending && class != SigmaClass::Ignorable {
            if class == SigmaClass::Cased {
                std::mem::swap(&mut self.current, &mut self.normal);
            } else {
                std::mem::swap(&mut self.current, &mut self.final_sigma);
            }
            self.pending = false;
        }
    }
    fn run(&mut self, text: &str) {
        if text.is_empty() || self.complete() {
            return;
        }
        // Runs contain no capital sigma, so std lowercase needs no outside context. Only
        // their first/last non-ignorable original scalars affect neighboring sigmas; ASCII
        // runs usually inspect just their endpoints rather than every payload character.
        let first = text
            .chars()
            .map(|c| self.class(c))
            .find(|c| *c != SigmaClass::Ignorable);
        if let Some(first) = first {
            self.resolve(first);
        }
        if self.pending {
            // Both branches see the same ignorable run; lowercase each bounded piece once.
            let mut rest = text;
            while !rest.is_empty() && !self.complete() {
                let mut end = rest.len().min(64 * 1024);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                let lowered = rest[..end].to_lowercase();
                if !self.normal.complete() {
                    self.normal.feed(&lowered);
                }
                if !self.final_sigma.complete() {
                    self.final_sigma.feed(&lowered);
                }
                rest = &rest[end..];
            }
        } else {
            self.current.fold_chunks(text);
        }
        if let Some(last) = text
            .chars()
            .rev()
            .map(|c| self.class(c))
            .find(|c| *c != SigmaClass::Ignorable)
        {
            self.before_cased = last == SigmaClass::Cased;
        }
    }
    fn push(&mut self, mut text: &str) {
        while !text.is_empty() && !self.complete() {
            let Some(index) = text.find('Σ') else {
                self.run(text);
                return;
            };
            self.run(&text[..index]);
            if self.complete() {
                return;
            }
            // Capital sigma is itself non-ignorable and cased, resolving any earlier sigma.
            self.resolve(SigmaClass::Cased);
            if self.before_cased {
                self.normal.copy_from(&self.current);
                self.final_sigma.copy_from(&self.current);
                self.normal.feed("σ");
                self.final_sigma.feed("ς");
                self.pending = true;
            } else {
                self.current.feed("σ");
            }
            self.before_cased = true;
            text = &text[index + 'Σ'.len_utf8()..];
        }
    }
    fn finish(mut self) -> usize {
        if self.pending {
            std::mem::swap(&mut self.current, &mut self.final_sigma);
        }
        self.current.hits()
    }
}

/// Piece-local sigma choices differ only at the complete UTF-8 scalar σ/ς. A valid String
/// needle containing neither cannot cover that position. Sensitive rosters use the same
/// bounded contextual matcher directly; stats and checked decoding always traverse the full
/// value, including after matcher completion, without replaying its source.
pub(super) fn source_declassify_stats(
    text: Option<crate::json::JsonView<'_>>,
    opts: &PruneOptions,
) -> Result<(SourceTextStats, usize)> {
    let mut stats = SourceTextStats::default();
    let sigma_sensitive = opts
        .declassify_tokens
        .iter()
        .any(|token| token.contains('σ') || token.contains('ς'));
    let hits = if sigma_sensitive {
        let mut matcher = SigmaMatcher::new(&opts.declassify_tokens);
        if let Some(text) = text {
            text.for_each_str_chunk(|part| {
                stats.append(part);
                matcher.push(part);
                Ok(())
            })?;
        }
        matcher.finish()
    } else {
        let mut matcher = DeclassMatcher::new(&opts.declassify_tokens);
        if let Some(text) = text {
            text.for_each_str_chunk(|part| {
                stats.append(part);
                matcher.fold_chunks(part);
                Ok(())
            })?;
        }
        matcher.hits()
    };
    Ok((stats, hits))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent whole-string Unicode oracle, never a production codec.
    fn declassify_hits(text: &str, opts: &PruneOptions) -> usize {
        let lower = text.to_lowercase();
        opts.declassify_tokens
            .iter()
            .filter(|token| !token.is_empty() && lower.contains(token.as_str()))
            .count()
    }

    #[test]
    fn source_declassify_matches_case_and_line_semantics_across_chunks() -> Result<()> {
        let world = crate::test_world("prune-declassify-chunks");
        let options = PruneOptions {
            declassify_tokens: vec![
                "alpha".into(),
                "beta".into(),
                "alphabeta".into(),
                "ALPHA".into(),
                "".into(),
                "alpha".into(),
                "ος".into(),
            ],
            ..Default::default()
        };
        for (index, text) in [
            format!("{}ALPHABETA\r\n{}\n", "x".repeat(65533), "x".repeat(65536)),
            format!("{} ΟΣ ΟΣΑ\r\n", "x".repeat(65536)),
            String::new(),
        ]
        .into_iter()
        .enumerate()
        {
            let path = world.base.join(format!("{index}.json"));
            let bytes = serde_json::to_vec(&text)?;
            std::fs::write(&path, &bytes)?;
            let json = crate::Json::source(&path, 0, bytes.len() as u64)?;
            json.with_view(|view| {
                let (stats, hits) = source_declassify_stats(Some(view), &options)?;
                let lower = text.to_lowercase();
                assert_eq!(
                    hits,
                    options
                        .declassify_tokens
                        .iter()
                        .filter(|token| !token.is_empty() && lower.contains(token.as_str()))
                        .count()
                );
                assert_eq!(stats.bytes, text.len());
                assert_eq!(stats.lines(), text.lines().count().max(1));
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn unicode_sigma_classes_and_chunked_matching_equal_std() -> Result<()> {
        // Cased+ignorable overlaps must be ignored, not treated as word context.
        for character in ['\u{0345}', '\u{02b0}', '\u{200d}', '\u{00ad}', '\'', '.', ':', '`'] {
            assert_eq!(sigma_class(character), SigmaClass::Ignorable, "{character:?}");
        }
        for character in ['A', 'a', 'Σ', 'ς', 'ǅ', 'Ⅳ', 'µ'] {
            assert_eq!(sigma_class(character), SigmaClass::Cased, "{character:?}");
        }
        for character in [' ', '\n', '0', '🙂', '京'] {
            assert_eq!(sigma_class(character), SigmaClass::Boundary, "{character:?}");
        }
        let tokens: Vec<String> = [
            "",
            "Σ",
            "A",
            "σ",
            "ς",
            "σς",
            "σσ",
            "aσ",
            "aς",
            "aσ",
            "i\u{0307}",
            "σ\u{0345}",
            "ς\u{0345}",
            "aσ\u{0345}a",
            "aς\u{0345}a",
            "ǆς",
            "🙂 alpha",
            "beta",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        for text in [
            "🙂 ALPHA beta İ",
            "Σ",
            "AΣ",
            "ΣΣ",
            "AΣΣA",
            "\u{0345}Σ",
            "AΣ\u{0345}",
            "AΣ\u{0345}A",
            "AΣ\u{0345}AΣ",
            "AΣ\u{02b0} ",
            "ǅΣ",
            "AΣǅ",
            "ⅣΣ\u{200d}",
            "AΣ\u{00ad} ",
            "AΣ:\u{0345}.",
            "ΣAΣAΣ",
        ] {
            let lowered = text.to_lowercase();
            let expected = tokens
                .iter()
                .filter(|token| !token.is_empty() && lowered.contains(token.as_str()))
                .count();
            for cut in text.char_indices().map(|(offset, _)| offset).chain([text.len()]) {
                let mut matcher = SigmaMatcher::new(&tokens);
                matcher.push(&text[..cut]);
                matcher.push(&text[cut..]);
                assert_eq!(matcher.finish(), expected, "text={text:?}, cut={cut}");
            }
        }
        // Repeated sigmas reuse all three matched-bit allocations, even when choosing both branches.
        let mut matcher = SigmaMatcher::new(&tokens);
        let pointers = [
            matcher.current.matched.as_ptr(),
            matcher.normal.matched.as_ptr(),
            matcher.final_sigma.matched.as_ptr(),
        ];
        let repeated = "AΣ AΣA ".repeat(4096);
        matcher.push(&repeated);
        for pointer in [
            matcher.current.matched.as_ptr(),
            matcher.normal.matched.as_ptr(),
            matcher.final_sigma.matched.as_ptr(),
        ] {
            assert!(pointers.contains(&pointer));
        }
        let lowered = repeated.to_lowercase();
        assert_eq!(
            matcher.finish(),
            tokens
                .iter()
                .filter(|token| !token.is_empty() && lowered.contains(token.as_str()))
                .count()
        );
        Ok(())
    }

    #[test]
    fn unicode_declassification_source_preserves_stats_and_pending_branches() -> Result<()> {
        let world = crate::test_world("prune-unicode-context");
        let options = PruneOptions {
            declassify_tokens: [
                "",
                "aσ",
                "aς",
                "aσ",
                "σ\u{0345}",
                "ς\u{0345}",
                "Σ",
                "i\u{0307}",
                "🙂 alpha",
                "beta",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            ..Default::default()
        };
        let ignorable = "\u{0345}\u{200d}\u{02b0}".repeat(32768);
        for (index, text) in [
            format!("{}🙂 ALPHA beta İ\r\n", "x".repeat(65533)),
            format!("AΣ{ignorable} \r\n"),
            format!("AΣ{ignorable}A\r\n"),
            format!("\u{0345}Σ{ignorable} ΣΣ\n"),
            format!("{}AΣ{}", "x".repeat(65535), "\u{0345}".repeat(65536)),
        ]
        .into_iter()
        .enumerate()
        {
            // Exercise literal UTF-8 and escaped JSON independently of callback framing.
            let literal = serde_json::to_string(&text)?;
            let escaped = literal
                .chars()
                .map(|c| {
                    if c.is_ascii() {
                        c.to_string()
                    } else {
                        let mut units = [0u16; 2];
                        c.encode_utf16(&mut units)
                            .iter()
                            .map(|u| format!("\\u{u:04x}"))
                            .collect::<String>()
                    }
                })
                .collect::<String>();
            for (kind, bytes) in [("literal", literal), ("escaped", escaped)] {
                let path = world.base.join(format!("{index}-{kind}.json"));
                std::fs::write(&path, &bytes)?;
                let source = crate::Json::source(&path, 0, bytes.len() as u64)?;
                source.with_view(|view| {
                    let (stats, hits) = source_declassify_stats(Some(view), &options)?;
                    assert_eq!(stats.bytes, text.len());
                    assert_eq!(stats.lines(), text.lines().count().max(1));
                    assert_eq!(hits, declassify_hits(&text, &options), "case={index}/{kind}");
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    #[test]
    fn sigma_insensitive_tokens_keep_exact_hits_across_context_boundaries() -> Result<()> {
        let world = crate::test_world("prune-sigma-demand");
        let options = PruneOptions {
            declassify_tokens: ["alpha", "βeta", "🙂", "Σ", "A", "", "alpha", "Alpha"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        };
        let text = format!(
            "{}AΣ{} AΣA ALPHA βeta 🙂\r\n",
            "x".repeat(65535),
            "\u{0345}".repeat(65536)
        );
        let bytes = serde_json::to_vec(&text)?;
        let path = world.base.join("source.json");
        std::fs::write(&path, &bytes)?;
        let source = crate::Json::source(&path, 0, bytes.len() as u64)?;
        source.with_view(|view| {
            let (stats, hits) = source_declassify_stats(Some(view), &options)?;
            assert_eq!(hits, declassify_hits(&text, &options));
            assert_eq!(stats.bytes, text.len());
            assert_eq!(stats.lines(), text.lines().count().max(1));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn sigma_context_batches_long_ordinary_runs_without_changing_matches() {
        let tokens = vec!["alpha".into(), "ος".into(), "σx".into(), "ςx".into(), "ς ".into()];
        for text in [
            format!("ALPHA {}ΟΣ ", "x".repeat(1 << 20)),
            format!("AΣ{}AΣ ", "x".repeat(1 << 20)),
            format!("AΣ{}Σ ", "'".repeat(1 << 17)),
        ] {
            let lowered = text.to_lowercase();
            let expected = tokens
                .iter()
                .filter(|token: &&String| !token.is_empty() && lowered.contains(token.as_str()))
                .count();
            for chunk_size in [65535, 65536, 65537] {
                let mut matcher = SigmaMatcher::new(&tokens);
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let mut end = rest.len().min(chunk_size);
                    while !rest.is_char_boundary(end) {
                        end -= 1;
                    }
                    matcher.push(&rest[..end]);
                    rest = &rest[end..];
                }
                assert_eq!(matcher.finish(), expected);
            }
        }
    }
}
