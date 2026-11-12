//! Zstandard single-frame stream decoding for external compressed sources (Codex rollouts, Zed
//! and OpenClaw blobs) AND Zstandard Seekable Format multi-frame archives (`packs/`).
//!
//! External single-frame `.zst` inputs continue to decode exactly one frame (`decoder`), while
//! sealed multi-record packs write ~2 MiB independent uncompressed frames followed by a trailing
//! `0x184D2A5E` skippable seek table with 32-bit truncated XXH64 frame checksums (`SeekableWriter`,
//! `FrameIndex`, `SeekableReader`).
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Zstandard standard frame magic (`RFC 8878 §3.1.1`).
pub const ZSTD_FRAME_MAGIC: u32 = 0xFD2F_B528;
/// Skippable frame magic reserved by the Zstandard Seekable Format specification.
pub const SEEKABLE_SKIPPABLE_MAGIC: u32 = 0x184D_2A5E;
/// Footer magic ending the 9-byte Zstandard Seekable Format seek table footer.
pub const SEEKABLE_FOOTER_MAGIC: u32 = 0x8F92_EAB1;
/// Default uncompressed frame target (~2 MiB) for seekable multi-record packs.
pub const DEFAULT_SEEKABLE_FRAME_SIZE: usize = 2 * 1024 * 1024;
/// Defensive upper bound (64 MiB) on any single frame's declared uncompressed size in an
/// untrusted seek table so a forged header cannot trigger multi-GiB allocations.
pub const MAX_SEEKABLE_FRAME_SIZE: u32 = 64 * 1024 * 1024;

const SEEK_DESCRIPTOR_CHECKSUM_FLAG: u8 = 0x80;
const SEEK_DESCRIPTOR_RESERVED_MASK: u8 = 0x03;
const SEEK_FOOTER_LEN: u64 = 9;
const SKIPPABLE_HEADER_LEN: u64 = 8;

/// A reader over the first zstd frame of `input`. A malformed header fails here, as ruzstd's
/// constructor did, so callers keep telling "not zstd" apart from a failure mid-stream.
#[cfg(all(feature = "zstd", not(target_family = "wasm")))]
pub(crate) fn decoder<R: Read>(input: R) -> io::Result<impl Read> {
    use std::io::BufRead;
    let mut decoded =
        io::BufReader::with_capacity(super::IO_CHUNK, zstd::stream::read::Decoder::new(input)?.single_frame());
    decoded.fill_buf()?;
    Ok(decoded)
}

#[cfg(all(feature = "zstd", target_family = "wasm"))]
pub(crate) fn decoder<R: Read>(input: R) -> io::Result<impl Read> {
    ruzstd::decoding::StreamingDecoder::new(input).map_err(io::Error::other)
}

/// A disabled decoder is an explicit failed demand, never compressed bytes presented as JSONL.
#[cfg(not(feature = "zstd"))]
pub(crate) fn decoder<R: Read>(_: R) -> io::Result<std::io::Empty> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "compressed sources require the `zstd` feature",
    ))
}

/// The decoded size the first frame header of `path` declares (RFC 8878 §3.1.1.1), if any.
/// An unverified size hint only; callers must capture/check the source before trusting its data.
pub(crate) fn content_size(path: &std::path::Path) -> Option<u64> {
    let mut head = [0u8; 18];
    let mut file = std::fs::File::open(path).ok()?;
    let mut len = 0;
    while len < head.len() {
        match file.read(&mut head[len..]).ok()? {
            0 => break,
            n => len += n,
        }
    }
    frame_content_size(&head[..len])
}

pub(crate) fn frame_content_size(head: &[u8]) -> Option<u64> {
    if head.get(..4)? != ZSTD_FRAME_MAGIC.to_le_bytes() {
        return None;
    }
    let descriptor = *head.get(4)?;
    let single_segment = descriptor & 0x20 != 0;
    let at = 5 + usize::from(!single_segment) + [0, 1, 2, 4][usize::from(descriptor & 3)];
    let size = match descriptor >> 6 {
        0 if single_segment => 1,
        0 => return None,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut value = [0u8; 8];
    value[..size].copy_from_slice(head.get(at..at + size)?);
    Some(u64::from_le_bytes(value) + if size == 2 { 256 } else { 0 })
}

/// Pure-Rust XXH64 (`seed = 0` in the Zstandard Seekable Format specification).
pub fn xxh64(input: &[u8], seed: u64) -> u64 {
    const P1: u64 = 0x9E37_79B1_85EB_CA87;
    const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    const P3: u64 = 0x1656_67B1_9E37_79F9;
    const P4: u64 = 0x85EB_CA77_C2B2_AE63;
    const P5: u64 = 0x27D4_EB2F_1656_67C5;

    #[inline]
    fn round(acc: u64, lane: u64) -> u64 {
        acc.wrapping_add(lane.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
    }

    #[inline]
    fn merge_round(acc: u64, val: u64) -> u64 {
        (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
    }

    let mut rem = input;
    let mut h64 = if rem.len() >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while rem.len() >= 32 {
            v1 = round(v1, u64::from_le_bytes(rem[0..8].try_into().unwrap()));
            v2 = round(v2, u64::from_le_bytes(rem[8..16].try_into().unwrap()));
            v3 = round(v3, u64::from_le_bytes(rem[16..24].try_into().unwrap()));
            v4 = round(v4, u64::from_le_bytes(rem[24..32].try_into().unwrap()));
            rem = &rem[32..];
        }
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge_round(h, v1);
        h = merge_round(h, v2);
        h = merge_round(h, v3);
        merge_round(h, v4)
    } else {
        seed.wrapping_add(P5)
    };

    h64 = h64.wrapping_add(input.len() as u64);

    while rem.len() >= 8 {
        let k1 = round(0, u64::from_le_bytes(rem[..8].try_into().unwrap()));
        h64 = (h64 ^ k1).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        rem = &rem[8..];
    }
    if rem.len() >= 4 {
        let k1 = (u32::from_le_bytes(rem[..4].try_into().unwrap()) as u64).wrapping_mul(P1);
        h64 = (h64 ^ k1).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rem = &rem[4..];
    }
    for &b in rem {
        h64 = (h64 ^ ((b as u64).wrapping_mul(P5))).rotate_left(11).wrapping_mul(P1);
    }

    h64 ^= h64 >> 33;
    h64 = h64.wrapping_mul(P2);
    h64 ^= h64 >> 29;
    h64 = h64.wrapping_mul(P3);
    h64 ^= h64 >> 32;
    h64
}

/// Low 32 bits of `XXH64(input, 0)` per the Zstandard Seekable Format checksum specification.
#[inline]
pub fn xxh64_low32(input: &[u8]) -> u32 {
    xxh64(input, 0) as u32
}

/// Compress a single independent Zstandard frame with no cross-frame history.
pub fn compress_independent_frame(uncompressed: &[u8], level: i32) -> io::Result<Vec<u8>> {
    #[cfg(all(feature = "zstd", not(target_family = "wasm")))]
    {
        zstd::bulk::compress(uncompressed, level)
    }
    #[cfg(all(feature = "zstd", target_family = "wasm"))]
    {
        let _ = level;
        Ok(ruzstd::encoding::compress_to_vec(
            uncompressed,
            ruzstd::encoding::CompressionLevel::Fastest,
        ))
    }
    #[cfg(not(feature = "zstd"))]
    {
        let _ = (uncompressed, level);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "zstd frame compression requires the `zstd` feature",
        ))
    }
}

/// Decompress a single independent Zstandard frame of known `expected_decoded_size`, failing closed
/// if the frame header is invalid, truncated, or expands past `expected_decoded_size`.
pub fn decompress_independent_frame(compressed: &[u8], expected_decoded_size: u32) -> io::Result<Vec<u8>> {
    if expected_decoded_size > MAX_SEEKABLE_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("seekable frame uncompressed size {expected_decoded_size} exceeds {MAX_SEEKABLE_FRAME_SIZE} limit"),
        ));
    }
    #[cfg(feature = "zstd")]
    {
        let expected = expected_decoded_size as usize;
        let mut reader = decoder(compressed)?.take((expected as u64).saturating_add(1));
        let mut out = Vec::with_capacity(expected);
        reader.read_to_end(&mut out)?;
        if out.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("seekable frame decoded {} bytes, expected {}", out.len(), expected),
            ));
        }
        Ok(out)
    }
    #[cfg(not(feature = "zstd"))]
    {
        let _ = compressed;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "zstd frame decompression requires the `zstd` feature",
        ))
    }
}

/// One compressed frame's physical and logical coordinates in a Zstandard Seekable archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameEntry {
    pub compressed_offset: u64,
    pub compressed_size: u32,
    pub decoded_offset: u64,
    pub decoded_size: u32,
    pub checksum_xxh64_low32: Option<u32>,
}

/// `decoded_offset -> compressed_frame` index parsed from (or serialized to) the Zstandard Seekable
/// Format `0x184D2A5E` skippable seek table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameIndex {
    frames: Vec<FrameEntry>,
    total_compressed_len: u64,
    total_decoded_len: u64,
    has_checksums: bool,
}

impl FrameIndex {
    pub fn frames(&self) -> &[FrameEntry] {
        &self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn total_compressed_len(&self) -> u64 {
        self.total_compressed_len
    }

    pub fn total_decoded_len(&self) -> u64 {
        self.total_decoded_len
    }

    pub fn has_checksums(&self) -> bool {
        self.has_checksums
    }

    /// Locate the frame containing `decoded_offset` in `O(log N)`.
    pub fn frame_for_offset(&self, decoded_offset: u64) -> Option<usize> {
        if decoded_offset >= self.total_decoded_len {
            return None;
        }
        let idx = self
            .frames
            .partition_point(|f| f.decoded_offset.saturating_add(f.decoded_size as u64) <= decoded_offset);
        (idx < self.frames.len()).then_some(idx)
    }

    /// Compute the half-open frame range `[start_frame..end_frame)` intersecting
    /// `[decoded_offset .. decoded_offset + length)` using checked bounds validation.
    pub fn frames_for_range(&self, decoded_offset: u64, length: u64) -> io::Result<std::ops::Range<usize>> {
        let end_offset = decoded_offset
            .checked_add(length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "decoded range overflow"))?;
        if end_offset > self.total_decoded_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "decoded range {decoded_offset}..{end_offset} exceeds archive decoded length {}",
                    self.total_decoded_len
                ),
            ));
        }
        if length == 0 {
            return Ok(0..0);
        }
        // Skip empty 0-byte frames cleanly: find first frame with `decoded_end > decoded_offset`.
        let first = self
            .frames
            .partition_point(|f| f.decoded_offset.saturating_add(f.decoded_size as u64) <= decoded_offset);
        // Include every frame with `decoded_offset < end_offset` and `decoded_size > 0`.
        let last_exclusive = self.frames.partition_point(|f| f.decoded_offset < end_offset);
        Ok(first..last_exclusive)
    }

    /// Encode this `FrameIndex` as a trailing `0x184D2A5E` skippable seek table frame.
    pub fn encode_seek_table(&self) -> io::Result<Vec<u8>> {
        let num_frames = u32::try_from(self.frames.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many seekable frames for u32 table"))?;
        let entry_size: u64 = if self.has_checksums { 12 } else { 8 };
        let entries_bytes = (num_frames as u64)
            .checked_mul(entry_size)
            .and_then(|n| n.checked_add(SEEK_FOOTER_LEN))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek table size overflow"))?;
        let frame_size = u32::try_from(entries_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek table exceeds 4 GiB skippable frame limit",
            )
        })?;
        let total = usize::try_from(SKIPPABLE_HEADER_LEN + entries_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek table allocation overflow"))?;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&SEEKABLE_SKIPPABLE_MAGIC.to_le_bytes());
        out.extend_from_slice(&frame_size.to_le_bytes());
        for frame in &self.frames {
            out.extend_from_slice(&frame.compressed_size.to_le_bytes());
            out.extend_from_slice(&frame.decoded_size.to_le_bytes());
            if self.has_checksums {
                let csum = frame.checksum_xxh64_low32.unwrap_or(0);
                out.extend_from_slice(&csum.to_le_bytes());
            }
        }
        out.extend_from_slice(&num_frames.to_le_bytes());
        let descriptor = if self.has_checksums {
            SEEK_DESCRIPTOR_CHECKSUM_FLAG
        } else {
            0u8
        };
        out.push(descriptor);
        out.extend_from_slice(&SEEKABLE_FOOTER_MAGIC.to_le_bytes());
        Ok(out)
    }

    /// Parse and strictly validate a trailing `0x184D2A5E` Zstandard Seekable Format seek table
    /// from any positioned reader (`read_exact_at`) of total byte length `total_len`.
    pub fn parse_untrusted<F>(total_len: u64, mut read_exact_at: F) -> io::Result<Self>
    where
        F: FnMut(u64, &mut [u8]) -> io::Result<()>,
    {
        if total_len < SKIPPABLE_HEADER_LEN + SEEK_FOOTER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive shorter than minimum 17-byte zstd seekable footer",
            ));
        }
        let mut footer = [0u8; 9];
        read_exact_at(total_len - SEEK_FOOTER_LEN, &mut footer)?;
        let magic = u32::from_le_bytes(footer[5..9].try_into().unwrap());
        if magic != SEEKABLE_FOOTER_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("missing zstd seekable footer magic (got {magic:#010x})"),
            ));
        }
        let descriptor = footer[4];
        if (descriptor & SEEK_DESCRIPTOR_RESERVED_MASK) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("non-zero reserved bits in seekable descriptor {descriptor:#04x}"),
            ));
        }
        let has_checksums = (descriptor & SEEK_DESCRIPTOR_CHECKSUM_FLAG) != 0;
        let entry_size: u64 = if has_checksums { 12 } else { 8 };
        let num_frames = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as u64;

        let entries_len = num_frames
            .checked_mul(entry_size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "seek table entries size overflow"))?;
        let payload_len = entries_len
            .checked_add(SEEK_FOOTER_LEN)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "seek table payload size overflow"))?;
        let declared_payload_u32 = u32::try_from(payload_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "seek table payload exceeds u32"))?;
        let skippable_total = payload_len
            .checked_add(SKIPPABLE_HEADER_LEN)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "seek table skippable size overflow"))?;
        let skippable_offset = total_len.checked_sub(skippable_total).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "seek table claims more bytes than archive length",
            )
        })?;

        let mut header = [0u8; 8];
        read_exact_at(skippable_offset, &mut header)?;
        let skippable_magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if skippable_magic != SEEKABLE_SKIPPABLE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad seekable skippable frame magic {skippable_magic:#010x}"),
            ));
        }
        let frame_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if frame_size != declared_payload_u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("skippable frame size {frame_size} mismatches footer payload {declared_payload_u32}"),
            ));
        }

        let entries_usize = usize::try_from(entries_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "seek table entries exceed address space"))?;
        let mut table_bytes = vec![0u8; entries_usize];
        if entries_usize > 0 {
            read_exact_at(skippable_offset + SKIPPABLE_HEADER_LEN, &mut table_bytes)?;
        }

        let count_usize = usize::try_from(num_frames)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame count exceeds usize"))?;
        let mut frames = Vec::with_capacity(count_usize);
        let mut comp_offset = 0u64;
        let mut dec_offset = 0u64;
        let step = entry_size as usize;

        for chunk in table_bytes.chunks_exact(step) {
            let compressed_size = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let decoded_size = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            if decoded_size > MAX_SEEKABLE_FRAME_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame decoded_size {decoded_size} exceeds {MAX_SEEKABLE_FRAME_SIZE} limit"),
                ));
            }
            if decoded_size > 0 && compressed_size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "non-empty seekable frame has zero compressed_size",
                ));
            }
            let checksum_xxh64_low32 = has_checksums.then(|| u32::from_le_bytes(chunk[8..12].try_into().unwrap()));
            let next_comp = comp_offset
                .checked_add(compressed_size as u64)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "compressed offset overflow"))?;
            if next_comp > skippable_offset {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame compressed_size overruns seek table offset",
                ));
            }
            let next_dec = dec_offset
                .checked_add(decoded_size as u64)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "decoded offset overflow"))?;
            frames.push(FrameEntry {
                compressed_offset: comp_offset,
                compressed_size,
                decoded_offset: dec_offset,
                decoded_size,
                checksum_xxh64_low32,
            });
            comp_offset = next_comp;
            dec_offset = next_dec;
        }

        if comp_offset != skippable_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cumulative compressed frame bytes ({comp_offset}) != seek table offset ({skippable_offset})"),
            ));
        }

        Ok(Self {
            frames,
            total_compressed_len: comp_offset,
            total_decoded_len: dec_offset,
            has_checksums,
        })
    }

    /// Parse from an in-memory slice (`wasm32` / mapped pack).
    pub fn parse_slice(archive: &[u8]) -> io::Result<Self> {
        Self::parse_untrusted(archive.len() as u64, |offset, out| {
            let start = usize::try_from(offset)
                .map_err(|_| io::Error::new(io::ErrorKind::UnexpectedEof, "offset out of range"))?;
            let end = start
                .checked_add(out.len())
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "slice end overflow"))?;
            let slice = archive
                .get(start..end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated archive slice"))?;
            out.copy_from_slice(slice);
            Ok(())
        })
    }
}

/// Streaming writer producing a standard Zstandard Seekable Format archive (~2 MiB independent
/// frames + trailing `0x184D2A5E` skippable seek table with 32-bit truncated XXH64 checksums).
pub struct SeekableWriter<W> {
    inner: W,
    frame_buf: Vec<u8>,
    max_frame_size: usize,
    level: i32,
    frames: Vec<FrameEntry>,
    compressed_offset: u64,
    decoded_offset: u64,
}

impl<W: Write> SeekableWriter<W> {
    pub fn new(inner: W) -> Self {
        Self::with_options(inner, DEFAULT_SEEKABLE_FRAME_SIZE, 3)
    }

    pub fn with_options(inner: W, max_frame_size: usize, level: i32) -> Self {
        let clamped = max_frame_size.clamp(1024, MAX_SEEKABLE_FRAME_SIZE as usize);
        Self {
            inner,
            frame_buf: Vec::with_capacity(clamped.min(64 * 1024)),
            max_frame_size: clamped,
            level,
            frames: Vec::new(),
            compressed_offset: 0,
            decoded_offset: 0,
        }
    }

    pub fn decoded_offset(&self) -> u64 {
        self.decoded_offset + self.frame_buf.len() as u64
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        if self.frame_buf.is_empty() {
            return Ok(());
        }
        let decoded_size = u32::try_from(self.frame_buf.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds u32"))?;
        let checksum = xxh64_low32(&self.frame_buf);
        let compressed = compress_independent_frame(&self.frame_buf, self.level)?;
        let compressed_size = u32::try_from(compressed.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "compressed frame exceeds u32"))?;
        self.inner.write_all(&compressed)?;

        let next_comp = self
            .compressed_offset
            .checked_add(compressed_size as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "compressed archive offset overflow"))?;
        let next_dec = self
            .decoded_offset
            .checked_add(decoded_size as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "decoded archive offset overflow"))?;

        self.frames.push(FrameEntry {
            compressed_offset: self.compressed_offset,
            compressed_size,
            decoded_offset: self.decoded_offset,
            decoded_size,
            checksum_xxh64_low32: Some(checksum),
        });
        self.compressed_offset = next_comp;
        self.decoded_offset = next_dec;
        self.frame_buf.clear();
        Ok(())
    }

    /// Seal the final frame (if non-empty), write the trailing `0x184D2A5E` skippable seek table,
    /// and return `(writer, FrameIndex)`.
    pub fn finish(mut self) -> io::Result<(W, FrameIndex)> {
        self.flush_frame()?;
        let index = FrameIndex {
            frames: self.frames,
            total_compressed_len: self.compressed_offset,
            total_decoded_len: self.decoded_offset,
            has_checksums: true,
        };
        let seek_table = index.encode_seek_table()?;
        self.inner.write_all(&seek_table)?;
        self.inner.flush()?;
        Ok((self.inner, index))
    }
}

impl<W: Write> Write for SeekableWriter<W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let total = buf.len();
        while !buf.is_empty() {
            let room = self.max_frame_size - self.frame_buf.len();
            let take = room.min(buf.len());
            self.frame_buf.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            if self.frame_buf.len() == self.max_frame_size {
                self.flush_frame()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Random-access range reader over a Zstandard Seekable Format archive, decoding ONLY the
/// compressed frames intersecting each requested `(decoded_offset, length)` range and verifying
/// each frame's truncated XXH64 checksum.
pub struct SeekableReader<S> {
    source: S,
    index: FrameIndex,
    frames_decoded: AtomicU64,
}

/// Positioned byte-slice access shared by `File` (`read_at`), `Mmap`, and `&[u8]`.
pub trait PositionedRead {
    fn byte_len(&self) -> io::Result<u64>;
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()>;
}

impl PositionedRead for [u8] {
    fn byte_len(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::UnexpectedEof, "slice offset overflow"))?;
        let end = start
            .checked_add(out.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "slice end overflow"))?;
        let slice = self
            .get(start..end)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "slice read past EOF"))?;
        out.copy_from_slice(slice);
        Ok(())
    }
}

impl PositionedRead for &[u8] {
    fn byte_len(&self) -> io::Result<u64> {
        (**self).byte_len()
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        (**self).read_exact_at(offset, out)
    }
}

impl PositionedRead for Vec<u8> {
    fn byte_len(&self) -> io::Result<u64> {
        self.as_slice().byte_len()
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_exact_at(offset, out)
    }
}

impl PositionedRead for std::fs::File {
    fn byte_len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn read_exact_at(&self, mut offset: u64, mut out: &mut [u8]) -> io::Result<()> {
        while !out.is_empty() {
            match super::bytes::read_at(self, out, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "seekable archive truncated during positioned read",
                    ));
                }
                Ok(n) => {
                    offset = offset.saturating_add(n as u64);
                    out = &mut out[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[cfg(feature = "mmap")]
impl PositionedRead for memmap2::Mmap {
    fn byte_len(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self[..].read_exact_at(offset, out)
    }
}

impl<S: PositionedRead> SeekableReader<S> {
    pub fn open(source: S) -> io::Result<Self> {
        let len = source.byte_len()?;
        let index = FrameIndex::parse_untrusted(len, |off, buf| source.read_exact_at(off, buf))?;
        Ok(Self {
            source,
            index,
            frames_decoded: AtomicU64::new(0),
        })
    }

    pub fn index(&self) -> &FrameIndex {
        &self.index
    }

    /// Number of compressed frames decompressed through this reader so far.
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded.load(Ordering::Relaxed)
    }

    /// Decode and verify a single frame by index.
    pub fn decode_frame(&self, frame_idx: usize) -> io::Result<Vec<u8>> {
        let entry = self
            .index
            .frames
            .get(frame_idx)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame index out of bounds"))?;
        if entry.decoded_size == 0 {
            return Ok(Vec::new());
        }
        let mut comp = vec![0u8; entry.compressed_size as usize];
        self.source.read_exact_at(entry.compressed_offset, &mut comp)?;
        let decoded = decompress_independent_frame(&comp, entry.decoded_size)?;
        self.frames_decoded.fetch_add(1, Ordering::Relaxed);
        if let Some(expected_csum) = entry.checksum_xxh64_low32 {
            let actual_csum = xxh64_low32(&decoded);
            if actual_csum != expected_csum {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "seekable frame {frame_idx} XXH64 checksum mismatch: got {actual_csum:#010x}, expected {expected_csum:#010x}"
                    ),
                ));
            }
        }
        Ok(decoded)
    }

    /// Read `[decoded_offset .. decoded_offset + length)` by decompressing ONLY the intersecting
    /// independent frames.
    pub fn read_range(&self, decoded_offset: u64, length: u64) -> io::Result<Vec<u8>> {
        let frame_range = self.index.frames_for_range(decoded_offset, length)?;
        let total_len = usize::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "requested range exceeds usize"))?;
        if total_len == 0 {
            return Ok(Vec::new());
        }
        let range_end = decoded_offset + length;
        let mut out = Vec::with_capacity(total_len);
        for frame_idx in frame_range {
            let entry = self.index.frames[frame_idx];
            if entry.decoded_size == 0 {
                continue;
            }
            let frame_bytes = self.decode_frame(frame_idx)?;
            let frame_end = entry.decoded_offset + entry.decoded_size as u64;
            let copy_start = decoded_offset.saturating_sub(entry.decoded_offset) as usize;
            let copy_end = (range_end.min(frame_end) - entry.decoded_offset) as usize;
            out.extend_from_slice(&frame_bytes[copy_start..copy_end]);
        }
        debug_assert_eq!(out.len(), total_len);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxh64_matches_official_reference_vectors() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"", 2_654_435_761), 0xAC75_FDA2_929B_17EF);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        assert_eq!(
            xxh64(b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ", 0),
            0x7639_D419_DE61_4EED
        );

        // Cross-verify our pure-Rust `xxh64_low32` against libzstd's C `XXH64` frame checksum
        // across every stripe/tail boundary (0..=300 bytes and multi-KiB).
        #[cfg(all(feature = "zstd", not(target_family = "wasm")))]
        for len in (0..=140).chain([222, 511, 512, 513, 4096]) {
            let data: Vec<u8> = (0..len).map(|i| ((i * 73 + 19) & 0xFF) as u8).collect();
            let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
            enc.include_checksum(true).unwrap();
            std::io::Write::write_all(&mut enc, &data).unwrap();
            let frame = enc.finish().unwrap();
            let libzstd_csum = u32::from_le_bytes(frame[frame.len() - 4..].try_into().unwrap());
            assert_eq!(
                xxh64_low32(&data),
                libzstd_csum,
                "xxh64 mismatch vs libzstd at len={len}"
            );
        }
    }

    #[cfg(all(feature = "zstd", not(target_family = "wasm")))]
    #[test]
    fn frame_headers_declare_their_content_size() {
        for len in [0usize, 1, 255, 256, 300, 65_791, 65_792, 1 << 20] {
            let text = vec![b'x'; len];
            assert_eq!(
                super::frame_content_size(&zstd::bulk::compress(&text, 1).unwrap()),
                Some(len as u64)
            );
        }
        let mut streamed = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        std::io::Write::write_all(&mut streamed, b"unknown size").unwrap();
        assert_eq!(super::frame_content_size(&streamed.finish().unwrap()), None);
        assert_eq!(super::frame_content_size(b"{\"type\":\"session_meta\"}"), None);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn seekable_multi_frame_range_reads_decode_only_intersecting_frames_and_keep_single_frame_compat() {
        // 10 frames of 4 KiB each = 40 KiB total uncompressed.
        let mut raw = Vec::with_capacity(40 * 1024);
        for i in 0..(40 * 1024) {
            raw.push(((i * 37 + 11) & 0xFF) as u8);
        }

        let mut writer = SeekableWriter::with_options(Vec::new(), 4096, 1);
        writer.write_all(&raw[..10_000]).unwrap();
        writer.write_all(&raw[10_000..]).unwrap();
        let (archive, written_idx) = writer.finish().unwrap();
        assert_eq!(written_idx.len(), 10);
        assert_eq!(written_idx.total_decoded_len(), raw.len() as u64);

        // Single-frame `decoder()` on the archive still cleanly reads frame 0 without breaking.
        let mut frame0 = Vec::new();
        decoder(archive.as_slice()).unwrap().read_to_end(&mut frame0).unwrap();
        assert_eq!(frame0, &raw[..4096]);

        // Random-access `SeekableReader` decodes ONLY the 1 frame containing bytes 20_500..20_600.
        let reader = SeekableReader::open(archive.as_slice()).unwrap();
        assert_eq!(reader.index(), &written_idx);
        let slice_one = reader.read_range(20_500, 100).unwrap();
        assert_eq!(slice_one, &raw[20_500..20_600]);
        assert_eq!(reader.frames_decoded(), 1, "must decode only the 1 intersecting frame");

        // Straddling frame boundary 8190..8200 (frames 1 and 2) decodes exactly 2 more frames.
        let slice_straddle = reader.read_range(8190, 10).unwrap();
        assert_eq!(slice_straddle, &raw[8190..8200]);
        assert_eq!(reader.frames_decoded(), 3);

        // Corrupted frame payload triggers XXH64 checksum rejection.
        let mut corrupted = archive.clone();
        // Flip a byte in frame 5's stored XXH64 checksum in the seek table:
        let csum_offset = corrupted.len() - 9 - (10 - 5) * 12 + 8;
        corrupted[csum_offset] ^= 0xFF;
        let bad_reader = SeekableReader::open(corrupted.as_slice()).unwrap();
        // Reading frame 2 still succeeds without touching corrupted frame 5!
        assert_eq!(bad_reader.read_range(8200, 32).unwrap(), &raw[8200..8232]);
        // Reading frame 5 fails closed on checksum mismatch!
        let err = bad_reader.read_range(5 * 4096 + 10, 32).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum") || err.to_string().contains("decoded"));
    }

    #[test]
    fn untrusted_seek_table_rejects_overflows_reserved_bits_and_mismatched_offsets() {
        // 1. Empty 0-frame seekable archive is valid.
        let empty_idx = FrameIndex {
            frames: Vec::new(),
            total_compressed_len: 0,
            total_decoded_len: 0,
            has_checksums: true,
        };
        let empty_bytes = empty_idx.encode_seek_table().unwrap();
        assert_eq!(FrameIndex::parse_slice(&empty_bytes).unwrap(), empty_idx);

        // 2. Non-zero reserved bits in descriptor must fail closed.
        let mut bad_desc = empty_bytes.clone();
        let desc_idx = bad_desc.len() - 5;
        bad_desc[desc_idx] |= 0x01;
        assert_eq!(
            FrameIndex::parse_slice(&bad_desc).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        // 3. `num_frames = u32::MAX` must fail cleanly via checked arithmetic without OOM/panic.
        let mut huge_count = empty_bytes.clone();
        let count_idx = huge_count.len() - 9;
        huge_count[count_idx..count_idx + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            FrameIndex::parse_slice(&huge_count).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        // 4. Forged frame entry claiming `decoded_size > MAX_SEEKABLE_FRAME_SIZE` is rejected.
        let mut forged = Vec::new();
        forged.extend_from_slice(&[0u8; 16]); // fake compressed frame
        forged.extend_from_slice(&SEEKABLE_SKIPPABLE_MAGIC.to_le_bytes());
        forged.extend_from_slice(&(12u32 + 9).to_le_bytes());
        forged.extend_from_slice(&16u32.to_le_bytes()); // compressed_size
        forged.extend_from_slice(&(MAX_SEEKABLE_FRAME_SIZE + 1).to_le_bytes()); // excessive decoded_size
        forged.extend_from_slice(&0u32.to_le_bytes()); // checksum
        forged.extend_from_slice(&1u32.to_le_bytes()); // num_frames = 1
        forged.push(SEEK_DESCRIPTOR_CHECKSUM_FLAG);
        forged.extend_from_slice(&SEEKABLE_FOOTER_MAGIC.to_le_bytes());
        assert_eq!(
            FrameIndex::parse_slice(&forged).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
