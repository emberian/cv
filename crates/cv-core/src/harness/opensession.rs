//! **OpenSession** — cv's own interchange format (`docs/OPENSESSION.md`), written by
//! `cv export --format json` and read back in here.
//!
//! OpenSession 0.3 *is* the IR: same keys, same snake_case, blocks tagged `type`, messages
//! carrying `kind`/`origin`. So the writer ([`document`]) is the IR's own serialization plus one
//! version marker, and the reader below is its inverse — which is the only way the spec's central
//! claim ("clustervision's in-memory IR is the reference implementation") can be checked rather
//! than asserted.
//!
//! **Discovery.** A document has no fixed home, so — exactly like the account-export adapters —
//! it is found through the sources the user registers with `cv config --add-export <path>`
//! (unioned with `$CV_EXPORTS`). Under each source (depth ≤ 2) a file counts as a document when
//! it is named `*.opensession.json`, or when it is a `*.json` whose head carries an
//! `open_session` / `openSession` key. **Opt-in**: with nothing registered, discovery is a no-op.
//!
//! **One version.** The reader is the writer run backwards: check and strip the `open_session`
//! marker, hand the rest to the IR's own `Deserialize`. A 0.3 document round-trips byte-identical
//! through [`document`] → [`from_str`] → [`document`]. A 0.2 document (`openSession`, camelCase,
//! `kind`-tagged blocks — written by cv ≤ 0.10) is refused with a pointer to the renames in
//! `docs/OPENSESSION.md`; so is any other version. The ~450-line canonicalizer that used to accept
//! 0.2 spellings beside 0.3 ones was a second, hand-written copy of the IR's vocabulary, and the
//! clean break (`docs/INTERFACE-V2.md` §3: no aliases for old names) applies to documents too.
//!
//! The one rule beyond deserialization: a document that names no harness, or one this build has
//! never heard of, is an `opensession` session (what [`Harness::OpenSession`] exists for) rather
//! than a parse failure — the document is still a session, it just came from somewhere cv cannot
//! name.

use super::store::document::Fields;
use super::{ts_from_value, Adapter};
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The OpenSession version cv writes — the one place it is spelled, so `docs/OPENSESSION.md`,
/// the export and the reader cannot drift apart.
///
/// It lives at the **document boundary** ([`document`]), not as a field on
/// [`Session`](crate::ir::Session): a `Session` is cv's in-memory IR and is serialized raw by
/// `cv show --json`, the MCP payloads and `cv serve`, whose key sets are pinned by tests. A version
/// marker is a fact about a *document* someone wrote to disk or to a wire, not about a value in
/// memory — so it is added exactly where a document is produced, and stripped (ignored) where one
/// is read.
pub const VERSION: &str = "0.3";

/// The document's version key, and the 0.2 spelling — recognized only so a 0.2 document is
/// discovered and refused with a pointer, instead of silently not being a document.
const VERSION_KEY: &str = "open_session";
const VERSION_KEY_V02: &str = "openSession";

// Projection vocabularies are pinned against the authoritative deserializer in tests. Nested
// rich Values preserve their source order; ignored typed fields never need materialization.
const SESSION_FIELDS: &[&str] = &[
    VERSION_KEY,
    VERSION_KEY_V02,
    "id",
    "harness",
    "principal",
    "cwd",
    "title",
    "created_at",
    "updated_at",
    "model",
    "system_prompt",
    "source_path",
    "extra",
];
const MESSAGE_FIELDS: &[&str] = &[
    "id",
    "parent_id",
    "role",
    "kind",
    "origin",
    "timestamp",
    "model",
    "extra",
];
fn block_projection(ty: Option<&str>) -> (&'static [&'static str], Option<&'static str>) {
    match ty {
        Some("text") => (&["type"], Some("text")),
        Some("thinking") => (&["type", "signature", "encrypted", "redacted"], Some("text")),
        Some("tool_use") => (&["type", "id", "name", "input", "namespace"], None),
        Some("tool_result") => (
            &["type", "tool_use_id", "is_error", "tool_name", "status", "details"],
            Some("content"),
        ),
        Some("image") => (&["type", "media_type", "data_ref"], None),
        Some("file") => (&["type", "mime", "path", "source"], None),
        _ => (&["type"], None),
    }
}

// ── Writing: the document boundary ────────────────────────────────────────────

/// A session as an OpenSession document: the IR's own serialization, with `open_session` first.
/// Produced by [`document`]; see [`VERSION`] for why the marker lives here and not on `Session`.
#[derive(Serialize)]
struct Envelope<'a> {
    open_session: &'static str,
    #[serde(flatten)]
    session: &'a Session,
}

pub struct Document<'a>(&'a Session);

pub(crate) fn opaque_message(message: &Message) -> Option<&Value> {
    (message.kind == MessageKind::Carrier)
        .then(|| message.harness_extra(Harness::OpenSession)?.get("message"))
        .flatten()
}

impl Serialize for Document<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let envelope = Envelope {
            open_session: VERSION,
            session: self.0,
        };
        if self.0.messages.iter().all(|m| opaque_message(m).is_none()) {
            return envelope.serialize(serializer);
        }
        let mut document = serde_json::to_value(envelope).map_err(serde::ser::Error::custom)?;
        for (wire, message) in document["messages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(&self.0.messages)
        {
            if let Some(original) = opaque_message(message) {
                wire.clone_from(original);
            }
        }
        document.serialize(serializer)
    }
}

/// Wrap `session` as an OpenSession [`Document`] for serialization. The session should be
/// materialized (`Session::materialize`, which `Adapter::parse` already does) so its content
/// serializes as text rather than as lazy spans into a file the reader will not have.
pub fn document(session: &Session) -> Document<'_> {
    Document(session)
}

// ── The adapter ───────────────────────────────────────────────────────────────

pub struct OpenSession;

impl Adapter for OpenSession {
    fn harness(&self) -> Harness {
        Harness::OpenSession
    }

    /// A document is one whole JSON file.
    fn native_custody(&self) -> Option<super::NativeCustody> {
        Some(super::NativeCustody::Document)
    }

    fn roots(&self) -> Vec<PathBuf> {
        super::export::export_dirs()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        Ok(discover_documents(super::export::export_dirs()))
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| stream_document(&r.path, opts, sink))
    }

    /// A `.cvbundle` is an OpenSession document in a sealed pack: it opens by its path.
    fn resolve_path(&self, path: &Path) -> Option<(SessionRef, Box<dyn Adapter>)> {
        bundle_ref(path).map(|r| (r, Box::new(OpenSession) as Box<dyn Adapter>))
    }

    /// An OpenSession document is the IR's own serialization: a selection is written whole.
    fn writes_selection(&self) -> bool {
        true
    }

    /// Written into `out`, else the first registered export source, staging any `.flat.jsonl`
    /// sidecar before publishing the document, then read back.
    fn write_selection(
        &self,
        _source: &Path,
        selected: &Session,
        out: Option<&Path>,
        new_id: &str,
        sidecar: &[crate::distill::Elided],
    ) -> Result<(PathBuf, Option<PathBuf>, Vec<String>)> {
        crate::publish::validate_session_id(new_id)?;
        let out_dir = match out {
            Some(dir) => dir.to_path_buf(),
            None => self
                .roots()
                .into_iter()
                .next()
                .context("opensession doesn't appear installed; pass --out <dir> to write somewhere")?,
        };
        let mut owned = selected.clone();
        owned.materialize()?;
        owned.id = new_id.to_string();
        std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
        let path = out_dir.join(format!("{new_id}.opensession.json"));
        let sidecar_path = (!sidecar.is_empty()).then(|| out_dir.join(format!("{new_id}.flat.jsonl")));
        let bytes = serde_json::to_vec_pretty(&document(&owned))?;
        let mut set = crate::publish::Set::new();
        if let Some(sc_path) = &sidecar_path {
            let staged_sc = crate::distill::stage_sidecar(sc_path, sidecar)?;
            set.add(staged_sc, sc_path, crate::publish::Claim::New);
        }
        set.stage(&path, "opensession", &bytes[..], crate::publish::Claim::New)?;
        set.publish()?;
        let back = read_document(&path)?;
        let mut lost = Vec::new();
        if back.messages.len() != owned.messages.len() {
            lost.push(format!(
                "messages: {} → {} (read back)",
                owned.messages.len(),
                back.messages.len()
            ));
        }
        if back.id != new_id {
            lost.push(format!("id: {new_id} → {} (read back)", back.id));
        }
        Ok((path, sidecar_path, lost))
    }
}

/// The [`SessionRef`] of a `.cvbundle` at `path`, `None` when it is not one.
fn bundle_ref(path: &Path) -> Option<SessionRef> {
    if !crate::bundle::is_bundle(path) {
        return None;
    }
    let manifest = crate::bundle::Bundle::open(path).ok()?.manifest;
    Some(SessionRef {
        message_count: Some(manifest.messages),
        ..SessionRef::new(manifest.session_id, Harness::OpenSession, path.to_owned())
    })
}

impl OpenSession {
    /// Discover only the supplied export files/directories, without consulting configuration.
    pub fn with_roots(roots: Vec<PathBuf>) -> impl Adapter {
        RootedOpenSession { roots }
    }
}
struct RootedOpenSession {
    roots: Vec<PathBuf>,
}
impl Adapter for RootedOpenSession {
    fn harness(&self) -> Harness {
        Harness::OpenSession
    }
    fn native_custody(&self) -> Option<super::NativeCustody> {
        Some(super::NativeCustody::Document)
    }
    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }
    fn discover(&self) -> Result<crate::Discovered> {
        Ok(discover_documents(self.roots.clone()))
    }
    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| stream_document(&r.path, opts, sink))
    }
}

/// Every OpenSession document under `roots`: an unreadable dir, and a document that cannot be
/// read, does not decode or names no `id`, is a reported skip.
fn discover_documents(roots: Vec<PathBuf>) -> crate::Discovered {
    let mut out = crate::Discovered::default();
    let paths = super::export::files_under(roots, is_document_file, &mut out.skipped);
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        // A document's ref is its own bytes': rescanned only when the file moves.
        let scanned = crate::discover_cache::cached_scan_checked(&path, || doc_ref(&path).map(Some))
            .and_then(|r| r.context("a document scan names its session"));
        if let Some(r) = out.keep(&path, scanned) {
            if seen.insert(r.id.clone()) {
                out.refs.push(r);
            }
        }
    }
    out
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Is this file an OpenSession document? By name (`*.opensession.json`), or for a plain `*.json`
/// by [`names_version_key`].
fn is_document_file(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.ends_with(".opensession.json") {
        return true;
    }
    if !name.ends_with(".json") {
        return false;
    }
    std::fs::File::open(p).is_ok_and(|file| names_version_key(std::io::BufReader::new(file)))
}

/// Does this JSON text's top-level object have the version key (or the 0.2 spelling)? A streaming
/// pass over the top-level keys, each value skipped unread, with no byte cap: it stops at the key
/// (cv writes it first), and rejects at the first structure an OpenSession document cannot have
/// (a top level that is not an object, or broken JSON before the key). Nested keys do not count.
fn names_version_key(reader: impl std::io::Read) -> bool {
    use serde::de::{Deserializer, Error, IgnoredAny, MapAccess, Visitor};
    struct TopLevel<'a>(&'a std::cell::Cell<bool>);
    impl<'de> Visitor<'de> for TopLevel<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an OpenSession document object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                if key == VERSION_KEY || key == VERSION_KEY_V02 {
                    // Found: stop here rather than read the rest of the document.
                    self.0.set(true);
                    return Err(A::Error::custom("version key found"));
                }
                map.next_value::<IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let found = std::cell::Cell::new(false);
    let _ = serde_json::Deserializer::from_reader(reader).deserialize_any(TopLevel(&found));
    found.get()
}

/// A lightweight [`SessionRef`] for one document: metadata only, counting conversational turns
/// (user + assistant, the cross-harness contract) straight off the raw records. A document with
/// no `id` is not a session cv can name: an error, as is one that cannot be read or decoded.
#[cfg(test)]
static SCANNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn doc_ref(path: &Path) -> Result<SessionRef> {
    #[cfg(test)]
    SCANNED.lock().unwrap().push(path.to_path_buf());
    JsonSource::capture(path)?.read(ReadInput::Document, Validation::Value, |view| {
        let record = Fields::parse(view, &["id", "cwd", "title", "created_at", "updated_at", "messages"])?
            .context("document object")?;
        let o = record.project(&["id", "cwd", "title", "created_at", "updated_at"])?;
        let string = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        let mut message_count = 0;
        if let Some(messages) = record.get("messages").map(JsonView::array).transpose()?.flatten() {
            for message in messages {
                if let Some(message) = Fields::parse(message?, &["role"])? {
                    if message
                        .string("role")?
                        .is_some_and(|role| matches!(role.as_str(), "user" | "assistant"))
                    {
                        message_count += 1;
                    }
                }
            }
        }
        Ok(SessionRef {
            cwd: string("cwd").map(PathBuf::from),
            title: string("title").map(str::to_owned),
            created_at: o.get("created_at").and_then(ts_from_value),
            updated_at: o.get("updated_at").and_then(ts_from_value),
            message_count: Some(message_count),
            ..SessionRef::new(
                string("id").context("document id")?.to_owned(),
                Harness::OpenSession,
                path.to_owned(),
            )
        })
    })
}

// ── Reading a document ────────────────────────────────────────────────────────

/// One whole rich read, collected into a `Session`.
fn collect(read: impl FnOnce(&mut dyn MessageSink) -> Result<Session>) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut session = read(&mut sink)?;
    session.messages = sink.messages;
    Ok(session)
}

/// Read one OpenSession document into the IR. The inverse of [`document`].
pub fn read_document(path: &Path) -> Result<Session> {
    collect(|sink| stream_document(path, &Demand::RICH, sink))
}

/// Traverse the document's IR messages without retaining a complete Session.
fn stream_document(path: &Path, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    if crate::bundle::is_bundle(path) {
        let document = crate::bundle::Bundle::open(path)?.document()?;
        let v: Value = serde_json::from_slice(&document).context("bundled OpenSession document")?;
        return read((&v).into(), None, opts, sink);
    }
    if let Some(stand_in) = crate::harness::store::jsonl::substitute(path) {
        let source = stand_in.source;
        let len = stand_in.bytes.byte_len()?;
        anyhow::ensure!(len != 0, "an OpenSession document is a JSON object");
        return source.read(ReadInput::Backing(stand_in.bytes, 0, len), Validation::Value, |view| {
            read(view, Some((path, &source)), opts, sink)
        });
    }
    let source = JsonSource::capture(path)?;
    source.read(ReadInput::Document, Validation::Value, |view| {
        read(view, Some((path, &source)), opts, sink)
    })
}

/// The document codec, the only decoder of this format: the session envelope through the IR's own
/// `Deserialize`, messages and blocks projected so text follows [`JsonView::lazy_text`]. Shape
/// validation finishes before the first callback, so a document is all-or-error without retaining
/// its decoded history. `file` is the document and its capture, which a source-backed message is
/// rechecked against; None for an owned view.
fn read(
    view: JsonView<'_>,
    file: Option<(&Path, &JsonSource)>,
    opts: &Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let keys = SESSION_FIELDS
        .iter()
        .copied()
        .chain(["git", "lineage", "messages"])
        .collect::<Vec<_>>();
    let record = Fields::parse(view, &keys)?.context("an OpenSession document is a JSON object")?;
    let mut meta = record.project(SESSION_FIELDS)?;
    meta.extend(record.project(&["git", "lineage"])?);
    meta.insert("messages".into(), Value::Array(Vec::new()));
    let mut session = from_object(meta)?;
    let messages = record
        .get("messages")
        .context("not an OpenSession 0.3 document: messages must be an array")?;
    anyhow::ensure!(
        messages.is_array()?,
        "not an OpenSession 0.3 document: messages must be an array"
    );
    session.source_path = file.map(|(path, _)| path.to_path_buf());
    // Validate and discard one turn at a time before publishing even metadata. Keep payloads
    // lazy in this pass regardless of delivery demand; only their shape needs validation.
    let mut coverage = crate::coverage::ReadCoverage::of_session(&session).unwrap_or_default();
    let mut opaque = false;
    for view in messages.array()?.unwrap() {
        let message = ir_message(view?, true)?;
        if opaque_message(&message).is_some() {
            coverage.limitations.insert("opaque_message_kind".into());
            opaque = true;
        }
        if message.content.iter().any(|b| matches!(b, Block::Opaque { .. })) {
            coverage.limitations.insert("opaque_block_type".into());
            opaque = true;
        }
    }
    if opaque {
        coverage
            .sources
            .insert(crate::coverage::ReadCoverage::EXPORTED_HISTORY.into());
        coverage.attach(&mut session);
    }
    sink.meta(&session);
    for view in messages.array()?.unwrap() {
        let message = ir_message(view?, opts.spans)?;
        // Timed only against a file that can change underneath: an in-memory document has
        // none, and wasm32 has no clock to read.
        let started = file.map(|_| std::time::Instant::now());
        let flow = sink.try_message(message)?;
        if let Some(((_, source), started)) = file.zip(started) {
            source.check_after(started)?;
        }
        if flow == Flow::Stop {
            break;
        }
    }
    Ok(session)
}

fn ir_message(view: JsonView<'_>, spans: bool) -> Result<Message> {
    let keys = MESSAGE_FIELDS
        .iter()
        .copied()
        .chain(["usage", "content"])
        .collect::<Vec<_>>();
    let Some(record) = Fields::parse(view, &keys)? else {
        return serde_json::from_value(view.resolve()?.into_owned()).context("not an OpenSession message");
    };
    if let Some(kind) = record.string("kind")? {
        if !MessageKind::ALL.iter().any(|known| known.as_str() == kind) {
            let mut message = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Import);
            message.id = record.string("id")?;
            message
                .harness_extra_mut(Harness::OpenSession)
                .insert("message".into(), view.resolve()?.into_owned());
            return Ok(message);
        }
    }
    let mut fields = record.project(MESSAGE_FIELDS)?;
    fields.extend(record.project(&["usage"])?);
    fields.extend(extensions(view, &keys)?);
    let content = if let Some(content) = record.get("content") {
        if let Some(blocks) = content.array()? {
            blocks.map(|view| ir_block(view?, spans)).collect::<Result<Vec<_>>>()?
        } else {
            serde_json::from_value(content.resolve()?.into_owned())?
        }
    } else {
        Vec::new()
    };
    let mut message: Message = serde_json::from_value(Value::Object(fields)).context("not an OpenSession message")?;
    message.content = content;
    Ok(message)
}

fn extensions(view: JsonView<'_>, known: &[&str]) -> Result<serde_json::Map<String, Value>> {
    let mut fields = serde_json::Map::new();
    view.for_each_entry(|key, value| {
        if !known.contains(&key) {
            fields.insert(key.to_owned(), value.resolve()?.into_owned());
        }
        Ok(())
    })?;
    Ok(fields)
}

fn ir_block(view: JsonView<'_>, spans: bool) -> Result<Block> {
    let Some(head) = Fields::parse(view, &["type"])? else {
        return serde_json::from_value(view.resolve()?.into_owned()).context("not an OpenSession block");
    };
    let ty = head.string("type")?;
    let (keys, text_key) = block_projection(ty.as_deref());
    if let Some(block_type) = ty.as_ref().filter(|_| keys == ["type"] && text_key.is_none()) {
        return Ok(Block::Opaque {
            block_type: block_type.clone(),
            value: view.resolve()?.into_owned(),
        });
    }
    let selected = keys.iter().copied().chain(text_key).collect::<Vec<_>>();
    let record = Fields::parse(view, &selected)?.unwrap();
    if ty.as_deref() == Some("tool_use") {
        // Every field is present in the constructor, so a new IR field requires an explicit
        // reader decision. The schema roster gate also detects serde rename/alias changes.
        return Ok(Block::ToolUse {
            id: serde_json::from_value(record.value("id")?.context("missing field id")?)?,
            name: serde_json::from_value(record.value("name")?.context("missing field name")?)?,
            input: record.get("input").context("missing field input")?.argument(spans)?,
            namespace: record
                .value("namespace")?
                .map(serde_json::from_value)
                .transpose()?
                .flatten(),
            extensions: extensions(view, &selected)?,
        });
    }
    let mut fields = record.project(keys)?;
    fields.extend(extensions(view, &selected)?);
    let mut selected_text = None;
    if let Some(key) = text_key {
        if let Some(view) = record.get(key) {
            let value = if let Some(text) = view.lazy_text(spans)? {
                // Keep the authoritative Block deserializer for metadata/required fields,
                // without serializing a Source leaf back into an owned payload first.
                selected_text = Some(text);
                Value::String(String::new())
            } else {
                view.resolve()?.into_owned()
            };
            fields.insert(key.into(), value);
        }
    }
    let mut block: Block = serde_json::from_value(Value::Object(fields)).context("not an OpenSession block")?;
    if let Some(selected) = selected_text {
        match &mut block {
            Block::Text { text, .. } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => {
                *text = selected
            }
            _ => unreachable!("text projection only selects text-bearing block variants"),
        }
    }
    Ok(block)
}

/// Read an OpenSession document from text through the same codec as a file, over an owned view.
/// A document with no marker is read as a bare session (what `cv show --json` prints); a marker
/// other than [`VERSION`] is an error that names the version and where the renames are documented.
pub fn from_str(text: &str) -> Result<Session> {
    let v: Value = serde_json::from_str(text).context("not JSON")?;
    collect(|sink| read((&v).into(), None, &Demand::RICH, sink))
}

fn from_object(mut o: serde_json::Map<String, Value>) -> Result<Session> {
    match o.remove(VERSION_KEY) {
        None if !o.contains_key(VERSION_KEY_V02) => {}
        Some(Value::String(v)) if v == VERSION => {}
        other => {
            let found = match other {
                Some(v) => v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
                None => o[VERSION_KEY_V02]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| "0.2".to_string()),
            };
            bail!(
                "OpenSession {found} document; cv reads {VERSION} only — re-export the session with \
                 `cv export --format json`, or apply the renames in docs/OPENSESSION.md \"0.2 → 0.3\""
            );
        }
    }
    // No harness, or one this build cannot name ⇒ an `opensession` session, not a failure.
    let named = o.get("harness").and_then(Value::as_str).and_then(Harness::parse);
    if named.is_none() {
        o.insert("harness".into(), Value::String(Harness::OpenSession.as_str().into()));
    }
    serde_json::from_value::<Session>(Value::Object(o)).context("not an OpenSession 0.3 document")
}

#[cfg(test)]
mod tests;
