//! 🧵 Splice — compose a NEW session from spans of existing ones.
//!
//! Because every harness parses into one IR, a transcript is just an ordered list of messages you can
//! cut and recombine. A [`Composition`] concatenates message windows from several sessions into a
//! fresh [`Session`] with provenance; a verbatim prefix (`a:..N`) forks a lane at message N, and two spans
//! (`base:..N other:M..`) graft one continuation onto another's head. The result is a normal session
//! you `emit` into any harness.
//!
//! Splice is a **selector**: it chooses messages and nothing more. Threading, open tool calls and
//! sidechain identity are the Claude emitter's rules (`harness::claude::emit::emit`): a span's source
//! parents that still resolve are kept, the cut points are re-linked, a span lifted from a sub-agent
//! joins the main thread. `cv splice` calls this.
//!
//! A selected message keeps the record it was read from (`emit::prepare_replayable`,
//! [`Demand::records`](crate::stream::Demand)), so a span emitted into its own harness is
//! written back as it was: a person's mid-turn message and an attached file are `attachment`
//! records in a Claude transcript, and they arrive as those records. A record is only its own
//! harness's: [`Composition::finish`] drops the ones the target could not replay.

use crate::ir::{GitInfo, Harness, Message, Principal, Session};
use std::path::PathBuf;

/// The first window's source facts: its harness (which may differ from its discovery reference —
/// OpenSession documents preserve their named underlying harness) and the session metadata that
/// carries to the composition.
struct Owner {
    harness: Harness,
    principal: Option<Principal>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    model: Option<String>,
    git: Option<GitInfo>,
}

/// Incremental composition of prepared windows. Each source owner is consumed immediately;
/// selected messages move into the composition without cloning their bodies.
#[derive(Default)]
pub struct Composition {
    owner: Option<Owner>,
    messages: Vec<Message>,
    provenance: Vec<serde_json::Value>,
    /// Each pushed window's harness and how many messages it contributed, in push order.
    windows: Vec<(Harness, usize)>,
}
impl Composition {
    pub fn push(&mut self, mut source: Session, total: usize, start: usize, end: Option<usize>) {
        let start = start.min(total);
        let end = end.unwrap_or(total).min(total).max(start);
        self.provenance.push(serde_json::json!({
            "source_id": source.id, "harness": source.harness.as_str(), "start": start, "end": end,
        }));
        self.windows.push((source.harness, source.messages.len()));
        self.messages.append(&mut source.messages);
        match &mut self.owner {
            Some(owner) => {
                if owner.principal != source.principal {
                    owner.principal = None;
                }
            }
            None => {
                self.owner = Some(Owner {
                    harness: source.harness,
                    principal: source.principal.take(),
                    cwd: source.cwd.take(),
                    title: source.title.take(),
                    model: source.model.take(),
                    git: source.git.take(),
                });
            }
        }
    }
    /// The first prepared session's harness, which may differ from its discovery reference
    /// (OpenSession documents preserve their named underlying harness).
    pub fn first_harness(&self) -> Option<Harness> {
        self.owner.as_ref().map(|owner| owner.harness)
    }
    pub fn finish(self, new_id: Option<String>, harness: Harness) -> Session {
        let mut messages = self.messages;
        // A record replays only into the harness that wrote it. For any other target the
        // message goes as its IR, and that target's writer says what it does with it.
        let mut at = 0;
        for (source, len) in self.windows {
            if source != harness {
                for message in &mut messages[at..at + len] {
                    if message.kind != crate::ir::MessageKind::Carrier {
                        message.extra.remove(crate::ir::CARRIER_KEY);
                    }
                }
            }
            at += len;
        }
        // Provenance is cv's own fact about a session cv synthesized, not something read out of a
        // harness store, so it lives in the `cv` namespace — `Session::extra` has no flat keys
        // (docs/INTERFACE-V2.md §4).
        let mut extra = serde_json::Map::new();
        extra.insert(
            crate::ir::CV_NAMESPACE.into(),
            serde_json::json!({ "loom": serde_json::Value::Array(self.provenance) }),
        );
        let (principal, cwd, title, model, git) = match self.owner {
            Some(mut owner) => (
                owner.principal.take(),
                owner.cwd.take(),
                owner.title.take(),
                owner.model.take(),
                owner.git.take(),
            ),
            None => (None, None, None, None, None),
        };
        Session {
            // created_at = min, updated_at = max of the included messages' timestamps.
            created_at: messages.iter().filter_map(|m| m.timestamp).min(),
            updated_at: messages.iter().filter_map(|m| m.timestamp).max(),
            messages,
            extra,
            cwd,
            title,
            model,
            git,
            principal,
            ..Session::new(new_id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string()), harness)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ir::{Block, Message, Principal, Role};
    use chrono::{TimeZone, Utc};

    /// Compose owned windows `(source, start, end)` the way `cv splice` composes prepared ones.
    pub(crate) fn compose(spans: &[(&Session, usize, Option<usize>)], id: Option<String>, harness: Harness) -> Session {
        let mut composition = Composition::default();
        for &(source, start, end) in spans {
            let total = source.messages.len();
            let from = start.min(total);
            let mut selected = source.clone();
            selected.messages = source.messages[from..end.unwrap_or(total).min(total).max(from)].to_vec();
            composition.push(selected, total, start, end);
        }
        composition.finish(id, harness)
    }

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
            cwd: Some("/tmp/a".into()),
            title: Some(format!("{id} title")),
            model: Some("m-1".into()),
            messages,
            source_path: Some("/tmp/orig.jsonl".into()),
            ..Session::new(id, harness)
        }
    }

    fn texts(s: &Session) -> Vec<String> {
        s.messages.iter().filter_map(|m| m.text()).collect()
    }

    #[test]
    fn composition_moves_selected_bodies_without_copying() {
        let a = session("A", Harness::Claude, 6);
        let mut builder = Composition::default();
        let mut selected = a.clone();
        selected.messages.drain(..1);
        let pointers: Vec<_> = selected
            .messages
            .iter()
            .map(|message| match &message.content[0] {
                Block::Text {
                    text: crate::Text::Inline(text),
                } => text.as_ptr(),
                _ => unreachable!(),
            })
            .collect();
        builder.push(selected, 6, 1, None);
        let actual = builder.finish(Some("fixed".into()), Harness::Gemini);
        for (message, pointer) in actual.messages.iter().zip(pointers) {
            let Block::Text {
                text: crate::Text::Inline(text),
            } = &message.content[0]
            else {
                unreachable!()
            };
            assert_eq!(
                text.as_ptr(),
                pointer,
                "composition must move selected body allocations"
            );
        }
    }

    #[test]
    fn splice_count_order_and_defaults() {
        let a = session("A", Harness::Claude, 4);
        let b = session("B", Harness::Codex, 3);

        // Take A[1..3] then B[0..2].
        let spliced = compose(&[(&a, 1, Some(3)), (&b, 0, Some(2))], None, Harness::Gemini);

        // (a) count + order.
        assert_eq!(spliced.messages.len(), 4);
        assert_eq!(texts(&spliced), vec!["A#1", "A#2", "B#0", "B#1"]);

        // (b) a selector copies messages as they are: source ids and parents ride along, and the
        // emitter re-links the cut points (`harness::claude::emit::emit`).
        assert_eq!(spliced.messages[0].id.as_deref(), Some("A-m1"));
        assert_eq!(spliced.messages[0].parent_id.as_deref(), Some("A-m0"));
        assert_eq!(spliced.messages[2].parent_id, None);

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
        let spliced = compose(
            &[(&a, 1, Some(3)), (&b, 0, None)],
            Some("fixed-id".into()),
            Harness::Claude,
        );

        assert_eq!(spliced.id, "fixed-id");
        // A synthesized session obeys the same `extra` rule as a parsed one: no flat keys, every
        // top-level key a namespace object. `loom` used to sit flat here, which quietly made the
        // contract's "a session's extra has no flat keys at all" false for our own output.
        crate::harness::assert_no_flat_keys(&spliced);
        let loom = spliced
            .cv_extra()
            .expect("cv namespace present")
            .get("loom")
            .expect("loom provenance");
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
    fn out_of_range_spans_clamp_without_panic() {
        let a = session("A", Harness::Claude, 3);
        let empty = session("E", Harness::Claude, 0);
        // Start past the end, end before start, end past the end, an empty source.
        let spliced = compose(
            &[
                (&a, 99, Some(200)),
                (&a, 2, Some(1)),
                (&a, 1, Some(999)),
                (&empty, 0, None),
            ],
            None,
            Harness::Claude,
        );
        assert_eq!(texts(&spliced), vec!["A#1", "A#2"]);
        let provenance = &spliced.cv_extra().unwrap()["loom"];
        assert_eq!(provenance[0]["start"], 3);
        assert_eq!(provenance[0]["end"], 3);
        assert_eq!(provenance[1]["start"], 2);
        assert_eq!(provenance[1]["end"], 2);

        // Empty result is fine, too.
        let none = compose(&[], None, Harness::Claude);
        assert!(none.messages.is_empty());
        assert_eq!(none.created_at, None);
        assert_eq!(none.updated_at, None);
        assert!(!none.id.is_empty());
    }

    #[test]
    fn composition_requires_unanimous_explicit_principals() {
        let mut first = Session::new("first", Harness::OpenSession);
        first.principal = Some(Principal::String("opaque-owner".into()));
        let mut other = first.clone();
        other.id = "other".into();
        for principal in [
            first.principal.clone(),
            None,
            Some(Principal::String("different-owner".into())),
        ] {
            other.principal = principal;
            let expected = (first.principal == other.principal)
                .then(|| first.principal.clone())
                .flatten();
            // Empty spans still name contributing sources; absence never acquires an owner.
            let mut moved = Composition::default();
            moved.push(first.clone(), 0, 0, None);
            moved.push(other.clone(), 0, 0, None);
            assert_eq!(moved.finish(None, Harness::OpenSession).principal, expected);
        }
        assert_eq!(
            compose(&[(&first, 0, None)], None, Harness::OpenSession).principal,
            first.principal
        );
    }
}
