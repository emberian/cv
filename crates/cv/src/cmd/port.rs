//! `cv port` / `cv resume` — producing a copy of a session that runs elsewhere (another harness,
//! another working directory, or both), and launching one in its native harness.
//!
//! `--harness` picks the target harness (default: the source's) and `--cwd` the new home.
//! The source is never touched.
//!
//! Claude replay `--thinking` defaults to native when every recorded model is `claude-*` and to
//! portable otherwise. Portable turns thinking plaintext into ordinary text, removes reasoning that
//! has no plaintext (signature-only, redacted), and gives illegal tool IDs one collision-free
//! mapping at both endpoints. The source remains unchanged. Native preserves archival format
//! fidelity, including provider signatures and identifiers. Other targets use the text policy in
//! portable mode.

use crate::util::{home_rel, parse_harness, shell_quote, usage};
use anyhow::{bail, Context, Result};
use cv_core::harness::claude::skills::SkillsPolicy;
use cv_core::ir::{Harness, Session};
use cv_core::EmitOptions;
use std::fs;
use std::path::{Path, PathBuf};

use cv_core::emit::prepare_for_emit;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_port(
    id: &str,
    harness: Option<String>,
    cwd: Option<PathBuf>,
    out: Option<PathBuf>,
    no_context: bool,
    strict: bool,
    thinking: Option<cv_core::emit::ThinkingMode>,
    skills_policy: SkillsPolicy,
    json: bool,
) -> Result<()> {
    // The source harness rides on the id (`codex:019e…`); `--harness` is the TARGET.
    let (r, adapter) = crate::cmd::distill::resolve_source(id, None)?;
    let to_h = match &harness {
        Some(s) => Harness::parse(s).ok_or_else(|| crate::util::UsageError(format!("unknown target harness: {s}")))?,
        None => r.harness, // a pure rehome
    };
    if skills_policy != SkillsPolicy::Keep && !cv_core::harness::has_derived_copies(to_h) {
        return usage(format!(
            "--skills {} requires a Claude target (target: {to_h})",
            skills_policy.as_str()
        ));
    }
    let (session, grown) = prepare_for_emit(adapter.as_ref(), &r, to_h)?;
    let thinking = thinking.unwrap_or_else(|| {
        cv_core::emit::ThinkingMode::replay_default(
            session
                .model
                .iter()
                .chain(session.messages.iter().filter_map(|m| m.model.as_ref()))
                .map(String::as_str),
        )
    });
    let new_cwd = cwd.clone();
    let mut artifacts = cv_core::harness::claude::artifacts::Artifacts::default();
    artifacts.collect_source(&r, &session)?;
    let skills_config = (skills_policy != SkillsPolicy::Keep)
        .then(|| cv_core::harness::claude::skills::config_dir_of(&r.path))
        .flatten();
    let res = emit_session_result(
        &session,
        to_h,
        out,
        EmitOptions {
            new_cwd: cwd,
            strict,
            thinking,
            parsed_at: cv_core::emit::ParsedAt::for_port(r.harness, to_h),
            skills_policy,
            skills_config,
            ..Default::default()
        },
        json,
        Some(&artifacts),
    )?;
    if let Some(grown) = grown {
        if json {
            eprintln!("  ↳ read {grown}");
        } else {
            println!("  ↳ read {grown}");
        }
    }

    if !res.skills.is_keep() {
        let mut h = String::new();
        crate::cmd::reshape::skills_line(&mut h, &res.skills);
        if json {
            eprint!("{h}");
        } else {
            print!("{h}");
        }
    }

    // Carry the project's context files to the new home, so the ported session keeps its memory.
    if !no_context {
        if let (Some(src), Some(dst)) = (session.cwd.as_deref(), new_cwd.as_deref()) {
            carry_context(src, dst, json);
        }
    }
    Ok(())
}

/// Project context files a harness reads from the cwd. We copy these alongside a ported session so
/// it lands with its memory/instructions intact. Best-effort: never overwrite, never fatal.
const CONTEXT_FILES: &[&str] = &[
    "CLAUDE.md",
    "CLAUDE.local.md",
    "AGENTS.md",
    "GEMINI.md",
    "MEMORY.md",
    ".cursorrules",
    ".windsurfrules",
];

fn carry_context(src: &Path, dst: &Path, json: bool) {
    if src == dst {
        return;
    }
    let mut copied = Vec::new();
    for name in CONTEXT_FILES {
        let from = src.join(name);
        if !from.is_file() {
            continue;
        }
        let to = dst.join(name);
        let already_exists = || eprintln!("  ↳ context: {name} already exists at target — left as-is");
        if to.exists() {
            already_exists();
            continue;
        }
        match fs::create_dir_all(dst).and_then(|_| copy_context_if_absent(&from, &to)) {
            Ok(Some(_)) => copied.push(*name),
            Ok(None) => already_exists(),
            Err(e) => eprintln!("  ↳ context: couldn't copy {name}: {e}"),
        }
    }
    if !copied.is_empty() {
        if json {
            eprintln!("  ↳ carried context: {}", copied.join(", "));
        } else {
            println!("  ↳ carried context: {}", copied.join(", "));
        }
    }
}

/// Stage the complete context before claiming its name, including permissions from the source.
fn copy_context_if_absent(from: &Path, to: &Path) -> std::io::Result<Option<u64>> {
    use cv_core::publish::{refuse_existing, Claim, Exists, Staged};
    let copy = || -> Result<u64> {
        refuse_existing(to, "")?;
        let mut source = fs::File::open(from)?;
        let mut staged = Staged::beside(to, "context", cv_core::publish::mode_of(&source))?;
        let copied = std::io::copy(&mut source, &mut staged)?;
        staged.file().set_permissions(source.metadata()?.permissions())?;
        staged.publish(to, Claim::New)?;
        Ok(copied)
    };
    match copy() {
        Ok(copied) => Ok(Some(copied)),
        Err(error) if error.downcast_ref::<Exists>().is_some() => Ok(None),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

pub(crate) fn emit_session(
    session: &Session,
    to_h: Harness,
    out: Option<PathBuf>,
    opts: EmitOptions,
) -> Result<PathBuf> {
    emit_session_output(session, to_h, out, opts, false)
}

pub(crate) fn emit_session_output(
    session: &Session,
    to_h: Harness,
    out: Option<PathBuf>,
    opts: EmitOptions,
    json: bool,
) -> Result<PathBuf> {
    Ok(emit_session_result(session, to_h, out, opts, json, None)?.path)
}

fn emit_session_result(
    session: &Session,
    to_h: Harness,
    out: Option<PathBuf>,
    opts: EmitOptions,
    json: bool,
    artifacts: Option<&cv_core::harness::claude::artifacts::Artifacts>,
) -> Result<cv_core::EmitResult> {
    if !cv_core::emit::supported_targets().contains(&to_h) {
        // A caller mistake, not a failure (exit 2): the source parsed, the target cannot be written.
        return usage(format!(
            "{to_h} is not a supported target (the source parses fine, {} messages); supported: {}",
            session.messages.len(),
            cv_core::emit::supported_targets()
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let out_dir = match out {
        Some(d) => d,
        None => cv_core::harness::for_harness(to_h)
            .and_then(|a| a.root())
            .with_context(|| format!("{to_h} doesn't appear installed; pass --out <dir> to write somewhere"))?,
    };
    let artifacts = artifacts.filter(|a| !a.is_empty());
    if let Some(artifacts) = artifacts {
        artifacts.validate().context("validating tool artifacts before port")?;
    }
    let out_existed = out_dir.exists();
    let out_was_empty = !out_existed || fs::read_dir(&out_dir).is_ok_and(|mut d| d.next().is_none());
    // Verified emit: after writing, re-parse the output with the target's own adapter and diff it
    // against the source IR, so a lossy conversion is *visible* instead of silent.
    let (res, warnings) = cv_core::emit::emit_verified(session, to_h, &out_dir, &opts)?;
    if let Some(artifacts) = artifacts {
        let anchor = cv_core::harness::claude::artifacts::anchor(&res.path, &res.new_id);
        let published = {
            let mut set = cv_core::publish::Set::new();
            artifacts
                .stage(&anchor, &mut set, cv_core::publish::Claim::New)
                .and_then(|()| set.publish())
        };
        if let Err(err) = published {
            return match cv_core::emit::retract_emitted(to_h, &out_dir, out_existed, out_was_empty, &res, &anchor) {
                Ok(()) => Err(err.context("carrying tool artifacts failed; retracted ported transcript")),
                Err(undo) => Err(err.context(format!(
                    "wrote {}, carrying tool artifacts failed and retracting output also failed ({undo:#})",
                    res.path.display()
                ))),
            };
        }
    }
    if json {
        let out = serde_json::json!({"path": res.path, "id": res.new_id, "harness": to_h,
            "resume_hint": res.resume_hint, "warnings": warnings});
        if super::place::current().is_some() {
            super::place::print_json("pack", &out)?;
        } else {
            println!("{out}");
        }
    } else {
        println!("✦ wrote {} ({})", res.path.display(), res.new_id);
        if let Some(hint) = &res.resume_hint {
            println!("  ↳ {hint}");
        }
    }
    for w in &warnings {
        eprintln!("  ⚠ lossy: {w}");
    }
    Ok(res)
}

// ---------- resume ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeTarget {
    Cli { program: &'static str, args: Vec<String> },
    GuiOnly { hint: String },
}

pub(crate) fn cmd_resume(id: &str, harness: Option<String>, launch: bool, json: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (want, source) = crate::util::split_harness_id(id, want);
    let explicit_path = Path::new(source).is_file();
    let (mut r, adapter) = if launch {
        crate::cmd::distill::resolve_source_quiet(source, want)?
    } else {
        crate::cmd::distill::resolve_source(source, want)?
    };
    if explicit_path {
        crate::util::require_session_file(&r, adapter.as_ref())?;
        // A path ref starts with its filename, not necessarily the native session ID or cwd.
        let read = cv_core::stream::stream_counted(
            adapter.as_ref(),
            &r,
            &cv_core::Demand::FACTS,
            &mut cv_core::stream::Discard,
            cv_core::stream::Pass::RECORD,
        )?;
        r.id = read.session.id;
        r.cwd = read.session.cwd;
    }
    let env = resume_environment(&r, adapter.as_ref(), explicit_path)?;
    let cwd = r.cwd.clone();
    let caveat = match (
        adapter.resume_caveat(&r),
        (!explicit_path)
            .then(|| crate::util::shadowed_copy_caveat(&r))
            .flatten(),
    ) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    if json {
        let mut plan = resume_json(r.harness, &r.id, cwd.as_deref(), caveat.as_deref());
        plan["env"] = serde_json::to_value(&env.set)?;
        plan["unset_env"] = serde_json::to_value(&env.unset)?;
        plan["path"] = serde_json::to_value(&r.path)?;
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }

    if launch {
        let (program, args) = match resume_target(r.harness, &r.id) {
            ResumeTarget::Cli { program, args } => (program, args),
            ResumeTarget::GuiOnly { hint } => {
                return crate::util::usage(format!("cannot --launch {}: {hint}", r.harness));
            }
        };
        let dir = cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Some(caveat) = &caveat {
            eprintln!("⚠ {caveat}");
        }
        eprintln!("✦ launching: (cd {}) {} {}", home_rel(&dir), program, args.join(" "));
        let mut command = std::process::Command::new(program);
        command.args(&args).envs(&env.set).current_dir(&dir);
        for name in &env.unset {
            command.env_remove(name);
        }
        let status = command
            .status()
            .with_context(|| format!("failed to launch {program:?}"))?;
        if !status.success() {
            bail!("{program} exited with status {status}");
        }
        return Ok(());
    }

    // Print the incantation, after what the store says about it as a shell comment.
    if let Some(caveat) = &caveat {
        println!("# {caveat}");
    }
    let mut lines = resume_lines(r.harness, &r.id, cwd.as_deref());
    if !env.set.is_empty() || !env.unset.is_empty() {
        let assignments = env
            .unset
            .iter()
            .map(|name| format!("-u {name}"))
            .chain(
                env.set
                    .iter()
                    .map(|(key, value)| format!("{key}={}", shell_quote(value))),
            )
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(command) = lines.last_mut() {
            *command = format!("env {assignments} {command}");
        }
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Select the source's native home, including an ID found in an extra root. Export files and
/// child transcripts cannot be safely addressed with the native resume CLI.
fn resume_environment(
    r: &cv_core::SessionRef,
    adapter: &dyn cv_core::Adapter,
    explicit_path: bool,
) -> Result<cv_core::harness::ResumeEnv> {
    if matches!(resume_target(r.harness, &r.id), ResumeTarget::GuiOnly { .. }) {
        return Ok(Default::default());
    }
    let why = match adapter.resume_env(r) {
        Some(Ok(env)) => return Ok(env),
        Some(Err(why)) => why,
        // No store selection for this harness: an ID resumes wherever its CLI looks.
        None if !explicit_path => return Ok(Default::default()),
        None => "no native store can be selected for this source; use the original harness/profile or port it into a supported native store".into(),
    };
    usage(format!(
        "cannot safely resume {} from {}: {why}",
        r.harness,
        r.path.display()
    ))
}

fn resume_json(harness: Harness, id: &str, cwd: Option<&Path>, warning: Option<&str>) -> serde_json::Value {
    let (program, args, hint) = match resume_target(harness, id) {
        ResumeTarget::Cli { program, args } => (Some(program), args, None),
        ResumeTarget::GuiOnly { hint } => (None, Vec::new(), Some(hint)),
    };
    serde_json::json!({
        "id": id, "harness": harness.as_str(), "program": program, "args": args,
        "cwd": cwd, "hint": hint, "warning": warning,
    })
}

/// The resume incantation `cv resume` prints, as lines: `cd <cwd>` (when known), then the
/// harness's own resume command (or a comment hint when the harness has no CLI resume). Shared
/// with `cv rewind`, which prints it for the session it made.
pub(crate) fn resume_lines(h: Harness, id: &str, cwd: Option<&Path>) -> Vec<String> {
    match resume_target(h, id) {
        ResumeTarget::Cli { program, args } => {
            let mut out = Vec::new();
            if let Some(dir) = cwd {
                out.push(format!("cd {}", shell_quote(&dir.display().to_string())));
            }
            if args.is_empty() {
                out.push(program.to_string());
            } else {
                out.push(format!(
                    "{program} {}",
                    args.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ")
                ));
            }
            out
        }
        ResumeTarget::GuiOnly { hint } => vec![format!("# {hint}")],
    }
}

/// Best-known resume target per harness: either the CLI program + args (cwd is handled separately,
/// since most harnesses resume relative to the directory they're launched in) or a GUI-only hint
/// that `cv resume --launch` refuses with exit 2 instead of `execvp`-ing a comment string.
pub(crate) fn resume_target(h: Harness, id: &str) -> ResumeTarget {
    match h {
        Harness::Claude => ResumeTarget::Cli {
            program: "claude",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Codex => ResumeTarget::Cli {
            program: "codex",
            args: vec!["resume".into(), id.into()],
        },
        Harness::Grok => ResumeTarget::Cli {
            program: "grok",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::OpenCode => ResumeTarget::Cli {
            program: "opencode",
            args: vec!["--session".into(), id.into()],
        },
        Harness::Gemini => ResumeTarget::Cli {
            program: "gemini",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Antigravity => ResumeTarget::Cli {
            program: "agy",
            args: vec!["--conversation".into(), id.into()],
        },
        Harness::Hermes => ResumeTarget::Cli {
            program: "hermes",
            args: vec!["resume".into(), id.into()],
        },
        Harness::OpenClaw => ResumeTarget::Cli {
            program: "openclaw",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Cursor => ResumeTarget::Cli {
            program: "cursor-agent",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Kimi => ResumeTarget::Cli {
            program: "kimi",
            args: vec!["--resume".into(), id.into()],
        },
        // Kimi Code (`kimi --session <id>` / `-S`): the native id is `session_<uuid>`; cv keys the
        // session by the bare uuid so prefixes work like every other harness.
        Harness::KimiCode => ResumeTarget::Cli {
            program: "kimi",
            args: vec!["--session".into(), format!("session_{id}")],
        },
        Harness::Qwen => ResumeTarget::Cli {
            program: "qwen",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Cline => ResumeTarget::Cli {
            program: "cline",
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Continue => ResumeTarget::Cli {
            program: "cn",
            args: vec!["--resume".into()],
        },
        Harness::Goose => ResumeTarget::Cli {
            program: "goose",
            args: vec!["session".into(), "--resume".into(), "--session-id".into(), id.into()],
        },
        Harness::Devin => ResumeTarget::Cli {
            program: "devin",
            args: vec!["-r".into(), id.into()],
        },
        Harness::Pi => ResumeTarget::Cli {
            program: "pi",
            args: vec!["--session".into(), id.into()],
        },
        Harness::LmStudio
        | Harness::Roo
        | Harness::Zed
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => ResumeTarget::GuiOnly {
            hint: format!("no CLI resume for {h}; open the app and find session {id}"),
        },
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn context_copy_read_failure_does_not_publish_an_empty_file() {
        let world = cv_testkit::World::new("port-context-read-failure");
        let destination = world.base.join("target.md");
        let before = fs::read_dir(&world.base).unwrap().count();
        // A source replaced by a directory between discovery and copying opens on Unix,
        // but reading it fails, just as an I/O failure after opening a regular file does.
        assert!(copy_context_if_absent(&world.base, &destination).is_err());
        assert!(!destination.exists(), "failed copy claimed the destination");
        assert_eq!(fs::read_dir(&world.base).unwrap().count(), before);
    }

    #[test]
    fn context_copy_never_overwrites_existing_destinations() {
        let world = cv_testkit::World::new("port-context-exclusive");
        let source = world.base.join("source.md");
        let destination = world.base.join("target.md");
        fs::write(&source, "project instructions λ").unwrap();
        assert_eq!(
            copy_context_if_absent(&source, &destination).unwrap(),
            Some("project instructions λ".len() as u64)
        );
        fs::write(&destination, "concurrent instructions").unwrap();
        assert_eq!(copy_context_if_absent(&source, &destination).unwrap(), None);
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent instructions");
        assert_eq!(fs::read(&source).unwrap(), "project instructions λ".as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn context_copy_leaves_broken_symlinks_and_their_targets_untouched() {
        let world = cv_testkit::World::new("port-context-symlink");
        let source = world.base.join("source.md");
        let destination = world.base.join("target.md");
        let missing = world.base.join("unrequested.md");
        fs::write(&source, "instructions").unwrap();
        std::os::unix::fs::symlink(&missing, &destination).unwrap();
        assert_eq!(copy_context_if_absent(&source, &destination).unwrap(), None);
        assert!(destination.is_symlink());
        assert!(!missing.exists());
    }

    #[test]
    fn resume_json_keeps_arguments_unquoted_and_gui_hints_separate() {
        let id = "session with 'quotes' and $shell";
        let cwd = Path::new("/work/a folder/it's here");
        let row = resume_json(Harness::Claude, id, Some(cwd), Some("use the original profile"));
        assert_eq!(row["program"], "claude");
        assert_eq!(row["args"], serde_json::json!(["--resume", id]));
        assert_eq!(row["cwd"], cwd.to_str().unwrap());
        assert_eq!(row["warning"], "use the original profile");
        assert!(row["hint"].is_null());
        let gui = resume_json(Harness::OpenSession, "document", None, None);
        assert!(gui["program"].is_null());
        assert_eq!(gui["args"], serde_json::json!([]));
        assert!(gui["hint"].as_str().unwrap().contains("no CLI resume"));
        assert!(gui["cwd"].is_null());
    }

    #[test]
    fn resume_target_separates_cli_programs_from_gui_hints() {
        for h in Harness::ALL {
            match resume_target(h, "sess-42") {
                ResumeTarget::Cli { program, args } => {
                    assert!(
                        !program.is_empty() && !program.starts_with('#') && !program.contains(' '),
                        "invalid CLI program for {h}: {program:?}"
                    );
                    let lines = resume_lines(h, "sess-42", Some(Path::new("/work/app")));
                    assert_eq!(lines[0], "cd /work/app");
                    assert!(lines[1].starts_with(program), "{h}: {:?} ({args:?})", lines[1]);
                }
                ResumeTarget::GuiOnly { hint } => {
                    assert!(!hint.starts_with('#') && hint.contains(h.as_str()), "{h}: {hint}");
                    let lines = resume_lines(h, "sess-42", Some(Path::new("/work/app")));
                    assert_eq!(lines, vec![format!("# {hint}")]);
                }
            }
        }
        assert_eq!(
            resume_target(Harness::parse("goose").unwrap(), "abc"),
            ResumeTarget::Cli {
                program: "goose",
                args: vec!["session".into(), "--resume".into(), "--session-id".into(), "abc".into()],
            }
        );
        assert_eq!(
            resume_target(Harness::parse("cursor").unwrap(), "abc"),
            ResumeTarget::Cli {
                program: "cursor-agent",
                args: vec!["--resume".into(), "abc".into()],
            }
        );
        assert_eq!(
            resume_target(Harness::parse("continue").unwrap(), "abc"),
            ResumeTarget::Cli {
                program: "cn",
                args: vec!["--resume".into()],
            }
        );
        assert_eq!(
            resume_target(Harness::parse("cline").unwrap(), "abc"),
            ResumeTarget::Cli {
                program: "cline",
                args: vec!["--resume".into(), "abc".into()],
            }
        );
        assert!(matches!(
            resume_target(Harness::parse("opensession").unwrap(), "abc"),
            ResumeTarget::GuiOnly { .. }
        ));
    }

    // #tooth Corrupt source sidecars stop before emit, and destination artifact collisions retract
    // the newly emitted transcript without touching pre-existing files.
    #[test]
    fn port_artifact_failure_leaves_no_partial_transcript() {
        use cv_core::ir::{Block, Message, Role};
        let world = cv_testkit::World::new("port-artifact-retract");
        let src_transcript = world.base.join("src-session.jsonl");
        fs::write(&src_transcript, "{}\n").unwrap();
        let src_flat = world.base.join("src-session.flat.jsonl");
        fs::write(&src_flat, "not valid json\n").unwrap();

        let mut session = Session::new("src-session", Harness::Claude);
        session.cwd = Some(world.base.join("proj"));
        let mut user = Message::new(Role::User);
        user.content.push(Block::from_text("hello".into()));
        session.messages.push(user);

        let mut bad_artifacts = cv_core::harness::claude::artifacts::Artifacts::default();
        bad_artifacts.collect(&src_transcript).unwrap();
        let fresh_out = world.base.join("fresh-out");
        assert!(emit_session_result(
            &session,
            Harness::Claude,
            Some(fresh_out.clone()),
            EmitOptions::default(),
            true,
            Some(&bad_artifacts),
        )
        .is_err());
        assert!(
            !fresh_out.exists(),
            "corrupt artifact must fail before creating out_dir"
        );

        // Now make the source sidecar valid, but plant a conflicting sidecar at the destination.
        fs::write(
            &src_flat,
            r#"{"id":"t1","slot":"content","name":"Bash","input":null,"content":"out","size":3,"line_count":1,"kind":"text"}"#,
        )
        .unwrap();
        let mut good_artifacts = cv_core::harness::claude::artifacts::Artifacts::default();
        good_artifacts.collect(&src_transcript).unwrap();

        let slug = format!(
            "-{}",
            session
                .cwd
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .trim_start_matches('/')
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
        );
        let dest_proj = world.base.join("dest-out").join(slug);
        fs::create_dir_all(&dest_proj).unwrap();
        let conflicting_flat = dest_proj.join("pinned-port.flat.jsonl");
        fs::write(&conflicting_flat, b"pre-existing sidecar").unwrap();

        let err = emit_session_result(
            &session,
            Harness::Claude,
            Some(world.base.join("dest-out")),
            EmitOptions {
                new_id: Some("pinned-port".into()),
                ..Default::default()
            },
            true,
            Some(&good_artifacts),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("retracted ported transcript"), "{msg}");
        assert!(
            !dest_proj.join("pinned-port.jsonl").exists(),
            "ported transcript must be retracted on artifact failure"
        );
        assert_eq!(fs::read(&conflicting_flat).unwrap(), b"pre-existing sidecar");
    }
}
