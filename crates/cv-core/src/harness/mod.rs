//! Per-harness adapters: discover sessions on disk, parse them into the IR, and (optionally)
//! emit the IR back into a harness's native format for cross-harness porting.

use crate::ir::{Harness, Message, Session, SessionRef};
#[cfg(test)]
use crate::stream::Flow;
use crate::stream::{Demand, MessageSink};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub mod antigravity;
pub mod claude;
/// Claude Code `Workflow`-tool runs (phase tree + script), a first-class layer over the
/// `subagents/workflows/` agent tier that [`claude`] walks.
pub mod claude_workflow;
pub mod cline;
pub mod cline_sdk;
pub mod codex;
pub mod continuedev;
#[cfg(feature = "sqlite")]
pub mod cursor;
#[cfg(feature = "sqlite")]
pub mod devin;
pub mod export;
pub mod gemini;
#[cfg(feature = "sqlite")]
pub mod goose;
pub mod grok;
#[cfg(feature = "sqlite")]
pub mod hermes;
pub mod kimi;
pub mod kimi_code;
pub mod lmstudio;
pub mod openclaw;
pub mod opencode;
/// cv's own **OpenSession** interchange documents (`docs/OPENSESSION.md`) — the format
/// `cv export --format json` writes — read back into the IR.
pub mod opensession;
pub mod qwen;
pub mod roo;
pub mod store;
pub mod wire;
#[cfg(feature = "sqlite")]
pub mod zed;

/// Parse an RFC3339 timestamp into UTC — the timestamp shape nearly every harness writes. Shared
/// here so adapters don't each carry a copy.
pub(crate) fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// Flexible timestamp from a JSON value: an RFC3339 string, or an epoch number as
/// [`ts_from_epoch`] reads it.
pub(crate) fn ts_from_value(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return parse_ts(s);
    }
    ts_from_epoch(v.as_f64()?)
}

/// An epoch number → UTC. The ms/s split is a heuristic — magnitudes above 10^12 are
/// **milliseconds** (10^12 seconds is the year 33658; 10^12 ms is Sep 2001, so any modern harness
/// timestamp disambiguates); anything else is **seconds**, and a fractional second keeps its
/// millisecond part. Zero and negative values are "unset", not 1970: every SQLite store defaults
/// an absent time column to 0, and no harness has a transcript older than itself. The one epoch
/// reader for every adapter — a SQLite column, a JSON number, a float of seconds all come here.
pub(crate) fn ts_from_epoch(n: impl EpochNumber) -> Option<DateTime<Utc>> {
    let n = n.to_f64();
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let millis = if n > 1_000_000_000_000.0 { n } else { n * 1000.0 };
    DateTime::from_timestamp_millis(millis as i64)
}

/// The numeric column/field types an epoch timestamp arrives as (see [`ts_from_epoch`]).
pub(crate) trait EpochNumber {
    fn to_f64(self) -> f64;
}
impl EpochNumber for f64 {
    fn to_f64(self) -> f64 {
        self
    }
}
impl EpochNumber for i64 {
    fn to_f64(self) -> f64 {
        self as f64
    }
}
impl EpochNumber for u64 {
    fn to_f64(self) -> f64 {
        self as f64
    }
}

/// Independent owned-value oracle for codec tests; production reads use `store::jsonl`.
#[cfg(test)]
pub(crate) fn for_each_json_line_str(text: &str, mut f: impl FnMut(Value) -> Flow) -> u64 {
    let mut skipped = 0u64;
    let mut parse = |line: &str| serde_json::from_str(line).ok();
    for line in text.lines() {
        if each_json_line_inner(line, &mut skipped, &mut parse, &mut f) == Flow::Stop {
            break;
        }
    }
    skipped
}

#[cfg(test)]
fn each_json_line_inner(
    line: &str,
    skipped: &mut u64,
    parse: &mut impl FnMut(&str) -> Option<Value>,
    f: &mut impl FnMut(Value) -> Flow,
) -> Flow {
    let line = store::json::trim_str(line);
    if line.is_empty() {
        return Flow::Continue;
    }
    let Some(v) = parse(line) else {
        *skipped += 1;
        return Flow::Continue; // tolerate the occasional corrupt line
    };
    f(v)
}

/// The namespace in `Session::extra` / `Message::extra` for cv's OWN bookkeeping — facts about the
/// *parse*, not about the harness. It sits beside the per-harness bags
/// ([`Session::harness_extra_mut`](crate::ir::Session::harness_extra_mut)) and can never collide
/// with one: `Harness::parse("cv")` is `None`.
///
/// Why a namespace and not a flat key (`docs/INTERFACE-V2.md` §4): the flat-key exception list is a
/// list that GROWS — every new cross-harness diagnostic would have to be added to it and to every
/// consumer that walks `extra` (see `harness::claude::emit::is_internal_extra_key`). A namespace does
/// not. So §4's exceptions stay exactly what they are today, both on `Message` only:
/// [`CARRIER_KEY`](crate::ir::CARRIER_KEY) (`_record`, the verbatim source record under `Demand::COMPLETE`)
/// and [`crate::offsets::OFFSET_KEY`] (`cv_byte_offset`, written per message on the lazy-offset hot
/// path, where a nested map per message would be pure allocation). `Session::extra` has **no** flat
/// exception: every top-level key there is a namespace object — a harness name or `"cv"`.
///
/// The namespace itself is [`crate::ir::CV_NAMESPACE`], reached with `Session::cv_extra_mut`.
///
/// Record `skipped` unreadable transcript lines under `extra["cv"]["skipped_lines"]` (only when
/// non-zero, so clean sessions stay byte-identical). The count is a *lower bound* when a pass
/// stopped early. It is a parse diagnostic shared by every adapter, so it lives in cv's own
/// namespace ([`crate::ir::CV_NAMESPACE`]) rather than in any harness bag.
pub(crate) fn note_skipped_lines(s: &mut Session, skipped: u64) {
    if skipped > 0 {
        s.cv_extra_mut()
            .insert("skipped_lines".into(), Value::Number(skipped.into()));
    }
}

/// An aborted in-memory read keeps its diagnostic on the partial session, never an unmarked empty.
pub(crate) fn note_read_error(session: &mut Session, result: anyhow::Result<()>) {
    if let Err(error) = result {
        session
            .cv_extra_mut()
            .insert("read_error".into(), Value::String(format!("{error:#}")));
    }
}

/// The first channel of the skip policy: the records one read could not parse, COUNTED, never
/// dropped silently. An adapter keeps one per read and writes it once with [`Unparsed::note`]
/// (`extra["cv"]["skipped_lines"]`, nothing when zero, so a clean read is unchanged).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unparsed(pub(crate) u64);

impl Unparsed {
    /// One record that decoded to nothing. (Its callers today are SQLite stores; the line
    /// readers carry their count in resumable state and hand it to [`Unparsed::note`].)
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn count(&mut self) {
        self.0 += 1;
    }

    /// Write the count on `s` ([`note_skipped_lines`]).
    pub(crate) fn note(self, s: &mut Session) {
        note_skipped_lines(s, self.0);
    }
}

/// A source [`Adapter::discover`] found but could not open (a locked or corrupt database, an
/// unreadable directory, a file that fails to decode at open): its sessions are absent from the
/// listing, and this says why. The skip policy is one rule in three channels: a record that
/// cannot parse is counted ([`note_skipped_lines`]); a source that cannot open is reported here,
/// persisted by the catalog and shown by `cv doctor`; an accelerator that cannot open degrades
/// once (one stderr line per process). A source that is not there (`NotFound`) is the only honest
/// empty and is never a `Skipped`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Skipped {
    pub path: PathBuf,
    /// The error's one-line rendering (its whole cause chain).
    pub why: String,
}

impl Skipped {
    /// `error` at `path` as a skip, or `None` when the error is a `NotFound` (nothing there).
    pub fn of(path: impl Into<PathBuf>, error: &anyhow::Error) -> Option<Skipped> {
        (!is_not_found(error)).then(|| Skipped {
            path: path.into(),
            why: format!("{error:#}"),
        })
    }

    /// One source's scan as an item a [`Discovered`] collects: its ref, nothing (the source holds
    /// no session), or a skip (any error but `NotFound`).
    pub fn listed(path: &std::path::Path, scanned: Result<Option<SessionRef>>) -> Option<Result<SessionRef, Skipped>> {
        match scanned {
            Ok(r) => r.map(Ok),
            Err(e) => Skipped::of(path, &e).map(Err),
        }
    }

    /// One source's scan of several sessions as items a [`Discovered`] collects.
    pub fn listed_many(
        path: &std::path::Path,
        scanned: Result<Vec<SessionRef>>,
    ) -> Vec<std::result::Result<SessionRef, Skipped>> {
        match scanned {
            Ok(refs) => refs.into_iter().map(Ok).collect(),
            Err(e) => Skipped::of(path, &e).map(Err).into_iter().collect(),
        }
    }

    /// A directory walk's error as a skip: an unreadable directory hides every session under it.
    /// `None` for a `NotFound` (an entry removed mid-walk).
    pub fn walk(error: &walkdir::Error) -> Option<Skipped> {
        if error
            .io_error()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return None;
        }
        Some(Skipped {
            path: error.path()?.to_path_buf(),
            why: error.to_string(),
        })
    }
}

/// `error` is a `NotFound` somewhere in its chain: the source is not there, the only honest empty.
pub(crate) fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// What [`Adapter::discover`] found: the sessions it could list and the sources it could not open.
/// Reads as its `refs` (`Deref` to the `Vec`, `IntoIterator` over them); `skipped` rides beside.
#[derive(Debug, Default)]
pub struct Discovered {
    pub refs: Vec<SessionRef>,
    pub skipped: Vec<Skipped>,
}

impl Discovered {
    /// `result` read at `path`: its value, or `None` with the error recorded as a skip (dropped
    /// when it is a `NotFound`).
    pub fn keep<T>(&mut self, path: &std::path::Path, result: Result<T>) -> Option<T> {
        result.map_err(|e| self.skipped.extend(Skipped::of(path, &e))).ok()
    }

    /// One source's outcome ([`Skipped::listed`]): a ref or a skip.
    pub fn push(&mut self, found: std::result::Result<SessionRef, Skipped>) {
        match found {
            Ok(r) => self.refs.push(r),
            Err(s) => self.skipped.push(s),
        }
    }
}

impl From<Vec<SessionRef>> for Discovered {
    fn from(refs: Vec<SessionRef>) -> Self {
        Discovered {
            refs,
            skipped: Vec::new(),
        }
    }
}

impl FromIterator<SessionRef> for Discovered {
    fn from_iter<I: IntoIterator<Item = SessionRef>>(iter: I) -> Self {
        Vec::from_iter(iter).into()
    }
}

/// Per-source outcomes of a parallel scan, a ref or a skip each.
impl FromIterator<Result<SessionRef, Skipped>> for Discovered {
    fn from_iter<I: IntoIterator<Item = Result<SessionRef, Skipped>>>(iter: I) -> Self {
        let mut out = Discovered::default();
        iter.into_iter().for_each(|found| out.push(found));
        out
    }
}

impl std::ops::Deref for Discovered {
    type Target = Vec<SessionRef>;
    fn deref(&self) -> &Vec<SessionRef> {
        &self.refs
    }
}

impl std::ops::DerefMut for Discovered {
    fn deref_mut(&mut self) -> &mut Vec<SessionRef> {
        &mut self.refs
    }
}

impl IntoIterator for Discovered {
    type Item = SessionRef;
    type IntoIter = std::vec::IntoIter<SessionRef>;
    fn into_iter(self) -> Self::IntoIter {
        self.refs.into_iter()
    }
}

impl<'a> IntoIterator for &'a Discovered {
    type Item = &'a SessionRef;
    type IntoIter = std::slice::Iter<'a, SessionRef>;
    fn into_iter(self) -> Self::IntoIter {
        self.refs.iter()
    }
}

/// One word for a POSIX shell command line: bare when every byte is inert in a word (letters,
/// digits, `/ . _ - + : @`), single-quoted otherwise. `~` is not inert: a bare leading `~` expands.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+' | b':' | b'@'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The resume hint for a harness that resumes relative to its working directory: one runnable
/// command line, `cd <dir> && <command>` when the directory is known.
pub fn resume_in(cwd: Option<&Path>, command: String) -> String {
    match cwd {
        Some(dir) => format!("cd {} && {command}", shell_quote(&dir.display().to_string())),
        None => command,
    }
}

/// Result of emitting an IR session into a harness's native on-disk format.
#[derive(Debug, Clone)]
pub struct EmitResult {
    /// Primary file (or directory) written.
    pub path: PathBuf,
    /// The new native session id in the target harness.
    pub new_id: String,
    /// A human hint for how to resume it, if known (e.g. `claude --resume <id>`).
    pub resume_hint: Option<String>,
}

/// The keys of a native record that carry its identity, as a derived copy rewrites them
/// ([`Adapter::identity`]): the session id is restamped, a standalone copy of a sub-agent
/// transcript drops the agent id and clears the sidechain flag, and a re-rooted record's parent
/// link is nulled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// Top-level keys whose string value is the session id.
    pub session_keys: &'static [&'static str],
    pub agent_key: &'static str,
    pub sidechain_key: &'static str,
    pub parent_key: &'static str,
}

/// The static facts an adapter declares about its harness, in one value: what a shared module asks
/// instead of naming a harness (`docs/INTERFACE-V2.md` §4), each "no opinion" in
/// [`Capabilities::NONE`]. An adapter that declares any writes the whole literal (no `..NONE`), so
/// a new fact is a compile error at every adapter that has an opinion, not a silent default.
/// `#[non_exhaustive]`: a crate outside cv-core starts from `NONE` and sets fields.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Capabilities {
    /// The wordings of an error tool result that mean the call was denied (an approval refused, a
    /// rule matched).
    pub denials: &'static [crate::affordance::Denial],
    /// What its edit hook answers when another live writer edited the file; `None`: no edit guard.
    pub edit_guard: Option<crate::hooks::EditGuard>,
    /// How its hook rows in cv's lane ledger name the sub-agents whose `stop` counts for a parent;
    /// `None`: its hooks write no lane rows.
    pub lane_stops: Option<crate::lanes::StopScope>,
    /// The deferred-tool wrapper its reader leaves in calls, for
    /// [`crate::affordance::AffordanceSink`] to unwrap; `None`: it leaves none.
    pub deferred_wrapper: Option<crate::affordance::DeferredWrapper>,
    /// Whether a [`Demand::RICH`] stream with spans keeps the same full-extra metadata as an
    /// inline one, so a tail can leave large payloads in the source; `false`: read inline.
    pub rich_spans: bool,
}

impl Capabilities {
    /// No opinion on any fact.
    pub const NONE: Capabilities = Capabilities {
        denials: &[],
        edit_guard: None,
        lane_stops: None,
        deferred_wrapper: None,
        rich_spans: false,
    };
}

/// A harness adapter: where a harness keeps its sessions ([`roots`](Adapter::roots)), how to list
/// them cheaply ([`discover`](Adapter::discover)), and how to read one as a message stream
/// ([`stream`](Adapter::stream)). Implementors live in submodules and are registered in [`all`].
/// A whole-session parse is not a fourth method: it is [`stream::collect`](crate::stream::collect)
/// over `stream`, spelled `adapter.parse(&r)` on a `dyn Adapter`, so the streaming and the
/// whole-session readers cannot diverge.
/// Incremental reads are optional: checkpoint/resume methods default to declining, while
/// source and dependency methods describe the inputs consumers must validate before reuse.
///
/// `Send + Sync` so [`crate::discover_all`] can fan discovery across adapters in parallel. Every
/// adapter holds only path data (connections are opened per call), so this bound is always satisfied.
pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;

    /// Every on-disk root this adapter reads sessions from (resolved against `$HOME`, existing
    /// ones only), the primary one first. Always **directories** — for a SQLite-backed store, the
    /// dir holding the database — so an emitter given the first as `out_dir` writes beside what the
    /// reader reads, and the catalog's freshness probe can watch each one (the database files are
    /// its per-session file-watches), re-discovering the harness when a root appears that it holds
    /// no watch for. Empty when the harness is not installed.
    fn roots(&self) -> Vec<PathBuf>;

    /// Cheaply enumerate sessions without fully parsing them, and name the sources found but
    /// unreadable ([`Discovered::skipped`]). `Err` only when the whole store cannot be listed.
    fn discover(&self) -> Result<Discovered>;

    /// Stream a session's messages into `sink` one at a time, dropping each before the next, so
    /// peak memory is O(largest message) rather than O(transcript). Returns the session's metadata
    /// (id/cwd/title/model/git/timestamps) with an **empty** `messages` vec — the messages went to
    /// the sink. `opts` controls how much of each message is materialized (see [`Demand`]).
    /// See [`claude`](crate::harness::claude) for the reference; a store that can only be read
    /// whole hands its parsed session to [`crate::stream::replay`].
    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session>;

    /// Checked stream boundary for all implementations, including third-party adapters which
    /// implement only [`Self::stream`]. Consumer failures are errors, never successful stops.
    fn try_stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| self.stream(r, opts, sink))
    }

    /// Read `r` from `from`: whole, from a checkpoint an earlier read of the same file took (then
    /// shorter), or as the first bytes of an append-only transcript ([`Start`](crate::stream::Start)).
    /// A continued read delivers to `sink` exactly what the whole read would after that point and
    /// returns the session the whole read returns; the caller has checked that the bytes before
    /// the checkpoint are the ones it was taken on. Consumer failures are errors, as in
    /// [`Self::try_stream`]. `Ok(None)`: this adapter or this file cannot start there (no state,
    /// another layout, another codec version); nothing reached `sink`. A JSONL transcript is read
    /// by the one driver ([`store::jsonl::drive`]), whose checkpoints are the only ones a read
    /// offers. Default: a whole read streams, any other start declines.
    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: crate::stream::Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        match from {
            crate::stream::Start::Whole => self.try_stream(r, opts, sink).map(Some),
            _ => Ok(None),
        }
    }

    /// The affordances that live only in a side store beside the transcript (a todo file, a
    /// table, a state row): the session's current plan, goals, schedules. Rows have no message
    /// index (`docs/design/AFFORDANCES.md`). Default: the store keeps none. Missing optional side
    /// files are empty; unreadable files or rows are errors.
    fn store_affordances(&self, _r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        Ok(Vec::new())
    }

    /// The affordances the session's own harness bag records once per session (a permission
    /// mode, a todo state, a ruleset), in the order [`crate::affordance::session_rows`] lists them
    /// after the shared mode row. Default: the bag records none.
    fn session_affordances(&self, _s: &Session) -> Vec<crate::affordance::Affordance> {
        Vec::new()
    }

    /// The launch description of the sub-agent `s` is, as this harness's bag in `s` records it
    /// (the store keeps it beside the transcript). Default: the harness records none.
    fn agent_description<'s>(&self, _s: &'s Session) -> Option<&'s str> {
        None
    }

    /// The static facts this adapter declares about its harness ([`Capabilities`]). Default:
    /// [`Capabilities::NONE`], no opinion on any.
    fn capabilities(&self) -> Capabilities {
        Capabilities::NONE
    }

    /// The affordance `m` is as one of this harness's own records that no `MessageKind` names (a
    /// skill load, a completion notice, a refused call), its leading text read through `first`.
    /// Default: the harness writes none.
    fn record_affordance(
        &self,
        _m: &Message,
        _first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        Ok(None)
    }

    /// The affordances `m`'s harness bag records outside its blocks (a patch or snapshot part
    /// riding the turn), in order. Default: the bag records none.
    fn message_affordances(&self, _m: &Message) -> Vec<crate::affordance::BagHit> {
        Vec::new()
    }

    /// The instant before which `r`'s turns were copied from another session rather than taken in
    /// this one: a Claude `--fork-session` copy keeps its source's records and their timestamps.
    /// A consumer that attributes each turn once (the search index) skips those; views keep them.
    /// Default: nothing copied (Codex lean passes already drop a rollout's inherited prefix).
    /// Paths the answer depends on are reported to `sink`.
    fn copied_before(&self, _r: &SessionRef, _sink: &mut dyn MessageSink) -> Option<DateTime<Utc>> {
        None
    }

    /// File whose immutable-prefix byte positions this adapter checkpoints. Directory-backed
    /// stores override this with their transcript file; side stores remain freshness dependencies.
    /// Consumers must use this same file for size, identity, sampled guards, and primary read state.
    /// Source selection depends only on the ref's harness, id and path: discovery may supply a
    /// projected ref without title, cwd, timestamps or counts when bracketing primary reads.
    fn resume_source(&self, r: &SessionRef) -> PathBuf {
        r.path.clone()
    }

    /// The session-local inputs a read of `r` consults besides its primary: side files (recorded
    /// even when absent) and directories (each enumerated one level). Consumers stat them before
    /// and after a read; a change rereads. `probe` answers stat-only questions once per operation.
    /// `None` when they cannot be determined, which no consumer then certifies. Default: none.
    fn dependencies(
        &self,
        _r: &SessionRef,
        _probe: &mut crate::source::Probe,
        _deps: &mut crate::source::Dependencies,
    ) -> Option<()> {
        Some(())
    }

    // The facts below are what a shared module asks instead of naming a harness
    // (`docs/INTERFACE-V2.md` §4). Each default is "no opinion"; none depends on the adapter's
    // roots, so a shared module asks them of [`rootless`] without detecting a store.

    /// The hook call `payload` (the JSON the harness pipes to `cv hook <name>`) as one
    /// [`crate::hooks::Event`], decoded by the adapter that owns the payload's shape. Default:
    /// `None`, the harness has no verified hook contract.
    fn hook_event(&self, _payload: &Value) -> Result<Option<crate::hooks::Event>> {
        Ok(None)
    }

    /// The lineage a live transcript records in its header, read without parsing its history
    /// (a hook's budget). Default: the header records none.
    fn hook_lineage(&self, _transcript: &std::path::Path) -> Result<Option<crate::ir::Lineage>> {
        Ok(None)
    }

    /// The transcript of `agent_session`, a sub-agent of `parent` (inside `workflow`'s run when
    /// given), as this store lays it out beside its parent. Default: the store keeps none there.
    fn agent_transcript(&self, _parent: &SessionRef, _agent_session: &str, _workflow: Option<&str>) -> Option<PathBuf> {
        None
    }

    /// Where the sub-agent transcript at `transcript` ended (last turn, last stop, a fatal API
    /// notice), when the store records it there. Default: it records none.
    fn subagent_end(&self, _transcript: &std::path::Path) -> Result<Option<crate::lanes::SubagentEnd>> {
        Ok(None)
    }

    /// The last status notice `parent`'s own records give each sub-agent, by agent id. Default:
    /// the parent records none.
    fn task_notices(
        &self,
        _parent: &SessionRef,
    ) -> Result<std::collections::HashMap<String, crate::lanes::TaskNotice>> {
        Ok(std::collections::HashMap::new())
    }

    /// The newest compaction boundary in the last `tail` bytes of `transcript`:
    /// `(trigger, tokens before)`. Default: the store records none a tail read can find.
    fn last_compact_boundary(
        &self,
        _transcript: &std::path::Path,
        _tail: u64,
    ) -> Result<Option<(Option<String>, Option<u64>)>> {
        Ok(None)
    }

    /// The native keys a derived copy of a session (`cv prune`, `cv rewind`, `cv adopt`) rewrites,
    /// for the shared writer in `derive`. Default: none; the harness has no derived copies.
    fn identity(&self) -> Option<Identity> {
        None
    }

    /// The [`Rows`](crate::source::Rows) certificate of every session the shared SQLite store
    /// at `store` holds, by session id, from one consistent read that copies no message body:
    /// what each session's own read consults (its rows' count and highest rowid, the flags and
    /// small rows beside them). A consumer's record of a session whose certificate is unchanged
    /// stays fresh across writes to the store's other sessions. `None`: the adapter certifies
    /// none, and any write to the store rereads every session in it. Default: none.
    fn rows(&self, _store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        None
    }

    /// The store shared by many sessions whose [`Self::rows`] certify `r`: its primary when that
    /// is shared (a SQLite database, a gemini-cli `logs.json`), or a side file every session of a
    /// group reads its own slice of (a Kimi Code sub-agent's `state.json`). `None`: `r` is
    /// certified by its files alone. Default: `r`'s path when it is a SQLite store
    /// ([`Self::sqlite_source`]).
    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        (self.sqlite_source(r) == Some(true)).then(|| r.path.clone())
    }

    /// The files a [`Self::rows_store`] is made of, whose identities (with their SQLite sidecars)
    /// are its state: a write to any moves every certificate's cache. Default: the store itself.
    fn rows_files(&self, store: &Path) -> Vec<PathBuf> {
        vec![store.to_path_buf()]
    }

    /// The key of `r`'s certificate in [`Self::rows`]: its id, unless its read is wholly another
    /// session's rows (a sub-agent chain stored in its parent session's forest). Default: its id.
    fn rows_key<'a>(&self, r: &'a SessionRef) -> &'a str {
        &r.id
    }

    /// Whether `r`'s source is a SQLite database, whose `-wal`/`-journal` sidecars then guard it;
    /// `None` when that cannot be determined (no consumer certifies it). Default: by its extension.
    fn sqlite_source(&self, r: &SessionRef) -> Option<bool> {
        Some(crate::source::has_sqlite_extension(&r.path))
    }

    /// A metadata file beside `r`'s store that, once written, can supply a cwd or title the store
    /// lacks: watched by the catalog and recorded as a read dependency even while absent.
    /// Default: none.
    fn metadata_file(&self, _r: &SessionRef) -> Option<PathBuf> {
        None
    }

    /// The identity that makes two of this harness's messages *the same API call*, when its
    /// store repeats one (so usage is counted once). Default: every message is its own call.
    fn usage_key(&self, _m: &Message) -> Option<String> {
        None
    }

    /// How a change to `r` shows on disk, so the catalog's freshness probe knows what to watch.
    /// Default: as a directory change ([`WatchPolicy::Tree`]).
    fn watch_policy(&self, _r: &SessionRef) -> WatchPolicy {
        WatchPolicy::Tree
    }

    /// Whether `r`'s transcript only ever grows by appending lines, so its first `n` bytes pin a
    /// past state, read back by [`Self::transcript`] from
    /// [`Start::Prefix`](crate::stream::Start::Prefix). A follower records a byte mark for such a
    /// session and resolves it later. Default: not append-only.
    fn append_only(&self, _r: &SessionRef) -> bool {
        false
    }

    /// The file edit `m`'s harness record states outside its tool blocks (Codex's `FileChange`
    /// item), so the event extractor counts it without reading the harness bag itself. Default:
    /// none.
    fn native_edit<'m>(&self, _m: &'m Message) -> Option<crate::events::NativeEdit<'m>> {
        None
    }

    /// The harness's own kind for an injected-context turn it recorded (Claude's
    /// `attachment_type`, e.g. `hook_success`). Default: the store records none.
    fn attachment_kind<'m>(&self, _m: &'m Message) -> Option<&'m str> {
        None
    }

    /// The unwrapped sender body of a message delivered mid-run (e.g. a coordinator turn or
    /// queued command wrapped in harness framing). Default: none.
    fn delivered_body<'a>(&self, _m: &Message, _text: &'a str) -> Option<&'a str> {
        None
    }

    /// Whether thinking recorded under `model` came from the provider this harness replays
    /// natively (its signatures are valid as they are). [`crate::emit::ThinkingMode::replay_default`]
    /// replays natively only when every recorded model is one some adapter claims. Default: no.
    fn native_thinking(&self, _model: &str) -> bool {
        false
    }

    /// The sub-agent forest `r` spawned, newest first, keeping the members `select(id, workflow)`
    /// admits (and only `workflow`'s, when given) before their bodies or optional metadata are
    /// read. `demand` bounds what each member carries. Default: the store records no forest.
    fn forest(
        &self,
        _r: &SessionRef,
        _demand: crate::SubagentDemand,
        _workflow: Option<&str>,
        _select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        Ok(Vec::new())
    }

    /// The `Workflow`-tool runs this store records beside a session, when it records any.
    /// Default: none.
    fn workflows(&self) -> Option<&'static dyn Workflows> {
        None
    }
}

/// The members of a forest read whole that `workflow` and `select` admit, in order: the
/// [`Adapter::forest`] of a store that cannot select before reading.
pub(crate) fn select_forest(
    forest: Vec<crate::AgentRef>,
    workflow: Option<&str>,
    select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
) -> Vec<crate::AgentRef> {
    forest
        .into_iter()
        .filter(|s| {
            workflow.is_none_or(|run| s.workflow.as_deref() == Some(run))
                && select(&s.session.id, s.workflow.as_deref())
        })
        .collect()
}

/// The [`Adapter::forest`] of a store whose sub-agents are plain session refs with no sidecar
/// facts (no agent type, launching call or journaled outcome): one member per ref, newest first.
pub(crate) fn ref_forest(
    refs: Vec<SessionRef>,
    workflow: Option<&str>,
    select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
) -> Vec<crate::AgentRef> {
    let mut forest: Vec<crate::AgentRef> = refs
        .into_iter()
        .map(|session| crate::AgentRef {
            session,
            agent_type: None,
            description: None,
            tool_use_id: None,
            workflow: None,
            result_status: None,
            result_summary: None,
        })
        .collect();
    forest.sort_by_key(|s| std::cmp::Reverse(s.session.created_at));
    select_forest(forest, workflow, select)
}

/// How a session's change shows on disk ([`Adapter::watch_policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchPolicy {
    /// A new or rewritten file changes its directory: watching the tree is enough.
    Tree,
    /// The file is a database written in place, which changes no directory: watch the file and
    /// its write-ahead log.
    Database,
}

/// The `Workflow`-tool runs a store records beside its sessions ([`Adapter::workflows`]).
pub trait Workflows: Sync {
    /// Every run `r` launched, newest first.
    fn runs(&self, r: &SessionRef) -> Vec<crate::Workflow>;
    /// The runs of `r` named `name` (exact or prefix), reading only matching state files.
    fn named(&self, r: &SessionRef, name: &str) -> Vec<crate::Workflow>;
    /// One run by its id or unambiguous name.
    fn run(&self, r: &SessionRef, key: &str) -> Option<crate::Workflow>;
    /// The id `key` (an id or name) resolves to, without reading undemanded bodies.
    fn run_id(&self, r: &SessionRef, key: &str) -> Option<String>;
    /// Launches in `r`'s transcript with no recorded run.
    fn ghosts(&self, r: &SessionRef) -> Vec<crate::WorkflowLaunch>;
    /// Whether `r` recorded any run at all (a cheap gate before [`Self::ghosts`]).
    fn has_records(&self, r: &SessionRef) -> bool;
    /// Whether one of `r`'s sub-agent transcripts (either tier) has a file name starting `stem`.
    fn has_agent(&self, r: &SessionRef, stem: &str) -> bool;
}

/// `parse` on a concrete adapter (`Claude::detect().parse(&r)`): the same
/// [`stream::collect`](crate::stream::collect), by a blanket impl no adapter can override.
pub trait Parse {
    fn parse(&self, r: &SessionRef) -> Result<Session>;
}

impl<T: Adapter> Parse for T {
    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }
}

impl dyn Adapter + '_ {
    /// Fully parse one discovered session into the IR: [`stream::collect`](crate::stream::collect).
    pub fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    /// The primary root (the first of [`roots`](Adapter::roots)), where an emitter writes.
    pub fn root(&self) -> Option<PathBuf> {
        self.roots().into_iter().next()
    }
}
// NOTE: emitting (being a *conversion target*) is deliberately NOT part of this trait. The single
// emit registry is `crate::emit::emitter_for` — an adapter becomes a target by exposing a
// `pub fn emit(&Session, &Path, &EmitOptions) -> Result<EmitResult>` and registering it there.
// (A previous `Adapter::emit`/`can_emit` pair had zero call sites and drifted out of sync with the
// real dispatcher, with `can_emit` returning `false` for supported targets.)

/// Registration order: [`all`]'s order, which listings and discovery observe.
const ORDER: [Harness; Harness::COUNT] = [
    Harness::Claude,
    Harness::Codex,
    Harness::Grok,
    Harness::OpenCode,
    Harness::Gemini,
    Harness::Antigravity,
    Harness::OpenClaw,
    Harness::Kimi,
    Harness::KimiCode,
    Harness::Qwen,
    Harness::LmStudio,
    Harness::Cline,
    Harness::Roo,
    Harness::Continue,
    Harness::ChatGptExport,
    Harness::ClaudeExport,
    Harness::OpenSession,
    Harness::Hermes,
    Harness::Cursor,
    Harness::Goose,
    Harness::Zed,
    Harness::Devin,
];

/// `h`'s place in registration order: where its rows sit in the catalog's listing.
#[cfg(feature = "sqlite")]
pub(crate) fn rank(h: Harness) -> usize {
    ORDER.iter().position(|o| *o == h).expect("ORDER names every harness")
}

/// All registered adapters.
pub fn all() -> Vec<Box<dyn Adapter>> {
    ORDER.into_iter().filter_map(for_harness).collect()
}

/// Whether `h`'s adapter declares derived copies ([`Adapter::identity`]): what `cv prune`,
/// `cv rewind`, `cv adopt` and a distilled agent write.
pub fn has_derived_copies(h: Harness) -> bool {
    rootless(h).is_some_and(|a| a.identity().is_some())
}

/// The harness a derived session is written into when nothing names one (`cv distill --session |
/// --agent-of`, `cv adopt`, a bare transcript path): the one adapter that declares derived copies.
/// A second such adapter makes this a choice for a flag; `one_derived_host` fails until then.
pub fn derived_host() -> Harness {
    Harness::ALL
        .iter()
        .copied()
        .find(|h| has_derived_copies(*h))
        .expect("the Claude adapter, always built, declares derived copies")
}

/// The adapter for `h` with no roots, `None` when it is not registered in this build. It
/// discovers nothing; a shared module asks it the facts that do not depend on where the store
/// lives (the [`Adapter`] methods after [`Adapter::resume`]) without detecting the store.
pub fn rootless(h: Harness) -> Option<&'static dyn Adapter> {
    Some(match h {
        Harness::Claude => &claude::ROOTLESS,
        Harness::Codex => &codex::ROOTLESS,
        Harness::Grok => &grok::ROOTLESS,
        Harness::OpenCode => &opencode::ROOTLESS,
        Harness::Gemini => &gemini::ROOTLESS,
        Harness::Antigravity => &antigravity::ROOTLESS,
        Harness::OpenClaw => &openclaw::ROOTLESS,
        Harness::Kimi => &kimi::ROOTLESS,
        Harness::KimiCode => &kimi_code::ROOTLESS,
        Harness::Qwen => &qwen::ROOTLESS,
        Harness::LmStudio => &lmstudio::ROOTLESS,
        Harness::Cline => &cline::ROOTLESS,
        Harness::Roo => &roo::ROOTLESS,
        Harness::Continue => &continuedev::ROOTLESS,
        Harness::ChatGptExport => &export::CHATGPT_EXPORT,
        Harness::ClaudeExport => &export::CLAUDE_EXPORT,
        Harness::OpenSession => &opensession::OpenSession,
        #[cfg(feature = "sqlite")]
        Harness::Hermes => &hermes::ROOTLESS,
        #[cfg(feature = "sqlite")]
        Harness::Cursor => &cursor::ROOTLESS,
        #[cfg(feature = "sqlite")]
        Harness::Goose => &goose::ROOTLESS,
        #[cfg(feature = "sqlite")]
        Harness::Zed => &zed::ROOTLESS,
        #[cfg(feature = "sqlite")]
        Harness::Devin => &devin::ROOTLESS,
        #[cfg(not(feature = "sqlite"))]
        Harness::Hermes | Harness::Cursor | Harness::Goose | Harness::Zed | Harness::Devin => return None,
    })
}

/// The adapters of the harness bags `m` carries (`extra[<harness>]`), in key order: the ones a
/// per-message fact is asked of.
fn bag_owners(m: &Message) -> impl Iterator<Item = &'static dyn Adapter> + '_ {
    m.extra
        .keys()
        .filter_map(|key| Harness::ALL.iter().find(|h| h.as_str() == key))
        .filter_map(|h| rootless(*h))
}

/// The harness's own kind for an injected-context turn ([`Adapter::attachment_kind`]), asked of
/// each bag's adapter, first answer wins.
pub fn attachment_kind(m: &Message) -> Option<&str> {
    bag_owners(m).find_map(|a| a.attachment_kind(m))
}

/// The unwrapped sender body of a message delivered mid-run ([`Adapter::delivered_body`]), asked
/// of each bag's adapter and the session's adapter.
pub fn delivered_body<'a>(h: Harness, m: &Message, text: &'a str) -> Option<&'a str> {
    bag_owners(m).chain(rootless(h)).find_map(|a| a.delivered_body(m, text))
}

/// The file edit `m`'s record states outside its tool blocks ([`Adapter::native_edit`]), asked
/// of each bag's adapter, first answer wins.
pub fn native_edit(m: &Message) -> Option<crate::events::NativeEdit<'_>> {
    bag_owners(m).find_map(|a| a.native_edit(m))
}

/// The adapter for a specific harness, if registered. Detects only that adapter: detection stats
/// roots and reads config, so a per-session lookup must not detect the other twenty.
pub fn for_harness(h: Harness) -> Option<Box<dyn Adapter>> {
    Some(match h {
        Harness::Claude => Box::new(claude::Claude::detect()),
        Harness::Codex => Box::new(codex::Codex::detect()),
        Harness::Grok => Box::new(grok::Grok::detect()),
        Harness::OpenCode => Box::new(opencode::OpenCode::detect()),
        Harness::Gemini => Box::new(gemini::Gemini::detect()),
        Harness::Antigravity => Box::new(antigravity::Antigravity::detect()),
        Harness::OpenClaw => Box::new(openclaw::OpenClaw::detect()),
        Harness::Kimi => Box::new(kimi::Kimi::detect()),
        Harness::KimiCode => Box::new(kimi_code::KimiCode::detect()),
        Harness::Qwen => Box::new(qwen::Qwen::detect()),
        Harness::LmStudio => Box::new(lmstudio::LmStudio::detect()),
        Harness::Cline => Box::new(cline::Cline::detect()),
        Harness::Roo => Box::new(roo::Roo::detect()),
        Harness::Continue => Box::new(continuedev::Continue::detect()),
        Harness::ChatGptExport => Box::new(export::CHATGPT_EXPORT.clone()),
        Harness::ClaudeExport => Box::new(export::CLAUDE_EXPORT.clone()),
        Harness::OpenSession => Box::new(opensession::OpenSession),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => Box::new(hermes::Hermes::detect()),
        #[cfg(feature = "sqlite")]
        Harness::Cursor => Box::new(cursor::Cursor::detect()),
        #[cfg(feature = "sqlite")]
        Harness::Goose => Box::new(goose::Goose::detect()),
        #[cfg(feature = "sqlite")]
        Harness::Zed => Box::new(zed::Zed::detect()),
        #[cfg(feature = "sqlite")]
        Harness::Devin => Box::new(devin::Devin::detect()),
        #[cfg(not(feature = "sqlite"))]
        Harness::Hermes | Harness::Cursor | Harness::Goose | Harness::Zed | Harness::Devin => return None,
    })
}

/// The adapter for `h` reading one explicit root and nothing it would detect (no `$HOME`, no
/// configured extra roots): what a probe or a fixture reads. `root` is the directory the
/// adapter's own explicit constructor takes: Claude's config dir, the native home of Codex, Gemini,
/// Grok, Kimi, Kimi Code, Qwen and Hermes, Cline's and Roo's user home, OpenClaw's state dir,
/// OpenCode's data dir, Continue's global dir, LM Studio's conversations dir, Goose's sessions dir,
/// the dir holding Devin's or Zed's database, a Cursor `User` dir or CLI home, or the dir an
/// export or OpenSession document sits in. `None` when `h` is not built in.
pub fn for_harness_with_root(h: Harness, root: &Path) -> Option<Box<dyn Adapter>> {
    Some(match h {
        Harness::Claude => Box::new(claude::Claude::with_config_dir(root)),
        Harness::Codex => Box::new(codex::Codex::with_home(root)),
        Harness::Grok => Box::new(grok::Grok::with_home(root)),
        Harness::Gemini => Box::new(gemini::Gemini::with_home(root)),
        Harness::Antigravity => Box::new(antigravity::Antigravity::with_home(root)),
        Harness::OpenClaw => Box::new(openclaw::OpenClaw::with_state_dir(root)),
        Harness::Kimi => Box::new(kimi::Kimi::with_home(root)),
        Harness::KimiCode => Box::new(kimi_code::KimiCode::with_home(root)),
        Harness::Qwen => Box::new(qwen::Qwen::with_home(root)),
        Harness::LmStudio => Box::new(lmstudio::LmStudio::with_root(root)),
        Harness::Cline => Box::new(cline::Cline::with_home(root)),
        Harness::Roo => Box::new(roo::Roo::with_home(root)),
        Harness::Continue => Box::new(continuedev::Continue::with_root(root)),
        Harness::OpenCode => Box::new(opencode::OpenCode::with_data_dir(root)),
        Harness::ChatGptExport => Box::new(export::ExportAdapter::with_roots(
            export::Kind::ChatGpt,
            vec![root.to_path_buf()],
        )),
        Harness::ClaudeExport => Box::new(export::ExportAdapter::with_roots(
            export::Kind::Claude,
            vec![root.to_path_buf()],
        )),
        Harness::OpenSession => Box::new(opensession::OpenSession::with_roots(vec![root.to_path_buf()])),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => Box::new(hermes::Hermes::with_home(root)),
        #[cfg(feature = "sqlite")]
        Harness::Cursor => Box::new(cursor::Cursor::with_root(root)),
        #[cfg(feature = "sqlite")]
        Harness::Goose => Box::new(goose::Goose::with_sessions_dir(root)),
        #[cfg(feature = "sqlite")]
        Harness::Zed => Box::new(zed::Zed::with_root(root)),
        #[cfg(feature = "sqlite")]
        Harness::Devin => Box::new(devin::Devin::with_root(root)),
        #[cfg(not(feature = "sqlite"))]
        Harness::Hermes | Harness::Cursor | Harness::Goose | Harness::Zed | Harness::Devin => return None,
    })
}

/// Assert the IR-v2 nesting invariant on a parsed session (`docs/INTERFACE-V2.md` §4), so every
/// adapter's tests state the rule the same way instead of each spelling out its own key list.
///
/// Every top-level key in `Session::extra` and in each `Message::extra` must be a **namespace whose
/// value is an object** — a canonical harness name (`Harness::as_str`, so an alias like `cc` fails)
/// or [`crate::ir::CV_NAMESPACE`]. Exactly two flat keys are allowed, both message-level and both cv's own
/// streaming bookkeeping rather than harness facts: [`CARRIER_KEY`](crate::ir::CARRIER_KEY) (`_record`, the verbatim
/// source record under [`Demand::COMPLETE`]) and [`crate::offsets::OFFSET_KEY`]
/// (`cv_byte_offset`). `Session::extra` has no flat exception at all.
#[cfg(test)]
pub(crate) fn assert_no_flat_keys(s: &Session) {
    let namespace = |k: &str| Harness::parse(k).is_some_and(|h| h.as_str() == k) || k == crate::ir::CV_NAMESPACE;
    for (k, v) in &s.extra {
        assert!(namespace(k), "flat session extra key {k:?} on a {} session", s.harness);
        assert!(v.is_object(), "namespace {k:?} must hold an object, got {v}");
    }
    for m in &s.messages {
        for (k, v) in &m.extra {
            if k == crate::ir::CARRIER_KEY || k == crate::offsets::OFFSET_KEY {
                continue; // the two documented flat exceptions
            }
            assert!(
                namespace(k),
                "flat message extra key {k:?} on {:?} (a {} session)",
                m.id,
                s.harness
            );
            assert!(v.is_object(), "namespace {k:?} must hold an object, got {v}");
        }
    }
}

/// A message carries the model its record states however much of the session preceded it
/// (INTERFACE-V2 §4). For every discovered session with a later assistant turn that a full read
/// gives a model, a read that discards everything before that turn, which is what `cv show
/// --range N..` and `--last N` do, reports the same model on its first delivered message.
#[cfg(test)]
pub(crate) fn assert_windowed_models(adapter: &dyn Adapter) {
    use crate::ir::{Message, Role};
    struct Window {
        start: usize,
        seen: usize,
        first: Option<Option<String>>,
    }
    impl MessageSink for Window {
        fn message(&mut self, m: Message) -> Flow {
            self.seen += 1;
            if self.seen <= self.start {
                return Flow::Continue;
            }
            self.first = Some(m.model);
            Flow::Stop
        }
    }
    let mut checked = 0;
    for r in adapter.discover().expect("discover") {
        let s = adapter
            .parse(&r)
            .unwrap_or_else(|e| panic!("parsing {}: {e:#}", r.path.display()));
        let Some(start) = s
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Assistant)
            .skip(1)
            .find(|(_, m)| m.model.is_some())
            .map(|(i, _)| i)
        else {
            continue;
        };
        let mut w = Window {
            start,
            seen: 0,
            first: None,
        };
        adapter
            .try_stream(&r, &Demand::LEAN.with_spans(), &mut w)
            .unwrap_or_else(|e| panic!("windowed read of {}: {e:#}", r.id));
        let windowed = w
            .first
            .unwrap_or_else(|| panic!("{}: the window from message {start} delivered nothing", r.id));
        assert_eq!(windowed, s.messages[start].model, "{}: message {start}", r.id);
        checked += 1;
    }
    assert!(
        checked > 0,
        "no session with a later assistant turn that states its model"
    );
}

/// Entries of an optional side directory. Only a missing directory is empty.
pub(crate) fn side_entries(path: &std::path::Path) -> Result<Vec<std::fs::DirEntry>> {
    use anyhow::Context;
    match std::fs::read_dir(path) {
        Ok(entries) => entries
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("reading {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_resume_hint_is_one_runnable_command_line() {
        assert_eq!(resume_in(None, "qwen --resume x".into()), "qwen --resume x");
        assert_eq!(
            resume_in(Some(Path::new("/work/my proj")), "claude --resume x".into()),
            "cd '/work/my proj' && claude --resume x"
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    use super::*;

    #[test]
    fn registration_order_names_every_harness_once_and_each_adapter_is_its_own() {
        let mut seen = std::collections::HashSet::new();
        assert!(ORDER.iter().all(|h| seen.insert(*h)), "ORDER repeats a harness");
        for h in ORDER {
            if let Some(adapter) = for_harness(h) {
                assert_eq!(adapter.harness(), h);
            }
        }
    }

    #[test]
    fn an_explicit_root_reads_nothing_else() {
        // A store's reported root may be the install dir above the one given (LM Studio's), never
        // anywhere detection would look.
        let world = crate::test_world("explicit-root-registry");
        let outer = world.base.join("explicit");
        let root = outer.join("store");
        std::fs::create_dir_all(&root).unwrap();
        for h in Harness::ALL {
            let Some(adapter) = for_harness_with_root(h, &root) else {
                continue;
            };
            assert_eq!(adapter.harness(), h);
            for found in adapter.roots() {
                assert!(
                    found.starts_with(&outer),
                    "{h}: {} is outside the explicit root",
                    found.display()
                );
            }
        }
    }

    #[test]
    fn one_derived_host() {
        // A second adapter with derived copies makes `derived_host` a choice the CLI must offer.
        let hosts: Vec<Harness> = Harness::ALL.into_iter().filter(|h| has_derived_copies(*h)).collect();
        assert_eq!(hosts, [derived_host()]);
    }

    #[test]
    fn ts_from_value_handles_all_shapes() {
        // RFC3339 string.
        let s = ts_from_value(&serde_json::json!("2026-01-02T03:04:05Z")).unwrap();
        assert_eq!(s.timestamp(), 1_767_323_045);
        // Epoch milliseconds (> 10^12).
        let ms = ts_from_value(&serde_json::json!(1_767_323_045_000i64)).unwrap();
        assert_eq!(ms, s);
        // Epoch seconds (<= 10^12), integer and float; a float keeps its millisecond part.
        assert_eq!(ts_from_value(&serde_json::json!(1_767_323_045i64)).unwrap(), s);
        assert_eq!(
            ts_from_value(&serde_json::json!(1_767_323_045.9f64))
                .unwrap()
                .timestamp_millis(),
            1_767_323_045_900
        );
        // Junk, and the "unset" column values no store means as 1970.
        assert!(ts_from_value(&serde_json::json!("not a time")).is_none());
        assert!(ts_from_value(&serde_json::json!(null)).is_none());
        assert!(ts_from_value(&serde_json::json!({"t": 1})).is_none());
        assert!(ts_from_value(&serde_json::json!(0)).is_none());
        assert!(ts_from_epoch(-5i64).is_none());
        assert!(ts_from_epoch(f64::NAN).is_none());
        assert_eq!(ts_from_epoch(1_767_323_045_000u64).unwrap(), s);
    }

    #[test]
    fn for_each_json_line_skips_junk_and_stops() {
        let text = "{\"n\":1}\n\n   \nnot json\n{\"n\":2}\n{\"n\":3}";
        let mut seen = Vec::new();
        let skipped = for_each_json_line_str(text, |v| {
            let n = v.get("n").and_then(Value::as_i64).unwrap_or(0);
            seen.push(n);
            if n == 2 {
                return Flow::Stop;
            }
            Flow::Continue
        });
        assert_eq!(seen, vec![1, 2], "blank/junk skipped, Stop honored");
        assert_eq!(skipped, 1, "the `not json` line counts; blank lines don't");

        // note_skipped_lines: zero leaves `extra` untouched (clean sessions stay byte-identical).
        let mut s = Session::new("x", Harness::Codex);
        note_skipped_lines(&mut s, 0);
        assert!(s.extra.is_empty());
        note_skipped_lines(&mut s, 3);
        // cv's own parse diagnostic lives in cv's namespace, never flat on the session
        // (`CV_NAMESPACE`; `docs/INTERFACE-V2.md` §4).
        let noted = |s: &Session| {
            s.extra
                .get(crate::ir::CV_NAMESPACE)
                .and_then(|v| v.get("skipped_lines"))
                .and_then(Value::as_u64)
        };
        assert_eq!(noted(&s), Some(3));
        assert!(!s.extra.contains_key("skipped_lines"), "never a flat session key");
        // Re-noting replaces rather than nesting again, and adds no second top-level key.
        note_skipped_lines(&mut s, 5);
        assert_eq!(noted(&s), Some(5));
        assert_eq!(s.extra.keys().collect::<Vec<_>>(), ["cv"]);
    }

    /// Every top-level key a session's `extra` may carry is a NAMESPACE object — a harness name or
    /// [`crate::ir::CV_NAMESPACE`]. `Session::extra` has no flat exception at all (the two documented flat
    /// keys, `_record` and `cv_byte_offset`, are message-level). Asserted here because
    /// `note_skipped_lines` is the one writer every adapter shares.
    #[test]
    fn cv_namespace_never_collides_with_a_harness_name() {
        assert!(
            Harness::parse(crate::ir::CV_NAMESPACE).is_none(),
            "`cv` must never be parseable as a harness, or its bag would shadow one"
        );
        assert!(!Harness::ALL.iter().any(|h| h.as_str() == crate::ir::CV_NAMESPACE));
    }
}
