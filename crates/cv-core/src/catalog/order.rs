//! Where each catalog row sits, so that one changed session writes one row.
//!
//! [`super::all_sessions_in`] lists rows by rowid, and that order is discovery's: harnesses in
//! registration order ([`crate::harness::rank`]), each harness's rows in the order its adapter
//! listed them. Each harness owns a span of rowids and keeps its rows in it `STRIDE` apart. A
//! publication keeps every row whose place is still in order (the longest increasing run of old
//! rowids along the new listing), gives each other row a rowid between its kept neighbours, and
//! rewrites a kept row only when its metadata changed. A gap with no room left is spread out
//! again with the rows around it, widening until there is room. Rows outside their span (an
//! older catalog's) move into it on their harness's first publication.

use super::imp::{row_to_ref, REF_COLUMNS};
use crate::ir::{Harness, SessionRef};
use rusqlite::params;
use std::collections::{HashMap, HashSet};

const SPAN_BITS: u32 = 56;
const STRIDE: i128 = 1 << 20;
/// The room a widened window leaves between neighbours.
const ROOMY: i128 = 1 << 10;
const _: () = assert!(Harness::COUNT < 127, "each harness's span must fit below i64::MAX");

#[cfg(test)]
thread_local! {
    /// Row statements (insert, update, delete) a publication ran: the one-change test's counter.
    pub(super) static WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn wrote() {
    #[cfg(test)]
    WRITES.with(|n| n.set(n.get() + 1));
}

/// `h`'s rowids, inclusive.
fn span(h: Harness) -> (i128, i128) {
    let lo = (crate::harness::rank(h) as i128 + 1) << SPAN_BITS;
    (lo, lo + (1 << SPAN_BITS) - 1)
}

type Key = (String, String);

fn key(r: &SessionRef) -> Key {
    (r.id.clone(), r.path.to_string_lossy().into_owned())
}

/// The listed metadata of two rows is equal: what decides whether a kept row is rewritten.
pub(super) fn same_row(a: &SessionRef, b: &SessionRef) -> bool {
    a.harness == b.harness
        && a.id == b.id
        && a.path == b.path
        && a.cwd == b.cwd
        && a.title == b.title
        && a.created_at == b.created_at
        && a.updated_at == b.updated_at
        && a.message_count == b.message_count
}

const SET_ROW: &str = "harness=?1,id=?2,path=?3,cwd=?4,title=?5,created_at=?6,updated_at=?7,\
                       message_count=?8,created_ns=?9,updated_ns=?10";

fn update(tx: &rusqlite::Transaction, rowid: i64, r: &SessionRef) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare_cached(&format!("UPDATE sessions SET {SET_ROW} WHERE rowid=?11"))?;
    stmt.execute(params![
        r.harness.as_str(),
        r.id,
        r.path.to_string_lossy(),
        r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        r.title,
        r.created_at.map(|t| t.timestamp()),
        r.updated_at.map(|t| t.timestamp()),
        r.message_count.map(|n| n as i64),
        r.created_at.and_then(|t| t.timestamp_nanos_opt()),
        r.updated_at.and_then(|t| t.timestamp_nanos_opt()),
        rowid,
    ])?;
    wrote();
    Ok(())
}

#[derive(Default)]
struct Preserved {
    version: Option<String>,
    model: Option<String>,
    git_branch: Option<String>,
    git_remote: Option<String>,
    compactions: Option<i64>,
    subagent_count: Option<i64>,
}

fn insert(tx: &rusqlite::Transaction, rowid: i64, r: &SessionRef, kept: Option<&Preserved>) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare_cached(&format!(
        "INSERT INTO sessions({REF_COLUMNS},writer_version,model,git_branch,git_remote,compactions,subagent_count,rowid) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"
    ))?;
    stmt.execute(params![
        r.harness.as_str(),
        r.id,
        r.path.to_string_lossy(),
        r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        r.title,
        r.created_at.map(|t| t.timestamp()),
        r.updated_at.map(|t| t.timestamp()),
        r.message_count.map(|n| n as i64),
        r.created_at.and_then(|t| t.timestamp_nanos_opt()),
        r.updated_at.and_then(|t| t.timestamp_nanos_opt()),
        kept.and_then(|p| p.version.as_deref()),
        kept.and_then(|p| p.model.as_deref()),
        kept.and_then(|p| p.git_branch.as_deref()),
        kept.and_then(|p| p.git_remote.as_deref()),
        kept.and_then(|p| p.compactions),
        kept.and_then(|p| p.subagent_count),
        rowid,
    ])?;
    wrote();
    Ok(())
}

struct Old {
    rowid: i64,
    row: Option<SessionRef>,
    preserved: Preserved,
}

fn old_rows(tx: &rusqlite::Transaction, h: Harness) -> rusqlite::Result<HashMap<Key, Old>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {REF_COLUMNS},rowid,writer_version,model,git_branch,git_remote,compactions,subagent_count \
         FROM sessions WHERE harness=?1"
    ))?;
    let rows = stmt.query_map([h.as_str()], |row| {
        // A cell this binary cannot decode is a changed row, rewritten, never a failed publication.
        let old = Old {
            rowid: row.get(10)?,
            row: row_to_ref(row).ok().flatten(),
            preserved: Preserved {
                version: row.get(11).ok().flatten(),
                model: row.get(12).ok().flatten(),
                git_branch: row.get(13).ok().flatten(),
                git_remote: row.get(14).ok().flatten(),
                compactions: row.get(15).ok().flatten(),
                subagent_count: row.get(16).ok().flatten(),
            },
        };
        Ok(((row.get(1)?, row.get(2)?), old))
    })?;
    rows.collect()
}

/// Make `h`'s rows exactly `refs`, in `refs`' order, writing only the rows that changed, moved,
/// appeared or vanished. A key listed twice takes its last place, as an insert-or-replace did.
pub(super) fn publish(tx: &rusqlite::Transaction, h: Harness, refs: &[SessionRef]) -> rusqlite::Result<()> {
    let last: HashMap<Key, usize> = refs.iter().enumerate().map(|(i, r)| (key(r), i)).collect();
    let listed: Vec<(Key, &SessionRef)> = refs
        .iter()
        .enumerate()
        .map(|(i, r)| (key(r), i, r))
        .filter(|(k, i, _)| last[k] == *i)
        .map(|(k, _, r)| (k, r))
        .collect();
    let old = old_rows(tx, h)?;
    let (lo, hi) = span(h);
    let place: Vec<Option<i128>> = listed
        .iter()
        .map(|(k, _)| old.get(k).map(|o| o.rowid as i128).filter(|id| (lo..=hi).contains(id)))
        .collect();
    let mut kept = increasing(&place);
    let ids = loop {
        match assign(&place, &kept, lo, hi) {
            Ok(ids) => break ids,
            Err((s, e)) => widen(&place, &mut kept, s, e, lo, hi),
        }
    };
    let staying: HashSet<&Key> = listed
        .iter()
        .zip(&kept)
        .filter(|(_, k)| **k)
        .map(|((k, _), _)| k)
        .collect();
    for (k, o) in &old {
        if !staying.contains(k) {
            tx.prepare_cached("DELETE FROM sessions WHERE rowid=?1")?
                .execute([o.rowid])?;
            wrote();
        }
    }
    for (((k, r), id), kept) in listed.iter().zip(ids).zip(kept) {
        let before = old.get(k);
        if !kept {
            insert(tx, id as i64, r, before.map(|o| &o.preserved))?;
        } else if let Some(o) = before.filter(|o| !o.row.as_ref().is_some_and(|row| same_row(row, r))) {
            update(tx, o.rowid, r)?;
        }
    }
    Ok(())
}

/// Upsert `r` without moving any other row: a known key is rewritten in place, a new one goes
/// after its harness's last row. The additive path ([`super::sync`]).
pub(super) fn upsert(tx: &rusqlite::Transaction, r: &SessionRef) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;
    let found: Option<i64> = tx
        .query_row(
            "SELECT rowid FROM sessions WHERE harness=?1 AND id=?2 AND path=?3",
            params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(rowid) = found {
        return update(tx, rowid, r);
    }
    let (lo, hi) = span(r.harness);
    let (lo64, hi64) = (lo as i64, hi as i64);
    let tail: Option<i64> = tx.query_row(
        "SELECT max(rowid) FROM sessions WHERE rowid BETWEEN ?1 AND ?2",
        params![lo64, hi64],
        |row| row.get(0),
    )?;
    let id = match tail {
        None => lo + (hi - lo) / 2,
        Some(tail) if (tail as i128) + STRIDE <= hi => tail as i128 + STRIDE,
        // A full span: the next publication of this harness places it.
        Some(_) => return publish_after(tx, r),
    };
    insert(tx, id as i64, r, None)
}

/// `r` appended to its harness's listing, through a whole publication of that harness.
fn publish_after(tx: &rusqlite::Transaction, r: &SessionRef) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {REF_COLUMNS} FROM sessions WHERE harness=?1 ORDER BY rowid"
    ))?;
    let mut refs: Vec<SessionRef> = stmt
        .query_map([r.harness.as_str()], row_to_ref)?
        .filter_map(Result::transpose)
        .collect::<rusqlite::Result<_>>()?;
    refs.push(r.clone());
    publish(tx, r.harness, &refs)
}

/// The longest strictly increasing run of known places, as a keep mask.
fn increasing(place: &[Option<i128>]) -> Vec<bool> {
    // Patience sorting: `tails[l]` ends the best run of length l + 1; `back` links each index to
    // its predecessor in that run.
    let mut tails: Vec<usize> = Vec::new();
    let mut back = vec![usize::MAX; place.len()];
    for (i, p) in place.iter().enumerate() {
        let Some(p) = *p else { continue };
        let l = tails.partition_point(|&t| place[t].expect("tails hold places") < p);
        if l > 0 {
            back[i] = tails[l - 1];
        }
        if l == tails.len() {
            tails.push(i);
        } else {
            tails[l] = i;
        }
    }
    let mut kept = vec![false; place.len()];
    let mut at = tails.last().copied().unwrap_or(usize::MAX);
    while at != usize::MAX {
        kept[at] = true;
        at = back[at];
    }
    kept
}

/// The rowid of every listed row: kept rows keep theirs, each run between kept neighbours `a`
/// and `b` takes rowids strictly between. `Err` names a run with no room.
fn assign(place: &[Option<i128>], kept: &[bool], lo: i128, hi: i128) -> Result<Vec<i128>, (usize, usize)> {
    let n = place.len();
    let mut ids = vec![0; n];
    let mut s = 0;
    while s < n {
        if kept[s] {
            ids[s] = place[s].expect("a kept row has a place");
            s += 1;
            continue;
        }
        let e = (s..n).find(|&i| kept[i]).unwrap_or(n);
        let j = (e - s) as i128;
        let a = if s == 0 { lo - 1 } else { ids[s - 1] };
        let b = if e == n {
            hi + 1
        } else {
            place[e].expect("a kept row has a place")
        };
        let run = &mut ids[s..e];
        if s == 0 && e == n && j * STRIDE < (hi - lo) / 2 {
            let first = lo + (hi - lo) / 2 - (j / 2) * STRIDE;
            run.iter_mut().zip(0..).for_each(|(id, t)| *id = first + t * STRIDE);
        } else if s == 0 && e < n && b - j * STRIDE > a {
            run.iter_mut().zip(0..).for_each(|(id, t)| *id = b - (j - t) * STRIDE);
        } else if s > 0 && e == n && a + j * STRIDE < b {
            run.iter_mut().zip(1..).for_each(|(id, t)| *id = a + t * STRIDE);
        } else if b - a > j {
            let step = (b - a) / (j + 1);
            run.iter_mut().zip(1..).for_each(|(id, t)| *id = a + t * step);
        } else {
            return Err((s, e));
        }
        s = e;
    }
    Ok(ids)
}

/// Release kept rows around the full run `s..e`, doubling on each side, until the window
/// leaves [`ROOMY`] between neighbours or holds the whole listing.
fn widen(place: &[Option<i128>], kept: &mut [bool], mut s: usize, mut e: usize, lo: i128, hi: i128) {
    let n = place.len();
    let mut k = 1;
    loop {
        for _ in 0..k {
            match (0..s).rev().find(|&i| kept[i]) {
                Some(i) => kept[i] = false,
                None => break,
            }
            s = (0..s).rev().find(|&i| kept[i]).map_or(0, |i| i + 1);
        }
        for _ in 0..k {
            match (e..n).find(|&i| kept[i]) {
                Some(i) => kept[i] = false,
                None => break,
            }
            e = (e..n).find(|&i| kept[i]).unwrap_or(n);
        }
        let a = if s == 0 {
            lo - 1
        } else {
            place[s - 1].expect("a kept row has a place")
        };
        let b = if e == n {
            hi + 1
        } else {
            place[e].expect("a kept row has a place")
        };
        if (s == 0 && e == n) || (b - a) / ((e - s) as i128 + 1) >= ROOMY {
            return;
        }
        k *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increasing_keeps_the_longest_ordered_run() {
        let p = |v: &[Option<i128>]| increasing(v);
        assert_eq!(p(&[Some(1), Some(2), Some(3)]), [true; 3]);
        assert_eq!(p(&[None, Some(1), Some(2)]), [false, true, true]);
        assert_eq!(p(&[Some(3), Some(1), Some(2)]), [false, true, true]);
        assert_eq!(p(&[]), Vec::<bool>::new());
    }

    #[test]
    fn a_full_gap_widens_until_it_has_room() {
        let (lo, hi) = (1 << 20, (1 << 21) - 1);
        // Two kept rows one apart, a new row between them: no room until both are released.
        let place = [Some(lo + 5), None, Some(lo + 6), Some(lo + 1000)];
        let mut kept = increasing(&place);
        let (s, e) = assign(&place, &kept, lo, hi).unwrap_err();
        widen(&place, &mut kept, s, e, lo, hi);
        let ids = assign(&place, &kept, lo, hi).unwrap();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
        assert!(ids.iter().all(|id| (lo..=hi).contains(id)));
    }
}
