//! 🧵 Splice / loom — compose a NEW session from spans of existing ones.
//!
//! Because every harness parses into one IR, a transcript is just an ordered list of messages you can
//! cut and recombine. `splice` concatenates message spans from several sessions into a fresh session
//! (re-threaded), and `graft` does the loom move — take a base up to a point, then graft an alternate
//! continuation from another branch. The result is a normal [`Session`] you can `emit` into any harness.
//!
//! OWNED BY the loom work. The `cv loom`/`cv splice` CLI calls these.

use crate::ir::{Harness, Message, Session};

/// A contiguous span of messages from a source session: `[start, end)` (None end → through the last).
pub struct Span<'a> {
    pub source: &'a Session,
    pub start: usize,
    pub end: Option<usize>,
}

impl<'a> Span<'a> {
    /// The clamped, panic-free `[start, end)` range into `source.messages`.
    ///
    /// Out-of-range indices saturate to the message count; an inverted or empty range
    /// (`end <= start`) yields an empty range so iteration produces nothing.
    fn range(&self) -> std::ops::Range<usize> {
        let len = self.source.messages.len();
        let start = self.start.min(len);
        let end = self.end.unwrap_or(len).min(len);
        // Treat end < start (and end == start) as an empty span.
        start..end.max(start)
    }

    /// The messages this span actually selects, in order. Never panics.
    fn messages(&self) -> &'a [Message] {
        &self.source.messages[self.range()]
    }
}

/// Concatenate `spans` (in order) into a new [`Session`] with `harness`, a fresh id (or `new_id`), and
/// linearly re-threaded `parent_id`s. The first span's session provides default cwd/title/model.
pub fn splice(spans: &[Span<'_>], new_id: Option<String>, harness: Harness) -> Session {
    let id = new_id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // Defaults come from the first span's source session (if any).
    let first_source = spans.first().map(|s| s.source);
    let cwd = first_source.and_then(|s| s.cwd.clone());
    let title = first_source.and_then(|s| s.title.clone());
    let model = first_source.and_then(|s| s.model.clone());
    let git = first_source.and_then(|s| s.git.clone());

    // Concatenate the selected messages (cloned) in span order, and record provenance per span.
    let mut messages: Vec<Message> = Vec::new();
    let mut provenance: Vec<serde_json::Value> = Vec::with_capacity(spans.len());
    for span in spans {
        let range = span.range();
        provenance.push(serde_json::json!({
            "source_id": span.source.id,
            "harness": span.source.harness.as_str(),
            "start": range.start,
            "end": range.end,
        }));
        messages.extend(span.messages().iter().cloned());
    }

    // created_at = min, updated_at = max of the included messages' timestamps (when present).
    let created_at = messages.iter().filter_map(|m| m.timestamp).min();
    let updated_at = messages.iter().filter_map(|m| m.timestamp).max();

    // Re-thread into a clean linear chain: each message gets a fresh id, parent_id points at the
    // previous message's new id, the first is None. This makes threading coherent regardless of the
    // sources' original ids (which may collide or reference messages that weren't spliced in).
    let mut prev_id: Option<String> = None;
    for msg in &mut messages {
        let fresh = uuid::Uuid::now_v7().to_string();
        msg.parent_id = prev_id.take();
        msg.id = Some(fresh.clone());
        prev_id = Some(fresh);
    }

    let mut extra = serde_json::Map::new();
    extra.insert("loom".into(), serde_json::Value::Array(provenance));

    Session {
        id,
        harness,
        cwd,
        title,
        created_at,
        updated_at,
        model,
        git,
        messages,
        source_path: None,
        extra,
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    }
}

/// Loom graft: `base[..at]` followed by `graft_src[from..]`, as one new branched session.
///
/// Indices clamp safely: `at` saturates to `base`'s length, `from` to `graft_src`'s length.
/// The new session's harness defaults to `base`'s.
pub fn graft(base: &Session, at: usize, graft_src: &Session, from: usize, new_id: Option<String>) -> Session {
    splice(
        &[
            Span {
                source: base,
                start: 0,
                end: Some(at),
            },
            Span {
                source: graft_src,
                start: from,
                end: None,
            },
        ],
        new_id,
        base.harness,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Message, Role};
    use chrono::{TimeZone, Utc};

    /// Build a session with `n` messages, each tagged with a deterministic id, parent chain, text,
    /// and an increasing timestamp so we can assert created/updated bounds.
    fn session(id: &str, harness: Harness, n: usize) -> Session {
        let mut messages = Vec::new();
        let mut prev: Option<String> = None;
        for i in 0..n {
            let mid = format!("{id}-m{i}");
            let mut m = Message::new(if i % 2 == 0 { Role::User } else { Role::Assistant });
            m.id = Some(mid.clone());
            m.parent_id = prev.take();
            m.timestamp = Some(Utc.timestamp_opt(1_000 + i as i64, 0).unwrap());
            m.content = vec![Block::Text {
                text: format!("{id}#{i}").into(),
            }];
            prev = Some(mid);
            messages.push(m);
        }
        Session {
            id: id.to_string(),
            harness,
            cwd: Some("/tmp/a".into()),
            title: Some(format!("{id} title")),
            created_at: None,
            updated_at: None,
            model: Some("m-1".into()),
            git: None,
            messages,
            source_path: Some("/tmp/orig.jsonl".into()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        }
    }

    fn texts(s: &Session) -> Vec<String> {
        s.messages.iter().filter_map(|m| m.text()).collect()
    }

    /// Assert the session forms a valid linear chain: first parent is None, every other parent_id
    /// equals the previous message's id, and all ids are present and unique.
    fn assert_linear_chain(s: &Session) {
        let mut seen = std::collections::HashSet::new();
        let mut prev: Option<&String> = None;
        for (i, m) in s.messages.iter().enumerate() {
            let id = m.id.as_ref().expect("every spliced message has an id");
            assert!(seen.insert(id.clone()), "ids must be unique");
            match prev {
                None => assert_eq!(m.parent_id, None, "first message has no parent (idx {i})"),
                Some(p) => assert_eq!(
                    m.parent_id.as_ref(),
                    Some(p),
                    "parent_id must point at previous id (idx {i})"
                ),
            }
            prev = m.id.as_ref();
        }
    }

    #[test]
    fn splice_count_order_and_rethread() {
        let a = session("A", Harness::Claude, 4);
        let b = session("B", Harness::Codex, 3);

        // Take A[1..3] then B[0..2].
        let spliced = splice(
            &[
                Span {
                    source: &a,
                    start: 1,
                    end: Some(3),
                },
                Span {
                    source: &b,
                    start: 0,
                    end: Some(2),
                },
            ],
            None,
            Harness::Gemini,
        );

        // (a) count + order.
        assert_eq!(spliced.messages.len(), 4);
        assert_eq!(texts(&spliced), vec!["A#1", "A#2", "B#0", "B#1"]);

        // (b) valid linear chain.
        assert_linear_chain(&spliced);

        // Fresh id generated, harness honored, defaults from first span's source.
        assert!(!spliced.id.is_empty());
        assert_ne!(spliced.id, "A");
        assert_eq!(spliced.harness, Harness::Gemini);
        assert_eq!(spliced.cwd, Some("/tmp/a".into()));
        assert_eq!(spliced.title, Some("A title".into()));
        assert_eq!(spliced.model, Some("m-1".into()));
        assert_eq!(spliced.source_path, None);

        // created/updated bounds from included timestamps:
        // A#1=1001, A#2=1002, B#0=1000, B#1=1001 → min 1000, max 1002.
        assert_eq!(spliced.created_at, Some(Utc.timestamp_opt(1_000, 0).unwrap()));
        assert_eq!(spliced.updated_at, Some(Utc.timestamp_opt(1_002, 0).unwrap()));
    }

    #[test]
    fn provenance_recorded_in_extra() {
        let a = session("A", Harness::Claude, 4);
        let b = session("B", Harness::Codex, 3);
        let spliced = splice(
            &[
                Span {
                    source: &a,
                    start: 1,
                    end: Some(3),
                },
                Span {
                    source: &b,
                    start: 0,
                    end: None,
                },
            ],
            Some("fixed-id".into()),
            Harness::Claude,
        );

        assert_eq!(spliced.id, "fixed-id");
        let loom = spliced.extra.get("loom").expect("loom provenance present");
        let arr = loom.as_array().expect("loom is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["source_id"], "A");
        assert_eq!(arr[0]["harness"], "claude");
        assert_eq!(arr[0]["start"], 1);
        assert_eq!(arr[0]["end"], 3);
        assert_eq!(arr[1]["source_id"], "B");
        assert_eq!(arr[1]["harness"], "codex");
        assert_eq!(arr[1]["start"], 0);
        assert_eq!(arr[1]["end"], 3); // None → clamped to len.
    }

    #[test]
    fn graft_is_base_head_plus_graft_tail() {
        let base = session("BASE", Harness::Claude, 4);
        let src = session("SRC", Harness::Codex, 4);

        // base[..2] + src[2..] → BASE#0, BASE#1, SRC#2, SRC#3.
        let g = graft(&base, 2, &src, 2, None);
        assert_eq!(texts(&g), vec!["BASE#0", "BASE#1", "SRC#2", "SRC#3"]);
        assert_linear_chain(&g);
        assert_eq!(g.harness, Harness::Claude); // defaults to base's.

        let loom = g.extra["loom"].as_array().unwrap();
        assert_eq!(loom[0]["source_id"], "BASE");
        assert_eq!(loom[1]["source_id"], "SRC");
    }

    #[test]
    fn out_of_range_spans_clamp_without_panic() {
        let a = session("A", Harness::Claude, 3);
        let empty = session("E", Harness::Claude, 0);

        let spliced = splice(
            &[
                // start past the end → empty.
                Span {
                    source: &a,
                    start: 99,
                    end: Some(200),
                },
                // end < start → empty span, not a panic.
                Span {
                    source: &a,
                    start: 2,
                    end: Some(1),
                },
                // end past the end → clamps to 3, yields A#1, A#2.
                Span {
                    source: &a,
                    start: 1,
                    end: Some(999),
                },
                // splicing from an empty session → nothing.
                Span {
                    source: &empty,
                    start: 0,
                    end: None,
                },
            ],
            None,
            Harness::Claude,
        );

        assert_eq!(texts(&spliced), vec!["A#1", "A#2"]);
        assert_linear_chain(&spliced);

        // graft with wildly out-of-range indices is also panic-free.
        let g = graft(&a, 999, &empty, 999, None);
        assert_eq!(texts(&g), vec!["A#0", "A#1", "A#2"]);
        assert_linear_chain(&g);

        // Empty result is fine, too.
        let none = splice(&[], None, Harness::Claude);
        assert!(none.messages.is_empty());
        assert_eq!(none.created_at, None);
        assert_eq!(none.updated_at, None);
        assert!(!none.id.is_empty());
    }
}
