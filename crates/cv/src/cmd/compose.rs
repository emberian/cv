//! `cv prune` / `cv splice` / `cv loom` / `cv dataset` — reshaping sessions into new ones.

use crate::cmd::port::emit_session;
use crate::util::{parse_harness, parse_range, resolve, short_id};
use anyhow::{bail, Context, Result};
use cv_core::ir::{Block, Harness, Message, Role, Session};
use cv_core::EmitOptions;
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
    drop: bool,
    thinking: bool,
    copy_resources: bool,
    revive: bool,
    window: Option<u64>,
    keep_range: Option<(usize, Option<usize>)>,
    declassify: bool,
    declassify_tokens: Vec<String>,
    dry_run: bool,
    json: bool, // also emit the report as one JSON object on stdout (the human report stays on stderr)
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, _adapter) = resolve(id, want)?;

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
        dry_run,
        ..Default::default()
    };
    let res = cv_core::prune::prune_session(&r.path, &opts)?;

    if json {
        // Machine-readable prune report: ONE snake_case JSON object on stdout — the human report
        // below stays on stderr (compose-family convention: status → stderr, data → stdout), so
        // stdout carries only the JSON and pipes cleanly. Dry-run honesty: nothing was written,
        // so new_path/sidecar_path/copied_resources are null, and new_id is null too unless --to
        // pinned it — the core mints a throwaway UUID a real run would NOT reuse (see `note`).
        let report_id = (!res.dry_run || pinned_id).then(|| res.new_id.clone());
        let note = match (res.dry_run, pinned_id) {
            (false, _) => None,
            (true, true) => Some("dry run — nothing written".to_string()),
            (true, false) => Some(
                "dry run — nothing written; a real run mints a fresh session id (pass --to to pin one)".to_string(),
            ),
        };
        // revived: false when revive is off or the recorded usage was already honest; else the
        // stale → honest rewrite detail (what the human report prints as "revived: …k → …k").
        let revived = match (res.revive_tokens, res.revive_old_tokens) {
            (Some(honest), Some(stale)) => serde_json::json!({
                "recorded_tokens_before": stale,
                "recorded_tokens_after": honest,
                "usage_records_rewritten": res.usage_rewritten,
            }),
            _ => serde_json::Value::Bool(false),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source_id": res.source_id,
                "new_id": report_id,
                "harness": r.harness.as_str(),
                "before_bytes": res.original_size,
                "after_bytes": res.new_size,
                "snipped_payloads": res.pruned_count,
                "image_blocks": res.image_blocks,
                "tokens_freed": res.est_context_tokens_saved,
                "dropped_turns": res.dropped_turns,
                "window_real_tokens": res.window_real_tokens,
                "revived": revived,
                "warnings": res.warnings,
                "new_path": (!res.dry_run).then(|| res.new_path.display().to_string()),
                "sidecar_path": (!res.dry_run)
                    .then(|| res.sidecar_path.as_ref().map(|p| p.display().to_string()))
                    .flatten(),
                "copied_resources": (!res.dry_run)
                    .then(|| res.copied_resources.as_ref().map(|p| p.display().to_string()))
                    .flatten(),
                "dry_run": res.dry_run,
                "note": note,
            }))?
        );
    }

    let pct = if res.original_size > 0 {
        100.0 * res.bytes_saved() as f64 / res.original_size as f64
    } else {
        0.0
    };
    let tag = if dry_run { " (dry run — nothing written)" } else { "" };
    eprintln!("✦ pruned {} → {}{tag}", short_id(&res.source_id), short_id(&res.new_id));
    eprintln!(
        "  {} payload(s) snipped ({} image(s)) · {} → {} on disk (−{:.0}%) · ~{} context tokens freed",
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
                " · kept ~{}k REAL tokens (Claude's recorded counts; resume loads ≈ this + ~30k overhead)",
                real / 1000
            ),
            None => " (sized by byte estimate — no usage records)".to_string(),
        };
        eprintln!(
            "  sliding window: dropped {} older turn(s){} — the source keeps the full history",
            res.dropped_turns, sized
        );
    }
    if revive {
        match (res.revive_tokens, res.revive_old_tokens) {
            (Some(honest), Some(stale)) => eprintln!(
                "  revived: recorded context {}k → {}k across {} usage record(s) — resume gate will let you back in",
                stale / 1000,
                honest / 1000,
                res.usage_rewritten,
            ),
            _ => eprintln!("  revive: recorded usage already honest (≤ loaded content) — nothing to rewrite"),
        }
    }
    for w in &res.warnings {
        eprintln!("  ⚠ {w}");
    }
    if !dry_run {
        eprintln!("  new session: {}", res.new_path.display());
        if let Some(sc) = &res.sidecar_path {
            eprintln!(
                "  sidecar:     {} (fetch an original: cv cat {} <tool_use_id>)",
                sc.display(),
                short_id(&res.new_id)
            );
        }
        if let Some(rc) = &res.copied_resources {
            eprintln!("  resources:   copied subagents/workflows → {}", rc.display());
        }
        eprintln!("  resume with: claude --resume {}", res.new_id);
    }
    Ok(())
}

/// Group-separate + unit a byte count (`5242880` → `5.0 MB`).
fn human_bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.1} {}", U[i])
    }
}

// ---------- splice / loom ----------

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
            let (start, end) = parse_range(range).with_context(|| format!("bad spec {spec:?}"))?;
            Ok(SpliceSpec {
                id: id.to_string(),
                start,
                end,
            })
        }
        Some((_, range))
            if !range.is_empty() && range.contains('-') && range.chars().all(|c| c.is_ascii_digit() || c == '-') =>
        {
            bail!(
                "bad spec {spec:?}: the `<id>:A-B` grammar is gone — use `<id>:A..B` (0-based, end-exclusive), \
                 `<id>:A..`, or `<id>:..B`"
            )
        }
        _ => Ok(SpliceSpec {
            id: spec.to_string(),
            start: 0,
            end: None,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_splice(
    specs: &[String],
    harness: Option<String>,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
) -> Result<()> {
    let parsed: Vec<SpliceSpec> = specs.iter().map(|s| parse_splice_spec(s)).collect::<Result<_>>()?;

    // Resolve + parse each spec's session FIRST, keeping the owners alive in a Vec so the spans can
    // borrow them. We pair each parsed session with the (start, end) it'll select.
    let mut owned: Vec<(Session, usize, Option<usize>)> = Vec::with_capacity(parsed.len());
    for sp in &parsed {
        let (r, adapter) = resolve(&sp.id, None)?;
        let session = adapter.parse(&r)?;
        owned.push((session, sp.start, sp.end));
    }

    let spans: Vec<cv_core::loom::Span<'_>> = owned
        .iter()
        .map(|(s, start, end)| cv_core::loom::Span {
            source: s,
            start: *start,
            end: *end,
        })
        .collect();

    // Target harness: --harness, else the first spec's source harness.
    let to_h = match &harness {
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
        None => owned
            .first()
            .map(|(s, _, _)| s.harness)
            .context("splice needs at least one session")?,
    };

    let spliced = cv_core::loom::splice(&spans, None, to_h);
    finish_composed(spliced, to_h, harness.is_some(), out, export, cwd, generate, gen_model)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_loom(
    base: &str,
    at: usize,
    graft: &str,
    from: usize,
    harness: Option<String>,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
) -> Result<()> {
    let (rb, ab) = resolve(base, None)?;
    let (rg, ag) = resolve(graft, None)?;
    let base_s = ab.parse(&rb)?;
    let graft_s = ag.parse(&rg)?;

    let grafted = cv_core::loom::graft(&base_s, at, &graft_s, from, None);
    // Default target harness is the base's (graft() already used it); honor --harness if given.
    let to_h = match &harness {
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
        None => base_s.harness,
    };
    // Re-stamp the harness if --harness overrode it (graft used base.harness).
    let mut grafted = grafted;
    grafted.harness = to_h;
    finish_composed(grafted, to_h, harness.is_some(), out, export, cwd, generate, gen_model)
}

/// Shared tail for splice/loom: emit to a harness when `--harness`/`--out` is in play, otherwise
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
) -> Result<()> {
    // The generative half of looming: grow the composed branch with an LLM-generated continuation.
    if generate {
        match cv_llm::available_provider() {
            Some(prov) => eprintln!("✦ looming a continuation via {prov}…"),
            None => bail!(
                "--generate needs an LLM provider: set OPENROUTER_API_KEY, ANTHROPIC_API_KEY, or \
                 LMSTUDIO_API_BASE=local (free local LM Studio)"
            ),
        }
        let text = cv_llm::generate(
            &session,
            &cv_llm::GenerateOptions {
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
                new_id: None,
                strict: false,
            },
        );
    }

    // Otherwise: print a summary, and with --export, dump the composed session to stdout.
    println!(
        "✦ composed {} ({}) · {} msg",
        short_id(&session.id),
        session.harness.as_str(),
        session.messages.len()
    );
    if let Some(prov) = session.extra.get("loom") {
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
        Some("json") => println!("{}", serde_json::to_string_pretty(&session)?),
        Some("md") | Some("markdown") => print!("{}", cv_core::render::to_markdown(&session)),
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
) -> Result<()> {
    use std::io::Write;
    if !matches!(format, "chatml" | "sharegpt") {
        bail!("unknown format {format:?} (use chatml or sharegpt)");
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
        .map(crate::cmd::query::TextSets::resolve)
        .unwrap_or_else(crate::cmd::query::TextSets::empty);
    let mut writer: Box<dyn Write> = match &out {
        Some(p) => Box::new(std::io::BufWriter::new(fs::File::create(p)?)),
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    };

    // Stream the corpus one session at a time (parse -> emit -> drop) so memory stays bounded.
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

    'outer: for r in cv_core::discover_all() {
        if let Some(w) = want {
            if r.harness != w {
                continue;
            }
        }
        if ctx.emit_ref(&r)? == Flow::LimitHit {
            break;
        }
        // `--subagents`: a Claude session fans out into a forest of sub-agent transcripts (both
        // directly-spawned and `Workflow`-spawned) that live in sidecar files `discover_all` never
        // lists. For a distillation corpus those agent transcripts are first-class training data —
        // and a parent on one model often spawns sub-agents on another, so a `model:` query that
        // misses them misses most of the matching turns. Walk the forest and emit each match too.
        if subagents && r.harness == cv_core::ir::Harness::Claude {
            for info in cv_core::harness::claude::subagent_tree(&r.path) {
                if ctx.emit_ref(&info.session)? == Flow::LimitHit {
                    break 'outer;
                }
            }
        }
    }
    writer.flush()?;
    let dest = out.as_ref().map(|p| format!(" to {}", p.display())).unwrap_or_default();
    let forest = if subagents { " (incl. sub-agent forest)" } else { "" };
    eprintln!("✦ {format}: wrote {emitted} record(s){dest}{forest} ({skipped} skipped)");
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
        // Catalog-cheap prefilter (everything but `model:`) prunes before we pay to parse.
        if let Some(q) = self.query {
            if !q.prefilter(r) {
                return Ok(Flow::Continue);
            }
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            return Ok(Flow::Continue);
        };
        // Lazy opts: large content stays a span (held as a 16-byte ref), and `write_record` streams
        // it straight to the writer in chunks — so even a 700 MB record never materializes. `extra`
        // isn't read by dataset rendering, so it's skipped too.
        let session = match cv_core::stream::collect_with(adapter.as_ref(), r, &cv_core::ParseOptions::lazy()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cv dataset: skipping {} ({}): {e:#}", r.id, r.harness);
                return Ok(Flow::Continue);
            }
        };
        if session.messages.len() < self.min_messages {
            *self.skipped += 1;
            return Ok(Flow::Continue);
        }
        // Full evaluation now that we have the parsed session (resolves model/git/text/tool/… that
        // the catalog-cheap prefilter left as Unknown).
        if let Some(q) = self.query {
            if !crate::cmd::query::matches_full(q, r, &session, self.text_sets) {
                *self.skipped += 1;
                return Ok(Flow::Continue);
            }
        }
        // Stream the record directly to the writer (chunked, JSON-escaped per chunk — never holds the
        // whole record). `--redact` scrubs one materialized message at a time inside the writer.
        let wrote = cv_core::dataset::write_record(&session, &mut self.writer, self.fmt, self.redact_opts)?;
        if wrote {
            writeln!(self.writer)?;
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
