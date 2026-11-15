//! One guarded mmap boundary for readers, indexers and schema migrations.
use std::path::{Path, PathBuf};
use tantivy::directory::MmapDirectory;
use tantivy::{schema::Schema, Index};

fn directory(path: &Path, read_only: bool) -> tantivy::Result<IndexDirectory> {
    Ok(IndexDirectory {
        inner: MmapDirectory::open(path)?,
        path: path.to_owned(),
        read_only,
    })
}

pub(super) fn open(path: &Path, read_only: bool) -> tantivy::Result<Index> {
    Index::open(directory(path, read_only)?)
}

pub(super) fn create(path: &Path, schema: Schema) -> tantivy::Result<Index> {
    let directory = directory(path, false)?;
    // Match Index::create_in_dir's refusal to overwrite an existing index.
    if Index::exists(&directory)? {
        return Err(tantivy::TantivyError::IndexAlreadyExists);
    }
    Index::create(directory, schema, Default::default())
}

/// The shared index directory, with query-only locking when requested. Tantivy holds `.tantivy-meta.lock` while a reader
/// opens its segments, so a writer's garbage collection cannot delete a file between the listing
/// and the open, and it takes that lock by opening the file for writing, which a sandbox that
/// denies writes to the home refuses. A reader needs no write for it: a shared lock on the lock
/// file opened read-only still excludes the writer's exclusive one, and readers no longer queue
/// behind each other. With no lock file no writer has run in this directory. A query asks for no
/// other lock (the writer lock belongs to `cv index`), so any other is refused.
#[derive(Clone, Debug)]
struct IndexDirectory {
    inner: MmapDirectory,
    path: PathBuf,
    read_only: bool,
}

impl tantivy::Directory for IndexDirectory {
    fn get_file_handle(
        &self,
        path: &Path,
    ) -> Result<std::sync::Arc<dyn tantivy::directory::FileHandle>, tantivy::directory::error::OpenReadError> {
        let handle = self.inner.get_file_handle(path)?;
        // ManagedDirectory parses the footer immediately after this boundary. Tantivy 0.26
        // checks for four bytes before slicing its eight-byte footer trailer; reject a
        // truncated handle here. Inspect the exact immutable handle, not a racy path stat.
        if handle.len() < 8 {
            return Err(tantivy::directory::error::OpenReadError::wrap_io_error(
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "truncated Tantivy segment: expected at least 8 footer bytes, got {}",
                        handle.len()
                    ),
                ),
                path.to_path_buf(),
            ));
        }
        Ok(handle)
    }
    fn delete(&self, path: &Path) -> Result<(), tantivy::directory::error::DeleteError> {
        self.inner.delete(path)
    }
    fn exists(&self, path: &Path) -> Result<bool, tantivy::directory::error::OpenReadError> {
        self.inner.exists(path)
    }
    fn open_write(
        &self,
        path: &Path,
    ) -> Result<tantivy::directory::WritePtr, tantivy::directory::error::OpenWriteError> {
        self.inner.open_write(path)
    }
    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, tantivy::directory::error::OpenReadError> {
        self.inner.atomic_read(path)
    }
    fn atomic_write(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        self.inner.atomic_write(path, data)
    }
    fn sync_directory(&self) -> std::io::Result<()> {
        self.inner.sync_directory()
    }
    fn watch(&self, callback: tantivy::directory::WatchCallback) -> tantivy::Result<tantivy::directory::WatchHandle> {
        self.inner.watch(callback)
    }
    fn acquire_lock(
        &self,
        lock: &tantivy::directory::Lock,
    ) -> Result<tantivy::directory::DirectoryLock, tantivy::directory::error::LockError> {
        use tantivy::directory::{error::LockError, DirectoryLock};
        if !self.read_only {
            return self.inner.acquire_lock(lock);
        }
        if lock.filepath != tantivy::directory::META_LOCK.filepath {
            return Err(LockError::LockBusy);
        }
        match std::fs::File::open(self.path.join(&lock.filepath)) {
            Ok(file) => {
                file.lock_shared().map_err(LockError::wrap_io_error)?;
                Ok(DirectoryLock::from(Box::new(file)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DirectoryLock::from(Box::new(()))),
            Err(error) => Err(LockError::wrap_io_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn tiny_segment_files_return_errors_from_public_search_and_rebuild_can_repair_them() {
        let world = cv_testkit::World::in_process("truncated-footer", cv_core::env::Env::enter_isolated);
        let path = world.write_session(
            "footer",
            &[serde_json::json!({
                "type":"user", "uuid":"footer", "message":{"role":"user", "content":"zqfooter searchable revision"}
            })],
        );
        let reference = cv_core::SessionRef::new("footer", cv_core::Harness::Claude, path);
        // Include both sides of the upstream 4..7-byte panic window.
        for len in [0, 3, 4, 5, 6, 7, 8] {
            let dir = world.base.join(format!("index-{len}"));
            index_sessions(&dir, std::slice::from_ref(&reference), false, false).unwrap();
            let store = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|extension| extension == "store"))
                .unwrap();
            let bytes = vec![b'x'; len];
            std::fs::write(&store, &bytes).unwrap();
            let metadata = std::fs::read(dir.join("meta.json")).unwrap();
            for read_only in [false, true] {
                let index = super::open(&dir, read_only).unwrap();
                let reader: tantivy::Result<tantivy::IndexReader> = index.reader();
                let error = reader.err().expect("truncated segment must be an ordinary read error");
                if len < 8 {
                    assert!(format!("{error:#}").contains("truncated Tantivy segment"));
                }
            }
            let error = crate::Searcher
                .search_in(&dir, "zqfooter", Default::default(), 10, crate::Source::Index, Some(0))
                .err()
                .expect("public search rejects the corrupt index");
            if len < 8 {
                assert!(format!("{error:#}").contains("truncated Tantivy segment"), "{error:#}");
            }
            assert!(index_sessions(&dir, std::slice::from_ref(&reference), false, false).is_err());
            assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), metadata);
            assert_eq!(
                std::fs::read(&store).unwrap(),
                bytes,
                "reads cannot repair or erase source files"
            );
            // The migration's corruption check uses this same directory boundary. It must
            // be able to build and publish a healthy replacement instead of panicking.
            assert_eq!(
                index_sessions(&dir, std::slice::from_ref(&reference), true, false).unwrap(),
                1
            );
            assert_eq!(text_search_indexed(&dir, "zqfooter", 10).unwrap().len(), 1);
        }
    }

    #[test]
    fn short_atomic_metadata_reads_do_not_use_segment_footer_rules() {
        use tantivy::Directory;
        let world = cv_testkit::World::in_process("short-metadata", cv_core::env::Env::enter_isolated);
        let directory = super::directory(&world.base, false).unwrap();
        directory.atomic_write(Path::new("metadata.json"), b"{}").unwrap();
        assert_eq!(directory.atomic_read(Path::new("metadata.json")).unwrap(), b"{}");
    }
}
