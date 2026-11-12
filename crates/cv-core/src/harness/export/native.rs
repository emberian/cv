//! Archive metadata and source ranges, never cached transcript bodies or source mappings.
use super::*;
use crate::harness::store::document::{text_summary, Fields};
use crate::json::{InvalidJson, JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::lazy::{Resolver, Text};
use crate::stream::Flow;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

const CACHE_ENTRIES: usize = 8;
const CACHE_BYTES: usize = 8 * 1024 * 1024;

struct Entry {
    id: String,
    offset: u64,
    len: u64,
    refs: [Option<SessionRef>; 2],
}
struct Archive {
    source: JsonSource,
    entries: Vec<Entry>,
    // Positions sorted by id then original position; discovery keeps archive order.
    by_id: crate::harness::store::selection::Positions,
    charge: usize,
    /// The file is not a JSON archive: read as none, reported by discovery.
    undecodable: bool,
}
impl Archive {
    fn entry(&self, id: &str) -> Option<&Entry> {
        self.by_id
            .equal(|i| self.entries[i].id.as_str().cmp(id))
            .first()
            .map(|&i| &self.entries[i])
    }
}
fn entry_index(entries: &[Entry]) -> crate::harness::store::selection::Positions {
    crate::harness::store::selection::Positions::new(entries.len(), |a, b| entries[a].id.cmp(&entries[b].id))
}

struct Cached {
    path: PathBuf,
    archive: Arc<Archive>,
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<Cached>,
    bytes: usize,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn slot(kind: Kind) -> usize {
    usize::from(kind == Kind::Claude)
}

/// A cache hit validates the file identity and carries only metadata. An uncached source is
/// retained through the current consumer so a cold selected read need not open the archive twice.
fn load(path: &Path, selected: Option<&str>) -> Result<Arc<Archive>> {
    // Leave room for Vec growth and the current metadata record while building a selected
    // lookup. Discovery's output is itself an unbounded list, but selected reads need no roster.
    load_bounded(
        path,
        selected,
        if selected.is_some() {
            CACHE_BYTES / 2
        } else {
            CACHE_BYTES
        },
    )
}
fn load_bounded(path: &Path, selected: Option<&str>, admission_bytes: usize) -> Result<Arc<Archive>> {
    let cache = CACHE.get_or_init(Mutex::default);
    // Relative roots can name different archives after a cwd change. Cache by absolute spelling,
    // while discovery stamps the caller's original path spelling onto returned refs.
    let key = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for _ in 0..2 {
        // Taken before the capture: an offset index published below describes this state.
        let state = offsets_state(&key);
        let source = JsonSource::capture(path)?;
        {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(position) = guard.entries.iter().position(|e| e.path == key) {
                let hit = guard.entries.remove(position).unwrap();
                if hit.archive.source == source {
                    let archive = hit.archive.clone();
                    guard.entries.push_back(hit);
                    return Ok(archive);
                }
                guard.bytes -= hit.archive.charge;
            }
        }
        // An archive past the roster cache: its offset index, when one describes this state.
        if let Some(id) = selected {
            if let Some(offsets) = offsets_record(&key, &state) {
                let entries: Vec<Entry> = offsets
                    .payload
                    .0
                    .get(id)
                    .map(|&(offset, len)| Entry {
                        id: id.to_owned(),
                        offset,
                        len,
                        refs: [None, None],
                    })
                    .into_iter()
                    .collect();
                let by_id = entry_index(&entries);
                return Ok(Arc::new(Archive {
                    source,
                    entries,
                    by_id,
                    charge: 0,
                    undecodable: false,
                }));
            }
        }
        // Every conversation's first accepted range, for a selected read's index.
        let mut offsets: HashMap<String, (u64, u64)> = HashMap::new();
        let mut entries = Vec::new();
        let mut selected_entry = None;
        let mut payload_charge = key.capacity().saturating_add(source.metadata_bytes());
        let mut charge = payload_charge;
        let mut cacheable = true;
        #[cfg(test)]
        SCANS.lock().unwrap().push(key.clone());
        let scanned = source.read(ReadInput::Document, Validation::Syntax, |values| {
            let values = values.accepted_array()?;
            let mut allow_discovery = true;
            for (position, view) in values.into_iter().flatten().enumerate() {
                let Some(view) = view? else {
                    if position == 0 {
                        allow_discovery = false;
                    }
                    continue;
                };
                let Some(record) = Fields::parse(view, CONVERSATION_FIELDS)? else {
                    continue;
                };
                // Sniff each valid element individually so a metadata/stub first element or mixed export
                // never blinds discovery for subsequent conversations in the archive.
                let elem_shapes = [record.get("mapping").is_some(), record.get("chat_messages").is_some()];
                let Some(id) = record
                    .get("conversation_id")
                    .or_else(|| record.get("id"))
                    .or_else(|| record.get("uuid"))
                    .map(|v| -> Result<Option<String>> {
                        if v.is_string()? {
                            Ok(v.resolve()?.as_str().map(str::to_owned))
                        } else {
                            Ok(None)
                        }
                    })
                    .transpose()?
                    .flatten()
                else {
                    continue;
                };
                let (offset, len) = view.source_range()?.context("export source range")?;
                if selected.is_some() {
                    offsets.entry(id.clone()).or_insert((offset, len));
                }
                if selected == Some(id.as_str()) && selected_entry.is_none() {
                    // Retain the first accepted duplicate, independent of cache admission. Reads
                    // receive their metadata in SessionRef and do not need unrelated discovery refs.
                    selected_entry = Some(Entry {
                        id: id.clone(),
                        offset,
                        len,
                        refs: [None, None],
                    });
                }
                if !cacheable && selected.is_some() {
                    continue;
                }
                let mut refs = [None, None];
                if allow_discovery {
                    for kind in [Kind::ChatGpt, Kind::Claude] {
                        if elem_shapes[slot(kind)] {
                            refs[slot(kind)] = Some(view_ref(kind, Path::new(""), &record, &id)?);
                        }
                    }
                }
                payload_charge = payload_charge.saturating_add(id.capacity());
                for r in refs.iter().flatten() {
                    payload_charge = payload_charge.saturating_add(r.id.capacity() + r.path.as_os_str().len());
                    payload_charge = payload_charge.saturating_add(r.title.as_ref().map_or(0, String::capacity));
                }
                entries.push(Entry { id, offset, len, refs });
                // Charge the lookup positions along with the roster before admission, so
                // indexed cached selection stays within the existing memory budget.
                charge = payload_charge
                    .saturating_add(entries.capacity() * (std::mem::size_of::<Entry>() + std::mem::size_of::<usize>()));
                if charge > admission_bytes {
                    cacheable = false;
                    if selected.is_some() {
                        // Drop the complete roster at the threshold rather than after the archive
                        // ends. Remaining siblings need validation and id comparison only.
                        drop(std::mem::take(&mut entries));
                    }
                }
            }
            Ok(())
        });
        let mut undecodable = false;
        if let Err(error) = scanned {
            if source.check().is_err() {
                continue;
            }
            if !error.is::<InvalidJson>() {
                return Err(error);
            }
            entries.clear();
            undecodable = true;
        }
        source.check()?;
        if !cacheable && selected.is_some() {
            entries = selected_entry.into_iter().collect();
            if !undecodable {
                publish_offsets(&key, state, Offsets(offsets));
            }
        }
        let by_id = entry_index(&entries);
        let archive = Arc::new(Archive {
            source,
            entries,
            by_id,
            charge,
            undecodable,
        });
        if cacheable && charge <= CACHE_BYTES {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            // Concurrent readers may build the same archive; replace rather than double-charge.
            if let Some(position) = guard.entries.iter().position(|e| e.path == key) {
                let old = guard.entries.remove(position).unwrap();
                guard.bytes -= old.archive.charge;
            }
            while guard.entries.len() >= CACHE_ENTRIES || guard.bytes + charge > CACHE_BYTES {
                if let Some(old) = guard.entries.pop_front() {
                    guard.bytes -= old.archive.charge;
                } else {
                    break;
                }
            }
            guard.bytes += charge;
            guard.entries.push_back(Cached {
                path: key.clone(),
                archive: archive.clone(),
            });
        }
        return Ok(archive);
    }
    anyhow::bail!("export changed while reading {}", path.display())
}

/// Where each conversation of an archive past the roster cache starts and ends (its first
/// accepted duplicate), from one scan of the archive at one state. Without it every selected read
/// of such an archive scanned the whole archive again to find its conversation: reading every
/// conversation of it was quadratic. Kept in this process and as a catalog source record (the
/// archive-wide consumer `export-offsets`, empty session id), admitted by the archive's identity
/// like every source record, so a later process reads it without a scan.
#[derive(serde::Serialize, serde::Deserialize)]
struct Offsets(HashMap<String, (u64, u64)>);

impl crate::source::Payload for Offsets {
    const TAG: &'static str = "export-offsets";
}

/// The archive-wide ref an [`Offsets`] record is kept under (whichever export shape reads it).
fn offsets_ref(key: &Path) -> SessionRef {
    SessionRef::new(String::new(), Harness::ChatGptExport, key.to_path_buf())
}

/// The archive's state an [`Offsets`] record is admitted against.
fn offsets_state(key: &Path) -> crate::source::Baseline {
    crate::source::Probe::default()
        .snapshot(&offsets_ref(key))
        .with_reads(&Default::default())
}

#[cfg(test)]
static SCANS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

type OffsetsMemo = Mutex<HashMap<PathBuf, Arc<crate::source::Record<Offsets>>>>;
static OFFSETS: std::sync::LazyLock<OffsetsMemo> = std::sync::LazyLock::new(Default::default);

/// The [`Offsets`] of the archive at `key` as of `now`: this process's, else the catalog's.
fn offsets_record(key: &Path, now: &crate::source::Baseline) -> Option<Arc<crate::source::Record<Offsets>>> {
    let mut probe = crate::source::Probe::default();
    let memo = OFFSETS.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned();
    if let Some(record) = memo.filter(|record| record.admit(now, &mut probe).is_some()) {
        return Some(record);
    }
    #[cfg(feature = "sqlite")]
    {
        let stored = crate::discover_cache::read::Context::current()
            .with_connection(|conn| {
                crate::source::record_in::<Offsets>(conn, <Offsets as crate::source::Payload>::TAG, &offsets_ref(key))
            })
            .flatten()
            .filter(|record| record.admit(now, &mut probe).is_some())?;
        let record = Arc::new(stored);
        OFFSETS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_path_buf(), record.clone());
        Some(record)
    }
    #[cfg(not(feature = "sqlite"))]
    None
}

/// Keep `offsets`, scanned from the archive at `state`, in this process and the catalog; the
/// publisher rechecks the archive's identity, and a frozen or unavailable catalog keeps none.
fn publish_offsets(key: &Path, state: crate::source::Baseline, offsets: Offsets) {
    #[cfg(feature = "sqlite")]
    crate::discover_cache::read::Context::current().with_connection_mut(|conn| {
        let mut publish = || -> Result<()> {
            let tx = conn.transaction()?;
            let mut probe = crate::source::Probe::default();
            crate::source::publish_in(
                &tx,
                <Offsets as crate::source::Payload>::TAG,
                &offsets_ref(key),
                &state,
                &offsets,
                &mut probe,
            )?;
            tx.commit()?;
            Ok(())
        };
        if let Err(error) = publish() {
            crate::env::warn_once("the export offset catalog", error);
        }
    });
    OFFSETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_path_buf(), Arc::new(crate::source::Record::new(state, offsets)));
}

/// Every conversation of `kind` in the archives at `paths`; an archive that cannot be read or
/// decoded is a reported skip.
pub(super) fn discover(kind: Kind, paths: Vec<PathBuf>) -> crate::Discovered {
    let mut out = crate::Discovered::default();
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        let Some(archive) = out.keep(&path, load(&path, None)) else {
            continue;
        };
        if archive.undecodable {
            out.skipped.push(crate::Skipped {
                path: path.clone(),
                why: "not a JSON export archive".into(),
            });
            continue;
        }
        for r in archive.entries.iter().filter_map(|e| e.refs[slot(kind)].as_ref()) {
            if seen.insert(r.id.clone()) {
                let mut r = r.clone();
                r.path = path.clone();
                out.refs.push(r);
            }
        }
    }
    out
}
const CONVERSATION_FIELDS: &[&str] = &[
    "conversation_id",
    "id",
    "uuid",
    "mapping",
    "chat_messages",
    "title",
    "name",
    "create_time",
    "created_at",
    "update_time",
    "updated_at",
    "current_node",
    "default_model_slug",
];
fn role_from_str(s: &str) -> Role {
    match s {
        "user" | "human" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::System,
    }
}

fn view_ref(kind: Kind, path: &Path, r: &Fields<'_>, id: &str) -> Result<SessionRef> {
    let title = r
        .string(if kind == Kind::ChatGpt { "title" } else { "name" })?
        .filter(|s| !s.is_empty());
    let timestamp = |primary, fallback| -> Result<_> {
        Ok(r.get(primary)
            .or_else(|| r.get(fallback))
            .map(|v| v.resolve().map(|v| ts_from_value(&v)))
            .transpose()?
            .flatten())
    };
    let nodes = Nodes::new(kind, r)?;
    let mut message_count = 0;
    for view in nodes.iter()? {
        if let Some(role) = node_role(kind, view?)? {
            if match kind {
                Kind::ChatGpt => matches!(role_from_str(&role), Role::User | Role::Assistant),
                Kind::Claude => matches!(role.as_str(), "human" | "assistant"),
            } {
                message_count += 1;
            }
        }
    }
    Ok(SessionRef {
        title,
        created_at: timestamp("create_time", "created_at")?,
        updated_at: timestamp("update_time", "updated_at")?,
        message_count: Some(message_count),
        ..SessionRef::new(id, kind.harness(), path.to_path_buf())
    })
}
fn active_chain<'a>(conv: &Fields<'a>) -> Result<Vec<JsonView<'a>>> {
    let Some(mapping) = conv.get("mapping") else {
        return Ok(Vec::new());
    };
    let mut chain = Vec::new();
    let Some(mut current) = conv.string("current_node")? else {
        mapping.for_each_entry(|_, value| {
            chain.push(value);
            Ok(())
        })?;
        return Ok(chain);
    };
    let mut nodes = HashMap::new();
    mapping.for_each_entry(|key, value| {
        nodes.insert(key.to_owned(), value);
        Ok(())
    })?;
    // Consuming the node entry proves finite traversal without truncating a legitimate
    // long branch. A cycle reaches an already consumed key and stops at that boundary.
    while let Some(node) = nodes.remove(&current) {
        chain.push(node);
        let parent = Fields::parse(node, &["parent"])?
            .map(|r| r.string("parent"))
            .transpose()?
            .flatten();
        let Some(parent) = parent else { break };
        current = parent;
    }
    chain.reverse();
    Ok(chain)
}
enum Nodes<'a> {
    Branch(Vec<JsonView<'a>>),
    Chronological(Option<JsonView<'a>>),
}
enum NodeIter<'a, 'b> {
    Branch(std::iter::Copied<std::slice::Iter<'b, JsonView<'a>>>),
    Chronological(Option<crate::json::JsonArray<'a>>),
}
impl<'a> Iterator for NodeIter<'a, '_> {
    type Item = Result<JsonView<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Branch(iter) => iter.next().map(Ok),
            Self::Chronological(iter) => iter.as_mut()?.next(),
        }
    }
}
impl<'a> Nodes<'a> {
    fn new(kind: Kind, conv: &Fields<'a>) -> Result<Self> {
        Ok(if kind == Kind::ChatGpt {
            Self::Branch(active_chain(conv)?)
        } else {
            Self::Chronological(conv.get("chat_messages"))
        })
    }
    fn iter(&self) -> Result<NodeIter<'a, '_>> {
        Ok(match self {
            Self::Branch(nodes) => NodeIter::Branch(nodes.iter().copied()),
            Self::Chronological(view) => NodeIter::Chronological(view.map(JsonView::array).transpose()?.flatten()),
        })
    }
}
fn node_role(kind: Kind, view: JsonView<'_>) -> Result<Option<String>> {
    let Some(record) = Fields::parse(view, &["sender", "message"])? else {
        return Ok(None);
    };
    if kind == Kind::Claude {
        return record.string("sender");
    }
    let Some(msg) = record
        .get("message")
        .map(|v| Fields::parse(v, &["author"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(author) = msg
        .get("author")
        .map(|v| Fields::parse(v, &["role"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    author.string("role")
}
pub(super) fn stream(kind: Kind, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    let archive = load(&r.path, Some(&r.id))?;
    let entry = archive
        .entry(&r.id)
        .with_context(|| format!("conversation {} not found in {}", r.id, r.path.display()))?;
    // Cached archives own metadata only. Generated multipart backing belongs to this read,
    // not to the cache's JsonSource/arena and its uncharged lifetime.
    let source = JsonSource::capture(&r.path)?;
    // `JsonSource` equality is identity: the same path carrying the same captured bytes.
    anyhow::ensure!(source == archive.source, "export source changed before selected read");
    let value = source.validated_range(entry.offset, entry.len)?;
    value.with_view(|view| {
        let conv = Fields::parse(view, CONVERSATION_FIELDS)?.context("export conversation is not an object")?;
        let mut session = Session::new(&r.id, kind.harness());
        session.title = r.title.clone();
        session.created_at = r.created_at;
        session.updated_at = r.updated_at;
        session.source_path = Some(r.path.clone());
        session.model = conv.string("default_model_slug")?.filter(|s| !s.is_empty());
        let nodes = Nodes::new(kind, &conv)?;
        let resolver = Resolver::new(Some(r.path.clone()));
        if session.title.is_none() {
            let mut calls = HashMap::new();
            for view in nodes.iter()? {
                let view = view?;
                if node_role(kind, view)?.as_deref().map(role_from_str) != Some(Role::User) {
                    continue;
                }
                if kind == Kind::ChatGpt {
                    if let Some(title) = chatgpt_title(view, &resolver)? {
                        session.title = title;
                        break;
                    }
                    continue;
                }
                if let Some(message) = build_message(kind, view, true, Some(&source), &resolver, &mut calls)? {
                    if message.role == Role::User {
                        session.title = text_summary(
                            message.content.iter().filter_map(|b| match b {
                                Block::Text { text } => Some(text),
                                _ => None,
                            }),
                            &resolver,
                            true,
                        );
                        break;
                    }
                }
            }
        }
        sink.meta(&session);
        let mut calls = HashMap::new();
        for view in nodes.iter()? {
            if let Some(message) = build_message(kind, view?, opts.spans, Some(&source), &resolver, &mut calls)? {
                let started = std::time::Instant::now();
                let flow = sink.try_message(message)?;
                archive.source.check_after(started)?;
                if flow == Flow::Stop {
                    break;
                }
            }
        }

        Ok(session)
    })
}

/// Parse all ChatGPT and/or Claude.ai export conversations from an in-memory JSON array or object
/// (used by [`crate::ingest`], wasm-safe).
pub(super) fn parse_all_str(text: &str) -> Result<Vec<Session>> {
    use crate::harness::store::document::range::RangeSource;
    let source = RangeSource::text(None, text, 0, false);
    let Some(root_val) = source.accept(source.range(), Validation::Syntax)? else {
        return Ok(Vec::new());
    };
    let root = JsonView::of(&source, root_val);
    let mut out = Vec::new();
    let parse_one = |view: JsonView<'_>, out: &mut Vec<Session>| -> Result<()> {
        let Some(conv) = Fields::parse(view, CONVERSATION_FIELDS)? else {
            return Ok(());
        };
        let kind = if conv.get("mapping").is_some() {
            Kind::ChatGpt
        } else if conv.get("chat_messages").is_some() {
            Kind::Claude
        } else {
            return Ok(());
        };
        let Some(id) = conv
            .get("conversation_id")
            .or_else(|| conv.get("id"))
            .or_else(|| conv.get("uuid"))
            .map(|v| -> Result<Option<String>> {
                if v.is_string()? {
                    Ok(v.resolve()?.as_str().map(str::to_owned))
                } else {
                    Ok(None)
                }
            })
            .transpose()?
            .flatten()
            .filter(|s| !s.is_empty())
        else {
            return Ok(());
        };
        let r = view_ref(kind, Path::new(""), &conv, &id)?;
        let mut session = Session::new(&id, kind.harness());
        session.title = r.title;
        session.created_at = r.created_at;
        session.updated_at = r.updated_at;
        session.model = conv.string("default_model_slug")?.filter(|s| !s.is_empty());
        let nodes = Nodes::new(kind, &conv)?;
        let resolver = Resolver::new(None);
        if session.title.is_none() {
            let mut calls = HashMap::new();
            for nview in nodes.iter()? {
                let nview = nview?;
                if node_role(kind, nview)?.as_deref().map(role_from_str) != Some(Role::User) {
                    continue;
                }
                if kind == Kind::ChatGpt {
                    if let Some(title) = chatgpt_title(nview, &resolver)? {
                        session.title = title;
                        break;
                    }
                    continue;
                }
                if let Some(message) = build_message(kind, nview, false, None, &resolver, &mut calls)? {
                    if message.role == Role::User {
                        session.title = text_summary(
                            message.content.iter().filter_map(|b| match b {
                                Block::Text { text } => Some(text),
                                _ => None,
                            }),
                            &resolver,
                            true,
                        );
                        break;
                    }
                }
            }
        }
        let mut calls = HashMap::new();
        for nview in nodes.iter()? {
            if let Some(message) = build_message(kind, nview?, false, None, &resolver, &mut calls)? {
                session.messages.push(message);
            }
        }
        out.push(session);
        Ok(())
    };
    if let Some(arr) = root.accepted_array()? {
        for item in arr {
            if let Some(view) = item? {
                parse_one(view, &mut out)?;
            }
        }
    } else {
        parse_one(root, &mut out)?;
    }
    Ok(out)
}

fn build_message(
    kind: Kind,
    view: JsonView<'_>,
    spans: bool,
    source: Option<&JsonSource>,
    resolver: &Resolver,
    calls: &mut HashMap<String, String>,
) -> Result<Option<Message>> {
    match kind {
        Kind::Claude => claude_message(view, spans),
        Kind::ChatGpt => chatgpt_message(view, spans, source, resolver, calls),
    }
}
fn claude_message(view: JsonView<'_>, spans: bool) -> Result<Option<Message>> {
    let Some(r) = Fields::parse(
        view,
        &["sender", "content", "text", "uuid", "parent_message_uuid", "created_at"],
    )?
    else {
        return Ok(None);
    };
    let Some(sender) = r.string("sender")? else {
        return Ok(None);
    };
    let role = role_from_str(&sender);
    let mut content = Vec::new();
    if let Some(parts) = r.get("content").map(JsonView::array).transpose()?.flatten() {
        for view in parts {
            let view = view?;
            let Some(mut block) = anthropic::block_from_view_with(
                view,
                &anthropic::CLAUDE_AI,
                spans,
                &mut |v| v.lazy_text(spans),
                crate::harness::wire::joined_over(spans),
            )?
            else {
                continue;
            };
            match &mut block {
                Block::ToolResult { content, .. } | Block::Thinking { text: content, .. } if content.is_empty() => {
                    if let Some(text) = Fields::parse(view, &["text"])?
                        .and_then(|r| r.get("text"))
                        .map(|v| v.lazy_text(spans))
                        .transpose()?
                        .flatten()
                    {
                        *content = text;
                    }
                }
                _ => {}
            }
            content.push(block);
        }
    }
    if content.is_empty() {
        if let Some(text) = r
            .get("text")
            .map(|v| v.lazy_text(spans))
            .transpose()?
            .flatten()
            .filter(|s| !s.is_empty())
        {
            content.push(Block::Text { text });
        }
    }
    if content.is_empty() {
        return Ok(None);
    };
    let mut m = Message::new(role);
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }
    m.id = r.string("uuid")?;
    m.parent_id = r.string("parent_message_uuid")?;
    m.timestamp = r.value("created_at")?.as_ref().and_then(ts_from_value);
    m.content = content;
    Ok(Some(m))
}
fn chatgpt_message(
    view: JsonView<'_>,
    spans: bool,
    source: Option<&JsonSource>,
    resolver: &Resolver,
    calls: &mut HashMap<String, String>,
) -> Result<Option<Message>> {
    let Some(node) = Fields::parse(view, &["message", "id", "parent"])? else {
        return Ok(None);
    };
    let Some(msg) = node
        .get("message")
        .map(|v| Fields::parse(v, &["author", "recipient", "content", "create_time"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(author) = msg
        .get("author")
        .map(|v| Fields::parse(v, &["role", "name"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(role) = author.string("role")? else {
        return Ok(None);
    };
    let role = role_from_str(&role);
    let id = node.string("id")?.unwrap_or_default();
    let recipient = msg.string("recipient")?.unwrap_or_else(|| "all".into());
    let raw_content = msg.get("content");
    let content_record = raw_content
        .map(|v| Fields::parse(v, &["parts", "text"]))
        .transpose()?
        .flatten();
    let mut content = Vec::new();
    if role == Role::Assistant && recipient != "all" {
        content.push(Block::ToolUse {
            id: id.clone(),
            name: recipient.clone(),
            input: raw_content.map(|v| v.argument(spans)).transpose()?.unwrap_or_default(),
            namespace: None,
        });
        calls.insert(recipient, id.clone());
    } else {
        let text = chatgpt_text(content_record.as_ref(), spans, source, resolver)?;
        if role == Role::Tool {
            let name = author.string("name")?.unwrap_or_default();
            content.push(Block::ToolResult {
                tool_use_id: calls.get(&name).cloned().unwrap_or_default(),
                content: text,
                is_error: false,
                tool_name: (!name.is_empty()).then_some(name),
                status: None,
                details: None,
            });
        } else if text_summary(std::iter::once(&text), resolver, true).is_some() {
            content.push(Block::Text { text });
        }
        if let Some(parts) = content_record
            .as_ref()
            .and_then(|r| r.get("parts"))
            .map(JsonView::array)
            .transpose()?
            .flatten()
        {
            for view in parts {
                if let Some(pointer) = Fields::parse(view?, &["asset_pointer"])?
                    .map(|r| r.string("asset_pointer"))
                    .transpose()?
                    .flatten()
                {
                    content.push(Block::Image {
                        media_type: None,
                        data_ref: Some(pointer),
                    });
                }
            }
        }
    }
    if content.is_empty() {
        return Ok(None);
    };
    let mut m = Message::new(role);
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }
    m.id = (!id.is_empty()).then_some(id);
    m.parent_id = node.string("parent")?;
    m.timestamp = msg.value("create_time")?.as_ref().and_then(ts_from_value);
    m.content = content;
    Ok(Some(m))
}
/// Title selection needs the joined prefix and whether an image keeps the first user message,
/// not a fully joined generated body. Preserve the old first accepted user stopping point.
fn chatgpt_title(view: JsonView<'_>, resolver: &Resolver) -> Result<Option<Option<String>>> {
    let Some(node) = Fields::parse(view, &["message"])? else {
        return Ok(None);
    };
    let Some(message) = node
        .get("message")
        .map(|v| Fields::parse(v, &["content"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(content) = message
        .get("content")
        .map(|v| Fields::parse(v, &["parts", "text"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let mut texts = Vec::new();
    let mut image = false;
    if let Some(parts) = content.get("parts").map(JsonView::array).transpose()?.flatten() {
        for part in parts {
            let part = part?;
            if let Some(text) = part.text(true)? {
                texts.push(text);
            } else if let Some(record) = Fields::parse(part, &["asset_pointer"])? {
                image |= record
                    .get("asset_pointer")
                    .map(JsonView::is_string)
                    .transpose()?
                    .unwrap_or(false);
            }
        }
    } else if let Some(text) = content.get("text").map(|v| v.text(true)).transpose()?.flatten() {
        texts.push(text);
    }
    let title = text_summary(texts.iter(), resolver, true);
    Ok((image || title.is_some()).then_some(title))
}
pub(super) fn chatgpt_text(
    r: Option<&Fields<'_>>,
    spans: bool,
    source: Option<&JsonSource>,
    resolver: &Resolver,
) -> Result<Text> {
    let Some(r) = r else { return Ok(Text::default()) };
    if let Some(parts) = r.get("parts").map(JsonView::array).transpose()?.flatten() {
        if let (true, Some(source)) = (spans, source) {
            let mut selected =
                parts.filter_map(
                    |part| match part.and_then(|part| Ok(part.is_string()?.then_some(part))) {
                        Ok(Some(part)) => Some(Ok(part)),
                        Ok(None) => None,
                        Err(error) => Some(Err(error)),
                    },
                );
            let Some(first) = selected.next().transpose()? else {
                return Ok(Text::default());
            };
            let Some(second) = selected.next().transpose()? else {
                return Ok(first.lazy_text(true)?.unwrap());
            };
            return crate::harness::store::text::append(source, &source.decoded_backing()?, |builder| {
                for (i, part) in [Ok(first), Ok(second)].into_iter().chain(selected).enumerate() {
                    if i != 0 {
                        builder.push("\n")?;
                    }
                    anyhow::ensure!(builder.push_string(part?)?, "selected ChatGPT string part");
                }
                Ok(())
            });
        }
        let mut first = None::<Text>;
        let mut joined = None::<String>;
        for view in parts {
            let Some(text) = view?.lazy_text(spans)? else { continue };
            if let Some(buffer) = &mut joined {
                buffer.push('\n');
                buffer.push_str(&text.try_resolve(resolver)?);
            } else if let Some(previous) = first.take() {
                let mut buffer = previous.try_resolve(resolver)?.into_owned();
                buffer.push('\n');
                buffer.push_str(&text.try_resolve(resolver)?);
                joined = Some(buffer);
            } else {
                first = Some(text);
            }
        }
        return Ok(joined.map(Text::Inline).or(first).unwrap_or_default());
    }
    Ok(r.get("text")
        .map(|v| v.lazy_text(spans))
        .transpose()?
        .flatten()
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_archive_selection_preserves_first_duplicate_and_archive_order() -> Result<()> {
        let world = crate::test_world("export-indexed-selection");
        let path = world.base.join("conversations.json");
        let mut values = Vec::new();
        for i in (0..4096).rev() {
            values.push(serde_json::json!({"uuid":format!("conversation-{i:04}"),"chat_messages":[]}));
        }
        // A later duplicate must never displace the first record at selection.
        values.push(
            serde_json::json!({"uuid":"conversation-2048","chat_messages":[{"sender":"human","text":"duplicate"}]}),
        );
        std::fs::write(&path, serde_json::to_vec(&values)?)?;
        let archive = load(&path, None)?;
        assert_eq!(archive.entries.len(), values.len());
        assert_eq!(archive.by_id.len(), archive.entries.len());
        for (entry, value) in archive.entries.iter().zip(&values) {
            assert_eq!(entry.id, value["uuid"].as_str().unwrap());
        }
        for i in 0..4096 {
            let id = format!("conversation-{i:04}");
            let expected = archive.entries.iter().find(|entry| entry.id == id).unwrap();
            assert!(std::ptr::eq(archive.entry(&id).unwrap(), expected));
        }
        assert!(archive.entry("conversation-2048-extra").is_none());
        assert!(archive.entry("before-all").is_none());
        assert!(archive.entry("zzz-after-all").is_none());
        // Selection keeps the first duplicate even if another reader evicts the shared cache.
        let selected = load(&path, Some("conversation-2048"))?;
        let expected = archive.entry("conversation-2048").unwrap();
        let actual = selected.entry("conversation-2048").unwrap();
        assert_eq!((actual.offset, actual.len), (expected.offset, expected.len));
        // Freshness still rejects changed archives.
        std::fs::write(&path, b"[]")?;
        assert!(archive.source.check().is_err());
        assert!(load(&path, Some("conversation-2048"))?
            .entry("conversation-2048")
            .is_none());
        Ok(())
    }

    /// Reading every conversation of an archive past the roster cache scans it once: the first
    /// selected read leaves the offset index, in this process and the catalog, that every later
    /// read (another process's too) finds its conversation by. A changed archive scans again.
    #[test]
    fn selected_reads_past_the_roster_cache_scan_the_archive_once() {
        let world = crate::test_world("export-offsets");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let path = world.base.join("conversations.json");
        let write = |n: usize| {
            let values: Vec<_> = (0..n)
                .map(|i| serde_json::json!({"uuid":format!("c{i}"),"chat_messages":[{"sender":"human","text":format!("body {i}")}]}))
                .collect();
            std::fs::write(&path, serde_json::to_vec(&values).unwrap()).unwrap();
        };
        let scans = || {
            let mut all = SCANS.lock().unwrap();
            let mine = all.iter().filter(|p| **p == path).count();
            all.retain(|p| *p != path);
            mine
        };
        let range = |id: &str| {
            let archive = load_bounded(&path, Some(id), 1024).unwrap();
            archive.entry(id).map(|e| (e.offset, e.len))
        };
        write(64);
        let expected: Vec<_> = (0..64)
            .map(|i| {
                load(&path, None)
                    .unwrap()
                    .entry(&format!("c{i}"))
                    .map(|e| (e.offset, e.len))
            })
            .collect();
        // Only the oversized path is under test: no roster in this process's cache.
        *CACHE.get_or_init(Mutex::default).lock().unwrap() = Cache::default();
        scans();
        for (i, expected) in expected.iter().enumerate() {
            assert_eq!(range(&format!("c{i}")), *expected);
        }
        assert_eq!(range("absent"), None);
        assert_eq!(scans(), 1);
        // Another process: the catalog's record, no scan.
        OFFSETS.lock().unwrap().clear();
        assert_eq!(range("c7"), expected[7]);
        assert_eq!(scans(), 0);
        write(65);
        assert!(range("c64").is_some());
        assert_eq!(scans(), 1);
    }

    #[test]
    fn oversized_selected_lookup_keeps_the_first_accepted_duplicate_on_either_side_of_admission() {
        let dir = std::env::temp_dir().join(format!("cv-export-admission-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conversations.json");
        let first = r#"{"uuid":"target","chat_messages":[{"sender":"human","text":"first"}]}"#;
        let duplicate = r#"{"uuid":"target","chat_messages":[{"sender":"human","text":"later"}]}"#;
        let bad = r#"{"uuid":"target","chat_messages":[],"unknown":1e400,"unknown":0}"#;
        let unrelated = (0..64)
            .map(|i| format!(r#"{{"uuid":"other{i}","chat_messages":[]}}"#))
            .collect::<Vec<_>>()
            .join(",");
        for archive in [
            format!("[{bad},{first},{unrelated},{duplicate}]"),
            format!("[{bad},{unrelated},{first},{duplicate}]"),
        ] {
            std::fs::write(&path, &archive).unwrap();
            let loaded = load_bounded(&path, Some("target"), 1024).unwrap();
            assert!(loaded.charge > 1024, "the complete metadata cache must be rejected");
            assert_eq!(
                loaded.entries.len(),
                1,
                "unrelated entries must be dropped during construction"
            );
            assert_eq!(loaded.entries[0].id, "target");
            let entry = &loaded.entries[0];
            let selected = loaded.source.validated_range(entry.offset, entry.len).unwrap();
            assert_eq!(serde_json::to_string(&selected).unwrap(), first);
            // Even after admission is rejected, discovery still returns its complete output.
            let discovery = load_bounded(&path, None, 1024).unwrap();
            assert_eq!(discovery.entries.len(), 66);
        }
        let mut sink = crate::stream::CollectSink::default();
        std::fs::write(&path, format!("[{first},]")).unwrap();
        let r = SessionRef::new("target", Harness::ClaudeExport, path.clone());
        assert!(stream(Kind::Claude, &r, &Demand::LEAN.with_spans(), &mut sink).is_err());
        assert!(
            sink.messages.is_empty(),
            "whole archive syntax must pass before callbacks"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn per_element_sniffing_discovers_conversations_after_stub_zero_and_mixed_shapes() -> Result<()> {
        let world = crate::test_world("export-per-element-sniff");
        let path = world.base.join("conversations.json");
        let archive = serde_json::json!([
            { "id": "stub-no-discriminator", "title": "metadata header" },
            { "uuid": "claude-1", "name": "Claude Chat", "chat_messages": [{ "sender": "human", "text": "hello claude" }] },
            {
                "id": "gpt-1",
                "title": "ChatGPT Chat",
                "current_node": "n1",
                "mapping": {
                    "n1": {
                        "id": "n1",
                        "parent": null,
                        "message": {
                            "id": "n1",
                            "author": { "role": "user" },
                            "content": { "parts": ["hello gpt"] }
                        }
                    }
                }
            }
        ]);
        std::fs::write(&path, serde_json::to_vec(&archive)?)?;

        let claude_disc = discover(Kind::Claude, vec![path.clone()]);
        assert_eq!(claude_disc.refs.len(), 1);
        assert_eq!(claude_disc.refs[0].id, "claude-1");

        let gpt_disc = discover(Kind::ChatGpt, vec![path]);
        assert_eq!(gpt_disc.refs.len(), 1);
        assert_eq!(gpt_disc.refs[0].id, "gpt-1");
        Ok(())
    }
}
