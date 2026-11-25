//! Checked wire fields and borrowed ordered-object overlays, not another IR representation.
use crate::{
    harness::store::text,
    lazy::{Resolver, Text},
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value};
use std::{collections::HashMap, io::Write};

pub(crate) struct ObjectWriter<'a> {
    writer: &'a mut dyn Write,
    first: bool,
}
impl<'a> ObjectWriter<'a> {
    pub(crate) fn new(writer: &'a mut dyn Write) -> Result<Self> {
        writer.write_all(b"{")?;
        Ok(Self { writer, first: true })
    }
    pub(crate) fn field(&mut self, key: &str, value: impl Serialize) -> Result<()> {
        self.field_with(key, |writer| serde_json::to_writer(writer, &value).map_err(Into::into))
    }
    pub(crate) fn field_with(&mut self, key: &str, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        if !self.first {
            self.writer.write_all(b",")?;
        }
        self.first = false;
        serde_json::to_writer(&mut *self.writer, key)?;
        self.writer.write_all(b":")?;
        write(self.writer)
    }
    pub(crate) fn finish(self) -> Result<()> {
        self.writer.write_all(b"}")?;
        Ok(())
    }
}
/// Preserve Value's first insertion position and last value, borrowing large replay fields.
pub(crate) struct Object<'a> {
    fields: Vec<(&'a str, &'a Value)>,
    index: Option<HashMap<&'a str, usize>>,
}
impl<'a> Object<'a> {
    pub(crate) fn shortened<'b>(self) -> Object<'b>
    where
        'a: 'b,
    {
        Object {
            fields: self.fields,
            index: self.index,
        }
    }
    pub(crate) fn new(map: &'a Map<String, Value>) -> Self {
        let mut object = Self {
            fields: map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            index: None,
        };
        object.index_large();
        object
    }
    // Tiny envelopes avoid a hash allocation; rich replay maps get linear-total construction.
    fn index_large(&mut self) {
        if self.fields.len() >= 32 && self.index.is_none() {
            self.index = Some(self.fields.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect());
        }
    }
    fn position(&self, key: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.get(key).copied(),
            None => self.fields.iter().position(|(k, _)| *k == key),
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&'a Value> {
        self.position(key).map(|i| self.fields[i].1)
    }
    pub(crate) fn set(&mut self, key: &'a str, value: &'a Value) {
        if let Some(i) = self.position(key) {
            self.fields[i].1 = value;
        } else {
            let i = self.fields.len();
            self.fields.push((key, value));
            if let Some(index) = &mut self.index {
                index.insert(key, i);
            } else {
                self.index_large();
            }
        }
    }
    pub(crate) fn remove(&mut self, key: &str) {
        if let Some(i) = self.position(key) {
            self.fields.remove(i);
            // Removal shifts every following position; rebuild the small overlay index so
            // subsequent replacements still address the correct field.
            if self.index.is_some() {
                self.index = Some(self.fields.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect());
            }
        }
    }
    pub(crate) fn write(
        &self,
        writer: &mut dyn Write,
        mut field: impl FnMut(&str, &Value, &mut dyn Write) -> Result<bool>,
    ) -> Result<()> {
        let mut object = ObjectWriter::new(writer)?;
        for (k, v) in &self.fields {
            object.field_with(k, |writer| {
                if !field(k, v, writer)? {
                    serde_json::to_writer(writer, v)?;
                }
                Ok(())
            })?;
        }
        object.finish()
    }
}
/// Append checked text to a generated string; return whether decoded text was present.
pub(crate) fn append_text(builder: &mut text::Builder<'_>, value: &Text, resolver: &Resolver) -> Result<bool> {
    let before = builder.decoded_len();
    if let Text::Source(source) = value {
        if source.skip == 0 {
            source.json().with_view(|view| {
                anyhow::ensure!(builder.push_string(view)?, "expected source text string");
                Ok(())
            })?;
            return Ok(builder.decoded_len() != before);
        }
    }
    value.try_for_each_chunk(resolver, 64 * 1024, |chunk| builder.push(chunk))?;
    Ok(builder.decoded_len() != before)
}
pub(crate) fn text(writer: &mut dyn Write, value: &Text, resolver: &Resolver) -> Result<()> {
    if let Some(inline) = value.inline_str() {
        return serde_json::to_writer(writer, inline).map_err(Into::into);
    }
    if let Text::Source(source) = value {
        if source.skip == 0 {
            // One complete string needs no concatenation/counting pass. Canonical publication
            // still checks strict string acceptance and the retained source before/after demand.
            return source.json().with_view(|view| {
                anyhow::ensure!(view.is_string()?, "expected source text string");
                view.write_canonical(&mut &mut *writer)
            });
        }
    }
    text::generate_string(writer, |builder| {
        append_text(builder, value, resolver)?;
        Ok(())
    })
}
/// Exactly Message::text: empty leading parts do not introduce a newline.
pub(crate) fn joined_text<'a>(
    writer: &mut dyn Write,
    values: impl IntoIterator<Item = &'a Text>,
    resolver: &Resolver,
) -> Result<()> {
    text::generate_string(writer, |builder| {
        let mut present = false;
        for value in values {
            if present {
                builder.push("\n")?;
            }
            present |= append_text(builder, value, resolver)?;
        }
        Ok(())
    })
}

/// Preserve direct-emitter source failures even when a target/mode omits a lazy field.
/// Source leaves were strictly validated at capture; recheck provenance and retained backing
/// bounds without decoding their payloads or constructing fidelity features.
pub(crate) fn check_source_fields(session: &crate::ir::Session) -> Result<()> {
    for message in &session.messages {
        if let Some(record) = &message.complete_record {
            if let crate::Json::Source(source) = &record.value {
                source.check_retained_bounds()?;
            }
        }
        for block in &message.content {
            match block {
                crate::ir::Block::Text { text, .. }
                | crate::ir::Block::Thinking { text, .. }
                | crate::ir::Block::ToolResult { content: text, .. } => {
                    if let Text::Source(source) = text {
                        source.range.check_retained_bounds()?;
                    }
                }
                crate::ir::Block::ToolUse {
                    input: crate::Json::Source(source),
                    ..
                } => source.check_retained_bounds()?,
                _ => {}
            }
        }
    }
    Ok(())
}

/// Native fallback labels use the same meaningful-prompt policy as cv's listings.
pub(crate) fn fallback_title(session: &crate::ir::Session, resolver: &Resolver) -> Result<Option<String>> {
    Ok(session
        .title_with(resolver)?
        .map(|title| crate::ir::truncate(&title, crate::projection::Head::TITLE)))
}

#[cfg(test)]
mod title_tests {
    use super::*;
    #[test]
    fn fallback_title_skips_injected_users_and_harness_wrappers() -> Result<()> {
        use crate::ir::{Block, Message, MessageKind, Origin, Role, Session};
        let mut session = Session::new("synthetic", crate::Harness::Continue);
        let mut injected = Message::of_kind(Role::User, MessageKind::InjectedContext, Origin::Harness);
        injected.content.push(Block::from_text("harness bookkeeping".into()));
        let mut noise = Message::new(Role::User);
        noise
            .content
            .push(Block::from_text("<command-name>/usage</command-name>".into()));
        let mut prompt = Message::new(Role::User);
        prompt.content.push(Block::from_text(
            "<system-reminder>metadata</system-reminder> please fix the parser".into(),
        ));
        session.messages = vec![injected, noise, prompt];
        let resolver = Resolver::new(None);
        assert_eq!(
            fallback_title(&session, &resolver)?.as_deref(),
            Some("please fix the parser")
        );
        session.title = Some("λ".repeat(100));
        assert_eq!(
            fallback_title(&session, &resolver)?,
            Some(crate::ir::truncate(&"λ".repeat(100), 80))
        );
        Ok(())
    }
}
