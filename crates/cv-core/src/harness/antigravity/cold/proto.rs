//! Rewrite only named wire fields. Unknown fields retain their original encoding.
use super::super::native::read_varint;
use anyhow::{ensure, Context, Result};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
struct Field<'a> {
    number: u32,
    wire: u8,
    value: &'a [u8],
    raw: &'a [u8],
}

fn walk(data: &[u8], mut f: impl FnMut(Field<'_>) -> Result<()>) -> Result<()> {
    let mut at = 0;
    while at < data.len() {
        let start = at;
        let tag = read_varint(data, &mut at)?;
        ensure!((1..=0x1fff_ffff).contains(&(tag >> 3)), "invalid protobuf tag");
        let wire = (tag & 7) as u8;
        let mut offset = at;
        match wire {
            0 => {
                read_varint(data, &mut at)?;
            }
            2 => {
                let len = usize::try_from(read_varint(data, &mut at)?)?;
                offset = at;
                at = at.checked_add(len).context("protobuf length overflow")?;
            }
            1 | 5 => {
                at = at
                    .checked_add(if wire == 1 { 8 } else { 4 })
                    .context("protobuf offset overflow")?
            }
            _ => anyhow::bail!("unsupported protobuf wire type {wire}"),
        }
        f(Field {
            number: (tag >> 3) as u32,
            wire,
            value: data.get(offset..at).context("short protobuf field")?,
            raw: &data[start..at],
        })?;
    }
    Ok(())
}

fn varint(buf: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        buf.push(n as u8 | 128);
        n >>= 7;
    }
    buf.push(n as u8);
}
pub(super) fn scalar(buf: &mut Vec<u8>, n: u32, v: u64) {
    varint(buf, u64::from(n) << 3);
    varint(buf, v);
}
pub(super) fn bytes(buf: &mut Vec<u8>, n: u32, v: &[u8]) {
    varint(buf, (u64::from(n) << 3) | 2);
    varint(buf, v.len() as u64);
    buf.extend(v);
}

fn unique(data: &[u8], number: u32, wire: u8) -> Result<Option<Vec<u8>>> {
    let mut value = None;
    walk(data, |f| {
        if f.number == number {
            ensure!(
                f.wire == wire && value.is_none(),
                "invalid or repeated protobuf field {number}"
            );
            value = Some(f.value.to_vec());
        }
        Ok(())
    })?;
    Ok(value)
}
pub(super) fn nested(data: &[u8], number: u32) -> Result<Vec<u8>> {
    unique(data, number, 2)?.with_context(|| format!("missing protobuf field {number}"))
}
pub(super) fn integer(data: &[u8], number: u32) -> Result<u64> {
    unique(data, number, 0)?.map_or(Ok(0), |v| read_varint(&v, &mut 0))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Reference<'a> {
    pub trajectory: &'a str,
    pub step: u64,
    pub turn: u64,
    pub cascade: &'a str,
}
impl Reference<'_> {
    pub fn verify(&self, md: &[u8]) -> Result<()> {
        let r = nested(md, 20)?;
        ensure!(
            nested(&r, 1)? == self.trajectory.as_bytes()
                && nested(&r, 4)? == self.cascade.as_bytes()
                && integer(&r, 2)? == self.step
                && integer(&r, 3)? == self.turn,
            "inconsistent native step reference at {}",
            self.step
        );
        Ok(())
    }
    fn encode(&self, old: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        walk(old, |f| {
            if !(1..=4).contains(&f.number) {
                out.extend(f.raw)
            }
            Ok(())
        })?;
        bytes(&mut out, 1, self.trajectory.as_bytes());
        if self.step != 0 {
            scalar(&mut out, 2, self.step)
        }
        if self.turn != 0 {
            scalar(&mut out, 3, self.turn)
        }
        bytes(&mut out, 4, self.cascade.as_bytes());
        Ok(out)
    }
}
pub(super) fn turn(md: &[u8]) -> Result<u64> {
    integer(&nested(md, 20)?, 3)
}

fn transform(data: &[u8], mut edit: impl FnMut(Field<'_>) -> Result<Option<Vec<u8>>>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    walk(data, |f| {
        if let Some(v) = edit(f)? {
            ensure!(f.wire == 2, "expected length-delimited field {}", f.number);
            bytes(&mut out, f.number, &v)
        } else {
            out.extend(f.raw)
        }
        Ok(())
    })?;
    Ok(out)
}
pub(super) fn metadata(data: &[u8], new: &Reference<'_>) -> Result<Vec<u8>> {
    transform(data, |f| {
        if f.number == 20 {
            new.encode(f.value).map(Some)
        } else {
            Ok(None)
        }
    })
}
pub(super) fn task(data: &[u8], old: &str, new: &str) -> Result<Vec<u8>> {
    transform(data, |f| {
        if f.number == 1 {
            if let Some(tail) = f
                .value
                .strip_prefix(old.as_bytes())
                .filter(|v| v.is_empty() || v.starts_with(b"/task-"))
            {
                return Ok(Some([new.as_bytes(), tail].concat()));
            }
        }
        Ok(None)
    })
}
pub(super) fn payload(data: &[u8], new: &Reference<'_>, old: &str) -> Result<Vec<u8>> {
    transform(data, |f| match f.number {
        5 => metadata(f.value, new).map(Some),
        148 => task(f.value, old, new.cascade).map(Some),
        _ => Ok(None),
    })
}
pub(super) fn trajectory(data: &[u8], old: &str, new: &str) -> Result<Vec<u8>> {
    transform(data, |f| {
        Ok((matches!(f.number, 5 | 6) && f.value == old.as_bytes()).then(|| new.as_bytes().to_vec()))
    })
}

/// Compare all payload fields outside the two metadata carriers to the source,
/// including signatures, repeated user bodies and literal UUIDs.
pub(super) fn body_digest(data: &[u8]) -> Result<blake3::Hash> {
    let mut hash = blake3::Hasher::new();
    walk(data, |f| {
        if !matches!(f.number, 5 | 148) {
            hash.update(f.raw);
        }
        Ok(())
    })?;
    Ok(hash.finalize())
}

pub(super) fn generation(data: &[u8], indices: &BTreeMap<u64, u64>) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut complete = true;
    let mut seen = false;
    walk(data, |f| {
        if f.number != 2 {
            out.extend(f.raw);
            return Ok(());
        }
        ensure!(matches!(f.wire, 0 | 2), "invalid generation step indices");
        let mut at = 0;
        let mut packed = Vec::new();
        while at < f.value.len() {
            seen = true;
            let old = read_varint(f.value, &mut at)?;
            if let Some(&new) = indices.get(&old) {
                if f.wire == 0 {
                    scalar(&mut out, 2, new)
                } else {
                    varint(&mut packed, new)
                }
            } else {
                complete = false
            }
        }
        if f.wire == 2 {
            bytes(&mut out, 2, &packed)
        }
        Ok(())
    })?;
    Ok((complete && seen).then_some(out))
}
