//! Immutable operation-owned bytes, bounded in memory and spilled privately when necessary.
pub(crate) const MEMORY_LIMIT: usize = 1024 * 1024;
/// Darwin can fault while writing mmap-backed input into the same vnode. All arena append
/// writes pass through independently owned bounded scratch, including serializer fast paths
/// that bypass their own BufWriter for large windows.
pub(crate) struct ScratchWriter<'a> {
    out: &'a mut dyn std::io::Write,
    scratch: Vec<u8>,
    quote: bool,
    pub(crate) len: usize,
}
impl<'a> ScratchWriter<'a> {
    pub(crate) fn new(out: &'a mut dyn std::io::Write, quote: bool) -> Self {
        Self {
            out,
            scratch: Vec::new(),
            quote,
            len: 0,
        }
    }
}
impl std::io::Write for ScratchWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.len = self
            .len
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("scratch writer length overflow"))?;
        for part in bytes.chunks(super::IO_CHUNK) {
            self.scratch.clear();
            self.scratch.reserve(part.len() * if self.quote { 2 } else { 1 });
            if self.quote {
                // Canonical JSON already escaped control bytes; embedding it as a string
                // only needs another escape layer for quotes and backslashes.
                for &byte in part {
                    if matches!(byte, b'"' | b'\\') {
                        self.scratch.push(b'\\');
                    }
                    self.scratch.push(byte);
                }
            } else {
                self.scratch.extend_from_slice(part);
            }
            self.out.write_all(&self.scratch)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[derive(Debug)]
pub(crate) struct Spill {
    pub(crate) file: Option<std::fs::File>,
    path: std::path::PathBuf,
    linked: bool,
}
impl Spill {
    pub(crate) fn create(append: bool) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("cv-byte-source-{}", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).append(append).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let spill = Self {
            file: Some(options.open(&path)?),
            path,
            // Unix unlinks the private path immediately so a reused path survives; other
            // targets unlink on Drop.
            linked: !cfg!(unix),
        };
        #[cfg(unix)]
        std::fs::remove_file(&spill.path)?;
        Ok(spill)
    }
}
impl Drop for Spill {
    fn drop(&mut self) {
        self.file.take();
        if self.linked {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
#[derive(Debug)]
pub struct ByteSource {
    bytes: Bytes,
}
#[derive(Debug)]
enum Bytes {
    Memory(Vec<u8>),
    File(Spill),
    Arena { spill: Spill, append: std::sync::Mutex<()> },
}

/// Writable until finish, then immutable. Partial/failed operations own and remove their spill.
#[derive(Default)]
pub struct ByteStage {
    memory: Vec<u8>,
    spill: Option<Spill>,
    len: u64,
}
impl std::io::Write for ByteStage {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let len = self
            .len
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("byte stage length overflow"))?;
        if self.spill.is_none() && self.memory.len().saturating_add(bytes.len()) > MEMORY_LIMIT {
            self.spill = Some(Spill::create(false)?);
            self.spill
                .as_mut()
                .unwrap()
                .file
                .as_mut()
                .unwrap()
                .write_all(&self.memory)?;
            self.memory = Vec::new();
        }
        if let Some(spill) = &mut self.spill {
            spill.file.as_mut().unwrap().write_all(bytes)?;
        } else {
            // Vec's geometric growth must not overshoot the memory budget for irregular writes.
            let required = self.memory.len() + bytes.len();
            if required > self.memory.capacity() {
                let capacity = self
                    .memory
                    .capacity()
                    .max(super::IO_CHUNK)
                    .saturating_mul(2)
                    .min(MEMORY_LIMIT)
                    .max(required);
                self.memory.reserve_exact(capacity - self.memory.len());
            }
            self.memory.extend_from_slice(bytes);
        }
        self.len = len;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(spill) = &mut self.spill {
            spill.file.as_mut().unwrap().flush()?;
        }
        Ok(())
    }
}
impl ByteStage {
    pub fn new() -> Self {
        Self::default()
    }
    /// Bytes written to this unpublished stage. Immutable sources expose no rollback.
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Discard an unpublished suffix and resume writing at the retained extent.
    pub fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        if len > self.len {
            return Err(std::io::Error::other("byte stage truncation exceeds length"));
        }
        if let Some(spill) = &mut self.spill {
            let file = spill.file.as_mut().unwrap();
            file.set_len(len)?;
            file.seek(SeekFrom::Start(len))?;
        } else {
            self.memory
                .truncate(usize::try_from(len).map_err(std::io::Error::other)?);
        }
        self.len = len;
        Ok(())
    }
    pub fn finish(mut self) -> std::io::Result<ByteSource> {
        use std::io::Write;
        self.flush()?;
        Ok(ByteSource {
            bytes: match self.spill.take() {
                Some(spill) => Bytes::File(spill),
                None => Bytes::Memory(std::mem::take(&mut self.memory)),
            },
        })
    }
}
impl ByteSource {
    pub(crate) fn is_arena(&self) -> bool {
        matches!(self.bytes, Bytes::Arena { .. })
    }
    pub(crate) fn memory_bytes(&self) -> Option<&[u8]> {
        match &self.bytes {
            Bytes::Memory(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// An operation-owned append-only arena. Published prefixes are never overwritten/truncated.
    pub(crate) fn arena() -> std::io::Result<Self> {
        Ok(Self {
            bytes: Bytes::Arena {
                spill: Spill::create(true)?,
                append: std::sync::Mutex::new(()),
            },
        })
    }
    pub(crate) fn append(
        &self,
        f: impl FnOnce(&mut dyn std::io::Write) -> anyhow::Result<()>,
    ) -> anyhow::Result<(u64, u64)> {
        let Bytes::Arena { spill, append } = &self.bytes else {
            anyhow::bail!("not an append arena")
        };
        let _guard = append
            .lock()
            .map_err(|_| std::io::Error::other("decoded arena poisoned"))?;
        let mut file = spill.file.as_ref().unwrap().try_clone()?;
        let offset = file.metadata()?.len();
        // O_APPEND makes published prefixes immutable even while positioned readers run.
        // Failed appends remain unreachable; truncating could invalidate mapped views.
        {
            use std::io::Write;
            let mut writer = std::io::BufWriter::with_capacity(super::IO_CHUNK, ScratchWriter::new(&mut file, false));
            let produced = f(&mut writer);
            // Flush before observing the published extent. Failed producer output remains an
            // unreachable append, and the producer's error takes precedence over a flush error.
            let flushed = writer.flush();
            produced?;
            flushed?;
        }
        let end = file.metadata()?.len();
        Ok((offset, end - offset))
    }
    /// Length of the immutable backing without reading its payload.
    pub fn byte_len(&self) -> std::io::Result<u64> {
        match &self.bytes {
            Bytes::Memory(bytes) => Ok(bytes.len() as u64),
            Bytes::File(spill) => Ok(spill.file.as_ref().unwrap().metadata()?.len()),
            Bytes::Arena { .. } => Ok(self.file()?.metadata()?.len()),
        }
    }
    pub(crate) fn file(&self) -> std::io::Result<std::fs::File> {
        match &self.bytes {
            Bytes::File(spill) => spill.file.as_ref().unwrap().try_clone(),
            Bytes::Arena { spill, .. } => spill.file.as_ref().unwrap().try_clone(),
            Bytes::Memory(_) => Err(std::io::Error::other("expected file-backed bytes")),
        }
    }

    /// Read an operation-owned source, spilling above the fixed memory budget.
    pub fn read(mut input: impl std::io::Read) -> std::io::Result<Self> {
        use std::io::Write;
        let mut stage = ByteStage::new();
        let mut chunk = [0; super::IO_CHUNK];
        loop {
            let n = input.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            stage.write_all(&chunk[..n])?;
        }
        stage.finish()
    }
    /// Prefer native contiguous parsing. Mapping never outlives this exclusively owned source.
    #[cfg(feature = "mmap")]
    pub(crate) fn with_bytes<T>(&self, f: impl FnOnce(&[u8]) -> T) -> std::io::Result<Option<T>> {
        match &self.bytes {
            Bytes::Memory(bytes) => Ok(Some(f(bytes))),
            Bytes::Arena { .. } => Ok(None),
            Bytes::File(spill) => {
                let file = spill.file.as_ref().unwrap();
                if file.metadata()?.len() == 0 {
                    return Ok(Some(f(&[])));
                }
                // Only this operation owns the backing descriptor; it is never mutated.
                Ok(unsafe { memmap2::Mmap::map(file) }.ok().map(|map| f(&map)))
            }
        }
    }
    /// Each bounded reader has an independent position, including on unlinked private spills.
    pub fn reader(&self) -> std::io::Result<Box<dyn std::io::Read + '_>> {
        match &self.bytes {
            Bytes::Memory(bytes) => Ok(Box::new(std::io::Cursor::new(bytes))),
            Bytes::Arena { .. } => Ok(Box::new(std::io::BufReader::new(FileReader {
                file: self.file()?,
                position: 0,
            }))),
            Bytes::File(spill) => Ok(Box::new(std::io::BufReader::new(FileReader {
                file: spill.file.as_ref().unwrap(),
                position: 0,
            }))),
        }
    }
    /// Publish only after the owning operation has validated its successful completion.
    pub fn copy_to(&self, out: &mut (impl std::io::Write + ?Sized)) -> std::io::Result<u64> {
        match &self.bytes {
            Bytes::Memory(bytes) => {
                out.write_all(bytes)?;
                Ok(bytes.len() as u64)
            }
            Bytes::File(_) | Bytes::Arena { .. } => std::io::copy(&mut self.reader()?, out),
        }
    }
}
struct FileReader<F> {
    file: F,
    position: u64,
}
impl<F: std::borrow::Borrow<std::fs::File>> std::io::Read for FileReader<F> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = read_at(self.file.borrow(), bytes, self.position)?;
        self.position += n as u64;
        Ok(n)
    }
}
/// The platform boundary for positioned source reads. Unix reads leave the shared cursor
/// untouched; other targets retain their platform's positioned-read or seek fallback.
pub(crate) fn read_at(file: &std::fs::File, bytes: &mut [u8], at: u64) -> std::io::Result<usize> {
    // Charge actual positioned I/O, including lazy large-record reads from a visitor.
    // Never retain admission across parsing or callbacks that can read another device.
    #[cfg(unix)]
    let _permit = {
        use std::os::unix::fs::MetadataExt;
        crate::io_admission::IoAdmission::global().acquire(crate::io_admission::DeviceId(file.metadata()?.dev()), None)
    };
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, bytes, at)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, bytes, at)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut file = file.try_clone()?;
        use std::io::{Read, Seek};
        file.seek(std::io::SeekFrom::Start(at))?;
        file.read(bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn dropping_unlinked_spill_preserves_reused_path() {
        for append in [false, true] {
            let spill = Spill::create(append).unwrap();
            let path = spill.path.clone();
            std::fs::write(&path, b"different owner").unwrap();
            drop(spill);
            assert_eq!(std::fs::read(&path).unwrap(), b"different owner");
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn private_spill_permissions_and_owned_cleanup() {
        let owner = Spill::create(false).unwrap();
        let file = owner.file.as_ref().unwrap().try_clone().unwrap();
        let path = owner.path.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert!(!path.exists());
        }
        drop(file);
        drop(owner);
        assert!(!path.exists());
    }
    #[test]
    fn unpublished_stage_rollback_preserves_memory_and_spilled_prefixes() {
        use std::io::{Read, Write};
        for initial in [16, MEMORY_LIMIT + 16] {
            let mut stage = ByteStage::new();
            stage.write_all(&vec![b'a'; initial]).unwrap();
            assert_eq!(stage.len(), initial as u64);
            stage.write_all(&vec![b'b'; MEMORY_LIMIT + 7]).unwrap();
            stage.truncate(initial as u64).unwrap();
            assert_eq!(stage.len(), initial as u64);
            assert!(stage.truncate(initial as u64 + 1).is_err());
            stage.write_all(b"last").unwrap();
            let mut actual = Vec::new();
            stage
                .finish()
                .unwrap()
                .reader()
                .unwrap()
                .read_to_end(&mut actual)
                .unwrap();
            assert_eq!(actual, [vec![b'a'; initial], b"last".to_vec()].concat());
        }
        let mut stage = ByteStage::new();
        stage.write_all(b"discard").unwrap();
        stage.truncate(0).unwrap();
        assert!(stage.is_empty());
        stage.write_all(b"accepted").unwrap();
        let mut actual = Vec::new();
        stage
            .finish()
            .unwrap()
            .reader()
            .unwrap()
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(actual, b"accepted");
    }

    #[test]
    fn arena_small_writes_flush_before_publishing_length() {
        let source = ByteSource::arena().unwrap();
        let expected = b"small escaped fragment\n".repeat(10000);
        let (offset, len) = source
            .append(|out| {
                for byte in &expected {
                    out.write_all(std::slice::from_ref(byte))?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(offset, 0);
        assert_eq!(len, expected.len() as u64);
        let mut actual = Vec::new();
        std::io::Read::read_to_end(&mut source.reader().unwrap(), &mut actual).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn arena_failed_appends_never_truncate_published_prefixes() {
        use std::io::Read;
        let source = ByteSource::arena().unwrap();
        assert_eq!(
            source
                .append(|file| {
                    file.write_all(b"first")?;
                    Ok(())
                })
                .unwrap(),
            (0, 5)
        );
        let error = source
            .append(|file| {
                file.write_all(b"partial")?;
                anyhow::bail!("injected decoder failure")
            })
            .unwrap_err();
        assert!(error.to_string().contains("injected decoder failure"));
        assert_eq!(
            source
                .append(|file| {
                    file.write_all(b"last")?;
                    Ok(())
                })
                .unwrap(),
            (12, 4)
        );
        let mut first = source.reader().unwrap();
        let mut second = source.reader().unwrap();
        let mut prefix = [0; 5];
        first.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"first");
        second.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"first");
        let Bytes::Arena { spill, .. } = &source.bytes else {
            unreachable!()
        };
        let path = spill.path.clone();
        #[cfg(unix)]
        assert!(!path.exists());
        drop(first);
        drop(second);
        drop(source);
        assert!(!path.exists());
    }

    #[test]
    fn writer_capacity_and_independent_readers_preserve_bytes() {
        use std::io::{Read, Write};
        let mut stage = ByteStage::new();
        stage.write_all(&vec![b'a'; 900000]).unwrap();
        stage.write_all(&vec![b'b'; 100000]).unwrap();
        assert!(stage.memory.capacity() <= MEMORY_LIMIT);
        stage.write_all(&vec![b'c'; 100000]).unwrap();
        let source = stage.finish().unwrap();
        let mut first = source.reader().unwrap();
        let mut second = source.reader().unwrap();
        let mut prefix = [0; 4];
        first.read_exact(&mut prefix).unwrap();
        second.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"aaaa");
        let mut output = Vec::new();
        assert_eq!(source.copy_to(&mut output).unwrap(), 1100000);
        assert_eq!(&output[899999..900001], b"ab");
        assert_eq!(&output[999999..1000001], b"bc");
        let mut tail = Vec::new();
        first.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, output[4..]);
        tail.clear();
        second.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, output[4..]);
    }

    #[test]
    fn owned_spill_is_unlinked_and_reader_stays_bounded() {
        use std::io::Read;
        let input = vec![b' '; MEMORY_LIMIT + 17];
        let source = ByteSource::read(input.as_slice()).unwrap();
        let Bytes::File(spill) = &source.bytes else {
            panic!("expected spill")
        };
        assert_eq!(
            spill.file.as_ref().unwrap().metadata().unwrap().len(),
            input.len() as u64
        );
        #[cfg(unix)]
        assert!(!spill.path.exists());
        let path = spill.path.clone();
        let mut prefix = [0; 17];
        source.reader().unwrap().read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, [b' '; 17]);
        #[cfg(feature = "mmap")]
        assert_eq!(source.with_bytes(|bytes| bytes.len()).unwrap(), Some(input.len()));
        drop(source);
        assert!(!path.exists());
    }
}
