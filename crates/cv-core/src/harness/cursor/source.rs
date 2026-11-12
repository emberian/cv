//! SQLite TEXT/BLOB cells are read incrementally under the caller's read snapshot.
use super::*;
use crate::harness::store::sqlite::{Cell, Cells};
use rusqlite::OptionalExtension;
use std::io::{self, Read};

/// The value cell of the key-value row `key` in `table`, copied once into `cells`' arena. TEXT is
/// read as stored; a BLOB keeps Cursor's historical lossy UTF-8 conversion. None for a missing,
/// NULL or empty value.
pub(super) fn cell(conn: &Connection, cells: &Cells, table: &str, key: &str) -> Result<Option<Cell>> {
    let row = conn.query_row(
        &format!("SELECT _rowid_, typeof(value) FROM {table} WHERE key = ?1"),
        [key],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    );
    match row {
        Ok((_, kind)) if kind == "null" => return Ok(None),
        Ok((rowid, kind)) if kind != "blob" => return cells.row(conn, table, "value", rowid),
        Ok((rowid, _)) => {
            if let Ok(blob) = conn.blob_open(rusqlite::MAIN_DB, table, "value", rowid, true) {
                let mut input = LossyUtf8::new(blob);
                return cells.append(|out| {
                    io::copy(&mut input, out)?;
                    Ok(())
                });
            }
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(_) => {}
    }
    // WITHOUT ROWID/indexed value schemas cannot expose incremental cells; this retains one
    // native cell with the same TEXT/BLOB tolerance.
    let raw = conn
        .query_row(&format!("SELECT value FROM {table} WHERE key = ?1"), [key], |row| {
            if row.get_ref(0)? == rusqlite::types::ValueRef::Null {
                return Ok(None);
            }
            row.get::<_, String>(0).map(Some).or_else(|_| {
                row.get::<_, Vec<u8>>(0)
                    .map(|b| Some(String::from_utf8_lossy(&b).into_owned()))
            })
        })
        .optional()?
        .flatten();
    match raw {
        Some(raw) => cells.append(|out| Ok(out.write_all(raw.as_bytes())?)),
        None => Ok(None),
    }
}

/// Cursor historically replaces invalid UTF-8 in BLOB values before JSON parsing. Chunked
/// conversion preserves exactly String::from_utf8_lossy's replacement groups, including tails.
struct LossyUtf8<R> {
    input: R,
    chunks: crate::utf8::Chunks,
    output: Vec<u8>,
    position: usize,
    done: bool,
}
impl<R: Read> LossyUtf8<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            chunks: crate::utf8::Chunks::new(crate::utf8::Verdict::Replace),
            output: Vec::new(),
            position: 0,
            done: false,
        }
    }
    fn fill(&mut self) -> io::Result<()> {
        self.output.clear();
        self.position = 0;
        let mut chunk = [0; 64 * 1024];
        let n = self.input.read(&mut chunk)?;
        self.done = n == 0;
        let output = &mut self.output;
        let mut push = |text: &str| -> io::Result<()> {
            output.extend_from_slice(text.as_bytes());
            Ok(())
        };
        if self.done {
            self.chunks.finish(&mut push)?;
        } else {
            self.chunks.feed(&chunk[..n], &mut push)?;
        }
        Ok(())
    }
}
impl<R: Read> Read for LossyUtf8<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.position == self.output.len() && !self.done {
            self.fill()?;
        }
        let n = out.len().min(self.output.len() - self.position);
        out[..n].copy_from_slice(&self.output[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lossy_blob_conversion_matches_whole_string_across_boundaries() {
        for suffix in [
            vec![0xff],
            vec![0xe0, 0xa0],
            vec![0xf0, 0x9f, 0x80, 0x80],
            vec![0xed, 0xa0, 0x80],
            vec![0xf0, 0x80, 0x80, 0x80],
        ] {
            for boundary in 65530..=65538 {
                let mut input = vec![b'x'; boundary];
                input.extend(&suffix);
                input.extend(b"tail");
                let expected = String::from_utf8_lossy(&input);
                let mut actual = String::new();
                LossyUtf8::new(input.as_slice()).read_to_string(&mut actual).unwrap();
                assert_eq!(actual, expected);
            }
        }
    }
    #[test]
    fn native_text_and_lossy_blob_cells_and_without_rowid_fallback() {
        let world = crate::test_world("cursor-cells");
        for rowid in [true, false] {
            let path = world.base.join(format!("state-{rowid}.vscdb"));
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value){}",
                if rowid { "" } else { " WITHOUT ROWID" }
            ))
            .unwrap();
            let invalid = b"{\"conversation\":[{\"type\":1,\"text\":\"\xff\"}]}";
            conn.execute("INSERT INTO cursorDiskKV VALUES('blob',?1)", [invalid.as_slice()])
                .unwrap();
            conn.execute("INSERT INTO cursorDiskKV VALUES('text',?1)", ["{\"conversation\":[]}"])
                .unwrap();
            conn.execute("INSERT INTO cursorDiskKV VALUES('null',NULL)", [])
                .unwrap();
            if rowid {
                assert!(conn
                    .blob_open(rusqlite::MAIN_DB, "cursorDiskKV", "value", 2, true)
                    .is_ok());
            }
            let cells = Cells::new(&conn, &path, &Default::default(), "cursorDiskKV").unwrap();
            for key in ["blob", "text"] {
                let cell = cell(&conn, &cells, "cursorDiskKV", key).unwrap().unwrap();
                let value = cells.read(cell, |v| Ok(v.resolve()?.into_owned())).unwrap().unwrap();
                if key == "blob" {
                    let expected: Value = serde_json::from_str(&String::from_utf8_lossy(invalid)).unwrap();
                    assert_eq!(value, expected);
                } else {
                    assert_eq!(value, serde_json::json!({"conversation":[]}));
                }
            }
            for key in ["null", "missing"] {
                assert!(cell(&conn, &cells, "cursorDiskKV", key).unwrap().is_none());
            }
        }
    }
}
