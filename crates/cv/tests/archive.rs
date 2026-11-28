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
    // Cold frozen reads use `--scan` over the pack without creating a persistent Tantivy index.
    let cold = json(&["--frozen", "search", "--scan", "zebrafish", "--json"]);
    let cold_hits = cold["hits"].as_array().unwrap();
    assert_eq!(cold_hits.len(), 1, "{cold}");
    let key = cold_hits[0]["session_key"].as_str().unwrap().to_owned();
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

/// #tooth Qwen session JSONL and OpenSession documents are held natively too: each chain alone
/// serves frozen search and show once its file is gone, Qwen's read recognizes the session by
/// the `projects/<p>/chats/` layout the archive kept, and a rewritten document is recaptured
/// whole as a fresh one-extent chain.
#[test]
fn archive_native_qwen_and_opensession_read_back_from_the_chain_alone() {
    let w = world("archive-native-more");
    let chats = w.home.join(".qwen/projects/-work-proj/chats");
    fs::create_dir_all(&chats).unwrap();
    let qwen = chats.join("qsess1.jsonl");
    let rec = |uuid: &str, ts: u32, body: &str| {
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":null,"sessionId":"qsess1","cwd":"/work/proj","version":"0.25.1","timestamp":"2026-10-11T00:00:{ts:02}.000Z",{body}}}"#
        )
    };
    let lines = [
        rec(
            "u1",
            1,
            r#""type":"user","message":{"role":"user","parts":[{"text":"count the capybara herd"}]}"#,
        ),
        rec(
            "a1",
            2,
            r#""type":"assistant","model":"m","message":{"role":"model","parts":[{"text":"forty capybaras"}]}"#,
        ),
    ];
    fs::write(&qwen, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();

    let exports = w.base.join("exports");
    fs::create_dir_all(&exports).unwrap();
    w.cv_ok(&["config", "--add-export", exports.to_str().unwrap()]);
    let doc_path = exports.join("doc-1.opensession.json");
    let write_doc = |text: &str| {
        let mut session = cv_core::ir::Session::new("doc-1", cv_core::ir::Harness::OpenSession);
        let mut m = cv_core::ir::Message::new(cv_core::ir::Role::User);
        m.content.push(cv_core::ir::Block::from_text(text.into()));
        session.messages.push(m);
        fs::write(
            &doc_path,
            serde_json::to_vec(&cv_core::harness::opensession::document(&session)).unwrap(),
        )
        .unwrap();
    };
    write_doc("the marmot ledger");

    w.cv_ok(&["archive", "sync"]);
    let q = custody_of(&w, "qsess1");
    assert_eq!(q["kind"], "native_extents", "{q}");
    assert_eq!(q["layout"], json!([".qwen", "projects", "-work-proj", "chats"]), "{q}");
    let d = custody_of(&w, "doc-1");
    assert_eq!(d["kind"], "native_extents", "{d}");
    assert_eq!(d["total_bytes"], json!(fs::metadata(&doc_path).unwrap().len()));

    // A rewritten document is a fresh chain of its new bytes, not an append to the old one.
    write_doc("the marmot ledger, amended with a wombat");
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&doc_path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    w.cv_ok(&["archive", "sync"]);
    let d = custody_of(&w, "doc-1");
    assert_eq!(d["extents"].as_array().unwrap().len(), 1, "{d}");
    assert_eq!(d["total_bytes"], json!(fs::metadata(&doc_path).unwrap().len()));

    fs::remove_file(&qwen).unwrap();
    fs::remove_file(&doc_path).unwrap();
    let json = |args: &[&str]| serde_json::from_str::<Value>(&w.cv_ok(args).0).unwrap();
    for (word, expect) in [("capybara", "forty capybaras"), ("wombat", "amended with a wombat")] {
        let hits = json(&["--frozen", "search", word, "--json"]);
        let hits = hits["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{word}: {hits:?}");
        let key = hits[0]["session_key"].as_str().unwrap().to_owned();
        let show = json(&["--frozen", "show", &key, "--json"]).to_string();
        assert!(show.contains(expect), "{word}: {show}");
    }
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
    {
        let conn = rusqlite::Connection::open(w.cv_home.join("state.db")).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let _ = fs::remove_file(w.cv_home.join("state.db-wal"));
    let _ = fs::remove_file(w.cv_home.join("state.db-shm"));
    let tmp = w.base.join("tmpdir");
    fs::create_dir_all(&tmp).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o500)).unwrap();
    }
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let left: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// #tooth Mutating byte 8,192 (strictly outside the 4 KiB head/tail boundary windows) of a
/// 16+ KiB `.opensession.json` at identical file length re-archives a fresh document extent so
/// `--frozen show` serves the updated text after the live file is deleted, and atomic temp-file
/// replacement (`(dev, ino)` change) or same-length middle edits on 16+ KiB JSONL start a fresh
/// 1-extent chain instead of appending to a stale prefix.
#[test]
fn archive_opensession_middle_edit_and_jsonl_inode_or_same_size_edit_recapture_cleanly() {
    let w = world("archive-middle-and-inode");
    let exports = w.base.join("exports");
    fs::create_dir_all(&exports).unwrap();
    w.cv_ok(&["config", "--add-export", exports.to_str().unwrap()]);

    // 1. Build a >16 KiB `.opensession.json` with >8 KiB padding on both sides of the target word
    //    so byte 8,192 sits strictly outside the first and last 4,096 bytes.
    let doc_path = exports.join("mid-doc.opensession.json");
    let make_doc = |middle: &str| {
        assert_eq!(middle.len(), 7);
        let pad_head = "h".repeat(8192);
        let pad_tail = "t".repeat(8192);
        let mut session = cv_core::ir::Session::new("mid-doc", cv_core::ir::Harness::OpenSession);
        let mut m = cv_core::ir::Message::new(cv_core::ir::Role::User);
        m.content.push(cv_core::ir::Block::from_text(
            format!("{pad_head} {middle} {pad_tail}").into(),
        ));
        session.messages.push(m);
        serde_json::to_vec(&cv_core::harness::opensession::document(&session)).unwrap()
    };
    let doc_v1 = make_doc("echidna");
    let doc_v2 = make_doc("wallaby");
    assert_eq!(doc_v1.len(), doc_v2.len(), "identical document length");
    assert!(doc_v1.len() > 16 * 1024);
    assert_eq!(&doc_v1[..4096], &doc_v2[..4096], "first 4 KiB identical");
    assert_eq!(
        &doc_v1[doc_v1.len() - 4096..],
        &doc_v2[doc_v2.len() - 4096..],
        "last 4 KiB identical"
    );
    assert_ne!(doc_v1[8300..8500], doc_v2[8300..8500]);

    fs::write(&doc_path, &doc_v1).unwrap();
    w.cv_ok(&["archive", "sync"]);

    fs::write(&doc_path, &doc_v2).unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&doc_path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    let (sync_out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        sync_out.contains("1 archived"),
        "same-length middle edit in >16 KiB OpenSession doc must re-archive: {sync_out}"
    );
    fs::remove_file(&doc_path).unwrap();

    let json = |args: &[&str]| serde_json::from_str::<Value>(&w.cv_ok(args).0).unwrap();
    let ls = json(&["--frozen", "ls", "--json"]);
    let doc_key = ls
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["harness"] == "opensession")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = json(&["--frozen", "show", &doc_key, "--json"]).to_string();
    assert!(shown.contains("wallaby") && !shown.contains("echidna"), "{shown}");

    // 2. JSONL `(dev, ino)` replacement via temp-file rename (`to > total_bytes` with identical
    //    head/boundary 4 KiB windows, mutated at ~8 KiB) must start a fresh 1-extent chain!
    let jsonl_path = w.transcript("alphasess");
    let make_jsonl = |middle: &str, extra_tail: bool| {
        assert_eq!(middle.len(), 7);
        let head = json!({"type": "user", "uuid": "h1", "timestamp": "2026-02-01T00:00:00Z",
                          "message": {"role": "user", "content": "a".repeat(6000)}});
        let mid = json!({"type": "assistant", "uuid": "m1", "timestamp": "2026-02-01T00:00:01Z",
                         "message": {"role": "assistant", "content": format!("middle-{middle}")}});
        let tail = json!({"type": "user", "uuid": "t1", "timestamp": "2026-02-01T00:00:02Z",
                          "message": {"role": "user", "content": "z".repeat(6000)}});
        let mut s = format!("{head}\n{mid}\n{tail}\n");
        if extra_tail {
            let grown = json!({"type": "assistant", "uuid": "g1", "timestamp": "2026-02-01T00:00:03Z",
                               "message": {"role": "assistant", "content": "post-rename-tail"}});
            s.push_str(&format!("{grown}\n"));
        }
        s
    };
    let j_v1 = make_jsonl("caracal", false);
    fs::write(&jsonl_path, &j_v1).unwrap();
    let bumped2 = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
    fs::File::options()
        .write(true)
        .open(&jsonl_path)
        .and_then(|f| f.set_modified(bumped2))
        .unwrap();
    w.cv_ok(&["archive", "sync"]);

    // Same-length in-place middle edit first: must recapture whole as 1 extent.
    let j_v2_same_len = make_jsonl("servals", false);
    assert_eq!(j_v1.len(), j_v2_same_len.len());
    fs::write(&jsonl_path, &j_v2_same_len).unwrap();
    let bumped3 = std::time::SystemTime::now() + std::time::Duration::from_secs(15);
    fs::File::options()
        .write(true)
        .open(&jsonl_path)
        .and_then(|f| f.set_modified(bumped3))
        .unwrap();
    let (out_same, _) = w.cv_ok(&["archive", "sync"]);
    assert!(out_same.contains("1 archived"), "{out_same}");
    assert_eq!(custody_of(&w, "alphasess")["extents"].as_array().unwrap().len(), 1);

    // Atomic temp-file rename (`new (dev, ino)`) that is ALSO longer (`to > total_bytes`) while
    // matching `[0..4096)` and `[old_len - 4096..old_len)`: must NOT reuse the old inode's prefix!
    let j_v3_renamed = make_jsonl("ocelots", true);
    assert_eq!(&j_v2_same_len.as_bytes()[..4096], &j_v3_renamed.as_bytes()[..4096]);
    assert_eq!(
        &j_v2_same_len.as_bytes()[j_v2_same_len.len() - 4096..],
        &j_v3_renamed.as_bytes()[j_v2_same_len.len() - 4096..j_v2_same_len.len()]
    );
    let tmp_replace = jsonl_path.with_extension("jsonl.tmp");
    fs::write(&tmp_replace, &j_v3_renamed).unwrap();
    fs::rename(&tmp_replace, &jsonl_path).unwrap();
    let (out_ren, _) = w.cv_ok(&["archive", "sync"]);
    assert!(out_ren.contains("1 archived"), "{out_ren}");
    let cust = custody_of(&w, "alphasess");
    assert_eq!(
        cust["extents"].as_array().unwrap().len(),
        1,
        "inode replacement must start a fresh 1-extent chain: {cust}"
    );
    fs::remove_file(&jsonl_path).unwrap();
    let ls2 = json(&["--frozen", "ls", "--json"]);
    let alpha_key = ls2
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str().unwrap().starts_with("alphasess@"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let alpha_shown = json(&["--frozen", "show", &alpha_key, "--json"]).to_string();
    assert!(
        alpha_shown.contains("middle-ocelots")
            && alpha_shown.contains("post-rename-tail")
            && !alpha_shown.contains("middle-servals"),
        "{alpha_shown}"
    );
}

#[cfg(unix)]
#[test]
fn archive_topologies_spool_seal_ssh_pull_delta_and_semiauthoritative_queries() {
    use std::os::unix::fs::PermissionsExt;

    // 1. Remote box (`remote_w`): archive a >120 KiB native JSONL session + betasess.
    let remote_w = world("topo-remote");
    let alpha_path = remote_w.transcript("alphasess");
    let mut big_body = fs::read_to_string(&alpha_path).unwrap();
    let pad = "x".repeat(420);
    for i in 0..250 {
        big_body.push_str(&format!(
            "{}\n",
            json!({
                "type": "user",
                "uuid": format!("ru-{i}"),
                "timestamp": "2026-06-10T10:00:00Z",
                "message": {"role": "user", "content": format!("initial remote turn {i} {pad}")}
            })
        ));
    }
    assert!(big_body.len() > 120 * 1024);
    fs::write(&alpha_path, &big_body).unwrap();
    let (ok, code, out, err) = remote_w.cv_env(&["archive", "sync"], &[("CV_PRODUCER_ID", "remote-box")]);
    assert!(ok, "remote initial sync exited {code}: {out}\n{err}");
    let remote_packs_dir = remote_w.cv_home.join("packs");
    let init_packs = pack_files(&remote_packs_dir);
    assert_eq!(init_packs.len(), 1);

    // 2. Hub (`hub_w`, zero local transcripts): pull Gen 1 from `remote-box` via `--pull` + `--frozen --export-delta`.
    let hub_w = cv_testkit::World::new("topo-hub");
    let fake_ssh = hub_w.base.join("fake-ssh.sh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\n[ \"$5\" = -- ] || exit 2\nshift 6\nHOME=\"$FAKE_REMOTE_HOME\" XDG_CONFIG_HOME=\"$FAKE_REMOTE_HOME/.config\" CLUSTERVISION_HOME=\"$FAKE_REMOTE_CV_HOME\" CV_PRODUCER_ID=\"remote-box\" exec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o755)).unwrap();

    let cv_bin = env!("CARGO_BIN_EXE_cv");
    let pull_env = [
        ("CV_SSH_BIN", fake_ssh.to_str().unwrap()),
        ("CV_REMOTE_BIN", cv_bin),
        ("FAKE_REMOTE_HOME", remote_w.home.to_str().unwrap()),
        ("FAKE_REMOTE_CV_HOME", remote_w.cv_home.to_str().unwrap()),
    ];
    let (ok, code, pull1_out, pull1_err) = hub_w.cv_env(&["archive", "sync", "--pull", "remote-box"], &pull_env);
    assert!(ok, "pull 1 exited {code}: {pull1_out}\n{pull1_err}");
    assert!(
        pull1_out.contains("pulled 2 new event(s)") && pull1_out.contains("generation 1"),
        "{pull1_out}"
    );

    // Append ~4 KiB (`narwhal-delta-marker`) to `alphasess` on `remote_w`, archive only the delta,
    // and DELETE `init_packs[0]` (>120 KiB) on `remote_w` to prove `--pull` reads ONLY the O(delta) pack!
    let appended = format!(
        "{}\n",
        json!({
            "type": "assistant",
            "uuid": "ra-delta-1",
            "timestamp": "2026-06-10T10:05:00Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": format!("narwhal-delta-marker {}", "y".repeat(3500))}, {"type":"tool_use","id":"edit-on-second","name":"Edit","input":{"file_path":"revision-proof.rs","old_string":"a","new_string":"b"}}, {"type":"tool_use","id":"spawn-on-second","name":"Agent","input":{"subagent_type":"general-purpose","description":"Review archived evidence","prompt":"Review archived evidence"}}]}
        })
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&alpha_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, appended.as_bytes()))
        .unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&alpha_path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    let (ok, code, out2, err2) = remote_w.cv_env(&["archive", "sync"], &[("CV_PRODUCER_ID", "remote-box")]);
    assert!(ok, "remote delta sync exited {code}: {out2}\n{err2}");
    // #tooth A different batching window and transport replay the same captured revision.
    let alternate = hub_w.base.join("remote-spool");
    let (ok, _, _, err) = remote_w.cv_env(
        &["archive", "sync", "--spool", alternate.to_str().unwrap()],
        &[("CV_PRODUCER_ID", "remote-box")],
    );
    assert!(ok, "{err}");
    fs::remove_file(&init_packs[0]).unwrap();

    let (ok, code, pull2_out, pull2_err) = hub_w.cv_env(&["archive", "sync", "--pull", "remote-box"], &pull_env);
    assert!(ok, "pull 2 (O(delta)) exited {code}: {pull2_out}\n{pull2_err}");
    assert!(
        pull2_out.contains("pulled 1 new event(s)") && pull2_out.contains("generation 2"),
        "{pull2_out}"
    );

    let (replay, _) = hub_w.cv_ok(&["archive", "sync", "--ingest-spool", alternate.to_str().unwrap()]);
    assert!(replay.contains("0 accepted, 3 replayed"), "{replay}");
    // 3. Credential-free ephemeral sandbox (`sbx_w`): `--spool <dir> --seal` + idempotent `--ingest-spool <dir>`.
    let sbx_w = world("topo-sbx");
    let beta_sbx = fs::read_to_string(sbx_w.transcript("betasess"))
        .unwrap()
        .replace("quokka", "pangolin-sandbox-marker");
    fs::write(sbx_w.transcript("betasess"), beta_sbx).unwrap();
    let spool_dir = hub_w.base.join("sbx-spool");
    let (ok, code, spool_out, spool_err) = sbx_w.cv_env(
        &["archive", "sync", "--spool", spool_dir.to_str().unwrap(), "--seal"],
        &[("CV_PRODUCER_ID", "sbx-ephemeral")],
    );
    assert!(ok, "spool --seal exited {code}: {spool_out}\n{spool_err}");
    assert!(spool_out.contains("[SEALED]"), "{spool_out}");

    let (ing1, _) = hub_w.cv_ok(&["archive", "sync", "--ingest-spool", spool_dir.to_str().unwrap()]);
    assert!(
        ing1.contains("2 accepted, 0 replayed") && ing1.contains("generation 3"),
        "{ing1}"
    );
    let (ing2, _) = hub_w.cv_ok(&["archive", "sync", "--ingest-spool", spool_dir.to_str().unwrap()]);
    assert!(
        ing2.contains("0 accepted, 2 replayed") && ing2.contains("generation 3"),
        "re-ingesting spool must be idempotent: {ing2}"
    );

    // 4. Semiauthoritative CLI queries (`--as-of`, `--place`, `--max-staleness`):
    let hub_json = |args: &[&str]| -> Value { serde_json::from_str(&hub_w.cv_ok(args).0).unwrap() };

    // #tooth Pull and offline ingest publish searchable content and file events at the same cut.
    let indexed = hub_json(&[
        "--frozen",
        "search",
        "narwhal-delta-marker",
        "--place",
        "remote-box",
        "--live-bytes",
        "0",
        "--json",
    ]);
    assert_eq!(indexed["total"], 1, "{indexed}");
    assert_eq!(indexed["authority"]["indexed_generation"], 3, "{indexed}");
    let touched = hub_json(&["touched", "revision-proof.rs", "--place", "remote-box", "--json"]);
    assert_eq!(touched["touched"].as_array().unwrap().len(), 1, "{touched}");
    let spool_hits = hub_json(&[
        "--frozen",
        "search",
        "pangolin-sandbox-marker",
        "--place",
        "sbx-*",
        "--live-bytes",
        "0",
        "--json",
    ]);
    assert_eq!(spool_hits["total"], 1, "{spool_hits}");
    assert_eq!(spool_hits["authority"]["status"], "complete", "{spool_hits}");

    // #tooth The historical file inventory reads the selected revision, even after the later edit arrived.
    for (generation, count) in [("1", 0), ("2", 1)] {
        let touched = hub_json(&["touched", "revision-proof.rs", "--as-of", generation, "--json"]);
        assert_eq!(touched["touched"].as_array().unwrap().len(), count, "{touched}");
    }
    let s_at_1 = hub_json(&["search", "narwhal-delta-marker", "--as-of", "1", "--json"]);
    assert_eq!(s_at_1["total"], 0, "Gen 1 precedes delta append: {s_at_1}");
    assert_eq!(s_at_1["authority"]["generation"], 1);

    let s_at_2 = hub_json(&["search", "narwhal-delta-marker", "--as-of", "2", "--json"]);
    assert_eq!(s_at_2["total"], 1, "Gen 2 includes delta append: {s_at_2}");
    assert_eq!(s_at_2["authority"]["generation"], 2);
    let alpha_hub_sel = s_at_2["hits"][0]["session_key"].as_str().unwrap().to_owned();
    let shown = hub_w.cv_ok(&["--frozen", "show", &alpha_hub_sel, "--json"]).0;
    assert!(
        shown.contains("initial remote turn 0") && shown.contains("narwhal-delta-marker"),
        "hub ExtentChain must serve both Gen 1 initial extent (has={}) and Gen 2 delta extent (has={}): head={}",
        shown.contains("initial remote turn 0"),
        shown.contains("narwhal-delta-marker"),
        &shown[..shown.len().min(800)]
    );

    // #tooth The same native ID exists at two places, with an extra file edit only in revision two.
    for (gen, changed) in [("1", false), ("2", true)] {
        let scoped = |args: &[&str]| {
            let cmd = [args, &["--place", "remote-box", "--as-of", gen, "--json"]].concat();
            let value = hub_json(&cmd);
            assert_eq!(value["authority"]["generation"], gen.parse::<u64>().unwrap(), "{value}");
            value
        };
        let files = scoped(&["show", "alphasess", "--kind", "files"]);
        assert_eq!(
            files["files"].to_string().contains("revision-proof.rs"),
            changed,
            "{files}"
        );
        let tools = scoped(&["tools", "alphasess", "--timeline"]);
        assert_eq!(
            tools["tools"].to_string().contains("revision-proof.rs"),
            changed,
            "{tools}"
        );
        let blame = scoped(&["blame", "revision-proof.rs"]);
        assert_eq!(
            blame["blame"]["event_only_sessions"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty()),
            changed,
            "{blame}"
        );
        let pack = scoped(&[
            "pack",
            "narwhal-delta-marker",
            "--session",
            "alphasess",
            "--excerpt-bytes",
            "500",
        ]);
        assert_eq!(
            pack["pack"]["content"].as_str().unwrap().contains("revision-proof.rs"),
            changed,
            "{pack}"
        );
        let workers = scoped(&["lanes", "alphasess"]);
        assert_eq!(
            workers["lanes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["tool_use_id"] == "spawn-on-second" && row["status"] == "unknown"),
            changed,
            "{workers}"
        );
        assert!(
            workers["lanes"].is_array() && workers["authority"]["status"] == "partial",
            "{workers}"
        );
    }
    let unselected = hub_json(&["pack", "narwhal", "--place", "sbx-*", "--json"]);
    assert!(unselected["pack"]["content"].is_null(), "{unselected}");

    // Unpinned place-scoped `blame` and `pack` use the committed event/text indexes without
    // losing revision-2 evidence, and HTTP `/api/*` serves the identical scoped cuts.
    let current_blame = hub_json(&["blame", "revision-proof.rs", "--place", "remote-box", "--json"]);
    assert_eq!(
        current_blame["blame"]["event_only_sessions"].as_array().unwrap().len(),
        1,
        "{current_blame}"
    );
    let current_pack = hub_json(&[
        "pack",
        "narwhal-delta-marker",
        "--place",
        "remote-box",
        "--excerpt-bytes",
        "500",
        "--json",
    ]);
    assert!(
        current_pack["pack"]["content"]
            .as_str()
            .unwrap()
            .contains("revision-proof.rs"),
        "{current_pack}"
    );
    {
        let scope = (*cv_core::env::Env::current()).clone().enter();
        scope.set("HOME", &hub_w.home);
        scope.set("CLUSTERVISION_HOME", &hub_w.cv_home);
        scope.set("CLUSTERVISION_CACHE", hub_w.base.join("cache"));
        let (st1, f1) = cv_core::api::route_json(
            &["api", "session", "claude", "alphasess", "files"],
            "place=remote-box&as_of=1&authority=1",
        );
        let (st2, f2) = cv_core::api::route_json(
            &["api", "session", "claude", "alphasess", "files"],
            "place=remote-box&as_of=2&authority=1",
        );
        assert_eq!((st1, st2), (200, 200));
        assert!(!f1["files"].to_string().contains("revision-proof.rs"), "{f1}");
        assert!(f2["files"].to_string().contains("revision-proof.rs"), "{f2}");
        let (bst, api_blame) = cv_core::api::route_json(
            &["api", "blame"],
            "file=revision-proof.rs&place=remote-box&as_of=2&authority=1",
        );
        assert_eq!(bst, 200, "{api_blame}");
        assert_eq!(
            api_blame["blame"]["event_only_sessions"].as_array().unwrap().len(),
            1,
            "{api_blame}"
        );
    }

    let never_written = hub_w.base.join("must-not-write-pack.md");
    let (ok_out, code_out, _, err_out) = hub_w.cv(&[
        "pack",
        "narwhal-delta-marker",
        "--out",
        never_written.to_str().unwrap(),
        "--place",
        "absent",
        "--max-staleness",
        "1s",
    ]);
    assert!(!ok_out && code_out == 2 && err_out.contains("absent"), "{err_out}");
    assert!(
        !never_written.exists(),
        "--max-staleness must fail closed before creating --out"
    );
    for cmd in [
        vec!["show", "alphasess", "--kind", "files"],
        vec!["tools", "alphasess"],
        vec!["blame", "x.rs"],
        vec!["pack", "query"],
        vec!["lanes", "alphasess"],
    ] {
        let cmd = [
            cmd.as_slice(),
            &["--place", "absent", "--max-staleness", "1s", "--json"],
        ]
        .concat();
        let (ok, code, _, err) = hub_w.cv(&cmd);
        assert!(!ok && code == 2 && err.contains("absent"), "{cmd:?} {err}");
    }

    // Sealed sandbox at final frontier is `Complete` forever even with `--max-staleness 0s`!
    let sealed_ls = hub_json(&["ls", "--place", "sbx-*", "--max-staleness", "0s", "--json"]);
    assert_eq!(sealed_ls["authority"]["status"], "complete", "{sealed_ls}");
    assert_eq!(sealed_ls["authority"]["places"][0]["state"], "sealed", "{sealed_ls}");
    assert_eq!(sealed_ls["sessions"].as_array().unwrap().len(), 2, "{sealed_ls}");

    // Unsealed disconnected place fails closed with exit code 2 under `--max-staleness 1s`!
    cv_core::state::hub::HubStore::at_home(&hub_w.cv_home)
        .enroll_producer("stale-box", "stale-host", &json!({}))
        .unwrap();
    let (ok, code, _, stale_err) = hub_w.cv(&["ls", "--place", "stale-box", "--max-staleness", "1s"]);
    assert!(
        !ok && code == 2 && stale_err.contains("stale-box"),
        "violated --max-staleness must fail closed with exit 2: code={code} err={stale_err}"
    );
}

/// #tooth Frozen delta export works without scratch files and never creates SQLite sidecars.
/// A receiver accepts the framed events only after exact EOF and transport success.
#[test]
fn archive_export_is_read_only_and_import_requires_complete_transport() {
    let source = world("export-readonly");
    source.cv_ok(&["archive", "sync"]);
    let db = source.cv_home.join("state.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    drop(conn);
    let stored = fs::read(&db).unwrap();
    let no_tempdir = source.base.join("regular-file");
    fs::write(&no_tempdir, b"").unwrap();
    let export = || {
        source
            .cv_command()
            .args(["archive", "sync", "--export-delta", "0"])
            .env("CV_PRODUCER_ID", "exporter")
            .env("TMPDIR", &no_tempdir)
            .output()
            .unwrap()
    };
    let output = export();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(&db).unwrap(), stored);
    for name in ["state.db-wal", "state.db-shm", "state.db-journal"] {
        assert!(!source.cv_home.join(name).exists(), "{name}");
    }
    // The remote can also have an active WAL writer with uncheckpointed commits.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE export_test_row(id INTEGER);")
        .unwrap();
    let files = ["state.db", "state.db-wal", "state.db-shm"].map(|name| source.cv_home.join(name));
    let before = files.each_ref().map(|p| fs::read(p).unwrap());
    let active = export();
    assert!(active.status.success(), "{}", String::from_utf8_lossy(&active.stderr));
    assert_eq!(files.each_ref().map(|p| fs::read(p).unwrap()), before);
    assert_eq!(active.stdout, output.stdout);
    let receiver = cv_testkit::World::new("framed-import");
    let hub = cv_core::state::hub::HubStore::at_home(&receiver.cv_home);
    let mut wire = output.stdout;
    assert!(hub
        .ingest_delta_stream(&wire[..wire.len() - 1], Some("source"))
        .is_err());
    wire.push(0);
    assert!(hub.ingest_delta_stream(wire.as_slice(), Some("source")).is_err());
    wire.pop();
    assert!(hub
        .ingest_delta_stream_checked(wire.as_slice(), Some("source"), || Err(std::io::Error::other(
            "transport closed"
        )))
        .is_err());
    assert_eq!(hub.pull_watermark_for("source").unwrap(), "0");
    assert_eq!(
        hub.ingest_delta_stream(wire.as_slice(), Some("source"))
            .unwrap()
            .events_accepted,
        2
    );
}

/// #tooth Reject option-like SSH destinations and non-literal executable names before spawning SSH.
#[test]
fn archive_pull_validates_ssh_arguments_before_transport() {
    let w = cv_testkit::World::new("pull-arguments");
    for (target, binary, reason) in [
        ("-invalid", "cv", "SSH target"),
        ("source", "two words", "executable path"),
    ] {
        let output = w
            .cv_command()
            .args(["archive", "sync", &format!("--pull={target}")])
            .env("CV_REMOTE_BIN", binary)
            .env("CV_SSH_BIN", w.base.join("absent"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
