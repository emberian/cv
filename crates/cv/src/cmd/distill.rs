//! `cv distill` / `cv fork` — make a transcript a reshapeable lane context.
//!
//! `distill` compresses a session (typically a long sub-agent lane) into a context pack plus a
//! verbatim tail (see [`cv_core::distill`]) and emits it three ways:
//!  * the markdown pack on stdout or `--pack FILE` — a fresh agent's first message;
//!  * `--session`: a new resumable Claude session (`claude -p --resume <id> "<next quest>"`);
//!  * `--agent-of <root>`: a new sub-agent transcript inside a root session's `subagents/` dir, which
//!    that root resumes by sending it a message (`SendMessage` to the printed agent id).
//!
//! `fork` cuts a session at a message index and emits the verbatim prefix the same two ways, to
//! branch a lane's context and run variants from one point.
//!
//! Neither command ever writes to the source session. Elided tool outputs go to a sidecar next to
//! the new transcript (`<stem>.flat.jsonl`), which `cv cat <new> <tool_use_id>` reads.

use crate::util::{parse_harness, resolve, short_id, usage};
use anyhow::{bail, Context, Result};
use cv_core::distill::{distill, findings, resumable_session, DistillOptions, Distilled};
use cv_core::ir::{Block, Harness, Role, Session, SessionRef};
use cv_core::EmitOptions;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Where a reshaped session goes.
pub(crate) enum Target {
    /// Only the pack (stdout / `--pack`).
    PackOnly,
    /// A new main-thread session under the harness store (or `--out`), rehomed to `cwd` if given.
    Session { cwd: Option<PathBuf>, out: Option<PathBuf> },
    /// A new sub-agent transcript of this root session.
    AgentOf { root: String },
}

/// The destination the flags name.
pub(crate) fn target(
    session: bool,
    agent_of: Option<String>,
    cwd: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<Target> {
    match (session, agent_of) {
        (_, Some(root)) => {
            if cwd.is_some() || out.is_some() {
                return usage("--cwd/--out apply to --session; --agent-of writes next to the root transcript");
            }
            Ok(Target::AgentOf { root })
        }
        (true, None) => Ok(Target::Session { cwd, out }),
        (false, None) => {
            if cwd.is_some() || out.is_some() {
                return usage("--cwd/--out need --session");
            }
            Ok(Target::PackOnly)
        }
    }
}

/// Resolve a session spec: an existing transcript path (read as Claude unless `--harness` says
/// otherwise), else an id / `harness:id` / agent id through the normal resolver.
fn load(spec: &str, harness: Option<Harness>) -> Result<(Session, PathBuf)> {
    let p = Path::new(spec);
    let (r, adapter) = if p.is_file() {
        let h = harness.unwrap_or(Harness::Claude);
        let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or("session").to_string();
        let r = SessionRef {
            id,
            harness: h,
            path: p.to_path_buf(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let a = cv_core::harness::for_harness(h).with_context(|| format!("no adapter for {h}"))?;
        (r, a)
    } else {
        resolve(spec, harness)?
    };
    let mut s = adapter.parse(&r)?;
    s.materialize();
    // A sub-agent transcript read by path: its label lives in the sibling `<stem>.meta.json`
    // (the resolver path reads it through the parent's sub-agent tree instead).
    if p.is_file() {
        if let Some(meta) = std::fs::read_to_string(p.with_extension("meta.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            let bag = s.harness_extra_mut(Harness::Claude);
            for (k, key) in [("description", "agent_description"), ("agentType", "agent_type")] {
                if let Some(v) = meta.get(k).and_then(Value::as_str) {
                    bag.entry(key).or_insert_with(|| json!(v));
                }
            }
        }
    }
    let path = r.path.clone();
    Ok((s, path))
}

fn new_agent_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("a{}", &u[..16])
}

/// Rewrite emitted Claude lines into one stream's identity: a main-thread session (`agent = None`)
/// or a sub-agent of `root_id` with id `agent`. The emitter restores per-message harness facts —
/// including the SOURCE's `isSidechain`/`agentId` — so both are set explicitly here.
fn restamp(raw: &str, session_id: &str, agent: Option<&str>) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(mut v) = serde_json::from_str::<Value>(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        if agent.is_some() && v.get("type").and_then(Value::as_str) == Some("ai-title") {
            continue; // sub-agent transcripts carry no title record
        }
        if let Some(o) = v.as_object_mut() {
            if o.contains_key("sessionId") {
                o.insert("sessionId".into(), json!(session_id));
            }
            match agent {
                Some(a) => {
                    o.insert("isSidechain".into(), json!(true));
                    o.insert("agentId".into(), json!(a));
                }
                None => {
                    if o.contains_key("isSidechain") {
                        o.insert("isSidechain".into(), json!(false));
                    }
                    o.remove("agentId");
                }
            }
        }
        out.push_str(&v.to_string());
        out.push('\n');
    }
    out
}

/// What an emit produced.
#[derive(Default, serde::Serialize)]
pub(crate) struct Emitted {
    session_id: Option<String>,
    agent_id: Option<String>,
    path: Option<PathBuf>,
    sidecar_path: Option<PathBuf>,
    resume: Option<String>,
}

fn write_sidecar(transcript: &Path, elided: &[cv_core::distill::Elided]) -> Result<Option<PathBuf>> {
    if elided.is_empty() {
        return Ok(None);
    }
    let stem = transcript.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    let p = transcript.with_file_name(format!("{stem}.flat.jsonl"));
    let mut body = String::new();
    for e in elided {
        body.push_str(&cv_core::prune::sidecar_line(
            &e.tool_use_id,
            &e.tool_name,
            &e.input,
            &e.content,
        ));
        body.push('\n');
    }
    std::fs::write(&p, body).with_context(|| format!("writing sidecar {}", p.display()))?;
    Ok(Some(p))
}

/// Emit an IR session as Claude JSONL to `target`, with the elided outputs as its sidecar.
pub(crate) fn emit_to(
    ir: &Session,
    target: &Target,
    elided: &[cv_core::distill::Elided],
    source_path: &Path,
    description: &str,
) -> Result<Emitted> {
    match target {
        Target::PackOnly => Ok(Emitted::default()),
        Target::Session { cwd, out } => {
            let out_dir = match out {
                Some(d) => d.clone(),
                None => cv_core::harness::for_harness(Harness::Claude)
                    .and_then(|a| a.storage_root())
                    .context("Claude Code's store was not found; pass --out <dir>")?,
            };
            let opts = EmitOptions {
                new_cwd: cwd.clone(),
                new_id: None,
                strict: false,
                thinking: Default::default(),
            };
            let res = cv_core::emit(ir, Harness::Claude, &out_dir, &opts)?;
            let raw = std::fs::read_to_string(&res.path)?;
            std::fs::write(&res.path, restamp(&raw, &res.new_id, None))?;
            let sidecar = write_sidecar(&res.path, elided)?;
            let dir = cwd.clone().or_else(|| ir.cwd.clone());
            let resume = format!(
                "{}claude -p --resume {} \"<next instruction>\"",
                dir.map(|d| format!("cd {} && ", d.display())).unwrap_or_default(),
                res.new_id
            );
            Ok(Emitted {
                session_id: Some(res.new_id),
                agent_id: None,
                path: Some(res.path),
                sidecar_path: sidecar,
                resume: Some(resume),
            })
        }
        Target::AgentOf { root } => {
            let (root_path, root_id) = root_transcript(root)?;
            let sub = root_path
                .parent()
                .context("root transcript has no parent dir")?
                .join(&root_id)
                .join("subagents");
            std::fs::create_dir_all(&sub).with_context(|| format!("creating {}", sub.display()))?;
            let agent = new_agent_id();
            let dest = sub.join(format!("agent-{agent}.jsonl"));
            if dest.exists() {
                bail!("{} already exists — refusing to overwrite", dest.display());
            }
            // Emit into a private temp dir, then restamp into the root's sub-agent stream.
            let tmp = std::env::temp_dir().join(format!("cv-distill-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&tmp)?;
            let opts = EmitOptions::default();
            let res = cv_core::emit(ir, Harness::Claude, &tmp, &opts);
            let raw = res.as_ref().map(|r| std::fs::read_to_string(&r.path));
            let _ = std::fs::remove_dir_all(&tmp);
            let raw = raw.map_err(|e| anyhow::anyhow!("{e}"))??;
            std::fs::write(&dest, restamp(&raw, &root_id, Some(&agent)))?;
            // The meta sidecar is optional for resume, but carries the model and the label a
            // harness shows; inherit the source's when it has one.
            let src_meta = source_path.with_extension("meta.json");
            let mut meta: Value = std::fs::read_to_string(&src_meta)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({"agentType": "general-purpose"}));
            if let Some(o) = meta.as_object_mut() {
                o.insert("description".into(), json!(description));
                o.remove("toolUseId");
            }
            std::fs::write(sub.join(format!("agent-{agent}.meta.json")), meta.to_string())?;
            let sidecar = write_sidecar(&dest, elided)?;
            Ok(Emitted {
                session_id: Some(root_id),
                agent_id: Some(agent.clone()),
                path: Some(dest),
                sidecar_path: sidecar,
                resume: Some(format!(
                    "from the root session: SendMessage to=\"{agent}\" with the next instruction"
                )),
            })
        }
    }
}

/// A root session's transcript path and id, from a path or an id.
fn root_transcript(root: &str) -> Result<(PathBuf, String)> {
    let p = Path::new(root);
    if p.is_file() {
        let id = p
            .file_stem()
            .and_then(|s| s.to_str())
            .context("bad root path")?
            .to_string();
        return Ok((p.to_path_buf(), id));
    }
    let (r, _) = resolve(root, Some(Harness::Claude))?;
    if r.id.starts_with("agent-") {
        return usage(format!(
            "{root} is a sub-agent; --agent-of takes the ROOT session that should own the new agent"
        ));
    }
    Ok((r.path, r.id))
}

fn print_report(d: &Distilled, e: &Emitted) {
    let s = &d.stats;
    let before = s.recorded_context_tokens.unwrap_or(s.source_est_tokens);
    let ratio = if s.distilled_est_tokens > 0 {
        before as f64 / s.distilled_est_tokens as f64
    } else {
        0.0
    };
    eprintln!(
        "✦ distilled {} · {} msg, {} tool calls · context {}{} → ~{} tokens (pack ~{}, tail ~{} over {} calls) · {:.1}× smaller",
        short_id(&s.source_id),
        s.messages,
        s.tool_calls,
        before,
        if s.recorded_context_tokens.is_some() { " (recorded)" } else { " (est.)" },
        s.distilled_est_tokens,
        s.pack_est_tokens,
        s.tail_est_tokens,
        s.tail_tool_calls,
        ratio,
    );
    eprintln!(
        "  ↳ kept: brief, {} timeline entries, {} commits, {} errors; elided {} tool outputs",
        s.timeline_entries, s.commits, s.errors, s.elided_outputs
    );
    if let Some(p) = &e.path {
        eprintln!("  ↳ wrote {}", p.display());
    }
    if let Some(p) = &e.sidecar_path {
        eprintln!("  ↳ sidecar {}", p.display());
    }
    if let Some(r) = &e.resume {
        eprintln!("  ↳ resume: {r}");
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_distill(
    spec: &str,
    harness: Option<String>,
    opts: DistillOptions,
    upto: Option<usize>,
    with: &[String],
    pack: Option<PathBuf>,
    target: Target,
    json_out: bool,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (mut s, src_path) = load(spec, want)?;
    if let Some(n) = upto {
        if n == 0 || n > s.messages.len() {
            return usage(format!("--upto {n}: the session has {} messages", s.messages.len()));
        }
        s.messages.truncate(n);
    }
    let mut d = distill(&s, &opts);
    for other in with {
        let (o, _) = load(other, None)?;
        d.head_md.push_str(&findings(&o, &opts));
        d.head_md.push('\n');
    }
    if !with.is_empty() {
        d.stats.pack_est_tokens = (d.head_md.len() as f64 / 3.5).ceil() as u64;
        d.stats.distilled_est_tokens = d.stats.pack_est_tokens + d.stats.tail_est_tokens;
    }
    let description = format!(
        "distilled: {}",
        s.extra
            .get("claude")
            .and_then(|b| b.get("agent_description"))
            .and_then(Value::as_str)
            .unwrap_or(&s.label())
    );
    let emitted = match &target {
        Target::PackOnly => Emitted::default(),
        _ => {
            let ir = resumable_session(&s, &d, &opts);
            emit_to(&ir, &target, &d.elided, &src_path, &description)?
        }
    };
    if let Some(p) = &pack {
        std::fs::write(p, d.pack()).with_context(|| format!("writing {}", p.display()))?;
        eprintln!("  ↳ pack {}", p.display());
    }
    print_report(&d, &emitted);
    if json_out {
        let v = json!({"stats": d.stats, "emitted": emitted, "pack_path": pack});
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if pack.is_none() && matches!(target, Target::PackOnly) {
        print!("{}", d.pack());
    }
    Ok(())
}

pub(crate) fn cmd_fork(spec: &str, harness: Option<String>, at: usize, target: Target, json_out: bool) -> Result<()> {
    if matches!(target, Target::PackOnly) {
        return usage("cv fork needs a destination: --session or --agent-of <root>");
    }
    let want = parse_harness(&harness)?;
    let (mut s, src_path) = load(spec, want)?;
    if at == 0 || at > s.messages.len() {
        return usage(format!("--at {at}: the session has {} messages", s.messages.len()));
    }
    s.messages.truncate(at);
    // A call whose result falls after the cut cannot be replayed: drop it.
    let answered: std::collections::HashSet<String> = s
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();
    for m in s.messages.iter_mut().filter(|m| m.role == Role::Assistant) {
        m.content
            .retain(|b| !matches!(b, Block::ToolUse { id, .. } if !answered.contains(id)));
    }
    s.messages.retain(|m| !m.content.is_empty());
    cv_core::distill::linearize(&mut s.messages);
    s.lineage.forked_from = Some(s.id.clone());
    let label = s
        .extra
        .get("claude")
        .and_then(|b| b.get("agent_description"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| s.label());
    let e = emit_to(&s, &target, &[], &src_path, &format!("fork@{at}: {label}"))?;
    eprintln!(
        "✦ forked {} at message {at} ({} messages kept)",
        short_id(&s.id),
        s.messages.len()
    );
    if let Some(p) = &e.path {
        eprintln!("  ↳ wrote {}", p.display());
    }
    if let Some(r) = &e.resume {
        eprintln!("  ↳ resume: {r}");
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&e)?);
    }
    Ok(())
}
