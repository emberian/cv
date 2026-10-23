//! Live session following — the one loop behind `cv scry`, `cv watch`, and the MCP `await_omen`
//! and `observe_stream` tools (the last one across processes, through a [`Cursor`]).
//!
//! It polls discovery on an interval, diffs each session's message tail against what it saw last,
//! and emits [`SessionEvent`]s for new sessions and newly-appended messages. Polling (rather than
//! inotify) keeps it uniform across all harness layouts (single file, dir-of-files, sqlite).

use crate::harness;
use crate::ir::{Harness, Message, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod read;
pub use read::{read as read_tail, read_session, read_session_into, Held, ReadPass, ReadState};

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A session we'd never seen before appeared.
    New,
    /// An existing session gained messages.
    Updated,
}

/// A live event about a session.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub kind: EventKind,
    pub reference: SessionRef,
    /// For `New`: the whole conversation. For `Updated`: only the newly-appended messages.
    pub new_messages: Vec<Message>,
}

/// Narrow what to follow.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub harness: Option<Harness>,
    pub cwd_contains: Option<String>,
}

impl Filter {
    /// Whether a session passes this filter (harness + cwd-substring). Public so non-`Watcher`
    /// consumers (e.g. the MCP `observe_stream` tool) can reuse the exact same filtering semantics
    /// rather than re-deriving them and drifting.
    pub fn matches(&self, r: &SessionRef) -> bool {
        if let Some(h) = self.harness {
            if r.harness != h {
                return false;
            }
        }
        if let Some(needle) = &self.cwd_contains {
            if !r.cwd.as_ref().is_some_and(|p| p.to_string_lossy().contains(needle)) {
                return false;
            }
        }
        true
    }
}

fn resolve_checked(r: &SessionRef, mark: Mark, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<usize> {
    check()?;
    let count = match mark {
        Mark::Messages(n) => n,
        Mark::Bytes(len) if append_only(r) => count_prefix(r, len, check)?,
        Mark::Bytes(_) => anyhow::bail!("watch byte mark unsupported for {}", r.harness),
    };
    check()?;
    Ok(count)
}

/// How many messages a whole read of `r`'s first `len` bytes yields: the count a read would have
/// returned when the append-only transcript was `len` bytes long ([`crate::stream::Start::Prefix`]).
/// `check` runs at every finished line the reader could resume after (its checkpoint offers),
/// blank ones included.
pub(crate) fn count_prefix(
    r: &SessionRef,
    len: u64,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<usize> {
    struct Count<'c> {
        messages: usize,
        check: &'c mut dyn FnMut() -> anyhow::Result<()>,
        error: Option<anyhow::Error>,
    }
    impl crate::MessageSink for Count<'_> {
        fn message(&mut self, _: Message) -> crate::Flow {
            self.messages += 1;
            crate::Flow::Continue
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            if let Err(error) = (self.check)() {
                self.error = Some(error);
            }
            false
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.error.take()
        }
    }
    let adapter = harness::rootless(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
    let mut count = Count {
        messages: 0,
        check,
        error: None,
    };
    adapter
        .transcript(r, &crate::Demand::RICH, crate::stream::Start::Prefix(len), &mut count)?
        .ok_or_else(|| anyhow::anyhow!("{} declined a byte prefix of {}", r.harness, r.path.display()))?;
    Ok(count.messages)
}

/// Where a follower stands in one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// This many parsed IR messages have already been seen.
    Messages(usize),
    /// Not parsed yet: the transcript was this many bytes long when the follower started. An
    /// append-only JSONL transcript's first `n` bytes are exactly what a parse at that moment would
    /// have read, so [`resolve`] recovers the same message count later — when, and only if, the
    /// session changes.
    Bytes(u64),
}

/// A transcript that only ever grows by appending lines, so a byte length pins a past state
/// ([`harness::Adapter::append_only`]). A byte mark on any other session (a compressed rollout, a
/// rewritten recording) resolves to nothing.
fn append_only(r: &SessionRef) -> bool {
    harness::rootless(r.harness).is_some_and(|adapter| adapter.append_only(r))
}

/// Record where `r` stands now, as cheaply as possible: one `stat` for an append-only transcript,
/// a streamed count otherwise. `None` when the session can't be read.
///
/// Following "from now on" used to parse every matching session up front just to learn its
/// message count — tens of seconds and ~1 GB transient on a large corpus, almost all of it for
/// sessions that never change while followed. A byte mark defers that parse to [`resolve`].
pub fn baseline(r: &SessionRef) -> Option<Mark> {
    if append_only(r) {
        if let Ok(md) = std::fs::metadata(&r.path) {
            return Some(Mark::Bytes(md.len()));
        }
    }
    count(r).map(Mark::Messages)
}

// Capture the source before the mark. Capturing it afterward can bless an append that
// happened after the byte mark, suppressing that unread append behind an unchanged trigger.
fn capture_baseline<T>(r: &SessionRef, mark: impl FnOnce() -> T) -> (T, ReadState) {
    let read = ReadState::baseline(r);
    (mark(), read)
}

/// The message count a full parse of `r` returns, without holding the parsed session.
///
/// It is the parse's own count by construction: the session is streamed through the adapter and
/// nothing is kept. A count taken any other way (a row count, a hand-written survival query) is a
/// second definition of "which records become messages" that has to agree with the adapter's
/// mapping forever, and a mark that disagrees with the parse makes a follower skip or repeat.
fn count(r: &SessionRef) -> Option<usize> {
    tail(r, usize::MAX, 0).map(|(total, _)| total)
}

/// What a full parse of `r` returns past the first `skip` messages: `(total, messages[skip..])`,
/// keeping at most `keep` of them. Every adapter's `parse` is its `stream` into a collecting sink
/// ([`crate::stream::collect`]), so streaming and keeping only the tail gives the same messages
/// while holding O(tail) instead of the whole transcript — what a follower needs, since it only
/// reports what was appended.
pub fn tail(r: &SessionRef, skip: usize, keep: usize) -> Option<(usize, Vec<Message>)> {
    let adapter = harness::for_harness(r.harness)?;
    let mut total = 0usize;
    let mut kept = Vec::new();
    let mut sink = |m: Message| {
        if total >= skip && kept.len() < keep {
            kept.push(m);
        }
        total += 1;
        crate::stream::Flow::Continue
    };
    let session = adapter.try_stream(r, &crate::stream::Demand::RICH, &mut sink).ok()?;
    // As `stream::collect` does: own any lazy content (full-fidelity streams rarely produce any).
    let resolver = session.resolver();
    for m in &mut kept {
        m.materialize(&resolver).ok()?;
    }
    Some((total, kept))
}

/// The parsed message count `mark` stands for. `None` when a byte mark's transcript can't be read.
pub fn resolve(r: &SessionRef, mark: Mark) -> Option<usize> {
    match mark {
        Mark::Messages(n) => Some(n),
        Mark::Bytes(len) => append_only(r)
            .then(|| count_prefix(r, len, &mut || Ok(())).ok())
            .flatten(),
    }
}

/// Where a follower stands in one session.
#[derive(Debug, Clone)]
struct State {
    /// Cheap change signal from discovery (no full parse needed to detect change).
    trigger: (Option<usize>, Option<i64>),
    /// How far we've already reported.
    mark: Mark,
    /// Messages past `mark` were left unread when a bounded poll's budget ran out: the next poll
    /// reads this session again even if its trigger has not moved, or the rest would wait for the
    /// session's next append.
    pending: bool,
    read: Option<ReadState>,
}

impl State {
    /// Whether the session must be read again: its trigger moved, a bounded poll left messages
    /// pending, or its source no longer matches the recorded baseline.
    fn stale(&self, trigger: (Option<usize>, Option<i64>), r: &SessionRef) -> bool {
        self.trigger != trigger
            || self.pending
            || self
                .read
                .as_ref()
                .map_or(matches!(self.mark, Mark::Messages(_)), |read| {
                    !read.baseline_unchanged(r)
                })
    }
}

/// One session's position, as the cursor carries it. The field names are the wire: `t` the
/// trigger, `n` the parsed count, `b` a byte baseline not yet resolved (see [`Mark::Bytes`]),
/// `more` the pending flag. Exactly one of `n` and `b` holds the mark.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pos {
    t: (Option<usize>, Option<i64>),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    b: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    more: bool,
    /// A reader state riding inline: what cursors before 0.18 carried for up to eight sessions.
    /// Still read; never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    r: Option<ReadState>,
    /// The generation of this session's reader state in the issuing process's [`Readers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    h: Option<u64>,
}

impl From<&State> for Pos {
    fn from(s: &State) -> Pos {
        let (n, b) = match s.mark {
            Mark::Messages(n) => (Some(n), None),
            Mark::Bytes(len) => (None, Some(len)),
        };
        Pos {
            t: s.trigger,
            n,
            b,
            more: s.pending,
            r: None,
            h: None,
        }
    }
}

impl Pos {
    fn mark(&self) -> Option<Mark> {
        match (self.n, self.b) {
            (Some(n), None) => Some(Mark::Messages(n)),
            (None, Some(len)) => Some(Mark::Bytes(len)),
            _ => None,
        }
    }
}

/// A [`Watcher`]'s whole position, serializable: the `seen` map as a compact JSON string, so a
/// stateless caller (the MCP `observe_stream`, one `tools/call` at a time) hands it back on its
/// next call and resumes exactly where an in-memory watcher would have. Versioned: a cursor from
/// another schema decodes to `None`, and the caller starts a fresh baseline — an unusable cursor
/// is NOT "no prior positions" (that would make every session look new and dump its history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    v: u32,
    o: BTreeMap<String, Pos>,
}

const CURSOR_VERSION: u32 = 2;

impl Cursor {
    /// The compact string a caller carries between polls.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a cursor serializes")
    }

    /// `None` when `s` is not a cursor this build issued (unparseable, another version, or a
    /// position without exactly one mark).
    pub fn decode(s: &str) -> Option<Cursor> {
        serde_json::from_str::<Cursor>(s)
            .ok()
            .filter(|c| c.v == CURSOR_VERSION && c.o.values().all(|p| p.mark().is_some()))
    }

    /// How many sessions the cursor positions.
    pub fn len(&self) -> usize {
        self.o.len()
    }

    pub fn is_empty(&self) -> bool {
        self.o.is_empty()
    }
}

/// The reader states behind the cursors one process issued, one per observed session: each
/// cursor position names its state's generation, and the state stays here, not on the wire. The
/// bound is one state per session this process observed, whatever the number of sessions or the
/// size of a state. A position whose generation is not here (a cursor another process issued,
/// or a second observer of the session replaced its state) reads that session from its start
/// once; [`Watcher::rereads`] counts those, and `observe_stream` reports them.
pub struct Readers {
    states: std::sync::Mutex<std::collections::HashMap<String, (u64, ReadState)>>,
    next: std::sync::atomic::AtomicU64,
}

impl Default for Readers {
    fn default() -> Self {
        // Generations start at a per-process point, so another process's cursor does not name
        // one of ours by coincidence.
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            ^ (u64::from(std::process::id()) << 40);
        Self {
            states: Default::default(),
            next: std::sync::atomic::AtomicU64::new(start),
        }
    }
}

impl Readers {
    fn get(&self, key: &str, generation: u64) -> Option<ReadState> {
        let states = self.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        states
            .get(key)
            .filter(|(held, _)| *held == generation)
            .map(|(_, read)| read.clone())
    }

    fn put(&self, key: &str, read: &ReadState) -> u64 {
        let generation = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string(), (generation, read.clone()));
        generation
    }

    /// How many sessions hold a reader state.
    pub fn len(&self) -> usize {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether `key` holds a reader state. A `Readers` serves every observer
    /// in its process, so a test asserting on `len` races with unrelated
    /// sessions: the per-key question is the isolation-proof one.
    pub fn contains(&self, key: &str) -> bool {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A stateful poller. Call [`poll`](Watcher::poll) repeatedly, or [`run`](Watcher::run) to loop;
/// [`to_cursor`](Watcher::to_cursor) / [`from_cursor`](Watcher::from_cursor) carry the state
/// across processes. Every poller in cv (`cv scry`, `cv watch`, MCP `await_omen` and
/// `observe_stream`) is this one loop: discover → cheap trigger → resolve the mark → stream the
/// tail — so every one of them compares against a parsed count, never a row count.
pub struct Watcher {
    filter: Filter,
    seen: BTreeMap<String, State>,
    /// The sessions whose reader state is the one stored under this generation in a
    /// [`Readers`]: written by [`Watcher::to_cursor`] only for states that changed.
    held: BTreeMap<String, u64>,
    /// Updated sessions read from their start for want of a reader state ([`Watcher::rereads`]).
    rereads: usize,
    /// Where sessions come from: [`crate::discover_all`] (stat the fleet, the default) or
    /// [`crate::sessions`] (the catalog fast path, for a poller that runs every few seconds).
    discover: Box<dyn FnMut() -> Vec<SessionRef>>,
}

/// What one [`Watcher::poll`] asks for.
pub struct Poll<'c> {
    /// Messages offered across the call; the rest stay pending.
    pub max: usize,
    /// An unreadable session or unresolvable prior position is an error, not a skip.
    pub strict: bool,
    /// Offered messages keep source-backed payloads; the callback resolves them through checked
    /// APIs, and a source change before acceptance fails the poll.
    pub retained: bool,
    /// Run before discovery, between sessions and at read and projection boundaries.
    pub checked: Option<&'c mut dyn FnMut() -> anyhow::Result<()>>,
}

impl Default for Poll<'_> {
    fn default() -> Self {
        Poll {
            max: usize::MAX,
            strict: false,
            retained: false,
            checked: None,
        }
    }
}

struct SessionPoll<'a> {
    kind: EventKind,
    reference: &'a SessionRef,
    adapter: &'a dyn harness::Adapter,
    previous: Option<&'a ReadState>,
    from: usize,
    max: usize,
}
struct Delivery {
    pass: ReadPass,
    taken: usize,
    stopped: bool,
}

impl Watcher {
    /// Create a watcher. If `emit_existing` is false, the current sessions are recorded silently so
    /// only activity *after* construction is reported (the usual `tail -f` feel).
    pub fn new(filter: Filter, emit_existing: bool) -> Self {
        Self::with_discovery(filter, emit_existing, crate::discover_all)
    }

    /// [`Watcher::new`] over another session source (see [`Watcher::discover`]).
    pub fn with_discovery(
        filter: Filter,
        emit_existing: bool,
        discover: impl FnMut() -> Vec<SessionRef> + 'static,
    ) -> Self {
        Self::with_discovery_until(filter, emit_existing, discover, || false)
    }

    /// [`Watcher::with_discovery`] whose baseline read stops once `expired` says so, for a
    /// follower bounded by a deadline; [`Watcher::following`] tells how far it got.
    pub fn with_discovery_until(
        filter: Filter,
        emit_existing: bool,
        discover: impl FnMut() -> Vec<SessionRef> + 'static,
        mut expired: impl FnMut() -> bool,
    ) -> Self {
        let mut w = Watcher {
            filter,
            seen: BTreeMap::new(),
            held: BTreeMap::new(),
            rereads: 0,
            discover: Box::new(discover),
        };
        if !emit_existing {
            w.prime(&mut expired);
        }
        w
    }

    /// Construct a demanded follower with cooperative checks around discovery and baseline
    /// reads. Discovery remains the supplied synchronous operation; non-append-only baselines
    /// check adapter streaming boundaries rather than collecting existing messages.
    pub fn try_with_discovery(
        filter: Filter,
        emit_existing: bool,
        discover: impl FnMut() -> Vec<SessionRef> + 'static,
        mut check: impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        check()?;
        let mut watcher = Self::with_discovery(filter, true, discover);
        if emit_existing {
            return Ok(watcher);
        }
        let refs = watcher.discover();
        check()?;
        for r in refs {
            check()?;
            let (mark, read) = if append_only(&r) {
                let (mark, read) = capture_baseline(&r, || {
                    std::fs::metadata(&r.path).map(|metadata| Mark::Bytes(metadata.len()))
                });
                (mark?, read)
            } else {
                struct Baseline;
                impl crate::MessageSink for Baseline {
                    fn message(&mut self, _: Message) -> crate::Flow {
                        unreachable!("baseline demanded an existing message")
                    }
                }
                let adapter =
                    harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("unknown watch harness"))?;
                let demand = read::Demand {
                    output: Some(&mut Baseline),
                    check: Some(&mut check),
                    ..Default::default()
                };
                let pass = read::pass(adapter.as_ref(), &r, None, usize::MAX, usize::MAX, demand)?;
                (Mark::Messages(pass.total), pass.accepted(pass.total))
            };
            check()?;
            watcher.seen.insert(
                Self::key(&r),
                State {
                    trigger: Self::trigger_of(&r),
                    mark,
                    pending: false,
                    read: Some(read),
                },
            );
        }
        Ok(watcher)
    }

    /// Resume from a [`Cursor`]: the watcher stands exactly where the one that issued it stood,
    /// with each session's reader state from `readers` where the cursor names it.
    pub fn from_cursor(
        filter: Filter,
        cursor: Cursor,
        discover: impl FnMut() -> Vec<SessionRef> + 'static,
        readers: &Readers,
    ) -> Self {
        let mut held = BTreeMap::new();
        let seen = cursor
            .o
            .into_iter()
            .filter_map(|(k, p)| {
                let mark = p.mark()?;
                let read = match p
                    .h
                    .and_then(|generation| Some((generation, readers.get(&k, generation)?)))
                {
                    Some((generation, read)) => {
                        held.insert(k.clone(), generation);
                        Some(read)
                    }
                    None => p.r,
                };
                let state = State {
                    trigger: p.t,
                    mark,
                    pending: p.more,
                    read,
                };
                Some((k, state))
            })
            .collect();
        Watcher {
            filter,
            seen,
            held,
            rereads: 0,
            discover: Box::new(discover),
        }
    }

    /// Where this watcher stands, to hand to [`Watcher::from_cursor`] later. Every position
    /// rides the wire; each reader state stays in `readers`, stored again only when it changed.
    pub fn to_cursor(&mut self, readers: &Readers) -> Cursor {
        let mut o = BTreeMap::new();
        for (k, s) in &self.seen {
            let mut pos = Pos::from(s);
            if let Some(read) = &s.read {
                let generation = *self.held.entry(k.clone()).or_insert_with(|| readers.put(k, read));
                pos.h = Some(generation);
            }
            o.insert(k.clone(), pos);
        }
        Cursor { v: CURSOR_VERSION, o }
    }

    /// Updated sessions this watcher read from their start because it held no reader state for
    /// them: the cost a cursor carried to another process, or a shared session, pays once.
    pub fn rereads(&self) -> usize {
        self.rereads
    }

    fn key(r: &SessionRef) -> String {
        format!("{}:{}", r.harness.as_str(), r.id)
    }

    /// How many sessions hold a position (the count a follower announces once primed).
    pub fn following(&self) -> usize {
        self.seen.len()
    }

    fn trigger_of(r: &SessionRef) -> (Option<usize>, Option<i64>) {
        (r.message_count, r.updated_at.map(|t| t.timestamp_millis()))
    }

    /// Record current state without emitting anything.
    fn prime(&mut self, expired: &mut impl FnMut() -> bool) {
        for r in self.discover() {
            if expired() {
                return;
            }
            // The cheap discover `message_count` is NOT the parsed IR length for several harnesses
            // (codex/hermes/claude add reasoning/tool/system turns), so seeding from it makes the
            // first `Updated` poll re-emit messages that predate the watcher. Record the true
            // baseline instead — a byte mark where the transcript is append-only (see [`baseline`]).
            // A session without one (unreadable now) stays unseen: the next poll surfaces it as
            // New rather than following it from a guessed position, which would replay or skip.
            let (mark, read) = capture_baseline(&r, || baseline(&r));
            let Some(mark) = mark else {
                continue;
            };
            self.held.remove(&Self::key(&r));
            self.seen.insert(
                Self::key(&r),
                State {
                    trigger: Self::trigger_of(&r),
                    mark,
                    pending: false,
                    read: Some(read),
                },
            );
        }
    }

    /// The sessions to follow, oldest-touched first, so a bounded poll yields a chronological
    /// slice. Followers use full discovery independently of catalog synchronization. The
    /// identity-aware per-file scan cache avoids reparsing unchanged transcripts. Public as the
    /// one-shot form of a follower: its current state is this list.
    pub fn discover(&mut self) -> Vec<SessionRef> {
        let mut refs: Vec<SessionRef> = (self.discover)()
            .into_iter()
            .filter(|r| self.filter.matches(r))
            .collect();
        // Positions are keyed by harness:id, so multiple concrete copies must have one
        // authority. Otherwise an old and new copy overwrite the same cursor in one poll.
        refs.sort_by(|a, b| {
            a.harness
                .as_str()
                .cmp(b.harness.as_str())
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| a.preferred_copy_cmp(b))
        });
        refs.dedup_by(|a, b| a.harness == b.harness && a.id == b.id);
        refs.sort_by_key(|r| r.updated_at.or(r.created_at));
        refs
    }

    // Reconcile identities, not just fleet size: an equally sized replacement can remove
    // a session too. Reuse these keys during polling and allocate the live set only when
    // an existing position actually disappeared.
    fn discover_live(&mut self) -> Vec<(String, SessionRef)> {
        let refs: Vec<_> = self.discover().into_iter().map(|r| (Self::key(&r), r)).collect();
        let existing = refs.iter().filter(|(key, _)| self.seen.contains_key(key)).count();
        if existing < self.seen.len() {
            let live: std::collections::HashSet<&str> = refs.iter().map(|(key, _)| key.as_str()).collect();
            self.seen.retain(|key, _| live.contains(key.as_str()));
        }
        refs
    }

    /// Poll once under `poll`, offering each session's newly appended messages to `take`, which
    /// returns how many of them it consumed. What it leaves (and every message past
    /// [`Poll::max`]) stays pending: the next poll re-reads that session even if its cheap
    /// trigger has not moved. Returns whether anything is pending. Best effort: an error is
    /// "nothing pending" and leaves every position retryable; [`try_poll`](Self::try_poll) says why.
    pub fn poll(&mut self, poll: Poll<'_>, take: impl FnMut(SessionEvent) -> usize) -> bool {
        self.try_poll(poll, take).unwrap_or(false)
    }

    /// Deliver new messages one at a time in discovery/message order. `check` runs before
    /// discovery, between sessions, and at streaming message/dependency/checkpoint boundaries.
    /// A consumer stop accepts only delivered messages and leaves unread records pending.
    /// Projection/read/check failures never accept the current session's position.
    pub fn try_poll_messages(
        &mut self,
        mut check: impl FnMut() -> anyhow::Result<()>,
        mut take: impl FnMut(EventKind, &SessionRef, Message) -> anyhow::Result<crate::Flow>,
    ) -> anyhow::Result<bool> {
        self.poll_sessions(
            Poll {
                strict: true,
                checked: Some(&mut check),
                ..Poll::default()
            },
            |session, check| {
                let mut taken = 0;
                let mut stopped = false;
                let mut consumer = crate::TryFnSink::new(|message| {
                    let flow = take(session.kind, session.reference, message)?;
                    taken += 1;
                    stopped = flow == crate::Flow::Stop;
                    Ok(flow)
                });
                let pass = read::pass(
                    session.adapter,
                    session.reference,
                    session.previous,
                    session.from,
                    session.max,
                    read::Demand {
                        output: Some(&mut consumer),
                        check: Some(check),
                        ..Default::default()
                    },
                )?;
                Ok(Delivery { pass, taken, stopped })
            },
        )
    }

    /// [`poll`](Self::poll), with failures returned: under [`Poll::strict`] an unreadable session
    /// or an unresolvable prior position is an error; otherwise it stays retryable.
    /// [`Poll::checked`] failures are always errors.
    pub fn try_poll(&mut self, poll: Poll<'_>, mut take: impl FnMut(SessionEvent) -> usize) -> anyhow::Result<bool> {
        let retained = poll.retained;
        let controlled = poll.checked.is_some();
        self.poll_sessions(poll, |session, check| {
            let mut pass = read::pass(
                session.adapter,
                session.reference,
                session.previous,
                session.from,
                session.max,
                read::Demand {
                    retained,
                    check: (retained && controlled).then_some(check),
                    ..Default::default()
                },
            )?;
            let messages = std::mem::take(&mut pass.messages);
            let offered = messages.len();
            let taken = if offered == 0 {
                0
            } else {
                take(SessionEvent {
                    kind: session.kind,
                    reference: session.reference.clone(),
                    new_messages: messages,
                })
                .min(offered)
            };
            if retained {
                anyhow::ensure!(
                    pass.accepted(session.from.min(pass.total))
                        .fresh_with(session.adapter, session.reference),
                    "watched source changed during retained projection"
                );
            }
            Ok(Delivery {
                pass,
                taken,
                stopped: false,
            })
        })
    }

    /// One trigger and position rule for every message delivery shape.
    fn next_change(
        &self,
        key: &str,
        r: &SessionRef,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<(EventKind, usize)>> {
        match self.seen.get(key) {
            None => Ok(Some((EventKind::New, 0))),
            Some(state) if state.stale(Self::trigger_of(r), r) => {
                resolve_checked(r, state.mark, check).map(|from| Some((EventKind::Updated, from)))
            }
            Some(_) => Ok(None),
        }
    }

    /// The per-session callback owns delivery; this loop alone owns discovery, change detection,
    /// budgets and cursor acceptance. An error leaves the session's previous position intact.
    fn poll_sessions(
        &mut self,
        poll: Poll<'_>,
        mut deliver: impl FnMut(SessionPoll<'_>, &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<Delivery>,
    ) -> anyhow::Result<bool> {
        let Poll {
            max,
            strict,
            checked: mut check,
            ..
        } = poll;
        let controlled = check.is_some();
        let mut check = || match check.as_mut() {
            Some(check) => check(),
            None => Ok(()),
        };
        let _reads = crate::source::read_scope();
        check()?;
        let refs = self.discover_live();
        check()?;
        let mut consumed = 0usize;
        let mut pending = false;
        for (key, r) in refs {
            check()?;
            let change = self.next_change(&key, &r, &mut check);
            let (kind, from) = match change {
                Ok(Some(change)) => change,
                Ok(None) => continue,
                Err(error) if strict || controlled => return Err(error),
                Err(_) => continue,
            };
            check()?;
            let Some(adapter) = harness::for_harness(r.harness) else {
                anyhow::ensure!(
                    !strict,
                    "no adapter for watched {} session {}",
                    r.harness,
                    r.path.display()
                );
                continue;
            };
            let previous = self.seen.get(&key).and_then(|state| state.read.as_ref());
            if kind == EventKind::Updated && previous.is_none() {
                self.rereads += 1;
            }
            let result = deliver(
                SessionPoll {
                    kind,
                    reference: &r,
                    adapter: adapter.as_ref(),
                    previous,
                    from,
                    max: max.saturating_sub(consumed),
                },
                &mut check,
            );
            let Delivery { pass, taken, stopped } = match result {
                Ok(delivery) => delivery,
                Err(error) if strict => {
                    return Err(error.context(format!("reading watched {} session {}", r.harness, r.path.display())))
                }
                Err(_) => continue,
            };
            check()?;
            consumed = consumed.saturating_add(taken);
            let seen_to = from.min(pass.total) + taken;
            let left = !pass.reached_eof || pass.total > seen_to;
            pending |= left;
            self.held.remove(&key);
            self.seen.insert(
                key,
                State {
                    trigger: Self::trigger_of(&r),
                    mark: Mark::Messages(seen_to),
                    pending: left,
                    read: Some(pass.accepted(seen_to)),
                },
            );
            if stopped {
                return Ok(true);
            }
        }
        Ok(pending)
    }

    /// Offer every followed source on the same discovery/filter loop, without first reading its
    /// message tail. The consumer validates source and derived artifact freshness itself,
    /// performs its one required read and returns the completed count/baseline only after
    /// publication succeeds. Failure leaves the previous position untouched, so the next poll
    /// retries even without a new append.
    ///
    /// A followed source is offered again only when [`poll`](Self::poll) would read it (its
    /// trigger moved, or its source no longer matches the accepted baseline) or when `kept`
    /// says the consumer's own output for it no longer stands. An unchanged fleet costs one
    /// baseline check and one `kept` per session, not one consumer validation each.
    pub fn offer_sources(
        &mut self,
        mut kept: impl FnMut(&SessionRef) -> bool,
        mut take: impl FnMut(EventKind, &SessionRef, usize) -> Option<(usize, ReadState)>,
    ) -> usize {
        let _reads = crate::source::read_scope();
        let refs = self.discover_live();
        let mut accepted = 0;
        for (key, r) in refs {
            let trigger = Self::trigger_of(&r);
            let (kind, previous) = match self.seen.get(&key) {
                None => (EventKind::New, 0),
                Some(state) if !state.stale(trigger, &r) && kept(&r) => continue,
                Some(state) => {
                    let Some(previous) = resolve(&r, state.mark) else {
                        continue;
                    };
                    (EventKind::Updated, previous)
                }
            };
            let Some((total, read)) = take(kind, &r, previous) else {
                continue;
            };
            self.held.remove(&key);
            self.seen.insert(
                key,
                State {
                    trigger,
                    mark: Mark::Messages(total),
                    pending: false,
                    read: Some(read),
                },
            );
            accepted += 1;
        }
        accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static READERS: std::sync::LazyLock<Readers> = std::sync::LazyLock::new(Readers::default);

    /// Every pending event of one unbounded poll.
    fn events(watcher: &mut Watcher) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        watcher.poll(Poll::default(), |event| {
            let taken = event.new_messages.len();
            events.push(event);
            taken
        });
        events
    }

    fn sref(harness: Harness, path: std::path::PathBuf) -> SessionRef {
        SessionRef {
            id: "baseline-test".into(),
            harness,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        }
    }

    #[test]
    fn duplicate_session_copies_follow_one_preferred_source_in_both_discovery_orders() {
        fn copies() -> Vec<SessionRef> {
            let mut old = sref(Harness::Claude, "/copies/old.jsonl".into());
            old.updated_at = chrono::DateTime::from_timestamp(1, 0);
            let mut new = old.clone();
            new.path = "/copies/new.jsonl".into();
            new.updated_at = chrono::DateTime::from_timestamp(2, 0);
            let mut foreign = new.clone();
            foreign.harness = Harness::Codex;
            vec![old, foreign, new]
        }
        fn reversed() -> Vec<SessionRef> {
            let mut refs = copies();
            refs.reverse();
            refs
        }
        for discovery in [copies as fn() -> Vec<SessionRef>, reversed] {
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discovery);
            let selected = watcher.discover();
            assert_eq!(selected.len(), 2, "different harnesses retain separate positions");
            assert_eq!(
                selected.iter().find(|r| r.harness == Harness::Claude).unwrap().path,
                std::path::PathBuf::from("/copies/new.jsonl")
            );
        }
        // Apply the requested filter before choosing among copies: a matching older cwd
        // remains observable even when another copy lives outside the requested project.
        fn projects() -> Vec<SessionRef> {
            let mut refs = copies();
            refs[0].cwd = Some("/requested".into());
            refs[2].cwd = Some("/elsewhere".into());
            refs
        }
        let mut watcher = Watcher::with_discovery(
            Filter {
                cwd_contains: Some("requested".into()),
                ..Default::default()
            },
            true,
            projects,
        );
        assert_eq!(
            watcher.discover()[0].path,
            std::path::PathBuf::from("/copies/old.jsonl")
        );
    }

    #[test]
    fn duplicate_copies_do_not_reset_the_cursor_and_repeat_preferred_copy_tails() {
        let world = crate::test_world("watch-copy-tails");
        fn discovery() -> Vec<SessionRef> {
            let dir = std::path::PathBuf::from(crate::env::var_os("CLUSTERVISION_HOME").unwrap());
            let mut old = sref(Harness::Claude, dir.join("old.jsonl"));
            old.updated_at = chrono::DateTime::from_timestamp(1, 0);
            old.message_count = Some(1);
            let mut new = old.clone();
            new.path = dir.join("new.jsonl");
            new.updated_at = chrono::DateTime::from_timestamp(2, 0);
            new.message_count = Some(3); // Hold the trigger fixed; source freshness must catch append.
            vec![old, new]
        }
        let row = |text: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type":"user","sessionId":"baseline-test","message":{"role":"user","content":text}})
            )
        };
        let old = world.cv_home.join("old.jsonl");
        let new = world.cv_home.join("new.jsonl");
        std::fs::write(&old, row("old-copy")).unwrap();
        std::fs::write(&new, (0..3).map(|n| row(&format!("new-{n}"))).collect::<String>()).unwrap();
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discovery);
        let first = events(&mut watcher);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].reference.path, new);
        assert_eq!(first[0].new_messages.len(), 3);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&new)
            .unwrap()
            .write_all(row("new-3").as_bytes())
            .unwrap();
        let next = events(&mut watcher);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].new_messages.len(), 1);
        assert_eq!(next[0].new_messages[0].text().as_deref(), Some("new-3"));
        assert!(events(&mut watcher).is_empty());
    }

    #[test]
    fn prefix_resolution_checks_read_boundaries_even_without_messages() {
        let world = crate::test_world("checked-prefix-empty-rows");
        let dir = world.base.join("projects").join("fixture").join("chats");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefix.jsonl");
        let empty_rows = "\n".repeat(32 * 1024);
        std::fs::write(&path, &empty_rows).unwrap();
        for harness in [Harness::Claude, Harness::Codex, Harness::Qwen] {
            let r = SessionRef::new("prefix", harness, &path);
            let mark = Mark::Bytes(empty_rows.len() as u64);
            let mut checks = 0;
            let error = resolve_checked(&r, mark, &mut || {
                checks += 1;
                anyhow::ensure!(checks < 4, "cancelled during prefix framing");
                Ok(())
            })
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("cancelled during prefix framing"),
                "{harness}: {error:#}"
            );
            assert_eq!(checks, 4, "{harness} ignored the interrupted input");
            assert_eq!(
                resolve_checked(&r, mark, &mut || Ok(())).unwrap(),
                resolve(&r, mark).unwrap()
            );
        }
    }

    #[test]
    fn interrupted_first_update_prefix_keeps_byte_cursor_and_retries_only_the_append() {
        static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let world = crate::test_world("cancelled-prefix-cursor");
        let path = world.write_session(
            "prefix-cursor",
            &[serde_json::json!({"type":"user", "uuid":"u0", "message":{"role":"user", "content":"existing"}})],
        );
        PATH.set(path).unwrap();
        fn discover() -> Vec<SessionRef> {
            vec![SessionRef::new("prefix-cursor", Harness::Claude, PATH.get().unwrap())]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), false, discover);
        let before = serde_json::to_value(watcher.to_cursor(&READERS)).unwrap();
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(PATH.get().unwrap())
                .unwrap(),
            "{}",
            serde_json::json!({"type":"user", "uuid":"u1", "message":{"role":"user", "content":"appended"}})
        )
        .unwrap();
        let mut checks = 0;
        let error = watcher
            .try_poll_messages(
                || {
                    checks += 1;
                    anyhow::ensure!(checks < 5, "cancelled while resolving byte prefix");
                    Ok(())
                },
                |_, _, _| panic!("prefix cancellation reached delivery"),
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("cancelled while resolving byte prefix"),
            "{error:#}"
        );
        assert_eq!(serde_json::to_value(watcher.to_cursor(&READERS)).unwrap(), before);
        let mut texts = Vec::new();
        watcher
            .try_poll_messages(
                || Ok(()),
                |_, r, m| {
                    texts.push(crate::projection::searchable_text_checked(
                        &m,
                        &crate::Resolver::new(Some(r.path.clone())),
                        || Ok(()),
                    )?);
                    Ok(crate::Flow::Continue)
                },
            )
            .unwrap();
        assert_eq!(texts, ["appended\n"]);
    }

    #[test]
    fn a_session_unreadable_at_prime_is_announced_new_not_followed_from_a_guess() {
        static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let world = crate::test_world("unreadable-at-prime");
        PATH.set(world.base.join("later.jsonl")).unwrap();
        fn discover() -> Vec<SessionRef> {
            vec![SessionRef::new("later", Harness::Claude, PATH.get().unwrap())]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), false, discover);
        assert_eq!(watcher.following(), 0, "an unreadable session got a guessed position");
        let lines = [
            serde_json::json!({"type":"user", "uuid":"u0", "message":{"role":"user", "content":"one"}}),
            serde_json::json!({"type":"user", "uuid":"u1", "message":{"role":"user", "content":"two"}}),
        ];
        std::fs::write(PATH.get().unwrap(), cv_testkit::jsonl(&lines)).unwrap();
        let events = events(&mut watcher);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::New);
        assert_eq!(events[0].new_messages.len(), 2);
    }

    #[test]
    fn append_between_byte_mark_and_baseline_completion_is_delivered_with_unchanged_trigger() {
        static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let world = crate::test_world("baseline-append-race");
        let path = world.write_session(
            "baseline-race",
            &[serde_json::json!({"type":"user", "uuid":"u0", "message":{"role":"user", "content":"existing"}})],
        );
        PATH.set(path).unwrap();
        fn discover() -> Vec<SessionRef> {
            vec![SessionRef::new("baseline-race", Harness::Claude, PATH.get().unwrap())]
        }
        let r = discover().remove(0);
        let (mark, read) = capture_baseline(&r, || {
            let mark = baseline(&r).unwrap();
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
                "{}",
                serde_json::json!({"type":"user", "uuid":"u1", "message":{"role":"user", "content":"racing append"}})
            )
            .unwrap();
            mark
        });
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        watcher.seen.insert(
            Watcher::key(&r),
            State {
                trigger: Watcher::trigger_of(&r),
                mark,
                pending: false,
                read: Some(read),
            },
        );
        let cursor = watcher.to_cursor(&READERS);
        for incremental in [false, true] {
            let mut watcher = Watcher::from_cursor(Filter::default(), cursor.clone(), discover, &READERS);
            let mut texts = Vec::new();
            if incremental {
                watcher
                    .try_poll_messages(
                        || Ok(()),
                        |_, reference, message| {
                            texts.push(crate::projection::searchable_text_checked(
                                &message,
                                &crate::Resolver::new(Some(reference.path.clone())),
                                || Ok(()),
                            )?);
                            Ok(crate::Flow::Continue)
                        },
                    )
                    .unwrap();
            } else {
                for event in events(&mut watcher) {
                    for message in event.new_messages {
                        let mut text = String::new();
                        message.append_searchable(&mut text).unwrap();
                        texts.push(text);
                    }
                }
            }
            assert_eq!(texts, ["racing append\n"], "racing append was silently baselined");
        }
    }

    #[test]
    fn demanded_baseline_counts_existing_messages_and_checks_during_read() {
        static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let world = crate::test_world("controlled-baseline");
        let path = world.base.join("checkpoint-baseline.json");
        std::fs::write(
            &path,
            include_bytes!("../tests/fixtures/gemini/checkpoint_legacy_array.json"),
        )
        .unwrap();
        PATH.set(path).unwrap();
        fn discover() -> Vec<SessionRef> {
            vec![SessionRef::new("baseline", Harness::Gemini, PATH.get().unwrap())]
        }
        let expected = baseline(&discover().remove(0)).unwrap();
        let mut watcher = Watcher::try_with_discovery(Filter::default(), false, discover, || Ok(())).unwrap();
        assert_eq!(watcher.seen["gemini:baseline"].mark, expected);
        watcher
            .try_poll_messages(|| Ok(()), |_, _, _| panic!("baseline emitted existing history"))
            .unwrap();
        let mut checks = 0;
        let error = Watcher::try_with_discovery(Filter::default(), false, discover, || {
            checks += 1;
            anyhow::ensure!(checks <= 3, "cancelled inside baseline read");
            Ok(())
        })
        .err()
        .expect("baseline ignored cancellation");
        assert!(format!("{error:#}").contains("cancelled inside baseline"), "{error:#}");
        assert_eq!(checks, 4, "cancelled baseline continued reading");
    }

    #[test]
    fn incremental_stop_cursor_and_failed_callback_preserve_unseen_messages() {
        static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let world = crate::test_world("incremental-watch");
        let path = world.write_session("incremental", &(0..5).map(|n| serde_json::json!({
            "type":"user", "uuid":format!("u{n}"), "message":{"role":"user", "content":format!("message-{n}")}
        })).collect::<Vec<_>>());
        PATH.set(path).unwrap();
        fn discover() -> Vec<SessionRef> {
            vec![SessionRef::new("incremental", Harness::Claude, PATH.get().unwrap())]
        }
        fn text(r: &SessionRef, m: &Message) -> String {
            crate::projection::searchable_text_checked(m, &crate::Resolver::new(Some(r.path.clone())), || Ok(()))
                .unwrap()
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        let mut delivered = Vec::new();
        assert!(watcher
            .try_poll_messages(
                || Ok(()),
                |_, r, m| {
                    delivered.push(text(r, &m));
                    Ok(if delivered.len() == 2 {
                        crate::Flow::Stop
                    } else {
                        crate::Flow::Continue
                    })
                }
            )
            .unwrap());
        assert_eq!(delivered, ["message-0\n", "message-1\n"]);
        let cursor = Cursor::decode(&watcher.to_cursor(&READERS).encode()).unwrap();
        let mut resumed = Watcher::from_cursor(Filter::default(), cursor, discover, &READERS);
        let before_failure = resumed.to_cursor(&READERS).encode();
        assert!(resumed
            .try_poll_messages(|| Ok(()), |_, _, _| anyhow::bail!("projection failed"))
            .is_err());
        assert_eq!(
            resumed.to_cursor(&READERS).encode(),
            before_failure,
            "failed projection advanced position"
        );
        let mut rest = Vec::new();
        assert!(!resumed
            .try_poll_messages(
                || Ok(()),
                |_, r, m| {
                    rest.push(text(r, &m));
                    Ok(crate::Flow::Continue)
                }
            )
            .unwrap());
        assert_eq!(rest, ["message-2\n", "message-3\n", "message-4\n"]);
        assert!(!resumed
            .try_poll_messages(|| Ok(()), |_, _, _| panic!("idle watch repeated messages"))
            .unwrap());
    }

    #[test]
    fn interrupted_stream_retries_delivery_and_rejects_changed_source() {
        static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let world = crate::test_world("interrupted-watch");
        let path = world.write_session("interrupted", &(0..4).map(|n| serde_json::json!({
            "type":"user", "uuid":format!("u{n}"), "message":{"role":"user", "content":format!("message-{n}")}
        })).collect::<Vec<_>>());
        PATH.set(path).unwrap();
        fn discover() -> Vec<SessionRef> {
            vec![SessionRef::new("interrupted", Harness::Claude, PATH.get().unwrap())]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        let stop = std::cell::Cell::new(false);
        let mut count = 0;
        let error = watcher
            .try_poll_messages(
                || {
                    anyhow::ensure!(!stop.get(), "cancelled during read");
                    Ok(())
                },
                |_, _, _| {
                    count += 1;
                    stop.set(true);
                    Ok(crate::Flow::Continue)
                },
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert_eq!(count, 1, "cancellation did not stop the active read");
        assert!(
            watcher.to_cursor(&READERS).is_empty(),
            "failed read accepted its delivered prefix"
        );
        stop.set(false);
        let error = watcher
            .try_poll(
                Poll {
                    max: 1,
                    strict: true,
                    retained: true,
                    checked: Some(&mut || {
                        anyhow::ensure!(!stop.get(), "cancelled during projection");
                        Ok(())
                    }),
                },
                |_| {
                    stop.set(true);
                    1
                },
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled during projection"));
        assert!(
            watcher.to_cursor(&READERS).is_empty(),
            "cancelled projection accepted its position"
        );
        let error = watcher
            .try_poll_messages(
                || Ok(()),
                |_, r, _| {
                    std::fs::write(&r.path, "changed")?;
                    Ok(crate::Flow::Stop)
                },
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("source changed") || format!("{error:#}").contains("identity changed"),
            "{error:#}"
        );
        assert!(watcher.to_cursor(&READERS).is_empty(), "stale match advanced position");
    }

    #[test]
    fn every_observed_session_keeps_its_reader_state_whatever_its_size() {
        let _world = crate::test_world("watch-every-reader-state");
        let readers = Readers::default();
        let mut watcher = Watcher::with_discovery(Filter::default(), true, Vec::new);
        for n in 0..32 {
            let r = sref(
                Harness::Claude,
                std::path::PathBuf::from("/nonexistent/cv-watch-budget"),
            );
            let mut read = ReadState::baseline(&r);
            if n < 4 {
                let mut value = serde_json::to_value(&read).unwrap();
                value["version"] = serde_json::Value::String("x".repeat(65 * 1024));
                read = serde_json::from_value(value).unwrap();
            }
            watcher.seen.insert(
                format!("claude:{n}"),
                State {
                    trigger: (Some(n), None),
                    mark: Mark::Messages(n),
                    pending: n % 2 == 0,
                    read: Some(read),
                },
            );
        }
        // The contract: every position rides the wire, no reader state does, and every state,
        // of any size, is kept once for its session (32 here; 0.17 kept eight, under 64 KiB).
        let cursor = watcher.to_cursor(&readers);
        assert_eq!(cursor.len(), 32);
        assert_eq!(readers.len(), 32);
        assert!(cursor.o.values().all(|pos| pos.r.is_none() && pos.h.is_some()));
        assert!(cursor.encode().len() < 32 * 128, "positions only on the wire");
        for (key, state) in &watcher.seen {
            let pos = &cursor.o[key];
            let expected = Pos::from(state);
            assert_eq!(pos.t, expected.t, "{key}: trigger must survive intact");
            assert_eq!(pos.n, expected.n, "{key}: message mark must survive intact");
            assert_eq!(pos.b, expected.b, "{key}: byte mark must survive intact");
            assert_eq!(pos.more, expected.more, "{key}: pending must survive intact");
        }
        let mut resumed = Watcher::from_cursor(
            Filter::default(),
            Cursor::decode(&cursor.encode()).unwrap(),
            Vec::new,
            &readers,
        );
        for (key, state) in &watcher.seen {
            let restored = &resumed.seen[key];
            assert_eq!(restored.mark, state.mark);
            assert_eq!(restored.pending, state.pending);
            assert_eq!(
                serde_json::to_value(restored.read.as_ref().unwrap()).unwrap(),
                serde_json::to_value(state.read.as_ref().unwrap()).unwrap(),
                "{key}: the state comes back whole"
            );
        }
        // An unchanged state is not stored again; another process's readers know none of them.
        assert_eq!(resumed.to_cursor(&readers).encode(), cursor.encode());
        let elsewhere = Watcher::from_cursor(Filter::default(), cursor, Vec::new, &Readers::default());
        assert!(elsewhere.seen.values().all(|state| state.read.is_none()));
    }

    #[test]
    fn same_size_replacement_prunes_positions_even_when_new_source_cannot_be_read() {
        let _world =
            crate::test_world("watch-same_size_replacement_prunes_positions_even_when_new_source_cannot_be_read");
        fn replacement() -> Vec<SessionRef> {
            let mut r = sref(
                Harness::Claude,
                std::path::PathBuf::from("/nonexistent/cv-watch-replacement"),
            );
            r.id = "replacement".into();
            vec![r]
        }
        fn original() -> Vec<SessionRef> {
            let mut r = replacement().remove(0);
            r.id = "original".into();
            vec![r]
        }
        let cursor = Cursor {
            v: CURSOR_VERSION,
            o: [(
                "claude:original".into(),
                Pos {
                    t: (Some(0), None),
                    n: Some(12),
                    b: None,
                    more: false,
                    r: None,
                    h: None,
                },
            )]
            .into_iter()
            .collect(),
        };
        for source_poll in [false, true] {
            let mut watcher = Watcher::from_cursor(Filter::default(), cursor.clone(), replacement, &READERS);
            if source_poll {
                assert_eq!(watcher.offer_sources(|_| true, |_, _, _| None), 0);
            } else {
                assert!(watcher
                    .try_poll(
                        Poll {
                            max: 1,
                            strict: true,
                            ..Poll::default()
                        },
                        |_| panic!("missing source emitted")
                    )
                    .is_err());
            }
            let encoded = watcher.to_cursor(&READERS).encode();
            let cursor = Cursor::decode(&encoded).unwrap();
            assert!(cursor.is_empty(), "vanished position survived: {encoded}");
            let mut resumed = Watcher::from_cursor(Filter::default(), cursor, original, &READERS);
            resumed.offer_sources(
                |_| true,
                |kind, _, previous| {
                    assert_eq!(kind, EventKind::New);
                    assert_eq!(previous, 0, "reappeared identity inherited its removed position");
                    None
                },
            );
        }
    }

    /// The codex shapes that make a prefix count subtle: `has_events` decided from the head, an
    /// `event_msg` twin that dedups its `response_item`, and an assistant message held back until a
    /// trailing `token_count` attaches usage.
    const CODEX: &str = concat!(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"x","cwd":"/work"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working on it"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3}}}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
        "\n",
    );

    /// A byte baseline resolved after the file grew must equal what a full parse returned when the
    /// file was that long — at EVERY cut point, including mid-line (a record still being written).
    #[test]
    fn byte_baseline_resolves_to_the_count_a_parse_saw_then() {
        let _world = crate::test_world("watch-byte_baseline_resolves_to_the_count_a_parse_saw_then");
        let dir = std::env::temp_dir().join(format!("cv-watch-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");
        let mut cases: Vec<(Harness, Vec<u8>)> = std::fs::read_dir(fixtures)
            .unwrap()
            .map(|e| (Harness::Claude, std::fs::read(e.unwrap().path()).unwrap()))
            .collect();
        cases.push((Harness::Codex, CODEX.as_bytes().to_vec()));

        for (h, full) in cases {
            let then = dir.join("then.jsonl");
            let now = dir.join("now.jsonl");
            std::fs::write(&now, &full).unwrap();
            let adapter = harness::for_harness(h).unwrap();
            let total = adapter.parse(&sref(h, now.clone())).unwrap().messages.len();
            assert!(total > 1, "{h} fixture must have several messages");
            for cut in 0..=full.len() {
                std::fs::write(&then, &full[..cut]).unwrap();
                let Ok(parsed) = adapter.parse(&sref(h, then.clone())) else {
                    continue; // the old baseline recorded nothing for an unparseable file either
                };
                assert_eq!(
                    resolve(&sref(h, now.clone()), Mark::Bytes(cut as u64)),
                    Some(parsed.messages.len()),
                    "{h} cut at byte {cut}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The baseline count and the appended tail are streamed instead of taken from a held parse;
    /// both must be exactly the parse's.
    #[test]
    fn streamed_count_and_tail_equal_the_parse() {
        let _world = crate::test_world("watch-streamed_count_and_tail_equal_the_parse");
        let fx = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let mut checked = 0;
        for (h, rel) in [
            (Harness::Claude, "claude/rich_blocks.jsonl"),
            (Harness::Claude, "claude/sidechain_subagent.jsonl"),
            (Harness::Claude, "claude/compact_summary.jsonl"),
            (Harness::Claude, "claude/system_lines.jsonl"),
            (Harness::Gemini, "gemini/session_0_46_session_context.jsonl"),
            (Harness::Gemini, "gemini/checkpoint_legacy_array.json"),
            (Harness::OpenClaw, "openclaw/v3-full.jsonl"),
            (Harness::OpenClaw, "openclaw/acp-session.jsonl"),
            (Harness::LmStudio, "lmstudio/sample.conversation.json"),
            (Harness::LmStudio, "lmstudio/harmony.conversation.json"),
            (Harness::Cline, "cline/api_conversation_history.json"),
            (Harness::Roo, "roo/api_conversation_history.json"),
        ] {
            let r = sref(h, std::path::Path::new(fx).join(rel));
            let Ok(parsed) = harness::for_harness(h).unwrap().parse(&r) else {
                continue;
            };
            if !append_only(&r) {
                assert_eq!(baseline(&r), Some(Mark::Messages(parsed.messages.len())), "{h} {rel}");
            }
            // The streamed tail is the parse's tail, message for message, at every split.
            let n = parsed.messages.len();
            for skip in [0, 1, n / 2, n.saturating_sub(1), n, n + 3] {
                for keep in [0, 1, usize::MAX] {
                    let (total, got) = tail(&r, skip, keep).unwrap();
                    assert_eq!(total, n, "{h} {rel}");
                    let want: Vec<&Message> = parsed.messages.iter().skip(skip).take(keep).collect();
                    assert_eq!(
                        serde_json::to_value(&got).unwrap(),
                        serde_json::to_value(&want).unwrap(),
                        "{h} {rel} skip={skip} keep={keep}"
                    );
                }
            }
            checked += 1;
        }
        assert!(checked >= 8, "only {checked} fixtures parsed standalone");
    }

    /// A transcript REWRITTEN under a byte mark (truncated, or replaced by different content) is
    /// not an append, so no count can recover "what was seen then". What must hold is that the mark
    /// still resolves to something the new file can index: past the new end it resolves to the
    /// whole file (nothing re-emitted, nothing invented), and inside a replaced prefix it resolves
    /// to that prefix's own count. Neither Claude's `/compact` (it appends) nor `cv prune` (it
    /// writes a new session) rewrites in place; this pins the failure mode should anything else.
    #[test]
    fn a_byte_mark_on_a_rewritten_transcript_resolves_within_the_new_file() {
        let _world = crate::test_world("watch-a_byte_mark_on_a_rewritten_transcript_resolves_within_the_new_file");
        let dir = std::env::temp_dir().join(format!("cv-watch-rewrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        for h in [Harness::Claude, Harness::Codex] {
            let full: Vec<u8> = if h == Harness::Codex {
                CODEX.as_bytes().to_vec()
            } else {
                std::fs::read(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/claude/rich_blocks.jsonl"
                ))
                .unwrap()
            };
            let r = sref(h, path.clone());
            let adapter = harness::for_harness(h).unwrap();
            let mark = Mark::Bytes(full.len() as u64);

            // Truncated to its first half (cut on a line boundary).
            let half = full[..full.len() / 2].iter().rposition(|b| *b == b'\n').unwrap() + 1;
            std::fs::write(&path, &full[..half]).unwrap();
            let now = adapter.parse(&r).unwrap().messages.len();
            assert_eq!(
                resolve(&r, mark),
                Some(now),
                "{h}: a mark past the end is the whole file"
            );
            let (total, fresh) = tail(&r, now, usize::MAX).unwrap();
            assert_eq!((total, fresh.len()), (now, 0), "{h}: nothing re-emitted");

            // Replaced: the old file's length now cuts through different records.
            let mut other = full[half..].to_vec();
            other.extend_from_slice(&full);
            std::fs::write(&path, &other).unwrap();
            let total = adapter.parse(&r).unwrap().messages.len();
            let seen = resolve(&r, mark).unwrap();
            std::fs::write(dir.join("prefix.jsonl"), &other[..full.len()]).unwrap();
            let prefix = adapter
                .parse(&sref(h, dir.join("prefix.jsonl")))
                .unwrap()
                .messages
                .len();
            assert_eq!(seen, prefix, "{h}: a mark inside a rewrite is that prefix's count");
            assert!(seen <= total, "{h}");
            assert_eq!(tail(&r, seen, usize::MAX).unwrap().1.len(), total - seen, "{h}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn baseline_is_a_byte_mark_only_for_append_only_transcripts() {
        let _world = crate::test_world("watch-baseline_is_a_byte_mark_only_for_append_only_transcripts");
        let dir = std::env::temp_dir().join(format!("cv-watch-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("s.jsonl");
        std::fs::write(&jsonl, CODEX).unwrap();
        let len = CODEX.len() as u64;
        assert_eq!(baseline(&sref(Harness::Claude, jsonl.clone())), Some(Mark::Bytes(len)));
        assert_eq!(baseline(&sref(Harness::Codex, jsonl.clone())), Some(Mark::Bytes(len)));
        // A compressed rollout's byte prefix is not a transcript prefix: parse it instead.
        assert!(!append_only(&sref(Harness::Codex, dir.join("r.jsonl.zst"))));
        // Missing file: no mark, as before.
        assert_eq!(baseline(&sref(Harness::Gemini, dir.join("absent.json"))), None);
        assert_eq!(resolve(&sref(Harness::Claude, jsonl), Mark::Messages(3)), Some(3));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A byte mark carried in on a compressed rollout (a cursor from elsewhere, a hand-made
    /// position) is not resumed: its byte prefix is not a transcript prefix, so it resolves to
    /// nothing instead of a count of the decoded stream's first bytes.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_compressed_rollout_is_never_byte_resumed() {
        let _world = crate::test_world("watch-a_compressed_rollout_is_never_byte_resumed");
        let dir = std::env::temp_dir().join(format!("cv-watch-zst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zst = dir.join("rollout-2026-01-01T00-00-00-019e0000-0000-7000-8000-000000000001.jsonl.zst");
        std::fs::write(&zst, zstd::encode_all(CODEX.as_bytes(), 1).unwrap()).unwrap();
        let r = sref(Harness::Codex, zst);
        let full = harness::for_harness(Harness::Codex).unwrap().parse(&r).unwrap();
        assert!(!full.messages.is_empty(), "the compressed fixture parses");
        // Its reader declines a byte prefix outright: decoded offsets are not file positions.
        assert!(count_prefix(&r, CODEX.len() as u64, &mut || Ok(())).is_err());
        assert_ne!(baseline(&r), Some(Mark::Bytes(CODEX.len() as u64)));
        for len in [1, CODEX.len() as u64 / 2, CODEX.len() as u64] {
            assert_eq!(resolve(&r, Mark::Bytes(len)), None, "byte mark {len}");
            assert!(
                resolve_checked(&r, Mark::Bytes(len), &mut || Ok(())).is_err(),
                "byte mark {len}"
            );
        }
        assert_eq!(resolve(&r, Mark::Messages(2)), Some(2));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cursor is the watcher's state and nothing else: a bounded poll takes what the budget
    /// allows and leaves the rest pending; a watcher resumed from the encoded cursor drains exactly
    /// the remainder; an idle poll then yields nothing. The session source is pluggable so this
    /// runs over one temp transcript instead of the machine's fleet.
    #[test]
    fn bounded_poll_leaves_the_rest_pending_and_a_cursor_resumes_it() {
        let _world = crate::test_world("watch-bounded_poll_leaves_the_rest_pending_and_a_cursor_resumes_it");
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let dir = DIR
            .get_or_init(|| std::env::temp_dir().join(format!("cv-watch-cursor-{}", std::process::id())))
            .clone();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fn discover() -> Vec<SessionRef> {
            let path = DIR.get().unwrap().join("s.jsonl");
            let len = std::fs::metadata(&path).map(|m| m.len() as usize).unwrap_or(0);
            let mut r = sref(Harness::Claude, path);
            r.id = "cursor-test".into();
            r.message_count = Some(len); // the cheap trigger: the file grew
            vec![r]
        }
        let full = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/claude/rich_blocks.jsonl"
        ))
        .unwrap();
        let cut = full[..full.len() / 2].iter().rposition(|b| *b == b'\n').unwrap() + 1;
        std::fs::write(&path, &full[..cut]).unwrap();
        let adapter = harness::for_harness(Harness::Claude).unwrap();
        let r = discover().remove(0);
        let before = adapter.parse(&r).unwrap().messages.len();

        // Baseline: nothing emitted, one position recorded, as a byte mark (no parse).
        let mut w = Watcher::with_discovery(Filter::default(), false, discover);
        let cursor = w.to_cursor(&READERS);
        assert_eq!(cursor.len(), 1);
        let encoded = cursor.encode();
        assert!(
            encoded.contains("\"b\":"),
            "a byte baseline rides the cursor: {encoded}"
        );
        assert!(
            Cursor::decode("{\"v\":99,\"o\":{}}").is_none(),
            "another schema is not a cursor"
        );
        assert!(Cursor::decode("garbage").is_none());

        // The transcript grows; a budget of one takes one message and leaves the rest pending.
        std::fs::write(&path, &full).unwrap();
        let after = adapter.parse(&r).unwrap().messages.len();
        assert!(
            after > before + 1,
            "fixture must grow by several messages ({before} → {after})"
        );
        let mut w = Watcher::from_cursor(Filter::default(), Cursor::decode(&encoded).unwrap(), discover, &READERS);
        let mut got = Vec::new();
        let pending = w.poll(
            Poll {
                max: 3,
                ..Poll::default()
            },
            |mut ev| {
                assert_eq!(ev.kind, EventKind::Updated);
                assert!(ev.new_messages.len() >= 2);
                got.push(ev.new_messages.remove(0));
                1
            },
        );
        assert!(pending);
        assert_eq!(got.len(), 1);
        let encoded = w.to_cursor(&READERS).encode();
        assert!(encoded.contains("\"more\":true"), "{encoded}");

        // Resumed from that cursor with no budget: exactly the remainder, nothing twice.
        let mut w = Watcher::from_cursor(Filter::default(), Cursor::decode(&encoded).unwrap(), discover, &READERS);
        let resumed = events(&mut w);
        assert_eq!(resumed.len(), 1);
        got.extend(resumed.into_iter().next().unwrap().new_messages);
        assert_eq!(got.len(), after - before, "the appended tail, once");
        let want = adapter.parse(&r).unwrap().messages;
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            serde_json::to_value(&want[before..]).unwrap()
        );
        assert!(events(&mut w).is_empty(), "idle: nothing pending, trigger unchanged");
        assert!(
            !w.to_cursor(&READERS).encode().contains("more"),
            "nothing pending rides the cursor"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cursor_carries_positions_only_and_marks_are_exclusive() {
        let _world = crate::test_world("watch-cursor-positions-only");
        let mut watcher = Watcher::with_discovery(Filter::default(), true, Vec::new);
        let r = sref(
            Harness::Claude,
            std::path::PathBuf::from("/absent/cv-cursor-test.jsonl"),
        );
        for i in 0..32 {
            watcher.seen.insert(
                format!("session-{i}"),
                State {
                    trigger: (Some(i), None),
                    mark: Mark::Messages(i),
                    pending: true,
                    read: Some(ReadState::baseline(&r)),
                },
            );
        }
        let cursor = watcher.to_cursor(&READERS);
        assert_eq!(cursor.len(), 32);
        assert!(
            cursor.o.values().all(|pos| pos.r.is_none()),
            "no reader state rides the wire"
        );
        assert!(cursor.encode().len() < 32 * 128);
        let sparse = Cursor::decode(r#"{"v":2,"o":{"s":{"t":[8,null],"n":3,"more":true}}}"#).unwrap();
        assert_eq!(sparse.o["s"].n, Some(3));
        let byte_mark = Pos::from(&State {
            trigger: (None, None),
            mark: Mark::Bytes(5),
            pending: false,
            read: None,
        });
        assert_eq!(serde_json::to_string(&byte_mark).unwrap(), r#"{"t":[null,null],"b":5}"#);
        assert!(sparse.o["s"].more);
        assert!(sparse.o["s"].r.is_none());
        for unmarked in [r#"{"t":[null,null],"n":0,"b":5}"#, r#"{"t":[null,null]}"#] {
            assert!(
                Cursor::decode(&format!(r#"{{"v":2,"o":{{"x":{unmarked}}}}}"#)).is_none(),
                "{unmarked}"
            );
        }
        let v1 = r#"{"v":1,"o":{"x":{"t":[8,null],"n":3}}}"#;
        assert!(Cursor::decode(v1).is_none(), "a 0.17 cursor starts a fresh baseline");
    }

    #[test]
    fn source_observer_retries_failure_without_advancing_positions() {
        let _world = crate::test_world("watch-source_observer_retries_failure_without_advancing_positions");
        fn discover() -> Vec<SessionRef> {
            vec![sref(
                Harness::Claude,
                std::path::PathBuf::from("/absent/cv-observer-test.jsonl"),
            )]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        for _ in 0..2 {
            assert_eq!(
                watcher.offer_sources(
                    |_| true,
                    |kind, _, previous| {
                        assert_eq!(kind, EventKind::New);
                        assert_eq!(previous, 0);
                        None
                    }
                ),
                0
            );
            assert!(watcher.to_cursor(&READERS).is_empty());
        }
        assert_eq!(
            watcher.offer_sources(
                |_| true,
                |kind, r, previous| {
                    assert_eq!(kind, EventKind::New);
                    assert_eq!(previous, 0);
                    Some((4, ReadState::baseline(r)))
                }
            ),
            1
        );
        assert_eq!(
            watcher.offer_sources(
                |_| true,
                |kind, r, previous| {
                    assert_eq!(kind, EventKind::Updated);
                    assert_eq!(previous, 4);
                    Some((4, ReadState::baseline(r)))
                }
            ),
            1
        );
    }

    #[test]
    fn an_unchanged_source_is_offered_once_and_one_change_offers_it_once_more() {
        let world = crate::test_world("watch-unchanged-source-offered-once");
        let path = world.base.join("offered.jsonl");
        let line = |i: usize| {
            format!(
                "{}\n",
                serde_json::json!({"type":"user","uuid":format!("u{i}"),"message":{"role":"user","content":"hi"}})
            )
        };
        std::fs::write(&path, line(0)).unwrap();
        let discovered = path.clone();
        let mut watcher = Watcher::with_discovery(Filter::default(), true, move || {
            vec![sref(Harness::Claude, discovered.clone())]
        });
        let mut offers = 0;
        let mut offer = |watcher: &mut Watcher, kept: bool| {
            watcher.offer_sources(
                |_| kept,
                |_, r, _| {
                    offers += 1;
                    let adapter = harness::for_harness(r.harness).unwrap();
                    let pass = read::read(adapter.as_ref(), r, None, 0, 0).ok()?;
                    Some((pass.total, pass.accepted(pass.total)))
                },
            )
        };
        assert_eq!(offer(&mut watcher, true), 1);
        for _ in 0..3 {
            assert_eq!(offer(&mut watcher, true), 0, "an unchanged source is not offered again");
        }
        assert_eq!(
            offer(&mut watcher, false),
            1,
            "the consumer's own output went: offer again"
        );
        std::fs::write(&path, format!("{}{}", line(0), line(1))).unwrap();
        assert_eq!(offer(&mut watcher, true), 1, "one change, one offer");
        assert_eq!(offer(&mut watcher, true), 0);
        drop(offer);
        assert_eq!(offers, 3);
    }

    #[test]
    fn demanded_poll_surfaces_read_failure_without_a_success_position() {
        let _world = crate::test_world("watch-demanded_poll_surfaces_read_failure_without_a_success_position");
        fn discover() -> Vec<SessionRef> {
            vec![sref(
                Harness::Claude,
                std::path::PathBuf::from("/absent/cv-failed-watch-read.jsonl"),
            )]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        assert!(watcher
            .try_poll(
                Poll {
                    max: 1,
                    strict: true,
                    ..Poll::default()
                },
                |_| panic!("failed source emitted a message")
            )
            .is_err());
        assert!(watcher.to_cursor(&READERS).is_empty());
        assert!(!watcher.poll(
            Poll {
                max: 1,
                ..Poll::default()
            },
            |_| panic!("failed source emitted a message")
        ));
        assert!(watcher.to_cursor(&READERS).is_empty());
    }
}
