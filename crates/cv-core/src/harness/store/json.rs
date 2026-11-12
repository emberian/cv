//! Allocation-free checks that serde's ignored/raw values omit. Call only after JSON syntax
//! validation; these checks preserve Value's finite numbers, UTF-8/UTF-16 and depth acceptance.

/// JSON's insignificant whitespace (RFC 8259): space, tab, LF and CR, never form feed or a Unicode
/// space. Every reader of a JSONL line trims by exactly this, so a line is blank to all or to none.
pub(crate) fn is_whitespace(byte: &u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// `bytes` without the JSON whitespace around it.
pub(crate) fn trim(bytes: &[u8]) -> &[u8] {
    let lead = bytes.iter().take_while(|b| is_whitespace(b)).count();
    let tail = bytes[lead..].iter().rev().take_while(|b| is_whitespace(b)).count();
    &bytes[lead..bytes.len() - tail]
}

/// [`trim`] over text.
pub(crate) fn trim_str(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r'])
}

/// The bytes a validation pass reads: a slice in memory, or a positioned source read in windows
/// (`document::range::Windowed`). Indexes are relative to the pass's first byte.
pub(crate) trait ValidationBytes {
    fn get(&mut self, i: usize) -> Option<u8>;
    /// `len` bytes from `i`; None past the end (never at a window boundary).
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]>;
    /// Whether `i..end` is UTF-8 (a pass over `str` bytes knows it is).
    fn utf8(&mut self, i: usize, end: usize) -> bool;
    /// Whether the number token at `i..end` is accepted: as a finite `serde_json::Number`
    /// (`strict`), or by JSON grammar alone.
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool;
}
impl ValidationBytes for &[u8] {
    #[inline]
    fn get(&mut self, i: usize) -> Option<u8> {
        (**self).get(i).copied()
    }
    #[inline]
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]> {
        (**self).get(i..i.checked_add(len)?)
    }
    fn utf8(&mut self, i: usize, end: usize) -> bool {
        simdutf8::basic::from_utf8(&self[i..end]).is_ok()
    }
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool {
        number_accepted(&self[i..end], strict)
    }
}
/// `str` bytes: UTF-8 is known.
pub(crate) struct Text<'a>(pub &'a [u8]);
impl ValidationBytes for Text<'_> {
    #[inline]
    fn get(&mut self, i: usize) -> Option<u8> {
        self.0.get(i).copied()
    }
    #[inline]
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]> {
        self.0.get(i..i.checked_add(len)?)
    }
    fn utf8(&mut self, _: usize, _: usize) -> bool {
        true
    }
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool {
        number_accepted(&self.0[i..end], strict)
    }
}
pub(crate) fn number_accepted(token: &[u8], strict: bool) -> bool {
    if strict {
        serde_json::from_slice::<serde_json::Number>(token).is_ok()
    } else {
        serde_json::from_slice::<&serde_json::value::RawValue>(token)
            .is_ok_and(|raw| matches!(raw.get().as_bytes().first(), Some(b'-' | b'0'..=b'9')))
    }
}

/// One pass over a JSON value from any byte source, accepting exactly what
/// `serde_json::from_str::<Value>` accepts when `strict` (JSON grammar, strings without control
/// characters or invalid escapes including lone surrogates, finite numbers, Value's nesting
/// limit, nothing after the value), and what `RawValue` accepts otherwise (lone UTF-16 escapes
/// and out-of-range number tokens pass; grammar, control characters and UTF-8 still apply,
/// without Value's nesting limit).
/// `depth` levels are already open around the value. `span` sees the bounds of every string
/// and container, so a caller can index them for later structural selection.
/// Returns the value's bounds, or None when it is not accepted.
pub(crate) fn validate_value(
    b: &mut impl ValidationBytes,
    strict: bool,
    depth: usize,
    mut span: impl FnMut(u8, usize, usize),
) -> Option<(usize, usize)> {
    // Open containers: (start, is_object). Value rejects the 128th level; grammar alone has no
    // limit, so deeper levels spill to the heap only when a document actually nests that far.
    let mut open = [(0usize, false); 128];
    let mut deep: Vec<(usize, bool)> = Vec::new();
    let base = depth;
    let mut depth = 0;
    let mut i = skip_ws(b, 0);
    let first = i;
    loop {
        // A value starts here.
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b'{' | b'[') => {
                let object = b.get(i) == Some(b'{');
                match open.get_mut(depth) {
                    Some(slot) => *slot = (i, object),
                    None => deep.push((i, object)),
                }
                depth += 1;
                if strict && base + depth >= 128 {
                    return None;
                }
                i = skip_ws(b, i + 1);
                if b.get(i) == Some(if object { b'}' } else { b']' }) {
                    depth -= 1;
                    let start = match open.get(depth) {
                        Some(open) => open.0,
                        None => deep.pop()?.0,
                    };
                    span(if object { b'{' } else { b'[' }, start, i + 1);
                    i += 1;
                } else {
                    if object {
                        i = key(b, i, strict, &mut span)?;
                    }
                    continue;
                }
            }
            Some(b'"') => {
                let end = string(b, i, strict)?;
                span(b'"', i, end);
                i = end;
            }
            Some(b't') if b.slice(i, 4) == Some(b"true") => i += 4,
            Some(b'f') if b.slice(i, 5) == Some(b"false") => i += 5,
            Some(b'n') if b.slice(i, 4) == Some(b"null") => i += 4,
            Some(b'-' | b'0'..=b'9') => {
                let start = i;
                while b.get(i).is_some_and(|c| !is_whitespace(&c) && !b",]}".contains(&c)) {
                    i += 1;
                }
                if !b.number(start, i, strict) {
                    return None;
                }
            }
            _ => return None,
        }
        // A value ended: close containers until another value is due.
        loop {
            let end = i;
            i = skip_ws(b, i);
            if depth == 0 {
                return b.get(i).is_none().then_some((first, end));
            }
            let (start, object) = *open.get(depth - 1).unwrap_or_else(|| &deep[depth - 1 - open.len()]);
            match b.get(i) {
                Some(b',') => {
                    i = skip_ws(b, i + 1);
                    i = key_or_value(b, i, object, strict, &mut span)?;
                    break;
                }
                Some(b'}') if object => {}
                Some(b']') if !object => {}
                _ => return None,
            }
            depth -= 1;
            deep.truncate(depth.saturating_sub(open.len()));
            span(if object { b'{' } else { b'[' }, start, i + 1);
            i += 1;
        }
    }
}

/// After a comma: an object's next key and colon, or an array's next value start.
fn key_or_value(
    b: &mut impl ValidationBytes,
    i: usize,
    object: bool,
    strict: bool,
    span: &mut impl FnMut(u8, usize, usize),
) -> Option<usize> {
    if object {
        key(b, i, strict, span)
    } else {
        Some(i)
    }
}

fn skip_ws(b: &mut impl ValidationBytes, mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| is_whitespace(&c)) {
        i += 1;
    }
    i
}

/// An object key and its colon; the offset after the colon.
fn key(b: &mut impl ValidationBytes, i: usize, strict: bool, span: &mut impl FnMut(u8, usize, usize)) -> Option<usize> {
    if b.get(i) != Some(b'"') {
        return None;
    }
    let end = string(b, i, strict)?;
    span(b'"', i, end);
    let i = skip_ws(b, end);
    (b.get(i) == Some(b':')).then_some(i + 1)
}

/// The end of a valid JSON string starting at the quote `i`.
///
/// Scans 64 bytes per iteration with NEON (`aarch64`), runtime AVX2 (`x86_64`), or borrow-free
/// 8-byte SWAR (`wasm32`/`i686`), resolving odd/even backslash runs and common 2-byte escapes
/// (`\"`, `\\`, `\/`, `\b`, `\f`, `\n`, `\r`, `\t`) in parallel bitmasks without breaking out to
/// scalar code on every `\n`. Falls back to scalar validation only at `\uXXXX` escapes, invalid
/// escapes, or trailing `< 64`-byte tails.
fn string(b: &mut impl ValidationBytes, i: usize, strict: bool) -> Option<usize> {
    let mut j = i + 1;
    loop {
        while let Some(chunk) = b.slice(j, 64) {
            let chunk: &[u8; 64] = chunk.try_into().unwrap();
            let masks = classify_block(chunk);
            if (masks.bs | masks.qt | masks.ctrl) == 0 {
                j += 64;
                continue;
            }
            let (escaped, carry_out) = escaped_mask(masks.bs, 0);
            let unescaped_qt = masks.qt & !escaped;
            if unescaped_qt != 0 {
                let q_idx = unescaped_qt.trailing_zeros() as usize;
                let mask_before = if q_idx == 0 { 0 } else { (1u64 << q_idx) - 1 };
                if (masks.ctrl & mask_before) != 0 {
                    return None;
                }
                let active_bs_before = masks.bs & !escaped & mask_before;
                let bad_esc = (active_bs_before << 1) & !masks.simple_esc;
                if bad_esc == 0 {
                    let end = j + q_idx;
                    return b.utf8(i + 1, end).then_some(end + 1);
                }
                // Jump directly to the first `\u` or invalid escape before the closing quote.
                j += (bad_esc >> 1).trailing_zeros() as usize;
                break;
            }
            // No unescaped quote in this 64-byte block: any `< 0x20` byte is illegal in JSON.
            if masks.ctrl != 0 {
                return None;
            }
            let active_bs = masks.bs & !escaped;
            let active_bs_in = active_bs & !(1u64 << 63);
            let bad_esc = (active_bs_in << 1) & !masks.simple_esc;
            if bad_esc != 0 {
                j += (bad_esc >> 1).trailing_zeros() as usize;
                break;
            }
            if carry_out != 0 {
                // Byte 63 is an unescaped `\` straddling into byte 64; let scalar consume it once.
                j += 63;
                break;
            }
            j += 64;
        }
        // Scalar/SWAR tail or `\uXXXX` / invalid-escape handler at `j`.
        j = special(b, j);
        match b.get(j)? {
            b'"' => {
                return b.utf8(i + 1, j).then_some(j + 1);
            }
            b'\\' => match b.get(j + 1)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => j += 2,
                b'u' => {
                    let code = hex_quad(b.slice(j + 2, 4))?;
                    j += 6;
                    if !strict {
                        continue;
                    }
                    if (0xd800..=0xdbff).contains(&code) {
                        if b.slice(j, 2) != Some(b"\\u") {
                            return None;
                        }
                        let low = hex_quad(b.slice(j + 2, 4))?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return None;
                        }
                        j += 6;
                    } else if (0xdc00..=0xdfff).contains(&code) {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None, // A control character.
        }
    }
}

/// Bitmasks over a 64-byte slice (`bit k == 1` corresponds to `chunk[k]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockMasks {
    bs: u64,
    qt: u64,
    ctrl: u64,
    /// Valid 1-byte JSON escape targets (`\"`, `\\`, `\/`, `\b`, `\f`, `\n`, `\r`, `\t`).
    /// Only populated when `bs != 0`.
    simple_esc: u64,
}

/// Branchless odd/even backslash-run carry propagation (`simdjson` even/odd span arithmetic).
/// Given `bs` (`bit k == 1` iff `chunk[k] == b'\\'`) and `prev_escaped` (`0` or `1`, whether
/// `chunk[0]` is already escaped by a trailing unescaped backslash in the previous block), returns
/// `(escaped_mask, next_prev_escaped)`.
#[inline]
fn escaped_mask(bs: u64, prev_escaped: u64) -> (u64, u64) {
    const EVEN_BITS: u64 = 0x5555_5555_5555_5555;
    const ODD_BITS: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    let active_input = bs & !prev_escaped;
    if active_input == 0 {
        return (prev_escaped, 0);
    }
    let starts = active_input & !(active_input << 1);
    let starts_even = starts & EVEN_BITS;
    let starts_odd = starts & ODD_BITS;
    let (sum_even, _c_even) = active_input.overflowing_add(starts_even);
    let (sum_odd, c_odd) = active_input.overflowing_add(starts_odd);
    let span_even = active_input ^ sum_even;
    let span_odd = active_input ^ sum_odd;
    let escaped = prev_escaped | (span_even & ODD_BITS) | (span_odd & EVEN_BITS);
    (escaped, u64::from(c_odd))
}

// Nibble lookup tables matching exactly `{"\"\\/bfnrt"}`:
//   '"' = 0x22 (hi 2: 0x01, lo 2: 0x0D)
//   '/' = 0x2F (hi 2: 0x01, lo F: 0x01)
//   '\\' = 0x5C (hi 5: 0x02, lo C: 0x02)
//   'b' = 0x62 (hi 6: 0x04, lo 2: 0x0D)
//   'f' = 0x66 (hi 6: 0x04, lo 6: 0x04)
//   'n' = 0x6E (hi 6: 0x04, lo E: 0x04)
//   'r' = 0x72 (hi 7: 0x08, lo 2: 0x0D)
//   't' = 0x74 (hi 7: 0x08, lo 4: 0x08)
#[cfg(any(all(target_arch = "aarch64", target_endian = "little"), target_arch = "x86_64"))]
const SIMPLE_ESC_LO_TBL: [u8; 16] = [
    0x00, 0x00, 0x0D, 0x00, 0x08, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04, 0x01,
];
#[cfg(any(all(target_arch = "aarch64", target_endian = "little"), target_arch = "x86_64"))]
const SIMPLE_ESC_HI_TBL: [u8; 16] = [
    0x00, 0x00, 0x01, 0x00, 0x00, 0x02, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

#[inline]
fn classify_block(chunk: &[u8; 64]) -> BlockMasks {
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    {
        // SAFETY: NEON is baseline on aarch64.
        return unsafe { classify_block_neon(chunk) };
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: guarded by runtime AVX2 feature detection.
            return unsafe { classify_block_avx2(chunk) };
        }
    }
    #[allow(unreachable_code)]
    classify_block_swar(chunk)
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
unsafe fn neon_movemask_64(
    v0: std::arch::aarch64::uint8x16_t,
    v1: std::arch::aarch64::uint8x16_t,
    v2: std::arch::aarch64::uint8x16_t,
    v3: std::arch::aarch64::uint8x16_t,
) -> u64 {
    use std::arch::aarch64::*;
    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let bit_mask = unsafe { vld1q_u8(WEIGHTS.as_ptr()) };
    let t0 = vandq_u8(v0, bit_mask);
    let t1 = vandq_u8(v1, bit_mask);
    let t2 = vandq_u8(v2, bit_mask);
    let t3 = vandq_u8(v3, bit_mask);
    let sum0 = vpaddq_u8(t0, t1);
    let sum1 = vpaddq_u8(t2, t3);
    let sum = vpaddq_u8(sum0, sum1);
    let packed = vpaddq_u8(sum, sum);
    vgetq_lane_u64(vreinterpretq_u64_u8(packed), 0)
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
unsafe fn classify_block_neon(chunk: &[u8; 64]) -> BlockMasks {
    use std::arch::aarch64::*;
    let ptr = chunk.as_ptr();
    let v0 = unsafe { vld1q_u8(ptr) };
    let v1 = unsafe { vld1q_u8(ptr.add(16)) };
    let v2 = unsafe { vld1q_u8(ptr.add(32)) };
    let v3 = unsafe { vld1q_u8(ptr.add(48)) };

    let bs_byte = vdupq_n_u8(b'\\');
    let qt_byte = vdupq_n_u8(b'"');
    let sp_byte = vdupq_n_u8(0x20);

    let bs = unsafe {
        neon_movemask_64(
            vceqq_u8(v0, bs_byte),
            vceqq_u8(v1, bs_byte),
            vceqq_u8(v2, bs_byte),
            vceqq_u8(v3, bs_byte),
        )
    };
    let qt = unsafe {
        neon_movemask_64(
            vceqq_u8(v0, qt_byte),
            vceqq_u8(v1, qt_byte),
            vceqq_u8(v2, qt_byte),
            vceqq_u8(v3, qt_byte),
        )
    };
    let ctrl = unsafe {
        neon_movemask_64(
            vcltq_u8(v0, sp_byte),
            vcltq_u8(v1, sp_byte),
            vcltq_u8(v2, sp_byte),
            vcltq_u8(v3, sp_byte),
        )
    };
    let simple_esc = if bs == 0 {
        0
    } else {
        let lo_tbl = unsafe { vld1q_u8(SIMPLE_ESC_LO_TBL.as_ptr()) };
        let hi_tbl = unsafe { vld1q_u8(SIMPLE_ESC_HI_TBL.as_ptr()) };
        let low_nibble = vdupq_n_u8(0x0F);
        let classify16 = |v: uint8x16_t| {
            let lo = vqtbl1q_u8(lo_tbl, vandq_u8(v, low_nibble));
            let hi = vqtbl1q_u8(hi_tbl, vshrq_n_u8(v, 4));
            let m = vandq_u8(lo, hi);
            vtstq_u8(m, m)
        };
        unsafe { neon_movemask_64(classify16(v0), classify16(v1), classify16(v2), classify16(v3)) }
    };
    BlockMasks {
        bs,
        qt,
        ctrl,
        simple_esc,
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn classify_block_avx2(chunk: &[u8; 64]) -> BlockMasks {
    use std::arch::x86_64::*;
    let ptr = chunk.as_ptr();
    let v0 = unsafe { _mm256_loadu_si256(ptr.cast::<__m256i>()) };
    let v1 = unsafe { _mm256_loadu_si256(ptr.add(32).cast::<__m256i>()) };

    let bs_vec = _mm256_set1_epi8(b'\\' as i8);
    let qt_vec = _mm256_set1_epi8(b'"' as i8);
    let hi3_vec = _mm256_set1_epi8(0xE0u8 as i8);
    let zero = _mm256_setzero_si256();

    let mask64 = |m0: __m256i, m1: __m256i| -> u64 {
        let lo = _mm256_movemask_epi8(m0) as u32 as u64;
        let hi = _mm256_movemask_epi8(m1) as u32 as u64;
        lo | (hi << 32)
    };

    let bs = mask64(_mm256_cmpeq_epi8(v0, bs_vec), _mm256_cmpeq_epi8(v1, bs_vec));
    let qt = mask64(_mm256_cmpeq_epi8(v0, qt_vec), _mm256_cmpeq_epi8(v1, qt_vec));
    let ctrl = mask64(
        _mm256_cmpeq_epi8(_mm256_and_si256(v0, hi3_vec), zero),
        _mm256_cmpeq_epi8(_mm256_and_si256(v1, hi3_vec), zero),
    );
    let simple_esc = if bs == 0 {
        0
    } else {
        let lo_tbl = _mm256_broadcastsi128_si256(unsafe { _mm_loadu_si128(SIMPLE_ESC_LO_TBL.as_ptr().cast()) });
        let hi_tbl = _mm256_broadcastsi128_si256(unsafe { _mm_loadu_si128(SIMPLE_ESC_HI_TBL.as_ptr().cast()) });
        let nibble_mask = _mm256_set1_epi8(0x0F);
        let classify32 = |v: __m256i| -> u32 {
            let lo = _mm256_shuffle_epi8(lo_tbl, _mm256_and_si256(v, nibble_mask));
            let hi = _mm256_shuffle_epi8(hi_tbl, _mm256_and_si256(_mm256_srli_epi16(v, 4), nibble_mask));
            let is_zero = _mm256_cmpeq_epi8(_mm256_and_si256(lo, hi), zero);
            !(_mm256_movemask_epi8(is_zero) as u32)
        };
        (classify32(v0) as u64) | ((classify32(v1) as u64) << 32)
    };
    BlockMasks {
        bs,
        qt,
        ctrl,
        simple_esc,
    }
}

/// Borrow-free portable 8-byte SWAR fallback (`wasm32`, `i686`, non-AVX2 `x86_64`, and test oracle).
#[inline]
fn classify_block_swar(chunk: &[u8; 64]) -> BlockMasks {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const LOW7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    const PACK: u64 = 0x0102_0408_1020_4080;

    #[inline]
    fn zero_bytes_mask8(x: u64) -> u8 {
        let non_zero_high = ((x & LOW7).wrapping_add(LOW7)) | x | LOW7;
        let zero_high = !non_zero_high;
        (((zero_high >> 7).wrapping_mul(PACK)) >> 56) as u8
    }

    let mut bs = 0u64;
    let mut qt = 0u64;
    let mut ctrl = 0u64;
    for lane in 0..8 {
        let w = u64::from_le_bytes(chunk[lane * 8..lane * 8 + 8].try_into().unwrap());
        let shift = lane * 8;
        bs |= (zero_bytes_mask8(w ^ (ONES * b'\\' as u64)) as u64) << shift;
        qt |= (zero_bytes_mask8(w ^ (ONES * b'"' as u64)) as u64) << shift;
        ctrl |= (zero_bytes_mask8(w & 0xE0E0_E0E0_E0E0_E0E0) as u64) << shift;
    }
    let mut simple_esc = 0u64;
    if bs != 0 {
        for (idx, &byte) in chunk.iter().enumerate() {
            if matches!(byte, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                simple_esc |= 1u64 << idx;
            }
        }
    }
    BlockMasks {
        bs,
        qt,
        ctrl,
        simple_esc,
    }
}

/// Whether `bytes` contains an unescaped JSON string terminator/invalid byte (`"` or `< 0x20`).
#[inline]
fn has_quote_or_control(mut bytes: &[u8]) -> bool {
    while bytes.len() >= 64 {
        let chunk: &[u8; 64] = bytes[..64].try_into().unwrap();
        let masks = classify_block(chunk);
        if (masks.qt | masks.ctrl) != 0 {
            return true;
        }
        bytes = &bytes[64..];
    }
    bytes.iter().any(|&b| b == b'"' || b < 0x20)
}

/// The first quote, backslash or control byte at or after `j`, eight bytes at a time.
fn special(b: &mut impl ValidationBytes, mut j: usize) -> usize {
    const ONES: u64 = u64::from_ne_bytes([1; 8]);
    const HIGH: u64 = u64::from_ne_bytes([0x80; 8]);
    let zero = |x: u64| x.wrapping_sub(ONES) & !x & HIGH;
    while let Some(chunk) = b.slice(j, 8) {
        let w = u64::from_le_bytes(chunk.try_into().unwrap());
        let found = zero(w ^ (ONES * b'"' as u64))
            | zero(w ^ (ONES * b'\\' as u64))
            | (w.wrapping_sub(ONES * 0x20) & !w & HIGH);
        if found != 0 {
            return j + (found.trailing_zeros() / 8) as usize;
        }
        j += 8;
    }
    while b.get(j).is_some_and(|c| c != b'"' && c != b'\\' && c >= 0x20) {
        j += 1;
    }
    j
}

fn hex_quad(b: Option<&[u8]>) -> Option<u16> {
    b?.iter()
        .try_fold(0u16, |value, c| Some(value * 16 + char::from(*c).to_digit(16)? as u16))
}

// The one JSON string-body codec: a body is the bytes between a string's quotes, read through
// any `BufRead` in bounded windows (a borrowed slice, a mapped span, a positioned range).

const BODY_WINDOW: usize = 32 * 1024;

/// Decode a body in bounded windows; at most one incomplete escape or UTF-8 scalar carries into
/// the next window.
///
/// Fast path: while windows contain no backslash (`memchr(b'\\', ..).is_none()`), validates UTF-8
/// via `simdutf8` and emits zero-copy `&str` slices directly from `reader`'s buffer without
/// allocating `vec![b'"', ...]` or running `serde_json::Deserializer`. Transitions seamlessly into
/// the escaped window decoder at the first backslash.
///
/// Stops early when `f` returns false (Ok after a stop has not validated the unread suffix).
/// Returns whether the body contained an escape. Callbacks may run before a later malformed window
/// or read error returns `Err`: callers discard or reset what they accumulated on failure.
pub(crate) fn decode_body(reader: &mut impl std::io::BufRead, f: &mut impl FnMut(&str) -> bool) -> Result<bool, ()> {
    use serde::Deserializer;

    let mut carry = [0u8; 4];
    let mut carry_len = 0usize;

    loop {
        if carry_len > 0 {
            let avail = reader.fill_buf().map_err(|_| ())?;
            if avail.is_empty() {
                // Dangling incomplete multi-byte UTF-8 scalar at EOF.
                return Err(());
            }
            let b0 = avail[0];
            if b0 == b'"' || b0 < 0x20 {
                return Err(());
            }
            carry[carry_len] = b0;
            carry_len += 1;
            reader.consume(1);
            match std::str::from_utf8(&carry[..carry_len]) {
                Ok(text) => {
                    carry_len = 0;
                    if !f(text) {
                        return Ok(false);
                    }
                }
                Err(e) if e.error_len().is_some() => return Err(()),
                Err(_) => {}
            }
            continue;
        }

        let avail = reader.fill_buf().map_err(|_| ())?;
        if avail.is_empty() {
            return Ok(false);
        }
        let avail_len = avail.len();
        let capped = avail_len > BODY_WINDOW;
        let window = &avail[..avail_len.min(BODY_WINDOW)];

        if memchr::memchr(b'\\', window).is_some() {
            return json_body_windows(reader, &mut |quoted| {
                let mut decoder = serde_json::Deserializer::from_slice(quoted);
                let keep_going = decoder.deserialize_str(JsonWindow(f)).map_err(|_| ())?;
                decoder.end().map_err(|_| ())?;
                Ok(keep_going)
            });
        }

        if has_quote_or_control(window) {
            return Err(());
        }

        match simdutf8::compat::from_utf8(window) {
            Ok(text) => {
                let n = window.len();
                let keep_going = f(text);
                reader.consume(n);
                if !keep_going {
                    return Ok(false);
                }
            }
            Err(error) => {
                if error.error_len().is_some() {
                    return Err(());
                }
                let valid = error.valid_up_to();
                let tail_bytes = window.len() - valid;
                let mut tail_tmp = [0u8; 4];
                if !capped {
                    tail_tmp[..tail_bytes].copy_from_slice(&window[valid..]);
                }
                if valid > 0 {
                    // SAFETY: `simdutf8::compat::from_utf8` verified `window[..valid]` is valid UTF-8.
                    let text = unsafe { std::str::from_utf8_unchecked(&window[..valid]) };
                    let keep_going = f(text);
                    reader.consume(valid);
                    if !keep_going {
                        return Ok(false);
                    }
                }
                if !capped {
                    // Underlying BufRead ended mid-scalar; carry the 1..=3 lead bytes across fill_buf().
                    carry[..tail_bytes].copy_from_slice(&tail_tmp[..tail_bytes]);
                    carry_len = tail_bytes;
                    reader.consume(tail_bytes);
                }
            }
        }
    }
}

fn json_body_windows(
    reader: &mut impl std::io::BufRead,
    visit: &mut impl FnMut(&[u8]) -> Result<bool, ()>,
) -> Result<bool, ()> {
    // serde decodes an escaped window into its own scratch, so a decode holds two windows.
    const WINDOW: usize = BODY_WINDOW;
    // One buffer: an opening quote, then at most one window of body. Its capacity follows what
    // the reader offers, so a short body costs its length and a long one one window.
    let mut buf = vec![b'"'];
    let mut has_escape = false;
    loop {
        while buf.len() - 1 < WINDOW {
            let bytes = reader.fill_buf().map_err(|_| ())?;
            if bytes.is_empty() {
                break;
            }
            let n = (WINDOW + 1 - buf.len()).min(bytes.len());
            // n fills at most WINDOW body bytes after the opening quote; the extra byte
            // reserves its closing quote. Thus needed <= WINDOW + 2 and needed > buf.len().
            let needed = buf.len() + n + 1;
            if needed > buf.capacity() {
                buf.reserve_exact(needed.max(2 * buf.capacity()).min(WINDOW + 2) - buf.len());
            }
            buf.extend_from_slice(&bytes[..n]);
            reader.consume(n);
        }
        let raw = &buf[1..];
        let eof = raw.len() < WINDOW;
        if raw.is_empty() {
            return Ok(has_escape);
        }
        let mut end = if eof {
            raw.len()
        } else {
            complete_json_window_prefix(raw)
        };
        if !eof && raw[end - 1] >= 0x80 {
            let mut tail = end.saturating_sub(4);
            while tail > 0 && (raw[tail] & 0xc0) == 0x80 {
                tail -= 1;
            }
            if let Err(e) = std::str::from_utf8(&raw[tail..end]) {
                if e.error_len().is_none() {
                    end = tail + e.valid_up_to();
                }
            }
        }
        has_escape |= raw[..end].contains(&b'\\');
        // Close the quote in place over the first carried byte, then restore it.
        let close = end + 1;
        let carried = buf.get(close).copied();
        match carried {
            Some(_) => buf[close] = b'"',
            None => buf.push(b'"'),
        }
        let keep_going = visit(&buf[..close + 1]);
        match carried {
            Some(byte) => buf[close] = byte,
            None => {
                buf.pop();
            }
        }
        if !keep_going? {
            return Ok(has_escape);
        }
        buf.drain(1..close);
        if eof {
            return Ok(has_escape);
        }
    }
}

/// The callback borrows serde's decode scratch; neither pass needs an owned decoded String.
struct JsonWindow<'a, F>(&'a mut F);

impl<'de, F: FnMut(&str) -> bool> serde::de::Visitor<'de> for JsonWindow<'_, F> {
    type Value = bool;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON string window")
    }

    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<bool, E> {
        Ok((self.0)(s))
    }
}

/// Only the last escape can cross a window cut. Twelve bytes cover a surrogate pair;
/// back up over a preceding backslash run so the tail scanner retains its escape parity.
fn complete_json_window_prefix(raw: &[u8]) -> usize {
    let mut start = raw.len().saturating_sub(12);
    while start > 0 && raw[start - 1] == b'\\' {
        start -= 1;
    }
    start + complete_escape_prefix(&raw[start..])
}

/// The longest prefix of `raw` (a JSON string body) that ends on a *complete* escape sequence —
/// i.e. doesn't cut a `\x` or `\uXXXX` in half. Truncating to this length keeps the prefix
/// parseable: the window decoder cuts there so an escape never spans two windows.
pub(crate) fn complete_escape_prefix(raw: &[u8]) -> usize {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] != b'\\' {
            let Some(next) = memchr::memchr(b'\\', &raw[i..]) else {
                break;
            };
            i += next;
        }
        // An escape starts here: 2 bytes (`\n`, `\"`, …), 6 (`\uXXXX`), or 12 (a `\uD8xx\uDCxx`
        // surrogate pair — a cut between the halves would leave an unparseable lone surrogate).
        if raw.get(i + 1) == Some(&b'u') {
            if i + 6 > raw.len() {
                return i; // incomplete at the cut — keep everything before it
            }
            let high_surrogate = std::str::from_utf8(&raw[i + 2..i + 6])
                .ok()
                .and_then(|h| u16::from_str_radix(h, 16).ok())
                .is_some_and(|h| (0xD800..0xDC00).contains(&h));
            let needed = if high_surrogate { 12 } else { 6 };
            if i + needed > raw.len() {
                return i;
            }
            i += needed;
        } else {
            if i + 2 > raw.len() {
                return i;
            }
            i += 2;
        }
    }
    raw.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_decoder_stops_after_one_bounded_window() {
        use std::io::Cursor;
        let body = "\\ud83c\\udf19\\n".repeat(100_000);
        // Independent full-value acceptance establishes the stoppable primitive's precondition.
        let expected: String = serde_json::from_str(&format!("\"{body}\"")).unwrap();
        let mut reader = Cursor::new(body.as_bytes());
        let mut output = String::new();
        decode_body(&mut reader, &mut |text| {
            output.push_str(text);
            false
        })
        .unwrap();
        assert!(reader.position() <= 65_536);
        assert!(reader.position() < body.len() as u64);
        assert!(!output.is_empty());
        assert!(expected.starts_with(&output));
        let mut full_reader = Cursor::new(body.as_bytes());
        let mut full = String::new();
        decode_body(&mut full_reader, &mut |text| {
            full.push_str(text);
            true
        })
        .unwrap();
        assert_eq!(full, expected);
        assert_eq!(full_reader.position(), body.len() as u64);
    }

    #[test]
    fn json_window_tail_matches_complete_escape_scan() {
        let bodies = [
            r#"plain\\\\\\\"\n\u0041\uD834\uDD1Etail"#.to_owned(),
            format!("{}uD834\\uDD1E", "\\".repeat(101)),
            format!("{}uD834\\uDD1E", "\\".repeat(100)),
        ];
        for body in bodies {
            for end in 0..=body.len() {
                assert_eq!(
                    complete_json_window_prefix(&body.as_bytes()[..end]),
                    complete_escape_prefix(&body.as_bytes()[..end]),
                    "{body:?}, cut {end}"
                );
            }
        }
    }

    #[test]
    fn complete_escape_prefix_never_splits_an_escape() {
        // Plain text: the whole slice survives.
        assert_eq!(complete_escape_prefix(b"hello"), 5);
        // Complete escapes survive whole.
        assert_eq!(complete_escape_prefix(b"a\\nb"), 4);
        assert_eq!(complete_escape_prefix(b"a\\u0041b"), 8);
        // A trailing half escape is cut *before* the backslash.
        assert_eq!(complete_escape_prefix(b"abc\\"), 3);
        assert_eq!(complete_escape_prefix(b"abc\\u00"), 3);
        // A high surrogate keeps its partner or is dropped entirely.
        assert_eq!(complete_escape_prefix(b"x\\uD834\\uDD1E"), 13);
        assert_eq!(complete_escape_prefix(b"x\\uD834"), 1);
        assert_eq!(complete_escape_prefix(b"x\\uD834\\uDD"), 1);
        // …and every kept prefix actually unescapes cleanly.
        for raw in [&b"abc\\"[..], b"abc\\u00", b"x\\uD834", b"a\\nb\\"] {
            let keep = complete_escape_prefix(raw);
            let requoted = [b"\"", &raw[..keep], b"\""].concat();
            assert!(
                serde_json::from_slice::<String>(&requoted).is_ok(),
                "prefix of {raw:?} must stay parseable"
            );
        }
    }

    #[test]
    fn preflight_matches_value_validation() {
        let mut cases: Vec<String> = [
            "0",
            "-0",
            "1.5",
            "1e-400",
            "1e400",
            "18446744073709551616",
            "-99999999999999999999999",
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud83c\udf19""#,
            r#""\\ud800""#,
            r#""\\\ud800""#,
            r#"{"\ud800":0}"#,
            r#"{"a":1e400,"a":0}"#,
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        cases.push("9".repeat(1000));
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
            cases.push(format!("{{\"a\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)));
        }
        for input in cases {
            let expected = serde_json::from_str::<serde_json::Value>(&input).is_ok();
            assert_eq!(
                validate_value(&mut input.as_bytes(), true, 0, |_, _, _| {}).is_some(),
                expected,
                "bytes: {input}"
            );
            assert_eq!(
                validate_value(&mut Text(input.as_bytes()), true, 0, |_, _, _| {}).is_some(),
                expected,
                "text: {input}"
            );
            // Grammar alone accepts what RawValue accepts.
            assert_eq!(
                validate_value(&mut Text(input.as_bytes()), false, 0, |_, _, _| {}).is_some(),
                serde_json::from_str::<&crate::lazy::RawValue>(&input).is_ok(),
                "syntax: {input}"
            );
        }
    }

    /// The one-pass validator is the record acceptance boundary: it must agree with Value on
    /// every input, including near-misses of valid documents.
    #[test]
    fn validate_value_accepts_exactly_what_value_accepts() {
        let mut cases: Vec<String> = [
            "",
            " ",
            "0",
            "-0",
            "01",
            "-",
            "1.",
            ".5",
            "1e",
            "1e+",
            "1E5",
            "-1.5e-3",
            "1e400",
            "18446744073709551616",
            "true",
            "tru",
            "truex",
            "null",
            "nul",
            "false ",
            " \t\r\n{}\n",
            "{}",
            "[]",
            "[,]",
            "[1,]",
            "{,}",
            r#"{"a":}"#,
            r#"{"a"}"#,
            r#"{"a":1,}"#,
            r#"{"a":1 "b":2}"#,
            r#"{1:2}"#,
            r#"[1 2]"#,
            "[1}",
            "{]",
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud83c\udf19""#,
            r#""\\ud800""#,
            r#""\\\ud800""#,
            r#""\ud800\u0041""#,
            r#""\x""#,
            r#""\u12""#,
            r#""\u12g4""#,
            "\"tab\tinside\"",
            "\"nl\ninside\"",
            "\"\u{7f}\"",
            "\"é🌙\"",
            r#"{"\ud800":0}"#,
            r#"{"a":1e400,"a":0}"#,
            r#"{"a":1,"a":2}"#,
            "[true,false,null]",
            "[]]",
            "[[]",
            "\"",
            r#""unterminated"#,
            r#""a"x"#,
            "{} {}",
            "[1] x",
            "\u{a0}1",
            "[-]",
            "[--1]",
            "[1.5.5]",
            "[0x10]",
            "[+1]",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        cases.push("9".repeat(1000));
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
            cases.push(format!("{{\"a\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)));
        }
        // Mutations of a realistic record: every single-byte deletion, and replacements by bytes
        // that matter to the grammar.
        let record = r#"{"type":"user","n":-12.5e2,"ok":true,"z":null,"m":{"c":[{"t":"a\"b\\c\u00e9\ud83c\udf19","x":[1,2,[]]}],"e":{}}}"#;
        for at in 0..record.len() {
            let mut deleted = record.as_bytes().to_vec();
            deleted.remove(at);
            cases.extend(String::from_utf8(deleted).ok());
            for byte in [
                b'"', b'\\', b',', b':', b'{', b'}', b'[', b']', b' ', b'0', b'e', b'-', b'u', 0x01,
            ] {
                let mut replaced = record.as_bytes().to_vec();
                replaced[at] = byte;
                cases.extend(String::from_utf8(replaced).ok());
            }
        }
        for input in cases {
            let oracle = serde_json::from_str::<serde_json::Value>(&input).is_ok();
            let bounds = validate_value(&mut Text(input.as_bytes()), true, 0, |_, _, _| {});
            assert_eq!(bounds.is_some(), oracle, "{input:?}");
            if let Some((start, end)) = bounds {
                assert_eq!(&input[start..end], trim_str(&input), "{input:?}");
            }
            // A child value validated with its parents' levels open sees the same limit.
            let nested = format!("[{input}]");
            assert_eq!(
                validate_value(&mut Text(input.as_bytes()), true, 1, |_, _, _| {}).is_some(),
                serde_json::from_str::<serde_json::Value>(&nested).is_ok() && oracle,
                "nested {input:?}"
            );
        }
    }

    #[test]
    fn rejects_non_utf8_ignored_text() {
        assert!(validate_value(&mut &b"{\"ignored\":\"\xff\"}"[..], true, 0, |_, _, _| {}).is_none());
        assert!(validate_value(&mut &b"{\"ignored\":\"\xff\"}"[..], false, 0, |_, _, _| {}).is_none());
    }

    #[test]
    fn simd_and_swar_block_classifiers_match_oracle_for_all_bytes_and_lanes() {
        let oracle = |chunk: &[u8; 64]| -> BlockMasks {
            let mut bs = 0u64;
            let mut qt = 0u64;
            let mut ctrl = 0u64;
            for (i, &b) in chunk.iter().enumerate() {
                if b == b'\\' {
                    bs |= 1u64 << i;
                }
                if b == b'"' {
                    qt |= 1u64 << i;
                }
                if b < 0x20 {
                    ctrl |= 1u64 << i;
                }
            }
            let mut simple_esc = 0u64;
            if bs != 0 {
                for (i, &b) in chunk.iter().enumerate() {
                    if matches!(b, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                        simple_esc |= 1u64 << i;
                    }
                }
            }
            BlockMasks {
                bs,
                qt,
                ctrl,
                simple_esc,
            }
        };

        // Test every byte 0x00..=0xFF at every lane 0..64, both without and with a backslash present.
        for byte in 0u8..=255 {
            for pos in 0..64 {
                for with_bs in [false, true] {
                    let mut chunk = [b'a'; 64];
                    if with_bs {
                        chunk[(pos + 31) % 64] = b'\\';
                    }
                    chunk[pos] = byte;
                    let expected = oracle(&chunk);
                    assert_eq!(classify_block_swar(&chunk), expected, "swar byte={byte:#x} pos={pos}");
                    assert_eq!(classify_block(&chunk), expected, "simd byte={byte:#x} pos={pos}");
                }
            }
        }
    }

    #[test]
    fn escaped_mask_matches_scalar_oracle_across_backslash_runs_and_carries() {
        let scalar_escaped = |bs: u64, mut esc: bool| -> (u64, u64) {
            let mut out = 0u64;
            for i in 0..64 {
                let is_bs = ((bs >> i) & 1) != 0;
                if esc {
                    out |= 1u64 << i;
                    esc = false;
                } else if is_bs {
                    esc = true;
                }
            }
            (out, u64::from(esc))
        };

        for prev in [0u64, 1u64] {
            for start in 0..64 {
                for len in 0..=(64 - start) {
                    let bs = if len == 0 {
                        0
                    } else if len == 64 {
                        u64::MAX
                    } else {
                        ((1u64 << len) - 1) << start
                    };
                    assert_eq!(
                        escaped_mask(bs, prev),
                        scalar_escaped(bs, prev != 0),
                        "prev={prev} start={start} len={len}"
                    );
                }
            }
            let mut state = 0x9E37_79B9_7F4A_7C15u64;
            for _ in 0..5_000 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Mix dense and sparse backslash patterns.
                for bs in [state, state & (state >> 1), state | (state << 1)] {
                    assert_eq!(escaped_mask(bs, prev), scalar_escaped(bs, prev != 0));
                }
            }
        }
    }

    #[test]
    fn vectorized_scanner_matches_serde_across_64b_and_32kib_escape_boundaries() {
        let mut cases = Vec::new();
        // Dense `\n` and `\"` transcript strings spanning multiple 64-byte SIMD blocks.
        cases.push(format!("\"{}\"", "hello\\nworld\\\"tab\\tend\\\\slash\\/".repeat(300)));

        // Odd and even backslash runs straddling every 64-byte boundary (0..=135) with various suffixes.
        let suffixes = [
            "\"",
            "n\"",
            "x\"",
            "u0041\"",
            "ud800\"",
            "udc00\"",
            "ud83c\\udf19\"",
            "ud83c\\u0041\"",
            "\n\"",
        ];
        for pad in [0, 1, 31, 32, 61, 62, 63, 64, 65, 127, 128] {
            for slashes in [0, 1, 2, 3, 4, 63, 64, 65, 66, 127, 128, 129] {
                for suffix in suffixes {
                    let s = format!(
                        "{{\"k\":\"{}{}{}\",\"n\":1}}",
                        "a".repeat(pad),
                        "\\".repeat(slashes),
                        suffix
                    );
                    cases.push(s);
                }
            }
        }
        // Straddle 32 KiB window boundary too.
        for pad in [32_760, 32_766, 32_767, 32_768, 32_769] {
            for slashes in [1, 2, 3, 64, 65] {
                cases.push(format!("\"{}{}n\"", "x".repeat(pad), "\\".repeat(slashes)));
                cases.push(format!("\"{}{}ud800\"", "x".repeat(pad), "\\".repeat(slashes)));
            }
        }

        for input in &cases {
            let expected_strict = serde_json::from_str::<serde_json::Value>(input).is_ok();
            let expected_raw = serde_json::from_str::<&crate::lazy::RawValue>(input).is_ok();
            assert_eq!(
                validate_value(&mut input.as_bytes(), true, 0, |_, _, _| {}).is_some(),
                expected_strict,
                "strict bytes mismatch len={}",
                input.len()
            );
            assert_eq!(
                validate_value(&mut input.as_bytes(), false, 0, |_, _, _| {}).is_some(),
                expected_raw,
                "raw bytes mismatch len={}",
                input.len()
            );
        }

        // Invalid UTF-8 after >64 bytes in a skipped field is still rejected in both modes.
        let mut bad_utf8 = br#"{"skipped":""#.to_vec();
        bad_utf8.extend(std::iter::repeat_n(b'a', 200));
        bad_utf8.extend_from_slice(b"\\n\\t");
        bad_utf8.extend_from_slice(b"\xf0\x9f\xff\",\"ok\":1}");
        assert!(validate_value(&mut &bad_utf8[..], true, 0, |_, _, _| {}).is_none());
        assert!(validate_value(&mut &bad_utf8[..], false, 0, |_, _, _| {}).is_none());
    }

    #[test]
    fn decode_body_unescaped_fast_path_and_hybrid_escape_parity() {
        use std::io::{BufReader, Cursor};

        // 1. Multi-window unescaped UTF-8 body stops after <= 1 bounded 32 KiB window.
        let unescaped = "plain-λ-雪-🦀-0123456789-".repeat(4_000);
        assert!(!unescaped.contains('\\') && unescaped.len() > 90_000);
        let mut stop_cursor = Cursor::new(unescaped.as_bytes());
        let mut first_piece = String::new();
        let had_esc = decode_body(&mut stop_cursor, &mut |part| {
            first_piece.push_str(part);
            false
        })
        .unwrap();
        assert!(!had_esc);
        assert!(stop_cursor.position() <= BODY_WINDOW as u64);
        assert!(!first_piece.is_empty() && unescaped.starts_with(&first_piece));

        // 2. Arbitrary BufReader capacities splitting 2/3/4-byte UTF-8 scalars and escapes.
        let hybrid = format!(
            "{}λ雪🦀{}\\n\\\"\\uD834\\uDD1E{}",
            "a".repeat(65),
            "β".repeat(16_400),
            "🦀".repeat(50)
        );
        let expected_hybrid: String = serde_json::from_str(&format!("\"{hybrid}\"")).unwrap();

        for cap in [1usize, 2, 3, 7, 63, 64, 65, 32_767, 32_768, 32_769] {
            let mut reader = BufReader::with_capacity(cap, Cursor::new(unescaped.as_bytes()));
            let mut out = String::with_capacity(unescaped.len());
            let esc = decode_body(&mut reader, &mut |part| {
                out.push_str(part);
                true
            })
            .unwrap();
            assert!(!esc, "cap={cap}");
            assert_eq!(out, unescaped, "unescaped mismatch at cap={cap}");

            let mut h_reader = BufReader::with_capacity(cap, Cursor::new(hybrid.as_bytes()));
            let mut h_out = String::with_capacity(expected_hybrid.len());
            let h_esc = decode_body(&mut h_reader, &mut |part| {
                h_out.push_str(part);
                true
            })
            .unwrap();
            assert!(h_esc, "hybrid cap={cap}");
            assert_eq!(h_out, expected_hybrid, "hybrid mismatch at cap={cap}");
        }

        // 3. Rejects unescaped quote, control bytes (< 0x20), invalid UTF-8, and dangling UTF-8 tail.
        for bad in [
            &b"unescaped\"quote"[..],
            &b"raw\nnewline"[..],
            &b"raw\x00nul"[..],
            &b"bad\xffutf8"[..],
            &b"dangling\xf0\x9f\xa6"[..],
        ] {
            let mut r = BufReader::with_capacity(3, Cursor::new(bad));
            assert!(decode_body(&mut r, &mut |_| true).is_err(), "accepted bad={bad:?}");
        }
    }
}
