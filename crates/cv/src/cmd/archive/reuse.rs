//! Consumer source stamps and bounded temporary serialization for the durable archive.
use super::*;
mod spool;
use crate::cmd::session_json::write_session;
use cv_core::projection::{Counted, Prefix};
use spool::{FirstUser, Spool, Spooled};
use std::io::{BufWriter, Read};

/// A published archive file: its identity, plus off Unix (where `FileId` has no inode or
/// ctime) a content digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Artifact {
    id: cv_core::source::FileId,
    hash: Option<String>,
}
impl Artifact {
    fn of(path: &Path) -> Option<Self> {
        let id = cv_core::source::FileId::of(&fs::metadata(path).ok()?)?;
        let hash = match cfg!(unix) {
            true => None,
            false => {
                let (mut input, mut digest) = (fs::File::open(path).ok()?, cv_core::digest::Sha256::new());
                let mut bytes = [0; 64 * 1024];
                loop {
                    match input.read(&mut bytes) {
                        Ok(0) => break Some(digest.finish_hex()),
                        Ok(n) => digest.update(&bytes[..n]),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => return None,
                    }
                }
            }
        };
        Some(Self { id, hash })
    }
    fn matches(&self, path: &Path) -> bool {
        Self::of(path).as_ref() == Some(self)
    }
}

/// What the archive keeps about one archived source, as the payload of its
/// [`cv_core::source::Record`] (written to `archive-source/<harness>/<id>.json`): a record of
/// another cv version does not decode, and its session is archived again.
/// On disk the read state is held without its baseline (`Stored<Held>`): the record's baseline
/// is the one copy, rejoined on load.
#[derive(Serialize, Deserialize)]
struct Stored<R = cv_core::watch::ReadState> {
    total: usize,
    artifact: Artifact,
    read: R,
    prefix: Option<Prefix>,
    /// The turns among the `prefix` messages, for the next pass that copies them.
    prefix_turns: usize,
    first_user: Option<FirstUser>,
}

impl cv_core::source::Payload for Stored<cv_core::watch::Held> {
    const TAG: &'static str = "archive";
}

impl<R> Stored<R> {
    fn map_read<S>(self, f: impl FnOnce(R) -> S) -> Stored<S> {
        let Stored {
            total,
            artifact,
            read,
            prefix,
            prefix_turns,
            first_user,
        } = self;
        Stored {
            total,
            artifact,
            read: f(read),
            prefix,
            prefix_turns,
            first_user,
        }
    }
}

pub(super) struct StoredPass {
    pub outcome: StoreOutcome,
    pub total: usize,
    pub read: cv_core::watch::ReadState,
    #[cfg(test)]
    pub parsed_messages: usize,
    #[cfg(test)]
    pub resumed: bool,
    #[cfg(test)]
    pub suffix_bytes: u64,
    #[cfg(test)]
    pub spool_bytes: u64,
    #[cfg(test)]
    pub prefix_bytes: u64,
}

struct Temporary(cv_core::publish::Staged);
impl Temporary {
    fn new(dest: &Path) -> Result<Self> {
        let parent = dest.parent().context("archive path has no parent")?;
        fs::create_dir_all(parent)?;
        Ok(Self(cv_core::publish::Staged::create(parent, "archive", 0o600)?))
    }
    #[cfg(test)]
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let file = self.0.file();
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    }
    fn publish(self, dest: &Path) -> Result<()> {
        self.0.publish(dest, cv_core::publish::Claim::Replace)?;
        Ok(())
    }
}

/// Compare the digest captured during serialization with only the previous artifact.
fn equal_artifact(path: &Path, len: u64, digest: &str) -> bool {
    let compare = || -> std::io::Result<bool> {
        let mut input = fs::File::open(path)?;
        if input.metadata()?.len() != len {
            return Ok(false);
        }
        let mut hash = cv_core::digest::Sha256::new();
        let mut bytes = [0; 64 * 1024];
        loop {
            let n = match input.read(&mut bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                return Ok(hash.finish_hex() == digest);
            }
            hash.update(&bytes[..n]);
        }
    };
    compare().unwrap_or(false)
}

/// The catalog row of an archived session: `message_count` becomes its turns, `messages_written`
/// the raw count of messages the pass wrote.
fn entry(session: &Session, title: Option<String>, turns: usize, written: usize) -> CatalogEntry {
    CatalogEntry {
        session: SessionRef {
            cwd: session.cwd.clone(),
            title,
            created_at: session.created_at,
            updated_at: session.updated_at,
            message_count: Some(turns),
            ..SessionRef::new(
                &session.id,
                session.harness,
                Archive::rel_session_path(session.harness, &session.id),
            )
        },
        messages_written: written,
        archived_at: Utc::now(),
    }
}

impl Archive {
    /// The identities of what an archive pass left for `r` (its session file and resume state),
    /// when both exist and the platform's file identity is exact enough to stand for their
    /// content (Unix: inode and ctime). Elsewhere `None`: every pass validates in full.
    pub(super) fn outputs(&self, r: &SessionRef) -> Option<[cv_core::source::FileId; 2]> {
        if !cfg!(unix) {
            return None;
        }
        let id = |path: PathBuf| cv_core::source::FileId::of(&fs::metadata(path).ok()?);
        Some([id(self.session_path(r.harness, &r.id))?, id(self.source_path(r))?])
    }

    fn source_path(&self, r: &SessionRef) -> PathBuf {
        self.home
            .join("archive-source")
            .join(r.harness.as_str())
            .join(format!("{}.json", archive_stem(&r.id)))
    }

    /// The resume state a previous archive pass left for `r`, when it still applies. A miss
    /// re-reads the source: no state yet (`NotFound`), one another cv version wrote (it does not decode, or names another version) or for another artifact. A state that
    /// is there but cannot be read degrades once per process, like every accelerator.
    fn stored(&self, r: &SessionRef) -> Option<Stored> {
        let path = self.source_path(r);
        let file = match fs::File::open(&path).and_then(|file| Ok((file.metadata()?, file))) {
            Ok(opened) => opened,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                cv_core::env::warn_once("the archive's resume state", format!("{}: {error}", path.display()));
                return None;
            }
        };
        // No size cap: the state is this session's own certificate, so declining a large one
        // would re-read and re-archive the session on every pass.
        let (metadata, mut file) = file;
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        if let Err(error) = file.read_to_end(&mut bytes) {
            cv_core::env::warn_once("the archive's resume state", format!("{}: {error}", path.display()));
            return None;
        }
        let record = cv_core::source::Record::<Stored<cv_core::watch::Held>>::decode(&bytes)?;
        let baseline = record.baseline;
        let stored = record
            .payload
            .map_read(|held| cv_core::watch::ReadState::join(baseline, held));
        stored
            .artifact
            .matches(&self.session_path(r.harness, &r.id))
            .then_some(stored)
    }

    /// Skip unchanged sources before parsing. A verified append replaces the archived suffix
    /// after the checkpoint, preserving held EOF messages and exact final session metadata.
    pub(super) fn store_ref(&self, adapter: &dyn cv_core::Adapter, r: &SessionRef) -> Result<StoredPass> {
        let previous = self.stored(r);
        if let Some(stored) = previous.as_ref().filter(|stored| stored.read.fresh_with(adapter, r)) {
            return Ok(StoredPass {
                outcome: StoreOutcome::Skipped,
                total: stored.total,
                read: stored.read.clone(),
                #[cfg(test)]
                parsed_messages: 0,
                #[cfg(test)]
                resumed: false,
                #[cfg(test)]
                suffix_bytes: 0,
                #[cfg(test)]
                spool_bytes: 0,
                #[cfg(test)]
                prefix_bytes: 0,
            });
        }
        let mut spool = Spool::new(&r.path);
        let mut pass = cv_core::watch::read_session_into(
            adapter,
            r,
            previous.as_ref().map(|stored| &stored.read),
            previous.as_ref().map_or(0, |stored| stored.total),
            &mut spool,
        )?;
        let dest = self.session_path(r.harness, &r.id);
        let old = previous.as_ref().filter(|stored| {
            pass.resumed
                && stored.artifact.matches(&dest)
                && stored
                    .prefix
                    .as_ref()
                    .is_some_and(|prefix| prefix.valid(stored.artifact.id.len, pass.prefix))
        });
        if pass.resumed && old.is_none() {
            // Older state has no canonical prefix range, or its artifact changed. The existing
            // checked adapter handles the whole fallback; discard only this owned partial spool.
            spool = Spool::new(&r.path);
            pass = cv_core::watch::read_session_into(adapter, r, None, 0, &mut spool)?;
        }
        let Spooled {
            bytes: spooled,
            messages,
            turns,
            mut first_user,
        } = spool.finish()?;
        #[cfg(test)]
        let spool_bytes = spooled.byte_len()?;
        #[cfg(test)]
        let prefix_bytes = old
            .and_then(|stored| stored.prefix.as_ref())
            .map_or(0, |prefix| prefix.end - prefix.start);
        anyhow::ensure!(
            messages.checked_add(pass.prefix) == Some(pass.total),
            "archive spool length does not match the completed read"
        );
        anyhow::ensure!(
            pass.prefix == 0 || old.is_some(),
            "archive resumed without the prefix it copies"
        );
        let old_turns = old.map_or(0, |stored| stored.prefix_turns);
        // The spool sees only the suffix: translate its one-based prompt ordinal to the
        // full artifact. A saved prompt inside the stable prefix precedes any suffix prompt.
        if let Some(first) = first_user.as_mut() {
            first.ordinal += pass.prefix;
        }
        if let Some(first) = old.and_then(|stored| stored.first_user.as_ref()) {
            anyhow::ensure!(
                first.ordinal <= pass.prefix,
                "archive label lies outside its stable prefix"
            );
            first_user = Some(first.clone());
        }
        let read = pass.accepted(pass.total);
        let total = pass.total;
        #[cfg(test)]
        let (parsed_messages, resumed) = (pass.parsed_messages, pass.resumed);
        #[cfg(test)]
        let suffix_bytes = pass.suffix_bytes;
        let session = pass.session;
        let mut temporary = Temporary::new(&dest)?;
        let (prefix, len, digest) = {
            let mut writer = Counted::new(BufWriter::with_capacity(64 * 1024, temporary.0.file())).hashing();
            let prefix = write_session(
                &mut writer,
                &session,
                &spooled,
                old.and_then(|stored| stored.prefix.as_ref())
                    .map(|prefix| (dest.as_path(), prefix)),
                read.stable_prefix(),
                total,
            )?;
            writer.flush()?;
            (
                prefix,
                writer.position.get(),
                writer.into_digest().expect("hashing writer"),
            )
        };
        let (outcome, artifact) = {
            let _lock = self.lock_catalog()?;
            if let Some(old) = old {
                anyhow::ensure!(
                    old.artifact.matches(&dest),
                    "archive artifact changed during prefix copy"
                );
            }
            let title = session.title.clone().or_else(|| {
                Some(cv_core::label_from(
                    None,
                    first_user.as_ref().and_then(|first| first.head.as_deref()),
                ))
            });
            let entry = entry(&session, title, old_turns + turns.before(messages), total);
            let outcome = if equal_artifact(&dest, len, &digest) {
                if previous.is_none() {
                    self.upsert_catalog_locked(entry)?;
                }
                StoreOutcome::Skipped
            } else {
                temporary.publish(&dest)?;
                self.upsert_catalog_locked(entry)?;
                StoreOutcome::Archived
            };
            (
                outcome,
                Artifact::of(&dest).context("cannot inspect archive artifact after publication")?,
            )
        };
        // The new prefix's turns: the copied prefix's, then the spooled ones up to its end.
        let prefix = prefix.filter(|prefix| prefix.messages >= pass.prefix);
        let prefix_turns = prefix
            .as_ref()
            .map_or(0, |prefix| old_turns + turns.before(prefix.messages - pass.prefix));
        // Freshness is checked once, under the catalog lock, right before the state publishes.
        {
            let (baseline, held) = read.clone().split();
            let stored = Stored {
                total,
                artifact,
                read: held,
                first_user: first_user
                    .filter(|first| prefix.as_ref().is_some_and(|prefix| first.ordinal <= prefix.messages)),
                prefix,
                prefix_turns,
            };
            let dest = self.source_path(r);
            let mut temporary = Temporary::new(&dest)?;
            let record = cv_core::source::Record::new(baseline, stored);
            temporary.write_all(&record.encode())?;
            let _lock = self.lock_catalog()?;
            if record.payload.artifact.matches(&self.session_path(r.harness, &r.id)) && read.holds_with(adapter, r) {
                temporary.publish(&dest)?;
            }
        }
        Ok(StoredPass {
            outcome,
            total,
            read,
            #[cfg(test)]
            parsed_messages,
            #[cfg(test)]
            resumed,
            #[cfg(test)]
            suffix_bytes,
            #[cfg(test)]
            spool_bytes,
            #[cfg(test)]
            prefix_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(harness: Harness) -> (PathBuf, Archive, SessionRef, String) {
        let root = std::env::temp_dir().join(format!("cv-archive-reuse-{}", uuid::Uuid::new_v4()));
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();
        let r = SessionRef::new("read", harness, input.join("read.jsonl"));
        let a = Archive::resolve(Some(root.join("archive"))).unwrap();
        let mut body = String::new();
        for i in 0..8192 {
            let line = match harness {
                Harness::Claude => {
                    serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read", "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}})
                }
                Harness::Codex => {
                    serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":format!("{i} {}", "x".repeat(256))}})
                }
                _ => unreachable!(),
            };
            body.push_str(&line.to_string());
            body.push('\n');
        }
        fs::write(&r.path, &body).unwrap();
        (root, a, r, body)
    }
    fn exact(a: &Archive, adapter: &dyn cv_core::Adapter, r: &SessionRef) {
        let expected = serde_json::to_vec_pretty(&adapter.parse(r).unwrap()).unwrap();
        assert_eq!(fs::read(a.session_path(r.harness, &r.id)).unwrap(), expected);
    }

    /// One explicitly supplied read-only source and owned scratch archive per process. Run this
    /// ignored test alone with probe-heap; fixture generation is outside the measured operation.
    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore]
    fn archive_retention_heap_probe() {
        let input = PathBuf::from(std::env::var_os("CV_ARCHIVE_PROBE_SOURCE").expect("explicit source"));
        let home = PathBuf::from(std::env::var_os("CV_ARCHIVE_PROBE_HOME").expect("owned scratch archive"));
        let mode = std::env::var("CV_ARCHIVE_PROBE_MODE").unwrap_or_else(|_| "archive".into());
        let r = SessionRef::new("perf", Harness::Claude, &input);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let a = Archive::resolve(Some(home)).unwrap();
        let baseline = cv_testkit::heap::begin();
        let started = std::time::Instant::now();
        let (total, parsed, resumed, suffix_bytes, spool_bytes, prefix_bytes) = match mode.as_str() {
            "archive" => {
                let pass = a.store_ref(adapter.as_ref(), &r).unwrap();
                (
                    pass.total,
                    pass.parsed_messages,
                    pass.resumed,
                    pass.suffix_bytes,
                    pass.spool_bytes,
                    pass.prefix_bytes,
                )
            }
            "watch" => {
                let pass = cv_core::watch::read_tail(adapter.as_ref(), &r, None, 0, 0).unwrap();
                (pass.total, pass.parsed_messages, pass.resumed, pass.suffix_bytes, 0, 0)
            }
            _ => panic!("unknown probe mode"),
        };
        let heap = cv_testkit::heap::end(baseline);
        let report = serde_json::json!({"operation":mode,"total":total,"callbacks":parsed,
            "resumed":resumed,"logical_source_suffix_bytes":suffix_bytes,
            "logical_spool_bytes":spool_bytes,"logical_archive_prefix_bytes":prefix_bytes,
            "artifact_bytes":fs::metadata(a.session_path(r.harness,&r.id)).ok().map(|m|m.len()),
            "elapsed_seconds":started.elapsed().as_secs_f64(),"heap":heap});
        eprintln!("archive_probe={report}");
    }

    #[test]
    fn a_resume_state_past_one_mib_is_kept_and_an_unchanged_source_is_skipped() {
        let (root, a, r, mut body) = setup(Harness::Claude);
        // The session's cwd rides in every checkpoint, so the state encodes above 1 MiB.
        let cwd = format!("/{}", "w".repeat(1024 * 1024));
        let wide = serde_json::json!({"type":"user","uuid":"wide","sessionId":"read","cwd":cwd,
            "message":{"role":"user","content":"wide"}});
        body.insert_str(0, &format!("{wide}\n"));
        fs::write(&r.path, &body).unwrap();
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let first = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(matches!(first.outcome, StoreOutcome::Archived));
        assert!(fs::metadata(a.source_path(&r)).unwrap().len() > 1024 * 1024);
        let again = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(matches!(again.outcome, StoreOutcome::Skipped));
        assert_eq!(again.parsed_messages, 0, "an unchanged source is not re-read");
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_wire_archive_reuses_suffix_and_keeps_pretty_output_exact() {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("cv-archive-directory-{}", uuid::Uuid::new_v4()));
        let input = root.join("input");
        let wire = input.join("agents/main/wire.jsonl");
        fs::create_dir_all(wire.parent().unwrap()).unwrap();
        let controls = input.join("state.json");
        fs::write(&controls, "{\"cwd\":\"/before\"}").unwrap();
        let line = format!(
            "{}\n",
            serde_json::json!({"type":"context.append_message",
            "message":{"role":"user","content":"x".repeat(128)}})
        );
        fs::write(&wire, line.repeat(4096)).unwrap();
        let a = Archive::resolve(Some(root.join("archive"))).unwrap();
        let r = SessionRef::new("directory", Harness::KimiCode, &input);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        assert_eq!(a.store_ref(adapter.as_ref(), &r).unwrap().total, 4096);
        exact(&a, adapter.as_ref(), &r);
        let unchanged = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(unchanged.outcome, StoreOutcome::Skipped);
        assert_eq!(unchanged.parsed_messages, 0);
        fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap()
            .write_all(line.repeat(4).as_bytes())
            .unwrap();
        let appended = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(appended.resumed);
        assert!(appended.parsed_messages < appended.total);
        assert_eq!(appended.total, 4100);
        exact(&a, adapter.as_ref(), &r);
        let turns = adapter
            .parse(&r)
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.is_turn())
            .count();
        let row = &a.load_catalog().unwrap()[0];
        assert_eq!(
            (row.session.message_count, row.messages_written),
            (Some(turns), 4100),
            "a resumed archive counts the copied prefix's turns"
        );
        fs::write(&controls, "{\"cwd\":\"/changed\"}").unwrap();
        let changed = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(!changed.resumed);
        assert_eq!(changed.parsed_messages, 4100);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_file(a.session_path(r.harness, &r.id)).unwrap();
        let repaired = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(!repaired.resumed);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owned_artifact_temps_clean_up() {
        let (root, a, r, _) = setup(Harness::Claude);
        let dest = a.session_path(r.harness, &r.id);
        let temporary = Temporary::new(&dest).unwrap();
        let own = temporary.path().to_owned();
        assert!(own.exists());
        drop(temporary);
        assert!(!own.exists());
        assert!(fs::read_dir(dest.parent().unwrap()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_sync_appends_rewrites_dependencies_and_artifact_repairs_are_exact() {
        let (root, a, r, mut body) = setup(Harness::Claude);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let first = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!((first.outcome, first.parsed_messages), (StoreOutcome::Archived, 8192));
        for _ in 0..4 {
            let cold = Archive::resolve(Some(a.home.clone())).unwrap();
            let idle = cold.store_ref(adapter.as_ref(), &r).unwrap();
            assert_eq!(
                (idle.outcome, idle.parsed_messages, idle.suffix_bytes),
                (StoreOutcome::Skipped, 0, 0)
            );
        }
        for i in 8192..8196 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read","message":{"role":"user","content":format!("append {i}")}}).to_string());
            body.push('\n');
        }
        fs::write(&r.path, &body).unwrap();
        let appended = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(appended.resumed);
        assert!(appended.parsed_messages < appended.total);
        assert!(appended.suffix_bytes < 300 * 1024);
        exact(&a, adapter.as_ref(), &r);
        eprintln!("archive repeated polls: 0 parses; append: {} messages/{} primary suffix bytes instead of {} messages/{} bytes", appended.parsed_messages, appended.suffix_bytes, appended.total, body.len());
        eprintln!("archive append file work: {} stable prefix JSON bytes copied, {} suffix message spool bytes, {} canonical JSON bytes written, {} source-state bytes per cold unchanged poll", appended.prefix_bytes, appended.spool_bytes, fs::metadata(a.session_path(r.harness, &r.id)).unwrap().len(), fs::metadata(a.source_path(&r)).unwrap().len());

        // A modified/deleted artifact cannot be blessed by an unchanged source stamp.
        let dest = a.session_path(r.harness, &r.id);
        fs::write(&dest, b"{\"user modification\":true}").unwrap();
        assert_eq!(a.store_ref(adapter.as_ref(), &r).unwrap().parsed_messages, 8196);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_file(&dest).unwrap();
        assert_eq!(a.store_ref(adapter.as_ref(), &r).unwrap().parsed_messages, 8196);
        exact(&a, adapter.as_ref(), &r);

        // A write-once spill added to a growing directory keeps the prefix reusable; rewriting an
        // existing spill or replacing the primary file at the same count forces a whole read.
        let outputs = r.path.with_extension("").join("tool-results");
        fs::create_dir_all(&outputs).unwrap();
        fs::write(outputs.join("new.txt"), "sidecar spilled").unwrap();
        let spilled = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(spilled.resumed);
        assert!(spilled.parsed_messages < spilled.total);
        exact(&a, adapter.as_ref(), &r);
        fs::write(outputs.join("new.txt"), "sidecar rewritten").unwrap();
        assert!(!a.store_ref(adapter.as_ref(), &r).unwrap().resumed);
        exact(&a, adapter.as_ref(), &r);
        let replacement = r.path.with_extension("new");
        let old_modified = fs::metadata(&r.path).unwrap().modified().unwrap();
        fs::write(&replacement, &body).unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old_modified))
            .unwrap();
        fs::rename(replacement, &r.path).unwrap();
        assert!(!a.store_ref(adapter.as_ref(), &r).unwrap().resumed);
        exact(&a, adapter.as_ref(), &r);

        let shorter: String = body.lines().take(2).map(|line| format!("{line}\n")).collect();
        fs::write(&r.path, shorter).unwrap();
        let truncated = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(
            (truncated.total, truncated.parsed_messages, truncated.resumed),
            (2, 2, false)
        );
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_archive_replaces_held_eof_suffix_after_usage_arrives() {
        let (root, a, r, mut body) = setup(Harness::Codex);
        body.push_str(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"held at EOF\"}}\n",
        );
        fs::write(&r.path, &body).unwrap();
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let first = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(first.total, 8193);
        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":7,\"output_tokens\":3}}}}\n");
        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"new prompt\"}}\n");
        fs::write(&r.path, &body).unwrap();
        let append = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(append.resumed);
        assert!(append.parsed_messages < 1024);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_selected_source_serialization_leaves_artifact_catalog_and_stamp_unchanged() {
        struct Input(Option<PathBuf>);
        impl cv_core::Adapter for Input {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                let mut session = Session::new(&r.id, r.harness);
                session.source_path = Some(r.path.clone());
                sink.meta(&session);
                let mut message = cv_core::Message::new(cv_core::Role::Assistant);
                if let Some(path) = &self.0 {
                    let input = cv_core::Json::source(path, 0, 2)?;
                    fs::remove_file(path)?;
                    message.content.push(cv_core::Block::ToolUse {
                        id: "call".into(),
                        name: "opaque".into(),
                        namespace: None,
                        input,
                    });
                }
                sink.try_message(message)?;
                Ok(session)
            }
        }
        let (root, a, r, _) = setup(Harness::Claude);
        a.store_ref(&Input(None), &r).unwrap();
        let artifact = a.session_path(r.harness, &r.id);
        let before = (
            fs::read(&artifact).unwrap(),
            fs::read(a.catalog_path()).unwrap(),
            fs::read(a.source_path(&r)).unwrap(),
        );
        let external = root.join("selected.json");
        fs::write(&external, "{}").unwrap();
        fs::write(&r.path, "changed primary").unwrap();
        assert!(a.store_ref(&Input(Some(external)), &r).is_err());
        assert_eq!(
            before,
            (
                fs::read(&artifact).unwrap(),
                fs::read(a.catalog_path()).unwrap(),
                fs::read(a.source_path(&r)).unwrap()
            )
        );
        assert!(fs::read_dir(artifact.parent().unwrap()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_catalog_publication_keeps_the_old_stamp_and_retry_repairs_it() {
        let (root, a, r, mut body) = setup(Harness::Claude);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        a.store_ref(adapter.as_ref(), &r).unwrap();
        let state = a.source_path(&r);
        let old_state = fs::read(&state).unwrap();
        let catalog = a.catalog_path();
        let backup = root.join("owned-catalog-backup");
        fs::rename(&catalog, &backup).unwrap();
        fs::create_dir(&catalog).unwrap();
        body.push_str("{\"type\":\"user\",\"uuid\":\"new\",\"sessionId\":\"read\",\"message\":{\"role\":\"user\",\"content\":\"new prompt\"}}\n");
        fs::write(&r.path, body).unwrap();
        assert!(a.store_ref(adapter.as_ref(), &r).is_err());
        assert_eq!(fs::read(&state).unwrap(), old_state);
        assert!(a.stored(&r).is_none(), "old stamp cannot bless new artifact");
        fs::remove_dir(&catalog).unwrap();
        fs::rename(backup, &catalog).unwrap();
        let repaired = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(repaired.total, 8193);
        assert!(a.stored(&r).unwrap().read.fresh(&r));
        let row = &a.load_catalog().unwrap()[0];
        assert_eq!((row.messages_written, row.session.message_count), (8193, Some(8193)));
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mid_pass_append_publishes_prefix_state_and_next_archive_resumes() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Growing(Box<dyn cv_core::Adapter>, AtomicBool);
        impl cv_core::Adapter for Growing {
            fn harness(&self) -> Harness {
                self.0.harness()
            }
            fn roots(&self) -> Vec<PathBuf> {
                self.0.roots()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                self.0.discover()
            }
            fn capabilities(&self) -> cv_core::harness::Capabilities {
                self.0.capabilities()
            }
            fn dependencies(
                &self,
                r: &SessionRef,
                probe: &mut cv_core::source::Probe,
                deps: &mut cv_core::source::Dependencies,
            ) -> Option<()> {
                self.0.dependencies(r, probe, deps)
            }
            fn transcript(
                &self,
                r: &SessionRef,
                opts: &cv_core::Demand,
                from: cv_core::stream::Start<'_>,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Option<Session>> {
                self.0.transcript(r, opts, from, sink)
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                let session = self.0.stream(r, opts, sink)?;
                if self.1.swap(false, Ordering::SeqCst) {
                    let mut file = fs::OpenOptions::new().append(true).open(&r.path).unwrap();
                    for i in 8192..8196 {
                        writeln!(
                            file,
                            "{}",
                            serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read","message":{"role":"user","content":format!("live {i}")}})
                        )
                        .unwrap();
                    }
                    let outputs = r.path.with_extension("").join("tool-results");
                    fs::create_dir_all(&outputs).unwrap();
                    fs::write(outputs.join("spill.txt"), "live spill").unwrap();
                }
                Ok(session)
            }
        }
        let (root, a, r, _) = setup(Harness::Claude);
        let adapter = Growing(cv_core::harness::for_harness(r.harness).unwrap(), AtomicBool::new(true));
        let first = a.store_ref(&adapter, &r).unwrap();
        assert_eq!((first.outcome, first.total), (StoreOutcome::Archived, 8192));
        assert!(
            a.stored(&r).is_some(),
            "mid-pass append still publishes prefix resume state"
        );
        let second = a.store_ref(&adapter, &r).unwrap();
        assert!(second.resumed);
        assert_eq!(second.total, 8196);
        assert!(second.parsed_messages < second.total);
        exact(&a, adapter.0.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }
}
