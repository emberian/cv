//! `cv prune` / `cv rewind` / `cv splice` / `cv dataset` — reshaping sessions into new ones.

use crate::cmd::port::emit_session;
use crate::util::{dry_run_nulls, parse_harness, parse_range, report, short_id, usage};
use anyhow::{bail, Context, Result};
use cv_core::harness::claude::skills::SkillsPolicy;
use cv_core::ir::{human_bytes, Block, Harness, Message, Role, Session};
use cv_core::EmitOptions;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

// ---------- prune ----------

/// `cv prune <id>` — compact a Claude session into a new resumable one (see [`cv_core::prune`]).
/// Stashed originals come back with `cv cat <new-id> <tool_use_id>`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_prune(
    id: &str,
    harness: Option<String>,
    min_size: usize,
    keep_last: usize,
    to: Option<String>,
    force: bool,
    drop: bool,
    thinking: bool,
    copy_resources: bool,
    revive: bool,
    window: Option<u64>,
    keep_range: Option<(usize, Option<usize>)>,
    declassify: bool,
    declassify_tokens: Vec<String>,
    skills_policy: SkillsPolicy,
    dry_run: bool,
    json: bool, // also emit the report as one JSON object on stdout (the human report stays on stderr)
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, _adapter) = crate::cmd::distill::resolve_source(id, want)?;

    if r.harness != Harness::Claude {
        bail!(
            "cv prune currently supports Claude Code sessions only (got {})",
            r.harness
        );
    }

    let pinned_id = to.is_some(); // --to: the new id is caller-chosen, so a dry run can still report it
    let opts = cv_core::prune::PruneOptions {
        min_size,
        keep_last,
        drop,
        thinking,
        new_id: to,
        copy_resources,
        revive,
        window,
        keep_range,
        declassify,
        declassify_tokens, // caller-supplied (external); cv ships no built-in list. min_hits from Default (2).
        skills_policy,
        dry_run,
        force,
        ..Default::default()
    };
    let res = cv_core::prune::prune_session(&r.path, &opts)?;

    let mut v = serde_json::to_value(&res)?;
    v["harness"] = r.harness.as_str().into();
    // Existing JSON names describe a metadata rewrite: recorded_tokens_after is the estimate
    // written into usage, not new provider-measured prompt usage or verified resume acceptance.
    v["revived"] = match (res.revive_tokens, res.revive_old_tokens) {
        (Some(estimated), Some(recorded)) => serde_json::json!({
            "recorded_tokens_before": recorded,
            "recorded_tokens_after": estimated,
            "usage_records_rewritten": res.usage_rewritten,
        }),
        _ => serde_json::Value::Bool(false),
    };
    dry_run_nulls(
        &mut v,
        pinned_id,
        &["new_path", "sidecar_path", "copied_resources"],
        &["new_id"],
    );

    let mut h = String::new();
    let pct = if res.original_size > 0 {
        100.0 * res.bytes_saved() as f64 / res.original_size as f64
    } else {
        0.0
    };
    let tag = if dry_run { " (dry run — nothing written)" } else { "" };
    let _ = writeln!(
        h,
        "✦ pruned {} → {}{tag}",
        short_id(&res.source_id),
        short_id(&res.new_id)
    );
    let _ = writeln!(
        h,
        "  {} payload(s) snipped ({} image(s)) · {} → {} on disk (−{:.0}%) · ~{} token equivalents removed (byte estimate)",
        res.pruned_count,
        res.image_blocks,
        human_bytes(res.original_size),
        human_bytes(res.new_size),
        pct,
        res.est_context_tokens_saved,
    );
    if res.dropped_turns > 0 {
        let sized = match res.window_real_tokens {
            Some(real) => format!(
                " · kept ~{}k tokens by recorded-usage deltas (new prompt size and system overhead unmeasured)",
                real / 1000
            ),
            None => " (sized by byte estimate — no usage records)".to_string(),
        };
        let _ = writeln!(
            h,
            "  sliding window: dropped {} older turn(s){} — the source keeps the full history",
            res.dropped_turns, sized
        );
    }
    if revive {
        match (res.revive_tokens, res.revive_old_tokens) {
            (Some(estimated), Some(recorded)) => {
                let _ = writeln!(
                    h,
                    "  revived: rewrote recorded context {}k → estimated {}k across {} usage record(s) — resume acceptance unverified",
                    recorded / 1000,
                    estimated / 1000,
                    res.usage_rewritten,
                );
            }
            _ => {
                let _ = writeln!(h, "  revive: no recorded usage required an estimated-window rewrite");
            }
        }
    }
    skills_line(&mut h, &res.skills);
    for w in &res.warnings {
        let _ = writeln!(h, "  ⚠ {w}");
    }
    if !dry_run {
        let _ = writeln!(h, "  new session: {}", res.new_path.display());
        if let Some(sc) = &res.sidecar_path {
            let _ = writeln!(
                h,
                "  sidecar:     {} (fetch an original: cv cat {} <tool_use_id>)",
                sc.display(),
                short_id(&res.new_id)
            );
        }
        if let Some(rc) = &res.copied_resources {
            let _ = writeln!(h, "  resources:   copied subagents/workflows → {}", rc.display());
        }
        let _ = writeln!(h, "  resume with: claude --resume {}", res.new_id);
    }
    report(json, &h, &v)
}

/// The report line for a skills policy that dropped something (`keep` prints nothing).
pub(crate) fn skills_line(h: &mut String, s: &cv_core::harness::claude::skills::SkillsReport) {
    if s.policy != "keep" {
        let _ = writeln!(
            h,
            "  skills ({}): dropped {} restored skill(s), {} chars, kept {} · {} record(s) rewritten",
            s.policy, s.dropped, s.dropped_chars, s.kept, s.records
        );
    }
}

// ---------- rewind ----------

/// The `--at` grammar: an all-digit value is a message index (the `cv show --range` counting), any
/// other 7–64 hex digits a commit sha, nothing at all the last message. (A sha whose digits happen to
/// be all decimal reads as an index — pass more of it; a full sha never is.)
pub(crate) fn parse_cut(at: Option<&str>) -> Result<cv_core::rewind::CutAt> {
    use cv_core::rewind::{normalize_sha, CutAt};
    let Some(at) = at.map(str::trim) else {
        return Ok(CutAt::End);
    };
    if !at.is_empty() && at.bytes().all(|b| b.is_ascii_digit()) {
        return match at.parse() {
            Ok(n) => Ok(CutAt::Message(n)),
            Err(_) => usage(format!("--at {at:?}: message index out of range")),
        };
    }
    match normalize_sha(at) {
        Some(sha) => Ok(CutAt::Commit(sha)),
        None => usage(format!(
            "--at {at:?}: expected a message index (as `cv show --range` counts) or a commit sha (7–64 hex digits)"
        )),
    }
}

/// `cv rewind <id>` — derive the agent as of a past message or commit (see [`cv_core::rewind`]).
/// `<id>` is any session id, an `agent-…` sub-agent id, or a transcript path; `--agent` picks a
/// sub-agent under `<id>` the way `cv show --agent` does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_rewind(
    id: &str,
    harness: Option<String>,
    agent: Option<String>,
    at: Option<&str>,
    out: Option<PathBuf>,
    to: Option<String>,
    full: bool,
    thinking: Option<cv_core::emit::ThinkingMode>,
    skills_policy: SkillsPolicy,
    dry_run: bool,
    json: bool,
    generator: &str,
) -> Result<()> {
    let cut = parse_cut(at)?;
    let (r, _adapter) = crate::cmd::distill::resolve_source(id, parse_harness(&harness)?)?;
    let r = match &agent {
        Some(a) => crate::cmd::view::resolve_subagent(&r, a)?.session,
        None => r,
    };
    if r.harness != Harness::Claude {
        return usage(format!(
            "cv rewind currently supports Claude Code sessions only (got {}) — `cv splice {}:..N` composes a \
             prefix of any harness",
            r.harness,
            short_id(&r.id)
        ));
    }

    let pinned = to.is_some();
    let res = cv_core::rewind::rewind_session(
        &r,
        &cv_core::rewind::RewindOptions {
            at: cut,
            full,
            thinking,
            out_dir: out,
            new_id: to,
            dry_run,
            skills_policy,
            generator: generator.to_string(),
        },
    )?;
    let cwd = res.cwd.as_deref().map(std::path::Path::new);
    let resume = crate::cmd::port::resume_lines(Harness::Claude, &res.new_id, cwd);
    // Claude Code finds a session by its project dir; a rewind written anywhere else must be moved
    // there first. (`--out` defaults to it, so this only fires for an explicit elsewhere.)
    let home = cv_core::rewind::project_dir(&r.path);
    let relocate = res.new_path.parent().is_some_and(|d| d != home);

    let mut v = serde_json::to_value(&res)?;
    v["harness"] = "claude".into();
    v["resume"] = serde_json::json!(resume);
    dry_run_nulls(&mut v, pinned, &["new_path", "provenance_path"], &["new_id", "resume"]);

    let mut h = String::new();
    let tag = if res.dry_run {
        " (dry run — nothing written)"
    } else {
        ""
    };
    let _ = writeln!(
        h,
        "✦ rewound {} → {}{tag}",
        short_id(&res.source_id),
        short_id(&res.new_id)
    );
    let of = match (&res.subagent, &res.source_session_id) {
        (true, Some(p)) => format!(" · sub-agent of {} → emitted as a top-level session", short_id(p)),
        _ => String::new(),
    };
    let _ = writeln!(
        h,
        "  source:  {} ({}, sha256 {}…){of}",
        res.source_path.display(),
        human_bytes(res.source_bytes),
        &res.source_sha256[..12],
    );
    let why = match &res.evidence {
        Some(e) => format!(" · {} — `{}`", e.kind.label(), cv_core::ir::truncate(&e.command, 80)),
        None => String::new(),
    };
    let _ = writeln!(h, "  cut:     msg {} · line {}{why}", res.cut_msg_idx, res.cut_line);
    let from = match (res.start_mode, res.boundary_msg_idx, res.preserved_head) {
        ("full", ..) => "the first record (--full)".to_string(),
        (_, None, _) => "the first record (no compaction before the cut)".to_string(),
        (_, Some(b), true) => format!("the compaction at msg {b}, from its preserved segment"),
        (_, Some(b), false) => format!("the compaction at msg {b}"),
    };
    let _ = writeln!(
        h,
        "  start:   msg {} · line {} · {from}",
        res.start_msg_idx, res.start_line
    );
    let closed = if res.closed_tool_calls > 0 {
        format!(
            " (+{} tool result(s) past the cut, closing its open calls)",
            res.closed_tool_calls
        )
    } else {
        String::new()
    };
    let _ = writeln!(
        h,
        "  copied:  lines {}..={} — {} line(s), {}{closed} · omitted the {} line(s) after it",
        res.start_line,
        res.end_line,
        res.lines_written,
        human_bytes(res.bytes_written),
        res.omitted_lines,
    );
    let _ = writeln!(h, "  thinking: {}", res.thinking);
    skills_line(&mut h, &res.skills);
    for w in &res.warnings {
        let _ = writeln!(h, "  ⚠ {w}");
    }
    if res.cut_cwd.is_some() && res.cut_cwd != res.cwd {
        let _ = writeln!(
            h,
            "  at the cut the agent was working in {}",
            res.cut_cwd.as_deref().unwrap_or_default()
        );
    }
    if !res.dry_run {
        let _ = writeln!(h, "  new session: {}", res.new_path.display());
        let _ = writeln!(h, "  provenance:  {}", res.provenance_path.display());
        if relocate {
            let _ = writeln!(h, "  ⚠ claude resumes from {} — copy it there first", home.display());
        }
        let _ = writeln!(h, "  resume with (add --fork-session to keep the rewind reusable):");
        for line in &resume {
            let _ = writeln!(h, "    {line}");
        }
    }
    report(json, &h, &v)
}

// ---------- splice ----------

/// A parsed splice spec: which session, and a `[start, end)` window (end None → through the last).
struct SpliceSpec {
    id: String,
    start: usize,
    end: Option<usize>,
}

/// Parse `<id>:A..B` | `<id>:A..` | `<id>:..B` | `<id>` into a [`SpliceSpec`] — the same `A..B`
/// window grammar as `show --range` (0-based, end-exclusive). `<id>` may carry a `harness:` prefix
/// (`codex:019e…:0..12`), so the range is split off the END.
fn parse_splice_spec(spec: &str) -> Result<SpliceSpec> {
    match spec.rsplit_once(':') {
        Some((id, range)) if range.contains("..") => {
            let (start, end) =
                parse_range(range).map_err(|error| crate::util::UsageError(format!("bad spec {spec:?}: {error}")))?;
            Ok(SpliceSpec {
                id: id.to_string(),
                start,
                end,
            })
        }
        Some((_, range))
            if !range.is_empty() && range.contains('-') && range.chars().all(|c| c.is_ascii_digit() || c == '-') =>
        {
            usage(format!(
                "bad spec {spec:?}: the `<id>:A-B` grammar is gone — use `<id>:A..B` (0-based, end-exclusive), \
                 `<id>:A..`, or `<id>:..B`"
            ))
        }
        _ => Ok(SpliceSpec {
            id: spec.to_string(),
            start: 0,
            end: None,
        }),
    }
}

/// Where a splice goes. `resumable` is `--session` / `--agent-of` (a Claude transcript the harness
/// resumes, shared with `distill`); otherwise `--harness`/`--out` emit to a harness and nothing at
/// all prints a summary (with `--export md|json`, the composed session).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_splice(
    specs: &[String],
    harness: Option<String>,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
    thinking: cv_core::emit::ThinkingMode,
    resumable: Option<crate::cmd::distill::Target>,
    json: bool,
) -> Result<()> {
    if let Some(format) = export.as_deref() {
        if !matches!(format, "json" | "md" | "markdown") {
            bail!("unknown export format {format:?} (use md or json)");
        }
    }
    let parsed: Vec<SpliceSpec> = specs.iter().map(|s| parse_splice_spec(s)).collect::<Result<_>>()?;

    let mut composition = cv_core::loom::Composition::default();
    let mut first_path = None;
    for sp in &parsed {
        let (reference, adapter) = crate::cmd::distill::resolve_source(&sp.id, None)?;
        first_path.get_or_insert_with(|| reference.path.clone());
        let (session, total) = cv_core::emit::prepare_selected(adapter.as_ref(), &reference, sp.start, sp.end)?;
        composition.push(session, total, sp.start, sp.end);
    }
    let to_h = match &harness {
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
        None => composition
            .first_harness()
            .context("splice needs at least one session")?,
    };
    let mut spliced = composition.finish(None, to_h);
    // Generation demands owned text. Exports retain checked leaves through staged publication.
    if generate {
        spliced.materialize()?;
    }
    if let Some(target) = resumable {
        // `--session` / `--agent-of`: a resumable Claude transcript, the way `distill` writes one.
        // Nothing is elided (every selected message is carried whole), so there is no sidecar.
        let label = format!("splice: {}", specs.join(" "));
        let src = first_path.context("splice needs at least one session")?;
        let e = crate::cmd::distill::emit_to(&spliced, &target, &[], &src, &label)?;
        let mut h = String::new();
        let _ = writeln!(
            h,
            "✦ spliced {} ({} messages from {} span(s))",
            short_id(&spliced.id),
            spliced.messages.len(),
            specs.len()
        );
        if let Some(p) = &e.path {
            let _ = writeln!(h, "  ↳ wrote {}", p.display());
        }
        if let Some(r) = &e.resume {
            let _ = writeln!(h, "  ↳ resume: {r}");
        }
        return report(json, &h, &serde_json::to_value(&e)?);
    }
    finish_composed(
        spliced,
        to_h,
        harness.is_some(),
        out,
        export,
        cwd,
        generate,
        gen_model,
        thinking,
    )
}

/// Shared tail for splice: emit to a harness when `--harness`/`--out` is in play, otherwise
/// print a summary and (with `--export md|json`) the composed session to stdout.
#[allow(clippy::too_many_arguments)]
fn finish_composed(
    mut session: Session,
    to_h: Harness,
    explicit_to: bool,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
    thinking: cv_core::emit::ThinkingMode,
) -> Result<()> {
    // The generative half of looming: grow the composed branch with an LLM-generated continuation.
    if generate {
        match crate::llm::available_provider() {
            Some(prov) => eprintln!("✦ looming a continuation via {prov}…"),
            None => bail!(
                "--generate needs an LLM provider: set OPENROUTER_API_KEY, ANTHROPIC_API_KEY, or \
                 LMSTUDIO_API_BASE=local (free local LM Studio)"
            ),
        }
        let text = crate::llm::generate(
            &session,
            &crate::llm::GenerateOptions {
                model: gen_model,
                ..Default::default()
            },
        )?;
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::Text { text: text.into() });
        session.messages.push(m);
        eprintln!(
            "  ↳ grew the branch by 1 generated turn ({} msg total)",
            session.messages.len()
        );
    }

    // Emit path: an explicit --harness or an --out directory means "materialize this for a harness".
    if explicit_to || out.is_some() {
        if let Some(dir) = &cwd {
            session.cwd = Some(dir.clone());
        }
        return emit_session(
            &session,
            to_h,
            out,
            EmitOptions {
                new_cwd: cwd,
                thinking,
                ..Default::default()
            },
        )
        .map(drop);
    }

    // Complete checked demands before publishing either the report or the exported document.
    let staged = match export.as_deref() {
        Some("json" | "md" | "markdown") => {
            use std::io::Write;
            let mut stage = std::io::BufWriter::with_capacity(64 * 1024, cv_core::bytes::ByteStage::new());
            let resolver = session.resolver();
            if export.as_deref() == Some("json") {
                session.write_json_pretty(&mut stage, &resolver)?;
                stage.write_all(b"\n")?;
            } else {
                cv_core::render::write_markdown(&session, &mut stage, &resolver)?;
            }
            Some(stage.into_inner().map_err(|e| e.into_error())?.finish()?)
        }
        _ => None,
    };
    // Otherwise: print a summary, and with --export, dump the composed session to stdout.
    println!(
        "✦ composed {} ({}) · {} msg",
        short_id(&session.id),
        session.harness.as_str(),
        session.messages.len()
    );
    if let Some(prov) = session.cv_extra().and_then(|b| b.get("loom")) {
        if let Some(arr) = prov.as_array() {
            for p in arr {
                println!(
                    "  ↳ {} {}[{}..{}]",
                    p.get("harness").and_then(|v| v.as_str()).unwrap_or("?"),
                    p.get("source_id")
                        .and_then(|v| v.as_str())
                        .map(short_id)
                        .unwrap_or_default(),
                    p.get("start").and_then(|v| v.as_u64()).unwrap_or(0),
                    p.get("end").and_then(|v| v.as_u64()).unwrap_or(0),
                );
            }
        }
    }
    match export.as_deref() {
        None => {
            println!("\n(use --export md|json to print it, or --harness <harness> [--out <dir>] to emit it)");
        }
        Some("json" | "md" | "markdown") => {
            staged
                .context("composed export was not staged")?
                .copy_to(&mut std::io::stdout().lock())?;
        }
        Some(other) => bail!("unknown export format {other:?} (use md or json)"),
    }
    Ok(())
}

// ---------- dataset ----------

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_dataset(
    format: &str,
    harness: Option<String>,
    query: Option<String>,
    subagents: bool,
    limit: Option<usize>,
    min_messages: usize,
    redact: bool,
    redact_only: Option<String>,
    out: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    use std::io::Write;
    if !matches!(format, "chatml" | "sharegpt") {
        bail!("unknown format {format:?} (use chatml or sharegpt)");
    }
    if let Some(path) = &out {
        crate::util::user_file_claim(path, force)?;
    }
    let want = parse_harness(&harness)?;
    let query = match query {
        Some(q) => Some(cv_core::SessionQuery::parse(&q).map_err(|e| anyhow::anyhow!(e))?),
        None => None,
    };
    let fmt = match format {
        "chatml" => cv_core::dataset::Format::Chatml,
        _ => cv_core::dataset::Format::ShareGpt,
    };
    // `--redact-only <classes>` scopes redaction to specific secret classes (and implies it);
    // plain `--redact` scrubs every class.
    let redact_opts = match &redact_only {
        Some(spec) => Some(cv_core::redact::RedactOptions {
            classes: cv_core::redact::RedactClasses::parse_only(spec).map_err(|e| anyhow::anyhow!(e))?,
            ..Default::default()
        }),
        None => redact.then(cv_core::redact::RedactOptions::default),
    };
    // Pre-resolve any `text:` predicates against the full-text index (one search per needle).
    let text_sets = query
        .as_ref()
        .map(crate::cmd::query::text_sets)
        .transpose()?
        .unwrap_or_else(crate::cmd::query::TextSets::empty);
    // File exports stage the whole operation before opening the destination. In particular,
    // --out must never truncate a transcript before discovery or source validation.
    let sources = cv_core::discover_all();
    let mut extra_sources = Vec::new();
    let mut stage = cv_core::bytes::ByteStage::new();
    let mut writer: Box<dyn Write + '_> = match &out {
        Some(_) => Box::new(std::io::BufWriter::new(&mut stage)),
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    };

    // Each lazy pass stages a record until EOF and filters succeed, retaining no message history.
    let mut emitted = 0usize;
    let mut skipped = 0usize;
    let mut ctx = EmitCtx {
        writer: writer.as_mut(),
        query: query.as_ref(),
        text_sets: &text_sets,
        fmt,
        redact_opts: redact_opts.as_ref(),
        min_messages,
        limit,
        emitted: &mut emitted,
        skipped: &mut skipped,
    };

    'outer: for r in &sources {
        if let Some(w) = want {
            if r.harness != w {
                continue;
            }
        }
        if ctx.emit_ref(r)? == Flow::LimitHit {
            break;
        }
        // `--subagents`: a Claude session fans out into a forest of sub-agent transcripts (both
        // directly-spawned and `Workflow`-spawned) that live in sidecar files `discover_all` never
        // lists. For a distillation corpus those agent transcripts are first-class training data —
        // and a parent on one model often spawns sub-agents on another, so a `model:` query that
        // misses them misses most of the matching turns. Walk the forest and emit each match too.
        if subagents && r.harness == cv_core::ir::Harness::Claude {
            for info in cv_core::harness::claude::subagent_tree(&r.path) {
                if out.is_some() {
                    extra_sources.push(info.session.path.clone());
                }
                if ctx.emit_ref(&info.session)? == Flow::LimitHit {
                    break 'outer;
                }
            }
        }
    }
    writer.flush()?;
    drop(writer);
    if let Some(path) = &out {
        publish_dataset(
            stage,
            path,
            force,
            sources
                .iter()
                .map(|r| r.path.as_path())
                .chain(extra_sources.iter().map(|p| p.as_path())),
        )?;
    }
    let dest = out.as_ref().map(|p| format!(" to {}", p.display())).unwrap_or_default();
    let forest = if subagents { " (incl. sub-agent forest)" } else { "" };
    eprintln!("✦ {format}: wrote {emitted} record(s){dest}{forest} ({skipped} skipped)");
    Ok(())
}

/// Publish the dataset whole at `--out` (refused if taken unless `--force`). Naming one of its
/// own source transcripts is refused even with `--force`: the rename would replace it.
fn publish_dataset<'a>(
    stage: cv_core::bytes::ByteStage,
    path: &std::path::Path,
    force: bool,
    inputs: impl Iterator<Item = &'a std::path::Path>,
) -> Result<()> {
    if let Ok(target) = fs::metadata(path) {
        for input in inputs {
            let Ok(source) = fs::metadata(input) else { continue };
            #[cfg(unix)]
            let aliases = {
                use std::os::unix::fs::MetadataExt;
                source.dev() == target.dev() && source.ino() == target.ino()
            };
            #[cfg(not(unix))]
            let aliases = {
                let _ = (&source, &target);
                fs::canonicalize(input).ok() == fs::canonicalize(path).ok()
            };
            anyhow::ensure!(
                !aliases,
                "dataset output aliases source transcript: {}",
                input.display()
            );
        }
    }
    let claim = crate::util::user_file_claim(path, force)?;
    let bytes = stage.finish()?;
    cv_core::publish::publish_reader(path, "out", bytes.reader()?, claim)?;
    Ok(())
}

/// Whether the emit loop should keep going or stop because `--limit` was reached.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    LimitHit,
}

/// The shared state for emitting one session ref as a dataset record — so the top-level corpus walk
/// and the per-session sub-agent forest walk run identical parse → filter → write logic.
struct EmitCtx<'a> {
    writer: &'a mut dyn std::io::Write,
    query: Option<&'a cv_core::SessionQuery>,
    text_sets: &'a crate::cmd::query::TextSets,
    fmt: cv_core::dataset::Format,
    redact_opts: Option<&'a cv_core::redact::RedactOptions>,
    min_messages: usize,
    limit: Option<usize>,
    emitted: &'a mut usize,
    skipped: &'a mut usize,
}

impl EmitCtx<'_> {
    /// Parse one ref and stream it as a record if it passes the filters. Returns [`Flow::LimitHit`]
    /// once `--limit` records have been emitted.
    fn emit_ref(&mut self, r: &cv_core::ir::SessionRef) -> Result<Flow> {
        if self.limit.is_some_and(|limit| *self.emitted >= limit) {
            return Ok(Flow::LimitHit);
        }

        // Catalog-cheap prefilter (everything but `model:`) prunes before we pay to parse.
        if let Some(q) = self.query {
            if !q.prefilter(r) {
                return Ok(Flow::Continue);
            }
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            return Ok(Flow::Continue);
        };
        let record = match cv_core::dataset::stage_record(
            adapter.as_ref(),
            r,
            self.fmt,
            self.redact_opts,
            self.min_messages,
            self.query,
            self.text_sets,
        ) {
            Ok(record) => record,
            Err(error) if error.is::<cv_core::dataset::InputError>() => {
                eprintln!("cv dataset: skipping {} ({}): {error:#}", r.id, r.harness);
                return Ok(Flow::Continue);
            }
            Err(error) => return Err(error),
        };
        if let Some(record) = record {
            record.publish(self.writer)?;
            *self.emitted += 1;
            if self.limit.is_some_and(|l| *self.emitted >= l) {
                return Ok(Flow::LimitHit);
            }
        } else {
            *self.skipped += 1;
        }
        Ok(Flow::Continue)
    }
}

#[cfg(test)]
mod splice_spec_tests {
    use super::*;
    #[test]
    fn invalid_splice_windows_keep_spec_context_in_usage_errors() {
        for spec in ["alphasess:zz..3", "codex:alphasess:9..3", "alphasess:1-2"] {
            let error = parse_splice_spec(spec).err().expect("invalid range");
            let usage = error
                .downcast_ref::<crate::util::UsageError>()
                .expect("usage exit status");
            assert!(usage.0.contains(&format!("bad spec {spec:?}")), "{usage}");
            if spec.ends_with("1-2") {
                assert!(usage.0.contains("<id>:A..B"));
            }
        }
    }
}
