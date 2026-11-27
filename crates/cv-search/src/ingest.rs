//! Raw document admission spans both the producer channel and Tantivy's indexing pipeline.
//!
//! Tantivy's arena budget excludes its 10,000-document input queue. Each document owns a byte
//! reservation until Tantivy finishes consuming it, so moving between queues cannot evade this
//! bound. Parser working sets, worker body buffers, index arenas and merges are separate costs.
use anyhow::{Context, Result};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Condvar, Mutex};
use tantivy::schema::Document;
use tantivy::{Index, IndexWriter, Opstamp, TantivyDocument};

pub(super) const DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
const TANTIVY_MIN_HEAP_PER_THREAD: usize = 15_000_000;

pub(super) fn writer_threads_for(workers: usize, heap: usize) -> usize {
    let max_by_heap = (heap / TANTIVY_MIN_HEAP_PER_THREAD).max(1);
    workers.min(4).min(max_by_heap).max(1)
}

#[derive(Default)]
struct State {
    bytes: usize,
    peak: usize,
    largest: usize,
    waits: usize,
    waiters: usize,
    closed: bool,
}

/// Admission counters at a sweep boundary: bytes currently charged, the peak concurrent
/// charge, the largest single document charged, and how often a worker waited for room.
#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct AdmissionStats {
    pub(super) charged_bytes: usize,
    pub(super) peak_charged_bytes: usize,
    pub(super) largest_document_bytes: usize,
    pub(super) queue_waits: usize,
}

pub(super) struct Admission {
    limit: usize,
    state: Mutex<State>,
    wake: Condvar,
}
impl Admission {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        })
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation> {
        let mut state = self.state.lock().unwrap();
        // An oversized document runs alone. Rejecting it would silently change what is indexed;
        // charging its actual allocation preserves semantics and prevents concurrent oversize.
        while !state.closed && state.bytes != 0 && bytes > self.limit.saturating_sub(state.bytes) {
            state.waits += 1;
            state.waiters += 1;
            let waited = self.wake.wait(state);
            state = waited.unwrap();
            state.waiters -= 1;
        }
        anyhow::ensure!(!state.closed, "index document admission closed");
        state.bytes = state
            .bytes
            .checked_add(bytes)
            .context("index document byte count overflow")?;
        state.peak = state.peak.max(state.bytes);
        state.largest = state.largest.max(bytes);
        Ok(Reservation {
            admission: Arc::clone(self),
            bytes,
        })
    }

    pub(super) fn document(self: &Arc<Self>, document: TantivyDocument) -> Result<AdmittedDocument> {
        let reservation = self.reserve(document_bytes(&document))?;
        Ok(AdmittedDocument {
            document,
            _reservation: reservation,
        })
    }

    pub(super) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.wake.notify_all();
    }

    #[cfg(test)]
    pub(super) fn stats(&self) -> AdmissionStats {
        let state = self.state.lock().unwrap();
        AdmissionStats {
            charged_bytes: state.bytes,
            peak_charged_bytes: state.peak,
            largest_document_bytes: state.largest,
            queue_waits: state.waits,
        }
    }
}

/// The compact document owns one public byte vector and a private field-address vector. Charge
/// the exact byte capacity plus twice the field count at a conservative address size (the private
/// packed address is smaller). A minimum four slots covers small-vector allocation slack; the
/// extra 128 bytes conservatively covers vector headers and allocator overhead beyond the wrapper.
fn document_bytes(document: &TantivyDocument) -> usize {
    document
        .node_data
        .capacity()
        .saturating_add(
            document
                .len()
                .max(4)
                .saturating_mul(2 * std::mem::size_of::<(usize, usize)>()),
        )
        .saturating_add(std::mem::size_of::<AdmittedDocument>() + 128)
}

struct Reservation {
    admission: Arc<Admission>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let notify = {
            let mut state = self.admission.state.lock().unwrap();
            state.bytes -= self.bytes;
            state.waiters > 0
        };
        if notify {
            self.admission.wake.notify_all();
        }
    }
}

// Field order matters: free the document's allocations before returning its reservation.
pub(super) struct AdmittedDocument {
    document: TantivyDocument,
    _reservation: Reservation,
}
#[cfg(test)]
impl AdmittedDocument {
    pub(super) fn inner(&self) -> &TantivyDocument {
        &self.document
    }
}
impl Document for AdmittedDocument {
    type Value<'a> = <TantivyDocument as Document>::Value<'a>;
    type FieldsValuesIter<'a> = <TantivyDocument as Document>::FieldsValuesIter<'a>;
    fn iter_fields_and_values(&self) -> Self::FieldsValuesIter<'_> {
        self.document.iter_fields_and_values()
    }
}

/// Every write path, including single-session/test indexing, uses the same raw-document budget.
pub(super) struct Writer {
    /// `None` only while [`Drop`] closes it.
    inner: Option<IndexWriter<AdmittedDocument>>,
    pub(super) admission: Arc<Admission>,
    #[cfg(test)]
    threads: usize,
    fault: Fault,
}
impl Writer {
    pub(super) fn new(index: &Index, heap: usize) -> Result<Self> {
        let threads = writer_threads_for(cv_core::configured_worker_threads(), heap);
        let inner = index
            .writer_with_num_threads(threads, heap)
            .context("creating tantivy index writer")?;
        Ok(Self {
            inner: Some(inner),
            admission: Admission::new(document_limit()),
            #[cfg(test)]
            threads,
            fault: Fault::default(),
        })
    }
    pub(super) fn add_admitted(&self, document: AdmittedDocument) -> Result<Opstamp> {
        self.fault.check()?;
        (**self).add_document(document).map_err(Into::into)
    }
}

#[cfg(not(all(test, feature = "probe-heap")))]
fn document_limit() -> usize {
    DOCUMENT_BYTES
}
#[cfg(all(test, feature = "probe-heap"))]
fn document_limit() -> usize {
    if std::env::var_os("CV_SWEEP_COMPARE_UNBOUNDED").is_some() {
        usize::MAX
    } else {
        DOCUMENT_BYTES
    }
}
#[cfg(not(test))]
#[derive(Default)]
struct Fault {}
#[cfg(not(test))]
impl Fault {
    fn check(&self) -> Result<()> {
        Ok(())
    }
}
#[cfg(test)]
#[derive(Default)]
struct Fault(Mutex<Option<usize>>);
#[cfg(test)]
impl Fault {
    fn check(&self) -> Result<()> {
        if let Some(remaining) = self.0.lock().unwrap().as_mut() {
            anyhow::ensure!(*remaining != 0, "injected index consumer failure");
            *remaining -= 1;
        }
        Ok(())
    }
}
#[cfg(test)]
impl Writer {
    pub(super) fn num_threads(&self) -> usize {
        self.threads
    }
    pub(super) fn add_document(&self, document: TantivyDocument) -> Result<Opstamp> {
        self.add_admitted(self.admission.document(document)?)
    }
    pub(super) fn document_limit(&mut self, bytes: usize) {
        self.admission = Admission::new(bytes);
    }
    pub(super) fn fail_after(&self, documents: usize) {
        *self.fault.0.lock().unwrap() = Some(documents);
    }
}

/// tantivy's `IndexWriter` drop joins its indexing workers but not the segment updater, whose
/// post-commit garbage collection (and any merge) can still be writing the directory after the
/// writer is gone: a test removing the directory then failed with "Directory not empty", and a
/// process could exit mid-GC. Every writer joins that work as it closes.
impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let _ = inner.wait_merging_threads();
        }
    }
}

impl Deref for Writer {
    type Target = IndexWriter<AdmittedDocument>;
    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("an open writer")
    }
}
impl DerefMut for Writer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().expect("an open writer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn document_admission_follows_the_payload_and_close_wakes_waiters() {
        let admission = Admission::new(128);
        let first = admission.reserve(96).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let admission = Arc::clone(&admission);
            scope.spawn(move || tx.send(admission.reserve(64)).unwrap());
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            drop(first);
            let next = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(next.admission.stats().charged_bytes, 64);
            // Moving a reservation through a queue cannot return its bytes early.
            let (tx, rx) = mpsc::sync_channel(1);
            tx.send(next).unwrap();
            assert_eq!(admission_stats_from_receiver(&rx).charged_bytes, 0);
        });
        assert_eq!(admission.stats().charged_bytes, 0);
        assert!(admission.stats().peak_charged_bytes <= 128);

        let oversized = admission.reserve(256).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let other = Arc::clone(&admission);
            scope.spawn(move || tx.send(other.reserve(1)).unwrap());
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            admission.close();
            assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
            drop(oversized);
        });
        assert_eq!(admission.stats().charged_bytes, 0);
        assert_eq!(admission.stats().peak_charged_bytes, 256);
    }

    fn admission_stats_from_receiver(rx: &mpsc::Receiver<Reservation>) -> AdmissionStats {
        let reservation = rx.recv().unwrap();
        let admission = Arc::clone(&reservation.admission);
        assert_eq!(admission.stats().charged_bytes, reservation.bytes);
        drop(reservation);
        admission.stats()
    }

    #[test]
    fn tantivy_commit_and_writer_drop_return_all_document_reservations() {
        let mut schema = tantivy::schema::Schema::builder();
        let body = schema.add_text_field("body", tantivy::schema::TEXT);
        let index = Index::create_in_ram(schema.build());
        let mut writer = Writer::new(&index, 15_000_000).unwrap();
        writer.document_limit(1024 * 1024);
        let admission = Arc::clone(&writer.admission);
        for _ in 0..40 {
            let mut document = TantivyDocument::default();
            document.add_text(body, "x".repeat(600_000));
            writer.add_document(document).unwrap();
        }
        writer.commit().unwrap();
        assert_eq!(admission.stats().charged_bytes, 0);
        let stats = admission.stats();
        assert!(stats.peak_charged_bytes <= admission.limit.max(stats.largest_document_bytes));
        assert_eq!(index.reader().unwrap().searcher().num_docs(), 40);
        writer.fail_after(0);
        let mut document = TantivyDocument::default();
        document.add_text(body, "failed");
        assert!(writer.add_document(document).is_err());
        assert_eq!(admission.stats().charged_bytes, 0);
        drop(writer);
        assert_eq!(admission.stats().charged_bytes, 0);
    }
}
