//! Accept one source snapshot before retaining its lazy file bodies for native emission.
use super::*;
use crate::json::JsonSource;
use crate::json::{ReadInput, Validation};
use crate::source::{Probe, ReadSet};
use crate::stream::{Flow, MessageSink};
use anyhow::Context;
use std::collections::HashMap;

struct Collect {
    messages: Vec<crate::Message>,
    index: usize,
    start: usize,
    end: Option<usize>,
    primary: PathBuf,
    implicit: PathBuf,
    reads: ReadSet,
    sources: HashMap<PathBuf, JsonSource>,
    latch: crate::stream::Latch,
}
impl Collect {
    fn capture(&mut self, path: &Path) -> Result<()> {
        let path = std::path::absolute(path)?;
        self.reads.record(&path);
        if !self.sources.contains_key(&path) {
            self.sources.insert(path.clone(), JsonSource::capture(&path)?);
        }
        Ok(())
    }
    fn note(&mut self, message: crate::Message) -> Result<()> {
        crate::source::record_message_reads(&message, Some(&self.primary), &mut self.reads);
        let index = self.index;
        self.index += 1;
        if index < self.start || self.end.is_some_and(|end| index >= end) {
            return Ok(());
        }
        for block in &message.content {
            if let Some(crate::Text::Span(span)) = block.text() {
                let path = span.source.clone().unwrap_or_else(|| self.implicit.clone());
                self.capture(&path)?;
            }
        }
        self.messages.push(message);
        Ok(())
    }
    /// The primary's captured length, extended to cover every span the pass kept from it: the
    /// adapter opens the file after this capture, so a live transcript may have grown in between.
    /// The extension is a later capture the first admits as growth ([`JsonSource::admits`]).
    /// `None` when the primary is not a captured file.
    fn cover_primary_spans(&mut self) -> Result<Option<u64>> {
        let path = std::path::absolute(&self.primary)?;
        let Some(captured) = self.sources.get(&path) else {
            return Ok(None);
        };
        let implicit = std::path::absolute(&self.implicit)?;
        let mut end = 0;
        for block in self.messages.iter().flat_map(|message| &message.content) {
            let Some(crate::Text::Span(span)) = block.text() else {
                continue;
            };
            let owner = span.source.as_ref().map(std::path::absolute).transpose()?;
            if owner.as_ref().unwrap_or(&implicit) == &path {
                // The span covers a JSON string's body; the string ends at its closing quote.
                end = end.max(span.offset.saturating_add(span.len).saturating_add(1));
            }
        }
        if end <= captured.len() {
            return Ok(Some(captured.len()));
        }
        let later = JsonSource::capture(&path)?;
        anyhow::ensure!(captured.admits(&later), "source changed during port preparation");
        let len = later.len();
        self.sources.insert(path, later);
        Ok(Some(len))
    }
}
impl MessageSink for Collect {
    fn message(&mut self, message: crate::Message) -> Flow {
        if let Err(error) = self.note(message) {
            self.latch.fail(error);
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("emit preparation")
    }
    fn meta(&mut self, metadata: &Session) {
        if let Some(path) = &metadata.source_path {
            self.implicit = path.clone();
        }
    }
    fn dependency(&mut self, path: &Path) {
        self.reads.record(path);
        // Absence/directory membership are read-set guards, not retained body files.
        if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            if let Err(error) = self.capture(path) {
                self.latch.fail(error);
            }
        }
    }
}
/// Bind one retained block text: a wire-form span is replaced by the checked JSON string it
/// covers (quotes included) from the captured source, non-span text passes through.
fn bind_span(
    sources: &HashMap<PathBuf, JsonSource>,
    primary: &Path,
    session: &Session,
    value: &mut crate::Text,
) -> Result<()> {
    let crate::Text::Span(span) = &*value else {
        return Ok(());
    };
    let owned = span.source.as_ref().or(session.source_path.as_ref());
    let path = std::path::absolute(owned.map(|path| path.as_path()).unwrap_or(primary))?;
    let source = sources
        .get(&path)
        .ok_or_else(|| anyhow::anyhow!("uncaptured retained span source: {}", path.display()))?;
    source.check()?;
    // A wire-form span covers a JSON string's body: bind that string, quotes included, in the
    // capture just checked rather than a fresh stat of the live path.
    let (offset, len) = span
        .offset
        .checked_sub(1)
        .zip(span.len.checked_add(2))
        .filter(|(offset, len)| offset.checked_add(*len).is_some_and(|end| end <= source.len()))
        .with_context(|| format!("retained span is outside its source {}", path.display()))?;
    let bound = source
        .read(ReadInput::Range(offset, len), Validation::Value, |view| {
            view.source_text()
        })?
        .with_context(|| format!("retained span is not a JSON string in {}", path.display()))?;
    source.check()?;
    *value = bound;
    Ok(())
}

pub(super) fn parse(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    options: &Demand,
) -> Result<(Session, Option<Grown>)> {
    let (session, _, grown) = selected(adapter, reference, options, 0, None)?;
    Ok((session, grown))
}

pub(super) fn selected(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    options: &Demand,
    start: usize,
    end: Option<usize>,
) -> Result<(Session, usize, Option<Grown>)> {
    let primary = adapter.resume_source(reference);
    let snapshot = Probe::default().snapshot_from(reference, &primary);
    let mut reads = snapshot
        .all_reads()
        .ok_or_else(|| anyhow::anyhow!("cannot capture source inventory"))?;
    // A live append-only transcript grows while it is read: the pass describes the prefix it
    // captured, so growth is admitted and only a rewrite of that prefix refuses.
    let appendable = adapter.append_only(reference) && primary == reference.path;
    if appendable {
        reads.record_append_only(&primary);
    } else {
        reads.record(&primary);
    }
    let mut sink = Collect {
        messages: Vec::new(),
        index: 0,
        start,
        end,
        primary: primary.clone(),
        implicit: reference.path.clone(),
        reads,
        sources: HashMap::new(),
        latch: Default::default(),
    };
    if fs::metadata(&primary).is_ok_and(|metadata| metadata.is_file()) {
        sink.capture(&primary)?;
    }
    let read = crate::stream::stream_counted(adapter, reference, options, &mut sink, Default::default())?;
    let (mut session, counted) = (read.session, read.counted);
    if let Some(counted) = &counted {
        sink.reads.merge(&counted.dependencies);
    }
    let guard = snapshot.with_reads(&sink.reads);
    let now = sink
        .reads
        .fresh(&mut Probe::default())
        .then(|| crate::source::grown_from(reference, &primary, &guard))
        .flatten();
    let Some(now) = now else {
        anyhow::bail!("source changed during port preparation")
    };
    let read = sink.cover_primary_spans()?;
    for message in &mut sink.messages {
        for block in &mut message.content {
            let Some(value) = block.text_mut() else { continue };
            bind_span(&sink.sources, &primary, &session, value)?;
        }
    }

    session.messages = sink.messages;
    if let Some(counted) = counted {
        counted.record_with_source(reference, &guard);
    }
    let grown = read.filter(|read| now > *read).map(|read| Grown {
        messages: end.unwrap_or(sink.index).min(sink.index),
        read,
        now,
    });
    Ok((session, sink.index, grown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Adapter;
    use crate::{Block, Harness, Message, Role, Text};
    fn assert_serialized_equal(actual: &Session, expected: &Session) -> Result<()> {
        let a = serde_json::to_vec(actual)?;
        let b = serde_json::to_vec(expected)?;
        if a != b {
            let offset = a
                .iter()
                .zip(&b)
                .position(|(a, b)| a != b)
                .unwrap_or(a.len().min(b.len()));
            let start = offset.saturating_sub(80);
            anyhow::bail!(
                "serialized IR differs at {offset}: actual={} expected={}",
                String::from_utf8_lossy(&a[start..a.len().min(offset + 160)]),
                String::from_utf8_lossy(&b[start..b.len().min(offset + 160)])
            );
        }
        Ok(())
    }

    #[test]
    fn complete_claude_preparation_matches_owned_fields_order_and_carriers() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-complete");
        let path = world.base.join("session.jsonl");
        let body = "escaped\n\" λ".repeat(12000);
        let rows = [
            serde_json::json!({"type":"user","uuid":"u","cwd":"/synthetic/complete","timestamp":"2026-10-12T12:00:00Z","message":{"role":"user","content":body}}),
            serde_json::json!({"type":"assistant","uuid":"synthetic","future_before":null,"isSidechain":false,"userType":"external","isApiErrorMessage":true,"errorDetails":{"small":"kept"},"requestId":"request","version":"v","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":body}],"future_message":17,"id":"provider-response-id","stop_reason":null,"stop_sequence":null,"stop_details":{"small":"kept"}}}),
            serde_json::json!({"type":"user","uuid":"compact","isCompactSummary":true,"message":{"role":"user","content":body}}),
            serde_json::json!({"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"thinking","thinking":body,"signature":"signed"},{"type":"tool_use","id":"c","name":"Read","input":{"blob":body}}]}}),
            serde_json::json!({"type":"user","uuid":"r","toolUseResult":{"stdout":"kept","agentId":"agent","status":"completed"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":body}]}}),
            serde_json::json!({"type":"future-meta","sessionId":"old","message":{"content":"verbatim carrier"},"unknown":{"kept":true}}),
        ];
        let mut wire = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
        wire.push_str(&format!(r#"{{"type":"user","uuid":"ordered","future_before":1,"message":{{"role":"user","content":{}}},"future_after_one":2,"future_after_two":3}}"#, serde_json::to_string(&body)?));
        wire.push('\n');
        wire.push_str(r#"{"type":"user","uuid":"old","uuid":"last","future":1,"future":2,"message":{"role":"user","content":"duplicate keys"}}"#);
        wire.push_str("\n{\"type\":\"user\",\"uuid\":\"invalid\",\"message\":{\"content\":\"must skip\"},\"ignored\":\"\\ud800\"}\n");
        fs::write(&path, &wire)?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        {
            let extra = "complete";
            let owned_options = Demand::COMPLETE;
            let expected = crate::stream::collect_with(&adapter, &reference, &owned_options)?;
            assert!(expected
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .all(|block| block.text().is_none_or(|t| t.inline_str().is_some())));
            let checked_options = Demand {
                spans: true,
                ..owned_options
            };
            struct Facts(crate::stream::CollectSink);
            impl MessageSink for Facts {
                fn tier(&self) -> crate::stream::Tier {
                    crate::stream::Tier::Facts
                }
                fn message(&mut self, message: Message) -> Flow {
                    self.0.message(message)
                }
            }
            let mut facts = Facts(crate::stream::CollectSink::default());
            adapter.stream(&reference, &checked_options, &mut facts)?;
            let mut fact_session = Session::new("facts", Harness::Claude);
            fact_session.source_path = Some(path.clone());
            fact_session.messages = facts.0.messages;
            fact_session.materialize()?;
            let actual_bytes = serde_json::to_vec(&fact_session.messages)?;
            let expected_bytes = serde_json::to_vec(&expected.messages)?;
            anyhow::ensure!(
                actual_bytes == expected_bytes,
                "complete overrides Facts construction demand"
            );
            let (actual, _) = super::parse(&adapter, &reference, &checked_options)?;
            assert!(actual
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|block| matches!(block.text(), Some(Text::Source(_)))));
            let mut materialized = actual.clone();
            materialized.materialize()?;
            assert_serialized_equal(&materialized, &expected)?;
            let options = EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                force: true, // this test emits the same pinned id again
                ..Default::default()
            };
            let (old, old_report) = super::super::emit_report(
                &expected,
                Harness::Claude,
                &world.base.join(format!("owned-{extra}")),
                &options,
            )?;
            let (new, new_report) = super::super::emit_report(
                &actual,
                Harness::Claude,
                &world.base.join(format!("checked-{extra}")),
                &options,
            )?;
            assert_eq!(fs::read(&new.path)?, fs::read(&old.path)?, "extra={extra}");
            assert_eq!(serde_json::to_value(new_report)?, serde_json::to_value(old_report)?);
            let before = fs::read(&new.path)?;
            let prepared = actual;
            fs::write(&path, b"{}")?;
            assert!(super::super::emit_report(
                &prepared,
                Harness::Claude,
                &world.base.join(format!("checked-{extra}")),
                &options
            )
            .is_err());
            assert_eq!(fs::read(&new.path)?, before);
            fs::write(&path, &wire)?;
        }
        Ok(())
    }

    #[test]
    fn complete_codex_preparation_matches_owned_acceptance_and_late_carriers() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-complete-codex");
        let path = world.base.join("rollout.jsonl");
        let body = "escaped\n\" λ".repeat(12000);
        let mut rows = vec![
            serde_json::json!({"type":"session_meta","timestamp":"2026-10-12T12:00:00Z","payload":{"id":"session","cwd":"/synthetic/complete","future":"kept"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":body}]}}),
        ];
        // The layout detector deliberately commits to no NL events after this gap.
        for i in 0..35 {
            rows.push(serde_json::json!({"type":"world_state","payload":{"i":i,"future":"kept"}}));
        }
        rows.extend([
            serde_json::json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":body}],"encrypted_content":"opaque"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c","name":"Read","arguments":serde_json::json!({"blob":body}).to_string()}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":body}}),
            serde_json::json!({"type":"event_msg","future_top":"kept","payload":{"type":"user_message","message":body,"future_payload":"kept"}}),
        ]);
        let mut wire = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
        wire.push_str("{\"type\":\"compacted\",\"payload\":{\"message\":\"must skip\",\"replacement_history\":[{\"ignored\":\"\\ud800\"}]}}\n");
        fs::write(&path, &wire)?;
        let reference = SessionRef::new("session", Harness::Codex, &path);
        let adapter = crate::harness::codex::Codex::detect();
        let expected = crate::stream::collect_with(&adapter, &reference, &Demand::COMPLETE)?;
        let actual = super::super::parse_for_emit(&adapter, &reference, Harness::Codex)?;
        assert!(actual
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|block| matches!(block.text(), Some(Text::Source(_)))));
        let mut materialized = actual.clone();
        materialized.materialize()?;
        assert_serialized_equal(&materialized, &expected)?;
        let options = EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        };
        let (old, old_report) =
            super::super::emit_report(&expected, Harness::Codex, &world.base.join("owned"), &options)?;
        let (new, new_report) =
            super::super::emit_report(&actual, Harness::Codex, &world.base.join("checked"), &options)?;
        assert_eq!(fs::read(&new.path)?, fs::read(&old.path)?);
        assert_eq!(serde_json::to_value(new_report)?, serde_json::to_value(old_report)?);
        let before = fs::read(&new.path)?;
        fs::write(&path, b"{}")?;
        assert!(super::super::emit_report(&actual, Harness::Codex, &world.base.join("checked"), &options).is_err());
        assert_eq!(fs::read(&new.path)?, before);
        Ok(())
    }

    #[test]
    fn complete_gemini_preparation_matches_owned_recordings_replay_and_source_errors() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-complete-gemini");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats)?;
        let body = "escaped\n\" λ🌙".repeat(6000);
        let meta = serde_json::json!({"sessionId":"source","projectHash":"project","startTime":"2026-10-12T12:00:00Z","lastUpdated":"2026-10-12T12:00:00Z","summary":"complete title","directories":["/synthetic/complete-gemini"],"kind":"subagent","future":{"ignored":true}});
        let rows = vec![
            serde_json::json!({"type":"user","id":"u","timestamp":"2026-10-12T12:00:00Z","content":[{"text":"small prompt"},{"inlineData":{"mimeType":"image/png","data":"small"}},{"fileData":{"mimeType":"text/plain","fileUri":"file:///synthetic/input"}}]}),
            serde_json::json!({"type":"gemini","id":"a","timestamp":"2026-10-12T12:00:00Z","model":"first-model","tokens":{"input":11,"output":3,"cached":2},"content":[{"text":body},{"text":"duplicate thought","thought":true},{"functionCall":{"id":"call","name":"Read","args":{"duplicate":"suppressed"}}}],"thoughts":[{"subject":"subject","description":body}],"toolCalls":[{"id":"call","name":"Read","agentId":"child","args":{"blob":body},"status":"success","result":[{"text":body},{"functionResponse":{"response":{"output":body}}}]},{"id":"object","name":"custom","args":null,"status":"error","result":{"z":body,"a":1}}],"future":{"ignored":true}}),
            serde_json::json!({"type":"user","id":"result-copy","content":[{"functionResponse":{"id":"call","name":"Read","response":{"output":body}}}]}),
            serde_json::json!({"type":"warning","id":"warning","content":body}),
            serde_json::json!({"type":"future-record","id":"future","content":"unknown record remains a native user turn","unknown":{"not_a_complete_carrier":true}}),
            serde_json::json!({"type":"gemini","id":"other-model","model":"second-model","content":"final answer"}),
        ];
        let normalized = |path: &Path| -> Result<Vec<u8>> {
            let mut raw = fs::read(path)?;
            let value: Value = serde_json::from_slice(&raw)?;
            for message in value["messages"].as_array().unwrap() {
                let id = message["id"].as_str().unwrap();
                anyhow::ensure!(uuid::Uuid::parse_str(id).is_ok(), "expected generated message UUID");
                let text = String::from_utf8(raw)?;
                raw = text.replace(id, "GENERATED-MESSAGE-ID").into_bytes();
            }
            Ok(raw)
        };
        let adapter = crate::harness::gemini::Gemini::detect();
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-modern.jsonl"
            } else {
                "session-legacy.json"
            });
            let wire = if modern {
                let discarded = serde_json::json!({"type":"user","id":"discard","content":"removed by rewind"});
                format!("{meta}\n{{\"type\":\"user\",\"id\":\"old\",\"content\":\"replaced by set\"}}\n{}\n{discarded}\n{{\"$rewindTo\":\"discard\"}}\n{{\"$set\":{{\"summary\":\"patched title\",\"kind\":\"subagent\"}}}}\n{{\"type\":\"user\",\"id\":\"duplicate-old\",\"id\":\"duplicate-last\",\"content\":\"duplicate winner\"}}\n{{\"type\":\"user\",\"id\":\"invalid\",\"content\":\"must skip\",\"ignored\":\"\\ud800\"}}\n", serde_json::json!({"$set":{"messages":rows}}))
            } else {
                let mut object = meta.as_object().unwrap().clone();
                object.insert("messages".into(), Value::Array(rows.clone()));
                Value::Object(object).to_string()
            };
            fs::write(&path, &wire)?;
            let reference = SessionRef::new("source", Harness::Gemini, &path);
            let expected = crate::stream::collect_with(&adapter, &reference, &Demand::COMPLETE)?;
            assert!(expected
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .all(|b| !matches!(b.text(), Some(Text::Span(_) | Text::Source(_)))));
            let actual = super::super::parse_for_emit(&adapter, &reference, Harness::Gemini)?;
            assert!(actual
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b.text(), Some(Text::Source(_)))));
            let mut materialized = actual.clone();
            materialized.materialize()?;
            assert_serialized_equal(&materialized, &expected)?;
            let mut custom = crate::stream::collect_with(&adapter, &reference, &Demand::COMPLETE.with_spans())?;
            custom.materialize()?;
            assert_serialized_equal(&custom, &expected)?;
            let options = EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                ..Default::default()
            };
            let owned_dir = world.base.join(if modern { "owned-modern" } else { "owned-legacy" });
            let checked_dir = world
                .base
                .join(if modern { "checked-modern" } else { "checked-legacy" });
            let (old, old_report) = super::super::emit_report(&expected, Harness::Gemini, &owned_dir, &options)?;
            let (new, new_report) = super::super::emit_report(&actual, Harness::Gemini, &checked_dir, &options)?;
            assert_eq!(normalized(&new.path)?, normalized(&old.path)?);
            assert_eq!(serde_json::to_value(new_report)?, serde_json::to_value(old_report)?);
            let before = fs::read(&new.path)?;
            fs::write(&path, b"{}")?;
            assert!(super::super::emit_report(&actual, Harness::Gemini, &checked_dir, &options).is_err());
            assert!(super::super::emit(&actual, Harness::Gemini, &checked_dir, &options).is_err());
            assert_eq!(fs::read(&new.path)?, before);
        }
        Ok(())
    }

    #[test]
    fn actual_claude_preparation_retains_original_body_identity() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-claude");
        let path = world.base.join("session.jsonl");
        let body = "plain retained body λ".repeat(512);
        fs::write(
            &path,
            serde_json::to_vec(
                &serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":[{"type":"text","text":body}]}}),
            )?,
        )?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let session =
            super::super::parse_for_emit(&crate::harness::claude::Claude::detect(), &reference, Harness::Codex)?;
        assert!(matches!(
            &session.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let output = world.base.join("output");
        let options = EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        };
        let result = emit(&session, Harness::Codex, &output, &options)?;
        let bytes = fs::read(&result.path)?;
        let mut changed = fs::read(&path)?;
        let at = changed.windows(5).position(|bytes| bytes == b"plain").unwrap();
        changed[at] = b'P';
        fs::write(&path, changed)?;
        assert!(emit(&session, Harness::Codex, &output, &options).is_err());
        assert_eq!(fs::read(&result.path)?, bytes);
        Ok(())
    }
    #[test]
    fn selected_preparation_keeps_only_window_and_original_guard() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare");
        let path = world.base.join("session.jsonl");
        let mut bytes = Vec::new();
        for index in 0..12 {
            let body = if index == 5 {
                "selected λ".repeat(1024)
            } else {
                "discarded".repeat(128 * 1024)
            };
            serde_json::to_writer(
                &mut bytes,
                &serde_json::json!({
                    "type":"user", "uuid":format!("u{index}"), "cwd":"/tmp/selected", "timestamp":"2025-01-01T00:00:00Z",
                    "message":{"role":"user","content":[{"type":"text","text":body}]}
                }),
            )?;
            bytes.push(b'\n');
        }
        fs::write(&path, bytes)?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        let mut old = crate::stream::collect_with(&adapter, &reference, &Demand::RICH)?;
        old.materialize()?;
        for (start, end) in [(5, Some(6)), (0, Some(0)), (8, Some(2)), (30, None), (11, None)] {
            let (mut selected, total, _) = super::super::prepare_selected(&adapter, &reference, start, end)?;
            assert_eq!(total, 12);
            let start = start.min(total);
            let end = end.unwrap_or(total).min(total).max(start);
            assert_eq!(selected.messages.len(), end - start);
            assert_eq!(selected.cwd, old.cwd);
            selected.materialize()?;
            assert_eq!(
                serde_json::to_value(&selected.messages)?,
                serde_json::to_value(&old.messages[start..end])?
            );
        }
        let (selected, _, _) = super::super::prepare_selected(&adapter, &reference, 5, Some(6))?;
        assert!(matches!(
            &selected.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let mut builder = crate::splice::Composition::default();
        builder.push(selected.clone(), 12, 5, Some(6));
        let actual = builder.finish(Some("00000000-0000-0000-0000-000000000002".into()), Harness::Claude);
        let expected = crate::splice::tests::compose(&[(&old, 5, Some(6))], Some(actual.id.clone()), Harness::Claude);
        let mut owned_actual = actual.clone();
        owned_actual.materialize()?;
        assert_eq!(serde_json::to_value(&owned_actual)?, serde_json::to_value(&expected)?);
        assert_eq!(
            crate::render::to_markdown(&owned_actual)?,
            crate::render::to_markdown(&expected)?
        );
        for target in [Harness::Claude, Harness::Codex] {
            let options = EmitOptions {
                new_id: Some(actual.id.clone()),
                ..Default::default()
            };
            let old_result = emit(&expected, target, &world.base.join(format!("old-{target}")), &options)?;
            let new_result = emit(&actual, target, &world.base.join(format!("new-{target}")), &options)?;
            assert!(
                fs::read(old_result.path)? == fs::read(new_result.path)?,
                "native {target} output"
            );
        }
        // Retained descriptors continue to reject a changed source after preparation returns.
        fs::write(&path, b"changed")?;
        let mut selected = selected;
        assert!(selected.materialize().is_err());
        Ok(())
    }

    struct Sidecar {
        path: PathBuf,
        mutate: bool,
        range: Option<(u64, u64)>,
    }
    impl crate::harness::Adapter for Sidecar {
        fn harness(&self) -> Harness {
            Harness::Claude
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<crate::Discovered> {
            Ok(Default::default())
        }
        fn stream(&self, reference: &SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            let mut session = Session::new(&reference.id, Harness::Claude);
            session.source_path = Some(reference.path.clone());
            sink.meta(&session);
            sink.dependency(&self.path);
            let len = fs::metadata(&self.path)?.len();
            let (offset, len) = self.range.unwrap_or((1, len - 1));
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text {
                text: Text::Span(crate::lazy::Span {
                    source: Some(self.path.clone()),
                    offset,
                    len,
                    escaped: false,
                }),
            });
            sink.try_message(message)?;
            if self.mutate {
                fs::write(&self.path, b"changed after source callback")?;
            }
            Ok(session)
        }
    }
    #[test]
    fn selected_opensession_defaults_to_underlying_harness_and_provenance() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-opensession");
        let path = world.base.join("source.opensession.json");
        let mut original = Session::new("wrapped-codex", Harness::Codex);
        original.cwd = Some("/tmp/wrapped".into());
        for text in ["head", "selected", "tail"] {
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.into() });
            original.messages.push(message);
        }
        fs::write(
            &path,
            serde_json::to_vec(&crate::harness::opensession::document(&original))?,
        )?;
        let reference = SessionRef::new("wrapped-codex", Harness::OpenSession, &path);
        let adapter = crate::harness::opensession::OpenSession;
        let mut legacy = crate::stream::collect_with(&adapter, &reference, &Demand::RICH)?;
        legacy.materialize()?;
        let (selected, total, _) = super::super::prepare_selected(&adapter, &reference, 1, Some(2))?;
        assert_eq!(selected.harness, Harness::Codex);
        let mut builder = crate::splice::Composition::default();
        builder.push(selected, total, 1, Some(2));
        let default_target = builder.first_harness().unwrap();
        assert_eq!(default_target, legacy.harness);
        let mut actual = builder.finish(Some("fixed".into()), default_target);
        actual.materialize()?;
        let expected = crate::splice::tests::compose(&[(&legacy, 1, Some(2))], Some("fixed".into()), legacy.harness);
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        Ok(())
    }

    #[test]
    fn selected_rich_messages_preserve_full_fidelity_and_eof_defaults() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-rich");
        let path = world.base.join("rich.jsonl");
        let rows = [
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":"head"}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","timestamp":"2025-01-01T00:00:00Z","message":{"role":"assistant","model":"model","usage":{"input_tokens":20,"output_tokens":10},"content":[{"type":"thinking","thinking":"reason","signature":"signature"},{"type":"tool_use","id":"tool","name":"Read","input":{"path":"/tmp/input","large":"x".repeat(8192)}}]}}),
            serde_json::json!({"type":"user","uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool","content":[{"type":"text","text":"result λ".repeat(1024)}],"is_error":false}]},"toolUseResult":{"rich":"metadata"}}),
            serde_json::json!({"type":"user","uuid":"tail","cwd":"/tmp/eof-default","message":{"role":"user","content":"tail"}}),
        ];
        let mut bytes = Vec::new();
        for row in rows {
            serde_json::to_writer(&mut bytes, &row)?;
            bytes.push(b'\n');
        }
        fs::write(&path, bytes)?;
        let reference = SessionRef::new("rich", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        let mut old = crate::stream::collect_with(&adapter, &reference, &Demand::RICH)?;
        old.materialize()?;
        let mut builder = crate::splice::Composition::default();
        for (start, end) in [(1, Some(3)), (2, Some(3))] {
            let (source, total, _) = super::super::prepare_selected(&adapter, &reference, start, end)?;
            assert_eq!(total, old.messages.len());
            assert_eq!(source.cwd, old.cwd);
            builder.push(source, total, start, end);
        }
        let mut actual = builder.finish(Some("fixed".into()), Harness::Claude);
        actual.materialize()?;
        let expected = crate::splice::tests::compose(
            &[(&old, 1, Some(3)), (&old, 2, Some(3))],
            Some("fixed".into()),
            Harness::Claude,
        );
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        Ok(())
    }

    /// A live Claude transcript: the whole read, then `change` to the file before publication.
    struct Live {
        change: fn(&Path) -> Result<()>,
    }
    impl crate::harness::Adapter for Live {
        fn harness(&self) -> Harness {
            Harness::Claude
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<crate::Discovered> {
            Ok(Default::default())
        }
        fn append_only(&self, reference: &SessionRef) -> bool {
            crate::harness::claude::Claude::detect().append_only(reference)
        }
        fn stream(&self, reference: &SessionRef, demand: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            let session = crate::harness::claude::Claude::detect().stream(reference, demand, sink)?;
            (self.change)(&reference.path)?;
            Ok(session)
        }
    }

    fn live_fixture(name: &str) -> Result<(cv_testkit::World, SessionRef, u64)> {
        let world = cv_testkit::World::new(name);
        let path = world.base.join("live.jsonl");
        let mut bytes = Vec::new();
        for (i, text) in ["head", "middle", "tail"].iter().enumerate() {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            let content = text.repeat(3000);
            let row = serde_json::json!({"type":role,"uuid":format!("m{i}"),"message":{"role":role,"content":[{"type":"text","text":content}]}});
            serde_json::to_writer(&mut bytes, &row)?;
            bytes.push(b'\n');
        }
        fs::write(&path, &bytes)?;
        let reference = SessionRef::new("live", Harness::Claude, &path);
        Ok((world, reference, bytes.len() as u64))
    }

    fn append(path: &Path) -> Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(
            b"{\"type\":\"user\",\"uuid\":\"later\",\"message\":{\"role\":\"user\",\"content\":\"later\"}}\n",
        )?;
        Ok(())
    }

    #[test]
    fn selected_live_source_grown_after_read_publishes_its_prefix() -> Result<()> {
        let (_world, reference, len) = live_fixture("selected-prepare-live-grown")?;
        let still = Live { change: |_| Ok(()) };
        let (mut expected, total, grown) = super::super::prepare_selected(&still, &reference, 0, None)?;
        assert_eq!((total, grown), (3, None));
        expected.materialize()?;
        let live = Live { change: append };
        let (mut actual, total, grown) = super::super::prepare_selected(&live, &reference, 0, None)?;
        let now = fs::metadata(&reference.path)?.len();
        assert!(now > len);
        assert_eq!(total, 3);
        assert_eq!(
            grown,
            Some(Grown {
                messages: 3,
                read: len,
                now
            })
        );
        actual.materialize()?;
        assert_serialized_equal(&actual, &expected)?;
        // `--upto N` reads the session as it stood at message N: the note cites the same N.
        let (_, _, grown) = super::super::prepare_selected(&live, &reference, 0, Some(2))?;
        assert_eq!(grown.map(|grown| grown.messages), Some(2));
        assert!(grown.unwrap().to_string().starts_with("as of message 2 / byte offset "));
        Ok(())
    }

    #[test]
    fn selected_live_source_with_its_read_prefix_rewritten_refuses() -> Result<()> {
        fn rewrite(path: &Path) -> Result<()> {
            let bytes = fs::read(path)?;
            let at = bytes.windows(4).position(|w| w == b"head").expect("fixture text");
            let mut changed = bytes.clone();
            changed[at..at + 4].copy_from_slice(b"HEAD");
            // In place, keeping the inode: only the sampled content tells the rewrite apart.
            use std::io::{Seek, Write};
            let mut file = fs::OpenOptions::new().write(true).open(path)?;
            file.seek(std::io::SeekFrom::Start(0))?;
            file.write_all(&changed)?;
            Ok(())
        }
        fn rewrite_and_grow(path: &Path) -> Result<()> {
            rewrite(path)?;
            append(path)
        }
        for change in [rewrite as fn(&Path) -> Result<()>, rewrite_and_grow] {
            let (_world, reference, _) = live_fixture("selected-prepare-live-rewritten")?;
            let error = super::super::prepare_selected(&Live { change }, &reference, 0, None).unwrap_err();
            assert!(
                format!("{error:#}").contains("source changed during port preparation"),
                "{error:#}"
            );
        }
        Ok(())
    }

    #[test]
    fn selected_preparation_checks_discarded_dependencies_and_failure() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-dependency");
        let primary = world.base.join("primary");
        let sidecar = world.base.join("sidecar");
        fs::write(&primary, b"{}")?;
        fs::write(&sidecar, b"!discarded")?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar,
            mutate: true,
            range: None,
        };
        // Even an empty selection must reject metadata/dependency mutation during the EOF pass.
        assert!(super::super::prepare_selected(&adapter, &reference, 5, None).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod same_target;
