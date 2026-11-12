//! cv-web — stateful WASM core and archive bindings for clustervision.
//!
//! Two entry surfaces share the same in-memory archive extraction and IR pipeline:
//! - [`ingest_zip`] keeps the original stateless `Uint8Array -> JSON Session[]` contract intact.
//! - [`WasmStore`] retains parsed [`Session`]s inside WASM linear memory, returning lightweight
//!   `§3` session-row stubs plus explicit skipped-file diagnostics on upload instead of serializing
//!   multi-100MB message arrays into one JS string, and serves on-demand session detail, message
//!   windows, deterministic [`cv_core::distill`], and secret [`cv_core::redact`] directly to JS.

use cv_core::distill::{distill_pack, DistillOptions};
use cv_core::ir::{Harness, Session, SessionRef};
use cv_core::redact::{redact_with, RedactClasses, RedactOptions, RedactStats};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::Cursor;
use std::path::PathBuf;
use wasm_bindgen::prelude::*;

fn js_err(prefix: &str, err: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&format!("{prefix}: {err}"))
}

fn to_json_string<T: Serialize>(value: &T) -> Result<String, JsValue> {
    serde_json::to_string(value).map_err(|e| js_err("json", e))
}

/// Stateless archive ingestion: extract + sniff + parse all recognized transcripts in `bytes` and
/// return the full `Session[]` array as a JSON string. Preserved for callers using the original API.
#[wasm_bindgen]
pub fn ingest_zip(bytes: &[u8]) -> Result<String, JsValue> {
    let sessions = extract_and_parse_legacy(bytes).map_err(|e| JsValue::from_str(&e))?;
    to_json_string(&sessions)
}

fn extract_and_parse_legacy(bytes: &[u8]) -> Result<Vec<Session>, String> {
    let files =
        cv_core::ingest::archive::extract(Cursor::new(bytes), Default::default()).map_err(|e| format!("zip: {e:#}"))?;
    let mut opensessions = Vec::new();
    let mut rest = Vec::with_capacity(files.len());
    for (name, raw) in files {
        if let Ok(text) = std::str::from_utf8(&raw) {
            if looks_like_opensession(&name, text) {
                if let Ok(mut s) = cv_core::harness::opensession::from_str(text) {
                    if s.source_path.is_none() {
                        s.source_path = Some(PathBuf::from(&name));
                    }
                    opensessions.push(s);
                    continue;
                }
            }
        }
        rest.push((name, raw));
    }
    let mut out = cv_core::ingest::ingest_files(rest).map_err(|e| format!("transcript: {e:#}"))?;
    out.extend(opensessions);
    Ok(out)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
struct StoredSession {
    session: Session,
    size_bytes: Option<u64>,
}

fn session_ref_of(entry: &StoredSession) -> SessionRef {
    let s = &entry.session;
    let path = s
        .source_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}:{}", s.harness.as_str(), s.id)));
    let title = Some(s.label());
    SessionRef {
        id: s.id.clone(),
        harness: s.harness,
        path,
        cwd: s.cwd.clone(),
        title,
        created_at: s.created_at,
        updated_at: s.updated_at.or(s.created_at),
        message_count: Some(s.messages.len()),
    }
}

fn summary_row(entry: &StoredSession) -> Value {
    let r = session_ref_of(entry);
    let mut row = cv_core::rows::session_row(&r, entry.size_bytes);
    if let Some(obj) = row.as_object_mut() {
        if let Some(model) = &entry.session.model {
            obj.insert("model".into(), json!(model));
        }
    }
    row
}

fn session_meta_json(s: &Session) -> Value {
    let mut meta = json!({
        "title": s.title.clone().unwrap_or_else(|| s.label()),
        "model": s.model,
        "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
        "system_prompt": s.system_prompt,
        "lineage": s.lineage,
        "git": s.git,
    });
    if let Some(principal) = &s.principal {
        meta["principal"] = json!(principal);
    }
    meta
}

fn redact_stats_json(stats: &RedactStats) -> Value {
    json!({
        "total": stats.total(),
        "api_keys": stats.api_keys,
        "private_keys": stats.private_keys,
        "jwts": stats.jwts,
        "emails": stats.emails,
        "blobs": stats.blobs,
        "assignments": stats.assignments,
    })
}

fn looks_like_opensession(name: &str, text: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".opensession.json") {
        return true;
    }
    if !lower.ends_with(".json") {
        return false;
    }
    let trimmed = text.trim_start();
    trimmed.starts_with('{')
        && trimmed
            .get(..256.min(trimmed.len()))
            .is_some_and(|head| head.contains("\"open_session\"") || head.contains("\"openSession\""))
}

fn parse_distill_options(opts_json: Option<&str>) -> Result<DistillOptions, String> {
    let mut opts = DistillOptions::default();
    let Some(raw) = opts_json.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(opts);
    };
    let val: Value = serde_json::from_str(raw).map_err(|e| format!("invalid distill options JSON: {e}"))?;
    let Some(obj) = val.as_object() else {
        return Err("distill options must be a JSON object".into());
    };
    if let Some(n) = obj.get("keep_last").and_then(Value::as_u64) {
        opts.keep_last = n as usize;
    }
    if let Some(n) = obj.get("tail_result_max").and_then(Value::as_u64) {
        opts.tail_result_max = n as usize;
    }
    if let Some(n) = obj.get("note_max").and_then(Value::as_u64) {
        opts.note_max = n as usize;
    }
    if let Some(b) = obj.get("keep_tail_thinking").and_then(Value::as_bool) {
        opts.keep_tail_thinking = b;
    }
    if let Some(b) = obj.get("ledger").and_then(Value::as_bool) {
        opts.ledger = b;
    }
    if let Some(r) = obj.get("retrieve_ref").and_then(Value::as_str) {
        opts.retrieve_ref = Some(r.to_string());
    }
    Ok(opts)
}

fn parse_redact_options(only: Option<&str>) -> Result<RedactOptions, String> {
    match only.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(RedactOptions::default()),
        Some(spec) => Ok(RedactOptions {
            classes: RedactClasses::parse_only(spec)?,
        }),
    }
}

/// Stateful in-browser store holding ingested [`Session`]s in WASM linear memory so large `.zip`
/// archives do not serialize full transcripts across the JS boundary until a view opens one.
#[wasm_bindgen]
#[derive(Default)]
pub struct WasmStore {
    entries: Vec<StoredSession>,
}

impl WasmStore {
    fn upsert(&mut self, mut session: Session, path: Option<&str>, size_bytes: Option<u64>) -> Value {
        if session.source_path.is_none() {
            if let Some(p) = path {
                session.source_path = Some(PathBuf::from(p));
            }
        }
        let stored = StoredSession { session, size_bytes };
        let row = summary_row(&stored);
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.session.harness == stored.session.harness && e.session.id == stored.session.id)
        {
            *existing = stored;
        } else {
            self.entries.push(stored);
        }
        row
    }

    fn find_entry(&self, harness: &str, id: &str) -> Result<&StoredSession, String> {
        let want = Harness::parse(harness).ok_or_else(|| format!("unknown harness {harness:?}"))?;
        self.entries
            .iter()
            .find(|e| e.session.harness == want && e.session.id == id)
            .ok_or_else(|| format!("session not found: {}:{id}", want.as_str()))
    }

    fn find_entry_mut(&mut self, harness: &str, id: &str) -> Result<&mut StoredSession, String> {
        let want = Harness::parse(harness).ok_or_else(|| format!("unknown harness {harness:?}"))?;
        self.entries
            .iter_mut()
            .find(|e| e.session.harness == want && e.session.id == id)
            .ok_or_else(|| format!("session not found: {}:{id}", want.as_str()))
    }

    fn ingest_zip_inner(&mut self, bytes: &[u8]) -> Result<Value, String> {
        let files = cv_core::ingest::archive::extract(Cursor::new(bytes), Default::default())
            .map_err(|e| format!("zip: {e:#}"))?;
        let mut summaries = Vec::new();
        let mut skipped: Vec<SkippedEntry> = Vec::new();
        let mut deferred_opensessions: Vec<(String, u64, Session)> = Vec::new();
        let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
        let mut pending_sizes: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

        for (name, raw) in files {
            let size = raw.len() as u64;
            if raw.iter().all(u8::is_ascii_whitespace) {
                skipped.push(SkippedEntry {
                    path: name,
                    reason: "empty file".into(),
                });
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&raw) {
                if looks_like_opensession(&name, text) {
                    match cv_core::harness::opensession::from_str(text) {
                        Ok(session) => deferred_opensessions.push((name, size, session)),
                        Err(err) => skipped.push(SkippedEntry {
                            path: name,
                            reason: format!("{err:#}"),
                        }),
                    }
                    continue;
                }
            }
            pending_sizes.insert(name.clone(), size);
            pending.push((name, raw));
        }

        if !pending.is_empty() {
            let report = cv_core::ingest::ingest_files_with_report(pending).map_err(|e| format!("ingest: {e:#}"))?;
            for session in report.sessions {
                let src_name = session.source_path.as_ref().map(|p| p.to_string_lossy().into_owned());
                let size = src_name.as_deref().and_then(|n| pending_sizes.get(n).copied());
                summaries.push(self.upsert(session, src_name.as_deref(), size));
            }
            for s in report.skipped {
                skipped.push(SkippedEntry {
                    path: s.path,
                    reason: s.reason,
                });
            }
        }

        for (name, size, session) in deferred_opensessions {
            summaries.push(self.upsert(session, Some(&name), Some(size)));
        }

        Ok(json!({
            "sessions": summaries,
            "skipped": skipped,
        }))
    }

    fn ingest_json_inner(&mut self, name: &str, text: &str) -> Result<Value, String> {
        let size = text.len() as u64;
        if text.trim().is_empty() {
            return Err("empty file".into());
        }
        let mut summaries = Vec::new();
        if looks_like_opensession(name, text) {
            let session = cv_core::harness::opensession::from_str(text).map_err(|e| format!("{e:#}"))?;
            summaries.push(self.upsert(session, Some(name), Some(size)));
            return Ok(json!({ "sessions": summaries, "skipped": [] }));
        }
        let parsed = cv_core::ingest::ingest_files(vec![(name.to_string(), text.as_bytes().to_vec())])
            .map_err(|e| format!("{e:#}"))?;
        if parsed.is_empty() {
            return Err("unrecognized transcript format".into());
        }
        for session in parsed {
            summaries.push(self.upsert(session, Some(name), Some(size)));
        }
        Ok(json!({ "sessions": summaries, "skipped": [] }))
    }

    fn session_head_inner(&self, harness: &str, id: &str) -> Result<Value, String> {
        let entry = self.find_entry(harness, id)?;
        let s = &entry.session;
        let mut row = summary_row(entry);
        let obj = row.as_object_mut().expect("summary_row is an object");
        obj.insert("model".into(), json!(s.model));
        obj.insert("git".into(), json!(s.git));
        obj.insert("system_prompt".into(), json!(s.system_prompt));
        obj.insert("lineage".into(), json!(s.lineage));
        obj.insert("extra".into(), json!(s.extra));
        obj.insert("total".into(), json!(s.messages.len()));
        if let Some(principal) = &s.principal {
            obj.insert("principal".into(), json!(principal));
        }
        Ok(row)
    }

    fn messages_inner(&self, harness: &str, id: &str, start: usize, end: Option<usize>) -> Result<Value, String> {
        if let Some(e) = end {
            if e < start {
                return Err("end must be >= start".into());
            }
        }
        let entry = self.find_entry(harness, id)?;
        let s = &entry.session;
        let total = s.messages.len();
        let clamped_start = start.min(total);
        let clamped_end = end.unwrap_or(total).min(total);
        let slice = &s.messages[clamped_start..clamped_end];
        Ok(json!({
            "harness": s.harness.as_str(),
            "id": s.id,
            "start": clamped_start,
            "end": clamped_end,
            "has_more": clamped_end < total,
            "total_known": true,
            "total": total,
            "message_count": total,
            "session": session_meta_json(s),
            "messages": slice,
        }))
    }

    fn distill_inner(&self, harness: &str, id: &str, opts_json: Option<&str>) -> Result<Value, String> {
        let entry = self.find_entry(harness, id)?;
        let opts = parse_distill_options(opts_json)?;
        let distilled = distill_pack(&entry.session, &opts).map_err(|e| format!("distill: {e:#}"))?;
        Ok(json!({
            "harness": entry.session.harness.as_str(),
            "id": entry.session.id,
            "label": distilled.label(),
            "markdown": distilled.pack(),
            "head_md": distilled.head_md,
            "tail_md": distilled.tail_md,
            "tail_start": distilled.tail_start,
            "stats": distilled.stats,
        }))
    }

    fn redact_inner(&self, harness: &str, id: &str, only: Option<&str>) -> Result<Value, String> {
        let entry = self.find_entry(harness, id)?;
        let opts = parse_redact_options(only)?;
        let (scrubbed, stats) = redact_with(&entry.session, &opts).map_err(|e| format!("redact: {e:#}"))?;
        Ok(json!({
            "session": scrubbed,
            "stats": redact_stats_json(&stats),
        }))
    }

    fn redact_in_place_inner(&mut self, harness: &str, id: &str, only: Option<&str>) -> Result<Value, String> {
        let opts = parse_redact_options(only)?;
        let entry = self.find_entry_mut(harness, id)?;
        let (scrubbed, stats) = redact_with(&entry.session, &opts).map_err(|e| format!("redact: {e:#}"))?;
        entry.session = scrubbed;
        Ok(json!({
            "session": summary_row(entry),
            "stats": redact_stats_json(&stats),
        }))
    }
}

#[wasm_bindgen]
impl WasmStore {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmStore {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Ingest a `.zip` archive into WASM memory and return `{ sessions: [...stubs...], skipped: [...] }`
    /// without serializing full message bodies across the boundary.
    pub fn ingest_zip(&mut self, bytes: &[u8]) -> Result<String, JsValue> {
        let out = self.ingest_zip_inner(bytes).map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&out)
    }

    /// Ingest one UTF-8 `.json` / `.jsonl` transcript or OpenSession document into WASM memory.
    pub fn ingest_json(&mut self, name: &str, text: &str) -> Result<String, JsValue> {
        let out = self.ingest_json_inner(name, text).map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&out)
    }

    /// Return all stored sessions as `§3` session-row stubs (`SessionRef` JSON, newest first).
    pub fn sessions(&self) -> Result<String, JsValue> {
        let mut indices: Vec<usize> = (0..self.entries.len()).collect();
        indices.sort_by(|&a, &b| {
            let ka = self.entries[a]
                .session
                .updated_at
                .or(self.entries[a].session.created_at);
            let kb = self.entries[b]
                .session
                .updated_at
                .or(self.entries[b].session.created_at);
            kb.cmp(&ka)
        });
        let rows: Vec<Value> = indices.into_iter().map(|i| summary_row(&self.entries[i])).collect();
        to_json_string(&rows)
    }

    /// Return the full IR [`Session`] JSON for `(harness, id)`.
    pub fn session(&self, harness: &str, id: &str) -> Result<String, JsValue> {
        let entry = self.find_entry(harness, id).map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&entry.session)
    }

    /// Return the `/api/session/{harness}/{id}/head` metadata envelope without messages.
    pub fn session_head(&self, harness: &str, id: &str) -> Result<String, JsValue> {
        let head = self
            .session_head_inner(harness, id)
            .map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&head)
    }

    /// Return `[start, end)` messages in the exact `/api/session/{harness}/{id}/messages` envelope.
    pub fn messages(&self, harness: &str, id: &str, start: usize, end: Option<usize>) -> Result<String, JsValue> {
        let win = self
            .messages_inner(harness, id, start, end)
            .map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&win)
    }

    /// Run deterministic [`cv_core::distill`] on `(harness, id)` in-browser.
    pub fn distill(&self, harness: &str, id: &str, opts_json: Option<String>) -> Result<String, JsValue> {
        let res = self
            .distill_inner(harness, id, opts_json.as_deref())
            .map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&res)
    }

    /// Return a secret-redacted copy of `(harness, id)` plus per-class counts without mutating the store.
    pub fn redact(&self, harness: &str, id: &str, only: Option<String>) -> Result<String, JsValue> {
        let res = self
            .redact_inner(harness, id, only.as_deref())
            .map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&res)
    }

    /// Scrub secrets from `(harness, id)` in place inside WASM memory and return `{ session, stats }`.
    pub fn redact_in_place(&mut self, harness: &str, id: &str, only: Option<String>) -> Result<String, JsValue> {
        let res = self
            .redact_in_place_inner(harness, id, only.as_deref())
            .map_err(|e| JsValue::from_str(&e))?;
        to_json_string(&res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CLAUDE_SAMPLE: &str = concat!(
        r#"{"type":"user","sessionId":"abc","parentUuid":null,"uuid":"u1","cwd":"/work/demo","timestamp":"2026-02-01T00:00:00Z","message":{"role":"user","content":"inspect auth token sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF and reach alice@example.com"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"abc","parentUuid":"u1","uuid":"u2","timestamp":"2026-02-01T00:00:02Z","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"checked the config"},{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"cargo test --workspace"}}]}}"#,
        "\n",
        r#"{"type":"user","sessionId":"abc","parentUuid":"u2","uuid":"u3","timestamp":"2026-02-01T00:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"test result: ok. 12 passed; 0 failed"}]}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"abc","parentUuid":"u3","uuid":"u4","timestamp":"2026-02-01T00:00:04Z","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"all tests pass cleanly"}]}}"#,
    );

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, data) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }

    #[test]
    fn legacy_ingest_zip_still_returns_full_session_array() {
        let archive = make_zip(&[("abc.jsonl", CLAUDE_SAMPLE.as_bytes())]);
        let sessions = extract_and_parse_legacy(&archive).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].harness, Harness::Claude);
        assert_eq!(sessions[0].messages.len(), 4);
    }

    #[test]
    fn wasm_store_ingests_summaries_reports_skipped_and_serves_windows_distill_and_redact() {
        let mut opensess = Session::new("os-1", Harness::OpenSession);
        opensess.title = Some("OpenSession fixture".into());
        let mut om = cv_core::ir::Message::new(cv_core::ir::Role::User);
        om.content.push(cv_core::ir::Block::Text {
            text: "portable note".into(),
        });
        opensess.messages.push(om);
        let os_bytes = serde_json::to_vec(&cv_core::harness::opensession::document(&opensess)).unwrap();

        let archive = make_zip(&[
            ("abc.jsonl", CLAUDE_SAMPLE.as_bytes()),
            ("os-1.opensession.json", &os_bytes),
            ("notes/readme.txt", b"not a transcript"),
            ("empty.jsonl", b"   \n"),
        ]);

        let mut store = WasmStore::new();
        let report = store.ingest_zip_inner(&archive).unwrap();
        assert_eq!(store.len(), 2);

        let stubs = report["sessions"].as_array().unwrap();
        assert_eq!(stubs.len(), 2);
        // Stubs carry metadata + message_count, never raw messages[].
        assert!(stubs.iter().all(|row| row.get("messages").is_none()));
        assert_eq!(stubs[0]["id"], "abc");
        assert_eq!(stubs[0]["harness"], "claude");
        assert_eq!(stubs[0]["message_count"], 4);

        let skipped = report["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped.iter().any(|s| s["path"] == "notes/readme.txt"));
        assert!(skipped
            .iter()
            .any(|s| s["path"] == "empty.jsonl" && s["reason"] == "empty file"));

        // Windowed read matches /api/session/{h}/{id}/messages shape.
        let win1 = store.messages_inner("claude", "abc", 0, Some(2)).unwrap();
        assert_eq!(win1["start"], 0);
        assert_eq!(win1["end"], 2);
        assert_eq!(win1["has_more"], true);
        assert_eq!(win1["total"], 4);
        assert_eq!(win1["messages"].as_array().unwrap().len(), 2);

        let win2 = store.messages_inner("claude", "abc", 2, Some(10)).unwrap();
        assert_eq!(win2["start"], 2);
        assert_eq!(win2["end"], 4);
        assert_eq!(win2["has_more"], false);

        // Head metadata envelope.
        let head = store.session_head_inner("claude", "abc").unwrap();
        assert_eq!(head["total"], 4);
        assert_eq!(head["model"], "claude-sonnet-4-5");

        // Deterministic distill directly from WASM memory.
        let distilled = store
            .distill_inner("claude", "abc", Some(r#"{"keep_last":1,"ledger":true}"#))
            .unwrap();
        let md = distilled["markdown"].as_str().unwrap();
        assert!(
            md.contains("all tests pass cleanly") || md.contains("cargo test"),
            "{md}"
        );
        assert_eq!(distilled["stats"]["messages"], 4);

        // Non-mutating redact + mutating redact_in_place.
        let redacted = store.redact_inner("claude", "abc", None).unwrap();
        assert!(redacted["stats"]["total"].as_u64().unwrap() >= 2, "{redacted}");
        let scrubbed_prompt = redacted["session"]["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(!scrubbed_prompt.contains("alice@example.com"), "{scrubbed_prompt}");
        assert!(!scrubbed_prompt.contains("sk-ant-api03-"), "{scrubbed_prompt}");

        // Original in store is untouched until redact_in_place is called.
        let raw_before = store.messages_inner("claude", "abc", 0, Some(1)).unwrap();
        assert!(raw_before["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("alice@example.com"));

        store.redact_in_place_inner("claude", "abc", Some("email")).unwrap();
        let raw_after = store.messages_inner("claude", "abc", 0, Some(1)).unwrap();
        let after_text = raw_after["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(!after_text.contains("alice@example.com"), "{after_text}");
    }

    /// Wrap `payload` in an RFC 8878 single-segment uncompressed Zstandard frame (`0xFD2FB528`
    /// magic + 4-byte FCS + 3-byte `Raw_Block` header). Contains both non-UTF-8 magic bytes and
    /// `0x00` FCS bytes so a naive NUL-byte binary filter would falsely reject it.
    fn wrap_raw_zstd_frame(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u32;
        assert!(len < (1 << 21));
        let mut out = Vec::with_capacity(11 + payload.len());
        out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        out.push(0xA0); // Single_Segment_Flag = 1, FCS_Field_Size = 2 (4 bytes)
        out.extend_from_slice(&len.to_le_bytes());
        let block_hdr = (len << 3) | 0x01; // Last_Block = 1, Block_Type = 0 (Raw_Block)
        out.extend_from_slice(&block_hdr.to_le_bytes()[..3]);
        out.extend_from_slice(payload);
        assert!(out.contains(&0) && std::str::from_utf8(&out).is_err());
        out
    }

    #[test]
    fn wasm_store_batches_multi_file_cline_dirs_and_decodes_zst_transcripts() {
        let zst_claude = wrap_raw_zstd_frame(CLAUDE_SAMPLE.as_bytes());
        let cline_hist =
            br#"[{"role":"user","content":"fix batch zip"},{"role":"assistant","content":"batched cleanly"}]"#;
        let cline_meta = br#"{"files_in_context":[]}"#;

        let archive = make_zip(&[
            ("compressed-claude.jsonl.zst", &zst_claude),
            ("tasks/cline-42/api_conversation_history.json", cline_hist),
            ("tasks/cline-42/task_metadata.json", cline_meta),
        ]);

        let mut store = WasmStore::new();
        let report = store.ingest_zip_inner(&archive).unwrap();
        let stubs = report["sessions"].as_array().unwrap();
        let skipped = report["skipped"].as_array().unwrap();
        assert!(
            skipped.is_empty(),
            "companion metadata and .zst must not skip: {skipped:?}"
        );
        assert_eq!(stubs.len(), 2, "{stubs:?}");
        assert!(stubs
            .iter()
            .any(|s| s["harness"] == "claude" && s["id"] == "compressed-claude"));
        assert!(stubs.iter().any(|s| s["harness"] == "cline" && s["id"] == "cline-42"));
    }
}
