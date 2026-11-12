//! Cursor Agent CLI: hex JSON metadata and content-addressed protobuf/JSON blobs.
//! Root/summary references replay one message at a time under one SQLite snapshot. The
//! protobuf schema was recovered from the installed 2026.10.01-e373342 bundle; the corpus
//! contains actual CLI-written data, not reconstructed protocol examples.
use super::*;
use crate::harness::store::{bytes::ByteSource, document::Fields, sqlite::Cells};
use crate::json::JsonView;
use crate::lazy::Text;
use crate::stream::Tier;
use std::io::Read;

/// Cursor Agent CLI/ACP metadata (`meta.json` beside a `cli:` session's store) participates in
/// both catalog discovery and consumer freshness. The path is returned even while absent:
/// creating it can supply a previously missing cwd or title.
pub(super) fn metadata(r: &SessionRef) -> Option<PathBuf> {
    (r.harness == Harness::Cursor && r.id.starts_with("cli:"))
        .then(|| r.path.parent().map(|parent| parent.join("meta.json")))
        .flatten()
}

pub(super) fn roots(home: &Path) -> Vec<PathBuf> {
    ["chats", "acp-sessions"]
        .into_iter()
        .map(|name| home.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

pub(super) fn discover(home: &Path, out: &mut crate::Discovered) {
    for root in roots(home) {
        for entry in walkdir::WalkDir::new(root).max_depth(3) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    out.skipped.extend(crate::Skipped::walk(&e));
                    continue;
                }
            };
            if !entry.file_type().is_file() || entry.file_name() != "store.db" {
                continue;
            }
            let path = entry.path();
            let mut reads = crate::source::ReadSet::default();
            for suffix in ["", "-wal", "-journal"] {
                let mut dependency = path.as_os_str().to_os_string();
                dependency.push(suffix);
                reads.record(&PathBuf::from(dependency));
            }
            if let Some(meta) = path.parent().map(|parent| parent.join("meta.json")) {
                reads.record(&meta);
            }
            let result = crate::discover_cache::cached_scan_many_with_reads(path, reads, || {
                let reference = open_ro(path).and_then(|conn| {
                    conn.read(|conn, reads| -> Result<SessionRef> {
                        let mut collector = Count::default();
                        let r = SessionRef::new("cli:", Harness::Cursor, path);
                        let session = read_with_reads(
                            conn,
                            &r,
                            &crate::stream::Demand::LEAN.with_spans(),
                            true,
                            reads,
                            &mut collector,
                        )?;
                        if let Some(error) = collector.error {
                            return Err(error);
                        }
                        Ok(SessionRef {
                            id: session.id,
                            cwd: session.cwd,
                            title: session.title.or(collector.title),
                            created_at: session.created_at,
                            updated_at: session.updated_at,
                            message_count: Some(collector.count),
                            ..r
                        })
                    })
                })?;
                // An empty or uninitialized native store is not a conversation.
                Ok(if reference.message_count.is_some_and(|n| n > 0) {
                    vec![reference]
                } else {
                    Vec::new()
                })
            });
            if let Some(references) = out.keep(path, result) {
                out.refs.extend(references);
            }
        }
    }
}

/// Discovery's sink: turns and the canonical title of the first meaningful human prompt.
#[derive(Default)]
struct Count {
    count: usize,
    title: Option<String>,
    error: Option<anyhow::Error>,
}
impl MessageSink for Count {
    fn message(&mut self, message: Message) -> Flow {
        self.count += usize::from(message.is_turn());
        if self.title.is_none() && message.kind == MessageKind::Prompt && message.origin == Origin::Human {
            match message.prompt_title(&crate::lazy::Resolver::new(None)) {
                Ok(title) => self.title = title,
                Err(error) => {
                    self.error.get_or_insert(error);
                    return Flow::Stop;
                }
            }
        }
        Flow::Continue
    }
}
pub(super) fn stream(
    conn: &Connection,
    reads: &crate::source::ReadSet,
    r: &SessionRef,
    opts: &crate::stream::Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    read_with_reads(conn, r, opts, false, reads, sink)
}

#[cfg(test)]
fn read(
    conn: &Connection,
    r: &SessionRef,
    opts: &crate::stream::Demand,
    counting: bool,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    read_with_reads(conn, r, opts, counting, &Default::default(), sink)
}

fn read_with_reads(
    conn: &Connection,
    r: &SessionRef,
    opts: &crate::stream::Demand,
    counting: bool,
    reads: &crate::source::ReadSet,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    // The compound provenance is captured before the first SQL read establishes its snapshot: a
    // checkpoint can leave the DB unchanged while the WAL or the optional ACP head changes.
    let mut reads = reads.clone();
    let mut wal = r.path.as_os_str().to_os_string();
    wal.push("-wal");
    reads.record(&PathBuf::from(wal));
    if let Some(meta) = metadata(r) {
        reads.record(&meta);
    }
    let cells = Cells::new(conn, &r.path, &reads, "blobs")?;
    let result = read_snapshot(conn, r, opts, counting, sink, &cells);
    // Even Stop and callback errors must not hide a source mutation before publication.
    cells.check()?;
    result
}

fn read_snapshot(
    conn: &Connection,
    r: &SessionRef,
    opts: &crate::stream::Demand,
    counting: bool,
    sink: &mut dyn MessageSink,
    cells: &Cells,
) -> Result<Session> {
    let _snapshot = crate::harness::store::sqlite::snapshot(conn)?;
    let encoded: String = conn.query_row("SELECT value FROM meta WHERE key='0'", [], |row| row.get(0))?;
    let metadata: Value = serde_json::from_slice(&decode_hex(&encoded)?).context("Cursor CLI metadata JSON")?;
    let id = metadata
        .get("agentId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Cursor CLI agentId missing")?;
    if !counting {
        anyhow::ensure!(
            r.id == format!("cli:{id}"),
            "Cursor CLI store identity changed since discovery"
        );
    }
    let root_id = metadata
        .get("latestRootBlobId")
        .and_then(Value::as_str)
        .context("Cursor CLI root missing")?;
    anyhow::ensure!(
        decode_hex(root_id)?.len() == 32,
        "Cursor CLI root is not a SHA-256 identifier"
    );
    let root = blob(conn, root_id)?;
    let mut session = Session {
        source_path: Some(r.path.clone()),
        title: metadata
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && *name != "New Agent")
            .map(str::to_owned),
        created_at: metadata
            .get("createdAt")
            .and_then(Value::as_i64)
            .and_then(|ms| ts_from_epoch(ms as f64)),
        ..Session::new(format!("cli:{id}"), Harness::Cursor)
    };
    session
        .harness_extra_mut(Harness::Cursor)
        .insert("store".into(), "agent-cli".into());
    for key in ["mode", "isRunEverything", "approvalMode"] {
        if let Some(value) = metadata.get(key) {
            session
                .harness_extra_mut(Harness::Cursor)
                .insert(key.into(), value.clone());
        }
    }
    if let Some(info) = metadata.get("subagentInfo") {
        session.lineage.parent = info
            .get("parentAgentId")
            .and_then(Value::as_str)
            .map(|id| format!("cli:{id}"));
        if let Some(root) = info.get("rootParentAgentId") {
            session
                .harness_extra_mut(Harness::Cursor)
                .insert("root_parent_agent_id".into(), root.clone());
        }
        session.lineage.spawned_by_tool_use = info.get("toolCallId").and_then(Value::as_str).map(str::to_owned);
        session.lineage.agent_path = info.get("typeName").and_then(Value::as_str).map(str::to_owned);
    }
    session.model = metadata.get("lastUsedModel").and_then(Value::as_str).map(str::to_owned);
    // First validate the entire root while extracting only demanded metadata. No callbacks have
    // run yet, and repeated blob references do not become a growing Vec.
    fields(root.reader()?, &mut |number, wire, value, input| {
        match (number, wire) {
            (9, 2) if session.cwd.is_none() && value <= 65536 => {
                let mut uri = String::new();
                input.read_to_string(&mut uri)?;
                if let Some(path) = file_uri_to_path(&uri) {
                    session.cwd = Some(path);
                }
            }
            (10, 0) => {
                let mode = match value {
                    0 => "unspecified",
                    1 => "agent",
                    2 => "ask",
                    3 => "plan",
                    4 => "debug",
                    5 => "triage",
                    6 => "project",
                    7 => "multitask",
                    8 => "custom",
                    _ => "unknown",
                };
                session
                    .harness_extra_mut(Harness::Cursor)
                    .insert("unified_mode".into(), mode.into());
                session
                    .harness_extra_mut(Harness::Cursor)
                    .insert("cli_mode_raw".into(), value.into());
            }
            (26, 0) => {
                session.created_at = i64::try_from(value)
                    .ok()
                    .and_then(|ms| ts_from_epoch(ms as f64))
                    .or(session.created_at)
            }
            _ => {}
        }
        Ok(Flow::Continue)
    })?;
    if session.cwd.is_none() || session.title.is_none() {
        if let Some(meta) = acp_metadata(&r.path, sink) {
            session.cwd = session
                .cwd
                .or_else(|| meta.get("cwd").and_then(Value::as_str).map(PathBuf::from));
            session.title = session
                .title
                .or_else(|| meta.get("title").and_then(Value::as_str).map(str::to_owned));
        }
    }
    // Native metadata has a creation time but no logical update timestamp. WAL writes may not
    // change the main database yet; filesystem time is an explicitly approximate fallback.
    let mut wal = r.path.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    sink.dependency(&wal);
    session.updated_at = [&r.path, &wal]
        .into_iter()
        .filter_map(|path| {
            std::fs::metadata(path)
                .ok()?
                .modified()
                .ok()
                .map(chrono::DateTime::from)
        })
        .max()
        .or(session.created_at);
    sink.meta(&session);
    if let Some(error) = sink.take_error() {
        return Err(error);
    }
    // Cursor's fact reducers read native summary/context/error markers through the IR; the
    // record and part metadata bags are rich demands.
    let demand = *opts;
    let mut emit = |id: &str| -> Result<Flow> {
        let decoded = match cells.row(conn, "blobs", "data", blob_rowid(conn, id)?)? {
            Some(cell) => cells.read(cell, |view| Ok((message(view, demand)?, model(view)?)))?,
            None => None,
        };
        let (message, model) = decoded.with_context(|| format!("Cursor CLI message blob {id} is not JSON"))?;
        if let Some(model) = model {
            session.model.get_or_insert(model);
        }
        match message {
            Some(message) => Ok(sink.message(message)),
            None => Ok(Flow::Continue),
        }
    };
    // Archives precede the current prompt. Preserve each archive's ordered message references;
    // the summary_message field is represented in the current prompt, not emitted twice.
    let mut has_archives = false;
    fields(root.reader()?, &mut |n, _, _, _| {
        has_archives |= n == 13;
        Ok(Flow::Continue)
    })?;
    let archive_field = if has_archives { 13 } else { 11 };
    if references(&root, archive_field, &mut |id| {
        let archive = blob(conn, id)?;
        fields(archive.reader()?, &mut |_, _, _, _| Ok(Flow::Continue))?;
        references(&archive, 1, &mut emit)
    })? == Flow::Stop
    {
        return Ok(session);
    }
    references(&root, 1, &mut emit)?;
    Ok(session)
}

fn acp_metadata(path: &Path, sink: &mut dyn MessageSink) -> Option<Value> {
    let path = path.parent()?.join("meta.json");
    sink.dependency(&path);
    serde_json::from_reader(std::fs::File::open(path).ok()?).ok()
}

fn blob_input<'a>(conn: &'a Connection, id: &str) -> Result<rusqlite::blob::Blob<'a>> {
    Ok(conn.blob_open(rusqlite::MAIN_DB, "blobs", "data", blob_rowid(conn, id)?, true)?)
}
fn blob_rowid(conn: &Connection, id: &str) -> Result<i64> {
    conn.query_row("SELECT rowid FROM blobs WHERE id=?1", [id], |row| row.get(0))
        .with_context(|| format!("Cursor CLI blob {id} missing"))
}
fn blob(conn: &Connection, id: &str) -> Result<ByteSource> {
    Ok(ByteSource::read(blob_input(conn, id)?)?)
}

fn references(source: &ByteSource, field: u32, f: &mut dyn FnMut(&str) -> Result<Flow>) -> Result<Flow> {
    fields(source.reader()?, &mut |number, wire, length, input| {
        if number == field {
            anyhow::ensure!(
                wire == 2 && length == 32,
                "Cursor CLI blob reference has invalid wire shape"
            );
            let mut id = [0; 32];
            input.read_exact(&mut id)?;
            return f(&encode_hex(&id));
        }
        Ok(Flow::Continue)
    })
}

/// A bounded protobuf wire walker. Unknown fields are skipped exactly, including truncated
/// payload checks; no wire length controls an allocation. Callbacks may read a limited field.
type FieldVisitor<'a> = dyn FnMut(u32, u8, u64, &mut dyn Read) -> Result<Flow> + 'a;
fn fields(mut input: impl Read, f: &mut FieldVisitor<'_>) -> Result<Flow> {
    loop {
        let mut first = [0];
        if input.read(&mut first)? == 0 {
            return Ok(Flow::Continue);
        }
        let tag = varint(&mut input, Some(first[0]))?;
        let number = u32::try_from(tag >> 3)?;
        anyhow::ensure!(
            number > 0 && number < (1 << 29),
            "invalid Cursor CLI protobuf field number"
        );
        let wire = (tag & 7) as u8;
        if wire == 0 {
            let value = varint(&mut input, None)?;
            if f(number, wire, value, &mut std::io::empty())? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        } else {
            let length = match wire {
                1 => 8,
                2 => varint(&mut input, None)?,
                5 => 4,
                _ => anyhow::bail!("unsupported Cursor CLI protobuf wire type {wire}"),
            };
            let mut field = (&mut input).take(length);
            if f(number, wire, length, &mut field)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
            let remaining = field.limit();
            anyhow::ensure!(
                std::io::copy(&mut field, &mut std::io::sink())? == remaining,
                "truncated Cursor CLI protobuf field"
            );
        }
    }
}
fn varint(input: &mut dyn Read, first: Option<u8>) -> Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = if shift == 0 {
            if let Some(first) = first {
                first
            } else {
                let mut b = [0];
                input.read_exact(&mut b)?;
                b[0]
            }
        } else {
            let mut b = [0];
            input.read_exact(&mut b)?;
            b[0]
        };
        anyhow::ensure!(shift != 63 || byte <= 1, "overflowing Cursor CLI protobuf varint");
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    anyhow::bail!("overflowing Cursor CLI protobuf varint")
}
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(text.len().is_multiple_of(2), "odd Cursor CLI hex length");
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let hex = |b: u8| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                b'A'..=b'F' => Ok(b - b'A' + 10),
                _ => anyhow::bail!("invalid Cursor CLI hex digit"),
            };
            Ok((hex(pair[0])? << 4) | hex(pair[1])?)
        })
        .collect()
}
fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

fn string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    view.map(|view| view.string()).transpose().map(Option::flatten)
}
fn path<'a>(view: Option<JsonView<'a>>, keys: &[&str]) -> Result<Option<JsonView<'a>>> {
    let mut view = view;
    for key in keys {
        view = match view {
            Some(v) => v.get(key)?,
            None => return Ok(None),
        };
    }
    Ok(view)
}
fn flag(view: Option<JsonView<'_>>) -> Result<bool> {
    Ok(match view {
        Some(view) => view.as_bool()? == Some(true),
        None => false,
    })
}

/// The model the record's system prompt was fingerprinted for.
fn model(view: JsonView<'_>) -> Result<Option<String>> {
    string(path(
        Some(view),
        &["providerOptions", "cursor", "systemPromptFingerprint", "model"],
    )?)
}

/// An object's entries as an owned map holds them, less `taken` keys removed in order (an
/// order-preserving map's remove moves the last entry into the hole). Only the rest is resolved.
fn remainder(view: JsonView<'_>, taken: &[&str]) -> Result<Map<String, Value>> {
    let mut entries = Vec::new();
    view.for_each_entry(|key, value| {
        entries.push((key.to_owned(), value));
        Ok(())
    })?;
    for key in taken {
        if let Some(position) = entries.iter().position(|(name, _)| name == key) {
            entries.swap_remove(position);
        }
    }
    entries
        .into_iter()
        .map(|(key, value)| Ok((key, value.resolve()?.into_owned())))
        .collect()
}

/// One message record (`{role, id, content, providerOptions, ...}`); None for an unknown role or
/// an empty message the demand does not carry.
/// The demand resolves long text and arguments held in the store (`spans`), the record and part
/// metadata bags ([`Tier::Rich`]), and every record, carriers included ([`Tier::Complete`]).
fn message(view: JsonView<'_>, demand: crate::stream::Demand) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(view, &["role", "id", "content", "providerOptions"])? else {
        return Ok(None);
    };
    let role = match fields.string("role")?.as_deref() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        Some("system" | "developer") => Role::System,
        _ => return Ok(None),
    };
    let cursor = path(fields.get("providerOptions"), &["cursor"])?;
    let context = path(cursor, &["requestContextCompleteness"])?;
    let injected =
        role == Role::System || (role == Role::User && context.map(|v| v.is_null()).transpose()? == Some(false));
    let summary = flag(path(cursor, &["isSummary"])?)?;
    let is_error = flag(path(cursor, &["highLevelToolCallResult", "isError"])?)?;
    let mut message = if injected {
        Message::of_kind(role, MessageKind::InjectedContext, Origin::Harness)
    } else if summary {
        Message::of_kind(role, MessageKind::CompactionSummary, Origin::Harness)
    } else {
        Message::new(role)
    };
    message.id = fields.string("id")?;
    let spans = demand.spans;
    let text = |view: Option<JsonView<'_>>| -> Result<Option<Text>> {
        Ok(match view {
            Some(view) => view.lazy_text(spans)?,
            None => None,
        })
    };
    let content = fields.get("content");
    if let Some(content) = text(content)? {
        if !content.is_empty() {
            message.content.push(Block::Text { text: content });
        }
    } else if let Some(parts) = content.map(|v| v.array()).transpose()?.flatten() {
        for part in parts {
            let part = part?;
            let Some(fields) = Fields::parse(
                part,
                &[
                    "type",
                    "text",
                    "signature",
                    "toolCallId",
                    "toolName",
                    "args",
                    "result",
                    "isError",
                    "mimeType",
                    "filename",
                ],
            )?
            else {
                continue;
            };
            let kind = fields.string("type")?.unwrap_or_default();
            let taken: &[&str] = match kind.as_str() {
                "text" => {
                    if let Some(text) = text(fields.get("text"))?.filter(|text| !text.is_empty()) {
                        message.content.push(Block::Text { text });
                    }
                    &["text"]
                }
                "reasoning" | "redacted-reasoning" => {
                    message.content.push(Block::Thinking {
                        text: text(fields.get("text"))?.unwrap_or_default(),
                        signature: fields.string("signature")?,
                        encrypted: None,
                        redacted: kind == "redacted-reasoning",
                    });
                    &["text", "signature"]
                }
                "tool-call" => {
                    message.content.push(Block::ToolUse {
                        id: fields.string("toolCallId")?.unwrap_or_default(),
                        name: fields.string("toolName")?.unwrap_or_else(|| "tool".into()),
                        input: match fields.get("args") {
                            Some(args) => args.argument(spans)?,
                            None => Value::Null.into(),
                        },
                        namespace: None,
                    });
                    &["toolCallId", "toolName", "args"]
                }
                "tool-result" => {
                    // Native results are strings; any other value keeps its compact JSON.
                    let content = match fields.get("result") {
                        None => Text::default(),
                        Some(result) => match result.lazy_text(spans)? {
                            Some(text) => text,
                            None if result.is_null()? => Text::default(),
                            None => result.resolve()?.to_string().into(),
                        },
                    };
                    message.content.push(Block::ToolResult {
                        tool_use_id: fields.string("toolCallId")?.unwrap_or_default(),
                        content,
                        is_error: is_error || flag(fields.get("isError"))?,
                        tool_name: fields.string("toolName")?,
                        status: None,
                        details: None,
                    });
                    &["result", "toolCallId", "toolName"]
                }
                "image" => {
                    message.content.push(Block::Image {
                        media_type: fields.string("mimeType")?,
                        data_ref: Some("inline:cursor-cli".into()),
                    });
                    &["mimeType"]
                }
                "file" => {
                    message.content.push(Block::File {
                        mime: fields.string("mimeType")?,
                        path: fields.string("filename")?,
                        source: None,
                    });
                    &["mimeType", "filename"]
                }
                _ => {
                    if demand.tier == Tier::Complete {
                        message
                            .harness_extra_mut(Harness::Cursor)
                            .entry("unhandled_parts")
                            .or_insert_with(|| Value::Array(Vec::new()))
                            .as_array_mut()
                            .context("unhandled_parts")?
                            .push(part.resolve()?.into_owned());
                    }
                    &[]
                }
            };
            if demand.tier == Tier::Rich {
                let rest = remainder(part, taken)?;
                if rest.len() > 1 {
                    message
                        .harness_extra_mut(Harness::Cursor)
                        .entry("part_metadata")
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut()
                        .context("part_metadata")?
                        .push(Value::Object(rest));
                }
            }
        }
    }
    if message.content.is_empty() {
        if demand.tier != Tier::Complete {
            return Ok(None);
        }
        message.kind = MessageKind::Carrier;
        message.origin = Origin::Harness;
    }
    if demand.tier >= Tier::Rich {
        let rest = remainder(view, &["id", "content", "role"])?;
        if !rest.is_empty() {
            message
                .harness_extra_mut(Harness::Cursor)
                .insert("record_metadata".into(), Value::Object(rest));
        }
    }
    if demand.tier == Tier::Complete {
        message
            .harness_extra_mut(Harness::Cursor)
            .insert("record".into(), view.resolve()?.into_owned());
    }
    Ok((!message.content.is_empty() || !message.extra.is_empty()).then_some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;
    use crate::stream::CollectSink;
    use serde_json::json;

    fn field(number: u32, bytes: &[u8]) -> Vec<u8> {
        fn v(mut n: u64, out: &mut Vec<u8>) {
            while n > 127 {
                out.push((n as u8 & 127) | 128);
                n >>= 7;
            }
            out.push(n as u8);
        }
        let mut out = Vec::new();
        v(u64::from(number) << 3 | 2, &mut out);
        v(bytes.len() as u64, &mut out);
        out.extend(bytes);
        out
    }
    fn fixture(root: Vec<u8>, messages: &[(u8, Vec<u8>)], dir: &Path) -> (Connection, SessionRef) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("store.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT);",
        )
        .unwrap();
        let metadata = json!({"agentId":"fixture", "latestRootBlobId":encode_hex(&[255;32]), "createdAt":1, "subagentInfo":{"parentAgentId":"parent","toolCallId":"spawn","typeName":"worker"}});
        conn.execute(
            "INSERT INTO meta VALUES('0',?1)",
            [encode_hex(metadata.to_string().as_bytes())],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blobs VALUES(?1,?2)",
            rusqlite::params![encode_hex(&[255; 32]), root],
        )
        .unwrap();
        for (id, message) in messages {
            conn.execute(
                "INSERT INTO blobs VALUES(?1,?2)",
                rusqlite::params![encode_hex(&[*id; 32]), message],
            )
            .unwrap();
        }
        (conn, SessionRef::new("cli:fixture", Harness::Cursor, path))
    }
    #[test]
    fn discovery_title_peels_timestamp_without_changing_prompt_text() {
        let world = crate::test_world("cursor-timestamp-title");
        let body = "<timestamp>Monday, Oct 12, 2026, 7:50 PM (UTC-4)</timestamp>\nask about otters";
        let record = json!({"role":"user","content":body}).to_string().into_bytes();
        let (conn, r) = fixture(field(1, &[1; 32]), &[(1, record)], &world.base);
        let mut count = Count::default();
        let session = read(&conn, &r, &crate::stream::Demand::RICH, true, &mut count).unwrap();
        assert_eq!(count.title.as_deref(), Some("ask about otters"));
        assert_eq!(count.count, 1);
        let _ = session;
        let message = decode(&json!({"role":"user","content":body}), false, false).unwrap();
        assert_eq!(message.text().as_deref(), Some(body));
    }

    #[test]
    fn native_capture_counts_and_tool_links_match_stream() {
        let home = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/cursor/2026.10.01-e373342/.cursor");
        let adapter = Cursor::with_cli_home(&home);
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].message_count, Some(5));
        let session = adapter.parse(&refs[0]).unwrap();
        assert_eq!(session.messages.len(), 10);
        assert_eq!(
            Some(session.messages.iter().filter(|m| m.is_turn()).count()),
            refs[0].message_count
        );
        let uses: Vec<_> = session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolUse { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        let results: Vec<_> = session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect();
        assert_eq!(uses, results);
        assert_eq!(uses.len(), 3);
        assert_eq!(session.model.as_deref(), Some("grok-4.5"));
        assert_eq!(session.harness_extra(Harness::Cursor).unwrap()["unified_mode"], "agent");
    }
    #[test]
    fn archives_precede_live_messages_and_unknown_mode_is_retained() {
        let world = crate::test_world("cursor-cli-archives");
        for archive_field in [11, 13] {
            let mut root = field(archive_field, &[1; 32]);
            root.extend(field(1, &[3; 32]));
            root.extend([80, 99]);
            let archive = field(1, &[2; 32]);
            let older = json!({"role":"user","content":"older"}).to_string().into_bytes();
            let newer = json!({"role":"assistant","content":[{"type":"text","text":"newer"}]})
                .to_string()
                .into_bytes();
            let (conn, r) = fixture(
                root,
                &[(1, archive), (2, older), (3, newer)],
                &world.base.join(archive_field.to_string()),
            );
            let mut sink = CollectSink::default();
            let session = read(&conn, &r, &crate::stream::Demand::LEAN, false, &mut sink).unwrap();
            assert_eq!(sink.messages.len(), 2);
            assert_eq!(sink.messages[0].text().as_deref(), Some("older"));
            assert_eq!(session.lineage.parent.as_deref(), Some("cli:parent"));
            assert_eq!(session.harness_extra(Harness::Cursor).unwrap()["cli_mode_raw"], 99);
        }
    }
    /// One record decoded from an owned value.
    fn decode(value: &Value, extra: bool, complete: bool) -> Option<Message> {
        let demand = match (extra, complete) {
            (_, true) => crate::stream::Demand::COMPLETE,
            (true, false) => crate::stream::Demand::RICH,
            (false, false) => crate::stream::Demand::LEAN,
        };
        message(JsonView::owned(value), demand).unwrap()
    }
    #[test]
    fn count_obeys_injected_and_summary_semantics_and_titles_the_first_prompt() {
        let world = crate::test_world("cursor-cli-count");
        let cases = [
            (
                json!({"role":"user","content":"injected","providerOptions":{"cursor":{"requestContextCompleteness":{}}}}),
                MessageKind::InjectedContext,
                false,
            ),
            (
                json!({"role":"user","content":"summary","providerOptions":{"cursor":{"isSummary":true}}}),
                MessageKind::CompactionSummary,
                false,
            ),
            (
                json!({"role":"assistant","content":[{"type":"tool-call","toolCallId":"call","toolName":"Read","args":{"large":"x".repeat(2_000_000)}}]}),
                MessageKind::Reply,
                true,
            ),
            (
                json!({"role":"tool","content":[{"type":"tool-result","toolCallId":"call","result":"x".repeat(2_000_000)}]}),
                MessageKind::for_role(Role::Tool),
                false,
            ),
            (json!({"role":"user","content":"   "}), MessageKind::Prompt, true),
        ];
        for (value, kind, turn) in &cases {
            let message = decode(value, false, false).unwrap();
            assert_eq!((message.kind, message.is_turn()), (*kind, *turn), "{value}");
        }
        let prompt = json!({"role":"user","content":[{"type":"text","text":"  first line"},{"type":"image"},{"type":"text","text":format!("\n{}", "y".repeat(200))}]});
        let root = (1..=3).flat_map(|id| field(1, &[id; 32])).collect();
        let records = [&cases[4].0, &prompt, &cases[2].0].map(|v| v.to_string().into_bytes());
        let records: Vec<_> = records
            .into_iter()
            .enumerate()
            .map(|(i, r)| ((i + 1) as u8, r))
            .collect();
        let (conn, r) = fixture(root, &records, &world.base);
        let mut count = Count::default();
        read(&conn, &r, &crate::stream::Demand::LEAN.with_spans(), true, &mut count).unwrap();
        assert_eq!(count.count, 3);
        // A blank first prompt titles nothing, so the next prompt titles the session.
        let expected = crate::ir::truncate(&format!("first line\n\n{}", "y".repeat(200)), 80);
        assert_eq!(count.title.as_deref(), Some(expected.as_str()));
        let only = Message {
            content: vec![
                Block::Text {
                    text: "  first line".into(),
                },
                Block::Text {
                    text: format!("\n{}", "y".repeat(200)).into(),
                },
            ],
            ..Message::new(Role::User)
        };
        assert_eq!(
            only.prompt_title(&crate::lazy::Resolver::new(None)).unwrap().as_deref(),
            Some(expected.as_str())
        );
        let empty = json!({"role":"assistant","content":[],"providerOptions":{"cursor":{"opaque":1}}});
        assert!(decode(&empty, true, false).is_none());
        assert!(!decode(&empty, true, true).unwrap().is_turn());
        let v = json!({"role":"assistant","content":[{"type":"reasoning","text":"thought","providerOptions":{"opaque":42}}]});
        assert_eq!(
            decode(&v, true, false).unwrap().harness_extra(Harness::Cursor).unwrap()["part_metadata"][0]
                ["providerOptions"]["opaque"],
            42
        );
        assert_eq!(
            decode(&v, true, true).unwrap().harness_extra(Harness::Cursor).unwrap()["record"],
            v
        );
    }
    #[test]
    fn metadata_bags_keep_an_owned_map_s_order_after_its_removals() {
        let v =
            json!({"role":"user","a":1,"id":"m","b":2,"content":[{"type":"text","x":1,"text":"t","y":2,"z":3}],"c":3});
        let extra = decode(&v, true, false).unwrap();
        let bag = extra.harness_extra(Harness::Cursor).unwrap();
        let mut owned = v.as_object().unwrap().clone();
        for key in ["id", "content", "role"] {
            owned.remove(key);
        }
        assert_eq!(
            serde_json::to_string(&bag["record_metadata"]).unwrap(),
            serde_json::to_string(&owned).unwrap()
        );
        let mut part = v["content"][0].as_object().unwrap().clone();
        part.remove("text");
        assert_eq!(
            serde_json::to_string(&bag["part_metadata"][0]).unwrap(),
            serde_json::to_string(&part).unwrap()
        );
    }
    #[test]
    fn malformed_wire_and_missing_blob_fail_while_stop_is_demand_bounded() {
        let world = crate::test_world("cursor-cli-malformed");
        for (index, root) in [vec![0], vec![10, 32, 1], vec![128; 11], field(1, &[1; 31])]
            .into_iter()
            .enumerate()
        {
            let (conn, r) = fixture(root, &[], &world.base.join(index.to_string()));
            assert!(read(&conn, &r, &crate::stream::Demand::LEAN, false, &mut Count::default()).is_err());
        }
        let (conn, r) = fixture(field(1, &[1; 32]), &[], &world.base.join("missing"));
        assert!(read(&conn, &r, &crate::stream::Demand::LEAN, false, &mut Count::default()).is_err());
        let mut root = field(1, &[1; 32]);
        root.extend(field(1, &[2; 32]));
        let (conn, r) = fixture(
            root,
            &[(1, br#"{"role":"user","content":"one"}"#.to_vec())],
            &world.base.join("stop"),
        );
        struct Stop;
        impl MessageSink for Stop {
            fn message(&mut self, _: Message) -> Flow {
                Flow::Stop
            }
        }
        assert!(read(&conn, &r, &crate::stream::Demand::LEAN, false, &mut Stop).is_ok());
    }
    #[test]
    fn acp_sidecar_creation_change_and_identity_invalidate() {
        let world = crate::test_world("cursor-acp");
        let dir = &world.base;
        std::fs::create_dir_all(dir).unwrap();
        let (conn, mut reference) = fixture(
            field(1, &[1; 32]),
            &[(1, br#"{"role":"user","content":"one"}"#.to_vec())],
            dir,
        );
        struct Reads(crate::source::ReadSet);
        impl MessageSink for Reads {
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
            fn dependency(&mut self, path: &Path) {
                self.0.record(path)
            }
        }
        let mut absent = Reads(crate::source::ReadSet::default());
        read(&conn, &reference, &crate::stream::Demand::LEAN, false, &mut absent).unwrap();
        assert!(absent.0.fresh(&mut crate::source::Probe::default()));
        std::fs::write(dir.join("meta.json"), r#"{"cwd":"/scratch/project","title":"ACP"}"#).unwrap();
        assert!(!absent.0.fresh(&mut crate::source::Probe::default()));
        let mut current = Reads(crate::source::ReadSet::default());
        let session = read(&conn, &reference, &crate::stream::Demand::LEAN, false, &mut current).unwrap();
        assert_eq!(session.cwd.as_deref(), Some(Path::new("/scratch/project")));
        assert_eq!(session.title.as_deref(), Some("ACP"));
        std::fs::write(
            dir.join("meta.json"),
            r#"{"cwd":"/scratch/project2","title":"changed title"}"#,
        )
        .unwrap();
        assert!(!current.0.fresh(&mut crate::source::Probe::default()));
        reference.id = "cli:replacement".into();
        assert!(read(
            &conn,
            &reference,
            &crate::stream::Demand::LEAN,
            false,
            &mut Count::default()
        )
        .is_err());
    }
    #[test]
    fn updated_time_observes_active_wal() {
        let world = crate::test_world("cursor-cli-wal-time");
        std::fs::create_dir_all(&world.base).unwrap();
        let (conn, reference) = fixture(Vec::new(), &[], &world.base);
        for (name, seconds) in [("store.db", 1), ("store.db-wal", 2)] {
            let file = std::fs::File::options()
                .write(true)
                .create(true)
                .truncate(false)
                .open(world.base.join(name))
                .unwrap();
            file.set_times(
                std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .unwrap();
        }
        let session = read(
            &conn,
            &reference,
            &crate::stream::Demand::LEAN,
            false,
            &mut Count::default(),
        )
        .unwrap();
        assert_eq!(session.updated_at.unwrap().timestamp(), 2);
    }
    #[test]
    fn cold_wal_adapter_preserves_original_sidecars_and_journal_guard() {
        let world = crate::test_world("cursor-cli-cold-wal");
        let record = json!({"role":"user","content":"日本語\n".repeat(4096)})
            .to_string()
            .into_bytes();
        let (conn, r) = fixture(field(1, &[1; 32]), &[(1, record)], &world.base.join("source"));
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        drop(conn);
        let wal = r.path.with_file_name("store.db-wal");
        let shm = r.path.with_file_name("store.db-shm");
        let journal = r.path.with_file_name("store.db-journal");
        assert!(!wal.exists() && !shm.exists());
        let adapter = Cursor::with_cli_home(&world.base);
        let retained = crate::stream::collect_with(&adapter, &r, &crate::stream::Demand::FACTS.with_spans()).unwrap();
        assert!(!wal.exists() && !shm.exists());
        let Block::Text {
            text: crate::lazy::Text::Source(text),
        } = &retained.messages[0].content[0]
        else {
            panic!("source text required")
        };
        std::fs::write(&journal, b"changed journal").unwrap();
        assert!(text.try_prefix(10).is_err());
        std::fs::remove_file(&journal).unwrap();
        struct Stop(PathBuf);
        impl MessageSink for Stop {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::write(&self.0, b"changed journal").unwrap();
                Flow::Stop
            }
        }
        assert!(adapter
            .stream(&r, &crate::stream::Demand::LEAN, &mut Stop(journal))
            .unwrap_err()
            .to_string()
            .contains("dependencies changed"));
        assert!(!wal.exists() && !shm.exists());
    }
    #[test]
    fn lazy_and_bulk_demands_resolve_alike_and_stage_the_same_dataset() {
        let world = crate::test_world("cursor-cli-selected");
        let big = "日本語\n".repeat(4096);
        let records = [
            json!({"role":"user","content":[{"type":"text","text":big}],"providerOptions":{"cursor":{"requestId":"request"}}}),
            json!({"role":"assistant","content":[{"type":"reasoning","text":big,"signature":"sig"},{"type":"tool-call","toolCallId":"call","toolName":"Shell","args":{"text":big,"small":7}}],"providerOptions":{"cursor":{"ignored":big}}}),
            json!({"role":"tool","content":[{"type":"tool-result","toolCallId":"call","toolName":"Shell","result":big,"experimental_content":[{"type":"text","text":big}]}],"providerOptions":{"cursor":{"highLevelToolCallResult":{"isError":true,"output":big}}}}),
        ];
        let root = (1..=3).flat_map(|id| field(1, &[id; 32])).collect();
        let records: Vec<_> = records
            .into_iter()
            .enumerate()
            .map(|(i, v)| ((i + 1) as u8, v.to_string().into_bytes()))
            .collect();
        let (_conn, reference) = fixture(root, &records, &world.base.join("source"));
        let adapter = Cursor::with_cli_home(&world.base);
        let owned = crate::stream::collect_with(&adapter, &reference, &crate::stream::Demand::LEAN).unwrap();
        let mut lazy =
            crate::stream::collect_with(&adapter, &reference, &crate::stream::Demand::LEAN.with_spans()).unwrap();
        let mut owned_output = Vec::new();
        let mut lazy_output = Vec::new();
        crate::dataset::write_record(&owned, &mut owned_output, crate::dataset::Format::Chatml, None).unwrap();
        let staged = crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            0,
            None,
            &crate::query::TextSets::default(),
        )
        .unwrap()
        .unwrap();
        staged.publish(&mut lazy_output).unwrap();
        lazy_output.pop(); // stage publication includes the record's newline
        assert_eq!(owned_output, lazy_output);
        // A filter retains deferred descriptors before rendering rather than eagerly demanding
        // payloads. This exercises the same checked text/JSON carriers through registry restore.
        let query = crate::SessionQuery::parse("harness:cursor").unwrap();
        let filtered = crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            0,
            Some(&query),
            &crate::query::TextSets::default(),
        )
        .unwrap()
        .unwrap();
        let mut filtered_output = Vec::new();
        filtered.publish(&mut filtered_output).unwrap();
        filtered_output.pop();
        assert_eq!(owned_output, filtered_output);
        lazy.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(&lazy.messages).unwrap(),
            serde_json::to_value(&owned.messages).unwrap()
        );
    }
    #[test]
    fn schema_edges_and_invalid_records_resolve_alike_across_demands() {
        let world = crate::test_world("cursor-cli-selected-edges");
        let cases = [
            r#"{"role":"user","content":"first","content":"last","providerOptions":{"cursor":{"requestContextCompleteness":false,"isSummary":true}}}"#,
            r#"{"role":"developer","content":[{"type":"text","text":""},{"type":"unknown","opaque":7},{"type":"redacted-reasoning","text":null},{"type":"image","mimeType":false},{"type":"file","filename":"a"}]}"#,
            r#"{"role":"assistant","content":[{"type":"tool-call","args":"{\"literal\":true}"},{"type":"tool-result","result":{"z":2,"a":1}},{"type":"tool-result","result":null},{"type":"reasoning","text":false}]}"#,
            r#"{"role":"user","content":"ok","ignored":"\uD800"}"#,
            r#"{"role":"user","content":"ok","ignored":1e9999}"#,
            r#"{"role":"user","content":"ok"} garbage"#,
        ];
        for (index, record) in cases.into_iter().enumerate() {
            let (conn, reference) = fixture(
                field(1, &[1; 32]),
                &[(1, record.as_bytes().to_vec())],
                &world.base.join(index.to_string()),
            );
            let mut owned = CollectSink::default();
            let mut selected = CollectSink::default();
            let full = read(&conn, &reference, &crate::stream::Demand::LEAN, false, &mut owned);
            let lazy = read(
                &conn,
                &reference,
                &crate::stream::Demand::LEAN.with_spans(),
                false,
                &mut selected,
            );
            assert_eq!(full.is_ok(), lazy.is_ok(), "case {index}");
            if full.is_ok() {
                let resolver = crate::lazy::Resolver::new(Some(reference.path.clone()));
                for message in &mut selected.messages {
                    message.materialize(&resolver).unwrap();
                }
                assert_eq!(
                    serde_json::to_value(owned.messages).unwrap(),
                    serde_json::to_value(selected.messages).unwrap(),
                    "case {index}"
                );
            }
        }
    }
    #[test]
    fn facts_preserve_doctor_compaction_affordance_and_session_facts() {
        let world = crate::test_world("cursor-cli-facts");
        let big = "日本語\n".repeat(8192);
        let values = [
            json!({"role":"user","content":"question"}),
            json!({"role":"user","content":big,"providerOptions":{"cursor":{"requestContextCompleteness":false,"ignoredPayload":big}}}),
            json!({"role":"assistant","content":[{"type":"reasoning","text":"reason"},{"type":"tool-call","toolCallId":"call","toolName":"Read","args":{"file_path":"/scratch/file","content":big}}],"providerOptions":{"cursor":{"systemPromptFingerprint":{"model":"native-model"},"ignoredPayload":big}}}),
            json!({"role":"tool","content":[{"type":"tool-result","toolCallId":"call","toolName":"Read","result":big,"experimental_content":[{"type":"text","text":big}]}],"providerOptions":{"cursor":{"highLevelToolCallResult":{"isError":true,"output":big}}}}),
            json!({"role":"assistant","content":"native summary","providerOptions":{"cursor":{"isSummary":true,"ignoredPayload":big}}}),
        ];
        let root = (1..=5).flat_map(|id| field(1, &[id; 32])).chain([80, 1]).collect();
        let records: Vec<_> = values
            .into_iter()
            .enumerate()
            .map(|(i, value)| ((i + 1) as u8, value.to_string().into_bytes()))
            .collect();
        let (conn, reference) = fixture(root, &records, &world.base.join("source"));
        let mut reports = Vec::new();
        let mut sessions = Vec::new();
        let mut counts = Vec::new();
        for options in [
            crate::stream::Demand::RICH.with_spans(),
            crate::stream::Demand::FACTS.with_spans(),
        ] {
            let mut report = crate::doctor::Report::default();
            let mut pass = report.session(Some(reference.path.clone()));
            let session = read(&conn, &reference, &options, false, &mut pass).unwrap();
            pass.finish().unwrap();
            reports.push(report.to_json());
            let mut retained = CollectSink::default();
            read(&conn, &reference, &options, false, &mut retained).unwrap();
            assert_eq!(retained.messages.last().unwrap().kind, MessageKind::CompactionSummary);
            let mut counter = crate::affordance::Counter::new(&reference);
            for message in &retained.messages {
                counter.note(message).unwrap();
            }
            counts.push(counter.finish(&session, &[]));
            if options.tier == Tier::Facts {
                assert!(retained.messages.iter().all(|message| message.extra.is_empty()));
                assert!(matches!(
                    &retained.messages[1].content[0],
                    Block::Text {
                        text: crate::lazy::Text::Source(_)
                    }
                ));
            } else {
                assert!(retained.messages[1]
                    .harness_extra(Harness::Cursor)
                    .unwrap()
                    .contains_key("record_metadata"));
            }
            sessions.push(session);
        }
        assert_eq!(reports[0], reports[1]);
        // Native summaries are summary messages, not inferred compaction boundaries.
        assert_eq!(reports[0]["compactions"], 0);
        assert_eq!(counts[0], counts[1]);
        assert_eq!(sessions[0].model, sessions[1].model);
        assert_eq!(sessions[0].extra, sessions[1].extra);
        let complete = crate::stream::Demand::COMPLETE.with_spans();
        let mut retained = CollectSink::default();
        read(&conn, &reference, &complete, false, &mut retained).unwrap();
        assert!(retained.messages[1]
            .harness_extra(Harness::Cursor)
            .unwrap()
            .contains_key("record"));
    }
    #[test]
    fn wal_guard_survives_lazy_blob_arguments_and_intentional_stop() {
        let world = crate::test_world("cursor-cli-guard");
        let record=json!({"role":"assistant","content":[{"type":"tool-call","toolCallId":"call","toolName":"Read","args":{"body":"x".repeat(8192)}},{"type":"text","text":"日本語\n".repeat(4096)}]}).to_string().into_bytes();
        let (conn, reference) = fixture(field(1, &[1; 32]), &[(1, record)], &world.base.join("source"));
        let mut retained = CollectSink::default();
        read(
            &conn,
            &reference,
            &crate::stream::Demand::LEAN.with_spans(),
            false,
            &mut retained,
        )
        .unwrap();
        let input = match &retained.messages[0].content[0] {
            Block::ToolUse { input, .. } => input,
            _ => panic!("expected tool"),
        };
        assert!(matches!(input, crate::json::Json::Source(_)));
        let text = match &retained.messages[0].content[1] {
            Block::Text {
                text: crate::lazy::Text::Source(text),
            } => text,
            _ => panic!("expected checked source text"),
        };
        assert_eq!(text.try_decoded_len().unwrap(), "日本語\n".len() * 4096);
        let adapter = Cursor::with_cli_home(&world.base);
        let staged = crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            0,
            None,
            &crate::query::TextSets::default(),
        )
        .unwrap()
        .unwrap();
        let wal = reference.path.with_file_name("store.db-wal");
        std::fs::write(&wal, b"new WAL identity").unwrap();
        assert!(input.canonical_len().is_err());
        assert!(text.try_prefix(10).is_err());
        assert!(text.try_decoded_len().is_err());
        let mut output = Vec::new();
        // Staging has already demanded and validated every field. These finalized private
        // bytes represent the accepted snapshot and remain publishable after it changes.
        staged.publish(&mut output).unwrap();
        assert!(!output.is_empty());
        let resolver = crate::lazy::Resolver::new(Some(reference.path.clone()));
        assert!(retained.messages[0].materialize(&resolver).is_err());
        std::fs::remove_file(&wal).unwrap();
        struct Mutate(PathBuf);
        impl MessageSink for Mutate {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::write(&self.0, b"new WAL identity").unwrap();
                Flow::Stop
            }
        }
        let error = read(
            &conn,
            &reference,
            &crate::stream::Demand::LEAN.with_spans(),
            false,
            &mut Mutate(wal),
        )
        .unwrap_err();
        assert!(error.to_string().contains("dependencies changed"), "{error:#}");
    }
}
