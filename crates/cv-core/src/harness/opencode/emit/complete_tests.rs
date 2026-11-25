use super::*;
use crate::Harness;
use crate::{
    harness::{opencode::OpenCode, Adapter},
    stream::Demand,
};
use rusqlite::Connection;
use serde_json::Value;

fn rows(conn: &Connection, id: &str) -> Result<Vec<(String, Value)>> {
    let mut stmt = conn.prepare("SELECT type,data FROM session_message WHERE session_id=?1 ORDER BY seq")?;
    let rows = stmt
        .query_map([id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(ty, raw)| Ok((ty, serde_json::from_str(&raw)?)))
        .collect()
}
fn logical_rows(conn: &Connection, id: &str) -> Result<Value> {
    Ok(serde_json::to_value(rows(conn, id)?)?)
}
#[test]
fn complete_v2_replay_matches_native_rows_owned_and_public_ir() -> Result<()> {
    let world = crate::test_world("complete-opencode");
    let root = world.proj.clone();
    let source = root.join("source.db");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db"),
        &source,
    )?;
    let adapter = OpenCode::with_db(source.clone());
    let references = adapter.discover()?;
    let source_conn = Connection::open(&source)?;
    for reference in &references {
        let checked = crate::emit::parse_for_emit(&adapter, reference, Harness::OpenCode)?;
        assert!(
            checked.messages.iter().any(|m| m.complete_record.is_some()),
            "checked complete records retained"
        );
        let owned = crate::stream::collect_with(&adapter, reference, &Demand::COMPLETE)?;
        let serialized: Session = serde_json::from_slice(&serde_json::to_vec(&checked)?)?;
        let mut materialized = checked.clone();
        materialized.materialize()?;
        for (index, message) in checked.messages.iter().enumerate() {
            let mut plain = Vec::new();
            message.write_json(&mut plain, &checked.resolver())?;
            assert_eq!(
                serde_json::from_slice::<Value>(&plain)?,
                serde_json::to_value(message)?,
                "bounded full _record serialization"
            );
            let mut pretty = Vec::new();
            message.write_json_pretty(&mut pretty, &checked.resolver())?;
            assert_eq!(
                pretty,
                serde_json::to_vec_pretty(message)?,
                "bounded pretty complete row {index}"
            );
        }
        for mode in ThinkingMode::ALL {
            let options = EmitOptions {
                new_id: Some("ses_complete_export".into()),
                thinking: mode,
                ..Default::default()
            };
            let mut oracle = None;
            for (kind, session) in [
                ("checked", &checked),
                ("owned", &owned),
                ("public", &serialized),
                ("materialized", &materialized),
            ] {
                let output = root.join(format!("{}-{mode}-{kind}", reference.id));
                let result = emit(session, &output, &options)?;
                let actual = logical_rows(&Connection::open(&result.path)?, &result.new_id)?;
                if mode == ThinkingMode::Native {
                    let expected = logical_rows(&source_conn, &reference.id)?;
                    assert_eq!(
                        actual.as_array().unwrap().len(),
                        expected.as_array().unwrap().len(),
                        "row count {kind}/{}",
                        reference.id
                    );
                    for (row, (a, b)) in actual
                        .as_array()
                        .unwrap()
                        .iter()
                        .zip(expected.as_array().unwrap())
                        .enumerate()
                    {
                        assert_eq!(a, b, "native row {row} {kind}/{}", reference.id);
                    }
                }
                if let Some(expected) = &oracle {
                    assert_eq!(&actual, expected, "{kind}/{mode} {}", reference.id);
                } else {
                    oracle = Some(actual);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn embedded_v2_attachments_keep_bytes_external_references_report_loss() -> Result<()> {
    let world = crate::test_world("opencode-attachments");
    let root = world.proj.clone();
    let mut session = Session::new("foreign", Harness::Claude);
    let mut user = crate::Message::new(Role::User);
    user.content.push(Block::Image {
        media_type: Some("image/png".into()),
        data_ref: Some("data:image/png;base64,aGk=".into()),
        extensions: Default::default(),
    });
    user.content.push(Block::Image {
        media_type: Some("image/png".into()),
        data_ref: Some("https://unavailable.invalid/image.png".into()),
        extensions: Default::default(),
    });
    session.messages.push(user);
    let (result, report) = crate::emit::emit_report(&session, Harness::OpenCode, &root, &EmitOptions::default())?;
    let rows = rows(&Connection::open(&result.path)?, &result.new_id)?;
    assert_eq!(rows[0].1["files"][0]["data"], "aGk=");
    assert_eq!(rows[0].1["files"][0]["source"]["type"], "inline");
    assert_eq!(rows[0].1["files"].as_array().unwrap().len(), 1);
    assert!(rows[0].1["text"]
        .as_str()
        .unwrap()
        .contains("Image attachment unavailable"));
    assert!(
        report.deltas.iter().any(|d| d.what == "block:image" && !d.expected),
        "unavailable model input is an explicit fidelity loss"
    );
    Ok(())
}

#[test]
fn complete_v2_edits_groups_models_and_raw_only_sources_are_guarded() -> Result<()> {
    let world = crate::test_world("opencode-edit");
    let root = world.proj.clone();
    let source = root.join("source.db");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db"),
        &source,
    )?;
    let adapter = OpenCode::with_db(source.clone());
    let references = adapter.discover()?;
    let source_conn = Connection::open(&source)?;
    for title in ["tools", "compact", "image"] {
        let reference = references.iter().find(|r| r.title.as_deref() == Some(title)).unwrap();
        let baseline = crate::emit::parse_for_emit(&adapter, reference, Harness::OpenCode)?;
        for case in [
            "text",
            "tool",
            "usage",
            "model",
            "delete-derived",
            "reorder-derived",
            "duplicate-primary",
            "duplicate-derived",
            "only-derived",
            "idle-time",
        ] {
            let mut edited = baseline.clone();
            match case {
                "text" => {
                    if let Some(Block::Text { text, .. }) = edited
                        .messages
                        .iter_mut()
                        .filter(|m| m.role == Role::User)
                        .flat_map(|m| &mut m.content)
                        .find(|b| matches!(b, Block::Text { .. }))
                    {
                        *text = "edited native user body".into();
                    }
                }
                "tool" => {
                    for block in edited.messages.iter_mut().flat_map(|m| &mut m.content) {
                        if let Block::ToolResult { content, .. } = block {
                            *content = "clipped tool body".into();
                            break;
                        }
                    }
                }
                "usage" => {
                    for message in &mut edited.messages {
                        if message.role == Role::Assistant {
                            message.usage = None;
                        }
                    }
                }
                "model" => {
                    // The model is a per-turn fact the emitter writes as the turn states it; an
                    // edit changes the turns (and the session default they share).
                    edited.model = Some("changed-model".into());
                    for message in &mut edited.messages {
                        if message.role == Role::Assistant {
                            message.model = Some("changed-model".into());
                        }
                    }
                }
                "delete-derived" => edited.messages.retain(|m| {
                    m.harness_extra(Harness::OpenCode)
                        .and_then(|extra| extra.get("record_owner"))
                        .is_none()
                }),
                "reorder-derived" => {
                    if let Some(index) = edited.messages.iter().position(|m| {
                        m.harness_extra(Harness::OpenCode)
                            .and_then(|extra| extra.get("record_owner"))
                            .is_some()
                    }) {
                        edited.messages.swap(index, index - 1);
                    }
                }
                "duplicate-primary" => {
                    if let Some(index) = edited.messages.iter().position(|m| m.complete_record.is_some()) {
                        edited.messages.insert(index, edited.messages[index].clone());
                    }
                }
                "duplicate-derived" => {
                    if let Some(index) = edited.messages.iter().position(|m| {
                        m.harness_extra(Harness::OpenCode)
                            .and_then(|extra| extra.get("record_owner"))
                            .is_some()
                    }) {
                        edited.messages.push(edited.messages[index].clone());
                    }
                }
                "only-derived" => edited.messages.retain(|m| {
                    m.harness_extra(Harness::OpenCode)
                        .and_then(|extra| extra.get("record_owner"))
                        .is_some()
                }),
                "idle-time" => {
                    for message in &mut edited.messages {
                        if message.kind == crate::MessageKind::Carrier {
                            message.timestamp = None;
                        }
                    }
                }
                _ => unreachable!(),
            }
            let owned: Session = serde_json::from_slice(&serde_json::to_vec(&edited)?)?;
            for mode in ThinkingMode::ALL {
                let options = EmitOptions {
                    new_id: Some("ses_edit".into()),
                    thinking: mode,
                    ..Default::default()
                };
                let a = emit(&edited, &root.join(format!("{title}-{case}-{mode}-checked")), &options)?;
                let b = emit(&owned, &root.join(format!("{title}-{case}-{mode}-owned")), &options)?;
                assert_eq!(
                    logical_rows(&Connection::open(&a.path)?, &a.new_id)?,
                    logical_rows(&Connection::open(&b.path)?, &b.new_id)?,
                    "{title}/{case}/{mode}"
                );
                if case == "text" && title == "image" {
                    let original = rows(&source_conn, &reference.id)?
                        .into_iter()
                        .find(|(ty, _)| ty == "user")
                        .unwrap()
                        .1;
                    let changed = rows(&Connection::open(&a.path)?, &a.new_id)?
                        .into_iter()
                        .find(|(ty, _)| ty == "user")
                        .unwrap()
                        .1;
                    assert_eq!(changed["text"], "edited native user body");
                    assert_eq!(
                        changed["files"], original["files"],
                        "editing text does not drop native file bytes"
                    );
                }
                if case == "tool" {
                    let output = logical_rows(&Connection::open(&a.path)?, &a.new_id)?.to_string();
                    if baseline
                        .messages
                        .iter()
                        .flat_map(|m| &m.content)
                        .any(|b| matches!(b, Block::ToolResult { .. }))
                    {
                        assert!(output.contains("clipped tool body"));
                    }
                }
                if case == "model" {
                    for (ty, data) in rows(&Connection::open(&a.path)?, &a.new_id)? {
                        if ty == "assistant" {
                            assert_eq!(data["model"]["id"], "changed-model");
                        }
                    }
                }
                if case == "idle-time" {
                    assert!(
                        !rows(&Connection::open(&a.path)?, &a.new_id)?
                            .iter()
                            .any(|(ty, _)| ty == "idle"),
                        "changed owned/source carriers cannot bypass witness"
                    );
                }
            }
        }
    }
    let raw = root.join("raw-only.db");
    let mut empty = Session::new("foreign", Harness::Claude);
    empty.cwd = Some(root.clone());
    let base = EmitOptions {
        new_id: Some("ses_raw".into()),
        ..Default::default()
    };
    emit(&empty, &raw, &base)?;
    let connection = Connection::open(&raw)?;
    connection.execute("INSERT INTO session_message VALUES('msg_raw','ses_raw','user',0,1700000000000,1700000000000,?1)",[json!({"text":"","time":{"created":1700000000000_i64},"files":[{"data":"YWFh".repeat(20000),"mime":"image/png","source":{"type":"inline"}}]}).to_string()])?;
    drop(connection);
    let adapter = OpenCode::with_db(raw.clone());
    let reference = adapter.discover()?.into_iter().find(|r| r.id == "ses_raw").unwrap();
    let mut session = crate::emit::parse_for_emit(&adapter, &reference, Harness::OpenCode)?;
    assert!(session.messages[0]
        .content
        .iter()
        .all(|b| !matches!(b, Block::Text { .. } | Block::ToolUse { .. })));
    assert!(matches!(
        &session.messages[0].complete_record.as_ref().unwrap().value,
        crate::Json::Source(_)
    ));
    // Edited fallback still owes the retained native source guard.
    session.messages[0].role = Role::System;
    crate::harness::store::sqlite::export_tests::rollback_record_only_source(&session, raw, |session, connection| {
        super::v2::write_database(
            session,
            &EmitOptions {
                thinking: ThinkingMode::Drop,
                ..Default::default()
            },
            "ses_checked_raw",
            root.to_str().unwrap(),
            connection,
        )
    })?;
    Ok(())
}

#[test]
fn complete_legacy_producers_match_owned_preparation() -> Result<()> {
    let world = crate::test_world("opencode-legacy");
    let root = world.proj.clone();
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sqlite = root.join("source.db");
    fs::copy(
        corpus.join("tests/corpus/opencode/1.18.20/.local/share/opencode/opencode.db"),
        &sqlite,
    )?;
    let adapters = [
        OpenCode::with_db(sqlite),
        OpenCode::with_root(corpus.join("tests/fixtures/opencode/parts_gen")),
        OpenCode::with_root(corpus.join("tests/fixtures/opencode/summary_gen")),
    ];
    for (kind, adapter) in adapters.iter().enumerate() {
        for reference in adapter.discover()? {
            let checked = crate::emit::parse_for_emit(adapter, &reference, Harness::OpenCode)?;
            let owned = crate::stream::collect_with(adapter, &reference, &Demand::COMPLETE)?;
            assert_eq!(
                serde_json::to_value(&checked)?,
                serde_json::to_value(&owned)?,
                "complete legacy producer {kind}/{}",
                reference.id
            );
            for mode in ThinkingMode::ALL {
                let options = EmitOptions {
                    new_id: Some("ses_legacy_oracle".into()),
                    thinking: mode,
                    ..Default::default()
                };
                let a = emit(
                    &checked,
                    &root.join(format!("{kind}-{}-{mode}-checked", reference.id)),
                    &options,
                )?;
                let b = emit(
                    &owned,
                    &root.join(format!("{kind}-{}-{mode}-owned", reference.id)),
                    &options,
                )?;
                assert_eq!(
                    logical_rows(&Connection::open(&a.path)?, &a.new_id)?,
                    logical_rows(&Connection::open(&b.path)?, &b.new_id)?,
                    "legacy native output {kind}/{}",
                    reference.id
                );
            }
        }
    }
    Ok(())
}
