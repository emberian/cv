use super::*;
use crate::ir::{GitInfo, Harness, Message, Role};
use std::path::PathBuf;

fn refx(id: &str, h: Harness, cwd: Option<&str>, title: Option<&str>, msgs: usize) -> SessionRef {
    SessionRef {
        id: id.into(),
        harness: h,
        path: PathBuf::from("/tmp/x"),
        cwd: cwd.map(PathBuf::from),
        title: title.map(str::to_string),
        created_at: None,
        updated_at: "2026-06-01T00:00:00Z".parse().ok(),
        message_count: msgs,
    }
}

fn q(s: &str) -> SessionQuery {
    SessionQuery::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e}"))
}

#[test]
fn empty_matches_all() {
    let query = q("");
    assert!(query.is_empty());
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 0)));
}

#[test]
fn implicit_and() {
    let query = q("harness:claude msgs>=5");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 10)));
    assert!(!query.prefilter(&refx("a", Harness::Codex, None, None, 10)));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, None, 4)));
}

#[test]
fn or_and_grouping() {
    let query = q("(harness:claude OR harness:codex) msgs>=5");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 5)));
    assert!(query.prefilter(&refx("a", Harness::Codex, None, None, 5)));
    assert!(!query.prefilter(&refx("a", Harness::Grok, None, None, 5)));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, None, 4)));
}

#[test]
fn negation_both_forms() {
    for s in ["-harness:codex", "NOT harness:codex"] {
        let query = q(s);
        assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 1)), "{s}");
        assert!(!query.prefilter(&refx("a", Harness::Codex, None, None, 1)), "{s}");
    }
}

#[test]
fn or_pipe_and_keyword_equivalent() {
    assert_eq!(
        q("harness:claude | harness:codex").expr,
        q("harness:claude OR harness:codex").expr
    );
}

#[test]
fn comma_list_is_or() {
    let query = q("harness:claude,codex");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 1)));
    assert!(query.prefilter(&refx("a", Harness::Codex, None, None, 1)));
    assert!(!query.prefilter(&refx("a", Harness::Grok, None, None, 1)));
}

#[test]
fn num_ops_and_range() {
    assert!(q("msgs>5").prefilter(&refx("a", Harness::Claude, None, None, 6)));
    assert!(!q("msgs>5").prefilter(&refx("a", Harness::Claude, None, None, 5)));
    assert!(q("msgs<=5").prefilter(&refx("a", Harness::Claude, None, None, 5)));
    assert!(q("msgs:10..20").prefilter(&refx("a", Harness::Claude, None, None, 15)));
    assert!(!q("msgs:10..20").prefilter(&refx("a", Harness::Claude, None, None, 21)));
}

#[test]
fn dates_and_before_after_sugar() {
    let r = refx("a", Harness::Claude, None, None, 1); // updated 2026-06-01
    assert!(q("after:2026-01-01").prefilter(&r));
    assert!(!q("after:2026-07-01").prefilter(&r));
    assert!(q("before:2026-07-01").prefilter(&r));
    assert!(q("updated:2026-01-01..2026-12-31").prefilter(&r));
    assert!(q("updated>=2026-06-01").prefilter(&r));
}

#[test]
fn bare_word_matches_title_cwd_id() {
    let query = q("widget");
    assert!(query.prefilter(&refx("x", Harness::Claude, None, Some("the Widget thing"), 1)));
    assert!(query.prefilter(&refx("widget-7", Harness::Claude, None, None, 1)));
    assert!(query.prefilter(&refx("x", Harness::Claude, Some("/src/widget"), None, 1)));
    assert!(!query.prefilter(&refx("x", Harness::Claude, Some("/src/other"), Some("nope"), 1)));
}

#[test]
fn quoted_phrase_with_spaces() {
    let query = q(r#"title:"fix the bug""#);
    assert!(query.prefilter(&refx("a", Harness::Claude, None, Some("Fix The Bug now"), 1)));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, Some("fixthebug"), 1)));
}

#[test]
fn model_is_parse_required_three_valued() {
    let query = q("model:fable");
    assert!(query.needs_parse());
    // prefilter (no session) must NOT exclude on the unknown leaf
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 9)));
    // full match against a session
    let s = sess(Some("claude-opus-4"), &[Some("claude-fable-5"), None]);
    assert!(query.matches_session(&s));
    let s2 = sess(Some("claude-opus-4"), &[Some("claude-sonnet-4-6")]);
    assert!(!query.matches_session(&s2));
}

#[test]
fn not_over_parse_required_prefilter_keeps() {
    // `-model:fable`: with no session the inner leaf is Unknown → NOT Unknown = Unknown → prefilter
    // must keep it (can't prove false yet).
    let query = q("-model:fable");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 1)));
    // and decides correctly once parsed
    assert!(!query.matches_session(&sess(None, &[Some("claude-fable-5")])));
    assert!(query.matches_session(&sess(None, &[Some("claude-opus-4")])));
}

#[test]
fn git_field_matches_branch() {
    let query = q("git:lazy-content");
    assert!(query.needs_parse());
    let mut s = sess(None, &[]);
    s.git = Some(GitInfo {
        branch: Some("lazy-content-ir".into()),
        commit: None,
        remote: None,
    });
    assert!(query.matches_session(&s));
}

#[test]
fn external_fields_classified_and_unknown_in_prefilter() {
    let query = q("text:\"SandboxDenied\" tool:Bash touched:src/ir.rs has:errors");
    assert!(query.needs_index());
    assert!(query.needs_events());
    // All external → Unknown in the cheap prefilter, so it must not exclude.
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 1)));
}

#[test]
fn ext_facts_drive_external_terms() {
    struct Fx;
    impl ExtFacts for Fx {
        fn tool(&self, n: &str) -> Tri {
            Tri::of(n == "bash")
        }
        fn has(&self, f: &str) -> Tri {
            Tri::of(f == "errors")
        }
    }
    let query = q("tool:Bash has:errors");
    let r = refx("a", Harness::Claude, None, None, 1);
    let facts = Facts {
        r: &r,
        session: None,
        ext: &Fx,
    };
    assert!(query.matches(&facts));
    let query2 = q("tool:Grep");
    assert!(!query2.matches(&facts)); // Fx.tool("grep") = False
}

#[test]
fn regex_operator() {
    // case-insensitive by default
    let query = q("title~^fix.*bug$");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, Some("FIX the Bug"), 1)));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, Some("bug fix later"), 1)));
    // regex on model (parse-required)
    let mq = q(r"model~fable|opus");
    assert!(mq.needs_parse());
    assert!(mq.matches_session(&sess(Some("claude-opus-4-8"), &[])));
    assert!(mq.matches_session(&sess(Some("claude-fable-5"), &[])));
    assert!(!mq.matches_session(&sess(Some("claude-sonnet-4-6"), &[])));
    // equivalent AST regardless of source identity isn't expected; just ensure parse round-trips
    assert_eq!(q("title~ab").expr, q("title~ab").expr);
}

#[test]
fn regex_errors_and_restrictions() {
    // a literal paren in a regex must be quoted (bare `(` is grouping syntax)
    assert!(SessionQuery::parse(r#"title~"(unclosed""#)
        .unwrap_err()
        .contains("bad regex"));
    // `~` rejected on external/non-string fields
    assert!(SessionQuery::parse("text~foo").unwrap_err().contains("regex"));
    assert!(SessionQuery::parse("msgs~5").unwrap_err().contains("regex"));
}

#[test]
fn forest_fields_classify_and_resolve() {
    let query = q("agent:Explore subtool:Bash agents>=3 workflows>=1 compactions>=2 workflow:census has:compacted");
    assert!(query.needs_forest());
    // forest leaves are Unknown in the cheap prefilter → must not exclude
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 1)));

    struct Fx;
    impl ExtFacts for Fx {
        fn subtool(&self, n: &str) -> Tri {
            Tri::of(n == "bash")
        }
        fn agent_type(&self, n: &str) -> Tri {
            Tri::of(n == "explore")
        }
        fn workflow(&self, n: &str) -> Tri {
            Tri::of(n == "census")
        }
        fn has(&self, f: &str) -> Tri {
            Tri::of(f == "compacted")
        }
        fn count(&self, field: FieldId) -> Option<u64> {
            Some(match field {
                FieldId::Agents => 4,
                FieldId::Workflows => 1,
                FieldId::Compactions => 2,
                _ => return None,
            })
        }
    }
    let r = refx("a", Harness::Claude, None, None, 1);
    let facts = Facts {
        r: &r,
        session: None,
        ext: &Fx,
    };
    assert!(query.matches(&facts));

    // a count that fails the comparison flips it false
    let q2 = q("agents>=10");
    assert!(!q2.matches(&facts)); // Fx says 4
                                  // regex is rejected on forest fields
    assert!(SessionQuery::parse("agent~explore").unwrap_err().contains("regex"));
    assert!(SessionQuery::parse("subtool~bash").unwrap_err().contains("regex"));
}

#[test]
fn thread_matches_message_path() {
    use crate::ir::{Block, Message, Role};
    let msg = |id: &str, parent: Option<&str>, role: Role, blocks: Vec<Block>| Message {
        id: Some(id.into()),
        parent_id: parent.map(str::to_string),
        role,
        timestamp: None,
        model: None,
        content: blocks,
        usage: None,
        extra: serde_json::Map::new(),
        kind: crate::ir::MessageKind::for_role(role),
        origin: crate::ir::Origin::for_role(role),
    };
    let text = |t: &str| Block::Text { text: t.into() };
    // u1("please refactor") -> a1(assistant) -> t1(tool Bash)
    let s = Session {
        id: "s".into(),
        harness: Harness::Claude,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: vec![
            msg("u1", None, Role::User, vec![text("please refactor this")]),
            msg("a1", Some("u1"), Role::Assistant, vec![text("on it")]),
            msg(
                "t1",
                Some("a1"),
                Role::Tool,
                vec![Block::ToolUse {
                    id: "x".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({}),
                    namespace: None,
                }],
            ),
        ],
        source_path: None,
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };

    assert!(q(r#"thread:"text:refactor > assistant""#).matches_session(&s));
    assert!(q(r#"thread:"user > assistant > tool:Bash""#).matches_session(&s));
    assert!(q(r#"thread:"refactor > assistant > tool:bash""#).matches_session(&s)); // bare word=text, case-insens
                                                                                    // wrong child role
    assert!(!q(r#"thread:"user > tool""#).matches_session(&s)); // a1 is assistant, not tool
                                                                // tool not present as a child of user
    assert!(!q(r#"thread:"user > tool:Grep""#).matches_session(&s));
    // single step still works
    assert!(q(r#"thread:"tool:Bash""#).matches_session(&s));

    assert!(SessionQuery::parse(r#"thread:"user > > assistant""#)
        .unwrap_err()
        .contains("empty step"));
    assert!(SessionQuery::parse(r#"thread:"role:bogus""#)
        .unwrap_err()
        .contains("role"));
    assert!(SessionQuery::parse("thread~x").unwrap_err().contains("thread"));
    assert!(q("thread:\"user\"").needs_parse());
}

#[test]
fn errors_teach() {
    // A near-miss of a real field errors with a suggestion (the typo guard) …
    let err = SessionQuery::parse("harnes:claude").unwrap_err();
    assert!(err.contains("unknown field") && err.contains("harness"), "{err}");
    assert!(SessionQuery::parse("titel:x").unwrap_err().contains("title"));
    assert!(SessionQuery::parse("harness:nope").unwrap_err().contains("harness"));
    assert!(SessionQuery::parse("msgs>=abc").unwrap_err().contains("number"));
    assert!(SessionQuery::parse("(harness:claude").unwrap_err().contains(')'));
    assert!(SessionQuery::parse(r#"title:"unterminated"#)
        .unwrap_err()
        .contains("quote"));
    assert!(SessionQuery::parse("title>foo").unwrap_err().contains("isn't valid"));
}

#[test]
fn reference_and_schema_cover_every_field() {
    let r = reference();
    for f in FIELDS {
        assert!(r.contains(f.name), "reference missing {}", f.name);
    }
    let schema = schema_json();
    let names: Vec<&str> = schema["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), FIELDS.len());
    assert!(names.contains(&"model"));
}

#[test]
fn bare_url_and_unknown_keys_fall_back_to_needles() {
    // `http://…` is field-shaped ("http" + ':') but not a field — it must search, not error.
    let query = q("http://example.com/x");
    assert!(query.prefilter(&refx(
        "a",
        Harness::Claude,
        None,
        Some("see http://example.com/x please"),
        1
    )));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, Some("no links here"), 1)));
    // An unknown key that's nowhere near a real field is a needle too.
    let query = q("bogus:1");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, Some("the Bogus:1 flag"), 1)));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, Some("nothing"), 1)));
}

#[test]
fn quoted_values_keep_commas_literal() {
    // Quoted value: the comma is text, not an OR-list.
    let query = q(r#"title:"foo, bar""#);
    assert!(query.prefilter(&refx("a", Harness::Claude, None, Some("FOO, bar baz"), 1)));
    assert!(!query.prefilter(&refx("a", Harness::Claude, None, Some("just foo here"), 1)));
    // Unquoted still OR-splits (unchanged).
    let or = q("title:foo,bar");
    assert!(or.prefilter(&refx("a", Harness::Claude, None, Some("just foo here"), 1)));
    // A fully-quoted word is a needle even when it looks like a field term.
    let needle = q(r#""text: hello""#);
    assert!(!needle.needs_index(), "quoted phrase must not parse as the text: field");
    assert!(needle.prefilter(&refx("a", Harness::Claude, None, Some("prefix text: hello suffix"), 1)));
    assert!(!needle.prefilter(&refx("a", Harness::Claude, None, Some("hello text"), 1)));
}

#[test]
fn until_since_sugar_and_explicit_ops_on_sugar_error() {
    let r = refx("a", Harness::Claude, None, None, 1); // updated 2026-06-01
    assert!(q("until:2026-07-01").prefilter(&r));
    assert!(!q("until:2026-01-01").prefilter(&r));
    assert!(q("since:2026-01-01").prefilter(&r));
    assert!(!q("since:2026-07-01").prefilter(&r));
    // The sugar pairs are spelled differently but parse identically.
    assert_eq!(q("before:2026-07-01").expr, q("until:2026-07-01").expr);
    assert_eq!(q("after:2026-01-01").expr, q("since:2026-01-01").expr);
    // An explicit comparison on sugar contradicts the implied one — it must teach, not override.
    for s in [
        "before>=2026-01-01",
        "after<2026-01-01",
        "until=2026-01-01",
        "since>2026-01-01",
    ] {
        assert!(SessionQuery::parse(s).unwrap_err().contains("sugar"), "{s}");
    }
}

#[test]
fn plain_date_contains_means_whole_day() {
    let mut r = refx("a", Harness::Claude, None, None, 1);
    r.updated_at = "2026-06-01T15:30:00Z".parse().ok();
    assert!(q("updated:2026-06-01").prefilter(&r)); // any instant that day, not just midnight
    assert!(!q("updated:2026-05-31").prefilter(&r));
    assert!(!q("updated:2026-06-02").prefilter(&r));
    // `=` keeps exact-instant semantics, and an RFC3339 `:` value stays exact.
    assert!(!q("updated=2026-06-01").prefilter(&r));
    assert!(q("updated:2026-06-01T15:30:00Z").prefilter(&r));
    // Comparisons are unchanged: midnight boundary.
    assert!(q("updated>=2026-06-01").prefilter(&r));
    assert!(!q("updated<2026-06-01").prefilter(&r));
}

#[test]
fn msgs_counts_user_assistant_only_in_matches_session() {
    // SessionRef.message_count is contractually user+assistant turns only (ir.rs) — the ref view
    // matches_session builds must count the same way, or prefilter and full match disagree.
    let mut s = sess(None, &[Some("m1"), Some("m2")]); // two assistant turns
    let extra = |role: Role| Message {
        id: None,
        parent_id: None,
        role,
        timestamp: None,
        model: None,
        content: vec![],
        usage: None,
        extra: serde_json::Map::new(),
        kind: crate::ir::MessageKind::for_role(role),
        origin: crate::ir::Origin::for_role(role),
    };
    s.messages.push(extra(Role::User));
    s.messages.push(extra(Role::Tool));
    s.messages.push(extra(Role::System));
    assert!(q("msgs:3").matches_session(&s)); // 2 assistant + 1 user
    assert!(!q("msgs:5").matches_session(&s)); // NOT all five records
}

// --- helpers ---

fn sess(primary: Option<&str>, turn_models: &[Option<&str>]) -> Session {
    Session {
        id: "s".into(),
        harness: Harness::Claude,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: primary.map(str::to_string),
        git: None,
        messages: turn_models
            .iter()
            .map(|m| Message {
                id: None,
                parent_id: None,
                role: Role::Assistant,
                timestamp: None,
                model: m.map(str::to_string),
                content: vec![],
                usage: None,
                extra: serde_json::Map::new(),
                kind: crate::ir::MessageKind::for_role(Role::Assistant),
                origin: crate::ir::Origin::for_role(Role::Assistant),
            })
            .collect(),
        source_path: None,
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    }
}
