//! Hermes (Nous Research) adapter — `~/.hermes/state.db` (SQLite, including schema v31; back-compatible to the
//! pre-v11 tables).
//!
//! See `docs/FORMATS.md`. One DB holds all sessions: a `sessions` table (metadata) and a `messages`
//! table (OpenAI-shaped rows: role, content, tool_calls JSON, reasoning, …). cwd is NOT persisted
//! (runtime-only). Multimodal content is stored with a `\x00json:` sentinel prefix.
//!
//! Historical coverage: Hermes does NOT version-gate column additions — it reconciles live tables
//! against `SCHEMA_SQL` and `ALTER TABLE ADD COLUMN`s anything missing (the Beets/sqlite-utils
//! pattern). So a database that was last opened by an older Hermes can be missing newer columns
//! entirely (`token_count`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`,
//! `codex_message_items`, `platform_message_id`, `observed`, `finish_reason`, …). We therefore probe
//! `PRAGMA table_info` and only SELECT columns that actually exist, so the parser degrades
//! gracefully instead of failing the whole session on a missing column.
//!
//! Reasoning is recorded across several provider-specific columns and we map each:
//!   * `reasoning`         — short summary text (DeepSeek/Qwen, OpenRouter summary) → Thinking.text
//!   * `reasoning_content` — provider-native scratchpad (Moonshot/Novita) → Thinking.text (appended)
//!   * `reasoning_details` — `[{type:"reasoning.summary"|"reasoning.encrypted_content"|"thinking", …}]`
//!     (OpenRouter unified) → Thinking.text (summaries) + Thinking.encrypted
//!   * `codex_reasoning_items` — `[{type:"reasoning", id, encrypted_content}]` (Codex Responses API)
//!     → Thinking.encrypted + stashed verbatim in `extra`
//!   * `codex_message_items`   — `[{type:"message", phase, content:[{type:"output_text",text}]}]`
//!     → stashed verbatim in `extra` (Codex final/commentary phases)
//!
//! Compression chains: a long conversation is split across multiple session rows linked by
//! `parent_session_id`, where the parent has `end_reason='compression'`. `parse` walks the lineage
//! root→tip and merges all messages (mirroring Hermes's own
//! `get_messages_as_conversation(include_ancestors=True)`), deduplicating the replayed first user
//! message at each compression boundary, so a resumed/compressed conversation reads as one transcript.
//! Only COMPRESSION parents are merged (`_COMPRESSION_CHILD_SQL`): a `/branch` copy
//! (`model_config._branched_from`) owns a copied transcript and stands alone, and reset
//! (`_reset_from`) / delegate sub-agent (`_delegate_from`) children are separate conversations that
//! merely record their lineage (`hermes_state_common.py:151-216`); merging them duplicated the parent
//! under every child.
//!
//! Since 2026-07 (schema v11→v30) compaction is IN PLACE under one session id
//! (`hermes_state_messages.py archive_and_compact`): the summarized rows get `active=0, compacted=1`
//! (still displayed as history), the summary is inserted as a fresh active row with
//! `_compressed_summary=1` (`display_kind='hidden'` for a standalone handoff), the carried tail is
//! column-cloned to fresh ids and its originals — like rewound rows — get `active=0, compacted=0`.
//! Hermes's display projection is `(active = 1 OR compacted = 1) ORDER BY id`
//! (`_DISPLAY_ACTIVE_CLAUSE`; "timestamps are not monotonic and would break tool-call adjacency"),
//! generation-deduped by `(role, content, timestamp, tool_call_id, tool_calls, tool_name)`. The lean
//! passes mirror that exactly; `complete` reads every row and tags `active`/`compacted` instead. A
//! summary row becomes a [`MessageKind::CompactionBoundary`] System marker + a
//! [`MessageKind::CompactionSummary`] System message linked to it by `parent_id` — the pair
//! `crate::compaction` detects for every harness.
//!
//! Kinds (`docs/INTERFACE-V2.md` §4): a `system` row is the [`MessageKind::SystemPrompt`] (the
//! resolved prompt is also `Session::system_prompt`); harness-stamped `display_kind` rows are System
//! turns typed by what they are — `model_switch` → [`MessageKind::ModelChange`],
//! `async_delegation_complete` → [`MessageKind::SubagentReturn`] (origin [`Origin::Subagent`]),
//! `hidden` / `process_complete` / `internal_notification` / `auto_continue` →
//! [`MessageKind::Notice`] — while a `steer` (a human's mid-turn message) stays a
//! [`MessageKind::Prompt`]. A foreign import (`origin_json.imported_from`) gives every message
//! [`Origin::Import`]. Lineage is first-class (`Session::lineage`: `_branched_from` → `forked_from`,
//! `_delegate_from` → `parent`, a compression rotation → `continues` / `continued_in`); everything
//! Hermes-specific rides in `extra["hermes"]` — the display sidecars (`api_content` = the verbatim
//! provider view of a user row, `display_kind`, `display_metadata`, `effect_disposition`), the
//! compaction flags, the raw reasoning columns, and the session row under
//! `extra["hermes"]["session"]`.

use crate::json::{ReadInput, Validation};
mod emit;
pub use emit::emit;

use super::store::sqlite::{open_ro, table_exists, Cells, Select};
use super::wire::openai;
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{self, Flow, MessageSink, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

/// Keys inside a message's `extra["hermes"]` bag under which the parser stashes the raw `reasoning`
/// / `reasoning_content` source columns verbatim (the folded `Thinking.text` projection is lossy —
/// it merges and dedups across columns — so the originals are kept here for a lossless emit
/// round-trip). Named after the columns. Public so `emit_hermes` can read them back.
pub const RAW_REASONING_KEY: &str = "reasoning";
pub const RAW_REASONING_CONTENT_KEY: &str = "reasoning_content";

/// Key inside the session's `extra["hermes"]` bag holding the per-session columns that have no
/// first-class IR home (`source`, `user_id`, `model_config`, `end_reason`, the aggregate token
/// counters, the listing flags, …) so `emit_hermes` can write them back. The system prompt is NOT
/// here (it is `Session::system_prompt`), nor is lineage (`Session::lineage`).
pub const SESSION_META_KEY: &str = "session";

/// Optional `messages` columns that older schemas may lack. We probe for each before SELECTing.
const OPTIONAL_MSG_COLS: &[&str] = &[
    "token_count",
    "finish_reason",
    "reasoning",
    "reasoning_content",
    "reasoning_details",
    "codex_reasoning_items",
    "codex_message_items",
    "platform_message_id",
    "observed",
    // v11+ (2026-07, `hermes_cli/session_schema_history.py`): tool-effect classification, the
    // in-place compaction flags, and the display sidecars.
    "effect_disposition",
    "active",
    "compacted",
    "_compressed_summary",
    "api_content",
    "display_kind",
    "display_metadata",
    // v31 (0.21.6, `hermes_state_identity.py`): the durable per-message id every copy of a logical
    // message keeps (compaction generations, rotation children, branch copies), the uids a merged
    // user row absorbed, and per-occurrence tool-call identity (provider ids repeat).
    "message_uid",
    "absorbed_message_uids",
    "tool_call_uids",
    "tool_call_uid",
];

/// What a `display_kind` Hermes stamped on a row IT injected (not typed by the human) makes the
/// turn: a hidden compaction handoff / diagnostic, auto-continue nudges and process notices are
/// notices; a model switch is a [`MessageKind::ModelChange`]; a delegation delivery is the
/// sub-agent's return; a `failed_turn` (the boundary row Hermes writes when a turn ends without a
/// reply, `agent/turn_failure_copy.py`, 0.21+) is an [`MessageKind::Error`]. All read as System
/// turns so titles, first-prompt previews and turn counts stay honest. `None` for `steer` (a human mid-turn message: a prompt) and for kinds Hermes has
/// not documented (`personality_switch`, …), which keep their row role and carry the kind in
/// `extra["hermes"]["display_kind"]` only.
fn harness_display_kind(display_kind: &str) -> Option<(MessageKind, Origin)> {
    match display_kind {
        "model_switch" => Some((MessageKind::ModelChange, Origin::Harness)),
        "async_delegation_complete" => Some((MessageKind::SubagentReturn, Origin::Subagent)),
        "failed_turn" => Some((MessageKind::Error, Origin::Harness)),
        "hidden" | "auto_continue" | "process_complete" | "internal_notification" => {
            Some((MessageKind::Notice, Origin::Harness))
        }
        _ => None,
    }
}

/// The two notices Hermes writes as the closing assistant row of a turn that got no reply
/// (`FAILED_TURN_NOTICE` / `PARTIAL_FAILED_TURN_NOTICE`). Stores from before the row was typed
/// `display_kind = 'failed_turn'` carry only the text; Hermes's own reader
/// (`untyped_failed_turn_display_kind`) matches it exactly, so a reply quoting it stays a reply.
const FAILED_TURN_NOTICES: &[&str] = &[
    "Your request was not processed. Send it again if you still want me to carry it out.",
    "This turn did not complete. Some actions may already have run; verify their effects before resending.",
];

/// The `model_config` lineage markers (`_branched_from` / `_reset_from` / `_delegate_from`,
/// `hermes_state_common.py:151-216`).
const LINEAGE_MARKERS: &[&str] = &["_branched_from", "_reset_from", "_delegate_from"];

/// The `model_config` JSON lineage markers present on a session row, if any.
fn lineage_markers(model_config: Option<&str>) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let Some(Value::Object(cfg)) = model_config.and_then(|m| serde_json::from_str::<Value>(m).ok()) else {
        return out;
    };
    for k in LINEAGE_MARKERS {
        if let Some(v) = cfg.get(*k).filter(|v| !v.is_null()) {
            out.insert((*k).into(), v.clone());
        }
    }
    out
}

pub struct Hermes {
    /// Every Hermes `state.db` we can read: the top-level `<home>/state.db` PLUS one per
    /// `<home>/profiles/*/state.db`. Hermes stores most sessions under the *active* profile (e.g.
    /// `profiles/hermes-x/state.db`), so a single top-level DB only sees a small slice. Each DB is
    /// opened independently per call (read-only) — `SessionRef.path` carries which DB a ref came
    /// from, so cross-profile session-id collisions never alias.
    dbs: Vec<PathBuf>,
}

/// Hermes's deferred-tool wrapper: `tool_call{name, arguments}`.
pub(crate) const DEFERRED_WRAPPER: crate::affordance::DeferredWrapper = crate::affordance::DeferredWrapper {
    name_key: "name",
    args_key: "arguments",
};

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Hermes = Hermes { dbs: Vec::new() };

impl Hermes {
    pub fn detect() -> Self {
        // HERMES_HOME overrides ~/.hermes.
        let home = crate::env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .or_else(|| crate::env::home_dir().map(|h| h.join(".hermes")));
        match home {
            Some(home) => Self::with_home(&home),
            None => Hermes { dbs: Vec::new() },
        }
    }

    /// An adapter over one Hermes home (what `HERMES_HOME` names): its top-level `state.db` and
    /// every `profiles/<name>/state.db`. Tests and the corpus root it explicitly, never via env.
    pub fn with_home(home: &Path) -> Self {
        let mut dbs = Vec::new();
        // Top-level DB (legacy / no-profile installs).
        let top = home.join("state.db");
        if top.exists() {
            dbs.push(top);
        }
        // Every `profiles/<name>/state.db` — depth-1 read_dir, so pre-update
        // `state-snapshots/**/state.db` are naturally skipped (never recurse).
        if let Ok(rd) = std::fs::read_dir(home.join("profiles")) {
            let mut profiles: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("state.db")).collect();
            profiles.sort();
            dbs.extend(profiles.into_iter().filter(|p| p.exists())); // hermes-x, hermes-google, …
        }
        Hermes { dbs }
    }

    /// An adapter over exactly one `state.db` — lets emit-verification and tests re-parse a
    /// just-written DB without mutating `HERMES_HOME` (process-global env, races other threads).
    pub fn with_db(db: PathBuf) -> Self {
        Hermes { dbs: vec![db] }
    }
}

impl Adapter for Hermes {
    /// The `display_kind` the reader keeps: a finished process, a skill invocation.
    fn record_affordance(
        &self,
        m: &Message,
        _first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        use crate::affordance::{Kind, RecordHit};
        let display = m
            .harness_extra(Harness::Hermes)
            .and_then(|bag| bag.get("display_kind"))
            .and_then(Value::as_str);
        Ok(match display.unwrap_or("") {
            "process_complete" => Some(RecordHit {
                kind: Kind::BackgroundJob,
                action: "complete",
                tool: "process_complete",
            }),
            "skill_invocation" => Some(RecordHit {
                kind: Kind::Skill,
                action: "load",
                tool: "skill_invocation",
            }),
            _ => None,
        })
    }

    fn harness(&self) -> Harness {
        Harness::Hermes
    }

    /// The delegate runs of the conversation's compression chain ([`subagent_refs`]).
    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        Ok(super::ref_forest(subagent_refs(r)?, workflow, select))
    }

    /// Every dir holding a `state.db` (home + each profile), the first `state.db`'s first:
    /// `emit_hermes` writes `<root>/state.db`, the same file this adapter reads first. The db files
    /// themselves are the catalog's per-session file-watches, so growth in place is still noticed.
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .dbs
            .iter()
            .filter_map(|db| db.parent())
            .map(Path::to_path_buf)
            .collect();
        roots.dedup();
        roots
    }

    fn discover(&self) -> Result<crate::Discovered> {
        // Iterate every DB (top-level + each profile), opening each read-only, and concatenate the
        // per-DB discovery. Each `SessionRef` carries its own `path`, so refs from different
        // profiles stay disambiguated even when their internal session ids collide. A single
        // unreadable DB (permission-denied, locked, not a database) is skipped and reported rather
        // than failing the whole scan.
        let mut out = crate::Discovered::default();
        for db in &self.dbs {
            let refs = super::store::sqlite::cached_discover(db, || {
                open_ro(db)?.read_snapshot(|conn, reads| discover_conn_with_reads(conn, db, Some(reads)))
            });
            if let Some(refs) = out.keep(db, refs) {
                out.refs.extend(refs);
            }
        }
        Ok(out)
    }

    fn capabilities(&self) -> super::Capabilities {
        super::Capabilities {
            denials: &[],
            edit_guard: None,
            lane_stops: None,
            // Hermes routes lazily loaded tools through `tool_call{name, arguments}` and its reader
            // leaves the wrapper in place.
            deferred_wrapper: Some(DEFERRED_WRAPPER),
            rich_spans: false,
        }
    }

    /// Hermes writes each profile's `state.db` in place.
    fn watch_policy(&self, _r: &SessionRef) -> super::WatchPolicy {
        super::WatchPolicy::Database
    }

    fn stream(&self, r: &SessionRef, opts: &stream::Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Hermes, sink)
                .unwrapping(self.capabilities().deferred_wrapper);
            let sink: &mut dyn MessageSink = &mut spawns;
            // Route by the ref's own DB path (set at discovery time), NOT a single self.db — this is
            // what lets one Hermes adapter span many profile DBs without id collisions.
            let session_id = r.id.clone();
            super::store::sqlite::read_session(
                &r.path,
                move |conn| session_revision(conn, &session_id),
                |conn, reads, guard| stream_conn_with_reads(conn, r, opts, sink, reads, Some(guard)),
            )
        })
    }

    /// Every session's certificate, from one read of the store's small tables and the
    /// `messages(session_id, …)` indexes ([`rows_conn`]).
    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        open_ro(store)
            .ok()?
            .read_snapshot(|conn, reads| rows_from(conn, &*walked(conn, store, Some(reads))?))
            .ok()
    }

    /// What `/goal`, `/loop` and `/heartbeat` keep in `state_meta` under `<kind>:<session id>`.
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        let conn = open_ro(&r.path)?;
        conn.read_snapshot(|conn, _| {
            if !table_exists(conn, "state_meta") {
                return Ok(Vec::new());
            }
            state_meta_affordances(conn, &r.id)
        })
    }
}

/// Per-session metadata. Historical absent columns are selected as NULL; unreadable cells fail.
struct SessionMetaRow {
    model: Option<String>,
    started: Option<f64>,
    ended: Option<f64>,
    title: Option<String>,
    source: Option<String>,
    parent: Option<String>,
    end_reason: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    last_activity: Option<f64>,
    model_config: Option<String>,
}

/// The most recent of a session's activity stamps: Hermes bumps `last_activity_at` on every turn
/// while `ended_at` stays NULL for a live session, so it is the honest `updated_at`.
fn latest_of(stamps: [Option<f64>; 3]) -> Option<DateTime<Utc>> {
    stamps.into_iter().flatten().reduce(f64::max).and_then(ts_from_epoch)
}

/// One `SessionRef` per CONVERSATION. Excluded: delegate sub-agent runs
/// (`model_config._delegate_from` — Hermes's own picker hides them too; [`subagent_refs`] reaches
/// them through their parent) and
/// compression ANCESTORS — a session that ended in compression and has a continuation is listed
/// once, as its tip, whose parse merges the whole chain (Hermes lists the root and resumes to the
/// tip; cv's `parse` walks upward from the ref, so the tip is the one to list). Archived and hidden
/// sessions ARE listed: Hermes keeps both resumable (`set_session_hidden`: "still resumable"; the
/// archived view is its recovery surface), and a session cv does not list is one `cv show <id>`
/// cannot reach at all — checked against a store written by Hermes's own code. Their flags ride in
/// `Session.extra[hermes_session]` (`archived`, `hidden`, `pinned`). Branch and reset children are
/// user-visible conversations and stay listed. An untitled compression tip takes its root's title,
/// as Hermes's listing does (`COALESCE(tip.title, s.title)`, `hermes_state_sessions.py:1137`).
fn discover_conn(conn: &Connection, path: &Path) -> Result<Vec<SessionRef>> {
    discover_conn_with_reads(conn, path, None)
}

fn discover_conn_with_reads(
    conn: &Connection,
    path: &Path,
    reads: Option<&crate::source::ReadSet>,
) -> Result<Vec<SessionRef>> {
    Ok(refs_of(&*walked(conn, path, reads)?, path, false)?
        .into_iter()
        .map(|(_, r)| r)
        .collect())
}

/// The delegate sub-agent runs of one conversation (`model_config._delegate_from` naming it, or any
/// segment of its compression chain, since a delegate records the session id live when it was
/// spawned), as [`SessionRef`]s of the same shape discovery gives a session in that db, sorted by
/// id. Discovery leaves them out (Hermes's picker hides them too); this is how a caller reaches
/// them from their parent. Every listed conversation's answer is derived once per db state and
/// kept in cv's catalog, so an unchanged db is neither copied nor opened to answer.
pub fn subagent_refs(parent: &SessionRef) -> Result<Vec<SessionRef>> {
    let record = super::store::sqlite::derived(&parent.path, Harness::Hermes, forests_conn)?;
    let forests = &record.payload;
    if forests.listed.contains(&parent.id) {
        return Ok(forests.of.get(&parent.id).cloned().unwrap_or_default());
    }
    // A session discovery does not list (a delegate run, a compression ancestor) walks its chain.
    open_ro(&parent.path)?.read_snapshot(|conn, reads| {
        if crate::source::unchanged(parent, &record.baseline) {
            return Ok(chain_delegates(
                &*walked(conn, &parent.path, Some(reads))?,
                &parent.id,
                &forests.by_parent,
            ));
        }
        let fresh = forests_conn(conn, &parent.path)?;
        Ok(chain_delegates(
            &*walked(conn, &parent.path, Some(reads))?,
            &parent.id,
            &fresh.by_parent,
        ))
    })
}

/// What [`subagent_refs`] keeps per db state.
#[derive(serde::Serialize, serde::Deserialize)]
struct Forests {
    /// Every delegate run, by the session its `_delegate_from` names.
    by_parent: std::collections::HashMap<String, Vec<SessionRef>>,
    /// The conversations discovery lists, each answered by `of` (absent there: none).
    listed: HashSet<String>,
    of: std::collections::HashMap<String, Vec<SessionRef>>,
}

impl crate::source::Payload for Forests {
    const TAG: &'static str = "hermes-delegates";
}

/// One walk over `sessions` for the delegate runs, then each listed conversation's chain. The
/// walk reads the overflow pages too; a forest fold asks for every conversation's delegates, so
/// one walk per parent made a `--subagents` pass over a 4 GB store read ~4 GB per conversation.
fn forests_conn(conn: &Connection, path: &Path) -> Result<Forests> {
    let walk = walked(conn, path, None)?;
    let mut by_parent = std::collections::HashMap::<String, Vec<SessionRef>>::new();
    for (from, r) in refs_of(&walk, path, true)? {
        by_parent.entry(from.unwrap_or_default()).or_default().push(r);
    }
    let listed: HashSet<String> = super::store::sqlite::cached_discover(path, || discover_conn(conn, path))?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let of = if by_parent.is_empty() {
        Default::default()
    } else {
        listed
            .iter()
            .map(|id| (id.clone(), chain_delegates(&walk, id, &by_parent)))
            .filter(|(_, kids)| !kids.is_empty())
            .collect()
    };
    Ok(Forests { by_parent, listed, of })
}

/// The delegates of every segment of `id`'s compression chain, sorted by id.
fn chain_delegates(
    walk: &Walk,
    id: &str,
    by_parent: &std::collections::HashMap<String, Vec<SessionRef>>,
) -> Vec<SessionRef> {
    let chain = if walk.has("parent_session_id") {
        walk.lineage(id)
    } else {
        vec![id.to_string()]
    };
    let mut out: Vec<SessionRef> = chain
        .iter()
        .filter_map(|id| by_parent.get(id))
        .flatten()
        .cloned()
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// One walk over `sessions` per store state, serving discovery, the delegate forests and every
/// session's certificate ([`walked`]). Each read the table whole (its overflow pages too: the
/// system prompts of older schemas), three times per state, and a forest derivation probed the
/// schema and walked each listed conversation's chain by query once more.
pub(crate) struct Walk {
    columns: HashSet<String>,
    /// The rows in the listing order (`ORDER BY started_at DESC`), scanned in table order.
    rows: Vec<WalkRow>,
    /// Row positions by text id (`WHERE id = ?1`).
    by_id: std::collections::HashMap<String, usize>,
}

/// One `sessions` row as its readers take it.
struct WalkRow {
    /// The listing's typed reads (`id` as text, then its columns), or the first one's error.
    listed: std::result::Result<Listed, String>,
    /// Text of a column, as a lone-column query reads it (`None` for NULL, absent or not text).
    id: Option<String>,
    title: Option<String>,
    parent: Option<String>,
    end_reason: Option<String>,
    model_config: Option<String>,
    /// [`rows_from`]: the digest of every column, and of `(id, model_config, started_at)`.
    digest: u64,
    child: u64,
}

struct Listed {
    id: String,
    title: Option<String>,
    started: Option<f64>,
    ended: Option<f64>,
    cwd: Option<String>,
    last: Option<f64>,
    model_config: Option<String>,
}

/// A value as SQLite orders it: NULL, then numbers, then text, then blobs.
#[derive(PartialEq, PartialOrd)]
enum SqlOrder {
    Null,
    Number(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl Walk {
    fn has(&self, column: &str) -> bool {
        self.columns.contains(column)
    }

    fn row(&self, id: &str) -> Option<&WalkRow> {
        self.by_id.get(id).map(|&i| &self.rows[i])
    }

    /// [`session_lineage_root_to_tip`] over the walked rows.
    fn lineage(&self, session_id: &str) -> Vec<String> {
        if session_id.is_empty() {
            return vec![session_id.to_string()];
        }
        let (has_end_reason, has_model_config) = (self.has("end_reason"), self.has("model_config"));
        let mut chain: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut current = session_id.to_string();
        loop {
            if current.is_empty() || seen.contains(&current) {
                break;
            }
            seen.insert(current.clone());
            chain.push(current.clone());
            let row = self.row(&current);
            if has_model_config && !lineage_markers(row.and_then(|r| r.model_config.as_deref())).is_empty() {
                break;
            }
            let Some(parent) = row.and_then(|r| r.parent.clone()).filter(|p| !p.is_empty()) else {
                break;
            };
            if has_end_reason
                && !matches!(
                    self.row(&parent).and_then(|r| r.end_reason.as_deref()),
                    Some("compression") | Some("orphaned_compression")
                )
            {
                break;
            }
            current = parent;
        }
        chain.reverse();
        if chain.is_empty() {
            vec![session_id.to_string()]
        } else {
            chain
        }
    }
}

/// The [`Walk`] of `conn`'s `sessions` table.
fn walk_conn(conn: &Connection) -> Result<Walk> {
    use super::store::sqlite::fold_value;
    use crate::source::Rows;
    let mut stmt = conn.prepare("SELECT * FROM sessions")?;
    let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let at = |name: &str| names.iter().position(|n| n == name);
    let id_at = at("id").context("hermes sessions without id")?;
    let (title_at, parent_at, end_at, config_at) = (
        at("title"),
        at("parent_session_id"),
        at("end_reason"),
        at("model_config"),
    );
    let (started_at, ended_at, cwd_at, last_at) = (at("started_at"), at("ended_at"), at("cwd"), at("last_activity_at"));
    let lone = |row: &rusqlite::Row<'_>, i: Option<usize>| -> Option<String> {
        row.get::<_, Option<String>>(i?).ok().flatten()
    };
    let mut rows = Vec::new();
    let mut keys = Vec::new();
    let mut query = stmt.query([])?;
    while let Some(row) = query.next()? {
        let mut digest = Rows::SEED;
        for i in 0..names.len() {
            digest = fold_value(digest, row.get_ref(i)?);
        }
        let mut child = Rows::SEED;
        for i in [Some(id_at), config_at, started_at].into_iter().flatten() {
            child = fold_value(child, row.get_ref(i)?);
        }
        let typed = |i: Option<usize>| -> std::result::Result<Option<String>, String> {
            i.map_or(Ok(None), |i| row.get::<_, Option<String>>(i).map_err(|e| e.to_string()))
        };
        let number = |i: Option<usize>| -> std::result::Result<Option<f64>, String> {
            i.map_or(Ok(None), |i| row.get::<_, Option<f64>>(i).map_err(|e| e.to_string()))
        };
        let listed = (|| {
            Ok(Listed {
                id: row.get::<_, String>(id_at).map_err(|e| e.to_string())?,
                title: typed(title_at)?,
                started: number(started_at)?,
                ended: number(ended_at)?,
                cwd: typed(cwd_at)?,
                last: number(last_at)?,
                model_config: typed(config_at)?,
            })
        })();
        let key = match started_at.map(|i| row.get_ref(i)).transpose()? {
            None | Some(rusqlite::types::ValueRef::Null) => SqlOrder::Null,
            Some(rusqlite::types::ValueRef::Integer(i)) => SqlOrder::Number(i as f64),
            Some(rusqlite::types::ValueRef::Real(f)) => SqlOrder::Number(f),
            Some(rusqlite::types::ValueRef::Text(t)) => SqlOrder::Text(t.to_vec()),
            Some(rusqlite::types::ValueRef::Blob(b)) => SqlOrder::Blob(b.to_vec()),
        };
        keys.push(key);
        rows.push(WalkRow {
            listed,
            id: lone(row, Some(id_at)),
            title: lone(row, title_at),
            parent: lone(row, parent_at),
            end_reason: lone(row, end_at),
            model_config: lone(row, config_at),
            digest,
            child,
        });
    }
    // `ORDER BY started_at DESC`, ties in table order.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| keys[b].partial_cmp(&keys[a]).unwrap_or(std::cmp::Ordering::Equal));
    let mut slots: Vec<Option<WalkRow>> = rows.into_iter().map(Some).collect();
    let rows: Vec<WalkRow> = order.into_iter().filter_map(|i| slots[i].take()).collect();
    let mut by_id = std::collections::HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        if let Ok(listed) = &row.listed {
            by_id.entry(listed.id.clone()).or_insert(i);
        }
    }
    Ok(Walk {
        columns: names.into_iter().collect(),
        rows,
        by_id,
    })
}

/// The [`Walk`] of the store at `db`, once per store state (its database, WAL and journal
/// identities) in this process. When `reads` is supplied from a pinned MVCC snapshot whose live
/// WAL already moved on disk, the memo is bypassed so `conn` never pairs with another state's walk.
fn walked(conn: &Connection, db: &Path, reads: Option<&crate::source::ReadSet>) -> Result<std::sync::Arc<Walk>> {
    type State = Vec<Option<crate::source::FileId>>;
    type Walks = std::sync::Mutex<std::collections::HashMap<PathBuf, (State, std::sync::Arc<Walk>)>>;
    static WALKS: std::sync::LazyLock<Walks> = std::sync::LazyLock::new(Default::default);
    let state = || -> State {
        let mut files: Vec<PathBuf> = crate::source::sqlite_sidecars(db).into();
        files.push(db.to_path_buf());
        files.iter().map(|f| crate::source::FileId::at(f)).collect()
    };
    let key = std::path::absolute(db)?;
    let matches_disk = reads.is_none_or(|r| r.fresh(&mut crate::source::Probe::default()));
    let before = state();
    let lock = || WALKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches_disk {
        if let Some((_, walk)) = lock().get(&key).filter(|(at, _)| *at == before) {
            return Ok(walk.clone());
        }
    }
    #[cfg(test)]
    WALKED.with(|n| n.set(n.get() + 1));
    let walk = std::sync::Arc::new(walk_conn(conn)?);
    // Kept only when `conn`'s snapshot matches disk and no write landed while it walked.
    if matches_disk && reads.is_none_or(|r| r.fresh(&mut crate::source::Probe::default())) && state() == before {
        lock().insert(key, (before, walk.clone()));
    }
    Ok(walk)
}

#[cfg(test)]
thread_local! { static WALKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// Session refs in one db, each with the session its `_delegate_from` names: with `delegates`
/// unset, every listed conversation (see [`discover_conn`]); with it set, only the delegate runs.
fn refs_of(walk: &Walk, path: &Path, delegates: bool) -> Result<Vec<(Option<String>, SessionRef)>> {
    // The listing selects these whatever the schema.
    for column in ["started_at", "ended_at"] {
        anyhow::ensure!(walk.has(column), "no such column: {column}");
    }
    let has_parent = walk.has("parent_session_id");
    // Compression ancestors: parents (ended in compression) that some session continues from.
    let mut compression_parents: HashSet<&str> = HashSet::new();
    if has_parent && walk.has("end_reason") {
        let continued: HashSet<&str> = walk.rows.iter().filter_map(|r| r.parent.as_deref()).collect();
        for row in &walk.rows {
            let Ok(listed) = &row.listed else { continue };
            if matches!(
                row.end_reason.as_deref(),
                Some("compression") | Some("orphaned_compression")
            ) && continued.contains(listed.id.as_str())
            {
                compression_parents.insert(listed.id.as_str());
            }
        }
    }
    let mut out = Vec::new();
    for row in &walk.rows {
        let listed = row.listed.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
        if compression_parents.contains(listed.id.as_str()) {
            continue;
        }
        let delegate_from = lineage_markers(listed.model_config.as_deref())
            .get("_delegate_from")
            .map(|v| v.as_str().unwrap_or_default().to_string());
        if delegates != delegate_from.is_some() {
            continue;
        }
        let title = listed.title.clone().filter(|t| !t.is_empty()).or_else(|| {
            has_parent
                .then(|| inherited_title(walk, &walk.lineage(&listed.id)))
                .flatten()
        });
        out.push((
            delegate_from,
            SessionRef {
                cwd: listed.cwd.clone().filter(|c| !c.is_empty()).map(PathBuf::from),
                title: title.map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE)),
                created_at: listed.started.and_then(ts_from_epoch),
                updated_at: latest_of([listed.ended, listed.last, listed.started]),
                message_count: None,
                ..SessionRef::new(listed.id.clone(), Harness::Hermes, path.to_path_buf())
            },
        ));
    }
    // Counts stay unknown here; complete reads/indexing count canonical turns and certify them.
    Ok(out)
}

/// Hermes titles a compression chain `COALESCE(tip.title, root.title)` (`hermes_state_sessions.py:1137`):
/// the title is carried root→tip only after the publish transaction, so an untitled tip is the normal
/// state right after a rotation (a real 0.21.3 store: `publish_compression_child` leaves the child's
/// `title` NULL). Given a root→tip `chain` (see [`session_lineage_root_to_tip`]), the nearest titled
/// ancestor, root first; `None` for a chain of one.
fn inherited_title_conn(conn: &Connection, chain: &[String]) -> Option<String> {
    let (_, ancestors) = chain.split_last()?;
    ancestors.iter().find_map(|id| {
        conn.query_row("SELECT title FROM sessions WHERE id = ?1", [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
    })
}

/// [`inherited_title_conn`] over the walked rows.
fn inherited_title(walk: &Walk, chain: &[String]) -> Option<String> {
    let (_, ancestors) = chain.split_last()?;
    ancestors
        .iter()
        .find_map(|id| walk.row(id)?.title.clone().filter(|t| !t.is_empty()))
}

/// Per-session columns with no first-class IR home. Captured into
/// `Session.extra["hermes"][SESSION_META_KEY]` on parse and written back by `emit_hermes`, so the
/// round-trip loses no session-row data. Text columns map to JSON strings; the aggregate counters to
/// JSON numbers (see [`SESSION_META_INT_COLS`]). `source` / `end_reason` are read by the metadata
/// SELECT already and passed in (so we don't re-probe them); everything here is probed independently
/// to stay graceful on older schemas. `system_prompt` is not here: it is `Session::system_prompt`.
const SESSION_META_TEXT_COLS: &[&str] = &[
    "user_id",
    "model_config",
    // v11+: where the transcript came from and how it is labelled/routed.
    "display_name",
    "origin_json",
    "title_source",
    "profile_name",
    "transport_profile",
    "git_repo_root",
    // v29: the immutable source the row was created with (`source` is live routing state that later
    // retags rewrite; Hermes checks both for kanban-owned worker transcripts).
    "created_source",
];
const SESSION_META_INT_COLS: &[&str] = &[
    // Listing flags (non-zero only, like the counters): the user tucked the session away or pinned it.
    "archived",
    // v30: archived by the auto-archive sweep rather than by the user.
    "auto_archived",
    "hidden",
    "pinned",
    "tool_call_count",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
];

/// Session-level facts read from the `sessions` row beyond the first-class columns: the contents of
/// the session's `extra["hermes"]` bag (the dropped columns under [`SESSION_META_KEY`], the
/// importer's `imported_from`), the resolved system prompt, and whether the transcript is a foreign
/// import (which makes every message's origin [`Origin::Import`]).
struct SessionFacts {
    bag: serde_json::Map<String, Value>,
    system_prompt: Option<String>,
    imported: bool,
}

/// Read the [`SessionFacts`] for `id`. `source` / `end_reason` come pre-read from the metadata
/// SELECT; the rest are probed here.
fn read_session_facts(
    conn: &Connection,
    (path, reads): (&Path, &crate::source::ReadSet),
    guard: Option<&std::sync::Arc<crate::json::SqliteSessionGuard>>,
    id: &str,
    source: Option<&str>,
    end_reason: Option<&str>,
    opts: &stream::Demand,
) -> Result<SessionFacts> {
    let mut meta = serde_json::Map::new();
    let select = Select::of(conn, "sessions")
        .present(SESSION_META_TEXT_COLS)
        .present(SESSION_META_INT_COLS);
    if opts.tier >= Tier::Rich {
        if let Some(s) = source.filter(|s| !s.is_empty()) {
            meta.insert("source".into(), Value::String(s.to_string()));
        }
        if let Some(e) = end_reason.filter(|s| !s.is_empty()) {
            meta.insert("end_reason".into(), Value::String(e.to_string()));
        }
        // One row read for every column the bag carries; a value of another type reads as absent.
        let mut text: Vec<(&str, Option<String>)> = Vec::new();
        let mut ints: Vec<(&str, Option<i64>)> = Vec::new();
        if !select.list().is_empty() {
            let row = conn.query_row(
                &format!("SELECT {} FROM sessions WHERE id = ?1", select.list()),
                [id],
                |row| {
                    let at = |c: &str| select.at(c);
                    text = SESSION_META_TEXT_COLS
                        .iter()
                        .filter_map(|c| at(c).map(|i| (*c, row.get::<_, Option<String>>(i).ok().flatten())))
                        .collect();
                    ints = SESSION_META_INT_COLS
                        .iter()
                        .filter_map(|c| at(c).map(|i| (*c, row.get::<_, Option<i64>>(i).ok().flatten())))
                        .collect();
                    Ok(())
                },
            );
            if row.is_err() {
                (text, ints) = (Vec::new(), Vec::new());
            }
        }
        for (col, v) in text {
            if let Some(v) = v.filter(|s| !s.is_empty()) {
                meta.insert(col.into(), Value::String(v));
            }
        }
        for (col, v) in ints {
            // Only carry non-zero aggregates: a freshly-emitted DB leaves these at their DEFAULT 0, so
            // storing 0 would make the round-trip "lose" a value that was never meaningfully set. emit
            // writes back exactly the keys present here, so omitting 0 keeps emit→parse symmetric.
            if let Some(v) = v.filter(|n| *n != 0) {
                meta.insert(col.into(), Value::Number(v.into()));
            }
        }
    }
    // The system prompt: schema v25 hollowed `sessions.system_prompt` out into
    // `system_prompts(hash, prompt)` keyed by `system_prompt_hash` (`hermes_state_schema.py:180-199`);
    // Hermes reads `COALESCE(sp.prompt, s.system_prompt)`, so the table wins and the legacy column is
    // the fallback for a store an older Hermes wrote.
    let str_col = |sql: &str| -> Option<String> {
        conn.query_row(sql, [id], |row| row.get::<_, Option<String>>(0))
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
    };
    let mut system_prompt = None;
    if select.has("system_prompt_hash") && table_exists(conn, "system_prompts") {
        system_prompt = str_col(
            "SELECT sp.prompt FROM sessions s JOIN system_prompts sp ON sp.hash = s.system_prompt_hash \
             WHERE s.id = ?1",
        );
    }
    if system_prompt.is_none() && select.has("system_prompt") {
        system_prompt = str_col("SELECT system_prompt FROM sessions WHERE id = ?1");
    }
    let mut bag = serde_json::Map::new();
    // A foreign import (`hermes sessions import --from claude|codex`, `hermes_cli/foreign_sessions.py`)
    // records its provenance in `origin_json.imported_from{tool, path, foreign_session_id}` — the
    // very transcript cv also parses natively. Surface it in the bag (full demand) so consumers can
    // dedupe; the messages themselves carry `Origin::Import` on every demand.
    let mut imported_from = None;
    if select.has("origin_json") {
        let cells = match guard {
            Some(guard) => Cells::for_session(conn, path, reads, guard, "sessions")?,
            None => Cells::new(conn, path, reads, "sessions")?,
        };
        let origin = Select::of(conn, "sessions").cell(&cells, "origin_json");
        let at = origin.index("origin_json")?;
        let sql = format!("SELECT {} FROM sessions WHERE id = ?1", origin.list());
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query([id])?;
        if let Some(cell) = rows
            .next()?
            .map(|row| cells.cell(conn, "sessions", "origin_json", row, at))
            .transpose()?
            .flatten()
        {
            imported_from = cells
                .read(cell, |view| match view.get("imported_from")? {
                    Some(from) if from.is_object()? => Ok(Some(if opts.tier >= Tier::Rich {
                        from.resolve()?.into_owned()
                    } else {
                        Value::Null
                    })),
                    _ => Ok(None),
                })?
                .flatten();
        }
    }
    let imported = imported_from.is_some();
    let imported_from = imported_from.filter(|_| opts.tier >= Tier::Rich);
    if let Some(imported_from) = imported_from {
        bag.insert("imported_from".into(), imported_from);
    }
    if !meta.is_empty() {
        bag.insert(SESSION_META_KEY.into(), Value::Object(meta));
    }
    Ok(SessionFacts {
        bag,
        system_prompt,
        imported,
    })
}

/// The compression continuation of `id`, if any: the child whose `parent_session_id` is `id` and
/// that carries no branch / reset / delegate marker (`_COMPRESSION_CHILD_SQL`). Only meaningful for
/// a session that ended in compression; `None` when the store does not say, an error when it
/// cannot be asked. A child whose id is not text is no session cv can name.
/// Every session's [`Rows`](crate::source::Rows) certificate: what a read of it consults besides
/// message bodies. The message rows of its compression chain (count, highest id and, from schema
/// v11 on, the `active` flags: a rewind or an in-place compaction flips them), each chain
/// session's whole `sessions` row (title, prompt hash, lineage, counters), the end reason of the
/// parent that stopped the walk, the `(id, model_config, started_at)` of its children (the
/// compression continuation), and its `state_meta` rows. The aggregates come from the
/// `messages(session_id, …)` indexes; no `content` is read.
#[cfg(test)]
fn rows_conn(conn: &Connection) -> Result<std::collections::HashMap<String, crate::source::Rows>> {
    rows_from(conn, &walk_conn(conn)?)
}

/// [`rows_conn`] with the `sessions` table already walked.
fn rows_from(conn: &Connection, walk: &Walk) -> Result<std::collections::HashMap<String, crate::source::Rows>> {
    use super::store::sqlite::fold_value;
    use crate::source::Rows;
    use std::collections::HashMap;
    struct Meta {
        row: u64,
        parent: Option<String>,
        end_reason: Option<String>,
        marked: bool,
        child: u64,
    }
    let columns = &walk.columns;
    let mut sessions: HashMap<String, Meta> = HashMap::new();
    let mut children: HashMap<String, u64> = HashMap::new();
    let text = |value: &Option<String>| value.clone().filter(|s| !s.is_empty());
    for row in &walk.rows {
        let Some(id) = text(&row.id) else { continue };
        sessions.insert(
            id,
            Meta {
                row: row.digest,
                parent: text(&row.parent),
                end_reason: text(&row.end_reason),
                marked: !lineage_markers(text(&row.model_config).as_deref()).is_empty(),
                child: row.child,
            },
        );
    }
    let mut by_parent: Vec<(&str, &str, u64)> = sessions
        .iter()
        .filter_map(|(id, meta)| Some((meta.parent.as_deref()?, id.as_str(), meta.child)))
        .collect();
    by_parent.sort_unstable();
    for (parent, _, child) in by_parent {
        let digest = children.entry(parent.to_owned()).or_insert(Rows::SEED);
        *digest = Rows::fold(*digest, &child.to_le_bytes());
    }
    let msg_cols = super::store::sqlite::columns(conn, "messages");
    let mut messages: HashMap<String, (u64, i64, u64)> = HashMap::new();
    {
        let mut exprs = vec![
            "session_id".to_string(),
            "id".to_string(),
            "role".to_string(),
            "tool_call_id".to_string(),
            "tool_name".to_string(),
            "timestamp".to_string(),
        ];
        for col in [
            "active",
            "compacted",
            "_compressed_summary",
            "display_kind",
            "finish_reason",
            "token_count",
            "observed",
            "effect_disposition",
            "platform_message_id",
            "message_uid",
            "tool_call_uid",
        ] {
            if msg_cols.contains(col) {
                exprs.push(col.to_string());
            }
        }
        for col in [
            "content",
            "tool_calls",
            "reasoning",
            "reasoning_content",
            "reasoning_details",
            "codex_reasoning_items",
            "codex_message_items",
            "api_content",
            "display_metadata",
            "absorbed_message_uids",
            "tool_call_uids",
        ] {
            if msg_cols.contains(col) {
                exprs.push(format!("typeof({col})"));
                exprs.push(format!("length({col})"));
                exprs.push(format!("substr({col}, 1, 128)"));
            }
        }
        let col_count = exprs.len();
        let sql = format!("SELECT {} FROM messages ORDER BY session_id, id", exprs.join(", "));
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let Some(id) = row.get::<_, Option<String>>(0)? else {
                continue;
            };
            let entry = messages.entry(id).or_insert((0, 0, Rows::SEED));
            entry.0 += 1;
            let row_id = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
            entry.1 = entry.1.max(row_id);
            let mut d = entry.2;
            for i in 1..col_count {
                d = fold_value(d, row.get_ref(i)?);
            }
            entry.2 = d;
        }
    }
    let mut meta_rows: HashMap<String, u64> = HashMap::new();
    if table_exists(conn, "state_meta") {
        let mut stmt = conn.prepare("SELECT key, value FROM state_meta ORDER BY key")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let Some(key) = row.get::<_, Option<String>>(0).ok().flatten() else {
                continue;
            };
            let Some((_, id)) = key.split_once(':') else { continue };
            let digest = meta_rows.entry(id.to_owned()).or_insert(Rows::SEED);
            *digest = fold_value(fold_value(*digest, row.get_ref(0)?), row.get_ref(1)?);
        }
    }
    let compressed = |id: &str| {
        sessions.get(id).is_some_and(|m| {
            matches!(
                m.end_reason.as_deref(),
                Some("compression") | Some("orphaned_compression")
            )
        })
    };
    let has_end = columns.contains("end_reason");
    let mut out = HashMap::with_capacity(sessions.len());
    for id in sessions.keys() {
        let (mut count, mut high, mut digest) = (0u64, 0i64, Rows::SEED);
        let mut seen = HashSet::new();
        let mut current = id.as_str();
        // The walk [`session_lineage_root_to_tip`] makes, over the rows just read.
        loop {
            if !seen.insert(current) {
                break;
            }
            let Some(meta) = sessions.get(current) else { break };
            let (n, max, msg_digest) = messages.get(current).copied().unwrap_or((0, 0, Rows::SEED));
            count += n;
            high = high.max(max);
            for part in [meta.row, n, max as u64, msg_digest] {
                digest = Rows::fold(digest, &part.to_le_bytes());
            }
            if meta.marked {
                break;
            }
            let Some(parent) = meta.parent.as_deref() else { break };
            digest = Rows::fold(digest, parent.as_bytes());
            if let Some(end) = sessions.get(parent).and_then(|p| p.end_reason.as_deref()) {
                digest = Rows::fold(digest, end.as_bytes());
            }
            if has_end && !compressed(parent) {
                break;
            }
            current = parent;
        }
        for part in [children.get(id), meta_rows.get(id)] {
            digest = Rows::fold(digest, &part.copied().unwrap_or(0).to_le_bytes());
        }
        out.insert(id.clone(), Rows { count, high, digest });
    }
    Ok(out)
}

/// Exact single-session revision certificate for [`super::store::sqlite::read_session`]: folds
/// every column of `id`'s compression-chain `sessions` rows, `system_prompts`, stopping parent
/// `end_reason`, direct children, `state_meta` entries, and `messages` rows (`WHERE session_id = ?1`),
/// touching zero rows of unrelated sessions in the same database.
fn session_revision(conn: &Connection, id: &str) -> Result<crate::source::Rows> {
    use super::store::sqlite::fold_value;
    use crate::source::Rows;
    let select = Select::of(conn, "sessions");
    let has_parent = select.has("parent_session_id");
    let has_end = select.has("end_reason");
    let has_prompt_hash = select.has("system_prompt_hash") && table_exists(conn, "system_prompts");
    let chain = if has_parent {
        session_lineage_root_to_tip(conn, id, &select)
    } else {
        vec![id.to_string()]
    };

    let (mut count, mut high, mut digest) = (0u64, 0i64, Rows::SEED);
    digest = Rows::fold(digest, &(chain.len() as u64).to_le_bytes());
    for (idx, sid) in chain.iter().enumerate() {
        digest = Rows::fold(digest, sid.as_bytes());
        {
            let mut stmt = conn.prepare("SELECT * FROM sessions WHERE id = ?1")?;
            let ncols = stmt.column_count();
            let mut rows = stmt.query([sid])?;
            let mut found = 0u64;
            while let Some(row) = rows.next()? {
                found += 1;
                for i in 0..ncols {
                    digest = fold_value(digest, row.get_ref(i)?);
                }
            }
            digest = Rows::fold(digest, &found.to_le_bytes());
        }
        if has_prompt_hash {
            let prompt = conn
                .query_row(
                    "SELECT sp.prompt FROM sessions s JOIN system_prompts sp ON sp.hash = s.system_prompt_hash WHERE s.id = ?1",
                    [sid],
                    |r| r.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten();
            digest = Rows::fold(digest, &[u8::from(prompt.is_some())]);
            if let Some(p) = prompt {
                digest = Rows::fold(digest, p.as_bytes());
            }
        }
        if idx == 0 && has_parent && has_end {
            let parent_end = conn
                .query_row(
                    "SELECT p.end_reason FROM sessions c JOIN sessions p ON p.id = c.parent_session_id WHERE c.id = ?1",
                    [sid],
                    |r| r.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten();
            digest = Rows::fold(digest, &[u8::from(parent_end.is_some())]);
            if let Some(end) = parent_end {
                digest = Rows::fold(digest, end.as_bytes());
            }
        }
        {
            let mut stmt = conn.prepare("SELECT * FROM messages WHERE session_id = ?1 ORDER BY id ASC")?;
            let ncols = stmt.column_count();
            let id_col = stmt.column_names().position(|c| c == "id");
            let mut rows = stmt.query([sid])?;
            while let Some(row) = rows.next()? {
                count += 1;
                if let Some(i) = id_col {
                    high = high.max(row.get::<_, Option<i64>>(i)?.unwrap_or(0));
                }
                for i in 0..ncols {
                    digest = fold_value(digest, row.get_ref(i)?);
                }
            }
        }
    }
    if has_parent {
        let child_sel = Select::of(conn, "sessions")
            .always(&["id"])
            .probed(&["model_config", "started_at"]);
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM sessions WHERE parent_session_id = ?1 ORDER BY id ASC",
            child_sel.list()
        ))?;
        let mut rows = stmt.query([id])?;
        while let Some(row) = rows.next()? {
            for i in 0..3 {
                digest = fold_value(digest, row.get_ref(i)?);
            }
        }
    }
    if table_exists(conn, "state_meta") {
        let mut stmt = conn.prepare("SELECT key, value FROM state_meta WHERE key LIKE '%:' || ?1 ORDER BY key ASC")?;
        let mut rows = stmt.query([id])?;
        while let Some(row) = rows.next()? {
            let Some(key) = row.get::<_, Option<String>>(0).ok().flatten() else {
                continue;
            };
            if key.split_once(':').is_some_and(|(_, k)| k == id) {
                digest = fold_value(fold_value(digest, row.get_ref(0)?), row.get_ref(1)?);
            }
        }
    }
    Ok(Rows { count, high, digest })
}

fn compression_child(conn: &Connection, id: &str) -> Result<Option<String>> {
    let select = Select::of(conn, "sessions").always(&["id"]).probed(&["model_config"]);
    if !select.has("parent_session_id") {
        return Ok(None);
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM sessions WHERE parent_session_id = ?1 ORDER BY started_at ASC",
        select.list()
    ))?;
    let mut rows = stmt.query([id])?;
    while let Some(row) = rows.next()? {
        let Ok(child) = row.get::<_, String>(0) else { continue };
        let model_config = row.get::<_, Option<String>>(1).ok().flatten();
        if lineage_markers(model_config.as_deref()).is_empty() {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

#[cfg(test)]
fn stream_conn(
    conn: &Connection,
    r: &SessionRef,
    opts: &stream::Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    stream_conn_with_reads(conn, r, opts, sink, &crate::source::ReadSet::default(), None)
}

fn stream_conn_with_reads(
    conn: &Connection,
    r: &SessionRef,
    opts: &stream::Demand,
    sink: &mut dyn MessageSink,
    reads: &crate::source::ReadSet,
    guard: Option<&std::sync::Arc<crate::json::SqliteSessionGuard>>,
) -> Result<Session> {
    // session-level metadata (guard optional columns for historical schemas).
    let select = Select::of(conn, "sessions")
        .probed(&["model"])
        .always(&["started_at", "ended_at"])
        .probed(&[
            "title",
            "source",
            "parent_session_id",
            "end_reason",
            "cwd",
            "git_branch",
            "last_activity_at",
            "model_config",
        ]);
    let has_parent = select.has("parent_session_id");
    let meta_sql = format!("SELECT {} FROM sessions WHERE id = ?1", select.list());
    let SessionMetaRow {
        model,
        started,
        ended,
        title,
        source,
        parent,
        end_reason,
        cwd,
        git_branch,
        last_activity,
        model_config,
    } = conn
        .query_row(&meta_sql, [&r.id], |row| {
            Ok(SessionMetaRow {
                model: row.get(0)?,
                started: row.get(1)?,
                ended: row.get(2)?,
                title: row.get(3)?,
                source: row.get(4)?,
                parent: row.get(5)?,
                end_reason: row.get(6)?,
                cwd: row.get(7)?,
                git_branch: row.get(8)?,
                last_activity: row.get(9)?,
                model_config: row.get(10)?,
            })
        })
        .with_context(|| format!("hermes session {:?}", r.id))?;

    // Format-complete: capture the session columns that have no first-class IR home, so emit can
    // write them back (small metadata only — text/aggregate counters, never message bodies). The tip
    // session's row is the one whose metadata survives (lineage flattening keeps the tip).
    let SessionFacts {
        bag: mut hermes_bag,
        system_prompt,
        imported,
    } = read_session_facts(
        conn,
        (&r.path, reads),
        guard,
        &r.id,
        source.as_deref(),
        end_reason.as_deref(),
        opts,
    )?;
    // A foreign import: every message of the session came through the importer.
    let session_origin = imported.then_some(Origin::Import);

    // Walk the compression lineage root→tip so a compressed-and-continued conversation reads as a
    // single transcript (mirrors `_resume_lineage_ids`); branches/resets/delegates stand alone.
    let chain = if has_parent {
        session_lineage_root_to_tip(conn, &r.id, &select)
    } else {
        vec![r.id.clone()]
    };
    // Lineage, first-class: a `/branch` copy points at what it was branched from, a delegate run at
    // the session that spawned it, a compression rotation at the session it continues (whose rows
    // are merged into this transcript) and — for a session that ended in compression — at the
    // session that continued it. `_reset_from` (a fresh conversation after `/new`; nothing carried
    // over) has no IR field and stays a Hermes fact, as does an unmarked parent that was not merged.
    let markers = lineage_markers(model_config.as_deref());
    let marker = |k: &str| markers.get(k).and_then(Value::as_str).map(str::to_string);
    let mut lineage = Lineage {
        forked_from: marker("_branched_from"),
        parent: marker("_delegate_from"),
        ..Default::default()
    };
    if chain.len() > 1 {
        lineage.continues = chain.get(chain.len() - 2).cloned();
    }
    if matches!(
        end_reason.as_deref(),
        Some("compression") | Some("orphaned_compression")
    ) {
        lineage.continued_in = compression_child(conn, &r.id)?;
    }
    if let Some(reset) = marker("_reset_from").filter(|_| opts.tier >= Tier::Rich) {
        hermes_bag.insert("_reset_from".into(), Value::String(reset));
    }
    if opts.tier >= Tier::Rich && chain.len() == 1 && markers.is_empty() {
        if let Some(p) = parent.filter(|p| !p.is_empty()) {
            hermes_bag.insert("parent_session_id".into(), Value::String(p));
        }
    }
    let mut extra = serde_json::Map::new();
    if !hermes_bag.is_empty() {
        extra.insert(Harness::Hermes.as_str().into(), Value::Object(hermes_bag));
    }

    let s = Session {
        cwd: cwd
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title: title
            .filter(|t| !t.is_empty())
            .or_else(|| r.title.clone())
            .or_else(|| inherited_title_conn(conn, &chain)),
        created_at: started.and_then(ts_from_epoch).or(r.created_at),
        updated_at: latest_of([ended, last_activity, started]).or(r.updated_at),
        model,
        git: git_branch.filter(|b| !b.is_empty()).map(|branch| GitInfo {
            branch: Some(branch),
            ..Default::default()
        }),
        source_path: Some(r.path.clone()),
        extra,
        system_prompt,
        lineage,
        ..Session::new(r.id.clone(), Harness::Hermes)
    };

    // Build the SELECT defensively: required cols always present; optional cols gated by probe.
    // Plain columns are selected by name; every JSON column (and `content`) as a cell, which the
    // row decoder reads through `cells`. Generation identity reads `content` and
    // `_content_rowid` from the content cell's columns (the cell's type and rowid when reads are
    // incremental, the cell itself otherwise).
    let cells = match guard {
        Some(guard) => Cells::for_session(conn, &r.path, reads, guard, "messages")?,
        None => Cells::new(conn, &r.path, reads, "messages")?,
    };
    let incremental = super::store::sqlite::incremental_utf8_table(conn, "messages");
    let plain: Vec<&str> = OPTIONAL_MSG_COLS
        .iter()
        .copied()
        .filter(|c| !CELL_COLS.contains(c))
        .collect();
    let mut select = Select::of(conn, "messages")
        .always(&["id", "role", "tool_call_id", "tool_calls", "tool_name", "timestamp"])
        .present(&plain)
        .cell_named(
            &cells,
            "content",
            if incremental {
                ["_content_rowid".to_string(), "content".to_string()]
            } else {
                ["content".to_string(), "content#type".to_string()]
            },
        );
    for column in CELL_COLS {
        select = select.cell_named(&cells, column, [format!("{column}#"), format!("{column}#type")]);
    }
    let select_list = select.list();
    let idx = |name: &str| select.at(name);

    // Row visibility (schema v12+): Hermes's display projection is `(active = 1 OR compacted = 1)`
    // — live rows plus the history a compaction summarized away — which drops rewound rows and the
    // superseded originals of carried tails. `complete` reads every row instead and tags the flags.
    let has_flags = select.has("active") && select.has("compacted");
    let visibility = if has_flags && opts.tier != Tier::Complete {
        " AND (active = 1 OR compacted = 1)"
    } else {
        ""
    };
    // Display-generation dedup (`_dedupe_display_generations`): a compaction copies the protected
    // tail into each generation — same role/content/timestamp/tool fields, different id/flags — so
    // each logical message is emitted once (its first row; the copies are byte-identical).
    let dedup_generations = select.has("compacted") && opts.tier != Tier::Complete;
    let mut generations = GenerationDedup::default();

    // Hand the session-level metadata to the sink before the body (header-rendering sinks use it).
    sink.meta(&s);

    // Dedup the replayed first user message at compression boundaries — streamed: a tiny bounded
    // look-back (the trailing run of user single-texts since the last substantive assistant turn),
    // not the whole transcript, so peak stays O(one message).
    let mut dedup = ReplayDedup::default();
    let demand = Demand::of(opts, incremental);
    'lineage: for sid in &chain {
        // `ORDER BY id`, as Hermes itself reads (`_ACTIVE_IDS_SQL`, `_fetch_conversation_rows`):
        // timestamps are not monotonic and would split a tool call from its result.
        let sql = format!("SELECT {select_list} FROM messages WHERE session_id = ?1{visibility} ORDER BY id ASC");
        let mut stmt = conn.prepare(&sql)?;
        // Stream rows lazily: one row -> one `Message` at a time, emitted and dropped before the
        // next row (a single Hermes session can be large; never materialize the whole Vec).
        let mut rows = stmt.query([sid])?;
        while let Some(row) = rows.next()? {
            if dedup_generations && !generations.admit(row, &idx, conn, incremental)? {
                continue;
            }
            let row = read_row(conn, &cells, row, &idx, &demand)?;
            let Some(m) = row.into_message(session_origin, demand.tier) else {
                continue;
            };
            // A compaction summary row: Hermes keeps no row for the compaction itself, so cv
            // synthesizes the boundary before it (no content, no id); `crate::compaction` pairs the
            // two by adjacency.
            if m.kind == MessageKind::CompactionSummary {
                let mut boundary = Message::synthesized(MessageKind::CompactionBoundary, m.origin);
                boundary.timestamp = m.timestamp;
                if sink.message(boundary) == Flow::Stop {
                    break 'lineage;
                }
                if sink.message(m) == Flow::Stop {
                    break 'lineage;
                }
                continue;
            }
            // Dedup the replayed first user message at compression boundaries (`complete` keeps it).
            if opts.tier != Tier::Complete && !dedup.admit(&m)? {
                continue;
            }
            if sink.message(m) == Flow::Stop {
                break 'lineage;
            }
        }
    }
    #[cfg(test)]
    ARENA_WRITE_OBSERVER.with(|observer| -> Result<()> {
        if let Some(bytes) = observer.borrow_mut().as_mut() {
            bytes.push(cells.out().1.byte_len()?);
        }
        Ok(())
    })?;
    Ok(s)
}

/// Streams the replayed-user-message dedup Hermes's reader does whole-Vec
/// (`SessionDB._is_duplicate_replayed_user_message`): tracks only the trailing run of
/// user single-texts since the last substantive assistant turn (the exact look-back window the
/// whole-Vec scan uses), so the streaming path stays byte-identical without retaining the session.
#[derive(Default)]
struct ReplayDedup {
    /// Single-texts of consecutive trailing user messages (most recent last). Cleared when a
    /// substantive assistant turn passes (which ends the look-back window).
    recent_user_texts: crate::spill::ExactStrings,
}

impl ReplayDedup {
    /// Admit a message and fold its look-back state once. Insertion provides exact membership,
    /// avoiding a second whole-key hash and text copy for each new trailing prompt.
    fn admit(&mut self, msg: &Message) -> Result<bool> {
        if msg.role == Role::User {
            if let Some(text) = single_text(msg).filter(|text| !text.is_empty()) {
                return self
                    .recent_user_texts
                    .insert_source(text, &crate::lazy::Resolver::new(None));
            }
        }
        self.observe(msg)?;
        Ok(true)
    }

    /// Fold an *emitted* message that `admit` did not take as a new trailing prompt into the
    /// look-back state: a substantive assistant turn (text or tool call) ends the window.
    fn observe(&mut self, m: &Message) -> Result<()> {
        if m.role == Role::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, Block::Text { .. } | Block::ToolUse { .. }))
        {
            self.recent_user_texts.clear();
        }
        Ok(())
    }
}

/// Walk `parent_session_id` from `session_id` up through COMPRESSION continuations only, returning
/// root→tip order — the ids a Hermes display resume materializes (`_resume_lineage_ids` /
/// `_session_lineage_root_to_tip`, `_COMPRESSION_CHILD_SQL`). A `/branch` copy (`_branched_from`)
/// owns a copied transcript and stands alone; reset (`_reset_from`) and delegate (`_delegate_from`)
/// children are separate conversations that only record their lineage; a parent that ended for any
/// reason other than compression is not a continuation either. Bounded + cycle-guarded. `probe` is
/// the caller's `sessions` schema probe (column presence only; the caller reuses its own).
fn session_lineage_root_to_tip(conn: &Connection, session_id: &str, probe: &Select) -> Vec<String> {
    if session_id.is_empty() {
        return vec![session_id.to_string()];
    }
    let has_end_reason = probe.has("end_reason");
    let has_model_config = probe.has("model_config");
    let str_col = |col: &str, id: &str| -> Option<String> {
        conn.query_row(&format!("SELECT {col} FROM sessions WHERE id = ?1"), [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
    };
    let mut chain: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = session_id.to_string();
    loop {
        if current.is_empty() || seen.contains(&current) {
            break;
        }
        seen.insert(current.clone());
        chain.push(current.clone());
        // An explicit lineage marker on this row ends the walk: it is a branch/reset/delegate child.
        if has_model_config && !lineage_markers(str_col("model_config", &current).as_deref()).is_empty() {
            break;
        }
        let Some(parent) = str_col("parent_session_id", &current).filter(|p| !p.is_empty()) else {
            break;
        };
        // Merge only through a parent that ended in compression (the continuation needs its rows).
        if has_end_reason
            && !matches!(
                str_col("end_reason", &parent).as_deref(),
                Some("compression") | Some("orphaned_compression")
            )
        {
            break;
        }
        current = parent;
    }
    chain.reverse();
    if chain.is_empty() {
        vec![session_id.to_string()]
    } else {
        chain
    }
}

/// Plain text of a message iff it is exactly one Text block (the shape Hermes replays as a string).
fn single_text(m: &Message) -> Option<&crate::Text> {
    match m.content.as_slice() {
        [Block::Text { text }] => Some(text),
        _ => None,
    }
}

/// Borrow native TEXT only when the old String decoder would have accepted the column.
fn borrowed_text<'a>(row: &'a rusqlite::Row<'_>, index: Option<usize>) -> Option<&'a str> {
    match row.get_ref(index?).ok()? {
        rusqlite::types::ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
        _ => None,
    }
}

/// Explicit FNV-1a framing: optional fields have a presence byte, strings end in 0xff
/// (not a UTF-8 byte), and numbers use little endian. No std::Hash layout is persisted.
#[derive(Clone)]
struct GenerationHash(u64);
impl GenerationHash {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn text(&mut self, text: Option<&str>) {
        self.write(&[u8::from(text.is_some())]);
        if let Some(text) = text {
            self.write(text.as_bytes());
            self.write(&[0xff]);
        }
    }
    fn number(&mut self, value: Option<u64>) {
        self.write(&[u8::from(value.is_some())]);
        if let Some(value) = value {
            self.write(&value.to_le_bytes());
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Hermes's copied-generation identity precedes lazy body selection and extra projection.
/// Borrowing the native cells preserves the original String/Option hashing without body copies.
#[cfg(test)]
fn generation_key(
    row: &rusqlite::Row<'_>,
    index: &impl Fn(&str) -> Option<usize>,
    conn: &Connection,
    incremental: bool,
) -> Result<u64> {
    generation_identity(row, index, conn, incremental).map(|identity| identity.hash)
}

#[derive(Clone, Copy)]
struct GenerationIdentity {
    hash: u64,
    content: bool,
}
fn generation_identity(
    row: &rusqlite::Row<'_>,
    index: &impl Fn(&str) -> Option<usize>,
    conn: &Connection,
    incremental_content: bool,
) -> Result<GenerationIdentity> {
    let mut hash = GenerationHash::new();
    hash.text(Some(borrowed_text(row, index("role")).unwrap_or_default()));
    let content_present;
    if incremental_content {
        let mut content_hash = hash.clone();
        let present = if borrowed_text(row, index("content")).is_some_and(|kind| kind == "text") {
            content_hash.write(&[1]);
            super::store::sqlite::with_cell_reader(
                conn,
                "messages",
                "content",
                row.get(index("_content_rowid").unwrap())?,
                |reader, _| {
                    super::store::sqlite::for_each_utf8(reader, |chunk| {
                        content_hash.write(chunk.as_bytes());
                        Ok(())
                    })
                },
            )?
        } else {
            false
        };
        content_present = present;
        if present {
            content_hash.write(&[0xff]);
            hash = content_hash;
        } else {
            hash.text(None);
        }
    } else {
        let content = borrowed_text(row, index("content"));
        content_present = content.is_some();
        hash.text(content);
    }
    hash.number(
        index("timestamp")
            .and_then(|i| row.get::<_, Option<f64>>(i).ok().flatten())
            .map(f64::to_bits),
    );
    for key in ["tool_call_id", "tool_calls", "tool_name"] {
        hash.text(borrowed_text(row, index(key)));
    }
    Ok(GenerationIdentity {
        hash: hash.finish(),
        content: content_present,
    })
}

#[derive(Default)]
struct GenerationDedup {
    index: Option<super::store::sqlite::Selection>,
    #[cfg(test)]
    forced_hash: Option<u64>,
}
impl GenerationDedup {
    fn fingerprint(&self, hash: u64) -> u64 {
        #[cfg(test)]
        if let Some(forced) = self.forced_hash {
            return forced;
        }
        hash
    }
    fn admit(
        &mut self,
        row: &rusqlite::Row<'_>,
        index: &impl Fn(&str) -> Option<usize>,
        conn: &Connection,
        incremental: bool,
    ) -> Result<bool> {
        let mut header = GenerationHash::new();
        header.text(Some(borrowed_text(row, index("role")).unwrap_or_default()));
        for column in ["tool_call_id", "tool_calls", "tool_name"] {
            header.text(borrowed_text(row, index(column)));
        }
        header.number(
            index("timestamp")
                .and_then(|i| row.get::<_, Option<f64>>(i).ok().flatten())
                .map(f64::to_bits),
        );
        let id: i64 = row.get(index(if incremental { "_content_rowid" } else { "id" }).unwrap())?;
        let fingerprint = self.fingerprint(header.finish());
        if self.index.is_none() {
            self.index = Some(super::store::sqlite::Selection::build(
                "CREATE TABLE generations(ordinal INTEGER PRIMARY KEY,header BLOB,body BLOB,native_id INTEGER,content INTEGER); CREATE INDEX generation_membership ON generations(header,body);",
                |_| Ok(()),
            )?);
        }
        let selection = self.index.as_ref().unwrap();
        let header = fingerprint.to_le_bytes();
        use rusqlite::OptionalExtension as _;
        let first = selection.with(|private| {
            let exists: bool = private
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM generations WHERE header=?1)")?
                .query_row([&header], |row| row.get(0))?;
            if !exists {
                // Unique headers do not demand their bodies. Resolve the pending member only
                // when another row shares its header, under the same native snapshot.
                private
                    .prepare_cached("INSERT INTO generations(header,native_id) VALUES (?1,?2)")?
                    .execute(rusqlite::params![header, id])?;
            }
            Ok(!exists)
        })?;
        if first {
            return Ok(true);
        }
        let mut current = generation_identity(row, index, conn, incremental)?;
        current.hash = self.fingerprint(current.hash);
        // Only collided headers need their original content. Re-query small original columns
        // under the same snapshot. Equal digests are a filter, never proof of tuple equality.
        let columns = [
            "role",
            "content",
            "_content_rowid",
            "timestamp",
            "tool_call_id",
            "tool_calls",
            "tool_name",
        ];
        let prior_index = |name: &str| columns.iter().position(|column| *column == name);
        let sql = if incremental {
            "SELECT role,typeof(content),rowid,timestamp,tool_call_id,tool_calls,tool_name FROM messages WHERE rowid=?1"
        } else {
            "SELECT role,content,id,timestamp,tool_call_id,tool_calls,tool_name FROM messages WHERE id=?1"
        };
        #[cfg(test)]
        let forced = self.forced_hash;
        let mut statement = conn.prepare_cached(sql)?;
        selection.with(|private| {
            let pending: Option<i64> = private
                .prepare_cached("SELECT native_id FROM generations WHERE header=?1 AND body IS NULL")?
                .query_row([&header], |row| row.get(0))
                .optional()?;
            if let Some(prior) = pending {
                let key = statement.query_row([prior], |prior_row| {
                    generation_identity(prior_row, &prior_index, conn, incremental)
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
                })?;
                #[cfg(test)]
                let key = GenerationIdentity {
                    hash: forced.unwrap_or(key.hash),
                    ..key
                };
                private
                    .prepare_cached("UPDATE generations SET body=?1,content=?2 WHERE header=?3 AND body IS NULL")?
                    .execute(rusqlite::params![key.hash.to_le_bytes(), key.content, header])?;
            }
            // Digests select candidates, never establish equality. Keep all native decoder
            // normalizations (including f64 bits), then compare demanded bodies exactly.
            let mut candidates = private.prepare_cached(
                "SELECT native_id,content FROM generations WHERE header=?1 AND body=?2 ORDER BY ordinal",
            )?;
            let mut rows = candidates.query(rusqlite::params![header, current.hash.to_le_bytes()])?;
            while let Some(candidate) = rows.next()? {
                let prior: i64 = candidate.get(0)?;
                let content: bool = candidate.get(1)?;
                let equal = statement.query_row([prior], |prior_row| {
                    generation_rows_equal(
                        row,
                        index,
                        prior_row,
                        &prior_index,
                        conn,
                        incremental,
                        (current.content, content),
                    )
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
                })?;
                if equal {
                    return Ok(false);
                }
            }
            drop(rows);
            drop(candidates);
            private
                .prepare_cached("INSERT INTO generations(header,body,native_id,content) VALUES (?1,?2,?3,?4)")?
                .execute(rusqlite::params![
                    header,
                    current.hash.to_le_bytes(),
                    id,
                    current.content
                ])?;
            Ok(true)
        })
    }
}

fn generation_rows_equal(
    current: &rusqlite::Row<'_>,
    current_index: &impl Fn(&str) -> Option<usize>,
    prior: &rusqlite::Row<'_>,
    prior_index: &impl Fn(&str) -> Option<usize>,
    conn: &Connection,
    incremental: bool,
    content: (bool, bool),
) -> Result<bool> {
    let (current_content, prior_content) = content;
    if borrowed_text(current, current_index("role")).unwrap_or_default()
        != borrowed_text(prior, prior_index("role")).unwrap_or_default()
    {
        return Ok(false);
    }
    for column in ["tool_call_id", "tool_calls", "tool_name"] {
        if borrowed_text(current, current_index(column)) != borrowed_text(prior, prior_index(column)) {
            return Ok(false);
        }
    }
    let stamp = |row: &rusqlite::Row<'_>, index: Option<usize>| {
        index
            .and_then(|i| row.get::<_, Option<f64>>(i).ok().flatten())
            .map(f64::to_bits)
    };
    if stamp(current, current_index("timestamp")) != stamp(prior, prior_index("timestamp"))
        || current_content != prior_content
    {
        return Ok(false);
    }
    if !current_content {
        return Ok(true);
    }
    if !incremental {
        return Ok(borrowed_text(current, current_index("content")) == borrowed_text(prior, prior_index("content")));
    }
    super::store::sqlite::with_cell_reader(
        conn,
        "messages",
        "content",
        current.get(current_index("_content_rowid").unwrap())?,
        |left, left_len| {
            super::store::sqlite::with_cell_reader(
                conn,
                "messages",
                "content",
                prior.get(prior_index("_content_rowid").unwrap())?,
                |right, right_len| {
                    if left_len != right_len {
                        return Ok(false);
                    }
                    let mut left_bytes = [0; 64 * 1024];
                    let mut right_bytes = [0; 64 * 1024];
                    let mut remaining = left_len;
                    while remaining != 0 {
                        let take = remaining.min(left_bytes.len());
                        left.read_exact(&mut left_bytes[..take])?;
                        right.read_exact(&mut right_bytes[..take])?;
                        if left_bytes[..take] != right_bytes[..take] {
                            return Ok(false);
                        }
                        remaining -= take;
                    }
                    Ok(true)
                },
            )
        },
    )
}

/// The JSON `messages` columns the row decoder reads as cells.
const CELL_COLS: &[&str] = &[
    "tool_calls",
    "reasoning_details",
    "codex_reasoning_items",
    "codex_message_items",
];

/// Row materialization: the read's tier and spans, and the SQLite cell access mode.
struct Demand {
    tier: Tier,
    spans: bool,
    incremental: bool,
}

impl Demand {
    fn of(demand: &stream::Demand, incremental: bool) -> Self {
        Demand {
            tier: demand.tier,
            spans: demand.spans,
            incremental,
        }
    }
}

/// A row's `content`: text by the codec's rule, or the parts of a `\x00json:` multimodal document.
enum Body {
    Text(crate::Text),
    Blocks(Vec<Block>),
}

/// One `messages` row through the one decoder: plain columns from the row, `content` and the JSON
/// columns from their cells. The demand decides what is resolved, never which code runs.
fn read_row(
    conn: &Connection,
    cells: &Cells,
    row: &rusqlite::Row<'_>,
    idx: &impl Fn(&str) -> Option<usize>,
    demand: &Demand,
) -> Result<MsgRow> {
    let (spans, full) = (demand.spans, demand.tier >= Tier::Rich);
    let get_str =
        |name: &str| -> Option<String> { idx(name).and_then(|i| row.get::<_, Option<String>>(i).ok().flatten()) };
    let get_i64 = |name: &str| -> Option<i64> { idx(name).and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten()) };
    let get_extra = |name| full.then(|| get_str(name)).flatten();
    let cell = |column: &str| cells.cell(conn, "messages", column, row, idx(&format!("{column}#")).unwrap());
    let role = borrowed_text(row, idx("role"));
    let display_kind = get_str("display_kind");
    let compressed_summary = get_i64("_compressed_summary").is_some_and(|n| n != 0);
    let check_failed = role == Some("assistant") && display_kind.is_none();

    let content_at = idx(if demand.incremental {
        "_content_rowid"
    } else {
        "content"
    })
    .unwrap();
    let mut body = None;
    let mut untyped_failed_turn = false;
    {
        if let Some(text) = cells.text(conn, "messages", "content", row, content_at)? {
            let multimodal = role != Some("tool");
            (body, untyped_failed_turn) = cells
                .read(text, |view| content(view, multimodal, check_failed, spans, cells))?
                .unwrap_or_default();
        }
    }

    let calls = match cell("tool_calls")? {
        Some(calls) => cells
            .read(calls, |view| {
                let mut calls = Vec::new();
                if let Some(array) = view.array()? {
                    for call in array {
                        calls.push(openai::tool_call_from_view(call?, spans)?);
                    }
                }
                Ok(calls)
            })?
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // Reasoning: summary text from `reasoning`, `reasoning_content` (unless identical, Hermes' own
    // dedup) and the text entries of `reasoning_details`; the first encrypted handle of
    // `reasoning_details` or else `codex_reasoning_items`. Full demand keeps the JSON sidecars.
    let mut reasoning = Reasoning::default();
    {
        let summary = borrowed_text(row, idx("reasoning"));
        if let Some(summary) = summary {
            reasoning.push(summary);
        }
        if let Some(content) = borrowed_text(row, idx("reasoning_content")).filter(|c| Some(*c) != summary) {
            reasoning.push(content);
        }
        if let Some(details) = cell("reasoning_details")? {
            reasoning.details = cells.read(details, |view| reasoning.read(view, true, full))?.flatten();
            reasoning.present = true;
        }
        if full || reasoning.encrypted.is_none() {
            if let Some(items) = cell("codex_reasoning_items")? {
                reasoning.codex = cells.read(items, |view| reasoning.read(view, false, full))?.flatten();
                reasoning.present = true;
            }
        }
    }
    let thinking = if reasoning.text.is_empty() && reasoning.encrypted.is_none() {
        None
    } else {
        let text = if spans && reasoning.text.len() > crate::lazy::INLINE_MAX {
            let (source, arena) = cells.out();
            super::store::text::append(source, arena, |builder| builder.push(&reasoning.text))?
        } else {
            std::mem::take(&mut reasoning.text).into()
        };
        Some(Block::Thinking {
            text,
            signature: None,
            encrypted: reasoning.encrypted.take(),
            redacted: false,
        })
    };
    let codex_message_items = if full {
        cell("codex_message_items")?
            .map(|items| cells.read(items, |view| Ok(view.resolve()?.into_owned())))
            .transpose()?
            .flatten()
    } else {
        None
    };
    let has_body = match &body {
        Some(Body::Blocks(blocks)) => !blocks.is_empty(),
        Some(Body::Text(_)) => true,
        None => false,
    } || !calls.is_empty()
        || thinking.is_some()
        || role == Some("tool");
    // A metadata-only row is still a message on lean reads, whose extras are not demanded.
    let has_extra = !full
        && !has_body
        && (reasoning.present
            || row_has_extra(row, idx)
            || cell("codex_message_items")?
                .map(|items| cells.read(items, |_| Ok(())))
                .transpose()?
                .flatten()
                .is_some());
    Ok(MsgRow {
        role: get_str("role").unwrap_or_default(),
        body,
        untyped_failed_turn,
        tool_call_id: get_str("tool_call_id"),
        calls,
        thinking,
        has_extra,
        tool_name: get_str("tool_name"),
        timestamp: idx("timestamp").and_then(|i| row.get::<_, Option<f64>>(i).ok().flatten()),
        token_count: get_i64("token_count"),
        finish_reason: get_extra("finish_reason"),
        reasoning: get_extra("reasoning"),
        reasoning_content: get_extra("reasoning_content"),
        reasoning_details: reasoning.details,
        codex_reasoning_items: reasoning.codex,
        codex_message_items,
        platform_message_id: get_extra("platform_message_id"),
        observed: get_i64("observed"),
        effect_disposition: get_extra("effect_disposition"),
        active: get_i64("active"),
        compacted: get_i64("compacted"),
        compressed_summary,
        api_content: get_extra("api_content"),
        display_kind,
        display_metadata: get_extra("display_metadata"),
        message_uid: get_extra("message_uid"),
        absorbed_message_uids: get_extra("absorbed_message_uids"),
        tool_call_uids: get_extra("tool_call_uids"),
        tool_call_uid: get_extra("tool_call_uid"),
    })
}

/// A row's folded reasoning.
#[derive(Default)]
struct Reasoning {
    text: String,
    encrypted: Option<String>,
    /// A `reasoning_details` or `codex_reasoning_items` document was read.
    present: bool,
    details: Option<Value>,
    codex: Option<Value>,
}

impl Reasoning {
    fn push(&mut self, summary: &str) {
        let summary = summary.trim();
        if summary.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(summary);
    }

    /// Fold one reasoning document's items (`summaries` for `reasoning_details`, whose text entries
    /// count too); the owned document on full demand.
    fn read(&mut self, view: crate::json::JsonView<'_>, summaries: bool, full: bool) -> Result<Option<Value>> {
        let string = |v: Option<crate::json::JsonView<'_>>| -> Result<Option<String>> {
            match v {
                Some(v) if v.is_string()? => Ok(v.resolve()?.as_str().map(str::to_owned)),
                _ => Ok(None),
            }
        };
        if let Some(items) = view.array()? {
            for item in items {
                let item = item?;
                if self.encrypted.is_none() {
                    self.encrypted = string(item.get("encrypted_content")?)?;
                    if !summaries && self.encrypted.is_some() {
                        break;
                    }
                }
                if summaries {
                    // The first present key wins, even when it is not a string.
                    let mut summary = None;
                    for key in ["summary", "thinking", "content", "text"] {
                        if let Some(v) = item.get(key)? {
                            summary = Some(v);
                            break;
                        }
                    }
                    if let Some(summary) = string(summary)? {
                        self.push(&summary);
                    }
                }
            }
        }
        Ok(if full { Some(view.resolve()?.into_owned()) } else { None })
    }
}

/// A content string: the parts of a `\x00json:` multimodal document (never for a tool result,
/// whose sentinel is literal output), else the text by the codec's rule. A document that is not
/// valid JSON, or a dict without parts, stays literal text. Also whether the text is the
/// failed-turn notice (when `check_failed`).
fn content(
    view: crate::json::JsonView<'_>,
    multimodal: bool,
    check_failed: bool,
    spans: bool,
    cells: &Cells,
) -> Result<(Option<Body>, bool)> {
    let mut head = String::new();
    view.for_each_char(|c| {
        head.push(c);
        head.len() < MULTIMODAL_SENTINEL.len()
    })?;
    if multimodal && head == MULTIMODAL_SENTINEL {
        let (source, arena) = cells.out();
        let (offset, len) = arena.append(|writer| {
            let mut skip = MULTIMODAL_SENTINEL.len();
            view.for_each_str_chunk(|chunk| {
                let skipped = skip.min(chunk.len());
                skip -= skipped;
                writer.write_all(&chunk.as_bytes()[skipped..])?;
                Ok(())
            })?;
            Ok(())
        })?;
        match source.read(ReadInput::Backing(arena.clone(), offset, len), Validation::Value, |v| {
            multimodal_blocks(v, spans)
        }) {
            Ok(Some(blocks)) => return Ok((Some(Body::Blocks(blocks)), false)),
            Ok(None) => {}
            Err(error) if error.is::<crate::json::InvalidJson>() => {}
            Err(error) => return Err(error),
        }
    }
    let mut failed = false;
    if check_failed {
        let mut notice = FailedNotice::new();
        let mut matching = true;
        view.for_each_char(|c| {
            matching = notice.push(c.encode_utf8(&mut [0; 4]));
            matching
        })?;
        failed = matching && notice.finished();
    }
    Ok((view.lazy_text(spans)?.map(Body::Text), failed))
}

/// A multimodal document's parts: an array, or a dict's `parts` (provider wrappers). None for a
/// dict whose parts are missing or yield nothing (the content stays literal text).
fn multimodal_blocks(view: crate::json::JsonView<'_>, spans: bool) -> Result<Option<Vec<Block>>> {
    let array = view.is_array()?;
    let parts = if array { Some(view) } else { view.get("parts")? };
    let Some(parts) = parts.map(|parts| parts.array()).transpose()?.flatten() else {
        return Ok(None);
    };
    let mut blocks = Vec::new();
    for part in parts {
        blocks.extend(openai::part_from_view(part?, &openai::HERMES, &mut |v| {
            v.lazy_text(spans)
        })?);
    }
    Ok((array || !blocks.is_empty()).then_some(blocks))
}

/// Preserve acceptance of metadata-only rows even when their extras are not demanded: string
/// sidecars need only a presence check.
fn row_has_extra(row: &rusqlite::Row<'_>, index: &impl Fn(&str) -> Option<usize>) -> bool {
    [
        "display_kind",
        "effect_disposition",
        "api_content",
        "display_metadata",
        "message_uid",
        "tool_call_uid",
        "absorbed_message_uids",
        "tool_call_uids",
        "finish_reason",
        "platform_message_id",
        "reasoning",
        "reasoning_content",
    ]
    .iter()
    .any(|key| borrowed_text(row, index(key)).is_some_and(|raw| !raw.is_empty()))
        || ["compacted", "observed"].iter().any(|key| {
            index(key)
                .and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten())
                .is_some_and(|value| value != 0)
        })
        || index("active").and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten()) == Some(0)
}

struct MsgRow {
    role: String,
    body: Option<Body>,
    untyped_failed_turn: bool,
    tool_call_id: Option<String>,
    calls: Vec<Block>,
    thinking: Option<Block>,
    has_extra: bool,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    token_count: Option<i64>,
    finish_reason: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<Value>,
    codex_reasoning_items: Option<Value>,
    codex_message_items: Option<Value>,
    platform_message_id: Option<String>,
    observed: Option<i64>,
    // v11+ columns (NULL / default on older schemas).
    effect_disposition: Option<String>,
    active: Option<i64>,
    compacted: Option<i64>,
    compressed_summary: bool,
    api_content: Option<String>,
    display_kind: Option<String>,
    display_metadata: Option<String>,
    message_uid: Option<String>,
    absorbed_message_uids: Option<String>,
    tool_call_uids: Option<String>,
    tool_call_uid: Option<String>,
}

impl MsgRow {
    /// The IR message for this row. `session_origin` overrides every message's origin when the
    /// session as a whole came from somewhere else (a foreign import).
    fn into_message(self, session_origin: Option<Origin>, tier: Tier) -> Option<Message> {
        let (mut role, mut kind, mut origin) = match self.role.as_str() {
            "assistant" => (Role::Assistant, MessageKind::Reply, Origin::Model),
            "tool" => (Role::Tool, MessageKind::ToolResult, Origin::Harness),
            // The system prompt, stored as a row (older Hermes; `Session::system_prompt` today).
            "system" => (Role::System, MessageKind::SystemPrompt, Origin::Harness),
            // `user`, and anything unknown.
            _ => (Role::User, MessageKind::Prompt, Origin::Human),
        };
        // Harness-injected rows (a hidden compaction handoff, auto-continue nudges, model-switch /
        // delegation / process notices) are System turns typed by what they are, never prompts.
        let display_kind = self
            .display_kind
            .as_deref()
            .or(self.untyped_failed_turn.then_some("failed_turn"));
        if let Some((k, o)) = display_kind.and_then(harness_display_kind) {
            role = Role::System;
            kind = k;
            origin = o;
        }
        // A compaction summary is what seeds the next context window: never a human prompt.
        if self.compressed_summary {
            role = Role::System;
            kind = MessageKind::CompactionSummary;
            origin = Origin::Harness;
        }
        if let Some(o) = session_origin {
            origin = o;
        }
        let mut m = Message::of_kind(role, kind, origin);
        m.timestamp = self.timestamp.and_then(ts_from_epoch);

        // Everything Hermes-specific goes in the `extra["hermes"]` bag, built here and attached once.
        let mut bag = serde_json::Map::new();

        // In-place compaction / rewind flags (non-default values only, so old-schema rows carry
        // nothing): `compacted` = summarized away but still displayed history; `active = false`
        // alone = a rewound or superseded row (only reachable under `complete`).
        if tier >= Tier::Rich && matches!(self.compacted, Some(c) if c != 0) {
            bag.insert("compacted".into(), Value::Bool(true));
        }
        if tier >= Tier::Rich && matches!(self.active, Some(0)) {
            bag.insert("active".into(), Value::Bool(false));
        }
        if let Some(k) = self.display_kind.filter(|s| !s.is_empty() && (tier >= Tier::Facts)) {
            bag.insert("display_kind".into(), Value::String(k));
        }
        if let Some(ed) = self.effect_disposition.filter(|s| !s.is_empty()) {
            bag.insert("effect_disposition".into(), Value::String(ed));
        }
        // `api_content`: the verbatim provider view of this (user) row — what was actually sent, when
        // it differs from the displayed `content` (e.g. injected context). Kept as a sidecar so the
        // block text stays the user's own words.
        if let Some(api) = self.api_content.filter(|s| !s.is_empty()) {
            bag.insert("api_content".into(), Value::String(api));
        }
        if let Some(raw) = self.display_metadata.filter(|s| !s.is_empty()) {
            let v = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::String(raw));
            bag.insert("display_metadata".into(), v);
        }
        // v31 identity columns: plain strings stay strings, the JSON list/map columns are parsed.
        for (key, raw, json) in [
            ("message_uid", self.message_uid, false),
            ("tool_call_uid", self.tool_call_uid, false),
            ("absorbed_message_uids", self.absorbed_message_uids, true),
            ("tool_call_uids", self.tool_call_uids, true),
        ] {
            if let Some(raw) = raw.filter(|s| !s.is_empty()) {
                let v = if json {
                    serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::String(raw))
                } else {
                    Value::String(raw)
                };
                bag.insert(key.into(), v);
            }
        }

        // Per-message token_count → Usage. Hermes records a single combined count per message; we
        // route it to output_tokens for assistant turns and input_tokens otherwise (best-effort).
        if let Some(tc) = self.token_count {
            if tc > 0 {
                let mut u = Usage::default();
                if role == Role::Assistant {
                    u.output_tokens = Some(tc as u64);
                } else {
                    u.input_tokens = Some(tc as u64);
                }
                m.usage = Some(u);
            }
        }

        if let Some(fr) = self.finish_reason.filter(|s| !s.is_empty()) {
            bag.insert("finish_reason".into(), Value::String(fr));
        }
        if let Some(pmid) = self.platform_message_id.filter(|s| !s.is_empty()) {
            bag.insert("platform_message_id".into(), Value::String(pmid));
        }
        if tier >= Tier::Rich && matches!(self.observed, Some(o) if o != 0) {
            bag.insert("observed".into(), Value::Bool(true));
        }
        // Format-complete capture: the reasoning sidecars and the raw reasoning columns verbatim,
        // since the folded `Thinking.text` (merged and deduped across columns) cannot reproduce
        // them for a parse→emit round-trip.
        for (key, value) in [
            ("reasoning_details", self.reasoning_details),
            ("codex_reasoning_items", self.codex_reasoning_items),
            ("codex_message_items", self.codex_message_items),
        ] {
            if let Some(value) = value {
                bag.insert(key.into(), value);
            }
        }
        if let Some(r) = self.reasoning.filter(|s| !s.is_empty()) {
            bag.insert(RAW_REASONING_KEY.into(), Value::String(r));
        }
        if let Some(r) = self.reasoning_content.filter(|s| !s.is_empty()) {
            bag.insert(RAW_REASONING_CONTENT_KEY.into(), Value::String(r));
        }
        if let Some(thinking) = self.thinking {
            m.content.push(thinking);
        }
        if !bag.is_empty() {
            *m.harness_extra_mut(Harness::Hermes) = bag;
        }

        // Tool result rows carry the output in `content`; the tool's name is the block's.
        if role == Role::Tool {
            m.content.push(Block::ToolResult {
                tool_use_id: self.tool_call_id.unwrap_or_default(),
                content: match self.body {
                    Some(Body::Text(text)) => text,
                    _ => crate::Text::default(),
                },
                is_error: false,
                tool_name: self.tool_name,
                status: None,
                details: None,
            });
            return Some(m);
        }
        match self.body {
            Some(Body::Text(text)) => m.content.push(Block::Text { text }),
            Some(Body::Blocks(blocks)) => m.content.extend(blocks),
            None => {}
        }
        m.content.extend(self.calls);

        (!m.content.is_empty() || !m.extra.is_empty() || self.has_extra).then_some(m)
    }
}

#[derive(Default)]
// FAILED_TURN_NOTICES must be ASCII: positions are byte offsets compared to decoded chars.
struct FailedNotice {
    leading: bool,
    positions: [Option<usize>; FAILED_TURN_NOTICES.len()],
}
impl FailedNotice {
    fn new() -> Self {
        Self {
            leading: true,
            positions: [Some(0); FAILED_TURN_NOTICES.len()],
        }
    }
    fn push(&mut self, chunk: &str) -> bool {
        if self.positions.iter().all(Option::is_none) {
            return false;
        }
        for character in chunk.chars() {
            if self.leading && character.is_whitespace() {
                continue;
            }
            self.leading = false;
            for (position, notice) in self.positions.iter_mut().zip(FAILED_TURN_NOTICES) {
                if let Some(current) = *position {
                    if current < notice.len() && character == notice.as_bytes()[current] as char {
                        *position = Some(current + 1);
                    } else if current < notice.len() || !character.is_whitespace() {
                        *position = None;
                    }
                }
            }
            if self.positions.iter().all(Option::is_none) {
                return false;
            }
        }
        true
    }
    fn finished(&self) -> bool {
        self.positions
            .iter()
            .zip(FAILED_TURN_NOTICES)
            .any(|(p, notice)| *p == Some(notice.len()))
    }
}

/// Whole-`Session` convenience over [`stream_conn`] for tests: stream into a [`CollectSink`] and
/// reattach the messages. The production path goes through `Adapter::parse` → `stream::collect`.
#[cfg(test)]
fn parse_conn(conn: &Connection, r: &SessionRef) -> Result<Session> {
    parse_conn_with(conn, r, &stream::Demand::RICH)
}

#[cfg(test)]
fn parse_conn_with(conn: &Connection, r: &SessionRef, opts: &stream::Demand) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_conn(conn, r, opts, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

/// `state_meta` rows `goal:<sid>` (`{goal, status, turns_used, max_turns, …}`), `loop:<sid>`
/// (`{prompt, status, mode, interval_seconds, ticks_fired, …}`) and `heartbeat:<sid>` (`{prompt,
/// interval_seconds, status, fire_count}`) as the session's goal and schedules.
fn state_meta_affordances(conn: &Connection, id: &str) -> Result<Vec<crate::affordance::Affordance>> {
    use crate::affordance::{Affordance, Item, Kind};
    use rusqlite::OptionalExtension as _;
    let mut out = Vec::new();
    for key in ["goal", "loop", "heartbeat"] {
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM state_meta WHERE key = ?1",
                [format!("{key}:{id}")],
                |row| row.get(0),
            )
            .optional()?;
        let Some(v) = value.map(|t| serde_json::from_str::<Value>(&t)).transpose()? else {
            continue;
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
        let n = |k: &str| {
            v.get(k)
                .and_then(Value::as_f64)
                .map(|x| x.to_string())
                .unwrap_or_default()
        };
        let status = s("status");
        out.push(match key {
            "goal" => Affordance::store(
                Kind::Goal,
                "state_meta[goal]",
                format!("{status} · {} of {} turns", n("turns_used"), n("max_turns")),
                vec![Item {
                    content: s("goal").to_string(),
                    status: Some(status.to_string()).filter(|x| !x.is_empty()),
                }],
            ),
            _ => Affordance::store(
                Kind::Schedule,
                format!("state_meta[{key}]"),
                format!("{status} · every {}s · {}", n("interval_seconds"), s("prompt")),
                Vec::new(),
            ),
        });
    }
    Ok(out)
}

// Thread-local observation of existing arena length keeps the consumer connection control
// isolated from concurrent adapter tests. No production counter or shared store hook is needed.
#[cfg(test)]
std::thread_local! {
    static ARENA_WRITE_OBSERVER: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;
    use rusqlite::Connection;

    /// A file under `tests/fixtures/hermes/`: a schema (`schema-*.sql`) or a test's rows
    /// (`<test>.sql`, run in place of the `INSERT`s the test used to spell inline).
    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/hermes/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    /// A store file with a fixture schema: `schema-v14.sql` (hermes_state.py
    /// SCHEMA_VERSION 14), `schema-old.sql` (pre-v14 columns only, for the column-probing
    /// degradation path) or `schema-v30.sql` (a subset of schema 30; see its header).
    fn mk_conn(schema: &str) -> Connection {
        let dir = std::env::temp_dir().join(format!("cv-hermes-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("state.db")).unwrap();
        conn.execute_batch(&fixture(schema)).unwrap();
        conn
    }

    /// A message's `extra["hermes"]` bag (the only place a Hermes message fact may live).
    #[test]
    fn adapter_rejects_source_mutated_by_metadata_callback() {
        let world = cv_testkit::World::new("hermes-checked-read");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(&fixture("schema-v14.sql")).unwrap();
        insert_session(&writer, "t", None, None, 1000.0, None);
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let adapter = Hermes::with_db(path.clone());
        let reference = adapter.discover().unwrap().remove(0);
        struct MutatingSink<'a>(&'a Connection);
        impl MessageSink for MutatingSink<'_> {
            fn meta(&mut self, _: &Session) {
                self.0.execute_batch("CREATE TABLE callback_mutation(value);").unwrap();
            }
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
        }
        let failure = adapter
            .stream(&reference, &stream::Demand::LEAN, &mut MutatingSink(&writer))
            .unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"), "{failure:#}");
    }

    #[test]
    fn incremental_content_matches_typed_view_for_cells_notices_and_generations() -> Result<()> {
        let world = crate::test_world("hermes-incremental-content");
        let path = world.base.join("native.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v30.sql"))?;
        insert_session(&writer, "s", None, None, 1000.0, None);
        let body = format!("{}🌙{}", "x".repeat(65535), "界".repeat(30000));
        for (i, value) in [
            rusqlite::types::Value::Text(body.clone()),
            rusqlite::types::Value::Text(String::new()),
            rusqlite::types::Value::Text("embedded\0NUL".into()),
            rusqlite::types::Value::Text(format!(
                "{}{}{}",
                "\u{2003} ".repeat(20000),
                FAILED_TURN_NOTICES[0],
                "\n\u{2003}".repeat(20000)
            )),
            rusqlite::types::Value::Text(format!("{} quoted", FAILED_TURN_NOTICES[1])),
            rusqlite::types::Value::Blob(b"not a typed String".to_vec()),
            rusqlite::types::Value::Null,
            rusqlite::types::Value::Integer(42),
        ]
        .into_iter()
        .enumerate()
        {
            writer.execute("INSERT INTO messages(session_id,role,content,timestamp,api_content) VALUES('s','assistant',?1,?2,'kept metadata')",rusqlite::params![value,1000+i])?;
        }
        for bytes in [b"invalid\xff".as_slice(), b"truncated\xf0\x9f".as_slice()] {
            writer.execute("INSERT INTO messages(session_id,role,content,timestamp,api_content) VALUES('s','assistant',CAST(?1 AS TEXT),?2,'typed rejection')",rusqlite::params![bytes,writer.last_insert_rowid()+2000])?;
        }
        writer.execute("INSERT INTO messages(session_id,role,content,timestamp,active,compacted) VALUES('s','assistant',?1,3000,0,1)",[&body])?;
        writer.execute("INSERT INTO messages(session_id,role,content,timestamp,active,compacted) VALUES('s','assistant',?1,3000,1,0)",[&body])?;
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('s','assistant',?1,3000)",
            [format!("distinct {body}")],
        )?;
        let fields = [
            "role",
            "content",
            "timestamp",
            "tool_call_id",
            "tool_calls",
            "tool_name",
            "_content_rowid",
        ];
        let index = |name: &str| fields.iter().position(|field| *field == name);
        let mut keys = Vec::new();
        for (projection, incremental) in [("content", false), ("typeof(content)", true)] {
            let mut statement = writer.prepare(&format!(
                "SELECT role,{projection},timestamp,tool_call_id,tool_calls,tool_name,rowid FROM messages ORDER BY id"
            ))?;
            let hashes = statement
                .query_map([], |row| {
                    generation_key(row, &index, &writer, incremental)
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.push(hashes);
        }
        assert_eq!(
            keys[0], keys[1],
            "incremental generation identity matches typed borrowed cells"
        );
        drop(writer);
        // A view selects the old typed payload column and cannot use incremental BLOB access.
        let legacy_path = world.base.join("typed.db");
        std::fs::copy(&path, &legacy_path)?;
        let legacy = Connection::open(&legacy_path)?;
        legacy.execute_batch(
            "ALTER TABLE messages RENAME TO stored_messages; CREATE VIEW messages AS SELECT * FROM stored_messages",
        )?;
        drop(legacy);
        let adapter = Hermes::with_db(path.clone());
        let reference = SessionRef::new("s", Harness::Hermes, &path);
        let typed = Hermes::with_db(legacy_path.clone());
        let typed_ref = SessionRef::new("s", Harness::Hermes, &legacy_path);
        for options in [
            stream::Demand::LEAN,
            stream::Demand::RICH,
            stream::Demand::LEAN.with_spans(),
            stream::Demand::FACTS.with_spans(),
            stream::Demand::COMPLETE,
        ] {
            let mut actual = crate::stream::collect_with(&adapter, &reference, &options)?;
            let mut expected = crate::stream::collect_with(&typed, &typed_ref, &options)?;
            actual.materialize()?;
            expected.materialize()?;
            expected.source_path = actual.source_path.clone();
            assert_eq!(
                serde_json::to_value(actual)?,
                serde_json::to_value(expected)?,
                "{options:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn complete_checked_preparation_preserves_owned_rows_and_fidelity() -> Result<()> {
        let world = crate::test_world("hermes-complete-preparation");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v14.sql"))?;
        writer.execute_batch(
            "ALTER TABLE messages ADD active INTEGER DEFAULT 1; ALTER TABLE messages ADD compacted INTEGER DEFAULT 0;",
        )?;
        insert_session(&writer, "root", None, Some("compression"), 900.0, Some(999.0));
        insert_session(&writer, "s", Some("root"), None, 1000.0, None);
        let body = "ordinary λ\nquoted \"body\"".repeat(20000);
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('root','user',?1,1001)",
            [&body],
        )?;
        for (role, active, compacted) in [("user", 1, 0), ("user", 0, 0), ("assistant", 0, 1), ("tool", 1, 0)] {
            writer.execute("INSERT INTO messages(session_id,role,content,timestamp,active,compacted,reasoning,reasoning_content,reasoning_details,tool_call_id,tool_name) VALUES('s',?1,?2,1001,?3,?4,' raw reasoning ','second','[{\"type\":\"thinking\",\"text\":\"third\"}]','call','Read')", rusqlite::params![role,body,active,compacted])?;
        }
        let failed = format!("{}{}", " ".repeat(4096), FAILED_TURN_NOTICES[0]);
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('s','assistant',?1,1002)",
            [failed],
        )?;
        let multimodal = format!(
            "{MULTIMODAL_SENTINEL}{}",
            serde_json::json!([{"type":"text","text":body}])
        );
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('s','user',?1,1003)",
            [multimodal],
        )?;
        drop(writer);
        let adapter = Hermes::with_db(path.clone());
        let reference = adapter.discover()?.remove(0);
        let owned = crate::stream::collect_with(&adapter, &reference, &stream::Demand::COMPLETE)?;
        let checked = crate::emit::parse_for_emit(&adapter, &reference, Harness::Hermes)?;
        assert_eq!(
            checked.messages.len(),
            7,
            "complete keeps lineage replay, rewound and copied generations"
        );
        assert!(checked
            .messages
            .iter()
            .take(5)
            .all(|message| message.content.iter().any(|block| matches!(
                block,
                Block::Text {
                    text: crate::Text::Source(_)
                } | Block::ToolResult {
                    content: crate::Text::Source(_),
                    ..
                }
            ))));
        assert_eq!(checked.messages[5].kind, owned.messages[5].kind);
        assert!(matches!(
            checked.messages[6].content[0],
            Block::Text {
                text: crate::Text::Source(_)
            }
        ));
        let mut materialized = checked.clone();
        materialized.materialize()?;
        assert_eq!(serde_json::to_value(&materialized)?, serde_json::to_value(&owned)?);
        let options = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let (old, old_report) = crate::emit::emit_report(&owned, Harness::Hermes, &world.base.join("old"), &options)?;
        let (new, new_report) = crate::emit::emit_report(&checked, Harness::Hermes, &world.base.join("new"), &options)?;
        assert_eq!(serde_json::to_value(old_report)?, serde_json::to_value(new_report)?);
        assert_eq!(
            super::super::store::sqlite::export_tests::logical(&Connection::open(old.path)?, Harness::Hermes)?,
            super::super::store::sqlite::export_tests::logical(&Connection::open(new.path)?, Harness::Hermes)?
        );
        // A retained arena body still guards its producer, not just its private backing.
        let writer = Connection::open(&path)?;
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE);")?;
        let wal_checked = crate::emit::parse_for_emit(&adapter, &reference, Harness::Hermes)?;
        let before_main = std::fs::read(&path)?;
        writer.execute("UPDATE messages SET content='changed' WHERE id=1", [])?;
        assert_eq!(std::fs::read(&path)?, before_main, "mutation is WAL-only");
        assert!(crate::emit::emit(&wal_checked, Harness::Hermes, &world.base.join("stale-wal"), &options).is_err());
        assert!(crate::emit::emit(&checked, Harness::Hermes, &world.base.join("stale"), &options).is_err());
        Ok(())
    }

    #[test]
    fn complete_checked_multimodal_matches_owned_acceptance_and_guards_sources() -> Result<()> {
        let world = crate::test_world("hermes-checked-multimodal");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v14.sql"))?;
        insert_session(&writer, "s", None, None, 1000.0, None);
        let body = "large 🌙\n\"quoted\"".repeat(4096);
        let parts = serde_json::json!([
            {"type":"text","text":body},
            {"type":"text","text":""},
            {"type":"image_url","image_url":{"url":"λ\n".repeat(100)}},
            {"type":"unknown","text":"kept"},
            false
        ]);
        let documents = [
            parts.to_string(),
            serde_json::json!({"parts":parts}).to_string(),
            // Duplicate keys use the final Value just as the owned decoder does.
            format!(r#"[{{"text":"discarded","text":{}}}]"#, serde_json::to_string(&body)?),
            format!(r#"[{{"text":{},"ignored":"\ud800"}}]"#, serde_json::to_string(&body)?),
            format!(r#"[{{"text":{},"ignored":1e400}}]"#, serde_json::to_string(&body)?),
            format!(r#"{{"parts":[],"ignored":{}}}"#, serde_json::to_string(&body)?),
            format!(r#"[{{"text":{}}}"#, serde_json::to_string(&body)?),
        ];
        for document in documents {
            writer.execute(
                "INSERT INTO messages(session_id,role,content,timestamp) VALUES('s','user',?1,1001)",
                [format!("{MULTIMODAL_SENTINEL}{document}")],
            )?;
        }
        // A tool result's sentinel is literal output, never decoded as multimodal parts.
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,tool_call_id) VALUES('s','tool',?1,1002,'call')",
            [format!("{MULTIMODAL_SENTINEL}{parts}")],
        )?;
        drop(writer);
        let adapter = Hermes::with_db(path.clone());
        let reference = adapter.discover()?.remove(0);
        let owned = crate::stream::collect_with(&adapter, &reference, &stream::Demand::COMPLETE)?;
        let checked = crate::emit::parse_for_emit(&adapter, &reference, Harness::Hermes)?;
        assert_eq!(checked.messages.len(), 8);
        for message in checked.messages.iter().take(3) {
            assert!(matches!(
                message.content[0],
                Block::Text {
                    text: crate::Text::Source(_)
                }
            ));
        }
        let mut materialized = checked.clone();
        materialized.materialize()?;
        assert_eq!(serde_json::to_value(materialized)?, serde_json::to_value(owned)?);
        std::fs::write(&path, "replaced producer")?;
        let Block::Text { text } = &checked.messages[0].content[0] else {
            unreachable!()
        };
        assert!(text.try_resolve(&checked.resolver()).is_err());
        Ok(())
    }

    #[test]
    fn lean_checked_reads_preserve_facts_dedup_and_actual_consumers() -> Result<()> {
        let world = crate::test_world("hermes-lean-checked");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v30.sql"))?;
        insert_session(&writer, "root", None, Some("compression"), 900.0, Some(999.0));
        insert_session(&writer, "s", Some("root"), None, 1000.0, None);
        let body = "large 🌙\n\"quoted\"".repeat(4096);
        let origin = serde_json::json!({"imported_from":{"tool":"claude"},"ignored":body}).to_string();
        writer.execute("UPDATE sessions SET origin_json=?1, system_prompt='system', cwd='/tmp/project', git_branch='branch' WHERE id='s'", [&origin])?;
        writer.execute_batch(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('root','user','first prompt',1000)",
        )?;
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('root','assistant',?1,1001)",
            [&body],
        )?;
        for (session, timestamp) in [("root", 1002.0), ("s", 1002.5)] {
            writer.execute(
                "INSERT INTO messages(session_id,role,content,timestamp) VALUES(?1,'user',?2,?3)",
                rusqlite::params![session, body, timestamp],
            )?;
        }
        // Same-generation clones are suppressed; different large content at the same timestamp
        // must stay distinct even after both cells become source-backed.
        for (content, active, compacted) in [(&body, 0, 1), (&body, 1, 0), (&format!("distinct {body}"), 1, 0)] {
            writer.execute("INSERT INTO messages(session_id,role,content,timestamp,active,compacted,api_content,display_metadata,token_count,reasoning,reasoning_details) VALUES('s','assistant',?1,1003,?2,?3,?4,?4,7,?1,?5)", rusqlite::params![content,active,compacted,body,serde_json::json!([{"text":" detail ","encrypted_content":"handle","ignored":body}]).to_string()])?;
        }
        writer.execute("INSERT INTO messages(session_id,role,content,timestamp,tool_call_id,tool_name) VALUES('s','tool',?1,1004,'call','terminal')", [&body])?;
        let calls = serde_json::json!([{"id":"call","function":{"name":"terminal","arguments":serde_json::json!({"command":"true","opaque":body}).to_string()}}]).to_string();
        writer.execute(
            "INSERT INTO messages(session_id,role,tool_calls,timestamp) VALUES('s','assistant',?1,1005)",
            [&calls],
        )?;
        writer.execute_batch("INSERT INTO messages(session_id,role,content,timestamp,display_kind) VALUES('s','user','finished',1006,'process_complete')")?;
        writer.execute("INSERT INTO messages(session_id,role,content,timestamp,_compressed_summary) VALUES('s','assistant',?1,1007,1)", [&body])?;
        writer.execute(
            "INSERT INTO messages(session_id,role,api_content,timestamp) VALUES('s','assistant',?1,1008)",
            [&body],
        )?;
        writer.execute(
            "INSERT INTO messages(session_id,role,codex_message_items,timestamp) VALUES('s','assistant',?1,1009)",
            [serde_json::json!({"opaque":body}).to_string()],
        )?;
        writer.execute_batch("INSERT INTO messages(session_id,role,codex_message_items,timestamp) VALUES('s','assistant','{',1010); INSERT INTO messages(session_id,role,content,timestamp,active,compacted) VALUES('s','assistant','rewound',1011,0,0)")?;
        drop(writer);
        let adapter = Hermes::with_db(path.clone());
        let reference = adapter.discover()?.into_iter().find(|r| r.id == "s").unwrap();
        let full = crate::stream::collect_with(&adapter, &reference, &stream::Demand::RICH)?;
        assert_eq!(full.messages.len(), 12);
        assert_eq!(full.lineage.continues.as_deref(), Some("root"));
        assert!(full.messages.iter().all(|message| message.origin == Origin::Import));
        for options in [
            stream::Demand::LEAN,
            stream::Demand::LEAN.with_spans(),
            stream::Demand::USAGE.with_spans(),
            stream::Demand::FACTS.with_spans(),
            stream::Demand::RICH.with_spans(),
        ] {
            let mut actual = crate::stream::collect_with(&adapter, &reference, &options)?;
            if options.spans {
                assert!(matches!(
                    actual.messages[1].content[0],
                    Block::Text {
                        text: crate::Text::Source(_)
                    }
                ));
                assert!(actual.messages.iter().flat_map(|m| &m.content).any(|block| matches!(
                    block,
                    Block::ToolUse {
                        input: crate::Json::Source(_),
                        ..
                    }
                )));
                if options.tier < Tier::Rich {
                    assert!(actual.messages.iter().flat_map(|m| &m.content).any(|block| matches!(
                        block,
                        Block::Thinking {
                            text: crate::Text::Source(_),
                            ..
                        }
                    )));
                }
            }
            actual.materialize()?;
            let mut expected = full.clone();
            if options.tier < Tier::Rich {
                expected.extra.clear();
                for message in &mut expected.messages {
                    let kind = message
                        .harness_extra(Harness::Hermes)
                        .and_then(|bag| bag.get("display_kind"))
                        .cloned();
                    message.extra.retain(|k, _| k == crate::ir::CV_NAMESPACE);
                    if options.tier == Tier::Facts {
                        if let Some(kind) = kind {
                            message
                                .harness_extra_mut(Harness::Hermes)
                                .insert("display_kind".into(), kind);
                        }
                    }
                }
            }
            assert_eq!(
                serde_json::to_value(actual)?,
                serde_json::to_value(expected)?,
                "{options:?}"
            );
        }
        let complete = crate::stream::collect_with(&adapter, &reference, &stream::Demand::COMPLETE)?;
        let mut override_facts =
            crate::stream::collect_with(&adapter, &reference, &stream::Demand::COMPLETE.with_spans())?;
        override_facts.materialize()?;
        assert_eq!(serde_json::to_value(override_facts)?, serde_json::to_value(complete)?);
        assert_eq!(
            serde_json::to_value(crate::compaction::detect(&reference, true)?)?,
            serde_json::to_value(crate::compaction::detect_in_session(&full, true)?)?
        );
        assert_eq!(
            serde_json::to_value(crate::affordance::read(&adapter, &reference)?)?,
            serde_json::to_value(crate::affordance::of_session(&full)?)?
        );
        // Every retained lean body/argument uses the producer read set, including WAL-only writes.
        let writer = Connection::open(&path)?;
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE)")?;
        let lean = crate::stream::collect_with(&adapter, &reference, &stream::Demand::FACTS.with_spans())?;
        let before_main = std::fs::read(&path)?;
        writer.execute("UPDATE messages SET content='changed' WHERE id=2", [])?;
        assert_eq!(std::fs::read(&path)?, before_main);
        let Block::Text { text } = &lean.messages[1].content[0] else {
            unreachable!()
        };
        assert!(text.try_resolve(&lean.resolver()).is_err());
        let input = lean
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| {
                if let Block::ToolUse { input, .. } = b {
                    Some(input)
                } else {
                    None
                }
            })
            .unwrap();
        assert!(input.resolve().is_err());
        Ok(())
    }

    #[test]
    fn replay_dedup_tiny_prompts_stay_off_disk() {
        // The dedup's look-back budget must absorb a long run of empty prompts without spilling:
        // the generic spill budgets this tests against live in `crate::spill`'s own suite.
        let mut dedup = ReplayDedup::default();
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text { text: "".into() });
        for _ in 0..40000 {
            dedup.admit(&message).unwrap();
        }
        assert!(!dedup.recent_user_texts.contains("").unwrap());
        assert!(!dedup.recent_user_texts.is_spilled());
    }

    fn bag(m: &Message) -> &serde_json::Map<String, Value> {
        m.harness_extra(Harness::Hermes).expect("extra[\"hermes\"]")
    }

    /// The session-row facts, `extra["hermes"]["session"]`.
    fn smeta(s: &Session) -> &Value {
        &s.harness_extra(Harness::Hermes).expect("extra[\"hermes\"]")[SESSION_META_KEY]
    }

    /// Every `extra` key, on the session and on each message, is `hermes` (or the carrier's
    /// `_record`): no flat harness keys anywhere.
    /// The shared IR-v2 nesting invariant ([`crate::harness::assert_no_flat_keys`]), tightened for
    /// Hermes: the only namespace a Hermes session may carry is `hermes` (plus cv's own `cv` bag
    /// and the `_record` carrier), so a fact leaking into some other harness's bag also fails.
    fn assert_nested_extra(s: &Session) {
        crate::harness::assert_no_flat_keys(s);
        let allowed =
            |k: &str| k == Harness::Hermes.as_str() || k == crate::ir::CV_NAMESPACE || k == crate::ir::CARRIER_KEY;
        for k in s.extra.keys() {
            assert!(allowed(k), "flat session extra key {k:?}");
        }
        for (i, m) in s.messages.iter().enumerate() {
            for k in m.extra.keys() {
                assert!(allowed(k), "flat extra key {k:?} on message {i}");
            }
        }
    }

    /// The store file a test connection reads; Hermes reads cells against the file they live in.
    fn at(conn: &Connection) -> PathBuf {
        PathBuf::from(conn.path().expect("file store"))
    }

    fn sref(conn: &Connection, id: &str) -> SessionRef {
        SessionRef::new(id, Harness::Hermes, at(conn))
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        parent: Option<&str>,
        end_reason: Option<&str>,
        started: f64,
        ended: Option<f64>,
    ) {
        conn.execute(
            "INSERT INTO sessions (id, source, model, parent_session_id, started_at, ended_at, end_reason, title) \
             VALUES (?1, 'cli', 'nous/hermes-4', ?2, ?3, ?4, ?5, 'T')",
            rusqlite::params![id, parent, started, ended, end_reason],
        )
        .unwrap();
    }

    /// Every corpus version: each session has a certificate, a certificate is deterministic, and
    /// a row written to one session moves its certificate alone, while every session whose
    /// certificate held reads exactly as it did before the write.
    /// Over both corpus stores: discovery, the delegate forests and every session's certificate
    /// walk `sessions` once per store state, and give what the walks of their own gave.
    #[test]
    fn corpus_sessions_are_walked_once_per_store_state() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/hermes");
        for version in std::fs::read_dir(&corpus).unwrap().map(|e| e.unwrap().path()) {
            let world = cv_testkit::World::new("hermes-walk-once");
            let db = world.base.join("state.db");
            std::fs::copy(version.join(".hermes/state.db"), &db).unwrap();
            let adapter = Hermes::with_db(db.clone());
            WALKED.with(|n| n.set(0));
            let refs = adapter.discover().unwrap().refs;
            for r in &refs {
                subagent_refs(r).unwrap();
            }
            let certified = adapter.rows(&db).unwrap();
            assert_eq!(WALKED.with(|n| n.replace(0)), 1, "{version:?}");
            let conn = open_ro(&db).unwrap();
            conn.read(|conn, _| {
                let walk = walk_conn(conn)?;
                assert_eq!(rows_from(conn, &walk)?, certified, "{version:?}");
                let listed: Vec<SessionRef> = refs_of(&walk, &db, false)?.into_iter().map(|(_, r)| r).collect();
                assert_eq!(
                    serde_json::to_string(&listed)?,
                    serde_json::to_string(&refs)?,
                    "{version:?}"
                );
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn corpus_rows_certify_each_session_and_move_only_with_its_rows() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/hermes");
        let mut versions: Vec<_> = std::fs::read_dir(&corpus).unwrap().map(|e| e.unwrap().path()).collect();
        versions.sort();
        assert!(!versions.is_empty());
        for version in versions {
            let world = cv_testkit::World::new("hermes-corpus-rows");
            let db = world.base.join("state.db");
            std::fs::copy(version.join(".hermes/state.db"), &db).unwrap();
            let conn = Connection::open(&db).unwrap();
            let before = rows_conn(&conn).unwrap();
            assert_eq!(rows_conn(&conn).unwrap(), before, "{version:?}");
            let refs = discover_conn(&conn, &db).unwrap();
            assert!(!refs.is_empty());
            for r in &refs {
                assert!(before.contains_key(&r.id), "{version:?} {}", r.id);
            }
            let read = |conn: &Connection, r: &SessionRef| {
                serde_json::to_string(&parse_conn_with(conn, r, &stream::Demand::RICH).unwrap().messages).unwrap()
            };
            let reads: Vec<String> = refs.iter().map(|r| read(&conn, r)).collect();
            // A session no other session's read consults: no parent, no children.
            let parents: HashSet<String> = conn
                .prepare("SELECT parent_session_id FROM sessions WHERE parent_session_id IS NOT NULL")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let leaf = refs.iter().find(|r| !parents.contains(&r.id)).expect("a leaf session");
            conn.execute(
                "INSERT INTO messages(session_id, role, content, timestamp) VALUES(?1, 'user', 'zqcertified', 9e9)",
                [&leaf.id],
            )
            .unwrap();
            let after = rows_conn(&conn).unwrap();
            assert_eq!(after[&leaf.id].count, before[&leaf.id].count + 1, "{version:?}");
            assert!(after[&leaf.id].high > before[&leaf.id].high, "{version:?}");
            for (r, old) in refs.iter().zip(&reads) {
                if r.id == leaf.id {
                    assert_ne!(read(&conn, r), *old);
                    continue;
                }
                assert_eq!(after[&r.id], before[&r.id], "{version:?} {}", r.id);
                assert_eq!(
                    read(&conn, r),
                    *old,
                    "{version:?} {}: certified unchanged, read changed",
                    r.id
                );
            }
        }
    }

    /// Every corpus version, listed as `cv ls` lists it: cold (nothing counted yet), then after
    /// full reads recorded each session's count (the certified counts list), then with the store
    /// replaced by a fresh copy of the same bytes, as the dump-hash stage rebuilds it: that is
    /// another store, so the listing is the cold one again, not the counts certified for the old
    /// file's rows. The copy is created after the old file is unlinked and renamed over its path,
    /// so a filesystem that reuses freed inodes (ext4) gives it the old inode number.
    #[test]
    fn corpus_listing_of_a_replaced_store_is_the_cold_listing() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/hermes");
        let mut versions: Vec<_> = std::fs::read_dir(&corpus).unwrap().map(|e| e.unwrap().path()).collect();
        versions.sort();
        for version in versions {
            let world = crate::test_world("hermes-cold-warm");
            let home = world.home.join(".hermes");
            std::fs::create_dir_all(&home).unwrap();
            let db = home.join("state.db");
            let stage = || {
                let _ = std::fs::remove_file(&db);
                let next = home.join("state.db.next");
                std::fs::copy(version.join(".hermes/state.db"), &next).unwrap();
                std::fs::rename(&next, &db).unwrap();
            };
            let listing = || {
                let mut rows: Vec<(String, Option<usize>)> = crate::sessions()
                    .into_iter()
                    .filter(|r| r.harness == Harness::Hermes)
                    .map(|r| (r.id, r.message_count))
                    .collect();
                rows.sort();
                rows
            };
            stage();
            let cold = listing();
            assert!(!cold.is_empty(), "{version:?}");
            let adapter = Hermes::with_home(&home);
            for r in adapter.discover().unwrap().refs {
                crate::stream::collect_with(&adapter, &r, &stream::Demand::RICH).unwrap();
            }
            assert_ne!(listing(), cold, "{version:?}: full reads certify counts on this store");
            stage();
            assert_eq!(listing(), cold, "{version:?}: a replaced store lists as a cold one");
        }
    }

    #[test]
    fn parses_basic_text_conversation() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute_batch(&fixture("parses_basic_text_conversation.sql"))
            .unwrap();

        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("Hello"));
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(
            (s.messages[1].kind, s.messages[1].origin),
            (MessageKind::Reply, Origin::Model)
        );
        assert_eq!(s.messages[1].text().as_deref(), Some("Hi there!"));
        // token_count → Usage.output_tokens for assistant.
        assert_eq!(s.messages[1].usage.as_ref().unwrap().output_tokens, Some(42));
        assert_eq!(bag(&s.messages[1])["finish_reason"], "stop");
        assert_eq!(s.model.as_deref(), Some("nous/hermes-4"));
        assert!(s.messages[0].extra.is_empty(), "a plain prompt carries no Hermes facts");
        assert_nested_extra(&s);
    }

    #[test]
    fn system_row_is_the_system_prompt() {
        // Older Hermes stored the system prompt as a `system` row; it is `SystemPrompt`, not a notice,
        // and the session-level copy comes from the column.
        let conn = mk_conn("schema-v14.sql");
        conn.execute_batch(&fixture("system_row_is_the_system_prompt.sql"))
            .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are Hermes"));
        assert_eq!(s.messages[0].role, Role::System);
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::SystemPrompt, Origin::Harness)
        );
        assert_eq!(s.messages[1].kind, MessageKind::Prompt);
    }

    #[test]
    fn decodes_multimodal_sentinel() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let content = "\u{0}json:[{\"type\":\"text\",\"text\":\"see this\"},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,AAAA\"}}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        assert_eq!(s.messages.len(), 1);
        let blocks = &s.messages[0].content;
        assert!(matches!(&blocks[0], Block::Text { text } if text.matches_str("see this") == Some(true)));
        assert!(matches!(&blocks[1], Block::Image { data_ref: Some(u), .. } if u.starts_with("data:image/png")));
    }

    #[test]
    fn decodes_input_image_string_form() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        // input_image with image_url as a bare string (Responses API form).
        let content = "\u{0}json:[{\"type\":\"input_text\",\"text\":\"hi\"},{\"type\":\"input_image\",\"image_url\":\"data:image/jpeg;base64,ZZZ\"}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        let blocks = &s.messages[0].content;
        assert!(matches!(&blocks[0], Block::Text { text } if text.matches_str("hi") == Some(true)));
        assert!(matches!(&blocks[1], Block::Image { data_ref: Some(u), .. } if u.starts_with("data:image/jpeg")));
    }

    #[test]
    fn decodes_dict_shaped_content() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let content = "\u{0}json:{\"parts\":[{\"text\":\"hi\"}]}";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));
    }

    #[test]
    fn parses_tool_call_and_result() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute_batch(&fixture("parses_tool_call_and_result.sql")).unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        let tu = &s.messages[0].content[0];
        assert!(matches!(tu, Block::ToolUse { name, id, .. } if name == "web_search" && id == "c1"));
        if let Block::ToolUse { input, .. } = tu {
            assert_eq!(input.resolve().unwrap().get("q").and_then(Value::as_str), Some("x"));
        }
        let tr = &s.messages[1].content[0];
        assert!(
            matches!(tr, Block::ToolResult { tool_use_id, tool_name, .. } if tool_use_id == "c1" && tool_name.as_deref() == Some("web_search")),
            "the tool name lives on the block, not in extra"
        );
        assert_eq!(s.messages[1].role, Role::Tool);
        assert_eq!(
            (s.messages[1].kind, s.messages[1].origin),
            (MessageKind::ToolResult, Origin::Harness)
        );
        assert!(s.messages[1].extra.is_empty());
    }

    #[test]
    fn maps_all_reasoning_variants() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute_batch(&fixture("maps_all_reasoning_variants.sql")).unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        let think = s.messages[0]
            .content
            .iter()
            .find_map(|b| match b {
                Block::Thinking { text, encrypted, .. } => Some((text.clone(), encrypted.clone())),
                _ => None,
            })
            .unwrap();
        assert!(think.0.contains("short summary"));
        assert!(think.0.contains("native scratchpad"));
        assert!(think.0.contains("summarised"));
        assert_eq!(think.1.as_deref(), Some("ENC123"));
        // structured blob and the raw source columns preserved in the Hermes bag
        let b = bag(&s.messages[0]);
        assert!(b.contains_key("reasoning_details"));
        assert_eq!(b[RAW_REASONING_KEY], "short summary");
        assert_eq!(b[RAW_REASONING_CONTENT_KEY], "native scratchpad");
        assert_nested_extra(&s);
    }

    #[test]
    fn reasoning_content_not_duplicated_when_equal() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute_batch(&fixture("reasoning_content_not_duplicated_when_equal.sql"))
            .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        let text = s.messages[0]
            .content
            .iter()
            .find_map(|b| match b {
                Block::Thinking { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(text.inline_str(), Some("same"));
    }

    #[test]
    fn maps_codex_items() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute_batch(&fixture("maps_codex_items.sql")).unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        let enc = s.messages[0].content.iter().find_map(|b| match b {
            Block::Thinking { encrypted, .. } => encrypted.clone(),
            _ => None,
        });
        assert_eq!(enc.as_deref(), Some("BLOB"));
        assert!(bag(&s.messages[0]).contains_key("codex_reasoning_items"));
        assert!(bag(&s.messages[0]).contains_key("codex_message_items"));
    }

    #[test]
    fn platform_message_id_and_observed() {
        let conn = mk_conn("schema-v14.sql");
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute_batch(&fixture("platform_message_id_and_observed.sql"))
            .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        assert_eq!(bag(&s.messages[0])["platform_message_id"], "abc-123");
        assert_eq!(bag(&s.messages[0])["observed"], Value::Bool(true));
    }

    #[test]
    fn walks_compression_chain_and_dedups_replayed_user() {
        let conn = mk_conn("schema-v14.sql");
        // root ended via compression at t=2000; child created after, replays the last user msg.
        insert_session(&conn, "root", None, Some("compression"), 1000.0, Some(2000.0));
        insert_session(&conn, "child", Some("root"), None, 2001.0, None);
        conn.execute_batch(&fixture("walks_compression_chain_and_dedups_replayed_user.sql"))
            .unwrap();

        // Parsing the CHILD should walk up to the root and merge, deduping the replayed user msg.
        let s = parse_conn(&conn, &sref(&conn, "child")).unwrap();
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["first prompt", "first answer", "second prompt", "second answer"]
        );
    }

    #[test]
    fn old_schema_missing_columns_still_parses() {
        let conn = mk_conn("schema-old.sql");
        conn.execute_batch(&fixture("old_schema_missing_columns_still_parses.sql"))
            .unwrap();
        // Must not error despite missing reasoning/token/codex/parent columns.
        let s = parse_conn(&conn, &sref(&conn, "s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hello old"));
        assert!(matches!(&s.messages[1].content[0], Block::ToolUse { name, .. } if name == "f"));
        // discover also works on the old schema.
        let refs = discover_conn(&conn, &at(&conn)).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "s1");
    }

    /// Branch-don't-fork: the adapter must discover sessions in BOTH the top-level `state.db` AND
    /// every `profiles/<name>/state.db`. Regression guard for the bug where `Hermes::detect()` bound a
    /// single `<home>/state.db` and left the active profile's sessions (2782 on the real install)
    /// invisible. Synthesizes a minimal `~/.hermes`-shaped tree under a temp `HERMES_HOME`.
    #[test]
    fn discovers_top_level_and_profile_dbs() {
        let home = std::env::temp_dir().join(format!("cv-hermes-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("profiles/hermes-x")).unwrap();
        // A pre-update snapshot dir at a DEEPER level must NOT be picked up (depth-1 read_dir only).
        std::fs::create_dir_all(home.join("profiles/state-snapshots/old")).unwrap();

        // helper: create a state.db with one named session.
        let mk_db = |path: &std::path::Path, sid: &str| {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(&fixture("schema-v14.sql")).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, source, model, started_at, message_count, title) \
                 VALUES (?1,'cli','nous/hermes-4',1000.0,1,?2)",
                rusqlite::params![sid, sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1,'user','hi',1001.0)",
                [sid],
            )
            .unwrap();
        };
        mk_db(&home.join("state.db"), "top-session");
        mk_db(&home.join("profiles/hermes-x/state.db"), "profile-session");
        // A snapshot DB deeper than depth-1 — must stay hidden.
        mk_db(&home.join("profiles/state-snapshots/old/state.db"), "snapshot-session");

        // Point the adapter at our synthetic home and discover.
        let env = crate::env::Env::from_vars([("HERMES_HOME", &home)]).enter();
        let hermes = Hermes::detect();
        // Two readable DBs: top-level + the one profile (snapshot is depth-2, skipped).
        assert_eq!(hermes.dbs.len(), 2, "should bind top-level + profile DB only");
        let refs = hermes.discover().unwrap();
        drop(env);

        let ids: HashSet<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains("top-session"), "top-level session must be discovered");
        assert!(
            ids.contains("profile-session"),
            "profile session must be discovered (the bug)"
        );
        assert!(
            !ids.contains("snapshot-session"),
            "depth-2 snapshot DB must NOT be discovered"
        );

        // parse() must route by the ref's own DB path, so the profile session parses from its DB.
        let pref = refs.iter().find(|r| r.id == "profile-session").unwrap();
        let s = hermes.parse(pref).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));

        let _ = std::fs::remove_dir_all(&home);
    }

    /// Insert a v30 message row with explicit flags; returns nothing (ids are AUTOINCREMENT in order).
    fn insert_v30(conn: &Connection, sid: &str, role: &str, content: &str, ts: f64, active: i64, compacted: i64) {
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, active, compacted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![sid, role, content, ts, active, compacted],
        )
        .unwrap();
    }

    fn texts(s: &Session) -> Vec<String> {
        s.messages.iter().filter_map(|m| m.text()).collect()
    }

    #[test]
    fn orders_by_id_not_timestamp() {
        // Hermes reads `ORDER BY id`: timestamps are not monotonic and would split a tool call from
        // its result (hermes_state_messages.py:42).
        let conn = mk_conn("schema-v30.sql");
        insert_session(&conn, "s", None, None, 1000.0, None);
        insert_v30(&conn, "s", "user", "later stamp, first row", 2000.0, 1, 0);
        insert_v30(&conn, "s", "assistant", "earlier stamp, second row", 1500.0, 1, 0);
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        assert_eq!(texts(&s), vec!["later stamp, first row", "earlier stamp, second row"]);
    }

    #[test]
    fn in_place_compaction_display_view() {
        // archive_and_compact: summarized rows → active=0,compacted=1 (still history); the summary is
        // a fresh active row with _compressed_summary=1; the carried tail is cloned (same content +
        // timestamp, new id) and superseded originals / rewound rows get active=0,compacted=0.
        let conn = mk_conn("schema-v30.sql");
        insert_session(&conn, "s", None, None, 1000.0, None);
        insert_v30(&conn, "s", "user", "q1", 1001.0, 0, 1); // id 1: summarized away
        insert_v30(&conn, "s", "assistant", "a1", 1002.0, 0, 1); // id 2
        insert_v30(&conn, "s", "user", "q2", 1003.0, 0, 1); // id 3: older generation of the carried tail
        insert_v30(&conn, "s", "user", "typo", 1003.5, 0, 0); // id 4: rewound — never displayed
        conn.execute_batch(&fixture("in_place_compaction_display_view.sql"))
            .unwrap(); // id 5: the summary
        insert_v30(&conn, "s", "user", "q2", 1003.0, 1, 0); // id 6: live clone of id 3
        insert_v30(&conn, "s", "assistant", "a2", 1005.0, 1, 0); // id 7

        // Lean: the display projection, generation-deduped, summary as boundary + summary.
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        assert_eq!(
            texts(&s),
            vec!["q1", "a1", "q2", "[CONTEXT COMPACTION] earlier turns…", "a2"]
        );
        assert!(
            !texts(&s).contains(&"typo".to_string()),
            "rewound rows are not displayed"
        );
        assert_eq!(bag(&s.messages[0])["compacted"], Value::Bool(true));
        assert_eq!(s.messages[0].role, Role::User, "summarized-away history keeps its role");
        assert_eq!(s.messages[0].kind, MessageKind::Prompt);
        let boundary = &s.messages[3];
        assert_eq!(boundary.role, Role::System);
        assert_eq!(
            (boundary.kind, boundary.origin),
            (MessageKind::CompactionBoundary, Origin::Harness)
        );
        assert!(
            boundary.content.is_empty() && boundary.id.is_none(),
            "cv invents no text, no id"
        );
        assert!(boundary.is_synthesized(), "the boundary is cv's, and says so");
        assert!(
            boundary.harness_extra(Harness::Hermes).is_none(),
            "no harness facts on it"
        );
        let summary = &s.messages[4];
        assert_eq!(summary.role, Role::System);
        assert_eq!(
            (summary.kind, summary.origin),
            (MessageKind::CompactionSummary, Origin::Harness)
        );
        assert_eq!(
            summary.parent_id, None,
            "no invented link: the detector pairs by adjacency"
        );
        assert_eq!(bag(summary)["display_kind"], "hidden");
        assert!(!bag(summary).contains_key("isCompactSummary"), "the kind carries it");
        // the shared detector sees one compaction with its summary
        let found = crate::compaction::detect_in_session(&s, true).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].summary.as_deref().is_some_and(|t| t.contains("earlier turns")));
        assert_nested_extra(&s);

        // Complete: every row, flags tagged, no dedup.
        let c = parse_conn_with(&conn, &sref(&conn, "s"), &stream::Demand::COMPLETE).unwrap();
        assert_eq!(c.messages.len(), 8, "7 rows + 1 boundary marker");
        let typo = c.messages.iter().find(|m| m.text().as_deref() == Some("typo")).unwrap();
        assert_eq!(bag(typo)["active"], Value::Bool(false));
        assert!(!bag(typo).contains_key("compacted"));
        assert_eq!(
            c.messages.iter().filter(|m| m.text().as_deref() == Some("q2")).count(),
            2
        );
    }

    #[test]
    fn system_prompt_resolves_via_system_prompts_table() {
        // v25 hollowed `sessions.system_prompt` out into `system_prompts(hash, prompt)`.
        let conn = mk_conn("schema-v30.sql");
        conn.execute_batch(&fixture("system_prompt_resolves_via_system_prompts_table.sql"))
            .unwrap();
        insert_v30(&conn, "s", "user", "hi", 1001.0, 1, 0);
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are Hermes"));
        assert!(
            !smeta(&s).as_object().unwrap().contains_key("system_prompt"),
            "the system prompt is first-class, not a session fact"
        );
        // the old direct column still wins on a v14 DB
        let old = mk_conn("schema-v14.sql");
        old.execute_batch(&fixture("system_prompt_resolves_via_system_prompts_table-2.sql"))
            .unwrap();
        let s = parse_conn(&old, &sref(&old, "s")).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("legacy prompt"));
    }

    #[test]
    fn subagent_refs_are_the_delegates_of_the_whole_compression_chain() {
        let conn = mk_conn("schema-v30.sql");
        // `r` compressed into `t`; one delegate was spawned from each segment, one from elsewhere.
        insert_session(&conn, "r", None, Some("compression"), 1000.0, Some(1100.0));
        insert_session(&conn, "t", Some("r"), None, 1100.0, None);
        insert_session(&conn, "x", None, None, 1200.0, None);
        for (id, from, at) in [("d1", "r", 1050.0), ("d2", "t", 1150.0), ("dx", "x", 1250.0)] {
            insert_session(&conn, id, Some(from), Some("agent_close"), at, Some(at + 1.0));
            conn.execute(
                "UPDATE sessions SET model_config = json_object('_delegate_from', ?2), source = 'subagent' WHERE id = ?1",
                [id, from],
            )
            .unwrap();
            insert_v30(&conn, id, "user", "delegated", at, 1, 0);
        }
        let dir = std::env::temp_dir().join(format!("cv-hermes-subagent-refs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let _ = std::fs::remove_file(&db);
        conn.execute("VACUUM INTO ?1", [db.to_str().unwrap()]).unwrap();

        let refs = Hermes::with_db(db.clone()).discover().unwrap();
        let mut ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["t", "x"],
            "discovery lists conversations, not delegates or ancestors"
        );
        let tip = refs.iter().find(|r| r.id == "t").unwrap();
        let kids = subagent_refs(tip).unwrap();
        assert_eq!(kids.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["d1", "d2"]);
        assert!(kids.iter().all(|k| k.path == db && k.harness == Harness::Hermes));
        // The forest is the same delegates, newest first.
        let mut forest: Vec<String> = crate::subagent_tree_of(tip)
            .unwrap()
            .into_iter()
            .map(|a| a.session.id)
            .collect();
        forest.sort();
        assert_eq!(forest, ["d1", "d2"]);
        let x = refs.iter().find(|r| r.id == "x").unwrap();
        let opens = || super::super::store::sqlite::DERIVE_OPENS.with(std::cell::Cell::get);
        let before = opens();
        assert_eq!(
            subagent_refs(x)
                .unwrap()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["dx"]
        );
        assert_eq!(
            opens(),
            before,
            "an unchanged db answers every conversation from one scan"
        );
        // a child parses as its own conversation that names its parent
        let d1 = Hermes::with_db(db.clone()).parse(&kids[0]).unwrap();
        assert_eq!(d1.lineage.parent.as_deref(), Some("r"));
        // The delegate scan is kept per db state: a delegate written after it is still seen.
        let live = Connection::open(&db).unwrap();
        insert_session(&live, "d3", Some("t"), Some("agent_close"), 1160.0, Some(1161.0));
        live.execute(
            "UPDATE sessions SET model_config = json_object('_delegate_from', 't'), source = 'subagent' WHERE id = 'd3'",
            [],
        )
        .unwrap();
        drop(live);
        let kids = subagent_refs(tip).unwrap();
        assert_eq!(
            kids.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["d1", "d2", "d3"]
        );
        assert_eq!(opens(), before + 1, "a written db is scanned again");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn branches_and_delegates_stand_alone_but_compression_still_merges() {
        let conn = mk_conn("schema-v30.sql");
        // /branch: parent ended 'branched'; the child owns a COPY of the transcript.
        insert_session(&conn, "p", None, Some("branched"), 1000.0, Some(1500.0));
        insert_v30(&conn, "p", "user", "shared prompt", 1001.0, 1, 0);
        conn.execute_batch(&fixture(
            "branches_and_delegates_stand_alone_but_compression_still_merges.sql",
        ))
        .unwrap();
        insert_v30(&conn, "b", "user", "shared prompt", 1001.0, 1, 0); // the copied row
        insert_v30(&conn, "b", "assistant", "branch answer", 1601.0, 1, 0);
        let b = parse_conn(&conn, &sref(&conn, "b")).unwrap();
        assert_eq!(
            texts(&b),
            vec!["shared prompt", "branch answer"],
            "no parent merge, no duplicate"
        );
        assert_eq!(
            b.lineage,
            Lineage {
                forked_from: Some("p".into()),
                ..Default::default()
            }
        );
        assert!(
            !b.harness_extra(Harness::Hermes)
                .unwrap()
                .contains_key("parent_session_id"),
            "a marked parent is already in the lineage"
        );
        assert_eq!(b.model.as_deref(), None, "markers never leak into the model name");

        // delegate sub-agent run: separate conversation whose parent spawned it.
        conn.execute_batch(&fixture(
            "branches_and_delegates_stand_alone_but_compression_still_merges-2.sql",
        ))
        .unwrap();
        insert_v30(&conn, "d", "user", "delegate task", 1701.0, 1, 0);
        let d = parse_conn(&conn, &sref(&conn, "d")).unwrap();
        assert_eq!(texts(&d), vec!["delegate task"]);
        assert_eq!(
            d.lineage,
            Lineage {
                parent: Some("p".into()),
                ..Default::default()
            }
        );

        // reset child: a fresh conversation; the marker is a Hermes fact only.
        conn.execute_batch(&fixture(
            "branches_and_delegates_stand_alone_but_compression_still_merges-3.sql",
        ))
        .unwrap();
        insert_v30(&conn, "r", "user", "fresh", 1801.0, 1, 0);
        let r = parse_conn(&conn, &sref(&conn, "r")).unwrap();
        assert!(r.lineage.is_empty(), "{:?}", r.lineage);
        assert_eq!(r.harness_extra(Harness::Hermes).unwrap()["_reset_from"], "p");

        // compression continuation: parent ended in compression → merged root→tip, and the store's
        // pointers survive both ways.
        insert_session(&conn, "c0", None, Some("compression"), 2000.0, Some(2500.0));
        insert_v30(&conn, "c0", "user", "long ago", 2001.0, 1, 0);
        insert_session(&conn, "c1", Some("c0"), None, 2600.0, None);
        insert_v30(&conn, "c1", "assistant", "continued", 2601.0, 1, 0);
        let c1 = parse_conn(&conn, &sref(&conn, "c1")).unwrap();
        assert_eq!(texts(&c1), vec!["long ago", "continued"]);
        assert_eq!(c1.lineage.continues.as_deref(), Some("c0"));
        assert_eq!(c1.lineage.continued_in, None);
        let c0 = parse_conn(&conn, &sref(&conn, "c0")).unwrap();
        assert_eq!(c0.lineage.continued_in.as_deref(), Some("c1"));
        assert_eq!(c0.lineage.continues, None);
        assert!(
            !c1.harness_extra(Harness::Hermes)
                .unwrap()
                .contains_key("parent_session_id"),
            "a compression parent is in the lineage, not the bag"
        );
    }

    #[test]
    fn discovery_mirrors_hermes_listing() {
        let conn = mk_conn("schema-v30.sql");
        insert_session(&conn, "root", None, Some("compression"), 1000.0, Some(1500.0));
        insert_session(&conn, "tip", Some("root"), None, 1600.0, None);
        conn.execute_batch(&fixture("discovery_mirrors_hermes_listing.sql"))
            .unwrap();
        let refs = discover_conn(&conn, &at(&conn)).unwrap();
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["plain", "hid", "arch", "tip"],
            "tip lists (root is its compression ancestor) and so do archived/hidden (Hermes keeps \
             them resumable); the delegate run does not"
        );
        assert_eq!(
            refs.iter().find(|r| r.id == "tip").unwrap().title.as_deref(),
            Some("T"),
            "an untitled tip is titled by its compression root"
        );
        let plain = &refs[0];
        assert_eq!(plain.cwd.as_deref(), Some(Path::new("/work/proj")));
        assert_eq!(
            plain.updated_at,
            ts_from_epoch(5000.0),
            "last_activity_at beats ended_at"
        );
        // a v14 DB is listed exactly as before (no listing columns → no filters)
        let old = mk_conn("schema-v14.sql");
        insert_session(&old, "a", None, None, 1000.0, None);
        assert_eq!(discover_conn(&old, &at(&old)).unwrap().len(), 1);
    }

    #[test]
    fn session_cwd_git_and_activity_are_first_class() {
        let conn = mk_conn("schema-v30.sql");
        conn.execute_batch(&fixture("session_cwd_git_and_activity_are_first_class.sql"))
            .unwrap();
        insert_v30(&conn, "s", "user", "hi", 1001.0, 1, 0);
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work/proj")));
        assert_eq!(s.git.as_ref().and_then(|g| g.branch.as_deref()), Some("main"));
        assert_eq!(s.updated_at, ts_from_epoch(3000.0));
        assert_eq!(smeta(&s)["git_repo_root"], "/work/proj");
        assert_eq!(smeta(&s)["profile_name"], "hermes-x");
        assert_nested_extra(&s);
    }

    #[test]
    fn v31_identity_columns_and_v29_v30_session_columns_ride_in_extra() {
        // 0.21.6 (schema 31) adds them by ALTER TABLE, as Hermes's reconcile does.
        let conn = mk_conn("schema-v30.sql");
        conn.execute_batch(
            "ALTER TABLE messages ADD COLUMN message_uid TEXT; \
             ALTER TABLE messages ADD COLUMN absorbed_message_uids TEXT; \
             ALTER TABLE messages ADD COLUMN tool_call_uids TEXT; \
             ALTER TABLE messages ADD COLUMN tool_call_uid TEXT; \
             ALTER TABLE sessions ADD COLUMN created_source TEXT; \
             ALTER TABLE sessions ADD COLUMN auto_archived INTEGER NOT NULL DEFAULT 0;",
        )
        .unwrap();
        insert_session(&conn, "s", None, None, 1000.0, None);
        conn.execute_batch(
            "UPDATE sessions SET created_source = 'oneshot', auto_archived = 1 WHERE id = 's'; \
             INSERT INTO messages (session_id, role, content, timestamp, message_uid, absorbed_message_uids) \
               VALUES ('s', 'user', 'hi', 1001.0, 'u1', '[\"u0\"]'); \
             INSERT INTO messages (session_id, role, content, tool_calls, timestamp, message_uid, tool_call_uids) \
               VALUES ('s', 'assistant', '', '[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"terminal\",\"arguments\":\"{}\"}}]', 1002.0, 'u2', '{\"call_1\":\"t1\"}'); \
             INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp, message_uid, tool_call_uid) \
               VALUES ('s', 'tool', 'ok', 'call_1', 'terminal', 1003.0, 'u3', 't1');",
        )
        .unwrap();
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        assert_eq!(bag(&s.messages[0])["message_uid"], "u1");
        assert_eq!(bag(&s.messages[0])["absorbed_message_uids"], serde_json::json!(["u0"]));
        assert_eq!(bag(&s.messages[1])["tool_call_uids"]["call_1"], "t1");
        assert_eq!(bag(&s.messages[2])["tool_call_uid"], "t1");
        assert_eq!(smeta(&s)["created_source"], "oneshot");
        assert_eq!(smeta(&s)["auto_archived"], 1);
    }

    #[test]
    fn a_failed_turn_boundary_is_an_error_typed_or_not() {
        // 0.21 types the boundary row `failed_turn`; earlier stores carry only its exact text, and a
        // reply that merely quotes the notice stays a reply.
        let conn = mk_conn("schema-v30.sql");
        insert_session(&conn, "s", None, None, 1000.0, None);
        let notice = FAILED_TURN_NOTICES[0];
        for (content, kind) in [
            (notice, Some("failed_turn")),
            (FAILED_TURN_NOTICES[1], None),
            ("I said: Your request was not processed.", None),
        ] {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp, display_kind) VALUES ('s', 'assistant', ?1, 1001.0, ?2)",
                rusqlite::params![content, kind],
            )
            .unwrap();
        }
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        let kinds: Vec<(Role, MessageKind, Origin)> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::Error, Origin::Harness),
                (Role::System, MessageKind::Error, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(bag(&s.messages[0])["display_kind"], "failed_turn");
        assert!(
            s.messages[1]
                .extra
                .get("hermes")
                .is_none_or(|b| b.get("display_kind").is_none()),
            "an inferred kind is not written back as a column value"
        );
    }

    #[test]
    fn display_kind_notices_and_sidecars() {
        let conn = mk_conn("schema-v30.sql");
        insert_session(&conn, "s", None, None, 1000.0, None);
        conn.execute_batch(&fixture("display_kind_notices_and_sidecars.sql"))
            .unwrap();
        for (kind, text) in [
            ("hidden", "[diagnostic] provider latency 900ms"),
            ("auto_continue", "continue"),
            ("process_complete", "job 7 finished"),
            ("internal_notification", "context 80% full"),
            ("async_delegation_complete", "delegate 7f3c finished"),
            ("personality_switch", "now terse"),
        ] {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp, display_kind) VALUES ('s', 'user', ?1, 1004.0, ?2)",
                [text, kind],
            )
            .unwrap();
        }
        let s = parse_conn(&conn, &sref(&conn, "s")).unwrap();
        let m = &s.messages[0];
        assert_eq!(m.role, Role::System, "harness-injected notice");
        assert_eq!((m.kind, m.origin), (MessageKind::ModelChange, Origin::Harness));
        assert_eq!(bag(m)["display_kind"], "model_switch");
        let steer = &s.messages[1];
        assert_eq!(steer.role, Role::User, "a human steer stays a user turn");
        assert_eq!((steer.kind, steer.origin), (MessageKind::Prompt, Origin::Human));
        assert_eq!(bag(steer)["display_kind"], "steer");
        let u = &s.messages[2];
        assert_eq!(u.role, Role::User);
        assert_eq!(
            u.text().as_deref(),
            Some("what the user typed"),
            "block text is the user's words"
        );
        assert_eq!(bag(u)["api_content"], "[context] what the user typed");
        assert_eq!(bag(u)["effect_disposition"], "observed");
        assert_eq!(bag(u)["display_metadata"]["k"], 1);
        // every documented harness display_kind, by kind
        let kinds: Vec<(Role, MessageKind, Origin)> =
            s.messages[3..].iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::SubagentReturn, Origin::Subagent),
                // undocumented: keeps its row role, carries the kind in the bag only
                (Role::User, MessageKind::Prompt, Origin::Human),
            ]
        );
        assert_eq!(bag(&s.messages[8])["display_kind"], "personality_switch");
        assert_eq!(
            s.messages
                .iter()
                .filter(|m| m.role == Role::User)
                .find_map(|m| m.text().filter(|t| !t.trim().is_empty()))
                .as_deref(),
            Some("keep going but faster"),
            "notices never become the first user text"
        );
        assert_nested_extra(&s);
    }

    #[test]
    fn imported_session_messages_carry_import_origin() {
        // `hermes sessions import` records `origin_json.imported_from`; every message of such a
        // session came through the importer, whatever its role.
        let conn = mk_conn("schema-v30.sql");
        conn.execute_batch(&fixture("imported_session_messages_carry_import_origin.sql"))
            .unwrap();
        insert_v30(&conn, "f", "user", "hello", 1001.0, 1, 0);
        insert_v30(&conn, "f", "assistant", "hi", 1002.0, 1, 0);
        let f = parse_conn(&conn, &sref(&conn, "f")).unwrap();
        assert_eq!(f.messages.len(), 2);
        assert!(
            f.messages.iter().all(|m| m.origin == Origin::Import),
            "{:?}",
            f.messages
        );
        assert_eq!(f.messages[0].kind, MessageKind::Prompt);
        assert_eq!(f.messages[1].kind, MessageKind::Reply);
        let hb = f.harness_extra(Harness::Hermes).unwrap();
        assert_eq!(hb["imported_from"]["tool"], "codex-cli");
        assert_eq!(hb["imported_from"]["foreign_session_id"], "x1");
        assert_eq!(hb[SESSION_META_KEY]["source"], "codex-cli");
        assert!(!f.extra.contains_key("imported_from"), "nested, never flat");
        assert_nested_extra(&f);
        assert_nested_extra(&f);
    }

    #[test]
    fn discover_orders_without_reducing_messages() {
        let conn = mk_conn("schema-v14.sql");
        conn.execute_batch(&fixture("discover_orders_and_counts.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES \
            ('a','system','system',1),('a','tool','result',2),('a','assistant','reply',3), \
            ('b','user','prompt',1),('b','assistant','reply',2),('b','tool','result',3), \
            ('b','system','system',4),('b','assistant','another reply',5);",
        )
        .unwrap();
        let refs = discover_conn(&conn, &at(&conn)).unwrap();
        assert_eq!(refs.len(), 2);
        // ordered by started_at DESC
        assert_eq!(refs[0].id, "b");
        assert_eq!(refs[0].message_count, None);
        assert_eq!(
            parse_conn(&conn, &refs[0])
                .unwrap()
                .messages
                .iter()
                .filter(|m| m.is_turn())
                .count(),
            3
        );
        assert_eq!(refs[1].id, "a");
        assert_eq!(refs[1].message_count, None);
        assert_eq!(
            parse_conn(&conn, &refs[1])
                .unwrap()
                .messages
                .iter()
                .filter(|m| m.is_turn())
                .count(),
            1
        );
    }

    #[test]
    fn generation_dedup_verifies_exact_cells_and_headers_under_forced_collisions() -> Result<()> {
        let conn = mk_conn("schema-v30.sql");
        insert_session(&conn, "s", None, None, 1000.0, None);
        for (role, body, stamp, tool) in [
            ("assistant", "left", 1001, ""),
            ("assistant", "right", 1001, ""),
            ("assistant", "left", 1001, ""),
            ("user", "left", 1001, ""),
            ("user", "left", 1002, ""),
            ("user", "left", 1002, ""),
            ("user", "left", 1002, "different tool"),
        ] {
            conn.execute(
                "INSERT INTO messages(session_id,role,content,timestamp,tool_name) VALUES('s',?1,?2,?3,?4)",
                rusqlite::params![role, body, stamp, tool],
            )?;
        }
        for incremental in [false, true] {
            let sql = if incremental {
                "SELECT role,typeof(content),rowid,timestamp,tool_call_id,tool_calls,tool_name FROM messages ORDER BY id"
            } else {
                "SELECT role,content,id,timestamp,tool_call_id,tool_calls,tool_name FROM messages ORDER BY id"
            };
            let columns = [
                "role",
                "content",
                "_content_rowid",
                "timestamp",
                "tool_call_id",
                "tool_calls",
                "tool_name",
            ];
            let index = |name: &str| {
                columns
                    .iter()
                    .position(|column| *column == name)
                    .or_else(|| (name == "id").then_some(2))
            };
            let mut dedup = GenerationDedup {
                forced_hash: Some(7),
                ..Default::default()
            };
            let mut statement = conn.prepare(sql)?;
            let mut rows = statement.query([])?;
            let mut admission = Vec::new();
            while let Some(row) = rows.next()? {
                admission.push(dedup.admit(row, &index, &conn, incremental)?);
            }
            assert_eq!(admission, [true, true, false, true, true, false, true], "{incremental}");
        }
        Ok(())
    }

    #[test]
    fn generation_selection_preserves_loose_native_types_and_null_normalization() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE messages(id INTEGER PRIMARY KEY,role,content,timestamp,tool_call_id,tool_calls,tool_name);
            INSERT INTO messages(role,content,timestamp,tool_name) VALUES
            (NULL,NULL,NULL,NULL),(x'01',x'02','bad',1),('', '',NULL,NULL),(NULL,'',NULL,NULL),
            ('assistant','same',1,NULL),('assistant','same',1.0,NULL),('assistant','same','1',NULL),
            ('assistant','same','1',x'01'),('assistant','same','1','');",
        )?;
        let columns = [
            "role",
            "content",
            "_content_rowid",
            "timestamp",
            "tool_call_id",
            "tool_calls",
            "tool_name",
        ];
        let index = |name: &str| {
            columns
                .iter()
                .position(|column| *column == name)
                .or_else(|| (name == "id").then_some(2))
        };
        for incremental in [false, true] {
            let sql = if incremental {
                "SELECT role,typeof(content),rowid,timestamp,tool_call_id,tool_calls,tool_name FROM messages ORDER BY id"
            } else {
                "SELECT role,content,id,timestamp,tool_call_id,tool_calls,tool_name FROM messages ORDER BY id"
            };
            let mut dedup = GenerationDedup {
                forced_hash: Some(0),
                ..Default::default()
            };
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query([])?;
            let mut admitted = Vec::new();
            while let Some(row) = rows.next()? {
                admitted.push(dedup.admit(row, &index, &conn, incremental)?);
            }
            assert_eq!(
                admitted,
                [true, false, true, false, true, false, true, false, true],
                "{incremental}"
            );
        }
        Ok(())
    }

    #[test]
    fn production_discovery_writes_no_ordinary_assistant_body_arena() -> Result<()> {
        fn observe<T>(run: impl FnOnce() -> Result<T>) -> Result<(T, Vec<u64>)> {
            struct Clear;
            impl Drop for Clear {
                fn drop(&mut self) {
                    ARENA_WRITE_OBSERVER.with(|observer| *observer.borrow_mut() = None);
                }
            }
            ARENA_WRITE_OBSERVER.with(|observer| *observer.borrow_mut() = Some(Vec::new()));
            let _clear = Clear;
            let result = run()?;
            let bytes = ARENA_WRITE_OBSERVER.with(|observer| observer.borrow_mut().take().unwrap());
            Ok((result, bytes))
        }
        let world = crate::test_world("hermes-discovery-body-demand");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v30.sql"))?;
        insert_session(&writer, "s", None, None, 1000.0, None);
        let body = "ordinary λ\nbody".repeat(100_000);
        writer.execute(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES('s','assistant',?1,1001)",
            [&body],
        )?;
        let adapter = Hermes::with_db(path);
        let (references, discovery_bytes) = observe(|| adapter.discover())?;
        assert_eq!(references[0].message_count, None);
        assert_eq!(
            discovery_bytes.iter().sum::<u64>(),
            0,
            "actual listing must not stage the undemanded reply body"
        );
        let (session, collection_bytes) =
            observe(|| crate::stream::collect_with(&adapter, &references[0], &stream::Demand::LEAN.with_spans()))?;
        assert_eq!(session.messages.len(), 1);
        assert!(
            collection_bytes.iter().sum::<u64>() >= body.len() as u64,
            "the same observer must detect arena writes when lazy collection actually demands the body"
        );
        Ok(())
    }

    #[test]
    fn streamed_turn_classification_preserves_owned_parser_acceptance() -> Result<()> {
        let world = crate::test_world("hermes-count-demand");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v30.sql"))?;
        insert_session(&writer, "s", None, None, 1000.0, None);
        let large = "ordinary λ body".repeat(100_000);
        for (role, body, reasoning, stamp) in [
            ("assistant", large.as_str(), None, 1001),
            ("assistant", large.as_str(), None, 1001), // copied generation
            ("assistant", FAILED_TURN_NOTICES[0], None, 1002),
            ("assistant", "", Some("thought only"), 1003),
            ("user", "same prompt", Some("user reasoning"), 1004),
            ("user", "same prompt", Some("user reasoning"), 1005),
            ("assistant", "\0json:[]", None, 1006),
            (
                "assistant",
                "\0json:[{\"type\":\"text\",\"text\":\"multimodal\"}]",
                None,
                1007,
            ),
        ] {
            writer.execute(
                "INSERT INTO messages(session_id,role,content,reasoning,timestamp) VALUES('s',?1,?2,?3,?4)",
                rusqlite::params![role, body, reasoning, stamp],
            )?;
        }
        writer.execute_batch(
            "INSERT INTO messages(session_id,role,content,timestamp) VALUES \
            ('s','assistant',CAST(X'FF' AS TEXT),1008),('s','assistant',X'6162',1009), \
            ('s','assistant',NULL,1010);",
        )?;
        let reference = SessionRef::new("s", Harness::Hermes, &path);
        let parsed = parse_conn(&writer, &reference)?;
        struct CountSink(usize);
        impl MessageSink for CountSink {
            fn message(&mut self, message: Message) -> Flow {
                self.0 += usize::from(message.is_turn());
                Flow::Continue
            }
        }
        let mut count = CountSink(0);
        stream_conn(&writer, &reference, &stream::Demand::LEAN.with_spans(), &mut count)?;
        assert_eq!(count.0, parsed.messages.iter().filter(|m| m.is_turn()).count());
        Ok(())
    }

    #[test]
    fn checked_giant_prompts_dedup_exactly_and_reset_after_a_reply() -> Result<()> {
        let world = crate::test_world("hermes-checked-prompt-dedup");
        let path = world.base.join("state.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(&fixture("schema-v30.sql"))?;
        insert_session(&writer, "s", None, None, 1000.0, None);
        let prompt = "λ\n\0quoted \"body\"".repeat(100_000);
        for (role, body, timestamp) in [
            ("user", prompt.as_str(), 1001),
            ("user", prompt.as_str(), 1002),
            ("assistant", "reply resets replay", 1003),
            ("user", prompt.as_str(), 1004),
        ] {
            writer.execute(
                "INSERT INTO messages(session_id,role,content,timestamp) VALUES('s',?1,?2,?3)",
                rusqlite::params![role, body, timestamp],
            )?;
        }
        let adapter = Hermes::with_db(path);
        let reference = adapter.discover()?.remove(0);
        assert_eq!(reference.message_count, None);
        let owned = parse_conn(&writer, &reference)?;
        let mut checked = crate::stream::collect_with(&adapter, &reference, &stream::Demand::RICH.with_spans())?;
        assert_eq!(checked.messages.len(), 3);
        assert!(matches!(
            &checked.messages[0].content[0],
            Block::Text {
                text: crate::Text::Source(_)
            }
        ));
        checked.materialize()?;
        assert_eq!(serde_json::to_value(&checked)?, serde_json::to_value(&owned)?);
        Ok(())
    }

    #[test]
    fn compression_lineage_walks_all_ancestors_and_still_stops_on_cycles() {
        let conn = mk_conn("schema-v14.sql");
        for index in 0..150 {
            let id = format!("s{index}");
            let parent = (index > 0).then(|| format!("s{}", index - 1));
            insert_session(&conn, &id, parent.as_deref(), Some("compression"), index as f64, None);
            conn.execute(
                "INSERT INTO messages(session_id,role,content,timestamp) VALUES(?1,'assistant',?2,?3)",
                rusqlite::params![id, format!("reply {index}"), index],
            )
            .unwrap();
        }
        assert_eq!(
            session_lineage_root_to_tip(&conn, "s149", &Select::of(&conn, "sessions")).len(),
            150
        );
        let reference = SessionRef::new("s149", Harness::Hermes, at(&conn));
        assert_eq!(parse_conn(&conn, &reference).unwrap().messages.len(), 150);
        conn.execute("UPDATE sessions SET parent_session_id='s149' WHERE id='s0'", [])
            .unwrap();
        assert_eq!(
            session_lineage_root_to_tip(&conn, "s149", &Select::of(&conn, "sessions")).len(),
            150
        );
    }

    #[test]
    fn discovery_defers_turns_and_complete_reads_publish_native_display_counts() -> Result<()> {
        let world = crate::test_world("hermes-discovery-turn-counts");
        let path = world.base.join("state.db");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes/state-v30.db"),
            &path,
        )?;
        let writer = Connection::open(&path)?;
        writer.execute("UPDATE sessions SET message_count=999", [])?;
        let adapter = Hermes::with_db(path.clone());
        let mut imported = false;
        let references = adapter.discover()?;
        assert!(references.iter().all(|r| r.message_count.is_none()));
        crate::catalog::sync(&references);
        for reference in references {
            let parsed = crate::stream::collect_with(&adapter, &reference, &stream::Demand::RICH)?;
            imported |= parsed.messages.iter().any(|message| message.origin == Origin::Import);
            let done = crate::stream::stream_counted(
                &adapter,
                &reference,
                &stream::Demand::FACTS.with_spans(),
                &mut crate::stream::Discard,
                crate::stream::Pass::RECORD,
            )?;
            let turns = parsed.messages.iter().filter(|m| m.is_turn()).count();
            assert_eq!(done.counted.unwrap().turns, turns, "{}", reference.id);
            let reader = crate::catalog::Reader::open().unwrap();
            let mut refreshed = reference.clone();
            reader.refill_turns(std::iter::once(&mut refreshed));
            assert_eq!(refreshed.message_count, Some(turns), "{}", reference.id);
            for child in subagent_refs(&reference).unwrap() {
                assert_eq!(child.message_count, None);
                let parsed = crate::stream::collect_with(&adapter, &child, &stream::Demand::RICH)?;
                let done = crate::stream::stream_counted(
                    &adapter,
                    &child,
                    &stream::Demand::FACTS.with_spans(),
                    &mut crate::stream::Discard,
                    crate::stream::Pass::RECORD,
                )?;
                assert_eq!(
                    done.counted.unwrap().turns,
                    parsed.messages.iter().filter(|m| m.is_turn()).count()
                );
            }
        }
        assert!(
            imported,
            "production discovery must include the native imported-session control"
        );
        let changed: i64 = writer.query_row("SELECT COUNT(*) FROM sessions WHERE message_count<>999", [], |row| {
            row.get(0)
        })?;
        assert_eq!(changed, 0, "discovery leaves native counters untouched");
        Ok(())
    }

    /// `tests/fixtures/hermes/state-v30.db` was written by Hermes's OWN store code (hermes-agent
    /// `6d8a8bebf7`, `SCHEMA_VERSION = 30`) through `SessionDB` — `create_session`, `append_message`,
    /// `append_messages_batch`, `archive_and_compact` (in place, `tail_count=2`),
    /// `publish_compression_child` (rotation), `update_system_prompt`, `promote_to_session_reset`,
    /// `set_session_{title,archived,hidden}`, `append_delegation_delivery`, and
    /// `hermes_cli.foreign_sessions.import_foreign_session` — never raw SQL; only the derived FTS
    /// tables/triggers were dropped to keep it small (cv never reads them). Ten sessions: A compacted
    /// in place (+ steer / hidden / async_delegation_complete rows), R root with a `/branch` child B,
    /// a reset child S and a delegate child D, a rotation chain C1→C2, X archived, Y hidden, and F, a
    /// Claude Code import. Every assertion below was first checked against Hermes's own views
    /// (`list_sessions_rich`, `get_messages(include_compacted=True)`, `get_messages_as_conversation`).
    #[test]
    fn real_v30_store_written_by_hermes() {
        const A: &str = "20260919_161227_a657eb";
        const R: &str = "20260919_161227_3b223a";
        const B: &str = "20260919_161227_3584d9";
        const S: &str = "20260919_161227_804285";
        const D: &str = "20260919_161227_65066f";
        const C1: &str = "20260919_161227_f36517";
        const C2: &str = "20260919_161227_bbce2d";
        const X: &str = "20260919_161227_12996c";
        const Y: &str = "20260919_161227_f94c97";
        const F: &str = "20260919_161227_212153";
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes/state-v30.db");
        let conn = open_ro(&path).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 30);

        // Discovery: every conversation once — the same six Hermes's picker lists, plus the archived
        // and hidden ones it keeps resumable; not the delegate run, not the merged compression root.
        let refs = discover_conn(&conn, &path).unwrap();
        let ids: HashSet<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        for want in [A, R, B, S, C2, X, Y, F] {
            assert!(ids.contains(want), "{want} is listed");
        }
        assert!(
            !ids.contains(D),
            "a delegate sub-agent run is not a top-level conversation"
        );
        assert!(!ids.contains(C1), "a compression ancestor lists as its tip");
        let by_id = |id: &str| refs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by_id(A).title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(by_id(B).title.as_deref(), Some("Design the cache (branch)"));
        assert_eq!(
            by_id(C2).title.as_deref(),
            Some("Long migration"),
            "an untitled rotation tip is titled by its root, as Hermes lists it"
        );
        assert!(by_id(F)
            .title
            .as_deref()
            .unwrap()
            .starts_with("Imported from Claude Code"));
        assert_eq!(by_id(A).cwd, Some(PathBuf::from("/tmp/proj")));
        assert_eq!(
            by_id(A).message_count,
            None,
            "cold discovery defers canonical turn counting"
        );

        // A: the display view — each carried message once, in id order, the summary as a system turn.
        let a = parse_conn(&conn, &sref(&conn, A)).unwrap();
        let t = texts(&a);
        assert_eq!(a.messages.len(), 13, "{t:?}");
        assert_eq!(a.messages.iter().filter(|m| m.is_turn()).count(), 8);
        // Index by message (the tool-result turn has no text, so `t` is one shorter).
        let txt = |i: usize| a.messages[i].text().unwrap_or_default();
        assert_eq!(txt(0), "fix the flaky test in tests/sched.py");
        assert_eq!(txt(1), "Let me look at the test.");
        assert_eq!(a.messages[2].role, Role::Tool);
        assert!(txt(5).starts_with("[CONTEXT COMPACTION"), "{}", txt(5));
        assert_eq!(
            t.iter().filter(|x| x.as_str() == "ok do it").count(),
            1,
            "the carried tail is not duplicated"
        );
        assert_eq!(txt(6), "ok do it");
        assert_eq!(txt(8), "now run the suite");
        // Every turn typed: prompt / reply / tool result / the compaction pair / steer / notices /
        // the delegation return.
        let kinds: Vec<(Role, MessageKind, Origin)> = a.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        use MessageKind as K;
        use Origin as O;
        use Role as R_;
        assert_eq!(
            kinds,
            vec![
                (R_::User, K::Prompt, O::Human),
                (R_::Assistant, K::Reply, O::Model),
                (R_::Tool, K::ToolResult, O::Harness),
                (R_::Assistant, K::Reply, O::Model),
                (R_::System, K::CompactionBoundary, O::Harness),
                (R_::System, K::CompactionSummary, O::Harness),
                (R_::User, K::Prompt, O::Human),
                (R_::Assistant, K::Reply, O::Model),
                (R_::User, K::Prompt, O::Human),
                (R_::Assistant, K::Reply, O::Model),
                (R_::User, K::Prompt, O::Human),     // steer
                (R_::System, K::Notice, O::Harness), // hidden diagnostic
                (R_::System, K::SubagentReturn, O::Subagent),
            ]
        );
        assert_eq!(
            a.messages[5].parent_id, a.messages[4].id,
            "summary links to its boundary"
        );
        assert_eq!(bag(&a.messages[5])["display_kind"], "hidden");
        assert_eq!(
            bag(&a.messages[0])["compacted"],
            true,
            "summarized-away history is flagged"
        );
        assert_eq!(bag(&a.messages[10])["display_kind"], "steer");
        assert_eq!(bag(&a.messages[12])["display_kind"], "async_delegation_complete");
        assert_eq!(a.model.as_deref(), Some("anthropic/claude-sonnet-4.5"));
        assert!(
            a.messages.iter().all(|m| m.model.is_none()),
            "no per-message model to record"
        );
        assert_eq!(a.cwd, Some(PathBuf::from("/tmp/proj")));
        let sp = a.system_prompt.as_deref().unwrap();
        assert!(
            sp.ends_with("Context was compacted once."),
            "system prompt resolved via system_prompts: {sp}"
        );
        assert!(a.lineage.is_empty(), "{:?}", a.lineage);
        assert_nested_extra(&a);
        // complete: the superseded originals of the carried tail are present, tagged as rewind rows.
        let ac = parse_conn_with(&conn, &sref(&conn, A), &stream::Demand::COMPLETE).unwrap();
        assert!(ac.messages.len() > a.messages.len());
        // (Flags are recorded as non-default values only: a superseded original carries
        // `active: false` and no `compacted` key — the same shape as a rewound row.)
        assert!(ac.messages.iter().any(|m| {
            m.harness_extra(Harness::Hermes)
                .is_some_and(|b| b.get("active") == Some(&Value::Bool(false)) && !b.contains_key("compacted"))
        }));
        assert_nested_extra(&ac);

        // C2: the rotation chain reads root→tip, titled by the root, the carried tail once; the
        // store's pointers survive both ways.
        let c2 = parse_conn(&conn, &sref(&conn, C2)).unwrap();
        assert_eq!(c2.title.as_deref(), Some("Long migration"));
        let t = texts(&c2);
        assert_eq!(t[0], "step 0: migrate table t0");
        assert_eq!(t.iter().filter(|x| x.as_str() == "migrated t3").count(), 1);
        assert!(t.iter().any(|x| x.starts_with("[CONTEXT COMPACTION")));
        assert_eq!(t.last().map(String::as_str), Some("migrated t4"));
        assert_eq!(
            c2.lineage,
            Lineage {
                continues: Some(C1.into()),
                ..Default::default()
            }
        );
        assert_eq!(
            parse_conn(&conn, &sref(&conn, C1)).unwrap().lineage,
            Lineage {
                continued_in: Some(C2.into()),
                ..Default::default()
            }
        );
        assert_eq!(
            c2.system_prompt.as_deref(),
            Some("You are Hermes, a helpful coding agent. Working dir: /tmp/proj.")
        );
        assert_nested_extra(&c2);

        // Lineage: branch / reset / delegate children stand alone with their markers.
        let b = parse_conn(&conn, &sref(&conn, B)).unwrap();
        assert_eq!(
            b.messages.len(),
            4,
            "a branch owns its copied transcript; never merged with R"
        );
        assert_eq!(b.lineage.forked_from.as_deref(), Some(R));
        assert_eq!(b.lineage.parent, None);
        let s = parse_conn(&conn, &sref(&conn, S)).unwrap();
        assert!(s.lineage.is_empty(), "a reset has no IR lineage field: {:?}", s.lineage);
        assert_eq!(s.harness_extra(Harness::Hermes).unwrap()["_reset_from"], R);
        assert_eq!(s.messages.len(), 2);
        let d = parse_conn(&conn, &sref(&conn, D)).unwrap();
        assert_eq!(d.lineage.parent.as_deref(), Some(R));
        assert_eq!(d.lineage.forked_from, None);

        // Flags and provenance.
        assert_eq!(smeta(&parse_conn(&conn, &sref(&conn, X)).unwrap())["archived"], 1);
        assert_eq!(smeta(&parse_conn(&conn, &sref(&conn, Y)).unwrap())["hidden"], 1);
        let f = parse_conn(&conn, &sref(&conn, F)).unwrap();
        let fb = f.harness_extra(Harness::Hermes).unwrap();
        assert_eq!(fb["imported_from"]["tool"], "claude-code");
        assert_eq!(smeta(&f)["source"], "claude-code");
        assert_eq!(texts(&f)[0], "Reply with exactly: ONE");
        assert_eq!(f.messages.len(), 4);
        assert!(
            f.messages.iter().all(|m| m.origin == Origin::Import),
            "every message of an import came through the importer"
        );
        assert_eq!(f.messages[0].kind, MessageKind::Prompt);
        assert_eq!(f.messages[1].kind, MessageKind::Reply);
        assert_nested_extra(&f);
    }

    #[test]
    fn fifty_concurrent_wal_writers_do_not_break_target_session_reads_or_discovery() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Barrier,
        };
        let world = crate::test_world("hermes-50-writers");
        let db = world.base.join("state.db");
        {
            let seed = Connection::open(&db).unwrap();
            seed.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
                .unwrap();
            seed.execute_batch(&fixture("schema-v30.sql")).unwrap();
            insert_session(&seed, "target", None, None, 1000.0, None);
            let large_reply = format!("target-reply-{}", "payload-".repeat(1200));
            insert_v30(&seed, "target", "user", "pinpoint target prompt", 1001.0, 1, 0);
            insert_v30(&seed, "target", "assistant", &large_reply, 1002.0, 1, 0);
            for w in 0..50 {
                insert_session(&seed, &format!("writer-{w}"), None, None, 2000.0 + w as f64, None);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(51));
        let mut handles = Vec::with_capacity(50);
        for w in 0..50 {
            let db = db.clone();
            let stop = Arc::clone(&stop);
            let ready = Arc::clone(&ready);
            handles.push(std::thread::spawn(move || {
                let conn = Connection::open(&db).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                let sid = format!("writer-{w}");
                ready.wait();
                let mut turn = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    turn += 1;
                    let _ = conn.execute(
                        "INSERT INTO messages(session_id, role, content, timestamp, active, compacted) \
                         VALUES(?1, 'user', ?2, ?3, 1, 0)",
                        rusqlite::params![sid, format!("msg-{w}-{turn}"), 3000.0 + turn as f64],
                    );
                    if w == 0 && turn.is_multiple_of(10) {
                        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
                    }
                    std::thread::yield_now();
                }
            }));
        }

        ready.wait();
        let adapter = Hermes::with_db(db.clone());
        for _ in 0..15 {
            let refs = adapter.discover().unwrap().refs;
            let target = refs
                .iter()
                .find(|r| r.id == "target")
                .expect("target session discovered");
            let mut session = crate::stream::collect_with(&adapter, target, &stream::Demand::LEAN.with_spans())
                .expect("reading target session under 50 concurrent WAL writers must succeed");
            session
                .materialize()
                .expect("materializing lazy spans of unchanged target session must succeed");
            assert_eq!(session.messages.len(), 2);
            assert_eq!(session.messages[0].text().as_deref(), Some("pinpoint target prompt"));
            assert!(session.messages[1]
                .text()
                .as_deref()
                .unwrap_or_default()
                .starts_with("target-reply-"));
            assert!(adapter.rows(&db).is_some());
        }

        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn session_revision_detects_update_only_compaction_and_isolates_per_session() {
        let world = crate::test_world("hermes-revision-honesty");
        let db = world.base.join("state.db");
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        writer.execute_batch(&fixture("schema-v30.sql")).unwrap();
        insert_session(&writer, "target", None, None, 1000.0, None);
        insert_session(&writer, "other", None, None, 1000.0, None);
        // Two rows in `target`: row 1 active=1, row 2 active=0 -> total(active) == 1.
        insert_v30(&writer, "target", "user", "first prompt", 1001.0, 1, 0);
        insert_v30(&writer, "target", "assistant", "draft reply", 1002.0, 0, 0);
        insert_v30(&writer, "other", "user", "unrelated", 1003.0, 1, 0);

        let before = rows_conn(&writer).unwrap();
        // Balanced UPDATE-only compaction swap: count(*), max(id), and total(active) all stay
        // identical (2 rows, same ids, total(active) == 1), yet visible history changes!
        writer
            .execute_batch(
                "UPDATE messages SET active = 0, compacted = 1 WHERE session_id = 'target' AND role = 'user'; \
                 UPDATE messages SET active = 1, _compressed_summary = 1 WHERE session_id = 'target' AND role = 'assistant';",
            )
            .unwrap();
        let after = rows_conn(&writer).unwrap();
        assert_ne!(
            after["target"], before["target"],
            "UPDATE-only compaction must move target's certificate even when count(*), max(id) and total(active) match"
        );
        assert_eq!(
            after["other"], before["other"],
            "unrelated session certificate must stay stable"
        );

        // Mid-stream write to `other` succeeds, while mid-stream UPDATE on `target` is rejected.
        let adapter = Hermes::with_db(db.clone());
        let refs = adapter.discover().unwrap().refs;
        let target_ref = refs.iter().find(|r| r.id == "target").unwrap();

        struct OtherWriter<'a>(&'a Connection);
        impl MessageSink for OtherWriter<'_> {
            fn meta(&mut self, _: &Session) {
                self.0
                    .execute(
                        "INSERT INTO messages(session_id, role, content, timestamp, active, compacted) \
                         VALUES('other', 'assistant', 'concurrent other write', 2000.0, 1, 0)",
                        [],
                    )
                    .unwrap();
            }
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
        }
        adapter
            .stream(target_ref, &stream::Demand::LEAN, &mut OtherWriter(&writer))
            .expect("concurrent write to another session must not fail target stream");

        struct TargetMutator<'a>(&'a Connection);
        impl MessageSink for TargetMutator<'_> {
            fn meta(&mut self, _: &Session) {
                self.0
                    .execute(
                        "UPDATE messages SET compacted = 0 WHERE session_id = 'target' AND role = 'user'",
                        [],
                    )
                    .unwrap();
            }
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
        }
        let err = adapter
            .stream(target_ref, &stream::Demand::LEAN, &mut TargetMutator(&writer))
            .unwrap_err();
        assert!(err.to_string().contains("dependencies changed"), "{err:#}");
    }
}
