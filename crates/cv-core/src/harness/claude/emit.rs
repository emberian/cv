//! The Claude emitter: an IR [`Session`] written as a Claude Code transcript that `claude --resume`
//! accepts — the Anthropic codec run backwards plus the JSONL writer, and the resumability rules
//! (linear parents, no orphan `tool_use`, sidechain identity) that make the result resumable,
//! checked by [`verify_claude_resumable`] on the staged file before it is published. Registered in
//! `crate::emit::emitter_for`; the shared skeleton is in [`crate::emit`].

use crate::emit::*;
use crate::harness::wire::anthropic;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, MessageKind, Role, Session};
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Resolve symlinks in `cwd` the way Claude Code does when it looks for sessions to `--resume`.
///
/// Claude derives a session's project dir from the *realpath* of the directory it's launched in, so
/// a session we write must be keyed off the resolved path or `claude --resume` reports "No
/// conversation found". This bites on macOS, where `/tmp` and `/var` are symlinks into `/private`:
/// porting a session whose cwd is `/tmp/foo` would write `-tmp-foo` while Claude, launched from
/// `/tmp/foo`, looks under `-private-tmp-foo`. Canonicalizing here keeps the two in lockstep and
/// also matches what Claude records in its own transcripts (it runs in the resolved dir).
///
/// Falls back to the path as-given when it can't be resolved — e.g. the target dir doesn't exist
/// yet — which preserves the prior behavior for those cases.
fn realpath_for_claude(cwd: &Path) -> PathBuf {
    fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Encode a cwd into Claude's project-dir name: leading `-`, then every non-ASCII-alphanumeric
/// character (`/`, `.`, `_`, spaces, `@`, `+`, `:`, `\\`, …) becomes `-`, matching Claude Code's
/// `sanitizeProjectName`.
pub(crate) fn claude_encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    // Strip a single leading '/' so the mandatory leading '-' isn't doubled.
    let s = s.strip_prefix('/').unwrap_or(&s);
    let mut out = String::with_capacity(s.len() + 1);
    out.push('-');
    for ch in s.chars() {
        out.push(if ch.is_ascii_alphanumeric() { ch } else { '-' });
    }
    out
}

mod stream;
/// Write `session` as a Claude transcript, applying the resumability rules Claude Code enforces,
/// once, to the records about to be written, so every IR→Claude door (port, distill, splice, pack)
/// gets them without knowing they exist and no selector has to re-thread, close calls or strip
/// markers itself:
///
/// 1. **No orphan `tool_use`.** A call whose `tool_result` is not in the file (the source stopped
///    mid-call, or the selector cut between them) is dropped; an assistant record left with no
///    content goes with it. The API rejects a resume with an unanswered call.
/// 2. **No dangling `parentUuid`.** Claude Code resumes by walking `parentUuid` back from the newest
///    record and stops silently at the first missing link, so a parent that names a record not in
///    the file (a dropped turn, a hook attachment the IR never carried, a span cut) is re-pointed at
///    the previous record. A parent that resolves is kept, so a source's branches survive.
/// 3. **One identity.** A sub-agent transcript carries `isSidechain: true` and its `agentId` on
///    every record; a main-thread session carries neither (`isSidechain` false where present), so a
///    span lifted from a sub-agent joins the thread instead of being skipped by the loader.
///
/// [`verify_claude_resumable`] checks the same three on a written file.
pub use stream::emit;

/// Check a written Claude transcript against the three rules [`emit`] establishes: no
/// dangling `parentUuid`, no `tool_use` without its `tool_result`, and one identity (`agent`
/// `None`: no record with `isSidechain: true`; `Some`: every record `isSidechain: true` with that
/// `agentId`). Every Claude emit runs it on its staged file before publishing; an error there is a
/// bug in cv, never in the input, and leaves nothing behind.
mod verify;
pub fn verify_claude_resumable(path: &Path, agent: Option<&str>) -> Result<()> {
    verify::verify(path, agent)
}

/// Check a reshape's staged `output` (prune, rewind, adopt) before it is published as
/// `destination`: it keeps [`verify_claude_resumable`]'s rules except where `source`'s captured
/// extent already breaks them the same way. An error names `destination`'s record.
pub(crate) fn verify_derived(
    source: &crate::json::JsonSource,
    file: &fs::File,
    source_agent: Option<&str>,
    output: &Path,
    destination: &Path,
    agent: Option<&str>,
) -> Result<()> {
    verify::verify_derived(source, file, source_agent, output, destination, agent)
}

pub(crate) fn verify_carried(
    sources: &[(
        &crate::json::JsonSource,
        &fs::File,
        crate::harness::store::jsonl::ranges::Range,
    )],
    joined_tail: Option<(
        &crate::json::JsonSource,
        &fs::File,
        crate::harness::store::jsonl::ranges::Range,
    )>,
    source_agent: Option<&str>,
    output: &Path,
    destination: &Path,
    agent: Option<&str>,
) -> Result<()> {
    verify::verify_carried(sources, joined_tail, source_agent, output, destination, agent)
}

/// The verbatim meta record a format-complete carrier message stashed (an object under
/// `extra["_record"]`), or `None` for an ordinary message. See
/// [`claude::carrier_record`](crate::ir::CARRIER_KEY).
fn carrier_record_of(m: &crate::ir::Message) -> Option<&Value> {
    m.extra.get(crate::ir::CARRIER_KEY).filter(|v| v.is_object())
}

/// What the Claude writer does with one message, decided here once for the writer and for the
/// fidelity report ([`crate::emit::fidelity::WriteRules::system_fate`]), so what is reported as
/// not written is exactly what was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemFate {
    /// A user, assistant or tool turn: written as a turn.
    Turn,
    /// The message keeps its Claude record ([`crate::ir::CARRIER_KEY`]): replayed verbatim.
    Record,
    /// A foreign session's message of a kind Claude Code has a record for (a compaction
    /// boundary or summary, injected context): written as that record.
    Special(MessageKind),
    /// Not written, and why. A system-role message is an `attachment` or `system` record in a
    /// Claude transcript, and the IR holds only its text: without the record there is nothing
    /// to write it back as.
    Unwritten(&'static str),
}

/// [`SystemFate`] of `message` in a session read from `source`.
pub(crate) fn system_fate(source: Harness, message: &crate::ir::Message) -> SystemFate {
    if carrier_record_of(message).is_some() {
        return SystemFate::Record;
    }
    if source != Harness::Claude
        && matches!(
            message.kind,
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary | MessageKind::InjectedContext
        )
    {
        return SystemFate::Special(message.kind);
    }
    if message.role != Role::System {
        return SystemFate::Turn;
    }
    SystemFate::Unwritten(if source == Harness::Claude {
        "read without its record; a read that keeps records carries it"
    } else {
        "the source harness's record has no Claude form"
    })
}

/// Internal `extra` keys the IR adds for its own bookkeeping — never part of the source record, so
/// they must NOT be replayed into an emitted Claude line.
fn is_internal_extra_key(k: &str) -> bool {
    k == crate::ir::CARRIER_KEY || k == crate::offsets::OFFSET_KEY
}

/// Reconstruct Claude's `message.usage` object from the IR [`Usage`], reversing
/// [`claude::parse_usage`]'s field renames (`cache_read_input_tokens` ↔ `cache_read_tokens`,
/// `cache_creation_input_tokens` ↔ `cache_creation_tokens`). Emits only the fields actually present,
/// so a source that carried just `input_tokens`/`output_tokens` round-trips with the same key set.
/// `None` when there's no usage at all (an assistant turn the source had no `usage` block on).
fn claude_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    if let Some(x) = u.input_tokens {
        m.insert("input_tokens".into(), json!(x));
    }
    if let Some(x) = u.output_tokens {
        m.insert("output_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_read_tokens {
        m.insert("cache_read_input_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_creation_tokens {
        m.insert("cache_creation_input_tokens".into(), json!(x));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

// ------------------------------------------------------------------------------------------------
// shared by the emitters (each lives beside its reader: `harness/<h>/emit.rs`)
// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod checked_verifier_tests {
    use super::*;
    #[test]
    fn resumability_verdicts_and_messages_are_the_recorded_ones() -> Result<()> {
        let world = cv_testkit::World::new("claude-verifier");
        let path = world.base.join("output.jsonl");
        let nested = format!("{}0{}", "[".repeat(140), "]".repeat(140));
        let cases = vec![
            "\n \t\n\u{2003}\n".to_string(),
            r#"{"uuid":"a","uuid":"b","parentUuid":"b"}"#.into(),
            r#"{"uuid":"a","message":{"content":[{"type":"tool_use","id":"c"}]} }
{"uuid":"a","parentUuid":"a","message":{"content":[{"type":"tool_result","tool_use_id":"c"}]}}"#
                .replace("\\n", "\n"),
            r#"{"uuid":"a","message":{"content":[{"type":"tool_use","id":"missing"}],"content":[]}}"#.into(),
            r#"{"parentUuid":"missing"}"#.into(),
            r#"{"message":{"content":[{"type":"tool_use","id":"missing"}]}}"#.into(),
            r#"{"isSidechain":true,"agentId":"agent"}"#.into(),
            r#"{"isSidechain":false,"agentId":17}"#.into(),
            r#"{"ignored":"\ud800"}"#.into(),
            r#"{"ignored":1e400}"#.into(),
            format!("{{\"ignored\":{nested}}}"),
            "\u{2003}{\"uuid\":\"a\"}".into(),
            "true\n17\n[]\nnull".into(),
        ];
        let mut frozen = crate::emit::frozen::Frozen::new("claude-verifier");
        for (n, case) in cases.into_iter().enumerate() {
            fs::write(&path, &case)?;
            for agent in [None, Some("agent")] {
                let new = verify_claude_resumable(&path, agent);
                let verdict = match &new {
                    Ok(()) => "ok".to_owned(),
                    Err(error) => error.to_string().replace(path.to_str().unwrap(), "<path>"),
                };
                frozen.insert(format!("{n:02} {agent:?}"), json!(verdict));
            }
        }
        frozen.check()?;
        Ok(())
    }

    /// A reshape's copy is held to its source: what the source already breaks, a parent the
    /// source holds before the copy starts, a dangling parent off the resume walk and a request
    /// blob's blocks are no break; a link the copy drops from its own range is.
    #[test]
    fn a_derived_copy_may_only_keep_its_sources_breaks() -> Result<()> {
        let world = cv_testkit::World::new("claude-derived-verifier");
        let (source, output) = (world.base.join("source.jsonl"), world.base.join("output.jsonl"));
        let check = |src: &str, out: &str| -> Result<()> {
            fs::write(&source, src)?;
            fs::write(&output, out)?;
            let captured = crate::json::JsonSource::capture(&source)?;
            verify_derived(&captured, &fs::File::open(&source)?, None, &output, &output, None)
        };
        let u = |id: &str, parent: &str| format!(r#"{{"type":"user","uuid":"{id}","parentUuid":"{parent}"}}"#);
        let call =
            r#"{"type":"assistant","uuid":"c","parentUuid":"b","message":{"content":[{"type":"tool_use","id":"t"}]}}"#;
        let blob = r#"{"type":"api-request-blob","message":{"content":[{"type":"tool_use","id":"t2"}]}}"#;
        // Inherited: the continuation's parent and the still-running call.
        let src = format!("{}\n{}\n{call}\n", u("a", "elsewhere"), u("b", "a"));
        check(&src, &src)?;
        // The copy starts past `a`; `x` hangs off a parent written later; a blob is no turn.
        let src = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            u("a", "root"),
            u("b", "a"),
            u("x", "z"),
            u("c", "b"),
            u("z", "c")
        );
        let src = src.replacen(r#""parentUuid":"root""#, r#""parentUuid":null"#, 1);
        check(
            &src,
            &format!("{}\n{}\n{}\n{blob}\n", u("b", "a"), u("x", "z"), u("c", "b")),
        )?;
        // Dropped from the copy's own range: the walk from `c` stops at `b`.
        let err = check(&src, &format!("{}\n{}\n", u("a", "root"), u("c", "b"))).unwrap_err();
        assert!(err.to_string().contains(":2: parentUuid b names no record"), "{err}");
        // A call the source answers.
        let answered = format!(
            "{call}\n{}\n",
            r#"{"type":"user","uuid":"r","parentUuid":"c","message":{"content":[{"type":"tool_result","tool_use_id":"t"}]}}"#
        );
        let err = check(&answered, &format!("{call}\n")).unwrap_err();
        assert!(
            err.to_string().contains("tool_use t has no tool_result; the source"),
            "{err}"
        );
        Ok(())
    }

    /// A system-role message that keeps its record is written back as that record, restamped
    /// under every spelling of the session id. One without is named by kind with the writer's
    /// reason, never folded into a count the reader is told to expect.
    #[test]
    fn a_system_message_is_replayed_from_its_record_or_named_as_unwritten() -> Result<()> {
        use crate::ir::{Message, Origin, CARRIER_KEY};
        let world = cv_testkit::World::new("claude-system-fate");
        let text = |m: &mut Message, id: &str, parent: Option<&str>, text: &str| {
            m.id = Some(id.into());
            m.parent_id = parent.map(str::to_owned);
            m.content.push(Block::from_text(text.into()));
        };
        let mut session = Session::new("source", Harness::Claude);
        let mut prompt = Message::new(Role::User);
        text(&mut prompt, "u", None, "plan the beds");
        let mut queued = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness);
        text(&mut queued, "k", Some("u"), "also water the ferns");
        queued.extra.insert(
            CARRIER_KEY.into(),
            json!({"type": "attachment", "uuid": "k", "parentUuid": "u", "sessionId": "source", "session_id": "source",
                "attachment": {"type": "queued_command", "prompt": "also water the ferns", "origin": {"kind": "human"}},
                "rendered": [{"content": "also water the ferns"}], "renderedRole": "user"}),
        );
        let mut notice = Message::of_kind(Role::System, MessageKind::Notice, Origin::Harness);
        text(&mut notice, "n", Some("k"), "compost turned");
        let mut reply = Message::new(Role::Assistant);
        text(&mut reply, "a", Some("n"), "done");
        session.messages = vec![prompt, queued, notice, reply];
        let opts = EmitOptions {
            new_id: Some("derived".into()),
            ..Default::default()
        };
        let (result, report) = crate::emit::emit_report(&session, Harness::Claude, &world.base.join("claude"), &opts)?;
        let rows: Vec<Value> = fs::read_to_string(&result.path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        let types: Vec<&str> = rows.iter().filter_map(|r| r["type"].as_str()).collect();
        assert_eq!(types, ["user", "attachment", "assistant"]);
        assert_eq!(rows[1]["attachment"]["origin"]["kind"], "human");
        assert_eq!(rows[1]["renderedRole"], "user");
        assert_eq!(
            (&rows[1]["sessionId"], &rows[1]["session_id"]),
            (&json!("derived"), &json!("derived"))
        );
        assert_eq!(rows[2]["parentUuid"], "k", "relinked past the unwritten notice");
        let stated: Vec<_> = report
            .deltas
            .iter()
            .filter(|d| d.what.starts_with("system:") || d.what.starts_with("role:") || d.what.starts_with("kind:"))
            .collect();
        assert_eq!(stated.len(), 1, "{:?}", report.deltas);
        assert_eq!(
            stated[0].line(),
            "system:notice: 1 → 0 (read without its record; a read that keeps records carries it)"
        );
        // Its text is still a loss the format could have held.
        assert!(
            report
                .deltas
                .iter()
                .any(|d| d.what == "block:text" && !d.expected && d.a == "4" && d.b == "3"),
            "{:?}",
            report.deltas
        );

        // Without its record the mid-turn message is unwritten too, and says so; nothing here
        // is the old `role:system: N → 0 (expected: target format policy)`.
        session.messages[1].extra.remove(CARRIER_KEY);
        let opts = EmitOptions {
            new_id: Some("bare".into()),
            ..Default::default()
        };
        let (_, report) = crate::emit::emit_report(&session, Harness::Claude, &world.base.join("claude"), &opts)?;
        let lines = report.lost_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("system:injected_context: 1 → 0 (read without its record")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("role:system")), "{lines:?}");

        // Another harness's notice has no Claude record; its injected context does, and is written.
        session.harness = Harness::Codex;
        let opts = EmitOptions {
            new_id: Some("foreign".into()),
            ..Default::default()
        };
        let (result, report) = crate::emit::emit_report(&session, Harness::Claude, &world.base.join("claude"), &opts)?;
        let written = fs::read_to_string(&result.path)?;
        assert!(written.contains(r#""type":"injected_context""#), "{written}");
        let lines = report.lost_lines();
        assert!(
            lines.contains(&"system:notice: 1 → 0 (the source harness's record has no Claude form)".to_string()),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("role:system") || l.contains("kind:meta")),
            "{lines:?}"
        );
        Ok(())
    }

    #[test]
    fn carrier_parent_resolves_through_unwritten_records_and_advances_previous() -> Result<()> {
        use crate::ir::{Message, Origin, CARRIER_KEY};
        let world = cv_testkit::World::new("claude-carrier-parents");
        let text = |m: &mut Message, id: Option<&str>, parent: Option<&str>, body: &str| {
            m.id = id.map(str::to_owned);
            m.parent_id = parent.map(str::to_owned);
            m.content.push(Block::from_text(body.into()));
        };
        let mut session = Session::new("source", Harness::Claude);
        let mut u1 = Message::new(Role::User);
        text(&mut u1, Some("u1"), None, "root");
        let mut branch = Message::new(Role::Assistant);
        text(&mut branch, Some("branch"), Some("u1"), "side branch");
        let mut unwritten = Message::of_kind(Role::System, MessageKind::Notice, Origin::Harness);
        text(&mut unwritten, Some("n1"), Some("u1"), "unwritten notice");
        let mut carrier = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness);
        text(&mut carrier, Some("c1"), Some("n1"), "queued");
        carrier.extra.insert(
            CARRIER_KEY.into(),
            json!({"type": "attachment", "uuid": "c1", "parentUuid": "n1", "sessionId": "source",
                "attachment": {"type": "queued_command"}, "rendered": [{"content": "queued"}]}),
        );
        // Next span's root (`parent_id: None`) must hang off `c1`, not `branch` or `u1`.
        let mut span2 = Message::new(Role::User);
        text(&mut span2, Some("b_u1"), None, "span 2");
        session.messages = vec![u1, branch, unwritten, carrier, span2];

        let res = emit(
            &session,
            &world.base.join("claude"),
            &EmitOptions {
                new_id: Some("derived".into()),
                ..Default::default()
            },
        )?;
        let rows: Vec<Value> = fs::read_to_string(&res.path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        let by: std::collections::HashMap<&str, &Value> = rows
            .iter()
            .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
            .collect();
        // `c1` resolves `n1 -> u1` across the intervening `branch` record.
        assert_eq!(by["c1"]["parentUuid"], "u1");
        // `b_u1` links to `c1`, keeping `c1` on the resumed walk.
        assert_eq!(by["b_u1"]["parentUuid"], "c1");
        Ok(())
    }

    #[test]
    fn claude_encode_cwd_sanitizes_all_non_alphanumeric_and_reuses_in_place_parent() -> Result<()> {
        assert_eq!(
            claude_encode_cwd(Path::new("/Users/me/my_proj@v2+build 1")),
            "-Users-me-my-proj-v2-build-1"
        );
        assert_eq!(
            claude_encode_cwd(Path::new("C:\\Users\\dev_1\\repo.ts")),
            "-C--Users-dev-1-repo-ts"
        );

        let world = cv_testkit::World::new("claude-inplace-project-dir");
        let projects = world.base.join("projects");
        let custom_dir = projects.join("-legacy_custom_dir");
        fs::create_dir_all(&custom_dir)?;
        let src_file = custom_dir.join("orig.jsonl");
        fs::write(&src_file, "{}\n")?;

        let mut session = Session::new("orig", Harness::Claude);
        session.cwd = Some(PathBuf::from("/Users/me/legacy_custom_dir"));
        session.source_path = Some(src_file);
        let mut u = crate::ir::Message::new(Role::User);
        u.content.push(Block::from_text("hi".into()));
        session.messages.push(u);

        let res = emit(
            &session,
            &projects,
            &EmitOptions {
                new_id: Some("derived".into()),
                ..Default::default()
            },
        )?;
        assert_eq!(res.path, custom_dir.join("derived.jsonl"));
        Ok(())
    }
}
