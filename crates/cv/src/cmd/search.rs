//! `cv search` / `cv index` — full-text and semantic search.

use crate::util::{parse_harness, short_id};
use anyhow::Result;
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_search::{Found, Note, Source};
use std::io::Write;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_search(
    query: &str,
    harness: Option<String>,
    limit: usize,
    semantic: bool,
    scan: bool,
    kind: Option<String>,
    live_bytes: Option<u64>,
    json: bool, // emit `{hits, total}` (session rows + score/snippet/provenance) instead of the table
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let scope = match kind.as_deref() {
        None => cv_search::Scope::Exchange,
        Some(k) => match cv_search::Scope::parse(k) {
            Some(scope) => scope,
            None => return crate::util::usage(format!("unknown --kind {k:?}: exchange, context or all")),
        },
    };
    let source = if semantic {
        Source::Semantic
    } else {
        if scan {
            eprintln!("(--scan: scanning every transcript live; `cv index` makes search instant)");
            Source::Live
        } else {
            Source::Index
        }
    };
    let filter = cv_search::SearchFilter {
        harness: want.map(|h| h.as_str()),
        cwd: None,
        scope,
    };
    // The index answers every full-text search: an empty result means "no match", and an index
    // that is missing or unusable is an error naming the cause and the fix, never a silent scan
    // of every transcript (minutes on a large corpus).
    let found = match cv_search::Searcher::default().search(query, filter, limit, source, live_bytes) {
        Ok(found) => found,
        Err(e) => {
            return match source {
                Source::Semantic => Err(e.context("semantic search failed (run `cv index --semantic` first?)")),
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
/// so the output pipes cleanly (`cv mcp` relays stderr as its own content block).
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
        let hint = match source {
            Source::Semantic => "(semantic; run `cv index --semantic` to (re)build embeddings)",
            _ => "(index; try `cv index` to refresh)",
        };
        println!("no matches for {query:?} {hint}");
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

pub(crate) fn cmd_index(semantic: bool, rebuild: bool, subagents: bool, strict: bool, json: bool) -> Result<()> {
    if semantic && subagents {
        return crate::util::usage("--semantic indexes top-level sessions; --subagents currently applies only to full-text indexing. Run `cv index --subagents` and `cv index --semantic` separately.");
    }

    eprintln!(
        "✦ {} full-text index{}…",
        if rebuild { "rebuilding" } else { "updating" },
        if subagents { " (+ sub-agent forest)" } else { "" }
    );
    let (indexed, refs) = cv_search::index_all(None, rebuild, subagents, strict)?;
    let dir = cv_search::default_tantivy_dir();
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
    let mut embedded = None;
    if semantic {
        eprintln!("✦ refreshing semantic vectors (downloads a small model on first use)…");
        let e = cv_search::embed_sessions(None, &refs)?;
        if !json {
            println!(
                "embedded {e} session(s) → {}",
                cv_search::default_embeddings_path().display()
            );
            if let Some(chars) = cv_search::embedded_chars()? {
                println!("(each vector embeds a session's first {chars} characters: what the model reads)");
            }
        }
        embedded = Some(e);
    }
    if json {
        let mut out = serde_json::json!({
            "indexed": indexed.total,
            "changed": indexed.changed,
            "removed": indexed.removed,
            "errors": indexed.errors,
            "path": dir,
            "unreadable": indexed.unreadable,
        });
        if let Some(e) = embedded {
            out["embedded"] = e.into();
        }
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
    fn unsupported_semantic_forest_combination_fails_before_index_work() {
        for rebuild in [false, true] {
            let error = cmd_index(true, rebuild, true, false, false).unwrap_err();
            assert!(error.is::<crate::util::UsageError>());
            assert!(error.to_string().contains("top-level"));
            assert!(error.to_string().contains("cv index --subagents"));
            assert!(error.to_string().contains("cv index --semantic"));
        }
    }

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
}
