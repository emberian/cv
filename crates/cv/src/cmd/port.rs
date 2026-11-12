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

use crate::util::{home_rel, parse_harness, resolve, shell_quote, usage};
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
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
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
    let written = emit_session_output(
        &session,
        to_h,
        out,
        EmitOptions {
            new_cwd: cwd,
            strict,
            thinking,
            parsed_at: cv_core::emit::ParsedAt::for_port(r.harness, to_h),
            ..Default::default()
        },
        json,
    )?;
    if let Some(grown) = grown {
        if json {
            eprintln!("  ↳ read {grown}");
        } else {
            println!("  ↳ read {grown}");
        }
    }

    if skills_policy != SkillsPolicy::Keep {
        use cv_core::harness::claude::skills::{config_dir_of, rewrite_file};
        let config = config_dir_of(&written).or_else(|| config_dir_of(&r.path));
        let report = rewrite_file(&written, skills_policy, config.as_deref())?;
        let mut h = String::new();
        crate::cmd::reshape::skills_line(&mut h, &report);
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
    // Verified emit: after writing, re-parse the output with the target's own adapter and diff it
    // against the source IR, so a lossy conversion is *visible* instead of silent.
    let (res, warnings) = cv_core::emit::emit_verified(session, to_h, &out_dir, &opts)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"path": res.path, "id": res.new_id, "harness": to_h,
            "resume_hint": res.resume_hint, "warnings": warnings})
        );
    } else {
        println!("✦ wrote {} ({})", res.path.display(), res.new_id);
        if let Some(hint) = res.resume_hint {
            println!("  ↳ {hint}");
        }
    }
    for w in &warnings {
        eprintln!("  ⚠ lossy: {w}");
    }
    Ok(res.path)
}

// ---------- resume ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeTarget {
    Cli { program: &'static str, args: Vec<String> },
    GuiOnly { hint: String },
}

pub(crate) fn cmd_resume(id: &str, harness: Option<String>, launch: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, _adapter) = resolve(id, want)?;
    let cwd = r.cwd.clone();

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
        eprintln!("✦ launching: (cd {}) {} {}", home_rel(&dir), program, args.join(" "));
        let status = std::process::Command::new(program)
            .args(&args)
            .current_dir(&dir)
            .status()
            .with_context(|| format!("failed to launch {program:?}"))?;
        if !status.success() {
            bail!("{program} exited with status {status}");
        }
        return Ok(());
    }

    // Print the incantation.
    for line in resume_lines(r.harness, &r.id, cwd.as_deref()) {
        println!("{line}");
    }
    Ok(())
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
            args: vec!["--resume".into(), id.into()],
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
}
