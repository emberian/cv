//! Checked complete records retain the public `extra._record` contract.
use super::{Harness, Message};
use crate::{harness::wire::write::ObjectWriter, Json};
use anyhow::Result;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::io::Write;

#[derive(Debug, Clone)]
pub struct CompleteRecord {
    #[cfg_attr(not(any(feature = "sqlite", test)), allow(dead_code))]
    pub(crate) harness: Harness,
    pub(crate) value: Json,
    pub(crate) identity: Map<String, Value>,
    // Ordinary source descriptors clone cheaply. Uncommon owned metadata keeps its owned cost.
    pub(crate) projection: Vec<Message>,
}
impl CompleteRecord {
    /// Retain a complete record solely for its public serialization contract.
    pub(crate) fn attach(messages: &mut [Message], harness: Harness, value: Json, identity: Map<String, Value>) {
        Self::attach_inner(messages, harness, value, identity, false);
    }
    /// Retain cheap checked projection descriptors for unchanged native replay.
    pub(crate) fn attach_replay(messages: &mut [Message], harness: Harness, value: Json, identity: Map<String, Value>) {
        Self::attach_inner(messages, harness, value, identity, true);
    }
    fn attach_inner(
        messages: &mut [Message],
        harness: Harness,
        value: Json,
        identity: Map<String, Value>,
        replay: bool,
    ) {
        if messages.is_empty() {
            return;
        }
        let projection = if replay && matches!(value, Json::Source(_)) {
            messages
                .iter()
                .map(|message| Message {
                    extensions: message.extensions.clone(),
                    id: message.id.clone(),
                    parent_id: message.parent_id.clone(),
                    role: message.role,
                    kind: message.kind,
                    origin: message.origin,
                    timestamp: message.timestamp,
                    model: message.model.clone(),
                    content: message.content.clone(),
                    usage: message.usage.clone(),
                    extra: message
                        .extra
                        .iter()
                        .filter(|(key, _)| key.as_str() != crate::ir::CARRIER_KEY)
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                    complete_record: None,
                })
                .collect()
        } else {
            Vec::new()
        };
        messages[0].extra.remove(crate::ir::CARRIER_KEY);
        messages[0].complete_record = Some(std::sync::Arc::new(Self {
            harness,
            value,
            identity,
            projection,
        }));
    }
    /// Full reads own raw payloads; keep only the captured envelopes needed to reject
    /// edited metadata when a native codec reconstructs that record's blocks later.
    pub(crate) fn materialize(&mut self) -> Result<()> {
        self.value.make_owned()?;
        for message in &mut self.projection {
            message.content.clear();
        }
        Ok(())
    }
    pub(crate) fn unchanged(&self, harness: Harness, messages: &[Message]) -> bool {
        self.harness == harness
            && matches!(self.value, Json::Source(_))
            && !self.projection.is_empty()
            && messages.len() >= self.projection.len()
            && self
                .projection
                .iter()
                .zip(messages)
                .all(|(original, current)| same_message(original, current))
    }
    #[cfg(any(feature = "sqlite", test))]
    pub(crate) fn text_overlay(&self, harness: Harness, messages: &[Message]) -> Result<bool> {
        if self.harness != harness || self.projection.is_empty() || messages.len() < self.projection.len() {
            return Ok(false);
        }
        let ty = self.identity.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(ty, "user")
            || !same_except_first_text(&self.projection[0], &messages[0])
            || !self
                .projection
                .iter()
                .skip(1)
                .zip(messages.iter().skip(1))
                .all(|(a, b)| same_message(a, b))
        {
            return Ok(false);
        }
        self.value.with_view(|view| match view.get("text")? {
            Some(value) if value.is_string()? => match value.source_range()? {
                Some((_, len)) => Ok(len > 2),
                None => Ok(value.resolve()?.as_str().is_some_and(|text| !text.is_empty())),
            },
            _ => Ok(false),
        })
    }
    /// The one serialization of a retained record: its native entries in source order (the
    /// identity keys left out), then the identity the reader attached. serde delegates here.
    pub(crate) fn write(&self, writer: &mut dyn Write) -> Result<()> {
        self.value.with_view(|view| {
            let mut out = ObjectWriter::new(writer)?;
            view.for_each_entry(|key, value| {
                if !self.identity.contains_key(key) {
                    out.field_with(key, |mut w| value.write_canonical(&mut w))?;
                }
                Ok(())
            })?;
            for (key, value) in &self.identity {
                out.field(key, value)?;
            }
            out.finish()
        })
    }
}
/// serde for owned consumers (`to_value`, pretty printers): [`CompleteRecord::write`]'s bytes,
/// read back as a value, so the two can never disagree.
fn via_writer<S: Serializer>(
    serializer: S,
    write: impl FnOnce(&mut Vec<u8>) -> Result<()>,
) -> std::result::Result<S::Ok, S::Error> {
    let mut bytes = Vec::new();
    write(&mut bytes).map_err(serde::ser::Error::custom)?;
    serde_json::from_slice::<Value>(&bytes)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}
impl Serialize for CompleteRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        via_writer(serializer, |w| self.write(w))
    }
}
/// `Message::extra` with the retained record under the carrier key: the owned map when there is
/// no record to carry, else [`write_extra`]'s bytes.
pub(crate) struct Extra<'a>(pub &'a Message);
impl Extra<'_> {
    /// No owned facts and no record to carry: `extra` is omitted.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.extra.is_empty() && self.0.complete_record.is_none()
    }
}
impl Serialize for Extra<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match carried(self.0) {
            None => self.0.extra.serialize(serializer),
            Some(_) => via_writer(serializer, |w| write_extra(self.0, w)),
        }
    }
}
/// The retained record `extra` carries: an explicit owned edit under the carrier key wins.
fn carried(message: &Message) -> Option<&CompleteRecord> {
    match message.extra.contains_key(crate::ir::CARRIER_KEY) {
        true => None,
        false => message.complete_record.as_deref(),
    }
}
pub(crate) fn write_extra(message: &Message, writer: &mut dyn Write) -> Result<()> {
    let mut out = ObjectWriter::new(writer)?;
    for (key, value) in &message.extra {
        out.field(key, value)?;
    }
    if let Some(record) = carried(message) {
        out.field_with(crate::ir::CARRIER_KEY, |w| record.write(w))?;
    }
    out.finish()
}
pub(crate) fn same_envelope(a: &Message, b: &Message) -> bool {
    a.extensions == b.extensions
        && a.id == b.id
        && a.parent_id == b.parent_id
        && a.role == b.role
        && a.kind == b.kind
        && a.origin == b.origin
        && a.timestamp == b.timestamp
        && a.model == b.model
        && a.usage == b.usage
        && a.extra
            .iter()
            .filter(|(key, _)| key.as_str() != crate::ir::CARRIER_KEY)
            .eq(b.extra.iter().filter(|(key, _)| key.as_str() != crate::ir::CARRIER_KEY))
}

pub(crate) fn same_message(a: &Message, b: &Message) -> bool {
    same_envelope(a, b) && a.content == b.content
}
#[cfg(any(feature = "sqlite", test))]
pub(crate) fn same_except_first_text(a: &Message, b: &Message) -> bool {
    same_envelope(a, b)
        && a.content.len() == b.content.len()
        && matches!(
            (a.content.first(), b.content.first()),
            (Some(super::Block::Text { .. }), Some(super::Block::Text { .. }))
        )
        && a.content.iter().skip(1).eq(b.content.iter().skip(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialized_empty_replay_never_looks_like_checked_unchanged() -> Result<()> {
        let world = crate::test_world("record-empty-replay");
        let path = world.base.join("empty.json");
        std::fs::write(&path, "{}")?;
        let mut messages = vec![Message::new(super::super::Role::User)];
        CompleteRecord::attach_replay(&mut messages, Harness::OpenClaw, Json::source(&path, 0, 2)?, Map::new());
        let mut record = messages[0].complete_record.as_ref().unwrap().as_ref().clone();
        assert!(record.unchanged(Harness::OpenClaw, &messages));
        record.materialize()?;
        assert!(!record.unchanged(Harness::OpenClaw, &messages));
        assert!(!record.text_overlay(Harness::OpenClaw, &messages)?);
        assert_eq!(record.projection.len(), 1);
        assert!(record.projection[0].content.is_empty());
        Ok(())
    }

    #[test]
    fn usage_equality_is_serialized_equality() {
        let mut usages = vec![None];
        for cost in [
            None,
            Some(0.0),
            Some(-0.0),
            Some(1.25),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ] {
            let usage = super::super::Usage {
                cost_usd: cost,
                ..Default::default()
            };
            usages.push(Some(usage.clone()));
            usages.push(Some(super::super::Usage {
                input_tokens: Some(0),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                output_tokens: Some(u64::MAX),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                cache_read_tokens: Some(1),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                cache_creation_tokens: Some(2),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                reasoning_tokens: Some(3),
                ..usage
            }));
        }
        for a in &usages {
            for b in &usages {
                assert_eq!(
                    a == b,
                    serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap(),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }
}
