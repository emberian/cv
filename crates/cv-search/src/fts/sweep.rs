//! Parallel sweep admission, worker feeding and publication.
use super::*;

/// Conservative primary-source pressure guard. This is not an allocated-heap estimate: mapped
/// Source JSON may own only descriptors, and a SQLite primary contains other sessions too.
/// Retain it until every adapter's parser working set has an independently enforced bound.
pub(super) const IN_FLIGHT_BYTES: u64 = 2 << 30;

/// Decoded bytes discovery may hand to [`sweep_decoded`] ahead of it (a larger rollout alone).
pub(super) const DECODED_WINDOW: u64 = 2 << 30;

/// Decoded rollouts discovery may hand ahead at once: one being read by each sweep worker and
/// one waiting for each. A handed rollout keeps two descriptors open (its source and its decoded
/// copy) until its job is done, so the early sweep holds a few per worker, never one per session.
pub(super) fn decoded_slots() -> usize {
    2 * available_workers()
}

/// The worker queue admits logical-session groups, largest first. Planning chooses one
/// preferred copy per ID; group reservations bound concurrent source bytes.
pub(super) struct Queue {
    /// Waiting groups keyed by (charge, arrival): the largest charge that fits is one ordered
    /// lookup, and among equal charges the latest arrival goes first. A sorted `Vec` re-sorted on
    /// every push made a backlog of Z groups fed one batch at a time O(Z²).
    groups: std::collections::BTreeMap<(u64, u64), Vec<Job>>,
    arrived: u64,
    pub(super) in_flight: u64,
    pub(super) draining: bool,
    pub(super) stopped: bool,
    /// No more groups will arrive.
    pub(super) closed: bool,
    /// Sessions [`Feed::push`] could not queue: their old baselines could not be invalidated, so
    /// their docs, baselines and marks stay as they were and a later pass reads them.
    deferred: Vec<Unreadable>,
}

/// The largest waiting group that fits. The workload is finite, so a large waiting group cannot
/// starve: after fitting work completes it is admitted alone when the guard empties.
impl Queue {
    pub(super) fn new() -> Self {
        Queue {
            groups: Default::default(),
            arrived: 0,
            in_flight: 0,
            draining: false,
            stopped: false,
            closed: false,
            deferred: Vec::new(),
        }
    }
    /// Queue one group under its charge ([`job_cost`], taken once here).
    pub(super) fn push(&mut self, group: Vec<Job>) {
        self.groups.insert((job_cost(&group), self.arrived), group);
        self.arrived += 1;
    }
    pub(super) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
    fn clear(&mut self) {
        self.groups.clear();
    }
    /// The key of the group [`Self::take_fitting`] would take.
    pub(super) fn next_fitting(&self) -> Option<(u64, u64)> {
        if self.stopped || self.draining {
            return None;
        }
        let room = IN_FLIGHT_BYTES.saturating_sub(self.in_flight);
        let fits = if self.in_flight == 0 { u64::MAX } else { room };
        self.groups.range(..=(fits, u64::MAX)).next_back().map(|(key, _)| *key)
    }
    /// Take the largest waiting group that fits, charging its reservation to `in_flight`.
    pub(super) fn take_fitting(&mut self) -> Option<(u64, Vec<Job>)> {
        let key = self.next_fitting()?;
        let group = self.groups.remove(&key)?;
        self.in_flight += key.0;
        Some((key.0, group))
    }
}

/// Return the parser reservation on every exit, including a disconnected writer.
pub(super) struct GroupReservation<'a> {
    pub(super) queue: &'a std::sync::Mutex<Queue>,
    pub(super) wake: &'a std::sync::Condvar,
    pub(super) bytes: u64,
}
impl Drop for GroupReservation<'_> {
    fn drop(&mut self) {
        self.queue.lock().unwrap().in_flight -= self.bytes;
        self.wake.notify_all();
    }
}

/// An early writer/commit failure must wake workers parked on either admission condition.
/// This guard drops before thread::scope waits for workers, avoiding disconnected-channel hangs.
pub(super) struct Shutdown<'a> {
    pub(super) queue: &'a std::sync::Mutex<Queue>,
    pub(super) wake: &'a std::sync::Condvar,
    pub(super) admission: &'a Admission,
    pub(super) complete: bool,
}
impl Shutdown<'_> {
    fn stop(&self) {
        let mut q = self.queue.lock().unwrap();
        q.stopped = true;
        // Waiting jobs may hold decoded sources discovery is waiting to see freed.
        q.clear();
        drop(q);
        self.admission.close();
        self.wake.notify_all();
    }
}
impl Drop for Shutdown<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.stop();
        }
    }
}

pub(super) fn job_cost(group: &[Job]) -> u64 {
    group
        .iter()
        // A compressed extent does not bound the decoded working set: charge the bytes discovery
        // already decoded, else the size the frame header declares, else the whole guard (the
        // source runs alone). Charging every compressed source the whole guard ran a cold index
        // of compressed rollouts one at a time.
        .map(|j| match &j.decoded {
            Some(decoded) => decoded.decoded_bytes(),
            None if j.r.path.extension().is_some_and(|ext| ext == "zst") => {
                cv_core::codex_handoff::decoded_size_hint(&j.r.path).unwrap_or(IN_FLIGHT_BYTES)
            }
            None => j.size.max(0) as u64,
        })
        .fold(0u64, u64::saturating_add)
        .min(IN_FLIGHT_BYTES)
}

/// What [`run_jobs`] did.
#[derive(Default)]
pub(super) struct Ran {
    #[cfg(test)]
    pub(super) observed: observe::Sweep,
    /// Event and search baseline publications that failed: the catalog could not be written.
    pub(super) errors: usize,
    pub(super) first_error: Option<String>,
    /// Sessions whose read failed: each stays out of the index until a later pass reads it.
    pub(super) unreadable: Vec<Unreadable>,
    /// Sessions whose FTS docs were rewritten.
    pub(super) changed: usize,
    /// Sessions read from their mark on.
    pub(super) resumed: usize,
    /// Mark updates still waiting for the commit that holds their docs.
    pub(super) marks: Vec<cv_core::resume::MarkUpdate>,
    pub(super) dependencies: Vec<cv_core::source::Update>,
}

/// Publish the committed docs' baselines; a row that fails is an error of the pass (its session
/// is read again next pass) and the rest stick.
pub(super) fn publish_dependencies(ran: &mut Ran, consumer: &str) {
    for (r, why) in cv_core::source::update::<InIndex>(consumer, &std::mem::take(&mut ran.dependencies)) {
        let message = format!("search baseline publication failed for {} ({}): {why}", r.id, r.harness);
        eprintln!("cv: {message}");
        ran.errors += 1;
        ran.first_error
            .get_or_insert_with(|| cv_core::ir::truncate(&message, 512));
    }
}

pub(super) fn available_workers() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// Run every job on a pool of workers, writing on this thread.
pub(super) fn run_jobs(writer: &mut Writer, f: &Fields, jobs: Vec<Job>, consumer: &str) -> Result<Ran> {
    if jobs.is_empty() {
        return Ok(Ran::default());
    }
    let workers = available_workers().min(jobs.len());
    run_fed(writer, f, consumer, workers, |feed| feed.push(jobs))
}

/// Where a sweep's jobs enter its worker queue, all at once or as they are planned.
pub(super) struct Feed<'a> {
    pub(super) queue: &'a std::sync::Mutex<Queue>,
    pub(super) wake: &'a std::sync::Condvar,
    pub(super) consumer: &'a str,
}

impl Feed<'_> {
    /// Queue `jobs`, invalidating the old dependency baselines of those that rewrite docs first.
    /// When that fails (the catalog cannot be opened or written: out of descriptors, say), the
    /// jobs that rewrite docs are not run: each is named unreadable for this pass, its docs and
    /// baseline left for the next. Fails once the sweep stopped.
    pub(super) fn push(&self, mut jobs: Vec<Job>) -> Result<()> {
        let changing: Vec<_> = jobs.iter().filter(|j| j.fts).map(|j| j.r.clone()).collect();
        if let Err(err) = cv_core::source::invalidate(self.consumer, &changing) {
            let reason = cv_core::ir::truncate(&format!("invalidating old search dependency baselines: {err:#}"), 512);
            let (deferred, rest): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|j| j.fts);
            jobs = rest;
            let mut q = self.queue.lock().unwrap();
            for job in deferred {
                eprintln!("cv: index deferred for {} ({}): {reason}", job.r.id, job.r.harness);
                q.deferred.push(Unreadable {
                    id: job.r.id,
                    harness: job.r.harness.to_string(),
                    reason: reason.clone(),
                });
            }
        }
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut groups: Vec<Vec<Job>> = Vec::new();
        for j in jobs {
            match by_id.get(&j.r.id) {
                Some(&g) => groups[g].push(j),
                None => {
                    by_id.insert(j.r.id.clone(), groups.len());
                    groups.push(vec![j]);
                }
            }
        }
        let mut q = self.queue.lock().unwrap();
        anyhow::ensure!(!q.stopped, "index sweep stopped");
        for group in groups {
            q.push(group);
        }
        drop(q);
        self.wake.notify_all();
        Ok(())
    }
}

/// Run the jobs `feed` queues on `workers` workers, writing on this thread, until `feed` returns
/// and its jobs are done.
pub(super) fn run_fed(
    writer: &mut Writer,
    f: &Fields,
    consumer: &str,
    workers: usize,
    feed: impl FnOnce(&Feed) -> Result<()> + Send,
) -> Result<Ran> {
    use std::sync::{mpsc, Condvar, Mutex};
    let queue = Mutex::new(Queue::new());
    let wake = Condvar::new();
    let (tx, rx) = mpsc::sync_channel::<Out>(2 * workers);
    let admission = std::sync::Arc::clone(&writer.admission);
    let sources = cv_core::source::read_scope();
    // Initialize the catalog before the feeder opens it to invalidate old baselines. Racing
    // first opens can fail SQLite's transition to WAL before any jobs are admitted.
    let mut catalog = cv_core::catalog::Writer::open();
    let mut col_store = (!cv_core::env::frozen())
        .then(|| crate::columnar_events::ColumnarEventStore::open(crate::default_columnar_events_dir()))
        .transpose()
        .context("opening columnar events store")?;

    std::thread::scope(|scope| -> Result<Ran> {
        let (queue, wake) = (&queue, &wake);
        let sources = &sources;
        let fed = scope.spawn(cv_core::env::carry(move || {
            let fed = sources.run(|| feed(&Feed { queue, wake, consumer }));
            queue.lock().unwrap().closed = true;
            wake.notify_all();
            fed
        }));
        for _ in 0..workers {
            let tx = tx.clone();
            let admission = &admission;
            scope.spawn(cv_core::env::carry(move || {
                let marks = cv_core::resume::Reader::default();
                sources.run(|| loop {
                    let group = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if q.stopped || (q.closed && q.is_empty()) {
                                break None;
                            }
                            if let Some(taken) = q.take_fitting() {
                                break Some(taken);
                            }
                            q = wake.wait(q).unwrap();
                        }
                    };
                    let Some((bytes, group)) = group else { return };
                    let _reservation = GroupReservation { queue, wake, bytes };
                    for job in group {
                        if !run_job(job, f, &tx, admission, &marks) {
                            return; // the writer is gone
                        }
                    }
                })
            }));
        }
        drop(tx);

        let mut shutdown = Shutdown {
            queue,
            wake,
            admission: &admission,
            complete: false,
        };
        let mut open: HashSet<String> = HashSet::new();
        let mut since_commit = 0usize;
        let mut ran = Ran::default();
        let mut failure: Option<anyhow::Error> = None;
        let mut col_batch = col_store
            .as_mut()
            .map(crate::columnar_events::ColumnarEventStore::begin_batch);
        for out in rx {
            match out {
                Out::Begin { id, delete } => {
                    match delete {
                        Delete::Nothing => {}
                        Delete::All => {
                            writer.delete_term(Term::from_field_text(f.id, &id));
                        }
                        Delete::From(n) => {
                            if let Err(e) = writer.delete_query(Box::new(resumed_docs(f, &id, n))) {
                                failure.get_or_insert(e.into());
                                shutdown.stop();
                            }
                        }
                    }
                    open.insert(id);
                }
                Out::Doc(doc) => {
                    #[cfg(test)]
                    ran.observed.document(&doc, f);
                    if failure.is_none() {
                        if let Err(e) = writer.add_admitted(doc) {
                            failure = Some(e);
                            shutdown.stop();
                        }
                    }
                    since_commit += 1;
                }
                Out::Done {
                    job,
                    events,
                    counted,
                    mark,
                    resumed,
                    fts_reads,
                    #[cfg(test)]
                    callbacks,
                } => {
                    #[cfg(test)]
                    {
                        ran.observed.work.callbacks += callbacks;
                    }
                    // Only stamp events after a complete pass: a parse error leaves the sync rows
                    // untouched so the session is retried next run.
                    let source = counted
                        .as_ref()
                        .map_or_else(|| job.source.clone(), |c| job.source.with_reads(&c.dependencies));
                    let events_published = match events {
                        Some((from, events)) => {
                            let need_sql = job.sql_events;
                            let need_col = job.col_events && col_batch.is_some();
                            let teed = if need_sql && need_col {
                                let mut col_copy = cv_core::events::EventSpool::default();
                                events
                                    .tee(|ev| col_copy.push(ev.clone()))
                                    .map(|rebuilt| (Some(rebuilt), Some(col_copy)))
                            } else if need_col {
                                Ok((None, Some(events)))
                            } else {
                                Ok((need_sql.then_some(events), None))
                            };
                            let result = teed.and_then(|(sql_spool, col_spool)| {
                                if let Some(events) = sql_spool {
                                    match catalog.as_mut() {
                                        Some(catalog) => {
                                            catalog.record_events(&job.r, from, events, &source, &job.prov)?
                                        }
                                        None => cv_core::events::record_spooled_with_source(
                                            &job.r, from, events, &source, &job.prov,
                                        )?,
                                    }
                                }
                                if let (Some(batch), Some(col_spool)) = (col_batch.as_mut(), col_spool) {
                                    let rows = col_spool.into_rows()?;
                                    if from == 0 || batch.can_resume(&job.r, &job.prov) {
                                        batch.stage_session(&job.r, from, rows, &source, &job.prov, None)?;
                                    } else {
                                        batch.invalidate_session(&job.r)?;
                                        anyhow::bail!(
                                            "columnar event authority mismatch on resumed pass for {} ({})",
                                            job.r.id,
                                            job.r.harness
                                        );
                                    }
                                }
                                Ok(())
                            });
                            match result {
                                Ok(()) => true,
                                Err(err) => {
                                    ran.errors += 1;
                                    ran.first_error.get_or_insert_with(|| {
                                        cv_core::ir::truncate(
                                            &format!(
                                                "event publication failed for {} ({}): {err:#}",
                                                job.r.id, job.r.harness
                                            ),
                                            512,
                                        )
                                    });
                                    eprintln!(
                                        "cv: event publication failed for {} ({}): {err:#}",
                                        job.r.id, job.r.harness
                                    );
                                    false
                                }
                            }
                        }
                        None => true,
                    };
                    if let Some(c) = counted {
                        match catalog.as_mut() {
                            Some(catalog) => catalog.record_counted(&job.r, &job.source, c),
                            None => c.record_with_source(&job.r, &job.source),
                        }
                    }
                    if mark.is_some() || job.mark.is_some() {
                        // A mark must describe persisted event rows as well as FTS docs. If the
                        // SQL transaction failed, retry the entire event pass rather than resume
                        // past a missing suffix and incorrectly stamp it fresh.
                        ran.marks.push(cv_core::resume::MarkUpdate {
                            session: job.r.clone(),
                            mark: if events_published { mark } else { None },
                        });
                    }
                    ran.resumed += usize::from(resumed);
                    if job.fts {
                        ran.dependencies
                            .push((job.r.clone(), job.source.with_reads(&fts_reads)));
                        ran.changed += 1;
                        open.remove(&job.r.id);
                    }
                }
                Out::Failed { job, err } => {
                    ran.unreadable.push(Unreadable {
                        id: job.r.id.clone(),
                        harness: job.r.harness.to_string(),
                        reason: cv_core::ir::truncate(&format!("{err:#}"), 512),
                    });
                    // The session drops out of the index (and its skip-set entry) until a later
                    // run parses it whole — never frozen in as a truncated body stamped fresh.
                    if job.fts {
                        writer.delete_term(Term::from_field_text(f.id, &job.r.id));
                        open.remove(&job.r.id);
                    }
                    if job.mark.is_some() {
                        ran.marks.push(cv_core::resume::MarkUpdate {
                            session: job.r.clone(),
                            mark: None,
                        });
                    }
                    eprintln!("cv: parse failed for {} ({}): {err:#}", job.r.id, job.r.harness);
                }
            }
            if since_commit >= COMMIT_EVERY && failure.is_none() {
                if open.is_empty() {
                    writer.commit().context("interim commit")?;
                    if let Some(batch) = col_batch.as_mut() {
                        batch.checkpoint_commit().context("interim columnar commit")?;
                    }
                    cv_core::resume::update(&std::mem::take(&mut ran.marks));
                    publish_dependencies(&mut ran, consumer);
                    since_commit = 0;
                    let mut q = queue.lock().unwrap();
                    q.draining = false;
                    drop(q);
                    wake.notify_all();
                } else if since_commit >= 8 * COMMIT_EVERY {
                    queue.lock().unwrap().draining = true;
                }
            }
        }
        let fed = fed.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        ran.unreadable.append(&mut queue.lock().unwrap().deferred);
        match failure.map_or(fed, Err) {
            Err(e) => Err(e),
            Ok(()) => {
                if let Some(batch) = col_batch {
                    batch.commit().context("committing columnar events")?;
                }
                shutdown.complete = true;
                Ok(ran)
            }
        }
    })
}
