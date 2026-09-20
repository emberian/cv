//! `cv search` / `cv index` — full-text and semantic search.

use crate::util::{dirs_home, parse_harness, session_row, short_id};
use anyhow::{Context, Result};
use cv_core::ir::{truncate, Harness, SessionRef};
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

fn rfc3339(secs: Option<i64>) -> Option<String> {
    secs.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.to_rfc3339())
}

/// One `--json` object for an index/semantic hit: the session row (from the catalog, so `path`,
/// `message_count` and `size_bytes` are the same values `ls --json` gives; null for a sub-agent
/// lane, which the catalog doesn't list) with the hit's own title/cwd/dates laid over it, plus
/// `score`, `snippet`, and the sub-agent provenance trio — always present, null for a top-level hit.
fn hit_json(h: &cv_search::Hit) -> serde_json::Value {
    let harness = Harness::parse(&h.harness);
    let cataloged = cv_core::catalog::lookup(&h.id, harness)
        .into_iter()
        .find(|r| r.id == h.id);
    let mut row = match &cataloged {
        Some(r) => session_row(r, std::fs::metadata(&r.path).ok().map(|m| m.len())),
        None => serde_json::json!({
            "id": h.id,
            "harness": h.harness,
            "path": serde_json::Value::Null,
            "cwd": serde_json::Value::Null,
            "title": serde_json::Value::Null,
            "created_at": serde_json::Value::Null,
            "updated_at": serde_json::Value::Null,
            "message_count": serde_json::Value::Null,
            "size_bytes": serde_json::Value::Null,
        }),
    };
    let obj = row.as_object_mut().expect("json object");
    // The index is what matched: its title/cwd/dates win when it has them.
    if let Some(c) = &h.cwd {
        obj.insert("cwd".into(), serde_json::json!(c));
    }
    if let Some(t) = &h.title {
        obj.insert("title".into(), serde_json::json!(t));
    }
    if let Some(t) = rfc3339(h.created_at) {
        obj.insert("created_at".into(), serde_json::json!(t));
    }
    if let Some(t) = rfc3339(h.updated_at) {
        obj.insert("updated_at".into(), serde_json::json!(t));
    }
    obj.insert("score".into(), serde_json::json!(h.score));
    obj.insert("snippet".into(), serde_json::json!(h.snippet));
    // Sub-agent provenance (an index built with `cv index --subagents` folds lane transcripts
    // in): the lane's own agent id (`cv show <agent_id>` resolves it), the top-level session
    // that spawned it, and the workflow run it belonged to.
    obj.insert("agent_id".into(), serde_json::json!(h.agent_id));
    obj.insert("parent_id".into(), serde_json::json!(h.parent_id));
    obj.insert("workflow".into(), serde_json::json!(h.workflow));
    row
}

/// The live-scan twin of [`hit_json`]: the ref is in hand, so the row is exact; a live scan has
/// no score and never walks sub-agents (provenance is null).
fn live_hit_json(r: &SessionRef, title: &str, snippet: &str) -> serde_json::Value {
    let mut row = session_row(r, std::fs::metadata(&r.path).ok().map(|m| m.len()));
    let obj = row.as_object_mut().expect("json object");
    obj.insert("title".into(), serde_json::json!(title));
    obj.insert("score".into(), serde_json::Value::Null);
    obj.insert("snippet".into(), serde_json::json!(snippet));
    obj.insert("agent_id".into(), serde_json::Value::Null);
    obj.insert("parent_id".into(), serde_json::Value::Null);
    obj.insert("workflow".into(), serde_json::Value::Null);
    row
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
    use cv_core::ParseOptions;
    let needle = query.to_lowercase();
    let mut hits = 0;
    // --json: accumulate the SAME hits the table would print (same order, same snippet), emitted
    // as one array at the end. No live-scan score exists, so `score` is an explicit null; ids are
    // full (the table truncates to 8 chars).
    let mut rows: Vec<serde_json::Value> = Vec::new();

    // Streams each session into pack's head-capped `CapSink` under a lazy parse: peak per session
    // is O(LIVE_HAY_BYTES), never O(session) — this is exactly the first-run (no index yet) path,
    // where the old bulk parse materialized every message plus a lowercased copy of the whole
    // transcript (multi-GB RSS on a big corpus). Trade-off: a match beyond the capped head is
    // missed here; finding those is the index's job (`cv index`).
    for adapter in cv_core::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = super::pack::CapSink::new(&r.path);
            let meta = match adapter.stream(&r, &ParseOptions::lazy(), &mut sink) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Lowercase only the bounded head (the match check), windowing the snippet from the
            // original-case haystack.
            let low = sink.hay.to_lowercase();
            if let Some(pos) = low.find(&needle) {
                hits += 1;
                let label = cv_core::label_from(meta.title.as_deref(), sink.first_user.as_deref());
                let snip = snippet(&sink.hay, pos.min(sink.hay.len()), needle.len());
                if json {
                    rows.push(live_hit_json(&r, &label, &snip));
                } else {
                    println!(
                        "{:8}  {:8}  {:10}  {}",
                        r.harness.as_str(),
                        short_id(&r.id),
                        r.updated_at
                            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d"))
                            .unwrap_or_else(|| "----------".into()),
                        sanitize_line(&label),
                    );
                    println!("          … {}", sanitize_line(&snip));
                }
                if hits >= limit {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                        eprintln!("(stopped at {limit} hits; use --limit)");
                    } else {
                        println!("\n(stopped at {limit} hits; use --limit)");
                    }
                    return Ok(());
                }
            }
        }
    }
    if json {
        // Pure JSON on stdout even for a miss: an empty array, no prose.
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits == 0 {
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

fn snippet(hay: &str, pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(40);
    let end = (pos + len + 40).min(hay.len());
    let s = &hay[floor_char(hay, start)..ceil_char(hay, end)];
    truncate(&s.replace('\n', " "), 120)
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
