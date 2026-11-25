//! Checked per-block projection: retained text stays borrowed, discarded text is never copied.
use super::{thinking_fate_with_presence, thinking_placeholder, ThinkingFate, ThinkingMode};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use crate::{lazy::Resolver, Block, Harness, Text};
use anyhow::Result;
use std::borrow::Cow;

/// Source strings and wire-form spans both require strict JSON string acceptance.
/// Checked callbacks revalidate provenance even at an early stop.
pub(crate) fn first_character(text: &Text, resolver: &Resolver, skip_whitespace: bool) -> Result<Option<char>> {
    let select = |chunk: &str| chunk.chars().find(|ch| !skip_whitespace || !ch.is_whitespace());
    if let Some(inline) = text.inline_str() {
        return Ok(select(inline));
    }
    let mut first = None;
    match text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
        first = select(chunk);
        if first.is_some() {
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    }) {
        Ok(()) => Ok(None),
        // Only our early-stop sentinel means success; source validation errors still escape.
        Err(error) if error.is::<crate::stream::Stop>() => Ok(first),
        Err(error) => Err(error),
    }
}
pub(crate) fn fate(block: &Block, target: Harness, mode: ThinkingMode, resolver: &Resolver) -> Result<ThinkingFate> {
    if matches!(block, Block::Opaque { .. }) {
        return Ok(ThinkingFate::Drop);
    }
    let Block::Thinking {
        text,
        signature,
        encrypted,
        redacted,
        ..
    } = block
    else {
        return Ok(ThinkingFate::Keep);
    };
    // Native retains the existing emitter codec policy, including its empty-block handling.
    if mode == ThinkingMode::Native {
        return Ok(ThinkingFate::Keep);
    }
    if mode == ThinkingMode::Drop {
        return Ok(ThinkingFate::Drop);
    }
    let decide = |present| {
        thinking_fate_with_presence(
            target,
            mode,
            present,
            signature.as_deref(),
            encrypted.as_deref(),
            *redacted,
        )
    };
    // If absent plaintext already means Keep, presence cannot change the decision; avoid
    // resolving a retained source field just to rediscover that it will be kept.
    if decide(false) == ThinkingFate::Keep {
        return Ok(ThinkingFate::Keep);
    }
    Ok(decide(first_character(text, resolver, true)?.is_some()))
}
pub(crate) fn text<'a>(
    block: &'a Block,
    target: Harness,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<Option<Cow<'a, Text>>> {
    if let Block::Text { text, .. } = block {
        return Ok(Some(Cow::Borrowed(text)));
    }
    Ok(text_with_fate(block, fate(block, target, mode, resolver)?))
}
pub(crate) fn text_with_fate(block: &Block, fate: ThinkingFate) -> Option<Cow<'_, Text>> {
    match fate {
        ThinkingFate::AsText => {
            let Block::Thinking { text, .. } = block else {
                unreachable!()
            };
            Some(Cow::Borrowed(text))
        }
        ThinkingFate::Placeholder => {
            let Block::Thinking {
                signature,
                encrypted,
                redacted,
                ..
            } = block
            else {
                unreachable!()
            };
            Some(Cow::Owned(
                thinking_placeholder(signature.as_deref(), encrypted.as_deref(), *redacted).into(),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::JsonSource;

    #[test]
    fn checked_first_character_matches_inline_and_preserves_strict_source_errors() -> Result<()> {
        let world = crate::test_world("thinking-first-character");
        let path = world.base.join("text.json");
        for body in [
            String::new(),
            "\u{2003}".repeat(40_000),
            format!("{}?🌙", " ".repeat(70_000)),
            format!("🌙{}", "x".repeat(70_000)),
        ] {
            std::fs::write(&path, serde_json::to_vec(&body)?)?;
            let source = JsonSource::capture(&path)?;
            let text = source
                .read(ReadInput::Range(0, source.len()), Validation::Value, |v| {
                    v.source_text()
                })?
                .unwrap();
            let resolver = Resolver::new(None);
            for skip_whitespace in [false, true] {
                assert_eq!(
                    first_character(&text, &resolver, skip_whitespace)?,
                    first_character(&body.clone().into(), &resolver, skip_whitespace)?
                );
            }
            std::fs::write(&path, b"changed")?;
            if text.is_lazy() {
                assert!(first_character(&text, &resolver, false).is_err());
                assert!(first_character(&text, &resolver, true).is_err());
            }
        }
        // Finding a valid first character never authorizes an invalid later string escape.
        let bytes = format!("\"x{}\\ud800\"", "a".repeat(70_000));
        std::fs::write(&path, &bytes)?;
        let source = JsonSource::capture(&path)?;
        assert!(source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |v| v
                .source_text())
            .is_err());
        // A wire-form span over the same malformed string is refused like source text.
        let resolver = Resolver::new(Some(path.clone()));
        let span = Text::Span(crate::lazy::Span {
            source: None,
            offset: 1,
            len: bytes.len() as u64 - 2,
            escaped: true,
        });
        assert!(span.try_resolve(&resolver).is_err());
        let invalid_bound = Text::Span(crate::lazy::Span {
            source: None,
            offset: 1,
            len: bytes.len() as u64,
            escaped: true,
        });
        assert!(first_character(&invalid_bound, &resolver, false).is_err());
        Ok(())
    }
}
