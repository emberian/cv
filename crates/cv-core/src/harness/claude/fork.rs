//! The source of a `--fork-session` session. Claude Code copies the source's active chain into a
//! new file under the new `sessionId` and writes no pointer back; the copied records keep their
//! `uuid`s and timestamps, so the shared `uuid` is the link.
//!
//! A fork is recognized from its own head, so an ordinary session pays one short head read:
//!
//! - its first prompt (`type: user` record) is older than the records written before its first
//!   `uuid` record (the fork's own `queue-operation` for the prompt it was started with precedes
//!   the copied history, whose prompts keep their times), or
//! - it begins at a `compact_boundary` (the copy starts at the source's last compaction, so the
//!   source holds that boundary mid-file).
//!
//! Its source is a sibling transcript in the same project directory that was live at the copied
//! record's time (modified since, first record no later), and either begins with the same `uuid`
//! without itself looking copied, or holds the boundary `uuid` past its own first record. Only
//! those few candidates are read whole. Siblings that are themselves forks of the same source are
//! never chosen; a fork of a fork resolves to the original.
//! The first `uuid` record's own time is no signal: a session's SessionStart hook writes its
//! attachment (the first `uuid` record) with a time before the prompt's `queue-operation`, so by
//! that rule most sessions looked copied. The prompt a session takes is written after its queue
//! record; only a copied one is older.

use super::record::Facts;
use crate::harness::store::jsonl;
use crate::stream::Flow;
use chrono::{DateTime, Utc};
#[cfg(test)]
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// How far a head read goes before giving up on finding the first `uuid` record.
const HEAD_LINES: usize = 64;

/// What a transcript's head says: its first `uuid` record and whether it looks copied.
#[derive(Clone)]
struct Head {
    uuid: String,
    at: Option<DateTime<Utc>>,
    /// The first record is a `compact_boundary`.
    boundary: bool,
    /// The head looks copied (see the module docs): it begins at a boundary, or its first prompt
    /// is older than `began`.
    copied: bool,
    /// The newest time written before the first `uuid` record: in a fork, when the fork began, so
    /// every record older than it was copied.
    began: Option<DateTime<Utc>>,
    /// Whether `copied` is final: the head begins at a boundary, holds its first prompt, or ran to
    /// [`HEAD_LINES`]. An unsettled head can still be made copied by an append.
    settled: bool,
}

#[cfg(test)]
thread_local! {
    /// Heads this thread read from disk.
    static HEAD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`read_head`], kept while the file stats as it did when it was read. Fork inference reads the
/// head of every eligible sibling for each session it is asked about, so one pass over a project
/// directory of n copied-looking heads read n² heads: `cv index` over 322 such sessions (when a
/// SessionStart hook's record made a head look copied) opened each transcript ~660 times.
fn head(path: &Path) -> Option<Head> {
    use crate::source::FileId;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, PoisonError};
    type Memo = HashMap<PathBuf, (FileId, Option<Head>)>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);
    let before = FileId::at(path)?;
    if let Some((_, hit)) = memo
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .filter(|(seen, _)| *seen == before)
    {
        return hit.clone();
    }
    let head = read_head(path);
    // A write during the read leaves no entry: the next ask reads the file as it then stands.
    if FileId::at(path) == Some(before) {
        memo.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (before, head.clone()));
    }
    head
}

/// The head through the record codec's byte source and its typed [`Facts`]: at most
/// [`HEAD_LINES`] physical lines, counting blank and rejected ones.
fn read_head(path: &Path) -> Option<Head> {
    #[cfg(test)]
    HEAD_READS.with(|n| n.set(n.get() + 1));
    let read = jsonl::Read::open(path).ok()?;
    let mut newest: Option<DateTime<Utc>> = None;
    let mut found: Option<Head> = None;
    let mut lines = 0;
    let read = read.lines(0, |line| {
        lines += 1;
        if let Some(view) = line.view {
            // Past the first `uuid` record a record that does not parse ends the search for the
            // first prompt, as a head that ended there.
            let parsed = Facts::parse(view);
            let (facts, _) = match (parsed, &found) {
                (Ok(parsed), _) => parsed,
                (Err(_), Some(_)) => return Ok(Flow::Stop),
                (Err(error), None) => return Err(error),
            };
            let at = facts.timestamp.as_deref().and_then(super::parse_ts);
            if let Some(uuid) = facts.uuid {
                let head = found.get_or_insert_with(|| {
                    // A head boundary has no string parent, as decoded by `Facts::parent`.
                    let boundary =
                        facts.subtype.as_deref() == Some("compact_boundary") && facts.parent.flatten().is_none();
                    Head {
                        uuid,
                        at,
                        boundary,
                        copied: boundary,
                        began: newest,
                        settled: boundary,
                    }
                });
                if head.boundary {
                    return Ok(Flow::Stop);
                }
                if facts.ty == "user" {
                    head.copied = matches!((at, head.began), (Some(a), Some(began)) if a < began);
                    head.settled = true;
                    return Ok(Flow::Stop);
                }
            } else if found.is_none() {
                if let Some(a) = at {
                    newest = Some(newest.map_or(a, |n| n.max(a)));
                }
            }
        }
        Ok(if lines == HEAD_LINES {
            if let Some(head) = found.as_mut() {
                head.settled = true;
            }
            Flow::Stop
        } else {
            Flow::Continue
        })
    });
    // A read failure past the first `uuid` record leaves the head it found.
    if read.is_err() && found.is_none() {
        return None;
    }
    found
}

use crate::harness::file_mtime as modified;

/// Whether `path` holds `needle` anywhere, over bounded windows that keep the needle's length
/// minus one byte between reads.
fn holds(path: &Path, needle: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    if needle.is_empty() {
        return true;
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut window = Vec::new();
    loop {
        let Ok(bytes) = reader.fill_buf() else {
            return false;
        };
        if bytes.is_empty() {
            return false;
        }
        window.extend_from_slice(bytes);
        if memchr::memmem::find(&window, needle.as_bytes()).is_some() {
            return true;
        }
        let n = bytes.len();
        reader.consume(n);
        window.drain(..window.len() - (needle.len() - 1).min(window.len()));
    }
}

/// When the fork at `path` began, so its records written earlier are its source's copied turns;
/// `None` unless a source is found ([`source_tracked`]).
pub(super) fn copied_before(path: &Path, sink: &mut dyn crate::MessageSink) -> Option<DateTime<Utc>> {
    let (_, began) = source_tracked(path, sink)?;
    began
}

/// The session id `path` was forked from, when its head says it is a fork and a sibling holds the
/// records it copied.
#[cfg(test)]
pub(super) fn source(path: &Path) -> Option<String> {
    source_tracked(path, &mut crate::stream::Discard).map(|(id, _)| id)
}

/// The source's session id and `path`'s own head's begin time, from one head read.
///
/// The answer depends on the directory's entries and, of each sibling, on its head, whether it
/// was live at the copied record's time, and (a boundary match) whether it holds the boundary.
/// A transcript is append-only: a head that holds a `uuid` record never changes, and a source
/// wrote the boundary before the fork copied it, so bytes appended since cannot make a sibling
/// the source. Every sibling is therefore an append-only dependency (its growth leaves the
/// answer; a rewrite of its sampled bytes does not), except a live one without a `uuid` head yet
/// (an append can give it one) or a candidate whose head has not reached its first prompt (an
/// append can make it copied); the directory is exact (a new or removed sibling). An append to a
/// sibling rereads no fork of the directory.
pub(super) fn source_tracked(
    path: &Path,
    sink: &mut dyn crate::MessageSink,
) -> Option<(String, Option<DateTime<Utc>>)> {
    let me = head(path)?;
    if !me.copied {
        return None;
    }
    let at = me.at?;
    let dir = path.parent()?;
    sink.dependency(dir); // A new/deleted eligible sibling can change the inferred source.
    let mut candidates: Vec<(DateTime<Utc>, PathBuf, Head)> = Vec::new();
    for p in fs::read_dir(dir).ok()?.flatten().map(|e| e.path()) {
        let eligible = p != path
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".jsonl") && !n.ends_with(".flat.jsonl"));
        if !eligible {
            continue;
        }
        if !p.is_file() {
            sink.dependency(&p);
            continue;
        }
        // Live at the copied record's time: written since.
        if !modified(&p).is_some_and(|m| m >= at) {
            sink.append_only_dependency(&p);
            continue;
        }
        let Some(h) = head(&p) else {
            sink.dependency(&p);
            continue;
        };
        match h.at {
            Some(first) if first <= at => candidates.push((first, p, h)),
            _ => sink.append_only_dependency(&p),
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut source = None;
    for (_, p, h) in &candidates {
        // A candidate whose head has not settled can be made copied (no source) by an append.
        if h.settled {
            sink.append_only_dependency(p);
        } else {
            sink.dependency(p);
        }
        // Only the first match is read for: later candidates' bytes are never searched.
        let matched = source.is_none()
            && if h.uuid == me.uuid {
                !h.copied
            } else {
                me.boundary && holds(p, &me.uuid)
            };
        if matched {
            source = p.file_stem().and_then(|s| s.to_str()).map(str::to_string);
        }
    }
    source.map(|id| (id, me.began))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, lines: &[Value]) -> PathBuf {
        let p = dir.join(name);
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(&p, text.join("\n") + "\n").unwrap();
        p
    }

    fn rec(uuid: &str, parent: Option<&str>, ts: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "timestamp": ts,
               "message": {"role": "user", "content": "hi"}})
    }

    fn queued(ts: &str) -> Value {
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": ts})
    }

    #[test]
    fn projected_heads_match_value_validation_and_physical_line_limits() {
        let w = crate::test_world("fork-head-projection");
        let path = w.base.join("head.jsonl");
        let uuid = r#"{"uuid":"shared","timestamp":"2026-09-19T00:00:00Z"}"#;
        let queued = r#"{"timestamp":"2026-09-19T01:00:00Z"}"#;
        let mut cases: Vec<Vec<u8>> = vec![
            format!("{queued}\n{uuid}\n").into_bytes(),
            format!("{}{uuid}\n", "\n".repeat(63)).into_bytes(),
            format!("{}{uuid}\n", "\n".repeat(64)).into_bytes(),
            format!("malformed\nnull\n[1,2]\n{uuid}\n").into_bytes(),
            format!("\u{a0}{queued}\n{uuid}\n").into_bytes(),
            format!("{{\"uuid\":\"bad\",\"ignored\":1e400}}\n{uuid}\n").into_bytes(),
            format!("{{\"uuid\":\"bad\",\"ignored\":\"\\ud800\"}}\n{uuid}\n").into_bytes(),
            br#"{"uuid":"first","uuid":17}
{"uuid":"last","uuid":"final","subtype":"compact_boundary","parentUuid":null}
"#
            .to_vec(),
            br#"{"uuid":"parent","subtype":"compact_boundary","parentUuid":false}
"#
            .to_vec(),
            br#"{"uuid":"escaped\u0031","timestamp":[],"parentUuid":null}
"#
            .to_vec(),
        ];
        cases.push(format!("{{\"uuid\":\"giant\",\"message\":\"{}\"}}\n", "payload".repeat(150_000)).into_bytes());
        cases.push(
            format!(
                "{{\"uuid\":\"escaped\",\"message\":\"{}\"}}\n",
                "\\u0061".repeat(150_000)
            )
            .into_bytes(),
        );
        // Frozen from the retired line/`Value` head reader (decision 11: goldens, not oracles).
        let mut frozen = crate::emit::frozen::Frozen::new("claude-fork-heads");
        for (i, bytes) in cases.iter().enumerate() {
            fs::write(&path, bytes).unwrap();
            let observed = read_head(&path).map(|h| (h.uuid, h.at, h.boundary, h.copied, h.began));
            frozen.insert(format!("case {i}"), serde_json::to_value(observed).unwrap());
        }
        frozen.check().unwrap();
        // An invalid UTF-8 line is one rejected record, not the end of the file.
        fs::write(&path, [b"\xff\n".as_slice(), uuid.as_bytes()].concat()).unwrap();
        assert_eq!(read_head(&path).map(|h| h.uuid).as_deref(), Some("shared"));
        fs::write(&path, [b"prefix\n\xff\n".as_slice(), b"needle\n"].concat()).unwrap();
        assert!(holds(&path, "needle"));
    }

    /// An uncompacted source: the fork starts with the same `uuid`, after its own newer queue
    /// record. The source, a sibling fork, and an unrelated session are told apart.
    #[test]
    fn a_fork_of_an_uncompacted_session_names_its_source() {
        let dir = std::env::temp_dir().join(format!("cv-claude-fork-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let t0 = "2026-10-11T20:00:00Z";
        let src = write(
            &dir,
            "src.jsonl",
            &[
                queued(t0),
                rec("u0", None, t0),
                rec("u1", Some("u0"), "2026-10-11T20:00:05Z"),
            ],
        );
        let fork_lines = [
            json!({"type": "custom-title", "customTitle": "t"}),
            queued("2026-10-11T21:00:00Z"),
            rec("u0", None, t0),
            rec("u1", Some("u0"), "2026-10-11T20:00:05Z"),
            rec("f1", Some("u1"), "2026-10-11T21:00:01Z"),
        ];
        let fork = write(&dir, "fork.jsonl", &fork_lines);
        let sibling = write(&dir, "sibling.jsonl", &fork_lines);
        write(&dir, "other.jsonl", &[queued(t0), rec("o0", None, t0)]);
        assert_eq!(source(&fork).as_deref(), Some("src"));
        assert_eq!(source(&sibling).as_deref(), Some("src"));
        assert_eq!(source(&src), None, "the source's head is not a copy");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A session's SessionStart hook writes its attachment (the first `uuid` record) with a time
    /// before the prompt's queue record, and the prompt after it: such a head does not look
    /// copied, so no sibling is read for it. A fork copies the source's attachment and prompt with
    /// their times and names that source, which no longer looks copied itself.
    #[test]
    fn a_hook_record_older_than_the_queue_is_no_fork() {
        let dir = std::env::temp_dir().join(format!("cv-claude-fork-hook-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hook = |uuid: &str, ts: &str| {
            json!({"type": "attachment", "uuid": uuid, "parentUuid": null, "timestamp": ts,
                   "attachment": {"type": "hook_success"}})
        };
        let session = |name: &str, i: usize| {
            write(
                &dir,
                name,
                &[
                    queued("2026-10-19T23:08:43.336Z"),
                    hook(&format!("a{i}"), "2026-10-19T23:08:43.329Z"),
                    rec(&format!("u{i}"), Some(&format!("a{i}")), "2026-10-19T23:08:43.346Z"),
                ],
            )
        };
        let src = session("src.jsonl", 0);
        let plain: Vec<PathBuf> = (1..8).map(|i| session(&format!("s{i}.jsonl"), i)).collect();
        let fork = write(
            &dir,
            "fork.jsonl",
            &[
                queued("2026-10-19T23:30:00Z"),
                hook("a0", "2026-10-19T23:08:43.329Z"),
                rec("u0", Some("a0"), "2026-10-19T23:08:43.346Z"),
                rec("f1", Some("u0"), "2026-10-19T23:30:01Z"),
            ],
        );
        for p in plain.iter().chain([&src]) {
            assert!(!read_head(p).unwrap().copied, "{}", p.display());
            let before = HEAD_READS.with(std::cell::Cell::get);
            assert_eq!(source(p), None);
            assert!(
                HEAD_READS.with(std::cell::Cell::get) - before <= 1,
                "no sibling is read"
            );
        }
        assert!(read_head(&fork).unwrap().copied);
        assert_eq!(source(&fork).as_deref(), Some("src"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A directory of copied-looking heads (each session's first prompt is older than its queue
    /// record) asks fork inference about every sibling once per session; each head is read from
    /// disk once per file state, not once per asking session, and an append rereads that one head.
    #[test]
    fn each_sibling_head_is_read_once_per_file_state() {
        let dir = std::env::temp_dir().join(format!("cv-claude-fork-heads-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let n = 24;
        let paths: Vec<PathBuf> = (0..n)
            .map(|i| {
                write(
                    &dir,
                    &format!("s{i:02}.jsonl"),
                    &[
                        queued("2026-10-11T21:00:01Z"),
                        rec(&format!("u{i}"), None, "2026-10-11T21:00:00Z"),
                    ],
                )
            })
            .collect();
        let reads = || HEAD_READS.with(std::cell::Cell::get);
        let start = reads();
        for p in &paths {
            assert_eq!(source(p), None, "no sibling begins with another's uuid");
        }
        assert_eq!(
            reads() - start,
            n,
            "one head read per transcript, not one per asking session"
        );
        let again = reads();
        for p in &paths {
            assert_eq!(source(p), None);
        }
        assert_eq!(reads() - again, 0, "unchanged heads are not reread");
        let mut f = fs::OpenOptions::new().append(true).open(&paths[3]).unwrap();
        std::io::Write::write_all(&mut f, b"{\"type\":\"user\"}\n").unwrap();
        drop(f);
        let after = reads();
        for p in &paths {
            assert_eq!(source(p), None);
        }
        assert_eq!(reads() - after, 1, "the appended transcript's head alone is reread");
        let _ = fs::remove_dir_all(&dir);
    }
}
