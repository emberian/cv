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
/// Resolve the caller-supplied `--declassify` term list from a CSV flag and/or a file (one term per
/// line, `#` comments + blanks ignored). Terms are lowercased + de-duplicated. cv ships NO built-in
/// list — the terms are always external (config/data), so cv's own source carries no domain shitlist.
pub(crate) fn resolve_declassify_tokens(csv: Option<String>, file: Option<PathBuf>) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |t: &str| {
        let t = t.trim().to_ascii_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    };
    if let Some(csv) = csv {
        for t in csv.split(',') {
            push(t);
        }
    }
    if let Some(path) = file {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading --declassify-tokens-file {}", path.display()))?;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("");
            push(line);
        }
    }
    Ok(out)
}

pub(crate) struct PruneArgs<'a> {
    pub id: &'a str,
    pub harness: Option<String>,
    pub min_size: usize,
    pub keep_last: usize,
    pub to: Option<String>,
    pub force: bool,
    pub drop: bool,
    pub thinking: bool,
    pub copy_resources: bool,
    pub revive: bool,
    pub window: Option<u64>,
    pub keep: Option<String>,
    pub declassify: bool,
    pub declassify_tokens: Option<String>,
    pub declassify_tokens_file: Option<PathBuf>,
    pub skills_policy: SkillsPolicy,
    pub plan: Option<PathBuf>,
    pub dry_run: bool,
    pub json: bool,
}

/// Read a `--plan` file; an unreadable or malformed plan is a usage error (exit 2).
fn read_plan(path: &std::path::Path) -> Result<cv_core::prune::Plan> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return usage(format!("--plan {}: {e}", path.display())),
    };
    cv_core::prune::Plan::parse(&text).or_else(|e| usage(format!("--plan {}: {e}", path.display())))
}

pub(crate) fn cmd_prune(args: PruneArgs<'_>) -> Result<()> {
    let PruneArgs {
        id,
        harness,
        min_size,
        keep_last,
        to,
        force,
        drop,
        thinking,
        copy_resources,
        revive,
        window,
        keep,
        declassify,
        declassify_tokens,
        declassify_tokens_file,
        skills_policy,
        plan,
        dry_run,
        json,
    } = args;
    let plan = plan.as_deref().map(read_plan).transpose()?;
    let keep_range = keep.as_deref().map(parse_range).transpose()?;
    let declassify_tokens = resolve_declassify_tokens(declassify_tokens, declassify_tokens_file)?;
    if declassify && declassify_tokens.is_empty() {
        eprintln!(
            "cv prune --declassify: no terms supplied (--declassify-tokens / \
             --declassify-tokens-file) — nothing will be snipped. cv ships no built-in list."
        );
    }
    let want = parse_harness(&harness)?;
    let (r, _adapter) = crate::cmd::distill::resolve_source(id, want)?;

    if !cv_core::harness::has_derived_copies(r.harness) {
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
        plan,
        dry_run,
        force,
        ..Default::default()
    };
    let res = cv_core::prune::prune_session(r.harness, &r.path, &opts).map_err(|e| match e
        .downcast_ref::<cv_core::prune::PlanRefused>()
    {
        Some(refused) => crate::util::UsageError(refused.to_string()).into(),
        None => e,
    })?;

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
    if let Some(p) = &res.plan {
        let _ = writeln!(
            h,
            "  plan: {} kept · {} précis · {} excerpt(s) · {} attachment(s) dropped · {} payload bytes removed",
            p.keep,
            p.precis,
            p.excerpt,
            p.attachments_dropped,
            human_bytes(p.bytes_saved),
        );
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

/// Report only an explicitly selected policy; keep preserves existing output.
pub(crate) fn skills_line(h: &mut String, s: &cv_core::harness::claude::skills::SkillsReport) {
    if !s.is_keep() {
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
pub(crate) struct RewindArgs<'a> {
    pub id: &'a str,
    pub harness: Option<String>,
    pub agent: Option<String>,
    pub at: Option<&'a str>,
    pub out: Option<PathBuf>,
    pub to: Option<String>,
    pub full: bool,
    pub thinking: Option<cv_core::emit::ThinkingMode>,
    pub skills_policy: SkillsPolicy,
    pub dry_run: bool,
    pub json: bool,
    pub generator: &'a str,
}

pub(crate) fn cmd_rewind(args: RewindArgs<'_>) -> Result<()> {
    let RewindArgs {
        id,
        harness,
        agent,
        at,
        out,
        to,
        full,
        thinking,
        skills_policy,
        dry_run,
        json,
        generator,
    } = args;
    let cut = parse_cut(at)?;
    let (r, _adapter) = crate::cmd::distill::resolve_source(id, parse_harness(&harness)?)?;
    let r = match &agent {
        Some(a) => crate::cmd::view::resolve_subagent(&r, a)?.session,
        None => r,
    };
    if !cv_core::harness::has_derived_copies(r.harness) {
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
            skills_policy,
            full,
            thinking,
            out_dir: out,
            new_id: to,
            dry_run,
            generator: generator.to_string(),
        },
    )?;
    let cwd = res.cwd.as_deref().map(std::path::Path::new);
    let resume = crate::cmd::port::resume_lines(r.harness, &res.new_id, cwd);
    // Claude Code finds a session by its project dir; a rewind written anywhere else must be moved
    // there first. (`--out` defaults to it, so this only fires for an explicit elsewhere.)
    let home = cv_core::rewind::project_dir(&r.path);
    let relocate = res.new_path.parent().is_some_and(|d| d != home);

    let mut v = serde_json::to_value(&res)?;
    v["harness"] = r.harness.as_str().into();
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
    // An `End` rewind copies the whole source, so there is usually nothing to say about a tail.
    let omitted = match res.omitted_lines {
        0 => String::new(),
        n => format!(" · omitted the {n} line(s) after it"),
    };
    let _ = writeln!(
        h,
        "  copied:  lines {}..={} — {} line(s), {}{closed}{omitted}",
        res.start_line,
        res.end_line,
        res.lines_written,
        human_bytes(res.bytes_written),
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
pub(crate) struct SpliceArgs<'a> {
    pub specs: &'a [String],
    pub harness: Option<String>,
    pub out: Option<PathBuf>,
    pub export: Option<String>,
    pub cwd: Option<PathBuf>,
    pub generate: bool,
    pub gen_model: Option<String>,
    pub thinking: cv_core::emit::ThinkingMode,
    pub resumable: Option<crate::cmd::distill::Target>,
    pub json: bool,
}

pub(crate) fn cmd_splice(args: SpliceArgs<'_>) -> Result<()> {
    let SpliceArgs {
        specs,
        harness,
        out,
        export,
        cwd,
        generate,
        gen_model,
        thinking,
        resumable,
        json,
    } = args;
    if let Some(format) = export.as_deref() {
        if !matches!(format, "json" | "md" | "markdown") {
            bail!("unknown export format {format:?} (use md or json)");
        }
    }
    let parsed: Vec<SpliceSpec> = specs.iter().map(|s| parse_splice_spec(s)).collect::<Result<_>>()?;

    let mut composition = cv_core::splice::Composition::default();
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
    finish_composed(ComposedArgs {
        session: spliced,
        to_h,
        explicit_to: harness.is_some(),
        out,
        export,
        cwd,
        generate,
        gen_model,
        thinking,
    })
}

/// Shared tail for splice: emit to a harness when `--harness`/`--out` is in play, otherwise
/// print a summary and (with `--export md|json`) the composed session to stdout.
pub(crate) struct ComposedArgs {
    pub session: Session,
    pub to_h: Harness,
    pub explicit_to: bool,
    pub out: Option<PathBuf>,
    pub export: Option<String>,
    pub cwd: Option<PathBuf>,
    pub generate: bool,
    pub gen_model: Option<String>,
    pub thinking: cv_core::emit::ThinkingMode,
}

fn finish_composed(args: ComposedArgs) -> Result<()> {
    let ComposedArgs {
        mut session,
        to_h,
        explicit_to,
        out,
        export,
        cwd,
        generate,
        gen_model,
        thinking,
    } = args;
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
    if let Some(staged) = staged {
        staged.copy_to(&mut std::io::stdout().lock())?;
    } else {
        println!("\n(use --export md|json to print it, or --harness <harness> [--out <dir>] to emit it)");
    }
    Ok(())
}

// ---------- dataset ----------

pub(crate) struct DatasetArgs<'a> {
    pub format: &'a str,
    pub harness: Option<String>,
    pub query: Option<String>,
    pub subagents: bool,
    pub limit: Option<usize>,
    pub min_messages: usize,
    pub redact: bool,
    pub redact_only: Option<String>,
    pub file: Option<PathBuf>,
    pub force: bool,
}

pub(crate) fn cmd_dataset(args: DatasetArgs<'_>) -> Result<()> {
    let DatasetArgs {
        format,
        harness,
        query,
        subagents,
        limit,
        min_messages,
        redact,
        redact_only,
        file,
        force,
    } = args;
    use std::io::Write;
    let out = file;
    let fmt = match format {
        "chatml" => cv_core::dataset::Format::Chatml,
        "sharegpt" => cv_core::dataset::Format::ShareGpt,
        _ => bail!("unknown format {format:?} (use chatml or sharegpt)"),
    };
    if let Some(path) = &out {
        crate::util::user_file_claim(path, force)?;
    }
    let want = parse_harness(&harness)?;
    let query = match query {
        Some(q) => Some(cv_core::SessionQuery::parse(&q).map_err(|e| anyhow::anyhow!(e))?),
        None => None,
    };
    // `--redact-only <classes>` scopes redaction to specific secret classes (and implies it);
    // plain `--redact` scrubs every class.
    let redact_opts = match &redact_only {
        Some(spec) => Some(cv_core::redact::RedactOptions {
            classes: cv_core::redact::RedactClasses::parse_only(spec).map_err(|e| anyhow::anyhow!(e))?,
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
    // --file must never truncate a transcript before discovery or source validation.
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
        if ctx.emit_ref(r)? == DatasetFlow::LimitHit {
            break;
        }
        // `--subagents`: a session fans out into a forest of sub-agent transcripts (Claude's
        // direct and `Workflow`-spawned agents, Codex's children, Kimi's and Hermes's delegates)
        // that `discover_all` never lists. For a distillation corpus those agent transcripts are
        // first-class training data — and a parent on one model often spawns sub-agents on
        // another, so a `model:` query that misses them misses most of the matching turns. Walk
        // the adapter's forest and emit each match too.
        if subagents {
            for info in cv_core::subagent_tree_of(r)? {
                if out.is_some() {
                    extra_sources.push(info.session.path.clone());
                }
                if ctx.emit_ref(&info.session)? == DatasetFlow::LimitHit {
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

/// Publish the dataset whole at `--file` (refused if taken unless `--force`). Naming one of its
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
enum DatasetFlow {
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
    /// Parse one ref and stream it as a record if it passes the filters. Returns [`DatasetFlow::LimitHit`]
    /// once `--limit` records have been emitted.
    fn emit_ref(&mut self, r: &cv_core::ir::SessionRef) -> Result<DatasetFlow> {
        if self.limit.is_some_and(|limit| *self.emitted >= limit) {
            return Ok(DatasetFlow::LimitHit);
        }

        // Catalog-cheap prefilter (everything but `model:`) prunes before we pay to parse.
        if let Some(q) = self.query {
            if !q.prefilter(r) {
                return Ok(DatasetFlow::Continue);
            }
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            return Ok(DatasetFlow::Continue);
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
                return Ok(DatasetFlow::Continue);
            }
            Err(error) => return Err(error),
        };
        if let Some(record) = record {
            record.publish(self.writer)?;
            *self.emitted += 1;
            if self.limit.is_some_and(|l| *self.emitted >= l) {
                return Ok(DatasetFlow::LimitHit);
            }
        } else {
            *self.skipped += 1;
        }
        Ok(DatasetFlow::Continue)
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
