//! Integration tests for `cv archive` & `cv watch` custody over sealed Zstd-Seekable `packs/`
//! and `StateStore` (`state.db`): multi-session pack batching, BLAKE3 payload verification,
//! `publish-before-reference` inbox draining, deduplication/idempotency, and `--frozen` read safety.

mod common;

use common::world;
use cv_core::packs::{PayloadAddr, SealedPack};
use cv_core::state::StateStore;
use serde_json::{json, Value};
use std::fs;

#[test]
fn sync_batches_into_sealed_pack_is_idempotent_and_verifies_blake3() {
    let w = world("sync-packs");

    // First sync batches BOTH discovered sessions into ONE sealed `.pack.zst` archive.
    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        out.contains("2 archived, 0 skipped (unchanged) of 2 discovered"),
        "{out}"
    );

    // Clean break from legacy per-session JSON files and flat catalog.jsonl.
    assert!(!w.cv_home.join("archive").exists(), "legacy archive/ dir must be gone");
    assert!(
        !w.cv_home.join("catalog.jsonl").exists(),
        "legacy catalog.jsonl must be gone"
    );

    let packs_dir = w.cv_home.join("packs");
    let zst_files: Vec<_> = fs::read_dir(&packs_dir)
        .expect("packs/ dir created")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".pack.zst"))
        .collect();
    assert_eq!(
        zst_files.len(),
        1,
        "both sessions in one sync pass must batch into ONE sealed .pack.zst: {zst_files:?}"
    );

    let store = StateStore::at_dir(&w.cv_home);
    assert_eq!(
        store.inbox_count().unwrap(),
        0,
        "publish_pack_range must drain capture_inbox"
    );
    let manifests = store.list_pack_manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].record_count, 2);
    assert!(manifests[0].digest.starts_with("blake3:"));

    // Second sync: nothing changed -> 0 archived, 2 skipped, no new pack files or manifests.
    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        out.contains("0 archived, 2 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    assert_eq!(store.list_pack_manifests().unwrap().len(), 1);

    // `ls` lists each session once with pack attribution.
    let (out, _) = w.cv_ok(&["archive", "ls"]);
    assert!(out.contains("2 session(s) across 1 pack(s)"), "{out}");
    assert_eq!(out.matches("alphases").count(), 1, "{out}");
    assert!(out.contains("alpha adventures"), "{out}");

    // `--frozen` read safety: `archive ls` and `archive path` succeed, `archive sync` exits 2.
    let (frozen_ls, _) = w.cv_ok(&["--frozen", "archive", "ls"]);
    assert!(frozen_ls.contains("2 session(s) across 1 pack(s)"), "{frozen_ls}");
    let (frozen_path, _) = w.cv_ok(&["--frozen", "archive", "path"]);
    assert_eq!(frozen_path.trim(), w.cv_home.display().to_string());
    let (ok, code, _, err) = w.cv(&["--frozen", "archive", "sync"]);
    assert!(
        !ok && code == 2,
        "frozen archive sync must exit 2: code={code} err={err}"
    );

    // Append a turn to `alphasess`: only `alphasess` re-archives into a second delta pack;
    // `betasess` is skipped, and `archive ls` deduplicates to 2 sessions with updated msg count.
    let mut body = fs::read_to_string(w.transcript("alphasess")).unwrap();
    body.push_str(
        &json!({"type": "user", "uuid": "u9", "timestamp": "2026-06-02T10:00:00Z",
                "message": {"role": "user", "content": "one more thing"}})
        .to_string(),
    );
    body.push('\n');
    fs::write(w.transcript("alphasess"), body).unwrap();
    // Bump mtime explicitly so sub-second filesystems always register the mutation.
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(w.transcript("alphasess"))
        .and_then(|f| f.set_modified(bumped))
        .unwrap();

    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        out.contains("1 archived, 1 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    let manifests = store.list_pack_manifests().unwrap();
    assert_eq!(manifests.len(), 2);
    assert_eq!(store.inbox_count().unwrap(), 0);

    let (out, _) = w.cv_ok(&["archive", "ls"]);
    assert!(
        out.contains("2 session(s) across 2 pack(s)"),
        "changed session must deduplicate across packs:\n{out}"
    );
    assert!(out.contains("3 msgs"), "updated message count visible:\n{out}");

    // Verify BLAKE3-authenticated random-access payload retrieval from the latest sealed pack.
    let latest = manifests.last().unwrap();
    let sessions = latest.index_meta["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let alpha_entry = &sessions[0];
    assert_eq!(alpha_entry["id"], "alphasess");
    let addr: PayloadAddr = serde_json::from_value(alpha_entry["addr"].clone()).unwrap();
    let pack = SealedPack::open(&packs_dir, &latest.pack_id).unwrap();
    let raw_ir = pack.read_payload(&addr).unwrap();
    let archived: Value = serde_json::from_slice(&raw_ir).expect("archived payload is valid IR JSON");
    assert_eq!(archived["id"], "alphasess");
    assert_eq!(archived["messages"].as_array().unwrap().len(), 3);

    // Checkpoint & drop `-wal`/`-shm` (or verify `--frozen` on populated `state.db`) so `cv --frozen
    // archive ls|path` proves zero `-wal`/`-shm` creation inside `$CLUSTERVISION_HOME`.
    {
        let conn = rusqlite::Connection::open(w.cv_home.join("state.db")).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let _ = fs::remove_file(w.cv_home.join("state.db-wal"));
    let _ = fs::remove_file(w.cv_home.join("state.db-shm"));
    fn snap(root: &std::path::Path, dir: &std::path::Path, out: &mut std::collections::BTreeMap<String, usize>) {
        for entry in fs::read_dir(dir).unwrap() {
            let e = entry.unwrap();
            let p = e.path();
            if p.is_dir() {
                snap(root, &p, out);
            } else if p.is_file() {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, fs::metadata(&p).unwrap().len() as usize);
            }
        }
    }
    let mut before = std::collections::BTreeMap::new();
    snap(&w.cv_home, &w.cv_home, &mut before);
    let (frozen_ls, _) = w.cv_ok(&["--frozen", "archive", "ls"]);
    assert!(frozen_ls.contains("2 session(s) across 2 pack(s)"), "{frozen_ls}");
    let (frozen_path, _) = w.cv_ok(&["--frozen", "archive", "path"]);
    assert!(frozen_path.trim().ends_with("cvhome"), "{frozen_path}");
    let mut after = std::collections::BTreeMap::new();
    snap(&w.cv_home, &w.cv_home, &mut after);
    assert_eq!(
        after, before,
        "--frozen archive ls/path must not create state.db-wal or state.db-shm"
    );
}

#[test]
fn ls_and_path_on_empty_archive_are_read_only_under_frozen() {
    let w = world("empty");
    // `--frozen` on a cold/non-existent `$CLUSTERVISION_HOME` must succeed without creating files.
    let (out, _) = w.cv_ok(&["--frozen", "archive", "ls"]);
    assert!(out.contains("archive empty"), "{out}");
    let (out, _) = w.cv_ok(&["--frozen", "archive", "path"]);
    assert_eq!(out.trim(), w.cv_home.display().to_string(), "{out}");
    assert!(
        !w.cv_home.join("state.db").exists(),
        "read-only archive ls/path must not create state.db"
    );
    assert!(
        !w.cv_home.join("packs").exists(),
        "read-only archive ls/path must not create packs/"
    );
}
