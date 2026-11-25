//! Verified continuations for repeated API reads, one per session read. No messages are cached.
use super::{session_meta_json, WindowSink};
use crate::source::{Baseline, Probe, ReadSet};
use crate::stream::{Checkpoint, Demand, Flow, Start};
use crate::{Adapter, Session, SessionRef};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

/// The continuations kept, one per (session, read, demand), least recently used out first. The
/// count is the whole bound: an entry is as large as its checkpoint and metadata, the state the
/// read held anyway. Past it an evicted session's next page or head reads from its start, and
/// stderr says so once per process.
const ENTRIES: usize = 1024;

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
struct Key {
    harness: String,
    id: String,
    path: std::path::PathBuf,
    primary: std::path::PathBuf,
    version: String,
    extra: bool,
    cwd: Option<std::path::PathBuf>,
    title: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    message_count: Option<usize>,
}

impl Key {
    fn of(r: &SessionRef, primary: &std::path::Path, version: String, extra: bool) -> Self {
        Self {
            harness: r.harness.as_str().into(),
            id: r.id.clone(),
            path: r.path.clone(),
            primary: primary.to_path_buf(),
            version,
            extra,
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            message_count: r.message_count,
        }
    }
}

#[derive(Clone, Serialize)]
enum Payload {
    Window {
        index: usize,
        checkpoint: Checkpoint,
        meta: Option<Value>,
    },
    Head(Value),
}

#[derive(Clone, Serialize)]
struct Entry {
    key: Key,
    source: Baseline,
    payload: Payload,
}

#[derive(Default)]
struct Entries {
    by_key: HashMap<Key, (u64, Entry)>,
    by_use: BTreeMap<u64, Key>,
    tick: u64,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: Mutex<Entries>,
}

impl Cache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn get(&self, key: &Key) -> Option<Entry> {
        let mut entries = self.lock();
        let Entries { by_key, by_use, tick } = &mut *entries;
        let (used, entry) = by_key.get_mut(key)?;
        by_use.remove(used);
        *tick += 1;
        *used = *tick;
        by_use.insert(*tick, key.clone());
        Some(entry.clone())
    }
    fn remove(&self, key: &Key) {
        let mut entries = self.lock();
        if let Some((used, _)) = entries.by_key.remove(key) {
            entries.by_use.remove(&used);
        }
    }
    fn insert(&self, entry: Entry) {
        let mut entries = self.lock();
        let Entries { by_key, by_use, tick } = &mut *entries;
        if let Some((used, _)) = by_key.remove(&entry.key) {
            by_use.remove(&used);
        }
        while by_key.len() >= ENTRIES {
            let Some((_, oldest)) = by_use.pop_first() else { break };
            by_key.remove(&oldest);
            noted_eviction();
        }
        *tick += 1;
        by_use.insert(*tick, entry.key.clone());
        by_key.insert(entry.key.clone(), (*tick, entry));
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().by_key.len()
    }
    #[cfg(test)]
    fn entries(&self) -> Vec<Entry> {
        self.lock().by_key.values().map(|(_, entry)| entry.clone()).collect()
    }
}

/// The budget's one report: the first eviction in a process says the cache is full.
fn noted_eviction() {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "cv: the API read cache holds its {ENTRIES} most recent sessions; a page or head of an older one reads from its start"
        );
    }
}

pub(super) fn shared() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

fn captured(r: &SessionRef, primary: &std::path::Path) -> (Baseline, ReadSet) {
    let before = Probe::default().snapshot_from(r, primary);
    let mut reads = ReadSet::default();
    // Include replacement identity and ctime, even when size and mtime were restored.
    reads.record(primary);
    (before, reads)
}

fn fresh(r: &SessionRef, before: &Baseline, primary: &std::path::Path) -> bool {
    let mut probe = Probe::default();
    let current = probe.snapshot_from(r, primary);
    before.fresh(&current, &mut probe)
}

pub(super) fn window(
    cache: &Cache,
    adapter: &dyn Adapter,
    r: &SessionRef,
    start: usize,
    end: Option<usize>,
    extra: bool,
) -> Result<(Session, WindowSink)> {
    let opts = if extra { Demand::RICH } else { Demand::LEAN }.with_spans();
    let version = crate::resume::version(adapter, "api-window");
    let primary = adapter.resume_source(r);
    let key = Key::of(r, &primary, version, extra);
    let (before, reads) = captured(r, &primary);
    let mut sink = WindowSink::page(&primary, start, end, true, reads.clone());
    let mut resumed = false;
    let mut shell = None;
    {
        if let Some(entry) = cache.get(&key) {
            if fresh(r, &entry.source, &primary) {
                if let Payload::Window {
                    index,
                    checkpoint,
                    meta,
                } = entry.payload
                {
                    if index <= start {
                        sink.idx = index;
                        sink.meta = meta;
                        if let Some(old_reads) = entry.source.reads() {
                            sink.reads.merge(&old_reads);
                        }
                        shell = adapter.transcript(r, &opts, Start::Checkpoint(&checkpoint), &mut sink)?;
                        resumed = shell.is_some();
                    }
                }
            } else {
                cache.remove(&key);
            }
        }
    }
    if shell.is_none() {
        // A declined resume promises no callbacks, but discard its initial state too: the
        // stream starts from the capture taken before the cache was consulted.
        sink = WindowSink::page(&primary, start, end, true, reads);
        shell = Some(adapter.try_stream(r, &opts, &mut sink)?);
    }
    let source = before.with_reads(&sink.reads);
    let stable = fresh(r, &source, &primary);
    if resumed && !stable {
        // Do not return a new suffix composed with an old prefix's metadata after a mutation.
        cache.remove(&key);
        let (retry_before, reads) = captured(r, &primary);
        let mut whole = WindowSink::page(&primary, start, end, false, reads);
        let shell = adapter.try_stream(r, &opts, &mut whole)?;
        verify(r, &primary, &retry_before, &whole)?;
        whole.finish_meta(&shell);
        return Ok((shell, whole));
    }
    // `stable` is `verify`'s primary check on this same baseline and read set.
    anyhow::ensure!(stable, "API window source changed before publication");
    if let Some((index, checkpoint, meta)) = sink.checkpoint.take() {
        cache.insert(Entry {
            key,
            source,
            payload: Payload::Window {
                index,
                checkpoint,
                meta,
            },
        });
    }
    let shell = shell.expect("a stream or accepted resume returned metadata");
    sink.finish_meta(&shell);
    Ok((shell, sink))
}

fn verify(r: &SessionRef, primary: &std::path::Path, before: &Baseline, sink: &WindowSink) -> Result<()> {
    anyhow::ensure!(
        fresh(r, &before.with_reads(&sink.reads), primary),
        "API window source changed before publication"
    );
    Ok(())
}

fn value(
    cache: &Cache,
    r: &SessionRef,
    primary: &std::path::Path,
    consumer: &str,
    read: impl FnOnce(&mut ReadSet) -> Result<Value>,
) -> Result<Value> {
    let key = Key::of(r, primary, crate::source::consumer_version(consumer), false);
    if let Some(entry) = cache.get(&key) {
        if fresh(r, &entry.source, primary) {
            if let Payload::Head(value) = entry.payload {
                return Ok(value);
            }
        } else {
            cache.remove(&key);
        }
    }
    let (before, mut reads) = captured(r, primary);
    let value = read(&mut reads)?;
    let source = before.with_reads(&reads);
    anyhow::ensure!(fresh(r, &source, primary), "API source changed before publication");
    cache.insert(Entry {
        key,
        source,
        payload: Payload::Head(value.clone()),
    });
    Ok(value)
}

pub(super) fn head(cache: &Cache, adapter: &dyn Adapter, r: &SessionRef) -> Result<Value> {
    let primary = adapter.resume_source(r);
    value(cache, r, &primary, "api-head1", |reads| {
        struct Counter {
            primary: std::path::PathBuf,
            total: usize,
            reads: ReadSet,
        }
        impl crate::MessageSink for Counter {
            fn tier(&self) -> crate::Tier {
                crate::Tier::Facts
            }
            fn message(&mut self, message: crate::Message) -> Flow {
                crate::source::record_message_reads(&message, Some(&self.primary), &mut self.reads);
                self.total += 1;
                Flow::Continue
            }
            fn dependency(&mut self, path: &std::path::Path) {
                self.reads.record(path);
            }
        }
        let mut counter = Counter {
            primary: primary.clone(),
            total: 0,
            reads: std::mem::take(reads),
        };
        let shell = adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut counter)?;
        let mut value = session_meta_json(&shell);
        let map = value.as_object_mut().expect("session metadata is an object");
        // `title`/`cwd` are carried only when the transcript knows them: a null would overwrite
        // the catalog row's discovery hint when this map merges into it (`session_head`).
        for key in ["title", "cwd"] {
            if map.get(key).is_some_and(Value::is_null) {
                map.remove(key);
            }
        }
        map.insert("total".into(), json!(counter.total));
        map.insert("extra".into(), Value::Object(shell.extra));
        *reads = counter.reads;
        Ok(value)
    })
}

pub(super) fn compactions(cache: &Cache, adapter: &dyn Adapter, r: &SessionRef) -> Result<Value> {
    let primary = adapter.resume_source(r);
    value(cache, r, &primary, "api-compactions1", |reads| {
        struct Sink<'a> {
            inner: crate::compaction::CompactionSink,
            primary: &'a std::path::Path,
            reads: &'a mut ReadSet,
        }
        impl crate::MessageSink for Sink<'_> {
            fn message(&mut self, m: crate::Message) -> Flow {
                crate::source::record_message_reads(&m, Some(self.primary), self.reads);
                self.inner.message(m)
            }
            fn dependency(&mut self, path: &std::path::Path) {
                self.reads.record(path);
            }
        }
        let mut sink = Sink {
            inner: crate::compaction::CompactionSink::new(true).with_source(&r.path),
            primary: &primary,
            reads,
        };
        adapter.try_stream(r, &Demand::FACTS.with_spans(), &mut sink)?;
        let boundaries = sink.inner.into_boundaries()?;
        Ok(Value::Array(
            (0..boundaries.len())
                .map(|n| {
                    let mut row = crate::compaction::row(&boundaries, n);
                    row.as_object_mut()
                        .expect("compaction row")
                        .insert("headline".into(), json!(boundaries[n].headline(n + 1)));
                    row
                })
                .collect(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Harness, Message, MessageSink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Observed<'a> {
        inner: &'a dyn Adapter,
        messages: AtomicUsize,
        dependency: Option<std::path::PathBuf>,
        rewrite: Mutex<Option<String>>,
        decline: bool,
    }
    impl<'a> Observed<'a> {
        fn new(inner: &'a dyn Adapter) -> Self {
            Self {
                inner,
                messages: AtomicUsize::new(0),
                dependency: None,
                rewrite: Mutex::new(None),
                decline: false,
            }
        }
    }
    struct Observer<'a> {
        sink: &'a mut dyn MessageSink,
        messages: &'a AtomicUsize,
    }
    impl MessageSink for Observer<'_> {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.fetch_add(1, Ordering::Relaxed);
            self.sink.message(message)
        }
        fn meta(&mut self, session: &Session) {
            self.sink.meta(session);
        }
        fn dependency(&mut self, path: &std::path::Path) {
            self.sink.dependency(path);
        }
        fn wants_checkpoint(&mut self, offset: u64) -> bool {
            self.sink.wants_checkpoint(offset)
        }
        fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
            self.sink.wants_final_checkpoint(offset)
        }
        fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
            self.sink.checkpoint(checkpoint);
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.sink.take_error()
        }
    }
    impl Adapter for Observed<'_> {
        fn harness(&self) -> Harness {
            self.inner.harness()
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            self.inner.roots()
        }
        fn discover(&self) -> Result<crate::Discovered> {
            self.inner.discover()
        }
        fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            self.inner.try_stream(
                r,
                opts,
                &mut Observer {
                    sink,
                    messages: &self.messages,
                },
            )
        }
        fn resume_source(&self, r: &SessionRef) -> std::path::PathBuf {
            self.inner.resume_source(r)
        }
        fn transcript(
            &self,
            r: &SessionRef,
            opts: &Demand,
            from: Start<'_>,
            sink: &mut dyn MessageSink,
        ) -> Result<Option<Session>> {
            if matches!(from, Start::Whole) {
                return self.try_stream(r, opts, sink).map(Some);
            }
            if self.decline {
                return Ok(None);
            }
            if let Some(body) = self.rewrite.lock().unwrap().take() {
                std::fs::write(&r.path, body)?;
            }
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            self.inner.transcript(
                r,
                opts,
                from,
                &mut Observer {
                    sink,
                    messages: &self.messages,
                },
            )
        }
    }
    fn result(shell: &Session, sink: &mut WindowSink) -> Value {
        json!({ "messages": sink.values().unwrap(), "idx": sink.idx, "more": sink.more,
            "meta": sink.meta.clone().unwrap_or_else(|| session_meta_json(shell)) })
    }
    fn oracle(adapter: &dyn Adapter, r: &SessionRef, start: usize, end: Option<usize>, extra: bool) -> Value {
        let mut sink = WindowSink::page(&adapter.resume_source(r), start, end, false, ReadSet::default());
        let shell = adapter
            .try_stream(
                r,
                &if extra { Demand::RICH } else { Demand::LEAN }.with_spans(),
                &mut sink,
            )
            .unwrap();
        result(&shell, &mut sink)
    }
    fn fixture(world: &cv_testkit::World, count: usize) -> SessionRef {
        use std::io::Write as _;
        let path = world.transcript("cached-pages");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for i in 0..count {
            writeln!(
                file,
                r#"{{"type":"user","uuid":"m{i}","message":{{"role":"user","content":"hello"}}}}"#
            )
            .unwrap();
        }
        file.flush().unwrap();
        SessionRef::new("cached-pages", Harness::Claude, path)
    }

    #[test]
    fn read_coverage_window_publishes_late_returned_evidence() {
        use crate::coverage::ReadCoverage;
        struct Late;
        impl Adapter for Late {
            fn harness(&self) -> Harness {
                Harness::OpenSession
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(&self, r: &SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
                let mut session = Session::new(&r.id, Harness::OpenSession);
                session.title = Some("initial title".into());
                sink.meta(&session);
                sink.message(Message::new(crate::Role::Assistant));
                session.title = Some("final title".into());
                let mut coverage = ReadCoverage::default();
                coverage.sources.insert(ReadCoverage::NATIVE_STORE.into());
                coverage.limitations.insert(ReadCoverage::MISSING_PAYLOADS.into());
                coverage.attach(&mut session);
                Ok(session)
            }
        }
        let world = cv_testkit::World::new("coverage-api-window");
        let r = fixture(&world, 1);
        let cache = Cache::default();
        for extra in [false, true] {
            let (shell, mut sink) = window(&cache, &Late, &r, 0, None, extra).unwrap();
            let reply = result(&shell, &mut sink);
            assert_eq!(reply["meta"]["title"], "initial title");
            assert_eq!(
                reply["meta"]["extra"]["cv"]["read_coverage"]["limitations"],
                json!(["missing_payloads"])
            );
        }
    }

    #[test]
    fn heads_and_windows_reject_mutation_before_publication() {
        struct Mutating;
        impl Adapter for Mutating {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(&self, r: &SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
                let mut message = Message::new(crate::Role::Assistant);
                message.content.push(crate::Block::from_text("selected".into()));
                sink.try_message(message)?;
                std::fs::write(&r.path, b"changed")?;
                Ok(Session::new(r.id.clone(), r.harness))
            }
        }
        let world = crate::test_world("api-staged-mutation");
        let path = world.home.join("source");
        std::fs::write(&path, b"original").unwrap();
        let r = SessionRef::new("mutating", Harness::Claude, path);
        let cache = Cache::default();
        assert!(window(&cache, &Mutating, &r, 0, Some(1), false).is_err());
        assert_eq!(cache.len(), 0);
        std::fs::write(&r.path, b"original").unwrap();
        let cache = Cache::default();
        assert!(head(&cache, &Mutating, &r).is_err());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn native_window_cache_matches_whole_pass_oracles() {
        let world = crate::test_world("api_native_windows");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut checked = 0;
        for harness in [
            Harness::Claude,
            Harness::Codex,
            Harness::KimiCode,
            Harness::Grok,
            Harness::Kimi,
        ] {
            let adapter: Box<dyn Adapter> = match harness {
                Harness::Claude => Box::new(crate::harness::claude::Claude::with_config_dir(
                    &world.home.join(".claude"),
                )),
                Harness::Codex => Box::new(crate::harness::codex::Codex::with_home(&world.home.join(".codex"))),
                Harness::Kimi => Box::new(crate::harness::kimi::Kimi::with_home(&world.home.join(".kimi"))),
                Harness::Grok => Box::new(crate::harness::grok::Grok::with_home(&world.home.join(".grok"))),
                _ => Box::new(crate::harness::kimi_code::KimiCode::with_home(
                    &world.home.join(".kimi-code"),
                )),
            };
            for file in walkdir::WalkDir::new(root.join(harness.as_str()))
                .into_iter()
                .filter_map(Result::ok)
            {
                if file.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let path = if harness == Harness::KimiCode {
                    if file.file_name() != "wire.jsonl" {
                        continue;
                    }
                    let agent = file.path().parent().unwrap();
                    if agent.file_name().unwrap() == "main" {
                        agent.parent().unwrap().parent().unwrap()
                    } else {
                        agent
                    }
                } else if harness == Harness::Kimi {
                    if file.file_name() != "context.jsonl" {
                        continue;
                    }
                    file.path().parent().unwrap()
                } else if harness == Harness::Grok {
                    if file.file_name() != "chat_history.jsonl" {
                        continue;
                    }
                    file.path().parent().unwrap()
                } else {
                    file.path()
                };
                let r = SessionRef::new("oracle", harness, path);
                for extra in [false, true] {
                    let cache = Cache::default();
                    for (start, end) in [
                        (0, Some(2)),
                        (2, Some(4)),
                        (4, Some(6)),
                        (0, Some(1)),
                        (1000, Some(1000)),
                        (1, None),
                    ] {
                        let expected = oracle(adapter.as_ref(), &r, start, end, extra);
                        let (shell, mut sink) = window(&cache, adapter.as_ref(), &r, start, end, extra).unwrap();
                        assert_eq!(
                            result(&shell, &mut sink),
                            expected,
                            "{} {start} {end:?} extra={extra}",
                            file.path().display()
                        );
                    }
                }
                checked += 1;
            }
        }
        assert!(
            checked >= 30,
            "expected frozen Claude/Codex/Kimi-Code native JSONL fixtures, found {checked}"
        );
    }

    #[test]
    fn compactions_reuse_verified_rows_and_invalidate_on_append() {
        let world = crate::test_world("api-compaction-cache");
        let r = fixture(&world, 3);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        assert_eq!(compactions(&cache, &adapter, &r).unwrap(), json!([]));
        let count = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(compactions(&cache, &adapter, &r).unwrap(), json!([]));
        assert_eq!(adapter.messages.load(Ordering::Relaxed), count);
        use std::io::Write as _;
        writeln!(std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            r#"{{"type":"system","subtype":"compact_boundary","uuid":"boundary","compactMetadata":{{"trigger":"auto","preTokens":1000}}}}"#).unwrap();
        let rows = compactions(&cache, &adapter, &r).unwrap();
        assert_eq!(rows[0]["boundary_msg_idx"], 3);
        assert!(adapter.messages.load(Ordering::Relaxed) > count);
        let count = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(compactions(&cache, &adapter, &r).unwrap(), rows);
        assert_eq!(adapter.messages.load(Ordering::Relaxed), count);
    }

    #[test]
    fn pages_resume_and_heads_reuse_only_unchanged_sources() {
        let world = crate::test_world("api_cache_invalidation");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in [0, 5, 10] {
            let expected = oracle(&inner, &r, start, Some(start + 5), false);
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 5), false).unwrap();
            assert_eq!(result(&shell, &mut sink), expected);
        }
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 18); // Five selected plus one probe per page.
        let first = head(&cache, &adapter, &r).unwrap();
        let counted = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        assert_eq!(adapter.messages.load(Ordering::Relaxed), counted);
        // An append invalidates both count and continuation rather than assuming prefix immutability.
        use std::io::Write as _;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            r#"{{"type":"user","uuid":"new","message":{{"role":"user","content":"added"}}}}"#
        )
        .unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 21);
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 15, Some(20), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 15, Some(20), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 21);
        // Switching extra flavor must read whole rather than reuse a checkpoint built lean.
        let before = adapter.messages.load(Ordering::Relaxed);
        window(&cache, &adapter, &r, 20, Some(21), true).unwrap();
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 21);
    }

    #[test]
    fn catalog_fallback_hints_are_part_of_read_identity() {
        struct Hints(AtomicUsize);
        impl Adapter for Hints {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(&self, r: &SessionRef, _: &Demand, _: &mut dyn MessageSink) -> Result<Session> {
                self.0.fetch_add(1, Ordering::Relaxed);
                let mut shell = Session::new(&r.id, r.harness);
                shell.title = r.title.clone();
                shell.cwd = r.cwd.clone();
                shell.created_at = r.created_at;
                shell.updated_at = r.updated_at;
                shell.extra.insert("catalog_count".into(), json!(r.message_count));
                shell.extra.insert("created_hint".into(), json!(r.created_at));
                shell.extra.insert("updated_hint".into(), json!(r.updated_at));
                Ok(shell)
            }
        }
        let world = crate::test_world("api_catalog_hint_identity");
        let mut r = fixture(&world, 1);
        let adapter = Hints(AtomicUsize::new(0));
        let cache = Cache::default();
        for i in 0..6 {
            match i {
                1 => r.title = Some("new catalog title".into()),
                2 => r.cwd = Some(world.home.join("new-cwd")),
                3 => r.created_at = Some("2026-01-01T00:00:00Z".parse().unwrap()),
                4 => r.updated_at = Some("2026-02-01T00:00:00Z".parse().unwrap()),
                5 => r.message_count = Some(42),
                _ => {}
            }
            let value = head(&cache, &adapter, &r).unwrap();
            assert_eq!(value["title"], json!(r.title));
            assert_eq!(value["cwd"], json!(r.cwd));
            assert_eq!(value["extra"]["created_hint"], json!(r.created_at));
            assert_eq!(value["extra"]["updated_hint"], json!(r.updated_at));
            assert_eq!(value["extra"]["catalog_count"], json!(r.message_count));
            assert_eq!(head(&cache, &adapter, &r).unwrap(), value);
            assert_eq!(adapter.0.load(Ordering::Relaxed), i + 1);
        }
        // A hint of any size is a key like any other: read once, then served (0.17 reread a
        // title over 256 KiB on every call).
        r.title = Some("x".repeat(512 * 1024));
        let prior = cache.len();
        for _ in 0..2 {
            assert_eq!(head(&cache, &adapter, &r).unwrap()["title"], json!(r.title));
        }
        assert_eq!(adapter.0.load(Ordering::Relaxed), 7);
        assert_eq!(cache.len(), prior + 1);
    }

    #[test]
    fn distinct_actual_sources_with_equal_size_and_mtime_do_not_alias() {
        struct Selected(std::path::PathBuf, AtomicUsize);
        impl Adapter for Selected {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn resume_source(&self, _: &SessionRef) -> std::path::PathBuf {
                self.0.clone()
            }
            fn stream(&self, r: &SessionRef, _: &Demand, _: &mut dyn MessageSink) -> Result<Session> {
                self.1.fetch_add(1, Ordering::Relaxed);
                let mut s = Session::new(&r.id, r.harness);
                s.title = Some(std::fs::read_to_string(&self.0)?);
                Ok(s)
            }
        }
        let world = crate::test_world("api_distinct_actual_sources");
        let r = fixture(&world, 1);
        let old = world.home.join("old-source");
        let new = world.home.join("new-source");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        let modified = std::fs::metadata(&old).unwrap().modified().unwrap();
        std::fs::File::open(&new)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let first = Selected(old, AtomicUsize::new(0));
        let second = Selected(new, AtomicUsize::new(0));
        let cache = Cache::default();
        assert_eq!(head(&cache, &first, &r).unwrap()["title"], "old");
        assert_eq!(head(&cache, &second, &r).unwrap()["title"], "new");
        assert_eq!(head(&cache, &second, &r).unwrap()["title"], "new");
        assert_eq!(second.1.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cache_keeps_entries_of_any_size_up_to_its_count_least_recently_used_out() {
        let world = crate::test_world("api_cache_budget");
        let r = fixture(&world, 1);
        let (source, reads) = captured(&r, &crate::source::primary_path(&r));
        let source = source.with_reads(&reads);
        let cache = Cache::default();
        let key = |i: usize| Key::of(&r, &r.path, format!("head{i}"), false);
        let entry = |i: usize| Entry {
            key: key(i),
            source: source.clone(),
            // A few entries past the old 256 KiB budget, the rest small.
            payload: Payload::Head(json!("x".repeat(if i < 4 { 300_000 } else { 16 }))),
        };
        for i in 0..ENTRIES {
            cache.insert(entry(i));
        }
        assert_eq!(cache.len(), ENTRIES, "no entry is too large to keep");
        assert!(cache.get(&key(0)).is_some(), "touch the oldest");
        cache.insert(entry(ENTRIES));
        assert_eq!(cache.len(), ENTRIES);
        assert!(cache.get(&key(0)).is_some(), "a used entry stays");
        assert!(cache.get(&key(1)).is_none(), "the least recently used goes");
    }
    #[test]
    fn changed_dynamic_reads_and_declined_resume_fall_back_whole() {
        let world = crate::test_world("api_dependency_invalidation");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let mut adapter = Observed::new(&inner);
        let dependency = world.home.join("absent-helper.json");
        adapter.dependency = Some(dependency.clone());
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        std::fs::write(dependency, "new metadata").unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 11);
        adapter.decline = true;
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 10, Some(15), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 10, Some(15), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 16);
    }

    #[test]
    fn mutation_during_resume_retries_without_stale_prefix_metadata() {
        let world = crate::test_world("api_resume_mutation");
        let r = fixture(&world, 20);
        let old = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"old-title\"}}\n{}",
            std::fs::read_to_string(&r.path).unwrap()
        );
        std::fs::write(&r.path, &old).unwrap();
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        *adapter.rewrite.lock().unwrap() = Some(old.replace("old-title", "new-title"));
        let (shell, mut sink) = window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 5, Some(10), false));
        assert_eq!(shell.title.as_deref(), Some("new-title"));
        assert_eq!(cache.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn same_length_rewrite_with_restored_mtime_invalidates_primary_read() {
        let world = crate::test_world("api_primary_identity");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        let modified = std::fs::metadata(&r.path).unwrap().modified().unwrap();
        let body = std::fs::read_to_string(&r.path).unwrap().replace("hello", "other");
        std::fs::write(&r.path, body).unwrap();
        std::fs::File::open(&r.path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 5, Some(10), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 11);
    }

    #[test]
    #[ignore = "paired callback proof using a frozen scratch transcript"]
    fn sequential_pages_and_repeated_heads_read_work() {
        let world = crate::test_world("api_cached_read_work");
        let r = fixture(&world, 20_000);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let expected = oracle(&inner, &r, start, Some(start + 200), false);
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(result(&shell, &mut sink), expected);
        }
        let page_callbacks = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(page_callbacks, 20_099);
        for _ in 0..5 {
            assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 20_000);
        }
        let head_callbacks = adapter.messages.load(Ordering::Relaxed) - page_callbacks;
        assert_eq!(head_callbacks, 20_000);
        eprintln!("pages=100 selected=20000 cached_page_callbacks={page_callbacks} repeated_heads=5 head_callbacks={head_callbacks}");
    }
    #[test]
    fn plain_codex_pages_reuse_matching_reader_checkpoints() {
        let world = crate::test_world("api_plain_codex_pages");
        let path = world.base.join("rollout-pages.jsonl");
        let line = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n";
        std::fs::write(&path, line.repeat(20_000)).unwrap();
        let r = SessionRef::new("pages", Harness::Codex, path);
        let inner = crate::harness::codex::Codex::with_home(&world.home.join(".codex"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(sink.selected, 200);
            assert_eq!(sink.idx, (start + 201).min(20_000));
            if start % 2000 == 0 || start == 19800 {
                assert_eq!(
                    result(&shell, &mut sink),
                    oracle(&inner, &r, start, Some(start + 200), false)
                );
            }
        }
        let pages = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(pages, 20_099);
        let first = head(&cache, &adapter, &r).unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        let heads = adapter.messages.load(Ordering::Relaxed) - pages;
        assert_eq!(heads, 20_000);
        eprintln!("codex production pages=100 selected=20000 callbacks={pages} repeated_heads=2 callbacks={heads}");
    }

    #[test]
    fn directory_pages_reuse_actual_wire_and_invalidate_on_wire_or_state_changes() {
        use std::io::Write as _;
        let world = crate::test_world("api_kimi_code_pages");
        let dir = world.home.join(".kimi-code/sessions/wd_test/session_pages");
        std::fs::create_dir_all(dir.join("agents/main")).unwrap();
        let wire = dir.join("agents/main/wire.jsonl");
        let state = dir.join("state.json");
        std::fs::write(&state, r#"{"id":"session_pages","cwd":"/old"}"#).unwrap();
        let line = "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
        std::fs::write(&wire, line.repeat(20_000)).unwrap();
        let r = SessionRef::new("pages", Harness::KimiCode, dir);
        let inner = crate::harness::kimi_code::KimiCode::with_home(&world.home.join(".kimi-code"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(sink.selected, 200);
            assert_eq!(sink.idx, (start + 201).min(20_000));
            if start % 2000 == 0 || start == 19800 {
                assert_eq!(
                    result(&shell, &mut sink),
                    oracle(&inner, &r, start, Some(start + 200), false)
                );
            }
        }
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 20_099);
        let first = head(&cache, &adapter, &r).unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 40_099);
        // Direct changes to the nested source must invalidate a directory-keyed head and page.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 20_001);
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 19900, Some(19905), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 19900, Some(19905), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 19_906);
        std::fs::write(&state, r#"{"id":"session_pages","cwd":"/new"}"#).unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 19905, Some(19910), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 19905, Some(19910), false));
        assert_eq!(shell.cwd.as_deref(), Some(std::path::Path::new("/new")));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 19_911);
        assert!(cache
            .entries()
            .iter()
            .all(|entry| serde_json::to_vec(entry).unwrap().len() < 16 * 1024));
        eprintln!("kimi-code actual API: pages=100 selected=20000 callbacks=20099; two heads callbacks=20000; wire/state changes force exact whole fallback");
    }

    #[test]
    fn source_json_paths_are_dependencies_even_without_adapter_notifications() {
        struct External {
            path: std::path::PathBuf,
            calls: AtomicUsize,
        }
        impl Adapter for External {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(&self, r: &SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                let input = crate::json::Json::source(&self.path, 0, std::fs::metadata(&self.path)?.len())?;
                let mut message = Message::new(crate::Role::Assistant);
                message.content.push(crate::Block::ToolUse {
                    id: "call".into(),
                    name: "Bash".into(),
                    namespace: None,
                    input,
                    extensions: Default::default(),
                });
                sink.message(message);
                Ok(Session::new(&r.id, r.harness))
            }
        }
        let world = crate::test_world("api_external_json_dependencies");
        let r = fixture(&world, 1);
        let path = world.home.join("external.json");
        std::fs::write(&path, r#"{"command":"old"}"#).unwrap();
        let adapter = External {
            path: path.clone(),
            calls: AtomicUsize::new(0),
        };
        let cache = Cache::default();
        head(&cache, &adapter, &r).unwrap();
        head(&cache, &adapter, &r).unwrap();
        assert_eq!(adapter.calls.load(Ordering::Relaxed), 1);
        let (before, reads) = captured(&r, &crate::source::primary_path(&r));
        let mut sink = WindowSink::page(&crate::source::primary_path(&r), 1, Some(2), false, reads);
        adapter.try_stream(&r, &Demand::LEAN.with_spans(), &mut sink).unwrap();
        let source = before.with_reads(&sink.reads);
        assert!(fresh(&r, &source, &crate::source::primary_path(&r)));
        std::fs::write(path, r#"{"command":"updated"}"#).unwrap();
        assert!(!fresh(&r, &source, &crate::source::primary_path(&r)));
        head(&cache, &adapter, &r).unwrap();
        assert_eq!(adapter.calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn large_demanded_metadata_is_returned_and_retained() {
        let world = crate::test_world("api_large_metadata_budget");
        let r = fixture(&world, 20);
        let title = "x".repeat(512 * 1024);
        let body = format!(
            "{}\n{}",
            json!({"type":"ai-title", "aiTitle":title}),
            std::fs::read_to_string(&r.path).unwrap()
        );
        std::fs::write(&r.path, body).unwrap();
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in [0, 5] {
            let (shell, _) = window(&cache, &adapter, &r, start, Some(start + 5), false).unwrap();
            assert_eq!(shell.title.as_deref(), Some(title.as_str()));
        }
        // The second page resumes at the first one's end (0.17 reread the head of a session with
        // a title over 256 KiB on every page).
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 12);
        assert_eq!(head(&cache, &adapter, &r).unwrap()["title"], title);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn discarded_giant_tool_arguments_do_not_enter_continuation_state() {
        let world = crate::test_world("api_discarded_tool_arguments");
        let r = fixture(&world, 20);
        let body = std::fs::read_to_string(&r.path).unwrap();
        let (first, rest) = body.split_once('\n').unwrap();
        let giant = json!({"type":"assistant", "uuid":"giant", "message":{"role":"assistant", "content":[{
            "type":"tool_use", "id":"tool", "name":"Edit", "input":{"file_path":"a.rs", "content":"discarded-tool-content".repeat(100_000)}
        }]}});
        std::fs::write(&r.path, format!("{first}\n{giant}\n{rest}")).unwrap();
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let cache = Cache::default();
        let (shell, mut sink) = window(&cache, &adapter, &r, 2, Some(4), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&adapter, &r, 2, Some(4), false));
        let entries = cache.entries();
        assert_eq!(entries.len(), 1);
        assert!(serde_json::to_vec(&entries[0]).unwrap().len() < 16 * 1024);
        assert!(!serde_json::to_string(&entries[0])
            .unwrap()
            .contains("discarded-tool-content"));
    }
}
