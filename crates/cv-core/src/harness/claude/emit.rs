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

/// The verbatim meta record a format-complete carrier message stashed (an object under
/// `extra["_record"]`), or `None` for an ordinary message. See
/// [`claude::carrier_record`](crate::ir::CARRIER_KEY).
fn carrier_record_of(m: &crate::ir::Message) -> Option<&Value> {
    m.extra.get(crate::ir::CARRIER_KEY).filter(|v| v.is_object())
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
        u.content.push(Block::Text { text: "hi".into() });
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
