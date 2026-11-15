//! Composition of native source revisions, failed replacement and pinned search readers.
use cv_core::harness::antigravity::Antigravity;
use cv_core::Adapter;
use cv_search::{fts, PreparedIndex, SearchFilter};

fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        bytes.push(byte | if value == 0 { 0 } else { 128 });
        if value == 0 {
            return bytes;
        }
    }
}
fn field(number: u64, bytes: &[u8]) -> Vec<u8> {
    [varint((number << 3) | 2), varint(bytes.len() as u64), bytes.to_vec()].concat()
}
fn planner(text: &str) -> Vec<u8> {
    // Synthetic CortexStep PLANNER_RESPONSE, DONE, response text.
    [vec![8, 15, 32, 3], field(20, &field(1, text.as_bytes()))].concat()
}

#[test]
fn native_revision_failure_retains_search_and_retry_replaces_it() {
    let world = cv_testkit::World::in_process("agy-search-revision", cv_core::env::Env::enter_isolated);
    let root = world.base.join("native");
    std::fs::create_dir_all(root.join("conversations")).unwrap();
    let path = root.join("conversations/11111111-2222-4333-8444-555555555555.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER,
         step_payload BLOB, step_format INTEGER DEFAULT 0);",
    )
    .unwrap();
    db.execute("INSERT INTO steps VALUES(0,15,3,?1,0)", [planner("originalnativeword")])
        .unwrap();
    let adapter = Antigravity::with_home(&root);
    let refs = adapter.discover().unwrap();
    assert_eq!(refs.len(), 1);
    let index = world.base.join("index");
    fts::index_sessions(&index, &refs[..], false, false).unwrap();
    let mut pinned = PreparedIndex::open(&index).unwrap();
    let search =
        |reader: &PreparedIndex, word: &str| reader.search(word, 10, SearchFilter::default(), None).unwrap().total;
    assert_eq!(search(&pinned, "originalnativeword"), 1);

    // The body changes, but its new encoding cannot be decoded. A failed read must
    // neither erase the old body nor certify it as this new database revision.
    db.execute(
        "UPDATE steps SET step_payload=?1,step_format=1 WHERE idx=0",
        [planner("replacementnativeword")],
    )
    .unwrap();
    let error = fts::index_sessions(&index, &refs[..], false, false).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported Antigravity native step format 1"));
    assert_eq!(
        fts::text_search_indexed(&index, "originalnativeword", 10)
            .unwrap()
            .len(),
        1
    );
    assert!(fts::text_search_indexed(&index, "replacementnativeword", 10)
        .unwrap()
        .is_empty());

    db.execute("UPDATE steps SET step_format=0 WHERE idx=0", []).unwrap();
    fts::index_sessions(&index, &refs[..], false, false).unwrap();
    assert_eq!(
        fts::text_search_indexed(&index, "replacementnativeword", 10)
            .unwrap()
            .len(),
        1
    );
    assert!(fts::text_search_indexed(&index, "originalnativeword", 10)
        .unwrap()
        .is_empty());
    assert_eq!(search(&pinned, "originalnativeword"), 1);
    pinned.refresh().unwrap();
    assert_eq!(search(&pinned, "replacementnativeword"), 1);
    assert_eq!(search(&pinned, "originalnativeword"), 0);
}
