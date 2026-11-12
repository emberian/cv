//! Connection-scoped export oracles; callbacks own no production instrumentation.
use crate::json::{ReadInput, Validation};
use crate::{Block, Harness, Message, Role, Session};
use anyhow::Result;
use rusqlite::{ffi, Connection};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Default)]
struct Counts {
    commits: usize,
    inserts: HashMap<String, HashSet<usize>>,
    runs: HashMap<String, Vec<i32>>,
    mutate: Option<PathBuf>,
    mutation_error: Option<String>,
    row_calls: usize,
    mutate_at: usize,
}
struct Hooks<'a> {
    connection: &'a mut Connection,
    counts: Box<Counts>,
}
impl<'a> Hooks<'a> {
    fn new(connection: &'a mut Connection, mutate: Option<PathBuf>) -> Self {
        let mut counts = Box::new(Counts {
            mutate,
            mutate_at: 1,
            ..Default::default()
        });
        let ptr = std::ptr::from_mut(counts.as_mut()).cast();
        // SAFETY: the boxed callback context stays at a stable address until Drop unregisters
        // both hooks. The exclusively borrowed connection cannot execute on another thread.
        unsafe {
            ffi::sqlite3_commit_hook(connection.handle(), Some(commit), ptr);
            ffi::sqlite3_trace_v2(connection.handle(), ffi::SQLITE_TRACE_STMT, Some(trace), ptr);
        }
        Self { connection, counts }
    }
}
impl Drop for Hooks<'_> {
    fn drop(&mut self) {
        // SAFETY: the connection and context are still alive. Unregister before freeing context.
        unsafe {
            ffi::sqlite3_trace_v2(self.connection.handle(), 0, None, std::ptr::null_mut());
            ffi::sqlite3_commit_hook(self.connection.handle(), None, std::ptr::null_mut());
        }
    }
}
unsafe extern "C" fn commit(context: *mut std::ffi::c_void) -> i32 {
    // SAFETY: installed only with the stable Counts box retained by Hooks.
    unsafe { &mut *context.cast::<Counts>() }.commits += 1;
    0
}
unsafe extern "C" fn trace(
    _: u32,
    context: *mut std::ffi::c_void,
    statement: *mut std::ffi::c_void,
    text: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: SQLite supplies the current live statement and our registered stable context.
    let counts = unsafe { &mut *context.cast::<Counts>() };
    if !text.is_null()
        && unsafe { std::ffi::CStr::from_ptr(text.cast()) }
            .to_bytes()
            .starts_with(b"--")
    {
        return 0;
    }
    let sql = unsafe { ffi::sqlite3_sql(statement.cast()) };
    if sql.is_null() {
        return 0;
    }
    let sql = unsafe { std::ffi::CStr::from_ptr(sql) }.to_string_lossy();
    if sql.starts_with("INSERT INTO session ")
        || sql.starts_with("INSERT OR REPLACE INTO session ")
        || sql.starts_with("INSERT OR REPLACE INTO sessions ")
        || sql.starts_with("INSERT INTO session_message ")
        || sql.starts_with("INSERT INTO message ")
        || sql.starts_with("INSERT INTO part ")
        || sql.starts_with("INSERT INTO messages ")
    {
        let key = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        counts
            .inserts
            .entry(key.clone())
            .or_default()
            .insert(statement as usize);
        // RUN counters advance on a reused statement; pointer identity alone could miss an
        // allocator reusing the same address for a freshly prepared statement each row.
        let runs = unsafe { ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_RUN, 0) };
        counts.runs.entry(key).or_default().push(runs);
        // Mutate after the first prepared row has been produced. A later demanded source
        // must fail inside the transaction, after schema/session/earlier row changes.
        // Session metadata is traced for prepared-shape accounting, but mutation stays
        // after a body row as before the v1 writer stopped using INSERT OR REPLACE.
        if sql.starts_with("INSERT INTO ") && !sql.starts_with("INSERT INTO session ") {
            counts.row_calls += 1;
            if counts.row_calls == counts.mutate_at {
                if let Some(path) = counts.mutate.take() {
                    if let Err(error) = std::fs::write(path, b"changed during row insertion") {
                        counts.mutation_error = Some(error.to_string());
                    }
                }
            }
        }
    }
    0
}

pub(crate) fn fixture(world: &cv_testkit::World) -> Result<(Session, PathBuf)> {
    let path = world.base.join("body.json");
    std::fs::write(&path, serde_json::to_vec(&"λ\nquoted \"body\"".repeat(2000))?)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let text = source
        .read(ReadInput::Range(0, source.len()), Validation::Value, |v| {
            v.source_text()
        })?
        .unwrap();
    let arguments = world.base.join("arguments.json");
    std::fs::write(&arguments, br#"{"z":1,"a":"escaped\n","dup":1,"dup":2,"n":1.25}"#)?;
    let input = crate::Json::source(&arguments, 0, std::fs::metadata(&arguments)?.len())?;
    let mut session = Session::new("source", Harness::Claude);
    session.title = Some("exact title".into());
    session.cwd = Some("/synthetic/sqlite".into());
    session.model = Some("session model".into());
    session.system_prompt = Some("system prompt".into());
    session.created_at =
        Some(chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")?.with_timezone(&chrono::Utc));
    session.updated_at = session.created_at;
    let mut user = Message::new(Role::User);
    user.content = vec![
        Block::Text { text: "".into() },
        Block::Text { text: text.clone() },
        Block::Text { text: "".into() },
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: None,
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("/synthetic/file".into()),
            source: Some("https://synthetic/file".into()),
        },
    ];
    session.messages.push(user);
    let mut assistant = Message::new(Role::Assistant);
    assistant.model = Some("message model".into());
    assistant.content = vec![
        Block::Text { text: text.clone() },
        Block::Thinking {
            text: text.clone(),
            signature: Some("signed".into()),
            encrypted: Some("encrypted".into()),
            redacted: false,
        },
        Block::Thinking {
            text: " \n".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        },
        Block::Thinking {
            text: "".into(),
            signature: Some("signature only".into()),
            encrypted: None,
            redacted: true,
        },
        Block::ToolUse {
            id: "paired".into(),
            name: "Read".into(),
            input,
            namespace: None,
        },
    ];
    assistant.usage = Some(crate::Usage {
        input_tokens: Some(7),
        output_tokens: Some(3),
        cost_usd: Some(0.25),
        ..Default::default()
    });
    assistant.extra.insert(
        "hermes".into(),
        json!({"finish_reason":"stop","platform_message_id":"provider","observed":true}),
    );
    assistant.content.push(Block::ToolUse {
        id: "deferred".into(),
        name: "Read".into(),
        input: json!({"path":"/synthetic"}).into(),
        namespace: Some(crate::affordance::DEFERRED.into()),
    });
    session.extra.insert("hermes".into(), json!({"session":{"source":"native","user_id":"user","model_config":"{\"temperature\":0}","end_reason":"stop","tool_call_count":9,"input_tokens":17,"output_tokens":19,"cache_read_tokens":23,"cache_write_tokens":29,"reasoning_tokens":31}}));
    session.messages.push(assistant);
    let mut tools = Message::new(Role::Tool);
    for (id, error, details) in [
        (
            "paired",
            false,
            json!({"metadata":{"collision":"old","first":1},"title":"kept","collision":"new","time":{"start":1}}),
        ),
        ("paired", true, json!({"metadata":false,"title":"last","other":2})),
        ("orphan", false, json!([1, 2])),
    ] {
        tools.content.push(Block::ToolResult {
            tool_use_id: id.into(),
            content: text.clone(),
            is_error: error,
            tool_name: Some("Read".into()),
            status: Some("completed".into()),
            details: Some(details),
        });
    }
    session.messages.push(tools);
    let mut plain = Message::new(Role::System);
    plain.content = vec![
        Block::Text { text: "".into() },
        Block::Text { text },
        Block::Text { text: "".into() },
    ];
    session.messages.push(plain);
    let mut raw = Message::new(Role::Assistant);
    raw.content = vec![Block::Thinking {
        text: "projection".into(),
        signature: None,
        encrypted: None,
        redacted: false,
    }];
    raw.extra.insert("hermes".into(), json!({"reasoning":"raw reasoning","reasoning_content":"raw second","reasoning_details":[{"type":"native"}],"codex_reasoning_items":[1],"codex_message_items":[2]}));
    session.messages.push(raw);
    Ok((session, path))
}

fn rows(connection: &Connection, sql: &str) -> Result<Vec<Vec<Value>>> {
    let mut statement = connection.prepare(sql)?;
    let width = statement.column_count();
    let result = statement
        .query_map([], |row| {
            (0..width)
                .map(|i| {
                    Ok(match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Real(n) => json!(n),
                        rusqlite::types::ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                        rusqlite::types::ValueRef::Blob(s) => json!(s),
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(result)
}

pub(crate) fn logical(connection: &Connection, target: Harness) -> Result<Value> {
    let schema = rows(
        connection,
        "SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY name",
    )?;
    Ok(if target == Harness::OpenCode {
        json!({"schema":schema,"session":rows(connection,"SELECT * FROM session ORDER BY id")?,"message":rows(connection,"SELECT session_id,time_created,time_updated,data FROM message ORDER BY time_created,id")?,"part":rows(connection,"SELECT session_id,time_created,time_updated,data FROM part ORDER BY time_created,id")?})
    } else {
        json!({"schema":schema,"sessions":rows(connection,"SELECT * FROM sessions ORDER BY id")?,"messages":rows(connection,"SELECT * FROM messages ORDER BY id")?,"fts":rows(connection,"SELECT rowid,content FROM messages_fts ORDER BY rowid")?,"trigram":rows(connection,"SELECT rowid,content FROM messages_fts_trigram ORDER BY rowid")?})
    })
}

/// [`exercise_with`] against frozen goldens (`tests/fixtures/emit-frozen/<name>.json`): the
/// SHA-256 of each thinking mode's logical rows, frozen from the retired legacy emitter.
pub(crate) fn exercise_frozen(
    target: Harness,
    name: &'static str,
    direct: fn(&Session, &Path, &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult>,
    writer: impl Fn(&Session, &crate::emit::EmitOptions, &mut Connection) -> Result<()>,
) -> Result<()> {
    let mut frozen = crate::emit::frozen::Frozen::new(name);
    exercise_with(
        target,
        |mode, rows| {
            let digest = crate::digest::sha256_hex(&serde_json::to_vec(rows)?);
            frozen
                .0
                .insert(format!("{target}-{mode}"), json!({ "logical": digest }));
            Ok(())
        },
        direct,
        writer,
    )?;
    frozen.check()
}

/// What the shared oracle needs to know about one target's store: the per-target facts the
/// exercise branches on, in one table instead of inline conditionals.
struct TargetFacts {
    /// One prepared INSERT shape per row type.
    insert_shapes: usize,
    /// The fixture's row count, where the last-read guard's mutation lands.
    row_count_sql: &'static str,
    /// SQL rejecting a synthetic row mid-write, to exercise SQL-failure rollback.
    reject_trigger: &'static str,
    /// SQL run once before the failure sweeps (Hermes drops its FTS triggers so the
    /// failure comes from the source, not full-text maintenance).
    pre_failure_sql: &'static str,
    /// The `new_id` a second export must use to reach the failure sweeps at all
    /// (OpenCode rejects identity reuse; the rest reuse "fixed").
    failure_new_id: &'static str,
}

fn facts(target: Harness) -> TargetFacts {
    match target {
        Harness::OpenCode => TargetFacts {
            insert_shapes: 3,
            row_count_sql: "SELECT (SELECT count(*) FROM message)+(SELECT count(*) FROM part)",
            reject_trigger: "CREATE TRIGGER reject_system BEFORE INSERT ON message WHEN json_extract(new.data,'$.role')='system' BEGIN SELECT RAISE(ABORT,'synthetic row rejection'); END;",
            pre_failure_sql: "",
            failure_new_id: "fresh",
        },
        Harness::Hermes => TargetFacts {
            insert_shapes: 2,
            row_count_sql: "SELECT count(*) FROM messages",
            reject_trigger: "CREATE TRIGGER reject_system BEFORE INSERT ON messages WHEN new.role='system' BEGIN SELECT RAISE(ABORT,'synthetic row rejection'); END;",
            pre_failure_sql: "DROP TRIGGER messages_fts_insert; DROP TRIGGER messages_fts_trigram_insert;",
            failure_new_id: "fixed",
        },
        _ => TargetFacts {
            insert_shapes: 3,
            row_count_sql: "SELECT count(*) FROM messages",
            reject_trigger: "CREATE TRIGGER reject_system BEFORE INSERT ON messages WHEN new.role='system' BEGIN SELECT RAISE(ABORT,'synthetic row rejection'); END;",
            pre_failure_sql: "",
            failure_new_id: "fixed",
        },
    }
}

/// Emit the shared fixture in every thinking mode through the connection `writer`, `direct` and
/// the dispatcher, which must agree, hand each mode's rows to `observe`, then check that every
/// source and SQL failure rolls the transaction back.
fn exercise_with(
    target: Harness,
    mut observe: impl FnMut(crate::emit::ThinkingMode, &Value) -> Result<()>,
    direct: fn(&Session, &Path, &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult>,
    writer: impl Fn(&Session, &crate::emit::EmitOptions, &mut Connection) -> Result<()>,
) -> Result<()> {
    let world = crate::test_world("sqlite-export-oracle");
    let (session, path) = fixture(&world)?;
    let facts = facts(target);
    for mode in crate::emit::ThinkingMode::ALL {
        let options = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            thinking: mode,
            ..Default::default()
        };
        let mut new = Connection::open_in_memory()?;
        let hooks = Hooks::new(&mut new, None);
        writer(&session, &options, hooks.connection)?;
        assert_eq!(hooks.counts.commits, 1);
        assert_eq!(
            hooks.counts.inserts.len(),
            facts.insert_shapes,
            "one prepared INSERT shape per row type"
        );
        assert!(hooks.counts.inserts.values().all(|pointers| pointers.len() == 1));
        assert!(
            hooks
                .counts
                .runs
                .values()
                .all(|runs| runs.windows(2).all(|pair| pair[1] > pair[0])),
            "prepared inserts must retain advancing execution counters"
        );
        drop(hooks);
        let rows = logical(&new, target)?;
        observe(mode, &rows)?;
        if target == Harness::OpenCode {
            // This oracle exercises existing v1 stores (the writer's schema); fresh exports now
            // bootstrap native v2.
            let mut ddl = new.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type DESC,name")?;
            let schema = ddl
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for kind in ["direct", "dispatch"] {
                let directory = world.base.join(format!("{kind}-{target}-{mode}"));
                std::fs::create_dir_all(&directory)?;
                let connection = Connection::open(directory.join("opencode.db"))?;
                for statement in &schema {
                    connection.execute_batch(statement)?;
                }
            }
        }
        let direct_result = direct(&session, &world.base.join(format!("direct-{target}-{mode}")), &options)?;
        let dispatch_result = crate::emit::emit(
            &session,
            target,
            &world.base.join(format!("dispatch-{target}-{mode}")),
            &options,
        )?;
        assert_eq!(logical(&Connection::open(direct_result.path)?, target)?, rows);
        assert_eq!(logical(&Connection::open(dispatch_result.path)?, target)?, rows);
    }
    let options = crate::emit::EmitOptions {
        new_id: Some("fixed".into()),
        ..Default::default()
    };
    // Targets that reject identity reuse (OpenCode) seed one preserved session, then the
    // failure sweeps insert a distinct one, so collision rejection cannot masquerade as
    // successful source/SQL rollback coverage.
    let failure_options = crate::emit::EmitOptions {
        new_id: Some(facts.failure_new_id.into()),
        ..options.clone()
    };
    let mut database = Connection::open_in_memory()?;
    writer(&session, &options, &mut database)?;
    if !facts.pre_failure_sql.is_empty() {
        database.execute_batch(facts.pre_failure_sql)?;
    }
    let before_schema = logical(&database, target)?;
    let hooks = Hooks::new(&mut database, Some(path.clone()));
    assert!(writer(&session, &failure_options, hooks.connection).is_err());
    assert_eq!(hooks.counts.commits, 0);
    assert!(hooks.counts.mutation_error.is_none());
    assert!(
        hooks.counts.mutate.is_none(),
        "body-row source mutation was not reached"
    );
    drop(hooks);
    assert_eq!(
        logical(&database, target)?,
        before_schema,
        "logical replacement/rows/schema/FTS rollback"
    );
    let (json_error, _) = fixture(&world)?;
    let mut database = Connection::open_in_memory()?;
    writer(&json_error, &options, &mut database)?;
    let before = logical(&database, target)?;
    let hooks = Hooks::new(&mut database, Some(world.base.join("arguments.json")));
    assert!(writer(&json_error, &failure_options, hooks.connection).is_err());
    assert_eq!(hooks.counts.commits, 0);
    assert!(
        hooks.counts.mutate.is_none(),
        "argument-source mutation was not reached"
    );
    drop(hooks);
    assert_eq!(
        logical(&database, target)?,
        before,
        "late argument Source failure rollback"
    );
    let (last, path) = fixture(&world)?;
    let mut database = Connection::open_in_memory()?;
    writer(&last, &options, &mut database)?;
    let before = logical(&database, target)?;
    let row_count = database.query_row(facts.row_count_sql, [], |r| r.get::<_, usize>(0))?;
    let mut hooks = Hooks::new(&mut database, Some(path));
    hooks.counts.mutate_at = row_count;
    assert!(
        writer(&last, &failure_options, hooks.connection).is_err(),
        "last-read source change must reject commit"
    );
    assert_eq!(hooks.counts.commits, 0);
    assert!(hooks.counts.mutate.is_none());
    drop(hooks);
    assert_eq!(logical(&database, target)?, before);
    let (sql_error, _) = fixture(&world)?;
    let mut database = Connection::open_in_memory()?;
    writer(&sql_error, &options, &mut database)?;
    database.execute_batch(facts.reject_trigger)?;
    let before = logical(&database, target)?;
    let hooks = Hooks::new(&mut database, None);
    let error = writer(&sql_error, &failure_options, hooks.connection).unwrap_err();
    assert!(
        format!("{error:#}").contains("synthetic row rejection"),
        "unexpected failure before the SQL rejection: {error:#}"
    );
    assert_eq!(hooks.counts.commits, 0);
    drop(hooks);
    assert_eq!(
        logical(&database, target)?,
        before,
        "SQL failure rolls back prior rows and preserves existing triggers"
    );
    // Fresh schema changes also roll back after an earlier row was inserted.
    let (fresh, path) = fixture(&world)?;
    let mut database = Connection::open_in_memory()?;
    let hooks = Hooks::new(&mut database, Some(path));
    assert!(writer(&fresh, &options, hooks.connection).is_err());
    assert_eq!(hooks.counts.commits, 0);
    drop(hooks);
    assert_eq!(
        database.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}

pub(crate) fn exercise_v2(
    writer: impl Fn(&Session, &crate::emit::EmitOptions, &mut Connection) -> Result<()>,
) -> Result<()> {
    let world = crate::test_world("sqlite-v2-export-oracle");
    let (session, path) = fixture(&world)?;
    for mode in crate::emit::ThinkingMode::ALL {
        let options = crate::emit::EmitOptions {
            thinking: mode,
            ..Default::default()
        };
        let mut owned = session.clone();
        owned.materialize()?;
        let mut expected = Connection::open_in_memory()?;
        writer(&owned, &options, &mut expected)?;
        let mut actual = Connection::open_in_memory()?;
        let hooks = Hooks::new(&mut actual, None);
        writer(&session, &options, hooks.connection)?;
        assert_eq!(hooks.counts.commits, 1, "bootstrap and export commit together");
        let inserts = hooks
            .counts
            .inserts
            .iter()
            .filter(|(sql, _)| sql.starts_with("INSERT INTO session_message "))
            .collect::<Vec<_>>();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].1.len(), 1, "one prepared native message INSERT");
        assert!(hooks
            .counts
            .runs
            .values()
            .any(|runs| runs.len() > 1 && runs.windows(2).all(|pair| pair[1] > pair[0])));
        assert_eq!(
            rows(
                hooks.connection,
                "SELECT type,seq,time_created,data FROM session_message ORDER BY seq"
            )?,
            rows(
                &expected,
                "SELECT type,seq,time_created,data FROM session_message ORDER BY seq"
            )?
        );
    }
    let mut conn = Connection::open_in_memory()?;
    let hooks = Hooks::new(&mut conn, Some(path));
    assert!(writer(&session, &Default::default(), hooks.connection).is_err());
    assert_eq!(hooks.counts.commits, 0);
    assert!(
        hooks.counts.mutate.is_none(),
        "source changed during first row insertion"
    );
    assert_eq!(
        hooks.connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0,
        "rollback includes native bootstrap"
    );
    Ok(())
}

pub(crate) fn rollback_record_only_source(
    session: &Session,
    path: PathBuf,
    writer: impl Fn(&Session, &mut Connection) -> Result<()>,
) -> Result<()> {
    let mut conn = Connection::open_in_memory()?;
    let hooks = Hooks::new(&mut conn, Some(path));
    assert!(writer(session, hooks.connection).is_err());
    assert!(
        hooks.counts.mutate.is_none(),
        "record-only source changed inside the export"
    );
    assert_eq!(hooks.counts.commits, 0);
    assert_eq!(
        hooks.connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}
