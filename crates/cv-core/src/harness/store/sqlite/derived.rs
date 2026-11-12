//! Database-wide projections use the catalog's existing source records and admission.
use crate::source::{Payload, Probe, Record};
use crate::{Harness, SessionRef};
use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Key = (Option<PathBuf>, Harness, PathBuf, &'static str);
type Memo = std::collections::HashMap<Key, Arc<dyn std::any::Any + Send + Sync>>;
static MEMO: std::sync::LazyLock<Mutex<Memo>> = std::sync::LazyLock::new(Default::default);

/// Derive once per database state. The empty session id keys a database-wide consumer in
/// `source_dependencies`; the payload tag and `source::VERSION` are its only format authority.
/// Both memory and disk hits pass `Record::admit`, including the db, WAL and journal identities.
/// The callback must read only this database and return owned metadata, never lazy store ranges.
pub(crate) fn derived<T: Payload + Send + Sync + 'static>(
    db: &Path,
    harness: Harness,
    derive: impl FnOnce(&Connection, &Path) -> Result<T>,
) -> Result<Arc<Record<T>>> {
    let path = std::path::absolute(db)?;
    let r = SessionRef::new(String::new(), harness, path.clone());
    let context = crate::discover_cache::read::Context::current();
    let key = (context.path().map(Path::to_path_buf), harness, path, T::TAG);
    let mut probe = Probe::default();
    let now = probe.snapshot(&r);
    let hit = MEMO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
        .and_then(|record| record.downcast::<Record<T>>().ok())
        .filter(|record| record.admit(&now, &mut probe).is_some());
    if let Some(record) = hit {
        return Ok(record);
    }
    let stored = context
        .with_connection(|conn| crate::source::record_in::<T>(conn, T::TAG, &r))
        .flatten()
        .filter(|record| record.admit(&now, &mut probe).is_some());
    let record = match stored {
        Some(record) => record,
        None => {
            #[cfg(test)]
            DERIVE_OPENS.with(|n| n.set(n.get() + 1));
            let reader = super::open_ro(&r.path)?;
            let payload = reader.read_snapshot(|conn, _| derive(conn, &r.path))?;
            let baseline = now.with_reads(&reader.reads);
            let publishable = reader.reads.fresh(&mut Probe::default());
            drop(reader);
            // The existing publisher rechecks the certificate under the catalog transaction;
            // unavailable/frozen catalogs or concurrent WAL writes degrade to the in-memory record.
            if publishable {
                context.with_connection_mut(|conn| {
                    let mut publish = || -> Result<()> {
                        let tx = conn.transaction()?;
                        crate::source::publish_in(&tx, T::TAG, &r, &baseline, &payload, &mut Probe::default())?;
                        tx.commit()?;
                        Ok(())
                    };
                    if let Err(error) = publish() {
                        crate::env::warn_once("the SQLite forest catalog", error);
                    }
                });
            }
            Record::new(baseline, payload)
        }
    };
    let record = Arc::new(record);
    MEMO.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, record.clone());
    Ok(record)
}

#[cfg(test)]
thread_local! {
    pub(crate) static DERIVE_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Sum(i64);
    impl Payload for Sum {
        const TAG: &'static str = "test-sqlite-sum";
    }
    fn sum(conn: &Connection, _: &Path) -> Result<Sum> {
        Ok(Sum(conn.query_row("SELECT sum(x) FROM t", [], |r| r.get(0))?))
    }
    fn forget(db: &Path) {
        MEMO.lock().unwrap().retain(|(_, _, path, _), _| path != db);
    }

    #[test]
    fn catalog_record_skips_unchanged_sqlite_and_invalidates_each_source() {
        let world = crate::test_world("sqlite-derived");
        let db = world.home.join("state.db");
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(7);")
            .unwrap();
        let get = || derived(&db, Harness::Hermes, sum).unwrap();
        assert_eq!(get().payload, Sum(7));
        let r = SessionRef::new(String::new(), Harness::Hermes, db.clone());
        let catalog = crate::catalog::open_db().unwrap();
        let record = crate::source::record_in::<Sum>(&catalog, Sum::TAG, &r).unwrap();
        assert_eq!(record.version, crate::source::VERSION);
        assert!(!world.cv_home.join("sqlite-derived").exists());
        let before = DERIVE_OPENS.with(std::cell::Cell::get);
        for disk in [false, true] {
            if disk {
                forget(&db);
            }
            assert_eq!(
                derived(&db, Harness::Hermes, |_, _| -> Result<Sum> {
                    panic!("unchanged source was opened")
                })
                .unwrap()
                .payload,
                Sum(7)
            );
        }
        assert_eq!(DERIVE_OPENS.with(std::cell::Cell::get), before);
        writer.execute("INSERT INTO t VALUES(8)", []).unwrap();
        assert_eq!(get().payload, Sum(15));
        let journal = super::super::sidecar(&db, "-journal");
        std::fs::write(&journal, []).unwrap();
        assert_eq!(get().payload, Sum(15));
        std::fs::remove_file(journal).unwrap();
        assert_eq!(get().payload, Sum(15));
        assert_eq!(DERIVE_OPENS.with(std::cell::Cell::get), before + 3);
        drop(writer); // Checkpoint removes the WAL, changing the same certificate.
        assert_eq!(get().payload, Sum(15));
        assert_eq!(DERIVE_OPENS.with(std::cell::Cell::get), before + 4);
        std::fs::rename(&db, world.home.join("old.db")).unwrap();
        let replacement = Connection::open(&db).unwrap();
        replacement
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(99);")
            .unwrap();
        assert_eq!(get().payload, Sum(99));
        assert_eq!(DERIVE_OPENS.with(std::cell::Cell::get), before + 5);

        // Unknown versions/tags and malformed records all miss the existing decoder.
        for damage in ["version", "tag", "malformed"] {
            let record = crate::source::record_in::<Sum>(&catalog, Sum::TAG, &r).unwrap();
            let mut wire: serde_json::Value = serde_json::from_slice(&record.encode()).unwrap();
            match damage {
                "version" => wire["version"] = (crate::source::VERSION + 1).into(),
                "tag" => wire["tag"] = "foreign-consumer".into(),
                _ => wire = serde_json::Value::Null,
            }
            catalog
                .execute(
                    "UPDATE source_dependencies SET record=?1 WHERE consumer=?2",
                    rusqlite::params![serde_json::to_vec(&wire).unwrap(), Sum::TAG],
                )
                .unwrap();
            forget(&db);
            assert_eq!(get().payload, Sum(99));
        }
        assert_eq!(DERIVE_OPENS.with(std::cell::Cell::get), before + 8);
    }

    #[test]
    fn frozen_sqlite_derivation_and_errors_do_not_publish() {
        let world = crate::test_world("sqlite-derived-frozen");
        let db = world.home.join("state.db");
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(7);")
            .unwrap();
        let r = SessionRef::new(String::new(), Harness::Hermes, db.clone());
        let catalog = crate::catalog::open_db().unwrap();
        assert!(derived::<Sum>(&db, Harness::Hermes, |_, _| anyhow::bail!("failed projection")).is_err());
        assert!(crate::source::record_in::<Sum>(&catalog, Sum::TAG, &r).is_none());
        let _frozen = crate::env::Env::current().as_ref().clone().with_frozen().enter();
        assert_eq!(derived(&db, Harness::Hermes, sum).unwrap().payload, Sum(7));
        assert!(crate::source::record_in::<Sum>(&catalog, Sum::TAG, &r).is_none());
    }
}
