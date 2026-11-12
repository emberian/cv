//! Retained JSON-string backing with original source provenance and bounded decoded demands.
use crate::json::{Json, JsonRange};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use anyhow::{Context, Result};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SourceText {
    pub(crate) range: Arc<JsonRange>,
    pub(crate) skip: usize,
    // Immutable producer count for a complete generated string. Private deferred transport may
    // omit this optional optimization; restored strings retain the checked decoding fallback.
    known_len: Option<usize>,
}
impl PartialEq for SourceText {
    fn eq(&self, other: &Self) -> bool {
        self.skip == other.skip && self.range == other.range
    }
}
impl Eq for SourceText {}
impl SourceText {
    pub(crate) fn new(range: Arc<JsonRange>) -> Self {
        Self {
            range,
            skip: 0,
            known_len: None,
        }
    }
    /// A producer-validated string whose decoded byte count is already known.
    pub(crate) fn generated(range: Arc<JsonRange>, decoded_len: usize) -> Self {
        Self {
            known_len: Some(decoded_len),
            ..Self::new(range)
        }
    }
    pub fn path(&self) -> &std::path::Path {
        self.range.path()
    }
    pub fn dependencies(&self) -> Option<&crate::source::ReadSet> {
        self.range.dependencies()
    }
    pub(crate) fn json(&self) -> Json {
        Json::Source(self.range.clone())
    }
    /// Upper bound for the retained decoded body: the range includes its two JSON quotes;
    /// the skipped prefix counts decoded bytes. Escapes may make the actual body shorter.
    pub fn raw_len(&self) -> usize {
        usize::try_from(self.range.len().saturating_sub(2))
            .unwrap_or(usize::MAX)
            .saturating_sub(self.skip)
    }
    pub fn try_for_each_chunk(&self, chunk: usize, mut f: impl FnMut(&str) -> Result<()>) -> Result<()> {
        let mut skip = self.skip;
        let mut scratch = String::new();
        let mut visit = |mut part: &str| {
            if skip >= part.len() {
                skip -= part.len();
                return Ok(());
            }
            part = part.get(skip..).context("source text prefix is not a UTF8 boundary")?;
            skip = 0;
            while !part.is_empty() {
                let mut end = part.len().min(chunk.max(1));
                while !part.is_char_boundary(end) {
                    end += 1;
                }
                // Stable scratch remains valid if a callback mutates/truncates the original.
                scratch.clear();
                scratch.push_str(&part[..end]);
                let result = f(&scratch);
                self.range.captured_source().check()?;
                result?;
                part = &part[end..];
            }
            Ok(())
        };
        self.range.with_view(|view| {
            anyhow::ensure!(
                view.for_each_str_chunk(&mut visit)?,
                "source text backing is not a JSON string"
            );
            Ok(())
        })?;
        anyhow::ensure!(skip == 0, "source text prefix is past end");
        Ok(())
    }

    pub fn try_resolve(&self) -> Result<String> {
        let mut output = String::new();
        self.try_for_each_chunk(64 * 1024, |part| {
            output.push_str(part);
            Ok(())
        })?;
        Ok(output)
    }
    /// A decoded UTF8 byte prefix with strict JSON semantics.
    pub fn try_prefix(&self, cap: usize) -> Result<String> {
        self.try_prefix_with_truncation(cap).map(|(text, _)| text)
    }
    /// Retain whether the next scalar did not fit, even when UTF8 rounding leaves unused bytes.
    /// Revalidate the full source guard before treating the internal prefix cutoff as success.
    pub fn try_prefix_with_truncation(&self, cap: usize) -> Result<(String, bool)> {
        let mut output = String::with_capacity(cap.min(64 * 1024));
        let result = self.try_for_each_chunk(64 * 1024, |part| {
            let mut end = part.len().min(cap - output.len());
            while !part.is_char_boundary(end) {
                end -= 1;
            }
            let required = output.len() + end;
            if required > output.capacity() {
                output.reserve_exact(required - output.len());
            }
            output.push_str(&part[..end]);
            if end < part.len() {
                return Err(crate::stream::Stop.into());
            }
            Ok(())
        });
        let truncated = match result {
            Err(error) if error.downcast_ref::<crate::stream::Stop>().is_some() => true,
            other => {
                other?;
                false
            }
        };
        Ok((output, truncated))
    }
    pub(crate) fn validate_backing(&self) -> Result<()> {
        self.range.with_view(|view| {
            anyhow::ensure!(view.is_string()?, "source text backing is not a JSON string");
            Ok(())
        })?;
        if self.skip > 0 {
            self.try_decoded_len()?;
        }
        Ok(())
    }
    pub fn try_decoded_len(&self) -> Result<usize> {
        if self.skip == 0 {
            if let Some(len) = self.known_len {
                self.range.check_retained_bounds()?;
                return Ok(len);
            }
        }
        let mut len = 0usize;
        let mut skip = self.skip;
        // This reducer exposes no borrowed bytes or callbacks to its caller. Count directly in
        // the existing decoder; the scoped view checks original provenance before and after the
        // whole demand, without copying fragments or checking every external-callback boundary.
        self.range.with_view(|view| {
            anyhow::ensure!(
                view.for_each_str_chunk(|part| {
                    if skip >= part.len() {
                        skip -= part.len();
                        return Ok(());
                    }
                    let part = part.get(skip..).context("source text prefix is not a UTF8 boundary")?;
                    skip = 0;
                    len = len.checked_add(part.len()).context("source text length overflow")?;
                    Ok(())
                })?,
                "source text backing is not a JSON string"
            );
            anyhow::ensure!(skip == 0, "source text prefix is past end");
            Ok(len)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{harness::store::bytes::ByteSource, json::JsonSource, lazy::Resolver};

    #[test]
    fn decoded_lengths_preserve_prefix_boundaries_and_source_guards() {
        let world = crate::test_world("source-text-length");
        let path = world.base.join("text.json");
        let body = "a🌙\n\"\\".repeat(8192);
        let bytes = serde_json::to_vec(&body).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let Json::Source(range) = Json::source(&path, 0, bytes.len() as u64).unwrap() else {
            panic!("source descriptor required");
        };
        for skip in [0, 1, 2, 4, 5, 6, body.len() - 1, body.len(), body.len() + 1] {
            let source = SourceText {
                range: range.clone(),
                skip,
                known_len: None,
            };
            match body.get(skip..) {
                Some(suffix) => assert_eq!(source.try_decoded_len().unwrap(), suffix.len()),
                None => assert!(source.try_decoded_len().is_err(), "accepted invalid skip {skip}"),
            }
        }
        let source = SourceText::new(range);
        std::fs::remove_file(path).unwrap();
        assert!(source.try_decoded_len().is_err());
    }

    #[test]
    fn giant_prefix_demands_preserve_utf8_and_original_identity() {
        let world = crate::test_world("source-text-prefix-window");
        let path = world.base.join("original.json");
        let body = format!("🌙\n{}", "x".repeat(1024 * 1024));
        std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let text = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |v| {
                v.source_text()
            })
            .unwrap()
            .unwrap();
        let resolver = Resolver::new(None);
        for _ in 0..16 {
            assert_eq!(text.try_resolve_prefix(&resolver, 3).unwrap(), "");
            assert_eq!(text.try_resolve_prefix(&resolver, 5).unwrap(), "🌙\n");
        }
        let skipped = text.with_skipped_prefix(4).unwrap();
        assert_eq!(skipped.try_resolve_prefix(&resolver, 2).unwrap(), "\nx");
        std::fs::remove_file(&path).unwrap();
        assert!(skipped.try_resolve_prefix(&resolver, 0).is_err());
    }

    #[test]
    fn backed_text_retains_arena_and_checked_decoded_demands() {
        let world = crate::test_world("source-text-demands");
        let path = world.base.join("original");
        std::fs::write(&path, "original compressed provenance").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let backing = Arc::new(ByteSource::arena().unwrap());
        let body = format!("[error] {}", "🌙\n\"\\".repeat(8192));
        let wire = serde_json::to_vec(&body).unwrap();
        let (offset, len) = backing
            .append(|out| {
                out.write_all(&wire)?;
                Ok(())
            })
            .unwrap();
        let text = source
            .read(
                ReadInput::Backing(backing.clone(), offset, len),
                Validation::Value,
                |view| view.source_text(),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(text, crate::lazy::Text::Source(_)));
        drop(source);
        drop(backing);
        let resolver = Resolver::new(None);
        assert_eq!(text.try_resolve_prefix(&resolver, 11).unwrap(), "[error] ");
        assert_eq!(text.try_decoded_len(&resolver).unwrap(), body.len());
        let suffix = text.clone().with_skipped_prefix(8).unwrap();
        assert_eq!(suffix.try_resolve(&resolver).unwrap(), &body[8..]);
        let mut actual = String::new();
        suffix
            .try_for_each_chunk(&resolver, 7, |chunk| {
                assert!(chunk.len() <= 10);
                actual.push_str(chunk);
                Ok(())
            })
            .unwrap();
        assert_eq!(actual, &body[8..]);
        assert_eq!(
            serde_json::to_string(&suffix).unwrap(),
            serde_json::to_string(&body[8..]).unwrap()
        );
        let error = suffix
            .try_for_each_chunk(&resolver, 7, |_| {
                std::fs::write(&path, "mutated").unwrap();
                anyhow::bail!("callback failed")
            })
            .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error:#}");
        assert!(suffix.try_decoded_len(&resolver).is_err());
        assert!(suffix.try_resolve_prefix(&resolver, 0).is_err());
        let mut message = crate::ir::Message::new(crate::ir::Role::Assistant);
        message.content.push(crate::ir::Block::Text { text: suffix });
        assert!(message.materialize(&resolver).is_err());
        message.materialize_text(&resolver);
        assert!(matches!(
            &message.content[0],
            crate::ir::Block::Text {
                text: crate::lazy::Text::Source(_)
            }
        ));
    }
}
