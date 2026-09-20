//! `cv port` / `cv resume` — producing a copy of a session that runs elsewhere (another harness,
//! another working directory, or both), and launching one in its native harness.
//!
//! `port` is the one verb for "make this session runnable somewhere else": 0.10's `convert` (same
//! place, different harness) and `port` (different place) were the same act, so `--harness` picks
//! the target harness (default: the source's) and `--cwd` the new home. The source is never touched.

use crate::util::{home_rel, parse_harness, resolve};
use anyhow::{bail, Context, Result};
use cv_core::ir::{Harness, Session, SessionRef};
use cv_core::{Adapter, EmitOptions, ParseOptions};
use std::fs;
use std::path::{Path, PathBuf};

/// Parse a session at the fidelity the port needs. Same-harness (a pure rehome) parses
/// **format-complete** so the carrier/replay machinery preserves every record (meta lines, compact
/// boundaries, exhaustive `extra`) — the emitter replays them verbatim. Cross-harness sticks to the
/// plain full-fidelity parse: carriers hold *source*-native records that no other emitter can
/// replay (they'd just surface as empty turns).
fn parse_for_emit(adapter: &dyn Adapter, r: &SessionRef, to_h: Harness) -> Result<Session> {
    if r.harness == to_h {
        let mut s = cv_core::stream::collect_with(adapter, r, &ParseOptions::complete())?;
        s.materialize();
        Ok(s)
    } else {
        adapter.parse(r)
    }
}

pub(crate) fn cmd_port(
    id: &str,
    harness: Option<String>,
    cwd: Option<PathBuf>,
    out: Option<PathBuf>,
    no_context: bool,
    strict: bool,
) -> Result<()> {
    // The source harness rides on the id (`codex:019e…`); `--harness` is the TARGET.
    let (r, adapter) = resolve(id, None)?;
    let to_h = match &harness {
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
        None => r.harness, // a pure rehome
    };
    let session = parse_for_emit(adapter.as_ref(), &r, to_h)?;
    let new_cwd = cwd.clone();
    emit_session(
        &session,
        to_h,
        out,
        EmitOptions {
            new_cwd: cwd,
            new_id: None,
            strict,
        },
    )?;

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
        if to.exists() {
            eprintln!("  ↳ context: {name} already exists at target — left as-is");
            continue;
        }
        match fs::create_dir_all(dst).and_then(|_| fs::copy(&from, &to)) {
            Ok(_) => copied.push(*name),
            Err(e) => eprintln!("  ↳ context: couldn't copy {name}: {e}"),
        }
    }
    if !copied.is_empty() {
        println!("  ↳ carried context: {}", copied.join(", "));
    }
}

pub(crate) fn emit_session(session: &Session, to_h: Harness, out: Option<PathBuf>, opts: EmitOptions) -> Result<()> {
    if !cv_core::emit::supported_targets().contains(&to_h) {
        bail!(
            "emitting to {to_h} isn't supported yet — the source parses fine ({} messages), but the \
             {to_h} emitter is still TODO (supported: {})",
            session.messages.len(),
            cv_core::emit::supported_targets()
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let out_dir = match out {
        Some(d) => d,
        None => cv_core::harness::for_harness(to_h)
            .and_then(|a| a.storage_root())
            .with_context(|| format!("{to_h} doesn't appear installed; pass --out <dir> to write somewhere"))?,
    };
    // Verified emit: after writing, re-parse the output with the target's own adapter and diff it
    // against the source IR, so a lossy conversion is *visible* instead of silent.
    let (res, warnings) = cv_core::emit::emit_verified(session, to_h, &out_dir, &opts)?;
    println!("✦ wrote {} ({})", res.path.display(), res.new_id);
    if let Some(hint) = res.resume_hint {
        println!("  ↳ {hint}");
    }
    for w in &warnings {
        eprintln!("  ⚠ lossy: {w}");
    }
    Ok(())
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
    if let Some(dir) = &cwd {
        println!("cd {}", shell_quote(&dir.display().to_string()));
    }
    println!("{} {}", program, args.join(" "));
    Ok(())
}

/// Best-known resume incantation per harness: the program + its args (the cwd is handled
/// separately, since most harnesses resume relative to the directory they're launched in).
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
        // Desktop/IDE apps (and any future harness) have no documented CLI resume.
        _ => (
            format!("# no CLI resume for {h}; open the app and find the session"),
            vec![id.into()],
        ),
    }
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'~' | b'+' | b':' | b'@'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
