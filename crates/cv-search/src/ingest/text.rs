//! Streaming projection shared by the full-text and trigram index writers.
use anyhow::{Context, Result};
use cv_core::projection::Searchable;
use cv_core::source::{Baseline, FileId, Prefix, Probe, ReadSet};
use cv_core::{Block, Json, Message, Resolver, SessionRef, Text};
use std::path::Path;

pub(crate) const PREVIEW_CHARS: usize = 240;

pub(crate) fn signature(primary: Option<FileId>) -> Option<(i64, i64)> {
    primary.map(|file| (file.mtime_ns as i64, file.len as i64))
}

/// Sample once before streaming. Each index keeps its historical missing-size sentinel.
pub(crate) fn snapshot(probe: &mut Probe, session: &SessionRef, missing_size: i64) -> (Baseline, (i64, i64)) {
    let baseline = probe.snapshot(session);
    let stamp = signature(baseline.primary).unwrap_or((0, missing_size));
    (baseline, stamp)
}

/// Attach exactly this consumer's reads to the original sample. Only a pass with a resumable
/// append checkpoint may certify the captured prefix across subsequent growth.
pub(crate) fn completed(
    baseline: &Baseline,
    session: &SessionRef,
    reads: &ReadSet,
    append: Option<Prefix>,
) -> Baseline {
    match append {
        Some(prefix) => baseline.with_reads_appended(reads, &cv_core::source::primary_path(session), prefix),
        None => baseline.with_reads(reads),
    }
}

pub(crate) fn first_prompt(
    message: &Message,
    resolver: &Resolver,
    primary: &Path,
    reads: &mut ReadSet,
) -> Result<Option<String>> {
    if message.role != cv_core::Role::User {
        return Ok(None);
    }
    for block in &message.content {
        if let Block::Text { text, .. } = block {
            reads.record_text(text, Some(primary));
        }
    }
    message.prompt_title(resolver)
}

pub(crate) trait IndexText {
    const CHUNK_BYTES: usize;
    fn sources(&mut self) -> (&Path, &mut ReadSet);
    fn failed(&self) -> bool;
    fn push(&mut self, text: &str);
    fn push_json(&mut self, input: &Json) -> Result<()>;
}

pub(crate) fn blocks<T: IndexText>(sink: &mut T, content: &[Block], resolver: &Resolver) -> Result<()> {
    cv_core::projection::searchable(content, resolver, &mut Pieces(sink), &mut || Ok(()))
}

struct Pieces<'a, T>(&'a mut T);

impl<T: IndexText> Searchable for Pieces<'_, T> {
    fn full(&self) -> bool {
        self.0.failed()
    }
    fn str(&mut self, text: &str) {
        self.0.push(text);
    }
    fn text(&mut self, text: &Text, resolver: &Resolver, _: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if let Some(inline) = text.inline_str() {
            self.0.push(inline);
            return Ok(());
        }
        let (path, reads) = self.0.sources();
        reads.record_text(text, Some(path));
        text.try_for_each_chunk(resolver, T::CHUNK_BYTES, |part| {
            self.0.push(part);
            anyhow::ensure!(!self.0.failed(), "index output failed");
            Ok(())
        })
        .context("resolving search text")
    }
    fn json(&mut self, input: &Json, _: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        let (path, reads) = self.0.sources();
        reads.record_json(input, Some(path));
        self.0.push_json(input)
    }
    fn after_block(&mut self, block: &Block) -> Result<()> {
        if let Some(path) = block.persisted_output_path().map(Path::new) {
            self.0.sources().1.record(path);
            let read = cv_core::lazy::read_pieces_checked(path, T::CHUNK_BYTES, |part| {
                self.0.push(part);
                anyhow::ensure!(!self.0.failed(), "index output failed");
                Ok(())
            })
            .with_context(|| format!("reading persisted output {}", path.display()))?;
            match read {
                cv_core::lazy::PersistedHead::Unreadable(reason) => {
                    anyhow::bail!("reading persisted output {}: {reason}", path.display())
                }
                cv_core::lazy::PersistedHead::Text(_) => self.0.push("\n"),
                cv_core::lazy::PersistedHead::Absent => {}
            }
        }
        Ok(())
    }
}

pub(crate) fn json_pieces(input: &Json, consume: impl FnMut(&str) -> std::io::Result<()>) -> Result<()> {
    struct Fragments<F>(F);
    impl<F: FnMut(&str) -> std::io::Result<()>> std::io::Write for Fragments<F> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let s = std::str::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            (self.0)(s)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    input
        .write_canonical(&mut Fragments(consume))
        .context("serializing tool input for search")
}

/// Collapse whitespace across formatter fragments, bounded in Unicode characters.
#[derive(Default)]
pub(crate) struct Preview {
    chars: usize,
    word_open: bool,
}
impl Preview {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            chars: text.chars().count(),
            word_open: false,
        }
    }
    pub(crate) fn push(&mut self, out: &mut String, text: &str) {
        for c in text.chars() {
            if self.chars >= PREVIEW_CHARS {
                break;
            }
            if c.is_whitespace() {
                self.word_open = false;
            } else {
                if !self.word_open && !out.is_empty() {
                    out.push(' ');
                    self.chars += 1;
                }
                if self.chars < PREVIEW_CHARS {
                    out.push(c);
                    self.chars += 1;
                }
                self.word_open = true;
            }
        }
    }
}
