//! `cv search` / `cv index` — full-text and exact substring search.

use crate::util::{parse_harness, short_id};
use anyhow::Result;
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_search::{Found, Note, Source};
use std::io::Write;

pub(crate) struct SearchArgs<'a> {
    pub query: &'a str,
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub filter_query: Option<String>,
    pub limit: usize,
    pub scan: bool,
    pub kind: Option<String>,
    pub live_bytes: Option<u64>,
    pub json: bool, // emit `{hits, total}` (session rows + score/snippet/provenance) instead of the table
}

fn resolve_allowed_ids(
    want: Option<cv_core::Harness>,
    cwd: Option<&str>,
    filter_query: Option<String>,
) -> Result<Option<std::collections::HashSet<String>>> {
    let Some(sq) = crate::cmd::query::build(filter_query)? else {
        return Ok(None);
    };
    let text = crate::cmd::query::text_sets(&sq)?;
    let mut candidates = cv_core::sessions_for(want);
    if let Some(needle) = cwd {
        candidates.retain(|r| r.cwd.as_ref().is_some_and(|p| p.to_string_lossy().contains(needle)));
    }
    crate::cmd::query::filter_refs(&mut candidates, &sq, &text)?;
    Ok(Some(candidates.into_iter().map(|r| r.id).collect()))
}

pub(crate) fn cmd_search(args: SearchArgs<'_>) -> Result<()> {
    let SearchArgs {
        query,
        harness,
        cwd,
        filter_query,
        limit,
        scan,
        kind,
        live_bytes,
        json,
    } = args;
    let want = parse_harness(&harness)?;
    let scope = match kind.as_deref() {
        None => cv_search::Scope::Exchange,
        Some(k) => match cv_search::Scope::parse(k) {
            Some(scope) => scope,
            None => return crate::util::usage(format!("unknown --kind {k:?}: exchange, context or all")),
        },
    };
    let allowed_ids = resolve_allowed_ids(want, cwd.as_deref(), filter_query)?;
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
    // The index (+ live suffix of unindexed changes) answers full-text search: an empty result
    // means "no match", and an unusable index is an error naming the cause and the fix, never a
    // silent full scan of every transcript.
    let found = match cv_search::Searcher.search_allowed(query, filter, allowed_ids.as_ref(), limit, source, live_bytes)
    {
        Ok(found) => found,
        Err(e) => {
            return match source {
                Source::Index if cv_search::is_invalid_text_query(&e) || e.is::<cv_search::BadLiveBytes>() => {
                    crate::util::usage(format!("{e:#}"))
                }
                Source::Index => Err(e.context(
                    "full-text search needs a usable index (or pass --scan for a slow live scan of every session)",
                )),
                Source::Live => Err(e),
            }
        }
    };
    render(found, query, json)
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

/// The footer: `N of M matches`, and how to see the rest when `--limit` cut them.
fn matches_line(shown: usize, total: usize) -> String {
    let more = if shown < total { "; use --limit for more" } else { "" };
    format!("{shown} of {total} matches{more}")
}

/// Render what the search found: the table with harness/short-id/date/title/snippet, the
/// `N of M matches` footer and the notes, or, with `json`, `{"hits": [...], "total": M}` on
/// stdout with the SAME hits in the same order — notes on stderr, an empty result `"hits": []` —
/// so the output pipes cleanly.
fn render(found: Found, query: &str, json: bool) -> Result<()> {
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
        write_json_rows(&mut out, &hits, total, |hit| cv_search::row(hit, &rows))?;
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
    Ok(())
}

// Preserve the shared row contract, retaining only one projected row at a time rather than
// another complete Value array and serialized String.
#[derive(serde::Serialize)]
struct JsonFound<R> {
    hits: R,
    total: usize,
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
fn write_json_rows<T>(
    out: &mut impl Write,
    rows: &[T],
    total: usize,
    row: impl Fn(&T) -> serde_json::Value,
) -> Result<()> {
    serde_json::to_writer_pretty(
        &mut *out,
        &JsonFound {
            hits: JsonRows { rows, row },
            total,
        },
    )?;
    writeln!(out)?;
    Ok(())
}

pub(crate) fn cmd_index(rebuild: bool, subagents: bool, if_changed: bool, strict: bool, json: bool) -> Result<()> {
    let dir = cv_search::default_tantivy_dir();
    if if_changed {
        // The timer gate: a stat-only probe (catalog inventory + one stat per session + the
        // index's own stamps) answers whether a pass would find work at all.
        match cv_search::fts::pass_needed(&dir, subagents) {
            Some(true) => eprintln!("  ↳ corpus moved since the last pass — indexing"),
            Some(false) => {
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
            None => eprintln!("  ↳ index unreadable — running the pass"),
        }
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
            write_json_rows(&mut output, &rows, 7, Clone::clone).unwrap();
            let expected = serde_json::json!({"hits": rows, "total": 7});
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!("{}\n", serde_json::to_string_pretty(&expected).unwrap())
            );
        }
        let rows = vec![1, 2, 3];
        let visits = std::cell::RefCell::new(Vec::new());
        let mut out = Vec::new();
        write_json_rows(&mut out, &rows, 3, |row| {
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
                    assert_eq!(query, "error: std::io::Error");
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
        let allowed = resolve_allowed_ids(None, Some("proj-alpha"), Some("after:2026-06-01".into())).unwrap();
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
