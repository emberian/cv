//! `cv search` / `cv index` — full-text and exact substring search.

use crate::util::{parse_harness, short_id};
use anyhow::Result;
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_search::{Found, Note, Source};
use std::io::Write;

pub(crate) struct SearchArgs<'a> {
    pub batch: bool,
    pub all: bool,
    pub query: &'a str,
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub filter_query: Option<String>,
    pub limit: usize,
    pub scan: bool,
    pub exact: bool,
    pub kind: Option<String>,
    pub live_bytes: Option<u64>,
    pub json: bool, // emit `{hits, total}` (session rows + score/snippet/provenance) instead of the table
    pub stats: bool,
    pub authority: crate::AuthorityArgs,
}

fn resolve_allowed_ids(
    want: Option<cv_core::Harness>,
    cwd: Option<&str>,
    filter_query: Option<String>,
    place_scope: Option<&cv_core::state::hub::AuthorityEnvelope>,
) -> Result<Option<std::collections::HashSet<String>>> {
    if filter_query.is_none() && place_scope.is_none() {
        return Ok(None);
    }
    let sq = crate::cmd::query::build(filter_query)?;
    if sq.is_none() {
        if let Some(scope) = place_scope.filter(|s| !s.includes("local")) {
            let mut out = std::collections::HashSet::new();
            if let Ok(hub) = cv_core::state::hub::HubStore::default_hub() {
                if let Ok(heads) = hub.heads_at_generation(None) {
                    for e in heads {
                        if !scope.includes(&e.producer_id) {
                            continue;
                        }
                        if want.is_some_and(|w| w.as_str() != e.harness) {
                            continue;
                        }
                        if let Some(needle) = cwd {
                            if !e.cwd.as_deref().is_some_and(|p| p.contains(needle)) {
                                continue;
                            }
                        }
                        out.insert(e.selector_string());
                    }
                }
            }
            return Ok(Some(out));
        }
    }
    let mut candidates = cv_core::try_sessions_for(want)?;
    if let Some(needle) = cwd {
        candidates.retain(|r| r.cwd.as_ref().is_some_and(|p| p.to_string_lossy().contains(needle)));
    }
    if let Some(scope) = place_scope {
        let mut selector_places: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Ok(hub) = cv_core::state::hub::HubStore::default_hub() {
            if let Ok(heads) = hub.heads_at_generation(None) {
                for e in heads {
                    selector_places.insert(e.selector_string(), e.producer_id);
                }
            }
        }
        candidates.retain(|r| {
            let p = selector_places.get(&r.id).map_or("local", String::as_str);
            scope.includes(p)
        });
    }
    if let Some(ref sq) = sq {
        let text = crate::cmd::query::text_sets(sq)?;
        crate::cmd::query::filter_refs(&mut candidates, sq, &text)?;
    }
    let mut out = std::collections::HashSet::with_capacity(candidates.len() * 2);
    for c in &candidates {
        out.insert(c.session_selector());
        out.insert(c.id.clone());
    }
    Ok(Some(out))
}

pub(crate) fn search_pinned_generation(
    gen: u64,
    query: &str,
    filter: cv_search::SearchFilter<'_>,
    scope: &cv_core::state::hub::AuthorityEnvelope,
    predicate: Option<&cv_core::SessionQuery>,
    limit: usize,
) -> Result<Found> {
    use cv_core::harness::Parse as _;
    let hub = cv_core::state::hub::HubStore::default_hub()?;
    let mut hits = Vec::new();
    let mut total = 0;
    for entry in hub.heads_at_generation(Some(gen))? {
        if !scope.includes(&entry.producer_id)
            || filter.harness.is_some_and(|h| h != entry.harness)
            || filter
                .cwd
                .is_some_and(|cwd| !entry.cwd.as_ref().is_some_and(|p| p.contains(cwd)))
        {
            continue;
        }
        if let Some(q) = predicate {
            if !entry.matches_query(hub.packs_dir(), q)? {
                continue;
            }
        }
        let b = cv_core::custody::backing_from_hub_head(hub.packs_dir(), entry)?
            .ok_or_else(|| anyhow::anyhow!("historical search payload unavailable at generation {gen}"))?;
        let session = b.parse(&b.reference)?;
        let mut matcher = cv_core::scan::Matcher::new(vec![query.to_lowercase()], 1);
        let resolver = session.resolver();
        for msg in session.messages.iter().filter(|m| filter.scope.holds(m.kind)) {
            cv_core::projection::searchable(&msg.content, &resolver, &mut matcher, &mut || Ok(()))?;
        }
        let excerpt = matcher.finish();
        if matcher.anchored() {
            total += 1;
            if hits.len() < limit {
                hits.push(
                    cv_core::scan::LiveHit {
                        title: cv_core::label_from(session.title.as_deref(), session.synth_title().as_deref()),
                        snippet: cv_core::scan::snippet(&excerpt.text, excerpt.at, excerpt.len),
                        session: b.reference,
                    }
                    .into(),
                );
            }
        }
    }
    Ok(Found {
        hits,
        source: Source::Live,
        notes: Vec::new(),
        total,
    })
}

pub(crate) fn cmd_search(args: SearchArgs<'_>) -> Result<()> {
    let SearchArgs {
        query,
        batch,
        all,
        harness,
        cwd,
        filter_query,
        limit,
        scan,
        exact,
        kind,
        live_bytes,
        json,
        stats,
        authority,
    } = args;
    let want = parse_harness(&harness)?;
    let scope = match kind.as_deref() {
        None => cv_search::Scope::Exchange,
        Some(k) => match cv_search::Scope::parse(k) {
            Some(scope) => scope,
            None => return crate::util::usage(format!("unknown --kind {k:?}: exchange, context or all")),
        },
    };
    if batch {
        if authority.is_active() {
            return crate::util::usage("batch search cannot apply place or generation constraints");
        }
        return batch_search(
            cv_search::SearchFilter {
                harness: want.map(|h| h.as_str()),
                cwd: cwd.as_deref(),
                scope,
            },
            limit,
        );
    }
    let live_allowed = !cv_core::env::frozen() && live_bytes != Some(0);
    let auth_envelope = crate::cmd::browse::evaluate_cli_authority(
        &authority,
        cwd.as_deref(),
        Some(query),
        false,
        !scan && !live_allowed && authority.as_of.is_none(),
    )?;

    if let Some(gen) = authority.as_of {
        let predicate = crate::cmd::query::build(filter_query)?;
        let filter = cv_search::SearchFilter {
            harness: want.map(|h| h.as_str()),
            cwd: cwd.as_deref(),
            scope,
        };
        let found = search_pinned_generation(
            gen,
            query,
            filter,
            auth_envelope.as_ref().expect("pinned scope"),
            predicate.as_ref(),
            limit,
        )?;
        let work = cv_search::SearchWork::default();
        return render(found, work, query, json, stats, auth_envelope.as_ref());
    }

    let allowed_ids = resolve_allowed_ids(
        want,
        cwd.as_deref(),
        filter_query,
        auth_envelope.as_ref().filter(|_| authority.place.is_some()),
    )?;
    let source = if scan {
        eprintln!("(--scan: scanning every transcript live; `cv index` makes search instant)");
        Source::Live
    } else {
        Source::Index
    };
    let filter = cv_search::SearchFilter {
        harness: want.map(|h| h.as_str()),
        cwd: cwd.as_deref(),
        scope,
    };
    let live_allowed = !cv_core::env::frozen() && live_bytes != Some(0);
    let fetch_limit = if all || limit == 0 {
        limit
    } else {
        limit
            .saturating_mul(2)
            .max(limit.saturating_add(4))
            .min(limit.saturating_add(16))
    };
    // The index (+ live suffix of unindexed changes) answers full-text search: an empty result
    // means "no match", and an unusable index is an error naming the cause and the fix, never a
    // silent full scan of every transcript.
    let result = if exact {
        cv_search::Searcher.search_exact_in_allowed_with_work(
            &cv_search::default_trigram_dir(),
            query,
            filter,
            false,
            allowed_ids.as_ref(),
            fetch_limit,
            live_bytes,
        )
    } else {
        cv_search::Searcher.search_allowed_with_work(
            query,
            filter,
            allowed_ids.as_ref(),
            fetch_limit,
            source,
            live_bytes,
        )
    };
    let (mut found, work) =
        match result {
            Ok(pair) => pair,
            Err(e) => return match source {
                Source::Index if cv_search::is_invalid_text_query(&e) || e.is::<cv_search::BadLiveBytes>() => {
                    crate::util::usage(format!("{e:#}"))
                }
                Source::Index if exact => Err(e.context(
                    "exact substring search needs a usable trigram index (build or refresh with `cv index --exact`)",
                )),
                Source::Index => Err(e.context(
                    "full-text search needs a usable index (or pass --scan for a slow live scan of every session)",
                )),
                Source::Live => Err(e),
            },
        };
    if all {
        found.hits.truncate(limit);
    } else {
        collapse_search_hits(&mut found, limit, live_allowed);
    }
    render(found, work, query, json, stats, auth_envelope.as_ref())
}

fn state_lineage_parent(id: &str) -> Option<String> {
    cv_core::lanes::session_lineage(id)
        .ancestors
        .into_iter()
        .find(|link| {
            link.dst_session_id == id
                && link.src_session_id != id
                && matches!(
                    link.edge_kind.as_str(),
                    "continues" | "fork" | "rewind" | "splice" | "distill"
                )
        })
        .map(|link| link.src_session_id)
}

fn collapse_search_hits(found: &mut Found, limit: usize, live_allowed: bool) {
    if found.hits.len() <= 1 {
        found.hits.truncate(limit);
        return;
    }
    let refs: Vec<Option<cv_core::SessionRef>> = found
        .hits
        .iter()
        .map(|h| {
            if h.agent_id.is_some() {
                return None;
            }
            if let Some(r) = &h.session {
                return Some(r.clone());
            }
            let sel = h.session_key.as_deref().unwrap_or(&h.id);
            cv_core::find(sel, cv_core::Harness::parse(&h.harness))
                .ok()
                .flatten()
                .map(|(r, _)| r)
        })
        .collect();
    let derived: Vec<Option<String>> = found
        .hits
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            if h.agent_id.is_some() {
                return None;
            }
            if live_allowed {
                refs[idx]
                    .as_ref()
                    .and_then(|s| s.derived_from())
                    .or_else(|| state_lineage_parent(&h.id))
            } else {
                state_lineage_parent(&h.id)
            }
        })
        .collect();
    if derived.iter().all(Option::is_none) {
        found.hits.truncate(limit);
        return;
    }
    let counts: Vec<Option<usize>> = refs
        .iter()
        .map(|r| {
            if live_allowed {
                r.as_ref().and_then(cv_core::lineage::effective_message_count)
            } else {
                r.as_ref().and_then(|s| s.message_count)
            }
        })
        .collect();
    let sizes: Vec<Option<u64>> = refs
        .iter()
        .map(|r| {
            if live_allowed {
                r.as_ref()
                    .and_then(|s| std::fs::metadata(&s.path).ok().map(|m| m.len()))
            } else {
                None
            }
        })
        .collect();

    let hits = std::mem::take(&mut found.hits);
    let indexed: Vec<(usize, cv_search::Hit)> = hits.into_iter().enumerate().collect();
    let (kept, collapsed) = cv_core::lineage::collapse_derived_chains_with_lookup(
        indexed,
        |(idx, h)| cv_core::lineage::ChainMeta {
            harness: h.harness.clone(),
            id: if h.agent_id.is_some() {
                format!("subagent:{idx}:{}", h.id)
            } else {
                h.id.clone()
            },
            derived_from: derived[*idx].clone(),
            message_count: counts[*idx],
            updated_at: h
                .updated_at
                .or_else(|| refs[*idx].as_ref().and_then(|r| r.updated_at.map(|t| t.timestamp()))),
            size_bytes: sizes[*idx],
        },
        |harness, pid| {
            if live_allowed {
                cv_core::find(pid, cv_core::Harness::parse(harness))
                    .ok()
                    .flatten()
                    .and_then(|(r, _)| r.derived_from())
                    .or_else(|| state_lineage_parent(pid))
            } else {
                state_lineage_parent(pid)
            }
        },
    );
    found.total = found.total.saturating_sub(collapsed.len());
    found.hits = kept.into_iter().map(|(_, h)| h).take(limit).collect();
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchControl {
    #[serde(default, deserialize_with = "explicit_string")]
    query: Option<String>,
    #[serde(default)]
    refresh: bool,
}

fn explicit_string<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    <String as serde::Deserialize>::deserialize(d).map(Some)
}

enum BatchOutcome {
    Answer {
        answer: cv_search::IndexedAnswer,
        refreshed: bool,
    },
    Refreshed,
}

/// A streaming batch pins the index once and refreshes in place on request; malformed queries
/// or failed refreshes do not discard the active snapshot or later requests.
fn batch_search(filter: cv_search::SearchFilter<'_>, limit: usize) -> Result<()> {
    use std::io::{BufRead, IsTerminal};
    anyhow::ensure!(
        !std::io::stdin().is_terminal(),
        "search --batch reads JSON query strings from piped stdin"
    );
    let mut prepared = cv_search::PreparedIndex::open(&cv_search::default_tantivy_dir())?;
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    // A malformed/oversized request is isolated to its line. Drain rejected input
    // without retaining it, then continue with the next query.
    const MAX_QUERY_LINE: u64 = 1024 * 1024;
    let mut input = std::io::stdin().lock();
    loop {
        let mut line = Vec::new();
        let read = std::io::Read::take(&mut input, MAX_QUERY_LINE + 1).read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let result: Result<BatchOutcome> = if read as u64 > MAX_QUERY_LINE {
            if !line.ends_with(b"\n") {
                input.skip_until(b'\n')?;
            }
            Err(anyhow::anyhow!("batch query line exceeds 1048576-byte input limit"))
        } else if line
            .iter()
            .find(|b| !matches!(**b, b' ' | b'\t' | b'\r' | b'\n'))
            .is_some_and(|&b| b == b'{')
        {
            serde_json::from_slice::<BatchControl>(&line)
                .map_err(anyhow::Error::from)
                .and_then(|req| match (req.refresh, req.query) {
                    (false, None) => Err(anyhow::anyhow!(
                        "batch request object must specify \"query\" or \"refresh\": true"
                    )),
                    (true, None) => {
                        prepared.refresh()?;
                        Ok(BatchOutcome::Refreshed)
                    }
                    (refresh, Some(query)) => {
                        if refresh {
                            prepared.refresh()?;
                        }
                        prepared
                            .search(&query, limit, filter, None)
                            .map(|answer| BatchOutcome::Answer {
                                answer,
                                refreshed: refresh,
                            })
                    }
                })
        } else {
            serde_json::from_slice::<String>(&line)
                .map_err(anyhow::Error::from)
                .and_then(|query| prepared.search(&query, limit, filter, None))
                .map(|answer| BatchOutcome::Answer {
                    answer,
                    refreshed: false,
                })
        };
        let row = match result {
            Ok(BatchOutcome::Answer { answer, refreshed }) => {
                let work = answer.search_work(if refreshed { prepared.setup_micros() } else { 0 });
                serde_json::json!({
                    "answer": answer,
                    "work": work,
                    "setup_micros": prepared.setup_micros(),
                })
            }
            Ok(BatchOutcome::Refreshed) => serde_json::json!({
                "refreshed": true,
                "snapshot_id": prepared.snapshot_id(),
                "indexed_documents": prepared.indexed_documents(),
                "setup_micros": prepared.setup_micros(),
            }),
            Err(error) => serde_json::json!({"error":format!("{error:#}"),"snapshot_id":prepared.snapshot_id()}),
        };
        serde_json::to_writer(&mut output, &row)?;
        writeln!(output)?;
        output.flush()?;
    }
    Ok(())
}

/// A note in the CLI's words. The sub-agent miss is prose for the table only.
fn note_line(note: &Note) -> Option<String> {
    match note {
        Note::Stale(s) => Some(format!("(note: {s})")),
        Note::Live(s) => Some(format!("(live: {s})")),
        Note::SnippetsUnavailable(e) => Some(format!(
            "(note: live snippets unavailable ({e}); showing indexed previews)"
        )),
        Note::Unscanned(s) => Some(format!("(note: {s})")),
        Note::NoSubagents => None,
    }
}

fn work_line(w: &cv_search::SearchWork) -> String {
    format!(
        "(work: basis={} index_docs_examined={} stored_previews_used={} discovery_sessions={} source_sessions_read={} source_bytes_read={} setup_micros={} eval_micros={})",
        w.basis,
        w.index_docs_examined,
        w.stored_previews_used,
        w.discovery_sessions,
        w.source_sessions_read,
        w.source_bytes_read,
        w.setup_micros,
        w.eval_micros,
    )
}

/// The footer: `N of M matches`, and how to see the rest when `--limit` cut them.
fn matches_line(shown: usize, total: usize) -> String {
    let more = if shown < total { "; use --limit for more" } else { "" };
    format!("{shown} of {total} matches{more}")
}

/// Render what the search found: the table with harness/short-id/date/title/snippet, the
/// `N of M matches` footer and the notes, or, with `json`, `{"hits": [...], "total": M}` on
/// stdout with the SAME hits in the same order — notes on stderr, an empty result `"hits": []` —
/// so the output pipes cleanly.
fn render(
    found: Found,
    work: cv_search::SearchWork,
    query: &str,
    json: bool,
    stats: bool,
    authority: Option<&cv_core::state::hub::AuthorityEnvelope>,
) -> Result<()> {
    let Found {
        hits,
        source,
        notes,
        total,
    } = found;
    if json {
        for line in notes.iter().filter_map(note_line) {
            eprintln!("{line}");
        }
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let rows = std::cell::LazyCell::new(cv_core::rows::SearchRowContext::new);
        write_json_rows_with_authority(&mut out, &hits, total, stats.then_some(&work), authority, |hit| {
            cv_search::row(hit, &rows)
        })?;
        out.flush()?;
        if hits.len() < total {
            eprintln!("({})", matches_line(hits.len(), total));
        }
        return Ok(());
    }
    if hits.is_empty() && source != Source::Live {
        println!("no matches for {query:?} (index; try `cv index` to refresh)");
        // The likely reasons a recent conversation isn't findable: the index has fallen behind, or
        // the wanted text lives in a *sub-agent* transcript the default index doesn't fold in.
        for note in &notes {
            match note {
                Note::NoSubagents => println!(
                    "(note: sub-agent transcripts aren't searched yet — rebuild with \
                     `cv index --subagents` to include the Agent/Workflow lanes)"
                ),
                other => println!("{}", note_line(other).unwrap_or_default()),
            }
        }
        if stats {
            eprintln!("{}", work_line(&work));
        }
        return Ok(());
    }
    for h in &hits {
        // Titles and snippets are transcript-derived (untrusted) — sanitize at the terminal
        // seam (G5); JSON surfaces stay raw.
        let title = sanitize_line(h.title.as_deref().unwrap_or_default());
        if source == Source::Live {
            let date = h
                .updated_at
                .and_then(|t| crate::util::fmt_local_ts(t, "%Y-%m-%d"))
                .unwrap_or_else(|| "----------".into());
            println!("{:8}  {:8}  {:10}  {}", h.harness, short_id(&h.id), date, title);
            println!("          … {}", sanitize_line(&h.snippet));
            continue;
        }
        // Dates ride on the hit straight from the index.
        let date = h
            .updated_at
            .or(h.created_at)
            .and_then(|t| crate::util::fmt_local_ts(t, "%Y-%m-%d"))
            .unwrap_or_else(|| "----------".into());
        // A folded-in sub-agent hit (`cv index --subagents`): its own id is `agent-<hex>`, so a
        // bare `short_id` would render the shared `agent-` prefix, useless for drill-in. Show the
        // bare agent id (what `cv show <id>` resolves) and tag the row with its parent + workflow so
        // the reader sees it's a lane, not a top-level session.
        let (id_disp, provenance) = match h.agent_id.as_deref() {
            Some(aid) => {
                let parent = h.parent_id.as_deref().map(short_id).unwrap_or_default();
                let wf = h.workflow.as_deref().map(|w| format!(" · ⟐{w}")).unwrap_or_default();
                (short_id(aid), format!("   ⤷ sub-agent of {parent}{wf}"))
            }
            None => (short_id(&h.id), String::new()),
        };
        println!("{:8}  {:8}  {:10}  {}{}", h.harness, id_disp, date, title, provenance);
        if !h.snippet.trim().is_empty() {
            println!("          … {}", truncate(&sanitize_line(&h.snippet), 120));
        }
    }
    if hits.is_empty() {
        println!("no matches for {query:?}");
    } else {
        println!("\n{}", matches_line(hits.len(), total));
    }
    // The disclosure on the hit path: a stale index may be missing the rows the query would have
    // caught, and a hit list that doesn't say so reads as complete.
    for line in notes.iter().filter_map(note_line) {
        println!("{line}");
    }
    if stats {
        eprintln!("{}", work_line(&work));
    }
    Ok(())
}

// Preserve the shared row contract, retaining only one projected row at a time rather than
// another complete Value array and serialized String.
#[derive(serde::Serialize)]
struct JsonFound<'a, R> {
    hits: R,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    work: Option<&'a cv_search::SearchWork>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authority: Option<&'a cv_core::state::hub::AuthorityEnvelope>,
}
struct JsonRows<'a, T, F> {
    rows: &'a [T],
    row: F,
}
impl<T, F: Fn(&T) -> serde_json::Value> serde::Serialize for JsonRows<'_, T, F> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.rows.len()))?;
        for row in self.rows {
            sequence.serialize_element(&(self.row)(row))?;
        }
        sequence.end()
    }
}
#[cfg(test)]
fn write_json_rows<T>(
    out: &mut impl Write,
    rows: &[T],
    total: usize,
    work: Option<&cv_search::SearchWork>,
    row: impl Fn(&T) -> serde_json::Value,
) -> Result<()> {
    write_json_rows_with_authority(out, rows, total, work, None, row)
}

fn write_json_rows_with_authority<T>(
    out: &mut impl Write,
    rows: &[T],
    total: usize,
    work: Option<&cv_search::SearchWork>,
    authority: Option<&cv_core::state::hub::AuthorityEnvelope>,
    row: impl Fn(&T) -> serde_json::Value,
) -> Result<()> {
    serde_json::to_writer_pretty(
        &mut *out,
        &JsonFound {
            hits: JsonRows { rows, row },
            total,
            work,
            authority,
        },
    )?;
    writeln!(out)?;
    Ok(())
}

pub(crate) fn cmd_index(
    rebuild: bool,
    subagents: bool,
    if_changed: bool,
    strict: bool,
    threads: Option<usize>,
    json: bool,
    exact: bool,
) -> Result<()> {
    let threads = cv_core::resolve_worker_threads(threads).map_err(|e| crate::util::UsageError(format!("{e:#}")))?;
    cv_core::with_worker_threads(threads, || {
        let dir = cv_search::default_tantivy_dir();
        let trigram_dir = cv_search::default_trigram_dir();
        let mut trigram_need = Some(true);
        if if_changed && !rebuild {
            // The timer gate: a stat-only probe (catalog inventory + one stat per session + the
            // index's own stamps) answers whether a pass would find work at all.
            let fts_need = cv_search::fts::pass_needed(&dir, subagents);
            trigram_need = if exact {
                cv_search::trigram::pass_needed(&trigram_dir, subagents)
            } else {
                Some(false)
            };
            match (fts_need, trigram_need) {
                (Some(false), Some(false)) => {
                    let hub = cv_core::state::hub::HubStore::default_hub()?;
                    if hub.latest_generation_fast()?.is_some() {
                        cv_search::fts::index_changed_heads(&hub)?;
                    }
                    if json {
                        let indexed = cv_search::fts::indexed_top_level_count(&dir).unwrap_or(0);
                        let out = serde_json::json!({
                            "indexed": indexed,
                            "changed": 0,
                            "removed": 0,
                            "errors": 0,
                            "path": dir,
                            "unreadable": [],
                        });
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        println!("index already current — nothing changed since the last pass (skipped)");
                    }
                    return Ok(());
                }
                (None, _) | (_, None) => eprintln!("  ↳ index unreadable — running the pass"),
                _ => eprintln!("  ↳ corpus moved since the last pass — indexing"),
            }
        }
        if exact && (!if_changed || rebuild || trigram_need != Some(false)) {
            let (synced, _) = cv_search::trigram::sync_all(&trigram_dir, rebuild, subagents, strict)?;
            eprintln!(
                "trigram: {} indexed, {} unchanged, {} unreadable",
                synced.indexed,
                synced.unchanged,
                synced.unreadable.len()
            );
        }

        eprintln!(
            "✦ {} full-text index{}…",
            if rebuild { "rebuilding" } else { "updating" },
            if subagents { " (+ sub-agent forest)" } else { "" }
        );
        let (indexed, _refs) = cv_search::index_all(None, rebuild, subagents, strict)?;
        if !json {
            println!(
                "indexed {} top-level session(s) → {}{}",
                indexed.total,
                dir.display(),
                if subagents {
                    " (sub-agent transcripts folded in)"
                } else {
                    ""
                }
            );
        }
        if json {
            let out = serde_json::json!({
                "indexed": indexed.total,
                "changed": indexed.changed,
                "removed": indexed.removed,
                "errors": indexed.errors,
                "path": dir,
                "unreadable": indexed.unreadable,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("events: extracted on the same pass → try `cv events <id>` / `cv touched <path>`");
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_search_json_is_the_pretty_hits_object_with_its_total() {
        for rows in [
            Vec::new(),
            vec![
                serde_json::json!({"id":"one","text":"雪\n\""}),
                serde_json::json!({"id":"two","score":null,"fields":[1,true,"é"]}),
            ],
        ] {
            let mut output = Vec::new();
            write_json_rows(&mut output, &rows, 7, None, Clone::clone).unwrap();
            let expected = serde_json::json!({"hits": rows, "total": 7});
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!("{}\n", serde_json::to_string_pretty(&expected).unwrap())
            );
        }
        let rows = vec![1, 2, 3];
        let visits = std::cell::RefCell::new(Vec::new());
        let mut out = Vec::new();
        write_json_rows(&mut out, &rows, 3, None, |row| {
            visits.borrow_mut().push(*row);
            serde_json::json!(row)
        })
        .unwrap();
        assert_eq!(*visits.borrow(), rows);
    }

    #[test]
    fn search_cwd_and_query_filter_and_bare_colons_work_end_to_end() {
        crate::on_main_sized_stack(|| {
            use clap::Parser;
            let cli = crate::Cli::try_parse_from([
                "cv",
                "search",
                "error: std::io::Error",
                "--cwd",
                "proj-alpha",
                "-q",
                "after:2026-06-01",
                "--limit",
                "5",
            ])
            .expect("cv search --cwd and -q should parse");
            match cli.cmd {
                crate::Cmd::Search {
                    query,
                    cwd,
                    filter_query,
                    limit,
                    ..
                } => {
                    assert_eq!(query.as_deref(), Some("error: std::io::Error"));
                    assert_eq!(cwd.as_deref(), Some("proj-alpha"));
                    assert_eq!(filter_query.as_deref(), Some("after:2026-06-01"));
                    assert_eq!(limit, 5);
                }
                _ => panic!("expected Cmd::Search"),
            }
        });

        let world = crate::test_world("search-cwd-query-colon");
        let _p1 = world.write_session(
            "sess-old-alpha",
            &[serde_json::json!({
                "type": "user",
                "cwd": "/Users/ember/dev/proj-alpha",
                "timestamp": "2026-01-15T12:00:00Z",
                "message": {"role": "user", "content": "error: std::io::Error in ~/dev/x:123"}
            })],
        );
        let _p2 = world.write_session(
            "sess-new-alpha",
            &[serde_json::json!({
                "type": "user",
                "cwd": "/Users/ember/dev/proj-alpha",
                "timestamp": "2026-08-15T12:00:00Z",
                "message": {"role": "user", "content": "error: std::io::Error in ~/dev/x:123"}
            })],
        );
        let _p3 = world.write_session(
            "sess-new-beta",
            &[serde_json::json!({
                "type": "user",
                "cwd": "/Users/ember/dev/proj-beta",
                "timestamp": "2026-08-15T12:00:00Z",
                "message": {"role": "user", "content": "error: std::io::Error in ~/dev/x:123"}
            })],
        );
        cv_search::index_all(None, true, false, false).unwrap();

        let searcher = cv_search::Searcher;
        // `--cwd proj-alpha` narrows from 3 matching sessions to the 2 alpha sessions.
        let alpha_only = searcher
            .search_allowed(
                "error: std::io::Error ~/dev/x:123",
                cv_search::SearchFilter {
                    harness: None,
                    cwd: Some("proj-alpha"),
                    scope: cv_search::Scope::Exchange,
                },
                None,
                20,
                Source::Index,
                None,
            )
            .unwrap();
        assert_eq!(alpha_only.total, 2);
        assert_eq!(alpha_only.hits.len(), 2);

        // Combining `--cwd proj-alpha` + `-q after:2026-06-01` narrows to `sess-new-alpha` in both Index and Live (`--scan`).
        let allowed = resolve_allowed_ids(None, Some("proj-alpha"), Some("after:2026-06-01".into()), None).unwrap();
        for source in [Source::Index, Source::Live] {
            let filtered = searcher
                .search_allowed(
                    "std::io::Error",
                    cv_search::SearchFilter {
                        harness: None,
                        cwd: Some("proj-alpha"),
                        scope: cv_search::Scope::Exchange,
                    },
                    allowed.as_ref(),
                    20,
                    source,
                    None,
                )
                .unwrap();
            assert_eq!(filtered.total, 1, "source={source:?}");
            assert_eq!(filtered.hits.len(), 1, "source={source:?}");
            assert_eq!(filtered.hits[0].id, "sess-new-alpha", "source={source:?}");
        }
    }
}
