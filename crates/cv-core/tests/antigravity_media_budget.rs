//! End-to-end allocation regression for native inline media. The allocator counts
//! live Rust allocations; SQLite/C allocation, mappings, and stacks are separate.
#![cfg(feature = "sqlite")]
use cv_core::harness::antigravity::Antigravity;
use cv_core::{Block, Demand, Flow, Harness, Message, MessageSink, SessionRef, Tier};
use std::io::Write;
use std::path::{Path, PathBuf};

#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
const ID: &str = "cccccccc-2222-4333-8444-555555555555";

fn varint(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 127) as u8;
        n >>= 7;
        out.push(byte | if n == 0 { 0 } else { 128 });
        if n == 0 {
            return out;
        }
    }
}
fn header(field: usize, len: usize) -> Vec<u8> {
    [varint(field << 3 | 2), varint(len)].concat()
}
fn field(field: usize, bytes: &[u8]) -> Vec<u8> {
    [header(field, bytes.len()), bytes.to_vec()].concat()
}
fn database(root: &Path, id: &str, len: usize) -> PathBuf {
    let dir = root.join("conversations");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.db"));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER DEFAULT 0);",
    )
    .unwrap();
    let mime = field(1, b"image/png");
    let media = [mime, header(2, len)].concat();
    let input = [header(9, media.len() + len), media].concat();
    let prefix = [vec![8, 14, 32, 3], header(19, input.len() + len), input].concat();
    conn.execute(
        "INSERT INTO steps(idx,step_payload) VALUES (0,zeroblob(?1))",
        [prefix.len() + len],
    )
    .unwrap();
    let mut blob = conn
        .blob_open(rusqlite::MAIN_DB, "steps", "step_payload", 0, false)
        .unwrap();
    blob.write_all(&prefix).unwrap();
    let chunk = [b'x'; 8192];
    let mut remaining = len;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        blob.write_all(&chunk[..n]).unwrap();
        remaining -= n;
    }
    drop(blob);
    drop(conn);
    path
}
fn image(session: &cv_core::Session) -> &str {
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|block| match block {
            Block::Image { media_type, data_ref } => {
                assert_eq!(media_type.as_deref(), Some("image/png"));
                data_ref.as_deref()
            }
            _ => None,
        })
        .expect("image remains first-class")
}

// One test keeps global allocation observation isolated from parallel test fixtures.
#[test]
fn native_image_counter_is_bounded_and_public_payloads_remain_complete() {
    let world = cv_testkit::World::in_process("agy-media-budget", cv_core::env::Env::enter_isolated);
    let large_root = world.base.join("large");
    let reference = SessionRef::new(ID, Harness::Antigravity, database(&large_root, ID, 24 << 20));
    let adapter = Antigravity::with_home(&large_root);
    struct Images(usize, bool);
    impl MessageSink for Images {
        fn tier(&self) -> Tier {
            Tier::Facts
        }
        fn message(&mut self, message: Message) -> Flow {
            for block in message.content {
                if let Block::Image { media_type, data_ref } = block {
                    assert_eq!(media_type.as_deref(), Some("image/png"));
                    self.1 &= data_ref.as_deref() == Some("base64:inline");
                    self.0 += 1;
                }
            }
            Flow::Continue
        }
    }
    let mut images = Images(0, true);
    let mut discard = cv_core::stream::Discard;
    let mut events = cv_core::events::EventSink::new(None);
    let mut reducers = cv_core::TeeSink::new(&mut discard, &mut events);
    let mut sink = cv_core::TeeSink::new(&mut reducers, &mut images);
    let before = cv_testkit::heap::begin();
    let read = cv_core::stream::stream_counted(
        &adapter,
        &reference,
        &Demand::LEAN.with_spans(),
        &mut sink,
        cv_core::stream::Pass::default(),
    )
    .unwrap();
    let heap = cv_testkit::heap::end(before);
    drop(sink);
    drop(reducers);
    assert!(read.counted.is_some());
    assert_eq!(images.0, 1);
    println!(
        "inline_image_bytes={} peak_rust_bytes={}",
        24 << 20,
        heap.peak_above_baseline_bytes
    );
    assert!(
        heap.peak_above_baseline_bytes < 8 << 20,
        "counting allocated {} bytes for a 24 MiB inline image",
        heap.peak_above_baseline_bytes
    );

    assert!(images.1, "reducer did not receive the existing inline-media marker");

    // Consumers that read ids, tool names or text over a span read never need the bytes.
    let before = cv_testkit::heap::begin();
    assert_eq!(cv_core::api::message_index(&adapter, &reference, "0").unwrap(), Some(0));
    assert_eq!(
        cv_core::api::message_index(&adapter, &reference, "absent").unwrap(),
        None
    );
    let tools = cv_core::tools::ToolHistogram::from_stream(&adapter, &reference).unwrap();
    assert_eq!(tools.distinct(), 0);
    let heap = cv_testkit::heap::end(before);
    assert!(
        heap.peak_above_baseline_bytes < 8 << 20,
        "id and tool lookups allocated {} bytes for a 24 MiB inline image",
        heap.peak_above_baseline_bytes
    );

    // gen_metadata rows carry whole model requests (12 MiB seen in a real store). A session
    // header needs a model slug and step indices from them; discovery needs neither.
    let meta_root = world.base.join("metadata");
    let path = database(&meta_root, ID, 3);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB, size INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
    // GeneratorMetadata: chat_model(1){ request(2): 16 MiB, model slug(19) }, step_indices(2) = 0.
    let request = 16usize << 20;
    let slug = field(19, b"synthetic-model");
    let prefix = [
        header(1, header(2, request).len() + request + slug.len()),
        header(2, request),
    ]
    .concat();
    let suffix = [slug, vec![16, 0]].concat();
    conn.execute(
        "INSERT INTO gen_metadata(idx,data) VALUES (0,zeroblob(?1))",
        [prefix.len() + request + suffix.len()],
    )
    .unwrap();
    let mut blob = conn
        .blob_open(rusqlite::MAIN_DB, "gen_metadata", "data", 0, false)
        .unwrap();
    blob.write_all(&prefix).unwrap();
    for _ in 0..request / 8192 {
        blob.write_all(&[b'x'; 8192]).unwrap();
    }
    blob.write_all(&suffix).unwrap();
    drop(blob);
    drop(conn);
    let adapter = Antigravity::with_home(&meta_root);
    let before = cv_testkit::heap::begin();
    let found = cv_core::Adapter::discover(&adapter).unwrap();
    let heap = cv_testkit::heap::end(before);
    assert_eq!(found.len(), 1);
    assert!(
        heap.peak_above_baseline_bytes < 4 << 20,
        "discovery allocated {} bytes beside a 16 MiB metadata row",
        heap.peak_above_baseline_bytes
    );
    let before = cv_testkit::heap::begin();
    let session = cv_core::stream::collect_with(&adapter, &found[0], &Demand::LEAN.with_spans()).unwrap();
    let heap = cv_testkit::heap::end(before);
    assert_eq!(session.model.as_deref(), Some("synthetic-model"));
    assert!(
        heap.peak_above_baseline_bytes < 4 << 20,
        "a session header allocated {} bytes for a 16 MiB metadata row",
        heap.peak_above_baseline_bytes
    );

    // Public payload requests use the existing String IR and retain actual data,
    // even when the public API internally uses a span-capable read.
    let root = world.home.join(".gemini/antigravity-cli");
    let reference = SessionRef::new(ID, Harness::Antigravity, database(&root, ID, 3));
    let adapter = Antigravity::with_home(&root);
    let expected = "data:image/png;base64,eHh4";
    for demand in [Demand::LEAN, Demand::LEAN.with_spans(), Demand::RICH, Demand::COMPLETE] {
        let session = cv_core::stream::collect_with(&adapter, &reference, &demand).unwrap();
        assert_eq!(image(&session), expected);
        let markdown = cv_core::render::to_markdown(&session).unwrap();
        assert!(markdown.contains("_[image]_"), "existing Markdown image marker changed");
    }
    for extra in [false, true] {
        let (status, reply) = cv_core::api::route_json(
            &["api", "session", "antigravity", ID, "messages"],
            &format!("extra={extra}"),
        );
        assert_eq!(status, 200);
        assert_eq!(reply["messages"][0]["content"][0]["data_ref"], expected);
    }
    let (status, head) = cv_core::api::route_json(&["api", "session", "antigravity", ID, "head"], "");
    assert_eq!(status, 200);
    assert_eq!(head["total"], 1);
}
