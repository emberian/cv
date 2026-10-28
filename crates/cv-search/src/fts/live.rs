//! The unindexed suffix: what a full-text search reads live beside the index.
//!
//! The index is a cache and the transcripts are the truth. A search therefore answers from the
//! index plus a live read of every session the index has not seen as it is now: the
//! same freshness judgment `cv index` plans with ([`Freshness`]) names them, and each is read
//! through the same chunking the index writes ([`index_session`]) into a private in-memory
//! index on disk, from the session's resume mark when it grew (its unread suffix) and whole otherwise:
//! `cv index` takes marks at chunk boundaries (every 4 MiB of indexed text, or 32 MiB of source)
//! and where each read ended, so only what was appended since is read.
//! The private index lives in a temporary directory removed with it, so its heap is the writer's
//! arena whatever the read covers. Nothing else is written: no index, catalog, mark or count;
//! `--frozen` reads nothing live.
//!
//! **Ranking.** One scorer: both indexes are ranked by tantivy's BM25 with corpus statistics
//! summed over the two ([`Union`]), so a live chunk and an indexed chunk with the same text and
//! the same statistics score alike. The indexed docs a live read replaces (every doc of a session
//! read whole; the meta doc and the chunks from the resume point of a session read from its mark)
//! are excluded from the indexed query but, like tantivy's own deleted docs until a merge, still
//! counted in its statistics. A session's score is its best chunk on either side, so merging the
//! two top-`limit` lists is exact.
//!
//! **Budget.** By default every stale session is read ([`LiveRead::All`]). `--live-bytes` bounds
//! the read ([`LiveRead::Within`]), charged in source bytes: a file-backed session costs what lies
//! past its mark (its length when read whole), known before reading, and is skipped when it does
//! not fit what remains. A session in a shared store (a SQLite database holding many sessions)
//! has no per-session byte length, so it is charged the text it indexes as it goes and is
//! abandoned at the budget. Sessions are taken newest first. A session not read keeps whatever
//! the index last saw of it, and the footer says how many and how large.
use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// How much of what the index has not seen a search reads live.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LiveRead {
    /// Every stale session: the complete answer, and the default.
    #[default]
    All,
    /// At most this many bytes of source, newest sessions first (`--live-bytes`); the footer
    /// names what it left out.
    Within(u64),
    /// Nothing (`--frozen`): the index alone answers.
    Frozen,
}

impl LiveRead {
    /// The read a search makes: none under `--frozen`, else within `live_bytes` when given,
    /// else all of it.
    pub fn of(live_bytes: Option<u64>) -> Self {
        if cv_core::env::frozen() {
            Self::Frozen
        } else {
            live_bytes.map_or(Self::All, Self::Within)
        }
    }
}

/// What the live read beside the index covered: the facts the search footer states.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Suffix {
    /// What the search was allowed to read live.
    pub live: LiveRead,
    /// No index exists yet: every session was unindexed.
    pub no_index: bool,
    /// Sessions whose indexed docs are not current (changed since `cv index`, or never indexed).
    pub stale: usize,
    /// Of those, read live (from the mark when it held), and the source bytes that cost.
    pub read: usize,
    pub bytes: u64,
    /// Of those, read from their resume mark rather than whole.
    pub resumed: usize,
    /// Not read for the budget, and the bytes they would have cost (metered stores: what they
    /// had indexed when abandoned).
    pub cut: usize,
    pub cut_bytes: u64,
    /// Could not be read (unreadable, changed under the read).
    pub failed: usize,
    /// The gap between the index's newest stamp and the newest session file, ns.
    pub behind_ns: i64,
}

impl Suffix {
    /// Stale sessions this search did not read live: the index answered for them as it last saw
    /// them (or, unindexed, not at all).
    pub fn unread(&self) -> usize {
        self.stale - self.read
    }

    /// The footer line for what was read live; `None` when nothing was.
    pub fn read_line(&self) -> Option<String> {
        (self.read > 0).then(|| {
            let what = if self.no_index { "no index yet; read" } else { "read" };
            let resumed = if self.resumed > 0 {
                format!(", {} from where the index stopped", self.resumed)
            } else {
                String::new()
            };
            let of = match self.live {
                LiveRead::Within(budget) => format!(" of the {} --live-bytes budget", bytes(budget)),
                LiveRead::All | LiveRead::Frozen => String::new(),
            };
            format!(
                "{what} {} session(s) the index has not seen live ({}{of}{resumed})",
                self.read,
                bytes(self.bytes),
            )
        })
    }

    /// The footer line for what was left unread; `None` when every stale session was read.
    pub fn unread_line(&self) -> Option<String> {
        let unread = self.unread();
        if unread == 0 {
            return None;
        }
        if self.live == LiveRead::Frozen {
            return Some(format!(
                "--frozen: the index alone answered; it is ~{} behind for {unread} session(s) — run `cv index`",
                fmt_age_ns(self.behind_ns)
            ));
        }
        let mut why = Vec::new();
        if self.cut > 0 {
            why.push(format!(
                "{} over the --live-bytes budget ({})",
                self.cut,
                bytes(self.cut_bytes)
            ));
        }
        if self.failed > 0 {
            why.push(format!("{} unreadable", self.failed));
        }
        Some(if self.no_index {
            format!(
                "{unread} session(s) are not indexed and were not searched: {} — run `cv index`",
                why.join(", ")
            )
        } else {
            format!(
                "the index is ~{} behind for {unread} session(s) not read live: {} — run `cv index`",
                fmt_age_ns(self.behind_ns),
                why.join(", ")
            )
        })
    }
}

/// `12.3 MiB` / `4.0 KiB` / `512 B`: the sizes the footer names.
fn bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GiB", n as f64 / 1_073_741_824.0),
    }
}

/// How a session's live read is charged.
#[derive(Clone, Copy)]
enum Cost {
    /// Source bytes known before reading: the primary past the mark, or whole.
    Known(u64),
    /// A shared store: charged the text indexed, as it is indexed.
    Metered,
}

/// One stale session to read live.
struct Pending {
    job: Job,
    from: Option<cv_core::stream::Checkpoint>,
    cost: Cost,
}

/// What reading one pending session came to.
enum Outcome {
    Read { bytes: u64, resumed: Option<usize> },
    Cut { bytes: u64 },
    Failed,
}

/// The budget ran out under a metered read.
#[derive(Debug)]
struct OverBudget;
impl std::fmt::Display for OverBudget {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("over the live-read budget")
    }
}
impl std::error::Error for OverBudget {}

/// What the index does not describe as it is now, read into a private in-memory index.
pub(super) struct Live {
    /// Owns the temporary directory the searcher reads; dropped with it.
    _index: Index,
    pub(super) f: Fields,
    pub(super) searcher: tantivy::Searcher,
    /// Sessions the live index describes (its meta doc is current), with the first message it
    /// holds: the indexed docs it replaces are that session's meta doc and chunks from there on.
    pub(super) shadows: HashMap<String, usize>,
}

/// The indexed side a live read is planned against.
pub(super) struct Indexed<'a> {
    pub(super) index: &'a Index,
    pub(super) f: &'a Fields,
}

/// Plan the stale sessions of `refs` against the index (absent: every session is unindexed),
/// and, unless frozen, read them live. `refs` are top-level sessions; a folded sub-agent the
/// index holds is answered by the index as `cv index --subagents` last saw it.
pub(super) fn read(
    dir: &Path,
    indexed: Option<Indexed<'_>>,
    refs: &[cv_core::SessionRef],
    live: LiveRead,
) -> Result<(Option<Live>, Suffix)> {
    let mut suffix = Suffix {
        live,
        no_index: indexed.is_none(),
        ..Default::default()
    };
    let mut pending = plan(dir, indexed, refs, &mut suffix)?;
    let budget = match live {
        LiveRead::Frozen => return Ok((None, suffix)),
        LiveRead::All => u64::MAX,
        LiveRead::Within(budget) => budget,
    };
    // Newest first: what changed last is what a search most likely means.
    pending.sort_by_key(|p| std::cmp::Reverse((p.job.mtime, p.job.r.updated_at)));
    let mut remaining = budget;
    let mut admitted = Vec::with_capacity(pending.len());
    for p in pending {
        match p.cost {
            Cost::Known(cost) if cost > remaining => {
                suffix.cut += 1;
                suffix.cut_bytes += cost;
            }
            Cost::Known(cost) => {
                remaining -= cost;
                admitted.push(p);
            }
            Cost::Metered => admitted.push(p),
        }
    }
    if admitted.is_empty() {
        return Ok((None, suffix));
    }
    let ram = Index::create_from_tempdir(build_schema()).context("creating the live suffix's index")?;
    let f = fields_of(&ram.schema())?;
    let mut writer = open_writer(&ram, 50_000_000)?;
    // Metered reads draw on what the known costs left; known costs were reserved above.
    let remaining = AtomicU64::new(remaining);
    let next = AtomicUsize::new(0);
    let workers = available_workers().min(admitted.len());
    let outcomes: Vec<(usize, Outcome)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let (admitted, next, writer, f, remaining) = (&admitted, &next, &writer, &f, &remaining);
                scope.spawn(cv_core::env::carry(move || {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(p) = admitted.get(i) else { break };
                        done.push((i, read_one(p, writer, f, remaining)));
                    }
                    done
                }))
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    writer.commit().context("committing the live suffix")?;
    drop(writer);
    let mut shadows = HashMap::new();
    for (i, outcome) in outcomes {
        match outcome {
            Outcome::Read { bytes, resumed } => {
                suffix.read += 1;
                suffix.bytes += bytes;
                suffix.resumed += usize::from(resumed.is_some());
                shadows.insert(admitted[i].job.r.id.clone(), resumed.unwrap_or(0));
            }
            Outcome::Cut { bytes } => {
                suffix.cut += 1;
                suffix.cut_bytes += bytes;
            }
            Outcome::Failed => suffix.failed += 1,
        }
    }
    if shadows.is_empty() {
        return Ok((None, suffix));
    }
    let searcher = ram.reader().context("opening the live suffix")?.searcher();
    Ok((
        Some(Live {
            _index: ram,
            f,
            searcher,
            shadows,
        }),
        suffix,
    ))
}

/// The stale sessions of `refs`, each with where its read starts and what it costs; counts them
/// and how far the index trails into `suffix`.
fn plan(
    dir: &Path,
    indexed: Option<Indexed<'_>>,
    refs: &[cv_core::SessionRef],
    suffix: &mut Suffix,
) -> Result<Vec<Pending>> {
    let (existing, _, metadata) = match &indexed {
        Some(i) => read_indexed_metadata(i.index, i.f, true).context("reading index metadata")?,
        None => Default::default(),
    };
    let dependencies = match indexed_consumer(&dir.join("cv-index-id"))? {
        Some(consumer) if indexed.is_some() => cv_core::source::Table::<InIndex>::load(&consumer),
        _ => Default::default(),
    };
    let reader = cv_core::resume::Reader::default();
    let mut probe = cv_core::source::Probe::default();
    let newest_indexed = existing
        .values()
        .filter(|(_, size)| *size >= 0)
        .map(|&(mtime, _)| mtime)
        .max()
        .unwrap_or(0);
    let mut newest_on_disk = 0i64;
    let prov = cv_core::events::Provenance::default();
    let mut pending = Vec::new();
    for (r, _) in preferred_index_candidates(refs.iter().map(|r| (r, &prov)).collect()) {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let source = probe.snapshot(r);
        let fresh = Freshness::of(&existing, &metadata, &dependencies, &mut probe, r, &prov, &source);
        newest_on_disk = newest_on_disk.max(fresh.mtime);
        if fresh.fresh {
            continue;
        }
        suffix.stale += 1;
        let job = Job {
            r: r.clone(),
            mtime: fresh.mtime,
            size: fresh.size,
            fts: true,
            events: false,
            existing: existing.contains_key(&r.id),
            grew: existing.get(&r.id).is_some_and(|&(_, old)| old > 0 && fresh.size > old),
            source,
            dependencies_fresh: fresh.dependencies_fresh,
            prov: prov.clone(),
            mark: reader.head(r),
            decoded: None,
        };
        let from = resumable_from(&job, &cv_core::resume::version(adapter.as_ref(), RESUME_TAG), &reader);
        let cost = if adapter.sqlite_source(r) == Some(true) {
            Cost::Metered
        } else {
            let size = u64::try_from(fresh.size).unwrap_or(0);
            Cost::Known(size.saturating_sub(from.as_ref().map_or(0, |cp| cp.offset)))
        };
        pending.push(Pending { job, from, cost });
    }
    suffix.behind_ns = newest_on_disk.saturating_sub(newest_indexed);
    Ok(pending)
}

/// Read one pending session into the live index: from its mark when the adapter takes it, else
/// whole (charging the difference, or cut when it does not fit). A failed or cut read leaves
/// none of its docs behind.
fn read_one(p: &Pending, writer: &Writer, f: &Fields, remaining: &AtomicU64) -> Outcome {
    let Some(adapter) = cv_core::harness::for_harness(p.job.r.harness) else {
        return Outcome::Failed;
    };
    let job = &p.job;
    let charged = std::cell::Cell::new(0u64);
    let metered = matches!(p.cost, Cost::Metered);
    let mut out = |doc: TantivyDocument| -> Result<()> {
        if metered {
            let text: u64 = [f.body, f.context]
                .into_iter()
                .filter_map(|field| doc.get_first(field).and_then(|v| v.as_str()))
                .map(|s| s.len() as u64)
                .sum();
            if !take(remaining, text) {
                return Err(OverBudget.into());
            }
            charged.set(charged.get() + text);
        }
        writer.add_admitted(writer.admission.document(doc)?)?;
        Ok(())
    };
    let pass = |out: &mut DocOut<'_>, from| {
        index_session(
            out,
            f,
            adapter.as_ref(),
            &job.r,
            job.mtime,
            job.size,
            None,
            job.prov.clone(),
            from,
        )
    };
    let mut resumed = None;
    let mut result = match &p.from {
        Some(cp) => pass(&mut out, Some(cp)).map(|done| {
            resumed = done.is_some().then(|| resumes_docs_at(cp).unwrap_or(0));
            done.is_some()
        }),
        None => Ok(false),
    };
    let mut known = match p.cost {
        Cost::Known(cost) => cost,
        Cost::Metered => 0,
    };
    if matches!(result, Ok(false)) {
        // Declined (or no mark): a whole read, charged the rest of the file on a known cost.
        if let (Cost::Known(cost), Some(_)) = (p.cost, &p.from) {
            let whole = u64::try_from(job.size).unwrap_or(0);
            if !take(remaining, whole.saturating_sub(cost)) {
                return Outcome::Cut { bytes: whole };
            }
            known = whole;
        }
        result = pass(&mut out, None).and_then(|done| done.map(|_| true).context("a whole pass never declines"));
    }
    match result {
        Ok(_) => Outcome::Read {
            bytes: if metered { charged.get() } else { known },
            resumed,
        },
        Err(error) => {
            writer.delete_term(Term::from_field_text(f.id, &job.r.id));
            if error.chain().any(|cause| cause.is::<OverBudget>()) {
                Outcome::Cut { bytes: charged.get() }
            } else {
                Outcome::Failed
            }
        }
    }
}

/// Take `n` bytes from `remaining` when that many are left.
fn take(remaining: &AtomicU64, n: u64) -> bool {
    remaining
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(n))
        .is_ok()
}

/// BM25 statistics over the index and the live suffix together: one scorer for both sides.
pub(super) struct Union<'a>(
    pub(super) Option<&'a tantivy::Searcher>,
    pub(super) Option<&'a tantivy::Searcher>,
);

impl Union<'_> {
    fn sum(&self, each: impl Fn(&tantivy::Searcher) -> tantivy::Result<u64>) -> tantivy::Result<u64> {
        let mut total = 0;
        for searcher in [self.0, self.1].into_iter().flatten() {
            total += each(searcher)?;
        }
        Ok(total)
    }
}

impl tantivy::query::Bm25StatisticsProvider for Union<'_> {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        self.sum(|s| s.total_num_tokens(field))
    }
    fn total_num_docs(&self) -> tantivy::Result<u64> {
        self.sum(|s| s.total_num_docs())
    }
    fn doc_freq(&self, term: &Term) -> tantivy::Result<u64> {
        self.sum(|s| s.doc_freq(term))
    }
}

/// Exclude from `query` the indexed docs the live suffix replaces.
pub(super) fn without_shadowed(
    f: &Fields,
    query: Box<dyn tantivy::query::Query>,
    shadows: &HashMap<String, usize>,
) -> Box<dyn tantivy::query::Query> {
    use tantivy::query::{BooleanQuery, Occur, Query};
    if shadows.is_empty() {
        return query;
    }
    let shadowed: Vec<(Occur, Box<dyn Query>)> = shadows
        .iter()
        .map(|(id, &from)| (Occur::Should, Box::new(resumed_docs(f, id, from)) as Box<dyn Query>))
        .collect();
    Box::new(BooleanQuery::new(vec![
        (Occur::Must, query),
        (Occur::MustNot, Box::new(BooleanQuery::new(shadowed))),
    ]))
}
