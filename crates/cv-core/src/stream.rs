//! Streaming parse: the message stream — not the whole [`Session`] — is the fundamental unit.
//!
//! [`Adapter::parse`](crate::harness::Adapter::parse) materializes an entire transcript into one
//! owned `Session { messages: Vec<Message> }`. That sets the memory floor for everything: a 1 GB
//! transcript becomes several GB of `Vec<Message>` + owned `String`s, even when the consumer only
//! makes a single forward pass (index, export, dataset, live-search).
//!
//! [`Adapter::stream`](crate::harness::Adapter::stream) instead hands each [`Message`] to a
//! [`MessageSink`] and drops it before reading the next. Retention depends on the adapter and
//! consumer: selected payloads may remain checked source ranges, while owned fallbacks still
//! scale with their largest message. A collecting sink retains the entire history. Every adapter
//! implements `stream` (a store that can only be read whole hands its parsed session to
//! [`replay`]); the whole-`Session` `parse` is "stream into a [`CollectSink`]" ([`collect`]), a
//! blanket [`crate::harness::Parse`] impl no adapter overrides.
//!
//! **Who owns the source guarantee.** A reader captures its source (identity, length, the
//! dependencies it consults) before it reads a byte and binds its descriptor to that capture. It
//! rechecks only where something can have changed under it, and one owner places each check: the
//! guard of a JSONL read ([`Guard`], [`jsonl::drive`]) after a slow sink callback
//! (`SLOW_CALLBACK`), after the session metadata and after a checkpoint; the line pass once per
//! window of mapped input; `Read::check` at EOF (descriptor and path). A
//! consumer that publishes (a writer's commit, a cache insert, an archive rename) rechecks once,
//! immediately before publishing; a span demand checks its own source (`lazy::Resolver`). Emitters
//! also preflight before creating output so an already-unreadable source leaves no files behind.
//! Any other recheck duplicates one of these.

use crate::harness::{store::jsonl, Adapter};
use crate::ir::{Message, Session, SessionRef};
use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// How much of a session a read asks for. The tiers are ordered: each one is everything the one
/// below it is plus more, so `tier >= Tier::Facts` reads "at least the reducer facts".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// The listing facts discovery reads (id, cwd, title, timestamps, writer version); a codec
    /// that has a discovery path builds no message bodies, others read as [`Tier::Lean`].
    Discovery,
    /// Faithful message text and first-class IR fields, no harness metadata. The default.
    #[default]
    Lean,
    /// [`Tier::Lean`] plus the provider response identity usage deduplication needs (Claude's
    /// `message.id`), without other metadata.
    Usage,
    /// [`Tier::Usage`] plus the shared reducer facts (doctor, compaction, affordances), selected
    /// before unrelated large metadata is decoded and kept in their usual bag / tool-detail
    /// places. An adapter with a facts path names its facts (Claude, Kimi Code, Hermes); every
    /// other adapter returns [`Tier::Rich`] metadata, so a consumer never depends on a field's
    /// absence. Session facts written at EOF still arrive after a selected window. Internal
    /// reducers only: never JSON output, conversion, redaction or watch.
    Facts,
    /// Every harness-specific field and structured sidecar (`toolUseResult` and kin).
    Rich,
    /// Format-complete: [`Tier::Rich`] plus every record carried, meta records included, as
    /// round-trippable carrier messages, so `parse → emit` loses nothing.
    Complete,
}

/// A read's demand: the [`Tier`] of content it needs, and two orthogonal delivery choices.
///
/// Pick the smallest tier the consumer reads: re-emit or round-trip ⇒ [`Demand::COMPLETE`];
/// `--json` and rich views ⇒ [`Demand::RICH`]; reducers ⇒ [`Demand::FACTS`]; usage totals ⇒
/// [`Demand::USAGE`]; text ⇒ [`Demand::LEAN`]. Add [`with_spans`](Demand::with_spans) when the
/// consumer skips, windows or chunks large fields (they arrive as checked source ranges;
/// adapters without a span path hand inline content, so spans are never less correct).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Demand {
    pub tier: Tier,
    /// Large content stays in the source as [`Span`](crate::lazy::Span)s / checked source
    /// ranges instead of owned strings. Adapters without span support ignore it.
    pub spans: bool,
    /// Stamp each message's record byte offset into
    /// `Message.extra[`[`offsets::OFFSET_KEY`](crate::offsets::OFFSET_KEY)`]` (`cv rewind`'s
    /// message-to-byte pass). Only span-tracking JSONL paths (Claude, Codex) stamp.
    pub offsets: bool,
}

impl Demand {
    pub const DISCOVERY: Demand = Demand::of(Tier::Discovery);
    pub const LEAN: Demand = Demand::of(Tier::Lean);
    pub const USAGE: Demand = Demand::of(Tier::Usage);
    pub const FACTS: Demand = Demand::of(Tier::Facts);
    pub const RICH: Demand = Demand::of(Tier::Rich);
    pub const COMPLETE: Demand = Demand::of(Tier::Complete);

    /// The tier, inline, without offsets.
    pub const fn of(tier: Tier) -> Self {
        Demand {
            tier,
            spans: false,
            offsets: false,
        }
    }

    /// The same demand with large content left in the source.
    pub const fn with_spans(self) -> Self {
        Demand { spans: true, ..self }
    }

    /// The same demand, inline.
    pub const fn inline(self) -> Self {
        Demand { spans: false, ..self }
    }

    /// The same demand with record offsets stamped.
    pub const fn with_offsets(self) -> Self {
        Demand { offsets: true, ..self }
    }

    /// The tier one message is built at, given its sink's [`MessageSink::tier`]: a sink may
    /// narrow a [`Tier::Rich`] read to [`Tier::Facts`], never below and never a complete read.
    pub fn for_sink(&self, sink: &dyn MessageSink) -> Tier {
        if self.tier == Tier::Rich {
            sink.tier().clamp(Tier::Facts, Tier::Rich)
        } else {
            self.tier
        }
    }
}

/// The one early-stop signal. A chunk, character or record callback that has what it needs
/// returns `Err(Stop.into())`; the code that started that traversal reads it back with
/// [`stopped`]. Source and decoder failures are never `Stop`, so they stay errors, and an
/// enclosing checked read still revalidates its source after the stop.
#[derive(Debug, thiserror::Error)]
#[error("traversal stopped early")]
pub struct Stop;

/// A traversal's result with [`Stop`] read back: `Some` ran to the end, `None` was stopped.
pub fn stopped<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is::<Stop>() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether a [`MessageSink`] wants the stream to keep going. Returning [`Flow::Stop`] lets a
/// consumer that has seen enough (e.g. a head-only embedder, or a `--range` window) end the parse
/// without reading the rest of the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Receives messages as an adapter streams them. [`message`](MessageSink::message) is called once
/// per message, in transcript order; the message is dropped after the call returns unless the sink
/// keeps it.
///
/// [`meta`](MessageSink::meta) is an optional early signal carrying the session-level fields
/// (model/cwd/title/git) with an empty `messages` vec. Adapters that know the metadata before the
/// body call it *before* the first [`message`](MessageSink::message); the bridge `stream` (over a
/// full `parse`) always does. Sinks that must render a header ahead of the body (e.g. `cv show`)
/// use it; most sinks ignore it and just read the returned `Session` afterward.
pub trait MessageSink {
    /// The richest tier the next delivered message needs. Advisory: under a [`Tier::Rich`] read
    /// an adapter with a facts path may build that message at [`Tier::Facts`]
    /// ([`Demand::for_sink`]); record acceptance, source checks and EOF session facts never
    /// weaken. Wrappers forward or combine their consumers' tiers.
    fn tier(&self) -> Tier {
        Tier::Rich
    }

    fn message(&mut self, m: Message) -> Flow;

    /// Return a latched consumer error, if any. A source-backed field can fail while being
    /// inspected: that failure must not be represented as an intentional [`Flow::Stop`].
    /// Wrappers forward this channel; completed passes check it before publishing any output.
    fn take_error(&mut self) -> Option<anyhow::Error> {
        None
    }

    /// Deliver a message and surface failures distinctly from an intentional early stop.
    fn try_message(&mut self, m: Message) -> Result<Flow> {
        let flow = self.message(m);
        match self.take_error() {
            Some(error) => Err(error),
            None => Ok(flow),
        }
    }

    /// Session metadata, possibly emitted before the first message. Default: ignore.
    fn meta(&mut self, _session: &Session) {}

    /// An auxiliary path is about to be consulted (including metadata/absence or directory
    /// membership). Adapters report before reading; wrappers must forward the notification.
    fn dependency(&mut self, _path: &std::path::Path) {}

    /// [`dependency`](MessageSink::dependency) on an append-only file of which the adapter
    /// consults the head, never what is appended: its growth leaves the reader's output as it
    /// was ([`crate::source::ReadSet::record_append_only`]). Wrappers forward it; a recorder
    /// that does not know it records the path exactly. Default:
    /// [`dependency`](MessageSink::dependency).
    fn append_only_dependency(&mut self, path: &std::path::Path) {
        self.dependency(path);
    }

    /// The adapter is between records, holding nothing back, `offset` bytes into the file: may it
    /// take a [`Checkpoint`] here? Only a sink that can put everything it holds into one says
    /// yes, so a pass through any other sink never takes one. Default: no.
    fn wants_checkpoint(&mut self, _offset: u64) -> bool {
        false
    }

    /// [`wants_checkpoint`](MessageSink::wants_checkpoint) at the last settled position of a read
    /// (its EOF): a sink that spaces its checkpoints takes this one regardless of the spacing, so
    /// every read leaves its mark where it stopped and the next pass over the grown file reads
    /// only the appended bytes. Wrappers forward it with their own conditions. Default:
    /// [`wants_checkpoint`](MessageSink::wants_checkpoint).
    fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
        self.wants_checkpoint(offset)
    }

    /// Add this sink's state to `cp` (the adapter's is already in) and pass it on to any sink
    /// this one wraps. Called only after [`wants_checkpoint`](MessageSink::wants_checkpoint) said
    /// yes. Default: nothing to add.
    fn checkpoint(&mut self, _cp: &mut Checkpoint) {}
}

/// Run an adapter boundary with a consumer error guard. Legacy adapter loops can keep using
/// `message() -> Flow`; failures stop delivery immediately and surface after the adapter returns,
/// including errors raised by metadata, dependencies, checkpoints or an EOF-only sink check.
/// An intentional stop succeeds only without source or consumer errors; it never suppresses
/// invalidation detected during completion. Discard staged output from every failed pass.
pub fn checked<T>(sink: &mut dyn MessageSink, read: impl FnOnce(&mut dyn MessageSink) -> Result<T>) -> Result<T> {
    guard(sink, None, |guard| read(guard))
}

/// [`checked`] over a read of the captured transcript `backing`, whose descriptor the guard
/// rechecks after a slow callback ([`jsonl::Read::SLOW_CALLBACK`]), after the session metadata
/// and after a checkpoint, before the reader touches mapped bytes again ([`Guard`]).
pub(crate) fn guarded<T>(
    sink: &mut dyn MessageSink,
    backing: &jsonl::Read,
    read: impl FnOnce(&mut Guard<'_>) -> Result<T>,
) -> Result<T> {
    guard(sink, Some(backing), read)
}

fn guard<'a, T>(
    sink: &'a mut dyn MessageSink,
    backing: Option<&'a jsonl::Read>,
    read: impl FnOnce(&mut Guard<'a>) -> Result<T>,
) -> Result<T> {
    let mut guard = Guard {
        inner: sink,
        latch: Latch::default(),
        backing,
    };
    if let Some(error) = guard.take_error() {
        return Err(error);
    }
    let result = read(&mut guard);
    // Held output and a fast failing callback may follow the last line-pass check. Completion
    // belongs here, including errors: a changed backing outranks the consumer error it caused.
    if let Some(backing) = backing {
        backing.check()?;
    }
    let error = guard.take_error();
    // An inner checked boundary may already have returned the original diagnostic while its
    // failed outer sink now reports only a persistent failure marker. Preserve that Result.
    match result {
        Err(error) => Err(error),
        Ok(value) => match error {
            Some(error) => Err(error),
            None => Ok(value),
        },
    }
}

/// A sink's first error, held until taken, and the failure it leaves: once failed, a sink stops
/// delivery and, after the error is taken, reports that it "has already failed".
#[derive(Default)]
pub struct Latch {
    error: Option<anyhow::Error>,
    failed: bool,
}
impl Latch {
    pub fn failed(&self) -> bool {
        self.failed
    }
    /// Record a failure; the first error is the one reported.
    pub fn fail(&mut self, error: anyhow::Error) {
        self.failed = true;
        self.error.get_or_insert(error);
    }
    /// Latch an error `inner` reports, if any.
    pub fn capture(&mut self, inner: &mut dyn MessageSink) {
        if let Some(error) = inner.take_error() {
            self.fail(error);
        }
    }
    /// The held error once, then the persistent "`what` has already failed" marker.
    pub fn take(&mut self, what: &str) -> Option<anyhow::Error> {
        self.error
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("{what} has already failed")))
    }
    /// [`take`](Self::take), asking `inner` for its error only when this latch holds none.
    pub fn take_or(&mut self, inner: &mut dyn MessageSink, what: &str) -> Option<anyhow::Error> {
        if self.error.is_none() {
            self.capture(inner);
        }
        self.take(what)
    }
}

/// The consumer guard of a checked read: a consumer's first failure stops delivery and is
/// returned after the reader. Over a captured transcript ([`guarded`]) it also owns the backing
/// checks: after a callback slower than [`jsonl::Read::SLOW_CALLBACK`] (a consumer that writes
/// or blocks), and always after the session metadata and a checkpoint (the next resume depends
/// on the file being intact). A failed check is the read's error, latched like a consumer's, so
/// the reader stops before its mapping is touched again.
pub(crate) struct Guard<'a> {
    inner: &'a mut dyn MessageSink,
    latch: Latch,
    backing: Option<&'a jsonl::Read>,
}
impl Guard<'_> {
    /// A callback or a backing check has failed: the reader must stop.
    pub(crate) fn failed(&self) -> bool {
        self.latch.failed()
    }
    /// The timer for a callback, when the guard has a backing to check after it.
    fn start(&self) -> Option<std::time::Instant> {
        self.backing.map(|_| std::time::Instant::now())
    }
    /// After a callback that began at `started`: recheck the backing if it was slow (or, with
    /// `None`, always), and latch what the consumer reported.
    fn after(&mut self, started: Option<std::time::Instant>, forced: bool) {
        self.latch.capture(self.inner);
        let Some(backing) = self.backing else {
            return;
        };
        if forced || started.is_some_and(|started| started.elapsed() >= jsonl::Read::SLOW_CALLBACK) {
            if let Err(error) = backing.check_backing() {
                self.latch.fail(error);
            }
        }
    }
}
impl MessageSink for Guard<'_> {
    fn tier(&self) -> Tier {
        self.inner.tier()
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.capture(self.inner);
        self.latch.take("stream consumer")
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        let started = self.start();
        let flow = self.inner.try_message(m).unwrap_or_else(|error| {
            self.latch.fail(error);
            Flow::Stop
        });
        self.after(started, false);
        if self.latch.failed() {
            Flow::Stop
        } else {
            flow
        }
    }
    fn meta(&mut self, s: &Session) {
        if !self.latch.failed() {
            self.inner.meta(s);
            self.after(None, true);
        }
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            let started = self.start();
            self.inner.dependency(path);
            self.after(started, false);
        }
    }
    fn append_only_dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            let started = self.start();
            self.inner.append_only_dependency(path);
            self.after(started, false);
        }
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        if self.latch.failed() {
            return false;
        }
        let started = self.start();
        let wants = self.inner.wants_checkpoint(offset);
        self.after(started, false);
        wants && !self.latch.failed()
    }
    fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
        if self.latch.failed() {
            return false;
        }
        let started = self.start();
        let wants = self.inner.wants_final_checkpoint(offset);
        self.after(started, false);
        wants && !self.latch.failed()
    }
    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if !self.latch.failed() {
            self.inner.checkpoint(cp);
            self.after(None, true);
        }
    }
}

/// Where an [`Adapter::transcript`] read starts and ends.
#[derive(Debug, Clone, Copy)]
pub enum Start<'c> {
    /// The whole session.
    Whole,
    /// From a checkpoint an earlier read of the same file took (then shorter), to EOF.
    Checkpoint(&'c Checkpoint),
    /// The first `len` bytes of an append-only transcript, read as that past state's EOF: the
    /// session as it was when the file was `len` bytes long ([`Adapter::append_only`]).
    Prefix(u64),
}

/// A point a pass over an append-only transcript can resume from: `offset` is the byte after the
/// last whole line read, and `state` holds what each layer of the pass held there, by layer
/// (`adapter`, `counter`, and each consumer's own key). Resuming at `offset` with every layer
/// restored from `state` gives what the whole pass would have given from there on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub offset: u64,
    pub state: serde_json::Map<String, serde_json::Value>,
}

impl Checkpoint {
    pub fn new(offset: u64) -> Self {
        Checkpoint {
            offset,
            state: Default::default(),
        }
    }

    /// Store `layer`'s state (serialization of these plain structs cannot fail; if it did, the
    /// layer would be missing and a resume would decline).
    pub fn put(&mut self, layer: &str, state: &impl Serialize) {
        match serde_json::to_value(state) {
            Ok(v) => {
                self.state.insert(layer.to_string(), v);
            }
            // A release build keeps the documented decline; a debug build refuses to ship it.
            Err(error) => debug_assert!(false, "checkpoint layer {layer:?} did not serialize: {error}"),
        }
    }

    /// `layer`'s state, when it is there and still reads as `T`.
    pub fn get<T: DeserializeOwned>(&self, layer: &str) -> Option<T> {
        T::deserialize(self.state.get(layer)?).ok()
    }

    /// How many messages the pass had delivered at this point (the counter layer's count): the
    /// index the first resumed message gets.
    pub fn messages(&self) -> Option<usize> {
        self.get::<CounterState>(COUNTER_LAYER).map(|c| c.messages)
    }
}

/// A sink that drops every message and holds nothing, so it can always be checkpointed: the
/// consumer of a pass that only counts.
pub struct Discard;

impl MessageSink for Discard {
    fn message(&mut self, _m: Message) -> Flow {
        Flow::Continue
    }
    fn wants_checkpoint(&mut self, _offset: u64) -> bool {
        true
    }
}

/// Any `FnMut(Message) -> Flow` is a sink — the ergonomic form for ad-hoc consumers.
impl<F: FnMut(Message) -> Flow> MessageSink for F {
    fn message(&mut self, m: Message) -> Flow {
        self(m)
    }
}

/// An ad-hoc fallible message callback. Existing `FnMut(Message) -> Flow` sinks stay infallible;
/// this wrapper latches a `Result` error, stops future delivery and exposes it to checked passes.
pub struct TryFnSink<F> {
    callback: F,
    latch: Latch,
}
impl<F: FnMut(Message) -> Result<Flow>> TryFnSink<F> {
    pub fn new(callback: F) -> Self {
        Self {
            callback,
            latch: Latch::default(),
        }
    }
}
impl<F: FnMut(Message) -> Result<Flow>> MessageSink for TryFnSink<F> {
    fn message(&mut self, message: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        (self.callback)(message).unwrap_or_else(|error| {
            self.latch.fail(error);
            Flow::Stop
        })
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("message callback")
    }
}

/// Fans one adapter pass into two sinks, so a single read of a transcript feeds multiple consumers
/// — e.g. `cv index` driving the FTS [`ChunkSink`] and the event-catalog
/// [`EventSink`](crate::events::EventSink) off the same `stream()`. Each message is cloned once
/// while both sinks are live; under [`Demand::with_spans`] large content is a
/// [`Span`](crate::lazy::Span) handle, so the clone stays O(small).
///
/// Flow semantics: the stream continues until **both** sinks have said [`Flow::Stop`], and a sink
/// that stopped is never called again. (Stopping on the first `Stop` would silently truncate the
/// other consumer's view — e.g. a head-only sink ending event extraction mid-session.)
pub struct TeeSink<'a> {
    a: &'a mut dyn MessageSink,
    b: &'a mut dyn MessageSink,
    a_stopped: bool,
    b_stopped: bool,
    latch: Latch,
}

impl<'a> TeeSink<'a> {
    pub fn new(a: &'a mut dyn MessageSink, b: &'a mut dyn MessageSink) -> Self {
        TeeSink {
            a,
            b,
            a_stopped: false,
            b_stopped: false,
            latch: Latch::default(),
        }
    }
}

impl TeeSink<'_> {
    fn capture(&mut self) {
        if let Some(error) = self.a.take_error().or_else(|| self.b.take_error()) {
            self.latch.fail(error);
        }
    }
    fn deliver(sink: &mut dyn MessageSink, m: Message, stopped: &mut bool) -> Result<()> {
        *stopped = sink.try_message(m)? == Flow::Stop;
        Ok(())
    }
}
impl MessageSink for TeeSink<'_> {
    fn tier(&self) -> Tier {
        let live = |stopped: bool, tier: Tier| if stopped { Tier::Facts } else { tier };
        live(self.a_stopped, self.a.tier()).max(live(self.b_stopped, self.b.tier()))
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.capture();
        self.latch.take("stream consumer")
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if self.latch.failed() {
            return;
        }
        self.a.dependency(path);
        self.capture();
        if !self.latch.failed() {
            self.b.dependency(path);
            self.capture();
        }
    }
    fn append_only_dependency(&mut self, path: &std::path::Path) {
        if self.latch.failed() {
            return;
        }
        self.a.append_only_dependency(path);
        self.capture();
        if !self.latch.failed() {
            self.b.append_only_dependency(path);
            self.capture();
        }
    }
    fn meta(&mut self, s: &Session) {
        if self.latch.failed() {
            return;
        }
        self.a.meta(s);
        self.capture();
        if !self.latch.failed() {
            self.b.meta(s);
            self.capture();
        }
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        if self.latch.failed() {
            return false;
        }
        let wants = self.a.wants_checkpoint(offset) && self.b.wants_checkpoint(offset);
        self.capture();
        wants && !self.latch.failed()
    }
    fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
        if self.latch.failed() {
            return false;
        }
        let wants = self.a.wants_final_checkpoint(offset) && self.b.wants_final_checkpoint(offset);
        self.capture();
        wants && !self.latch.failed()
    }
    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if self.latch.failed() {
            return;
        }
        self.a.checkpoint(cp);
        self.capture();
        if !self.latch.failed() {
            self.b.checkpoint(cp);
            self.capture();
        }
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        let result = match (self.a_stopped, self.b_stopped) {
            (false, false) => {
                let copy = m.clone();
                Self::deliver(self.a, m, &mut self.a_stopped)
                    .and_then(|()| Self::deliver(self.b, copy, &mut self.b_stopped))
            }
            (false, true) => Self::deliver(self.a, m, &mut self.a_stopped),
            (true, false) => Self::deliver(self.b, m, &mut self.b_stopped),
            (true, true) => Ok(()),
        };
        if let Err(error) = result {
            self.latch.fail(error);
        }
        if self.latch.failed() || (self.a_stopped && self.b_stopped) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// A sink that retains every message — the bridge back to a whole [`Session`]. See [`collect`].
#[derive(Default)]
pub struct CollectSink {
    pub messages: Vec<Message>,
}

impl MessageSink for CollectSink {
    fn message(&mut self, m: Message) -> Flow {
        self.messages.push(m);
        Flow::Continue
    }
}

/// Hand an already-parsed whole session to `sink` — the `stream` of a store that can only be read
/// whole (an account export, an OpenSession document): metadata first, so header-rendering sinks
/// have it, then each message until the sink stops. Returns the metadata with `messages` emptied.
pub fn replay(mut session: Session, sink: &mut dyn MessageSink) -> Result<Session> {
    let messages = std::mem::take(&mut session.messages);
    sink.meta(&session);
    if let Some(error) = sink.take_error() {
        return Err(error);
    }
    for m in messages {
        if sink.try_message(m)? == Flow::Stop {
            break;
        }
    }
    Ok(session)
}

/// Drive `adapter.stream` into a [`CollectSink`] and reattach the messages — i.e. fully parse `r`
/// at full fidelity. This is the default body of [`Adapter::parse`](crate::harness::Adapter::parse)
/// for adapters that implement a native `stream`.
pub fn collect(adapter: &dyn Adapter, r: &SessionRef) -> Result<Session> {
    let mut session = collect_with(adapter, r, &Demand::RICH)?;
    // Full-fidelity parse: resolve any lazy content spans so the returned `Session` owns all its
    // content inline (safe for `--json` serialization, cross-harness emit, `Deref` reads, etc.).
    session.materialize()?;
    Ok(session)
}

/// Like [`collect`] but with explicit [`Demand`] and **without** materializing — the returned
/// session may carry lazy content [`Span`]s. Consumers that hold the whole session but read it once
/// (e.g. dataset export) use this and resolve per block, so a giant field is never owned wholesale.
/// Use [`collect`] (or call `.materialize()`) when you need owned inline content.
pub fn collect_with(adapter: &dyn Adapter, r: &SessionRef, opts: &Demand) -> Result<Session> {
    let mut sink = CollectSink::default();
    let mut session = stream_counted(adapter, r, opts, &mut sink, Pass::RECORD)?.session;
    session.messages = sink.messages;
    Ok(session)
}

/// How a [`stream_counted`] pass reads. The default reads the whole session once, takes no
/// checkpoint, records nothing and gives no receipt.
#[derive(Default, Clone, Copy)]
pub struct Pass<'a> {
    /// Start at this checkpoint instead of byte 0, with the counts restored from it. The caller
    /// restores its own sinks from it and checks that it still describes the file. An adapter
    /// that cannot resume from it touches nothing and the pass fails with [`Declined`].
    pub from: Option<&'a Checkpoint>,
    /// Take a checkpoint each time the file has advanced this many bytes past the last one,
    /// where the adapter and every sink allow.
    pub every: Option<u64>,
    /// Amortize checkpoint work against new transcript bytes: after a checkpoint the next one
    /// also waits for that checkpoint's encoded size in new bytes. Every layer still keeps its
    /// exact owned state; a large growing replay history takes fewer checkpoints at the cost of a
    /// larger suffix to reread on the next append.
    pub amortized: bool,
    /// Publish what a whole read counted to the catalog, against a source snapshot taken before
    /// the read (never from a [`Demand::COMPLETE`] read): a "not counted" (`0`) row gets the parse's turn count, so `cv ls` shows the real
    /// number from the first full read on, and the affordance counts that let `has:<kind>` answer
    /// from the catalog are recorded unless still fresh. A sweep that parses on many threads
    /// leaves this off and publishes [`Counted`] itself, on one.
    pub record: bool,
    /// Return a [`StagedReadGuard`] covering the primary, adapter dependencies and retained
    /// message sources, for an operation that stages output before publishing it.
    pub guard: bool,
}

impl Pass<'_> {
    /// A whole read that records what it counted: what the full-read doors use.
    pub const RECORD: Pass<'static> = Pass {
        from: None,
        every: None,
        amortized: false,
        record: true,
        guard: false,
    };
}

/// The adapter could not resume from [`Pass::from`] and touched nothing: read the session whole
/// instead, from sinks that were not restored from the checkpoint.
#[derive(Debug)]
pub struct Declined;
impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("adapter declined to resume from the checkpoint")
    }
}
impl std::error::Error for Declined {}

/// What a [`stream_counted`] pass gives back.
pub struct Streamed {
    pub session: Session,
    /// What the whole session counted (a resumed part added to the checkpoint's), `None` when the
    /// sink stopped early.
    pub counted: Option<Counted>,
    /// The last checkpoint the pass took, or else the one it resumed from (`None` when it has
    /// neither, or stopped early): the mark the next pass over the grown file resumes from.
    pub last: Option<Checkpoint>,
    /// With [`Pass::guard`]: the receipt to verify before publishing staged output.
    pub guard: Option<StagedReadGuard>,
}

/// Source receipt for an operation that stages output before publication.
/// Validate after resolving all demanded payloads, before exposing the completed stage.
pub struct StagedReadGuard {
    source: crate::source::Baseline,
    reads: crate::source::ReadSet,
    primary: std::path::PathBuf,
}
impl StagedReadGuard {
    pub fn verify(&self, r: &SessionRef) -> Result<()> {
        self.verify_baseline(r).map(|_| ())
    }

    /// Verify that the session source is unchanged since capture and return the completed
    /// [`crate::source::Baseline`] (with recorded dynamic reads and any shared-store
    /// [`crate::source::Rows`] certificate) for downstream freshness checks.
    pub fn verify_baseline(&self, r: &SessionRef) -> Result<crate::source::Baseline> {
        let mut probe = crate::source::Probe::default();
        let mut baseline = self.source.with_reads(&self.reads);
        let rows = probe.rows(r);
        anyhow::ensure!(
            crate::source::unchanged_with(r, &self.primary, &baseline, &mut probe),
            "session source changed while preparing output"
        );
        baseline.rows = rows;
        Ok(baseline)
    }
}

/// The one counted read: `adapter.try_stream(r, opts, sink)` (or `try_resume` from
/// [`Pass::from`]) through a wrapper that counts turns ([`Message::is_turn`]), messages and
/// affordances ([`crate::affordance::Counter`]) as they go by, and takes the checkpoints the pass
/// asks for. The full-read doors (`collect`/`parse`, `cv show`, `cv index`, watch) all read
/// through this.
pub fn stream_counted(
    adapter: &dyn Adapter,
    r: &SessionRef,
    opts: &Demand,
    sink: &mut dyn MessageSink,
    pass: Pass<'_>,
) -> Result<Streamed> {
    // Captured before the adapter opens anything; a guarded pass's covers the resume source.
    let guard = pass.guard.then(|| {
        let primary = adapter.resume_source(r);
        StagedReadGuard {
            source: crate::source::Probe::default().snapshot_from(r, &primary),
            reads: Default::default(),
            primary,
        }
    });
    let source = match &guard {
        Some(guard) => Some(guard.source.clone()),
        None => pass.record.then(|| crate::source::Probe::default().snapshot(r)),
    };
    let mut counting = Counting::new(r, sink, pass.every);
    counting.amortized = pass.amortized;
    counting.guard = guard;
    let session = match pass.from {
        Some(cp) => {
            let state = cp.get::<CounterState>(COUNTER_LAYER).ok_or(Declined)?;
            if !counting.restore(state, cp.offset) {
                counting.space_after(cp);
            }
            adapter
                .transcript(r, opts, Start::Checkpoint(cp), &mut counting)?
                .ok_or(Declined)?
        }
        None => adapter.try_stream(r, opts, &mut counting)?,
    };
    let session = counting.stamp_session(session);
    // A resumed pass that read too little to take one still passed through `from`.
    let last = counting
        .last
        .take()
        .or_else(|| pass.from.cloned())
        .filter(|_| !counting.stopped);
    let counted = (!counting.stopped)
        .then(|| -> Result<Counted> {
            let mut models = counting.models;
            if let Some(primary) = session.model.as_deref().filter(|s| !s.is_empty()) {
                models.insert(primary.to_lowercase());
            }
            let primary_model = session
                .model
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| counting.first_model.clone());
            let facts = crate::catalog::SessionQueryFacts {
                primary_model,
                models: models.into_iter().collect(),
                git_branch: session.git.as_ref().and_then(|g| g.branch.clone()),
                git_remote: session.git.as_ref().and_then(|g| g.remote.clone()),
                compactions: counting.compactions,
                subagent_count: None,
                subagent_guard: Default::default(),
            };
            Ok(Counted {
                turns: counting.turns,
                dependencies: counting.dependencies,
                counts: counting.affordances.finish(&session, &adapter.store_affordances(r)?),
                facts: Some(facts),
            })
        })
        .transpose()?;
    // A complete read also carries what left the conversation (rewound and superseded rows,
    // carriers) for round-trip: it never certifies what the session counts.
    if let (true, Some(source), Some(counted)) = (pass.record && opts.tier != Tier::Complete, &source, &counted) {
        counted.record_with_source(r, source);
    }
    Ok(Streamed {
        session,
        counted,
        last,
        guard: counting.guard.take(),
    })
}

/// What a whole-session read counted: its turns, its affordances, and its indexed query facts.
#[derive(Debug, Clone)]
pub struct Counted {
    pub turns: usize,
    pub counts: crate::affordance::Counts,
    /// Adapter dependencies consulted before the pass computed these counts.
    pub dependencies: crate::source::ReadSet,
    /// Queryable session metadata and structural counts captured during the pass.
    pub facts: Option<crate::catalog::SessionQueryFacts>,
}

impl Counted {
    /// Publish against the dependencies captured before this pass, including any side-store rows.
    pub fn record_with_source(&self, r: &SessionRef, source: &crate::source::Baseline) {
        let source = source.with_reads(&self.dependencies);
        if crate::source::unchanged(r, &source) {
            crate::catalog::note_counted_with_source(r, &source, self.turns, self.facts.clone(), || {
                self.counts.clone()
            });
        }
    }
}

/// The [`Checkpoint`] layer an adapter keeps its own state under.
pub const ADAPTER_LAYER: &str = "adapter";

/// The [`Checkpoint`] layer the counting wrapper keeps its state under.
const COUNTER_LAYER: &str = "counter";

/// What the counting wrapper holds at a checkpoint.
#[derive(Serialize, Deserialize)]
struct CounterState {
    messages: usize,
    turns: usize,
    counts: std::collections::BTreeMap<String, u32>,
    dependencies: crate::source::ReadSet,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    models: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_model: Option<String>,
    #[serde(default)]
    compactions: u32,
    /// At a read's end mark: the last spaced checkpoint before it, so a pass resumed there keeps
    /// the schedule (and the consumers the doc boundaries) a whole pass has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<Anchor>,
}

/// Where the spaced checkpoints stood at an end mark: the last one's offset and encoded size.
#[derive(Serialize, Deserialize, Clone, Copy)]
struct Anchor {
    offset: u64,
    state_bytes: u64,
}

// Count the complete owned snapshot without allocating another JSON buffer. Value serialization
// is infallible; saturating the count merely declines further checkpoints at unreachable offsets.
fn checkpoint_bytes(cp: &Checkpoint) -> u64 {
    struct Size(u64);
    impl std::io::Write for Size {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut size = Size(0);
    if serde_json::to_writer(&mut size, cp).is_err() {
        return u64::MAX;
    }
    size.0
}

/// The wrapper every counted pass streams through: turns, messages and affordances as they go
/// by, and (in a resumable pass) the checkpoints.
struct Counting<'a> {
    inner: &'a mut dyn MessageSink,
    messages: usize,
    turns: usize,
    affordances: crate::affordance::Counter,
    dependencies: crate::source::ReadSet,
    models: std::collections::BTreeSet<String>,
    first_model: Option<String>,
    last_model: Option<String>,
    compactions: u32,
    stopped: bool,
    latch: Latch,
    /// Records what a guarded pass read, for its [`StagedReadGuard`].
    guard: Option<StagedReadGuard>,
    /// Take a checkpoint every this many bytes (`None`: never).
    every: Option<u64>,
    last_offset: u64,
    amortized: bool,
    spacing: u64,
    /// The encoded size of the last checkpoint's state, in an amortized pass.
    state_bytes: u64,
    /// The checkpoint about to be taken is the read's end mark.
    ending: bool,
    last: Option<Checkpoint>,
    /// The primary's identity before the adapter opened it: a file replaced at any later point is
    /// not the one this pass's spans point into.
    stamp: Option<crate::lazy::SourceStamp>,
}

impl<'a> Counting<'a> {
    fn new(r: &SessionRef, inner: &'a mut dyn MessageSink, every: Option<u64>) -> Self {
        Counting {
            inner,
            messages: 0,
            turns: 0,
            affordances: crate::affordance::Counter::new(r),
            dependencies: Default::default(),
            models: Default::default(),
            first_model: None,
            last_model: None,
            compactions: 0,
            stopped: false,
            latch: Latch::default(),
            guard: None,
            every,
            last_offset: 0,
            amortized: false,
            spacing: every.unwrap_or(0),
            state_bytes: 0,
            ending: false,
            last: None,
            stamp: crate::lazy::SourceStamp::capture(&r.path),
        }
    }

    /// `s` was read from this pass's primary and carries no stamp of its own.
    fn should_stamp(&self, s: &Session) -> bool {
        let primary = |stamp: &crate::lazy::SourceStamp| s.source_path.as_deref().is_some_and(|p| stamp.is_for(p));
        s.source_stamp.is_none() && self.stamp.as_ref().is_some_and(primary)
    }

    fn stamp_session(&self, mut session: Session) -> Session {
        if self.should_stamp(&session) {
            session.source_stamp = self.stamp.clone();
        }
        session
    }

    fn space_after(&mut self, cp: &Checkpoint) {
        if self.amortized {
            self.state_bytes = checkpoint_bytes(cp);
            self.spacing = self.every.unwrap_or(0).max(self.state_bytes);
        }
    }

    /// Restore the counts and the checkpoint schedule; `true` when `state` is an end mark's,
    /// whose schedule is its anchor's rather than the checkpoint's own.
    fn restore(&mut self, state: CounterState, offset: u64) -> bool {
        self.messages = state.messages;
        self.turns = state.turns;
        self.dependencies = state.dependencies;
        self.models = state.models;
        self.first_model = state.first_model;
        self.last_model = None;
        self.compactions = state.compactions;
        let counts = state
            .counts
            .iter()
            .filter_map(|(k, n)| Some((crate::affordance::Kind::parse(k)?, *n)))
            .collect();
        self.affordances.restore(counts);
        self.last_offset = offset;
        let Some(anchor) = state.anchor else { return false };
        self.last_offset = anchor.offset;
        self.state_bytes = anchor.state_bytes;
        if self.amortized {
            self.spacing = self.every.unwrap_or(0).max(anchor.state_bytes);
        }
        true
    }
}

impl MessageSink for Counting<'_> {
    fn tier(&self) -> Tier {
        self.inner.tier()
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        let error = self.latch.take_or(self.inner, "counting stream");
        self.stopped |= self.latch.failed();
        error
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if self.latch.failed() {
            return;
        }
        self.dependencies.record(path);
        if let Some(guard) = &mut self.guard {
            guard.reads.record(path);
        }
        self.inner.dependency(path);
    }
    fn append_only_dependency(&mut self, path: &std::path::Path) {
        if self.latch.failed() {
            return;
        }
        self.dependencies.record_append_only(path);
        if let Some(guard) = &mut self.guard {
            guard.reads.record_append_only(path);
        }
        self.inner.append_only_dependency(path);
    }
    fn meta(&mut self, s: &Session) {
        if self.latch.failed() {
            return;
        }
        // A sink that builds its resolver from the meta session (`session.resolver()`) holds the
        // primary to the pre-parse stamp. Streamed meta sessions hold no messages, so this copy
        // is of the header only; one that does hold messages is passed on as it is.
        if s.messages.is_empty() && self.should_stamp(s) {
            self.inner.meta(&self.stamp_session(s.clone()));
        } else {
            self.inner.meta(s);
        }
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        self.messages += 1;
        self.turns += usize::from(m.is_turn());
        if m.kind == crate::ir::MessageKind::CompactionBoundary {
            self.compactions = self.compactions.saturating_add(1);
        }
        if let Some(model) = m.model.as_deref().filter(|s| !s.is_empty()) {
            if self.first_model.is_none() {
                self.first_model = Some(model.to_owned());
            }
            if self.last_model.as_deref() != Some(model) {
                self.last_model = Some(model.to_owned());
                self.models.insert(model.to_lowercase());
            }
        }
        if let Err(error) = self.affordances.note(&m) {
            self.latch.fail(error);
            self.stopped = true;
            return Flow::Stop;
        }
        if let Some(guard) = &mut self.guard {
            crate::source::record_message_reads(&m, Some(&guard.primary), &mut guard.reads);
        }
        let flow = self.inner.message(m);
        self.stopped |= flow == Flow::Stop;
        flow
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        self.ending = false;
        !self.stopped
            && self.every.is_some()
            && offset >= self.last_offset.saturating_add(self.spacing)
            && self.inner.wants_checkpoint(offset)
    }
    /// The spacing aside: any position past the last checkpoint, by at least the size of that
    /// checkpoint's state in an amortized pass (a mark costs no more to write and restore than
    /// the reread it saves).
    fn wants_final_checkpoint(&mut self, offset: u64) -> bool {
        self.ending = !self.stopped
            && self.every.is_some()
            && offset > self.last_offset
            && offset - self.last_offset >= self.state_bytes
            && self.inner.wants_final_checkpoint(offset);
        self.ending
    }
    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if self.latch.failed() {
            return;
        }
        let counts = self
            .affordances
            .counts()
            .iter()
            .map(|(k, n)| (k.as_str().to_string(), *n))
            .collect();
        cp.put(
            COUNTER_LAYER,
            &CounterState {
                messages: self.messages,
                turns: self.turns,
                counts,
                dependencies: self.dependencies.clone(),
                models: self.models.clone(),
                first_model: self.first_model.clone(),
                compactions: self.compactions,
                anchor: self.ending.then_some(Anchor {
                    offset: self.last_offset,
                    state_bytes: self.state_bytes,
                }),
            },
        );
        self.inner.checkpoint(cp);
        if !std::mem::take(&mut self.ending) {
            self.space_after(cp);
            self.last_offset = cp.offset;
        }
        self.last = Some(cp.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Role};

    fn text_msg(t: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.content = vec![Block::Text { text: t.into() }];
        m
    }

    #[test]
    fn extra_construction_demand_forwards_and_unions_without_losing_counter_facts() {
        struct Wants(Tier);
        impl MessageSink for Wants {
            fn tier(&self) -> Tier {
                self.0
            }
            fn message(&mut self, _: Message) -> Flow {
                Flow::Stop
            }
        }
        let mut facts = Wants(Tier::Facts);
        let mut full = Wants(Tier::Rich);
        let mut tee = TeeSink::new(&mut facts, &mut full);
        assert_eq!(tee.tier(), Tier::Rich);
        tee.b_stopped = true;
        assert_eq!(tee.tier(), Tier::Facts);
        let r = SessionRef::new("facts", crate::Harness::Claude, "source");
        let mut counter = Counting::new(&r, &mut facts, None);
        assert_eq!(counter.tier(), Tier::Facts);
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "c".into(),
            content: "result".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: Some(serde_json::json!({"restorable":["file.rs"]})),
        });
        counter.message(result);
        let counted = counter
            .affordances
            .finish(&Session::new("facts", crate::Harness::Claude), &[]);
        assert_eq!(counted.get(&crate::affordance::Kind::Checkpoint), Some(&1));
        let mut guard = Guard {
            inner: &mut facts,
            latch: Latch::default(),
            backing: None,
        };
        assert_eq!(guard.tier(), Tier::Facts);
        let affordance = crate::affordance::AffordanceSink::new(crate::Harness::Claude, &mut guard);
        assert_eq!(affordance.tier(), Tier::Facts);
    }

    /// A sink that records the texts it saw and stops after `stop_after` messages.
    struct Probe {
        seen: Vec<String>,
        stop_after: usize,
    }
    impl MessageSink for Probe {
        fn message(&mut self, m: Message) -> Flow {
            self.seen.push(m.text().unwrap_or_default());
            if self.seen.len() >= self.stop_after {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    #[test]
    fn tee_feeds_both_until_both_stop_and_never_calls_a_stopped_sink() {
        let mut a = Probe {
            seen: Vec::new(),
            stop_after: 1,
        };
        let mut b = Probe {
            seen: Vec::new(),
            stop_after: 3,
        };
        let mut tee = TeeSink::new(&mut a, &mut b);
        // a stops after the first message; the tee keeps going for b until *it* stops.
        assert_eq!(tee.message(text_msg("one")), Flow::Continue);
        assert_eq!(tee.message(text_msg("two")), Flow::Continue);
        assert_eq!(tee.message(text_msg("three")), Flow::Stop);
        assert_eq!(a.seen, vec!["one"]);
        assert_eq!(b.seen, vec!["one", "two", "three"]);
    }

    /// Keeps every message (as JSON) and every checkpoint it is offered.
    #[derive(Default)]
    struct Keep {
        msgs: Vec<serde_json::Value>,
        cps: Vec<Checkpoint>,
    }
    impl MessageSink for Keep {
        fn message(&mut self, m: Message) -> Flow {
            self.msgs.push(serde_json::to_value(&m).unwrap());
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _offset: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.cps.push(cp.clone());
        }
    }

    #[test]
    fn amortized_checkpoints_space_growing_owned_state_and_restore_the_schedule() {
        let r = transcript(crate::Harness::Qwen, "growing.jsonl".into());
        let mut fixed_sink = Discard;
        let mut fixed = Counting::new(&r, &mut fixed_sink, Some(1024));
        let mut sink = Discard;
        let mut counting = Counting::new(&r, &mut sink, Some(1024));
        counting.amortized = true;
        let mut names = std::collections::BTreeMap::new();
        let mut fixed_bytes = 0;
        let mut snapshots = Vec::new();
        let mut cumulative_bytes = 0;
        for n in 1..=1024 {
            names.insert(format!("call-{n}"), format!("tool-{}\n\"\u{1f680}", "x".repeat(64)));
            let offset = n * 128;
            if fixed.wants_checkpoint(offset) {
                let mut cp = Checkpoint::new(offset);
                cp.put(ADAPTER_LAYER, &names);
                fixed.checkpoint(&mut cp);
                fixed_bytes += serde_json::to_vec(&cp).unwrap().len() as u64;
            }
            if counting.wants_checkpoint(offset) {
                let mut cp = Checkpoint::new(offset);
                cp.put(ADAPTER_LAYER, &names);
                counting.checkpoint(&mut cp);
                let encoded = serde_json::to_vec(&cp).unwrap();
                assert_eq!(checkpoint_bytes(&cp), encoded.len() as u64);
                cumulative_bytes += encoded.len() as u64;
                snapshots.push(cp);
            }
        }
        let input_bytes = 1024 * 128;
        assert!(fixed_bytes > 32 * input_bytes, "control must expose repeated prefixes");
        assert!(cumulative_bytes <= 4 * input_bytes, "snapshot bytes={cumulative_bytes}");
        assert!(snapshots.len() > 2, "must establish repeated and resumed snapshots");
        for cp in snapshots {
            let state = cp.get::<CounterState>(COUNTER_LAYER).unwrap();
            let restored_names = cp
                .get::<std::collections::BTreeMap<String, String>>(ADAPTER_LAYER)
                .unwrap();
            assert_eq!(restored_names.len() as u64, cp.offset / 128);
            let mut resumed_sink = Keep::default();
            let mut resumed = Counting::new(&r, &mut resumed_sink, Some(1024));
            resumed.amortized = true;
            resumed.restore(state, cp.offset);
            resumed.space_after(&cp);
            let spacing = 1024.max(serde_json::to_vec(&cp).unwrap().len() as u64);
            assert!(!resumed.wants_checkpoint(cp.offset + spacing - 1));
            assert!(resumed.wants_checkpoint(cp.offset + spacing));
        }
    }

    #[test]
    fn wrappers_forward_read_sets_and_counter_checkpoints_keep_the_prefix() {
        let w = crate::test_world("read-set-wrappers");
        let path = w.base.join("session.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let extra = w.base.join("external.txt");
        std::fs::write(&extra, "before").unwrap();
        let r = transcript(crate::ir::Harness::Codex, path);
        let base = crate::source::Probe::default().snapshot(&r);
        let mut a = crate::events::EventSink::new(None);
        let mut b = crate::events::EventSink::new(None);
        let mut tee = TeeSink::new(&mut a, &mut b);
        let mut affordances = crate::affordance::AffordanceSink::new(r.harness, &mut tee);
        let mut counting = Counting::new(&r, &mut affordances, Some(1));
        counting.dependency(&extra);
        let mut cp = Checkpoint::new(1);
        counting.checkpoint(&mut cp);
        let state = cp.get::<CounterState>(COUNTER_LAYER).unwrap();
        let events_resumed = crate::events::EventSink::resume(&cp).unwrap();
        let mut keep = Keep::default();
        let mut resumed = Counting::new(&r, &mut keep, Some(1));
        resumed.restore(state, cp.offset);
        assert!(crate::source::unchanged(&r, &base.with_reads(&resumed.dependencies)));
        drop(counting);
        std::fs::write(&extra, "after!").unwrap();
        assert!(!crate::source::unchanged(&r, &base.with_reads(a.dependencies())));
        assert!(!crate::source::unchanged(&r, &base.with_reads(b.dependencies())));
        assert!(!crate::source::unchanged(&r, &base.with_reads(&resumed.dependencies)));
        assert!(!crate::source::unchanged(
            &r,
            &base.with_reads(events_resumed.dependencies())
        ));
    }

    #[test]
    fn try_fn_sink_distinguishes_stops_and_latches_errors_after_drain() {
        let mut stop = TryFnSink::new(|_: Message| Ok(Flow::Stop));
        assert_eq!(stop.try_message(text_msg("enough")).unwrap(), Flow::Stop);
        assert!(stop.take_error().is_none());
        let calls = std::cell::Cell::new(0);
        let mut sink = TryFnSink::new(|_: Message| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(Flow::Continue)
            } else {
                anyhow::bail!("selected source changed")
            }
        });
        assert_eq!(sink.try_message(text_msg("first")).unwrap(), Flow::Continue);
        assert_eq!(sink.message(text_msg("second")), Flow::Stop);
        assert_eq!(sink.take_error().unwrap().to_string(), "selected source changed");
        assert_eq!(sink.message(text_msg("ignored")), Flow::Stop);
        assert_eq!(calls.get(), 2);
        assert!(
            checked(&mut sink, |_sink| Ok(())).is_err(),
            "drained sink remains failed"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum FailAt {
        Message,
        Meta,
        Dependency,
        Checkpoint,
        End,
    }
    struct FailingSink {
        at: FailAt,
        seen: usize,
        metas: usize,
        dependencies: usize,
        checkpoints: usize,
        error: Option<anyhow::Error>,
        ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl FailingSink {
        fn new(at: FailAt, ended: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
            Self {
                at,
                seen: 0,
                metas: 0,
                dependencies: 0,
                checkpoints: 0,
                error: None,
                ended,
            }
        }
        fn fail(&mut self) {
            self.error = Some(anyhow::anyhow!("source projection unavailable"));
        }
    }
    impl MessageSink for FailingSink {
        fn message(&mut self, _: Message) -> Flow {
            self.seen += 1;
            if matches!(self.at, FailAt::Message) && self.seen == 2 {
                self.fail();
            }
            Flow::Continue // A pending error must win even over Continue.
        }
        fn meta(&mut self, _: &Session) {
            self.metas += 1;
            if matches!(self.at, FailAt::Meta) {
                self.fail();
            }
        }
        fn dependency(&mut self, _: &std::path::Path) {
            self.dependencies += 1;
            if matches!(self.at, FailAt::Dependency) {
                self.fail();
            }
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, _: &mut Checkpoint) {
            self.checkpoints += 1;
            if matches!(self.at, FailAt::Checkpoint) {
                self.fail();
            }
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            if matches!(self.at, FailAt::End) && self.ended.load(std::sync::atomic::Ordering::Relaxed) {
                self.fail();
            }
            self.error.take()
        }
    }
    struct IgnoresStop {
        ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
        side_reads: std::sync::atomic::AtomicUsize,
    }
    impl Adapter for IgnoresStop {
        fn harness(&self) -> crate::Harness {
            crate::Harness::Codex
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<crate::Discovered> {
            Ok(Default::default())
        }
        fn stream(&self, r: &SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            let session = Session::new(&r.id, r.harness);
            sink.meta(&session);
            for n in 1..=3 {
                sink.message(text_msg("turn")); // Deliberately ignore Stop, as some metadata scans do.
                sink.dependency(&r.path);
                if sink.wants_checkpoint(n) {
                    sink.checkpoint(&mut Checkpoint::new(n));
                }
            }
            sink.meta(&session); // Late callbacks must not resume after a failure.
            self.ended.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(session)
        }
        fn store_affordances(&self, _: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
            self.side_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Vec::new())
        }
    }

    #[test]
    fn failed_streams_never_finish_counts_or_publish_cache_or_marks() {
        let w = crate::test_world("fallible-stream-publication");
        let path = w.base.join("session.jsonl");
        std::fs::write(&path, "scratch primary").unwrap();
        let r = transcript(crate::Harness::Codex, path);
        for at in [
            FailAt::Message,
            FailAt::Meta,
            FailAt::Dependency,
            FailAt::Checkpoint,
            FailAt::End,
        ] {
            for resumable in [false, true] {
                if matches!(at, FailAt::Checkpoint) && !resumable {
                    continue;
                }
                let ended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let adapter = IgnoresStop {
                    ended: ended.clone(),
                    side_reads: Default::default(),
                };
                let mut failing = FailingSink::new(at, ended);
                let result = if resumable {
                    stream_counted(
                        &adapter,
                        &r,
                        &Demand::LEAN.with_spans(),
                        &mut failing,
                        crate::stream::Pass {
                            every: Some(1),
                            amortized: false,
                            ..Default::default()
                        },
                    )
                    .map(|_| ())
                } else {
                    stream_counted(&adapter, &r, &Demand::LEAN.with_spans(), &mut failing, Pass::RECORD).map(|_| ())
                };
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("source projection unavailable"),
                    "{at:?}"
                );
                assert_eq!(
                    adapter.side_reads.load(std::sync::atomic::Ordering::Relaxed),
                    0,
                    "{at:?}: no finish after failure"
                );
                assert!(crate::catalog::AffordanceTable::load().counts(&r).is_none());
                assert!(crate::resume::MarkTable::load().get(&r).is_none());
                if matches!(at, FailAt::Message) {
                    assert_eq!((failing.seen, failing.metas, failing.dependencies), (2, 1, 1));
                    assert_eq!(failing.checkpoints, usize::from(resumable));
                }
                if matches!(at, FailAt::Meta) {
                    assert_eq!((failing.seen, failing.metas), (0, 1));
                }
            }
        }
    }

    #[test]
    fn tee_errors_abort_both_but_intentional_stop_keeps_the_other_live() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut failure = FailingSink::new(FailAt::Message, flag);
        let mut other = Keep::default();
        let mut tee = TeeSink::new(&mut failure, &mut other);
        let mut affordances = crate::affordance::AffordanceSink::new(crate::Harness::Codex, &mut tee);
        let error = checked(&mut affordances, |sink| {
            assert_eq!(sink.message(text_msg("first")), Flow::Continue);
            assert_eq!(sink.message(text_msg("second")), Flow::Stop);
            // Even if a legacy caller drains and ignores the diagnostic, the failed latch holds.
            assert!(sink.take_error().is_some());
            assert_eq!(sink.message(text_msg("late")), Flow::Stop);
            sink.meta(&Session::new("test", crate::Harness::Codex));
            sink.checkpoint(&mut Checkpoint::new(3));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("already failed"));
        assert_eq!(failure.seen, 2);
        assert_eq!(other.msgs.len(), 1, "failed first branch stops its peer immediately");
        assert!(other.cps.is_empty());

        let mut one = |_m: Message| Flow::Stop;
        let mut two = Keep::default();
        let mut tee = TeeSink::new(&mut one, &mut two);
        checked(&mut tee, |sink| {
            assert_eq!(sink.try_message(text_msg("one"))?, Flow::Continue);
            assert_eq!(sink.try_message(text_msg("two"))?, Flow::Continue);
            Ok(())
        })
        .unwrap();
        assert_eq!(two.msgs.len(), 2);
    }

    #[test]
    fn built_in_stream_boundary_and_nested_guard_return_the_original_error() {
        let w = crate::test_world("built-in-fallible-stream");
        let dir = w.base.join("projects/group/chats");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("q.jsonl");
        let record = r#"{"uuid":"u","type":"user","message":{"role":"user","parts":[{"text":"hello"}]}}"#;
        std::fs::write(&path, format!("{record}\n{record}\n{record}\n")).unwrap();
        let r = transcript(crate::Harness::Qwen, path);
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        for checked_entry in [false, true] {
            let mut failing = FailingSink::new(FailAt::Message, Default::default());
            let result = if checked_entry {
                adapter.try_stream(&r, &Demand::LEAN.with_spans(), &mut failing)
            } else {
                adapter.stream(&r, &Demand::LEAN.with_spans(), &mut failing)
            };
            assert_eq!(result.unwrap_err().to_string(), "source projection unavailable");
            assert_eq!(failing.seen, 2);
        }
    }

    /// The claude and codex transcripts of the harness corpus (the two adapters that resume).
    fn corpus_transcripts() -> Vec<SessionRef> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut out = Vec::new();
        for (dir, harness) in [
            ("claude", crate::ir::Harness::Claude),
            ("codex", crate::ir::Harness::Codex),
        ] {
            for e in walkdir::WalkDir::new(root.join(dir)).sort_by_file_name() {
                let p = e.unwrap().into_path();
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                let top = match harness {
                    crate::ir::Harness::Claude => p
                        .parent()
                        .and_then(|d| d.parent())
                        .is_some_and(|d| d.ends_with("projects")),
                    _ => name.starts_with("rollout-"),
                };
                if top && name.ends_with(".jsonl") {
                    out.push(transcript(harness, p));
                }
            }
        }
        out
    }

    fn transcript(harness: crate::ir::Harness, path: std::path::PathBuf) -> SessionRef {
        SessionRef {
            id: path.file_stem().unwrap().to_string_lossy().into_owned(),
            harness,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        }
    }

    /// One pass: the messages, the session (as JSON), the counts, and every checkpoint offered.
    fn pass(r: &SessionRef, from: Option<&Checkpoint>) -> (Keep, serde_json::Value, Counted) {
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let mut keep = Keep::default();
        let done = stream_counted(
            &*adapter,
            r,
            &Demand::LEAN.with_spans(),
            &mut keep,
            crate::stream::Pass {
                from,
                every: Some(1),
                amortized: false,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("{}: {e:#}", r.path.display()));
        let session = serde_json::to_value(&done.session).unwrap();
        (keep, session, done.counted.expect("a whole pass counts"))
    }

    /// Resuming at any checkpoint a pass took reproduces the rest of that pass: the same messages
    /// from the checkpoint's message on, the same session, the same counts, and the same later
    /// checkpoints. Over every claude and codex transcript of the corpus, at up to a dozen
    /// checkpoints spread through each.
    #[test]
    fn a_resumed_pass_is_the_rest_of_the_whole_pass() {
        let transcripts = corpus_transcripts();
        assert!(transcripts.len() >= 20, "corpus moved? found {}", transcripts.len());
        let mut resumed = 0;
        for r in &transcripts {
            let (full, session, counted) = pass(r, None);
            assert!(full.cps.len() > 1, "{}: no checkpoints", r.path.display());
            let step = (full.cps.len() / 12).max(1);
            for (i, cp) in full.cps.iter().enumerate().step_by(step) {
                let (rest, s, c) = pass(r, Some(cp));
                let n = cp.messages().unwrap();
                let at = format!("{} @ {}", r.path.display(), cp.offset);
                assert_eq!(rest.msgs, full.msgs[n..], "{at}: messages");
                assert_eq!(s, session, "{at}: session");
                assert_eq!((c.turns, &c.counts), (counted.turns, &counted.counts), "{at}: counts");
                assert!(rest.cps == full.cps[i + 1..], "{at}: later checkpoints");
                resumed += 1;
            }
        }
        assert!(resumed > 200, "only {resumed} resumes");
    }

    /// Native Qwen records finalize their own messages, but emission needs prefix call/model
    /// state. Compression and rewind remain IR markers; legacy Gemini replay declines resume.
    #[test]
    fn qwen_record_checkpoints_preserve_state_and_partial_eof_records() {
        let w = crate::test_world("qwen-record-resume");
        let dir = w.base.join("projects/group/chats");
        std::fs::create_dir_all(&dir).unwrap();
        let r = transcript(crate::Harness::Qwen, dir.join("s1.jsonl"));
        let rec = |id: &str, body: serde_json::Value| {
            let mut v = serde_json::json!({"uuid":id,"sessionId":"s1","cwd":"/scratch",
                "version":"0.25.1","timestamp":"2026-10-11T00:00:01Z"});
            v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
            v.to_string()
        };
        let lines = [
            rec(
                "u1",
                serde_json::json!({"type":"user","provenance":"real_user","message":{"role":"user",
                "parts":[{"text":"first prompt"},{"text":"<qwen:user-prompt-submit-context>\nhook\n</qwen:user-prompt-submit-context>"}]}}),
            ),
            rec(
                "a1",
                serde_json::json!({"type":"assistant","model":"first-model","message":{"role":"model",
                "parts":[{"functionCall":{"id":"c1","name":"tool_call","args":{"name":"agent","arguments":{"prompt":"investigate"}}}}]},
                "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":3}}),
            ),
            "invalid json".into(),
            "".into(),           // Empty rows never enter the nonempty positioned JSON API.
            " \u{2003} ".into(), // Blank physical rows still provide safe resume boundaries.
            "[]".into(),         // Accepted non-object rows also preserve checkpoint boundaries.
            rec(
                "r1",
                serde_json::json!({"type":"tool_result","message":{"role":"user","parts":[
                {"functionResponse":{"id":"c1","name":"tool_call","response":{"output":"returned"}}}]},
                "toolCallResult":{"callId":"c1","status":"success"}}),
            ),
            rec(
                "c1",
                serde_json::json!({"type":"system","subtype":"chat_compression","systemPayload":{
                "info":{"originalTokenCount":9,"newTokenCount":7},"compressedHistory":[{"role":"user","parts":[{"text":"summary"}]}]}}),
            ),
            rec(
                "b1",
                serde_json::json!({"type":"system","subtype":"rewind","systemPayload":{"targetMessageUuid":"u1"}}),
            ),
            rec(
                "t1",
                serde_json::json!({"type":"system","subtype":"custom_title","systemPayload":{"title":"late title"}}),
            ),
            rec(
                "a2",
                serde_json::json!({"type":"assistant","model":"next-model","gitBranch":"other",
                "message":{"role":"model","parts":[{"text":"tail answer"}]}}),
            ),
        ];
        let bytes = lines.join("\n"); // A valid final EOF record, deliberately without newline.
        std::fs::write(&r.path, &bytes).unwrap();
        let (full, session, counted) = pass(&r, None);
        assert_eq!(full.cps.len(), lines.len() - 1);
        assert!(full.cps.last().unwrap().offset < bytes.len() as u64);
        for (i, cp) in full.cps.iter().enumerate() {
            let (rest, s, c) = pass(&r, Some(cp));
            assert_eq!(rest.msgs, full.msgs[cp.messages().unwrap()..]);
            assert_eq!(s, session, "checkpoint {i}: metadata");
            assert_eq!((c.turns, c.counts), (counted.turns, counted.counts.clone()));
            assert_eq!(rest.cps, full.cps[i + 1..]);
        }
        let cp = full.cps.last().unwrap();
        let grown = format!(
            "{bytes}\n{}\n",
            rec(
                "u2",
                serde_json::json!({"type":"user","message":{"role":"user","parts":[{"text":"appended"}]}})
            )
        );
        std::fs::write(&r.path, &grown).unwrap();
        let (whole, expected, counts) = pass(&r, None);
        let (rest, got, c) = pass(&r, Some(cp));
        assert_eq!(rest.msgs, whole.msgs[cp.messages().unwrap()..]);
        assert_eq!(got, expected);
        assert_eq!((c.turns, c.counts), (counts.turns, counts.counts));

        // Every physical byte prefix counts exactly what the full parser saw at that baseline.
        let before = dir.join("prefix.jsonl");
        for cut in (0..grown.len()).step_by(17).chain([grown.len()]) {
            std::fs::write(&before, &grown.as_bytes()[..cut]).unwrap();
            let mut count = 0;
            let mut sink = |_: Message| {
                count += 1;
                Flow::Continue
            };
            crate::harness::for_harness(crate::Harness::Qwen)
                .unwrap()
                .stream(
                    &transcript(crate::Harness::Qwen, before.clone()),
                    &Demand::RICH,
                    &mut sink,
                )
                .unwrap();
            assert_eq!(
                crate::watch::resolve(&r, crate::watch::Mark::Bytes(cut as u64)),
                Some(count),
                "cut {cut}"
            );
        }
        assert_eq!(
            crate::watch::baseline(&r),
            Some(crate::watch::Mark::Bytes(grown.len() as u64))
        );

        let legacy = transcript(crate::Harness::Qwen, w.base.join("tmp/hash/chats/session.jsonl"));
        let adapter = crate::harness::for_harness(crate::Harness::Qwen).unwrap();
        let mut keep = Keep::default();
        assert!(adapter
            .transcript(&legacy, &Demand::LEAN.with_spans(), Start::Checkpoint(cp), &mut keep)
            .unwrap()
            .is_none());
        assert!(
            keep.msgs.is_empty() && keep.cps.is_empty(),
            "decline must touch nothing"
        );
    }

    /// The append case: a pass over a file whose writer stopped mid-line takes its last checkpoint
    /// before that line; once the line and more are appended, resuming there gives what a whole
    /// pass over the grown file gives.
    #[test]
    fn a_pass_resumes_across_an_append_that_finishes_a_partial_line() {
        let dir = std::env::temp_dir().join(format!("cv-resume-{}", uuid::Uuid::new_v4()));
        for src in corpus_transcripts().into_iter().step_by(4) {
            let bytes = std::fs::read(&src.path).unwrap();
            let cut = bytes.len() * 2 / 3;
            assert_ne!(bytes[cut - 1], b'\n', "pick a cut inside a line");
            let path = dir.join(src.path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes[..cut]).unwrap();
            let r = transcript(src.harness, path.clone());
            let (head, _, _) = pass(&r, None);
            let last = head.cps.last().unwrap().clone();
            assert!(last.offset < cut as u64);
            assert_eq!(bytes[last.offset as usize - 1], b'\n');

            std::fs::write(&path, &bytes).unwrap();
            let (full, session, counted) = pass(&r, None);
            let (rest, s, c) = pass(&r, Some(&last));
            let n = last.messages().unwrap();
            assert_eq!(head.msgs[..n], full.msgs[..n]);
            assert_eq!(rest.msgs, full.msgs[n..], "{}", src.path.display());
            assert_eq!(s, session, "{}", src.path.display());
            assert_eq!((c.turns, c.counts), (counted.turns, counted.counts));

            // Resumed at the last checkpoint, the pass takes no new one: it hands that one back,
            // so the next pass still has a mark to resume from.
            let final_cp = full.cps.last().unwrap();
            let adapter = crate::harness::for_harness(r.harness).unwrap();
            let done = stream_counted(
                &*adapter,
                &r,
                &Demand::LEAN.with_spans(),
                &mut Keep::default(),
                crate::stream::Pass {
                    from: Some(final_cp),
                    every: Some(1),
                    amortized: false,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(done.last.as_ref(), Some(final_cp));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
