//! `cv search` / `cv index` — full-text and semantic search.

use crate::util::{dirs_home, parse_harness, short_id};
use anyhow::{Context, Result};
use cv_core::ir::{truncate, Harness};
use cv_core::sanitize::sanitize_line;
use std::path::PathBuf;

pub(crate) fn cmd_search(
    query: &str,
    harness: Option<String>,
    limit: usize,
    semantic: bool,
    json: bool, // emit the hits as one JSON array (session rows + score/snippet/provenance) instead of the table
) -> Result<()> {
    let want = parse_harness(&harness)?;

    // Semantic search: embed the query and rank stored vectors. Requires `cv index --semantic`.
    if semantic {
        let hits = cv_search::semantic_search(None, query, limit.saturating_mul(4))
            .context("semantic search failed (run `cv index --semantic` first?)")?;
        render_search_hits(&hits, want, limit, query, "semantic", json)?;
        return Ok(());
    }

    // A retired sqlite FTS index may still be sitting on disk from older versions; tantivy is
    // canonical now, so let the user know it's safe to remove.
    if let Some(legacy) = legacy_sqlite_index_path() {
        if legacy.exists() {
            eprintln!(
                "(note: legacy sqlite index no longer used; safe to delete {})",
                legacy.display()
            );
        }
    }

    // Preferred path: the tantivy full-text index (real tokenization + BM25). Authoritative when
    // present — an empty result means "no match", not "fall back to a live scan".
    if cv_search::default_tantivy_dir().exists() {
        match cv_search::text_search(None, query, limit.saturating_mul(4)) {
            Ok(hits) => {
                render_search_hits(&hits, want, limit, query, "index", json)?;
                return Ok(());
            }
            Err(e) => eprintln!("(tantivy index unavailable: {e:#}; scanning live)"),
        }
    } else {
        eprintln!("(no index yet — scanning live; run `cv index` for instant search)");
    }
    cmd_search_live(query, want, limit, json)
}

/// Where the retired sqlite FTS index used to live: `$CLUSTERVISION_HOME/index.sqlite` or
/// `~/.clustervision/index.sqlite`. Only used to nudge cleanup of a stale file.
fn legacy_sqlite_index_path() -> Option<PathBuf> {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".clustervision")))
        .map(|d| d.join("index.sqlite"))
}

/// Render a slice of cv-search [`cv_search::Hit`]s with harness/short-id/date/title/snippet,
/// applying the `--harness` filter and `--limit`. `source` labels the empty-result hint.
/// With `json`, the SAME hits in the same order go to stdout as one JSON array instead —
/// no hint lines, an empty result is `[]` — so the output pipes cleanly.
fn render_search_hits(
    hits: &[cv_search::Hit],
    want: Option<Harness>,
    limit: usize,
    query: &str,
    source: &str,
    json: bool,
) -> Result<()> {
    let rows: Vec<&cv_search::Hit> = hits
        .iter()
        .filter(|h| want.is_none_or(|w| h.harness == w.as_str()))
        .take(limit)
        .collect();
    if json {
        // Machine-readable hits: a session row (the same shape `ls --json` emits, filled from the
        // catalog) plus the untruncated snippet and the index's relevance score (BM25 for FTS,
        // cosine similarity for --semantic). Timestamps are RFC 3339, null when unknown.
        let vals: Vec<serde_json::Value> = rows.iter().map(|h| hit_json(h)).collect();
        println!("{}", serde_json::to_string_pretty(&vals)?);
        return Ok(());
    }
    if rows.is_empty() {
        let hint = if source == "semantic" {
            "(semantic; run `cv index --semantic` to (re)build embeddings)"
        } else {
            "(index; try `cv index` to refresh)"
        };
        println!("no matches for {query:?} {hint}");
        // The likely reason a recent conversation isn't findable: the index has fallen behind.
        if source == "index" {
            if let Some(days) = index_days_behind() {
                println!("(note: the index is ~{days} day(s) behind the newest session — run `cv index`)");
            }
            // The other common miss: the wanted text lives in a *sub-agent* transcript, which the
            // default index doesn't fold in. Say so — a search that came back empty for something an
            // Agent/Workflow lane discussed is exactly this case.
            if !cv_search::fts::has_subagent_docs(&cv_search::default_tantivy_dir()) {
                println!(
                    "(note: sub-agent transcripts aren't searched yet — rebuild with \
                     `cv index --subagents` to include the Agent/Workflow lanes)"
                );
            }
        }
        return Ok(());
    }
    for h in rows {
        // Dates ride on the hit straight from the index (FTS); semantic hits carry none.
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
        // Titles and snippets are transcript-derived (untrusted) — sanitize at the terminal
        // seam (G5); JSON surfaces stay raw.
        println!(
            "{:8}  {:8}  {:10}  {}{}",
            h.harness,
            id_disp,
            date,
            sanitize_line(h.title.as_deref().unwrap_or_default()),
            provenance,
        );
        if !h.snippet.trim().is_empty() {
            println!("          … {}", truncate(&sanitize_line(&h.snippet), 120));
        }
    }
    Ok(())
}

/// One `--json` object for an index/semantic hit — [`cv_core::rows::SearchRow`] does the shaping,
/// so `cv search --json` and `cvd`'s `/api/search` emit the identical row for the identical hit.
fn hit_json(h: &cv_search::Hit) -> serde_json::Value {
    cv_core::rows::SearchRow {
        id: &h.id,
        harness: &h.harness,
        cwd: h.cwd.as_deref(),
        title: h.title.as_deref(),
        created_at: h.created_at,
        updated_at: h.updated_at,
        score: Some(h.score),
        snippet: &h.snippet,
        agent_id: h.agent_id.as_deref(),
        parent_id: h.parent_id.as_deref(),
        workflow: h.workflow.as_deref(),
    }
    .to_json()
}

/// Whole days the FTS index lags the newest session file on disk, when ≥ 1. Cheap enough for the
/// no-matches path it decorates: one stored-field scan of the index (the newest indexed mtime
/// stamp) plus a metadata-only discovery sweep (a stat per session, no parsing).
fn index_days_behind() -> Option<u64> {
    const DAY_NS: i64 = 86_400 * 1_000_000_000;
    let indexed = cv_search::fts::newest_indexed_mtime(&cv_search::default_tantivy_dir())?;
    let newest_on_disk = cv_core::discover_all()
        .iter()
        .map(|r| cv_core::offsets::file_sig(&r.path).0)
        .max()?;
    let days = newest_on_disk.saturating_sub(indexed) / DAY_NS;
    (days >= 1).then_some(days as u64)
}

fn cmd_search_live(query: &str, want: Option<Harness>, limit: usize, json: bool) -> Result<()> {
    // The scan itself is [`cv_core::scan::live_search`] (head-capped per session, so peak memory is
    // O(cap) not O(session)) — the same call `cvd`'s `/api/search` degrades to when the index is
    // missing, so the no-index answer is identical through both doors.
    let found = cv_core::scan::live_search(query, want, limit)?;
    if json {
        // Pure JSON on stdout even for a miss: an empty array, no prose.
        let rows: Vec<serde_json::Value> = found
            .hits
            .iter()
            .map(|h| cv_core::rows::live_search_row(&h.session, &h.title, &h.snippet))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        if found.truncated {
            eprintln!("(stopped at {limit} hits; use --limit)");
        }
        return Ok(());
    }
    for h in &found.hits {
        println!(
            "{:8}  {:8}  {:10}  {}",
            h.session.harness.as_str(),
            short_id(&h.session.id),
            h.session
                .updated_at
                .map(|d| crate::util::fmt_local(d, "%Y-%m-%d"))
                .unwrap_or_else(|| "----------".into()),
            sanitize_line(&h.title),
        );
        println!("          … {}", sanitize_line(&h.snippet));
    }
    if found.truncated {
        println!("\n(stopped at {limit} hits; use --limit)");
    } else if found.hits.is_empty() {
        println!("no matches for {query:?}");
    }
    Ok(())
}

pub(crate) fn cmd_index(semantic: bool, rebuild: bool, subagents: bool) -> Result<()> {
    eprintln!(
        "✦ {} full-text index{}…",
        if rebuild { "rebuilding" } else { "updating" },
        if subagents { " (+ sub-agent forest)" } else { "" }
    );
    let n = cv_search::index_all(None, rebuild, subagents)?;
    println!(
        "indexed {n} top-level session(s) → {}{}",
        cv_search::default_tantivy_dir().display(),
        if subagents {
            " (sub-agent transcripts folded in)"
        } else {
            ""
        }
    );
    if semantic {
        eprintln!("✦ embedding sessions (downloads a small model on first use)…");
        let e = cv_search::embed_all(None)?;
        println!(
            "embedded {e} session(s) → {}",
            cv_search::default_embeddings_path().display()
        );
    }
    println!("events: extracted on the same pass → try `cv events <id>` / `cv touched <path>`");
    Ok(())
}
