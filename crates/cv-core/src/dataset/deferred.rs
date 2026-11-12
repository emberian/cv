//! Private dataset projection transport: source-backed arguments become opaque descriptors, and
//! all text spans remain spans. No unresolved placeholder can leave this module as a Message.
use crate::ir::{Block, Message};
use crate::json::{Json, SourceDescriptor, SourceRegistry};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use crate::spill::Record;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

#[derive(Serialize, Deserialize)]
enum SourceSlot {
    Input(usize),
    Text { index: usize, skip: usize },
}
#[derive(Serialize, Deserialize)]
pub(super) struct DeferredMessage {
    message: Message,
    sources: Vec<(SourceSlot, SourceDescriptor)>,
}
impl DeferredMessage {
    pub(super) fn stage(mut message: Message, registry: &mut SourceRegistry) -> Self {
        // The query has already inspected the original message. Keep `id`/`parent_id` and tool
        // call linkage (`id`, `tool_use_id`, `tool_name`) for `Format::OpenAiTools` and active-branch
        // trajectory selection, while dropping unconsumed rich sidecars/usage from the private spool.
        let is_sidechain = message
            .extra
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || message
                .extra
                .get("cv")
                .and_then(|v| v.get("inactive_branch"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        message.timestamp = None;
        message.model = None;
        message.usage = None;
        message.extra = serde_json::Map::new();
        if is_sidechain {
            message
                .extra
                .insert("isSidechain".into(), serde_json::Value::Bool(true));
        }
        message.complete_record = None;
        let mut sources = Vec::new();
        for (index, block) in message.content.iter_mut().enumerate() {
            let text = match &mut *block {
                Block::Text { text } | Block::Thinking { text, .. } => Some(text),
                Block::ToolResult { content, .. } => Some(content),
                _ => None,
            };
            if let Some(text) = text {
                if let crate::lazy::Text::Source(source) = text {
                    let skip = source.skip;
                    let mut json = source.json();
                    sources.push((
                        SourceSlot::Text { index, skip },
                        registry
                            .detach(&mut json)
                            .expect("source text yields a source JSON descriptor"),
                    ));
                    *text = crate::lazy::Text::Inline(String::new());
                }
            }
            match block {
                Block::Thinking {
                    signature, encrypted, ..
                } => {
                    *signature = None;
                    *encrypted = None;
                }
                Block::ToolUse { input, namespace, .. } => {
                    *namespace = None;
                    if let Some(source) = registry.detach(input) {
                        sources.push((SourceSlot::Input(index), source));
                    }
                }
                Block::ToolResult { status, details, .. } => {
                    *status = None;
                    *details = None;
                }
                Block::Image { data_ref, .. } => *data_ref = None,
                Block::File { source, .. } => *source = None,
                Block::Text { .. } => {}
            }
        }
        Self { message, sources }
    }

    pub(super) fn restore(mut self, registry: &SourceRegistry) -> Result<Message> {
        for (slot, descriptor) in self.sources {
            let index = match slot {
                SourceSlot::Input(index) | SourceSlot::Text { index, .. } => index,
            };
            let block = self
                .message
                .content
                .get_mut(index)
                .context("invalid staged source block index")?;
            let restored = registry.restore(descriptor)?;
            match slot {
                SourceSlot::Input(_) => {
                    let Block::ToolUse { input, .. } = block else {
                        anyhow::bail!("invalid staged source block type")
                    };
                    anyhow::ensure!(
                        matches!(input, Json::Owned(serde_json::Value::Null)),
                        "invalid staged source placeholder"
                    );
                    *input = restored;
                }
                SourceSlot::Text { skip, .. } => {
                    let text = match block {
                        Block::Text { text } | Block::Thinking { text, .. } => text,
                        Block::ToolResult { content, .. } => content,
                        _ => anyhow::bail!("invalid staged text block type"),
                    };
                    anyhow::ensure!(
                        matches!(text, crate::lazy::Text::Inline(value) if value.is_empty()),
                        "invalid staged text placeholder"
                    );
                    let Json::Source(range) = restored else {
                        anyhow::bail!("missing staged text source")
                    };
                    let mut source = crate::lazy::SourceText::new(range);
                    source.skip = skip;
                    *text = crate::lazy::Text::Source(source);
                }
            }
        }
        Ok(self.message)
    }
}

fn text_heap(text: &crate::lazy::Text) -> usize {
    match text {
        crate::lazy::Text::Inline(text) => text.capacity(),
        crate::lazy::Text::Span(span) => span.source.as_ref().map_or(0, std::path::PathBuf::capacity),
        crate::lazy::Text::Source(_) => unreachable!("source text detached before staging"),
    }
}
fn value_heap(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.capacity(),
        serde_json::Value::Array(values) => {
            values.capacity() * std::mem::size_of::<serde_json::Value>() + values.iter().map(value_heap).sum::<usize>()
        }
        // Conservative node/table accounting covers both BTreeMap and preserve_order layouts.
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| 1024 + key.capacity() + value_heap(value))
            .sum(),
        _ => 0,
    }
}
impl Record for DeferredMessage {
    type ReadState = Vec<u8>;
    fn heap_bytes(&self) -> usize {
        self.sources.capacity() * std::mem::size_of::<(SourceSlot, SourceDescriptor)>()
            + self.message.id.as_ref().map_or(0, String::capacity)
            + self.message.parent_id.as_ref().map_or(0, String::capacity)
            + self.message.content.capacity() * std::mem::size_of::<Block>()
            + self
                .message
                .content
                .iter()
                .map(|block| match block {
                    Block::Text { text } | Block::Thinking { text, .. } => text_heap(text),
                    Block::ToolUse { id, name, input, .. } => {
                        id.capacity()
                            + name.capacity()
                            + match input {
                                Json::Owned(value) => value_heap(value),
                                Json::Source(_) => unreachable!("source values are detached before staging"),
                            }
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        tool_name,
                        ..
                    } => tool_use_id.capacity() + tool_name.as_ref().map_or(0, String::capacity) + text_heap(content),
                    Block::Image { media_type, .. } => media_type.as_ref().map_or(0, String::capacity),
                    Block::File { path, mime, .. } => {
                        path.as_ref().map_or(0, String::capacity) + mime.as_ref().map_or(0, String::capacity)
                    }
                })
                .sum::<usize>()
    }
    fn write(&self, writer: &mut impl Write) -> Result<()> {
        serde_json::to_writer(&mut *writer, self).context("writing deferred dataset message")?;
        writer.write_all(b"\n")?;
        Ok(())
    }
    fn read(reader: &mut impl BufRead, prefix: &mut Vec<u8>) -> Result<Self> {
        // Most projections fit in the spill reader's buffer. Slice deserialization avoids
        // serde's byte-at-a-time Read adapter without retaining a second encoded record.
        // Split rows use a bounded reusable prefix; oversized inline fields keep streaming.
        const PREFIX_LIMIT: usize = 64 * 1024;
        prefix.clear();
        loop {
            let bytes = reader.fill_buf()?;
            anyhow::ensure!(!bytes.is_empty(), "missing deferred dataset record");
            if let Some(end) = memchr::memchr(b'\n', bytes) {
                if prefix.is_empty() {
                    let row = serde_json::from_slice(&bytes[..end]).context("reading deferred dataset message")?;
                    reader.consume(end + 1);
                    return Ok(row);
                }
                if prefix.len() + end <= PREFIX_LIMIT {
                    prefix.extend_from_slice(&bytes[..end]);
                    reader.consume(end + 1);
                    return serde_json::from_slice(prefix).context("reading deferred dataset message");
                }
            }
            if prefix.capacity() < PREFIX_LIMIT {
                prefix.reserve_exact(PREFIX_LIMIT - prefix.len());
            }
            let take = bytes.len().min(PREFIX_LIMIT - prefix.len());
            prefix.extend_from_slice(&bytes[..take]);
            reader.consume(take);
            if prefix.len() == PREFIX_LIMIT {
                // Only this bounded prefix overlaps the decoded message, even for giant
                // owned fields. Chain reads exactly the bytes consumed above back into serde.
                let mut chained = std::io::Cursor::new(prefix.as_slice()).chain(&mut *reader);
                let row = Self::deserialize(&mut serde_json::Deserializer::from_reader(&mut chained))
                    .context("reading deferred dataset message")?;
                let mut delimiter = [0];
                chained.read_exact(&mut delimiter)?;
                anyhow::ensure!(&delimiter == b"\n", "invalid deferred dataset delimiter");
                return Ok(row);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Format, RecordWriter};
    use crate::ir::Role;
    #[test]
    fn backed_text_spool_keeps_one_owner_and_exact_rendering() {
        use crate::harness::store::bytes::ByteSource;
        use crate::json::JsonSource;
        use std::sync::Arc;
        let world = crate::test_world("deferred-source-text");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot provenance").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let backing = Arc::new(ByteSource::arena().unwrap());
        let body = format!("[error] {}", "🌙\n\"".repeat(8192));
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
            .unwrap()
            .with_skipped_prefix(8)
            .unwrap();
        let mut original = Message::new(Role::Assistant);
        original.content.push(Block::Text { text });
        let mut registry = SourceRegistry::default();
        let mut ordered = crate::spill::Ordered::default();
        for _ in 0..1000 {
            ordered
                .push(DeferredMessage::stage(original.clone(), &mut registry))
                .unwrap();
        }
        assert_eq!(registry.owner_counts(), (1, 1));
        assert!(ordered.spilled_bytes().unwrap() < body.len() as u64 * 10);
        drop(original);
        drop(source);
        drop(backing);
        let resolver = crate::lazy::Resolver::new(None);
        let mut owned = Message::new(Role::Assistant);
        owned.content.push(Block::Text { text: body[8..].into() });
        let mut expected = Vec::new();
        let mut writer = RecordWriter::new(&mut expected, Format::Chatml, None);
        writer.message(&owned, &resolver).unwrap();
        writer.finish().unwrap();
        let mut last = None;
        for (index, row) in ordered.rows().unwrap().enumerate() {
            let restored = row.unwrap().restore(&registry).unwrap();
            assert!(matches!(
                &restored.content[0],
                Block::Text {
                    text: crate::lazy::Text::Source(_)
                }
            ));
            if index != 0 && index != 999 {
                continue;
            }
            let mut actual = Vec::new();
            let mut writer = RecordWriter::new(&mut actual, Format::Chatml, None);
            writer.message(&restored, &resolver).unwrap();
            writer.finish().unwrap();
            assert_eq!(actual, expected);
            let mut source_json = Vec::new();
            let mut owned_json = Vec::new();
            restored.write_json(&mut source_json, &resolver).unwrap();
            owned.write_json(&mut owned_json, &resolver).unwrap();
            assert_eq!(source_json, owned_json);
            source_json.clear();
            owned_json.clear();
            restored.write_json_pretty(&mut source_json, &resolver).unwrap();
            owned.write_json_pretty(&mut owned_json, &resolver).unwrap();
            assert_eq!(source_json, owned_json);
            last = Some(restored);
        }
        std::fs::write(&path, "changed snapshot").unwrap();
        let restored = last.unwrap();
        assert!(restored.write_json(&mut Vec::new(), &resolver).is_err());
    }

    #[test]
    fn spilled_sources_restore_exact_training_bytes_without_serializing_payloads() {
        let world = crate::test_world("deferred-dataset-source-spill");
        let path = world.base.join("source.json");
        let body = serde_json::json!({"rich":"escaped\n\"🌙\"".repeat(8192)}).to_string();
        std::fs::write(&path, &body).unwrap();
        let mut original = Message::new(Role::Assistant);
        original.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Tool".into(),
            namespace: None,
            input: Json::source(&path, 0, body.len() as u64).unwrap(),
        });
        let mut registry = SourceRegistry::default();
        let mut ordered = crate::spill::Ordered::default();
        for _ in 0..1000 {
            ordered
                .push(DeferredMessage::stage(original.clone(), &mut registry))
                .unwrap();
        }
        let temporary_bytes = ordered.spilled_bytes().unwrap();
        assert!(temporary_bytes > 0);
        assert!(
            temporary_bytes < body.len() as u64 * 10,
            "payloads were not duplicated into the spool"
        );
        let resolver = crate::lazy::Resolver::new(None);
        let mut expected = Vec::new();
        let mut record = RecordWriter::new(&mut expected, Format::Chatml, None);
        record.message(&original, &resolver).unwrap();
        record.finish().unwrap();
        let mut restored_count = 0;
        for row in ordered.rows().unwrap() {
            let restored = row.unwrap().restore(&registry).unwrap();
            restored_count += 1;
            if !matches!(restored_count, 1 | 1000) {
                continue;
            }
            let mut actual = Vec::new();
            let mut record = RecordWriter::new(&mut actual, Format::Chatml, None);
            record.message(&restored, &resolver).unwrap();
            record.finish().unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(restored_count, 1000);
    }

    #[test]
    fn split_and_oversized_rows_preserve_exact_fields_and_record_boundaries() {
        for capacity in [31, 65536] {
            let mut bytes = Vec::new();
            for size in [3, 65530, 100000, 7] {
                let mut message = Message::new(Role::User);
                message.content.push(Block::Text {
                    text: "a\n\"🌙".repeat(size).into(),
                });
                DeferredMessage::stage(message, &mut SourceRegistry::default())
                    .write(&mut bytes)
                    .unwrap();
            }
            let mut reader = std::io::BufReader::with_capacity(capacity, bytes.as_slice());
            let mut state = Vec::new();
            for size in [3, 65530, 100000, 7] {
                let message = DeferredMessage::read(&mut reader, &mut state)
                    .unwrap()
                    .restore(&SourceRegistry::default())
                    .unwrap();
                let Block::Text { text } = &message.content[0] else {
                    panic!("text")
                };
                assert_eq!(text.inline_str(), Some("a\n\"🌙".repeat(size).as_str()));
                assert!(state.capacity() <= 65536);
            }
            assert!(reader.fill_buf().unwrap().is_empty());
        }
    }

    #[test]
    fn corrupted_descriptors_cannot_escape_as_unresolved_messages() {
        let world = crate::test_world("deferred-dataset-descriptor-corruption");
        let path = world.base.join("source.json");
        std::fs::write(&path, "null").unwrap();
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Tool".into(),
            namespace: None,
            input: Json::source(&path, 0, 4).unwrap(),
        });
        let mut registry = SourceRegistry::default();
        let staged = DeferredMessage::stage(message, &mut registry);
        let encoded = serde_json::to_value(&staged).unwrap();
        for failure in ["index", "type", "placeholder", "source", "duplicate"] {
            let mut corrupted = encoded.clone();
            match failure {
                "index" => corrupted["sources"][0][0]["Input"] = 999.into(),
                "type" => corrupted["message"]["content"][0] = serde_json::json!({"type":"text","text":"x"}),
                "placeholder" => corrupted["message"]["content"][0]["input"] = true.into(),
                "source" => corrupted["sources"][0][1]["source"] = 999.into(),
                "duplicate" => {
                    let duplicate = corrupted["sources"][0].clone();
                    corrupted["sources"].as_array_mut().unwrap().push(duplicate);
                }
                _ => unreachable!(),
            }
            let staged: DeferredMessage = serde_json::from_value(corrupted).unwrap();
            assert!(staged.restore(&registry).is_err(), "{failure}");
        }
    }
}
