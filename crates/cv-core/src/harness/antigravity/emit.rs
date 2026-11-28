//! One-step seed emitter (`IR -> Antigravity` cold app-data root).
//!
//! Writes a single native `USER_INPUT` step (`step_type = 14`, `status = 3`) carrying the
//! provenance-fenced seed text plus a matching `conversation_summaries` catalog row inside a
//! `BEGIN IMMEDIATE` transaction, verifying readback through both [`super::native::project_rows`]
//! and [`super::Antigravity::with_home`] before committing. Tool calls and assistant turns are
//! never synthesized as fabricated protobuf steps; `port --strict --harness antigravity` refuses
//! any session whose turns or tool calls would be flattened.

use super::cold::{self, proto, SummarySpec};
use super::{native, Antigravity};
use crate::coverage::ReadCoverage;
use crate::emit::{thinking_fate_with_presence, thinking_placeholder, EmitOptions, ThinkingFate, ThinkingMode};
use crate::harness::store::bytes::ByteSource;
use crate::harness::{EmitResult, Parse as _};
use crate::ir::{Block, Harness, MessageKind, Role, Session, SessionRef};
use crate::publish::{Claim, StagedDir};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;

pub(super) fn pbtxt_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn project_session_thinking(session: &Session, mode: ThinkingMode) -> Result<Session> {
    let mut projected = session.clone();
    projected.materialize()?;
    let resolver = projected.resolver();
    for msg in &mut projected.messages {
        let mut blocks = Vec::with_capacity(msg.content.len());
        for b in std::mem::take(&mut msg.content) {
            match b {
                Block::Thinking {
                    text,
                    signature,
                    encrypted,
                    redacted,
                    extensions,
                } => {
                    let text_str = text.try_resolve(&resolver)?;
                    let has_text = !text_str.trim().is_empty();
                    let fate = thinking_fate_with_presence(
                        Harness::Antigravity,
                        mode,
                        has_text,
                        signature.as_deref(),
                        encrypted.as_deref(),
                        redacted,
                    );
                    match fate {
                        ThinkingFate::Keep => blocks.push(Block::Thinking {
                            text,
                            signature,
                            encrypted,
                            redacted,
                            extensions,
                        }),
                        ThinkingFate::AsText => blocks.push(Block::from_text(text_str.into_owned().into())),
                        ThinkingFate::Placeholder => blocks.push(Block::from_text(
                            thinking_placeholder(signature.as_deref(), encrypted.as_deref(), redacted).into(),
                        )),
                        ThinkingFate::Drop => {}
                    }
                }
                other => blocks.push(other),
            }
        }
        msg.content = blocks;
    }
    Ok(projected)
}

/// The seed text, and whether it is the session's only prompt verbatim (otherwise every turn is
/// flattened into one rendered Markdown prompt).
fn build_seed_text(projected: &Session) -> Result<(String, bool)> {
    let resolver = projected.resolver();
    // Single plain user prompt with no system prompt or other turns: preserve verbatim so a
    // 1-turn seed session round-trips cleanly with zero unexpected deltas under `--strict`.
    let non_carrier: Vec<_> = projected
        .messages
        .iter()
        .filter(|m| m.kind != MessageKind::Carrier && !m.content.is_empty())
        .collect();
    if projected.system_prompt.as_deref().is_none_or(str::is_empty) && non_carrier.len() == 1 {
        let only = non_carrier[0];
        if only.role == Role::User
            && only.kind == MessageKind::Prompt
            && only.content.iter().all(|b| matches!(b, Block::Text { .. }))
        {
            let mut parts = Vec::with_capacity(only.content.len());
            for b in &only.content {
                if let Block::Text { text, .. } = b {
                    parts.push(text.try_resolve(&resolver)?);
                }
            }
            let joined = parts.join("\n");
            if !joined.is_empty() {
                return Ok((joined, true));
            }
        }
    }

    let mut rendered = Vec::new();
    if let Some(sys) = projected.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        let block = crate::render::fenced_block(sys, "text");
        std::io::Write::write_all(&mut rendered, format!("## System Prompt\n\n{block}\n\n").as_bytes())?;
    }
    crate::render::write_markdown(projected, &mut rendered, &resolver)?;
    let text = String::from_utf8(rendered).context("rendered Markdown seed is not valid UTF-8")?;
    Ok((text, false))
}

fn encode_field(number: u32, v: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    proto::bytes(&mut out, number, v);
    out
}

fn encode_int(number: u32, v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    proto::scalar(&mut out, number, v);
    out
}

fn encode_trajectory_blob(
    new_id: &str,
    cwd: Option<&Path>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<u8> {
    let mut blob = Vec::new();
    if let Some(dir) = cwd {
        let uri = super::path_to_file_uri(dir);
        let ws = encode_field(1, uri.as_bytes());
        proto::bytes(&mut blob, 1, &ws);
    }
    if let Some(ts) = created_at {
        if let Ok(secs) = u64::try_from(ts.timestamp()) {
            if secs > 0 {
                let mut ts_msg = encode_int(1, secs);
                let nanos = ts.timestamp_subsec_nanos();
                if nanos > 0 {
                    ts_msg.extend(encode_int(2, u64::from(nanos)));
                }
                proto::bytes(&mut blob, 2, &ts_msg);
            }
        }
    }
    proto::bytes(&mut blob, 6, new_id.as_bytes());
    blob
}

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    emit_staged(session, out_dir, opts, None)
}

/// [`emit`] with an optional pre-publication verifier, which reads the staged conversation
/// database and annotation before anything under `out_dir` changes.
pub(crate) fn emit_staged(
    session: &Session,
    out_dir: &Path,
    opts: &EmitOptions,
    verify: Option<crate::emit::stage::VerifyFn<'_>>,
) -> Result<EmitResult> {
    if opts.sidechain_of.is_some() {
        bail!("emit to antigravity supports standalone conversations only (--agent-of is not supported)");
    }
    opts.validate_ids()?;
    let new_id = opts.new_id.clone().unwrap_or_else(crate::prune::new_uuid);
    crate::publish::validate_output_id(&new_id)?;
    ensure!(
        super::looks_like_uuid(&new_id),
        "Antigravity conversation IDs must be UUIDs (got {new_id:?})"
    );
    if cold::live_ls_active() {
        if let Some(active) = super::active_cli_app_dir() {
            ensure!(
                !super::same_app_dir(out_dir, &active),
                "active Antigravity language server owns {}; pass --out <dir> for a cold export or use `cv distill <id> --session=antigravity --delivery=live` for live delivery",
                active.display()
            );
        }
    }
    if out_dir.exists() {
        ensure!(out_dir.is_dir(), "--out must be an app-data directory");
        cold::probe_summary_unlocked(out_dir)?;
    } else if let Some(parent) = out_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let projected = project_session_thinking(session, opts.thinking)?;
    let (seed_text, verbatim) = build_seed_text(&projected)?;
    // Refuse before anything is written: a strict port cannot be retracted after the fact.
    ensure!(
        verbatim || !opts.strict,
        "--strict: Antigravity holds one seed prompt; this session's turns and tool calls would be flattened into it"
    );
    let cwd = opts.new_cwd.as_deref().or(projected.cwd.as_deref());
    let title = projected.title.as_deref().unwrap_or("");
    let created_at = projected
        .created_at
        .or_else(|| projected.messages.iter().find_map(|m| m.timestamp));
    let updated_at = projected
        .updated_at
        .or_else(|| projected.messages.iter().rev().find_map(|m| m.timestamp))
        .or(created_at);
    let project_id = projected
        .harness_extra(Harness::Antigravity)
        .and_then(|v| v.get("project_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let stage = StagedDir::beside(out_dir, "antigravity-emit")?;
    fs::create_dir_all(stage.path().join("conversations"))?;
    fs::create_dir_all(stage.path().join("annotations"))?;
    fs::create_dir_all(stage.path().join("brain").join(&new_id))?;

    let trajectory_id = crate::prune::new_uuid();
    let rel_db = Path::new("conversations").join(format!("{new_id}.db"));
    let mut dst = Connection::open(stage.path().join(&rel_db))?;
    {
        let tx = dst.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(cold::SCHEMA)?;
        tx.execute(
            "INSERT INTO trajectory_meta VALUES(?,?,1,1)",
            params![trajectory_id, new_id],
        )?;
        let blob = encode_trajectory_blob(&new_id, cwd, created_at);
        tx.execute("INSERT INTO trajectory_metadata_blob VALUES('main', ?)", params![blob])?;

        let ref_inner = [
            encode_field(1, trajectory_id.as_bytes()),
            encode_field(4, new_id.as_bytes()),
        ]
        .concat();
        let md = encode_field(20, &ref_inner);
        let user_input = [
            encode_field(2, seed_text.as_bytes()),
            encode_field(3, &encode_field(1, seed_text.as_bytes())),
        ]
        .concat();
        let step_payload = [
            encode_int(1, 14),
            encode_int(4, 3),
            encode_field(5, &md),
            encode_field(19, &user_input),
        ]
        .concat();
        tx.execute(
            "INSERT INTO steps(idx,step_type,status,metadata,step_payload,step_format) VALUES(0,14,3,?,?,0)",
            params![md, step_payload],
        )?;

        // Verify native row projection inside the staged SQLite transaction before commit.
        native::project_rows(&tx, false, -1, &ByteSource::arena()?)?;
        tx.commit()?;
    }
    drop(dst);

    let pbtxt_body = if title.trim().is_empty() {
        String::new()
    } else {
        format!("title: \"{}\"\n", pbtxt_escape(title))
    };
    fs::write(
        stage.path().join("annotations").join(format!("{new_id}.pbtxt")),
        pbtxt_body.as_bytes(),
    )?;

    let resume_cmd = format!("agy --conversation {new_id}");
    let resume_hint = Some(crate::harness::resume_in(cwd, resume_cmd));
    if let Some(verify) = verify {
        let staged = EmitResult {
            path: stage.path().join(&rel_db),
            new_id: new_id.clone(),
            resume_hint: resume_hint.clone(),
            ..Default::default()
        };
        verify(crate::emit::stage::StagedSource::Files {
            result: &staged,
            destination: &out_dir.join(&rel_db),
        })?;
    }

    let claim = opts.claim();
    let spec = SummarySpec {
        new_id: &new_id,
        trajectory_id: &trajectory_id,
        title,
        step_count: 1,
        last_user_input_step_index: Some(0),
        cwd,
        project_id,
        modified: updated_at,
        last_user_time: created_at,
        replace: matches!(claim, Claim::Replace),
    };

    let db_out = out_dir.join(&rel_db);
    cold::publish_staged_app_root(stage, out_dir, &spec, claim, |root| {
        // Readback verification through the Antigravity adapter before publishing `stage` (when
        // `!out_dir.exists()`) or before committing `conversation_summaries.db` (existing roots).
        let agy = Antigravity::with_home(root);
        let sref = SessionRef::new(&new_id, Harness::Antigravity, root.join(&rel_db));
        let reparsed = agy
            .parse(&sref)
            .context("re-parsing emitted Antigravity SQLite trajectory")?;
        ensure!(
            reparsed.id == new_id,
            "reparsed id mismatch ({} != {new_id})",
            reparsed.id
        );
        ensure!(
            reparsed.messages.len() == 1,
            "expected 1 seed message in reparsed Antigravity trajectory, got {}",
            reparsed.messages.len()
        );
        ensure!(
            reparsed.messages[0].role == Role::User && reparsed.messages[0].kind == MessageKind::Prompt,
            "expected reparsed step 0 to be a user prompt"
        );
        let got_text = reparsed.messages[0].text().unwrap_or_default();
        ensure!(
            got_text == seed_text,
            "reparsed Antigravity seed text differs from emitted seed"
        );
        if let Some(cov) = ReadCoverage::of_session(&reparsed) {
            ensure!(
                !cov.limitations.contains(ReadCoverage::UNSUPPORTED_NATIVE_FIELDS)
                    && !cov.limitations.contains(ReadCoverage::UNRECONCILED_HISTORY),
                "emitted Antigravity trajectory reported coverage limitations: {:?}",
                cov.limitations
            );
        }
        Ok(())
    })?;

    Ok(EmitResult {
        path: db_out,
        new_id,
        resume_hint,
        ..Default::default()
    })
}

/// Undo a fresh [`emit`] publication: the conversation database and its SQLite sidecars, its
/// annotation and brain dir, and its catalog row (or the whole catalog, and the root's empty
/// subdirectories, when the root held nothing before).
pub(crate) fn retract(out_dir: &Path, out_existed: bool, out_was_empty: bool, res: &EmitResult) {
    let remove_db = |db: &Path| {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = db.as_os_str().to_os_string();
            p.push(suffix);
            let _ = fs::remove_file(std::path::PathBuf::from(p));
        }
    };
    remove_db(&res.path);
    let _ = fs::remove_file(out_dir.join("annotations").join(format!("{}.pbtxt", res.new_id)));
    let _ = fs::remove_dir_all(out_dir.join("brain").join(&res.new_id));
    let sum_db = out_dir.join("conversation_summaries.db");
    if out_was_empty {
        remove_db(&sum_db);
        for sub in ["annotations", "brain", "conversations"] {
            let _ = fs::remove_dir(out_dir.join(sub));
        }
        if !out_existed {
            let _ = fs::remove_dir(out_dir);
        }
    } else if sum_db.is_file() {
        if let Ok(conn) = Connection::open(&sum_db) {
            let _ = conn.execute(
                "DELETE FROM conversation_summaries WHERE conversation_id = ?1",
                [&res.new_id],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_stage_verification_never_publishes_new_app_root() {
        // #tooth When `!out.exists()`, `publish_staged_app_root` verifies `stage.path()` *before*
        // `stage.publish(out, claim)` so a verification error never creates `out`.
        let world = crate::test_world("agy-stage-verify-before-publish");
        let out = world.base.join("fresh-agy-root");
        let stage = StagedDir::beside(&out, "agy-verify-test").unwrap();
        fs::create_dir_all(stage.path().join("conversations")).unwrap();
        let new_id = crate::prune::new_uuid();
        let trajectory_id = crate::prune::new_uuid();
        let spec = SummarySpec {
            new_id: &new_id,
            trajectory_id: &trajectory_id,
            title: "test",
            step_count: 1,
            last_user_input_step_index: Some(0),
            cwd: None,
            project_id: "",
            modified: None,
            last_user_time: None,
            replace: false,
        };
        let mut saw_staged = false;
        let err = cold::publish_staged_app_root(stage, &out, &spec, Claim::New, |verified_root| {
            saw_staged = verified_root != out && verified_root.exists() && !out.exists();
            anyhow::bail!("simulated readback failure")
        })
        .unwrap_err();
        assert!(err.to_string().contains("simulated readback failure"));
        assert!(
            saw_staged,
            "verifier must inspect staged root while `out` does not yet exist"
        );
        assert!(!out.exists(), "failed verification must leave `out` unpublished");
    }
}
