//! The Cline SDK session store: `~/.cline/data/sessions/`, written by the Cline CLI 3.x (npm
//! `cline`) and by the 4.x VS Code extension, which are both built on `@cline/core`. The 3.x
//! extension's per-task `tasks/<id>/api_conversation_history.json` layout is the older format,
//! read by [`super::cline`]; this module is the newer one, and [`super::cline::Cline`] reads both.
//!
//! # Layout (verified against cline 3.0.70, `tests/corpus/cline/3.0.70`)
//!
//! ```text
//! <data>/sessions/<sid>/                 <sid> = "<epoch ms>_<5 chars>", e.g. 1791750113981_lflrg
//!   <sid>.json                            the session row: {version, session_id, source, pid,
//!                                         started_at, ended_at, exit_code, status, interactive,
//!                                         provider, model, cwd, workspace_root, team_name,
//!                                         enable_tools, enable_spawn, enable_teams, prompt,
//!                                         metadata: {title, usage, aggregateUsage, totalCost, ...},
//!                                         messages_path}
//!   <sid>.messages.json                   {version, updated_at, agent: "lead", sessionId, origin,
//!                                         messages: [...], system_prompt}
//!   agent_<uuid>.messages.json            a spawn_agent child: agent "subagent", taskType
//!                                         "subagent_task", origin {mode: "subagent",
//!                                         parentThreadId: <sid>, subagent: "agent_<uuid>"}
//!   <teammate>__<id>.messages.json        a team teammate: agent "teammate", taskType "team",
//!                                         origin {mode: "team", parentThreadId, subagent: <name>}
//! <data>/db/sessions.db                   the same session rows (plus subagent rows, schedules,
//!                                         schedule_executions, subagent_spawn_queue); only
//!                                         `schedule_executions` is read ([`store_affordances`])
//! <data>/db/cron.db                       cron_specs + cron_runs (the hub's automation queue);
//!                                         `cron_runs` is read
//! <data>/db/teams.db                      team state (events, tasks, runs, outcomes), keyed by the
//!                                         lead session id; `team_events` is read
//! ```
//!
//! A schedule names no session that made it (`created_by` is the hub client); a session it ran
//! as is named by `schedule_executions.session_id` / `cron_runs.session_id`, so those are the
//! session's `schedule`/`fire` rows. A team's `team_events` (`teammate_spawned`, `run_queued`,
//! `run_started`, `run_completed`, `run_failed`, `run_cancelled`) are the lead's teammates: one
//! `subagent_spawn`/`state` row per run (teammate, task, last status, result), and one per
//! teammate that never ran.
//!
//! Only the child files carry no `<id>.json`; their session id here is `<sid>/<file stem>` (for
//! example `1791750138208_50wuo/agent_6522b1bb-…`), parented to `<sid>`, with the file's own
//! `sessionId` (the SDK's `<sid>__agent_<uuid>` / `<sid>__teamtask__<name>__<id>`) kept in
//! `extra["cline"]["sdk_session_id"]`.
//!
//! # `messages[]`
//!
//! `{id, role: "user"|"assistant", content: [block] | string, ts: epoch ms}` plus, on assistant
//! turns, `modelInfo {id, provider}` and `metrics {inputTokens, outputTokens, cacheReadTokens,
//! cacheWriteTokens}`. Blocks are Anthropic-shaped (`text`, `thinking`, `tool_use`, `tool_result`,
//! `image`), except that a `tool_result` names its tool (`name`) and its `content` may be an array
//! of plain result objects (`read_files`: `{query, result, success}`), kept as JSON text. A user
//! turn holding only tool results is a [`Role::Tool`] turn. An assistant turn with
//! `metadata {displayOnly: true, displayRole: "error"}` is a run error shown to the user, never
//! sent to the model ([`MessageKind::Error`]).

use super::store::document::text_summary;
use super::store::document::Fields;
use super::wire::{self, anthropic};
use super::{parse_ts, ts_from_epoch};
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
#[cfg(feature = "sqlite")]
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The SDK `sessions/` dirs under `home`, honouring the SDK's own overrides (`CLINE_SESSION_DATA_DIR`,
/// `CLINE_DATA_DIR`, `CLINE_DIR`) only when `env` is set. Only existing dirs are returned.
pub fn session_roots(home: &Path, env: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if env {
        if let Some(d) = crate::env::var_os("CLINE_SESSION_DATA_DIR") {
            out.push(PathBuf::from(d));
        }
        if let Some(d) = crate::env::var_os("CLINE_DATA_DIR") {
            out.push(PathBuf::from(d).join("sessions"));
        }
        if let Some(d) = crate::env::var_os("CLINE_DIR") {
            out.push(PathBuf::from(d).join("data").join("sessions"));
        }
    }
    out.push(home.join(".cline").join("data").join("sessions"));
    let mut seen = Vec::new();
    out.retain(|p| {
        p.is_dir() && !seen.contains(p) && {
            seen.push(p.clone());
            true
        }
    });
    out
}

/// True when `path` is an SDK messages file (`*.messages.json`), as opposed to a legacy task dir.
pub fn is_sdk_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".messages.json"))
}

/// One `SessionRef` per messages file under each root: the lead session first, then its children.
/// Every SDK messages file under `roots`, into `out`; an unreadable dir or file is skipped and
/// reported.
pub fn discover(roots: &[PathBuf], out: &mut crate::Discovered) {
    let entries_of = |dir: &Path, out: &mut crate::Discovered| -> Vec<PathBuf> {
        let Some(entries) = out.keep(dir, fs::read_dir(dir).map_err(Into::into)) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| out.keep(dir, e.map_err(Into::into)).map(|e| e.path()))
            .collect();
        paths.sort();
        paths
    };
    for root in roots {
        let dirs: Vec<PathBuf> = entries_of(root, out).into_iter().filter(|p| p.is_dir()).collect();
        for dir in dirs {
            let Some(sid) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            let files: Vec<PathBuf> = entries_of(&dir, out).into_iter().filter(|p| is_sdk_path(p)).collect();
            let lead = dir.join(format!("{sid}.messages.json"));
            for f in std::iter::once(&lead).chain(files.iter().filter(|f| **f != lead)) {
                if let Some(found) = crate::Skipped::listed(f, scan(f, &sid).map(Some)) {
                    out.push(found);
                }
            }
        }
    }
}

/// A child file's session id: `<sid>/<stem>`; the lead's is `<sid>`.
fn ref_id(path: &Path, sid: &str) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".messages.json")?;
    Some(if stem == sid {
        sid.to_string()
    } else {
        format!("{sid}/{stem}")
    })
}

/// The `<sid>.json` session row next to a messages file, if any.
fn manifest(dir: &Path, sid: &str) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(dir.join(format!("{sid}.json"))).ok()?).ok()
}

use super::file_mtime as mtime;

/// Whether a `messages[]` entry is a turn ([`Message::is_turn`]) as [`message_with_blocks`] kinds
/// it: an assistant reply that is not a display-only error, or a user entry that does not hold
/// only tool results. Reads `role`, `metadata` and block types, never bodies.
fn is_turn(entry: &Fields<'_>) -> Result<bool> {
    Ok(match entry.string("role")?.as_deref() {
        Some("assistant") => {
            let meta = entry
                .get("metadata")
                .map(|m| Fields::parse(m, &["displayOnly", "displayRole"]))
                .transpose()?
                .flatten();
            let display_only = meta
                .as_ref()
                .and_then(|m| m.get("displayOnly"))
                .map(|v| v.as_bool())
                .transpose()?
                .flatten();
            let display_role = meta.as_ref().map(|m| m.string("displayRole")).transpose()?.flatten();
            !(display_only == Some(true) && display_role.as_deref() == Some("error"))
        }
        Some("user") => !anthropic::only_tool_results(entry.get("content"), |part| {
            anthropic::block_kind(part, &anthropic::ANTHROPIC)
        })?,
        _ => false,
    })
}

fn scan(path: &Path, sid: &str) -> Result<SessionRef> {
    JsonSource::capture(path)?.read(ReadInput::Document, Validation::Value, |view| {
        let record = Fields::parse(view, &["messages", "updated_at"])?.context("SDK document object")?;
        let messages = record.get("messages").context("SDK messages")?;
        let id = ref_id(path, sid).context("SDK id")?;
        let lead = id == sid;
        let row = if lead {
            path.parent().and_then(|dir| manifest(dir, sid))
        } else {
            None
        };
        let mut r = SessionRef::new(&id, Harness::Cline, path.to_path_buf());
        r.cwd = row
            .as_ref()
            .and_then(|m| m.get("cwd"))
            .and_then(Value::as_str)
            .map(PathBuf::from);
        r.title = row
            .as_ref()
            .and_then(|m| m.pointer("/metadata/title"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        r.created_at = row
            .as_ref()
            .and_then(|m| m.get("started_at"))
            .and_then(Value::as_str)
            .and_then(parse_ts);
        let mut first = true;
        let mut first_user = false;
        let mut count = 0;
        for message in messages.array()?.context("SDK messages array")? {
            let message = message?;
            // A message entry, not the document-level `record` above.
            if let Some(entry) = Fields::parse(message, &["ts", "role", "content", "metadata"])? {
                count += usize::from(is_turn(&entry)?);
                if first && r.created_at.is_none() {
                    r.created_at = entry.value("ts")?.and_then(|v| v.as_i64()).and_then(ts_from_epoch);
                }
                if !first_user && entry.string("role")?.as_deref() == Some("user") {
                    first_user = true;
                    if r.title.is_none() {
                        r.title = super::wire::text_title(entry.get("content"))?;
                    }
                }
            }
            first = false;
        }
        r.message_count = Some(count);
        r.title = r.title.map(|t| truncate(&t, crate::projection::Head::TITLE));
        r.updated_at = record
            .string("updated_at")?
            .as_deref()
            .and_then(parse_ts)
            .or_else(|| mtime(path))
            .or(r.created_at);
        Ok(r)
    })
}
/// Read one messages file into `sink`, returning the session with empty `messages`.
pub fn stream(path: &Path, id: &str, sink: &mut dyn MessageSink) -> Result<Session> {
    stream_with_options(path, id, &Demand::RICH, sink)
}

pub(crate) fn stream_with_options(path: &Path, id: &str, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    let source = JsonSource::capture(path)?;
    source.read(ReadInput::Document, Validation::Value, |view| {
        let keys = [
            "agent",
            "taskType",
            "sessionId",
            "origin",
            "updated_at",
            "system_prompt",
            "messages",
        ];
        let record = Fields::parse(view, &keys)?;
        // `messages` streams below; every other parsed key projects into the document value.
        let [doc_keys @ .., messages_key] = keys;
        let doc = Value::Object(
            record
                .as_ref()
                .map(|r| r.project(&doc_keys))
                .transpose()?
                .unwrap_or_default(),
        );
        let messages = record.as_ref().and_then(|r| r.get(messages_key));
        let mut s = Session::new(id.to_string(), Harness::Cline);
        s.source_path = Some(path.to_path_buf());
        let sid = id.split('/').next().unwrap_or(id);
        let lead = sid == id;
        if let Some(dir) = path.parent() {
            sink.dependency(&dir.join(format!("{sid}.json")));
        }
        if let Some(error) = sink.take_error() {
            return Err(error);
        }
        // Child sessions deliberately do not inherit the lead's row metadata. Keep the
        // dependency guard, but avoid decoding the same growing manifest for every child.
        let row = if lead {
            path.parent().and_then(|d| manifest(d, sid))
        } else {
            None
        };
        let origin = doc.get("origin");
        {
            let bag = s.harness_extra_mut(Harness::Cline);
            bag.insert("store".into(), json!("sdk"));
            for (k, key) in [
                ("agent", "agent"),
                ("task_type", "taskType"),
                ("sdk_session_id", "sessionId"),
            ] {
                if let Some(v) = doc.get(key) {
                    bag.insert(k.into(), v.clone());
                }
            }
            if let Some(o) = origin {
                bag.insert("origin".into(), o.clone());
            }
            if let Some(row) = row.as_ref() {
                for k in [
                    "source",
                    "status",
                    "exit_code",
                    "provider",
                    "team_name",
                    "workspace_root",
                    "interactive",
                ] {
                    if let Some(v) = row.get(k).filter(|v| !v.is_null()) {
                        bag.insert(k.into(), v.clone());
                    }
                }
            }
        }
        if !lead {
            s.lineage.parent = Some(
                origin
                    .and_then(|o| o.get("parentThreadId"))
                    .and_then(Value::as_str)
                    .unwrap_or(sid)
                    .to_string(),
            );
            s.lineage.agent_path = origin
                .and_then(|o| o.get("subagent"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        s.cwd = row
            .as_ref()
            .and_then(|m| m.get("cwd"))
            .and_then(Value::as_str)
            .map(PathBuf::from);
        s.model = row
            .as_ref()
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string);
        s.title = row
            .as_ref()
            .and_then(|m| m.pointer("/metadata/title"))
            .and_then(Value::as_str)
            .map(str::to_string);
        s.created_at = row
            .as_ref()
            .and_then(|m| m.get("started_at"))
            .and_then(Value::as_str)
            .and_then(parse_ts);
        s.updated_at = doc.get("updated_at").and_then(Value::as_str).and_then(parse_ts);
        s.system_prompt = doc.get("system_prompt").and_then(Value::as_str).map(str::to_string);
        sink.meta(&s);

        let mut prompted = false;
        let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
        for view in messages
            .map(JsonView::array)
            .transpose()?
            .flatten()
            .into_iter()
            .flatten()
        {
            let Some(m) = view_message(view?, opts.spans, lead, &mut prompted)? else {
                continue;
            };
            if s.created_at.is_none() {
                s.created_at = m.timestamp;
            }
            if s.model.is_none() {
                s.model.clone_from(&m.model);
            }
            if s.title.is_none() && m.kind == MessageKind::Prompt {
                s.title = text_summary(
                    m.content.iter().filter_map(|b| {
                        if let Block::Text { text, .. } = b {
                            Some(text)
                        } else {
                            None
                        }
                    }),
                    &resolver,
                    false,
                );
            }
            // A span demand checks its own source; the stream rechecks after a slow callback and
            // at the end (`stream` module: who owns the source guarantee).
            let started = std::time::Instant::now();
            let flow = sink.try_message(m)?;
            source.check_after(started)?;
            if flow == Flow::Stop {
                break;
            }
        }
        if s.updated_at.is_none() {
            s.updated_at = mtime(path).or(s.created_at);
        }
        source.check()?;
        Ok(s)
    })
}

fn result_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::Array(items)) if items.iter().all(|i| i.get("type").is_some()) => {
            wire::joined_owned(c, wire::RESULT)
        }
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The message codec, the only decoder of `messages[]`: Anthropic blocks through the family's view
/// codec with [`JsonView::lazy_text`], except a `tool_result`, which names its tool and may hold
/// plain result objects.
fn view_message(view: JsonView<'_>, spans: bool, lead: bool, prompted: &mut bool) -> Result<Option<Message>> {
    let keys = ["id", "role", "ts", "modelInfo", "metrics", "metadata", "content"];
    let Some(record) = Fields::parse(view, &keys)? else {
        return Ok(None);
    };
    // `content` decodes into blocks below; the other parsed keys project into the meta value.
    let [meta_keys @ .., content_key] = keys;
    let meta = Value::Object(record.project(&meta_keys)?);
    let mut blocks = Vec::new();
    if let Some(content) = record.get(content_key) {
        if let Some(text) = content.lazy_text(spans)? {
            blocks.push(Block::from_text(text));
        } else if let Some(items) = content.array()? {
            for view in items {
                let view = view?;
                let part = Fields::parse(view, &["type", "tool_use_id", "content", "is_error", "name"])?;
                if part
                    .as_ref()
                    .map(|r| r.string("type"))
                    .transpose()?
                    .flatten()
                    .as_deref()
                    == Some("tool_result")
                {
                    let part = part.unwrap();
                    let text = part.get("content").map(|v| v.lazy_text(spans)).transpose()?.flatten();
                    let content = if let Some(text) = text {
                        text
                    } else {
                        result_text(part.value("content")?.as_ref()).into()
                    };
                    blocks.push(Block::ToolResult {
                        tool_use_id: part.string("tool_use_id")?.unwrap_or_default(),
                        content,
                        is_error: part.value("is_error")?.and_then(|v| v.as_bool()).unwrap_or(false),
                        tool_name: part.string("name")?,
                        status: None,
                        details: None,
                        extensions: Default::default(),
                    });
                } else if let Some(block) = anthropic::block_from_view_with(
                    view,
                    &anthropic::ANTHROPIC,
                    spans,
                    &mut |v| v.lazy_text(spans),
                    crate::harness::wire::joined_over(spans),
                )? {
                    blocks.push(block);
                }
            }
        }
    }
    Ok(message_with_blocks(&meta, blocks, lead, prompted))
}

fn message_with_blocks(v: &Value, blocks: Vec<Block>, lead: bool, prompted: &mut bool) -> Option<Message> {
    let role = v.get("role").and_then(Value::as_str)?;
    let meta = v.get("metadata");
    let display_error = meta.and_then(|m| m.get("displayOnly")).and_then(Value::as_bool) == Some(true)
        && meta.and_then(|m| m.get("displayRole")).and_then(Value::as_str) == Some("error");
    let mut m = match role {
        "assistant" if display_error => Message::of_kind(Role::Assistant, MessageKind::Error, Origin::Harness),
        "assistant" => Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model),
        "user" if !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. })) => {
            Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness)
        }
        "user" => {
            // A child's first prompt is the task its lead handed it.
            let origin = if !lead && !*prompted {
                Origin::Subagent
            } else {
                Origin::Human
            };
            *prompted = true;
            Message::of_kind(Role::User, MessageKind::Prompt, origin)
        }
        "system" => Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness),
        _ => return None,
    };
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.timestamp = v.get("ts").and_then(Value::as_i64).and_then(ts_from_epoch);
    m.model = v.pointer("/modelInfo/id").and_then(Value::as_str).map(str::to_string);
    if let Some(x) = v.get("metrics").filter(|x| x.is_object()) {
        let n = |k: &str| x.get(k).and_then(Value::as_u64);
        m.usage = Some(Usage {
            input_tokens: n("inputTokens"),
            output_tokens: n("outputTokens"),
            cache_read_tokens: n("cacheReadTokens"),
            cache_creation_tokens: n("cacheWriteTokens"),
            cost_usd: x.get("cost").and_then(Value::as_f64),
            ..Usage::default()
        });
    }
    if let Some(p) = v.pointer("/modelInfo/provider") {
        m.harness_extra_mut(Harness::Cline).insert("provider".into(), p.clone());
    }
    m.content = blocks;
    Some(m)
}

/// The side databases under `<data>/db/` a lead session reads its rows of.
pub(crate) const SIDE_DATABASES: [&str; 3] = ["sessions.db", "cron.db", "teams.db"];

/// The `<data>/db/` directory of a lead session's side databases; a child file (`<sid>/<stem>`)
/// reads none.
pub(crate) fn side_store(r: &SessionRef) -> Option<PathBuf> {
    if r.id.contains('/') || !is_sdk_path(&r.path) {
        return None;
    }
    r.path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|d| d.join("db"))
}

/// The side-database rows of a lead session (see the module docs): schedule fires from
/// `sessions.db` / `cron.db`, team runs from `teams.db`. A child file (`<sid>/<stem>`) has none.
#[cfg(feature = "sqlite")]
pub fn store_affordances(r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
    let Some(db) = side_store(r) else {
        return Ok(Vec::new());
    };
    let mut out = schedule_fires(&db, &r.id)?;
    out.extend(team_runs(&db.join("teams.db"), &r.id)?);
    Ok(out)
}

/// Every lead session's certificate in the side databases at `db` (`<data>/db/`): the rows its
/// read takes ([`schedule_fires`], [`team_runs`]), each query's result digested per session, from
/// one grouped query per database. Every session directory beside `db` is certified, rows or not.
#[cfg(feature = "sqlite")]
pub(crate) fn side_rows(db: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
    use super::store::sqlite::{open_ro, table_exists};
    use crate::source::Rows;
    use rusqlite::types::ValueRef;
    let sessions = db.parent()?.join("sessions");
    let mut certificates: std::collections::HashMap<String, Rows> = fs::read_dir(&sessions)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|sid| {
            (
                sid,
                Rows {
                    count: 0,
                    high: 0,
                    digest: Rows::SEED,
                },
            )
        })
        .collect();
    // (database, tables the read requires, the read's query grouped by its session column first)
    const QUERIES: [(&str, [&str; 2], &str); 3] = [
        (
            "sessions.db",
            ["schedule_executions", "schedules"],
            "SELECT e.session_id, s.name, s.cron_pattern, s.prompt, e.status, e.triggered_at FROM schedule_executions e \
             JOIN schedules s ON s.schedule_id = e.schedule_id ORDER BY e.session_id, e.triggered_at",
        ),
        (
            "cron.db",
            ["cron_runs", "cron_specs"],
            "SELECT r.session_id, p.title, p.schedule_expr, p.prompt, r.status, COALESCE(r.started_at, r.created_at) \
             FROM cron_runs r JOIN cron_specs p ON p.spec_id = r.spec_id ORDER BY r.session_id, r.created_at",
        ),
        (
            "teams.db",
            ["team_events", "team_events"],
            "SELECT team_name, ts, event_type, payload_json FROM team_events ORDER BY team_name, id",
        ),
    ];
    for (index, (file, tables, sql)) in QUERIES.into_iter().enumerate() {
        let path = db.join(file);
        if !path.try_exists().ok()? {
            continue;
        }
        open_ro(&path)
            .ok()?
            .read(|conn, _| {
                if !tables.iter().all(|t| table_exists(conn, t)) {
                    return Ok(());
                }
                let mut statement = conn.prepare(sql)?;
                let columns = statement.column_count();
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    let Ok(sid) = row.get::<_, String>(0) else { continue };
                    let Some(cert) = certificates.get_mut(&sid) else {
                        continue;
                    };
                    cert.count += 1;
                    cert.digest = Rows::fold(cert.digest, &[index as u8]);
                    for column in 1..columns {
                        let bytes: Vec<u8> = match row.get_ref(column)? {
                            ValueRef::Null => b"n".to_vec(),
                            ValueRef::Integer(i) => format!("i{i}").into_bytes(),
                            ValueRef::Real(f) => format!("r{f}").into_bytes(),
                            ValueRef::Text(t) => [b"t".as_slice(), &(t.len() as u64).to_le_bytes(), t].concat(),
                            ValueRef::Blob(b) => [b"b".as_slice(), &(b.len() as u64).to_le_bytes(), b].concat(),
                        };
                        cert.digest = Rows::fold(cert.digest, &bytes);
                    }
                }
                Ok(())
            })
            .ok()?;
    }
    Some(certificates)
}

#[cfg(not(feature = "sqlite"))]
pub(crate) fn side_rows(_db: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
    None
}

#[cfg(not(feature = "sqlite"))]
pub fn store_affordances(_r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
    Ok(Vec::new())
}

/// One `schedule`/`fire` row per execution that ran as `sid`.
#[cfg(feature = "sqlite")]
fn schedule_fires(db: &Path, sid: &str) -> Result<Vec<crate::affordance::Affordance>> {
    use super::store::sqlite::{open_ro, table_exists};
    use crate::affordance::{Affordance, Kind};
    const QUERIES: [(&str, &str, &str, &str); 2] = [
        (
            "sessions.db",
            "schedule_executions",
            "schedules",
            "SELECT s.name, s.cron_pattern, s.prompt, e.status, e.triggered_at FROM schedule_executions e \
             JOIN schedules s ON s.schedule_id = e.schedule_id WHERE e.session_id = ?1 ORDER BY e.triggered_at",
        ),
        (
            "cron.db",
            "cron_runs",
            "cron_specs",
            "SELECT p.title, p.schedule_expr, p.prompt, r.status, COALESCE(r.started_at, r.created_at) FROM cron_runs r \
             JOIN cron_specs p ON p.spec_id = r.spec_id WHERE r.session_id = ?1 ORDER BY r.created_at",
        ),
    ];
    let mut out = Vec::new();
    for (file, runs, specs, sql) in QUERIES {
        if !db.join(file).try_exists()? {
            continue;
        }
        let conn = open_ro(&db.join(file))?;
        type Row = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = conn.read(|conn, _| {
            if !table_exists(conn, runs) || !table_exists(conn, specs) {
                return Ok(Vec::new());
            }
            Ok(conn.prepare(sql).and_then(|mut st| {
                st.query_map([sid], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                })?
                .collect()
            })?)
        })?;
        for (name, cron, prompt, status, at) in rows {
            let summary = [name, cron, prompt, status]
                .into_iter()
                .flatten()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            out.push(Affordance {
                action: "fire",
                timestamp: at.as_deref().and_then(parse_ts),
                ..Affordance::store(
                    Kind::Schedule,
                    format!("{file}:{runs}"),
                    truncate(&summary, 120),
                    Vec::new(),
                )
            });
        }
    }
    Ok(out)
}

/// The lead's team, from `team_events` (keyed `team_name = <lead session id>`): one
/// `subagent_spawn`/`state` row per run, in the order the runs were queued, then one per teammate
/// spawned that never ran.
#[cfg(feature = "sqlite")]
fn team_runs(teams_db: &Path, sid: &str) -> Result<Vec<crate::affordance::Affordance>> {
    use super::store::sqlite::{open_ro, table_exists};
    use crate::affordance::{Affordance, Kind};
    if !teams_db.try_exists()? {
        return Ok(Vec::new());
    }
    let conn = open_ro(teams_db)?;
    let events: Vec<(String, String, String)> = conn.read(|conn, _| {
        if !table_exists(conn, "team_events") {
            return Ok(Vec::new());
        }
        Ok(conn
            .prepare("SELECT ts, event_type, payload_json FROM team_events WHERE team_name = ?1 ORDER BY id")
            .and_then(|mut st| {
                st.query_map([sid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect()
            })?)
    })?;
    /// One team run as its events tell it: the last status and result win.
    struct Run {
        queued: Option<DateTime<Utc>>,
        agent: String,
        task: String,
        status: String,
        result: String,
    }
    let mut runs: Vec<Run> = Vec::new();
    // The Vec preserves first-event order; membership must not scan all prior runs
    // for every update in a growing team's event log.
    let mut run_positions = std::collections::HashMap::new();
    let mut teammates: Vec<(Option<DateTime<Utc>>, String)> = Vec::new();
    for (ts, ty, payload) in events {
        let p = serde_json::from_str::<Value>(&payload)?;
        let at = parse_ts(&ts);
        if ty == "teammate_spawned" {
            if let Some(a) = p.get("agentId").and_then(Value::as_str) {
                teammates.push((at, a.to_string()));
            }
            continue;
        }
        let Some(run) = p.get("run").filter(|_| ty.starts_with("run_")) else {
            continue;
        };
        let s = |k: &str| run.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let id = s("id");
        let i = match run_positions.get(&id).copied() {
            Some(i) => i,
            None => {
                let index = runs.len();
                run_positions.insert(id, index);
                runs.push(Run {
                    queued: at,
                    agent: s("agentId"),
                    task: s("message"),
                    status: String::new(),
                    result: String::new(),
                });
                index
            }
        };
        let r = &mut runs[i];
        r.status = s("status");
        if let Some(text) = run.pointer("/result/text").and_then(Value::as_str) {
            r.result = text.to_string();
        }
        if let Some(e) = run.get("error").and_then(Value::as_str) {
            r.result = e.to_string();
        }
    }
    let row = |at: Option<DateTime<Utc>>, summary: String| Affordance {
        timestamp: at,
        ..Affordance::store(
            Kind::SubagentSpawn,
            "teams.db:team_events",
            truncate(&summary, 120),
            Vec::new(),
        )
    };
    let mut out: Vec<Affordance> = runs
        .iter()
        .map(|r| {
            let mut summary = format!("{}: {} · {}", r.agent, r.task, r.status);
            if !r.result.is_empty() {
                summary.push_str(&format!(" → {}", r.result));
            }
            row(r.queued, summary)
        })
        .collect();
    let ran_agents: std::collections::HashSet<&str> = runs.iter().map(|r| r.agent.as_str()).collect();
    for (at, agent) in teammates {
        if !ran_agents.contains(agent.as_str()) {
            out.push(row(at, format!("{agent}: spawned, never ran")));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectSink;

    fn write(dir: &Path, name: &str, v: Value) {
        fs::write(dir.join(name), serde_json::to_vec(&v).unwrap()).unwrap();
    }

    fn read(r: &SessionRef) -> Session {
        let mut sink = CollectSink::default();
        let mut s = stream(&r.path, &r.id, &mut sink).unwrap();
        s.messages = sink.messages;
        s
    }

    #[test]
    fn child_fanout_ignores_large_lead_metadata_but_keeps_dependency() {
        let world = crate::test_world("cline-child-manifest-fanout");
        let dir = world.base.join("lead");
        fs::create_dir(&dir).unwrap();
        let manifest = dir.join("lead.json");
        write(
            &dir,
            "lead.json",
            json!({"cwd":"/lead-only", "model":"lead-only", "metadata":{"title":"lead-only","large_usage": "x".repeat(4 * 1024 * 1024)}}),
        );
        #[derive(Default)]
        struct ChildSink {
            messages: Vec<Message>,
            dependencies: Vec<PathBuf>,
        }
        impl MessageSink for ChildSink {
            fn message(&mut self, message: Message) -> Flow {
                self.messages.push(message);
                Flow::Continue
            }
            fn dependency(&mut self, path: &Path) {
                self.dependencies.push(path.to_path_buf());
            }
        }
        for index in 0..128 {
            let stem = format!("agent_{index}");
            let path = dir.join(format!("{stem}.messages.json"));
            write(
                &dir,
                &format!("{stem}.messages.json"),
                json!({
                    "origin":{"parentThreadId":"lead","subagent":stem},
                    "messages":[{"role":"user","content":"child task"},{"role":"assistant","content":"child reply","modelInfo":{"id":"child-model"}}]
                }),
            );
            let mut sink = ChildSink::default();
            let session = stream_with_options(&path, &format!("lead/{stem}"), &Demand::RICH, &mut sink).unwrap();
            assert!(sink.dependencies.contains(&manifest));
            assert_eq!(session.cwd, None);
            assert_eq!(session.model.as_deref(), Some("child-model"));
            assert_eq!(session.title.as_deref(), Some("child task"));
            assert_eq!(session.lineage.parent.as_deref(), Some("lead"));
            assert_eq!(sink.messages[0].origin, Origin::Subagent);
        }
    }

    #[test]
    fn lead_and_child_sessions_with_tool_results_and_a_display_error() {
        let root = cv_testkit::temp_dir("cline-sdk");
        let dir = root.join("100_abcde");
        fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "100_abcde.json",
            json!({"session_id": "100_abcde", "cwd": "/p", "model": "m1",
            "started_at": "2026-10-11T00:00:00Z", "status": "completed", "metadata": {"title": "t"}}),
        );
        write(
            &dir,
            "100_abcde.messages.json",
            json!({"version": 1, "agent": "lead", "sessionId": "100_abcde",
            "system_prompt": "sys", "messages": [
            {"id": "u1", "role": "user", "content": [{"type": "text", "text": "read it"}], "ts": 1791748105679_i64},
            {"id": "a1", "role": "assistant", "content": [{"type": "thinking", "thinking": "hm"},
                {"type": "tool_use", "id": "t1", "name": "read_files", "input": {"files": ["a"]}}],
             "modelInfo": {"id": "m1", "provider": "p"}, "metrics": {"inputTokens": 10, "outputTokens": 2, "cacheReadTokens": 5}},
            {"id": "r1", "role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "name": "read_files",
                "content": [{"query": "a", "result": "alpha", "success": true}]}]},
            {"id": "e1", "role": "assistant", "content": [{"type": "text", "text": "boom"}],
             "metadata": {"displayOnly": true, "displayRole": "error"}}]}),
        );
        write(
            &dir,
            "agent_x.messages.json",
            json!({"agent": "subagent", "sessionId": "100_abcde__agent_x",
            "origin": {"mode": "subagent", "parentThreadId": "100_abcde", "subagent": "agent_x"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "do it"}]},
                         {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}]}),
        );

        let mut refs = crate::Discovered::default();
        discover(std::slice::from_ref(&root), &mut refs);
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["100_abcde", "100_abcde/agent_x"]);
        assert_eq!(refs[0].title.as_deref(), Some("t"));

        let lead = read(&refs[0]);
        assert_eq!(lead.cwd.as_deref(), Some(Path::new("/p")));
        assert_eq!(lead.system_prompt.as_deref(), Some("sys"));
        let kinds: Vec<MessageKind> = lead.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::ToolResult,
                MessageKind::Error
            ]
        );
        let u = lead.messages[1].usage.as_ref().unwrap();
        assert_eq!((u.input_tokens, u.cache_read_tokens), (Some(10), Some(5)));
        match &lead.messages[2].content[0] {
            Block::ToolResult { content, tool_name, .. } => {
                assert_eq!(tool_name.as_deref(), Some("read_files"));
                assert!(content.to_string().contains("alpha"));
            }
            b => panic!("{b:?}"),
        }

        let child = read(&refs[1]);
        assert_eq!(child.lineage.parent.as_deref(), Some("100_abcde"));
        assert_eq!(child.lineage.agent_path.as_deref(), Some("agent_x"));
        assert_eq!(child.messages[0].origin, Origin::Subagent);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn native_raw_messages_preserve_full_codec_and_wrapper_validation() {
        let dir = cv_testkit::temp_dir("sdk-raw");
        let path = dir.join("sdk.messages.json");
        let body = "a\n\"é😀".repeat(8192);
        let doc = json!({"messages":[
            {"id":"u", "role":"user", "content":[{"type":"text","text":body}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":body,"signature":"s"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":body}},
                {"type":"tool_use","id":"t","name":"read","input":{"path":"a","payload":body}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":body}]}
        ]});
        fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let mut expected = None;
        for opts in [Demand::RICH, Demand::LEAN.with_spans(), Demand::COMPLETE] {
            let mut sink = CollectSink::default();
            let mut actual = stream_with_options(&path, "sdk", &opts, &mut sink).unwrap();
            actual.messages = sink.messages;
            assert_eq!(
                actual.messages.iter().flat_map(|m| &m.content).any(|block| {
                    matches!(
                        block,
                        Block::ToolUse {
                            input: crate::json::Json::Source(_),
                            ..
                        }
                    )
                }),
                opts.spans
            );
            actual.materialize().unwrap();
            let actual = serde_json::to_string(&actual).unwrap();
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected);
            } else {
                expected = Some(actual);
            }
        }
        let mut published = None;
        let mut mutate = |message: Message| {
            let input = message.content.iter().find(|block| {
                matches!(
                    block,
                    Block::ToolUse {
                        input: crate::json::Json::Source(_),
                        ..
                    }
                )
            });
            if let Some(Block::ToolUse { input, .. }) = input {
                published = Some(input.clone());
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b" ")
                    .unwrap();
                crate::stream::Flow::Stop
            } else {
                crate::stream::Flow::Continue
            }
        };
        assert!(
            stream_with_options(&path, "sdk", &Demand::LEAN.with_spans(), &mut mutate).is_err(),
            "source mutation after publication must reject completion"
        );
        assert!(
            published.unwrap().resolve().is_err(),
            "published descriptor must retain its captured identity"
        );
        for scalar in ["1e400", "\"\\ud800\""] {
            fs::write(
                &path,
                format!("{{\"messages\":[{{\"role\":\"user\",\"content\":\"ok\"}}],\"unknown\":{scalar}}}"),
            )
            .unwrap();
            let mut sink = CollectSink::default();
            assert!(stream_with_options(&path, "sdk", &Demand::LEAN.with_spans(), &mut sink).is_err());
            assert!(sink.messages.is_empty());
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// The corpus run made no schedule, so the fire path is proven on the documented DDL
    /// (`ensureSessionSchema` / `ensureCronSchema`, the columns the reader selects): an execution
    /// and a cron run that ran as the session are its fires; the team events are its team.
    #[cfg(feature = "sqlite")]
    #[test]
    fn wide_team_log_preserves_first_order_final_states_and_idle_duplicates() {
        let world = crate::test_world("cline-wide-team-log");
        let path = world.base.join("teams.db");
        let count = 10_000;
        {
            let mut database = rusqlite::Connection::open(&path).unwrap();
            database.execute_batch("CREATE TABLE team_events(id INTEGER PRIMARY KEY, team_name TEXT, ts TEXT, event_type TEXT, payload_json TEXT)").unwrap();
            let transaction = database.transaction().unwrap();
            {
                let mut insert = transaction.prepare("INSERT INTO team_events(team_name,ts,event_type,payload_json) VALUES('lead','2026-10-11T05:00:00Z',?1,?2)").unwrap();
                for index in 0..count {
                    insert
                        .execute(rusqlite::params![
                            "teammate_spawned",
                            json!({"agentId":format!("agent-{index}")}).to_string()
                        ])
                        .unwrap();
                    insert
                        .execute(rusqlite::params![
                            "teammate_spawned",
                            json!({"agentId":format!("idle-{index}")}).to_string()
                        ])
                        .unwrap();
                    insert.execute(rusqlite::params!["run_queued", json!({"run":{"id":format!("run-{index}"),"agentId":format!("agent-{index}"),"message":format!("task-{index}"),"status":"queued"}}).to_string()]).unwrap();
                }
                // Reverse completion order must not replace first-event run order,
                // agent/task identity, or the original queued timestamp.
                for index in (0..count).rev() {
                    insert.execute(rusqlite::params!["run_completed", json!({"run":{"id":format!("run-{index}"),"agentId":"changed","message":"changed","status":"completed","result":{"text":"done"},"error":"last error wins"}}).to_string()]).unwrap();
                }
                insert
                    .execute(rusqlite::params!["teammate_spawned", r#"{"agentId":"idle-0"}"#])
                    .unwrap();
                insert.execute(rusqlite::params!["run_failed", "{malformed"]).unwrap();
                insert
                    .execute(rusqlite::params!["unrelated", r#"{"run":{"id":"ignored"}}"#])
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        assert!(team_runs(&path, "lead").is_err(), "corrupt side rows are reported");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM team_events WHERE payload_json = '{malformed'", [])
            .unwrap();
        let rows = team_runs(&path, "lead").unwrap();
        assert_eq!(rows.len(), count * 2 + 1);
        for (index, row) in rows[..count].iter().enumerate() {
            assert_eq!(
                row.summary,
                format!("agent-{index}: task-{index} · completed → last error wins")
            );
            assert_eq!(row.timestamp, parse_ts("2026-10-11T05:00:00Z"));
        }
        for (index, row) in rows[count..count * 2].iter().enumerate() {
            assert_eq!(row.summary, format!("idle-{index}: spawned, never ran"));
        }
        assert_eq!(rows.last().unwrap().summary, "idle-0: spawned, never ran");
        assert!(team_runs(&path, "other lead").unwrap().is_empty());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn schedule_fires_and_team_runs_from_the_side_databases() {
        use crate::affordance::Kind;
        let root = cv_testkit::temp_dir("cline-sdk-dbs");
        let sid = "200_fghij";
        let dir = root.join("sessions").join(sid);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(root.join("db")).unwrap();
        let path = dir.join(format!("{sid}.messages.json"));
        write(&dir, &format!("{sid}.messages.json"), json!({"messages": []}));
        let exec = |db: &str, sql: &str| {
            rusqlite::Connection::open(root.join("db").join(db))
                .unwrap()
                .execute_batch(sql)
                .unwrap()
        };
        exec(
            "sessions.db",
            "CREATE TABLE schedules (schedule_id TEXT PRIMARY KEY, name TEXT NOT NULL, cron_pattern TEXT NOT NULL, prompt TEXT NOT NULL);
             CREATE TABLE schedule_executions (execution_id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL, session_id TEXT,
               triggered_at TEXT NOT NULL, status TEXT NOT NULL);
             INSERT INTO schedules VALUES ('s1', 'nightly', '0 3 * * *', 'tidy up');
             INSERT INTO schedule_executions VALUES ('e1', 's1', '200_fghij', '2026-10-11T03:00:00Z', 'completed');
             INSERT INTO schedule_executions VALUES ('e2', 's1', 'someone-else', '2026-10-12T03:00:00Z', 'completed');",
        );
        exec(
            "cron.db",
            "CREATE TABLE cron_specs (spec_id TEXT PRIMARY KEY, title TEXT NOT NULL, prompt TEXT, schedule_expr TEXT);
             CREATE TABLE cron_runs (run_id TEXT PRIMARY KEY, spec_id TEXT NOT NULL, status TEXT NOT NULL, started_at TEXT,
               session_id TEXT, created_at TEXT NOT NULL);
             INSERT INTO cron_specs VALUES ('c1', 'hourly ping', 'ping', '0 * * * *');
             INSERT INTO cron_runs VALUES ('r1', 'c1', 'done', '2026-10-11T04:00:00Z', '200_fghij', '2026-10-11T04:00:00Z');",
        );
        exec(
            "teams.db",
            r#"CREATE TABLE team_events (id INTEGER PRIMARY KEY AUTOINCREMENT, team_name TEXT NOT NULL, ts TEXT NOT NULL,
                 event_type TEXT NOT NULL, payload_json TEXT NOT NULL);
             INSERT INTO team_events (team_name, ts, event_type, payload_json) VALUES
               ('200_fghij', '2026-10-11T05:00:00Z', 'teammate_spawned', '{"agentId":"helper"}'),
               ('200_fghij', '2026-10-11T05:00:01Z', 'teammate_spawned', '{"agentId":"idle"}'),
               ('200_fghij', '2026-10-11T05:00:02Z', 'run_queued', '{"run":{"id":"run_1","agentId":"helper","status":"queued","message":"say ok"}}'),
               ('200_fghij', '2026-10-11T05:00:09Z', 'run_completed', '{"run":{"id":"run_1","agentId":"helper","status":"completed","message":"say ok","result":{"text":"ok"}}}');"#,
        );
        let mut r = SessionRef::new(sid, Harness::Cline, path);
        r.created_at = parse_ts("2026-10-11T00:00:00Z");
        let rows = store_affordances(&r).unwrap();
        let got: Vec<(Kind, &str, &str)> = rows.iter().map(|a| (a.kind, a.action, a.summary.as_str())).collect();
        assert_eq!(
            got,
            [
                (Kind::Schedule, "fire", "nightly · 0 3 * * * · tidy up · completed"),
                (Kind::Schedule, "fire", "hourly ping · 0 * * * * · ping · done"),
                (Kind::SubagentSpawn, "state", "helper: say ok · completed → ok"),
                (Kind::SubagentSpawn, "state", "idle: spawned, never ran"),
            ]
        );
        let child = SessionRef::new(
            format!("{sid}/agent_x"),
            Harness::Cline,
            dir.join("agent_x.messages.json"),
        );
        assert!(
            store_affordances(&child).unwrap().is_empty(),
            "a child file has no side rows"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
