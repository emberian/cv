//! Test observations at sweep boundaries; no product bookkeeping.
use super::*;

/// Successful-sweep delivered callbacks and document submissions, not input scans or timings.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SweepWork {
    pub(super) callbacks: usize,
    pub(super) documents: usize,
    pub(super) commits: usize,
    pub(super) body_bytes: usize,
    pub(super) writer_opened: bool,
}

#[cfg(feature = "probe-heap")]
#[derive(serde::Serialize)]
pub(super) struct BodySummary {
    pub(super) bytes: usize,
    pub(super) docs: usize,
    pub(super) hash: u64,
}
#[cfg(feature = "probe-heap")]
impl Default for BodySummary {
    fn default() -> Self {
        Self {
            bytes: 0,
            docs: 0,
            hash: 0xcbf29ce484222325,
        }
    }
}
pub(super) fn body_hash(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Default)]
pub(super) struct Sweep {
    pub(super) admission: crate::ingest::AdmissionStats,
    pub(super) work: SweepWork,
    pub(super) early: usize,
    #[cfg(feature = "probe-heap")]
    pub(super) bodies: HashMap<String, BodySummary>,
}
impl Sweep {
    pub(super) fn finished(mut self, admission: &Admission) -> Self {
        self.admission = admission.stats();
        self.work.writer_opened = true;
        self
    }
    pub(super) fn document(&mut self, doc: &AdmittedDocument, f: &Fields) {
        self.work.documents += 1;
        let inner = doc.inner();
        if let Some(body) = inner.get_first(f.body).and_then(|value| value.as_str()) {
            self.work.body_bytes += body.len();
            #[cfg(feature = "probe-heap")]
            if std::env::var_os("CV_SWEEP_HASH_BODY").is_some() {
                if let Some(id) = inner.get_first(f.id).and_then(|value| value.as_str()) {
                    let summary = self.bodies.entry(id.to_owned()).or_default();
                    summary.bytes += body.len();
                    summary.docs += 1;
                    summary.hash = body_hash(summary.hash, body.as_bytes());
                }
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Query {
    pub(super) queries: std::sync::atomic::AtomicUsize,
    pub(super) stored_reads: std::sync::atomic::AtomicUsize,
    pub(super) winner_body_reads: std::sync::atomic::AtomicUsize,
    pub(super) winner_metadata_reads: std::sync::atomic::AtomicUsize,
}
