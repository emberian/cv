//! Checked legacy framing and schema-selected discovery/message fields.
use super::*;
use crate::harness::store::{document::Fields, jsonl::ranges};
use crate::json::{InvalidJson, JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::stream::Tier;

/// The first physical record is always the header, including when it is malformed or empty.
/// Rich fields follow Demand demand; unrelated legacy fields never become owned trees.
pub(super) fn stream(name: &str, path: &Path, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    let source = JsonSource::capture(path)?;
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    source.check_metadata(metadata.clone())?;
    let records = ranges::records(
        &file,
        ranges::Range {
            offset: 0,
            len: metadata.len(),
        },
    )?;
    let mut reader = crate::harness::store::jsonl::Records::new(&source, &file);
    let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena()?);
    let mut session = Session::new(name, Harness::Goose);
    session.source_path = Some(path.to_path_buf());
    let mut tools = HashMap::new();
    let mut header = true;
    // A line that is not one JSON value (header or message) is counted.
    let mut unparsed = crate::harness::Unparsed::default();
    for record in records {
        let (range, _) = record?;
        // Empty lines are still headers when first; subsequent empty lines are ignored.
        let result = if range.len == 0 {
            Ok(None)
        } else {
            let visit = |view: JsonView<'_>| {
                if header {
                    read_header(view, &mut session, None)?;
                    Ok(None)
                } else {
                    read_message(view, &mut tools, opts, &source, &arena)
                }
            };
            reader.read(range.offset, range.len, visit)
        };
        let message = match result {
            Ok(message) => message,
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                unparsed.count();
                None
            }
            Err(error) => return Err(error),
        };
        if header {
            sink.meta(&session);
            header = false;
        } else if let Some(message) = message {
            if sink.message(message) == Flow::Stop {
                break;
            }
        }
    }
    if header {
        sink.meta(&session);
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    unparsed.note(&mut session);
    Ok(session)
}

fn read_header(view: JsonView<'_>, session: &mut Session, limit: Option<usize>) -> Result<()> {
    let Some(fields) = Fields::parse(view, &["description", "working_dir", "created_at", "updated_at"])? else {
        return Ok(());
    };
    session.title = if let Some(limit) = limit {
        let mut head = crate::projection::Head::bounded(limit);
        if let Some(description) = fields.get("description") {
            description.for_each_char(|ch| !head.push_char(ch))?;
        }
        (!head.is_blank()).then(|| head.finish())
    } else {
        fields.string("description")?.filter(|s| !s.trim().is_empty())
    };
    session.cwd = fields
        .string("working_dir")?
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    for (key, output) in [
        ("created_at", &mut session.created_at),
        ("updated_at", &mut session.updated_at),
    ] {
        if let Some(value) = fields.get(key) {
            if value.is_string()? || value.as_number()?.is_some() {
                *output = parse_ts_value(value.resolve()?.as_ref());
            }
        }
    }
    Ok(())
}

fn read_message(
    view: JsonView<'_>,
    tools: &mut HashMap<String, String>,
    opts: &Demand,
    source: &JsonSource,
    arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(view, &["role", "id", "created", "content"])? else {
        return Ok(None);
    };
    let role = fields.string("role")?.unwrap_or_else(|| "user".into());
    let id = fields.string("id")?;
    let timestamp = fields
        .get("created")
        .map(|v| v.as_number())
        .transpose()?
        .flatten()
        .and_then(|n| n.as_i64())
        .and_then(ts_from_epoch);
    let decoded = match fields.get("content") {
        Some(content) => record::content(content, tools, opts, Some((source, arena)))?,
        None => Decoded::default(),
    };
    let Some(mut message) = build_message(&role, id, timestamp, decoded) else {
        return Ok(None);
    };
    if opts.tier < Tier::Rich {
        omit_rich_details(&mut message);
    }
    Ok(Some(message))
}

#[cfg(test)]
pub(super) static SCANNED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

pub(super) fn discover(name: &str, path: &Path) -> Result<SessionRef> {
    #[cfg(test)]
    SCANNED.lock().unwrap().push(path.to_path_buf());
    let source = JsonSource::capture(path)?;
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    source.check_metadata(metadata.clone())?;
    let mut records = ranges::records(
        &file,
        ranges::Range {
            offset: 0,
            len: metadata.len(),
        },
    )?;
    records.keep_small_records(64 * 1024);
    let mut session = SessionRef::new(name, Harness::Goose, path);
    let mut header = true;
    while let Some(record) = records.next() {
        let (range, _) = record?;
        let result = if let Some(bytes) = records.small_record() {
            // read_to_string rejected invalid UTF-8 in malformed and ignored records too.
            let text = std::str::from_utf8(bytes)?;
            if header {
                match serde_json::from_str::<Value>(text) {
                    Ok(value) => note_header((&value).into(), &mut session),
                    Err(_) => Ok(()),
                }
            } else {
                Ok(())
            }
        } else if header {
            source.read(ReadInput::Range(range.offset, range.len), Validation::Value, |view| {
                note_header(view, &mut session)
            })
        } else {
            // Legacy discovery has always rejected invalid UTF-8 anywhere in the file. Keep
            // that inclusion contract without decoding undemanded message JSON or bodies.
            anyhow::ensure!(
                ranges::valid_utf8(&mut ranges::reader(&file, range)?)?,
                "invalid Goose legacy UTF-8"
            );
            Ok(())
        };
        match result {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                // Failed syntax can stop before an invalid UTF-8 suffix; it cannot hide it.
                anyhow::ensure!(
                    ranges::valid_utf8(&mut ranges::reader(&file, range)?)?,
                    "invalid Goose legacy UTF-8"
                );
            }
            Err(error) => return Err(error),
        }
        header = false;
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    Ok(session)
}
fn note_header(view: JsonView<'_>, reference: &mut SessionRef) -> Result<()> {
    let mut session = Session::new(&reference.id, Harness::Goose);
    read_header(view, &mut session, Some(crate::projection::Head::TITLE))?;
    reference.title = session.title;
    reference.cwd = session.cwd;
    reference.created_at = session.created_at;
    reference.updated_at = session.updated_at;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_discovery_preserves_header_and_whole_utf8_policy() {
        let world = crate::test_world("goose-legacy-discovery");
        let path = world.base.join("legacy.jsonl");
        // Frozen from the retired whole-file `Value` reader (decision 11: goldens, not oracles).
        let mut frozen = crate::emit::frozen::Frozen::new("goose-legacy-discovery");
        let mut observe = |label: String| {
            let mut r = serde_json::to_value(discover("legacy", &path).ok()).unwrap();
            if let Some(r) = r.as_object_mut() {
                r.remove("path");
            }
            frozen.insert(label, r);
        };
        let cases: Vec<Vec<u8>> = vec![
  vec![], b"\n".to_vec(), b"{bad}\nnull\n[1]\n3\ntrue\n{}\n".to_vec(),
  br#"{"description":"old","description":"new","working_dir":"/repo","created_at":1704067200,"updated_at":"2024-01-01 12:00:00"}
{"role":"assistant","content":[{"type":"toolResponse"}]}
{"role":"assistant","content":[{"type":"toolResponse"},{"type":"text","text":"x"}]}
{"role":false,"content":[]}
{"role":"system","content":[{"type":"text","text":"x"}]}
{"role":"user","role":"tool","content":null}
{"role":"user","content":[{"type":"text","type":"toolResponse"}]}
{"role":"user","content":[{"type":"toolResponse"},{"type":null}]}
"#.to_vec(),
  br#"{"description":null,"working_dir":false,"created_at":[],"updated_at":true}
{"role":"user","ignored":1e400}
{"role":"user","ignored":"\ud800"}
{"role":"user","ignored":1e400,"ignored":1}
{"role":"user","ignored":"\ud800","ignored":"ok"}
{"role":"user","content":[{"type":"toolResponse","ignored":1e400}]}
{"role":"assistant","content":"end"}"#.to_vec(),
  "\u{2003}{}\n{\"role\":\"user\"}\r\n".as_bytes().to_vec(),
  [b"{}\n{bad}\n".as_slice(),b"\xff\n"].concat(),
  [b"{bad}\n".as_slice(),b"\xff"].concat(),
 ];
        let mut cases = cases;
        cases.push([b"{}\n{bad".as_slice(), &vec![b'x'; 70_000], b"\xff\n{}\n"].concat());
        cases.push([b"{}\n{bad".as_slice(), &vec![b'x'; 70_000], b"\n{}\n"].concat());
        for (i, bytes) in cases.into_iter().enumerate() {
            std::fs::write(&path, bytes).unwrap();
            observe(format!("case {i}"));
        }
        for escaped in [false, true] {
            let body = if escaped { "\\n雪" } else { "x雪" }.repeat(100_000);
            let header = serde_json::json!({"description":" \nTitle","working_dir":"/repo","created_at":1704067200000_i64,"unused":body});
            let row = serde_json::json!({"role":"user","content":[{"type":"text","text":body}]});
            let text = format!("{header}\n{row}\n{{\"role\":\"assistant\",\"content\":[]}}");
            std::fs::write(&path, text).unwrap();
            observe(format!("body escaped={escaped}"));
        }
        for (j, title) in [
            " ".repeat(100_000),
            format!("{}Meaning", " \u{2003}".repeat(100_000)),
            "雪\n".repeat(100_000),
        ]
        .into_iter()
        .enumerate()
        {
            std::fs::write(&path, serde_json::json!({"description":title}).to_string()).unwrap();
            observe(format!("title {j}"));
        }
        let deep = format!("{{}}\n{}0{}\n{{\"role\":\"user\"}}\n", "[".repeat(129), "]".repeat(129));
        std::fs::write(&path, deep).unwrap();
        observe("deep".to_string());
        frozen.check().unwrap();
    }

    fn collected(path: &Path, opts: &Demand) -> Session {
        let mut sink = crate::stream::CollectSink::default();
        let mut session = stream("legacy", path, opts, &mut sink).unwrap();
        session.messages = sink.messages;
        session.materialize().unwrap();
        session
    }

    /// Oracle is a line/Value reader feeding owned values to the codec; it tests framing,
    /// acceptance and ordering, including malformed ignored fields and duplicate keys.
    fn owned_messages(path: &Path) -> Vec<Message> {
        let text = std::fs::read(path).unwrap();
        let source = JsonSource::capture(path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let mut tools = HashMap::new();
        text.split(|b| *b == b'\n')
            .skip(1)
            .filter_map(|line| {
                let value = serde_json::from_slice::<Value>(line).ok()?;
                let role = value.get("role").and_then(Value::as_str).unwrap_or("user");
                let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
                let timestamp = value.get("created").and_then(Value::as_i64).and_then(ts_from_epoch);
                let decoded = value
                    .get("content")
                    .map(|c| record::content(c.into(), &mut tools, &Demand::RICH, Some((&source, &arena))).unwrap())
                    .unwrap_or_default();
                build_message(role, id, timestamp, decoded)
            })
            .collect()
    }

    #[test]
    fn selected_stream_preserves_legacy_acceptance_order_and_rich_demand() {
        let world = crate::test_world("goose-legacy-stream");
        let path = world.base.join("legacy.jsonl");
        let giant = "雪\n\"".repeat(30_000);
        let header =
            serde_json::json!({"description":"old","working_dir":"/repo","created_at":1704067200,"ignored":giant});
        let messages = serde_json::json!([
            {"id":"user","role":"user","created":1704067200,"content":[{"type":"text","text":giant}]},
            {"id":"call","role":"assistant","content":[{"type":"toolRequest","id":"tool","toolCall":{"value":{"name":"shell","arguments":{"command":giant}}}}]},
            {"id":"result","role":"user","content":[{"type":"toolResponse","id":"tool","toolResult":{"value":{"content":[{"type":"text","text":giant},{"type":"resource","resource":{"text":"end"}}],"structuredContent":{"stdout":giant},"_meta":{"child":"s"}}}}]},
            {"id":"notice","role":"assistant","content":[{"type":"systemNotification","msg":giant,"ignored":giant}]},
            {"id":"error","role":"assistant","content":[{"type":"error","message":giant}]}
        ]);
        let mut bytes = format!("{header}\n").into_bytes();
        for message in messages.as_array().unwrap() {
            bytes.extend_from_slice(message.to_string().as_bytes());
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(
            br#"{bad}
{"role":"user","content":"reject","ignored":1e400}
{"role":"user","content":"reject","ignored":"\ud800"}
{"role":"user","role":"assistant","content":"accepted duplicate","ignored":1e400,"ignored":1}
"#,
        );
        bytes.extend_from_slice(b"\xff\n");
        bytes.extend_from_slice(br#"{"role":null,"id":false,"created":1.5,"content":"final"}"#);
        std::fs::write(&path, &bytes).unwrap();
        let expected = owned_messages(&path);
        let observe = |messages: Vec<Message>| serde_json::to_value(messages).unwrap();
        for opts in [Demand::RICH, Demand::COMPLETE, Demand::RICH.with_spans()] {
            let actual = collected(&path, &opts);
            assert_eq!(actual.title.as_deref(), Some("old"));
            assert_eq!(actual.cwd.as_deref(), Some(Path::new("/repo")));
            assert_eq!(observe(actual.messages), observe(expected.clone()), "{opts:?}");
        }
        let mut expected = expected;
        for message in &mut expected {
            message.extra.clear();
            for block in &mut message.content {
                if let Block::ToolResult { details, .. } = block {
                    *details = None;
                }
            }
        }
        for opts in [
            Demand::LEAN.with_spans(),
            Demand::LEAN,
            Demand::FACTS.with_spans(),
            Demand::USAGE.with_spans(),
        ] {
            assert_eq!(
                observe(collected(&path, &opts).messages),
                observe(expected.clone()),
                "{opts:?}"
            );
        }
    }

    #[test]
    fn selected_stream_retains_checked_bodies_and_stops_before_later_records() {
        let world = crate::test_world("goose-legacy-stop");
        let path = world.base.join("legacy.jsonl");
        let giant = "\n雪".repeat(30_000);
        let first = serde_json::json!({"role":"assistant","content":[{"type":"text","text":giant},{"type":"toolRequest","id":"tool","toolCall":{"value":{"name":"shell","arguments":{"command":giant}}}}]});
        // Invalid UTF-8 on a later row is skipped by the old reader; first-message demand must
        // retain only the admitted record and remain usable after this read ends.
        let mut bytes = format!("{{\"description\":\"before\"}}\n{first}\n").into_bytes();
        bytes.extend_from_slice(b"\xff\n");
        bytes.extend_from_slice(br#"{"role":"user","content":"unread"}"#);
        std::fs::write(&path, bytes).unwrap();
        struct Stop(Option<Message>);
        impl MessageSink for Stop {
            fn message(&mut self, message: Message) -> Flow {
                assert!(self.0.is_none());
                self.0 = Some(message);
                Flow::Stop
            }
        }
        let mut stop = Stop(None);
        let session = stream("legacy", &path, &Demand::LEAN.with_spans(), &mut stop).unwrap();
        assert_eq!(session.title.as_deref(), Some("before"));
        let mut message = stop.0.unwrap();
        assert!(matches!(
            &message.content[0],
            Block::Text {
                text: crate::lazy::Text::Source(_),
                ..
            }
        ));
        assert!(matches!(
            &message.content[1],
            Block::ToolUse {
                input: crate::json::Json::Source(_),
                ..
            }
        ));
        let mut copy = message.clone();
        copy.materialize(&crate::lazy::Resolver::new(None)).unwrap();
        assert!(matches!(&copy.content[0], Block::Text { text , .. } if text.inline_str() == Some(giant.as_str())));
        // Header changes invalidate retained fields even when their own message bytes are stable.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"description":"after!"}"#)
            .unwrap();
        assert!(message.materialize(&crate::lazy::Resolver::new(None)).is_err());
        struct ChangeAtStop(PathBuf);
        impl MessageSink for ChangeAtStop {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.0)
                    .unwrap()
                    .write_all(br#"{"description":"before"}"#)
                    .unwrap();
                Flow::Stop
            }
        }
        assert!(stream(
            "legacy",
            &path,
            &Demand::LEAN.with_spans(),
            &mut ChangeAtStop(path.clone())
        )
        .is_err());
    }
}
