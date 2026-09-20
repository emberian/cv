//! The OpenSession adapter: the document boundary, the real-data round trip, the 0.2 tolerance
//! rules, and discovery over registered sources.

use super::*;
use crate::harness::{claude, codex, Adapter};
use serde_json::json;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cv-opensession-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn block_type(b: &Block) -> &'static str {
    match b {
        Block::Text { .. } => "text",
        Block::Thinking { .. } => "thinking",
        Block::ToolUse { .. } => "tool_use",
        Block::ToolResult { .. } => "tool_result",
        Block::Image { .. } => "image",
        Block::File { .. } => "file",
    }
}

// ── the document boundary ─────────────────────────────────────────────────────

/// The version marker is the document's FIRST key — and it is *only* on the document. A `Session`
/// serialized on its own (what `cv show --json`, the MCP payloads and cvd emit) has no
/// `open_session` key, which is the whole reason the marker lives at this boundary.
#[test]
fn document_leads_with_the_version_and_session_never_carries_it() {
    let s = Session {
        id: "s1".into(),
        harness: Harness::Claude,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        system_prompt: None,
        lineage: Lineage::default(),
        messages: vec![Message::new(Role::User)],
        source_path: None,
        extra: Map::new(),
    };
    let doc = serde_json::to_string_pretty(&document(&s)).unwrap();
    assert!(
        doc.starts_with("{\n  \"open_session\": \"0.3\","),
        "the version marker leads the document:\n{doc}"
    );
    assert_eq!(VERSION, "0.3", "docs/OPENSESSION.md is at 0.3");
    let bare = serde_json::to_value(&s).unwrap();
    assert!(
        bare.get("open_session").is_none(),
        "a raw Session must NOT grow the marker: {bare}"
    );
}

// ── the decisive test: a round trip on real data ──────────────────────────────

/// The newest real local session with a workable size, from whichever harness has one.
fn a_real_session() -> Option<(&'static str, Session)> {
    let adapters: Vec<(&'static str, Box<dyn Adapter>)> = vec![
        ("claude", Box::new(claude::Claude::new())),
        ("codex", Box::new(codex::Codex::new())),
    ];
    for (name, a) in adapters {
        if a.storage_root().is_none() {
            continue;
        }
        let Ok(mut refs) = a.discover() else { continue };
        refs.retain(|r| (5..=250).contains(&r.message_count));
        refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
        for r in refs.iter().take(4) {
            if let Ok(s) = a.parse(r) {
                if s.messages.len() >= 5 {
                    return Some((name, s));
                }
            }
        }
    }
    None
}

/// **Export a real session to OpenSession, read it back, and compare the two IRs field by field.**
///
/// Everything is asserted equal except one field, and it is not a loss: `source_path` is "where cv
/// read this session from on disk", so for the re-read session it is the document — naming the
/// original transcript there would be a claim about a file this reader never opened. Every other
/// field, including the harness the document came from, survives.
///
/// Skips (loudly) when the machine has no local session to test with; there is nothing to fake
/// here, a synthetic fixture would only prove that the writer and reader agree with each other.
#[test]
fn round_trips_a_real_session_through_a_document() {
    let Some((harness_name, original)) = a_real_session() else {
        eprintln!("no local claude/codex session to round-trip — skipping (nothing to fake here)");
        return;
    };
    eprintln!(
        "round trip: {harness_name} session {} — {} messages, source {}",
        original.id,
        original.messages.len(),
        original
            .source_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );

    let dir = tmpdir("roundtrip");
    let path = dir.join(format!("{}.opensession.json", original.id));
    std::fs::write(&path, serde_json::to_string_pretty(&document(&original)).unwrap()).unwrap();

    // The written bytes really are an OpenSession 0.3 document.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["open_session"], "0.3");
    assert_eq!(raw["harness"], serde_json::to_value(original.harness).unwrap());

    // Read it back the way any other harness is read: through the adapter.
    let r = doc_ref(&path).expect("the document yields a ref");
    assert_eq!(r.harness, Harness::OpenSession, "the REF is tagged opensession");
    let back = OpenSession::new().parse(&r).expect("the document parses");

    // Session-level, field by field.
    assert_eq!(back.id, original.id, "id");
    assert_eq!(
        back.harness, original.harness,
        "harness (the document names its origin)"
    );
    assert_eq!(back.cwd, original.cwd, "cwd");
    assert_eq!(back.title, original.title, "title");
    assert_eq!(back.created_at, original.created_at, "created_at");
    assert_eq!(back.updated_at, original.updated_at, "updated_at");
    assert_eq!(back.model, original.model, "model");
    assert_eq!(
        serde_json::to_value(&back.git).unwrap(),
        serde_json::to_value(&original.git).unwrap(),
        "git"
    );
    assert_eq!(back.system_prompt, original.system_prompt, "system_prompt");
    assert_eq!(back.lineage, original.lineage, "lineage");
    assert_eq!(
        serde_json::to_value(&back.extra).unwrap(),
        serde_json::to_value(&original.extra).unwrap(),
        "session extra"
    );
    assert_eq!(
        back.cv_extra().and_then(|e| e.get("skipped_lines")),
        original.cv_extra().and_then(|e| e.get("skipped_lines")),
        "nothing was skipped on the way in that wasn't skipped on the way out"
    );

    // Messages, field by field.
    assert_eq!(back.messages.len(), original.messages.len(), "message count");
    for (i, (b, o)) in back.messages.iter().zip(&original.messages).enumerate() {
        assert_eq!(b.role, o.role, "message {i} role");
        assert_eq!(b.kind, o.kind, "message {i} kind");
        assert_eq!(b.origin, o.origin, "message {i} origin");
        assert_eq!(b.id, o.id, "message {i} id");
        assert_eq!(b.parent_id, o.parent_id, "message {i} parent_id");
        assert_eq!(b.timestamp, o.timestamp, "message {i} timestamp");
        assert_eq!(b.model, o.model, "message {i} model");
        assert_eq!(
            serde_json::to_value(&b.usage).unwrap(),
            serde_json::to_value(&o.usage).unwrap(),
            "message {i} usage"
        );
        assert_eq!(
            serde_json::to_value(&b.extra).unwrap(),
            serde_json::to_value(&o.extra).unwrap(),
            "message {i} extra"
        );
        let bt: Vec<&str> = b.content.iter().map(block_type).collect();
        let ot: Vec<&str> = o.content.iter().map(block_type).collect();
        assert_eq!(bt, ot, "message {i} block types");
        assert_eq!(
            serde_json::to_value(&b.content).unwrap(),
            serde_json::to_value(&o.content).unwrap(),
            "message {i} block contents"
        );
    }

    // The catch-all: everything, including anything the assertions above forgot to name. Only
    // `source_path` is realigned first, for the reason in this test's doc comment.
    assert_eq!(
        back.source_path.as_deref(),
        Some(path.as_path()),
        "source_path is the document"
    );
    let mut back_value = serde_json::to_value(&back).unwrap();
    back_value["source_path"] = serde_json::to_value(&original.source_path).unwrap();
    assert_eq!(
        back_value,
        serde_json::to_value(&original).unwrap(),
        "the whole IR, key for key"
    );

    // A census of what the round trip actually carried, for the record.
    let mut kinds: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut blocks: std::collections::BTreeMap<&str, usize> = Default::default();
    for m in &back.messages {
        *kinds.entry(m.kind.as_str()).or_default() += 1;
        for b in &m.content {
            *blocks.entry(block_type(b)).or_default() += 1;
        }
    }
    eprintln!("  kinds  {kinds:?}\n  blocks {blocks:?}");
    std::fs::remove_dir_all(&dir).ok();
}

// ── tolerance on the way in (spec principle 10) ───────────────────────────────

/// A document written by the PREVIOUS version still loads: `openSession`, camelCase session and
/// usage keys, blocks tagged `kind` with `toolUse`/`toolResult` and their camelCase fields.
#[test]
fn reads_a_0_2_document() {
    let doc = json!({
        "openSession": "0.2",
        "harness": "claude",
        "id": "old-1",
        "cwd": "/work/proj",
        "title": "an older document",
        "createdAt": "2026-05-29T21:00:00Z",
        "updatedAt": "2026-05-29T22:00:00Z",
        "model": "claude-opus-4-8",
        "systemPrompt": "be excellent",
        "lineage": { "forkedFrom": "older-0", "spawnedByToolUse": "call_0" },
        "messages": [
            { "id": "m1", "role": "human", "content": [{ "kind": "text", "text": "hi" }] },
            {
                "id": "m2", "parentId": "m1", "role": "assistant", "messageKind": "reply",
                "usage": { "inputTokens": 10, "outputTokens": 3, "cacheReadTokens": 7, "costUsd": 0.5 },
                "content": [
                    { "kind": "thinking", "text": "hmm", "signature": "sig" },
                    { "kind": "toolUse", "id": "call_1", "name": "Bash", "input": { "command": "ls" } },
                    { "kind": "image", "mediaType": "image/png", "dataRef": "ref-1" }
                ]
            },
            {
                "role": "tool",
                "content": [{ "kind": "toolResult", "toolUseId": "call_1", "content": "ok", "isError": true,
                              "toolName": "Bash" }]
            }
        ]
    });
    let s = from_str(&doc.to_string()).unwrap();

    assert_eq!(s.harness, Harness::Claude);
    assert_eq!(s.id, "old-1");
    assert_eq!(s.cwd.as_deref(), Some(Path::new("/work/proj")));
    assert_eq!(s.title.as_deref(), Some("an older document"));
    assert_eq!(s.created_at.unwrap().to_rfc3339(), "2026-05-29T21:00:00+00:00");
    assert_eq!(s.updated_at.unwrap().to_rfc3339(), "2026-05-29T22:00:00+00:00");
    assert_eq!(s.system_prompt.as_deref(), Some("be excellent"));
    assert_eq!(s.lineage.forked_from.as_deref(), Some("older-0"));
    assert_eq!(s.lineage.spawned_by_tool_use.as_deref(), Some("call_0"));

    // `human` is a user turn, and with no `kind` the role-implied default applies.
    assert_eq!(s.messages[0].role, Role::User);
    assert_eq!(s.messages[0].kind, MessageKind::Prompt);
    assert_eq!(s.messages[0].origin, Origin::Human);

    let a = &s.messages[1];
    assert_eq!(a.parent_id.as_deref(), Some("m1"));
    assert_eq!(a.kind, MessageKind::Reply, "messageKind is the 0.2 spelling of kind");
    let u = a.usage.as_ref().unwrap();
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cost_usd),
        (Some(10), Some(3), Some(7), Some(0.5))
    );
    assert!(matches!(&a.content[0], Block::Thinking { text, signature, .. }
        if text == "hmm" && signature.as_deref() == Some("sig")));
    assert!(matches!(&a.content[1], Block::ToolUse { id, name, input, .. }
        if id == "call_1" && name == "Bash" && input["command"] == "ls"));
    assert!(matches!(&a.content[2], Block::Image { media_type, data_ref }
        if media_type.as_deref() == Some("image/png") && data_ref.as_deref() == Some("ref-1")));

    let t = &s.messages[2];
    assert_eq!(t.role, Role::Tool);
    assert!(
        matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, .. }
        if tool_use_id == "call_1" && content == "ok" && *is_error && tool_name.as_deref() == Some("Bash"))
    );
}

/// Nothing out of vocabulary is fatal: an unknown block type, an unknown message kind and an
/// unknown origin all load, with the original spellings kept in cv's own namespace. A record that
/// is not a message at all is skipped and counted.
#[test]
fn unknown_vocabulary_is_carried_never_fatal() {
    let doc = json!({
        "open_session": "0.3",
        "harness": "claude",
        "id": "new-1",
        "messages": [
            {
                "role": "assistant",
                "kind": "hologram",
                "origin": "oracle",
                "content": [
                    { "type": "text", "text": "kept" },
                    { "type": "video", "src": "movie.mp4" }
                ]
            },
            "not a message at all",
            { "role": "user", "content": [{ "type": "text", "text": "fine" }] }
        ]
    });
    let s = from_str(&doc.to_string()).unwrap();

    assert_eq!(s.messages.len(), 2, "the junk record is skipped, the rest survive");
    let m = &s.messages[0];
    assert_eq!(
        m.kind,
        MessageKind::Reply,
        "an unknown kind falls back to the role's default"
    );
    assert_eq!(m.origin, Origin::Model, "so does an unknown origin");
    assert_eq!(
        m.content.len(),
        1,
        "the unknown block type is dropped, the known one kept"
    );
    let cv = m.extra.get(CV_NAMESPACE).expect("the spellings are remembered");
    assert_eq!(cv["unknown_kind"], "hologram");
    assert_eq!(cv["unknown_origin"], "oracle");
    assert_eq!(cv["unknown_blocks"], json!(["video"]));
    assert_eq!(
        s.cv_extra().unwrap()["skipped_lines"],
        json!(1),
        "the unreadable record is counted like any other adapter's corrupt line"
    );
    crate::harness::assert_no_flat_keys(&s);
}

/// A document that names no harness (or one this build has never heard of) is an `opensession`
/// session rather than a parse failure — and every harness cv itself writes is recognized, which
/// includes the IR's serde spelling (`claudeapp`) as well as the canonical name (`claude-app`).
#[test]
fn harness_names_round_trip_including_the_serde_spelling() {
    for h in Harness::ALL {
        let serde_name = serde_json::to_value(h).unwrap();
        let serde_name = serde_name.as_str().unwrap();
        assert_eq!(harness_from(serde_name), Some(h), "serde spelling {serde_name}");
        assert_eq!(harness_from(h.as_str()), Some(h), "canonical name {}", h.as_str());
    }
    assert_eq!(harness_from("chronovisor"), None);
    let s = from_str(&json!({ "id": "x", "messages": [] }).to_string()).unwrap();
    assert_eq!(s.harness, Harness::OpenSession);
}

/// Content that a store spelled as an Anthropic-style part array still reads as text.
#[test]
fn tool_result_content_shapes_are_flattened() {
    let b = block_from(&json!({
        "type": "tool_result", "tool_use_id": "c1",
        "content": [{ "type": "text", "text": "line one" }, { "type": "text", "text": "line two" }]
    }))
    .unwrap();
    assert!(matches!(&b, Block::ToolResult { content, .. } if content == "line one\nline two"));

    // A tool call missing its id is still a tool call (lossy but honest), not a dropped block.
    let b = block_from(&json!({ "type": "tool_use", "name": "Bash" })).unwrap();
    assert!(matches!(&b, Block::ToolUse { id, name, .. } if id.is_empty() && name == "Bash"));
}

#[test]
fn snake_normalizes_every_tag_spelling() {
    assert_eq!(snake("toolUse"), "tool_use");
    assert_eq!(snake("toolResult"), "tool_result");
    assert_eq!(snake("TOOL_RESULT"), "tool_result");
    assert_eq!(snake("InjectedContext"), "injected_context");
    assert_eq!(snake("compaction-boundary"), "compaction_boundary");
    assert_eq!(snake("text"), "text");
}

// ── discovery ─────────────────────────────────────────────────────────────────

/// Discovery over the registered export sources: a `*.opensession.json` is taken by name, a plain
/// `*.json` only when its head carries the version key, and anything else is left alone.
///
/// Mutates `$CV_EXPORTS`/`$XDG_CONFIG_HOME`, which is process-global — fine under nextest (a
/// process per test), and the reason this is the only test here that touches the environment.
#[test]
fn discovers_documents_in_registered_sources() {
    let dir = tmpdir("discover");
    let doc = json!({
        "open_session": "0.3", "harness": "codex", "id": "disco-1", "title": "found me",
        "messages": [
            { "role": "user", "kind": "prompt", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "kind": "reply", "content": [{ "type": "text", "text": "yo" }] },
            { "role": "tool", "kind": "tool_result", "content": [
                { "type": "tool_result", "tool_use_id": "c", "content": "out" }] }
        ]
    });
    std::fs::write(dir.join("disco-1.opensession.json"), doc.to_string()).unwrap();
    // A plain .json that IS a document (sniffed by its key), one that is not, and a non-JSON file.
    let mut by_key = doc.clone();
    by_key["id"] = json!("disco-2");
    std::fs::write(dir.join("some-session.json"), by_key.to_string()).unwrap();
    std::fs::write(dir.join("settings.json"), json!({ "theme": "dark" }).to_string()).unwrap();
    std::fs::write(dir.join("notes.txt"), "not json").unwrap();

    std::env::set_var("XDG_CONFIG_HOME", dir.join("no-config"));
    std::env::set_var("CV_EXPORTS", &dir);
    let refs = OpenSession::new().discover().unwrap();
    std::env::remove_var("CV_EXPORTS");
    std::env::remove_var("XDG_CONFIG_HOME");

    let mut ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["disco-1", "disco-2"], "by name and by sniff, and nothing else");
    let one = refs.iter().find(|r| r.id == "disco-1").unwrap();
    assert_eq!(one.harness, Harness::OpenSession);
    assert_eq!(one.title.as_deref(), Some("found me"));
    assert_eq!(one.message_count, 2, "conversational turns only: user + assistant");

    // And the ref parses into the session the document describes, with ITS harness.
    let s = OpenSession::new().parse(one).unwrap();
    assert_eq!(s.harness, Harness::Codex);
    assert_eq!(s.messages.len(), 3);
    std::fs::remove_dir_all(&dir).ok();
}
