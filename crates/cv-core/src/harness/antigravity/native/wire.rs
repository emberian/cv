//! Checked protobuf wire ranges over an immutable, operation-owned SQLite cell.
//! Payload bytes stay in the staged file; navigation owns only field descriptors.
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use serde_json::ser::Formatter;
use std::fs::File;
use std::io::{self, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Value {
    Varint(u64),
    Fixed64(u64),
    Bytes { at: u64, len: u64 },
    Fixed32(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Field {
    pub number: u32,
    pub value: Value,
}

/// Fields of one message. Real steps hold tens; the bound keeps a hostile or corrupt
/// cell from turning each two bytes into a 24-byte descriptor (6 MiB at the limit).
const MAX_FIELDS: usize = 1 << 18;

#[derive(Debug, Default)]
pub(super) struct Message {
    fields: Vec<Field>,
}

impl Message {
    /// A staged cell: the one range checked against the file. Every nested range is
    /// bounded by the message that holds it.
    pub fn read(file: &File, at: u64, len: u64) -> Result<Self> {
        let end = at.checked_add(len).context("protobuf message range overflow")?;
        ensure!(end <= file.metadata()?.len(), "protobuf message outside staged cell");
        Self::parse(file, at, end)
    }

    fn parse(file: &File, mut at: u64, end: u64) -> Result<Self> {
        let mut fields = Vec::new();
        // Field headers are read a window at a time: a positioned syscall per window
        // that holds headers, not per field. Payloads are skipped, never read here.
        let mut window = [0u8; 512];
        let (mut held_at, mut held) = (0u64, 0usize);
        while at < end {
            // A descriptor is 24 bytes for as little as two bytes of input.
            ensure!(fields.len() < MAX_FIELDS, "protobuf message has too many fields");
            // Two maximum-width varints cover a tag and length.
            let n = (end - at).min(20) as usize;
            if at < held_at || at + n as u64 > held_at + held as u64 {
                held = (end - at).min(window.len() as u64) as usize;
                held_at = at;
                exact_at(file, &mut window[..held], at)?;
            }
            let header = &window[(at - held_at) as usize..][..n];
            let mut offset = 0;
            let tag = varint(header, &mut offset)?;
            let number = u32::try_from(tag >> 3).context("protobuf field number overflow")?;
            ensure!((1..=0x1fff_ffff).contains(&number), "invalid protobuf field number");
            let value = match tag & 7 {
                0 => Value::Varint(varint(header, &mut offset)?),
                1 => {
                    let bytes = header.get(offset..offset + 8).context("truncated protobuf fixed64")?;
                    offset += 8;
                    Value::Fixed64(u64::from_le_bytes(bytes.try_into()?))
                }
                2 => {
                    let len = varint(header, &mut offset)?;
                    let start = at
                        .checked_add(offset as u64)
                        .context("protobuf field offset overflow")?;
                    let next = start.checked_add(len).context("protobuf field length overflow")?;
                    ensure!(next <= end, "protobuf field exceeds enclosing message");
                    fields.push(Field {
                        number,
                        value: Value::Bytes { at: start, len },
                    });
                    at = next;
                    continue;
                }
                5 => {
                    let bytes = header.get(offset..offset + 4).context("truncated protobuf fixed32")?;
                    offset += 4;
                    Value::Fixed32(u32::from_le_bytes(bytes.try_into()?))
                }
                wire => anyhow::bail!("unsupported protobuf wire type {wire}"),
            };
            at = at
                .checked_add(offset as u64)
                .context("protobuf field offset overflow")?;
            ensure!(at <= end, "protobuf field exceeds enclosing message");
            fields.push(Field { number, value });
        }
        Ok(Self { fields })
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn last(&self, number: u32) -> Option<Field> {
        self.fields.iter().rev().find(|field| field.number == number).copied()
    }

    pub fn repeated(&self, number: u32) -> impl Iterator<Item = Field> + '_ {
        self.fields.iter().filter(move |field| field.number == number).copied()
    }
}

impl Field {
    pub fn u64(self) -> Option<u64> {
        match self.value {
            Value::Varint(value) | Value::Fixed64(value) => Some(value),
            Value::Fixed32(value) => Some(u64::from(value)),
            Value::Bytes { .. } => None,
        }
    }

    pub fn message(self, file: &File) -> Result<Message> {
        let Value::Bytes { at, len } = self.value else {
            anyhow::bail!("protobuf message field is not length-delimited")
        };
        Message::parse(
            file,
            at,
            at.checked_add(len).context("protobuf message range overflow")?,
        )
    }

    pub fn reader(self, file: &File) -> Result<RangeReader<'_>> {
        let Value::Bytes { at, len } = self.value else {
            anyhow::bail!("protobuf byte field is not length-delimited")
        };
        let end = at.checked_add(len).context("protobuf byte range overflow")?;
        Ok(RangeReader { file, at, end })
    }

    /// Scalar metadata only. Callers must name a maximum, and oversize input is
    /// refused rather than silently truncated. Body text uses the streaming writer.
    #[cfg(test)]
    pub fn string(self, file: &File, max_bytes: usize) -> Result<String> {
        let mut reader = self.reader(file)?;
        ensure!(
            reader.end - reader.at <= max_bytes as u64,
            "protobuf metadata string exceeds its limit"
        );
        let mut string = String::new();
        reader
            .read_to_string(&mut string)
            .context("invalid protobuf UTF-8 string")?;
        Ok(string)
    }

    /// Strict protobuf UTF-8, unlike a lossy terminal-output transport. A caller
    /// must discard its unpublished output if validation fails after a prefix.
    pub fn write_json_string(self, file: &File, out: &mut impl Write) -> Result<()> {
        struct Fragment;
        impl Formatter for Fragment {
            fn begin_string<W: ?Sized + Write>(&mut self, _: &mut W) -> io::Result<()> {
                Ok(())
            }
            fn end_string<W: ?Sized + Write>(&mut self, _: &mut W) -> io::Result<()> {
                Ok(())
            }
        }
        out.write_all(b"\"")?;
        let valid = crate::utf8::read(self.reader(file)?, crate::utf8::Verdict::Reject, |chunk| {
            chunk
                .serialize(&mut serde_json::Serializer::with_formatter(&mut *out, Fragment))
                .map_err(io::Error::other)
        })?;
        ensure!(valid, "invalid protobuf UTF-8 string");
        out.write_all(b"\"")?;
        Ok(())
    }
}

/// A reader over one staged range, checked against the file.
pub(super) fn range(file: &File, at: u64, len: u64) -> Result<RangeReader<'_>> {
    let end = at.checked_add(len).context("protobuf byte range overflow")?;
    ensure!(end <= file.metadata()?.len(), "protobuf bytes outside staged cell");
    Ok(RangeReader { file, at, end })
}

pub(super) struct RangeReader<'a> {
    file: &'a File,
    at: u64,
    end: u64,
}
impl Read for RangeReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = (self.end - self.at).min(bytes.len() as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        let got = crate::harness::store::bytes::read_at(self.file, &mut bytes[..n], self.at)?;
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated staged protobuf cell",
            ));
        }
        self.at += got as u64;
        Ok(got)
    }
}

fn exact_at(file: &File, mut bytes: &mut [u8], mut at: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = crate::harness::store::bytes::read_at(file, bytes, at)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated protobuf header",
            ));
        }
        at += n as u64;
        bytes = &mut bytes[n..];
    }
    Ok(())
}

fn varint(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*offset).context("truncated protobuf varint")?;
        *offset += 1;
        ensure!(shift != 63 || byte <= 1, "protobuf varint overflow");
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("protobuf varint overflow")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged_file(bytes: &[u8]) -> File {
        let source = crate::harness::store::bytes::ByteSource::arena().unwrap();
        source
            .append(|out| {
                out.write_all(bytes)?;
                Ok(())
            })
            .unwrap();
        source.file().unwrap()
    }

    #[test]
    fn repeated_fields_and_nested_ranges_preserve_wire_order() {
        let file = staged_file(&[8, 1, 18, 2, 8, 3, 8, 2, 26, 0, 37, 7, 0, 0, 0]);
        let message = Message::read(&file, 0, 15).unwrap();
        assert_eq!(message.fields().len(), 5);
        assert_eq!(message.repeated(1).filter_map(Field::u64).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(message.last(1).unwrap().u64(), Some(2));
        assert_eq!(
            message.last(2).unwrap().message(&file).unwrap().last(1).unwrap().u64(),
            Some(3)
        );
        assert!(message.last(3).unwrap().message(&file).unwrap().fields().is_empty());
        assert_eq!(message.last(4).unwrap().u64(), Some(7));
    }

    #[test]
    fn malformed_wire_ranges_and_varints_are_rejected() {
        for bytes in [
            vec![0],
            vec![8, 128],
            vec![10, 4, 1],
            vec![13, 1],
            vec![9, 1],
            vec![11],
            [vec![8], vec![255; 9], vec![2]].concat(),
        ] {
            let file = staged_file(&bytes);
            assert!(Message::read(&file, 0, bytes.len() as u64).is_err(), "{bytes:?}");
        }
        let many = [8, 1].repeat(MAX_FIELDS + 1);
        let file = staged_file(&many);
        assert_eq!(
            Message::read(&file, 0, 2 * MAX_FIELDS as u64).unwrap().fields().len(),
            MAX_FIELDS
        );
        assert!(Message::read(&file, 0, many.len() as u64).is_err());
        let file = staged_file(&[]);
        assert!(Message::read(&file, u64::MAX, 2).is_err());
        assert!(Message::read(&file, 0, 1).is_err());
    }

    #[test]
    fn streamed_strings_escape_and_reject_invalid_utf8() {
        let text = "λ🌙\"\\\n\t".repeat(10_000);
        let file = staged_file(text.as_bytes());
        let field = Field {
            number: 1,
            value: Value::Bytes {
                at: 0,
                len: text.len() as u64,
            },
        };
        let mut output = Vec::new();
        field.write_json_string(&file, &mut output).unwrap();
        assert_eq!(output, serde_json::to_vec(&text).unwrap());
        assert!(field.string(&file, 10).is_err());
        let invalid = staged_file(&[0xe2, 0x82]);
        let field = Field {
            number: 1,
            value: Value::Bytes { at: 0, len: 2 },
        };
        assert!(field.write_json_string(&invalid, &mut Vec::new()).is_err());
        assert!(field.string(&invalid, 10).is_err());
    }
}
