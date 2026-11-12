//! In-memory ingestion of an uploaded harness file tree — no filesystem access, so it works on
//! wasm32 (the browser app). Given `(relative_path, bytes)` pairs, sniff each file's harness by
//! content and parse it into IR [`Session`]s.
//!
//! OWNED BY the wasm-ingest work. `cv-core::lib` declares this module.

use crate::harness::store::document::range::{RangeSource, Validation};
use crate::harness::{antigravity, claude, codex, continuedev, export, gemini, openclaw, opensession, qwen};
use crate::ir::Session;
use crate::json::JsonView;
use serde::{Deserialize, Serialize};

#[cfg(feature = "zip-ingest")]
pub mod archive;

/// Structured diagnostic for a zip/upload entry that produced no session during ingestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

/// Complete ingestion result: parsed [`Session`]s alongside structured diagnostics for skipped entries.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestReport {
    pub sessions: Vec<Session>,
    pub skipped: Vec<SkippedEntry>,
}

/// Strict borrowed shape inspection: ignored uploaded bodies never become a second Value tree.
fn inspect<T>(text: &str, f: impl FnOnce(JsonView<'_>) -> T) -> Option<T> {
    let source = RangeSource::text(None, text, 0, false);
    let value = source.accept(source.range(), Validation::Value).ok()??;
    Some(f(JsonView::of(&source, value)))
}
/// The fields of an object view; a nonobject (or a read failure) has none.
struct Fields<'a>(JsonView<'a>);
fn fields(view: JsonView<'_>) -> Fields<'_> {
    Fields(view)
}
impl<'a> Fields<'a> {
    fn get(&self, key: &str) -> Option<JsonView<'a>> {
        self.0.get(key).ok().flatten()
    }
    fn string(&self, key: &str) -> Option<String> {
        let value = self.get(key)?;
        value
            .is_string()
            .ok()?
            .then(|| value.resolve().ok())??
            .as_str()
            .map(str::to_owned)
    }
}
/// An array view's items, in order; a nonarray has none.
fn items<'a>(view: JsonView<'a>) -> impl Iterator<Item = JsonView<'a>> {
    view.array().ok().flatten().into_iter().flatten().filter_map(Result::ok)
}

/// Which single-file harness a given file looks like.
enum Sniff {
    Codex { is_jsonl: bool },
    Claude,
    Gemini,
    Antigravity,
    Qwen,
    OpenClaw,
    OpenSession,
    Export,
    Continue,
    Unknown,
}

/// Is this JSONL line a Qwen 0.25 `ChatRecord` (`{uuid, parentUuid, sessionId, type, message:{parts}, ...}`)?
/// Checked BEFORE Gemini and Claude so `type:"user"|"assistant"` + `sessionId`/`parentUuid` never
/// misclassifies Qwen 0.25 transcripts as Claude or Gemini.
fn is_qwen_chat_record_view(raw: JsonView<'_>) -> bool {
    let r = fields(raw);
    let has_thread = r.get("sessionId").is_some() || r.get("parentUuid").is_some();
    if !has_thread {
        return false;
    }
    let ty = r.string("type").unwrap_or_default();
    if !matches!(ty.as_str(), "user" | "assistant" | "system" | "tool_result") {
        return false;
    }
    let msg_has_parts = r.get("message").map(fields).is_some_and(|m| m.get("parts").is_some());
    msg_has_parts
        || r.get("usageMetadata").is_some()
        || r.get("toolCallResult").is_some()
        || r.get("systemPayload").is_some()
}

/// The file stem (basename without extension), used as a fallback session id.
fn file_stem(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, _ext)) if !stem.is_empty() => stem,
        _ => base,
    }
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Sniff the harness from filename + content. More robust than relying on the path layout, since an
/// uploaded zip may have been re-rooted or renamed.
fn sniff(name: &str, text: &str) -> Sniff {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = base.to_ascii_lowercase();
    let is_jsonl = lower.ends_with(".jsonl");
    if lower.ends_with(".opensession.json") {
        return Sniff::OpenSession;
    }
    if let Some(first) = first_non_empty_line(text) {
        if first.starts_with('[') {
            if inspect(text, |raw| {
                items(raw).take(8).any(|item| {
                    let item = fields(item);
                    (item.get("sessionId").is_some()
                        && item.get("messageId").is_some()
                        && item.get("message").is_some())
                        || (item.get("role").is_some() && item.get("parts").is_some())
                })
            })
            .unwrap_or(false)
            {
                return Sniff::Gemini;
            }
            if !lower.starts_with("rollout-")
                && inspect(text, |raw| {
                    items(raw).take(8).any(|item| {
                        let item = fields(item);
                        item.get("mapping").is_some() || item.get("chat_messages").is_some()
                    })
                })
                .unwrap_or(false)
            {
                return Sniff::Export;
            }
        } else if first.starts_with('{') {
            if !is_jsonl
                && !lower.starts_with("rollout-")
                && inspect(text, |raw| {
                    let record = fields(raw);
                    record.get("open_session").is_some() || record.get("openSession").is_some()
                })
                .unwrap_or(false)
            {
                return Sniff::OpenSession;
            }
            if inspect(text, |raw| {
                let record = fields(raw);
                (record.get("messages").is_some()
                    && (record.get("sessionId").is_some() || record.get("projectHash").is_some()))
                    || record.get("history").into_iter().flat_map(items).take(8).any(|item| {
                        let item = fields(item);
                        item.get("role").is_some() && item.get("parts").is_some()
                    })
            })
            .unwrap_or(false)
            {
                return Sniff::Gemini;
            }
            if !is_jsonl && !lower.starts_with("rollout-") && lower != "sessions.json" {
                if let Some(doc_sniff) = inspect(text, |raw| {
                    let record = fields(raw);
                    if record.get("mapping").is_some() || record.get("chat_messages").is_some() {
                        return Some(Sniff::Export);
                    }
                    if let Some(hist) = record.get("history") {
                        if record.get("workspaceDirectory").is_some()
                            || items(hist).take(8).any(|item| fields(item).get("message").is_some())
                        {
                            return Some(Sniff::Continue);
                        }
                    }
                    None
                })
                .flatten()
                {
                    return doc_sniff;
                }
            }
            // Disambiguate Qwen 0.25 `ChatRecord` BEFORE Gemini JSONL and Claude JSONL.
            if !lower.starts_with("rollout-")
                && text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(8)
                    .any(|line| inspect(line, is_qwen_chat_record_view).unwrap_or(false))
            {
                return Sniff::Qwen;
            }
            if is_jsonl {
                if inspect(first, |raw| {
                    let v = fields(raw);
                    v.string("type").as_deref() == Some("session")
                        && v.get("version").is_some()
                        && v.get("id").is_some()
                })
                .unwrap_or(false)
                {
                    return Sniff::OpenClaw;
                }
                if inspect(first, |raw| {
                    let v = fields(raw);
                    v.get("projectHash").is_some() || v.get("messages").is_some()
                })
                .unwrap_or(false)
                {
                    return Sniff::Gemini;
                }
                if text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(8)
                    .any(|line| {
                        inspect(line, |raw| {
                            let v = fields(raw);
                            v.get("$set").is_some()
                                || v.get("$rewindTo").is_some()
                                || v.get("projectHash").is_some()
                                || v.get("thoughts").is_some()
                                || v.get("toolCalls").is_some()
                                || (v.get("parts").is_some() && v.get("parentUuid").is_none())
                        })
                        .unwrap_or(false)
                    })
                {
                    return Sniff::Gemini;
                }
            }
        }
    }
    if lower.starts_with("rollout-") {
        return Sniff::Codex { is_jsonl };
    }
    let in_chunks = name.split(['/', '\\']).any(|seg| seg == "chunks");
    if !in_chunks {
        if let Some(first) = first_non_empty_line(text) {
            if first.starts_with('{') && inspect(first, antigravity::is_step_record_view).unwrap_or(false) {
                return Sniff::Antigravity;
            }
        }
    }
    if let Some(first) = first_non_empty_line(text) {
        if first.contains("\"session_meta\"") {
            return Sniff::Codex { is_jsonl };
        }
        if inspect(first, |raw| {
            let v = fields(raw);
            v.get("type").is_none() && v.get("id").is_some() && v.get("timestamp").is_some()
        })
        .unwrap_or(false)
            && (text.contains("\"response_item\"") || text.contains("\"event_msg\""))
        {
            return Sniff::Codex { is_jsonl };
        }
    }
    let mut inspected = 0;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()).take(64) {
        if inspected >= 8 {
            break;
        }
        if let Some((ty, has_thread)) = inspect(line, |raw| {
            let r = fields(raw);
            (
                r.string("type").unwrap_or_default(),
                r.get("parentUuid").is_some() || r.get("sessionId").is_some(),
            )
        }) {
            // A record the Claude codec knows is no turn says nothing either way.
            if crate::harness::claude::record::META_TYPES.contains(&ty.as_str()) {
                continue;
            }
            if has_thread && matches!(ty.as_str(), "user" | "assistant") {
                return Sniff::Claude;
            }
        }
        inspected += 1;
    }
    Sniff::Unknown
}

/// Pull a session id out of a Codex transcript's meta/header if present, else fall back to stem.
fn codex_id(name: &str, text: &str) -> String {
    if let Some(first) = first_non_empty_line(text) {
        if let Some(id) = inspect(first, |raw| {
            let record = fields(raw);
            record
                .get("payload")
                .map(fields)
                .and_then(|p| p.string("id"))
                .or_else(|| {
                    if record.get("type").is_none() {
                        record.string("id")
                    } else {
                        None
                    }
                })
        })
        .flatten()
        {
            return id;
        }
    }
    file_stem(name).to_owned()
}

/// Recover `<uuid>` from `…/brain/<uuid>/…` when present, else fall back to filename stem.
fn antigravity_id_and_rank(name: &str) -> (String, u8) {
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let base = parts.last().copied().unwrap_or(name);
    let rank = match base {
        "transcript_full.jsonl" => 3,
        "transcript.jsonl" => 2,
        "overview.txt" => 1,
        _ => 0,
    };
    if let Some(pos) = parts.iter().position(|&seg| seg == "brain") {
        if let Some(&uuid) = parts.get(pos + 1) {
            if !uuid.is_empty() && rank > 0 {
                return (uuid.to_owned(), rank);
            }
        }
    }
    (file_stem(name).to_owned(), rank)
}

/// Ingest a set of in-memory files (e.g. the contents of an uploaded zip) into sessions.
///
/// Each file's bytes are decoded as UTF-8 (lossy) and its harness sniffed by content. Files that
/// can't be classified are skipped; use [`ingest_files_with_report`] for structured skipped-entry
/// diagnostics. Works without any filesystem access on wasm32, and supports temp-staged SQLite and
/// companion-file stores on native builds.
pub fn ingest_files(files: Vec<(String, Vec<u8>)>) -> anyhow::Result<Vec<Session>> {
    Ok(ingest_files_with_report(files)?.sessions)
}

/// Extract and ingest a `.zip` archive in one call, returning both parsed sessions and structured
/// skipped-entry diagnostics.
#[cfg(feature = "zip-ingest")]
pub fn ingest_zip(reader: impl std::io::Read + std::io::Seek) -> anyhow::Result<IngestReport> {
    let files = archive::extract(reader, archive::Limits::default())?;
    ingest_files_with_report(files)
}

fn is_sqlite_bytes(name: &str, bytes: &[u8]) -> bool {
    let lower = name.to_ascii_lowercase();
    bytes.starts_with(b"SQLite format 3\0")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite3")
        || lower.ends_with(".db")
        || lower.ends_with(".vscdb")
}

fn is_sqlite_sidecar(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with("-wal") || lower.ends_with("-shm") || lower.ends_with("-journal")
}

/// Ingest `(relative_path, bytes)` pairs and return both parsed sessions and structured
/// diagnostics for skipped entries.
pub fn ingest_files_with_report(files: Vec<(String, Vec<u8>)>) -> anyhow::Result<IngestReport> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut agy_candidates: std::collections::BTreeMap<String, (u8, usize, String)> = std::collections::BTreeMap::new();
    let mut pending_unknown: Vec<(String, Vec<u8>)> = Vec::new();

    for (idx, (name, bytes)) in files.into_iter().enumerate() {
        if is_sqlite_bytes(&name, &bytes) || is_sqlite_sidecar(&name) {
            pending_unknown.push((name, bytes));
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        match sniff(&name, &text) {
            Sniff::Codex { is_jsonl } => {
                let id = codex_id(&name, &text);
                out.push(codex::parse_str(&id, &text, is_jsonl, None)?);
            }
            Sniff::Claude => {
                let id = file_stem(&name).to_string();
                out.push(claude::parse_str(&id, &text, None));
            }
            Sniff::Gemini => {
                out.extend(gemini::parse_all_str(&text, None)?);
            }
            Sniff::Antigravity => {
                let (id, rank) = antigravity_id_and_rank(&name);
                let replace = agy_candidates
                    .get(&id)
                    .is_none_or(|(prev_rank, _, _)| rank > *prev_rank);
                if replace {
                    agy_candidates.insert(id, (rank, idx, text.into_owned()));
                }
            }
            Sniff::Qwen => {
                let id = file_stem(&name).to_string();
                out.push(qwen::parse_str(&id, &text)?);
            }
            Sniff::OpenClaw => {
                let id = file_stem(&name).to_string();
                out.push(openclaw::parse_str(&id, &text)?);
            }
            Sniff::OpenSession => {
                out.push(opensession::from_str(&text)?);
            }
            Sniff::Export => {
                out.extend(export::parse_all_str(&text)?);
            }
            Sniff::Continue => {
                let id = file_stem(&name).to_string();
                out.push(continuedev::parse_str(&id, &text)?);
            }
            Sniff::Unknown => {
                pending_unknown.push((name, bytes));
            }
        }
    }
    let mut agy_ordered: Vec<(usize, String, String)> = agy_candidates
        .into_iter()
        .map(|(id, (_rank, idx, text))| (idx, id, text))
        .collect();
    agy_ordered.sort_by_key(|(idx, _, _)| *idx);
    for (_, id, text) in agy_ordered {
        out.push(antigravity::parse_str(&id, &text, None)?);
    }

    if !pending_unknown.is_empty() {
        ingest_pending_entries(pending_unknown, &mut out, &mut skipped)?;
    }

    Ok(IngestReport { sessions: out, skipped })
}

#[cfg(target_arch = "wasm32")]
fn ingest_pending_entries(
    pending: Vec<(String, Vec<u8>)>,
    _out: &mut Vec<Session>,
    skipped: &mut Vec<SkippedEntry>,
) -> anyhow::Result<()> {
    for (name, bytes) in pending {
        if is_sqlite_sidecar(&name) {
            continue;
        }
        let reason = if is_sqlite_bytes(&name, &bytes) {
            "SQLite store requires native sqlite support".to_string()
        } else {
            "unrecognized or non-transcript file".to_string()
        };
        skipped.push(SkippedEntry { path: name, reason });
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn sanitize_relative_path(raw: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path, PathBuf};
    let mut clean = PathBuf::new();
    for comp in Path::new(raw).components() {
        match comp {
            Component::Normal(seg) => clean.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

#[cfg(not(target_arch = "wasm32"))]
fn ingest_pending_entries(
    pending: Vec<(String, Vec<u8>)>,
    out: &mut Vec<Session>,
    skipped: &mut Vec<SkippedEntry>,
) -> anyhow::Result<()> {
    use crate::harness::{Adapter as _, Parse as _};

    let has_stageable = pending.iter().any(|(name, bytes)| {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        is_sqlite_bytes(name, bytes)
            || is_sqlite_sidecar(name)
            || matches!(
                base,
                "summary.json"
                    | "chat_history.jsonl"
                    | "ui_messages.json"
                    | "api_conversation_history.json"
                    | "wire.jsonl"
            )
            || base.ends_with(".conversation.json")
    });

    if !has_stageable {
        for (name, _) in pending {
            skipped.push(SkippedEntry {
                path: name,
                reason: "unrecognized or non-transcript file".into(),
            });
        }
        return Ok(());
    }

    let temp_root = std::env::temp_dir().join(format!("cv-ingest-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_root)?;
    struct TempGuard(std::path::PathBuf);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = TempGuard(temp_root.clone());

    let mut staged_paths: Vec<(String, std::path::PathBuf, bool)> = Vec::new();
    for (name, bytes) in pending {
        let Some(rel) = sanitize_relative_path(&name) else {
            skipped.push(SkippedEntry {
                path: name,
                reason: "unsafe relative path in archive".into(),
            });
            continue;
        };
        let full = temp_root.join(&rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, &bytes)?;
        let sqlite_like = is_sqlite_bytes(&name, &bytes);
        staged_paths.push((name, full, sqlite_like));
    }

    let mut claimed_files: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();

    #[cfg(feature = "sqlite")]
    for (orig_name, full_path, sqlite_like) in &staged_paths {
        if !*sqlite_like {
            continue;
        }
        if let Ok(conn) = crate::harness::store::sqlite::open_ro(full_path) {
            enum SqliteFlavor {
                OpenClaw,
                Goose,
                Hermes,
                OpenCode,
                Unknown,
            }
            let flavor = conn
                .read(|raw, _| {
                    let has_table = |t: &str| crate::harness::store::sqlite::table_exists(raw, t);
                    if has_table("session_windows") && has_table("transcript_events") {
                        Ok(SqliteFlavor::OpenClaw)
                    } else if has_table("sessions") && has_table("messages") {
                        let is_goose = raw
                            .prepare("PRAGMA table_info(messages)")
                            .and_then(|mut st| {
                                let cols: Vec<String> = st
                                    .query_map([], |row| row.get::<_, String>(1))?
                                    .filter_map(Result::ok)
                                    .collect();
                                Ok(cols.iter().any(|c| c == "content_json"))
                            })
                            .unwrap_or(false);
                        Ok(if is_goose {
                            SqliteFlavor::Goose
                        } else {
                            SqliteFlavor::Hermes
                        })
                    } else if has_table("session") && has_table("part") {
                        Ok(SqliteFlavor::OpenCode)
                    } else {
                        Ok(SqliteFlavor::Unknown)
                    }
                })
                .unwrap_or(SqliteFlavor::Unknown);
            drop(conn);

            let parsed_sqlite: anyhow::Result<Option<Vec<Session>>> = match flavor {
                SqliteFlavor::OpenClaw => {
                    // Stage OpenClaw SQLite under `<state>/agents/main/agent/openclaw-agent.sqlite` if renamed.
                    let oc_state = temp_root.join(format!("_oc_{}", uuid::Uuid::new_v4()));
                    let oc_db_dir = oc_state.join("agents/main/agent");
                    std::fs::create_dir_all(&oc_db_dir)?;
                    let target_db = oc_db_dir.join("openclaw-agent.sqlite");
                    std::fs::copy(full_path, &target_db)?;
                    for suffix in ["-wal", "-shm"] {
                        let src_side = std::path::PathBuf::from(format!("{}{suffix}", full_path.display()));
                        if src_side.is_file() {
                            let _ = std::fs::copy(
                                &src_side,
                                std::path::PathBuf::from(format!("{}{suffix}", target_db.display())),
                            );
                        }
                    }
                    let adapter = crate::harness::openclaw::OpenClaw::with_state_dir(&oc_state);
                    let disc = adapter.discover()?;
                    let mut sessions = Vec::new();
                    for r in &disc.refs {
                        let mut s = adapter.parse(r)?;
                        s.source_path = None;
                        sessions.push(s);
                    }
                    Ok(Some(sessions))
                }
                SqliteFlavor::Goose => {
                    let g_dir = temp_root.join(format!("_goose_{}", uuid::Uuid::new_v4()));
                    std::fs::create_dir_all(&g_dir)?;
                    let target_db = g_dir.join("sessions.db");
                    std::fs::copy(full_path, &target_db)?;
                    let adapter = crate::harness::goose::Goose::with_sessions_dir(&g_dir);
                    let disc = adapter.discover()?;
                    let mut sessions = Vec::new();
                    for r in &disc.refs {
                        let mut s = adapter.parse(r)?;
                        s.source_path = None;
                        sessions.push(s);
                    }
                    Ok(Some(sessions))
                }
                SqliteFlavor::Hermes => {
                    let adapter = crate::harness::hermes::Hermes::with_db(full_path.clone());
                    let disc = adapter.discover()?;
                    let mut sessions = Vec::new();
                    for r in &disc.refs {
                        let mut s = adapter.parse(r)?;
                        s.source_path = None;
                        sessions.push(s);
                    }
                    Ok(Some(sessions))
                }
                SqliteFlavor::OpenCode => {
                    let adapter = crate::harness::opencode::OpenCode::with_db(full_path.clone());
                    let disc = adapter.discover()?;
                    let mut sessions = Vec::new();
                    for r in &disc.refs {
                        let mut s = adapter.parse(r)?;
                        s.source_path = None;
                        sessions.push(s);
                    }
                    Ok(Some(sessions))
                }
                SqliteFlavor::Unknown => Ok(None),
            };
            match parsed_sqlite? {
                Some(mut sessions) => {
                    claimed_files.insert(full_path.clone());
                    out.append(&mut sessions);
                }
                None => {
                    claimed_files.insert(full_path.clone());
                    skipped.push(SkippedEntry {
                        path: orig_name.clone(),
                        reason: "unrecognized SQLite schema".into(),
                    });
                }
            }
        }
    }

    #[cfg(not(feature = "sqlite"))]
    for (orig_name, full_path, sqlite_like) in &staged_paths {
        if *sqlite_like {
            claimed_files.insert(full_path.clone());
            skipped.push(SkippedEntry {
                path: orig_name.clone(),
                reason: "SQLite store requires sqlite feature".into(),
            });
        }
    }

    // Check directory-paired adapters (Grok `summary.json` + `chat_history.jsonl`, LmStudio `.conversation.json`).
    let grok = crate::harness::grok::Grok::with_home(&temp_root);
    if let Ok(disc) = grok.discover() {
        for r in &disc.refs {
            if let Ok(mut s) = grok.parse(r) {
                claimed_files.insert(r.path.join("summary.json"));
                claimed_files.insert(r.path.join("chat_history.jsonl"));
                claimed_files.insert(r.path.join("updates.jsonl"));
                claimed_files.insert(r.path.join("plan.json"));
                claimed_files.insert(r.path.join("rewind_points.jsonl"));
                s.source_path = None;
                out.push(s);
            }
        }
    }

    let lm = crate::harness::lmstudio::LmStudio::with_root(&temp_root);
    if let Ok(disc) = lm.discover() {
        for r in &disc.refs {
            if let Ok(mut s) = lm.parse(r) {
                claimed_files.insert(r.path.clone());
                s.source_path = None;
                out.push(s);
            }
        }
    }

    for (orig_name, full_path, _) in staged_paths {
        if is_sqlite_sidecar(&orig_name) || claimed_files.contains(&full_path) {
            continue;
        }
        skipped.push(SkippedEntry {
            path: orig_name,
            reason: "unrecognized or non-transcript file".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;

    const CLAUDE_SAMPLE: &str = r#"{"type":"user","sessionId":"abc","parentUuid":null,"uuid":"u1","cwd":"/home/x","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}
{"type":"assistant","sessionId":"abc","parentUuid":"u1","uuid":"u2","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi there"}]}}"#;

    const CODEX_SAMPLE: &str = r#"{"timestamp":"2025-01-02T00:00:00Z","type":"session_meta","payload":{"id":"sess-123","cwd":"/work","timestamp":"2025-01-02T00:00:00Z"}}
{"timestamp":"2025-01-02T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"do a thing"}}
{"timestamp":"2025-01-02T00:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#;

    #[test]
    fn ingests_claude_and_codex() {
        let files = vec![
            ("abc.jsonl".to_string(), CLAUDE_SAMPLE.as_bytes().to_vec()),
            (
                "rollout-2025-01-02-sess.jsonl".to_string(),
                CODEX_SAMPLE.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files).unwrap();
        assert_eq!(sessions.len(), 2);

        let claude = sessions
            .iter()
            .find(|s| s.harness == Harness::Claude)
            .expect("claude session");
        assert_eq!(claude.id, "abc");
        assert_eq!(claude.messages.len(), 2);

        let codex = sessions
            .iter()
            .find(|s| s.harness == Harness::Codex)
            .expect("codex session");
        assert_eq!(codex.id, "sess-123");
        assert_eq!(codex.messages.len(), 2);
    }

    #[test]
    fn claude_with_many_leading_meta_records_still_sniffs() {
        // Real transcripts can open with >8 summary / file-history-snapshot records; they must not
        // exhaust the sniff window and drop the upload as Unknown.
        let mut lines: Vec<String> = (0..9)
            .map(|i| format!(r#"{{"type":"summary","summary":"topic {i}","leafUuid":"l{i}"}}"#))
            .collect();
        lines.push(r#"{"type":"file-history-snapshot","messageId":"m0","snapshot":{"trackedFileBackups":{}}}"#.into());
        // Every type the codec knows is no turn is skipped, not just the ones sniffing once named.
        lines.extend((0..9).map(|i| format!(r#"{{"type":"custom-title","customTitle":"t{i}"}}"#)));
        lines.push(r#"{"type":"cost-state","totalCostUsd":0}"#.into());
        lines.push(CLAUDE_SAMPLE.to_string());
        let text = lines.join("\n");

        let sessions = ingest_files(vec![("abc.jsonl".to_string(), text.into_bytes())]).unwrap();
        assert_eq!(sessions.len(), 1, "meta-heavy claude transcript should ingest");
        assert_eq!(sessions[0].harness, Harness::Claude);
        assert_eq!(sessions[0].messages.len(), 2);
    }

    #[test]
    fn skips_unclassifiable() {
        let files = vec![("random.txt".to_string(), b"just some text".to_vec())];
        assert!(ingest_files(files).unwrap().is_empty());
    }

    // A legacy whole-file ConversationRecord object (sessionId + projectHash + messages).
    const GEMINI_LEGACY_RECORDING: &str = r#"{"sessionId":"g-legacy-1","projectHash":"deadbeef","startTime":"2025-03-01T00:00:00Z","lastUpdated":"2025-03-01T00:01:00Z","messages":[{"id":"m1","type":"user","timestamp":"2025-03-01T00:00:00Z","content":"hi gemini"},{"id":"m2","type":"gemini","timestamp":"2025-03-01T00:00:30Z","model":"gemini-3-flash","content":[{"text":"hello back"}]}]}"#;

    // A modern append-only .jsonl recording: metadata line + message records + $set + $rewindTo.
    const GEMINI_MODERN_JSONL: &str = r#"{"sessionId":"g-modern-1","projectHash":"cafef00d","startTime":"2025-03-02T00:00:00Z"}
{"id":"u1","type":"user","timestamp":"2025-03-02T00:00:00Z","content":"list files"}
{"id":"a1","type":"gemini","timestamp":"2025-03-02T00:00:10Z","model":"gemini-3","content":[{"text":"sure"}],"toolCalls":[{"id":"t1","name":"list_directory","args":{},"result":"main.py"}]}
{"$set":{"summary":"Listed the project files."}}"#;

    // A checkpoint: {history: Content[]} with {role, parts}.
    const GEMINI_CHECKPOINT: &str =
        r#"{"history":[{"role":"user","parts":[{"text":"what is 2+2"}]},{"role":"model","parts":[{"text":"4"}]}]}"#;

    #[test]
    fn ingests_gemini_legacy_recording() {
        let files = vec![(
            "session-123.json".to_string(),
            GEMINI_LEGACY_RECORDING.as_bytes().to_vec(),
        )];
        let sessions = ingest_files(files).unwrap();
        assert!(!sessions.is_empty(), "legacy recording should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
        assert_eq!(sessions[0].id, "g-legacy-1");
    }

    #[test]
    fn ingests_gemini_modern_jsonl() {
        let files = vec![("session-456.jsonl".to_string(), GEMINI_MODERN_JSONL.as_bytes().to_vec())];
        let sessions = ingest_files(files).unwrap();
        assert!(!sessions.is_empty(), "modern jsonl recording should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
        assert_eq!(sessions[0].id, "g-modern-1");
    }

    #[test]
    fn ingests_gemini_checkpoint() {
        let files = vec![(
            "checkpoint-test.json".to_string(),
            GEMINI_CHECKPOINT.as_bytes().to_vec(),
        )];
        let sessions = ingest_files(files).unwrap();
        assert!(!sessions.is_empty(), "checkpoint should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
    }

    #[test]
    fn gemini_shapes_dont_regress_claude_codex() {
        // Claude and Codex samples must still classify correctly alongside the gemini widening.
        let files = vec![
            ("abc.jsonl".to_string(), CLAUDE_SAMPLE.as_bytes().to_vec()),
            (
                "rollout-2025-01-02-sess.jsonl".to_string(),
                CODEX_SAMPLE.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files).unwrap();
        assert!(sessions.iter().any(|s| s.harness == Harness::Claude));
        assert!(sessions.iter().any(|s| s.harness == Harness::Codex));
        assert!(!sessions.iter().any(|s| s.harness == Harness::Gemini));
    }
    #[test]
    fn borrowed_sniff_acceptance_and_shape_windows_are_the_recorded_ones() {
        fn label(value: Sniff) -> u8 {
            match value {
                Sniff::Unknown => 0,
                Sniff::Claude => 1,
                Sniff::Gemini => 2,
                Sniff::Codex { is_jsonl: false } => 3,
                Sniff::Codex { is_jsonl: true } => 4,
                Sniff::Antigravity => 5,
                Sniff::Qwen => 6,
                Sniff::OpenClaw => 7,
                Sniff::OpenSession => 8,
                Sniff::Export => 9,
                Sniff::Continue => 10,
            }
        }
        let body = "large escaped λ\n\"".repeat(5000);
        let cases = [
            CLAUDE_SAMPLE.to_owned(),
            CODEX_SAMPLE.to_owned(),
            GEMINI_LEGACY_RECORDING.to_owned(),
            GEMINI_MODERN_JSONL.to_owned(),
            GEMINI_CHECKPOINT.to_owned(),
            serde_json::json!({"messages":[],"sessionId":"id","ignored":body}).to_string(),
            serde_json::json!([{"role":"user","parts":[{"text":body}]}]).to_string(),
            serde_json::json!({"type":"user","sessionId":null,"message":{"content":body}}).to_string(),
            r#"{"type":"user","sessionId":null,"ignored":"\ud800"}"#.into(),
            r#"{"type":"user","ty\u0070e":"assistant","sessionId":"id","ignored":1e400}"#.into(),
            r#"{"type":"user","sessionId":null,"ignored":1e400,"ignored":0}"#.into(),
            r#"{"id":"bare","timestamp":0}
{"type":"response_item"}"#
                .into(),
            "[]".into(),
            "not JSON".into(),
        ];
        // Recorded from the retired owned-Value sniffer, which the borrowed one matched for every
        // name x case (names outermost, cases in the order above).
        #[rustfmt::skip]
        let recorded: [u8; 42] = [
            1, 3, 2, 0, 2, 2, 2, 1, 0, 0, 0, 3, 0, 0,
            1, 4, 2, 2, 2, 2, 2, 1, 0, 0, 0, 4, 0, 0,
            4, 4, 2, 2, 2, 2, 2, 4, 4, 4, 4, 4, 4, 4,
        ];
        let mut actual = Vec::new();
        for name in ["upload.json", "upload.jsonl", "rollout-renamed.jsonl"] {
            for text in &cases {
                actual.push(label(sniff(name, text)));
            }
        }
        assert_eq!(actual, recorded);
        let snapshots = (0..63)
            .map(|_| serde_json::json!({"type":"file-history-snapshot","ignored":body}).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("{snapshots}\n{CLAUDE_SAMPLE}");
        // 63 meta records do not consume the 8-line window; the 64th line is the first conversational one.
        assert_eq!(label(sniff("history.jsonl", &text)), 1);
    }

    #[test]
    fn ingests_antigravity_and_prefers_full_log_over_compact_and_chunks() {
        let full = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-11T10:00:00Z","content":"full prompt"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-11T10:00:01Z","content":"full reply"}"#;
        let compact = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-11T10:00:00Z","content":"trunc","truncated_fields":["content"]}"#;
        let files = vec![
            (
                "brain/11111111-2222-4333-8444-555555555555/.system_generated/logs/transcript.jsonl".into(),
                compact.as_bytes().to_vec(),
            ),
            (
                "brain/11111111-2222-4333-8444-555555555555/.system_generated/logs/transcript_full.jsonl".into(),
                full.as_bytes().to_vec(),
            ),
            (
                "brain/11111111-2222-4333-8444-555555555555/.system_generated/logs/chunks/transcript_full/0.jsonl"
                    .into(),
                full.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].harness, Harness::Antigravity);
        assert_eq!(sessions[0].id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(sessions[0].messages.len(), 2);
    }

    #[test]
    fn disambiguates_qwen_025_before_claude_and_gemini_and_reports_skipped_entries() {
        let qwen_025 = [
            serde_json::json!({
                "uuid": "q-u1",
                "parentUuid": null,
                "sessionId": "qwen-sess-1",
                "timestamp": "2026-06-01T10:00:00Z",
                "type": "user",
                "cwd": "/repo/qwen",
                "message": { "role": "user", "parts": [{ "text": "hello qwen 0.25" }] }
            })
            .to_string(),
            serde_json::json!({
                "uuid": "q-u2",
                "parentUuid": "q-u1",
                "sessionId": "qwen-sess-1",
                "timestamp": "2026-06-01T10:00:02Z",
                "type": "assistant",
                "model": "qwen3-coder-plus",
                "message": { "role": "model", "parts": [{ "text": "hi from qwen" }] },
                "usageMetadata": { "promptTokenCount": 12, "candidatesTokenCount": 5 }
            })
            .to_string(),
        ]
        .join("\n");

        let openclaw_v4 = [
            serde_json::json!({
                "type": "session",
                "version": 4,
                "id": "oc-sess-1",
                "timestamp": "2026-08-01T00:00:00Z",
                "cwd": "/repo/oc"
            })
            .to_string(),
            serde_json::json!({
                "type": "message",
                "id": "m1",
                "parentId": null,
                "timestamp": "2026-08-01T00:00:01Z",
                "message": { "role": "user", "content": [{ "type": "text", "text": "hello openclaw" }] }
            })
            .to_string(),
        ]
        .join("\n");

        let continue_doc = serde_json::json!({
            "sessionId": "cont-1",
            "title": "Continue Test",
            "workspaceDirectory": "file:///Users/alice/my%20repo",
            "history": [
                { "message": { "role": "user", "content": "hi continue" }, "contextItems": [] }
            ]
        })
        .to_string();

        let opensession_doc = {
            let mut s = Session::new("os-1", Harness::OpenSession);
            let mut m = crate::ir::Message::new(crate::ir::Role::User);
            m.content.push(crate::ir::Block::Text {
                text: "hello opensession".into(),
            });
            s.messages.push(m);
            serde_json::to_string(&crate::harness::opensession::document(&s)).unwrap()
        };

        let claude_export = serde_json::json!([
            {
                "uuid": "cexp-1",
                "name": "Exported Claude",
                "chat_messages": [{ "sender": "human", "text": "from export" }]
            }
        ])
        .to_string();

        let report = ingest_files_with_report(vec![
            ("chats/qwen-sess-1.jsonl".into(), qwen_025.into_bytes()),
            ("sessions/oc-sess-1.jsonl".into(), openclaw_v4.into_bytes()),
            ("sessions/cont-1.json".into(), continue_doc.into_bytes()),
            ("backup.opensession.json".into(), opensession_doc.into_bytes()),
            ("conversations.json".into(), claude_export.into_bytes()),
            ("README.txt".into(), b"not a transcript".to_vec()),
        ])
        .unwrap();

        assert_eq!(report.sessions.len(), 5);
        assert!(report
            .sessions
            .iter()
            .any(|s| s.harness == Harness::Qwen && s.id == "qwen-sess-1"));
        assert!(report
            .sessions
            .iter()
            .any(|s| s.harness == Harness::OpenClaw && s.id == "oc-sess-1"));
        assert!(report
            .sessions
            .iter()
            .any(|s| s.harness == Harness::Continue && s.id == "cont-1"));
        assert!(report
            .sessions
            .iter()
            .any(|s| s.harness == Harness::OpenSession && s.id == "os-1"));
        assert!(report
            .sessions
            .iter()
            .any(|s| s.harness == Harness::ClaudeExport && s.id == "cexp-1"));

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path, "README.txt");
    }

    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    #[test]
    fn ingests_sqlite_stores_via_isolated_temp_staging() {
        let world = crate::test_world("ingest-sqlite-hermes");
        let mut s = Session::new("hermes-zip-1", Harness::Hermes);
        let mut m = crate::ir::Message::new(crate::ir::Role::User);
        m.content.push(crate::ir::Block::Text {
            text: "zipped sqlite prompt".into(),
        });
        s.messages.push(m);
        let emitted = crate::harness::hermes::emit(
            &s,
            &world.base,
            &crate::emit::EmitOptions {
                new_id: Some("hermes-zip-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let db_bytes = std::fs::read(&emitted.path).unwrap();

        let report = ingest_files_with_report(vec![
            ("hermes/state.db".into(), db_bytes),
            ("hermes/notes.txt".into(), b"ignored note".to_vec()),
        ])
        .unwrap();
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].harness, Harness::Hermes);
        assert_eq!(report.sessions[0].id, "hermes-zip-1");
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path, "hermes/notes.txt");
    }
}
