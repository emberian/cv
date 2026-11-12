//! `cv ls` / `cv stats` — browsing the discovered corpus.
//!
//! These read [`cv_core::sessions`] (the probed catalog — milliseconds warm, transparently a full
//! discovery when cold/stale); `cv ls --fresh` forces [`cv_core::discover_all`]'s full scan.

use crate::util::{dim_cwd, parse_harness, session_row, short_id_len};
use anyhow::{Context, Result};
use cv_core::ir::{truncate, SessionRef};
use cv_core::sanitize::sanitize_line;

const METADATA_BATCH_SIZE: usize = 256;
const HEAP_WINDOW_RATIO: usize = 8;

/// Apply a parsed query to a ref list in place. The two-phase filter itself is
/// [`cv_core::query::filter_refs`] — shared with `cv serve`'s `/api/stats?q=`, so `ls`/`stats` and
/// the daemon all speak exactly the same `-q`; the CLI only supplies the `text:`
/// resolution, which needs the tantivy index cv-core cannot depend on.
fn apply_query(refs: &mut Vec<SessionRef>, query: &Option<cv_core::SessionQuery>) -> Result<()> {
    let Some(q) = query else { return Ok(()) };
    crate::cmd::query::filter_refs(refs, q, &crate::cmd::query::text_sets(q)?)
}

/// Session rows for `--json` listings, capped at `limit`. Fresh render-phase metadata supplies
/// both presence and `size_bytes`; rows sharing a path reuse that observation within each batch.
fn json_rows<'a>(refs: impl Iterator<Item = &'a SessionRef>, limit: usize) -> Vec<(&'a SessionRef, serde_json::Value)> {
    // Several logical sessions can share one database. Recheck at this rendering boundary,
    // then reuse metadata in small batches: a missing tail must not grow another fleet-sized map.
    let mut probe = cv_core::source::Probe::default();
    refs.enumerate()
        .filter_map(|(index, r)| {
            if index % METADATA_BATCH_SIZE == 0 {
                probe = cv_core::source::Probe::default();
            }
            probe.size(&r.path).map(|size| (r, size))
        })
        .take(limit)
        .map(|(r, size)| (r, session_row(r, Some(size))))
        .collect()
}

/// Rank metadata without sorting the unrequested tail. Original ordinals reproduce stable ties.
#[derive(Eq, PartialEq, Ord, PartialOrd)]
enum RankKey {
    Time(Option<chrono::DateTime<chrono::Utc>>),
    /// `None` (uncounted) ranks after every counted session, including `Some(0)`.
    Messages(Option<usize>),
}
struct RankedRef<'a> {
    key: RankKey,
    ordinal: usize,
    reference: &'a SessionRef,
}
impl PartialEq for RankedRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for RankedRef<'_> {}
impl PartialOrd for RankedRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
#[cfg(test)]
thread_local! { static RANK_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
impl Ord for RankedRef<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(test)]
        RANK_COMPARISONS.with(|count| count.set(count.get() + 1));
        self.key.cmp(&other.key).then_with(|| other.ordinal.cmp(&self.ordinal))
    }
}
fn ranked_json_rows<'a>(
    refs: &'a [SessionRef],
    limit: usize,
    sort_by: &str,
    asc: bool,
) -> Vec<(&'a SessionRef, serde_json::Value)> {
    if limit == 0 {
        return Vec::new();
    }
    let entries: Vec<_> = refs
        .iter()
        .enumerate()
        .map(|(ordinal, reference)| RankedRef {
            key: match sort_by {
                "created" => RankKey::Time(reference.created_at.or(reference.updated_at)),
                "messages" => RankKey::Messages(reference.message_count),
                _ => RankKey::Time(reference.updated_at.or(reference.created_at)),
            },
            ordinal,
            reference,
        })
        .collect();
    let mut heap = std::collections::BinaryHeap::from(entries);
    let ranked = std::iter::from_fn(move || heap.pop().map(|entry| entry.reference));
    if asc {
        // Ascending selects the newest K before the existence filter, then reverses them.
        let selected: Vec<_> = ranked.take(limit).collect();
        json_rows(selected.into_iter().rev(), limit)
    } else {
        // Descending keeps ranked tail candidates available if an earlier file is missing.
        json_rows(ranked, limit)
    }
}

/// Reduce the stable inline prefix with the title rule (`Message::prompt_title`). Once lazy text appears,
/// preserve deferred user collection: resolution needs EOF source metadata and may fail after
/// a source mutation, allowing a later user turn to become the title.
struct TitleUsers {
    candidates: cv_core::Session,
    deferred: bool,
    fallback: Option<String>,
}
impl TitleUsers {
    fn new(harness: cv_core::Harness) -> Self {
        Self {
            candidates: cv_core::Session::new("", harness),
            deferred: false,
            fallback: None,
        }
    }
    /// `listed` is the row's own `title`: when the listing has one, `display_title` is that title.
    fn finish(self, mut session: cv_core::Session, listed: Option<&str>) -> (cv_core::Session, Option<String>) {
        session.messages = self.candidates.messages;
        let title = listed
            .map(str::to_string)
            .or_else(|| session.title.clone())
            .or(self.fallback)
            .or_else(|| session.synth_title());
        session.messages.clear();
        (session, title)
    }
}
impl cv_core::MessageSink for TitleUsers {
    fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
        // Only a prompt can title a session (`Message::prompt_title`).
        if m.role != cv_core::Role::User || m.kind != cv_core::MessageKind::Prompt || self.fallback.is_some() {
            return cv_core::Flow::Continue;
        }
        self.deferred |= m.content.iter().any(|block| {
            matches!(
                block,
                cv_core::Block::Text {
                    text: cv_core::lazy::Text::Span(_) | cv_core::lazy::Text::Source(_)
                }
            )
        });
        self.candidates.messages.push(m);
        if !self.deferred {
            // No resolver reads: synth_title borrows complete Inline strings, joins Text blocks,
            // and applies the canonical reminder/wrapper filtering and truncation policy.
            self.fallback = self.candidates.synth_title();
            self.candidates.messages.clear();
        }
        cv_core::Flow::Continue
    }
}

/// Read all metadata/counts through EOF, retaining only the title's deferred candidate suffix.
fn enrich_head(adapter: &dyn cv_core::Adapter, r: &SessionRef) -> Result<(cv_core::Session, Option<String>)> {
    let mut users = TitleUsers::new(r.harness);
    let session = cv_core::stream::stream_counted(
        adapter,
        r,
        &cv_core::Demand::LEAN.with_spans(),
        &mut users,
        cv_core::stream::Pass::RECORD,
    )?
    .session;
    Ok(users.finish(session, r.title.as_deref()))
}

/// Select the requested listing window. Text listings retain their exact-total contract;
/// machine listings rank first and demand only facts needed for the selected rows.
fn select_refs(
    refs: &mut Vec<SessionRef>,
    query: &Option<cv_core::SessionQuery>,
    limit: usize,
    sort_by: &str,
    asc: bool,
    json: bool,
) -> Result<usize> {
    if json && limit == 0 {
        refs.clear();
        return Ok(0);
    }
    if !json {
        apply_query(refs, query)?;
    }

    match sort_by {
        "created" => refs.sort_by_key(|r| std::cmp::Reverse(r.created_at.or(r.updated_at))),
        "messages" => refs.sort_by_key(|r| std::cmp::Reverse(r.message_count)),
        // "updated" (the default; clap's value_parser admits nothing else)
        _ => refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at))),
    }

    if json {
        if let Some(q) = query {
            cv_core::query::filter_refs_limited(refs, q, &crate::cmd::query::text_sets(q)?, limit)?;
        }
    }
    let total = refs.len();
    if asc {
        // The SAME newest `limit` rows, read oldest → newest like a feed: select first, then flip,
        // so `--asc` never changes which sessions are shown — only the order they print in.
        refs.truncate(limit);
        refs.reverse();
    }
    Ok(total)
}

pub(crate) struct LsArgs<'a> {
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub query: Option<String>,
    pub limit: usize,
    pub sort_by: &'a str,
    pub asc: bool,
    pub fresh: bool,
    pub json: bool,
    pub enrich: bool,
}

pub(crate) fn cmd_ls(args: LsArgs<'_>) -> Result<()> {
    let LsArgs {
        harness,
        cwd,
        query,
        limit,
        sort_by,
        asc,
        fresh,
        json,
        enrich,
    } = args;
    let want = parse_harness(&harness)?;
    let query = crate::cmd::query::build(query)?;
    // An empty machine window requires no inventory or enrichment; still validate inputs.
    if json && limit == 0 {
        println!("[]");
        return Ok(());
    }
    let all = if fresh {
        cv_core::discover_all()
    } else {
        cv_core::sessions()
    };
    let discovered = all.len();
    let mut refs: Vec<SessionRef> = all
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r.cwd.as_ref().is_some_and(|p| p.to_string_lossy().contains(c)),
        })
        .collect();
    // Queried windows still require ordered predicate/error evaluation; text needs exact totals.
    // Heapify only a small unqueried machine window, preserving the existing row stat seam.
    let heap_window = json && query.is_none() && limit.saturating_mul(HEAP_WINDOW_RATIO) < refs.len();
    let total = if heap_window {
        refs.len()
    } else {
        select_refs(&mut refs, &query, limit, sort_by, asc, json)?
    };
    if json {
        // Machine-readable listing: the SAME rows the table below would print (same filters, sort,
        // exists() guard, and limit), as one JSON array on stdout — no header/footer, so it pipes
        // cleanly. Transcript-derived text (title) stays raw here — sanitizing is the terminal
        // seam's job (G5); JSON is the machine contract.
        //
        // `--enrich` (git + display_title) is opt-in because it costs one transcript parse per
        // emitted row — O(limit), not O(fleet), but not the catalog-cheap default `ls --json`
        // promises. See the perf note in the flag's help.
        let ranked_rows = if heap_window {
            ranked_json_rows(&refs, limit, sort_by, asc)
        } else {
            json_rows(refs.iter(), limit)
        };
        let rows: Vec<serde_json::Value> = ranked_rows
            .into_iter()
            .map(|(r, mut obj)| {
                if enrich {
                    // Same transcript source `cv show --json` reads. A lazy parse leaves giant
                    // content on disk (memory-safe on the multi-MB "single-exchange giants" sesh
                    // warns about) while still carrying the session's git metadata and enough of the
                    // first user turn to synthesize a title.
                    let enriched = cv_core::harness::for_harness(r.harness)
                        .with_context(|| format!("no adapter for {}", r.harness))
                        .and_then(|a| enrich_head(a.as_ref(), r));
                    let map = obj.as_object_mut().expect("json object");
                    match enriched {
                        // A row whose transcript cannot be read says why, so a consumer can tell
                        // it from "not enriched" (no key) and from "no title anywhere" (null).
                        Err(error) => {
                            map.insert("enrich_error".into(), serde_json::json!(format!("{error:#}")));
                        }
                        Ok((session, display_title)) => {
                            // `git`: the same object `cv show --json` emits (branch/commit/remote,
                            // skip-none), omitted entirely when the transcript records no git context.
                            if let Some(git) = &session.git {
                                if let Ok(g) = serde_json::to_value(git) {
                                    map.insert("git".into(), g);
                                }
                            }
                            // `display_title`: `title` with a first-real-user-text fallback. Explicit
                            // null (not absent) when a session has neither, so consumers can tell "no
                            // title anywhere" from "not enriched" (the key is absent without --enrich).
                            map.insert("display_title".into(), serde_json::json!(display_title));
                        }
                    }
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if total < discovered {
        println!("{total} session(s) (of {discovered} discovered; filtered)\n");
    } else {
        println!("{total} session(s)\n");
    }
    // The exists() check guards the catalog read path's one residual lie — a session deleted since
    // the probe last looked — and is bounded by `limit`, not the fleet (it stats only listed rows).
    let shown: Vec<&SessionRef> = refs.iter().filter(|r| r.path.exists()).take(limit).collect();
    let plen = cv_core::task::unique_prefix_len(shown.iter().map(|r| r.id.strip_prefix("agent-").unwrap_or(&r.id)), 8);
    let id_col = shown
        .iter()
        .map(|r| short_id_len(&r.id, plen).chars().count())
        .max()
        .unwrap_or(8)
        .max(8);
    for r in shown {
        // Titles/cwds are transcript-derived (untrusted, replayed forever) — sanitize at the
        // terminal seam (G5), exactly like task/board rows.
        println!(
            "{:8}  {:id_col$}  {}  {:>4} msg  {}",
            r.harness.as_str(),
            short_id_len(&r.id, plen),
            crate::util::fmt_span(r.created_at, r.updated_at),
            r.message_count.map_or("–".to_string(), |n| n.to_string()),
            r.title
                .as_deref()
                .map(|t| truncate(&sanitize_line(t), 60))
                .unwrap_or_else(|| sanitize_line(&dim_cwd(r.cwd.as_deref())).into_owned()),
        );
    }
    if total > limit {
        println!("\n… {} more (use --limit)", total - limit);
    }
    Ok(())
}

// ---------- stats ----------

pub(crate) fn cmd_stats(query: Option<String>, json: bool, tokens: bool) -> Result<()> {
    let query = crate::cmd::query::build(query)?;
    let mut refs = cv_core::sessions();
    apply_query(&mut refs, &query)?;
    // The aggregation and the `--json` payload both live in `cv_core::stats`, so `cv stats --json`
    // and `cv serve`'s `/api/stats` are the same six numbers computed once — the dashboard used to
    // accumulate its own from whichever sessions the user had happened to open.
    let s = cv_core::stats::CorpusStats::compute(&refs);
    let total = s.sessions;
    let t = tokens.then(|| cv_core::stats::TokenStats::compute(&refs)).transpose()?;

    if json {
        let mut v = s.to_json();
        if let Some(t) = &t {
            v["tokens"] = t.to_json();
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    if total == 0 {
        let scope = if query.is_some() {
            " match the query"
        } else {
            " discovered"
        };
        println!("no sessions{scope}.");
        return Ok(());
    }

    println!("✦ clustervision fleet stats\n");
    let uncounted = match s.uncounted {
        0 => String::new(),
        n => format!(" · {n} not counted yet"),
    };
    println!("{total} session(s) · {} message(s){uncounted}\n", s.messages);

    println!("by harness:");
    for (h, n) in &s.by_harness {
        println!("  {h:12} {n:>5}");
    }

    println!("\ntop cwds:");
    for (c, n) in s.top_cwds.iter().take(cv_core::stats::TOP_CWDS) {
        println!("  {n:>5}  {}", truncate(c, 70));
    }

    println!("\ndate range:");
    println!(
        "  earliest created: {}",
        s.earliest_created
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "  latest updated:   {}",
        s.latest_updated
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "?".into())
    );
    if let Some(t) = &t {
        print_token_stats(t);
    }
    Ok(())
}

/// `1.23B` / `45.6M` / `7.8K` / `12`: token counts span nine orders of magnitude across a fleet.
fn si(n: u64) -> String {
    let f = n as f64;
    match n {
        1_000_000_000.. => format!("{:.2}B", f / 1e9),
        1_000_000.. => format!("{:.1}M", f / 1e6),
        1_000.. => format!("{:.1}K", f / 1e3),
        _ => n.to_string(),
    }
}

fn print_token_stats(t: &cv_core::stats::TokenStats) {
    use cv_core::stats::TokenRow;
    println!(
        "
tokens ({} session(s) + {} sub-agent transcript(s){}{}):",
        t.sessions,
        t.subagents,
        if t.failed > 0 {
            format!(", {} unparseable", t.failed)
        } else {
            String::new()
        },
        if t.duplicates > 0 {
            format!(", {} repeated usage record(s) skipped", t.duplicates)
        } else {
            String::new()
        },
    );
    println!(
        "  {:8} {:30} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "harness", "model", "calls", "input", "c.write", "c.read", "output", "uncached", "total"
    );
    let line = |r: &TokenRow| {
        let cost = r.cost_usd.map(|c| format!("  ${c:.2}")).unwrap_or_default();
        println!(
            "  {:8} {:30} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}{cost}",
            r.harness,
            truncate(&r.model, 30),
            r.calls,
            si(r.input),
            si(r.cache_write),
            si(r.cache_read),
            si(r.output),
            si(r.uncached()),
            si(r.total()),
        )
    };
    for r in &t.rows {
        line(r);
    }
    let by_h = t.by_harness();
    if by_h.len() > 1 {
        println!();
        for r in &by_h {
            line(r);
        }
    }
    println!();
    line(&t.total());
    println!("  (input excludes cache; uncached = input + cache writes + output)");
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn machine_windows_recheck_shared_paths_and_keep_readable_old_timestamps() {
        let world = crate::test_world("shared_window_metadata");
        let path = world.base.join("shared.db");
        let refs: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|id| SessionRef::new(id, cv_core::Harness::OpenCode, path.clone()))
            .collect();
        let query = cv_core::SessionQuery::parse("text:hit").unwrap();
        let texts = cv_core::query::TextSets::resolve(&query, |_| refs.iter().map(|r| r.id.clone()).collect());
        for content in ["one", "longer"] {
            std::fs::write(&path, content).unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)),
                )
                .unwrap();
            assert!(cv_core::source::FileId::at(&path).is_none());
            let mut selected = refs.clone();
            cv_core::query::filter_refs_limited(&mut selected, &query, &texts, 2).unwrap();
            let rows = json_rows(selected.iter(), 2);
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|(_, row)| row["size_bytes"] == content.len()));
        }
        // The rendering phase must not retain the query's earlier presence observation.
        let mut selected = refs;
        cv_core::query::filter_refs_limited(&mut selected, &query, &texts, 2).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(json_rows(selected.iter(), 2).is_empty());
    }

    #[test]
    fn heap_machine_windows_match_stable_rank_presence_and_ties() {
        let world = crate::test_world("heap_json_windows");
        let present = world.base.join("present");
        std::fs::write(&present, "fixture").unwrap();
        let refs: Vec<_> = (0..65)
            .map(|i| {
                let path = if i % 5 == 0 {
                    world.base.join("missing")
                } else {
                    present.clone()
                };
                let mut r = SessionRef::new(format!("row-{i}"), cv_core::Harness::Claude, path);
                r.message_count = (i % 7 != 6).then_some(i % 7); // one in seven is uncounted
                r.created_at = chrono::DateTime::from_timestamp(100 + (i % 4) as i64, 0);
                r.updated_at = if i % 3 == 0 {
                    None
                } else {
                    chrono::DateTime::from_timestamp(200 + (i % 5) as i64, 0)
                };
                r
            })
            .collect();
        for sort in ["created", "updated", "messages"] {
            for asc in [false, true] {
                for limit in [0, 1, 3, 20, 65, usize::MAX] {
                    let mut baseline = refs.clone();
                    select_refs(&mut baseline, &None, limit, sort, asc, true).unwrap();
                    let expected: Vec<_> = json_rows(baseline.iter(), limit)
                        .into_iter()
                        .map(|(_, row)| row)
                        .collect();
                    let actual: Vec<_> = ranked_json_rows(&refs, limit, sort, asc)
                        .into_iter()
                        .map(|(_, row)| row)
                        .collect();
                    assert_eq!(actual, expected, "sort={sort},asc={asc},limit={limit}");
                }
            }
        }
    }

    fn inventory_refs(present: &std::path::Path, count: usize, shape: &str) -> Vec<SessionRef> {
        (0..count)
            .map(|i| {
                let mut reference = SessionRef::new(i.to_string(), cv_core::Harness::Claude, present.to_path_buf());
                reference.message_count = Some(match shape {
                    "sorted" => count - i,
                    "reverse" => i,
                    "tied" => 7,
                    _ => ((i as u64 * 2_654_435_761) % 10_007) as usize,
                });
                reference
            })
            .collect()
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated diagnostic; run with --test-threads=1 and --nocapture"]
    fn listing_metadata_heap_control() {
        let world = crate::test_world("listing_metadata_heap");
        let mut first_peak = None;
        for count in [20_000, 100_000] {
            let refs: Vec<_> = (0..count)
                .map(|i| {
                    SessionRef::new(
                        i.to_string(),
                        cv_core::Harness::Claude,
                        world.base.join(format!("missing-{i}")),
                    )
                })
                .collect();
            let start = cv_testkit::heap::begin();
            let old: Vec<_> = refs
                .iter()
                .filter_map(|r| std::fs::metadata(&r.path).ok().map(|m| (r, m.len())))
                .take(20)
                .map(|(r, size)| session_row(r, Some(size)))
                .collect();
            let previous = cv_testkit::heap::end(start);
            let start = cv_testkit::heap::begin();
            let selected = json_rows(refs.iter(), 20);
            let current = cv_testkit::heap::end(start);
            assert!(old.is_empty() && selected.is_empty());
            if let Some(first_peak) = first_peak {
                assert!(
                    current.peak_above_baseline_bytes <= first_peak * 2,
                    "metadata cache grew with missing inventory"
                );
            } else {
                first_peak = Some(current.peak_above_baseline_bytes);
            }
            eprintln!(
                "listing metadata heap {}",
                serde_json::json!({"refs":count,"previous":previous,"current":current})
            );
        }
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated diagnostic; run with --test-threads=1 and --nocapture"]
    fn listing_rank_heap_control() {
        let world = crate::test_world("listing_rank_heap");
        let present = world.base.join("present");
        std::fs::write(&present, "fixture").unwrap();
        for count in [20_000, 100_000] {
            for shape in ["shuffled", "sorted", "reverse", "tied"] {
                let refs = inventory_refs(&present, count, shape);
                let mut baseline_refs = refs.clone();
                let start = cv_testkit::heap::begin();
                select_refs(&mut baseline_refs, &None, 20, "messages", false, true).unwrap();
                let expected: Vec<_> = json_rows(baseline_refs.iter(), 20)
                    .into_iter()
                    .map(|(_, row)| row)
                    .collect();
                let stable = cv_testkit::heap::end(start);
                let start = cv_testkit::heap::begin();
                let actual: Vec<_> = ranked_json_rows(&refs, 20, "messages", false)
                    .into_iter()
                    .map(|(_, row)| row)
                    .collect();
                let heap = cv_testkit::heap::end(start);
                assert_eq!(actual, expected);
                eprintln!(
                    "listing rank heap {}",
                    serde_json::json!({"refs":count,"window":20,"shape":shape,
                    "stable":stable,"heap":heap,"scope":"rank and selected row projection; inventory construction and final serialization excluded"})
                );
            }
        }
    }

    #[test]
    fn small_heap_window_reports_shuffled_and_natural_run_work() {
        let world = crate::test_world("heap_inventory_work");
        let present = world.base.join("present");
        std::fs::write(&present, "fixture").unwrap();
        for count in [20_000, 100_000] {
            for shape in ["shuffled", "sorted", "reverse", "tied"] {
                let refs = inventory_refs(&present, count, shape);
                let comparisons = std::cell::Cell::new(0usize);
                let mut baseline = refs.clone();
                baseline.sort_by_key(|reference| {
                    // sort_by_key evaluates both keys once per comparator invocation.
                    comparisons.set(comparisons.get() + 1);
                    std::cmp::Reverse(reference.message_count)
                });
                assert_eq!(comparisons.get() % 2, 0);
                comparisons.set(comparisons.get() / 2);
                let expected: Vec<_> = json_rows(baseline.iter(), 20).into_iter().map(|(_, row)| row).collect();
                RANK_COMPARISONS.with(|value| value.set(0));
                let actual: Vec<_> = ranked_json_rows(&refs, 20, "messages", false)
                    .into_iter()
                    .map(|(_, row)| row)
                    .collect();
                let heap_comparisons = RANK_COMPARISONS.with(|value| value.get());
                assert_eq!(actual, expected);
                if shape == "shuffled" {
                    assert!(heap_comparisons * 3 < comparisons.get(), "rank work did not improve");
                }
                eprintln!(
                "rank control shape={shape},refs={count},window=20,stable_comparisons={},heap_comparisons={heap_comparisons},heap_entry_bytes={}",
                comparisons.get(),
                std::mem::size_of::<RankedRef<'_>>()
            );
            }
        }
    }

    #[test]
    fn machine_windows_match_full_filter_sort_limit_with_missing_paths_and_ties() {
        let world = crate::test_world("json_query_windows");
        let mut refs: Vec<_> = (0..6)
            .map(|i| {
                let path = world.write_session(
                    &format!("row-{i}"),
                    &[serde_json::json!({
                        "type":"user","message":{"role":"user","content":"hello"}
                    })],
                );
                let mut r = SessionRef::new(format!("row-{i}"), cv_core::Harness::Claude, path);
                r.message_count = Some(i % 3);
                r.created_at = chrono::DateTime::from_timestamp(100 + (i / 2) as i64, 1);
                r.updated_at = chrono::DateTime::from_timestamp(200 + (i / 3) as i64, 2);
                r
            })
            .collect();
        refs[4].created_at = None;
        refs[2].updated_at = None;
        std::fs::remove_file(&refs[5].path).unwrap();
        let query = Some(cv_core::SessionQuery::parse("msgs>=0").unwrap());
        for sort in ["created", "updated", "messages"] {
            for asc in [false, true] {
                for limit in [0, 1, 3, 6, usize::MAX] {
                    let mut baseline = refs.clone();
                    apply_query(&mut baseline, &query).unwrap();
                    // An absent session occupies no window slot, ascending or not.
                    baseline.retain(|r| r.path.exists());
                    match sort {
                        "created" => baseline.sort_by_key(|r| std::cmp::Reverse(r.created_at.or(r.updated_at))),
                        "messages" => baseline.sort_by_key(|r| std::cmp::Reverse(r.message_count)),
                        _ => baseline.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at))),
                    }
                    if asc {
                        baseline.truncate(limit);
                        baseline.reverse();
                    }
                    let expected: Vec<_> = json_rows(baseline.iter(), limit)
                        .into_iter()
                        .map(|(_, row)| row)
                        .collect();
                    let mut actual = refs.clone();
                    select_refs(&mut actual, &query, limit, sort, asc, true).unwrap();
                    let actual: Vec<_> = json_rows(actual.iter(), limit)
                        .into_iter()
                        .map(|(_, row)| row)
                        .collect();
                    assert_eq!(actual, expected, "sort={sort},asc={asc},limit={limit}");
                }
            }
        }
    }
}

#[cfg(test)]
mod enrich_tests {
    use super::*;
    use cv_core::{Block, Flow, Harness, Message, MessageSink, Role, Session, Text};

    fn user(texts: impl IntoIterator<Item = Text>) -> Message {
        let mut message = Message::new(Role::User);
        message.content = texts.into_iter().map(|text| Block::Text { text }).collect();
        message
    }
    // Independent prior collector: keeps every user turn and resolves only after EOF.
    #[derive(Default)]
    struct OldUsers(Vec<Message>);
    impl MessageSink for OldUsers {
        fn message(&mut self, message: Message) -> Flow {
            if message.role == Role::User {
                self.0.push(message);
            }
            Flow::Continue
        }
    }
    fn retained_text(users: &TitleUsers) -> usize {
        users
            .candidates
            .messages
            .iter()
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| {
                        if let Block::Text {
                            text: Text::Inline(text),
                        } = block
                        {
                            Some(text.len())
                        } else {
                            None
                        }
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    #[test]
    fn enrich_inline_history_matches_old_title_without_retaining_user_payloads() {
        for count in [100, 10_000] {
            for explicit in [None, Some("EOF title")] {
                let mut old = OldUsers::default();
                let mut reduced = TitleUsers::new(Harness::Claude);
                // A complete Inline reminder must not be capped before its closing tag.
                let long_reminder = format!("<system-reminder>{}</system-reminder>", "x".repeat(9000));
                for texts in [
                    vec!["Caveat: local command".to_string()],
                    vec!["<command-name>/help</command-name>".to_string()],
                    vec![long_reminder, "real question\nwith joined blocks".to_string()],
                ] {
                    let message = user(texts.into_iter().map(Text::from));
                    old.message(message.clone());
                    reduced.message(message);
                    assert!(reduced.candidates.messages.is_empty());
                    assert_eq!(retained_text(&reduced), 0);
                }
                for i in 0..count {
                    let message = user([Text::from(format!("later user {i}: {}", "y".repeat(128)))]);
                    old.message(message.clone());
                    reduced.message(message);
                }
                assert_eq!(old.0.len(), count + 3);
                assert!(reduced.candidates.messages.is_empty());
                assert!(reduced.fallback.as_ref().unwrap().len() <= 323);
                let mut metadata = Session::new("inline", Harness::Claude);
                metadata.title = explicit.map(str::to_owned);
                metadata.git = Some(cv_core::ir::GitInfo {
                    branch: Some("late branch".into()),
                    ..Default::default()
                });
                let mut baseline = metadata.clone();
                baseline.messages = old.0;
                let (actual, title) = reduced.finish(metadata, None);
                assert_eq!(title, baseline.synth_title());
                assert_eq!(actual.title, baseline.title);
                assert_eq!(actual.git.unwrap().branch.as_deref(), Some("late branch"));
                assert!(actual.messages.is_empty());
            }
        }
    }

    #[test]
    fn enrich_inline_noise_has_no_title_or_retained_messages() {
        let mut reduced = TitleUsers::new(Harness::Claude);
        let mut old = OldUsers::default();
        for _ in 0..1000 {
            let message = user([
                "<system-reminder>noise</system-reminder>".into(),
                "<command-message>/help</command-message>".into(),
            ]);
            old.message(message.clone());
            reduced.message(message);
        }
        let mut baseline = Session::new("noise", Harness::Claude);
        baseline.messages = old.0;
        let (actual, title) = reduced.finish(Session::new("noise", Harness::Claude), None);
        assert_eq!(title, baseline.synth_title());
        assert_eq!(title, None);
        assert!(actual.messages.is_empty());
    }

    struct Scripted {
        messages: Vec<Message>,
        metadata: Session,
        mutate: Option<std::path::PathBuf>,
        callbacks: std::sync::atomic::AtomicUsize,
    }
    impl cv_core::Adapter for Scripted {
        fn harness(&self) -> Harness {
            Harness::Claude
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<cv_core::Discovered> {
            Ok(Default::default())
        }
        fn stream(&self, _: &SessionRef, _: &cv_core::Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            for message in &self.messages {
                self.callbacks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(sink.message(message.clone()), Flow::Continue);
            }
            if let Some(path) = &self.mutate {
                std::fs::write(path, "\"changed after delivery\"")?;
            }
            Ok(self.metadata.clone())
        }
    }
    fn compare_streams(adapter: &Scripted, reference: &SessionRef) -> (Session, Option<String>) {
        // Candidate runs while its initial guarded source is still valid.
        let (actual, title) = enrich_head(adapter, reference).unwrap();
        let published = cv_core::catalog::AffordanceTable::load().counts(reference);
        let mut old = OldUsers::default();
        let mut baseline = cv_core::stream::stream_counted(
            adapter,
            reference,
            &cv_core::Demand::LEAN.with_spans(),
            &mut old,
            cv_core::stream::Pass::RECORD,
        )
        .unwrap()
        .session;
        baseline.messages = old.0;
        assert_eq!(published, cv_core::catalog::AffordanceTable::load().counts(reference));
        assert_eq!(title, baseline.synth_title());
        assert_eq!(
            serde_json::to_value(&actual.git).unwrap(),
            serde_json::to_value(&baseline.git).unwrap()
        );
        assert_eq!(
            adapter.callbacks.load(std::sync::atomic::Ordering::Relaxed),
            adapter.messages.len() * 2
        );
        (actual, title)
    }

    #[test]
    fn enrich_deferred_source_mutation_keeps_later_fallback() {
        let world = crate::test_world("enrich_source_mutation");
        let make_adapter = |id: &str| {
            let primary = world.base.join(format!("{id}.jsonl"));
            std::fs::write(&primary, "primary").unwrap();
            let body = world.base.join(format!("{id}-body.json"));
            std::fs::write(
                &body,
                serde_json::to_vec(&format!("first source prompt{}", "x".repeat(5000))).unwrap(),
            )
            .unwrap();
            let text = cv_core::Json::source(&body, 0, std::fs::metadata(&body).unwrap().len())
                .unwrap()
                .with_view(|view| Ok(view.source_text()?.unwrap()))
                .unwrap();
            assert!(matches!(text, Text::Source(_)));
            assert!(text
                .try_resolve_prefix(&cv_core::Resolver::new(None), 32)
                .unwrap()
                .starts_with("first source prompt"));
            (
                SessionRef::new(id, Harness::Claude, primary),
                Scripted {
                    messages: vec![
                        user(["Caveat: noise".into()]),
                        user([text]),
                        user(["later fallback".into()]),
                    ],
                    metadata: Session::new(id, Harness::Claude),
                    mutate: Some(body),
                    callbacks: Default::default(),
                },
            )
        };
        // Separate fresh captures ensure both readers deliver valid Sources before mutation.
        let (reference, adapter) = make_adapter("candidate");
        let (actual, title) = enrich_head(&adapter, &reference).unwrap();
        let (old_reference, old_adapter) = make_adapter("oracle");
        let mut old = OldUsers::default();
        let mut baseline = cv_core::stream::stream_counted(
            &old_adapter,
            &old_reference,
            &cv_core::Demand::LEAN.with_spans(),
            &mut old,
            cv_core::stream::Pass::RECORD,
        )
        .unwrap()
        .session;
        baseline.messages = old.0;
        assert_eq!(title, baseline.synth_title());
        assert_eq!(title.as_deref(), Some("later fallback"));
        assert!(actual.messages.is_empty());
        assert_eq!(adapter.callbacks.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert_eq!(old_adapter.callbacks.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[test]
    fn enrich_deferred_span_uses_eof_source_path_and_preserves_counts() {
        let world = crate::test_world("enrich_span_source");
        let primary = world.base.join("primary.jsonl");
        std::fs::write(&primary, "wrong source").unwrap();
        let body = world.base.join("resolved.txt");
        std::fs::write(&body, "\"right EOF source\"").unwrap();
        drop(cv_core::catalog::Writer::open().expect("scratch catalog"));
        let reference = SessionRef::new("span", Harness::Claude, primary);
        let mut metadata = Session::new("span", Harness::Claude);
        metadata.source_path = Some(body);
        metadata.git = Some(cv_core::ir::GitInfo {
            commit: Some("late commit".into()),
            ..Default::default()
        });
        let mut adapter = Scripted {
            messages: vec![
                user(["<system-reminder>noise</system-reminder>".into()]),
                user([Text::Span(cv_core::Span {
                    source: None,
                    offset: 1,
                    len: 16,
                    escaped: false,
                })]),
                user(["later fallback".into()]),
            ],
            metadata,
            mutate: None,
            callbacks: Default::default(),
        };
        let (actual, title) = compare_streams(&adapter, &reference);
        assert_eq!(title.as_deref(), Some("right EOF source"));
        assert!(actual.messages.is_empty());
        assert!(cv_core::catalog::AffordanceTable::load().counts(&reference).is_some());
        adapter.metadata.title = Some("explicit EOF title".into());
        adapter.callbacks.store(0, std::sync::atomic::Ordering::Relaxed);
        let (actual, title) = compare_streams(&adapter, &reference);
        assert_eq!(title.as_deref(), Some("explicit EOF title"));
        assert_eq!(actual.title.as_deref(), Some("explicit EOF title"));
    }
}
