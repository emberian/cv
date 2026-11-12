//! Full-fidelity tail reads continued only from the adapters' existing verified checkpoints.
use crate::{harness::Adapter, source, stream, Message, Session, SessionRef};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CONSUMER: &str = "watch-full";
const CHECKPOINT_EVERY: u64 = 256 * 1024;

/// The state's version: the mark version ([`crate::resume::version`]). Never the cv release.
fn state_version(adapter: &dyn Adapter) -> String {
    crate::resume::version(adapter, CONSUMER)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Anchor {
    head: crate::resume::MarkHead,
    checkpoint: stream::Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Completed {
    session: Session,
    total: usize,
}

/// A completed read's source baseline and optional stable append boundary. This is consumer
/// state, not a second source index. Unsupported adapters transparently read whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadState {
    version: String,
    path: PathBuf,
    primary: Option<PathBuf>,
    pub source: source::Baseline,
    anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed: Option<Completed>,
    #[serde(skip)]
    appended: Option<source::Prefix>,
}

/// A [`ReadState`] without its baseline, for a store whose [`source::Record`] carries the
/// baseline: [`ReadState::split`] and [`ReadState::join`] keep one copy of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Held {
    version: String,
    path: PathBuf,
    primary: Option<PathBuf>,
    anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed: Option<Completed>,
}

impl ReadState {
    /// The baseline, and everything else.
    pub fn split(self) -> (source::Baseline, Held) {
        let ReadState {
            version,
            path,
            primary,
            source,
            anchor,
            completed,
            appended: _,
        } = self;
        (
            source,
            Held {
                version,
                path,
                primary,
                anchor,
                completed,
            },
        )
    }

    /// The state [`Self::split`] took apart.
    pub fn join(source: source::Baseline, held: Held) -> Self {
        let Held {
            version,
            path,
            primary,
            anchor,
            completed,
        } = held;
        ReadState {
            version,
            path,
            primary,
            source,
            anchor,
            completed,
            appended: None,
        }
    }

    pub(crate) fn baseline(r: &SessionRef) -> Self {
        Self {
            version: crate::harness::for_harness(r.harness)
                .map_or_else(String::new, |adapter| state_version(adapter.as_ref())),
            path: r.path.clone(),
            primary: Some(source::primary_path(r)),
            source: source::Probe::default().snapshot(r),
            anchor: None,
            completed: None,
            appended: None,
        }
    }

    /// Fully completed consumer read still describes this source, including dynamic dependencies.
    pub fn fresh(&self, r: &SessionRef) -> bool {
        crate::harness::for_harness(r.harness).is_some_and(|adapter| self.fresh_with(adapter.as_ref(), r))
    }

    /// The identity preamble every freshness predicate starts from: the same mark version, the
    /// same logical session, and the same primary file the state was built against.
    fn describes(&self, adapter: &dyn Adapter, r: &SessionRef, primary: &Path) -> bool {
        self.version == state_version(adapter) && self.path == r.path && self.primary.as_deref() == Some(primary)
    }

    /// Freshness against the same concrete adapter that will perform the next read.
    pub fn fresh_with(&self, adapter: &dyn Adapter, r: &SessionRef) -> bool {
        let mut probe = source::Probe::default();
        let now = probe.snapshot_from(r, &adapter.resume_source(r));
        self.describes(adapter, r, &adapter.resume_source(r)) && self.source.fresh(&now, &mut probe)
    }

    /// Whether the prefix this pass read still stands when its consumer publishes resume state:
    /// [`Self::fresh_with`] in general, or append-only growth since capture when this pass
    /// sampled its prefix (so a transcript appended to mid-pass still records its checkpoint).
    pub fn holds_with(&self, adapter: &dyn Adapter, r: &SessionRef) -> bool {
        let primary = adapter.resume_source(r);
        match self.appended.as_ref() {
            Some(prefix) => {
                self.describes(adapter, r, &primary)
                    && source::grown_from(r, &primary, &self.source, Some(prefix)).is_some()
            }
            None => self.fresh_with(adapter, r),
        }
    }

    /// The cheaper pre-read gate, distinct from [`Self::fresh`]: identity holds and the primary
    /// source is unchanged since the baseline (a check against the live file, no dependency
    /// re-probe) — enough to keep replaying a stable anchor, not enough to call the state fresh.
    pub(crate) fn baseline_unchanged(&self, r: &SessionRef) -> bool {
        crate::harness::for_harness(r.harness)
            .is_some_and(|adapter| self.describes(adapter.as_ref(), r, &source::primary_path(r)))
            && source::unchanged(r, &self.source)
    }

    /// Number of old messages covered by the accepted stable checkpoint, when available.
    pub fn stable_prefix(&self) -> Option<usize> {
        self.anchor.as_ref()?.checkpoint.messages()
    }

    /// The checkpoint suitable for a tail starting at `skip`. Growth is interpreted under the
    /// adapter's immutable-prefix contract; MarkHead samples do not prove arbitrary edits absent.
    fn from<'a>(
        &'a self,
        adapter: &dyn Adapter,
        r: &SessionRef,
        current: &source::Baseline,
        skip: usize,
    ) -> Option<&'a stream::Checkpoint> {
        if !self.describes(adapter, r, &adapter.resume_source(r))
            || current.primary_len() < self.source.primary_len()
            || !self.source.dependencies_fresh(current, &mut source::Probe::default())
        {
            return None;
        }
        // A primary that did not grow is held to its whole identity, not only the anchor's
        // sample; its dependencies are held as above (a file added to a growing directory, as a
        // harness writes one just before the record naming it, is not a reason to read whole).
        if current.primary_len() == self.source.primary_len() && current.primary != self.source.primary {
            return None;
        }
        let anchor = self.anchor.as_ref()?;
        (anchor.checkpoint.messages()? <= skip).then_some(())?;
        anchor.head.admit(
            &adapter.resume_source(r),
            &crate::resume::version(adapter, CONSUMER),
            &anchor.checkpoint,
        )
    }
}

/// EOF metadata and selected full messages, plus positions that can be accepted after a bounded
/// callback reports how much it consumed. Never checkpoint past that accepted message position.
pub struct ReadPass {
    version: String,
    pub session: Session,
    pub messages: Vec<Message>,
    pub total: usize,
    /// Whether this pass reached EOF (rather than a consumer stopping after delivery).
    pub reached_eof: bool,
    pub resumed: bool,
    /// Old messages proven stable by the adapter checkpoint. Archives replace the suffix after
    /// this boundary, including a Codex EOF message whose trailing usage may change on append.
    pub prefix: usize,
    pub parsed_messages: usize,
    /// Primary bytes from the admitted checkpoint to captured EOF, excluding auxiliary reads
    /// and guard samples. A bounded unchanged read may stop before traversing this whole suffix.
    pub suffix_bytes: u64,
    source: source::Baseline,
    path: PathBuf,
    primary: PathBuf,
    before: Option<Anchor>,
    last: Option<Anchor>,
    completed: Option<Completed>,
    appended: Option<source::Prefix>,
}

impl ReadPass {
    pub fn accepted(&self, seen_to: usize) -> ReadState {
        let anchor = self
            .last
            .as_ref()
            .filter(|anchor| anchor.checkpoint.messages().is_some_and(|n| n <= seen_to))
            .or_else(|| {
                self.before
                    .as_ref()
                    .filter(|anchor| anchor.checkpoint.messages().is_some_and(|n| n <= seen_to))
            })
            .cloned();
        ReadState {
            version: self.version.clone(),
            path: self.path.clone(),
            primary: Some(self.primary.clone()),
            source: self.source.clone(),
            anchor,
            completed: self.completed.clone(),
            appended: self.appended,
        }
    }
}

struct Tail<'a> {
    check: Option<&'a mut dyn FnMut() -> Result<()>>,
    output: Option<&'a mut dyn stream::MessageSink>,
    resolver_source: Option<PathBuf>,
    // Streaming output can resolve implicit-path spans during callbacks. Retained/owned tails
    // instead materialize under the final source and dependency checks before returning.
    // Tool input Json carries its own source identity and is recorded via record_message_reads.
    used_implicit_source: bool,
    primary: PathBuf,
    materialized_reads: source::ReadSet,
    error: Option<anyhow::Error>,
    total: usize,
    skip: usize,
    keep: usize,
    messages: Vec<Message>,
    before: Option<stream::Checkpoint>,
    last: Option<stream::Checkpoint>,
    stop_after_selection: bool,
}
impl Tail<'_> {
    fn check(&mut self) -> bool {
        if self.error.is_some() {
            return false;
        }
        if let Some(check) = self.check.as_mut() {
            if let Err(error) = check() {
                self.error = Some(error);
                return false;
            }
        }
        true
    }
}
impl stream::MessageSink for Tail<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error
            .take()
            .or_else(|| self.output.as_mut().and_then(|sink| sink.take_error()))
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if !self.check() {
            return;
        }
        if self.check.is_some() {
            self.materialized_reads.record(path);
        }
        if let Some(output) = self.output.as_mut() {
            output.dependency(path);
        }
    }
    fn meta(&mut self, session: &Session) {
        if !self.check() {
            return;
        }
        if self.output.is_some() {
            if self.used_implicit_source && session.source_path != self.resolver_source {
                self.error
                    .get_or_insert_with(|| anyhow::anyhow!("session materialization source changed during streaming"));
            }
            self.resolver_source = session.source_path.clone();
        }
        if let Some(output) = self.output.as_mut() {
            output.meta(session);
        }
    }
    fn message(&mut self, message: Message) -> stream::Flow {
        if !self.check() {
            return stream::Flow::Stop;
        }
        if let (true, Some(output)) = (self.total >= self.skip, self.output.as_mut()) {
            self.total += 1;
            self.used_implicit_source |= message.content.iter().any(|block| {
                // ToolUse input is absent here on purpose: its Json blocks carry their own source
                // identity (record_message_reads, below, records them), so a ToolUse span is
                // never an implicit-source span needing the materialization guard.
                match block {
                    crate::Block::Text { text } | crate::Block::Thinking { text, .. } => text.as_span(),
                    crate::Block::ToolResult { content, .. } => content.as_span(),
                    _ => None,
                }
                .is_some_and(|span| span.source.is_none())
            });
            source::record_message_reads(&message, Some(&self.primary), &mut self.materialized_reads);
            let result = output.try_message(message);
            return match result {
                Ok(flow) => flow,
                Err(error) => {
                    self.error = Some(error);
                    stream::Flow::Stop
                }
            };
        }
        if self.total >= self.skip && self.messages.len() < self.keep {
            self.messages.push(message);
        }
        self.total += 1;
        if self.stop_after_selection && self.total >= self.skip && self.messages.len() >= self.keep {
            stream::Flow::Stop
        } else {
            stream::Flow::Continue
        }
    }
    fn wants_checkpoint(&mut self, _: u64) -> bool {
        self.check()
    }
    // No size cap: the pass is amortized (`stream::Pass::amortized`), so the counting layer
    // spaces checkpoints at least their own encoded size apart and keeping each one costs no
    // more than the bytes it covers. A cap here stopped anchoring at the first large state and
    // made every later append re-read from the last small one.
    fn checkpoint(&mut self, cp: &mut stream::Checkpoint) {
        if cp.messages().is_some_and(|n| n <= self.skip) {
            self.before = Some(cp.clone());
        }
        self.last = Some(cp.clone());
        if let Some(output) = self.output.as_mut() {
            output.checkpoint(cp);
        }
    }
}

/// A whole or verified resumed pass, retaining only the requested tail and always reaching EOF
/// for exact metadata. No early Stop is mistaken for a complete source baseline.
pub fn read(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    skip: usize,
    keep: usize,
) -> Result<ReadPass> {
    pass(adapter, r, previous, skip, keep, Demand::default())
}

/// Full session output: on a verified append retain only the checkpoint's stable old prefix,
/// replaying its suffix; on declined/invalid reuse collect the whole session in the same pass.
pub fn read_session(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    previous_count: usize,
) -> Result<ReadPass> {
    let demand = Demand {
        session: true,
        ..Demand::default()
    };
    pass(adapter, r, previous, previous_count, usize::MAX, demand)
}

/// Full-fidelity views through one bounded consumer, preserving final metadata and verified
/// suffix admission without retaining selected history. The sink receives resolver metadata
/// through `meta` and may stream Source/Span payloads instead of owning them. Consumer errors
/// never publish state.
pub fn read_session_into(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    previous_count: usize,
    output: &mut dyn stream::MessageSink,
) -> Result<ReadPass> {
    let demand = Demand {
        session: true,
        output: Some(output),
        ..Demand::default()
    };
    pass(adapter, r, previous, previous_count, usize::MAX, demand)
}

/// What one watch read asks for. The tail past `skip` (or, with `session`, the whole session
/// resumed from its stable checkpoint prefix); payloads owned, or left in the source when
/// `retained` or when `output` streams them; an `output` that receives messages as they stream
/// (an early stop on a tail returns a checked partial read without claiming EOF metadata, so
/// the caller keeps its pending position); and a cooperative `check` at read boundaries.
#[derive(Default)]
pub(super) struct Demand<'a> {
    pub session: bool,
    pub retained: bool,
    pub output: Option<&'a mut dyn stream::MessageSink>,
    pub check: Option<&'a mut dyn FnMut() -> Result<()>>,
}

/// One watch read under its [`Demand`].
pub(super) fn pass(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    mut skip: usize,
    keep: usize,
    demand: Demand<'_>,
) -> Result<ReadPass> {
    let Demand {
        session: session_output,
        retained,
        output,
        check,
    } = demand;
    let streaming_tail = !session_output && output.is_some();
    let retained = retained || output.is_some();
    let primary = adapter.resume_source(r);
    let captured = source::Probe::default().snapshot_from(r, &primary);
    // An append-only log is read up to its captured length; its head and the bytes just before
    // that length are sampled now, so a pass that a writer appended to while it ran still
    // describes what it read, and the appended bytes are the next pass's input.
    let appended = crate::json::is_append_only_log(&primary)
        .then(|| std::fs::File::open(&primary).ok())
        .flatten()
        .and_then(|file| source::Prefix::of(&file, captured.primary_len()));
    let cached = previous
        .filter(|state| !session_output && state.fresh_with(adapter, r))
        .and_then(|state| state.completed.as_ref());
    let stop_after_selection = cached.is_some_and(|complete| skip.saturating_add(keep) < complete.total);
    // The primary's whole identity, for a pass whose dependency inventory was unavailable.
    let mut metadata_guard = source::ReadSet::default();
    metadata_guard.record_as(&primary, appended.is_some());
    let from = previous.and_then(|state| state.from(adapter, r, &captured, skip));
    if session_output {
        skip = from.and_then(stream::Checkpoint::messages).unwrap_or(0);
    }
    // Exact materialized parity is checked against the independent full collector; discarded
    // payloads remain unowned.
    let opts = if adapter.capabilities().rich_spans {
        stream::Demand::RICH.with_spans()
    } else {
        stream::Demand::RICH
    };
    // The one Tail literal, for both passes of this read: a fresh sink, and — when the adapter
    // declines the resumed pass — the same sink restarted from nothing (no checkpoint, no
    // carried-over progress), so the two spellings cannot drift.
    let tail_sink = |check, output, skip, from: Option<&stream::Checkpoint>| Tail {
        check,
        output,
        resolver_source: Some(r.path.clone()),
        used_implicit_source: false,
        primary: primary.clone(),
        materialized_reads: source::ReadSet::default(),
        error: None,
        total: from.and_then(stream::Checkpoint::messages).unwrap_or(0),
        skip,
        keep,
        messages: Vec::new(),
        before: from.cloned(),
        last: from.cloned(),
        stop_after_selection,
    };
    let mut sink = tail_sink(check, output, skip, from);
    let mut resumed = from.is_some();
    let pass = |from| stream::Pass {
        from,
        every: Some(CHECKPOINT_EVERY),
        amortized: true,
        ..Default::default()
    };
    let done = match stream::stream_counted(adapter, r, &opts, &mut sink, pass(from)) {
        Ok(done) => done,
        Err(error) if !error.is::<stream::Declined>() => return Err(error),
        Err(_) => {
            resumed = false;
            let output = sink.output.take();
            let check = sink.check.take();
            sink = tail_sink(check, output, if session_output { 0 } else { skip }, None);
            stream::stream_counted(adapter, r, &opts, &mut sink, pass(None))?
        }
    };
    let complete = done.counted.is_some();
    let mut reads = if let Some(counted) = done.counted {
        counted.dependencies
    } else if streaming_tail {
        source::ReadSet::default()
    } else {
        anyhow::ensure!(stop_after_selection, "session read stopped before EOF");
        previous
            .and_then(|state| state.source.reads())
            .context("incomplete cached source read set")?
    };
    if resumed {
        if let Some(prefix_reads) = previous.and_then(|state| state.source.reads()) {
            reads.merge(&prefix_reads);
        }
    }
    anyhow::ensure!(
        !sink.used_implicit_source || sink.resolver_source == done.session.source_path,
        "session materialization source changed before EOF"
    );
    reads.merge(&sink.materialized_reads);
    let resolver = done.session.resolver();
    for message in &mut sink.messages {
        source::record_message_reads(message, Some(&primary), &mut reads);
        if !retained {
            message.materialize(&resolver)?;
        }
    }
    let source = captured.with_reads(&reads);
    let version = crate::resume::version(adapter, CONSUMER);
    let anchor = |cp: &stream::Checkpoint| {
        Some(Anchor {
            head: crate::resume::MarkHead::of(&primary, cp, &version)?,
            checkpoint: cp.clone(),
        })
    };
    let before = sink.before.as_ref().and_then(anchor);
    let last = done.last.as_ref().or(sink.last.as_ref()).and_then(anchor);
    let now = || source::Probe::default().snapshot_from(r, &primary).primary;
    anyhow::ensure!(
        match (&appended, source.stamp.is_some()) {
            (Some(prefix), true) => source::grown_from(r, &primary, &source, Some(prefix)).is_some(),
            (Some(prefix), false) => source
                .primary
                .zip(now())
                .is_some_and(|(then, now)| then.grew_into(prefix, &now, &primary)),
            (None, true) => source::unchanged_from(r, &primary, &source),
            (None, false) => now() == source.primary,
        },
        "session source changed during read: {}",
        r.path.display()
    );
    anyhow::ensure!(
        reads.fresh(&mut source::Probe::default()),
        "session dependencies changed during read: {}",
        r.path.display()
    );
    anyhow::ensure!(
        metadata_guard.fresh_or_grown(&mut source::Probe::default()),
        "session identity changed during read: {}",
        r.path.display()
    );
    let first = if resumed {
        from.and_then(stream::Checkpoint::messages).unwrap_or(0)
    } else {
        0
    };
    let (session, total, completed) = if complete {
        // The metadata the read already holds, whatever its size: one per followed session. A
        // budget here silently sent every later bounded read of a large session to EOF.
        let completed = Some(Completed {
            session: done.session.clone(),
            total: sink.total,
        });
        (done.session, sink.total, completed)
    } else if streaming_tail {
        (done.session, sink.total, None)
    } else {
        let cached = cached.context("missing complete EOF metadata")?;
        (cached.session.clone(), cached.total, Some(cached.clone()))
    };
    Ok(ReadPass {
        version: state_version(adapter),
        session,
        total,
        reached_eof: complete,
        parsed_messages: sink.total - first,
        suffix_bytes: captured
            .primary_len()
            .saturating_sub(if resumed { from.map_or(0, |cp| cp.offset) } else { 0 }),
        messages: sink.messages,
        resumed,
        prefix: first,
        source,
        path: r.path.clone(),
        primary,
        before,
        last,
        completed,
        appended,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness;
    use crate::Harness;

    #[test]
    fn materialized_read_sets_keep_original_text_and_argument_guards() {
        let world = crate::test_world("watch-source-guards");
        let path = world.home.join("source.json");
        let sidecar = world.home.join("optional-meta.json");
        let body = serde_json::to_vec(&"text".repeat(4096)).unwrap();
        std::fs::write(&path, &body).unwrap();
        let mut guard = source::ReadSet::default();
        guard.record(&sidecar);
        let value = crate::Json::source_with_reads(&path, 0, body.len() as u64, &guard).unwrap();
        let text = value.with_view(|view| view.source_text()).unwrap().unwrap();
        assert!(matches!(text, crate::Text::Source(_)));
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(crate::Block::Text { text });
        let mut text_reads = source::ReadSet::default();
        source::record_message_reads(&message, Some(&path), &mut text_reads);
        message.content = vec![crate::Block::ToolUse {
            id: "call".into(),
            name: "test".into(),
            namespace: None,
            input: value,
        }];
        let mut input_reads = source::ReadSet::default();
        source::record_message_reads(&message, Some(&path), &mut input_reads);
        assert!(text_reads.fresh(&mut source::Probe::default()));
        assert!(input_reads.fresh(&mut source::Probe::default()));
        std::fs::write(&sidecar, "created after capture").unwrap();
        assert!(!text_reads.fresh(&mut source::Probe::default()));
        assert!(!input_reads.fresh(&mut source::Probe::default()));
    }

    #[test]
    fn different_concrete_primary_files_cannot_reuse_equal_stat_metadata() {
        struct Foreign(PathBuf);
        impl Adapter for Foreign {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn resume_source(&self, _: &SessionRef) -> PathBuf {
                self.0.clone()
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &stream::Demand,
                sink: &mut dyn stream::MessageSink,
            ) -> Result<Session> {
                let mut session = Session::new(&r.id, r.harness);
                session.title = Some(self.0.file_name().unwrap().to_string_lossy().into());
                session.source_path = Some(self.0.clone());
                sink.meta(&session);
                sink.try_message(Message::new(crate::Role::User))?;
                sink.try_message(Message::new(crate::Role::User))?;
                Ok(session)
            }
        }
        let dir = std::env::temp_dir().join(format!("cv-watch-foreign-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.jsonl"), dir.join("b.jsonl"));
        std::fs::write(&a, "same").unwrap();
        std::fs::write(&b, "same").unwrap();
        let modified = std::fs::metadata(&a).unwrap().modified().unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&b)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(
            source::FileId::at(&a).map(|id| (id.len, id.mtime_ns)),
            source::FileId::at(&b).map(|id| (id.len, id.mtime_ns))
        );
        let r = SessionRef::new("foreign", Harness::Codex, dir.join("logical"));
        let first = read(&Foreign(a), &r, None, 0, 0).unwrap();
        let state = first.accepted(first.total);
        assert!(!state.fresh_with(&Foreign(b.clone()), &r));
        let second = read(&Foreign(b), &r, Some(&state), 0, 0).unwrap();
        assert_eq!(second.session.title.as_deref(), Some("b.jsonl"));
        assert_eq!(second.total, 2);
        let mut old = state;
        old.primary = None;
        assert!(!old.fresh_with(&Foreign(dir.join("a.jsonl")), &r));
        let mut failed = stream::TryFnSink::new(|_| anyhow::bail!("owned output failed"));
        assert!(read_session_into(&Foreign(dir.join("a.jsonl")), &r, None, 0, &mut failed).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_materialization_never_establishes_a_completed_read() {
        struct RemovesInput(PathBuf);
        impl Adapter for RemovesInput {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &stream::Demand,
                sink: &mut dyn stream::MessageSink,
            ) -> Result<Session> {
                let mut message = Message::new(crate::Role::Assistant);
                message.content.push(crate::Block::ToolUse {
                    id: "call".into(),
                    name: "opaque".into(),
                    namespace: None,
                    input: crate::Json::source(&self.0, 0, 2)?,
                });
                sink.try_message(message)?;
                std::fs::remove_file(&self.0)?;
                Ok(Session::new(&r.id, r.harness))
            }
        }
        let dir = std::env::temp_dir().join(format!("cv-watch-read-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("primary.jsonl");
        let external = dir.join("input.json");
        std::fs::write(&path, "primary").unwrap();
        std::fs::write(&external, "{}").unwrap();
        let r = SessionRef::new("read", Harness::Codex, path);
        assert!(read_session(&RemovesInput(external), &r, None, 0).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn followed_full_messages_and_eof_metadata_equal_the_original_parse() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut checked = 0;
        for (harness, directory) in [(Harness::Claude, "claude"), (Harness::Codex, "codex")] {
            let adapter = harness::for_harness(harness).unwrap();
            for path in std::fs::read_dir(fixtures.join(directory)).unwrap() {
                let path = path.unwrap().path();
                if !path.is_file() || !path.extension().is_some_and(|ext| ext == "jsonl" || ext == "json") {
                    continue;
                }
                let r = SessionRef::new("fidelity", harness, &path);
                let Ok(expected) = adapter.parse(&r) else { continue };
                let mut pass = read(adapter.as_ref(), &r, None, 0, usize::MAX).unwrap();
                pass.session.messages = std::mem::take(&mut pass.messages);
                assert_eq!(
                    serde_json::to_value(pass.session).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{}",
                    path.display()
                );
                let mut messages = Vec::new();
                let mut output = stream::TryFnSink::new(|message| {
                    messages.push(message);
                    Ok(stream::Flow::Continue)
                });
                let mut streamed = read_session_into(adapter.as_ref(), &r, None, 0, &mut output).unwrap();
                assert!(streamed.messages.is_empty());
                let resolver = streamed.session.resolver();
                for message in &mut messages {
                    message.materialize(&resolver).unwrap();
                }
                streamed.session.messages = messages;
                assert_eq!(
                    serde_json::to_value(streamed.session).unwrap(),
                    serde_json::to_value(adapter.parse(&r).unwrap()).unwrap(),
                    "streamed {}",
                    path.display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 20, "only {checked} fidelity fixtures");
        let scratch = std::env::temp_dir()
            .join(format!("cv-watch-fidelity-{}", uuid::Uuid::new_v4()))
            .join("chats");
        std::fs::create_dir_all(&scratch).unwrap();
        for (harness, rel) in [
            (Harness::Gemini, "gemini/session_0_46_session_context.jsonl"),
            (Harness::Gemini, "gemini/checkpoint_legacy_array.json"),
            (Harness::OpenClaw, "openclaw/v3-full.jsonl"),
            (Harness::OpenClaw, "openclaw/acp-session.jsonl"),
            (Harness::LmStudio, "lmstudio/sample.conversation.json"),
            (Harness::LmStudio, "lmstudio/harmony.conversation.json"),
            (Harness::Cline, "cline/api_conversation_history.json"),
            (Harness::Roo, "roo/api_conversation_history.json"),
        ] {
            let r = if harness == Harness::Gemini {
                let path = scratch.join(if rel.ends_with(".json") {
                    "checkpoint-legacy-array.json"
                } else {
                    "session.jsonl"
                });
                let text = std::fs::read_to_string(fixtures.join(rel)).unwrap();
                std::fs::write(&path, &text).unwrap();
                let id = crate::harness::gemini::parse_all_str(&text, Some(path.clone())).unwrap()[0]
                    .id
                    .clone();
                SessionRef::new(id, harness, path)
            } else if matches!(harness, Harness::Cline | Harness::Roo) {
                SessionRef::new("fidelity", harness, fixtures.join(rel).parent().unwrap().to_path_buf())
            } else {
                SessionRef::new("fidelity", harness, fixtures.join(rel))
            };
            let adapter = harness::for_harness(harness).unwrap();
            let expected = adapter.parse(&r).unwrap();
            let mut pass = read_session(adapter.as_ref(), &r, None, 0).unwrap();
            pass.session.messages = pass.messages;
            assert_eq!(
                serde_json::to_value(pass.session).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{rel}"
            );
        }
        std::fs::remove_dir_all(scratch.parent().unwrap()).unwrap();
    }

    #[test]
    fn directory_wire_continuation_keeps_exact_output_and_invalidates_on_controls() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("cv-watch-directory-{}", uuid::Uuid::new_v4()));
        let wire = dir.join("agents/main/wire.jsonl");
        std::fs::create_dir_all(wire.parent().unwrap()).unwrap();
        let controls = dir.join("state.json");
        std::fs::write(&controls, "{\"cwd\":\"/before\"}").unwrap();
        let line = format!(
            "{}\n",
            serde_json::json!({"type":"context.append_message",
            "message":{"role":"user","content":"x".repeat(128)}})
        );
        std::fs::write(&wire, line.repeat(4096)).unwrap();
        let r = SessionRef::new("directory", Harness::KimiCode, &dir);
        let adapter = harness::for_harness(r.harness).unwrap();
        assert_eq!(adapter.resume_source(&r), wire);
        let first = read_session(adapter.as_ref(), &r, None, 0).unwrap();
        assert_eq!(first.total, 4096);
        let state = first.accepted(first.total);
        assert!(state.fresh_with(adapter.as_ref(), &r));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap()
            .write_all(line.repeat(4).as_bytes())
            .unwrap();
        let suffix = read_session(adapter.as_ref(), &r, Some(&state), 4096).unwrap();
        assert!(suffix.resumed);
        assert!(suffix.parsed_messages < 2048);
        let expected = adapter.parse(&r).unwrap();
        let mut reconstructed = suffix.session.clone();
        reconstructed.messages = first.messages[..suffix.prefix].to_vec();
        reconstructed.messages.extend(suffix.messages.clone());
        assert_eq!(
            serde_json::to_value(reconstructed).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        let state = suffix.accepted(suffix.total);
        let pending = read(adapter.as_ref(), &r, Some(&state), 1, 1).unwrap();
        assert_eq!(pending.total, 4100);
        assert_eq!(pending.parsed_messages, 2);
        assert_eq!(
            serde_json::to_value(&pending.messages).unwrap(),
            serde_json::to_value(&expected.messages[1..2]).unwrap()
        );
        std::fs::write(&controls, "{\"cwd\":\"/changed\"}").unwrap();
        assert!(!state.fresh_with(adapter.as_ref(), &r));
        let changed = read_session(adapter.as_ref(), &r, Some(&state), 4100).unwrap();
        assert!(!changed.resumed);
        assert_eq!(changed.session.cwd, Some(PathBuf::from("/changed")));
        assert_eq!(changed.parsed_messages, 4100);
        let changed_state = changed.accepted(changed.total);
        std::fs::write(&wire, &line).unwrap();
        let truncated = read_session(adapter.as_ref(), &r, Some(&changed_state), 4100).unwrap();
        assert!(!truncated.resumed);
        assert_eq!(truncated.total, 1);
        eprintln!(
            "directory watch: whole=4096, append={}, pending={}, checkpoint_bytes={}",
            suffix.parsed_messages,
            pending.parsed_messages,
            serde_json::to_vec(&state.anchor).unwrap().len()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_checkpoint_state_past_64_kib_still_anchors_an_append() {
        let dir = std::env::temp_dir().join(format!("cv-watch-wide-anchor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.jsonl");
        // The session's cwd rides in every checkpoint, so each one encodes above 64 KiB.
        let cwd = format!("/{}", "w".repeat(80 * 1024));
        let line = |i: usize, cwd: Option<&str>| {
            let mut v = serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"wide",
                "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}});
            if let Some(cwd) = cwd {
                v["cwd"] = cwd.into();
            }
            format!("{v}\n")
        };
        let mut body: String = (0..4096).map(|i| line(i, (i == 0).then_some(cwd.as_str()))).collect();
        std::fs::write(&path, &body).unwrap();
        let r = SessionRef::new("wide", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let first = read(adapter.as_ref(), &r, None, 0, 0).unwrap();
        assert_eq!(first.total, 4096);
        let state = first.accepted(first.total);
        let anchor = state.anchor.as_ref().expect("a wide state still anchors");
        assert!(serde_json::to_vec(&anchor.checkpoint).unwrap().len() > 64 * 1024);
        body.extend((4096..4100).map(|i| line(i, None)));
        std::fs::write(&path, &body).unwrap();
        // One append, one suffix read: not a re-read of the whole transcript.
        let next = read(adapter.as_ref(), &r, Some(&state), 4096, usize::MAX).unwrap();
        assert!(next.resumed);
        assert!(next.parsed_messages < 1024, "replayed {}", next.parsed_messages);
        let expected = adapter.parse(&r).unwrap();
        assert_eq!(expected.cwd, Some(PathBuf::from(&cwd)));
        assert_eq!(
            serde_json::to_value(&next.messages).unwrap(),
            serde_json::to_value(&expected.messages[4096..]).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A live transcript the harness writes to while it is read: the pass ends at the length it
    /// captured, the bytes and the spilled output appended meanwhile (including creating
    /// `tool-results/` on the first spill) are the next pass's input, and that pass reads only
    /// them.
    #[test]
    fn an_append_during_a_read_is_the_next_reads_input() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("cv-watch-live-append-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let spilled = dir.join("live/tool-results");
        let path = dir.join("live.jsonl");
        let line = |i: usize| {
            format!(
                "{}\n",
                serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"live",
                "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}})
            )
        };
        std::fs::write(&path, (0..4096).map(line).collect::<String>()).unwrap();
        let appended: String = (4096..4100).map(line).collect();
        let r = SessionRef::new("live", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let mut writer = Some(());
        let mut output = stream::TryFnSink::new(|_| {
            if writer.take().is_some() {
                std::fs::create_dir_all(&spilled).unwrap();
                std::fs::write(spilled.join("first.txt"), "spilled during the read").unwrap();
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(appended.as_bytes())
                    .unwrap();
            }
            Ok(stream::Flow::Continue)
        });
        let first = read_session_into(adapter.as_ref(), &r, None, 0, &mut output).expect("growth is not a change");
        assert_eq!(first.total, 4096, "the pass ends where its capture did");
        let state = first.accepted(first.total);
        assert!(
            !state.fresh_with(adapter.as_ref(), &r),
            "the appended bytes are still unread"
        );
        let next = read_session(adapter.as_ref(), &r, Some(&state), first.total).unwrap();
        assert!(next.resumed, "no reread from the first byte");
        assert_eq!(next.suffix_bytes, appended.len() as u64, "only the appended bytes");
        assert!(next.parsed_messages < 1024, "replayed {}", next.parsed_messages);
        assert_eq!(next.total, 4100);
        let expected = adapter.parse(&r).unwrap();
        assert_eq!(
            serde_json::to_value(&next.messages[next.messages.len() - 4..]).unwrap(),
            serde_json::to_value(&expected.messages[4096..]).unwrap()
        );
        // A file added beside the session between passes is not a reason to read whole either.
        let state = next.accepted(next.total);
        std::fs::write(spilled.join("second.txt"), "spilled before its record").unwrap();
        let idle = read_session(adapter.as_ref(), &r, Some(&state), next.total).unwrap();
        assert!(idle.resumed);
        assert_eq!(idle.suffix_bytes, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// What an append-only reader still refuses: the bytes it read changed under it. A shrink, a
    /// rewrite of the head it read, a spilled output removed from beside the session, and a
    /// same-length rewrite outside the samples (no growth to admit).
    #[cfg(unix)]
    #[test]
    fn a_truncation_or_rewrite_during_a_read_still_rejects_it() {
        use std::os::unix::fs::FileExt as _;
        let dir = std::env::temp_dir().join(format!("cv-watch-live-change-{}", uuid::Uuid::new_v4()));
        let spilled = dir.join("live/tool-results");
        std::fs::create_dir_all(&spilled).unwrap();
        let path = dir.join("live.jsonl");
        let body: String = (0..2048)
            .map(|i| {
                format!(
                    "{}\n",
                    serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"live",
                    "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}})
                )
            })
            .collect();
        let r = SessionRef::new("live", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let changes: [(&str, &dyn Fn()); 4] = [
            ("truncated", &|| {
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.set_len(body.len() as u64 - 1).unwrap();
            }),
            ("head rewritten", &|| {
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                let at = body.find("xxx").unwrap() as u64;
                file.write_all_at(b"y", at).unwrap();
            }),
            ("spill removed", &|| {
                std::fs::remove_file(spilled.join("kept.txt")).unwrap();
            }),
            ("middle rewritten in place", &|| {
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                let at = body.len() as u64 / 2;
                assert!(at > 2 * source::SAMPLE_SPAN && at + 2 * source::SAMPLE_SPAN < body.len() as u64);
                file.write_all_at(b"y", at).unwrap();
            }),
        ];
        for (name, change) in changes {
            std::fs::write(&path, &body).unwrap();
            std::fs::write(spilled.join("kept.txt"), "spilled").unwrap();
            let mut once = Some(());
            let mut output = stream::TryFnSink::new(|_| {
                if once.take().is_some() {
                    change();
                }
                Ok(stream::Flow::Continue)
            });
            assert!(
                read_session_into(adapter.as_ref(), &r, None, 0, &mut output).is_err(),
                "{name}: accepted"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A capture that lands inside a record (the writer mid-line): the pass ends at the captured
    /// length without that record, the writer finishes it during the pass, and the next pass
    /// reads it whole, once.
    #[test]
    fn a_record_torn_at_the_capture_is_the_next_reads() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("cv-watch-torn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live.jsonl");
        let line = |i: usize| {
            format!(
                "{}\n",
                serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"live",
                "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}})
            )
        };
        let torn = line(64);
        let (head, rest) = torn.split_at(torn.len() / 2);
        std::fs::write(&path, (0..64).map(line).collect::<String>() + head).unwrap();
        let appended = format!("{rest}{}", line(65));
        let r = SessionRef::new("live", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let mut writer = Some(());
        let mut output = stream::TryFnSink::new(|_| {
            if writer.take().is_some() {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(appended.as_bytes())
                    .unwrap();
            }
            Ok(stream::Flow::Continue)
        });
        let first = read_session_into(adapter.as_ref(), &r, None, 0, &mut output).expect("growth is not a change");
        assert_eq!(first.total, 64, "the torn record is not the first pass's");
        let state = first.accepted(first.total);
        let next = read_session(adapter.as_ref(), &r, Some(&state), first.total).unwrap();
        assert_eq!(next.total, 66);
        let expected = adapter.parse(&r).unwrap();
        assert_eq!(expected.messages.len(), 66);
        assert_eq!(
            serde_json::to_value(&next.messages[next.messages.len() - 2..]).unwrap(),
            serde_json::to_value(&expected.messages[64..]).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_session_with_large_metadata_keeps_its_completed_read() {
        // 0.17 kept a completed read's metadata only under 16 KiB: past it, every bounded read
        // of the unchanged session went on to EOF.
        let dir = std::env::temp_dir().join(format!("cv-watch-large-meta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.jsonl");
        let mut body =
            serde_json::json!({"type":"ai-title","sessionId":"meta","aiTitle":"t".repeat(32 * 1024)}).to_string();
        body.push('\n');
        for i in 0..512 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"meta", "message":{"role":"user","content":format!("{i}")}}).to_string());
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();
        let r = SessionRef::new("meta", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let offered = read(adapter.as_ref(), &r, None, 0, 1).unwrap();
        let pending = read(adapter.as_ref(), &r, Some(&offered.accepted(1)), 1, 1).unwrap();
        assert_eq!(pending.parsed_messages, 2, "stops after the selection");
        assert_eq!(pending.total, 512);
        assert_eq!(pending.session.title.as_deref().map(str::len), Some(32 * 1024));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_resume_and_pending_replay_match_the_full_parse_with_less_work() {
        let dir = std::env::temp_dir().join(format!("cv-watch-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("read.jsonl");
        let mut body = String::new();
        for i in 0..8192 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read", "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}}).to_string());
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();
        let r = SessionRef::new("read", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let first = read(adapter.as_ref(), &r, None, 0, 0).unwrap();
        assert_eq!(first.total, 8192);
        let offered = read(adapter.as_ref(), &r, None, 0, 1).unwrap();
        let pending_state = offered.accepted(1);
        let mut old_version = pending_state.clone();
        old_version.version.push_str("/old");
        assert_eq!(
            read(adapter.as_ref(), &r, Some(&old_version), 1, 1)
                .unwrap()
                .parsed_messages,
            8192
        );
        let pending = read(adapter.as_ref(), &r, Some(&pending_state), 1, 1).unwrap();
        assert_eq!(pending.total, 8192);
        assert_eq!(pending.parsed_messages, 2);
        assert_eq!(
            serde_json::to_value(&pending.session).unwrap(),
            serde_json::to_value(&offered.session).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&pending.messages).unwrap(),
            serde_json::to_value(&adapter.parse(&r).unwrap().messages[1..2]).unwrap()
        );
        let state = first.accepted(first.total);
        for i in 8192..8196 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read", "message":{"role":"user","content":format!("new {i}")}}).to_string());
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();
        let next = read(adapter.as_ref(), &r, Some(&state), 8192, 1).unwrap();
        assert!(next.resumed);
        assert!(
            next.parsed_messages < 1024,
            "replayed {} prefix messages",
            next.parsed_messages
        );
        let expected = adapter.parse(&r).unwrap();
        assert_eq!(
            serde_json::to_value(&next.messages).unwrap(),
            serde_json::to_value(&expected.messages[8192..8193]).unwrap()
        );
        let state = next.accepted(8193);
        let bounded = read(adapter.as_ref(), &r, Some(&state), 8193, 1).unwrap();
        assert!(bounded.parsed_messages < next.parsed_messages);
        assert_eq!(bounded.total, expected.messages.len());
        assert_eq!(
            serde_json::to_value(&bounded.messages).unwrap(),
            serde_json::to_value(&expected.messages[8193..8194]).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&bounded.session).unwrap(),
            serde_json::to_value(&next.session).unwrap()
        );
        let rest = read(adapter.as_ref(), &r, Some(&state), 8193, usize::MAX).unwrap();
        assert!(rest.resumed);
        assert_eq!(
            serde_json::to_value(&rest.messages).unwrap(),
            serde_json::to_value(&expected.messages[8193..]).unwrap()
        );
        let mut got = rest.session;
        got.messages = expected.messages.clone();
        assert_eq!(
            serde_json::to_value(got).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // Preserve size+mtime and the sampled bytes, but rewrite an unsampled prefix record.
        // Its changed ctime must decline both unchanged EOF reuse and checkpoint admission.
        let complete = next.accepted(next.total);
        let modified = std::fs::metadata(&r.path).unwrap().modified().unwrap();
        let middle = body.len() / 2;
        let changed = middle + body[middle..].find("xxx").unwrap();
        // Edit the unsampled middle: outside the first and checkpoint-adjacent 4096-byte
        // guards. The timestamp-preserving rewrite exercises the additional identity check.
        assert!(changed > 4096 && changed + 4096 < complete.anchor.as_ref().unwrap().checkpoint.offset as usize);
        body.replace_range(changed..changed + 1, "y");
        std::fs::write(&r.path, &body).unwrap();
        std::fs::File::open(&r.path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let tuple = |source: &source::Baseline| source.primary.map(|id| (id.len, id.mtime_ns));
        assert_eq!(tuple(&source::Probe::default().snapshot(&r)), tuple(&complete.source));
        let anchor = complete.anchor.as_ref().unwrap();
        assert!(anchor
            .head
            .admit(&adapter.resume_source(&r), &anchor.head.version, &anchor.checkpoint)
            .is_some());
        assert!(!complete.fresh_with(adapter.as_ref(), &r));
        let mut rewritten = read_session(adapter.as_ref(), &r, Some(&complete), next.total).unwrap();
        assert!(!rewritten.resumed);
        rewritten.session.messages = rewritten.messages;
        assert_eq!(
            serde_json::to_value(rewritten.session).unwrap(),
            serde_json::to_value(adapter.parse(&r).unwrap()).unwrap()
        );
        eprintln!(
            "watch prefix 8192: initial {}, append {}, pending {} messages interpreted",
            first.parsed_messages, next.parsed_messages, rest.parsed_messages
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn qwen_native_and_legacy_full_metadata_and_extras_are_exact() {
        let dir = std::env::temp_dir().join(format!("cv-watch-qwen-{}", uuid::Uuid::new_v4()));
        let chats = dir.join("projects/group/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let path = chats.join("read.jsonl");
        let r = SessionRef::new("s1", Harness::Qwen, &path);
        let native = [
            serde_json::json!({"uuid":"u1","sessionId":"s1","cwd":"/scratch","type":"user","timestamp":"2026-10-11T00:00:01Z","extra_unknown":{"a":3},"message":{"role":"user","parts":[{"text":"hello"}]}}),
            serde_json::json!({"uuid":"a1","sessionId":"s1","type":"assistant","model":"test","message":{"role":"model","parts":[{"text":"hm","thought":true},{"functionCall":{"id":"c1","name":"Read","args":{"file_path":"/scratch/a","nested":{"a":4}}}}]},"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5}}),
            serde_json::json!({"uuid":"r1","sessionId":"s1","type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"id":"c1","name":"Read","response":{"output":"ok"}}}]},"toolCallResult":{"callId":"c1","status":"success"}}),
        ];
        std::fs::write(&path, native.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
        let adapter = harness::for_harness(Harness::Qwen).unwrap();
        let mut pass = read_session(adapter.as_ref(), &r, None, 0).unwrap();
        pass.session.messages = pass.messages;
        assert_eq!(
            serde_json::to_value(pass.session).unwrap(),
            serde_json::to_value(adapter.parse(&r).unwrap()).unwrap()
        );
        for name in ["session_modern.jsonl", "checkpoint.json"] {
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/gemini")
                .join(name);
            let out = chats.join(if name == "checkpoint.json" {
                "checkpoint-test.json"
            } else {
                name
            });
            std::fs::copy(fixture, &out).unwrap();
            let r = SessionRef::new(
                if name == "checkpoint.json" {
                    "checkpoint-test"
                } else {
                    "legacy"
                },
                Harness::Qwen,
                out,
            );
            let mut pass = read_session(adapter.as_ref(), &r, None, 0).unwrap();
            pass.session.messages = pass.messages;
            assert_eq!(
                serde_json::to_value(pass.session).unwrap(),
                serde_json::to_value(adapter.parse(&r).unwrap()).unwrap(),
                "{name}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
