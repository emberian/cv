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
) -> Result<()> {
    // The source harness rides on the id (`codex:019e…`); `--harness` is the TARGET.
    let (r, adapter) = resolve(id, None)?;
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
    let written = emit_session(
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
    )?;
    if let Some(grown) = grown {
        println!("  ↳ read {grown}");
    }

    if skills_policy != SkillsPolicy::Keep {
        use cv_core::harness::claude::skills::{config_dir_of, rewrite_file};
        let config = config_dir_of(&written).or_else(|| config_dir_of(&r.path));
        let report = rewrite_file(&written, skills_policy, config.as_deref())?;
        let mut h = String::new();
        crate::cmd::reshape::skills_line(&mut h, &report);
        print!("{h}");
    }

    // Carry the project's context files to the new home, so the ported session keeps its memory.
    if !no_context {
        if let (Some(src), Some(dst)) = (session.cwd.as_deref(), new_cwd.as_deref()) {
            carry_context(src, dst);
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

fn carry_context(src: &Path, dst: &Path) {
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
        println!("  ↳ carried context: {}", copied.join(", "));
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

pub(crate) fn cmd_resume(id: &str, harness: Option<String>, launch: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, _adapter) = resolve(id, want)?;
    let cwd = r.cwd.clone();
    let (program, args) = resume_command(r.harness, &r.id);

    if launch {
        let dir = cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        eprintln!("✦ launching: (cd {}) {} {}", home_rel(&dir), program, args.join(" "));
        let status = std::process::Command::new(&program)
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
/// harness's own resume command. Shared with `cv rewind`, which prints it for the session it made.
pub(crate) fn resume_lines(h: Harness, id: &str, cwd: Option<&Path>) -> Vec<String> {
    let (program, args) = resume_command(h, id);
    let mut out = Vec::new();
    if let Some(dir) = cwd {
        out.push(format!("cd {}", shell_quote(&dir.display().to_string())));
    }
    out.push(format!(
        "{} {}",
        program,
        args.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ")
    ));
    out
}

/// Best-known resume incantation per harness: the program + its args (the cwd is handled
/// separately, since most harnesses resume relative to the directory they're launched in). A
/// per-harness table, so a new harness states its own.
fn resume_command(h: Harness, id: &str) -> (String, Vec<String>) {
    match h {
        Harness::Claude => ("claude".into(), vec!["--resume".into(), id.into()]),
        Harness::Codex => ("codex".into(), vec!["resume".into(), id.into()]),
        Harness::Grok => ("grok".into(), vec!["--resume".into(), id.into()]),
        Harness::OpenCode => ("opencode".into(), vec!["--session".into(), id.into()]),
        Harness::Gemini => ("gemini".into(), vec!["--resume".into(), id.into()]),
        Harness::Hermes => ("hermes".into(), vec!["resume".into(), id.into()]),
        Harness::OpenClaw => ("openclaw".into(), vec!["--resume".into(), id.into()]),
        Harness::Kimi => ("kimi".into(), vec!["--resume".into(), id.into()]),
        // Kimi Code (`kimi --session <id>` / `-S`): the native id is `session_<uuid>`; cv keys the
        // session by the bare uuid so prefixes work like every other harness.
        Harness::KimiCode => ("kimi".into(), vec!["--session".into(), format!("session_{id}")]),
        Harness::Qwen => ("qwen".into(), vec!["--resume".into(), id.into()]),
        Harness::Devin => ("devin".into(), vec!["--resume".into(), id.into()]),
        // Desktop/IDE apps, Goose, the account exports and OpenSession documents have no
        // documented CLI resume.
        Harness::Cursor
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => (
            format!("# no CLI resume for {h}; open the app and find the session"),
            vec![id.into()],
        ),
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
}
