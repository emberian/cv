//! Frozen wire and fidelity oracles for the Grok and Qwen emitters; `$HOME` and generated Qwen
//! UUIDs normalize.
use super::frozen::Frozen;
use super::*;
use crate::harness::Adapter;
use crate::ir::{GitInfo, Message, Origin, Usage};
use crate::stream::Demand;
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture(harness: Harness) -> Session {
    let mut session = Session::new("source", harness);
    let timestamp = "2026-10-12T12:00:00Z".parse().unwrap();
    session.created_at = Some(timestamp);
    session.updated_at = Some(timestamp + chrono::Duration::seconds(2));
    session.cwd = Some("/synthetic/grok qwen λ".into());
    session.title = Some("title \"λ\"".into());
    session.model = Some("foreign-model".into());
    session.git = Some(GitInfo {
        branch: Some("branch".into()),
        commit: Some("commit".into()),
        remote: Some("remote".into()),
        ..Default::default()
    });
    session.lineage.forked_from = Some("parent-session".into());
    session
        .harness_extra_mut(Harness::Qwen)
        .insert("version".into(), json!("0.25.1"));
    let text = |value: &str| Block::from_text(value.into());
    let mut system = Message::new(Role::System);
    system.kind = crate::MessageKind::SystemPrompt;
    system.content.push(text("system"));
    let mut user = Message::new(Role::User);
    user.content = vec![
        text(""),
        text("prompt\nλ"),
        text(""),
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("file:image".into()),
            extensions: Default::default(),
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("path".into()),
            source: Some("https://source".into()),
            extensions: Default::default(),
        },
    ];
    let mut assistant = Message::new(Role::Assistant);
    assistant.model = Some("grok-message-model".into());
    assistant.usage = Some(Usage {
        input_tokens: Some(10),
        output_tokens: Some(3),
        cache_read_tokens: Some(4),
        reasoning_tokens: Some(2),
        ..Default::default()
    });
    assistant.content = vec![
        Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: Some("provider-bound".into()),
            redacted: true,
            extensions: Default::default(),
        },
        Block::Thinking {
            text: "reasoning λ\n".into(),
            signature: Some("sig".into()),
            encrypted: Some("second-blob".into()),
            redacted: false,
            extensions: Default::default(),
        },
        Block::Thinking {
            text: " ".into(),
            signature: Some("blank-signature".into()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        },
        text("answer \"\\\t"),
        text(""),
        Block::ToolUse {
            id: "linked".into(),
            name: "Read".into(),
            namespace: Some("tool_call".into()),
            input: json!({"β":[1,true,null,{"float":-0.0}]}).into(),
            extensions: Default::default(),
        },
        Block::ToolUse {
            id: "second".into(),
            name: "Run".into(),
            namespace: None,
            input: json!({"command":"printf λ"}).into(),
            extensions: Default::default(),
        },
        Block::Image {
            media_type: None,
            data_ref: None,
            extensions: Default::default(),
        },
        Block::File {
            mime: None,
            path: Some("fallback-path".into()),
            source: None,
            extensions: Default::default(),
        },
    ];
    let mut tool = Message::new(Role::Tool);
    tool.content = vec![
        text("not emitted"),
        Block::ToolResult {
            tool_use_id: "linked".into(),
            content: "failed λ\n".into(),
            is_error: true,
            tool_name: Some("Read".into()),
            status: Some("failed".into()),
            details: None,
            extensions: Default::default(),
        },
        Block::ToolResult {
            tool_use_id: "second".into(),
            content: "success".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
            extensions: Default::default(),
        },
    ];
    let mut boundary = Message::new(Role::System);
    boundary.kind = crate::MessageKind::CompactionBoundary;
    boundary.harness_extra_mut(Harness::Qwen).extend([
        ("subtype".into(), json!("old-compression")),
        ("tokens".into(), json!(17)),
        ("detail".into(), json!({"x":["y",true]})),
    ]);
    let mut summary = Message::new(Role::User);
    summary.kind = crate::MessageKind::CompactionSummary;
    summary.content.push(text("summary\n"));
    let mut change = Message::new(Role::System);
    change.kind = crate::MessageKind::ModelChange;
    change.model = Some("new-model".into());
    let mut error = Message::new(Role::System);
    error.kind = crate::MessageKind::Error;
    error.content = vec![text(""), text("failure"), text("")];
    let mut notice = Message::new(Role::System);
    notice.content = vec![text(""), text("notice"), text("")];
    let mut branch = Message::new(Role::System);
    branch.kind = crate::MessageKind::Branch;
    let mut injected = Message::new(Role::User);
    injected.kind = crate::MessageKind::InjectedContext;
    injected.origin = Origin::Scheduler;
    injected
        .harness_extra_mut(Harness::Qwen)
        .insert("subtype".into(), json!("original-subtype"));
    injected.content.push(text("injected"));
    let mut empty = Message::new(Role::Assistant);
    empty.content.push(text(""));
    session.messages = vec![
        system,
        user,
        assistant,
        tool,
        boundary,
        summary,
        change,
        error,
        notice,
        branch,
        injected,
        empty,
        Message::new(Role::System),
    ];
    for message in &mut session.messages {
        message.timestamp = Some(timestamp);
    }
    session
}

/// The producer's wire, before any normalization: compact, key-ordered, newline-terminated JSONL.
fn assert_compact_qwen_wire(bytes: &[u8]) -> Result<()> {
    if bytes == b"\n" {
        return Ok(());
    }
    anyhow::ensure!(bytes.ends_with(b"\n"), "JSONL terminator");
    for line in std::str::from_utf8(bytes)?.lines() {
        let row: Value = serde_json::from_str(line)?;
        assert_eq!(
            serde_json::to_string(&row)?,
            line,
            "compact wire/order before normalization"
        );
    }
    Ok(())
}

fn normalized_qwen(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes == b"\n" {
        return Ok(bytes.to_vec());
    }
    anyhow::ensure!(bytes.ends_with(b"\n"), "JSONL terminator");
    let mut output = Vec::new();
    let mut prior = None;
    let mut prior_normalized = None;
    for (index, line) in std::str::from_utf8(bytes)?.lines().enumerate() {
        let mut row: Value = serde_json::from_str(line)?;
        let uuid = row["uuid"].as_str().unwrap().to_owned();
        assert_eq!(Uuid::parse_str(&uuid)?.get_version(), Some(uuid::Version::Random));
        assert_eq!(row["parentUuid"], json!(prior));
        row["uuid"] = json!(format!("generated-{index}"));
        row["parentUuid"] = json!(prior_normalized);
        prior = Some(uuid);
        prior_normalized = Some(format!("generated-{index}"));
        output.extend(serde_json::to_vec(&row)?);
        output.push(b'\n');
    }
    Ok(output)
}

/// One emitted session, frozen: each output file's length and SHA-256 (`$HOME` and Qwen UUIDs normalized)
/// and the SHA-256 of the fidelity report from re-reading it.
fn entry(target: Harness, result: &EmitResult, input: &Session, mode: ThinkingMode) -> Result<Value> {
    let mut files = serde_json::Map::new();
    // Grok's summary names the per-test `$HOME`; everything else is fixed by the fixture.
    let home = crate::env::var("HOME")?;
    let mut digest = |name: &str, bytes: Vec<u8>| {
        let bytes = String::from_utf8(bytes).unwrap().replace(&home, "$HOME");
        files.insert(
            name.into(),
            json!([bytes.len(), crate::digest::sha256_hex(bytes.as_bytes())]),
        );
    };
    if target == Harness::Qwen {
        let bytes = fs::read(&result.path)?;
        assert_compact_qwen_wire(&bytes)?;
        digest("session.jsonl", normalized_qwen(&bytes)?);
    } else {
        for name in ["summary.json", "chat_history.jsonl", "updates.jsonl"] {
            let path = result.path.join(name);
            if path.exists() {
                digest(name, fs::read(path)?);
            }
        }
    }
    let reread = fidelity::reparse_emitted(target, result, &Demand::RICH)?;
    let report = serde_json::to_vec(&fidelity::diff_fidelity(target, mode, input, &reread))?;
    Ok(json!({ "files": files, "fidelity": crate::digest::sha256_hex(&report) }))
}

/// Emit `session` and record its frozen entry under `label`.
fn compare(frozen: &mut Frozen, session: &Session, target: Harness, mode: ThinkingMode, base: &Path) -> Result<()> {
    let opts = EmitOptions {
        new_id: Some("fixed-session".into()),
        thinking: mode,
        ..Default::default()
    };
    let mut owned = session.clone();
    owned.materialize()?;
    let (new, report) = emit_report(session, target, &base.join("new"), &opts)?;
    let actual = entry(target, &new, &owned, mode)?;
    let report = crate::digest::sha256_hex(&serde_json::to_vec(&report)?);
    assert_eq!(actual["fidelity"], report, "exact fidelity classification");
    let label = base.file_name().unwrap().to_string_lossy().into_owned();
    frozen.insert(label, actual);
    Ok(())
}

fn capture_bodies(session: &mut Session, base: &Path) -> Result<()> {
    fs::create_dir_all(base)?;
    let mut index = 0;
    for message in &mut session.messages {
        for block in &mut message.content {
            index += 1;
            let path = base.join(format!("{index}.json"));
            if let Some(text) = block.text_mut() {
                let raw = serde_json::to_vec(&text.inline_str().unwrap())?;
                fs::write(&path, &raw)?;
                // For tiny/empty leaves also force a captured Source while retaining strict acceptance.
                let json = crate::Json::source(&path, 0, raw.len() as u64)?;
                if let crate::Json::Source(source) = json {
                    *text = crate::lazy::Text::Source(crate::lazy::SourceText::new(source));
                }
            } else if let Block::ToolUse { input, .. } = block {
                let raw = serde_json::to_vec(input.resolve()?.as_ref())?;
                fs::write(&path, &raw)?;
                *input = crate::Json::source(&path, 0, raw.len() as u64)?;
            }
        }
    }
    Ok(())
}

#[test]
fn checked_grok_qwen_frozen_all_modes_owned_and_captured() -> Result<()> {
    let world = crate::test_world("checked-grok-qwen-wire");
    let mut frozen = Frozen::new("grok-qwen-fixture");
    for (target, source) in [
        (Harness::Grok, Harness::Grok),
        (Harness::Grok, Harness::Claude),
        (Harness::Qwen, Harness::Qwen),
        (Harness::Qwen, Harness::Claude),
    ] {
        let owned = fixture(source);
        let mut captured = owned.clone();
        capture_bodies(&mut captured, &world.base.join(format!("sources-{target}-{source}")))?;
        for mode in ThinkingMode::ALL {
            compare(
                &mut frozen,
                &owned,
                target,
                mode,
                &world.base.join(format!("owned-{target}-{source}-{mode}")),
            )?;
            compare(
                &mut frozen,
                &captured,
                target,
                mode,
                &world.base.join(format!("captured-{target}-{source}-{mode}")),
            )?;
        }
    }
    frozen.check()
}

#[test]
fn checked_grok_qwen_real_corpus_matches_frozen_all_modes() -> Result<()> {
    let world = crate::test_world("checked-grok-qwen-corpus");
    let mut frozen = Frozen::new("grok-qwen-corpus");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for (target, version, adapter) in [
        (
            Harness::Grok,
            "1.0.50",
            Box::new(crate::harness::grok::Grok::with_home(&root.join("grok/1.0.50/.grok"))) as Box<dyn Adapter>,
        ),
        (
            Harness::Qwen,
            "0.25.1",
            Box::new(crate::harness::qwen::Qwen::with_home(&root.join("qwen/0.25.1/.qwen"))) as Box<dyn Adapter>,
        ),
    ] {
        let refs = adapter.discover()?;
        assert!(!refs.is_empty(), "{target}/{version}");
        for reference in refs {
            let session = adapter.parse(&reference)?;
            for mode in ThinkingMode::ALL {
                compare(
                    &mut frozen,
                    &session,
                    target,
                    mode,
                    &world.base.join(format!("{target}-{}-{mode}", reference.id)),
                )?;
            }
        }
    }
    frozen.check()
}

#[test]
fn checked_grok_qwen_source_failure_preserves_every_destination() -> Result<()> {
    let world = crate::test_world("checked-grok-qwen-failure");
    for target in [Harness::Grok, Harness::Qwen] {
        for mode in ThinkingMode::ALL {
            for variant in ["text", "thinking", "argument", "omitted"] {
                let base = world.base.join(format!("{target}-{mode}-{variant}"));
                fs::create_dir_all(&base)?;
                let out = base.join("out");
                let mut session = fixture(Harness::Claude);
                let opts = EmitOptions {
                    new_id: Some("fixed".into()),
                    thinking: mode,
                    ..Default::default()
                };
                let existing = emit(&session, target, &out, &opts)?;
                let files = if target == Harness::Grok {
                    vec![
                        existing.path.join("summary.json"),
                        existing.path.join("chat_history.jsonl"),
                        existing.path.join("updates.jsonl"),
                    ]
                } else {
                    vec![existing.path]
                };
                for file in &files {
                    fs::write(file, b"existing destination")?;
                }
                let path = base.join("source.json");
                let raw = if variant == "argument" {
                    serde_json::to_vec(&json!({"body":"guarded λ\n".repeat(10000)}))?
                } else {
                    serde_json::to_vec(&"guarded λ\n".repeat(10000))?
                };
                fs::write(&path, &raw)?;
                let json = crate::Json::source(&path, 0, raw.len() as u64)?;
                let mut message = Message::new(if variant == "omitted" {
                    Role::System
                } else {
                    Role::Assistant
                });
                message.content.push(if variant == "argument" {
                    Block::ToolUse {
                        id: "guarded".into(),
                        name: "Read".into(),
                        namespace: None,
                        input: json,
                        extensions: Default::default(),
                    }
                } else {
                    let text = json.with_view(|view| Ok(view.source_text()?.unwrap()))?;
                    if variant == "thinking" {
                        Block::Thinking {
                            text,
                            signature: None,
                            encrypted: None,
                            redacted: false,
                            extensions: Default::default(),
                        }
                    } else {
                        Block::from_text(text)
                    }
                });
                if variant == "omitted" {
                    message.kind = crate::MessageKind::SystemPrompt;
                }
                session.messages.push(message);
                fs::write(&path, b"changed source")?;
                assert!(
                    emit(&session, target, &out, &opts).is_err(),
                    "{target}/{mode}/{variant}"
                );
                for file in files {
                    assert_eq!(fs::read(file)?, b"existing destination");
                }
                let missing = base.join("missing-output");
                assert!(emit(&session, target, &missing, &opts).is_err());
                assert!(!missing.exists());
            }
        }
    }
    Ok(())
}

fn grok_qwen_adapters(root: &Path) -> Vec<(Harness, Box<dyn Adapter>)> {
    vec![
        (
            Harness::Grok,
            Box::new(crate::harness::grok::Grok::with_home(&root.join("grok/1.0.50/.grok"))) as Box<dyn Adapter>,
        ),
        (
            Harness::Qwen,
            Box::new(crate::harness::qwen::Qwen::with_home(&root.join("qwen/0.25.1/.qwen"))) as Box<dyn Adapter>,
        ),
    ]
}

/// The checked corpus producers re-emit: selection matches the complete read, per mode, frozen.
#[test]
fn checked_grok_qwen_corpus_producer_matches_frozen() -> Result<()> {
    let world = crate::test_world("grok-qwen-selected-corpus");
    let mut frozen = Frozen::new("grok-qwen-selected-corpus");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for (target, adapter) in grok_qwen_adapters(&root) {
        for reference in adapter.discover()? {
            let owned = crate::stream::collect_with(adapter.as_ref(), &reference, &Demand::COMPLETE)?;
            let selected = parse_for_emit(adapter.as_ref(), &reference, target)
                .map_err(|error| anyhow::anyhow!("{target} {}: {error:#}", reference.path.display()))?;
            let mut materialized = selected.clone();
            materialized.materialize()?;
            assert_eq!(
                serde_json::to_value(&owned)?,
                serde_json::to_value(&materialized)?,
                "complete producer {target}"
            );
            for mode in ThinkingMode::ALL {
                compare(
                    &mut frozen,
                    &selected,
                    target,
                    mode,
                    &world.base.join(format!("corpus-{target}-{}-{mode}", reference.id)),
                )?;
            }
        }
    }
    frozen.check()
}

/// The large-source producer: selection matches the complete read across a malformed JSONL tail
/// and a whitespace-wrapped record, per mode, frozen; a truncated source refuses to re-emit.
#[test]
fn checked_grok_qwen_large_source_matches_frozen_and_rejects_truncation() -> Result<()> {
    let world = crate::test_world("grok-qwen-selected-large");
    let mut frozen = Frozen::new("grok-qwen-selected-large");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for (target, adapter) in grok_qwen_adapters(&root) {
        let mut session = fixture(target);
        session.messages[2]
            .content
            .insert(0, Block::from_text("body \" λ\n".repeat(1024 * 128).into()));
        for block in &mut session.messages[2].content {
            match block {
                Block::ToolUse { input, .. } => {
                    *input = serde_json::json!({"payload": "argument λ\n".repeat(128 * 1024)}).into()
                }
                Block::Thinking { text, .. } if !text.is_empty() => *text = "reasoning λ\n".repeat(128 * 1024).into(),
                _ => {}
            }
        }
        for block in &mut session.messages[3].content {
            if let Block::ToolResult { content, .. } = block {
                *content = "result λ\n".repeat(128 * 1024).into();
            }
        }
        let result = emit(
            &session,
            target,
            &world.base.join(format!("source-{target}")),
            &EmitOptions {
                new_id: Some("large-source".into()),
                ..Default::default()
            },
        )?;
        let reference = SessionRef::new("large-source", target, &result.path);
        let wire = adapter.resume_source(&reference);
        use std::io::Write;
        let mut append = std::fs::OpenOptions::new().append(true).open(&wire)?;
        append.write_all(b"\ninvalid-json\n\xff\n{\"n\":1e999}\n42\n")?;
        let unicode = if target == Harness::Grok {
            "\u{2003}{\"type\":\"user\",\"content\":\"unicode wrapper\"}\u{2003}\n"
        } else {
            "\u{2003}{\"type\":\"user\",\"message\":{\"parts\":[{\"text\":\"unicode wrapper\"}]}}\u{2003}\n"
        };
        append.write_all(unicode.as_bytes())?;
        drop(append);
        let owned = crate::stream::collect_with(adapter.as_ref(), &reference, &Demand::COMPLETE)?;
        let selected = parse_for_emit(adapter.as_ref(), &reference, target)
            .map_err(|error| anyhow::anyhow!("{target} {}: {error:#}", reference.path.display()))?;
        assert!(
            selected.messages.iter().flat_map(|m| &m.content).any(|b| matches!(
                b,
                Block::Text {
                    text: crate::Text::Source(_),
                    ..
                }
            )),
            "checked producer retains body {target}"
        );
        let mut materialized = selected.clone();
        materialized.materialize()?;
        assert_eq!(serde_json::to_value(&owned)?, serde_json::to_value(&materialized)?);
        for mode in ThinkingMode::ALL {
            compare(
                &mut frozen,
                &selected,
                target,
                mode,
                &world.base.join(format!("large-{target}-{mode}")),
            )?;
        }
        let wire = adapter.resume_source(&reference);
        // Truncate to invalidate the selected source; append-only growth is permitted.
        std::fs::write(&wire, b"")?;
        let absent = world.base.join(format!("changed-{target}"));
        assert!(emit(&selected, target, &absent, &EmitOptions::default()).is_err());
        assert!(!absent.exists());
    }
    frozen.check()
}

/// Grok's three files are one publication: a claim that fails after another has been made (a
/// directory holds `updates.jsonl` under `--force`, `summary.json` without it) leaves every
/// name as it was.
#[test]
fn grok_files_publish_together_or_not_at_all() -> Result<()> {
    let world = crate::test_world("grok-emit-set");
    let session = fixture(Harness::Claude);
    for (force, blocked) in [(true, "updates.jsonl"), (false, "summary.json")] {
        let out = world.base.join(format!("out-{force}"));
        let opts = EmitOptions {
            new_id: Some("fixed".into()),
            force,
            ..Default::default()
        };
        let dir = emit(&session, Harness::Grok, &out, &opts)?.path;
        for name in ["summary.json", "chat_history.jsonl", "updates.jsonl"] {
            fs::remove_file(dir.join(name))?;
            if force {
                fs::write(dir.join(name), "existing destination")?;
            }
        }
        if force {
            fs::remove_file(dir.join(blocked))?;
        }
        fs::create_dir(dir.join(blocked))?;
        assert!(emit(&session, Harness::Grok, &out, &opts).is_err());
        let mut left: Vec<_> = fs::read_dir(&dir)?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<_>>()?;
        left.sort();
        if force {
            assert_eq!(left, ["chat_history.jsonl", "summary.json", "updates.jsonl"]);
            assert_eq!(fs::read(dir.join("chat_history.jsonl"))?, b"existing destination");
            assert_eq!(fs::read(dir.join("summary.json"))?, b"existing destination");
        } else {
            assert_eq!(left, ["summary.json"], "only the blocking directory");
        }
    }
    Ok(())
}
