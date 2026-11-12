//! Codex CLI adapter — `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (+ archived, + 2025 legacy JSON).
//!
//! Format drift handled: the first line is either a `session_meta` record, a bare `{id,timestamp}`
//! header, or (2025 legacy) a single `{session, items[]}` JSON document. Natural-language text
//! appears both as `event_msg` and as `response_item message`; when `event_msg`s are present we take
//! NL text from them and skip the `response_item` duplicates.
//!
//! Record types (ground truth: codex-rs `history::RolloutItem` + `protocol::EventMsg`, verified
//! against Codex `132c2be23`, CLI 0.154):
//! - `session_meta` — id, cwd, originator, cli_version, source, model_provider, git, plus (since
//!   0.147) the thread's place in a swarm: `session_id` (root thread), `parent_thread_id`,
//!   `forked_from_id`, `thread_source`, `agent_nickname`/`agent_path`/`agent_role`, `history_mode`
//!   and `subagent_history_start_ordinal`. Every line carries an `ordinal` in paginated files.
//! - `turn_context`  — per-turn model/effort/cwd/approval/sandbox; we track model drift.
//! - `response_item` — the model-visible items: `message`, `reasoning`, `function_call`,
//!   `function_call_output`, `custom_tool_call(_output)`, `local_shell_call`, `web_search_call`,
//!   `tool_search_call`/`tool_search_output`, `image_generation_call`, `compaction`, and the
//!   inter-agent `agent_message` (paired with a preceding `inter_agent_communication_metadata`).
//! - `event_msg`     — UI-side events. Legacy history mode (CLI ≤ 0.147) echoes NL text as
//!   `user_message`/`agent_message`; paginated mode (0.147+) drops those echoes and instead records
//!   every finished `TurnItem` as `item_completed` (command executions, file changes, MCP calls,
//!   image views, sub-agent activity, plans, …). Both modes carry `token_count` (usage + rate
//!   limits), `thread_settings_applied`, `task_started`/`task_complete`, `turn_aborted`,
//!   `thread_rolled_back`.
//! - `token_usage_record` — per-response usage keyed by `response_id`/`turn_id` (paginated).
//! - `compacted`     — a top-level record marking an auto/manual history compaction boundary.
//!
//! Fork/subagent rollouts embed the parent's history up to the fork point (ordinals below
//! `subagent_history_start_ordinal`, stamped with the FORK time); the lean passes skip those
//! records (the thread's own work is what a listing/title/search should show) and take a message's
//! real time from `internal_chat_message_metadata_passthrough.create_time` when present.
//!
//! Every message carries a precise `kind`/`origin` (INTERFACE-V2 §4): a human prompt vs. Codex's
//! injected preambles, swarm `agent_message`s from `Origin::Subagent`, `item_completed` notes,
//! compaction boundaries, model/effort changes, rollbacks (`Branch`), persisted errors, and — under
//! `stream::Demand::COMPLETE` — verbatim `Carrier`s for every record with no conversational shape.
//! Harness-specific facts (rate limits, turn ids, the structured `item`, swarm routing) live in
//! `extra["codex"]`, the only bag this adapter writes; images become `Block::Image` references.

mod emit;
mod hook;
mod legacy;
mod record;
pub use emit::emit;

use super::store::{bytes::ByteSource, jsonl};
use super::wire::openai;
use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::lazy::Text;
use crate::stream::{self, Flow, MessageSink, Start, Tier};
use anyhow::Result;
use chrono::{DateTime, Utc};
use record::{Demand, Reader};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use walkdir::WalkDir;

/// The one door into this adapter's per-message facts: `extra["codex"]` (INTERFACE-V2 §4).
fn bag(m: &mut Message) -> &mut Map<String, Value> {
    m.harness_extra_mut(Harness::Codex)
}

/// The session-level fact bag, `Session::extra["codex"]`.
fn sbag(s: &mut Session) -> &mut Map<String, Value> {
    s.harness_extra_mut(Harness::Codex)
}

/// Read a fact from a message's `extra["codex"]` bag.
fn bag_get<'a>(m: &'a Message, key: &str) -> Option<&'a Value> {
    m.harness_extra(Harness::Codex).and_then(|b| b.get(key))
}

/// Codex's injected preambles — text the HARNESS put in the user turn, not the user's own words:
/// the environment/user-instructions envelopes (0.1xx), the internal-context bundle, the plugin
/// catalog, the AGENTS.md dump and the @-mention file bundle (0.147+). Such a user message is
/// [`MessageKind::InjectedContext`] from [`Origin::Harness`], and never a title.
fn is_injected_text(t: &str) -> bool {
    let trimmed = t.trim_start();
    [
        "<environment_context",
        "<user_instructions",
        "<codex_internal_context",
        "<recommended_plugins",
        "# Codex CLI",
        "# AGENTS.md instructions",
        "# Files mentioned by the user",
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
}

/// A harness-side note: `Role::System`, `Origin::Harness`, the given kind, one text block.
fn note(kind: MessageKind, ts: Option<DateTime<Utc>>, text: String) -> Message {
    let mut m = Message::of_kind(Role::System, kind, Origin::Harness);
    m.timestamp = ts;
    m.content.push(Block::Text { text: text.into() });
    m
}

pub struct Codex {
    roots: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Codex = Codex { roots: Vec::new() };

impl Codex {
    pub fn detect() -> Self {
        // Codex resolves its home as `$CODEX_HOME`, else `~/.codex` (codex-rs/utils/home-dir/src/lib.rs
        // `find_codex_home`); honour the same override so a test home (or a relocated store) is read.
        let codex_home = crate::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| crate::env::home_dir().map(|h| h.join(".codex")));
        codex_home.map_or(Codex { roots: Vec::new() }, |c| Codex::with_home(&c))
    }

    /// An adapter over an explicit Codex home (the dir holding `sessions/`), instead of
    /// `$CODEX_HOME` / `~/.codex` — for the harness corpus and other tests, without mutating the
    /// process environment.
    pub fn with_home(codex_home: &Path) -> Self {
        let roots = [codex_home.join("sessions"), codex_home.join("archived_sessions")]
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        Codex { roots }
    }
}

impl Adapter for Codex {
    /// An MCP call Codex refused because the approval policy could not ask (`never`), or a
    /// `Plan` (`update_plan`) item completed notice.
    fn record_affordance(
        &self,
        m: &Message,
        first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        let Some(bag) = m.harness_extra(Harness::Codex) else {
            return Ok(None);
        };
        match bag.get("item_type").and_then(Value::as_str) {
            Some("McpToolCall") if first.contains("requires approval")? => Ok(Some(crate::affordance::RecordHit {
                kind: crate::affordance::Kind::Approval,
                action: "deny",
                tool: "McpToolCall",
            })),
            Some("Plan") => Ok(Some(crate::affordance::RecordHit {
                kind: crate::affordance::Kind::Plan,
                action: "write",
                tool: "update_plan",
            })),
            _ if bag.get("codex_event").and_then(Value::as_str) == Some("plan") => {
                Ok(Some(crate::affordance::RecordHit {
                    kind: crate::affordance::Kind::Plan,
                    action: "write",
                    tool: "update_plan",
                }))
            }
            _ => Ok(None),
        }
    }

    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn hook_event(&self, payload: &serde_json::Value) -> Result<Option<crate::hooks::Event>> {
        hook::hook_event(payload)
    }

    fn capabilities(&self) -> super::Capabilities {
        super::Capabilities {
            denials: &[],
            edit_guard: Some(crate::hooks::EditGuard::Deny),
            // A descendant's hook row names its own parent, so a stop counts by forest membership.
            lane_stops: Some(crate::lanes::StopScope::Forest),
            deferred_wrapper: None,
            // Native spans retain the same full-extra metadata.
            rich_spans: true,
        }
    }

    fn hook_lineage(&self, transcript: &Path) -> Result<Option<Lineage>> {
        Ok(hook_header(transcript)?.map(|s| s.lineage))
    }

    /// An app-server `FileChange` item, completed, records its edit in the Codex bag (code mode
    /// applies patches without an `apply_patch` call the transcript shows).
    fn native_edit<'m>(&self, m: &'m crate::ir::Message) -> Option<crate::events::NativeEdit<'m>> {
        let item = m.extra.get("codex")?.get("item")?;
        if item["type"] != "FileChange" || item["status"] != "completed" {
            return None;
        }
        let mut paths = Vec::new();
        for (path, change) in item.get("changes").and_then(Value::as_object).into_iter().flatten() {
            paths.push(path.as_str());
            paths.extend(change.get("move_path").and_then(Value::as_str));
        }
        Some(crate::events::NativeEdit {
            id: item.get("id").and_then(Value::as_str),
            tool: "apply_patch",
            paths,
        })
    }

    /// A plain `.jsonl` rollout only grows by appended lines; a compressed `.jsonl.zst` one's
    /// byte prefix is not a logical prefix.
    fn append_only(&self, r: &SessionRef) -> bool {
        r.path.extension().is_some_and(|e| e == "jsonl")
    }

    /// Membership comes from installed hooks, each child verified against its own header.
    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        Ok(super::select_forest(hook_forest(r)?, workflow, select))
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        // Walk (cheap) to collect candidate paths, then scan (file read + parse) them in parallel.
        let mut paths = Vec::new();
        let mut walk_errors = Vec::new();
        for root in &self.roots {
            for entry in WalkDir::new(root)
                .into_iter()
                .filter_map(|e| e.map_err(|e| walk_errors.extend(crate::Skipped::walk(&e))).ok())
            {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with("rollout-") {
                    continue;
                }
                // `.jsonl.zst`: Codex compresses rollouts colder than 7 days in place
                // (rollout/src/compression.rs); `.tmp` are its staging files. Discovery reports a
                // missing zstd capability as an unreadable source, never as an empty transcript.
                if !name.ends_with(".jsonl") && !name.ends_with(".json") && !is_zst_rollout(name) {
                    continue;
                }
                paths.push(path.to_path_buf());
            }
        }
        // An unreadable dir, and a rollout whose scan fails, are skipped and reported.
        let mut found: crate::Discovered = crate::par_discovery_filter_map(paths, |path| {
            // A plain rollout grows in place: its scan resumes where the last one ended.
            if is_plain_rollout(&path) {
                return crate::Skipped::listed(&path, cached_metadata(&path));
            }
            let scanned = || match is_zst_path(&path) {
                // Decoded in full: an index running beside discovery streams these same bytes.
                true => crate::codex_handoff::decode_and_offer(
                    || super::store::zst::content_size(&path),
                    || scan_rollout(&path),
                ),
                false => scan(&path),
            };
            crate::Skipped::listed(
                &path,
                crate::discover_cache::cached_scan_checked(&path, || scanned().map(Some)),
            )
        })
        .into_iter()
        .collect();
        found.skipped.extend(walk_errors);
        Ok(found)
    }

    fn stream(&self, r: &SessionRef, opts: &stream::Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        use anyhow::Context;
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    /// A modern `.jsonl` rollout through the driver. `has_events` is a whole-file property (do NL
    /// texts come from `event_msg`s or `response_item`s?), decided by a bounded pass over the
    /// head of the bytes read; a resume revalidates it before any callback. Only a plain file
    /// resumes: a compressed one's decoded offsets are not file positions, and the 2025 legacy
    /// document has no record boundaries.
    fn transcript(
        &self,
        r: &SessionRef,
        opts: &stream::Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            if !is_jsonl_path(&r.path) {
                return match from {
                    Start::Whole => legacy::stream(r, opts, sink).map(Some),
                    _ => Ok(None),
                };
            }
            if is_zst_path(&r.path) && !matches!(from, Start::Whole) {
                return Ok(None);
            }
            let restored = match from {
                Start::Checkpoint(cp) => match jsonl::restore::<ResumeState>(cp) {
                    Some(state) if state.session.source_path.as_deref() == Some(r.path.as_path()) => Some(state),
                    _ => return Ok(None),
                },
                _ => None,
            };
            let rollout = Rollout::open(&r.path)?;
            let len = rollout.input.len();
            let state = match (from, restored) {
                (Start::Checkpoint(cp), Some(state)) => {
                    if cp.offset > len || rollout.events(len)? != state.has_events {
                        return Ok(None);
                    }
                    state
                }
                (Start::Prefix(to), _) => ResumeState::new(Some(r.path.clone()), rollout.events(to.min(len))?, opts),
                _ => ResumeState::new(Some(r.path.clone()), rollout.events(len)?, opts),
            };
            let codec = Transcript {
                state,
                id: &r.id,
                opts,
                input: &rollout.input,
                held: Vec::new(),
            };
            jsonl::drive(&rollout.input, from, codec, sink)
        })
    }
}

/// Whole state between records, when no message awaits a following usage record.
#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState {
    session: Session,
    ctx: CodexCtx,
    has_events: bool,
    meta_sent: bool,
}

impl ResumeState {
    fn new(source_path: Option<PathBuf>, has_events: bool, opts: &stream::Demand) -> Self {
        Self {
            session: Session {
                source_path,
                ..Session::new(String::new(), Harness::Codex)
            },
            ctx: CodexCtx::for_opts(opts),
            has_events,
            meta_sent: false,
        }
    }
}

/// A `rollout-*.jsonl.zst`: Codex's in-place compression of cold rollouts (readable only with the
/// `zstd` byte-source capability).
fn is_zst_rollout(name: &str) -> bool {
    name.ends_with(".jsonl.zst")
}

fn is_zst_path(path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.ends_with(".jsonl.zst"))
}

/// Modern JSONL rollout (plain or zstd-compressed), as opposed to the 2025 single-JSON layout.
fn is_jsonl_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl") || is_zst_path(path)
}

/// Per-parse state every reader carries between records (the record codec reads and updates it).
/// A checkpoint keeps it whole.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct CodexCtx {
    /// Format-complete parse: keep inherited-prefix records (tagged `inherited`) instead of
    /// skipping them.
    complete: bool,
    /// `session_meta.subagent_history_start_ordinal`: records with a smaller top-level `ordinal`
    /// are the parent thread's history embedded in this fork/subagent rollout.
    inherit_before: Option<u64>,
    /// This thread's own `agent_path` (from `session_meta`), to attribute `agent_message` authorship.
    agent_path: Option<String>,
    /// The first `session_meta`'s `cli_version`, under the metadata demand (discovery's writer
    /// version sensor).
    writer_version: Option<String>,
    /// `inter_agent_communication_metadata.trigger_turn` seen, waiting for the `agent_message`
    /// record it immediately precedes.
    pending_trigger_turn: Option<bool>,
    /// The canonical (first) `session_meta` has been applied — a fork embeds the parent's as a
    /// second line, whose thread-level fields must not overwrite the child's.
    meta_seen: bool,
    /// That `session_meta` names cv as the writer ([`CV_ORIGINATOR`]): only then is a tool
    /// output's [`ERROR_PREFIX`] cv's sentinel.
    #[serde(default)]
    cv_wrote: bool,
    /// Usage from the last `token_usage_record`, until its `token_count` twin (same numbers, a few
    /// records later) has been seen — so the twin never double-counts as a second carrier.
    pending_record_usage: Option<Usage>,
    /// `info.total_token_usage` of the last `token_count`: the thread's running total. A snapshot
    /// that repeats it reports no new model call (Codex re-emits the last one, e.g. after a user
    /// message), so its `last_token_usage` must not be counted again.
    last_total_usage: Option<Value>,
}

impl CodexCtx {
    fn for_opts(opts: &stream::Demand) -> Self {
        CodexCtx {
            complete: opts.tier == Tier::Complete,
            ..Default::default()
        }
    }
}

/// Codex hook membership is a native UUID/parent relation, verified against each child's header.
/// This also makes the registered forest available to tools and query consumers of the one API.
fn hook_forest(parent: &SessionRef) -> Result<Vec<crate::AgentRef>> {
    let ledger = crate::lanes::ledger_index();
    let rows = &ledger.rows;
    let mut known = std::collections::HashSet::from([parent.id.clone()]);
    let mut out = Vec::new();
    // The rows whose parent is known and that are not yet decided, by ledger position. Each round
    // takes them in ledger order, and a child found mid-round makes its own rows candidates of
    // the same round when they lie further on, of the next round otherwise: the passes the fold
    // made over every row, without scanning a row whose parent is not in the forest. A row is
    // decided once (its header verifies or never will), so a forest costs its own rows.
    let mut candidates: std::collections::BTreeSet<usize> = ledger.rows_of(&parent.id).iter().copied().collect();
    while !candidates.is_empty() {
        let mut cursor = 0;
        while let Some(at) = candidates.range(cursor..).next().copied() {
            candidates.remove(&at);
            cursor = at + 1;
            let row = &rows[at];
            if known.contains(&row.agent_id) {
                continue;
            }
            let Some(path) = row.agent_transcript_path.as_deref() else {
                continue;
            };
            // A child whose transcript cannot be read yet is not listed (the ledger keeps it).
            let Some(head) = (match hook_header(path) {
                Ok(head) => head,
                Err(error) if super::is_not_found(&error) => None,
                Err(error) => return Err(error),
            }) else {
                continue;
            };
            if head.id != row.agent_id || head.lineage.parent.as_deref() != Some(row.session_id.as_str()) {
                continue;
            }
            known.insert(head.id.clone());
            candidates.extend(ledger.rows_of(&head.id).iter().copied());
            let mut session = SessionRef::new(head.id, Harness::Codex, path);
            session.cwd = head.cwd;
            session.created_at = head.created_at.or(Some(row.ts));
            session.updated_at = Some(row.ts);
            out.push(crate::AgentRef {
                session,
                agent_type: row.agent_type.clone(),
                description: None,
                tool_use_id: None,
                workflow: None,
                result_status: None,
                result_summary: None,
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.session.created_at));
    Ok(out)
}

/// Canonical header for a live hook's rollout: its first record, through the same captured
/// reader and codec as every other read. Hooks never scan history; a blank, rejected, oversized
/// (over 1 MiB) or incomplete first record has no usable identity (`None`). Read and codec
/// errors are the caller's.
pub fn hook_header(path: &Path) -> Result<Option<Session>> {
    let read = jsonl::Read::open(path)?;
    let mut s = Session {
        source_path: Some(path.to_path_buf()),
        ..Session::new(String::new(), Harness::Codex)
    };
    let mut ctx = CodexCtx::default();
    let mut out = Vec::new();
    let mut reader = Reader {
        session: &mut s,
        ctx: &mut ctx,
        out: &mut out,
        has_events: false,
        demand: Demand::from(&stream::Demand::LEAN),
        source: None,
    };
    read.prefix(1024 * 1024, |line| {
        if let Some(view) = line.view {
            reader.record(view)?;
        }
        Ok(Flow::Stop)
    })?;
    Ok((!s.id.is_empty()).then_some(s))
}

/// Parse a Codex transcript from its text contents into a [`Session`].
///
/// Pure (no filesystem); handles both the modern `.jsonl` rollout (`is_jsonl = true`) and the 2025
/// legacy single-JSON layout. `id` is the fallback session id (used when the transcript carries no
/// `session_meta`/header id); `source_path` records provenance when known. Each record goes
/// through the same codec as a file read, over an owned value.
pub fn parse_str(id: &str, text: &str, is_jsonl: bool, source_path: Option<PathBuf>) -> Result<Session> {
    // Start the id empty so a transcript-provided id (session_meta / bare header / legacy
    // `session.id`) is authoritative; the `id` argument is only the fallback when none is present.
    let mut s = Session {
        source_path,
        ..Session::new(String::new(), Harness::Codex)
    };
    let mut ctx = CodexCtx::default();
    let mut out: Vec<Message> = Vec::new();
    let demand = Demand::from(&stream::Demand::LEAN);
    if is_jsonl {
        let mut det = EventDetector::default();
        super::store::jsonl::slice(text.as_bytes(), |line| {
            Ok(match line.view.map(nl_flags).transpose()? {
                Some(flags) => det.feed(flags),
                None => Flow::Continue,
            })
        })?;
        let mut reader = Reader {
            session: &mut s,
            ctx: &mut ctx,
            out: &mut out,
            has_events: det.found,
            demand,
            source: None,
        };
        let mut skipped = 0;
        super::store::jsonl::slice(text.as_bytes(), |line| {
            if let Some(view) = line.view {
                reader.record(view)?;
            } else {
                skipped += u64::from(!line.blank);
            }
            Ok(Flow::Continue)
        })?;
        super::Unparsed(skipped).note(&mut s);
    } else {
        let root = serde_json::from_str::<Value>(text)?;
        let root = JsonView::from(&root);
        let mut reader = Reader {
            session: &mut s,
            ctx: &mut ctx,
            out: &mut out,
            has_events: false,
            demand,
            source: None,
        };
        legacy::items(&mut reader, root)?;
    }
    s.messages = out;
    if s.id.is_empty() {
        s.id = id.to_string();
    }
    Ok(s)
}

/// What [`EventDetector`] reads of a record: whether it carries natural-language text as an
/// `event_msg` (modern) or as a `response_item` message (old format).
fn nl_flags(v: JsonView<'_>) -> Result<(bool, bool)> {
    let head = record::Head::of(v)?;
    let Some(payload) = head.payload else {
        return Ok((false, false));
    };
    let payload = record::Payload::of(payload)?;
    Ok(match head.ty.as_deref() {
        Some("event_msg") => (
            matches!(payload.ty.as_deref(), Some("user_message" | "agent_message")),
            false,
        ),
        Some("response_item") => (
            false,
            payload.ty.as_deref() == Some("message") && matches!(payload.role.as_deref(), Some("user" | "assistant")),
        ),
        _ => (false, false),
    })
}

/// Bounded `has_events` detection: does this rollout carry its natural-language text as
/// `event_msg`s (modern) or only as `response_item` messages (old format)?
///
/// In modern rollouts the `event_msg` duplicate *trails* its `response_item` by a couple of
/// records (the user's prompt is recorded as a `response_item message` first, then echoed as an
/// `event_msg user_message`), so the first NL record alone can't decide. Instead we keep scanning
/// for [`LOOKAHEAD`] records past the first NL `response_item`; if no NL `event_msg` shows up by
/// then, the file is old-format. Measured over the full 620-rollout local corpus the worst
/// observed gap is **5 records** (LOOKAHEAD is >6× that), and the first NL event always lands
/// within the first 15 records — so this is byte-identical to the previous whole-file scan on
/// every real file, while old-format files (which used to force a full extra read, twice for a
/// multi-hundred-MB rollout) now stop after the head.
#[derive(Default)]
struct EventDetector {
    /// Records seen since the first NL `response_item` message, once one has been seen.
    past_first_nl_resp: Option<u32>,
    /// The verdict: NL text comes from `event_msg`s.
    found: bool,
}

impl EventDetector {
    /// How many records past the first NL `response_item` to keep looking for its `event_msg` twin.
    const LOOKAHEAD: u32 = 32;

    /// Feed one record's [`nl_flags`]; returns [`Flow::Stop`] once the verdict is decided.
    fn feed(&mut self, (event, response): (bool, bool)) -> Flow {
        if event {
            self.found = true;
            return Flow::Stop;
        }
        if let Some(n) = self.past_first_nl_resp.as_mut() {
            *n += 1;
            if *n > Self::LOOKAHEAD {
                return Flow::Stop; // old format: NL response_item with no event_msg echo
            }
        } else if response {
            self.past_first_nl_resp = Some(0);
        }
        Flow::Continue
    }
}

/// Flush `scratch` to `sink` — except a trailing assistant message that has no usage yet, which is
/// held back (it stays in `scratch`) so a following `token_count` record can attach usage to it.
/// This keeps the streaming paths' usage attachment identical to [`parse_str`], where the whole
/// message vec is still reachable when the event arrives: [`apply_token_count`] only ever targets
/// the last message, and the last message is exactly what's held. The hold is at most one message,
/// flushed by the next [`flush_all_but_held`] call or by the caller at EOF.
fn flush_all_but_held(scratch: &mut Vec<Message>, sink: &mut dyn MessageSink) -> Flow {
    // Also hold an assistant message whose usage came from a `token_usage_record`: its
    // `token_count` twin (rate limits, context window) follows a few records later and merges into
    // it — see [`apply_token_count`].
    let hold = scratch.last().is_some_and(|m| {
        m.role == Role::Assistant
            && (m.usage.is_none() || bag_get(m, "usage_source").and_then(Value::as_str) == Some(USAGE_FROM_RECORD))
    });
    let upto = scratch.len() - hold as usize;
    for m in scratch.drain(..upto) {
        if sink.message(m) == Flow::Stop {
            return Flow::Stop;
        }
    }
    Flow::Continue
}

/// A rollout's bytes for one read: the plain file, or a `.jsonl.zst` decoded once into one
/// private arena whose lazy children keep the compressed original's provenance.
#[derive(Clone)]
pub(crate) struct Rollout {
    input: Arc<jsonl::Read>,
}

impl Rollout {
    /// The decoded bytes it holds.
    pub(crate) fn decoded_bytes(&self) -> u64 {
        self.input.len()
    }
}

thread_local! {
    /// A rollout discovery already decoded, standing in for decoding it again (see
    /// [`crate::handoff`]).
    static DECODED: std::cell::RefCell<Option<Rollout>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with `rollout` standing in for a decode of its unchanged source on this thread.
pub(crate) fn with_decoded<T>(rollout: &Rollout, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Rollout>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DECODED.with(|decoded| *decoded.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(DECODED.with(|decoded| decoded.replace(Some(rollout.clone()))));
    f()
}

impl Rollout {
    fn open(path: &Path) -> Result<Self> {
        let input = jsonl::Read::open(path)?;
        let source = input.source().clone();
        let decoded = DECODED.with(|decoded| {
            decoded
                .borrow()
                .as_ref()
                .filter(|r| r.input.source() == &source)
                .cloned()
        });
        if let Some(rollout) = decoded {
            return Ok(rollout);
        }
        if !is_zst_path(path) {
            return Ok(Self { input: Arc::new(input) });
        }
        let backing = Arc::new(ByteSource::arena()?);
        let (offset, len) = backing.append(|writer| {
            let bytes = input.reader()?;
            let mut bytes = super::store::zst::decoder(bytes)
                .map_err(|e| anyhow::anyhow!("zstd header of {}: {e}", path.display()))?;
            std::io::copy(&mut bytes, &mut { writer })?;
            Ok(())
        })?;
        source.check()?;
        debug_assert_eq!(offset, 0);
        Ok(Self {
            input: Arc::new(input.decoded(backing, len)),
        })
    }

    /// Every line `from..to` through the shared JSONL byte source (a decoded copy reads to its end).
    fn lines(&self, from: u64, to: u64, visit: impl FnMut(jsonl::Line<'_>) -> Result<Flow>) -> Result<()> {
        self.input.snapshot(from, to, visit)
    }

    /// The bounded `has_events` pass over records `0..to` (see [`EventDetector`]).
    fn events(&self, to: u64) -> Result<bool> {
        let mut detector = EventDetector::default();
        self.lines(0, to, |line| match line.view {
            Some(view) => Ok(detector.feed(nl_flags(view)?)),
            None => Ok(Flow::Continue),
        })?;
        Ok(detector.found)
    }
}

/// The Codex record codec over one rollout: each record accumulates into the session and context
/// (a checkpoint's, or fresh at 0). A trailing assistant message is held until the usage record
/// that may follow it, so the codec is settled only when nothing is held. A record that published
/// a source descriptor makes the reader recheck the source's identity before going on.
struct Transcript<'r> {
    state: ResumeState,
    id: &'r str,
    opts: &'r stream::Demand,
    input: &'r jsonl::Read,
    held: Vec<Message>,
}

impl jsonl::Codec for Transcript<'_> {
    fn record(&mut self, view: JsonView<'_>, at: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        let ResumeState {
            session: s,
            ctx,
            has_events,
            meta_sent,
        } = &mut self.state;
        let before = self.held.len();
        Reader {
            session: s,
            ctx,
            out: &mut self.held,
            has_events: *has_events,
            demand: self.opts.into(),
            source: Some(self.input.source()),
        }
        .record(view)?;
        if self.opts.offsets && !self.input.is_decoded() {
            // Stamp this record's byte offset on the message(s) it produced (see
            // [`crate::offsets::OFFSET_KEY`]). Messages a later record amends (token_count usage)
            // keep their creating record's offset.
            for m in &mut self.held[before..] {
                m.extra.insert(crate::offsets::OFFSET_KEY.into(), at.into());
            }
        }
        // Hand the session metadata to the sink as soon as the model is known (session_meta /
        // turn_context land in the first records, before any message), so header-rendering sinks
        // have it ahead of the body.
        if !*meta_sent && s.model.is_some() {
            sink.meta(s);
            *meta_sent = true;
        }
        Ok(flush_all_but_held(&mut self.held, sink))
    }

    /// A trailing assistant is provisional until its usage arrives.
    fn settled(&self) -> bool {
        self.held.is_empty()
    }

    fn demand(&self) -> stream::Demand {
        *self.opts
    }

    fn session(&mut self) -> &mut Session {
        &mut self.state.session
    }

    fn state(&self) -> impl serde::Serialize + '_ {
        &self.state
    }

    fn finish(mut self, stopped: bool, sink: &mut dyn MessageSink) -> Result<Session> {
        if !stopped {
            // EOF: emit the held trailing message, if any (no token_count followed it).
            for m in self.held.drain(..) {
                if sink.message(m) == Flow::Stop {
                    break;
                }
            }
        }
        let s = &mut self.state.session;
        if s.id.is_empty() {
            s.id = self.id.to_string();
        }
        if !self.state.meta_sent {
            sink.meta(s);
        }
        Ok(self.state.session)
    }
}

/// `session_meta` fields with no first-class IR home, stashed verbatim (wire names) into
/// `Session::extra["codex"]` from the canonical (first) meta record: where the thread sits in a
/// swarm (`session_id` = root thread, `thread_source`, `agent_nickname`/`agent_role`), how it was
/// made (`source` — a string or, for subagents, an object), and how its file is laid out
/// (`history_mode`, `subagent_history_start_ordinal`, `history_base`). The lineage fields
/// (`parent_thread_id`, `forked_from_id`, `agent_path`) are first-class in [`Session::lineage`];
/// `base_instructions.text` is [`Session::system_prompt`].
const META_EXTRA_KEYS: &[&str] = &[
    "session_id",
    "thread_source",
    "agent_nickname",
    "agent_role",
    "source",
    "model_provider",
    "history_mode",
    "subagent_history_start_ordinal",
    "history_base",
    "cli_version",
    "originator",
];

/// Seed or update the session-level model and reasoning effort (`extra["codex"]["reasoning_effort"]`);
/// when either *changes* mid-session (`/model`, escalation, a `thread_settings_applied` event) the
/// switch is one [`MessageKind::ModelChange`] note so the drift survives into the IR.
/// `Message::model` names the new model when the model is what changed (the one place a message's
/// model equals the session's: the session's has just become this one); an effort-only change
/// leaves it unset. `extra` rides on the note in the bag (which record noticed it, the turn, the
/// effort, the cwd).
fn note_settings_change(
    s: &mut Session,
    out: &mut Vec<Message>,
    model: Option<&str>,
    effort: Option<&str>,
    ts: Option<DateTime<Utc>>,
    source: &str,
    mut extra: Map<String, Value>,
) {
    let mut changes = Vec::new();
    let mut new_model = None;
    if let Some(m) = model {
        if let Some(prev) = s.model.as_deref().filter(|prev| *prev != m) {
            changes.push(format!("model changed: {prev} → {m}"));
            new_model = Some(m.to_string());
        }
        s.model = Some(m.to_string());
    }
    if let Some(e) = effort {
        let prev = sbag(s)
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(prev) = prev.filter(|prev| prev != e) {
            changes.push(format!("effort changed: {prev} → {e}"));
        }
        sbag(s).insert("reasoning_effort".into(), Value::String(e.to_string()));
    }
    if changes.is_empty() {
        return;
    }
    let mut msg = note(MessageKind::ModelChange, ts, format!("[{}]", changes.join("; ")));
    msg.model = new_model;
    let b = bag(&mut msg);
    b.insert("codex_event".into(), Value::String(source.into()));
    b.append(&mut extra);
    out.push(msg);
}

/// Marker in `extra.usage_source` for usage that came from a `token_usage_record` (authoritative:
/// keyed by `response_id`/`turn_id`), so the `token_count` twin merges instead of duplicating.
const USAGE_FROM_RECORD: &str = "token_usage_record";

/// cv's emitter writes a failed tool result as the string `"[error] <content>"`: Codex's
/// `FunctionCallOutputPayload` persists only a string or a content-item array — never an error
/// bit (models.rs:2252-2275) — so this prefix is the one form that both decodes in Codex and
/// round-trips `is_error` through cv. Recognized on parse and stripped from the content only in a
/// rollout cv wrote ([`CV_ORIGINATOR`]): a tool's own output that happens to start `[error] ` is
/// the tool's text, not cv's sentinel.
const ERROR_PREFIX: &str = "[error] ";

/// `session_meta.originator` of a rollout cv's emitter wrote: the provenance [`ERROR_PREFIX`]
/// is read under.
const CV_ORIGINATOR: &str = "clustervision";

/// A listing title candidate, already trimmed and truncated ([`title_of`]): the first that is
/// not one of Codex's injected preambles ([`is_injected_text`]) wins.
fn consider_title(title: &mut Option<String>, candidate: String) {
    if title.is_none() && !candidate.is_empty() && !is_injected_text(&candidate) {
        *title = Some(candidate);
    }
}

/// A listing title from a body: the trimmed [`Head`](crate::projection::Head) of its joined text, where
/// only parts of type `only` count when given.
fn title_of(v: Option<JsonView<'_>>, only: Option<&str>) -> Result<String> {
    let mut head = crate::projection::Head::trimmed(crate::projection::Head::TITLE);
    record::for_each_joined_char_of(v, only, |ch| !head.push_char(ch))?;
    Ok(head.finish())
}

/// A plain `.jsonl` rollout: what Codex appends to while a session runs.
fn is_plain_rollout(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// Metadata-only scan: bodies are never resolved past a title and counts are deferred until a
/// full read or index pass. Every record is read (0.17 sampled 1 MiB of head and tail past
/// 8 MiB, and listed a different title or cwd than a full read would). Exact content is parsed
/// on actual open.
fn scan(path: &Path) -> Result<SessionRef> {
    if is_plain_rollout(path) {
        return Ok(scan_at(path, None)?.0);
    }
    scan_rollout(path).map(|(r, _)| r)
}

/// The discovery fold of a plain rollout, through the per-file scan cache with its continuation:
/// an unchanged rollout costs a stat, a grown one the appended records.
fn cached_metadata(path: &Path) -> Result<Option<SessionRef>> {
    crate::discover_cache::cached_scan_continued(path, LISTING, |from| {
        let (reference, checkpoint) = scan_at(path, from)?;
        let continuation = checkpoint.and_then(|checkpoint| {
            let head = crate::resume::MarkHead::of(path, &checkpoint, LISTING)?;
            Some(crate::discover_cache::ScanContinuation {
                head,
                checkpoint,
                dependencies: Default::default(),
            })
        });
        Ok((Some(reference), continuation))
    })
}

/// The checkpoint layer of [`scan_at`]'s fold.
const LISTING: &str = "codex.discovery.v1";

/// The discovery fold between records: what [`Reader::record`] keeps under the metadata demand.
#[derive(serde::Serialize, serde::Deserialize)]
struct Listing {
    session: Session,
    ctx: CodexCtx,
}

#[cfg(test)]
thread_local! {
    /// The `(offset, bytes)` each plain-rollout discovery fold read: the continuation test's record.
    static FOLDED: std::cell::RefCell<Vec<(u64, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Fold a plain rollout's records from `from`'s offset (or 0) to its end, and the state after
/// its last finished line: a partial last line is folded now but read again next time.
fn scan_at(path: &Path, from: Option<&stream::Checkpoint>) -> Result<(SessionRef, Option<stream::Checkpoint>)> {
    let rollout = Rollout::open(path)?;
    let len = rollout.input.len();
    let restored = from.and_then(|cp| {
        let state = cp.get::<Listing>(LISTING)?;
        (cp.offset <= len && state.session.source_path.as_deref() == Some(path)).then_some((state, cp.offset))
    });
    let (Listing { mut session, mut ctx }, offset) = restored.unwrap_or_else(|| {
        let session = Session {
            source_path: Some(path.to_path_buf()),
            ..Session::new(String::new(), Harness::Codex)
        };
        (
            Listing {
                session,
                ctx: CodexCtx::default(),
            },
            0,
        )
    });
    #[cfg(test)]
    FOLDED.with(|folded| folded.borrow_mut().push((offset, len - offset)));
    let mut out = Vec::new();
    let mut prefix: Option<stream::Checkpoint> = None;
    let mut finished = offset;
    let mut partial = false;
    {
        let mut reader = Reader {
            session: &mut session,
            ctx: &mut ctx,
            out: &mut out,
            has_events: false,
            demand: (&stream::Demand::DISCOVERY).into(),
            source: None,
        };
        rollout.lines(offset, len, |line| {
            if !line.complete && !partial {
                partial = true;
                prefix = Some(listing_checkpoint(finished, reader.session, reader.ctx));
            }
            if let Some(view) = line.view {
                reader.record(view)?;
            }
            if line.complete {
                finished = line.end;
            }
            Ok(Flow::Continue)
        })?;
    }
    if !partial {
        prefix = Some(listing_checkpoint(finished, &session, &ctx));
    }
    Ok((listed(path, session, &ctx), prefix))
}

fn listing_checkpoint(offset: u64, session: &Session, ctx: &CodexCtx) -> stream::Checkpoint {
    #[derive(serde::Serialize)]
    struct ListingRef<'a> {
        session: &'a Session,
        ctx: &'a CodexCtx,
    }
    let mut checkpoint = stream::Checkpoint::new(offset);
    checkpoint.put(LISTING, &ListingRef { session, ctx });
    checkpoint
}

/// A decoded `.jsonl.zst` or a 2025 single-document rollout, scanned whole, keeping the rollout
/// when it was decoded.
fn scan_rollout(path: &Path) -> Result<(SessionRef, Option<Rollout>)> {
    let mut s = Session::new(String::new(), Harness::Codex);
    let mut ctx = CodexCtx::default();
    let mut out = Vec::new();
    let mut decoded = None;
    if !is_jsonl_path(path) {
        s = legacy::scan(path)?;
    } else {
        let mut s = Reader {
            session: &mut s,
            ctx: &mut ctx,
            out: &mut out,
            has_events: false,
            demand: (&stream::Demand::DISCOVERY).into(),
            source: None,
        };
        let rollout = Rollout::open(path)?;
        rollout.lines(0, rollout.input.len(), |line| {
            if let Some(view) = line.view {
                s.record(view)?;
            }
            Ok(Flow::Continue)
        })?;
        decoded = rollout.input.is_decoded().then_some(rollout);
    }
    Ok((listed(path, s, &ctx), decoded))
}

/// The listing row a scan's session makes, its writer version observed.
fn listed(path: &Path, mut s: Session, ctx: &CodexCtx) -> SessionRef {
    if s.id.is_empty() {
        // `rollout-<ts>-<uuid>.jsonl[.zst]`: strip both suffixes.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        s.id = name
            .trim_end_matches(".zst")
            .trim_end_matches(".jsonl")
            .trim_end_matches(".json")
            .to_string();
    }

    let r = SessionRef {
        cwd: s.cwd,
        title: s.title,
        created_at: s.created_at,
        updated_at: s.updated_at,
        message_count: None,
        ..SessionRef::new(s.id, Harness::Codex, path.to_path_buf())
    };
    crate::discover_cache::read::Context::current().observe_writer(&r, ctx.writer_version.as_deref());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Checkpoint;

    #[test]
    fn hook_forest_verifies_native_parents_and_follows_descendants() {
        let world = crate::test_world("codex-hook-forest");
        let parent = SessionRef::new("root", crate::Harness::Codex, world.base.join("root.jsonl"));
        let row = |id: &str, parent: &str, actual_parent: &str| {
            let path = world.base.join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                serde_json::json!({"type":"session_meta", "payload":{"id":id,"parent_thread_id":actual_parent}})
                    .to_string()
                    + "\n",
            )
            .unwrap();
            crate::lanes::LaneEvent {
                ts: chrono::Utc::now(),
                event: crate::lanes::LaneEventKind::Start,
                session_id: parent.into(),
                agent_id: id.into(),
                agent_type: Some("default".into()),
                transcript_path: None,
                agent_transcript_path: Some(path),
                cwd: None,
                last_text: None,
            }
        };
        // Descendant first; duplicated stop; wrong native parent must never become membership.
        crate::lanes::append_lane_event(&row("grandchild", "child", "child")).unwrap();
        let mut child = row("child", "root", "root");
        crate::lanes::append_lane_event(&child).unwrap();
        child.event = crate::lanes::LaneEventKind::Stop;
        crate::lanes::append_lane_event(&child).unwrap();
        crate::lanes::append_lane_event(&row("foreign", "root", "another-parent")).unwrap();
        let forest = crate::subagent_tree_of(&parent).unwrap();
        let ids: std::collections::HashSet<_> = forest.iter().map(|s| s.session.id.as_str()).collect();
        assert_eq!(ids, std::collections::HashSet::from(["child", "grandchild"]));
        assert!(forest.iter().all(|s| s.session.harness == crate::Harness::Codex));
        // A `--subagents` pass asks once per parent: the ledger is parsed once per file state.
        let rows = crate::lanes::read_ledger();
        crate::subagent_tree_of(&parent).unwrap();
        crate::subagent_tree_of(&SessionRef::new(
            "other",
            crate::Harness::Codex,
            world.base.join("other.jsonl"),
        ))
        .unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&rows, &crate::lanes::read_ledger()),
            "unchanged: not reparsed"
        );
        let index = crate::lanes::ledger_index();
        crate::subagent_tree_of(&parent).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&index, &crate::lanes::ledger_index()),
            "unchanged: indexed once"
        );
        crate::lanes::append_lane_event(&row("late", "root", "root")).unwrap();
        let after = crate::lanes::read_ledger();
        assert!(!std::sync::Arc::ptr_eq(&rows, &after), "an append is read");
        assert_eq!(after.len(), rows.len() + 1);
    }

    use crate::harness::Parse;

    #[test]
    fn legacy_title_early_stop_still_validates_the_whole_document() -> Result<()> {
        let world = crate::test_world("codex-legacy-tail");
        let path = world.proj.join("rollout-legacy.json");
        let start = r#"{"session":{"id":"legacy","timestamp":"2026-01-01T00:00:00Z"},"items":[{"type":"message","content":"first title"},"#;
        fs::write(
            &path,
            format!("{start}{{\"type\":\"reasoning\",\"ignored\":[true,null]}}]}}"),
        )?;
        assert_eq!(scan(&path)?.title.as_deref(), Some("first title"));
        fs::write(
            &path,
            format!("{start}{{\"type\":\"reasoning\",\"ignored\":[true,]}}]}}"),
        )?;
        assert!(scan(&path).is_err(), "a useful title cannot hide a malformed tail");
        Ok(())
    }

    #[test]
    fn historical_count_prefix_accepts_its_unterminated_snapshot_end() -> Result<()> {
        let world = crate::test_world("codex-count-snapshot");
        let path = world.proj.join("rollout-prefix.jsonl");
        let first =
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"snapshot question"}}"#;
        fs::write(&path, format!("{first}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"later question\"}}}}\n"))?;
        let r = SessionRef::new("prefix", Harness::Codex, &path);
        assert_eq!(crate::watch::count_prefix(&r, first.len() as u64, &mut || Ok(()))?, 1);
        assert_eq!(adapter().parse(&r)?.messages.iter().filter(|m| m.is_turn()).count(), 2);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn callback_truncation_stops_before_resuming_mapped_records() -> Result<()> {
        if !crate::harness::store::jsonl::tests::in_child(
            "harness::codex::tests::callback_truncation_stops_before_resuming_mapped_records",
        ) {
            return Ok(());
        }
        struct Truncate {
            path: PathBuf,
            stage: u8,
            mutated: bool,
        }
        impl Truncate {
            fn at(&mut self, stage: u8) {
                if self.stage == stage && !self.mutated {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&self.path)
                        .unwrap()
                        .set_len(0)
                        .unwrap();
                    self.mutated = true;
                    // A callback that changed the file is a slow one by construction; the reader
                    // checks the backing after a slow callback, before touching the mapping again.
                    std::thread::sleep(crate::harness::store::jsonl::Read::SLOW_CALLBACK * 10);
                }
            }
        }
        impl MessageSink for Truncate {
            fn message(&mut self, _: Message) -> Flow {
                self.at(0);
                Flow::Continue
            }
            fn meta(&mut self, _: &Session) {
                self.at(1);
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                self.at(2);
                self.at(3);
                self.stage != 2
            }
            fn checkpoint(&mut self, _: &mut Checkpoint) {
                self.at(4);
            }
        }
        let world = crate::test_world("codex-callback-shrink");
        let path = world.proj.join("rollout-shrink.jsonl");
        let prompt = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"owned prompt\"}}\n";
        let raw = format!(
            "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"test-model\"}}}}\n{}",
            prompt.repeat(1024)
        );
        let reference = SessionRef::new("shrink", Harness::Codex, &path);
        for stage in 0..5 {
            fs::write(&path, &raw)?;
            let mut sink = Truncate {
                path: path.clone(),
                stage,
                mutated: false,
            };
            assert!(
                adapter().stream(&reference, &stream::Demand::RICH, &mut sink).is_err(),
                "callback stage {stage}"
            );
            assert!(sink.mutated, "the intended callback must actually run");
        }
        Ok(())
    }

    #[test]
    fn held_final_assistant_verifies_source_after_delivery() -> Result<()> {
        let world = crate::test_world("codex-held-completion");
        let path = world.proj.join("rollout-held.jsonl");
        let raw = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"held answer"}]}}"#;
        let reference = SessionRef::new("held", Harness::Codex, &path);
        for stop in [Flow::Stop, Flow::Continue] {
            fs::write(&path, raw)?;
            let mut calls = 0;
            adapter().stream(&reference, &stream::Demand::RICH, &mut |_: Message| {
                calls += 1;
                stop
            })?;
            assert_eq!(calls, 1, "unchanged source accepts either callback outcome");
            let mut retained = None;
            let result = adapter().stream(&reference, &stream::Demand::RICH, &mut |m: Message| {
                retained = Some(m);
                fs::remove_file(&path).unwrap();
                stop
            });
            assert!(result.is_err(), "held EOF output must not bypass verification");
            assert_eq!(retained.unwrap().text().as_deref(), Some("held answer"));
        }
        struct RemoveAtFinalMeta(PathBuf);
        impl MessageSink for RemoveAtFinalMeta {
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
            fn meta(&mut self, _: &Session) {
                fs::remove_file(&self.0).unwrap();
            }
        }
        fs::write(&path, raw)?;
        assert!(
            adapter()
                .stream(&reference, &stream::Demand::RICH, &mut RemoveAtFinalMeta(path))
                .is_err(),
            "the last metadata callback is inside completion verification too"
        );
        Ok(())
    }

    #[test]
    fn shared_classification_preserves_missing_role_and_canonical_meta() -> Result<()> {
        let raw = [
            r#"{"type":"session_meta","payload":{"id":"child","cwd":"/child","cli_version":"0.163.0","subagent_history_start_ordinal":5,"agent_path":"child"}}"#,
            r#"{"ordinal":1,"type":"session_meta","payload":{"id":"parent","cwd":"/parent","cli_version":"99","subagent_history_start_ordinal":0}}"#,
            r#"{"ordinal":2,"type":"response_item","payload":{"type":"message","content":"parent prompt"}}"#,
            r#"{"ordinal":6,"type":"response_item","payload":{"type":"message","content":"own prompt"}}"#,
        ].join("\n");
        let mut scan = Session::new(String::new(), Harness::Codex);
        let mut ctx = CodexCtx::default();
        let mut out = Vec::new();
        let mut reader = Reader {
            session: &mut scan,
            ctx: &mut ctx,
            out: &mut out,
            has_events: false,
            demand: (&stream::Demand::DISCOVERY).into(),
            source: None,
        };
        jsonl::slice(raw.as_bytes(), |line| {
            if let Some(v) = line.view {
                reader.record(v)?;
            }
            Ok(Flow::Continue)
        })?;
        assert!(out.is_empty(), "the metadata demand builds no message");
        assert_eq!(scan.id, "child");
        assert_eq!(scan.cwd.as_deref(), Some(Path::new("/child")));
        assert_eq!(ctx.writer_version.as_deref(), Some("0.163.0"));
        assert_eq!(scan.title.as_deref(), Some("own prompt"));
        let session = parse_str("child", &raw, true, None).unwrap();
        assert_eq!(
            session.messages.iter().filter_map(Message::text).collect::<Vec<_>>(),
            ["own prompt"]
        );
        // An absent role is title/message compatible but does not certify an NL event twin.
        let v: Value =
            serde_json::from_str(r#"{"type":"response_item","payload":{"type":"message","content":"own prompt"}}"#)?;
        assert_eq!(nl_flags(JsonView::from(&v))?, (false, false));
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn deferred_discovery_counts_refill_and_invalidate_on_source_change() -> Result<()> {
        let world = crate::test_world("codex-count-publication");
        let home = world.base.join("codex");
        let dir = home.join("sessions");
        fs::create_dir_all(&dir)?;
        let path = dir.join("rollout-count.jsonl");
        let first = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"count\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"question\"}}\n";
        fs::write(&path, first)?;
        let adapter = Codex::with_home(&home);
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let r = adapter.discover()?.remove(0);
        assert_eq!(r.message_count, None);
        crate::catalog::sync(std::slice::from_ref(&r));
        crate::discover_cache::persist(false);
        let mut sink = crate::stream::CollectSink::default();
        let done = crate::stream::stream_counted(
            &adapter,
            &r,
            &stream::Demand::FACTS.with_spans(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(done.counted.unwrap().turns, 1);
        assert_eq!(adapter.discover()?[0].message_count, Some(1));
        drop(_scope);
        let next = crate::discover_cache::read::Context::current();
        let _next_scope = next.enter();
        assert_eq!(adapter.discover()?[0].message_count, Some(1));
        fs::write(&path, format!("{first}{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"next\"}}}}\n"))?;
        let changed = adapter.discover()?.remove(0);
        assert_eq!(changed.message_count, None, "changed sources lose certified counts");
        crate::catalog::sync(std::slice::from_ref(&changed));
        crate::stream::stream_counted(
            &adapter,
            &changed,
            &stream::Demand::FACTS.with_spans(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(adapter.discover()?[0].message_count, Some(2));
        Ok(())
    }

    #[derive(Default)]
    struct BufferedKept {
        messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
        metadata: usize,
        dependencies: usize,
    }

    impl MessageSink for BufferedKept {
        fn message(&mut self, m: Message) -> Flow {
            self.messages.push(m);
            Flow::Continue
        }
        fn meta(&mut self, _: &Session) {
            self.metadata += 1;
        }
        fn dependency(&mut self, _: &Path) {
            self.dependencies += 1;
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), cp.clone()));
        }
    }

    fn adapter() -> Codex {
        Codex { roots: Vec::new() }
    }

    fn buffered_pass(r: &SessionRef, opts: &stream::Demand, sink: &mut dyn MessageSink) -> Session {
        adapter().stream(r, opts, sink).unwrap()
    }

    /// The whole stream of `r`, messages collected.
    fn whole(r: &SessionRef, opts: &stream::Demand) -> Session {
        let mut sink = crate::stream::CollectSink::default();
        let mut s = buffered_pass(r, opts, &mut sink);
        s.messages = sink.messages;
        s
    }

    /// `text` as a rollout file streamed under `opts`, resolved before the file goes.
    fn stream_text(id: &str, text: &str, opts: &stream::Demand) -> Session {
        let dir = cv_testkit::temp_dir("codex-stream-text");
        let path = dir.join("rollout-text.jsonl");
        fs::write(&path, text).unwrap();
        let mut s = whole(&SessionRef::new(id, Harness::Codex, &path), opts);
        s.materialize().unwrap();
        s.source_path = None;
        fs::remove_dir_all(dir).unwrap();
        s
    }

    #[test]
    fn usage_disjoint_input_saturates_overlapping_cache_counts() {
        let usage = serde_json::json!({
            "input_tokens":u64::MAX,
            "cached_input_tokens":u64::MAX,
            "cache_write_input_tokens":1,
            "output_tokens":2
        });
        let usage = record::parse_usage(Some((&usage).into())).unwrap().unwrap();
        assert_eq!(usage.input_tokens, Some(0));
        assert_eq!(usage.cache_read_tokens, Some(u64::MAX));
        assert_eq!(usage.cache_creation_tokens, Some(1));
        assert_eq!(usage.output_tokens, Some(2));
    }

    // Independent old buffered transport/collector: preserve .lines(), trim, lean/full fallback,
    // held-message EOF handling, and corruption counts while the production driver tracks bytes.
    #[cfg(feature = "zstd")]
    #[test]
    fn portable_decoded_text_keeps_source_identity_and_rejects_mid_read_changes() {
        let world = crate::test_world("codex-portable-source-change");
        let text = "output\n\"🌙\\".repeat(8192);
        let record = serde_json::json!({"type":"response_item","payload":{
            "type":"function_call_output","call_id":"call","output":text}})
        .to_string();
        let compressed = zstd::encode_all(record.as_bytes(), 1).unwrap();
        for (path, bytes) in [
            (world.base.join("rollout-source.jsonl"), record.into_bytes()),
            (world.base.join("rollout-source.jsonl.zst"), compressed),
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let r = SessionRef::new("source", Harness::Codex, &path);
            let mut sink = crate::stream::CollectSink::default();
            let session = buffered_pass(&r, &stream::Demand::LEAN.with_spans(), &mut sink);
            assert_eq!(session.source_path.as_deref(), Some(path.as_path()));
            let Block::ToolResult { content, .. } = &sink.messages[0].content[0] else {
                panic!("missing result")
            };
            assert!(content.inline_str().is_none());
            let resolver = session.resolver();
            assert_eq!(content.try_resolve(&resolver).unwrap(), text);
            let mut owned = crate::stream::CollectSink::default();
            buffered_pass(&r, &stream::Demand::RICH, &mut owned);
            let Block::ToolResult {
                content: owned_text, ..
            } = &owned.messages[0].content[0]
            else {
                panic!("missing owned result")
            };
            assert_eq!(owned_text.inline_str(), Some(text.as_str()));
            std::fs::write(&path, b"replaced original source").unwrap();
            assert!(content.try_resolve(&resolver).is_err());
            assert!(content.try_resolve_prefix(&resolver, 128).is_err());
            assert_eq!(owned_text.inline_str(), Some(text.as_str()));
            std::fs::write(&path, &bytes).unwrap();
            let mut mutate = |_: Message| {
                std::fs::write(&path, b"changed during read").unwrap();
                Flow::Stop
            };
            assert!(adapter()
                .stream(&r, &stream::Demand::LEAN.with_spans(), &mut mutate)
                .is_err());
        }
    }

    #[test]
    fn buffered_resume_native_corpus_and_options_match_old_exact_suffixes() {
        let world = crate::test_world("codex_buffered_resume_corpus");
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/codex");
        let mut files = 0;
        let mut resumes = 0;
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            files += 1;
            let r = SessionRef::new("oracle", Harness::Codex, entry.path().to_path_buf());
            for opts in [
                stream::Demand::LEAN.with_spans(),
                stream::Demand::RICH.with_spans(),
                stream::Demand::RICH,
                stream::Demand::LEAN,
                stream::Demand::COMPLETE,
            ] {
                let mut full = BufferedKept::default();
                let mut shell = buffered_pass(&r, &opts, &mut full);
                shell.messages = full.messages.clone();
                for (index, (count, cp)) in full.checkpoints.iter().enumerate() {
                    let mut suffix = BufferedKept::default();
                    let mut resumed = adapter
                        .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(&suffix.messages).unwrap(),
                        serde_json::to_value(&full.messages[*count..]).unwrap()
                    );
                    resumed.messages = full.messages[..*count].iter().cloned().chain(suffix.messages).collect();
                    assert_eq!(
                        serde_json::to_value(resumed).unwrap(),
                        serde_json::to_value(&shell).unwrap()
                    );
                    assert_eq!(
                        suffix.checkpoints,
                        full.checkpoints[index + 1..]
                            .iter()
                            .map(|(n, cp)| (n - count, cp.clone()))
                            .collect::<Vec<_>>()
                    );
                    resumes += 1;
                }
            }
        }
        eprintln!("codex native files={files} exact suffix resumes={resumes}");
        assert!(files >= 16 && resumes > 100);
    }

    #[test]
    fn checked_complete_resume_rejects_old_flavors_and_preserves_current_suffix() {
        use std::io::Write as _;
        let world = crate::test_world("codex-complete-resume-flavor");
        let path = world.base.join("rollout-complete.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"cwd\":\"/synthetic\"}}\n",
                "{\"type\":\"world_state\",\"payload\":{\"kept\":true}}\n"
            ),
        )
        .unwrap();
        let r = SessionRef::new("s", Harness::Codex, path);
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let opts = stream::Demand {
            spans: true,
            ..stream::Demand::COMPLETE
        };
        let mut prefix = BufferedKept::default();
        buffered_pass(&r, &opts, &mut prefix);
        let (count, checkpoint) = prefix.checkpoints.last().unwrap();
        assert!(*count > 0);
        assert!(checkpoint.state[crate::stream::ADAPTER_LAYER]["demand"]["spans"] == Value::Bool(true));
        writeln!(fs::OpenOptions::new().append(true).open(&r.path).unwrap(), "{}",
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"appended"}]}})).unwrap();
        for old_native in [false, true] {
            let mut old = checkpoint.clone();
            let layer = old.state.get_mut(crate::stream::ADAPTER_LAYER).unwrap();
            layer["demand"]["spans"] = Value::Bool(false);
            if old_native {
                layer["state"]["ctx"]["complete"] = Value::Bool(false);
            }
            let mut sink = BufferedKept::default();
            assert!(adapter
                .transcript(&r, &opts, crate::stream::Start::Checkpoint(&old), &mut sink)
                .unwrap()
                .is_none());
            assert!(sink.messages.is_empty());
        }
        let mut suffix = BufferedKept::default();
        let mut resumed = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(checkpoint), &mut suffix)
            .unwrap()
            .unwrap();
        resumed.messages = prefix.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        let mut whole = BufferedKept::default();
        let mut expected = buffered_pass(&r, &opts, &mut whole);
        expected.messages = whole.messages;
        assert_eq!(
            serde_json::to_vec(&resumed).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }

    #[test]
    fn buffered_resume_replays_pending_usage_partial_eof_and_declines_before_callbacks() {
        use std::io::Write as _;
        let world = crate::test_world("codex_buffered_resume_pending");
        let path = world.base.join("rollout-pending.jsonl");
        let prefix = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"model\":\"test\"}}\r\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
            "bad json\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"held\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{"
        );
        let prefix = prefix.replace("\"held\"", &format!("\"{}\"", "held_sentinel".repeat(100_000)));
        fs::write(&path, &prefix).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let opts = stream::Demand::RICH;
        let mut first = BufferedKept::default();
        buffered_pass(&r, &opts, &mut first);
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 1, "held EOF reply is not part of the immutable prefix");
        // The rejected line is a boundary too: its checkpoint carries the skipped count.
        assert_eq!(
            cp.offset as usize,
            prefix.find("bad json\n").unwrap() + "bad json\n".len()
        );
        let state = serde_json::to_string(cp).unwrap();
        assert!(state.len() < 2048 && !state.contains("held_sentinel"));
        writeln!(
            fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            "\"total_token_usage\":{{\"input_tokens\":9,\"output_tokens\":3}},\"last_token_usage\":{{\"input_tokens\":9,\"output_tokens\":3}}}}}}}}"
        )
        .unwrap();
        let mut bytes = fs::read(&r.path).unwrap();
        bytes.extend_from_slice(b"\xff\n\n");
        bytes.extend_from_slice("\u{2003}{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"escaped \\\" \\u263a\"}}\u{2003}\r\n".as_bytes());
        fs::write(&r.path, bytes).unwrap();
        let mut suffix = BufferedKept::default();
        let mut resumed = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
            .unwrap()
            .unwrap();
        resumed.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        assert_eq!(resumed.messages[1].usage.as_ref().unwrap().input_tokens, Some(9));
        assert_eq!(
            serde_json::to_value(resumed).unwrap(),
            serde_json::to_value(whole(&r, &opts)).unwrap()
        );

        let mut untouched = BufferedKept::default();
        assert!(adapter
            .transcript(
                &r,
                &stream::Demand::COMPLETE,
                crate::stream::Start::Checkpoint(cp),
                &mut untouched
            )
            .unwrap()
            .is_none());
        let mut elsewhere = r.clone();
        elsewhere.path = world.base.join("rollout-other.jsonl");
        assert!(adapter
            .transcript(&elsewhere, &opts, crate::stream::Start::Checkpoint(cp), &mut untouched)
            .unwrap()
            .is_none());
        elsewhere.path = world.base.join("rollout.json");
        assert!(adapter
            .transcript(&elsewhere, &opts, crate::stream::Start::Checkpoint(cp), &mut untouched)
            .unwrap()
            .is_none());
        elsewhere.path = world.base.join("rollout.jsonl.zst");
        assert!(adapter
            .transcript(&elsewhere, &opts, crate::stream::Start::Checkpoint(cp), &mut untouched)
            .unwrap()
            .is_none());
        fs::write(&r.path, b"").unwrap();
        assert!(adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut untouched)
            .unwrap()
            .is_none());
        assert!(untouched.messages.is_empty() && untouched.checkpoints.is_empty());
        assert_eq!((untouched.metadata, untouched.dependencies), (0, 0));
    }

    #[test]
    fn buffered_resume_append_work_excludes_stable_prefix_and_revalidates_detector() {
        use std::io::Write as _;
        let world = crate::test_world("codex_buffered_resume_append");
        let path = world.base.join("rollout-append.jsonl");
        let line = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n";
        fs::write(&path, line.repeat(20_000)).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let opts = stream::Demand::RICH;
        let mut first = BufferedKept::default();
        buffered_pass(&r, &opts, &mut first);
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        fs::OpenOptions::new()
            .append(true)
            .open(&r.path)
            .unwrap()
            .write_all(line.repeat(200).as_bytes())
            .unwrap();
        let mut suffix = BufferedKept::default();
        let resumed = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
            .unwrap()
            .unwrap();
        let mut all = whole(&r, &opts);
        assert_eq!(
            serde_json::to_value(&suffix.messages).unwrap(),
            serde_json::to_value(&all.messages[*count..]).unwrap()
        );
        all.messages.clear();
        assert_eq!(
            serde_json::to_value(resumed).unwrap(),
            serde_json::to_value(all).unwrap()
        );
        assert_eq!(suffix.messages.len(), 200);
        let bytes = serde_json::to_vec(cp).unwrap().len();
        eprintln!("codex stable_prefix=20000 appended_callbacks=200 whole_callbacks=20200 checkpoint_bytes={bytes}");
        assert!(bytes < 2048);

        let old = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"old\"}]}}\n";
        fs::write(&r.path, old).unwrap();
        let mut first = BufferedKept::default();
        buffered_pass(&r, &opts, &mut first);
        let cp = &first.checkpoints.last().unwrap().1;
        fs::OpenOptions::new()
            .append(true)
            .open(&r.path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let mut untouched = BufferedKept::default();
        assert!(adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut untouched)
            .unwrap()
            .is_none());
        assert!(untouched.messages.is_empty() && untouched.checkpoints.is_empty());
        assert_eq!((untouched.metadata, untouched.dependencies), (0, 0));
    }

    /// A fixture under `tests/fixtures/codex/`, without its final newline (the text the old
    /// inline `lines.join("\n")` built).
    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/codex/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        text.strip_suffix('\n').map(str::to_string).unwrap_or(text)
    }

    /// A JSONL fixture's records, one string per line.
    fn fixture_lines(name: &str) -> Vec<String> {
        fixture(name).split('\n').map(str::to_string).collect()
    }

    /// Parse JSONL lines into a Session, checking the IR-v2 nesting invariant on the way out:
    /// every Codex fact must be in `extra["codex"]`, never flat (`crate::harness::assert_no_flat_keys`).
    /// Doing it here means every test below that builds a session enforces it for free.
    fn parse_jsonl(lines: &[impl std::borrow::Borrow<str>]) -> Session {
        let text = lines.join("\n");
        let s = parse_str("fallback-id", &text, true, None).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        s
    }

    /// [`parse_jsonl`] over a fixture's records.
    fn parse_fixture(name: &str) -> Session {
        parse_jsonl(&fixture_lines(name))
    }

    fn first_block<'a>(s: &'a Session, pred: impl Fn(&&'a Block) -> bool) -> &'a Block {
        s.messages
            .iter()
            .flat_map(|m| &m.content)
            .find(pred)
            .expect("matching block")
    }

    #[test]
    fn session_meta_seeds_id_cwd_git() {
        let s = parse_fixture("session_meta_seeds_id_cwd_git.jsonl");
        assert_eq!(s.id, "abc-123");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work")));
        let g = s.git.unwrap();
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.commit.as_deref(), Some("deadbeef"));
        assert_eq!(g.remote.as_deref(), Some("https://x/y"));
    }

    #[test]
    fn bare_header_provides_id() {
        // No session_meta: first line is a bare {id,timestamp} header.
        let s = parse_str("", &fixture("bare_header_provides_id.jsonl"), true, None).unwrap();
        assert_eq!(s.id, "hdr-1");
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn event_msg_dedups_response_item_text() {
        // When event_msgs are present, NL text comes from them and the response_item message dup is
        // skipped — so we expect exactly one user + one assistant text message.
        let s = parse_fixture("event_msg_dedups_response_item_text.jsonl");
        let texts: Vec<_> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, vec!["hello", "hi there"]);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
    }

    #[test]
    fn reasoning_prefers_content_keeps_summary_and_encrypted() {
        let s = parse_fixture("reasoning_prefers_content_keeps_summary_and_encrypted.jsonl");
        let m = &s.messages[0];
        match &m.content[0] {
            Block::Thinking { text, encrypted, .. } => {
                assert_eq!(text.inline_str(), Some("raw thoughts"));
                assert_eq!(encrypted.as_deref(), Some("ENC"));
            }
            other => panic!("expected thinking, got {other:?}"),
        }
        assert_eq!(
            m.extra["codex"].get("reasoning_summary").and_then(Value::as_str),
            Some("short summary")
        );
    }

    #[test]
    fn function_call_and_output_roundtrip_with_error() {
        let s = parse_fixture("function_call_and_output_roundtrip_with_error.jsonl");
        match first_block(&s, |b| matches!(b, Block::ToolUse { .. })) {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(input.resolve().unwrap()["command"][0], "ls");
            }
            _ => unreachable!(),
        }
        match first_block(&s, |b| matches!(b, Block::ToolResult { .. })) {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "c1");
                assert_eq!(content.inline_str(), Some("boom"));
                assert!(is_error);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn custom_tool_call_string_input() {
        let s = parse_fixture("custom_tool_call_string_input.jsonl");
        match &s.messages[0].content[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "apply_patch");
                assert_eq!(
                    input.resolve().unwrap().as_ref(),
                    &Value::String("*** Begin Patch".into())
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            s.messages[0].extra["codex"].get("status").and_then(Value::as_str),
            Some("completed")
        );
    }

    #[test]
    fn local_shell_call_maps_to_tool_use() {
        let s = parse_fixture("local_shell_call_maps_to_tool_use.jsonl");
        match &s.messages[0].content[0] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "ls1");
                assert_eq!(name, "local_shell");
                assert_eq!(input.resolve().unwrap()["action"]["command"][1], "hi");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn web_search_call_surfaces_query() {
        let s = parse_fixture("web_search_call_surfaces_query.jsonl");
        match &s.messages[0].content[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "web_search");
                assert_eq!(input.resolve().unwrap()["query"], "rust serde");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_search_call_and_output() {
        let s = parse_fixture("tool_search_call_and_output.jsonl");
        assert!(matches!(&s.messages[0].content[0], Block::ToolUse { name, .. } if name == "tool_search"));
        assert!(
            matches!(&s.messages[1].content[0], Block::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content.matches_str("[]") == Some(true))
        );
    }

    #[test]
    fn user_message_with_input_image() {
        let s = parse_fixture("user_message_with_input_image.jsonl");
        let m = &s.messages[0];
        assert!(matches!(&m.content[0], Block::Text { text } if text.matches_str("see this") == Some(true)));
        match &m.content[1] {
            Block::Image { media_type, data_ref } => {
                assert_eq!(media_type.as_deref(), Some("image/png"));
                assert_eq!(data_ref.as_deref(), Some("base64:inline"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn view_image_event_becomes_image_block() {
        let s = parse_fixture("view_image_event_becomes_image_block.jsonl");
        match &s.messages[0].content[0] {
            Block::Image { data_ref, .. } => assert_eq!(data_ref.as_deref(), Some("/tmp/x.png")),
            _ => unreachable!(),
        }
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn giant_fco_string_output_spans_and_resolves() {
        use std::io::Write;
        // A function_call_output whose string output is > INLINE_MAX and contains escapes (\n, ").
        let body = "out line \"q\"\nmore ".repeat(400); // ~7 KB, with \n and " escapes
        let rec = serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "response_item",
            "payload": { "type": "function_call_output", "call_id": "c1", "output": body }
        })
        .to_string();
        let dir = cv_testkit::temp_dir("codex-span");
        let path = dir.join("rollout-x.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{rec}").unwrap();
        }
        let data = std::fs::read(&path).unwrap();
        let s = whole(
            &SessionRef::new("fallback", Harness::Codex, &path),
            &stream::Demand::LEAN.with_spans(),
        );

        let content = s
            .messages
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    Block::ToolResult { content, .. } => Some(content),
                    _ => None,
                })
            })
            .expect("tool result present");
        assert!(content.is_lazy(), "giant fco output should be a span");
        let resolver = s.resolver();
        assert_eq!(
            content.try_resolve(&resolver).unwrap(),
            body.as_str(),
            "span must resolve to the exact output"
        );

        // And it matches the inline (bulk) parse of the same record.
        let inline = parse_str("fallback", &String::from_utf8(data).unwrap(), true, Some(path.clone())).unwrap();
        let inline_c = inline
            .messages
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    Block::ToolResult { content, .. } => content.inline_str(),
                    _ => None,
                })
            })
            .expect("inline tool result");
        assert_eq!(inline_c, body.as_str());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "mmap")]
    fn assert_giant_modes_match(text: &str) {
        let dir = cv_testkit::temp_dir("codex-giant-parity");
        let path = dir.join("rollout-parity.jsonl");
        std::fs::write(&path, text).unwrap();
        let mut lazy = whole(
            &SessionRef::new("fallback", Harness::Codex, &path),
            &stream::Demand::LEAN.with_spans(),
        );
        lazy.materialize().unwrap();
        let full = parse_str("fallback", text, true, Some(path)).unwrap();
        assert_eq!(serde_json::to_value(lazy).unwrap(), serde_json::to_value(full).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn giant_output_preserves_metadata_and_inherited_filtering() {
        for kind in ["function_call_output", "custom_tool_call_output"] {
            for ordinal in [2, 12] {
                let meta = serde_json::json!({
                    "type": "session_meta", "payload": {
                        "id": "child", "subagent_history_start_ordinal": 10,
                    }
                });
                let result = serde_json::json!({
                    "type": "response_item", "ordinal": ordinal,
                    "timestamp": "2026-01-02T00:00:00Z",
                    "payload": {
                        "type": kind, "call_id": "call", "output": "result\n".repeat(1000),
                        "name": "wait", "namespace": "collaboration",
                        "internal_chat_message_metadata_passthrough": {
                            "turn_id": "turn", "create_time": 1767225600.0,
                        },
                    }
                });
                assert_giant_modes_match(&format!("{meta}\n{result}\n"));
            }
        }
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn giant_output_strips_json_escaped_error_prefix() {
        let result = serde_json::json!({
            "type": "response_item", "payload": {
                "type": "function_call_output", "call_id": "call",
                "output": format!("[error] {}", "failure\n".repeat(1000)),
            }
        });
        let text = result.to_string().replace("[error] ", r"\u005berror\u005d\u0020");
        assert_giant_modes_match(&format!("{text}\n"));
    }

    /// The span path under offset-stamping marks every message with the byte offset of the record
    /// that created it — including an assistant a later `token_count` amends, the stale-snapshot
    /// carrier, and a giant span output.
    #[cfg(feature = "mmap")]
    #[test]
    fn offset_stamps_mark_each_message_creating_record() {
        let big = "giant tool output line\n".repeat(400); // > INLINE_MAX → a Span
        let lines = [
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"seek-1","cwd":"/work"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#.to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:06Z","type":"response_item",
                "payload":{"type":"function_call_output","call_id":"c1","output":big}})
            .to_string(),
            r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":9.0}}}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:08Z","type":"event_msg","payload":{"type":"agent_message","message":"bye"}}"#.to_string(),
        ];
        let data = format!("{}\n", lines.join("\n")).into_bytes();
        let dir = cv_testkit::temp_dir("codex-offsets");
        let path = dir.join("rollout-offsets.jsonl");
        fs::write(&path, &data).unwrap();
        let s = whole(
            &SessionRef::new("fb", Harness::Codex, &path),
            &stream::Demand::LEAN.with_spans().with_offsets(),
        );
        let full = s.messages.clone();
        assert_eq!(s.model.as_deref(), Some("gpt-test"));
        // user, assistant(+usage), tool-use, giant span result, carrier, trailing assistant.
        assert_eq!(full.len(), 6);
        assert!(full[1].usage.is_some(), "token_count attached to held assistant");
        assert!(full[3]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content.is_lazy())));
        let offs: Vec<u64> = full
            .iter()
            .map(|m| {
                m.extra
                    .get(crate::offsets::OFFSET_KEY)
                    .and_then(Value::as_u64)
                    .expect("every message stamped")
            })
            .collect();
        let line_starts: Vec<u64> = std::iter::once(0)
            .chain(
                data.iter()
                    .enumerate()
                    .filter(|(_, &b)| b == b'\n')
                    .map(|(i, _)| i as u64 + 1),
            )
            .collect();
        // user, assistant, function_call, its output, carrier, assistant: lines 2, 3, 5, 6, 7, 8.
        let want: Vec<u64> = [2, 3, 5, 6, 7, 8].iter().map(|&l| line_starts[l]).collect();
        assert_eq!(offs, want);
    }

    #[test]
    fn token_count_attaches_usage_to_assistant() {
        let s = parse_fixture("token_count_attaches_usage_to_assistant.jsonl");
        let m = s.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        let u = m.usage.as_ref().expect("usage attached");
        assert_eq!(
            u.input_tokens,
            Some(90),
            "Codex's 100 includes the 10 cached; the IR's is disjoint"
        );
        assert_eq!(u.output_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(10));
        assert!(m.extra["codex"].get("rate_limits").is_some());
        assert!(m.extra["codex"].get("model_context_window").is_some());
    }

    #[test]
    fn a_reemitted_token_count_counts_no_second_call() {
        // Codex re-emits its last snapshot (same `total_token_usage`) after the next user message;
        // before, that stale `last_token_usage` landed on the following reply as a second call.
        // The third snapshot's running total moved, so it is a real call and keeps its usage.
        let tc = |total: u64, last: u64| {
            format!(
                r#"{{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"output_tokens":0,"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{last},"output_tokens":0,"total_tokens":{last}}}}}}}}}"#
            )
        };
        let reply = |t: &str| {
            format!(
                r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{{"type":"agent_message","message":"{t}"}}}}"#
            )
        };
        let user = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"again"}}"#;
        let lines = [
            reply("one"),
            tc(100, 100),
            user.to_string(),
            reply("two"),
            tc(100, 100),
            tc(250, 150),
        ];
        let s = parse_str("s", &lines.join("\n"), true, None).unwrap();
        let usages: Vec<Option<u64>> = s
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.usage.as_ref().and_then(|u| u.input_tokens))
            .collect();
        assert_eq!(
            usages.iter().flatten().sum::<u64>(),
            250,
            "the thread's running total, counted once: {usages:?}"
        );
        assert_eq!(usages.iter().flatten().count(), 2, "{usages:?}");
    }

    #[test]
    fn stream_attaches_usage_like_parse() {
        // Task-4 regression guard: the streaming path holds back the trailing assistant message so
        // a `token_count` event can attach usage to it — and must agree with `parse_str` exactly,
        // including the carrier message for a stale snapshot and the EOF flush of a held message.
        // Lines 4-6: a tool exchange, then a token_count that trails the *tool result* (the
        // re-emitted snapshot codex writes after non-assistant records) — a carrier on both paths.
        // Line 7: trailing assistant message with no token_count after it: held, then flushed at EOF.
        let lines = fixture_lines("stream_attaches_usage_like_parse.jsonl");
        let text = lines.join("\n");
        let parsed = parse_str("s", &text, true, None).unwrap();

        let streamed = stream_text("s", &text, &stream::Demand::RICH);

        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&streamed).unwrap(),
            "parse and stream must produce identical sessions"
        );
        // The first turn's usage attached to the assistant message it trails — on both paths.
        for s in [&parsed, &streamed] {
            let m = &s.messages[1];
            assert_eq!(m.role, Role::Assistant);
            let u = m.usage.as_ref().expect("usage attached");
            assert_eq!(
                (u.input_tokens, u.output_tokens, u.cache_read_tokens),
                (Some(90), Some(20), Some(10))
            );
        }
        let usage_notice = parsed
            .messages
            .iter()
            .find(|m| bag_get(m, "codex_event").and_then(Value::as_str) == Some("token_count"))
            .unwrap();
        assert_eq!(
            (usage_notice.kind, usage_notice.origin),
            (MessageKind::Notice, Origin::Harness)
        );
        assert!(usage_notice.content.is_empty());
        // function_call ToolUse msg attached nothing (it has its own pending usage slot untouched);
        // the stale snapshot became a carrier; the trailing assistant survived the EOF flush.
        assert_eq!(parsed.messages.last().unwrap().text().as_deref(), Some("bye"));
    }

    #[test]
    fn event_detector_modern_old_and_bounded() {
        let resp_user = |text: &str| {
            format!(
                r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
            )
        };
        let event_user = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#;
        let filler =
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#;

        // Modern shape: preamble response_items, the real user response_item, then its event_msg
        // echo a few records later (codex writes the response_item FIRST) → has_events.
        let mut modern = vec![resp_user("<environment_context>…"), resp_user("real question")];
        modern.extend(std::iter::repeat_n(filler.to_string(), 5));
        modern.push(event_user.to_string());
        let detect = |lines: &[String]| {
            let mut det = EventDetector::default();
            for line in lines {
                let v: Value = serde_json::from_str(line).unwrap();
                if det.feed(nl_flags((&v).into()).unwrap()) == Flow::Stop {
                    break;
                }
            }
            det.found
        };
        assert!(detect(&modern));

        // Old format: NL response_items, no event echo ever → not has_events.
        assert!(!detect(&[resp_user("q"), resp_user("a")]));

        // The pass is bounded: it commits to "old format" LOOKAHEAD records past the first NL
        // response_item instead of scanning to EOF (the old whole-file pre-pass read a
        // multi-hundred-MB rollout twice). Verified by feeding records by hand and watching for
        // the Stop verdict.
        let mut det = EventDetector::default();
        let resp: Value = serde_json::from_str(&resp_user("q")).unwrap();
        let resp = nl_flags((&resp).into()).unwrap();
        assert_eq!(det.feed(resp), Flow::Continue);
        let fill: Value = serde_json::from_str(filler).unwrap();
        let fill = nl_flags((&fill).into()).unwrap();
        let mut fed = 0u32;
        loop {
            fed += 1;
            assert!(
                fed <= EventDetector::LOOKAHEAD + 1,
                "detector must stop within the window"
            );
            if det.feed(fill) == Flow::Stop {
                break;
            }
        }
        assert!(!det.found);

        // …and an event_msg inside the window still wins.
        let mut det = EventDetector::default();
        det.feed(resp);
        for _ in 0..EventDetector::LOOKAHEAD {
            assert_eq!(det.feed(fill), Flow::Continue);
        }
        let ev: Value = serde_json::from_str(event_user).unwrap();
        assert_eq!(det.feed(nl_flags((&ev).into()).unwrap()), Flow::Stop);
        assert!(det.found);
    }

    #[test]
    fn corrupt_lines_are_counted_identically_on_both_paths() {
        // A live/damaged rollout: one corrupt line amid good records. Both paths must (a) keep the
        // good records, (b) surface the same `extra["cv"]["skipped_lines"]` count (cv's own parse
        // diagnostic, not a harness fact — see `ir::CV_NAMESPACE`), and (c) stay
        // byte-identical to each other. Clean files get NO `cv` bag (see the other tests'
        // sessions, which assert exact JSON equality without it).
        // Line 2 is truncated mid-write.
        let lines = fixture_lines("corrupt_lines_are_counted_identically_on_both_paths.jsonl");
        let text = lines.join("\n");
        let parsed = parse_str("s", &text, true, None).unwrap();
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(
            parsed
                .extra
                .get("cv")
                .and_then(|v| v.get("skipped_lines"))
                .and_then(Value::as_u64),
            Some(1),
            "the corrupt line is tolerated but counted"
        );
        assert!(
            !parsed.extra.contains_key("skipped_lines"),
            "the diagnostic is namespaced, never flat"
        );

        let streamed = stream_text("s", &text, &stream::Demand::RICH);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&streamed).unwrap(),
            "skip accounting must not diverge parse from stream"
        );
    }

    #[test]
    fn compacted_record_is_a_boundary_plus_a_summary_when_readable() {
        // Legacy shape: a human-readable `message` — the boundary, then the summary that seeds the
        // next window.
        let s = parse_fixture("compacted_record_is_a_boundary_plus_a_summary_when_readable.jsonl");
        let kinds: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::System, MessageKind::CompactionSummary, Origin::Harness),
            ]
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("[history compacted]"));
        assert_eq!(s.messages[0].extra["codex"]["replacement_history_len"], 1);
        assert_eq!(s.messages[0].extra["codex"]["codex_event"], "compacted");
        assert_eq!(s.messages[1].text().as_deref(), Some("summary text"));

        // 0.147+ shape: `message` is "" and the replacement is an encrypted `compaction` item —
        // nothing readable seeds the window, so only the boundary is emitted.
        let s = parse_fixture("compacted_record_is_a_boundary_plus_a_summary_when_readable-2.jsonl");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].kind, MessageKind::CompactionBoundary);
        assert_eq!(s.messages[0].extra["codex"]["replacement_history_len"], 2);
        assert_eq!(s.messages[0].extra["codex"]["window_number"], 1);

        // The inline `response_item compaction` (mid-history) is a boundary too.
        let s = parse_fixture("compacted_record_is_a_boundary_plus_a_summary_when_readable-3.jsonl");
        assert_eq!(s.messages[0].kind, MessageKind::CompactionBoundary);
        assert_eq!(s.messages[0].extra["codex"]["encrypted"], true);
    }

    #[test]
    fn turn_context_records_model_change() {
        let s = parse_fixture("turn_context_records_model_change.jsonl");
        assert_eq!(s.model.as_deref(), Some("gpt-5.3-codex-high"));
        let note = s.messages.iter().find(|m| m.role == Role::System).unwrap();
        assert_eq!(note.kind, MessageKind::ModelChange);
        assert_eq!(note.origin, Origin::Harness);
        assert_eq!(
            note.text().as_deref(),
            Some("[model changed: gpt-5.3-codex → gpt-5.3-codex-high]")
        );
        assert_eq!(
            note.model.as_deref(),
            Some("gpt-5.3-codex-high"),
            "the new model is first-class on the note"
        );
        assert!(bag_get(note, "model").is_none(), "no bag copy of a first-class field");
        assert_eq!(note.extra["codex"]["codex_event"], "turn_context");
    }

    #[test]
    fn turn_context_effort_change_is_a_model_change_note_without_a_model() {
        let s = parse_fixture("turn_context_effort_change_is_a_model_change_note_without_a_model.jsonl");
        assert_eq!(s.extra["codex"]["reasoning_effort"], "xhigh");
        assert_eq!(
            s.messages.len(),
            1,
            "the first record seeds silently; the agreeing third adds nothing"
        );
        let note = &s.messages[0];
        assert_eq!(note.kind, MessageKind::ModelChange);
        assert_eq!(note.text().as_deref(), Some("[effort changed: high → xhigh]"));
        assert!(note.model.is_none(), "the model did not change");
        // Every `turn_context` fact the note carries is nested under `extra["codex"]`, never flat
        // (`parse_jsonl` also runs the whole-session check).
        assert_eq!(note.extra["codex"]["effort"], "xhigh");
        assert_eq!(note.extra["codex"]["turn_id"], "t1");
        assert_eq!(note.extra["codex"]["cwd"], "/w");
        assert_eq!(note.extra.keys().collect::<Vec<_>>(), ["codex"]);
    }

    #[test]
    fn legacy_2025_json_layout() {
        let text = fixture("legacy_2025_json_layout.json");
        let s = parse_str("fallback", &text, false, None).unwrap();
        assert_eq!(s.id, "legacy-1");
        // user text, reasoning, tool use, tool result => 4 messages (no event_msg dedup in legacy).
        assert_eq!(s.messages.len(), 4);
        assert!(s
            .messages
            .iter()
            .any(|m| matches!(m.content.first(), Some(Block::Thinking { .. }))));
        assert!(s
            .messages
            .iter()
            .any(|m| matches!(m.content.first(), Some(Block::ToolResult { content, .. }) if content.matches_str("ok") == Some(true))));
    }

    /// Developer smoke test against the real on-disk corpus (run with `--ignored`). Parses every
    /// discovered Codex session and asserts no panics + that every tool result references a tool use.
    #[test]
    #[ignore = "requires local ~/.codex corpus"]
    fn real_corpus_parses_without_panic() {
        let cx = Codex::detect();
        let refs = cx.discover().expect("discover");
        eprintln!("codex corpus: {} sessions", refs.len());
        let mut tool_uses = 0usize;
        let mut tool_results = 0usize;
        let mut images = 0usize;
        let mut thinking = 0usize;
        for r in &refs {
            let s = cx.parse(r).expect("parse");
            assert!(!s.id.is_empty());
            for m in &s.messages {
                for b in &m.content {
                    match b {
                        Block::ToolUse { .. } => tool_uses += 1,
                        Block::ToolResult { .. } => tool_results += 1,
                        Block::Image { .. } => images += 1,
                        Block::Thinking { .. } => thinking += 1,
                        _ => {}
                    }
                }
            }
        }
        eprintln!("tool_uses={tool_uses} tool_results={tool_results} images={images} thinking={thinking}");
        assert!(tool_uses > 0, "expected some tool calls in the corpus");
    }

    /// Stream a `.jsonl` transcript's lines under explicit options, collecting messages like
    /// `parse_jsonl` does.
    fn stream_jsonl_with(lines: &[impl std::borrow::Borrow<str>], opts: &stream::Demand) -> Session {
        stream_text("fallback-id", &lines.join("\n"), opts)
    }

    #[test]
    fn session_meta_swarm_fields_land_in_extra_and_first_meta_wins() {
        // The 0.154 subagent-rollout head: the child's meta (with swarm fields and an inherited-prefix
        // cutoff), the parent's meta embedded as line 1, then the parent's history up to the cutoff.
        let s = parse_fixture("session_meta_swarm_fields_land_in_extra_and_first_meta_wins.jsonl");
        assert_eq!(s.id, "child-1", "the first session_meta is canonical");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/ember/dev/minidregg")));
        assert_eq!(s.extra["codex"]["session_id"], "root-1");
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(s.lineage.forked_from.as_deref(), Some("parent-1"));
        assert_eq!(s.extra["codex"]["thread_source"], "subagent");
        assert_eq!(s.extra["codex"]["agent_nickname"], "Newton");
        assert_eq!(
            s.lineage.agent_path.as_deref(),
            Some("/root/cycle_client"),
            "the parent's /root did not overwrite it"
        );
        assert_eq!(
            s.extra["codex"]["source"]["subagent"]["thread_spawn"]["depth"], 1,
            "object-valued source kept raw"
        );
        assert_eq!(s.extra["codex"]["model_provider"], "openai");
        assert_eq!(s.extra["codex"]["history_mode"], "paginated");
        assert_eq!(s.extra["codex"]["subagent_history_start_ordinal"], 4);
        assert_eq!(s.extra["codex"]["cli_version"], "0.154.0");
        assert!(
            !s.extra["codex"].as_object().unwrap().contains_key("agent_role"),
            "null fields are not stashed"
        );
    }

    #[test]
    fn inherited_prefix_is_skipped_lean_and_tagged_complete() {
        // Ordinals 0-3 are the parent's embedded history (cutoff 4): its turn_context still seeds
        // the child's model, but its messages are not the child's turns.
        let lines = fixture_lines("inherited_prefix_is_skipped_lean_and_tagged_complete.jsonl");
        let lean = parse_jsonl(&lines);
        assert_eq!(
            lean.model.as_deref(),
            Some("gpt-6-astra"),
            "inherited turn_context still seeds the model"
        );
        assert_eq!(lean.extra["codex"]["reasoning_effort"], "xhigh");
        let texts: Vec<_> = lean.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["child's own reply"],
            "the parent's prompt is not the child's turn"
        );
        // create_time (epoch seconds, float) beats the envelope timestamp; turn_id rides along.
        let m = &lean.messages[0];
        assert_eq!(m.timestamp.map(|t| t.timestamp()), Some(1789845000));
        assert_eq!(m.extra["codex"]["turn_id"], "t1");

        let complete = stream_jsonl_with(&lines, &stream::Demand::COMPLETE);
        let texts: Vec<_> = complete.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, vec!["parent's original prompt", "child's own reply"]);
        assert_eq!(complete.messages[0].extra["codex"]["inherited"], true);
        assert_eq!(
            complete.messages[0].timestamp.map(|t| t.timestamp()),
            Some(1789840968),
            "the fork-time envelope stamp is not used"
        );
        assert!(bag_get(&complete.messages[1], "inherited").is_none());

        // parse == stream for the lean pass too
        let streamed = stream_jsonl_with(&lines, &stream::Demand::RICH);
        assert_eq!(
            serde_json::to_value(&lean).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn agent_message_pairs_with_preceding_metadata_and_attributes_by_author() {
        let lines = fixture_lines("agent_message_pairs_with_preceding_metadata_and_attributes_by_author.jsonl");
        let s = parse_jsonl(&lines);
        let inbound = &s.messages[0];
        assert_eq!(inbound.role, Role::User, "addressed to this thread ⇒ input");
        assert_eq!(
            (inbound.kind, inbound.origin),
            (MessageKind::Prompt, Origin::Subagent),
            "a task from another agent is a prompt, but not a human's"
        );
        assert!(inbound.text().unwrap().starts_with("Message Type: NEW_TASK"));
        assert_eq!(inbound.extra["codex"]["codex_event"], "agent_message");
        assert_eq!(inbound.extra["codex"]["author"], "/root");
        assert_eq!(inbound.extra["codex"]["recipient"], "/root/cycle_client");
        assert_eq!(inbound.extra["codex"]["encrypted"], true);
        assert_eq!(
            inbound.extra["codex"]["trigger_turn"], true,
            "the metadata line that preceded it"
        );
        // namespace kept on the call and its output; name stays bare
        let call = &s.messages[1];
        assert!(matches!(&call.content[0], Block::ToolUse { name, .. } if name == "send_message"));
        assert!(call
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { namespace: Some(ns), .. } if ns == "collaboration")));
        let out = &s.messages[2];
        assert!(
            matches!(&out.content[0], Block::ToolResult { tool_name, .. } if tool_name.as_deref() == Some("send_message"))
        );
        assert_eq!(out.extra["codex"]["namespace"], "collaboration");
        assert_eq!((out.kind, out.origin), (MessageKind::ToolResult, Origin::Harness));
        let outbound = &s.messages[3];
        assert_eq!(outbound.role, Role::Assistant, "authored by this thread ⇒ output");
        assert_eq!((outbound.kind, outbound.origin), (MessageKind::Reply, Origin::Subagent));
        assert_eq!(outbound.extra["codex"]["trigger_turn"], false);
        assert!(outbound.extra["codex"].get("encrypted").is_none());
        // identical on the streaming path (the pairing lives in parser state, not the sink)
        let streamed = stream_jsonl_with(&lines, &stream::Demand::RICH);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn item_completed_items_become_system_notes_except_twins() {
        let lines = fixture_lines("item_completed_items_become_system_notes_except_twins.jsonl");
        let s = parse_jsonl(&lines);
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "$ /bin/zsh -lc cargo test → failed (exit 101)",
                "[file change completed] /Users/ember/dev/x/src/lib.rs",
                "[sub-agent /root/cycle_client/helper started]",
                "[sub-agent /root/cycle_client/helper completed]",
                "[viewed image: file:///Users/ember/shot.png]",
                "[mcp node_repl.js] failed: {\"message\":\"boom\"}",
                "[web search] rust zstd crate · ruzstd docs",
                "1. read\n2. fix",
            ],
            "Reasoning/AgentMessage twins and clock.sleep emit nothing"
        );
        assert!(s
            .messages
            .iter()
            .all(|m| m.role == Role::System && m.origin == Origin::Harness));
        // Sub-agent lifecycle is first-class; every other item is a harness notice.
        let kinds: Vec<_> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::SubagentSpawn,
                MessageKind::SubagentReturn,
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::Notice,
            ]
        );
        assert_eq!(s.messages[2].extra["codex"]["agent_thread_id"], "grand-1");
        assert_eq!(s.messages[2].extra["codex"]["agent_path"], "/root/cycle_client/helper");
        assert_eq!(s.messages[3].extra["codex"]["item"]["kind"], "completed");
        assert!(
            s.messages
                .iter()
                .all(|m| !m.content.iter().any(|b| matches!(b, Block::Image { .. }))),
            "the viewed image is not duplicated as an Image block (the tool output already carries it)"
        );
        let exec = &s.messages[0];
        assert_eq!(exec.extra["codex"]["codex_event"], "item_completed");
        assert_eq!(exec.extra["codex"]["item_type"], "CommandExecution");
        assert_eq!(exec.extra["codex"]["item"]["exit_code"], 101);
        assert_eq!(exec.extra["codex"]["item"]["stdout"], "error: test failed");
        assert_eq!(exec.extra["codex"]["turn_id"], "t1");
        assert_eq!(exec.extra["codex"]["started_at_ms"], 1789841584000u64);
        assert_eq!(
            exec.timestamp.map(|t| t.timestamp_millis()),
            Some(1789841587000),
            "completed_at_ms is the note's time"
        );
        assert_eq!(s.messages[7].extra["codex"]["codex_event"], "plan");
    }

    #[test]
    fn token_usage_record_is_authoritative_and_its_token_count_merges() {
        let lines = fixture_lines("token_usage_record_is_authoritative_and_its_token_count_merges.jsonl");
        let s = parse_jsonl(&lines);
        assert_eq!(
            s.messages.len(),
            2,
            "no carrier: the token_count merged into the record-sourced usage"
        );
        let m = &s.messages[0];
        let u = m.usage.as_ref().expect("usage from the record");
        assert_eq!(
            (
                u.input_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
                u.output_tokens
            ),
            // 35507 on disk includes both cache counts: 35507 - 34432 - 512 = 563 uncached.
            (Some(563), Some(34432), Some(512), Some(55))
        );
        assert_eq!(
            u.reasoning_tokens,
            Some(7),
            "reasoning_output_tokens → reasoning_tokens"
        );
        assert_eq!(u.cost_usd, None, "Codex stores no cost");
        assert_eq!(m.extra["codex"]["usage_source"], "token_usage_record");
        assert_eq!(m.extra["codex"]["response_id"], "resp_1");
        assert_eq!(m.extra["codex"]["turn_id"], "t1");
        assert_eq!(m.extra["codex"]["thread_token_usage"]["input_tokens"], 70000);
        assert!(
            m.extra["codex"].get("rate_limits").is_some(),
            "token_count's snapshot merged in"
        );
        assert_eq!(m.extra["codex"]["model_context_window"], 258400);
        // and the streaming path (which holds the message across the two records) agrees
        let streamed = stream_jsonl_with(&lines, &stream::Demand::RICH);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn thread_settings_applied_aborts_and_rollbacks_are_noted() {
        let lines = fixture_lines("thread_settings_applied_aborts_and_rollbacks_are_noted.jsonl");
        let s = parse_jsonl(&lines);
        assert_eq!(s.model.as_deref(), Some("gpt-6-vega"));
        assert_eq!(s.extra["codex"]["reasoning_effort"], "xhigh");
        assert_eq!(s.extra["codex"]["thread_settings"]["model_provider_id"], "openai");
        assert_eq!(s.extra["codex"]["thread_settings"]["personality"], "pragmatic");
        assert_eq!(s.extra["codex"]["last_agent_message"], "all done here");
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "[model changed: gpt-6-astra → gpt-6-vega; effort changed: high → xhigh]",
                "[turn aborted: interrupted]",
                "[rolled back 2 turns]"
            ],
            "the settings event notes the switch once; the agreeing turn_context adds nothing"
        );
        let kinds: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::ModelChange, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Branch, Origin::Harness),
            ]
        );
        assert_eq!(s.messages[0].model.as_deref(), Some("gpt-6-vega"));
        assert_eq!(s.messages[0].extra["codex"]["codex_event"], "thread_settings_applied");
        assert_eq!(s.messages[1].extra["codex"]["duration_ms"], 10696120u64);
        assert_eq!(s.messages[2].extra["codex"]["num_turns"], 2);
    }

    /// `(content, is_error)` of every tool result in `s`.
    fn results(s: &Session) -> Vec<(String, bool)> {
        s.messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult { content, is_error, .. } => Some((content.to_string(), *is_error)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn error_prefix_marks_tool_failure_and_is_stripped() {
        // In a rollout cv wrote, `[error] ` is cv's sentinel: the failure bit, stripped.
        let meta = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"x","originator":"clustervision"}}"#;
        let mut lines = vec![meta.to_string()];
        lines.extend(fixture_lines("error_prefix_marks_tool_failure_and_is_stripped.jsonl"));
        let expected = |plain: bool| {
            let p = |t: &str| if plain { format!("[error] {t}") } else { t.to_string() };
            vec![
                (p("command not found: frobnicate"), !plain),
                (p("is only a marker at the very start"), !plain),
                ("[error] inside an array is plain text".to_string(), false),
            ]
        };
        assert_eq!(results(&parse_jsonl(&lines)), expected(false));
        // Anywhere else it is the tool's own text: no provenance, or another writer's.
        assert_eq!(
            results(&parse_fixture("error_prefix_marks_tool_failure_and_is_stripped.jsonl")),
            expected(true)
        );
        lines[0] = meta.replace("clustervision", "codex_cli_rs");
        assert_eq!(results(&parse_jsonl(&lines)), expected(true));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn error_prefix_survives_the_giant_span_path() {
        use std::io::Write;
        let body = format!("[error] {}", "boom line\n".repeat(600)); // > INLINE_MAX
        let meta = serde_json::json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta",
            "payload":{"id":"x","originator":CV_ORIGINATOR}});
        let output = serde_json::json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item",
            "payload":{"type":"function_call_output","call_id":"c1","output":body}});
        let line = format!("{meta}\n{output}");
        let dir = cv_testkit::temp_dir("codex-err");
        let path = dir.join("rollout-2026-01-01T00-00-00-err.jsonl");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let s = whole(
            &SessionRef::new("x", Harness::Codex, &path),
            &stream::Demand::LEAN.with_spans(),
        );
        let m = &s.messages[0];
        let Block::ToolResult { content, is_error, .. } = &m.content[0] else {
            panic!()
        };
        assert!(is_error);
        let resolved = content.try_resolve(&s.resolver()).unwrap().into_owned();
        assert!(
            resolved.starts_with("boom line\n"),
            "prefix stripped from the span: {:?}",
            &resolved[..20]
        );
        assert!(!resolved.contains("[error]"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(feature = "zstd"))]
    #[test]
    fn zst_without_decoder_is_an_error_not_jsonl() {
        let world = crate::test_world("codex-no-decoder");
        let path = world.base.join("rollout-no-decoder.jsonl.zst");
        std::fs::write(&path, b"\x28\xb5\x2f\xfd").unwrap();
        let error = match Rollout::open(&path) {
            Ok(_) => panic!("compressed input must require a decoder"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("zstd"));
        let found = Codex {
            roots: vec![world.base.clone()],
        }
        .discover()
        .unwrap();
        assert!(found.refs.is_empty());
        assert_eq!(found.skipped.len(), 1);
        assert_eq!(found.skipped[0].path, path);
        assert!(found.skipped[0].why.contains("zstd"));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_compressed_rollouts_are_discovered_scanned_and_parsed() {
        let _world = crate::test_world("codex-zstd");
        let lines = fixture_lines("zstd_compressed_rollouts_are_discovered_scanned_and_parsed.jsonl");
        let raw = lines.join("\n") + "\n";
        let zst = zstd::encode_all(raw.as_bytes(), 1).unwrap();
        let root = cv_testkit::temp_dir("codex-zst");
        let day = root.join("sessions").join("2026").join("08").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-2026-08-01T10-00-00-cold-1.jsonl.zst");
        std::fs::write(&path, &zst).unwrap();
        std::fs::write(day.join("rollout-2026-08-01T10-00-00-cold-1.jsonl.tmp"), b"{}").unwrap();

        let cx = Codex {
            roots: vec![root.join("sessions")],
        };
        let refs = cx.discover().unwrap();
        assert_eq!(refs.len(), 1, "the .zst is listed, the .tmp staging file is not");
        let r = &refs[0];
        assert_eq!(r.id, "cold-1");
        assert_eq!(r.title.as_deref(), Some("cold storage question"));
        assert_eq!(r.message_count, None);

        let s = cx.parse(r).unwrap();
        assert_eq!(s.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(
            s.messages.iter().filter_map(|m| m.text()).collect::<Vec<_>>(),
            vec!["cold storage question", "cold storage answer"]
        );
        // Resolved lazy streaming matches the full compressed parse.
        let mut sink = crate::stream::CollectSink::default();
        let mut st = cx.stream(r, &stream::Demand::LEAN.with_spans(), &mut sink).unwrap();
        st.messages = sink.messages;
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::to_value(&st).unwrap());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Discovery under a handoff hands each rollout it decoded over once, never a plain one; a
    /// read of the same unchanged source on that thread uses those bytes, a changed one decodes.
    #[cfg(feature = "zstd")]
    #[test]
    fn discovery_hands_decoded_rollouts_to_the_reads_of_the_same_source() {
        let _world = crate::test_world("codex-handoff");
        let raw = fixture_lines("zstd_compressed_rollouts_are_discovered_scanned_and_parsed.jsonl").join("\n") + "\n";
        let root = cv_testkit::temp_dir("codex-handoff");
        let day = root.join("sessions").join("2026").join("08").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let cold = day.join("rollout-2026-08-01T10-00-00-cold-1.jsonl.zst");
        std::fs::write(&cold, zstd::encode_all(raw.as_bytes(), 1).unwrap()).unwrap();
        std::fs::write(
            day.join("rollout-2026-08-01T11-00-00-warm-2.jsonl"),
            raw.replace("cold-1", "warm-2"),
        )
        .unwrap();
        let cx = Codex {
            roots: vec![root.join("sessions")],
        };
        let (handoff, decoded) = crate::codex_handoff::channel(1, 1);
        let refs = crate::codex_handoff::within(&Some(handoff), || cx.discover().unwrap());
        assert_eq!(refs.len(), 2);
        let (r, bytes) = decoded.recv().expect("the compressed rollout is handed over");
        assert!(decoded.recv().is_none(), "once, and never the plain rollout");
        assert_eq!((r.path.as_path(), r.message_count), (cold.as_path(), None));

        let arena = |rollout: Rollout| rollout.input;
        let handed = bytes.run(|| arena(Rollout::open(&cold).unwrap()));
        assert!(Arc::ptr_eq(
            &handed,
            &bytes.run(|| arena(Rollout::open(&cold).unwrap()))
        ));
        assert!(
            !Arc::ptr_eq(&handed, &arena(Rollout::open(&cold).unwrap())),
            "only inside run"
        );
        let read = |opts: &stream::Demand| {
            let mut sink = crate::stream::CollectSink::default();
            let mut session = cx.stream(&r, opts, &mut sink).unwrap();
            session.messages = sink.messages;
            serde_json::to_value(session).unwrap()
        };
        assert_eq!(bytes.run(|| read(&stream::Demand::RICH)), read(&stream::Demand::RICH));

        std::fs::write(
            &cold,
            zstd::encode_all(raw.replace("answer", "reply").as_bytes(), 1).unwrap(),
        )
        .unwrap();
        assert!(
            !Arc::ptr_eq(&handed, &bytes.run(|| arena(Rollout::open(&cold).unwrap()))),
            "a changed source decodes"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_titles_a_subagent_by_its_own_task_not_the_parents_prompt() {
        let lines = fixture_lines("scan_titles_a_subagent_by_its_own_task_not_the_parents_prompt.jsonl");
        let dir = cv_testkit::temp_dir("codex-scan");
        let path = dir.join("rollout-2026-09-19T14-12-43-child-1.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let r = scan(&path).unwrap();
        assert_eq!(r.id, "child-1");
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/ember/dev/minidregg")));
        // (`truncate` folds the newline, as it does for every listing title)
        assert_eq!(
            r.title.as_deref(),
            Some("Message Type: NEW_TASK Task name: build the client")
        );
        assert_eq!(r.message_count, None, "discovery defers exact turn counts");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_grown_rollout_folds_only_its_appended_records() {
        let _world = crate::test_world("codex-listing-continuation");
        let lines = fixture_lines("scan_titles_a_subagent_by_its_own_task_not_the_parents_prompt.jsonl");
        let dir = cv_testkit::temp_dir("codex-continuation");
        let path = dir.join("rollout-2026-09-19T14-12-43-child-1.jsonl");
        let cut = lines.len() / 2;
        let head = lines[..cut].join("\n") + "\n";
        // The first pass ends inside a record: that line is read again, never trusted.
        let torn = &lines[cut][..lines[cut].len() / 2];
        std::fs::write(&path, format!("{head}{torn}")).unwrap();
        let folded = || FOLDED.with(|f| std::mem::take(&mut *f.borrow_mut()));
        let first = cached_metadata(&path).unwrap().unwrap();
        assert_eq!(folded(), [(0, (head.len() + torn.len()) as u64)]);
        assert_eq!(cached_metadata(&path).unwrap(), Some(first), "unchanged: a hit");
        assert!(folded().is_empty());
        let whole = lines.join("\n") + "\n";
        std::fs::write(&path, &whole).unwrap();
        let grown = cached_metadata(&path).unwrap().unwrap();
        assert_eq!(
            folded(),
            [(head.len() as u64, (whole.len() - head.len()) as u64)],
            "only the records after the last finished line"
        );
        let fresh = scan(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&grown).unwrap(),
            serde_json::to_value(&fresh).unwrap(),
            "a resumed fold lists what a whole one does"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Developer smoke against the newest real paginated rollout and the newest subagent rollout
    /// on this machine (run with `--ignored`): parse == stream, and the 0.147+ records actually
    /// surface (item notes, usage from records, swarm messages, no inherited-prefix leakage).
    #[test]
    #[ignore = "requires local ~/.codex corpus"]
    fn real_newest_paginated_and_subagent_rollouts_smoke() {
        let root = dirs::home_dir()
            .unwrap()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09");
        let mut files: Vec<PathBuf> = WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_path_buf())
            .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".jsonl")))
            .collect();
        files.sort_by_key(|p| std::cmp::Reverse(fs::metadata(p).and_then(|m| m.modified()).ok()));
        let newest = files.first().cloned().expect("a September rollout");
        let subagent = files
            .iter()
            .find(|p| {
                fs::read_to_string(p).is_ok_and(|t| {
                    t.lines()
                        .next()
                        .is_some_and(|l| l.contains("subagent_history_start_ordinal"))
                })
            })
            .cloned()
            .expect("a subagent rollout");
        // Snapshot each file first: the newest rollout is usually a LIVE session still being
        // appended to, and parse-vs-stream must see identical bytes.
        let snap = cv_testkit::temp_dir("codex-smoke");
        for src in [newest, subagent] {
            let path = snap.join(src.file_name().unwrap());
            fs::copy(&src, &path).unwrap();
            let cx = Codex::detect();
            let r = scan(&path).unwrap();
            let parsed = cx.parse(&r).unwrap();
            let mut sink = crate::stream::CollectSink::default();
            let mut streamed = cx.stream(&r, &stream::Demand::RICH, &mut sink).unwrap();
            streamed.messages = sink.messages;
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(&streamed).unwrap(),
                "{}",
                path.display()
            );
            let notes = parsed
                .messages
                .iter()
                .filter(|m| bag_get(m, "codex_event").and_then(Value::as_str) == Some("item_completed"))
                .count();
            let agent_msgs = parsed
                .messages
                .iter()
                .filter(|m| bag_get(m, "codex_event").and_then(Value::as_str) == Some("agent_message"))
                .count();
            let with_usage = parsed.messages.iter().filter(|m| m.usage.is_some()).count();
            let from_record = parsed
                .messages
                .iter()
                .filter(|m| bag_get(m, "usage_source").is_some())
                .count();
            let carriers = parsed
                .messages
                .iter()
                .filter(|m| m.content.is_empty() && m.usage.is_some())
                .count();
            let inherit = parsed
                .harness_extra(Harness::Codex)
                .and_then(|b| b.get("subagent_history_start_ordinal"))
                .cloned();
            eprintln!(
                "{}: id={} title={:?} msgs={} scan_count={} item_notes={} agent_msgs={} usage={} (from record {}) usage_carriers={} model={:?} agent_path={:?} inherit_before={:?}",
                path.file_name().unwrap().to_string_lossy(),
                parsed.id,
                r.title,
                parsed.messages.len(),
                r.message_count.map_or("-".to_string(), |n| n.to_string()),
                notes,
                agent_msgs,
                with_usage,
                from_record,
                carriers,
                parsed.model,
                parsed.lineage.agent_path,
                inherit
            );
            assert!(from_record > 0, "paginated files carry token_usage_records");
            // A response whose only item was a tool call is followed by the call's OUTPUT before
            // its usage record, so the trailing-only rule has nothing to attach to and carries the
            // usage bare (as `token_count` always did on these files) — never more than one per
            // record, and never a lost record.
            assert!(carriers <= from_record, "at most one bare carrier per usage record");
            if inherit.is_some() {
                assert!(parsed.messages.iter().all(|m| bag_get(m, "inherited").is_none()));
            }
            // The v2 shape holds on real files: nothing flat, every kind precise.
            for m in &parsed.messages {
                assert!(
                    m.extra.keys().all(|k| k == "codex" || k == crate::ir::CARRIER_KEY),
                    "flat extra key on {}: {:?}",
                    path.display(),
                    m.extra.keys().collect::<Vec<_>>()
                );
                if m.role == Role::User && m.origin == Origin::Human {
                    assert_eq!(m.kind, MessageKind::Prompt);
                    assert!(!is_injected_text(&m.text().unwrap_or_default()));
                }
            }
        }
        fs::remove_dir_all(&snap).ok();
    }

    #[test]
    fn structured_output_with_image() {
        let s = parse_fixture("structured_output_with_image.jsonl");
        let m = &s.messages[0];
        assert!(
            matches!(&m.content[0], Block::ToolResult { content, .. } if content.matches_str("here") == Some(true))
        );
        assert!(
            matches!(&m.content[1], Block::Image { media_type, .. } if media_type.as_deref() == Some("image/jpeg"))
        );
    }

    #[test]
    fn user_prompts_vs_injected_preambles_kind_and_origin() {
        // Paginated (no NL event twins): the response_items carry the text.
        let s = parse_fixture("user_prompts_vs_injected_preambles_kind_and_origin.jsonl");
        let shape: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        let injected = (Role::User, MessageKind::InjectedContext, Origin::Harness);
        assert_eq!(
            shape,
            vec![
                (Role::System, MessageKind::InjectedContext, Origin::Harness), // developer preamble
                injected,                                                      // <environment_context>
                injected,                                                      // <recommended_plugins>
                injected,                                                      // <codex_internal_context …>
                injected,                                                      // AGENTS.md dump
                injected,                                                      // files-mentioned bundle
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[11].extra["codex"]["phase"], "final_answer");
        assert!(
            s.messages.iter().all(|m| m.model.is_none()),
            "no message repeats the session model"
        );

        // Legacy mode (NL event twins): the `user_message` event decides the same way.
        let s = parse_fixture("user_prompts_vs_injected_preambles_kind_and_origin-2.jsonl");
        let shape: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            shape,
            vec![
                injected,
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[1].extra["codex"]["kind"], "plain");
    }

    #[test]
    fn persisted_error_event_is_an_error_message() {
        let s = parse_fixture("persisted_error_event_is_an_error_message.jsonl");
        let m = &s.messages[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::System, MessageKind::Error, Origin::Harness)
        );
        assert!(m.text().unwrap().starts_with("You've hit your usage limit."));
        assert_eq!(m.extra["codex"]["codex_event"], "error");
        assert_eq!(m.extra["codex"]["error"]["codex_error_info"], "usage_limit_exceeded");
        assert!(
            m.extra["codex"]["error"].get("type").is_none(),
            "the wire `type` is the kind, not a fact"
        );
        assert!(m.extra["codex"]["error"]["message"].is_string());
    }

    #[test]
    fn session_meta_lineage_and_system_prompt() {
        let s = parse_fixture("session_meta_lineage_and_system_prompt.jsonl");
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(s.lineage.forked_from.as_deref(), Some("fork-src-1"));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("/root/reviewer"));
        assert_eq!(
            s.lineage.spawned_by_tool_use, None,
            "Codex does not record the spawning call"
        );
        assert_eq!(s.lineage.continued_in, None);
        assert_eq!(s.system_prompt.as_deref(), Some("You are Codex, a coding agent."));
        // The session bag: every listed meta fact, and nothing first-class duplicated into it.
        let bag = s.harness_extra(Harness::Codex).expect("codex bag");
        let mut keys: Vec<&str> = bag.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "agent_nickname",
                "cli_version",
                "history_base",
                "history_mode",
                "model_provider",
                "originator",
                "session_id",
                "source",
                "subagent_history_start_ordinal",
                "thread_source",
            ]
        );
        assert_eq!(bag["history_base"]["end_ordinal_exclusive"], 9);
        assert_eq!(
            s.extra.keys().collect::<Vec<_>>(),
            vec!["codex"],
            "session extra is nested only"
        );

        // Without those fields: empty lineage, no system prompt (a blank one does not count).
        let s = parse_fixture("session_meta_lineage_and_system_prompt-2.jsonl");
        assert!(s.lineage.is_empty());
        assert_eq!(s.system_prompt, None);
    }

    /// One paginated thread touching every arm: the v2 shape must hold everywhere — every message
    /// fact under `extra["codex"]`, `_record` the only other top-level key (complete mode only),
    /// precise kinds, `Message::model` unset except on the model-change note, `namespace` on the
    /// block and not in the bag.
    fn every_arm() -> Vec<String> {
        fixture_lines("every_arm.jsonl")
    }

    #[test]
    fn turn_aborted_keeps_the_0_161_error_and_0_162_root_turn() {
        let line = r#"{"timestamp":"2026-10-11T19:00:00Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t2","root_turn_id":"t1","reason":"interrupted","error":{"message":"stream closed","codex_error_info":"other"}}}"#;
        let s = parse_jsonl(&[line.to_string()]);
        let b = &s.messages[0].extra["codex"];
        assert_eq!(s.messages[0].kind, MessageKind::Notice);
        assert_eq!(b["error"]["message"], "stream closed");
        assert_eq!(b["root_turn_id"], "t1");
        assert_eq!(b["reason"], "interrupted");
    }

    #[test]
    fn every_arm_has_a_precise_kind_and_nothing_flat() {
        let s = parse_jsonl(&every_arm());
        let shape: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            shape,
            vec![
                (Role::User, MessageKind::InjectedContext, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model), // reasoning
                (Role::Assistant, MessageKind::Reply, Origin::Model), // spawn call
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::System, MessageKind::SubagentSpawn, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Subagent), // helper → /root
                (Role::Assistant, MessageKind::Reply, Origin::Model), // "fixed" (+usage)
                (Role::System, MessageKind::ModelChange, Origin::Harness),
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness), // turn_aborted
                (Role::System, MessageKind::Branch, Origin::Harness), // thread_rolled_back
                (Role::System, MessageKind::Error, Origin::Harness),
            ],
            "lean: no carriers, every kind precise"
        );
        // Nothing flat, anywhere; no `_record` in a lean pass.
        for m in &s.messages {
            let keys: Vec<&String> = m.extra.keys().collect();
            assert!(
                keys.iter().all(|k| *k == "codex"),
                "flat key(s) on {:?}: {keys:?}",
                m.kind
            );
        }
        assert_eq!(s.extra.keys().collect::<Vec<_>>(), vec!["codex"]);
        assert_eq!(s.system_prompt.as_deref(), Some("You are Codex."));
        assert_eq!(s.model.as_deref(), Some("gpt-6-vega"));
        assert_eq!(s.extra["codex"]["reasoning_effort"], "high");
        assert_eq!(s.extra["codex"]["last_agent_message"], "fixed");
        assert_eq!(s.extra["codex"]["thread_settings"]["model"], "gpt-6-vega");
        // `Message::model` only on the model-change note (the session default covers the rest).
        for m in &s.messages {
            assert_eq!(
                m.model.as_deref(),
                (m.kind == MessageKind::ModelChange).then_some("gpt-6-vega"),
                "{:?}",
                m.kind
            );
        }
        // namespace: on the block, not in the call's bag; the output keeps it in the bag (no
        // block field for it) alongside `tool_name`.
        let call = &s.messages[3];
        assert!(matches!(
            &call.content[0],
            Block::ToolUse { name, namespace: Some(ns), .. } if name == "spawn" && ns == "collaboration"
        ));
        assert!(bag_get(call, "namespace").is_none());
        assert_eq!(s.messages[4].extra["codex"]["namespace"], "collaboration");
        // Usage from the record (with the token_count snapshot merged) on the reply it trails.
        let reply = &s.messages[7];
        let u = reply.usage.as_ref().expect("usage");
        assert_eq!(
            (
                u.input_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
                u.output_tokens,
                u.reasoning_tokens
            ),
            // 100 on disk includes both cache counts: 100 - 50 - 5 = 45 uncached.
            (Some(45), Some(50), Some(5), Some(20), Some(7))
        );
        assert_eq!(reply.extra["codex"]["phase"], "final_answer");
        assert_eq!(reply.extra["codex"]["usage_source"], "token_usage_record");
        assert!(reply.extra["codex"].get("rate_limits").is_some());
        // The exact per-message bag keys, so a new flat-looking key cannot sneak in unnoticed.
        let keys = |i: usize| -> Vec<String> {
            let mut k: Vec<String> = s.messages[i].extra["codex"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            k.sort_unstable();
            k
        };
        assert_eq!(keys(1), vec!["turn_id"]);
        assert_eq!(
            keys(5),
            vec![
                "agent_path",
                "agent_thread_id",
                "codex_event",
                "completed_at_ms",
                "item",
                "item_type",
                "turn_id"
            ]
        );
        assert_eq!(keys(6), vec!["author", "codex_event", "recipient", "trigger_turn"]);
        assert_eq!(
            keys(7),
            vec![
                "model_context_window",
                "phase",
                "rate_limits",
                "response_id",
                "thread_token_usage",
                "turn_id",
                "usage_source"
            ]
        );
        assert_eq!(keys(8), vec!["codex_event"]);
        assert_eq!(keys(9), vec!["codex_event", "replacement_history_len", "window_number"]);
        assert_eq!(keys(10), vec!["codex_event", "reason", "turn_id"]);
        assert_eq!(keys(11), vec!["codex_event", "num_turns"]);
        assert_eq!(keys(12), vec!["codex_event", "error"]);
        // parse == stream
        let streamed = stream_jsonl_with(&every_arm(), &stream::Demand::RICH);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn complete_mode_carries_lifecycle_and_unknown_records_verbatim() {
        let lean = parse_jsonl(&every_arm());
        assert!(lean.messages.iter().all(|m| m.kind != MessageKind::Carrier));
        let complete = stream_jsonl_with(&every_arm(), &stream::Demand::COMPLETE);
        let carriers: Vec<(&str, &str)> = complete
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Carrier)
            .map(|m| {
                let record = &m.extra[crate::ir::CARRIER_KEY];
                (
                    m.extra["codex"]["record_type"].as_str().unwrap(),
                    record
                        .pointer("/payload/type")
                        .and_then(Value::as_str)
                        .unwrap_or(record["type"].as_str().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            carriers,
            vec![
                ("event_msg.task_started", "task_started"),
                ("world_state", "world_state"),
                ("event_msg.task_complete", "task_complete"),
            ],
            "lifecycle markers and unmodelled record types ride verbatim; represented records do not"
        );
        assert_eq!(complete.messages.len(), lean.messages.len() + carriers.len());
        for m in &complete.messages {
            let allowed = |k: &String| k == "codex" || k == crate::ir::CARRIER_KEY;
            assert!(m.extra.keys().all(allowed), "{:?}", m.extra.keys().collect::<Vec<_>>());
            assert_eq!(
                m.extra.contains_key(crate::ir::CARRIER_KEY),
                m.kind == MessageKind::Carrier,
                "`_record` marks exactly the carriers"
            );
        }
        let ws = complete
            .messages
            .iter()
            .find(|m| bag_get(m, "record_type").and_then(Value::as_str) == Some("world_state"))
            .unwrap();
        assert_eq!((ws.role, ws.origin), (Role::System, Origin::Harness));
        assert_eq!(ws.extra[crate::ir::CARRIER_KEY]["payload"]["agents_md"], "# AGENTS.md");
        assert!(ws.content.is_empty());
    }

    #[test]
    fn custom_tool_call_output_errors_and_modern_affordances_parse() {
        let lines = vec![
            r#"{"timestamp":"2026-10-18T00:00:00Z","type":"session_meta","payload":{"id":"aff-1","cwd":"/w","originator":"codex-cli","cli_version":"0.165.0","source":"cli","model_provider":"openai"}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:01Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"c_fail","name":"exec","namespace":"functions","input":"tools.exec_command('bad')"}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:02Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c_fail","output":[{"type":"input_text","text":"Script failed\nWall time 0.0 seconds\nOutput:\n"},{"type":"input_text","text":"Script error:\ntool `exec_command` expects a JSON object"}]}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:03Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"c_cell","name":"exec","namespace":"functions","input":"await new Promise(() => {})"}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c_cell","output":"Script running with cell ID 53\nWall time 10.0 seconds\nOutput:\nbuilding..."}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:05Z","type":"response_item","payload":{"type":"function_call","call_id":"c_wait","name":"wait","arguments":"{\"cell_id\":\"53\",\"yield_time_ms\":30000}"}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:06Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c_wait","output":[{"type":"input_text","text":"Script completed\nWall time 18.0 seconds\nOutput:\nok"}]}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:07Z","type":"response_item","payload":{"type":"function_call","call_id":"c_spawn","name":"spawn_agent","namespace":"collaboration","arguments":"{\"task_name\":\"verifier\",\"message\":\"run tests\"}"}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:08Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c_spawn","output":"{\"task_name\":\"/root/verifier\"}"}}"#.to_string(),
            r#"{"timestamp":"2026-10-18T00:00:09Z","type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"Plan","id":"plan-1","text":"1. audit\n2. fix"}}}"#.to_string(),
        ];
        let s = parse_jsonl(&lines);
        let Block::ToolResult { is_error, .. } = &s.messages[1].content[0] else {
            panic!("expected tool result");
        };
        assert!(*is_error, "Script failed array item must mark ToolResult as error");
        let Block::ToolResult { is_error: ok_err, .. } = &s.messages[5].content[0] else {
            panic!("expected tool result");
        };
        assert!(!*ok_err);

        let rows = crate::affordance::of_session(&s).unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.kind, r.action, r.tool.as_deref(), r.summary.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    crate::affordance::Kind::BackgroundJob,
                    "start",
                    None,
                    "Script running with cell ID 53"
                ),
                (crate::affordance::Kind::BackgroundJob, "read", Some("wait"), "53"),
                (
                    crate::affordance::Kind::SubagentSpawn,
                    "spawn",
                    Some("spawn_agent"),
                    "verifier"
                ),
                (
                    crate::affordance::Kind::Plan,
                    "write",
                    Some("update_plan"),
                    "1. audit 2. fix"
                ),
            ]
        );
    }
}
