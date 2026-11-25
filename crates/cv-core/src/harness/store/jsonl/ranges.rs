//! Bounded positioned JSONL framing. Producers retain/check source identity and choose their own
//! JSON/UTF-8 acceptance policy; these helpers preserve raw ranges, JSON whitespace trim and partial EOF.
use anyhow::{Context, Result};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read},
};

/// Read one captured extent without changing a shared descriptor's seek position.
pub(crate) fn reader<S: At + ?Sized>(file: &S, range: Range) -> Result<BufReader<Reader<'_, S>>> {
    let end = range.offset.checked_add(range.len).context("JSONL range overflow")?;
    Ok(BufReader::with_capacity(
        64 * 1024,
        Reader {
            file,
            at: range.offset,
            end,
        },
    ))
}
/// Each completed range excludes LF; CR and other whitespace remain until trim. A partial final
/// row is emitted with complete=false. On read error the consumer may stop or retry from offset().
pub(crate) fn records<S: At + ?Sized>(file: &S, range: Range) -> Result<Records<'_, S>> {
    Ok(Records {
        reader: reader(file, range)?,
        at: range.offset,
        small: Vec::new(),
        small_limit: 0,
        small_complete: false,
        digest: None,
    })
}

/// Trim JSON whitespace ([`super::super::json::is_whitespace`]) from a record of any size,
/// reading only its boundary windows.
pub(crate) fn trim<S: At + ?Sized>(file: &S, mut range: Range) -> Result<Range> {
    use crate::harness::store::json::is_whitespace;
    range.offset.checked_add(range.len).context("JSONL range overflow")?;
    let mut window = [0; 64 * 1024];
    while range.len > 0 {
        let n = range.len.min(window.len() as u64) as usize;
        read_exact_at(file, &mut window[..n], range.offset)?;
        let lead = window[..n].iter().take_while(|b| is_whitespace(b)).count();
        range.offset += lead as u64;
        range.len -= lead as u64;
        if lead != n {
            break;
        }
    }
    while range.len > 0 {
        let n = range.len.min(window.len() as u64) as usize;
        read_exact_at(file, &mut window[..n], range.offset + range.len - n as u64)?;
        let tail = window[..n].iter().rev().take_while(|b| is_whitespace(b)).count();
        range.len -= tail as u64;
        if tail != n {
            break;
        }
    }
    Ok(range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_source_is_an_error_not_a_partial_final_record() {
        let bytes = &b"whole\n12"[..];
        let mut rows = records(bytes, Range { offset: 0, len: 10 }).unwrap();
        assert!(rows.next().unwrap().unwrap().1);
        let error = rows.next().unwrap().err().expect("captured bytes disappeared");
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        // Reaching the captured end without LF is still an ordinary partial record.
        let mut rows = records(bytes, Range { offset: 6, len: 2 }).unwrap();
        assert!(!rows.next().unwrap().unwrap().1);
        assert!(rows.next().is_none());
    }

    #[test]
    fn positioned_frames_match_buffered_lines_and_trim_boundary_windows() {
        let world = cv_testkit::World::new("positioned-jsonl-frames");
        let text = format!(
            "{{\"n\":1}}\r\n\n{}{{\"body\":\"{}\"}}{}\n{}\n{}\n final 🌙 ",
            " \t".repeat(40000),
            "x\\n".repeat(30000),
            "\r ".repeat(40000),
            "\u{2003}".repeat(30000),
            " ".repeat(140000),
        );
        let path = world.base.join("rows.jsonl");
        std::fs::write(&path, &text).unwrap();
        let file = fs::File::open(path).unwrap();
        let range = Range {
            offset: 0,
            len: text.len() as u64,
        };
        let mut framed = records(&file, range).unwrap();
        let mut at = 0;
        for row in text.split_inclusive('\n') {
            let (raw, complete) = framed.next().unwrap().unwrap();
            let complete_expected = row.ends_with('\n');
            assert_eq!(
                (raw.offset, raw.len, complete),
                (
                    at,
                    (row.len() - usize::from(complete_expected)) as u64,
                    complete_expected
                )
            );
            at += row.len() as u64;
            assert_eq!(framed.offset(), at);
            let trimmed = trim(&file, raw).unwrap();
            let mut actual = String::new();
            reader(&file, trimmed).unwrap().read_to_string(&mut actual).unwrap();
            assert_eq!(actual, crate::harness::store::json::trim_str(row));
        }
        assert!(framed.next().is_none());
        assert!(reader(
            &file,
            Range {
                offset: u64::MAX,
                len: 1
            }
        )
        .is_err());
        assert!(trim(
            &file,
            Range {
                offset: u64::MAX,
                len: 1
            }
        )
        .is_err());
        let suffix = Range {
            offset: 10,
            len: range.len - 10,
        };
        assert_eq!(records(&file, suffix).unwrap().next().unwrap().unwrap().0.offset, 10);
    }

    #[test]
    fn framing_leaves_utf8_and_json_rejection_to_the_consumer() {
        let world = cv_testkit::World::new("positioned-jsonl-invalid");
        let path = world.base.join("rows.jsonl");
        let bytes = b"\xff\nnot json\n{\"ok\":1}";
        std::fs::write(&path, bytes).unwrap();
        let file = fs::File::open(path).unwrap();
        let rows = records(
            &file,
            Range {
                offset: 0,
                len: bytes.len() as u64,
            },
        )
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].0.offset, rows[0].0.len, rows[0].1), (0, 1, true));
        assert_eq!((rows[2].0.offset, rows[2].0.len, rows[2].1), (11, 8, false));
        let invalid = trim(&file, rows[0].0).unwrap();
        assert_eq!((invalid.offset, invalid.len), (0, 1));
    }
    #[cfg(not(feature = "mmap"))]
    #[test]
    fn optional_small_records_drop_giant_bytes_and_resume_after_window_boundaries() {
        let world = crate::test_world("positioned-small-record-cache");
        let path = world.base.join("rows.jsonl");
        let bytes = format!("{}\n{}\nsmall\r\npartial", "a".repeat(65535), "b".repeat(65537));
        fs::write(&path, &bytes).unwrap();
        let file = fs::File::open(path).unwrap();
        let mut rows = records(
            &file,
            Range {
                offset: 0,
                len: bytes.len() as u64,
            },
        )
        .unwrap();
        rows.keep_small_records(65536);
        for expected in bytes.split_inclusive('\n') {
            let complete = expected.ends_with('\n');
            let body = expected.strip_suffix('\n').unwrap_or(expected);
            let (range, got_complete) = rows.next().unwrap().unwrap();
            assert_eq!(range.len, body.len() as u64);
            assert_eq!(got_complete, complete);
            assert_eq!(rows.small_record(), (body.len() <= 65536).then_some(body.as_bytes()));
            assert!(rows.small.capacity() <= 65536);
        }
        assert!(rows.next().is_none());
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Range {
    pub offset: u64,
    pub len: u64,
}
pub(crate) struct Reader<'a, S: ?Sized = fs::File> {
    file: &'a S,
    at: u64,
    end: u64,
}
impl<S: At + ?Sized> Read for Reader<'_, S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() || self.at == self.end {
            return Ok(0);
        }
        let n = ((self.end - self.at).min(bytes.len() as u64)) as usize;
        let n = loop {
            match self.file.read_at(&mut bytes[..n], self.at) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "JSONL source was truncated",
            ));
        }
        self.at += n as u64;
        Ok(n)
    }
}
/// Positioned bytes: a file, or bytes already in memory (such as a mapped private decode), so one
/// framing implementation serves both without per-record syscalls on the second.
pub(crate) trait At {
    fn read_at(&self, bytes: &mut [u8], at: u64) -> io::Result<usize>;
}
impl At for fs::File {
    fn read_at(&self, bytes: &mut [u8], at: u64) -> io::Result<usize> {
        crate::harness::store::bytes::read_at(self, bytes, at)
    }
}
impl At for dyn super::super::bytes::Positioned + '_ {
    fn read_at(&self, bytes: &mut [u8], at: u64) -> io::Result<usize> {
        super::super::bytes::Positioned::read_at(self, bytes, at)
    }
}
impl At for [u8] {
    fn read_at(&self, bytes: &mut [u8], at: u64) -> io::Result<usize> {
        let start = usize::try_from(at).map_or(self.len(), |at| at.min(self.len()));
        let n = bytes.len().min(self.len() - start);
        bytes[..n].copy_from_slice(&self[start..start + n]);
        Ok(n)
    }
}
#[cfg(feature = "mmap")]
impl At for memmap2::Mmap {
    fn read_at(&self, bytes: &mut [u8], at: u64) -> io::Result<usize> {
        self[..].read_at(bytes, at)
    }
}
pub(crate) fn read_exact_at<S: At + ?Sized>(file: &S, mut bytes: &mut [u8], mut at: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        match file.read_at(bytes, at) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                at += n as u64;
                bytes = &mut bytes[n..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
/// Whole-range UTF-8 validates ignored/discarded fields too. At most one incomplete scalar is
/// carried over a window boundary; JSON callers independently validate their grammar.
pub(crate) fn valid_utf8(cursor: &mut impl BufRead) -> io::Result<bool> {
    let mut chunks = crate::utf8::Chunks::new(crate::utf8::Verdict::Reject);
    let mut ignore = |_: &str| Ok::<(), io::Error>(());
    loop {
        let bytes = cursor.fill_buf()?;
        if bytes.is_empty() {
            return chunks.finish(&mut ignore);
        }
        if !chunks.feed(bytes, &mut ignore)? {
            return Ok(false);
        }
        let n = bytes.len();
        cursor.consume(n);
    }
}

pub(crate) struct Records<'a, S: ?Sized = fs::File> {
    reader: BufReader<Reader<'a, S>>,
    at: u64,
    small: Vec<u8>,
    small_limit: usize,
    small_complete: bool,
    digest: Option<crate::digest::Sha256>,
}
impl<S: ?Sized> Records<'_, S> {
    /// Hash every byte the framer reads, in order, so a consumer that walks the whole range has
    /// its digest from this one read ([`Self::finish_digest`]).
    pub(crate) fn hashing(mut self) -> Self {
        self.digest = Some(crate::digest::Sha256::new());
        self
    }
    /// The SHA-256 of every byte framed so far (all of them once iteration returned `None`).
    pub(crate) fn finish_digest(self) -> Option<String> {
        self.digest.map(crate::digest::Sha256::finish_hex)
    }
    /// Optionally retain small physical records during framing, avoiding a second positioned
    /// read. Above this fixed cap the scratch is cleared; giant records remain only ranges.
    /// The scratch grows with the records it holds, never past the cap. Set this before iteration;
    /// changing it cannot retroactively retain a record already returned by next().
    pub(crate) fn keep_small_records(&mut self, limit: usize) {
        self.small_limit = limit;
    }
    pub(crate) fn small_record(&self) -> Option<&[u8]> {
        self.small_complete.then_some(&self.small)
    }
    pub(crate) fn offset(&self) -> u64 {
        self.at
    }
}
impl<S: At + ?Sized> Iterator for Records<'_, S> {
    type Item = Result<(Range, bool)>;
    fn next(&mut self) -> Option<Self::Item> {
        let start = self.at;
        self.small.clear();
        self.small_complete = self.small_limit > 0;
        loop {
            let bytes = match self.reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) => return Some(Err(error.into())),
            };
            if bytes.is_empty() {
                return (self.at > start).then_some(Ok((
                    Range {
                        offset: start,
                        len: self.at - start,
                    },
                    false,
                )));
            }
            let newline = memchr::memchr(b'\n', bytes);
            let n = newline.map_or(bytes.len(), |i| i + 1);
            if let Some(digest) = &mut self.digest {
                digest.update(&bytes[..n]);
            }
            if self.small_complete {
                let payload = n - usize::from(newline.is_some());
                if payload <= self.small_limit - self.small.len() {
                    let needed = self.small.len() + payload;
                    if needed > self.small.capacity() {
                        let grown = needed.max(2 * self.small.capacity()).min(self.small_limit);
                        self.small.reserve_exact(grown - self.small.len());
                    }
                    self.small.extend_from_slice(&bytes[..payload]);
                } else {
                    self.small.clear();
                    self.small_complete = false;
                }
            }
            self.at += n as u64;
            self.reader.consume(n);
            if newline.is_some() {
                return Some(Ok((
                    Range {
                        offset: start,
                        len: self.at - start - 1,
                    },
                    true,
                )));
            }
        }
    }
}
