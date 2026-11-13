//! Cursor adapter — IDE chat/composer `state.vscdb` and Agent CLI blob stores (SQLite).
//!
//! See `docs/FORMATS.md`. Cursor stores conversations across two SQLite databases:
//!
//!   * **Global:** `<AppSupport>/Cursor/User/globalStorage/state.vscdb`. Newer Cursor keeps the
//!     actual chat/composer content here, in a `cursorDiskKV(key TEXT, value BLOB)` table:
//!       - `composerData:<composerId>` — one row per composer/chat thread (metadata: `name`,
//!         `createdAt`, `lastUpdatedAt`, `unifiedMode`, `_v`, plus either an inline `conversation`
//!         array (older `_v`) or a `fullConversationHeadersOnly` list of `{bubbleId,type}` pointers
//!         (newer `_v`)).
//!       - `bubbleId:<composerId>:<bubbleId>` — one row per message ("bubble"): `type` (1=user,
//!         2=assistant), `text`, `richText` (Lexical JSON), `thinking:{text}`, `toolFormerData`
//!         (a tool call+result), `tokenCount:{inputTokens,outputTokens}`.
//!   * **Per-workspace:** `<AppSupport>/Cursor/User/workspaceStorage/<hash>/state.vscdb`, with a
//!     sibling `workspace.json` giving the workspace folder URI (= the cwd). Its `ItemTable` has
//!     `composer.composerData` → `{allComposers:[{composerId,name,createdAt,lastUpdatedAt,...}]}`
//!     which links composerIds to that workspace (and hence a cwd). Older Cursor stored chat blobs
//!     in the workspace DB itself (e.g. `workbench.panel.aichat.view.aichat.chatdata`).
//!
//! Strategy: build a composerId→cwd map by scanning every workspace's `composer.composerData`,
//! then enumerate every global `composerData:` thread. Threads not linked to any workspace (common:
//! ~20% on the dev machine) are still surfaced, just with `cwd = None`. The legacy in-workspace
//! `aichat.chatdata` shape is also parsed when present so old installs aren't dropped.
//!
//! Tolerance: Cursor is closed-source and the schema churns across versions (`_v` 1..=10+ seen on
//! one machine). Anything absent is skipped gracefully — a missing table, key, or field never
//! panics and never fails the whole session.

use super::store::sqlite::{open_ro, Cells};
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod cli;
mod legacy;
mod record;
mod source;

pub struct Cursor {
    /// `<AppSupport>/Cursor/User` if it exists.
    user_dir: Option<PathBuf>,
    cli_home: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Cursor = Cursor {
    user_dir: None,
    cli_home: None,
};

impl Cursor {
    pub fn detect() -> Self {
        // CURSOR_USER_DIR overrides the platform default (used by tests / non-mac).
        let user_dir = crate::env::var_os("CURSOR_USER_DIR")
            .map(PathBuf::from)
            .or_else(default_user_dir)
            .filter(|p| p.exists());
        let cli_home = crate::env::var_os("CURSOR_CONFIG_DIR")
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                crate::env::var_os("XDG_CONFIG_HOME")
                    .filter(|value| !value.to_string_lossy().trim().is_empty())
                    .map(|base| PathBuf::from(base).join("cursor"))
            })
            .or_else(|| crate::env::home_dir().map(|home| home.join(".cursor")))
            .filter(|path| path.is_dir());
        Cursor { user_dir, cli_home }
    }

    /// Use an explicit Cursor/User directory without consulting installed homes.
    pub fn with_user_dir(dir: &Path) -> Self {
        Self {
            user_dir: Some(dir.to_path_buf()).filter(|p| p.exists()),
            cli_home: None,
        }
    }

    /// Use one explicit root, whichever of the two stores it is: a `Cursor/User` directory (it
    /// holds `globalStorage/`) or else a CLI home ([`Self::with_cli_home`]).
    pub fn with_root(root: &Path) -> Self {
        if root.join("globalStorage").is_dir() {
            Self::with_user_dir(root)
        } else {
            Self::with_cli_home(root)
        }
    }

    /// Use only an explicit Cursor Agent CLI configuration home (`.cursor`), including chats
    /// and ACP sessions. Never consult the installed IDE or other harness homes.
    pub fn with_cli_home(home: &Path) -> Self {
        Self {
            user_dir: None,
            cli_home: Some(home.to_path_buf()),
        }
    }

    fn global_db(&self) -> Option<PathBuf> {
        self.user_dir
            .as_ref()
            .map(|d| d.join("globalStorage").join("state.vscdb"))
            .filter(|p| p.exists())
    }

    fn workspace_dirs(&self) -> Vec<PathBuf> {
        let Some(ws_root) = self
            .user_dir
            .as_ref()
            .map(|d| d.join("workspaceStorage"))
            .filter(|p| p.exists())
        else {
            return vec![];
        };
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&ws_root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.push(p);
                }
            }
        }
        out
    }
}

/// Default macOS/Linux/Windows location of `Cursor/User`.
fn default_user_dir() -> Option<PathBuf> {
    let home = crate::env::home_dir()?;
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support");
    #[cfg(target_os = "windows")]
    let base = crate::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"));
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = crate::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Some(base.join("Cursor").join("User"))
}

impl Adapter for Cursor {
    fn harness(&self) -> Harness {
        Harness::Cursor
    }

    fn metadata_file(&self, r: &SessionRef) -> Option<PathBuf> {
        cli::metadata(r)
    }

    /// `Cursor/User`: the dir under which both the global `state.vscdb` and every
    /// `workspaceStorage/<hash>/state.vscdb` live. The db files are catalog file-watches.
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<_> = self.user_dir.clone().into_iter().collect();
        if let Some(home) = &self.cli_home {
            roots.extend(cli::roots(home));
        }
        roots
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let mut out = crate::Discovered::default();
        // Map composerId -> cwd by scanning every workspace's composer.composerData. Also collect
        // metadata fallbacks (name/created) and legacy in-workspace chat threads.
        let cwd_map = self.build_cwd_map();

        // Each database that cannot open or read is skipped and reported.
        if let Some(db) = self.global_db() {
            let rows = super::store::sqlite::cached_discover(&db, || {
                open_ro(&db)?.read(|conn, _| {
                    let mut rows = Vec::new();
                    discover_global(conn, &db, &HashMap::new(), &mut rows);
                    Ok(rows)
                })
            });
            if let Some(rows) = out.keep(&db, rows) {
                out.refs.extend(rows.into_iter().map(|mut row| {
                    row.cwd = cwd_map.get(&row.id).cloned();
                    row
                }));
            }
        }

        // Legacy: old Cursor kept chat blobs inside the workspace DB itself.
        for ws in self.workspace_dirs() {
            let db = ws.join("state.vscdb");
            if !db.exists() {
                continue;
            }
            let cwd = workspace_cwd(&ws);
            let rows = super::store::sqlite::cached_discover(&db, || {
                open_ro(&db)?.read(|conn, _| {
                    let mut rows = Vec::new();
                    legacy::discover(conn, &db, None, &mut rows)?;
                    Ok(rows)
                })
            });
            if let Some(rows) = out.keep(&db, rows) {
                out.refs.extend(rows.into_iter().map(|mut row| {
                    row.cwd = cwd.clone();
                    row
                }));
            }
        }
        if let Some(home) = &self.cli_home {
            cli::discover(home, &mut out);
        }
        Ok(out)
    }

    /// Every composer's and legacy tab's certificate in a `state.vscdb` ([`rows_conn`]); a CLI
    /// chat's `store.db` holds one session and certifies none.
    fn rows(&self, store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
        open_ro(store).ok()?.read(|conn, _| rows_conn(conn)).ok()
    }

    /// Every legacy tab of a workspace database reads the one `chatdata` value: they share its
    /// certificate.
    fn rows_key<'a>(&self, r: &'a SessionRef) -> &'a str {
        match r.id.split_once(':') {
            Some(("legacy", _)) => "legacy",
            _ => &r.id,
        }
    }

    /// Cursor writes its session databases in place.
    fn watch_policy(&self, _r: &SessionRef) -> super::WatchPolicy {
        super::WatchPolicy::Database
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // `path` is the DB the thread lives in. Legacy refs carry a `legacy:` id prefix.
            for suffix in ["-wal", "-journal"] {
                let mut path = r.path.as_os_str().to_os_string();
                path.push(suffix);
                sink.dependency(&PathBuf::from(path));
            }
            let conn = open_ro(&r.path)?;
            conn.read(|conn, reads| {
                if r.id.starts_with("cli:") {
                    return cli::stream(conn, reads, r, opts, sink);
                }
                if let Some(legacy_id) = r.id.strip_prefix("legacy:") {
                    return legacy::stream(conn, reads, r, legacy_id, opts.spans, sink);
                }
                stream_global(conn, reads, r, opts.spans, sink)
            })
        })
    }
}

impl Cursor {
    /// composerId -> cwd, harvested from every workspace's `composer.composerData.allComposers`.
    fn build_cwd_map(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        for ws in self.workspace_dirs() {
            let db = ws.join("state.vscdb");
            if !db.exists() {
                continue;
            }
            let Some(cwd) = workspace_cwd(&ws) else {
                continue;
            };
            // The workspace's composer ids, read once per database state and kept in cv's
            // catalog: a discovery over unchanged workspaces opens none of them.
            let Ok(ids) = super::store::sqlite::derived(&db, Harness::Cursor, |conn, _| {
                let v = item_table_get(conn, "composer.composerData")
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .unwrap_or_default();
                Ok(WorkspaceComposers(
                    v.get("allComposers")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.get("composerId").and_then(Value::as_str).map(str::to_string))
                        .collect(),
                ))
            }) else {
                continue;
            };
            for id in &ids.payload.0 {
                map.entry(id.clone()).or_insert_with(|| cwd.clone());
            }
        }
        map
    }
}

/// The composer ids a workspace database's `composer.composerData` lists, in its order.
#[derive(serde::Serialize, serde::Deserialize)]
struct WorkspaceComposers(Vec<String>);

impl crate::source::Payload for WorkspaceComposers {
    const TAG: &'static str = "cursor-workspace-composers";
}

/// Read a single ItemTable value (TEXT or BLOB) as a UTF-8 string.
fn item_table_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        // value may be stored as TEXT or BLOB depending on version.
        match row.get::<_, String>(0) {
            Ok(s) => Ok(s),
            Err(_) => row
                .get::<_, Vec<u8>>(0)
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        }
    })
    .ok()
}

/// Resolve the cwd for a workspace dir from its sibling `workspace.json` `folder` URI.
fn workspace_cwd(ws: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(ws.join("workspace.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    // `folder` is a single-root folder URI; multi-root workspaces use `workspace` (a .json path) —
    // we only resolve the simple single-folder case to a cwd.
    let uri = v.get("folder").and_then(Value::as_str)?;
    file_uri_to_path(uri)
}

/// `file:///Users/x/p%20q` -> `/Users/x/p q`. Returns None for non-file URIs.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Drop an authority component if present (file://host/path); keep the leading '/'.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None => rest,
    };
    Some(PathBuf::from(percent_decode(path)))
}

/// Minimal percent-decoder (Cursor URIs use %XX for spaces etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Global (cursorDiskKV) format
// ---------------------------------------------------------------------------

// LIKE is case-insensitive by default and cannot use Cursor's binary key index. Bound the
// exact prefix instead, and keep the table-scan row order so timestamp ties remain stable.
const COMPOSER_ROWS: &str = "SELECT key FROM cursorDiskKV \
    WHERE key >= 'composerData:' AND key < 'composerData;' ORDER BY rowid";
const COMPOSER_ROWS_WITHOUT_ROWID: &str = "SELECT key FROM cursorDiskKV \
    WHERE key >= 'composerData:' AND key < 'composerData;'";

/// Every session's [`Rows`](crate::source::Rows) certificate in a `state.vscdb`: for a composer its
/// `composerData:<id>` row and every `bubbleId:<id>:*` row, in key order, each by its key, rowid
/// (`key UNIQUE ON CONFLICT REPLACE`: a rewritten value takes a new rowid) and the byte length of
/// its value from the record header; for the legacy tabs (key `legacy`, [`Cursor::rows_key`]) the
/// one `ItemTable` value they all live in. The key ranges run on the key index; no value is read.
fn rows_conn(conn: &Connection) -> Result<HashMap<String, crate::source::Rows>> {
    let _snapshot = super::store::sqlite::snapshot(conn)?;
    let mut certificates = super::store::sqlite::Certificates::default();
    let rowid = |table: &str| {
        if conn.prepare(&format!("SELECT rowid FROM {table} LIMIT 0")).is_ok() {
            "rowid"
        } else {
            "NULL"
        }
    };
    if super::store::sqlite::table_exists(conn, "cursorDiskKV") {
        let rowid = rowid("cursorDiskKV");
        certificates.rows(
            conn,
            &format!(
                "SELECT substr(key, 14), {rowid}, octet_length(value) FROM cursorDiskKV \
                 WHERE key >= 'composerData:' AND key < 'composerData;' ORDER BY key"
            ),
        )?;
        certificates.rows(
            conn,
            &format!(
                "SELECT substr(key, 10, instr(substr(key, 10), ':') - 1), {rowid}, key, octet_length(value) \
                 FROM cursorDiskKV WHERE key >= 'bubbleId:' AND key < 'bubbleId;' ORDER BY key"
            ),
        )?;
    }
    if super::store::sqlite::table_exists(conn, "ItemTable") {
        certificates.rows(
            conn,
            &format!(
                "SELECT 'legacy', {}, octet_length(value) FROM ItemTable WHERE key = '{}'",
                rowid("ItemTable"),
                legacy::CHAT_KEY
            ),
        )?;
    }
    Ok(certificates.0)
}

fn discover_global(conn: &Connection, db: &Path, cwd_map: &HashMap<String, PathBuf>, out: &mut Vec<SessionRef>) {
    // Bail quietly if this DB has no cursorDiskKV table (older layout).
    let mut stmt = match conn
        .prepare(COMPOSER_ROWS)
        .or_else(|_| conn.prepare(COMPOSER_ROWS_WITHOUT_ROWID))
    {
        Ok(s) => s,
        Err(_) => return,
    };
    let rows = stmt.query_map([], |row| row.get::<_, String>(0));
    let Ok(rows) = rows else { return };
    // A composer whose rows certify as when it was last decoded keeps that listing: a write
    // decodes the composers it moved, not every composer of the store.
    let certificates = rows_conn(conn).unwrap_or_default();
    let mut pass = super::store::sqlite::listing::Pass::<ComposerListing>::begin(Harness::Cursor, db);

    for key in rows.flatten() {
        let Some(composer_id) = key.strip_prefix("composerData:") else {
            continue;
        };
        // A private arena per composer: discovery holds one composer cell at a time.
        let Ok(cells) = Cells::new(conn, db, &crate::source::ReadSet::default(), "cursorDiskKV") else {
            return;
        };
        let listed = pass.list(composer_id, certificates.get(composer_id).copied(), || {
            // A cell that cannot be read is skipped and decoded again next time.
            let cell = match source::cell(conn, &cells, "cursorDiskKV", &key) {
                Ok(Some(cell)) => cell,
                Ok(None) => return Ok(ComposerListing(None)),
                Err(_) => return Err(()),
            };
            // The count is the turns a read makes: the bubble codec itself decides, so a bubble it
            // drops (a capability marker with no text, tool call or thinking) is no turn. Bubbles
            // kept in their own cells are not opened here: that composer's count waits for a full
            // read or `cv index`.
            let header = cells.read(cell, |root| {
                let composer = record::composer(root)?;
                let (mut rows, mut turns) = (0, 0);
                for (ordinal, row) in composer.rows.into_iter().flatten().enumerate() {
                    let row = row?;
                    rows += 1;
                    if !composer.external && record::bubble(row, &cells, true, ordinal)?.is_some() {
                        turns += 1;
                    }
                }
                Ok((composer.metadata, rows, (!composer.external).then_some(turns)))
            });
            let Ok(Some((d, rows, count))) = header else {
                return Err(());
            };
            // Skip threads that have no messages at all (Cursor leaves empty composer shells around).
            if rows == 0 {
                return Ok(ComposerListing(None));
            }
            let shell = SessionRef::new(composer_id, Harness::Cursor, db);
            let head = global_session(&d, &shell);
            Ok(ComposerListing(Some(Composed {
                title: head
                    .title
                    .map(|title| crate::ir::truncate(&title, crate::projection::Head::TITLE)),
                created_at: head.created_at,
                updated_at: head.updated_at,
                count,
            })))
        });
        let Ok(ComposerListing(Some(listed))) = listed else {
            continue;
        };
        out.push(SessionRef {
            cwd: cwd_map.get(composer_id).cloned(),
            title: listed.title,
            created_at: listed.created_at,
            updated_at: listed.updated_at,
            message_count: listed.count,
            ..SessionRef::new(composer_id.to_string(), Harness::Cursor, db.to_path_buf())
        });
    }
    pass.keep();
}

/// A composer's discovery listing ([`discover_global`]); `None` for a composer listed as none.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ComposerListing(Option<Composed>);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Composed {
    title: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `None` for a composer whose bubbles live in their own cells.
    count: Option<usize>,
}

impl super::store::sqlite::listing::Listing for ComposerListing {
    const TAG: &'static str = "cursor-listing";
}

/// One composer: its session facts, then each bubble of its authoritative conversation, inline or
/// external (`bubbleId:<composer>:<bubble>` cells; missing or unreadable ones are counted as
/// `skipped_lines`).
fn stream_global(
    conn: &Connection,
    reads: &crate::source::ReadSet,
    r: &SessionRef,
    spans: bool,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let _snapshot = super::store::sqlite::snapshot(conn)?;
    let cells = Cells::new(conn, &r.path, reads, "cursorDiskKV")?;
    let key = format!("composerData:{}", r.id);
    let cell = source::cell(conn, &cells, "cursorDiskKV", &key)?.with_context(|| format!("{key} missing"))?;
    let session = cells
        .read(cell, |root| {
            let composer = record::composer(root)?;
            let mut session = global_session(&composer.metadata, r);
            // The session model is the latest message's, else the composer's modelConfig.
            let mut last_model = None;
            // A header naming a bubble whose cell is missing or unreadable is a record that cannot
            // parse: counted (`skipped_lines`), never silently dropped.
            let mut unparsed = super::Unparsed::default();
            for (ordinal, row) in composer.rows.into_iter().flatten().enumerate() {
                let row = row?;
                let message = if composer.external {
                    let Some(id) = row.get("bubbleId")?.map(|view| view.string()).transpose()?.flatten() else {
                        unparsed.count();
                        continue;
                    };
                    let key = format!("bubbleId:{}:{id}", r.id);
                    let Ok(Some(cell)) = source::cell(conn, &cells, "cursorDiskKV", &key) else {
                        unparsed.count();
                        continue;
                    };
                    match cells.read(cell, |view| record::bubble(view, &cells, spans, ordinal))? {
                        Some(msg) => msg,
                        None => {
                            unparsed.count();
                            continue;
                        }
                    }
                } else {
                    record::bubble(row, &cells, spans, ordinal)?
                };
                if let Some(message) = message {
                    if message.model.is_some() {
                        last_model = message.model.clone();
                    }
                    if sink.message(message) == Flow::Stop {
                        break;
                    }
                }
            }
            session.model = last_model.or_else(|| {
                composer
                    .metadata
                    .pointer("/modelConfig/modelName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            });
            unparsed.note(&mut session);
            Ok(session)
        })?
        .context("composerData JSON")?;
    cells.check()?;
    Ok(session)
}

fn global_session(d: &Value, r: &SessionRef) -> Session {
    let created = d.get("createdAt").and_then(Value::as_i64);
    let updated = d.get("lastUpdatedAt").and_then(Value::as_i64).or(created);
    let title = d
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| r.title.clone());

    let mut session = Session {
        cwd: r.cwd.clone(),
        title,
        created_at: created.and_then(ts_from_epoch).or(r.created_at),
        updated_at: updated.and_then(ts_from_epoch).or(r.updated_at),
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Cursor)
    };
    // Sub-agent lineage: a composer spawned as a sub-agent records `subagentInfo`
    // (`parentComposerId`, the spawning `toolCallId`, the `subagentTypeName` nickname).
    if let Some(si) = d.get("subagentInfo").filter(|v| !v.is_null()) {
        if let Some(p) = si
            .get("parentComposerId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            session.lineage.parent = Some(p.to_string());
        }
        if let Some(tc) = si.get("toolCallId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            session.lineage.spawned_by_tool_use = Some(tc.to_string());
        }
        if let Some(name) = si
            .get("subagentTypeName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            session.lineage.agent_path = Some(name.to_string());
        }
    }
    // `unifiedMode` (chat/agent/edit) has no first-class IR home. The composer's is a *session*
    // fact — the mode the thread was opened in — so it lands in the session bag; a bubble that
    // carries its own overrides it for that message only (see `bubble_to_message`), which is how
    // Cursor records a mode switched mid-thread.
    if let Some(value) = d.get("unifiedMode") {
        let metadata = unified_mode_metadata(value);
        if !metadata.is_empty() {
            session.harness_extra_mut(Harness::Cursor).extend(metadata);
        }
    }

    session
}

/// IDE bubbles use RFC3339 creation times; tolerate older numeric epoch records too.
fn bubble_timestamp(value: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    match value {
        Value::String(text) => super::parse_ts(text),
        Value::Number(number) => number.as_i64().and_then(ts_from_epoch),
        _ => None,
    }
}

/// Numeric bubble modes use the installed Cursor 3.18.25 protocol enum (see the
/// read-only evidence in tools/harness-corpus/cursor/README.md). Keep the original
/// number alongside its readable mode, including unknown future enum values.
fn unified_mode_metadata(value: &Value) -> Map<String, Value> {
    let mut extra = Map::new();
    let mode = match value {
        Value::String(s) => Some(s.as_str()).filter(|s| !s.is_empty()),
        Value::Number(_) => {
            extra.insert("unified_mode_raw".into(), value.clone());
            match value.as_i64() {
                Some(0) => Some("unspecified"),
                Some(1) => Some("chat"),
                Some(2) => Some("agent"),
                Some(3) => Some("edit"),
                Some(4) => Some("custom"),
                Some(5) => Some("plan"),
                Some(6) => Some("debug"),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(mode) = mode {
        extra.insert("unified_mode".into(), mode.into());
    }
    extra
}

/// Whole-`Session` convenience over [`stream_global`] for tests: stream into a [`CollectSink`].
#[cfg(test)]
fn parse_global(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_global(conn, &Default::default(), r, false, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state.vscdb file resembling the global cursorDiskKV layout.
    struct Db {
        _world: cv_testkit::World,
        conn: Connection,
        path: PathBuf,
    }
    impl std::ops::Deref for Db {
        type Target = Connection;
        fn deref(&self) -> &Connection {
            &self.conn
        }
    }
    impl std::ops::DerefMut for Db {
        fn deref_mut(&mut self) -> &mut Connection {
            &mut self.conn
        }
    }
    impl Db {
        fn sref(&self, id: &str) -> SessionRef {
            SessionRef::new(id, Harness::Cursor, self.path.clone())
        }
    }
    fn mk_global() -> Db {
        let world = cv_testkit::World::new("cursor-global");
        let path = world.base.join("state.vscdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);",
        )
        .unwrap();
        Db {
            _world: world,
            conn,
            path,
        }
    }

    fn put(conn: &Connection, key: &str, value: &str) {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }

    /// One bubble decoded from an owned value, as the bulk demand reads it.
    fn bubble_to_message(b: &Value) -> Option<Message> {
        let db = mk_global();
        let cells = Cells::new(&db, &db.path, &Default::default(), "cursorDiskKV").unwrap();
        record::bubble(crate::json::JsonView::owned(b), &cells, false, 0).unwrap()
    }

    #[test]
    fn composer_prefix_uses_key_index_and_preserves_discovery_order() {
        use rusqlite::StatementStatus;
        let mut conn = mk_global();
        let tx = conn.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO cursorDiskKV VALUES (?1, ?2)").unwrap();
            for i in 0..20_000 {
                insert
                    .execute(rusqlite::params![format!("bubbleId:c:{i}"), "{}"])
                    .unwrap();
            }
            // Insertion order differs from key order; preserve the old table scan's ordering.
            for key in ["composerData:z", "composerData:a", "composerData:m"] {
                insert.execute(rusqlite::params![key, "{}"]).unwrap();
            }
            for key in ["composerData", "composerData;later", "ComposerData:wrong-case"] {
                insert.execute(rusqlite::params![key, "{}"]).unwrap();
            }
        }
        tx.commit().unwrap();
        let mut old = conn
            .prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")
            .unwrap();
        let old_keys: Vec<String> = old
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .filter(|k: &String| k.starts_with("composerData:"))
            .collect();
        let mut bounded = conn.prepare(COMPOSER_ROWS).unwrap();
        let keys: Vec<String> = bounded.query_map([], |r| r.get(0)).unwrap().flatten().collect();
        assert_eq!(keys, old_keys);
        // The bounded prefix must run on the key index, never as a full table scan.
        assert_eq!(bounded.get_status(StatementStatus::FullscanStep), 0);
    }

    /// The corpus global store with its composer copied under a second id: rewriting one
    /// composer's value (a new rowid, as Cursor's `ON CONFLICT REPLACE` writes it) moves its
    /// certificate alone, and the other composer reads exactly as before.
    #[test]
    fn corpus_rows_certify_each_session_and_move_only_with_its_rows() {
        let world = crate::test_world("cursor-corpus-rows");
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/cursor/3.24.12-ide/Cursor/User");
        let user = world.base.join("User");
        for entry in walkdir::WalkDir::new(&corpus) {
            let entry = entry.unwrap();
            let to = user.join(entry.path().strip_prefix(&corpus).unwrap());
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&to).unwrap();
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
        let db = user.join("globalStorage/state.vscdb");
        let leaf = "4850be27-a5fd-4c24-b25b-7786285ccbea";
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO cursorDiskKV SELECT replace(key, '{leaf}', 'copied-composer'), value FROM cursorDiskKV \
                 WHERE key LIKE 'composerData:{leaf}' OR key LIKE 'bubbleId:{leaf}:%';"
            ))
            .unwrap();
        let adapter = Cursor::with_user_dir(&user);
        super::super::store::sqlite::assert_rows_move_alone(&adapter, &db, leaf, |c| {
            c.execute(
                "INSERT INTO cursorDiskKV SELECT key, value FROM cursorDiskKV WHERE key = ?1",
                [format!("composerData:{leaf}")],
            )
            .unwrap();
        });
    }

    /// A write to one composer rediscovers the store decoding that composer alone (the others
    /// keep the listing their unchanged rows certify), and the workspace composer map is read
    /// once per workspace database state.
    #[test]
    fn a_composer_write_decodes_only_that_composer_at_discovery() {
        use super::super::store::sqlite::{listing::DECODED, DERIVE_OPENS};
        let world = crate::test_world("cursor-listing");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/cursor/3.24.12-ide/Cursor/User");
        let user = world.base.join("User");
        for entry in walkdir::WalkDir::new(&corpus) {
            let entry = entry.unwrap();
            let to = user.join(entry.path().strip_prefix(&corpus).unwrap());
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&to).unwrap();
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
        let db = user.join("globalStorage/state.vscdb");
        let leaf = "4850be27-a5fd-4c24-b25b-7786285ccbea";
        for copy in ["copied-one", "copied-two"] {
            Connection::open(&db)
                .unwrap()
                .execute_batch(&format!(
                    "INSERT INTO cursorDiskKV SELECT replace(key, '{leaf}', '{copy}'), value FROM cursorDiskKV \
                     WHERE key LIKE 'composerData:{leaf}' OR key LIKE 'bubbleId:{leaf}:%';"
                ))
                .unwrap();
        }
        let adapter = Cursor::with_user_dir(&user);
        let listing = || {
            let mut refs = adapter.discover().unwrap().refs;
            refs.sort_by(|a, b| a.id.cmp(&b.id));
            serde_json::to_string(&refs).unwrap()
        };
        let counters = || (DECODED.with(|n| n.replace(0)), DERIVE_OPENS.with(|n| n.replace(0)));
        let first = listing();
        let (decoded, opened) = counters();
        assert!(decoded >= 3, "three composers at least: {decoded}");
        assert!(opened > 0, "the corpus holds a workspace database");
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO cursorDiskKV SELECT key, value FROM cursorDiskKV WHERE key = ?1",
                [format!("composerData:{leaf}")],
            )
            .unwrap();
        assert_eq!(listing(), first);
        assert_eq!(counters(), (1, 0));
    }

    #[test]
    fn native_ide_failed_command_and_reopened_conversation() {
        use crate::harness::Parse;

        let _world = crate::test_world("cursor-native-ide");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/cursor/3.24.12-ide/Cursor/User");
        let adapter = Cursor::with_user_dir(&root);
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        let session = adapter.parse(&refs[0]).unwrap();
        assert_eq!(session.messages.len(), 44);
        assert_eq!(
            session.messages[0].timestamp,
            super::super::parse_ts("2026-10-14T00:49:06.803Z")
        );
        assert!(session.messages.iter().all(|m| m.timestamp.is_some()));
        for opts in [Demand::LEAN.with_spans(), Demand::COMPLETE] {
            let mut sink = crate::stream::CollectSink::default();
            adapter.stream(&refs[0], &opts, &mut sink).unwrap();
            assert_eq!(
                serde_json::to_value(&sink.messages).unwrap(),
                serde_json::to_value(&session.messages).unwrap()
            );
        }
        let users: Vec<_> = session.messages.iter().filter(|m| m.role == Role::User).collect();
        assert_eq!(users.len(), 4);
        assert!(users[3].text().unwrap().contains("We reopened the IDE after stopping"));
        let results: Vec<_> = session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult { content, is_error, .. } => Some((content, *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 17);
        let failed: Vec<_> = results.iter().filter(|(_, error)| *error).collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].0.contains("SyntaxError"));
        assert!(results
            .iter()
            .any(|(text, error)| !error && text.contains("Ran 7 tests")));
    }

    /// The skip policy's first channel under an injected failure: a header whose bubble row is
    /// gone was dropped without a trace; it is counted.
    #[test]
    fn a_missing_external_bubble_is_counted_not_silently_dropped() {
        let conn = mk_global();
        let cid = "comp-gone";
        let composer = |headers: &str| {
            format!(
                r#"{{"composerId":"{cid}","name":"Torn","createdAt":1700000000000,"_v":3,"fullConversationHeadersOnly":{headers}}}"#
            )
        };
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"kept"}"#,
        );
        put(
            &conn,
            &format!("composerData:{cid}"),
            &composer(r#"[{"bubbleId":"b1","type":1}]"#),
        );
        let clean = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert!(clean.cv_extra().is_none(), "a whole composer notes nothing");
        conn.execute(
            "UPDATE cursorDiskKV SET value=?2 WHERE key=?1",
            rusqlite::params![
                format!("composerData:{cid}"),
                composer(r#"[{"bubbleId":"b1","type":1},{"bubbleId":"gone","type":2}]"#)
            ],
        )
        .unwrap();
        let torn = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(torn.messages.len(), 1);
        assert_eq!(torn.cv_extra().unwrap()["skipped_lines"], 1);
    }

    #[test]
    fn parses_headers_only_with_external_bubbles() {
        let conn = mk_global();
        let cid = "comp-1";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-1","name":"My Chat","createdAt":1700000000000,"lastUpdatedAt":1700000050000,"unifiedMode":"agent","_v":3,
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"How do I write a test?"}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b2"),
            r#"{"bubbleId":"b2","type":2,"text":"Here is how.","thinking":{"text":"let me think"},
                "unifiedMode":"chat",
                "tokenCount":{"inputTokens":10,"outputTokens":5},
                "toolFormerData":{"name":"run_terminal_cmd","toolCallId":"t1","status":"completed",
                  "rawArgs":"{\"command\":\"cargo test\"}","result":"{\"output\":\"ok\"}"}}"#,
        );

        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, &conn.path, &map, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "comp-1");
        assert_eq!(out[0].title.as_deref(), Some("My Chat"));
        assert_eq!(
            out[0].message_count, None,
            "bubbles in their own cells are counted by a full read"
        );
        assert!(out[0].created_at.is_some());

        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(s.title.as_deref(), Some("My Chat"));
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("How do I write a test?"));

        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        // thinking block present
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { text, .. } if text.matches_str("let me think") == Some(true))));
        // text block present
        assert_eq!(a.text().as_deref(), Some("Here is how."));
        // tool use + result
        let tu = a.content.iter().find_map(|b| match b {
            Block::ToolUse { name, input, id, .. } => Some((name.clone(), input.clone(), id.clone())),
            _ => None,
        });
        let (name, input, tid) = tu.expect("tool use");
        assert_eq!(name, "run_terminal_cmd");
        assert_eq!(tid, "t1");
        assert_eq!(
            input.resolve().unwrap().get("command").and_then(Value::as_str),
            Some("cargo test")
        );
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { tool_use_id, content, is_error, .. }
            if tool_use_id == "t1" && content.contains("ok") && !is_error)));
        // usage mapped
        assert_eq!(a.usage.as_ref().unwrap().input_tokens, Some(10));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(5));

        // kind/origin: user bubble is a prompt, assistant bubble a reply.
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        // `unifiedMode` rides nested under `extra["cursor"]`, never flat. The COMPOSER's mode is a
        // session fact (the mode the thread was opened in); a bubble's own overrides it per
        // message — here b2 switched to "chat" while the thread is an "agent" thread, and b1
        // (which carries no mode of its own) gets no message-level key at all.
        assert_eq!(
            s.harness_extra(Harness::Cursor)
                .and_then(|b| b.get("unified_mode"))
                .and_then(Value::as_str),
            Some("agent"),
            "the composer's mode lands on the session bag"
        );
        assert_eq!(
            a.harness_extra(Harness::Cursor)
                .and_then(|b| b.get("unified_mode"))
                .and_then(Value::as_str),
            Some("chat"),
            "the bubble's own mode overrides for that message"
        );
        assert!(s.messages[0]
            .harness_extra(Harness::Cursor)
            .is_none_or(|b| !b.contains_key("unified_mode")));
        // No flat keys survive anywhere: every top-level key is a namespace (the harness bag, cv's
        // own `cv` bag, or the `_record` carrier). See `docs/INTERFACE-V2.md` §4.
        assert!(
            !a.extra.contains_key("unifiedMode"),
            "no flat keys besides the harness bag"
        );
        crate::harness::assert_no_flat_keys(&s);
    }

    #[test]
    fn numeric_modes_preserve_enum_names_raw_values_and_message_overrides() {
        for (number, name) in [
            (0, "unspecified"),
            (1, "chat"),
            (2, "agent"),
            (3, "edit"),
            (4, "custom"),
            (5, "plan"),
            (6, "debug"),
        ] {
            let bubble = serde_json::json!({"type":2,"text":"reply","unifiedMode":number});
            let message = bubble_to_message(&bubble).unwrap();
            let extra = message.harness_extra(Harness::Cursor).unwrap();
            assert_eq!(extra["unified_mode"], name);
            assert_eq!(extra["unified_mode_raw"], number);
        }
        for number in [serde_json::json!(-1), serde_json::json!(7), serde_json::json!(2.5)] {
            let message = bubble_to_message(&serde_json::json!({"type":2,"unifiedMode":number})).unwrap();
            let extra = message.harness_extra(Harness::Cursor).unwrap();
            assert!(!extra.contains_key("unified_mode"));
            assert_eq!(extra["unified_mode_raw"], number);
        }
        for value in [
            serde_json::json!(""),
            Value::Null,
            serde_json::json!(true),
            serde_json::json!({}),
        ] {
            assert!(bubble_to_message(&serde_json::json!({"type":2,"unifiedMode":value})).is_none());
        }
        assert!(bubble_to_message(&serde_json::json!({"type":2})).is_none());
        let future = bubble_to_message(&serde_json::json!({"type":2,"unifiedMode":"future-mode"})).unwrap();
        assert_eq!(
            future.harness_extra(Harness::Cursor).unwrap()["unified_mode"],
            "future-mode"
        );

        let conn = mk_global();
        put(
            &conn,
            "composerData:mode",
            r#"{"composerId":"mode","unifiedMode":"agent","conversation":[{"type":2,"text":"chat reply","unifiedMode":1}]}"#,
        );
        let reference = conn.sref("mode");
        let session = parse_global(&conn, &reference).unwrap();
        assert_eq!(session.harness_extra(Harness::Cursor).unwrap()["unified_mode"], "agent");
        assert_eq!(
            session.messages[0].harness_extra(Harness::Cursor).unwrap()["unified_mode"],
            "chat"
        );
        assert_eq!(
            session.messages[0].harness_extra(Harness::Cursor).unwrap()["unified_mode_raw"],
            1
        );
    }

    #[test]
    fn subagent_info_populates_lineage() {
        let conn = mk_global();
        let cid = "comp-sub";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-sub","_v":3,
                "subagentInfo":{"parentComposerId":"comp-parent","toolCallId":"tool_7","subagentTypeName":"explore"},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"look"}"#,
        );
        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("comp-parent"));
        assert_eq!(s.lineage.spawned_by_tool_use.as_deref(), Some("tool_7"));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("explore"));
    }

    #[test]
    fn extracts_model_from_bubble_and_composer() {
        let conn = mk_global();
        let cid = "comp-model";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-model","_v":3,"modelConfig":{"modelName":"gpt-5","maxMode":true},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":2,"text":"answer","modelInfo":{"modelName":"claude-4.5-sonnet-thinking"}}"#,
        );
        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        // Per-message model from modelInfo.modelName; session model prefers the assistant's.
        assert_eq!(s.messages[0].model.as_deref(), Some("claude-4.5-sonnet-thinking"));
        assert_eq!(s.model.as_deref(), Some("claude-4.5-sonnet-thinking"));
    }

    #[test]
    fn session_model_falls_back_to_composer_config() {
        let conn = mk_global();
        let cid = "comp-cfg";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-cfg","_v":3,"modelConfig":{"modelName":"gpt-5"},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"hi"}"#,
        );
        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(s.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn parses_inline_conversation_old_shape() {
        let conn = mk_global();
        let cid = "comp-old";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-old","createdAt":1700000000000,"_v":1,
                "conversation":[{"bubbleId":"x1","type":1,"text":"hi"},{"bubbleId":"x2","type":2,"text":"hello"}]}"#,
        );
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, &conn.path, &map, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_count, Some(2));
        // no name -> title falls back to None at discover; label() uses first user text.
        assert!(out[0].title.is_none());

        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));
        assert_eq!(s.messages[1].text().as_deref(), Some("hello"));
    }

    /// The inline shape on ember's store (a 50-bubble composer read as 26 turns): between replies
    /// Cursor keeps capability bubbles (`capabilityType` 15, sometimes `codeBlocks` and a
    /// checkpoint) with no text, tool call or thinking. The codec drops them; so does the count.
    #[test]
    fn inline_capability_bubbles_are_not_counted() {
        let conn = mk_global();
        let cid = "comp-capability";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-capability","_v":1,"conversation":[
                {"bubbleId":"u","type":1,"text":"fix it"},
                {"bubbleId":"r1","type":2,"text":"looking","serverBubbleId":"s1"},
                {"bubbleId":"c1","type":2,"capabilityType":15,"capabilitiesRan":{},"capabilityStatuses":{}},
                {"bubbleId":"r2","type":2,"text":"edited"},
                {"bubbleId":"c2","type":2,"capabilityType":15,"codeBlocks":[{"uri":"a.rs"}],"checkpoint":{}},
                {"bubbleId":"r3","type":2,"text":"done"}]}"#,
        );
        let mut out = Vec::new();
        discover_global(&conn, &conn.path, &HashMap::new(), &mut out);
        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(s.messages.iter().filter(|m| m.is_turn()).count(), 4);
        assert_eq!(out[0].message_count, Some(4));
    }

    #[test]
    fn empty_thread_is_skipped_in_discover() {
        let conn = mk_global();
        put(
            &conn,
            "composerData:empty",
            r#"{"composerId":"empty","_v":1,"conversation":[]}"#,
        );
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, &conn.path, &map, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn richtext_fallback_when_text_empty() {
        let conn = mk_global();
        let cid = "comp-rt";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-rt","_v":3,"fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        // text empty, richText holds a Lexical tree.
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"","richText":"{\"root\":{\"children\":[{\"children\":[{\"text\":\"from rich text\",\"type\":\"text\"}],\"type\":\"paragraph\"}],\"type\":\"root\"}}"}"#,
        );
        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("from rich text"));
    }

    #[test]
    fn tool_error_marks_result_as_error() {
        let conn = mk_global();
        let cid = "comp-err";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-err","_v":3,"fullConversationHeadersOnly":[{"bubbleId":"b1","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":2,"text":"oops",
                "toolFormerData":{"name":"read_file","toolCallId":"t9","status":"error","params":"{\"path\":\"/x\"}","error":"not found"}}"#,
        );
        let s = parse_global(&conn, &conn.sref(cid)).unwrap();
        let tr = s.messages[0].content.iter().find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
            _ => None,
        });
        let (content, is_error) = tr.expect("tool result");
        assert!(is_error);
        assert!(content.contains("not found"));
    }

    #[test]
    fn missing_table_does_not_panic() {
        // A DB with no cursorDiskKV table at all (older layout) must yield no rows, not error.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT, value BLOB)", [])
            .unwrap();
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn legacy_workspace_chatdata() {
        let conn = mk_global();
        let blob = r#"{"tabs":[{"tabId":"tab-1","chatTitle":"Old Tab",
            "bubbles":[{"type":"user","text":"legacy question"},{"type":"ai","text":"legacy answer"}]}]}"#;
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![legacy::CHAT_KEY, blob],
        )
        .unwrap();

        let mut out = Vec::new();
        legacy::discover(&conn, &conn.path, Some(Path::new("/tmp/proj")), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "legacy:tab-1");
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/tmp/proj")));
        assert_eq!(out[0].message_count, Some(2));

        let mut r = conn.sref("legacy:tab-1");
        r.cwd = Some(PathBuf::from("/tmp/proj"));
        let mut sink = crate::stream::CollectSink::default();
        let s = legacy::stream(&conn, &Default::default(), &r, "tab-1", false, &mut sink).unwrap();
        assert_eq!(s.title.as_deref(), Some("Old Tab"));
        assert_eq!(sink.messages.len(), 2);
        assert_eq!(sink.messages[0].role, Role::User);
        assert_eq!(sink.messages[0].text().as_deref(), Some("legacy question"));
        assert_eq!(sink.messages[1].role, Role::Assistant);
    }

    /// A file store with one composer and external bubbles, for the decoder's behavior tests.
    fn fixture(dir: &Path, root: &str, bubbles: &[(&str, &[u8])]) -> (Connection, SessionRef) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("state.vscdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")
            .unwrap();
        conn.execute("INSERT INTO cursorDiskKV VALUES('composerData:test',?1)", [root])
            .unwrap();
        for (id, raw) in bubbles {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES(?1,?2)",
                rusqlite::params![format!("bubbleId:test:{id}"), raw],
            )
            .unwrap();
        }
        (conn, SessionRef::new("test", Harness::Cursor, path))
    }
    /// The bulk read's messages; the lazy read resolves to the same (spans pick representation only).
    fn read_both(conn: &Connection, r: &SessionRef) -> Vec<Message> {
        let mut bulk = crate::stream::CollectSink::default();
        let mut lazy = crate::stream::CollectSink::default();
        let bulk_session = stream_global(conn, &Default::default(), r, false, &mut bulk).unwrap();
        let lazy_session = stream_global(conn, &Default::default(), r, true, &mut lazy).unwrap();
        assert_eq!(
            serde_json::to_value(bulk_session).unwrap(),
            serde_json::to_value(lazy_session).unwrap()
        );
        let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
        for message in &mut lazy.messages {
            message.materialize(&resolver).unwrap();
        }
        assert_eq!(
            serde_json::to_value(&bulk.messages).unwrap(),
            serde_json::to_value(&lazy.messages).unwrap()
        );
        bulk.messages
    }

    #[test]
    fn giant_identity_and_model_stay_inline_and_set_the_session_model() {
        let world = crate::test_world("cursor-giant-metadata");
        let id = "bubble-🌙".repeat(crate::lazy::INLINE_MAX);
        let model = "native-model-🌙".repeat(crate::lazy::INLINE_MAX);
        let root = serde_json::json!({"fullConversationHeadersOnly":[{"bubbleId":id}]});
        let bubble =
            serde_json::json!({"bubbleId":id,"type":2,"text":"retained message","modelInfo":{"modelName":model}});
        let (conn, r) = fixture(&world.base, &root.to_string(), &[(&id, bubble.to_string().as_bytes())]);
        let messages = read_both(&conn, &r);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id.as_deref(), Some(id.as_str()));
        assert_eq!(messages[0].model.as_deref(), Some(model.as_str()));
        let session = stream_global(
            &conn,
            &Default::default(),
            &r,
            true,
            &mut crate::stream::CollectSink::default(),
        )
        .unwrap();
        assert_eq!(session.model.as_deref(), Some(model.as_str()));
    }

    #[test]
    fn lazy_demands_keep_long_text_and_arguments_in_the_store() {
        use crate::json::Json;
        use crate::lazy::Text;
        let world = crate::test_world("cursor-ide-facts");
        let big = "日本語\n".repeat(8192);
        let root = serde_json::json!({"name":"composer","createdAt":1,"lastUpdatedAt":2,"unifiedMode":"agent","subagentInfo":{"parentComposerId":"parent","toolCallId":"spawn","subagentTypeName":"researcher","ignored":big},"modelConfig":{"modelName":"fallback","ignored":big},"fullConversationHeadersOnly":[{"bubbleId":"user"},{"bubbleId":"assistant"}],"conversation":[{"type":1,"text":"must not emit"}],"ignored":big});
        let user = serde_json::json!({"bubbleId":"user","type":1,"text":"question","unifiedMode":99});
        let assistant = serde_json::json!({"bubbleId":"assistant","type":2,"thinking":{"text":big,"signature":"sig"},"text":big,"modelInfo":{"modelName":"native-model","ignored":big},"tokenCount":{"inputTokens":10,"outputTokens":5,"ignored":big},"unifiedMode":1,"toolFormerData":{"name":"run_terminal_cmd","toolCallId":"call","rawArgs":serde_json::json!({"command":"cat /scratch/file","content":big}).to_string(),"result":big,"status":"ERROR","error":{"ignored":big},"restorable":["not-an-emitted-detail"]},"providerOptions":{"ignored":big}});
        let (conn, r) = fixture(
            &world.base,
            &root.to_string(),
            &[
                ("user", user.to_string().as_bytes()),
                ("assistant", assistant.to_string().as_bytes()),
            ],
        );
        let messages = read_both(&conn, &r);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content.len(), 4);
        let adapter = Cursor::with_user_dir(&world.base);
        let owned = crate::stream::collect_with(&adapter, &r, &Demand::LEAN).unwrap();
        assert_eq!(owned.title.as_deref(), Some("composer"));
        assert_eq!(owned.lineage.agent_path.as_deref(), Some("researcher"));
        assert_eq!(owned.model.as_deref(), Some("native-model"));
        let mut expected = Vec::new();
        crate::dataset::write_record(&owned, &mut expected, crate::dataset::Format::Chatml, None).unwrap();
        let query = crate::SessionQuery::parse("harness:cursor").unwrap();
        for filter in [None, Some(&query)] {
            let staged = crate::dataset::stage_record(
                &adapter,
                &r,
                crate::dataset::Format::Chatml,
                None,
                0,
                filter,
                &crate::query::TextSets::default(),
            )
            .unwrap()
            .unwrap();
            let mut actual = Vec::new();
            staged.publish(&mut actual).unwrap();
            actual.pop();
            assert_eq!(expected, actual);
        }
        for options in [Demand::FACTS.with_spans(), Demand::LEAN.with_spans()] {
            let collected = crate::stream::collect_with(&adapter, &r, &options).unwrap();
            let content = &collected.messages[1].content;
            assert!(matches!(&content[1], Block::Text { text: Text::Source(_) }));
            assert!(matches!(
                &content[2],
                Block::ToolUse {
                    input: Json::Source(_),
                    ..
                }
            ));
        }
    }

    #[test]
    fn array_authority_argument_fallback_and_lexical_results() {
        let world = crate::test_world("cursor-ide-edges");
        let mut bubbles = Vec::new();
        for raw in ["true", "null", "17", "\"literal\"", " ", "{"] {
            bubbles.push(serde_json::json!({"type":2,"toolFormerData":{"rawArgs":raw,"params":"[1,2]","result":{"z":2,"a":1},"status":"error"},"modelInfo":{"modelName":null},"modelType":"must-not-fallback"}));
        }
        bubbles.extend([
            serde_json::json!({"type":2,"text":"","richText":{"root":{"children":[{"text":"line","children":[{"text":" two"}]}]}},"unifiedMode":{}}),
            serde_json::json!({"type":2,"richText":serde_json::json!({"root":{"text":"encoded lexical"}}).to_string()}),
            serde_json::json!({"type":2,"toolFormerData":null}),
            serde_json::json!({"type":2,"toolFormerData":{"params":"invalid string","result":null,"error":"failure"}}),
            serde_json::json!({"type":2,"richText":{"root":{"text":"start","children":[{"text":"日本語","children":null},{"text":" next"},{"children":[{"text":"last"}]}]}}}),
            serde_json::json!({"type":2,"richText":{"root":{"text":" \n\t","children":[null,17,{"text":false}]}}}),
            serde_json::json!({"type":2,"richText":"{broken"}),
            serde_json::json!({"type":2,"richText":"","toolFormerData":{"name":"shell","result":"still visible"}}),
            serde_json::json!({"type":2,"toolFormerData":{"result":[true,null,1.5,{"z":"日本語\n","a":"\\quoted\""}]}}),
            serde_json::json!({"type":2,"richText":{"root":{"text":"\u{2003}".repeat(2048)}}}),
        ]);
        let root = serde_json::json!({"conversation":bubbles,"fullConversationHeadersOnly":false});
        let (conn, r) = fixture(&world.base.join("inline"), &root.to_string(), &[]);
        let messages = read_both(&conn, &r);
        let input = |m: &Message| match &m.content[0] {
            Block::ToolUse { input, .. } => input.resolve().unwrap().into_owned(),
            other => panic!("{other:?}"),
        };
        let inputs: Vec<_> = messages[..6].iter().map(input).collect();
        let fallback = serde_json::json!([1, 2]);
        assert_eq!(
            inputs,
            [
                serde_json::json!(true),
                Value::Null,
                serde_json::json!(17),
                serde_json::json!("literal"),
                fallback.clone(),
                fallback
            ]
        );
        assert!(messages[..6].iter().all(|m| m.model.is_none()));
        // No bubble names a call id: each call's is derived from its bubble's place, and its
        // result pairs with it.
        for (ordinal, m) in messages[..6].iter().enumerate() {
            let expected = format!("cv:cursor:{ordinal}");
            assert!(matches!(&m.content[0], Block::ToolUse { id, .. } if *id == expected));
            assert!(matches!(&m.content[1], Block::ToolResult { tool_use_id, .. } if *tool_use_id == expected));
        }
        assert!(
            matches!(&messages[0].content[1], Block::ToolResult { content, is_error: true, .. } if &**content == r#"{"z":2,"a":1}"#)
        );
        let texts: Vec<_> = messages[6..].iter().map(|m| m.text()).collect();
        assert_eq!(texts[0].as_deref(), Some("line two"));
        assert_eq!(texts[1].as_deref(), Some("encoded lexical"));
        assert!(
            matches!(&messages[8].content[..], [Block::ToolUse { name, input, .. }] if name == "tool" && input.resolve().unwrap().is_null())
        );
        assert!(
            matches!(&messages[9].content[0], Block::ToolUse { input, .. } if input.resolve().unwrap().as_str() == Some("invalid string"))
        );
        assert!(
            matches!(&messages[9].content[1], Block::ToolResult { content, is_error: true, .. } if &**content == "failure")
        );
        assert_eq!(messages[10].text().as_deref(), Some("start日本語\n nextlast"));
        // Blank and broken editor states carry nothing (those bubbles are skipped); a tool call shows.
        assert!(matches!(&messages[11].content[1], Block::ToolResult { content, .. } if &**content == "still visible"));
        assert!(
            matches!(&messages[12].content[1], Block::ToolResult { content, .. } if &**content == r#"[true,null,1.5,{"z":"日本語\n","a":"\\quoted\""}]"#)
        );
        assert_eq!(messages.len(), 13);
        for (index, root) in [
            r#"{"conversation":[{"type":1,"text":"first"}],"conversation":[{"type":1,"text":"last"}]}"#,
            r#"{"conversation":[{"type":1,"text":"fallback"}],"fullConversationHeadersOnly":[]}"#,
            r#"{"conversation":[{"type":1,"text":"fallback"}],"fullConversationHeadersOnly":[],"fullConversationHeadersOnly":false}"#,
            r#"{"fullConversationHeadersOnly":[{"bubbleId":"missing"},{"bubbleId":"invalid"},{"bubbleId":"empty"},{"bubbleId":"lossy"}]}"#,
            "null",
        ]
        .into_iter()
        .enumerate()
        {
            let (conn, r) = fixture(
                &world.base.join(index.to_string()),
                root,
                &[
                    ("invalid", b"{broken"),
                    ("empty", b""),
                    ("lossy", b"{\"type\":1,\"text\":\"bad\xff\"}"),
                ],
            );
            let texts: Vec<_> = read_both(&conn, &r).iter().map(|m| m.text()).collect();
            let expected: &[&str] = [&["last"][..], &[], &["fallback"], &["bad\u{fffd}"], &[]][index];
            assert_eq!(texts, expected.iter().map(|t| Some(t.to_string())).collect::<Vec<_>>(), "{root}");
        }
        for (index, root) in [
            r#"{"conversation":[],"ignored":"\uD800"}"#,
            r#"{"conversation":[],"ignored":1e9999}"#,
            r#"{"conversation":[]} trailing"#,
        ]
        .into_iter()
        .enumerate()
        {
            let (conn, r) = fixture(&world.base.join(format!("invalid-{index}")), root, &[]);
            assert!(stream_global(
                &conn,
                &Default::default(),
                &r,
                true,
                &mut crate::stream::CollectSink::default()
            )
            .is_err());
        }
    }

    #[test]
    fn invalid_external_bubbles_are_skipped_rows() {
        let world = crate::test_world("cursor-rich-lazy-validation");
        let root = r#"{"name":"title","modelConfig":{"modelName":"fallback"},"fullConversationHeadersOnly":[{"bubbleId":"first"},{"bubbleId":"bad-escape"},{"bubbleId":"bad-number"},{"bubbleId":"bad-type"},{"bubbleId":"missing"},{"bubbleId":"last"}]}"#;
        let (conn, r) = fixture(
            &world.base,
            root,
            &[
                ("first", br#"{"bubbleId":"first","type":1,"text":"first","unifiedMode":99}"#),
                ("bad-escape", br#"{"type":2,"text":"must be omitted","providerOptions":{"unused":"\ud800"}}"#),
                ("bad-number", br#"{"type":2,"text":"must be omitted","providerOptions":{"unused":1e9999}}"#),
                ("bad-type", br#"{"type":"future","text":false,"thinking":false,"toolFormerData":null,"modelInfo":{"modelName":null},"modelType":"do not fallback","unifiedMode":{}}"#),
                ("last", br#"{"bubbleId":"last","type":2,"text":"last","modelInfo":{"modelName":"last-model"},"unifiedMode":5}"#),
            ],
        );
        let messages = read_both(&conn, &r);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].model, None);
        assert_eq!(messages.last().unwrap().model.as_deref(), Some("last-model"));
        drop(conn);
        let parsed = crate::stream::collect_with(&Cursor::with_user_dir(&world.base), &r, &Demand::RICH).unwrap();
        assert_eq!(
            parsed.extra["cv"]["skipped_lines"], 3,
            "bad-escape, bad-number, and missing external bubbles must all be counted in skipped_lines"
        );
    }

    #[test]
    fn cold_wal_adapter_preserves_original_sidecars_and_journal_guard() {
        let world = crate::test_world("cursor-ide-cold-wal");
        let root = serde_json::json!({"conversation":[{"type":1,"text":"日本語\n".repeat(4096)}]});
        let (conn, r) = fixture(&world.base, &root.to_string(), &[]);
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        drop(conn);
        let wal = r.path.with_file_name("state.vscdb-wal");
        let shm = r.path.with_file_name("state.vscdb-shm");
        let journal = r.path.with_file_name("state.vscdb-journal");
        assert!(!wal.exists() && !shm.exists());
        let adapter = Cursor::with_user_dir(&world.base);
        let retained = crate::stream::collect_with(&adapter, &r, &Demand::RICH.with_spans()).unwrap();
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
            .stream(&r, &Demand::LEAN, &mut Stop(journal))
            .unwrap_err()
            .to_string()
            .contains("dependencies changed"));
        assert!(!wal.exists() && !shm.exists());
    }

    #[test]
    fn wal_guard_covers_retained_text_and_an_intentional_stop() {
        let world = crate::test_world("cursor-ide-facts-guard");
        let root = serde_json::json!({"conversation":[{"type":1,"text":"日本語\n".repeat(4096)}]});
        let (conn, r) = fixture(&world.base, &root.to_string(), &[]);
        let wal = r.path.with_file_name("state.vscdb-wal");
        let mut reads = crate::source::ReadSet::default();
        reads.record(&wal);
        let mut sink = crate::stream::CollectSink::default();
        stream_global(&conn, &reads, &r, true, &mut sink).unwrap();
        std::fs::write(&wal, b"changed WAL").unwrap();
        let Block::Text {
            text: crate::lazy::Text::Source(text),
        } = &sink.messages[0].content[0]
        else {
            panic!("expected source text")
        };
        assert!(text.try_prefix(10).is_err());
        std::fs::remove_file(&wal).unwrap();
        struct Stop(PathBuf);
        impl MessageSink for Stop {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::write(&self.0, b"changed WAL").unwrap();
                Flow::Stop
            }
        }
        assert!(stream_global(&conn, &reads, &r, true, &mut Stop(wal))
            .unwrap_err()
            .to_string()
            .contains("dependencies changed"));
    }

    #[test]
    fn file_uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///Users/x/proj"),
            Some(PathBuf::from("/Users/x/proj"))
        );
        assert_eq!(
            file_uri_to_path("file:///Users/x/Application%20Support"),
            Some(PathBuf::from("/Users/x/Application Support"))
        );
        assert_eq!(file_uri_to_path("vscode-remote://foo/bar"), None);
    }
}
