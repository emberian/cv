//! Incompatible schemas are built aside, then published at Tantivy's existing meta.json
//! boundary. Keeping the directory stable preserves explicit index paths and existing readers.
//! Old segment files are deliberately removed from Tantivy's managed set, not deleted: readers
//! in another process may still need them. A rare format upgrade retains this disk space until
//! the user retires the index; guessing that another process has finished is unsafe.
use super::*;
use tantivy::directory::{Directory, DirectoryLock, MmapDirectory, INDEX_WRITER_LOCK, META_LOCK};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Publication {
    BeforeMetadata,
    AfterMetadata,
}

pub(super) struct Migration {
    dir: PathBuf,
    stage: PathBuf,
    directory: MmapDirectory,
    _writer_lock: DirectoryLock,
    linked: Vec<PathBuf>,
    publication_attempted: bool,
}

impl Migration {
    pub(super) fn prepare(dir: &Path, rebuild: bool) -> Result<Option<Self>> {
        if !dir.join("meta.json").try_exists()? {
            return Ok(None);
        }
        let index = directory::open(dir, false).context("checking full-text index schema")?;
        let needs_replacement = |index: &Index| -> Result<bool> {
            if !schema_current(&index.schema()) {
                return Ok(true);
            }
            Ok(rebuild && read_indexed_metadata(index, &fields_of(&index.schema())?, true).is_err())
        };
        if !needs_replacement(&index)? {
            return Ok(None);
        }
        let directory = MmapDirectory::open(dir)?;
        let writer_lock = directory
            .acquire_lock(&INDEX_WRITER_LOCK)
            .context("locking full-text index for schema migration; retry after the active writer finishes")?;
        // A migration may have completed between the first open and acquiring the lock.
        if !needs_replacement(&directory::open(dir, false)?)? {
            return Ok(None);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let stage = dir.join(format!(".cv-migrate-{id}"));
        std::fs::create_dir(&stage)?;
        let migration = Self {
            dir: dir.to_owned(),
            stage,
            directory,
            _writer_lock: writer_lock,
            linked: Vec::new(),
            publication_attempted: false,
        };
        directory::create(&migration.stage, build_schema_with_consumer(Some(&id)))?;
        Ok(Some(migration))
    }

    pub(super) fn path(&self) -> &Path {
        &self.stage
    }

    pub(super) fn publish(self, swept: &Swept) -> Result<()> {
        self.publish_with(swept, |_| Ok(()))
    }

    fn publish_with(mut self, swept: &Swept, mut checkpoint: impl FnMut(Publication) -> Result<()>) -> Result<()> {
        if swept.errors != 0 || !swept.unreadable.is_empty() {
            let reason = swept
                .first_error
                .clone()
                .or_else(|| {
                    swept
                        .unreadable
                        .first()
                        .map(|failure| format!("{} ({}): {}", failure.id, failure.harness, failure.reason))
                })
                .unwrap_or_else(|| "source or derived-state publication failed".to_owned());
            anyhow::bail!(
                "full-text schema migration incomplete ({} failed sources or publications); previous index retained: {}",
                swept.errors.max(swept.unreadable.len()), cv_core::ir::truncate(&reason, 512)
            );
        }
        let (replacement, _) = open_existing(&self.stage)?;
        // Opening all segments validates the published generation before anything visible moves.
        let reader: tantivy::IndexReader = replacement.reader()?;
        let _snapshot = reader.searcher();
        anyhow::ensure!(
            replacement.validate_checksum()?.is_empty(),
            "replacement index checksum validation failed; previous index retained"
        );
        let metadata = std::fs::read(self.stage.join("meta.json"))?;
        let managed = std::fs::read(self.stage.join(".managed.json"))?;
        for path in replacement.directory().list_managed_files() {
            if path == Path::new("meta.json") {
                continue;
            }
            anyhow::ensure!(path.components().count() == 1, "invalid staged index file path");
            // Hard-link only immutable, UUID-named segment files; collisions fail closed.
            // Committed segment files are already synced by Tantivy before its meta commit.
            std::fs::hard_link(self.stage.join(&path), self.dir.join(&path))
                .with_context(|| format!("publishing index segment {}", path.display()))?;
            self.linked.push(path);
        }
        self.directory.sync_directory()?;
        let _meta_lock = self.directory.acquire_lock(&META_LOCK)?;
        // Untracking old files before the metadata switch is safe even if publication fails:
        // the old metadata and every old file remain readable. They simply cannot be GC'd.
        self.directory.atomic_write(Path::new(".managed.json"), &managed)?;
        self.directory.sync_directory()?;
        checkpoint(Publication::BeforeMetadata)?;
        // An I/O error after rename can leave a successfully published metadata file. From
        // this point onward its segment files must survive even an uncertain return status.
        self.publication_attempted = true;
        self.directory.atomic_write(Path::new("meta.json"), &metadata)?;
        self.directory.sync_directory()?;
        checkpoint(Publication::AfterMetadata)?;
        Ok(())
    }
}

impl Drop for Migration {
    fn drop(&mut self) {
        // Only this invocation's unpublished/staged directory belongs to us. Crash leftovers
        // from other invocations are never swept blindly; they cannot replace the active index.
        if !self.publication_attempted {
            for path in &self.linked {
                let _ = std::fs::remove_file(self.dir.join(path));
            }
        }
        let _ = std::fs::remove_dir_all(&self.stage);
    }
}

/// Migration identity lives in the same atomic metadata record as the schema/segments. Ordinary
/// legacy indexes retain cv-index-id; no consumer can certify a generation before publication.
pub(super) fn consumer(dir: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::read(dir.join("meta.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata: serde_json::Value = serde_json::from_slice(&metadata)?;
    Ok(metadata["schema"].as_array().and_then(|fields| {
        fields.iter().find_map(|field| {
            let id = field["name"].as_str()?.strip_prefix("cv_consumer_")?;
            uuid::Uuid::parse_str(id).ok()?;
            Some(format!("fts:{id}"))
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_index(dir: &Path) -> Index {
        std::fs::create_dir_all(dir).unwrap();
        let mut schema = serde_json::to_value(build_schema()).unwrap();
        schema
            .as_array_mut()
            .unwrap()
            .retain(|field| field["name"] != format_field());
        let schema: Schema = serde_json::from_value(schema).unwrap();
        let index = Index::create_in_dir(dir, schema.clone()).unwrap();
        let mut writer = index.writer::<TantivyDocument>(15_000_000).unwrap();
        writer.add_document(tantivy::doc!(schema.get_field("id").unwrap() => "old", schema.get_field("title").unwrap() => "retained")).unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        index
    }

    fn fill_stage(migration: &Migration) {
        let (index, fields) = open_or_create(migration.path()).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(fields.id => "new", fields.kind => KIND_META, fields.title => "replacement"))
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn migration_failure_and_retry_preserve_old_reader_and_consumer() {
        let world = cv_testkit::World::in_process("migration-publication", cv_core::env::Env::enter_isolated);
        let dir = world.base.join("index");
        let old = old_index(&dir);
        let reader: tantivy::IndexReader = old.reader().unwrap();
        let retained = reader.searcher();
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        let old_consumer = source_consumer(&dir).unwrap();
        let migration = Migration::prepare(&dir, false).unwrap().unwrap();
        let stage = migration.path().to_owned();
        fill_stage(&migration);
        let staged_files = Index::open_in_dir(migration.path())
            .unwrap()
            .directory()
            .list_managed_files();
        assert_ne!(source_consumer(migration.path()).unwrap(), old_consumer);
        assert!(
            Migration::prepare(&dir, false).is_err(),
            "migration excludes concurrent writers"
        );
        let error = migration
            .publish_with(&Swept::default(), |_| anyhow::bail!("cancel before metadata"))
            .unwrap_err();
        assert!(error.to_string().contains("cancel before metadata"));
        assert!(!stage.exists());
        for path in staged_files {
            if path != Path::new("meta.json") {
                assert!(
                    !dir.join(path).exists(),
                    "failed attempt must not accumulate its segment files"
                );
            }
        }
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        assert_eq!(source_consumer(&dir).unwrap(), old_consumer);
        assert_eq!(
            Index::open_in_dir(&dir)
                .unwrap()
                .reader()
                .unwrap()
                .searcher()
                .num_docs(),
            1
        );

        let migration = Migration::prepare(&dir, false).unwrap().unwrap();
        fill_stage(&migration);
        let new_consumer = source_consumer(migration.path()).unwrap();
        migration.publish(&Swept::default()).unwrap();
        assert_eq!(source_consumer(&dir).unwrap(), new_consumer);
        let (_, fields, fresh) = query_index(&dir).unwrap();
        assert_eq!(fresh.num_docs(), 1);
        let new_doc: TantivyDocument = fresh.doc(tantivy::DocAddress::new(0, 0)).unwrap();
        assert_eq!(
            new_doc.get_first(fields.title).and_then(|v| v.as_str()),
            Some("replacement")
        );
        // A new writer/GC cycle must not collect the previous generation's files.
        let writable = Index::open_in_dir(&dir).unwrap();
        let mut writer = open_writer(&writable, 15_000_000).unwrap();
        writer.commit().unwrap();
        writer.garbage_collect_files().wait().unwrap();
        drop(writer);
        let old_doc: TantivyDocument = retained.doc(tantivy::DocAddress::new(0, 0)).unwrap();
        assert_eq!(
            old_doc
                .get_first(old.schema().get_field("title").unwrap())
                .and_then(|v| v.as_str()),
            Some("retained")
        );
        assert!(
            open_writer(&old, 15_000_000).is_err(),
            "stale handles cannot restore old schema"
        );
    }

    #[test]
    fn migration_failure_after_publication_leaves_complete_new_generation() {
        let world = cv_testkit::World::in_process("migration-after-publication", cv_core::env::Env::enter_isolated);
        let dir = world.base.join("index");
        old_index(&dir);
        let migration = Migration::prepare(&dir, false).unwrap().unwrap();
        fill_stage(&migration);
        let consumer = source_consumer(migration.path()).unwrap();
        let error = migration
            .publish_with(&Swept::default(), |point| {
                anyhow::ensure!(point != Publication::AfterMetadata, "interrupted after publication");
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("interrupted after publication"));
        assert_eq!(query_index(&dir).unwrap().2.num_docs(), 1);
        assert_eq!(source_consumer(&dir).unwrap(), consumer);
        assert!(Migration::prepare(&dir, false).unwrap().is_none());
    }

    #[test]
    fn migration_rejects_partial_ingest_and_retries_whole() {
        let world = cv_testkit::World::in_process("migration-partial", cv_core::env::Env::enter_isolated);
        let dir = world.base.join("index");
        old_index(&dir);
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        let path = world.write_session("good", &[serde_json::json!({"type":"user", "uuid":"good", "message":{"role":"user","content":"migration needle"}})]);
        let good = cv_core::SessionRef::new("good", cv_core::Harness::Claude, path);
        let missing = cv_core::SessionRef::new("missing", cv_core::Harness::Claude, world.base.join("missing.jsonl"));
        assert!(index_sessions(&dir, &[good.clone(), missing], false, false).is_err());
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        assert_eq!(index_sessions(&dir, &[good], false, false).unwrap(), 1);
        assert_eq!(text_search(&dir, "migration", 10).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn migration_retry_cannot_resume_from_globally_advanced_derived_state() {
        use std::io::Write;
        let world = cv_testkit::World::in_process("migration-derived-state", cv_core::env::Env::enter_isolated);
        let dir = world.base.join("index");
        let line = |id: &str, text: &str| serde_json::json!({"type":"user", "uuid":id, "message":{"role":"user","content":text}});
        let path = world.write_session("source", &[line("first", "originalneedle")]);
        let reference = cv_core::SessionRef::new("source", cv_core::Harness::Claude, &path);
        index_sessions(&dir, std::slice::from_ref(&reference), false, false).unwrap();
        let original_counts = cv_core::catalog::AffordanceTable::load().counts(&reference).unwrap();
        let original_mark = cv_core::resume::MarkTable::load().get(&reference).unwrap().clone();
        let original_consumer = source_consumer(&dir).unwrap();
        let dependency = || -> Vec<u8> {
            rusqlite::Connection::open(world.cv_home.join("catalog.db"))
                .unwrap()
                .query_row(
                    "SELECT record FROM source_dependencies WHERE consumer=?1 AND id='source'",
                    [&original_consumer],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let original_dependency = dependency();
        let (retained_index, fields, retained) = query_index(&dir).unwrap();
        // Model an older format without changing any populated field positions or old segments.
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        metadata["schema"]
            .as_array_mut()
            .unwrap()
            .retain(|field| field["name"] != format_field());
        MmapDirectory::open(&dir)
            .unwrap()
            .atomic_write(Path::new("meta.json"), &serde_json::to_vec(&metadata).unwrap())
            .unwrap();
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            serde_json::json!({"type":"assistant", "uuid":"second", "message":{"role":"assistant", "content":[{"type":"text","text":"appendedneedle"},{"type":"tool_use","id":"plan","name":"TodoWrite","input":{"todos":[{"content":"verified plan","status":"pending"}]}}]}})
        )
        .unwrap();
        let missing = cv_core::SessionRef::new("missing", cv_core::Harness::Claude, world.base.join("missing.jsonl"));
        let migration = Migration::prepare(&dir, false).unwrap().unwrap();
        let swept = {
            let _sources = cv_core::source::read_scope();
            sweep_refs(migration.path(), &[reference.clone(), missing], true, false).unwrap()
        };
        assert_eq!((swept.changed, swept.errors), (1, 1), "{:?}", swept.unreadable);
        let failure = migration.publish(&swept).unwrap_err();
        assert!(failure.to_string().contains("migration incomplete"), "{failure:#}");
        assert!(
            failure.to_string().contains("missing"),
            "migration failure must identify its first failed source: {failure:#}"
        );
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        assert_eq!(source_consumer(&dir).unwrap(), original_consumer);
        assert_eq!(dependency(), original_dependency);
        // Global consumers certify the independently completed new source pass. They do not
        // certify the old FTS generation, whose metadata and consumer remain unchanged.
        assert_ne!(
            cv_core::catalog::AffordanceTable::load().counts(&reference).unwrap(),
            original_counts
        );
        assert!(!cv_core::events::needs_ingest(&reference));
        assert!(cv_core::resume::MarkTable::load().get(&reference).unwrap().offset() > original_mark.offset());
        let query = QueryParser::for_index(&retained_index, vec![fields.body])
            .parse_query("appendedneedle")
            .unwrap();
        assert_eq!(retained.search(&query, &tantivy::collector::Count).unwrap(), 0);
        let retried = sweep_migrating(&dir, std::slice::from_ref(&reference), false, false).unwrap();
        assert_eq!((retried.changed, retried.resumed, retried.errors), (1, 0, 0));
        assert_eq!(text_search(&dir, "originalneedle", 10).unwrap().len(), 1);
        assert_eq!(text_search(&dir, "appendedneedle", 10).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn migration_refuses_incomplete_discovery() {
        let world = cv_testkit::World::in_process("migration-discovery", cv_core::env::Env::enter_isolated);
        let dir = world.base.join("index");
        old_index(&dir);
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        // Directory enumeration succeeds, but opening this listed source fails deterministically.
        std::os::unix::fs::symlink("unreadable.jsonl", world.proj.join("unreadable.jsonl")).unwrap();
        let error = index_all(&dir, false, false, false).unwrap_err();
        assert!(error.to_string().contains("discovery was incomplete"), "{error:#}");
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        std::fs::remove_file(world.proj.join("unreadable.jsonl")).unwrap();
        assert_eq!(index_all(&dir, false, false, false).unwrap().0.total, 0);
    }

    #[test]
    fn migration_rebuild_repairs_corrupt_stored_fields_without_early_clear() {
        let world = cv_testkit::World::in_process("migration-corrupt", cv_core::env::Env::enter_isolated);
        let dir = world.base.join("index");
        let (index, fields) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(fields.id => "old", fields.kind => KIND_META))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        let store = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "store"))
            .unwrap();
        std::fs::write(store, b"invalid stored field data").unwrap();
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        assert!(index_sessions(&dir, [], false, false).is_err());
        let migration = Migration::prepare(&dir, true).unwrap().unwrap();
        drop(migration);
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        assert_eq!(index_sessions(&dir, [], true, false).unwrap(), 0);
        assert_eq!(query_index(&dir).unwrap().2.num_docs(), 0);
    }
}
