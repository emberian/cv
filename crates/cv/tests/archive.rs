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
    let appended = format!(
        "{}\n",
        json!({"type": "user", "uuid": "u9", "timestamp": "2026-06-02T10:00:00Z",
                "message": {"role": "user", "content": "one more thing"}})
    );
    body.push_str(&appended);
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
    // Native custody: the second pack holds exactly the appended line, chained after the first.
    assert_eq!(pack.read_payload(&addr).unwrap(), appended.as_bytes());
    assert_eq!(alpha_entry["custody"]["kind"], "native_extents");
    assert_eq!(alpha_entry["custody"]["extents"].as_array().unwrap().len(), 2);
    // The verbatim `ai-title` carrier is held, but the visible count excludes it.
    assert_eq!(alpha_entry["message_count"], 3, "{alpha_entry}");

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

/// #tooth A packed revision and its live namesake remain independently selectable after a sweep.
#[test]
fn archive_frozen_search_show_pack_and_strict_port_keep_profile_twins_separate() {
    let w = world("archive-profile-twins");
    let second = w.home.join(".claude-alt");
    let project = second.join("projects/test");
    fs::create_dir_all(&project).unwrap();
    let twin = project.join("alphasess.jsonl");
    let original = w.transcript("alphasess");
    fs::write(
        &twin,
        fs::read_to_string(&original).unwrap().replace("zebrafish", "swordfish"),
    )
    .unwrap();
    let run = |args: &[&str]| {
        let (ok, _, out, err) = w.cv_env(args, &[("CLUSTERVISION_CLAUDE_ROOTS", second.to_str().unwrap())]);
        assert!(ok, "{args:?}: {out}\n{err}");
        out
    };
    let json = |args: &[&str]| serde_json::from_str::<Value>(&run(args)).unwrap();
    run(&["archive", "sync"]);
    let hit = |word| {
        let results = json(&["--frozen", "search", word, "--json"]);
        let hits = results["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{results}");
        hits[0]["session_key"].as_str().unwrap().to_owned()
    };
    // Cold frozen reads use the pack without creating a persistent Tantivy index.
    let key = hit("zebrafish");
    assert!(!w.cv_home.join("tantivy/meta.json").exists());
    run(&["index"]);
    fs::remove_file(&original).unwrap();
    assert_eq!(hit("zebrafish"), key);
    run(&["index"]);
    assert_eq!(hit("zebrafish"), key);
    let live_key = hit("swordfish");
    assert_ne!(key, live_key);
    let show = json(&["--frozen", "show", &key, "--json"]);
    assert!(show.to_string().contains("zebrafish"));
    assert!(show["extra"]["cv"]["read_coverage"]["sources"]
        .as_array()
        .unwrap()
        .contains(&json!("custody_pack")));
    let pack = json(&["--frozen", "pack", "--session", &key, "--json"]);
    assert!(pack["content"].as_str().unwrap().contains("zebrafish"));
    assert!(pack["content"]
        .as_str()
        .unwrap()
        .contains(&format!("cv show claude:{key}")));
    let target = w.base.join("ported");
    let port = json(&[
        "port",
        &key,
        "--strict",
        "--harness",
        "claude",
        "--out",
        target.to_str().unwrap(),
        "--json",
    ]);
    assert!(fs::read_to_string(port["path"].as_str().unwrap())
        .unwrap()
        .contains("zebrafish"));
    let live = json(&["--frozen", "show", &live_key, "--json"]);
    assert!(live.to_string().contains("swordfish"));
    assert_eq!(live["source_path"], json!(twin));
    assert!(!original.exists());
}

/// The `.pack.zst` files of the archive, oldest first by name order of their v7 ids.
fn pack_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut packs: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".pack.zst"))
        .collect();
    packs.sort();
    packs
}

/// #tooth Re-archiving an appended native session packs only the appended bytes, and the chain
/// alone serves frozen show, search, pack and strict port with its carriers once the file is gone.
#[test]
fn archive_native_extents_appends_only_delta_bytes_and_preserves_native_records() {
    let w = world("archive-native-extents");
    let path = w.transcript("alphasess");
    let mut body = fs::read_to_string(&path).unwrap();
    body.push_str(&format!(
        "{}\n",
        json!({"type": "file-history-snapshot", "messageId": "snap-1",
               "snapshot": {"marker": "carrier-okapi-77", "trackedFileBackups": {}}})
    ));
    let filler = "lorem ipsum dolor sit amet ".repeat(18);
    for i in 0..400 {
        body.push_str(&format!(
            "{}\n",
            json!({"type": "user", "uuid": format!("fu{i}"), "parentUuid": null,
                   "timestamp": "2026-01-03T10:00:00Z",
                   "message": {"role": "user", "content": format!("turn {i} {filler}")}})
        ));
    }
    fs::write(&path, &body).unwrap();
    assert!(body.len() > 200 * 1024, "{}", body.len());
    w.cv_ok(&["archive", "sync"]);
    let packs_dir = w.cv_home.join("packs");
    let first = pack_files(&packs_dir);
    assert_eq!(first.len(), 1);

    let appended = format!(
        "{}\n",
        json!({"type": "user", "uuid": "late-1", "timestamp": "2026-01-04T10:00:00Z",
               "message": {"role": "user",
                           "content": format!("narwhal: custody.rs cargo clippy failed {}", "x".repeat(900))}})
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, appended.as_bytes()))
        .unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(out.contains("1 archived, 1 skipped"), "{out}");
    let second: Vec<_> = pack_files(&packs_dir)
        .into_iter()
        .filter(|p| !first.contains(p))
        .collect();
    assert_eq!(second.len(), 1);
    let on_disk = fs::metadata(&second[0]).unwrap().len();
    assert!(on_disk < 4096, "delta pack is {on_disk} bytes on disk");
    let store = StateStore::at_dir(&w.cv_home);
    let manifests = store.list_pack_manifests().unwrap();
    let entry = &manifests.last().unwrap().index_meta["sessions"][0];
    let delta: PayloadAddr = serde_json::from_value(entry["addr"].clone()).unwrap();
    assert_eq!(
        delta.length,
        appended.len() as u64,
        "only the appended line is captured"
    );
    assert_eq!(entry["custody"]["total_bytes"], json!(body.len() + appended.len()));

    fs::remove_file(&path).unwrap();
    let json = |args: &[&str]| serde_json::from_str::<Value>(&w.cv_ok(args).0).unwrap();
    let hits = json(&["--frozen", "search", "narwhal", "--json"]);
    let hits = hits["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let key = hits[0]["session_key"].as_str().unwrap().to_owned();
    let show = json(&["--frozen", "show", &key, "--json"]).to_string();
    assert!(
        show.contains("narwhal") && show.contains("turn 0 lorem"),
        "both extents read"
    );
    let pack = json(&["--frozen", "pack", "--session", &key, "--json"]);
    assert!(pack["content"].as_str().unwrap().contains("narwhal"));
    let target = w.base.join("ported-native");
    let port = json(&[
        "port",
        &key,
        "--strict",
        "--harness",
        "claude",
        "--out",
        target.to_str().unwrap(),
        "--json",
    ]);
    let ported = fs::read_to_string(port["path"].as_str().unwrap()).unwrap();
    assert!(
        ported.contains("carrier-okapi-77"),
        "the native carrier record survives"
    );
    assert!(ported.contains("narwhal"));
}

/// #tooth A native file rewritten shorter in place starts a fresh one-extent chain, and the
/// overwritten tail stops matching search.
#[test]
fn archive_native_rewrite_below_prefix_starts_fresh_chain_and_retracts_tail() {
    let w = world("archive-native-rewrite");
    let path = w.transcript("alphasess");
    let original = fs::read_to_string(&path).unwrap();
    let mut grown = original.clone();
    grown.push_str(&format!(
        "{}\n",
        json!({"type": "user", "uuid": "tail-1", "timestamp": "2026-01-05T10:00:00Z",
               "message": {"role": "user", "content": "the axolotl tail"}})
    ));
    fs::write(&path, &grown).unwrap();
    w.cv_ok(&["archive", "sync"]);
    w.cv_ok(&["index"]);
    let json = |args: &[&str]| serde_json::from_str::<Value>(&w.cv_ok(args).0).unwrap();
    let count = |word: &str| json(&["search", word, "--json"])["hits"].as_array().unwrap().len();
    assert_eq!(count("axolotl"), 1);

    // Rewind in place: shorter than the archived prefix, different bytes.
    let rewound = original.replace("zebrafish", "pangolin");
    fs::write(&path, &rewound).unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(out.contains("1 archived"), "{out}");
    let store = StateStore::at_dir(&w.cv_home);
    let manifests = store.list_pack_manifests().unwrap();
    let entry = &manifests.last().unwrap().index_meta["sessions"][0];
    assert_eq!(entry["custody"]["extents"].as_array().unwrap().len(), 1, "{entry}");
    assert_eq!(entry["custody"]["total_bytes"], json!(rewound.len()));

    w.cv_ok(&["index"]);
    assert_eq!(count("axolotl"), 0, "the retracted tail is no longer a hit");
    assert_eq!(count("zebrafish"), 0);
    assert_eq!(count("pangolin"), 1);
    fs::remove_file(&path).unwrap();
    let hits = json(&["--frozen", "search", "pangolin", "--json"]);
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1, "{hits}");
    assert_eq!(
        json(&["--frozen", "search", "axolotl", "--json"])["hits"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

/// #tooth The custody inventory reads one head row per archived session; a store without the
/// heads (one no writable open of this binary has converged yet) falls back to the manifests
/// under `--frozen`, and its next writable open rebuilds every head.
#[test]
fn custody_heads_track_newest_entry_and_fall_back_then_backfill() {
    let w = world("custody-heads");
    w.cv_ok(&["archive", "sync"]);
    let heads = |w: &cv_testkit::World| -> i64 {
        let conn = rusqlite::Connection::open(w.cv_home.join("state.db")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM custody_heads", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(heads(&w), 2);
    fs::remove_file(w.transcript("alphasess")).unwrap();
    let ls = |w: &cv_testkit::World| w.cv_ok(&["--frozen", "ls", "--json"]).0;
    assert!(ls(&w).contains("alpha adventures"));
    {
        let conn = rusqlite::Connection::open(w.cv_home.join("state.db")).unwrap();
        conn.execute_batch(
            "DROP TABLE custody_heads; DROP TABLE custody_heads_through;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    }
    assert!(ls(&w).contains("alpha adventures"), "frozen fallback to pack_manifests");
    let mut body = fs::read_to_string(w.transcript("betasess")).unwrap();
    body.push_str(&format!(
        "{}\n",
        json!({"type": "user", "uuid": "b9", "timestamp": "2026-03-02T09:00:00Z",
               "message": {"role": "user", "content": "recount"}})
    ));
    fs::write(w.transcript("betasess"), body).unwrap();
    w.cv_ok(&["archive", "sync"]);
    assert_eq!(heads(&w), 2, "rebuilt the archived-only session beside the new head");
    assert!(ls(&w).contains("alpha adventures"));
}

/// #tooth A Claude session whose read also consults a companion (`tasks/<sid>/`) keeps a
/// projection: a native chain of the transcript alone would drop the companion.
#[test]
fn archive_projects_claude_sessions_that_read_companions() {
    let w = world("archive-companion");
    let tasks = w.home.join(".claude/tasks/alphasess");
    fs::create_dir_all(&tasks).unwrap();
    fs::write(
        tasks.join("1.json"),
        r#"{"id":"1","subject":"Check companion","status":"pending"}"#,
    )
    .unwrap();
    w.cv_ok(&["archive", "sync"]);
    let store = StateStore::at_dir(&w.cv_home);
    let sessions = store.list_pack_manifests().unwrap()[0].index_meta["sessions"].clone();
    let kind = |id: &str| {
        let e = sessions.as_array().unwrap().iter().find(|e| e["id"] == id).unwrap();
        e["custody"]["kind"]
            .as_str()
            .unwrap_or("portable_projection")
            .to_owned()
    };
    assert_eq!(kind("alphasess"), "portable_projection");
    assert_eq!(kind("betasess"), "native_extents");
}

/// The kind and custody of each archived session in the newest manifest that names it.
fn custody_of(w: &cv_testkit::World, id: &str) -> Value {
    let store = StateStore::at_dir(&w.cv_home);
    store
        .list_pack_manifests()
        .unwrap()
        .iter()
        .rev()
        .find_map(|m| {
            m.index_meta["sessions"]
                .as_array()?
                .iter()
                .find(|e| e["id"] == id)
                .map(|e| e["custody"].clone())
        })
        .unwrap_or_else(|| panic!("{id} is archived"))
}

/// #tooth Frozen show and pack of a native session leave nothing behind in `TMPDIR` (the
/// adapter reads the chain by position; the in-process tooth in `custody.rs` checks that
/// nothing is staged while it streams).
#[test]
fn archive_native_frozen_reads_leave_tmpdir_untouched() {
    let w = world("archive-native-tmpdir");
    w.cv_ok(&["archive", "sync"]);
    assert_eq!(custody_of(&w, "betasess")["kind"], "native_extents");
    fs::remove_file(w.transcript("betasess")).unwrap();
    let tmp = w.base.join("tmpdir");
    fs::create_dir_all(&tmp).unwrap();
    let tmp_s = tmp.to_str().unwrap();
    let run = |args: &[&str]| {
        let (ok, code, out, err) = w.cv_env(args, &[("TMPDIR", tmp_s)]);
        assert!(ok, "cv {args:?} exited {code}: {err}");
        out
    };
    let hits: Value = serde_json::from_str(&run(&["--frozen", "search", "quokka", "--json"])).unwrap();
    let key = hits["hits"][0]["session_key"].as_str().unwrap().to_owned();
    assert!(run(&["--frozen", "show", &key, "--json"]).contains("seventeen"));
    assert!(run(&["--frozen", "pack", "--session", &key, "--json"]).contains("quokka"));
    let left: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
