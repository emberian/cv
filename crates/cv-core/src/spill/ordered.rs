//! Ordered, atomic staging without retaining an entire source pass in private memory.
use crate::harness::store::bytes::Spill as Owner;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, Write};

const MEMORY_BYTES: usize = 256 * 1024;
const IO_BYTES: usize = 64 * 1024;

pub trait Record: Sized {
    type ReadState: Default;
    /// Heap retained by this record, excluding its inline struct (charged by container capacity).
    fn heap_bytes(&self) -> usize;
    fn write(&self, writer: &mut impl Write) -> Result<()>;
    /// Read exactly one record. Codecs must preserve their consumer's exact wire/value contract.
    /// `state` is per-reader carry state — a scratch buffer or a read budget; `Default` arms
    /// nothing, the contract under which the ordered spool re-reads its own staged bytes.
    fn read(reader: &mut impl BufRead, state: &mut Self::ReadState) -> Result<Self>;
}

impl Record for String {
    type ReadState = ();
    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
    fn write(&self, writer: &mut impl std::io::Write) -> anyhow::Result<()> {
        writer.write_all(&(self.len() as u64).to_le_bytes())?;
        writer.write_all(self.as_bytes())?;
        Ok(())
    }
    fn read(reader: &mut impl std::io::BufRead, _: &mut ()) -> anyhow::Result<Self> {
        let mut len = [0; 8];
        reader.read_exact(&mut len)?;
        let len = usize::try_from(u64::from_le_bytes(len))?;
        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes)?;
        Ok(String::from_utf8(bytes)?)
    }
}

/// An `Option<string>` wire field: a flag byte, then the length and bytes when present.
pub(crate) fn write_string(writer: &mut impl Write, value: Option<&str>) -> Result<()> {
    writer.write_all(&[u8::from(value.is_some())])?;
    if let Some(value) = value {
        writer.write_all(&(value.len() as u64).to_le_bytes())?;
        writer.write_all(value.as_bytes())?;
    }
    Ok(())
}

/// [`write_string`]'s reader; `limit` bounds one string's length.
pub(crate) fn read_string(reader: &mut impl BufRead, limit: Option<usize>) -> Result<Option<String>> {
    let mut flag = [0];
    reader.read_exact(&mut flag)?;
    match flag[0] {
        0 => Ok(None),
        1 => {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            let len = usize::try_from(u64::from_le_bytes(bytes)).context("usage spool string too large")?;
            anyhow::ensure!(
                limit.is_none_or(|limit| len <= limit),
                "usage cache string exceeds source budget"
            );
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes)?;
            Ok(Some(String::from_utf8(bytes).context("invalid usage spool UTF-8")?))
        }
        _ => anyhow::bail!("invalid usage spool string"),
    }
}

pub struct Ordered<T> {
    memory: VecDeque<T>,
    heap_bytes: usize,
    spill: Option<Spill>,
    len: usize,
}
impl<T> Default for Ordered<T> {
    fn default() -> Self {
        Self {
            memory: VecDeque::new(),
            heap_bytes: 0,
            spill: None,
            len: 0,
        }
    }
}
struct Spill {
    writer: Option<BufWriter<File>>,
    _owner: Owner,
}
impl<T: Record> Ordered<T> {
    #[cfg(any(test, feature = "sqlite"))]
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    #[cfg(any(test, feature = "sqlite"))]
    pub(crate) fn spilled_bytes(&self) -> Result<u64> {
        match &self.spill {
            None => Ok(0),
            Some(spill) => {
                let writer = spill.writer.as_ref().context("ordered spool already consumed")?;
                Ok(writer.get_ref().metadata()?.len() + writer.buffer().len() as u64)
            }
        }
    }
    pub fn push(&mut self, row: T) -> Result<()> {
        let slots = if self.memory.len() == self.memory.capacity() {
            self.memory.capacity().max(4).saturating_mul(2)
        } else {
            self.memory.capacity()
        };
        let estimated = self
            .heap_bytes
            .saturating_add(row.heap_bytes())
            .saturating_add(slots.saturating_mul(std::mem::size_of::<T>()));
        if self.spill.is_none() && estimated > MEMORY_BYTES {
            let owner = Owner::create(false).context("creating private spill file")?;
            let file = owner.file.as_ref().unwrap().try_clone()?;
            self.spill = Some(Spill {
                writer: Some(BufWriter::with_capacity(IO_BYTES, file)),
                _owner: owner,
            });
            let writer = self.spill.as_mut().unwrap().writer.as_mut().unwrap();
            for old in self.memory.drain(..) {
                old.write(writer)?;
            }
            self.memory = VecDeque::new();
            self.heap_bytes = 0;
        }
        match &mut self.spill {
            Some(spill) => row.write(spill.writer.as_mut().unwrap())?,
            None => {
                self.heap_bytes += row.heap_bytes();
                self.memory.push_back(row);
            }
        }
        self.len += 1;
        Ok(())
    }
    #[cfg(all(test, feature = "sqlite"))]
    pub(crate) fn append_corrupt(&mut self, bytes: &[u8]) -> Result<()> {
        self.spill
            .as_mut()
            .context("test needs spilled records")?
            .writer
            .as_mut()
            .context("test spool already consumed")?
            .write_all(bytes)?;
        Ok(())
    }
    pub fn rows(mut self) -> Result<Rows<T>> {
        let reader = if let Some(spill) = &mut self.spill {
            let mut writer = spill.writer.take().unwrap();
            writer.flush().context("flushing ordered spool")?;
            writer.get_mut().rewind().context("rewinding ordered spool")?;
            let file = writer
                .into_inner()
                .map_err(|e| e.into_error())
                .context("opening ordered spool reader")?;
            Some(BufReader::with_capacity(IO_BYTES, file))
        } else {
            None
        };
        Ok(Rows {
            spool: self,
            reader,
            failed: false,
            read_count: 0,
            state: T::ReadState::default(),
        })
    }
}
pub struct Rows<T: Record> {
    // Reader closes before the ownership guard removes its path on platforms without unlink.
    reader: Option<BufReader<File>>,
    spool: Ordered<T>,
    failed: bool,
    read_count: usize,
    state: T::ReadState,
}
impl<T: Record> Iterator for Rows<T> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let Some(reader) = &mut self.reader else {
            return self.spool.memory.pop_front().map(Ok);
        };
        let result = if self.read_count == self.spool.len {
            match reader.fill_buf() {
                Ok([]) => return None,
                Ok(_) => Err(anyhow::anyhow!("extra ordered spool data")),
                Err(error) => Err(error.into()),
            }
        } else {
            T::read(reader, &mut self.state).context("reading ordered spool record")
        };
        self.failed = result.is_err();
        self.read_count += usize::from(result.is_ok());
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Number(u64);
    impl Record for Number {
        type ReadState = ();
        fn heap_bytes(&self) -> usize {
            0
        }
        fn write(&self, writer: &mut impl Write) -> Result<()> {
            Ok(writer.write_all(&self.0.to_le_bytes())?)
        }
        fn read(reader: &mut impl BufRead, _: &mut ()) -> Result<Self> {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            Ok(Self(u64::from_le_bytes(bytes)))
        }
    }
    #[test]
    fn empty_payloads_charge_slots_and_order_survives_spill() {
        let mut spool = Ordered::default();
        for i in 0..100_000 {
            spool.push(Number(i)).unwrap();
        }
        assert!(spool.spill.is_some());
        assert_eq!(spool.memory.capacity(), 0);
        for (i, row) in spool.rows().unwrap().enumerate() {
            assert_eq!(row.unwrap().0, i as u64);
        }
    }
    #[test]
    fn truncated_and_extra_spills_fail_explicitly() {
        for extra in [false, true] {
            let mut spool = Ordered::default();
            for i in 0..40_000 {
                spool.push(Number(i)).unwrap();
            }
            let writer = spool.spill.as_mut().unwrap().writer.as_mut().unwrap();
            writer.flush().unwrap();
            let length = writer.get_ref().metadata().unwrap().len();
            if extra {
                writer.write_all(b"extra").unwrap();
            } else {
                writer.get_ref().set_len(length - 1).unwrap();
            }
            assert!(spool.rows().unwrap().collect::<Result<Vec<_>>>().is_err());
        }
    }
}
