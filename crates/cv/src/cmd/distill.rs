//! `cv distill` — make a transcript a reshapeable lane context.
//!
//! `distill` compresses a session (typically a long sub-agent lane) into a context pack plus a
//! verbatim tail (see [`cv_core::distill`]) and emits it three ways:
//!  * the markdown pack on stdout or `--pack FILE` — a fresh agent's first message;
//!  * `--session`: a new resumable Claude session (`claude -p --resume <id> "<next quest>"`);
//!  * `--agent-of <root>`: a new sub-agent transcript inside a root session's `subagents/` dir, which
//!    that root resumes by sending it a message (`SendMessage` to the printed agent id).
//!
//! The same two destinations serve `cv splice … --session | --agent-of` (a verbatim prefix,
//! `<id>:..N`, is the fork of a lane): [`Target`], [`target`] and [`emit_to`] are shared. Both
//! destinations are one `cv_core::emit` of the IR to Claude — the emitter owns resumability
//! (threading, open calls, sidechain identity), the selectors here only choose the messages.
//!
//! Neither command ever writes to the source session. Elided tool outputs go to a sidecar next to
//! the new transcript (`<stem>.flat.jsonl`), which `cv cat <new> <tool_use_id>` reads.

use crate::util::{parse_harness, report, resolve, short_id, usage};
use anyhow::{bail, Context, Result};
use cv_core::distill::{distill_pack, findings, prepare_distill, DistillOptions, Distilled, PreparedDistillation};
use cv_core::emit::Grown;
use cv_core::harness::claude::sidecar;
use cv_core::ir::{Harness, Session, SessionRef};
use cv_core::{EmitOptions, SidechainOf};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where a reshaped session goes.
pub(crate) enum Target {
    /// Only the pack (stdout / `--pack`).
    PackOnly,
    /// A new main-thread session under the harness store (or `--out`), rehomed to `cwd` if given.
    Session { cwd: Option<PathBuf>, out: Option<PathBuf> },
    /// A new sub-agent transcript of this root session; `model` overrides its meta's `model`.
    AgentOf { root: String, model: Option<String> },
}

/// The destination the flags name.
pub(crate) fn target(
    session: bool,
    agent_of: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<Target> {
    if model.is_some() && agent_of.is_none() {
        // clap's `requires` yields to `--session`'s conflict with `--agent-of`, so say it here.
        return usage("--model sets the new sub-agent's meta; it needs --agent-of");
    }
    match (session, agent_of) {
        (_, Some(root)) => {
            if cwd.is_some() || out.is_some() {
                return usage("--cwd/--out apply to --session; --agent-of writes next to the root transcript");
            }
            Ok(Target::AgentOf { root, model })
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

pub(crate) fn resolve_source(
    spec: &str,
    harness: Option<Harness>,
) -> Result<(SessionRef, Box<dyn cv_core::harness::Adapter>)> {
    let p = Path::new(spec);
    let resolved = if p.is_file() {
        let h = harness.unwrap_or_else(cv_core::harness::derived_host);
        let a = cv_core::harness::for_harness(h).with_context(|| format!("no adapter for {h}"))?;
        (SessionRef::from_path(p, h), a)
    } else {
        resolve(spec, harness)?
    };
    Ok(resolved)
}

/// Resolve a session spec: an existing transcript path (read as the derived host's, Claude's,
/// unless `--harness` says otherwise), else an id / `harness:id` / agent id through the normal resolver.
#[cfg(test)]
fn load(spec: &str, harness: Option<Harness>) -> Result<(Session, PathBuf)> {
    let (r, adapter) = resolve_source(spec, harness)?;
    let p = Path::new(spec);
    let mut s = adapter.parse(&r)?;
    s.materialize()?;
    enrich_path_meta(&mut s, p);
    let path = r.path.clone();
    Ok((s, path))
}

/// A distill source: the session read, its transcript's path, and where a read of a live
/// transcript stopped.
type Loaded = (Session, PathBuf, Option<Grown>);

/// Checked full-fidelity bodies with EOF metadata, owned only when a distill product demands them.
fn load_distill(spec: &str, harness: Option<Harness>) -> Result<Loaded> {
    let (reference, adapter) = resolve_source(spec, harness)?;
    let (mut session, _, grown) = cv_core::emit::prepare_selected(adapter.as_ref(), &reference, 0, None)?;
    enrich_path_meta(&mut session, Path::new(spec));
    Ok((session, reference.path, grown))
}

/// Selected prefix with full EOF metadata and original-count validation. Other load callers
/// retain their existing full-session contract.
fn load_upto(spec: &str, harness: Option<Harness>, upto: usize) -> Result<Loaded> {
    let (reference, adapter) = resolve_source(spec, harness)?;
    let (mut session, total, grown) = cv_core::emit::prepare_selected(adapter.as_ref(), &reference, 0, Some(upto))?;
    if upto == 0 || upto > total {
        return usage(format!("--upto {upto}: the session has {total} messages"));
    }
    enrich_path_meta(&mut session, Path::new(spec));
    Ok((session, reference.path, grown))
}

/// Findings retain checked bodies for the shared analyzer rather than owning discarded history.
fn load_findings(spec: &str) -> Result<Session> {
    Ok(load_distill(spec, None)?.0)
}

fn enrich_path_meta(s: &mut Session, p: &Path) {
    // A sub-agent transcript read by path: its label lives in the sibling `<stem>.meta.json`
    // (the resolver path reads it through the parent's sub-agent tree instead).
    if p.is_file() {
        sidecar::label_from_meta(s, p);
    }
}

fn new_agent_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("a{}", &u[..16])
}

/// What an emit produced.
#[derive(Default, serde::Serialize)]
pub(crate) struct Emitted {
    pub(crate) session_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) sidecar_path: Option<PathBuf>,
    pub(crate) resume: Option<String>,
}

fn write_sidecar(transcript: &Path, elided: &[cv_core::distill::Elided]) -> Result<Option<PathBuf>> {
    if elided.is_empty() {
        return Ok(None);
    }
    let stem = transcript.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    let p = transcript.with_file_name(format!("{stem}.flat.jsonl"));
    // The sidecar of a transcript this command just created: a taken name is another artifact.
    cv_core::distill::write_sidecar(&p, elided, cv_core::publish::Claim::New)
        .with_context(|| format!("writing sidecar {}", p.display()))?;
    Ok(Some(p))
}

fn resume_hint(id: &str, cwd: Option<&Path>) -> String {
    let command = format!(
        "claude -p --resume {} \"<next instruction>\"",
        crate::util::shell_quote(id)
    );
    cv_core::harness::resume_in(cwd, command)
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
                None => cv_core::harness::for_harness(cv_core::harness::derived_host())
                    .and_then(|a| a.root())
                    .context("Claude Code's store was not found; pass --out <dir>")?,
            };
            let opts = EmitOptions {
                new_cwd: cwd.clone(),
                ..Default::default()
            };
            let res = emit_resumable(ir, &out_dir, &opts)?;
            let sidecar = write_sidecar(&res.path, elided)?;
            let dir = cwd.clone().or_else(|| ir.cwd.clone());
            let resume = resume_hint(&res.new_id, dir.as_deref());
            Ok(Emitted {
                session_id: Some(res.new_id),
                agent_id: None,
                path: Some(res.path),
                sidecar_path: sidecar,
                resume: Some(resume),
            })
        }
        Target::AgentOf { root, model } => {
            let root = root_transcript(root)?;
            let sub = root
                .path
                .parent()
                .context("root transcript has no parent dir")?
                .join(&root.id)
                .join("subagents");
            let agent = new_agent_id();
            let dest = sub.join(format!("agent-{agent}.jsonl"));
            if dest.exists() {
                bail!("{} already exists — refusing to overwrite", dest.display());
            }
            // The emitter writes the sub-agent transcript in place: the root's id on every record,
            // the sidechain markers, no title record (`EmitOptions::sidechain_of`).
            let opts = EmitOptions {
                sidechain_of: Some(SidechainOf {
                    root: root.id.clone(),
                    agent: agent.clone(),
                }),
                ..Default::default()
            };
            let res = emit_resumable(ir, &sub, &opts)?;
            debug_assert_eq!(res.path, dest);
            // The meta sidecar is optional for resume, but carries the model and the label a
            // harness shows; inherit the source's when it has one.
            let meta = agent_meta(source_path, description, model.as_deref());
            let meta_path = sub.join(format!("agent-{agent}.meta.json"));
            cv_core::publish::publish_reader(
                &meta_path,
                "meta",
                meta.to_string().as_bytes(),
                cv_core::publish::Claim::New,
            )?;
            let sidecar = write_sidecar(&res.path, elided)?;
            Ok(Emitted {
                session_id: Some(root.id),
                agent_id: Some(agent.clone()),
                path: Some(res.path),
                sidecar_path: sidecar,
                resume: Some(format!(
                    "from the root session: SendMessage to=\"{agent}\" with the next instruction"
                )),
            })
        }
    }
}

fn agent_meta(source_path: &Path, description: &str, model: Option<&str>) -> Value {
    let mut object = sidecar::agent_meta(&source_path.with_extension("meta.json"));
    object.insert("description".into(), json!(description));
    object.remove("toolUseId");
    if let Some(model) = model {
        object.insert("model".into(), json!(model));
    }
    Value::Object(object)
}

/// Emit `ir` as a Claude transcript under `out_dir` and verify what was written: the fidelity
/// read-back (anything the IR carried that the file does not comes back as a warning) and the
/// resumability rules ([`cv_core::harness::claude::verify_claude_resumable`], an error — it means a bug in cv).
fn emit_resumable(ir: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<cv_core::EmitResult> {
    let (res, warnings) = cv_core::emit::emit_verified(ir, cv_core::harness::derived_host(), out_dir, opts)?;
    for w in &warnings {
        eprintln!("  ⚠ lossy: {w}");
    }
    Ok(res)
}

/// A root session's transcript, from a path or an id.
fn root_transcript(root: &str) -> Result<SessionRef> {
    let p = Path::new(root);
    let r = if p.is_file() {
        SessionRef::from_path(p, cv_core::harness::derived_host())
    } else {
        resolve(root, Some(cv_core::harness::derived_host()))?.0
    };
    if r.id.starts_with("agent-") {
        return usage(format!(
            "{root} is a sub-agent; --agent-of takes the ROOT session that should own the new agent"
        ));
    }
    Ok(r)
}

/// The human report (stderr side of [`report`]).
fn report_text(d: &Distilled, e: &Emitted) -> String {
    let mut h = String::new();
    let s = &d.stats;
    let before = s.recorded_context_tokens.unwrap_or(s.source_est_tokens);
    let ratio = if s.distilled_est_tokens > 0 {
        before as f64 / s.distilled_est_tokens as f64
    } else {
        0.0
    };
    let _ = writeln!(
                h,
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
    let _ = writeln!(
        h,
        "  ↳ kept: brief, {} timeline entries, {} commits, {} errors; elided {} tool outputs",
        s.timeline_entries, s.commits, s.errors, s.elided_outputs
    );
    if let Some(p) = &e.path {
        let _ = writeln!(h, "  ↳ wrote {}", p.display());
    }
    if let Some(p) = &e.sidecar_path {
        let _ = writeln!(h, "  ↳ sidecar {}", p.display());
    }
    if let Some(r) = &e.resume {
        let _ = writeln!(h, "  ↳ resume: {r}");
    }
    h
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_distill(
    spec: &str,
    harness: Option<String>,
    opts: DistillOptions,
    upto: Option<usize>,
    with: &[String],
    pack: Option<PathBuf>,
    force: bool,
    target: Target,
    json_out: bool,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    if let Some(path) = &pack {
        crate::util::user_file_claim(path, force)?;
    }
    let (s, src_path, grown) = match upto {
        Some(n) => load_upto(spec, want, n)?,
        None => load_distill(spec, want)?,
    };
    enum Products {
        Pack(Distilled),
        Resume(PreparedDistillation),
    }
    let mut products = if matches!(target, Target::PackOnly) {
        Products::Pack(distill_pack(&s, &opts)?)
    } else {
        Products::Resume(prepare_distill(&s, &opts)?)
    };
    let d = match &mut products {
        Products::Pack(d) => d,
        Products::Resume(prepared) => &mut prepared.distilled,
    };
    if let Some(grown) = &grown {
        d.note_grown(grown);
    }
    for other in with {
        let o = load_findings(other)?;
        d.append_findings(&findings(&o, &opts)?);
    }
    let description = format!("distilled: {}", d.label());
    let (d, emitted) = match products {
        Products::Pack(d) => (d, Emitted::default()),
        Products::Resume(prepared) => {
            let (d, ir) = prepared.into_resumable_session(&s);
            let emitted = emit_to(&ir, &target, &d.elided, &src_path, &description)?;
            (d, emitted)
        }
    };
    let mut h = report_text(&d, &emitted);
    if let Some(grown) = &grown {
        let _ = writeln!(h, "  ↳ read {grown}");
    }
    if let Some(p) = &pack {
        crate::util::write_user_file(p, d.pack().as_bytes(), force)?;
        let _ = writeln!(h, "  ↳ pack {}", p.display());
    }
    let mut out = json!({"stats": d.stats, "emitted": emitted, "pack_path": pack});
    if let Some(grown) = grown {
        out["grown"] = json!(grown);
    }
    report(json_out, &h, &out)?;
    if !json_out && pack.is_none() && matches!(target, Target::PackOnly) {
        print!("{}", d.pack());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Run alone in a fresh process with synthetic inputs. Includes loading, analysis, requested
    /// resume/sidecar emission and reporting; fixture construction and report inspection are outside.
    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated whole-command allocation control; requires explicit scratch inputs"]
    fn whole_command_heap_control() -> Result<()> {
        let source = std::env::var("CV_DISTILL_PROBE_SOURCE")?;
        let destination = PathBuf::from(std::env::var("CV_DISTILL_PROBE_DESTINATION")?);
        let mode = std::env::var("CV_DISTILL_PROBE_MODE")?;
        let opts = DistillOptions {
            keep_last: std::env::var("CV_DISTILL_PROBE_KEEP_LAST")?.parse()?,
            keep_tail_thinking: mode == "thinking-tail",
            tail_result_max: std::env::var("CV_DISTILL_PROBE_TAIL_MAX").map_or(Ok(6000), |s| s.parse())?,
            note_max: std::env::var("CV_DISTILL_PROBE_NOTE_MAX").map_or(Ok(6000), |s| s.parse())?,
            ledger: std::env::var("CV_DISTILL_PROBE_LEDGER").as_deref() != Ok("0"),
            ..Default::default()
        };
        let with: Vec<_> = std::env::var("CV_DISTILL_PROBE_WITH").ok().into_iter().collect();
        let upto = std::env::var("CV_DISTILL_PROBE_UPTO")
            .ok()
            .map(|n| n.parse())
            .transpose()?;
        let target = match mode.as_str() {
            "resume" => Target::Session {
                cwd: None,
                out: Some(destination.join("resume")),
            },
            "agent" => Target::AgentOf {
                root: source.clone(),
                model: Some("probe-model".into()),
            },
            _ => Target::PackOnly,
        };
        let json_out = std::env::var("CV_DISTILL_PROBE_REPORT").as_deref() != Ok("human");
        let pack = (mode != "stdout").then(|| destination.join("pack.md"));
        let baseline = cv_testkit::heap::begin();
        let result = cmd_distill(&source, None, opts, upto, &with, pack, false, target, json_out);
        let heap = cv_testkit::heap::end(baseline);
        eprintln!("CV_DISTILL_HEAP {}", serde_json::to_string(&heap)?);
        result
    }
    #[test]
    fn ordinary_loader_preserves_relative_direct_path_and_path_metadata() -> Result<()> {
        let world = cv_testkit::World::new("distill-relative-path");
        let path = world.base.join("agent.jsonl");
        std::fs::write(&path, concat!(
            "{\"type\":\"user\",\"uuid\":\"u\",\"cwd\":\"/tmp/first\",\"message\":{\"role\":\"user\",\"content\":\"brief λ\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"a\",\"cwd\":\"/tmp/final\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]}}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"EOF title\"}\n",
        ))?;
        std::fs::write(
            path.with_extension("meta.json"),
            r#"{"description":"relative agent","agentType":"reviewer"}"#,
        )?;
        let mut relative = PathBuf::new();
        for _ in std::env::current_dir()?.components().skip(1) {
            relative.push("..");
        }
        relative.push(path.strip_prefix("/")?);
        for spec in [relative.to_str().unwrap(), path.to_str().unwrap()] {
            let (expected, old_path) = load(spec, None)?;
            let (actual, new_path, _) = load_distill(spec, None)?;
            assert_eq!(old_path, Path::new(spec));
            assert_eq!(new_path, old_path);
            assert_eq!(serde_json::to_value(&actual)?, serde_json::to_value(&expected)?);
            assert_eq!(actual.cwd.as_deref(), Some(Path::new("/tmp/first")));
            assert_eq!(actual.title.as_deref(), Some("EOF title"));
            assert_eq!(
                actual.harness_extra(Harness::Claude).unwrap()["agent_description"],
                "relative agent"
            );
            assert_eq!(
                distill_pack(&actual, &Default::default())?.pack(),
                prepare_distill(&expected, &Default::default())?.distilled.pack()
            );
        }
        Ok(())
    }
    #[test]
    fn findings_loader_retains_checked_ignored_bodies_and_matches_full_output() -> Result<()> {
        let world = cv_testkit::World::new("findings-loader");
        let path = world.base.join("peer.jsonl");
        let rows = [
            json!({"type":"user","uuid":"u","message":{"role":"user","content":"Read /tmp/input.md."}}),
            json!({"type":"assistant","uuid":"a","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"ignored λ\n".repeat(65536)},
                {"type":"text","text":"Chose the simple implementation.\n### embedded heading\nUnicode λ"}
            ]}}),
        ];
        std::fs::write(&path, rows.iter().map(|row| format!("{row}\n")).collect::<String>())?;
        let spec = path.to_str().unwrap();
        let lazy = load_findings(spec)?;
        assert!(lazy.messages.iter().flat_map(|m| &m.content).any(|block| matches!(
            block,
            cv_core::Block::Thinking {
                text: cv_core::Text::Source(_),
                ..
            }
        )));
        let (full, _) = load(spec, None)?;
        assert_eq!(
            cv_core::distill::findings(&lazy, &Default::default())?,
            cv_core::distill::findings(&full, &Default::default())?
        );
        std::fs::write(&path, b"changed")?;
        assert!(cv_core::distill::findings(&lazy, &Default::default()).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn distill_resume_hint_roundtrips_shell_arguments_without_expanding_paths() -> Result<()> {
        let world = cv_testkit::World::new("distill-resume-shell");
        let cwd = world.base.join("project's space; $(touch unexpected)");
        std::fs::create_dir(&cwd)?;
        let id = "session's id; $(touch unexpected)";
        let hint = resume_hint(id, Some(&cwd));
        // A local stub prints argv; this never launches a harness or writes its stores.
        let script = format!("claude() {{ printf '%s\n' \"$PWD\" \"$@\"; }}; {hint}");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(&world.base)
            .output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let lines: Vec<_> = String::from_utf8(output.stdout)?.lines().map(str::to_string).collect();
        assert_eq!(std::path::PathBuf::from(&lines[0]).canonicalize()?, cwd.canonicalize()?);
        assert_eq!(&lines[1..], &["-p", "--resume", id, "<next instruction>"]);
        assert!(!world.base.join("unexpected").exists());
        assert!(!cwd.join("unexpected").exists());
        assert_eq!(
            resume_hint("plain-id", None),
            "claude -p --resume plain-id \"<next instruction>\""
        );
        Ok(())
    }

    #[test]
    fn distill_sidecar_empty_roster_leaves_existing_artifact_untouched() -> Result<()> {
        let world = cv_testkit::World::new("distill-sidecar-empty");
        let transcript = world.base.join("session.jsonl");
        let sidecar = world.base.join("session.flat.jsonl");
        std::fs::write(&sidecar, b"existing")?;
        assert!(write_sidecar(&transcript, &[])?.is_none());
        assert_eq!(std::fs::read(sidecar)?, b"existing");
        Ok(())
    }

    #[test]
    fn distill_prefix_inherits_codex_and_opensession_full_parity() -> Result<()> {
        let world = cv_testkit::World::new("distill-prefix-other-harnesses");
        let codex = world.base.join("codex.jsonl");
        let rows = [
            json!({"timestamp":"2025-01-01T00:00:00Z","type":"session_meta","payload":{"id":"codex","cwd":"/tmp/project","source":"cli"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt λ"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"}}),
            json!({"type":"turn_context","payload":{"cwd":"/tmp/final","model":"model","approval_policy":"on-request"}}),
        ];
        std::fs::write(&codex, rows.iter().map(|row| format!("{row}\n")).collect::<String>())?;
        let opensession = world.base.join("wrapped.opensession.json");
        let mut original = Session::new("wrapped", Harness::Codex);
        for text in ["prompt λ", "reply", "tail"] {
            let mut message = cv_core::Message::new(cv_core::Role::User);
            message.content.push(cv_core::Block::Text { text: text.into() });
            original.messages.push(message);
        }
        std::fs::write(
            &opensession,
            serde_json::to_vec(&cv_core::harness::opensession::document(&original))?,
        )?;
        for (path, harness) in [(codex, Harness::Codex), (opensession, Harness::OpenSession)] {
            let spec = path.to_str().unwrap();
            let (full, _) = load(spec, Some(harness))?;
            assert!(!full.messages.is_empty());
            for n in [1, full.messages.len()] {
                let mut expected = full.clone();
                expected.messages.truncate(n);
                let (actual, _, _) = load_upto(spec, Some(harness), n)?;
                assert_eq!(
                    serde_json::to_value(actual)?,
                    serde_json::to_value(expected)?,
                    "{harness} prefix"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn distill_prefix_matches_full_load_truncate_and_eof_path_meta() -> Result<()> {
        let world = cv_testkit::World::new("distill-prefix");
        let path = world.base.join("agent.jsonl");
        let mut bytes = Vec::new();
        for index in 0..9 {
            serde_json::to_writer(
                &mut bytes,
                &json!({
                    "type":"user", "uuid":format!("u{index}"), "cwd":format!("/tmp/eof-{index}"),
                    "timestamp":format!("2025-01-01T00:00:0{index}Z"), "permissionMode":format!("mode-{index}"),
                    "message":{"role":"user", "content":if index == 0 { "selected λ".to_string() } else { "discarded".repeat(128 * 1024) }}
                }),
            )?;
            bytes.push(b'\n');
        }
        std::fs::write(&path, bytes)?;
        std::fs::write(
            path.with_extension("meta.json"),
            r#"{"description":"path agent","agentType":"reviewer"}"#,
        )?;
        let spec = path.to_str().unwrap();
        let (full, original_path) = load(spec, None)?;
        for n in [1, 3, 9] {
            let mut expected = full.clone();
            expected.messages.truncate(n);
            let (actual, actual_path, _) = load_upto(spec, None, n)?;
            assert_eq!(actual_path, original_path);
            assert_eq!(serde_json::to_value(&actual)?, serde_json::to_value(&expected)?);
            let opts = DistillOptions::default();
            let old = prepare_distill(&expected, &opts)?.distilled;
            let new = prepare_distill(&actual, &opts)?.distilled;
            assert_eq!(new.pack(), old.pack());
            assert_eq!(serde_json::to_value(new.stats)?, serde_json::to_value(old.stats)?);
        }
        for n in [0, 10] {
            let error = load_upto(spec, None, n).unwrap_err();
            assert_eq!(error.to_string(), format!("--upto {n}: the session has 9 messages"));
            assert!(error.downcast_ref::<crate::util::UsageError>().is_some());
        }
        Ok(())
    }
    #[test]
    fn subagent_root_path_is_rejected_and_metadata_is_always_an_object() -> Result<()> {
        let world = cv_testkit::World::new("distill-agent-target");
        let path = world.base.join("agent-child.jsonl");
        std::fs::write(&path, b"read-only source")?;
        let error = root_transcript(path.to_str().unwrap()).unwrap_err();
        assert!(error.downcast_ref::<crate::util::UsageError>().is_some());
        for raw in ["null", "false", "17", "[]", "\"string\""] {
            std::fs::write(path.with_extension("meta.json"), raw)?;
            let metadata = agent_meta(&path, "new description", Some("new-model"));
            assert_eq!(
                metadata,
                json!({"agentType":"general-purpose", "description":"new description", "model":"new-model"})
            );
        }
        std::fs::write(
            path.with_extension("meta.json"),
            r#"{"agentType":"reviewer","description":"old","model":"old-model","toolUseId":"source-link","extra":1}"#,
        )?;
        let metadata = agent_meta(&path, "new description", Some("new-model"));
        assert_eq!(
            metadata,
            json!({"agentType":"reviewer", "description":"new description", "model":"new-model", "extra":1})
        );
        let mut foreign = Session::new("other", Harness::Codex);
        enrich_path_meta(&mut foreign, &path);
        assert!(foreign.harness_extra(Harness::Claude).is_none());
        assert_eq!(std::fs::read(&path)?, b"read-only source");
        Ok(())
    }
}
