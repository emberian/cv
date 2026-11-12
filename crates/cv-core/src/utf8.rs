//! UTF-8 decoding over byte windows, spelled once: valid text is handed on as `&str` pieces, a
//! scalar split across windows is carried (at most three bytes), and an invalid sequence gets
//! the caller's [`Verdict`]: replaced exactly as `String::from_utf8_lossy` replaces it, or the
//! whole input rejected.
use std::io::{self, Read};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    /// One U+FFFD per invalid subsequence (`from_utf8_lossy`'s groups), including a dangling tail.
    Replace,
    /// Invalid input is rejected; pieces already handed on must be discarded by the caller.
    Reject,
}

/// The decoder state between windows.
pub(crate) struct Chunks {
    carry: [u8; 4],
    len: usize,
    verdict: Verdict,
}
impl Chunks {
    pub(crate) fn new(verdict: Verdict) -> Self {
        Self {
            carry: [0; 4],
            len: 0,
            verdict,
        }
    }
    /// Decode one window. `Ok(false)` is a rejection (only under [`Verdict::Reject`]).
    pub(crate) fn feed<E>(&mut self, mut bytes: &[u8], f: &mut impl FnMut(&str) -> Result<(), E>) -> Result<bool, E> {
        // Complete a scalar carried from the previous window one byte at a time.
        while self.len > 0 && !bytes.is_empty() {
            self.carry[self.len] = bytes[0];
            self.len += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.carry[..self.len]) {
                Ok(text) => {
                    f(text)?;
                    self.len = 0;
                }
                Err(error) => match error.error_len() {
                    None => {}
                    Some(invalid) => {
                        if self.verdict == Verdict::Reject {
                            return Ok(false);
                        }
                        f("\u{fffd}")?;
                        // The bytes after the invalid group start the stream again.
                        let rest = self.carry;
                        let (start, end) = (invalid, self.len);
                        self.len = 0;
                        if !self.feed(&rest[start..end], f)? {
                            return Ok(false);
                        }
                    }
                },
            }
        }
        while !bytes.is_empty() {
            match simdutf8::compat::from_utf8(bytes) {
                Ok(text) => {
                    f(text)?;
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // SAFETY: `simdutf8::compat::from_utf8` verified `bytes[..valid]` is valid UTF-8.
                        f(unsafe { std::str::from_utf8_unchecked(&bytes[..valid]) })?;
                    }
                    match error.error_len() {
                        Some(invalid) => {
                            if self.verdict == Verdict::Reject {
                                return Ok(false);
                            }
                            f("\u{fffd}")?;
                            bytes = &bytes[valid + invalid..];
                        }
                        None => {
                            let tail = &bytes[valid..];
                            self.carry[..tail.len()].copy_from_slice(tail);
                            self.len = tail.len();
                            break;
                        }
                    }
                }
            }
        }
        Ok(true)
    }
    /// End of input: a dangling partial scalar is one replacement, or a rejection.
    pub(crate) fn finish<E>(&mut self, f: &mut impl FnMut(&str) -> Result<(), E>) -> Result<bool, E> {
        if self.len == 0 {
            return Ok(true);
        }
        self.len = 0;
        if self.verdict == Verdict::Reject {
            return Ok(false);
        }
        f("\u{fffd}")?;
        Ok(true)
    }
}

/// A whole reader through [`Chunks`] in 64 KiB windows; `Ok(false)` is a rejection.
pub(crate) fn read<E: From<io::Error>>(
    mut input: impl Read,
    verdict: Verdict,
    mut f: impl FnMut(&str) -> Result<(), E>,
) -> Result<bool, E> {
    let mut chunks = Chunks::new(verdict);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = match input.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(E::from)?,
        };
        if n == 0 {
            return chunks.finish(&mut f);
        }
        if !chunks.feed(&buffer[..n], &mut f)? {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every split of every sample decodes as `from_utf8_lossy` (Replace) and is accepted exactly
    /// when the bytes are UTF-8 (Reject).
    #[test]
    fn chunks_are_from_utf8_lossy_at_every_split() {
        let samples: &[&[u8]] = &[
            b"",
            "plain λ 雪 🦀".as_bytes(),
            b"\xff",
            b"a\xe2\x82",
            b"\xe2\x82a\xf0\x9f\xa6",
            b"\xf0\x9f\xa6\x80\xc0\xafz",
            b"\xed\xa0\x80x",
            b"\xe2\x82\xe2\x82\xac",
        ];
        for bytes in samples {
            let expected = String::from_utf8_lossy(bytes);
            for split in 0..=bytes.len() {
                for second in split..=bytes.len() {
                    for verdict in [Verdict::Replace, Verdict::Reject] {
                        let mut out = String::new();
                        let mut push = |s: &str| -> Result<(), ()> {
                            out.push_str(s);
                            Ok(())
                        };
                        let mut chunks = Chunks::new(verdict);
                        let ok = chunks.feed(&bytes[..split], &mut push).unwrap()
                            && chunks.feed(&bytes[split..second], &mut push).unwrap()
                            && chunks.feed(&bytes[second..], &mut push).unwrap()
                            && chunks.finish(&mut push).unwrap();
                        match verdict {
                            Verdict::Replace => assert!(ok && out == expected, "{bytes:?} at {split}/{second}"),
                            Verdict::Reject => assert_eq!(ok, std::str::from_utf8(bytes).is_ok()),
                        }
                    }
                }
            }
        }

        // Exercise >64-byte SIMD lanes in `simdutf8::compat` across multi-byte UTF-8 and invalid sequences.
        let mut long = "ascii-prefix-0123456789-abcdefghijklmnopqrstuvwxyz-0123456789-λ雪🦀"
            .as_bytes()
            .to_vec();
        long.extend_from_slice(b"\xf0\x9f\xa6\xff\xe2\x82\xac-tail-0123456789-abcdefghijklmnopqrstuvwxyz-end");
        let expected_long = String::from_utf8_lossy(&long);
        for split in [0, 1, 63, 64, 65, 68, 69, 70, 74, 75, 76, long.len()] {
            let mut out = String::new();
            let mut push = |s: &str| -> Result<(), ()> {
                out.push_str(s);
                Ok(())
            };
            let mut chunks = Chunks::new(Verdict::Replace);
            assert!(chunks.feed(&long[..split], &mut push).unwrap());
            assert!(chunks.feed(&long[split..], &mut push).unwrap());
            assert!(chunks.finish(&mut push).unwrap());
            assert_eq!(out, expected_long, "long split {split}");

            let mut reject = Chunks::new(Verdict::Reject);
            let ok = reject.feed(&long[..split], &mut |_| Ok::<_, ()>(())).unwrap()
                && reject.feed(&long[split..], &mut |_| Ok::<_, ()>(())).unwrap()
                && reject.finish(&mut |_| Ok::<_, ()>(())).unwrap();
            assert!(!ok, "long invalid must reject at split {split}");
        }
    }
}
