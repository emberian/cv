//! Byte marks: where a pass over a grown append-only transcript picks back up.
//!
//! A pass that streams through [`stream::stream_counted`](crate::stream::stream_counted) with checkpoints
//! takes [`Checkpoint`]s as it goes; its last one, stored here as a **mark** per transcript file,
//! lets the next pass over the same file (grown by an append) read only what came after it. A
//! mark passes its guards when the file's device/inode identity matches, it is at least `offset`
//! bytes long, the sampled head and bytes immediately before `offset` hash as they did, and it was
//! written under this stored-format version, adapter state format and consumer ([`version`]). Anything else means a whole pass, which writes a new mark.
//!
//! These samples ([`Prefix`]) do not establish that the entire prefix is unchanged, here or in
//! the append admission of a captured source ([`crate::source::FileId::admits`]), which uses the
//! same sample. Both assume the adapter's immutable-prefix append contract: an in-place edit
//! outside the samples (past the first 4 KiB and before the last 4 KiB under the mark or captured
//! length), at the same length or followed by growth, escapes detection, and the docs and counts
//! of the edited bytes stay as they were read. Proving arbitrary prefix edits absent requires
//! reading that prefix again: hashing the whole prefix on every resume was measured and rejected
//! (docs/design/PERF.md, "Resume marks").
//!
//! The marks live in the catalog db beside the counts and events they continue (`resume_marks`);
//! like the rest of the catalog it is a cache, best-effort throughout.

use crate::harness::Adapter;
use crate::ir::SessionRef;
use crate::source::Prefix;
use crate::stream::Checkpoint;
use std::path::Path;

/// A stored mark minus its state: version, and the sampled prefix it continues.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MarkHead {
    pub prefix: Prefix,
    pub version: String,
}

impl MarkHead {
    /// The head of a mark for `cp`, taken on `path` by a pass at `version`. `None` when the file
    /// can no longer be read up to the checkpoint, or stable file identity is unavailable.
    /// Device/inode guards are currently implemented on Unix; other platforms read whole.
    pub fn of(path: &Path, cp: &Checkpoint, version: &str) -> Option<MarkHead> {
        Self::of_file(&std::fs::File::open(path).ok()?, cp, version)
    }

    /// [`Self::of`] on an open handle: the identity and samples of the file that handle names,
    /// whatever its path names now. A pass that opens its source before reading takes its mark
    /// this way, so a file renamed over the path mid-pass is never paired with this pass's
    /// checkpoint (its next pass sees another inode and reads whole).
    pub fn of_file(file: &std::fs::File, cp: &Checkpoint, version: &str) -> Option<MarkHead> {
        // Without a stable file identity (off Unix), prefer a whole read.
        #[cfg(not(unix))]
        let mark = {
            let _ = (file, cp, version);
            None
        };
        #[cfg(unix)]
        let mark = Some(MarkHead {
            prefix: Prefix::of(file, cp.offset)?,
            version: version.to_string(),
        });
        mark
    }

    pub fn offset(&self) -> u64 {
        self.prefix.len
    }

    /// The one admission of a stored checkpoint: written at `version`, describing `cp` (its
    /// offset is the mark's), and the file `path` names now still holds the sampled prefix
    /// ([`Prefix::holds`]). This does not establish that the whole old prefix is unchanged.
    pub fn admit<'c>(&self, path: &Path, version: &str, cp: &'c Checkpoint) -> Option<&'c Checkpoint> {
        (self.version == version && cp.offset == self.prefix.len && self.prefix.holds(path)).then_some(cp)
    }
}

/// The version a mark records and must match: the one stored-format version
/// ([`crate::source::VERSION`], which the affordance view shares), the harness and `consumer`
/// (the caller's own state format). Not the cv release: a release that changes what a mark means
/// bumps one of those. A checkpoint's codec state carries no version of its own: what a codec
/// keeps, and what a record parses to, are versioned by that same `source::VERSION`.
pub fn version(adapter: &dyn Adapter, consumer: &str) -> String {
    crate::source::consumer_version(format_args!("{}/{consumer}", adapter.harness().as_str()))
}

/// A session and its new mark (`mark: None`: it has none any more), for [`update`].
pub struct MarkUpdate {
    pub session: SessionRef,
    pub mark: Option<(MarkHead, Checkpoint)>,
}

#[cfg(feature = "sqlite")]
mod imp {
    use super::*;
    use rusqlite::{params, Connection};
    use std::collections::HashMap;

    /// The catalog connection: `resume_marks` is part of its migration.
    fn open() -> Option<Connection> {
        crate::catalog::open_db()
    }

    fn key(r: &SessionRef) -> (String, String, String) {
        (
            r.harness.as_str().to_string(),
            r.id.clone(),
            r.path.to_string_lossy().into_owned(),
        )
    }

    /// Every mark's head. Index and live planners use [`Reader::head`] instead: SQLite may
    /// traverse a row's large state to reach its trailing identity, even without selecting state.
    pub struct MarkTable {
        rows: HashMap<(String, String, String), MarkHead>,
    }

    impl MarkTable {
        pub fn load() -> MarkTable {
            #[cfg(feature = "probe-resume")]
            probe::note(|reads| reads.scans += 1);
            let load = || -> Option<HashMap<(String, String, String), MarkHead>> {
                let conn = open()?;
                let mut stmt = conn
                    .prepare("SELECT harness, id, path, offset, hash, version, identity FROM resume_marks")
                    .ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            (row.get(0)?, row.get(1)?, row.get(2)?),
                            row_head(row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?),
                        ))
                    })
                    .ok()?;
                Some(rows.flatten().filter_map(|(key, head)| Some((key, head?))).collect())
            };
            MarkTable {
                rows: load().unwrap_or_default(),
            }
        }

        pub fn get(&self, r: &SessionRef) -> Option<&MarkHead> {
            self.rows.get(&key(r))
        }
    }

    /// The checkpoint `r`'s mark holds, when it is still the one `head` describes.
    pub fn state(r: &SessionRef, head: &MarkHead) -> Option<Checkpoint> {
        Reader::default().state(r, head)
    }

    /// One operation's lazy catalog reader, reused by every job in an index worker.
    pub struct Reader(crate::discover_cache::read::Context);
    impl Default for Reader {
        fn default() -> Self {
            Self(crate::discover_cache::read::Context::current())
        }
    }
    impl Reader {
        /// Demand only this session's head, after the caller has selected it for work.
        pub fn head(&self, r: &SessionRef) -> Option<MarkHead> {
            #[cfg(feature = "probe-resume")]
            probe::note(|reads| reads.heads.push(r.clone()));
            self.0.with_connection(|conn| head_in(conn, r)).flatten()
        }

        pub fn state(&self, r: &SessionRef, head: &MarkHead) -> Option<Checkpoint> {
            #[cfg(feature = "probe-resume")]
            probe::note(|reads| reads.states.push(r.clone()));
            self.0.with_connection(|conn| state_in(conn, r, head)).flatten()
        }
    }

    fn head_in(conn: &Connection, r: &SessionRef) -> Option<MarkHead> {
        let (h, i, p) = key(r);
        conn.query_row(
            "SELECT offset, hash, version, identity FROM resume_marks WHERE harness=?1 AND id=?2 AND path=?3",
            params![h, i, p],
            |row| Ok(row_head(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .ok()
        .flatten()
    }

    fn state_in(conn: &Connection, r: &SessionRef, head: &MarkHead) -> Option<Checkpoint> {
        let (h, i, p) = key(r);
        let (offset, hash, version, text, identity): (i64, i64, String, String, Option<String>) = conn
            .query_row(
                "SELECT offset, hash, version, state, identity FROM resume_marks WHERE harness=?1 AND id=?2 AND path=?3",
                params![h, i, p],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .ok()?;
        let cp: Checkpoint = serde_json::from_str(&text).ok()?;
        (row_head(offset, hash, version, identity).as_ref() == Some(head) && cp.offset == head.offset()).then_some(cp)
    }

    /// A row's head; a row without a device/inode identity has none and is read whole.
    fn row_head(offset: i64, hash: i64, version: String, identity: Option<String>) -> Option<MarkHead> {
        let (dev, ino) = serde_json::from_str(&identity?).ok()?;
        Some(MarkHead {
            prefix: Prefix {
                len: offset as u64,
                hash: hash as u64,
                dev,
                ino,
            },
            version,
        })
    }

    /// Bring each session's mark up to date in one transaction: the new mark replaces the old,
    /// `None` drops it (the next pass over that file reads it whole).
    pub fn update(marks: &[MarkUpdate]) {
        if crate::env::frozen() || marks.is_empty() {
            return;
        }
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        {
            let Ok(mut put) = tx.prepare(
                "INSERT OR REPLACE INTO resume_marks(harness,id,path,offset,hash,version,state,identity)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            ) else {
                return;
            };
            let Ok(mut drop) = tx.prepare("DELETE FROM resume_marks WHERE harness=?1 AND id=?2 AND path=?3") else {
                return;
            };
            for MarkUpdate { session, mark } in marks {
                let (h, i, p) = key(session);
                let _ = match mark {
                    Some((head, cp)) => {
                        let Ok(text) = serde_json::to_string(cp) else { continue };
                        put.execute(params![
                            h,
                            i,
                            p,
                            head.prefix.len as i64,
                            head.prefix.hash as i64,
                            head.version,
                            text,
                            serde_json::to_string(&(head.prefix.dev, head.prefix.ino)).ok()
                        ])
                    }
                    None => drop.execute(params![h, i, p]),
                };
            }
        }
        let _ = tx.commit();
    }
}

#[cfg(feature = "sqlite")]
pub use imp::{state, update, MarkTable, Reader};

/// Without the catalog there are no marks: every pass reads the whole file.
#[cfg(not(feature = "sqlite"))]
pub struct MarkTable;
#[cfg(not(feature = "sqlite"))]
impl MarkTable {
    pub fn load() -> MarkTable {
        MarkTable
    }
    pub fn get(&self, _r: &SessionRef) -> Option<&MarkHead> {
        None
    }
}
#[cfg(not(feature = "sqlite"))]
pub fn state(_r: &SessionRef, _head: &MarkHead) -> Option<Checkpoint> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub fn update(_marks: &[MarkUpdate]) {}

/// Opt-in integration-test observations, scoped to an isolated catalog home and its workers.
#[cfg(feature = "probe-resume")]
pub mod probe {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Default, Debug)]
    pub struct Reads {
        pub scans: usize,
        pub heads: Vec<SessionRef>,
        pub states: Vec<SessionRef>,
    }
    type Counters = Arc<Mutex<Reads>>;
    fn active() -> &'static Mutex<HashMap<PathBuf, Counters>> {
        static ACTIVE: OnceLock<Mutex<HashMap<PathBuf, Counters>>> = OnceLock::new();
        ACTIVE.get_or_init(Default::default)
    }
    pub struct Probe(PathBuf, Counters);
    impl Probe {
        pub fn new() -> Self {
            let home = crate::env::Env::current().cv_home().expect("isolated catalog home");
            let reads = Arc::new(Mutex::new(Reads::default()));
            assert!(active().lock().unwrap().insert(home.clone(), reads.clone()).is_none());
            Self(home, reads)
        }
        pub fn take(&self) -> Reads {
            std::mem::take(&mut *self.1.lock().unwrap())
        }
    }
    impl Default for Probe {
        fn default() -> Self {
            Self::new()
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            active().lock().unwrap().remove(&self.0);
        }
    }
    pub(super) fn note(f: impl FnOnce(&mut Reads)) {
        let Some(home) = crate::env::Env::current().cv_home() else {
            return;
        };
        let reads = active().lock().unwrap().get(&home).cloned();
        if let Some(reads) = reads {
            f(&mut reads.lock().unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "sqlite")]
    fn demanded_head_uses_the_exact_key_and_rejects_invalid_identity() {
        let world = cv_testkit::World::in_process("resume-head", crate::env::Env::enter_isolated);
        let path = world.base.join("session.jsonl");
        std::fs::write(&path, b"line\n").unwrap();
        let r = SessionRef::new("selected", crate::Harness::Claude, path.clone());
        let cp = Checkpoint::new(5);
        let head = MarkHead::of(&path, &cp, "v1").unwrap();
        update(&[MarkUpdate {
            session: r.clone(),
            mark: Some((head.clone(), cp.clone())),
        }]);
        let reader = Reader::default();
        assert_eq!(reader.head(&r), Some(head.clone()));
        assert_eq!(MarkTable::load().get(&r), Some(&head));
        assert!(reader.state(&r, &head).is_some());
        for other in [
            SessionRef {
                id: "other".into(),
                ..r.clone()
            },
            SessionRef {
                harness: crate::Harness::Codex,
                ..r.clone()
            },
            SessionRef {
                path: world.base.join("other.jsonl"),
                ..r.clone()
            },
        ] {
            assert!(reader.head(&other).is_none(), "{other:?}");
        }
        let conn = crate::catalog::open_db().unwrap();
        // A head query neither decodes checkpoint state nor rejects a usable head because the
        // state is broken; state admission remains a separate, guarded read.
        conn.execute("UPDATE resume_marks SET state='not a checkpoint'", [])
            .unwrap();
        assert_eq!(reader.head(&r), Some(head.clone()));
        assert!(reader.state(&r, &head).is_none());
        for identity in [None, Some("broken"), Some("[1]")] {
            conn.execute("UPDATE resume_marks SET identity=?1", [identity]).unwrap();
            assert!(reader.head(&r).is_none(), "{identity:?}");
        }
        update(&[MarkUpdate {
            session: r.clone(),
            mark: None,
        }]);
        assert!(reader.head(&r).is_none());
    }

    /// Sampling an open transcript must not move its reader or any cloned handle.
    #[test]
    #[cfg(unix)]
    fn sampling_preserves_the_open_readers_position_and_hash() {
        use std::io::{Read, Seek, SeekFrom};
        let world = cv_testkit::World::new("mark-position");
        let path = world.base.join("transcript");
        let body: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut reader = file.try_clone().unwrap();
        for offset in [0u64, 100, 4096, 10_000, body.len() as u64] {
            file.seek(SeekFrom::Start(123)).unwrap();
            let mark = MarkHead::of_file(&file, &Checkpoint::new(offset), "v1").unwrap();
            let end = offset as usize;
            let expected = body[..end.min(crate::source::SAMPLE_SPAN as usize)]
                .iter()
                .chain(&body[end.saturating_sub(crate::source::SAMPLE_SPAN as usize)..end])
                .chain(&offset.to_le_bytes())
                .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
                });
            assert_eq!(mark.prefix.hash, expected);
            assert_eq!(file.stream_position().unwrap(), 123);
            let mut next = [0; 8];
            reader.read_exact(&mut next).unwrap();
            assert_eq!(next, body[123..131]);
        }
        file.seek(SeekFrom::Start(123)).unwrap();
        assert!(MarkHead::of_file(&file, &Checkpoint::new(body.len() as u64 + 1), "v1").is_none());
        assert_eq!(file.stream_position().unwrap(), 123);
    }

    /// A mark permits growth; sampled-byte edits, truncation and another version break it.
    #[test]
    #[cfg(unix)]
    fn a_mark_checks_samples_length_and_version_across_an_append() {
        let dir = std::env::temp_dir().join(format!("cv-mark-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let line = |i: usize| format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "x".repeat(i % 97));
        let body: String = (0..400).map(line).collect();
        std::fs::write(&path, &body).unwrap();
        let offset = body[..body.len() / 2].rfind('\n').unwrap() as u64 + 1;
        let head = MarkHead::of(&path, &Checkpoint::new(offset), "v1").unwrap();
        let cp = Checkpoint::new(offset);

        assert!(head.admit(&path, "v1", &cp).is_some());
        assert!(head.admit(&path, "v2", &cp).is_none(), "another version");

        assert!(
            head.admit(&path, "v1", &Checkpoint::new(offset - 1)).is_none(),
            "another checkpoint"
        );
        let grown = format!("{body}{}", line(401));
        std::fs::write(&path, &grown).unwrap();
        assert!(head.admit(&path, "v1", &cp).is_some(), "an append keeps it");
        let replacement = dir.join("replacement.jsonl");
        std::fs::write(&replacement, &grown).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            Prefix::of(&std::fs::File::open(&path).unwrap(), offset).unwrap().hash,
            head.prefix.hash
        );
        assert!(
            head.admit(&path, "v1", &cp).is_none(),
            "replacement identity differs even with identical samples"
        );

        let head = MarkHead::of(&path, &Checkpoint::new(offset), "v1").unwrap();
        assert!(head.admit(&path, "v1", &cp).is_some());

        // A mark taken on a handle opened before a rename-over describes the file read, so it
        // does not hold for what the path names now; reopening the path would have paired them.
        let read = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, &grown).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let of_read = MarkHead::of_file(&read, &Checkpoint::new(offset), "v1").unwrap();
        assert_eq!(of_read.prefix.hash, head.prefix.hash);
        assert!(of_read.admit(&path, "v1", &cp).is_none());
        let head = MarkHead::of(&path, &Checkpoint::new(offset), "v1").unwrap();

        // One byte changed just before the mark, then one in the head: both break it.
        for at in [offset as usize - 10, 7] {
            let mut b = grown.clone().into_bytes();
            b[at] = b'y';
            std::fs::write(&path, &b).unwrap();
            assert!(head.admit(&path, "v1", &cp).is_none(), "byte {at} changed");
        }
        std::fs::write(&path, &grown.as_bytes()[..offset as usize - 1]).unwrap();
        assert!(head.admit(&path, "v1", &cp).is_none(), "truncated before the mark");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(not(feature = "sqlite"))]
#[derive(Default)]
pub struct Reader;
#[cfg(not(feature = "sqlite"))]
impl Reader {
    pub fn head(&self, _: &SessionRef) -> Option<MarkHead> {
        None
    }
    pub fn state(&self, _: &SessionRef, _: &MarkHead) -> Option<Checkpoint> {
        None
    }
}
