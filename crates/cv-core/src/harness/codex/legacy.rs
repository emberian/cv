//! Legacy single-document rollouts (2025: `{session, items[]}` or a bare `items` array) read
//! through the same record codec as modern rollouts: the session object is a `session_meta`
//! payload, each item a `response_item` payload. Only one item's demanded fields are owned at once.
use super::*;
use crate::harness::store::document::Fields;
use crate::json::InvalidJson;
use crate::json::{ReadInput, Validation};

/// Value acceptance is whole-document, including ignored descendants. A zero-byte file is an
/// empty session (nothing recorded yet); any other document that does not decode is a named read
/// failure, never an empty session: the source is reported, as every unopenable source is.
fn read<T>(path: &Path, f: impl for<'a> FnOnce(&JsonSource, JsonView<'a>) -> Result<T>) -> Result<Option<T>> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    source.check_metadata(metadata)?;
    if len == 0 {
        source.check()?;
        return Ok(None);
    }
    let mut entered = false;
    match source.read(ReadInput::Document, Validation::Value, |view| {
        entered = true;
        f(&source, view)
    }) {
        Ok(value) => Ok(Some(value)),
        Err(error) if !entered && error.is::<InvalidJson>() => {
            source.check()?;
            Err(error.context(format!(
                "reading {}: not one legacy Codex JSON document",
                path.display()
            )))
        }
        Err(error) => Err(error),
    }
}

fn items_of(root: JsonView<'_>) -> Result<Option<crate::json::JsonArray<'_>>> {
    if let Some(items) = root.get("items")? {
        if let Some(array) = items.array()? {
            return Ok(Some(array));
        }
    }
    root.array()
}

/// A whole document into `reader`: its session object, then every item. Legacy rollouts have no
/// modern event authority, inheritance or complete carriers.
pub(super) fn items(reader: &mut Reader<'_>, root: JsonView<'_>) -> Result<()> {
    reader.meta(root.get("session")?)?;
    if let Some(items) = items_of(root)? {
        for item in items {
            reader.item(Some(item?), None)?;
        }
    }
    Ok(())
}

/// Discovery deliberately keeps the legacy policy: only session timestamps, no cwd, no event
/// authority; counts are deferred until full reads.
pub(super) fn scan(path: &Path) -> Result<Session> {
    let listing = read(path, |_source, root| {
        let mut scan = Session::new(String::new(), Harness::Codex);
        let meta = root
            .get("session")?
            .map(|v| Fields::parse(v, &["id", "timestamp"]))
            .transpose()?
            .flatten();
        if let Some(meta) = meta {
            scan.id = meta.string("id")?.unwrap_or_default();
            scan.created_at = meta.string("timestamp")?.as_deref().and_then(parse_ts);
            scan.updated_at = scan.created_at;
        }
        if let Some(items) = items_of(root)? {
            for item in items {
                let item = item?;
                let payload = record::Payload::of(item)?;
                if payload.ty.as_deref() == Some("message") && payload.role.as_deref().unwrap_or("user") == "user" {
                    consider_title(&mut scan.title, title_of(payload.view.get("content")?, None)?);
                    if scan.title.is_some() {
                        break;
                    }
                }
            }
        }
        Ok(scan)
    })?;
    Ok(listing.unwrap_or_else(|| Session::new(String::new(), Harness::Codex)))
}

pub(super) fn stream(r: &SessionRef, opts: &stream::Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    let empty = || Session {
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Codex)
    };
    let parsed = read(&r.path, |source, root| {
        let mut s = Session {
            source_path: Some(r.path.clone()),
            ..Session::new(String::new(), Harness::Codex)
        };
        let mut ctx = CodexCtx::default();
        let mut scratch = Vec::new();
        fn reader<'s>(
            session: &'s mut Session,
            ctx: &'s mut CodexCtx,
            out: &'s mut Vec<Message>,
            opts: &stream::Demand,
            source: &'s JsonSource,
        ) -> Reader<'s> {
            Reader {
                session,
                ctx,
                out,
                has_events: false,
                demand: opts.into(),
                source: Some(source),
            }
        }
        reader(&mut s, &mut ctx, &mut scratch, opts, source).meta(root.get("session")?)?;
        if s.id.is_empty() {
            s.id = r.id.clone();
        }
        sink.meta(&s);
        if let Some(error) = sink.take_error() {
            return Err(error);
        }
        if let Some(items) = items_of(root)? {
            for item in items {
                reader(&mut s, &mut ctx, &mut scratch, opts, source).item(Some(item?), None)?;
                for message in scratch.drain(..) {
                    if sink.try_message(message)? == Flow::Stop {
                        return Ok(s);
                    }
                }
            }
        }
        Ok(s)
    })?;
    match parsed {
        Some(s) => Ok(s),
        None => crate::stream::replay(empty(), sink),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collected(path: &Path, opts: &stream::Demand) -> Session {
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let mut sink = crate::stream::CollectSink::default();
        let mut session = Codex { roots: Vec::new() }.stream(&r, opts, &mut sink).unwrap();
        session.messages = sink.messages;
        session
    }

    #[test]
    fn checked_legacy_acceptance_stop_and_source_failures_are_preserved() {
        let world = crate::test_world("codex-legacy-acceptance");
        let path = world.base.join("rollout-legacy.json");
        let valid_prefix = r#"{"items":[{"type":"message","content":"visible"}],"ignored":"#;
        let deep = format!("{valid_prefix}{}0{} }}", "[".repeat(130), "]".repeat(130));
        for raw in ["", "false", "{}"] {
            // A zero-byte file, or a document with no items: an empty session, an empty listing.
            fs::write(&path, raw).unwrap();
            let actual = collected(&path, &stream::Demand::LEAN.with_spans());
            assert!(actual.messages.is_empty(), "{raw}");
            assert_eq!(actual.id, "fallback");
            let listing = scan(&path).unwrap();
            assert!(
                listing.id.is_empty() && listing.title.is_none() && listing.created_at.is_none(),
                "{raw}"
            );
        }
        for raw in [
            " ",
            r#"{"items":[{"type":"message","content":"visible"}],"ignored":"\ud800"}"#,
            &deep,
            r#"{"items":[{"type":"message","content":"visible"}],"ignored":1e9999}"#,
            r#"{"items":[{"type":"message","content":"visible"}]"#,
            r#"{"session":{"id":"legacy-broken"},"items":["#,
        ] {
            // Not exactly a serde Value: a named read failure, never an empty session.
            fs::write(&path, raw).unwrap();
            let r = SessionRef::new("fallback", Harness::Codex, &path);
            let mut sink = crate::stream::CollectSink::default();
            let error = Codex { roots: Vec::new() }
                .stream(&r, &stream::Demand::LEAN.with_spans(), &mut sink)
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("not one legacy Codex JSON document"),
                "{raw}: {error:#}"
            );
            assert!(sink.messages.is_empty(), "{raw}");
            assert!(scan(&path).is_err(), "{raw}");
        }
        let mut bad_utf8 = valid_prefix.as_bytes().to_vec();
        bad_utf8.extend_from_slice(b"\"\xff\"}");
        fs::write(&path, bad_utf8).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, &path);
        let mut sink = crate::stream::CollectSink::default();
        assert!(stream(&r, &stream::Demand::LEAN.with_spans(), &mut sink).is_err());
        assert!(sink.messages.is_empty());
        assert!(scan(&path).is_err());

        let raw = json!({"session":{"id":"legacy","timestamp":"2025-05-05T19:24:54Z"},"items":[
            {"type":"message","content":"body".repeat(8192)}, {"type":"message","content":"second"},
        ]})
        .to_string();
        fs::write(&path, &raw).unwrap();
        let mut saved = None;
        let mut stop = |m: Message| {
            saved = Some(m);
            Flow::Stop
        };
        let meta = stream(&r, &stream::Demand::LEAN.with_spans(), &mut stop).unwrap();
        assert_eq!(meta.id, "legacy");
        let saved = saved.unwrap();
        assert!(matches!(saved.content.first(), Some(Block::Text { text , .. }) if text.inline_str().is_none()));
        fs::write(&path, "replacement source").unwrap();
        if let Block::Text { text, .. } = &saved.content[0] {
            assert!(text.try_resolve(&crate::Resolver::new(Some(path.clone()))).is_err());
        }
        fs::write(&path, &raw).unwrap();
        let mut mutate = |_m: Message| {
            fs::write(&path, "replacement source").unwrap();
            Flow::Stop
        };
        assert!(stream(&r, &stream::Demand::LEAN.with_spans(), &mut mutate).is_err());
    }
}
