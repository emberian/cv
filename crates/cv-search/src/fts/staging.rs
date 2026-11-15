//! A complete validated source pass is staged before touching committed FTS documents.
use anyhow::{Context, Result};
use cv_core::staging::{Ordered, Record};
use std::io::{BufRead, Read, Write};
use tantivy::schema::{Document, Field, Value};
use tantivy::TantivyDocument;

#[derive(Default)]
pub(super) struct DocumentBatch(Ordered<StagedDocument>);
impl DocumentBatch {
    pub(super) fn push(&mut self, document: TantivyDocument) -> Result<()> {
        self.0.push(StagedDocument(document))
    }
    pub(super) fn replay(self, mut consume: impl FnMut(TantivyDocument) -> Result<()>) -> Result<()> {
        for document in self.0.rows()? {
            consume(document?.0)?;
        }
        Ok(())
    }
}
struct StagedDocument(TantivyDocument);
impl Record for StagedDocument {
    type ReadState = ();
    fn heap_bytes(&self) -> usize {
        self.0.node_data.capacity() + self.0.len() * 32
    }
    fn write(&self, writer: &mut impl Write) -> Result<()> {
        writer.write_all(&(self.0.len() as u64).to_le_bytes())?;
        for (field, value) in self.0.iter_fields_and_values() {
            writer.write_all(&field.field_id().to_le_bytes())?;
            if let Some(text) = value.as_str() {
                writer.write_all(&[0])?;
                writer.write_all(&(text.len() as u64).to_le_bytes())?;
                writer.write_all(text.as_bytes())?;
            } else if let Some(number) = value.as_i64() {
                writer.write_all(&[1])?;
                writer.write_all(&number.to_le_bytes())?;
            } else {
                anyhow::bail!("unsupported FTS staging value");
            }
        }
        Ok(())
    }
    fn read(reader: &mut impl BufRead, _: &mut ()) -> Result<Self> {
        let mut word = [0; 8];
        reader.read_exact(&mut word)?;
        let mut doc = TantivyDocument::default();
        for _ in 0..u64::from_le_bytes(word) {
            let mut field = [0; 4];
            reader.read_exact(&mut field)?;
            let field = Field::from_field_id(u32::from_le_bytes(field));
            let mut tag = [0];
            reader.read_exact(&mut tag)?;
            reader.read_exact(&mut word)?;
            match tag[0] {
                0 => {
                    let len = usize::try_from(u64::from_le_bytes(word))?;
                    let mut bytes = Vec::new();
                    (&mut *reader).take(len as u64).read_to_end(&mut bytes)?;
                    anyhow::ensure!(bytes.len() == len, "truncated staged FTS text");
                    doc.add_text(field, std::str::from_utf8(&bytes).context("invalid staged FTS text")?);
                }
                1 => doc.add_i64(field, i64::from_le_bytes(word)),
                _ => anyhow::bail!("invalid staged FTS type"),
            }
        }
        Ok(Self(doc))
    }
}

// Checkpoints may retain open chunk buffers and adapter state. Bound pending publication by
// their owned representation too, so many medium sessions cannot accumulate hundreds of MiB
// merely because they each emitted only one document. This estimates BTreeMap allocation
// conservatively; it never serializes or copies the retained strings.
pub(super) fn checkpoint_bytes(checkpoint: &cv_core::stream::Checkpoint) -> usize {
    fn value_bytes(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::String(text) => text.capacity(),
            serde_json::Value::Array(values) => {
                values.capacity() * std::mem::size_of::<serde_json::Value>()
                    + values.iter().map(value_bytes).sum::<usize>()
            }
            serde_json::Value::Object(values) => values
                .iter()
                .map(|(key, value)| 1024 + key.capacity() + value_bytes(value))
                .sum(),
            _ => 0,
        }
    }
    checkpoint
        .state
        .iter()
        .map(|(key, value)| 1024 + key.capacity() + value_bytes(value))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_documents_preserve_fields_and_propagate_consumer_failure() {
        let body = Field::from_field_id(0);
        let number = Field::from_field_id(1);
        let text = "λ staged text ".repeat(32 * 1024);
        let mut batch = DocumentBatch::default();
        for n in 0..3 {
            let mut document = TantivyDocument::default();
            document.add_text(body, &text);
            document.add_i64(number, n);
            batch.push(document).unwrap();
        }
        let mut seen = 0;
        let error = batch
            .replay(|document| {
                assert_eq!(document.get_first(body).and_then(|v| v.as_str()), Some(text.as_str()));
                assert_eq!(document.get_first(number).and_then(|v| v.as_i64()), Some(seen));
                seen += 1;
                anyhow::ensure!(seen < 2, "consumer stopped");
                Ok(())
            })
            .unwrap_err();
        assert_eq!(seen, 2);
        assert_eq!(error.to_string(), "consumer stopped");
    }

    #[test]
    fn truncated_or_invalid_spool_records_fail_instead_of_publishing_partial_documents() {
        let mut document = TantivyDocument::default();
        document.add_text(Field::from_field_id(7), "retained document");
        document.add_i64(Field::from_field_id(8), -42);
        let mut bytes = Vec::new();
        StagedDocument(document).write(&mut bytes).unwrap();
        for end in 0..bytes.len() {
            assert!(StagedDocument::read(&mut &bytes[..end], &mut ()).is_err());
        }
        bytes[12] = 2; // first field's type tag
        assert!(StagedDocument::read(&mut bytes.as_slice(), &mut ()).is_err());
    }
}
